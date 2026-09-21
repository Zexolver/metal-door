//! Process-level hardening applied before the daemon does anything else.
//!
//! Three layers live here. The **baseline** ([`apply_baseline`]) is a pair of cheap,
//! irreversible kernel toggles set at startup, before the IPC socket exists, so the
//! credential-handling process runs hardened for its whole life. The **syscall sandbox**
//! ([`apply_seccomp`]) installs a seccomp-BPF filter that bounds which syscalls the
//! long-lived, root, untrusted-input-facing *supervisor* may make. The **path sandbox**
//! ([`apply_landlock`]) installs a Landlock ruleset that bounds which filesystem paths
//! the same supervisor may reach.
//!
//! Why the sandbox is supervisor-only, and applied late: a seccomp filter is
//! inherited across `fork` and preserved across `execve`. The per-login session
//! worker and the user's desktop it `execve`s must run with a full syscall set, so
//! the filter must land on a process that is *not* on the path to spawning a session.
//! The pre-forked spawner (forked before this filter is applied) owns session
//! creation, so its descendants stay unconfined; only the supervisor gets the filter.
//! Caller ordering is load-bearing: fork spawner → bind listener → `apply_seccomp` →
//! serve. The baseline's `NO_NEW_PRIVS` is the precondition that lets an unprivileged
//! filter install take effect for children.
//!
//! Rollout is staged (log before enforce): [`SeccompMode::Log`] installs the filter
//! with a *logging* default action, so a syscall outside the allowlist is recorded to
//! the audit log but still runs — used on hardware to enumerate the supervisor's real
//! syscall set before [`SeccompMode::Enforce`] turns the default into an `EPERM`.

use std::collections::BTreeMap;
use std::io;

use landlock::{
    path_beneath_rules, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, ABI,
};
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch};

/// Apply the always-on baseline:
///
/// - **`NO_NEW_PRIVS`** — no `execve` from this process (or any child) can ever
///   gain privileges via setuid/setgid/file capabilities. A spawned session can
///   only drop privilege, never escalate. Also the precondition for an
///   unprivileged process to install a seccomp filter (see [`apply_seccomp`]).
/// - **non-dumpable** — disables core dumps and blocks `ptrace`-attach by other
///   processes of the same user, so a credential sitting in this daemon's memory
///   cannot be scraped out of a core file or a debugger.
///
/// Best-effort: a failure is logged to the journal (stderr) but does not abort
/// startup, so the daemon still comes up on a kernel that refuses one toggle —
/// it just comes up less hardened, and says so.
pub fn apply_baseline() {
    if let Err(e) = set_no_new_privs() {
        eprintln!("doord: warning: could not set NO_NEW_PRIVS: {e}");
    }
    if let Err(e) = set_non_dumpable() {
        eprintln!("doord: warning: could not clear the dumpable flag: {e}");
    }
}

fn set_no_new_privs() -> io::Result<()> {
    // prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)
    let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_non_dumpable() -> io::Result<()> {
    // prctl(PR_SET_DUMPABLE, 0)
    let ret = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// How the supervisor seccomp filter treats a syscall that is *not* in the
/// allowlist. Selected by `DOORD_SECCOMP` (`DOORD_NO_SANDBOX=1` forces `Off`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeccompMode {
    /// No filter installed. The supervisor runs with the baseline only.
    Off,
    /// Install the filter with a **logging** default action: an unlisted syscall
    /// is written to the seccomp audit log (`SCMP_ACT_LOG`) but still executes.
    /// Used to enumerate the real syscall set on hardware before enforcing.
    Log,
    /// Install the filter with an **`EPERM`** default action: an unlisted syscall
    /// fails with `EPERM` (not a process kill — a stray syscall degrades to a
    /// logged local failure rather than taking the seat down).
    Enforce,
}

impl SeccompMode {
    /// Resolve the mode from the environment. `DOORD_NO_SANDBOX` (any non-empty,
    /// non-`0` value) is the recovery kill-switch and wins over everything.
    pub fn from_env() -> Self {
        let no_sandbox = std::env::var("DOORD_NO_SANDBOX")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let value = std::env::var("DOORD_SECCOMP").ok();
        Self::parse(value.as_deref(), no_sandbox)
    }

    /// Pure parser behind [`from_env`](Self::from_env), split out so it is testable
    /// without touching process-global environment.
    fn parse(value: Option<&str>, no_sandbox: bool) -> Self {
        if no_sandbox {
            return Self::Off;
        }
        match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("log") => Self::Log,
            Some("enforce") => Self::Enforce,
            // Unset, empty, "off", "0", or anything unrecognized: no filter.
            _ => Self::Off,
        }
    }

    /// The default (mismatch) action for this mode, or `None` when no filter is
    /// installed at all.
    fn mismatch_action(self) -> Option<SeccompAction> {
        match self {
            Self::Off => None,
            Self::Log => Some(SeccompAction::Log),
            Self::Enforce => Some(SeccompAction::Errno(libc::EPERM as u32)),
        }
    }
}

