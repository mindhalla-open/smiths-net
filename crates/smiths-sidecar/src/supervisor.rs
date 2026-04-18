//! Subprocess supervisor for one sidecar plugin.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, info, instrument, warn};

use crate::error::Error;
use crate::rpc::{RpcRequest, RpcResponse};

/// Default RPC timeout — conservative because AI models can be slow.
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>>;

/// One running sidecar plugin.
///
/// Cheap to clone through `Arc` — the inner state is reference-counted
/// so multiple callers can invoke RPCs concurrently.
#[derive(Debug)]
pub struct Sidecar {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    name: String,
    next_id: AtomicU64,
    pending: Pending,
    stdin: Mutex<tokio::process::ChildStdin>,
    // Keep the reader/stderr tasks alive as long as Inner is alive.
    _reader: JoinHandle<()>,
    _stderr: JoinHandle<()>,
    child: Mutex<Option<Child>>,
}

impl Sidecar {
    /// Spawn a plugin executable from `plugin_dir`, piping stdin/stdout
    /// as the JSON-RPC wire. `name` is used for logs only.
    ///
    /// `async` is kept even though the body doesn't currently await
    /// on the hot path — the signature is contractual and future
    /// readiness checks (health ping, descriptor fetch) will await.
    #[allow(clippy::unused_async)]
    #[instrument(skip_all, fields(dir = %plugin_dir.display(), entry = %entry.display()))]
    pub async fn spawn(
        name: impl Into<String>,
        plugin_dir: &Path,
        entry: &Path,
    ) -> Result<Self, Error> {
        let name: String = name.into();
        // Canonicalize to an absolute path. We also `chdir` the child
        // into plugin_dir, so a relative entry with embedded slashes
        // (e.g. `./main.py`) would fail exec — exec resolves program
        // path *after* `chdir`.
        let raw_entry: PathBuf = if entry.is_absolute() {
            entry.to_path_buf()
        } else {
            plugin_dir.join(entry)
        };
        let entry_abs = std::fs::canonicalize(&raw_entry).map_err(|e| {
            warn!(plugin = %name, entry = %raw_entry.display(), ?e, "entry canonicalize failed");
            Error::Io(e)
        })?;
        let plugin_dir_abs =
            std::fs::canonicalize(plugin_dir).unwrap_or_else(|_| plugin_dir.to_path_buf());
        let mut cmd = Command::new(&entry_abs);
        cmd.current_dir(&plugin_dir_abs)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            warn!(plugin = %name, entry = %entry_abs.display(), ?e, "sidecar spawn failed");
            Error::Io(e)
        })?;
        info!(plugin = %name, pid = ?child.id(), entry = %entry_abs.display(), "sidecar spawned");

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

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader = spawn_stdout_reader(stdout, Arc::clone(&pending), name.clone());
        let stderr_task = spawn_stderr_forwarder(stderr, name.clone());

        Ok(Self {
            inner: Arc::new(Inner {
                name,
                next_id: AtomicU64::new(1),
                pending,
                stdin: Mutex::new(stdin),
                _reader: reader,
                _stderr: stderr_task,
                child: Mutex::new(Some(child)),
            }),
        })
    }

    /// Plugin name (for diagnostics).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Send a JSON-RPC request and await the correlated response.
    ///
    /// `params` may be any serializable JSON; `Value::Null` for
    /// methods that take no arguments.
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

        // Write the request as one line.
        let frame = RpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let mut buf = serde_json::to_vec(&frame)?;
        buf.push(b'\n');
        {
            let mut stdin = self.inner.stdin.lock().await;
            stdin.write_all(&buf).await?;
            stdin.flush().await?;
        }

        // Await the correlated response (or time out).
        let resp = match timeout(deadline, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => {
                // Sender dropped — plugin died before replying.
                self.inner.pending.lock().await.remove(&id);
                return Err(Error::Closed);
            }
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                return Err(Error::Timeout {
                    method: method.to_owned(),
                    timeout_ms: u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
                });
            }
        };

        if let Some(err) = resp.error {
            return Err(Error::Plugin {
                code: err.code,
                message: err.message,
            });
        }
        Ok(resp.result.unwrap_or(Value::Null))
    }

    /// Request the plugin exit gracefully. Cooperates with kill-on-drop
    /// — if the plugin hangs, the `Child` is killed when `self` is
    /// dropped.
    pub async fn shutdown(&self) {
        // Best-effort "shutdown" notification. Plugins that respect it
        // get a clean exit window; others are killed on drop.
        let _ = self
            .call_with_timeout("shutdown", Value::Null, Duration::from_millis(500))
            .await;
        if let Some(mut child) = self.inner.child.lock().await.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

impl Clone for Sidecar {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Background task: drain `stdout` as newline-delimited JSON-RPC
/// frames and route each response to its pending request by id.
fn spawn_stdout_reader(
    stdout: tokio::process::ChildStdout,
    pending: Pending,
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
                    dispatch_frame(trimmed, &pending, &name).await;
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

/// Parse one wire frame and forward it to the correlating request,
/// or treat it as a notification (not used in this slice).
async fn dispatch_frame(frame: &str, pending: &Pending, name: &str) {
    match serde_json::from_str::<RpcResponse>(frame) {
        Ok(resp) => {
            if let Some(id) = resp.id {
                let tx = pending.lock().await.remove(&id);
                if let Some(tx) = tx {
                    let _ = tx.send(resp);
                } else {
                    debug!(plugin = %name, id, "response to unknown id");
                }
            } else {
                debug!(
                    plugin = %name,
                    method = ?resp.method,
                    "plugin notification (ignored in MVP)"
                );
            }
        }
        Err(e) => {
            warn!(plugin = %name, ?e, raw = %frame, "bad frame");
        }
    }
}

/// Background task: tag plugin stderr lines with the plugin name and
/// re-emit them into the engine's tracing log.
fn spawn_stderr_forwarder(stderr: tokio::process::ChildStderr, name: String) -> JoinHandle<()> {
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

    /// Shell-script stub that implements a trivial JSON-RPC echo server
    /// over stdio — just enough for the RPC layer round-trip test
    /// without needing Python or a compiled binary.
    const ECHO_SCRIPT: &str = r#"#!/usr/bin/env bash
# Reads JSON-RPC requests line by line and echoes back results with the
# same id. No real dispatching — returns {"ok": true, "method": ...}.
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -nE 's/.*"id"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p')
  method=$(printf '%s' "$line" | sed -nE 's/.*"method"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p')
  printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true,"method":"%s"}}\n' "$id" "$method"
done
"#;

    #[tokio::test(flavor = "multi_thread")]
    async fn rpc_round_trip_through_echo_script() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("echo.sh");
        fs::write(&script, ECHO_SCRIPT).unwrap();
        let mut p = fs::metadata(&script).unwrap().permissions();
        p.set_mode(0o755);
        fs::set_permissions(&script, p).unwrap();

        let sidecar = Sidecar::spawn("echo-test", dir.path(), Path::new("./echo.sh"))
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
}
