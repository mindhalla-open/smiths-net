//! Subprocess supervisor for one sidecar plugin.
//!
//! A [`Sidecar`] wraps one child process, an auto-restart supervisor
//! task, and the JSON-RPC pending-response map. When the child dies
//! unexpectedly, the supervisor respawns it with exponential backoff
//! up to [`RestartPolicy::max_retries`]. External callers see the
//! same API before and after a restart; inflight RPCs that happened
//! to be outstanding at crash time resolve to [`Error::Closed`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use smiths_core::Metrics;
use smiths_core::metrics::PluginLabel;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use crate::error::Error;
use crate::rpc::{RpcRequest, RpcResponse};

/// Default RPC timeout — conservative because AI models can be slow.
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Depth of the notification broadcast. Small on purpose —
/// subscribers that fall behind lose intermediate partials, which is
/// the right behaviour for streaming ASR / TTS (agent reconnects +
/// resubscribes rather than serving stale frames).
const NOTIFICATION_BUFFER: usize = 128;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>>;

/// Plugin-initiated notification received over stdout — JSON-RPC 2.0
/// frame with `method` but no `id`. The [`Sidecar`] broadcasts every
/// incoming notification through [`Sidecar::subscribe_notifications`]
/// so higher layers (streaming MCP tools, the event bus bridge) can
/// fan them out.
#[derive(Debug, Clone)]
pub struct PluginNotification {
    /// Method name from the frame (e.g. `"emit_partial"`).
    pub method: String,
    /// Params the plugin attached, if any.
    pub params: Option<serde_json::Value>,
}

/// Restart policy applied when a sidecar child dies unexpectedly.
#[derive(Clone, Copy, Debug)]
pub struct RestartPolicy {
    /// Max consecutive failed spawns before the supervisor gives up.
    /// `0` disables auto-restart entirely — the supervisor tears down
    /// on first crash, mirroring the pre-restart behaviour.
    pub max_retries: u32,
    /// Backoff before the first restart attempt after a crash.
    pub initial_backoff: Duration,
    /// Upper bound after multiplier expansion.
    pub max_backoff: Duration,
    /// Multiplier applied to the current backoff each attempt.
    pub backoff_multiplier: f64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_retries: 5,
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(30),
            backoff_multiplier: 2.0,
        }
    }
}

impl RestartPolicy {
    /// Policy that never respawns — useful when the caller manages
    /// lifecycle themselves.
    #[must_use]
    pub const fn no_restart() -> Self {
        Self {
            max_retries: 0,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            backoff_multiplier: 1.0,
        }
    }

    fn next_backoff(self, current: Duration) -> Duration {
        let next = current.mul_f64(self.backoff_multiplier);
        if next > self.max_backoff {
            self.max_backoff
        } else {
            next
        }
    }
}

/// Writer handle swapped in/out on each respawn. Lives under
/// [`Inner::process`] so callers grab it briefly to send a frame
/// without holding the lock across the socket write.
#[derive(Debug)]
struct ProcessState {
    stdin: ChildStdin,
}

/// One running sidecar plugin.
///
/// Cheap to clone through `Arc` — the inner state is reference-counted
/// so multiple callers can invoke RPCs concurrently across restarts.
#[derive(Debug)]
pub struct Sidecar {
    inner: Arc<Inner>,
}