/// Install the supervisor seccomp filter for `mode`. A no-op for [`SeccompMode::Off`].
///
/// **Call supervisor-only, after the spawner has been forked** — a filter applied
/// before the spawner fork (or on the direct in-lineage path, where the supervisor
/// itself forks the session) is inherited by the desktop and breaks every login.
///
/// Installed with `TSYNC` ([`apply_filter_all_threads`](seccompiler::apply_filter_all_threads))
/// so it confines **every** supervisor thread, not just the caller. The supervisor is
/// no longer single-threaded: the session-lock reauth listener runs on its own
/// daemon-lifetime thread — created before this install, since the allowlist has no
/// `clone` — and it serves the daemon's most-exposed surface (the world-connectable
/// reauth socket). A per-thread `apply_filter` would leave that thread unconfined; TSYNC
/// covers it without needing a post-filter `clone`. The precondition (`NO_NEW_PRIVS`,
/// set in the baseline before any thread is spawned and inherited by them) is what lets
/// the TSYNC install take for all of them.
///
/// The filter is irreversible and inherited by children, so it is never installed
/// from a test — [`compile`] (which does everything up to the install) is the
/// testable half.
pub fn apply_seccomp(mode: SeccompMode) -> io::Result<()> {
    match compile(mode)? {
        None => Ok(()),
        Some(program) => seccompiler::apply_filter_all_threads(&program)
            .map_err(|e| io::Error::other(format!("seccomp: could not install filter: {e}"))),
    }
}

/// Build (but do not install) the BPF program for `mode`. `Ok(None)` for
/// [`SeccompMode::Off`]. This is the pure, testable half of [`apply_seccomp`]:
/// it exercises the allowlist and the target-arch compile without confining the
/// calling process.
fn compile(mode: SeccompMode) -> io::Result<Option<BpfProgram>> {
    let Some(mismatch_action) = mode.mismatch_action() else {
        return Ok(None);
    };

    // An empty rule list per syscall means "match on the syscall number alone,
    // unconditionally" — i.e. a plain allow. We gate on syscall numbers only; no
    // argument-level conditions in this tier.
    let rules: BTreeMap<i64, Vec<SeccompRule>> = SUPERVISOR_ALLOWLIST
        .iter()
        .map(|&nr| (nr, Vec::new()))
        .collect();

    let filter = SeccompFilter::new(
        rules,
        mismatch_action,      // syscalls outside the allowlist
        SeccompAction::Allow, // syscalls inside the allowlist
        target_arch()?,
    )
    .map_err(|e| io::Error::other(format!("seccomp: could not build filter: {e}")))?;

    let program: BpfProgram = filter
        .try_into()
        .map_err(|e| io::Error::other(format!("seccomp: could not compile filter: {e}")))?;

    Ok(Some(program))
}

fn target_arch() -> io::Result<TargetArch> {
    #[cfg(target_arch = "x86_64")]
    {
        Ok(TargetArch::x86_64)
    }
    #[cfg(target_arch = "aarch64")]
    {
        Ok(TargetArch::aarch64)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        Err(io::Error::other(
            "seccomp: no allowlist for this target architecture",
        ))
    }
}

