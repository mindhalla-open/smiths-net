//! Subprocess supervisor for one sidecar plugin.
//!
//! A [`Sidecar`] wraps one child process, an auto-restart supervisor
//! task, and the JSON-RPC pending-response map. When the child dies
//! unexpectedly, the supervisor respawns it with exponential backoff.
//! External callers see the same API before and after a restart;
//! inflight RPCs that happened to be outstanding at crash time
//! resolve to [`Error::Closed`].
//!
//! ## Restart budget
//!
//! Every crash or failed spawn consumes one of
//! [`RestartPolicy::max_retries`] attempts and grows the backoff.
//! The budget and backoff reset only once a child has stayed up for
//! [`RestartPolicy::min_uptime`] — a child that crashes on every
//! start therefore restarts a bounded number of times with growing
//! delays and is then left down, instead of looping forever at the
//! initial backoff.
//!
//! ## Frame cap
//!
//! Frames from the child's stdout are newline-delimited JSON. A line
//! longer than [`SpawnOptions::max_frame_bytes`] is a protocol
//! violation: the reader logs it, the child is killed, and the
//! supervisor's restart policy applies.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use smiths_core::metrics::PluginLabel;
use smiths_core::{Metrics, SandboxConfig};

use crate::sandbox;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use crate::error::Error;
use crate::rpc::{RpcRequest, RpcResponse};

/// Default RPC timeout — conservative because AI models can be slow.
pub const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Default cap on one newline-delimited frame from the child (16 MiB).
/// Generous enough for base64 audio in a single result; bounded so a
/// misbehaving plugin cannot grow the reader's buffer without limit.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

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
    /// Max consecutive restart attempts (crashes or failed spawns)
    /// before the supervisor gives up. `0` disables auto-restart
    /// entirely — the supervisor tears down on first crash.
    pub max_retries: u32,
    /// Backoff before the first restart attempt after a crash.
    pub initial_backoff: Duration,
    /// Upper bound after multiplier expansion.
    pub max_backoff: Duration,
    /// Multiplier applied to the current backoff each attempt.
    pub backoff_multiplier: f64,
    /// A child that stays up at least this long is considered
    /// healthy: when it eventually dies, the attempt counter and the
    /// backoff start over. Children that die sooner keep consuming
    /// the budget, which is what caps a crash loop.
    pub min_uptime: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_retries: 5,
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(30),
            backoff_multiplier: 2.0,
            min_uptime: Duration::from_secs(10),
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
            min_uptime: Duration::ZERO,
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

/// Everything [`Sidecar::spawn_with_options`] needs beyond the
/// executable location.
#[derive(Clone, Debug)]
pub struct SpawnOptions {
    /// Respawn behaviour after an unexpected exit.
    pub policy: RestartPolicy,
    /// Resource sandbox applied on every spawn (including respawns).
    /// Default is permissive.
    pub sandbox: SandboxConfig,
    /// Timeout for [`Sidecar::call`]. Calls that need a different
    /// bound use [`Sidecar::call_with_timeout`].
    pub rpc_timeout: Duration,
    /// Largest stdout frame the reader accepts; see the module docs.
    pub max_frame_bytes: usize,
    /// Extra environment variables for the child, layered over the
    /// engine's own environment. Applied on every spawn, so a
    /// restart picks up whatever the caller passed at spawn time.
    pub env: BTreeMap<String, String>,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            policy: RestartPolicy::default(),
            sandbox: SandboxConfig::default(),
            rpc_timeout: DEFAULT_RPC_TIMEOUT,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            env: BTreeMap::new(),
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
    /// Sandbox actions precomputed in the parent; cloned into each
    /// spawn's `pre_exec` closure.
    sandbox: sandbox::Plan,
    rpc_timeout: Duration,
    max_frame_bytes: usize,
    env: BTreeMap<String, String>,
    /// Flipped by `shutdown` so the supervisor stops respawning.
    shutdown: CancellationToken,
    /// Supervisor task handle. Owned so `shutdown` can `await` it.
    supervisor: Mutex<Option<JoinHandle<()>>>,
    /// Engine-wide metrics handle, optional because tests don't wire
    /// one. Set at most once via [`Sidecar::set_metrics`]; the
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
            .field("rpc_timeout", &self.rpc_timeout)
            .field("max_frame_bytes", &self.max_frame_bytes)
            .finish_non_exhaustive()
    }
}

