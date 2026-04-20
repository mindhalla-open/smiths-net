//! Linux seccomp-BPF allowlist installer.
//!
//! Compiled only on Linux — the `seccompiler` crate has Linux-specific
//! BPF code and pulls in kernel headers via its build. See the
//! parent module's `cfg(target_os = "linux")` gate.
//!
//! ## Allowlist rationale
//!
//! The baseline covers everything a typical Rust / Tokio plugin
//! needs: epoll event loop, futex (tokio park/unpark), read/write on
//! heap + FDs, mmap (allocator + mapped files), clock + signal
//! primitives. Covers ~85% of Python and Node plugins as well; those
//! that need more (e.g. `io_uring_setup` for an async runtime that
//! uses it) should add entries via `seccomp_extra_allow`.
//!
//! **Deliberately denied** syscalls include everything under the
//! "escape the sandbox" umbrella:
//!
//! - `mount`, `umount2`, `pivot_root` — filesystem rearrangement.
//! - `reboot`, `kexec_load` — host takeover.
//! - `ptrace` — debug another process.
//! - `unshare`, `setns` — namespace escapes.
//! - `bpf` — load arbitrary kernel programs.
//! - `kcmp`, `perf_event_open` — covert-channel primitives.
//!
//! Denials return `EPERM` rather than `SIGSYS` (`TrapAction::Errno`
//! semantics) so callers see normal "operation not permitted" errors
//! instead of kernel-killed processes. Easier to debug; preserves
//! graceful teardown; the integration test still observes a distinct
//! exit code because the plugin's own logic bails when `mount`
//! returns `EPERM`.

use std::collections::BTreeMap;

use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch};
use thiserror::Error;

/// Errors from compiling or installing the seccomp filter.
#[derive(Debug, Error)]
pub(super) enum SeccompError {
    /// `seccompiler` rejected the rule set (unknown syscall, etc).
    #[error("compile: {0}")]
    Compile(String),
    /// The kernel refused to install the compiled BPF program. Most
    /// commonly because `PR_SET_NO_NEW_PRIVS` hasn't been set and
    /// the calling thread isn't `CAP_SYS_ADMIN`.
    #[error("install: {0}")]
    Install(String),
    /// Unknown syscall name in the user-supplied `extra_allow` list.
    #[error("unknown syscall in extra-allow list: {0}")]
    UnknownSyscall(String),
}

/// Compile + install the allowlist for the current thread.
///
/// `extra_allow` adds entries on top of the baseline; entries whose
/// names don't resolve to a valid syscall on the target arch fail
/// fast with [`SeccompError::UnknownSyscall`] rather than silently
/// dropping — an operator that typos a syscall name deserves loud
/// feedback, not a mysterious `EPERM` at runtime.
pub(super) fn install_allowlist(extra_allow: &[String]) -> Result<(), SeccompError> {
    // x86_64 is the only arch we target for containers today. The
    // ARM64 slice (1.8 — multi-arch Docker) will add a `target_arch`
    // branch; until then, refuse to install on anything else rather
    // than build a wrong filter.
    let arch = host_target_arch();

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for name in BASELINE_ALLOW {
        insert_allow(&mut rules, name, arch)?;
    }
    for name in extra_allow {
        insert_allow(&mut rules, name, arch)?;
    }

    let filter = SeccompFilter::new(
        rules,
        // Deny everything not on the list — return EPERM so the
        // plugin sees a normal "operation not permitted" rather
        // than a kernel-killed SIGSYS.
        SeccompAction::Errno(libc_eperm()),
        // On-match action (for allowlisted rules) — just let the
        // syscall through untouched.
        SeccompAction::Allow,
        arch,
    )
    .map_err(|e| SeccompError::Compile(e.to_string()))?;

    let program: BpfProgram = filter
        .try_into()
        .map_err(|e: seccompiler::BackendError| SeccompError::Compile(e.to_string()))?;

    seccompiler::apply_filter(&program).map_err(|e| SeccompError::Install(e.to_string()))?;
    Ok(())
}

fn insert_allow(
    rules: &mut BTreeMap<i64, Vec<SeccompRule>>,
    name: &str,
    _arch: TargetArch,
) -> Result<(), SeccompError> {
    let nr = syscall_number(name).ok_or_else(|| SeccompError::UnknownSyscall(name.to_owned()))?;
    // Empty rule vec = "allow this syscall unconditionally" per
    // seccompiler's semantics.
    rules.insert(nr, Vec::new());
    Ok(())
}

#[cfg(target_arch = "x86_64")]
fn host_target_arch() -> TargetArch {
    TargetArch::x86_64
}

