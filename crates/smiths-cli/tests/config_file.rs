//! A missing or mistyped `--config` must never boot the engine on
//! the defaults (`0.0.0.0:5060`, no auth), and `validate` must not
//! print `ok` for a file that does not exist.

use std::process::Command;

use tempfile::tempdir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_smiths-net")
}

#[test]
fn validate_missing_config_exits_1_and_names_the_file() {
    let dir = tempdir().expect("tempdir");
    let missing = dir.path().join("typo.toml");
    let out = Command::new(bin())
        .arg("--config")
        .arg(&missing)
        .arg("validate")
        .output()
        .expect("spawn binary");
    assert_eq!(out.status.code(), Some(1), "status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stdout.contains("ok"), "stdout must not say ok: {stdout}");
    assert!(stderr.contains("config file not found"), "stderr: {stderr}");
    assert!(stderr.contains("typo.toml"), "stderr: {stderr}");
}

#[test]
fn run_with_missing_config_exits_1_without_binding_anything() {
    let dir = tempdir().expect("tempdir");
    let missing = dir.path().join("typo.toml");
    let out = Command::new(bin())
        .arg("--config")
        .arg(&missing)
        .env("SMITHS_DRAIN_SECS", "0")
        .output()
        .expect("spawn binary");
    assert_eq!(out.status.code(), Some(1), "status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("config file not found"), "stderr: {stderr}");
    assert!(
        !stderr.contains("smiths-net ready"),
        "engine must not start: {stderr}"
    );
}

#[test]
fn validate_rejects_unsupported_toggle_with_exit_2() {
    let dir = tempdir().expect("tempdir");
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, "[sip]\ntransports = [\"udp\", \"quic\"]\n").expect("write config");
    let out = Command::new(bin())
        .arg("--config")
        .arg(&cfg)
        .arg("validate")
        .output()
        .expect("spawn binary");
    assert_eq!(out.status.code(), Some(2), "status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("sip.transports"), "stderr: {stderr}");
}

#[test]
fn validate_accepts_the_shipped_example_config() {
    let root = env!("CARGO_MANIFEST_DIR");
    let example = std::path::Path::new(root).join("../../examples/config.toml");
    let out = Command::new(bin())
        .arg("--config")
        .arg(&example)
        .arg("validate")
        .output()
        .expect("spawn binary");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains(": ok"));
}

#[test]
fn reload_dry_run_refuses_removed_canary_secs_flag() {
    let dir = tempdir().expect("tempdir");
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, "[sip]\ndrain_timeout_secs = 3\n").expect("write config");
    let out = Command::new(bin())
        .arg("--config")
        .arg(&cfg)
        .arg("reload")
        .arg("--dry-run")
        .arg("--canary-secs")
        .arg("30")
        .output()
        .expect("spawn binary");
    assert!(!out.status.success(), "flag must be rejected by clap");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("canary-secs"), "stderr: {stderr}");
}