/// A live child and the handles the supervisor owns for it.
struct Live {
    child: Child,
    stdout: ChildStdout,
    stderr: ChildStderr,
    spawned_at: Instant,
}

/// Why `run_child` returned.
enum ChildExit {
    /// `shutdown` fired; the supervisor must exit.
    Shutdown,
    /// The child's stdout closed (it died or closed the pipe).
    Died { uptime: Duration },
}

impl Sidecar {
    /// Spawn with the default [`RestartPolicy`] and no sandbox.
    pub async fn spawn(
        name: impl Into<String>,
        plugin_dir: &Path,
        entry: &Path,
    ) -> Result<Self, Error> {
        Self::spawn_with_options(name, plugin_dir, entry, SpawnOptions::default()).await
    }

    /// Spawn with an explicit restart policy and the default (empty)
    /// sandbox. Kept for callers that don't care about resource caps.
    pub async fn spawn_with_policy(
        name: impl Into<String>,
        plugin_dir: &Path,
        entry: &Path,
        policy: RestartPolicy,
    ) -> Result<Self, Error> {
        Self::spawn_with_options(
            name,
            plugin_dir,
            entry,
            SpawnOptions {
                policy,
                ..SpawnOptions::default()
            },
        )
        .await
    }

    /// Spawn with an explicit restart policy + sandbox and the default
    /// RPC timeout / frame cap.
    pub async fn spawn_with(
        name: impl Into<String>,
        plugin_dir: &Path,
        entry: &Path,
        policy: RestartPolicy,
        sandbox: SandboxConfig,
    ) -> Result<Self, Error> {
        Self::spawn_with_options(
            name,
            plugin_dir,
            entry,
            SpawnOptions {
                policy,
                sandbox,
                ..SpawnOptions::default()
            },
        )
        .await
    }

    /// Spawn with full [`SpawnOptions`]. The sandbox, environment, and
    /// frame cap apply to this spawn and every supervisor-driven
    /// respawn.
    #[instrument(skip_all, fields(dir = %plugin_dir.display(), entry = %entry.display()))]
    pub async fn spawn_with_options(
        name: impl Into<String>,
        plugin_dir: &Path,
        entry: &Path,
        options: SpawnOptions,
    ) -> Result<Self, Error> {
        let name: String = name.into();
        let entry_abs = canonical_entry(plugin_dir, entry)?;
        let plugin_dir_abs =
            std::fs::canonicalize(plugin_dir).unwrap_or_else(|_| plugin_dir.to_path_buf());
        // Compile the sandbox once, in the parent, so the child's
        // pre-exec closure only issues syscalls.
        let sandbox_plan = sandbox::prepare(&options.sandbox).map_err(Error::Io)?;

        let (notif_tx, _) = broadcast::channel::<PluginNotification>(NOTIFICATION_BUFFER);
        let inner = Arc::new(Inner {
            name: name.clone(),
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            notifications: notif_tx,
            process: Mutex::new(None),
            plugin_dir: plugin_dir_abs,
            entry: entry_abs,
            policy: options.policy,
            sandbox: sandbox_plan,
            rpc_timeout: options.rpc_timeout,
            max_frame_bytes: options.max_frame_bytes,
            env: options.env,
            shutdown: CancellationToken::new(),
            supervisor: Mutex::new(None),
            metrics: OnceLock::new(),
        });

        // First spawn runs synchronously so the caller sees a clean
        // error (and doesn't have to race a background task for it).
        let (live, stdin) = spawn_once(&inner).await?;
        *inner.process.lock().await = Some(ProcessState { stdin });

        let sup = tokio::spawn(supervise_loop(Arc::clone(&inner), live));
        *inner.supervisor.lock().await = Some(sup);

        Ok(Self { inner })
    }

