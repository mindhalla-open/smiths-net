//! Full-binary smoke test for the MCP stdio transport.
//!
//! Spawns the real `smiths-net` binary with `--mcp stdio`, writes
//! line-delimited JSON-RPC to stdin, reads line-delimited replies from
//! stdout, and verifies the protocol-level contract:
//!
//! - `initialize` returns our server name + protocol version + the
//!   tools / resources capability block.
//! - `tools/list` enumerates the shipped tool set (canonical names
//!   present).
//! - `tools/call` for `health` returns `{"status":"ok"}` plus an
//!   `uptime_secs` number.
//! - Closing stdin causes the process to exit cleanly (CLI treats
//!   stdin EOF as shutdown).
//!
//! Unix-only: the existing `e2e.rs` takes the same stance, and
//! `tokio::process::Child` piping behaves identically on both, but the
//! exit-on-stdin-close path relies on POSIX SIGPIPE semantics. Windows
//! support is straightforward if we ever need it.
//!
//! Covers the last remaining Phase 5 pending item (`docs/plans/todo.md`):
//! integration tests that spawn the real binary over MCP stdio.

#![cfg(unix)]
// Single-function process-level smoke test — splitting the happy path
// into per-step helpers hurts readability more than the length cap
// helps here.
#![allow(clippy::too_many_lines)]

use std::net::TcpListener;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::{Instant, timeout};

/// Briefly bind a port to reserve it, then drop. Tiny race between the
/// drop and the binary's bind, same as `e2e.rs`.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve port")
        .local_addr()
        .expect("local_addr")
        .port()
}

fn write_config(path: &std::path::Path, health: u16, sip: u16) {
    // Same minimal shape as `e2e.rs`. We only need a valid config so
    // `Config::load` doesn't error; the assertions all happen over MCP
    // stdio, not over SIP.
    let toml = format!(
        "[observability]\n\
         log_format = \"json\"\n\
         log_level = \"warn\"\n\
         health_bind = \"127.0.0.1:{health}\"\n\
         \n\
         [sip]\n\
         bind = [\"127.0.0.1:{sip}\"]\n\
         transports = [\"udp\"]\n\
         drain_timeout_secs = 0\n"
    );
    std::fs::write(path, toml).expect("write config");
}

/// Await one JSON-RPC reply carrying `matching_id`. Notifications
/// (method-only frames without `id`) are skipped — the engine pushes
/// `notifications/call/*` / `notifications/plugin/*` on the same wire.
///
/// Panics (via `expect`) if no matching reply lands within the deadline.
async fn recv_reply(
    out: &mut BufReader<tokio::process::ChildStdout>,
    matching_id: i64,
    deadline: Instant,
) -> Value {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for JSON-RPC id={matching_id}"
        );
        let mut line = String::new();
        let n = timeout(remaining, out.read_line(&mut line))
            .await
            .expect("timed out on stdout")
            .expect("stdout closed before reply");
        assert!(n != 0, "stdout EOF before JSON-RPC id={matching_id}");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(trimmed)
            .unwrap_or_else(|e| panic!("non-JSON line on MCP stdio: `{trimmed}` ({e})"));
        // Notifications carry no `id`; skip them.
        let Some(id) = value.get("id").and_then(Value::as_i64) else {
            continue;
        };
        if id == matching_id {
            return value;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_stdio_initialize_list_call_over_spawned_binary() {
    let bin = env!("CARGO_BIN_EXE_smiths-net");

    let dir = tempdir().expect("tempdir");
    let cfg = dir.path().join("config.toml");
    write_config(&cfg, free_port(), free_port());

    let mut child = Command::new(bin)
        .arg("--config")
        .arg(&cfg)
        .arg("--mcp")
        .arg("stdio")
        .env("RUST_LOG", "warn")
        // No graceful-drain pause — the test shuts down promptly.
        .env("SMITHS_DRAIN_SECS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn binary");

    let mut stdin = child.stdin.take().expect("stdin pipe");
    let stdout = child.stdout.take().expect("stdout pipe");
    let mut out = BufReader::new(stdout);

    let deadline = Instant::now() + Duration::from_secs(10);

    // ---- 1. initialize ----------------------------------------------
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    });
    stdin
        .write_all(format!("{init}\n").as_bytes())
        .await
        .expect("write initialize");
    stdin.flush().await.expect("flush initialize");

    let resp = recv_reply(&mut out, 1, deadline).await;
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(
        resp["result"]["serverInfo"]["name"], "smiths-net",
        "unexpected initialize body: {resp}"
    );
    // Protocol version is a non-empty string.
    assert!(
        resp["result"]["protocolVersion"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "missing protocolVersion: {resp}"
    );
    // Tools + resources capabilities are advertised.
    assert!(resp["result"]["capabilities"]["tools"].is_object());
    assert!(resp["result"]["capabilities"]["resources"].is_object());

    // ---- 2. tools/list ----------------------------------------------
    let list = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    stdin
        .write_all(format!("{list}\n").as_bytes())
        .await
        .expect("write tools/list");
    stdin.flush().await.expect("flush tools/list");

    let resp = recv_reply(&mut out, 2, deadline).await;
    let tools = resp["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list missing array: {resp}"));
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for required in [
        "list_calls",
        "get_call_status",
        "health",
        "make_call",
        "end_call",
    ] {
        assert!(
            names.contains(&required),
            "tools/list missing `{required}`; got: {names:?}"
        );
    }

    // ---- 3. tools/call health ---------------------------------------
    let call = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "health",
            "arguments": {}
        }
    });
    stdin
        .write_all(format!("{call}\n").as_bytes())
        .await
        .expect("write tools/call");
    stdin.flush().await.expect("flush tools/call");

    let resp = recv_reply(&mut out, 3, deadline).await;
    // The MCP tools/call wrapper embeds the tool's Value under
    // `result.content[0].text` as a JSON string. Parse it back out and
    // assert on the structured payload.
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tools/call missing content[0].text: {resp}"));
    let payload: Value = serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("tools/call text not JSON: {text} ({e})"));
    assert_eq!(payload["status"], "ok", "health result: {payload}");
    assert!(
        payload["uptime_secs"].is_number(),
        "uptime_secs missing: {payload}"
    );

    // ---- 4. shutdown via stdin close --------------------------------
    drop(stdin);

    let exit = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("engine did not exit after stdin close")
        .expect("wait child");
    assert!(exit.success(), "engine exited with {exit}");
}