/// Initial supervisor syscall allowlist — an **empirically-refined seed**, not a
/// proven-minimal set. This tier ships in `SCMP_ACT_LOG` mode: the hardware log
/// run records every syscall the supervisor makes that is *not*
/// listed here, and the list is widened until the audit log is clean across a full
/// login → logout → recycle → re-login cycle. Only then does `Enforce` turn the
/// default action into an `EPERM`. Entries here cover the supervisor's known steady
/// state: the epoll/accept IPC loop, `SCM_RIGHTS` fd passing to the spawner, PAM-relay
/// proxying, seat/VT/DRM ioctls, the SIGCHLD reaper, and process teardown. The
/// spawner, worker, and session are *not* covered — they run outside this lineage.
/// Entries marked `#[cfg(target_arch = "x86_64")]` are **legacy syscalls that do
/// not exist on aarch64** — that architecture provides only the `*at`/`p-` forms,
/// which are already listed beside them, so gating them costs aarch64 nothing.
///
/// ⚠ The aarch64 allowlist is **not hardware-validated**. D-0016 derived the
/// x86_64 set empirically under `SCMP_ACT_LOG` across a full login cycle on real
/// hardware before enforcing it; no such run has happened on aarch64. Run with
/// `DOORD_SECCOMP=log` there first and widen this list from the audit log before
/// trusting `enforce`.
const SUPERVISOR_ALLOWLIST: &[i64] = &[
    // --- event loop / socket I/O ---
    libc::SYS_epoll_create1,
    libc::SYS_epoll_ctl,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_epoll_wait,
    libc::SYS_epoll_pwait,
    libc::SYS_ppoll,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_poll,
    libc::SYS_accept,
    libc::SYS_accept4,
    libc::SYS_socket,
    libc::SYS_socketpair,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_connect,
    libc::SYS_getsockname,
    libc::SYS_getpeername,
    libc::SYS_getsockopt, // incl. SO_PEERCRED, the greeter authorization gate
    libc::SYS_setsockopt,
    libc::SYS_shutdown,
    libc::SYS_recvmsg, // SCM_RIGHTS fd receive from the spawner
    libc::SYS_sendmsg, // SCM_RIGHTS fd send to the spawner
    libc::SYS_recvfrom,
    libc::SYS_sendto,
    // --- generic fd / file ops ---
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_readv,
    libc::SYS_writev,
    libc::SYS_pread64,
    libc::SYS_pwrite64,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_open,
    libc::SYS_openat,
    libc::SYS_close,
    libc::SYS_lseek,
    libc::SYS_fcntl,
    libc::SYS_dup,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_dup2,
    libc::SYS_dup3,
    libc::SYS_pipe2,
    libc::SYS_eventfd2,
    libc::SYS_fstat,
    libc::SYS_newfstatat,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_stat,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_lstat,
    libc::SYS_statx,
    libc::SYS_getdents64,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_readlink,
    libc::SYS_readlinkat,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_access,
    libc::SYS_faccessat,
    libc::SYS_faccessat2,
    // --- socket-dir bookkeeping (create/chmod/chown/unlink the listener) ---
    #[cfg(target_arch = "x86_64")]
    libc::SYS_mkdir,
    libc::SYS_mkdirat,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_unlink,
    libc::SYS_unlinkat,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_rename,
    libc::SYS_renameat2,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_chmod,
    libc::SYS_fchmod,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_chown,
    libc::SYS_fchown,
    libc::SYS_fchownat,
    // --- seat / VT / DRM management ---
    libc::SYS_ioctl,
    // --- signals + SIGCHLD reaper ---
    libc::SYS_rt_sigaction,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigreturn,
    libc::SYS_sigaltstack,
    libc::SYS_signalfd4,
    libc::SYS_wait4,
    libc::SYS_waitid,
    libc::SYS_kill,
    libc::SYS_tgkill,
    // --- memory management ---
    libc::SYS_mmap,
    libc::SYS_munmap,
    libc::SYS_mremap,
    libc::SYS_mprotect,
    libc::SYS_madvise,
    libc::SYS_brk,
    // --- process / identity / scheduling ---
    libc::SYS_getpid,
    libc::SYS_gettid,
    libc::SYS_getppid,
    libc::SYS_getuid,
    libc::SYS_geteuid,
    libc::SYS_getgid,
    libc::SYS_getegid,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_getpgrp,
    libc::SYS_getrandom,
    libc::SYS_prctl,
    libc::SYS_futex,
    libc::SYS_sched_yield,
    libc::SYS_set_robust_list,
    libc::SYS_get_robust_list,
    libc::SYS_rseq,
    // --- time ---
    libc::SYS_clock_gettime,
    libc::SYS_clock_getres,
    libc::SYS_clock_nanosleep,
    libc::SYS_nanosleep,
    libc::SYS_gettimeofday,
    // --- teardown ---
    libc::SYS_exit,
    libc::SYS_exit_group,
    libc::SYS_restart_syscall,
];

