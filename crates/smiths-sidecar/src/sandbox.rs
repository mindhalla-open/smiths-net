//! Per-sidecar resource sandbox applied at subprocess spawn time.
//!
//! The sandbox is split in two halves around `fork`:
//!
//! 1. [`prepare`] runs in the **parent** and turns a
//!    [`SandboxConfig`] into a [`Plan`]: the rlimit table, the
//!    `no_new_privs` flag, and (Linux) the compiled seccomp-BPF
//!    program. Everything that allocates or can fail with a
//!    descriptive error happens here.
//! 2. [`apply_in_child`] runs inside the forked child from
//!    [`std::os::unix::process::CommandExt::pre_exec`], immediately
//!    before `execve`. Every call it makes **must** be
//!    async-signal-safe: it only issues syscalls through `rustix`
//!    (pure-Rust wrappers, no allocator touches) and `seccompiler`'s
//!    installer (a `prctl` + a `seccomp` syscall on a stack-built
//!    `sock_fprog`), and every error path returns an `io::Error`
//!    built from an errno or an `ErrorKind` — never a formatted
//!    string.
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
//! - Linux only: seccomp-BPF allowlist — curated syscall set plus
//!   per-plugin extras; everything else returns `EPERM`.
//!
//! ## What it deliberately does not do
//!
//! User-namespace isolation and cgroup integration are intentionally
//! out of scope — their correctness is tightly bound to the
//! deployment topology (systemd vs. kubernetes vs. bare metal).

use std::io;

use smiths_core::SandboxConfig;
#[cfg(target_os = "linux")]
use smiths_core::SeccompPolicy;

#[cfg(target_os = "linux")]
mod seccomp_filter;

/// Number of rlimit slots a [`Plan`] carries.
#[cfg(unix)]
const RLIMIT_SLOTS: usize = 4;

/// Sandbox actions precomputed in the parent so the child's
/// `pre_exec` closure is allocation-free. Cloned once per spawn.
#[derive(Clone, Debug)]
pub struct Plan {
    /// `(resource, cap)` pairs; `None` caps are skipped.
    #[cfg(unix)]
    limits: [(rustix::process::Resource, Option<u64>); RLIMIT_SLOTS],
    /// Apply `PR_SET_NO_NEW_PRIVS` (Linux; no-op elsewhere).
    #[cfg(unix)]
    no_new_privs: bool,
    /// Compiled seccomp-BPF program, `None` when the policy is `Off`.
    #[cfg(target_os = "linux")]
    seccomp: Option<seccompiler::BpfProgram>,
}

/// Build the [`Plan`] for `cfg`. Runs in the parent; may allocate.
///
/// # Errors
///
/// Returns [`io::Error`] when the seccomp allowlist cannot be
/// compiled (unknown syscall in `seccomp_extra_allow`, backend
/// failure). Refusing to spawn is preferable to running the plugin
/// with a weaker sandbox than the operator asked for.
#[cfg(unix)]
pub fn prepare(cfg: &SandboxConfig) -> io::Result<Plan> {
    use rustix::process::Resource;
    Ok(Plan {
        limits: [
            (Resource::Nofile, cfg.max_fds),
            (Resource::As, cfg.max_memory_bytes),
            (Resource::Cpu, cfg.max_cpu_seconds),
            // Not all Unix systems expose RLIMIT_NPROC; rustix defines
            // it where the kernel does (Linux + *BSD + macOS).
            (Resource::Nproc, cfg.max_processes),
        ],
        no_new_privs: cfg.no_new_privs,
        #[cfg(target_os = "linux")]
        seccomp: compile_seccomp(cfg)?,
    })
}

/// Non-Unix targets don't have `pre_exec`, so the plan is empty.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // mirrors the Unix signature so call sites need no cfg
pub fn prepare(_cfg: &SandboxConfig) -> io::Result<Plan> {
    Ok(Plan {})
}