struct Inner {
    name: String,
    next_id: AtomicU64,
    pending: Pending,
    /// Broadcasts every incoming notification frame. Senders hold one
    /// clone; subscribers call [`Sidecar::subscribe_notifications`].
    notifications: broadcast::Sender<PluginNotification>,
    /// `Some` while a child is alive; `None` between crash and
    /// respawn, or permanently after retries are exhausted.
    process: Mutex<Option<ProcessState>>,
    plugin_dir: PathBuf,
    entry: PathBuf,
    policy: RestartPolicy,
    /// Flipped by `shutdown` so the supervisor stops respawning.
    shutdown: CancellationToken,
    /// Supervisor task handle. Owned so `shutdown` can `await` it.
    supervisor: Mutex<Option<JoinHandle<()>>>,
    /// Engine-wide metrics handle, optional because tests don't wire
    /// one. Set at most once via [`Sidecar::with_metrics`]; the
    /// supervise loop reads it on every respawn.
    metrics: OnceLock<Arc<Metrics>>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("name", &self.name)
            .field("plugin_dir", &self.plugin_dir)
            .field("entry", &self.entry)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl Sidecar {
    /// Spawn with the default [`RestartPolicy`].
    pub async fn spawn(
        name: impl Into<String>,
        plugin_dir: &Path,
        entry: &Path,
    ) -> Result<Self, Error> {
        Self::spawn_with_policy(name, plugin_dir, entry, RestartPolicy::default()).await
    }

    /// Spawn with an explicit restart policy. `RestartPolicy::no_restart()`
    /// is the suicide-on-crash behaviour used by the original tests.
    #[instrument(skip_all, fields(dir = %plugin_dir.display(), entry = %entry.display()))]
    pub async fn spawn_with_policy(
        name: impl Into<String>,
        plugin_dir: &Path,
        entry: &Path,
        policy: RestartPolicy,
    ) -> Result<Self, Error> {
        let name: String = name.into();
        let entry_abs = canonical_entry(plugin_dir, entry)?;
        let plugin_dir_abs =
            std::fs::canonicalize(plugin_dir).unwrap_or_else(|_| plugin_dir.to_path_buf());

        let (notif_tx, _) = broadcast::channel::<PluginNotification>(NOTIFICATION_BUFFER);
        let inner = Arc::new(Inner {
            name: name.clone(),
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            notifications: notif_tx,
            process: Mutex::new(None),
            plugin_dir: plugin_dir_abs,
            entry: entry_abs,
            policy,
            shutdown: CancellationToken::new(),
            supervisor: Mutex::new(None),
            metrics: OnceLock::new(),
        });

        // First spawn runs synchronously so the caller sees a clean
        // error (and doesn't have to race a background task for it).
        let (child, stdin, stdout, stderr) = spawn_once(&inner).await?;
        *inner.process.lock().await = Some(ProcessState { stdin });

        let sup = tokio::spawn(supervise_loop(Arc::clone(&inner), child, stdout, stderr));
        *inner.supervisor.lock().await = Some(sup);

        Ok(Self { inner })
    }

    /// Plugin name (for diagnostics).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Attach an engine-wide metrics handle. Can only be called once
    /// per sidecar; subsequent calls are no-ops (the first metrics
    /// wins). Used by the plugin loader when `LoaderOpts::metrics` is
    /// set, so `sidecar_restarts` counts flow through on respawn.
    pub fn set_metrics(&self, metrics: Arc<Metrics>) {
        let _ = self.inner.metrics.set(metrics);
    }

    /// Subscribe to plugin-initiated notifications (JSON-RPC frames
    /// with `method` but no `id`). Each call returns a fresh receiver;
    /// lagging subscribers miss intermediate frames rather than
    /// blocking the reader loop.
    #[must_use]
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<PluginNotification> {
        self.inner.notifications.subscribe()
    }

    /// Send a JSON-RPC request and await the correlated response.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        self.call_with_timeout(method, params, DEFAULT_RPC_TIMEOUT)
            .await
    }

    /// Same as [`Self::call`] with an explicit timeout override.
    #[instrument(skip(self, params), fields(plugin = %self.inner.name, %method))]
    pub async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        deadline: Duration,
    ) -> Result<Value, Error> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<RpcResponse>();
        self.inner.pending.lock().await.insert(id, tx);

        let frame = RpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let mut buf = serde_json::to_vec(&frame)?;
        buf.push(b'\n');

        // Write under the process lock so the child doesn't get mid-
        // frame bytes if it dies during the write.
        {
            let mut guard = self.inner.process.lock().await;
            let Some(p) = guard.as_mut() else {
                // Pending entry was installed above — remove it before
                // bailing so we don't leak.
                self.inner.pending.lock().await.remove(&id);
                return Err(Error::Closed);
            };
            if let Err(e) = p.stdin.write_all(&buf).await {
                self.inner.pending.lock().await.remove(&id);
                return Err(Error::Io(e));
            }
            let _ = p.stdin.flush().await;
        }

        match timeout(deadline, rx).await {
            Ok(Ok(resp)) => {
                if let Some(err) = resp.error {
                    return Err(Error::Plugin {
                        code: err.code,
                        message: err.message,
                    });
                }
                Ok(resp.result.unwrap_or(Value::Null))
            }
            Ok(Err(_)) => {
                // Sender dropped — fail_pending fires when the child dies.
                self.inner.pending.lock().await.remove(&id);
                Err(Error::Closed)
            }
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                Err(Error::Timeout {
                    method: method.to_owned(),
                    timeout_ms: u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
                })
            }
        }
    }

    /// Request the plugin exit gracefully. Stops the supervisor, so no
    /// more respawns happen; then kills the live child if still up.
    pub async fn shutdown(&self) {
        // Best-effort graceful notification.
        let _ = self
            .call_with_timeout("shutdown", Value::Null, Duration::from_millis(500))
            .await;
        self.inner.shutdown.cancel();
        let supervisor = self.inner.supervisor.lock().await.take();
        if let Some(sup) = supervisor {
            let _ = sup.await;
        }
        *self.inner.process.lock().await = None;
        fail_pending(&self.inner.pending).await;
    }
}