// ===========================================================================
// Landlock — filesystem-path sandbox on the supervisor (Tier 4)
// ===========================================================================
//
// Where seccomp bounds *which syscalls* the supervisor may make, Landlock bounds
// *which filesystem paths* it may reach — a compromised supervisor cannot read
// `/home`, write `/etc/shadow`, or touch arbitrary user data outside the small set
// of subtrees it genuinely needs. Same placement constraint as seccomp: a Landlock
// ruleset is inherited across `fork` and preserved across `execve`, so it is applied
// supervisor-only, after `fork_spawner`, or it would confine the user's desktop.
//
// **No permissive/log mode.** Unlike seccomp's `SCMP_ACT_LOG`, Landlock has no
// observe-without-breaking mode: any path outside the ruleset is *blocked the moment
// the ruleset is active*. (Kernel 6.15+ audits denials, but the denial still blocks —
// diagnostics while enforcing, not permissive operation.) So the rollout is
// enumerate-then-enforce: the path set below is an empirically-refined **seed**, the
// filter ships flag-gated + default-off, and a genny boot with `DOORD_LANDLOCK=enforce`
// runs a full login → logout → recycle → re-login cycle, widening the set (via the
// `EACCES` journal trail; `DOORD_NO_SANDBOX=1` recovers a miss) until the cycle is clean.
//
// **ABI floor is V1, deliberately.** The handled access set is pinned to
// [`ABI::V1`] (governs Execute/Read/Write/Dir/Make/Remove). Device-file `ioctl`
// governance (`LANDLOCK_ACCESS_FS_IOCTL_DEV`) only enters the handled set at ABI V5;
// declaring it on genny's newer kernel would govern the supervisor's DRM-master and
// VT `ioctl`s and lock out login unless `IoctlDev` were also granted on `/dev/dri` and
// the VT. Raising the ABI to also confine device ioctls is a documented genny-tuning
// follow-on, not part of this inert seed. Best-effort compatibility means an older
// kernel (or one without Landlock) degrades to `NotEnforced` rather than aborting.

/// Whether the supervisor installs the Landlock filesystem sandbox. Selected by
/// `DOORD_LANDLOCK` (`DOORD_NO_SANDBOX=1` forces `Off`). There is no `Log` variant —
/// Landlock cannot log-and-allow, so the safe rollout is flag-gated enforce validated
/// on hardware (see the module note above), not a permissive stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandlockMode {
    /// No ruleset installed. The supervisor's filesystem access is unbounded (Tier 0
    /// baseline + any seccomp only).
    Off,
    /// Install the path ruleset and confine the supervisor: any filesystem path outside
    /// the granted subtrees is denied (`EACCES`).
    Enforce,
}

impl LandlockMode {
    /// Resolve the mode from the environment. `DOORD_NO_SANDBOX` (any non-empty,
    /// non-`0` value) is the recovery kill-switch and wins over everything.
    pub fn from_env() -> Self {
        let no_sandbox = std::env::var("DOORD_NO_SANDBOX")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let value = std::env::var("DOORD_LANDLOCK").ok();
        Self::parse(value.as_deref(), no_sandbox)
    }

    /// Pure parser behind [`from_env`](Self::from_env), split out so it is testable
    /// without touching process-global environment.
    fn parse(value: Option<&str>, no_sandbox: bool) -> Self {
        if no_sandbox {
            return Self::Off;
        }
        match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("enforce") => Self::Enforce,
            // Unset, empty, "off", "0", or anything unrecognized: no ruleset.
            _ => Self::Off,
        }
    }
}