    /// Plugin name (for diagnostics).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Timeout applied by [`Self::call`].
    #[must_use]
    pub fn rpc_timeout(&self) -> Duration {
        self.inner.rpc_timeout
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

    /// Send a JSON-RPC request and await the correlated response,
    /// bounded by the spawn-time `rpc_timeout`.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        self.call_with_timeout(method, params, self.inner.rpc_timeout)
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

/// Own the child from first byte of stdout through EOF; respawn on
/// unexpected exit within the restart budget. Exits cleanly when
/// `shutdown` fires.
async fn supervise_loop(inner: Arc<Inner>, first: Live) {
    let mut live = Some(first);
    let mut backoff = inner.policy.initial_backoff;
    let mut attempts: u32 = 0;

    loop {
        if let Some(current) = live.take() {
            match run_child(&inner, current).await {
                ChildExit::Shutdown => return,
                ChildExit::Died { uptime } => {
                    *inner.process.lock().await = None;
                    fail_pending(&inner.pending).await;
                    if uptime >= inner.policy.min_uptime {
                        // The child proved itself; a fresh crash gets
                        // the full budget again.
                        attempts = 0;
                        backoff = inner.policy.initial_backoff;
                    }
                }
            }
        }

        if inner.policy.max_retries == 0 {
            info!(plugin = %inner.name, "no restart policy; supervisor exiting");
            return;
        }
        attempts += 1;
        if attempts > inner.policy.max_retries {
            warn!(
                plugin = %inner.name,
                attempts = attempts - 1,
                "restart attempts exhausted; supervisor giving up"
            );
            return;
        }
        info!(
            plugin = %inner.name,
            attempt = attempts,
            backoff_ms = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
            "sidecar down; scheduling respawn"
        );
        tokio::select! {
            biased;
            () = inner.shutdown.cancelled() => return,
            () = sleep(backoff) => {}
        }
        backoff = inner.policy.next_backoff(backoff);

        match spawn_once(&inner).await {
            Ok((new_live, stdin)) => {
                *inner.process.lock().await = Some(ProcessState { stdin });
                live = Some(new_live);
                if let Some(m) = inner.metrics.get() {
                    m.sidecar_restarts
                        .get_or_create(&PluginLabel {
                            plugin: inner.name.clone(),
                        })
                        .inc();
                }
                info!(plugin = %inner.name, attempt = attempts, "sidecar respawned");
            }
            Err(e) => {
                // Counts as a failed attempt; the loop comes back
                // around with a longer backoff.
                warn!(plugin = %inner.name, ?e, attempt = attempts, "respawn failed");
            }
        }
    }
}

/// Drive one child until it exits or `shutdown` fires. Only the
/// stdout reader is a correctness signal — once it returns, every
/// frame already on the wire has been dispatched. stderr is log
/// forwarding; a child that writes nothing to stderr closes that pipe
/// first, and waking on it would race us into aborting the reader
/// mid-dispatch and losing the last response frame.
async fn run_child(inner: &Inner, mut live: Live) -> ChildExit {
    let mut reader = spawn_stdout_reader(
        live.stdout,
        Arc::clone(&inner.pending),
        inner.notifications.clone(),
        inner.name.clone(),
        inner.max_frame_bytes,
    );
    let stderr_task = spawn_stderr_forwarder(live.stderr, inner.name.clone());

    tokio::select! {
        biased;
        () = inner.shutdown.cancelled() => {
            let _ = live.child.start_kill();
            let _ = live.child.wait().await;
            reader.abort();
            stderr_task.abort();
            ChildExit::Shutdown
        }
        _ = &mut reader => {
            stderr_task.abort();
            let _ = live.child.start_kill();
            let _ = live.child.wait().await;
            ChildExit::Died {
                uptime: live.spawned_at.elapsed(),
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

/// Spawn one child and hand back its live handles plus stdin.
///
/// `async` is kept even though the body doesn't currently await on the
/// hot path — respawn workflows benefit from a uniform await-point
/// signature, and a future readiness ping (SIGSTART / health probe)
/// lands here without ripple-changing call sites.
#[allow(clippy::unused_async)]
async fn spawn_once(inner: &Inner) -> Result<(Live, ChildStdin), Error> {
    let mut cmd = Command::new(&inner.entry);
    cmd.current_dir(&inner.plugin_dir)
        .envs(&inner.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Unix: attach the sandbox via `pre_exec`. The closure runs in
    // the forked child immediately before `execve` and must be
    // async-signal-safe: the plan was compiled in the parent, so the
    // closure only issues syscalls and never allocates (see
    // `sandbox::apply_in_child`).
    //
    // `tokio::process::Command::pre_exec` is itself `unsafe fn` — the
    // workspace lint is `deny`, not `forbid`, so we carry a scoped
    // `#[allow(unsafe_code)]` here with this justification comment.
    // The allow stays intentionally narrow: this is the only spot in
    // the codebase that touches `unsafe`.
    #[cfg(unix)]
    {
        let plan = inner.sandbox.clone();
        #[allow(unsafe_code)]
        unsafe {
            cmd.pre_exec(move || sandbox::apply_in_child(&plan));
        }
    }

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
    Ok((
        Live {
            child,
            stdout,
            stderr,
            spawned_at: Instant::now(),
        },
        stdin,
    ))
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
/// broadcast on `notifications` for streaming subscribers. A frame
/// longer than `max_frame_bytes` ends the task — the supervisor then
/// treats the child as dead.
fn spawn_stdout_reader(
    stdout: ChildStdout,
    pending: Pending,
    notifications: broadcast::Sender<PluginNotification>,
    name: String,
    max_frame_bytes: usize,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut buf: Vec<u8> = Vec::new();
        // One frame may consume at most the cap plus its newline.
        let limit = u64::try_from(max_frame_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        loop {
            buf.clear();
            let n = match (&mut reader).take(limit).read_until(b'\n', &mut buf).await {
                Ok(n) => n,
                Err(e) => {
                    warn!(plugin = %name, ?e, "stdout read error");
                    break;
                }
            };
            if n == 0 {
                debug!(plugin = %name, "sidecar stdout closed");
                break;
            }
            if buf.last() != Some(&b'\n') {
                if buf.len() > max_frame_bytes {
                    warn!(
                        plugin = %name,
                        max_frame_bytes,
                        "stdout frame exceeds the frame cap; dropping the child"
                    );
                } else {
                    debug!(plugin = %name, "sidecar stdout closed mid-frame");
                }
                break;
            }
            let Ok(line) = std::str::from_utf8(&buf) else {
                warn!(plugin = %name, "bad frame: not UTF-8");
                continue;
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            dispatch_frame(trimmed, &pending, &notifications, &name).await;
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
                // `subscribe_notifications` consumers. Send error
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

    /// Sandbox test: spawn a bash script that reports its own
    /// `RLIMIT_NOFILE` soft cap. With `max_fds` set on the sandbox,
    /// the pre-exec closure calls `setrlimit` so the child observes
    /// exactly the configured value — proves the rlimit actually
    /// flows through `pre_exec` onto the target process.
    #[tokio::test(flavor = "multi_thread")]
    async fn sandbox_rlimit_nofile_is_applied_to_child() {
        let dir = tempdir().unwrap();
        let script = r#"#!/usr/bin/env bash
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -nE 's/.*"id"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p')
  nofile=$(ulimit -n)
  printf '{"jsonrpc":"2.0","id":%s,"result":{"nofile":%s}}\n' "$id" "$nofile"
done
"#;
        make_script(dir.path(), "ulimit.sh", script);

        let sandbox = SandboxConfig {
            max_fds: Some(64),
            ..SandboxConfig::default()
        };
        let sidecar = Sidecar::spawn_with(
            "sandbox-nofile",
            dir.path(),
            Path::new("./ulimit.sh"),
            RestartPolicy::no_restart(),
            sandbox,
        )
        .await
        .unwrap();

        let out = sidecar.call("check", Value::Null).await.unwrap();
        assert_eq!(
            out["nofile"].as_u64(),
            Some(64),
            "child should see RLIMIT_NOFILE=64; got: {out}"
        );

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
                ..RestartPolicy::default()
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

    /// `no_restart` policy: after one crash the Sidecar stays dead
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

    /// A child that dies on every start is restarted at most
    /// `max_retries` times, with the attempt counter never reset in
    /// between, and is then left down.
    #[tokio::test(flavor = "multi_thread")]
    async fn crash_loop_is_capped_by_max_retries() {
        let dir = tempdir().unwrap();
        let marker = dir.path().join("spawns.log");
        let body = format!(
            "#!/usr/bin/env bash\necho start >> '{}'\nexit 1\n",
            marker.display()
        );
        make_script(dir.path(), "crashloop.sh", &body);

        let sidecar = Sidecar::spawn_with_policy(
            "crashloop",
            dir.path(),
            Path::new("./crashloop.sh"),
            RestartPolicy {
                max_retries: 2,
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(20),
                backoff_multiplier: 2.0,
                min_uptime: Duration::from_secs(10),
            },
        )
        .await
        .unwrap();

        // 1 initial spawn + 2 retries, then the supervisor gives up.
        // Generous window: the suite runs many bash-spawning tests in
        // parallel, but the loop exits as soon as the count lands.
        let mut spawns = 0;
        for _ in 0..400 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            spawns = fs::read_to_string(&marker).map_or(0, |s| s.lines().count());
            if spawns >= 3 {
                break;
            }
        }
        assert_eq!(spawns, 3, "expected initial spawn + 2 retries");
        // Nothing else starts afterwards.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let spawns = fs::read_to_string(&marker).unwrap().lines().count();
        assert_eq!(spawns, 3, "supervisor must stop after the retry budget");

        let err = sidecar
            .call_with_timeout("ping", Value::Null, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Closed), "got {err:?}");

        sidecar.shutdown().await;
    }

    /// A child that stays up for `min_uptime` before dying gets its
    /// retry budget back, so a plugin that crashes occasionally after
    /// serving traffic keeps being restarted indefinitely.
    #[tokio::test(flavor = "multi_thread")]
    async fn min_uptime_resets_the_retry_budget() {
        let dir = tempdir().unwrap();
        // Answers one RPC then exits: its uptime is however long we
        // wait before calling it.
        let body = r#"#!/usr/bin/env bash
read -r line
id=$(printf '%s' "$line" | sed -nE 's/.*"id"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p')
printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
exit 0
"#;
        make_script(dir.path(), "oneshot.sh", body);

        let sidecar = Sidecar::spawn_with_policy(
            "budget-reset",
            dir.path(),
            Path::new("./oneshot.sh"),
            RestartPolicy {
                max_retries: 1,
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(10),
                backoff_multiplier: 1.0,
                min_uptime: Duration::from_millis(50),
            },
        )
        .await
        .unwrap();

        // With max_retries = 1 and no reset, the third crash would
        // exhaust the budget. Each child lives > min_uptime before we
        // make it exit, so every crash starts with a fresh budget.
        for round in 0..4 {
            tokio::time::sleep(Duration::from_millis(120)).await;
            let out = sidecar
                .call_with_timeout("ping", Value::Null, Duration::from_secs(5))
                .await
                .unwrap_or_else(|e| panic!("round {round}: {e}"));
            assert_eq!(out["ok"], true);
        }

        sidecar.shutdown().await;
    }

    /// A frame longer than the cap kills the child (in-flight RPC
    /// resolves to `Closed`); the restart policy then brings up a
    /// fresh child that serves normal frames.
    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_frame_drops_the_child_and_respawn_recovers() {
        let dir = tempdir().unwrap();
        let body = r#"#!/usr/bin/env bash
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -nE 's/.*"id"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p')
  method=$(printf '%s' "$line" | sed -nE 's/.*"method"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p')
  if [ "$method" = "huge" ]; then
    printf '{"jsonrpc":"2.0","id":%s,"result":"' "$id"
    head -c 4096 /dev/zero | tr '\0' 'x'
    printf '"}\n'
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;
        make_script(dir.path(), "huge.sh", body);

        let sidecar = Sidecar::spawn_with_options(
            "framecap",
            dir.path(),
            Path::new("./huge.sh"),
            SpawnOptions {
                policy: RestartPolicy {
                    max_retries: 3,
                    initial_backoff: Duration::from_millis(10),
                    max_backoff: Duration::from_millis(10),
                    backoff_multiplier: 1.0,
                    min_uptime: Duration::ZERO,
                },
                max_frame_bytes: 1024,
                ..SpawnOptions::default()
            },
        )
        .await
        .unwrap();

        let err = sidecar
            .call_with_timeout("huge", Value::Null, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Closed), "got {err:?}");

        // Respawned child answers a normal request.
        let mut ok = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Ok(v) = sidecar
                .call_with_timeout("ping", Value::Null, Duration::from_secs(2))
                .await
            {
                ok = Some(v);
                break;
            }
        }
        assert_eq!(ok.expect("child should respawn")["ok"], true);

        sidecar.shutdown().await;
    }

    /// `SpawnOptions::rpc_timeout` bounds `call`.
    #[tokio::test(flavor = "multi_thread")]
    async fn rpc_timeout_is_configurable_per_sidecar() {
        let dir = tempdir().unwrap();
        // Reads a request and never answers it.
        make_script(
            dir.path(),
            "mute.sh",
            "#!/usr/bin/env bash\nwhile IFS= read -r line; do sleep 5; done\n",
        );
        let sidecar = Sidecar::spawn_with_options(
            "mute",
            dir.path(),
            Path::new("./mute.sh"),
            SpawnOptions {
                policy: RestartPolicy::no_restart(),
                rpc_timeout: Duration::from_millis(100),
                ..SpawnOptions::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(sidecar.rpc_timeout(), Duration::from_millis(100));

        let started = std::time::Instant::now();
        let err = sidecar.call("ping", Value::Null).await.unwrap_err();
        assert!(
            matches!(
                err,
                Error::Timeout {
                    timeout_ms: 100,
                    ..
                }
            ),
            "got {err:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));

        sidecar.shutdown().await;
    }

    /// `SpawnOptions::env` is visible inside the child.
    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_env_reaches_the_child() {
        let dir = tempdir().unwrap();
        let body = r#"#!/usr/bin/env bash
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -nE 's/.*"id"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p')
  printf '{"jsonrpc":"2.0","id":%s,"result":{"flag":"%s"}}\n' "$id" "$SMITHS_TEST_FLAG"
done
"#;
        make_script(dir.path(), "env.sh", body);
        let sidecar = Sidecar::spawn_with_options(
            "env",
            dir.path(),
            Path::new("./env.sh"),
            SpawnOptions {
                policy: RestartPolicy::no_restart(),
                env: BTreeMap::from([("SMITHS_TEST_FLAG".to_owned(), "hello".to_owned())]),
                ..SpawnOptions::default()
            },
        )
        .await
        .unwrap();
        let out = sidecar.call("check", Value::Null).await.unwrap();
        assert_eq!(out["flag"], "hello");
        sidecar.shutdown().await;
    }
}