/// Apply `plan` to the current process (the forked sidecar child).
///
/// Runs from inside `pre_exec`. Allocation-free: rlimits and
/// `no_new_privs` go through `rustix`, the seccomp program was
/// compiled by [`prepare`] and is only *installed* here. Any failure
/// short-circuits; the kernel then discards the child (the parent
/// sees a spawn error) rather than running a plugin with a weaker
/// sandbox than the operator requested.
///
/// # Errors
///
/// Returns the errno of the failing syscall as an [`io::Error`].
#[cfg(unix)]
pub fn apply_in_child(plan: &Plan) -> io::Result<()> {
    for (resource, limit) in plan.limits {
        let Some(value) = limit else {
            continue;
        };
        let rlim = rustix::process::Rlimit {
            current: Some(value),
            maximum: Some(value),
        };
        rustix::process::setrlimit(resource, rlim)?;
    }

    if plan.no_new_privs {
        apply_no_new_privs()?;
    }

    // Seccomp-BPF must land AFTER rlimit / no_new_privs — a filter
    // denying `prctl` or `setrlimit` would otherwise break its own
    // setup. It is also the one step whose installer we don't
    // control byte-for-byte, so it goes last: by then every other
    // sandbox action has already taken effect.
    install_seccomp(plan)
}

/// Non-Unix targets never call `pre_exec`; kept so callers don't
/// need `cfg` gates around the attach point.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // mirrors the Unix signature so call sites need no cfg
pub fn apply_in_child(_plan: &Plan) -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn compile_seccomp(cfg: &SandboxConfig) -> io::Result<Option<seccompiler::BpfProgram>> {
    match cfg.seccomp {
        SeccompPolicy::Off => Ok(None),
        SeccompPolicy::Allowlist => seccomp_filter::compile_allowlist(&cfg.seccomp_extra_allow)
            .map(Some)
            .map_err(|e| io::Error::other(format!("seccomp compile: {e}"))),
    }
}

/// Install the precompiled program. `seccompiler::apply_filter` does
/// one `prctl` and one `seccomp` syscall on a stack-allocated
/// `sock_fprog`; its error variants carry the raw errno, which we
/// pass through unchanged so no string is built in the child.
#[cfg(target_os = "linux")]
fn install_seccomp(plan: &Plan) -> io::Result<()> {
    let Some(program) = plan.seccomp.as_ref() else {
        return Ok(());
    };
    match seccompiler::apply_filter(program) {
        Ok(()) => Ok(()),
        Err(seccompiler::Error::Prctl(e) | seccompiler::Error::Seccomp(e)) => Err(e),
        // `EmptyFilter` is ruled out by `prepare` (the baseline is
        // never empty); the remaining variants are not produced by
        // `apply_filter`.
        Err(_) => Err(io::Error::from(io::ErrorKind::Unsupported)),
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
#[allow(clippy::unnecessary_wraps)] // mirrors the Linux signature so `apply_in_child` needs no cfg
fn install_seccomp(_plan: &Plan) -> io::Result<()> {
    // Seccomp is a Linux-kernel feature. BSD `pledge` / macOS
    // `sandbox_exec` would be the comparable primitives but their
    // semantics don't map cleanly to our allowlist shape — the
    // config knob is silently ignored rather than surface as an
    // error.
    Ok(())
}

#[cfg(target_os = "linux")]
fn apply_no_new_privs() -> io::Result<()> {
    rustix::thread::set_no_new_privs(true)?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
#[allow(clippy::unnecessary_wraps)] // mirrors the Linux signature so `apply_in_child` needs no cfg
fn apply_no_new_privs() -> io::Result<()> {
    // PR_SET_NO_NEW_PRIVS is a Linux prctl. macOS / BSDs have their
    // own equivalents (sandbox-exec, pledge/unveil) that don't fit
    // this closure's async-signal-safe constraint — documented on
    // the config field and skipped here.
    Ok(())
}