impl Clone for Sidecar {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[allow(clippy::too_many_lines)] // single-file supervisor loop; splitting hurts readability.
/// Own one child from first byte of stdout through EOF; respawn on
/// unexpected exit up to [`RestartPolicy::max_retries`]. Exits cleanly
/// when `shutdown` fires.
async fn supervise_loop(
    inner: Arc<Inner>,
    mut child: Child,
    mut stdout: ChildStdout,
    mut stderr: ChildStderr,
) {
    let mut backoff = inner.policy.initial_backoff;
    let mut attempts: u32 = 0;

    loop {
        let mut reader = spawn_stdout_reader(
            stdout,
            Arc::clone(&inner.pending),
            inner.notifications.clone(),
            inner.name.clone(),
        );
        let stderr_task = spawn_stderr_forwarder(stderr, inner.name.clone());

        // Wait for shutdown or for stdout EOF. Only the reader is a
        // correctness signal — once it returns, every frame already on
        // the wire has been dispatched. stderr is log forwarding; a
        // child that writes nothing to stderr closes that pipe first,
        // and waking on it would race us into aborting the reader mid-
        // dispatch and losing the last response frame.
        tokio::select! {
            biased;
            () = inner.shutdown.cancelled() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                reader.abort();
                stderr_task.abort();
                return;
            }
            _ = &mut reader => {}
        }
        stderr_task.abort();

        // Clean up this process and report pending RPCs as closed.
        let _ = child.start_kill();
        let _ = child.wait().await;
        *inner.process.lock().await = None;
        fail_pending(&inner.pending).await;

        if inner.policy.max_retries == 0 {
            info!(plugin = %inner.name, "no restart policy; supervisor exiting");
            return;
        }

        attempts += 1;
        if attempts > inner.policy.max_retries {
            warn!(
                plugin = %inner.name,
                attempts,
                "restart attempts exhausted; supervisor giving up"
            );
            return;
        }
        info!(
            plugin = %inner.name,
            attempt = attempts,
            backoff_ms = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
            "sidecar crashed; scheduling respawn"
        );
        sleep(backoff).await;

        match spawn_once(&inner).await {
            Ok((new_child, new_stdin, new_stdout, new_stderr)) => {
                *inner.process.lock().await = Some(ProcessState { stdin: new_stdin });
                child = new_child;
                stdout = new_stdout;
                stderr = new_stderr;
                attempts = 0;
                backoff = inner.policy.initial_backoff;
                if let Some(m) = inner.metrics.get() {
                    m.sidecar_restarts
                        .get_or_create(&PluginLabel {
                            plugin: inner.name.clone(),
                        })
                        .inc();
                }
                info!(plugin = %inner.name, "sidecar respawned");
            }
            Err(e) => {
                warn!(plugin = %inner.name, ?e, attempt = attempts, "respawn failed");
                backoff = inner.policy.next_backoff(backoff);
                // Loop back and retry spawn after another backoff.
                // We still need something in `child`/`stdout`/`stderr`
                // for the reader to poll — skip to the top with a fake
                // exit: easiest is to `continue` but we don't have
                // streams. Instead, keep retrying inline.
                loop {
                    if inner.shutdown.is_cancelled() || attempts > inner.policy.max_retries {
                        return;
                    }
                    sleep(backoff).await;
                    attempts += 1;
                    match spawn_once(&inner).await {
                        Ok((c, stdin, so, se)) => {
                            *inner.process.lock().await = Some(ProcessState { stdin });
                            child = c;
                            stdout = so;
                            stderr = se;
                            attempts = 0;
                            backoff = inner.policy.initial_backoff;
                            if let Some(m) = inner.metrics.get() {
                                m.sidecar_restarts
                                    .get_or_create(&PluginLabel {
                                        plugin: inner.name.clone(),
                                    })
                                    .inc();
                            }
                            break;
                        }
                        Err(err) => {
                            warn!(plugin = %inner.name, ?err, attempt = attempts, "respawn still failing");
                            backoff = inner.policy.next_backoff(backoff);
                        }
                    }
                }
            }
        }
    }
}