/// Read-only subtrees the supervisor needs (Execute/ReadFile/ReadDir). An
/// **empirically-refined seed**, not proven-minimal — see the module note. `/home`,
/// `/root`, `/var`, `/tmp`, `/boot`, `/opt`, `/srv`, `/mnt` are deliberately absent:
/// denying them is the point of this tier.
const SUPERVISOR_RO_PATHS: &[&str] = &[
    "/usr",  // session `.desktop` discovery + shared libraries/binaries the process maps
    "/etc",  // NSS / `nsswitch.conf` / `passwd` for user lookup, `ld.so.cache`
    "/proc", // `free_seat`'s per-process `/fd` scan (read_dir + read_link)
    "/sys",  // DRM / device metadata
    "/run",  // logind/D-Bus sockets + runtime dirs (the rw `/run/doord` subtree is below)
];

/// Read-write subtrees the supervisor needs (full filesystem access set at ABI V1).
const SUPERVISOR_RW_PATHS: &[&str] = &[
    "/run/doord",        // the IPC listener socket dir: create/bind/chmod/chown/unlink
    "/run/doord-reauth", // the session-lock reauth listener socket dir (create/bind/chmod/unlink)
    "/dev",              // `/dev/dri/card*` (DRM master) + `/dev/tty{N}` (VT); tighten on genny
];

/// Install the supervisor Landlock ruleset for `mode`. A no-op for [`LandlockMode::Off`].
///
/// **Call supervisor-only, after the spawner has been forked** — a ruleset applied
/// before the spawner fork (or on the direct in-lineage path, where the supervisor
/// itself forks the session) is inherited by the desktop and breaks every login.
///
/// Best-effort: on a kernel without Landlock (or an older ABI) the ruleset degrades
/// to [`RulesetStatus::NotEnforced`]/`PartiallyEnforced` rather than aborting startup —
/// the returned status says which, so the caller can log the real enforcement level.
/// The confinement is irreversible and inherited by children, so it is never applied
/// from a test.
pub fn apply_landlock(mode: LandlockMode) -> io::Result<RulesetStatus> {
    if mode == LandlockMode::Off {
        return Ok(RulesetStatus::NotEnforced);
    }

    // ABI V1 handled set (see the module note on why not the newest ABI): governs
    // path read/write/exec/dir/make/remove, not device ioctls.
    let abi = ABI::V1;
    let status = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .map_err(landlock_err)?
        .create()
        .map_err(landlock_err)?
        .add_rules(path_beneath_rules(
            SUPERVISOR_RO_PATHS,
            AccessFs::from_read(abi),
        ))
        .map_err(landlock_err)?
        .add_rules(path_beneath_rules(
            SUPERVISOR_RW_PATHS,
            AccessFs::from_all(abi),
        ))
        .map_err(landlock_err)?
        .restrict_self()
        .map_err(landlock_err)?;

    Ok(status.ruleset)
}