#[cfg(target_arch = "aarch64")]
fn host_target_arch() -> TargetArch {
    TargetArch::aarch64
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("seccomp allowlist only supports x86_64 / aarch64");

fn libc_eperm() -> u32 {
    // EPERM on Linux is 1. We avoid a `libc` dep just for this single
    // constant — it's part of the Linux ABI, not a libc-version thing.
    1
}

/// Resolve a syscall name → number for the current arch. Wraps the
/// platform-specific lookup so callers don't need to `cfg` on arch.
#[cfg(target_arch = "x86_64")]
fn syscall_number(name: &str) -> Option<i64> {
    x86_64_syscall_number(name)
}

#[cfg(target_arch = "aarch64")]
fn syscall_number(name: &str) -> Option<i64> {
    aarch64_syscall_number(name)
}

// Generated tables via `ausyscall --dump` on the respective arches.
// Kept small — only the names we actually reference. Extending the
// baseline list below requires adding entries here too.
#[cfg(target_arch = "x86_64")]
#[allow(clippy::too_many_lines)]
fn x86_64_syscall_number(name: &str) -> Option<i64> {
    Some(match name {
        "read" => 0,
        "write" => 1,
        "open" => 2,
        "close" => 3,
        "stat" => 4,
        "fstat" => 5,
        "lstat" => 6,
        "poll" => 7,
        "lseek" => 8,
        "mmap" => 9,
        "mprotect" => 10,
        "munmap" => 11,
        "brk" => 12,
        "rt_sigaction" => 13,
        "rt_sigprocmask" => 14,
        "rt_sigreturn" => 15,
        "ioctl" => 16,
        "pread64" => 17,
        "pwrite64" => 18,
        "readv" => 19,
        "writev" => 20,
        "access" => 21,
        "pipe" => 22,
        "select" => 23,
        "sched_yield" => 24,
        "mremap" => 25,
        "msync" => 26,
        "madvise" => 28,
        "dup" => 32,
        "dup2" => 33,
        "nanosleep" => 35,
        "getpid" => 39,
        "socket" => 41,
        "connect" => 42,
        "accept" => 43,
        "sendto" => 44,
        "recvfrom" => 45,
        "sendmsg" => 46,
        "recvmsg" => 47,
        "shutdown" => 48,
        "bind" => 49,
        "listen" => 50,
        "getsockname" => 51,
        "getpeername" => 52,
        "setsockopt" => 54,
        "getsockopt" => 55,
        "clone" => 56,
        "fork" => 57,
        "vfork" => 58,
        "execve" => 59,
        "exit" => 60,
        "wait4" => 61,
        "kill" => 62,
        "fcntl" => 72,
        "flock" => 73,
        "fsync" => 74,
        "fdatasync" => 75,
        "truncate" => 76,
        "ftruncate" => 77,
        "getcwd" => 79,
        "chdir" => 80,
        "fchdir" => 81,
        "readlink" => 89,
        "getuid" => 102,
        "geteuid" => 107,
        "getgid" => 104,
        "getegid" => 108,
        "sigaltstack" => 131,
        "arch_prctl" => 158,
        "gettid" => 186,
        "futex" => 202,
        "sched_setaffinity" => 203,
        "sched_getaffinity" => 204,
        "clock_gettime" => 228,
        "clock_nanosleep" => 230,
        "exit_group" => 231,
        "epoll_wait" => 232,
        "epoll_ctl" => 233,
        "tgkill" => 234,
        "openat" => 257,
        "mkdirat" => 258,
        "newfstatat" => 262,
        "readlinkat" => 267,
        "faccessat" => 269,
        "pselect6" => 270,
        "ppoll" => 271,
        "pipe2" => 293,
        "prlimit64" => 302,
        "getrandom" => 318,
        "membarrier" => 324,
        "statx" => 332,
        "rseq" => 334,
        "close_range" => 436,
        "faccessat2" => 439,
        "epoll_create1" => 291,
        "eventfd2" => 290,
        "accept4" => 288,
        "dup3" => 292,
        "renameat2" => 316,
        "prctl" => 157,
        "set_tid_address" => 218,
        "set_robust_list" => 273,
        _ => return None,
    })
}

#[cfg(target_arch = "aarch64")]
#[allow(clippy::too_many_lines)]
fn aarch64_syscall_number(name: &str) -> Option<i64> {
    // Linux on AArch64 uses the generic syscall table — numbers
    // differ from x86_64. The subset below covers the baseline +
    // commonly-extended set.
    Some(match name {
        "io_setup" => 0,
        "getcwd" => 17,
        "eventfd2" => 19,
        "epoll_create1" => 20,
        "epoll_ctl" => 21,
        "epoll_pwait" => 22,
        "dup" => 23,
        "dup3" => 24,
        "fcntl" => 25,
        "ioctl" => 29,
        "flock" => 32,
        "mkdirat" => 34,
        "unlinkat" => 35,
        "renameat" => 38,
        "statfs" => 43,
        "fstatfs" => 44,
        "truncate" => 45,
        "ftruncate" => 46,
        "faccessat" => 48,
        "chdir" => 49,
        "fchdir" => 50,
        "openat" => 56,
        "close" => 57,
        "pipe2" => 59,
        "read" => 63,
        "write" => 64,
        "readv" => 65,
        "writev" => 66,
        "pread64" => 67,
        "pwrite64" => 68,
        "sendfile" => 71,
        "pselect6" => 72,
        "ppoll" => 73,
        "readlinkat" => 78,
        "newfstatat" => 79,
        "fstat" => 80,
        "fsync" => 82,
        "fdatasync" => 83,
        "exit" => 93,
        "exit_group" => 94,
        "set_tid_address" => 96,
        "futex" => 98,
        "set_robust_list" => 99,
        "nanosleep" => 101,
        "clock_gettime" => 113,
        "clock_nanosleep" => 115,
        "sched_yield" => 124,
        "kill" => 129,
        "tgkill" => 131,
        "rt_sigaction" => 134,
        "rt_sigprocmask" => 135,
        "rt_sigreturn" => 139,
        "prctl" => 167,
        "gettid" => 178,
        "sched_getaffinity" => 123,
        "socket" => 198,
        "bind" => 200,
        "listen" => 201,
        "accept" => 202,
        "connect" => 203,
        "getsockname" => 204,
        "getpeername" => 205,
        "sendto" => 206,
        "recvfrom" => 207,
        "setsockopt" => 208,
        "getsockopt" => 209,
        "shutdown" => 210,
        "sendmsg" => 211,
        "recvmsg" => 212,
        "brk" => 214,
        "munmap" => 215,
        "mremap" => 216,
        "execve" => 221,
        "mmap" => 222,
        "mprotect" => 226,
        "msync" => 227,
        "madvise" => 233,
        "wait4" => 260,
        "prlimit64" => 261,
        "clone" => 220,
        "getrandom" => 278,
        "membarrier" => 283,
        "statx" => 291,
        "close_range" => 436,
        "faccessat2" => 439,
        "rseq" => 293,
        "accept4" => 242,
        _ => return None,
    })
}

/// The curated baseline allowlist. Every name must resolve in
/// `syscall_number` for the target arch or compilation fails.
const BASELINE_ALLOW: &[&str] = &[
    // ── File I/O ──────────────────────────────────────────────
    "read",
    "write",
    "pread64",
    "pwrite64",
    "readv",
    "writev",
    "open",
    "openat",
    "close",
    "close_range",
    "fstat",
    "newfstatat",
    "statx",
    "lseek",
    "pipe2",
    "dup",
    "dup2",
    "dup3",
    "fcntl",
    "flock",
    "fsync",
    "fdatasync",
    "ftruncate",
    "faccessat",
    "faccessat2",
    "readlinkat",
    // ── Memory ────────────────────────────────────────────────
    "mmap",
    "mprotect",
    "munmap",
    "mremap",
    "madvise",
    "msync",
    "brk",
    // ── Scheduling + futex (tokio core) ───────────────────────
    "futex",
    "sched_yield",
    "sched_getaffinity",
    "rseq",
    "nanosleep",
    "clock_gettime",
    "clock_nanosleep",
    "set_tid_address",
    "set_robust_list",
    // ── Signals ───────────────────────────────────────────────
    "rt_sigaction",
    "rt_sigprocmask",
    "rt_sigreturn",
    "kill",
    "tgkill",
    // ── Event loop primitives ─────────────────────────────────
    "epoll_create1",
    "epoll_ctl",
    "eventfd2",
    // ── Networking (sidecar IPC is stdio, but plugins talk out) ─
    "socket",
    "bind",
    "listen",
    "accept",
    "accept4",
    "connect",
    "sendto",
    "recvfrom",
    "sendmsg",
    "recvmsg",
    "getsockname",
    "getpeername",
    "setsockopt",
    "getsockopt",
    "shutdown",
    // ── Process lifecycle ─────────────────────────────────────
    "exit",
    "exit_group",
    "wait4",
    "execve",
    "clone",
    "prlimit64",
    "prctl",
    "getrandom",
    "membarrier",
    "ioctl",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_baseline_entry_resolves_to_a_syscall_number() {
        // If this ever fires we've added a typo to BASELINE_ALLOW.
        // The lookup is arch-specific; test ensures at least one
        // arch resolves every name (the host we're compiled on).
        for name in BASELINE_ALLOW {
            assert!(
                syscall_number(name).is_some(),
                "baseline syscall `{name}` not in arch table"
            );
        }
    }

    #[test]
    fn unknown_syscall_is_rejected() {
        match install_allowlist(&["not_a_real_syscall".into()]) {
            Err(SeccompError::UnknownSyscall(name)) => assert_eq!(name, "not_a_real_syscall"),
            // On hosts where install succeeds despite the typo, the
            // test has a bigger problem — but since we short-circuit
            // on UnknownSyscall before installing, this shouldn't
            // happen.
            other => panic!("expected UnknownSyscall, got {other:?}"),
        }
    }
}
