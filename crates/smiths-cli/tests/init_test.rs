//! Tests for `smiths-net init` (slice 7.1).

use tempfile::TempDir;

/// Run `smiths-net init` as a subprocess with the given args.
fn run_init(args: &[&str], dir: &TempDir) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_smiths-net");
    std::process::Command::new(bin)
        .args(["init"])
        .args(args)
        .current_dir(dir.path())
        .output()
        .expect("failed to execute smiths-net init")
}

#[test]
fn non_interactive_dev_preset_produces_valid_config() {
    let dir = TempDir::new().unwrap();
    let output_path = dir.path().join("dev.toml");
    let out = run_init(
        &[
            "--non-interactive",
            "--preset",
            "dev",
            "--output",
            output_path.to_str().unwrap(),
        ],
        &dir,
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "init --preset dev failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(output_path.exists(), "config file should be created");

    // Round-trip: parse the generated TOML.
    let content = std::fs::read_to_string(&output_path).unwrap();
    let _parsed: toml::Value = toml::from_str(&content).expect("generated TOML should parse");

    // Verify dev defaults.
    assert!(
        content.contains("debug"),
        "dev preset should set log_level = debug"
    );
    assert!(
        content.contains("pretty") || content.contains("Pretty"),
        "dev preset should set log_format = pretty"
    );
}

#[test]
fn non_interactive_prod_preset_produces_valid_config() {
    let dir = TempDir::new().unwrap();
    let output_path = dir.path().join("prod.toml");
    let out = run_init(
        &[
            "--non-interactive",
            "--preset",
            "prod",
            "--output",
            output_path.to_str().unwrap(),
        ],
        &dir,
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "init --preset prod failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(output_path.exists(), "config file should be created");

    let content = std::fs::read_to_string(&output_path).unwrap();
    let _parsed: toml::Value = toml::from_str(&content).expect("generated TOML should parse");

    // Verify prod defaults.
    assert!(
        content.contains("no_new_privs"),
        "prod preset should set no_new_privs"
    );
    assert!(
        content.contains("allowlist") || content.contains("Allowlist"),
        "prod preset should enable seccomp allowlist"
    );
}

#[test]
fn refuses_to_overwrite_without_force() {
    let dir = TempDir::new().unwrap();
    let output_path = dir.path().join("config.toml");

    // Create a file at the output path.
    std::fs::write(&output_path, "# existing").unwrap();

    let out = run_init(
        &[
            "--non-interactive",
            "--output",
            output_path.to_str().unwrap(),
        ],
        &dir,
    );

    assert!(
        !out.status.success(),
        "should fail when output already exists"
    );

    // With --force it should succeed.
    let out2 = run_init(
        &[
            "--non-interactive",
            "--force",
            "--output",
            output_path.to_str().unwrap(),
        ],
        &dir,
    );
    assert!(out2.status.success(), "should succeed with --force");
}

#[test]
fn default_non_interactive_without_preset_uses_dev() {
    let dir = TempDir::new().unwrap();
    let output_path = dir.path().join("default.toml");
    let out = run_init(
        &[
            "--non-interactive",
            "--output",
            output_path.to_str().unwrap(),
        ],
        &dir,
    );
    assert!(out.status.success());
    let content = std::fs::read_to_string(&output_path).unwrap();
    assert!(
        content.contains("debug"),
        "default non-interactive should use dev preset (debug level)"
    );
}