fn landlock_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("landlock: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_sandbox_overrides_every_value() {
        assert_eq!(SeccompMode::parse(Some("enforce"), true), SeccompMode::Off);
        assert_eq!(SeccompMode::parse(Some("log"), true), SeccompMode::Off);
        assert_eq!(SeccompMode::parse(None, true), SeccompMode::Off);
    }

    #[test]
    fn parse_recognizes_the_modes_case_insensitively() {
        assert_eq!(SeccompMode::parse(Some("log"), false), SeccompMode::Log);
        assert_eq!(SeccompMode::parse(Some("  LOG "), false), SeccompMode::Log);
        assert_eq!(
            SeccompMode::parse(Some("Enforce"), false),
            SeccompMode::Enforce
        );
    }

    #[test]
    fn parse_defaults_to_off_for_unset_or_unknown() {
        assert_eq!(SeccompMode::parse(None, false), SeccompMode::Off);
        assert_eq!(SeccompMode::parse(Some(""), false), SeccompMode::Off);
        assert_eq!(SeccompMode::parse(Some("off"), false), SeccompMode::Off);
        assert_eq!(SeccompMode::parse(Some("0"), false), SeccompMode::Off);
        assert_eq!(
            SeccompMode::parse(Some("yes-please"), false),
            SeccompMode::Off
        );
    }

    #[test]
    fn off_compiles_to_no_program() {
        assert!(compile(SeccompMode::Off).unwrap().is_none());
    }

    #[test]
    fn log_and_enforce_compile_to_a_nonempty_program() {
        // Compiling exercises the allowlist + target-arch lowering without
        // installing anything, so the test process stays unconfined.
        for mode in [SeccompMode::Log, SeccompMode::Enforce] {
            let program = compile(mode).unwrap().expect("a program for the mode");
            assert!(
                !program.is_empty(),
                "{mode:?} should compile to a non-empty BPF program"
            );
        }
    }

    /// The supervisor grew a second thread (the reauth listener), so the seccomp filter
    /// must confine *both* threads, not just the caller — hence `apply_filter_all_threads`
    /// (TSYNC). This proves it: in a forked child (so the test runner is never confined),
    /// spawn a sibling thread, install the real enforce filter with TSYNC from the main
    /// child thread, then have the **sibling** attempt a syscall outside the allowlist
    /// (`chdir`). If TSYNC covered the sibling, the call is `EPERM`'d; if the install had
    /// been per-thread, the sibling would run unconfined and the call would succeed —
    /// exactly the reauth-thread hole this guards against. The child's exit code encodes
    /// the outcome. Skips gracefully where an unprivileged seccomp install is unavailable.
    ///
    /// Coverage note: this proves the *filter* reaches a sibling thread. In `main` the
    /// reauth thread is additionally held on a release channel — it blocks on `recv`
    /// (only a futex wait) and touches no socket until the main thread releases it *after*
    /// this all-threads install — so its first `bind`/`listen`/`accept` already runs under
    /// the filter, with no scheduler race over the connectable window.
    #[test]
    fn all_threads_filter_confines_a_sibling_thread() {
        use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

        static PROCEED: AtomicBool = AtomicBool::new(false);
        static DONE: AtomicBool = AtomicBool::new(false);
        static SIBLING_ERRNO: AtomicI32 = AtomicI32::new(0);

        // SAFETY: fork; the child does only atomics + one thread spawn + the filter
        // install + a probe syscall before _exit, never returning to the harness.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // TSYNC on an unprivileged caller requires NO_NEW_PRIVS on every thread, so
            // set it before spawning the sibling (which then inherits it).
            let nnp = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
            if nnp != 0 {
                unsafe { libc::_exit(40) }; // can't set NNP → inconclusive, skip
            }

            let sibling = std::thread::spawn(|| {
                // Wait (unconfined syscalls are fine here) until the filter is installed.
                while !PROCEED.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
                // A syscall deliberately outside SUPERVISOR_ALLOWLIST. Under the enforce
                // filter this returns EPERM *iff* TSYNC confined this sibling thread.
                let rc = unsafe { libc::chdir(c"/".as_ptr()) };
                let err = if rc == 0 {
                    0
                } else {
                    io::Error::last_os_error().raw_os_error().unwrap_or(0)
                };
                SIBLING_ERRNO.store(err, Ordering::SeqCst);
                DONE.store(true, Ordering::SeqCst);
            });

            // Install the real enforce filter across ALL threads (main + sibling).
            let program = compile(SeccompMode::Enforce)
                .expect("compile")
                .expect("program");
            match seccompiler::apply_filter_all_threads(&program) {
                Ok(()) => {}
                // No unprivileged seccomp here (restricted sandbox) → inconclusive, skip.
                Err(_) => unsafe { libc::_exit(40) },
            }

            PROCEED.store(true, Ordering::SeqCst);
            while !DONE.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            let _ = sibling.join();

            let code = if SIBLING_ERRNO.load(Ordering::SeqCst) == libc::EPERM {
                0 // sibling was confined by TSYNC — the guarantee holds
            } else {
                10 // sibling ran unconfined — the per-thread hole would look like this
            };
            unsafe { libc::_exit(code) };
        }

        let mut wstatus = 0i32;
        unsafe { libc::waitpid(pid, &mut wstatus, 0) };
        let code = libc::WEXITSTATUS(wstatus);
        if code == 40 {
            eprintln!("unprivileged seccomp unavailable here — skipping the TSYNC assertion");
            return;
        }
        assert_eq!(
            code, 0,
            "child exit {code}: 0 = sibling thread confined by TSYNC, \
             10 = sibling ran UNCONFINED (per-thread filter would leave the reauth thread open)"
        );
    }

    // --- Landlock ---
    //
    // Only the (pure) mode parser is unit-tested: building a real ruleset syscalls,
    // and `restrict_self` would confine the test process irreversibly, so the ruleset
    // install is validated on hardware (a genny `DOORD_LANDLOCK=enforce` boot), not here.

    #[test]
    fn landlock_no_sandbox_overrides_every_value() {
        assert_eq!(
            LandlockMode::parse(Some("enforce"), true),
            LandlockMode::Off
        );
        assert_eq!(LandlockMode::parse(None, true), LandlockMode::Off);
    }

    #[test]
    fn landlock_parse_recognizes_enforce_case_insensitively() {
        assert_eq!(
            LandlockMode::parse(Some("enforce"), false),
            LandlockMode::Enforce
        );
        assert_eq!(
            LandlockMode::parse(Some("  ENFORCE "), false),
            LandlockMode::Enforce
        );
    }

    #[test]
    fn landlock_parse_defaults_to_off_for_unset_or_unknown() {
        assert_eq!(LandlockMode::parse(None, false), LandlockMode::Off);
        assert_eq!(LandlockMode::parse(Some(""), false), LandlockMode::Off);
        assert_eq!(LandlockMode::parse(Some("off"), false), LandlockMode::Off);
        assert_eq!(LandlockMode::parse(Some("0"), false), LandlockMode::Off);
        // No permissive stage exists for Landlock — "log" is not a valid mode.
        assert_eq!(LandlockMode::parse(Some("log"), false), LandlockMode::Off);
    }

    #[test]
    fn landlock_apply_off_is_a_noop() {
        // Off must never touch the kernel or confine the caller; safe to call in-test.
        assert_eq!(
            apply_landlock(LandlockMode::Off).unwrap(),
            RulesetStatus::NotEnforced
        );
    }

    /// Real confinement check: enforce inside a `fork`ed child (so the test runner is
    /// never confined) and assert the child can read an allowlisted subtree but is
    /// denied a path outside it. Ignored by default — it installs a live Landlock
    /// ruleset, so it only means anything on a Landlock-capable kernel:
    ///   cargo test -p doord --bin doord -- --ignored landlock_enforce_confines
    #[test]
    #[ignore = "installs a real Landlock ruleset; run manually on a Landlock kernel"]
    fn landlock_enforce_confines_a_forked_child() {
        // Probe file OUTSIDE the allowlist — the OS temp dir (/tmp) is deliberately
        // excluded from the supervisor path seed.
        let probe = std::env::temp_dir().join(format!("doord-ll-probe-{}", std::process::id()));
        std::fs::write(&probe, b"x").expect("write probe file");

        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // Child: confine self, then probe. Exit code encodes the outcome — never
            // returns to the harness.
            let code = match apply_landlock(LandlockMode::Enforce) {
                // Kernel without Landlock: inconclusive, tell the parent to skip.
                Ok(RulesetStatus::NotEnforced) => 40,
                Ok(_) => {
                    let outside_denied = std::fs::File::open(&probe).is_err();
                    let inside_ok = std::fs::read_dir("/usr").is_ok();
                    if outside_denied && inside_ok {
                        0
                    } else {
                        10
                    }
                }
                Err(_) => 41,
            };
            unsafe { libc::_exit(code) };
        }

        let mut wstatus = 0i32;
        unsafe { libc::waitpid(pid, &mut wstatus, 0) };
        let _ = std::fs::remove_file(&probe);
        let code = libc::WEXITSTATUS(wstatus);
        if code == 40 {
            eprintln!("landlock not enforced on this kernel — skipping the confinement assertion");
            return;
        }
        assert_eq!(
            code, 0,
            "child exit {code}: 0 = confined (outside denied, /usr allowed), \
             10 = ruleset did not confine as expected, 41 = apply_landlock errored"
        );
    }
}
