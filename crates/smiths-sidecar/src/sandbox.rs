//! Per-sidecar resource sandbox applied at subprocess spawn time.
//!
//! Converts a [`SandboxConfig`] into a closure suitable for
//! [`std::os::unix::process::CommandExt::pre_exec`]. The closure runs
//! inside the forked child immediately before `execve`; every call it
//! makes **must** be async-signal-safe. That's why we use `rustix`
//! (pure-Rust syscall wrappers, no allocator touches) rather than
//! libc via `unsafe` blocks, and why every error path here returns
//! `io::Error` without formatting heap strings.
//!
//! ## What the sandbox does today
//!
//! - `RLIMIT_NOFILE` — cap the plugin's open FD count.
//! - `RLIMIT_AS` — cap virtual memory in bytes.
//! - `RLIMIT_CPU` — cap soft CPU-time in seconds (`SIGXCPU` on
//!   overrun).
//! - `RLIMIT_NPROC` — cap additional processes (set to 0 to forbid
//!   `fork`/`exec` entirely).
//! - Linux only: `prctl(PR_SET_NO_NEW_PRIVS, 1)` — block privilege
//!   escalation via setuid binaries or file capabilities.
//!
//! ## What it deliberately does not do
//!
//! Full seccomp-BPF syscall filtering and user-namespace isolation
//! are intentionally out of this slice. They're significantly more
//! LOC and their correctness is tightly coupled to the plugin's
//! runtime (tokio, Python, etc.) — easy to lock the child out of
//! syscalls it genuinely needs. Item 8 on the roadmap (MVP sandbox)
//! is satisfied by the rlimit + `no_new_privs` surface above; the
//! hardened-seccomp follow-on is tracked separately.

use std::io;

use smiths_core::SandboxConfig;

/// Apply `cfg` to the current process (the forked sidecar child).
///
/// This runs from inside `pre_exec`'s closure. `rustix` wraps each
/// syscall safely and does not allocate, which is what the
/// async-signal-safe constraint demands. Any failure short-circuits;
/// the kernel then discards the child (the parent sees a spawn
/// error) rather than running a plugin with a weaker sandbox than
/// the operator requested.
///
/// # Errors
///
/// Returns [`io::Error`] when any configured syscall fails. The
/// error is mapped from `rustix::io::Errno` via its `io::Error`
/// `From` impl.
#[cfg(unix)]
pub fn apply_in_child(cfg: &SandboxConfig) -> io::Result<()> {
    use rustix::process::Resource;

    let limits = [
        (Resource::Nofile, cfg.max_fds),
        (Resource::As, cfg.max_memory_bytes),
        (Resource::Cpu, cfg.max_cpu_seconds),
        // Not all Unix systems expose RLIMIT_NPROC; rustix defines
        // it where the kernel does (Linux + *BSD + macOS). If your
        // target is one that doesn't, remove this pair via feature
        // gates.
        (Resource::Nproc, cfg.max_processes),
    ];

    for (resource, limit) in limits {
        let Some(value) = limit else {
            continue;
        };
        let rlim = rustix::process::Rlimit {
            current: Some(value),
            maximum: Some(value),
        };
        rustix::process::setrlimit(resource, rlim)?;
    }

    if cfg.no_new_privs {
        // Linux-only. Other Unix systems silently skip.
        apply_no_new_privs()?;
    }

    Ok(())
}

/// Non-Unix targets don't have `pre_exec` in the first place, so the
/// closure is never called. We still compile the helper as a
/// no-op stub so callers don't need `cfg` gates around the
/// `.pre_exec(...)` attach point.
#[cfg(not(unix))]
pub fn apply_in_child(_cfg: &SandboxConfig) -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn apply_no_new_privs() -> io::Result<()> {
    rustix::thread::set_no_new_privs(true)?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
// Signature mirrors the Linux impl so `apply_in_child` can `?` it
// without a cfg-gated call site. Clippy's "unnecessary Result" fires
// here because the body always returns `Ok` — that's the point.
#[allow(clippy::unnecessary_wraps)]
fn apply_no_new_privs() -> io::Result<()> {
    // PR_SET_NO_NEW_PRIVS is a Linux prctl. macOS / BSDs have their
    // own equivalents (sandbox-exec, pledge/unveil) that don't fit
    // this closure's async-signal-safe constraint — document via
    // the config field's docstring and move on.
    Ok(())
}