/// Resolve `entry` relative to `plugin_dir` and canonicalize. Split
/// out because both the first spawn and restarts re-check the path.
fn canonical_entry(plugin_dir: &Path, entry: &Path) -> Result<PathBuf, Error> {
    let raw: PathBuf = if entry.is_absolute() {
        entry.to_path_buf()
    } else {
        plugin_dir.join(entry)
    };
    std::fs::canonicalize(&raw).map_err(Error::Io)
}

/// Spawn one child and hand back its live handles.
///
/// `async` is kept even though the body doesn't currently await on the
/// hot path — respawn workflows benefit from a uniform await-point
/// signature, and a future readiness ping (SIGSTART / health probe)
/// lands here without ripple-changing call sites.
#[allow(clippy::unused_async)]
async fn spawn_once(inner: &Inner) -> Result<(Child, ChildStdin, ChildStdout, ChildStderr), Error> {
    let mut cmd = Command::new(&inner.entry);
    cmd.current_dir(&inner.plugin_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| {
        warn!(plugin = %inner.name, entry = %inner.entry.display(), ?e, "sidecar spawn failed");
        Error::Io(e)
    })?;
    info!(
        plugin = %inner.name,
        pid = ?child.id(),
        entry = %inner.entry.display(),
        "sidecar spawned"
    );
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::Io(std::io::Error::other("no stdin")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Io(std::io::Error::other("no stdout")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Io(std::io::Error::other("no stderr")))?;
    Ok((child, stdin, stdout, stderr))
}

/// Fail every in-flight request with `Error::Closed`. Called from the
/// supervisor when the child exits before sending its response.
async fn fail_pending(pending: &Pending) {
    let drained: Vec<_> = pending.lock().await.drain().collect();
    for (_id, tx) in drained {
        // Sender drop = receiver wakes with `Err(_)` = `Error::Closed`.
        drop(tx);
    }
}

/// Background task: drain `stdout` as newline-delimited JSON-RPC
/// frames and route each response to its pending request by id.
/// Plugin-initiated notifications (frames without `id`) are
/// broadcast on `notifications` for streaming subscribers.
fn spawn_stdout_reader(
    stdout: ChildStdout,
    pending: Pending,
    notifications: broadcast::Sender<PluginNotification>,
    name: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    dispatch_frame(trimmed, &pending, &notifications, &name).await;
                }
                Ok(None) => {
                    debug!(plugin = %name, "sidecar stdout closed");
                    break;
                }
                Err(e) => {
                    warn!(plugin = %name, ?e, "stdout read error");
                    break;
                }
            }
        }
    })
}

/// Parse one wire frame and forward it to the correlating request.
async fn dispatch_frame(
    frame: &str,
    pending: &Pending,
    notifications: &broadcast::Sender<PluginNotification>,
    name: &str,
) {
    match serde_json::from_str::<RpcResponse>(frame) {
        Ok(resp) => {
            if let Some(id) = resp.id {
                let tx = pending.lock().await.remove(&id);
                if let Some(tx) = tx {
                    let _ = tx.send(resp);
                } else {
                    debug!(plugin = %name, id, "response to unknown id");
                }
            } else if let Some(method) = resp.method {
                // Plugin-initiated notification — fan out to
                // `subscribe_notifications()` consumers. Send error
                // just means "no subscribers right now", which is
                // normal when no streaming tool is active.
                let _ = notifications.send(PluginNotification {
                    method,
                    params: resp.params,
                });
            } else {
                debug!(plugin = %name, "frame without id or method; dropped");
            }
        }
        Err(e) => {
            warn!(plugin = %name, ?e, raw = %frame, "bad frame");
        }
    }
}

/// Background task: tag plugin stderr lines with the plugin name and
/// re-emit them into the engine's tracing log.
fn spawn_stderr_forwarder(stderr: ChildStderr, name: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            info!(plugin = %name, "{}", line);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    const ECHO_SCRIPT: &str = r#"#!/usr/bin/env bash
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -nE 's/.*"id"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p')
  method=$(printf '%s' "$line" | sed -nE 's/.*"method"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p')
  printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true,"method":"%s"}}\n' "$id" "$method"
done
"#;

    fn make_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        let mut p = fs::metadata(&path).unwrap().permissions();
        p.set_mode(0o755);
        fs::set_permissions(&path, p).unwrap();
        path
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rpc_round_trip_through_echo_script() {
        let dir = tempdir().unwrap();
        make_script(dir.path(), "echo.sh", ECHO_SCRIPT);

        let sidecar = Sidecar::spawn_with_policy(
            "echo-test",
            dir.path(),
            Path::new("./echo.sh"),
            RestartPolicy::no_restart(),
        )
        .await
        .unwrap();

        let out = sidecar
            .call("describe_capabilities", Value::Null)
            .await
            .unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["method"], "describe_capabilities");

        sidecar.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_of_missing_binary_fails_cleanly() {
        let dir = tempdir().unwrap();
        let err = Sidecar::spawn("absent", dir.path(), Path::new("./does-not-exist"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    /// After the child crashes (stdout EOF), the supervisor respawns
    /// it and a subsequent RPC completes against the fresh child.
    #[tokio::test(flavor = "multi_thread")]
    async fn sidecar_respawns_after_crash() {
        let dir = tempdir().unwrap();
        // Script that answers ONE RPC then exits, forcing a respawn
        // before the second RPC can succeed.
        let body = r#"#!/usr/bin/env bash
read -r line
id=$(printf '%s' "$line" | sed -nE 's/.*"id"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p')
printf '{"jsonrpc":"2.0","id":%s,"result":{"n":1}}\n' "$id"
exit 0
"#;
        make_script(dir.path(), "flaky.sh", body);

        let sidecar = Sidecar::spawn_with_policy(
            "flaky",
            dir.path(),
            Path::new("./flaky.sh"),
            RestartPolicy {
                max_retries: 3,
                initial_backoff: Duration::from_millis(20),
                max_backoff: Duration::from_millis(50),
                backoff_multiplier: 1.5,
            },
        )
        .await
        .unwrap();

        let first = sidecar.call("ping", Value::Null).await.unwrap();
        assert_eq!(first["n"], 1);

        // Give the supervisor time to notice EOF and respawn.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let second = sidecar.call("ping", Value::Null).await.unwrap();
        assert_eq!(second["n"], 1); // fresh child answered afresh

        sidecar.shutdown().await;
    }

    /// `no_restart()` policy: after one crash the Sidecar stays dead
    /// and subsequent calls return `Error::Closed`.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_restart_policy_stays_down_after_crash() {
        let dir = tempdir().unwrap();
        let body = r#"#!/usr/bin/env bash
read -r line
id=$(printf '%s' "$line" | sed -nE 's/.*"id"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p')
printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
exit 1
"#;
        make_script(dir.path(), "oneshot.sh", body);

        let sidecar = Sidecar::spawn_with_policy(
            "oneshot",
            dir.path(),
            Path::new("./oneshot.sh"),
            RestartPolicy::no_restart(),
        )
        .await
        .unwrap();

        sidecar.call("ping", Value::Null).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        let err = sidecar
            .call_with_timeout("ping", Value::Null, Duration::from_millis(500))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Closed | Error::Timeout { .. }));

        sidecar.shutdown().await;
    }

    /// Many concurrent RPCs on the echo script all complete — proxy
    /// for backpressure / id-multiplexing sanity under load.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_calls_all_complete() {
        let dir = tempdir().unwrap();
        make_script(dir.path(), "echo.sh", ECHO_SCRIPT);

        let sidecar = Sidecar::spawn("echo-test", dir.path(), Path::new("./echo.sh"))
            .await
            .unwrap();

        let mut handles = Vec::new();
        for i in 0..32 {
            let sc = sidecar.clone();
            handles.push(tokio::spawn(async move {
                sc.call(&format!("call_{i}"), Value::Null).await
            }));
        }
        let mut ok = 0;
        for h in handles {
            if let Ok(Ok(_)) = h.await {
                ok += 1;
            }
        }
        assert_eq!(ok, 32);
        sidecar.shutdown().await;
    }
}
