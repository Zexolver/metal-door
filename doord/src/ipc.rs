//! The local IPC server: the daemon's side of the trust boundary.
//!
//! This is where untrusted bytes first reach the privileged core, so the
//! authorization checks live here, before any request is interpreted:
//!
//! - a **pathname** Unix socket (never the abstract namespace, which has no
//!   filesystem permissions) under a `root`-owned `0700` directory, with the
//!   socket itself `0660` so only `root` and the greeter group can reach it;
//! - an `SO_PEERCRED` check on every connection — the kernel-attested uid of the
//!   peer must match the configured greeter uid, regardless of socket
//!   permissions (defense in depth);
//! - **one** greeter at a time: connections are served sequentially, so there is
//!   never a race between two greeters over one seat;
//! - a **read timeout** so a peer that opens a connection and goes silent cannot
//!   pin the daemon;
//! - a mandatory **version handshake** ([`PROTOCOL_VERSION`]) before any other
//!   request is honored.
//!
//! Above the per-connection handling sits the **login loop** ([`serve`], D-0008):
//! when a greeter user is configured, doord owns the greeter lifecycle — it
//! launches the greeter, serves the login, performs the greeter→session VT handoff
//! (tears the greeter down before the session takes the seat), waits for the
//! session to end, and re-greets. Without a greeter user (dev), it just serves
//! whatever greeter connects.

use std::cell::RefCell;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use protocol::{read_frame, write_frame, FrameError, Request, Response, PROTOCOL_VERSION};

use crate::config::Config;
use crate::pam::{AuthOutcome, Login, LoginFactory};
use crate::spawn::SessionChild;
use crate::spawner::Spawner;
use crate::worker;

/// How long to wait for the greeter to exit after `SIGTERM` before `SIGKILL`.
/// The greeter-worker does real teardown in this window — it forwards the
/// terminate to its compositor (which releases seat0's DRM/VT) and then closes the
/// greeter logind session — so the grace must cover a DRM teardown plus a session
/// close, not just a process exit. If it overruns, the `SIGKILL` lands and the
/// compositor's `PR_SET_PDEATHSIG` still guarantees it dies and frees the VT.
const GREETER_TERM_GRACE: Duration = Duration::from_secs(2);

/// A managed greeter that lives less than this clearly failed (it never carried a
/// real login, which lasts a session); re-greeting it immediately would spin. Back
/// off [`GREETER_RESPAWN_BACKOFF`] before trying again.
const GREETER_MIN_UPTIME: Duration = Duration::from_secs(3);
const GREETER_RESPAWN_BACKOFF: Duration = Duration::from_secs(2);

/// After this many *consecutive* quick greeter failures the daemon stops trying
/// and exits cleanly ([`give_up`]). Relaunching a compositor (doorstep) that grabs
/// DRM/KMS on a tight loop is what wedges the GPU and blacks out every VT, so a
/// sustained failure must fail *safe* — leave a usable text console — not spin.
const GREETER_MAX_RAPID_FAILURES: u32 = 3;

/// Poll cadence for the live-session wait: short enough that an admin teardown
/// (`SIGTERM`/`SIGINT`) is noticed promptly so the seat is freed for the next
/// login manager, long enough not to busy-spin while a desktop runs for hours.
const SESSION_WAIT_POLL: Duration = Duration::from_millis(250);

/// Grace after `SIGTERM`ing the seat's GPU holder before escalating to `SIGKILL`,
/// so a compositor can release DRM and exit cleanly first.
const SEAT_FREE_GRACE: Duration = Duration::from_secs(2);

/// Brief settle after a `SIGKILL` round before re-checking whether the seat's GPU
/// is actually free.
const SEAT_FREE_KILL_SETTLE: Duration = Duration::from_millis(500);

/// How often the greeter-watch poll wakes to re-check the greeter process while
/// waiting for it to connect. Short enough that a greeter which dies *before*
/// connecting (the host couldn't take the seat's DRM master, say) is noticed
/// promptly and routed to the backoff/give-up path instead of hanging the daemon
/// on `accept()` forever; long enough not to busy-spin.
const GREETER_WATCH_INTERVAL: Duration = Duration::from_millis(250);

/// How long the daemon waits on a stalled *in-flight* frame before dropping the
/// peer. This bounds a transfer that has already begun (a partial-frame stall, à
/// la slowloris) — it is deliberately **not** applied to the idle wait between
/// messages, where a human is legitimately taking their time (see
/// [`read_greeter_request`]).
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Read one [`Request`] from the greeter, distinguishing an idle wait from a
/// stalled transfer.
///
/// The greeter sits silent between messages for as long as the human on the
/// other end likes — standing at the login prompt before typing a username, or
/// mid-login deciding on a password. Those waits must be **unbounded**: a blanket
/// read timeout across the whole connection would drop the greeter the moment the
/// user paused (the false positive that used to churn the login screen every
/// [`READ_TIMEOUT`]). But once a message *starts* arriving it must finish
/// promptly; a peer that dribbles a partial frame and then stalls is dropped.
///
/// The split is implemented by `poll`ing with no deadline until the socket is
/// readable — which observes an incoming frame (or a clean EOF) *without*
/// consuming any bytes — then reading the whole frame under [`READ_TIMEOUT`].
/// [`read_frame`] sees the same bytes `poll` reported, or, on a closed peer, a
/// zero-length read that surfaces as [`io::ErrorKind::UnexpectedEof`] — which the
/// callers already treat as "greeter disconnected". No socket read happens during
/// the idle wait, so the connection's own read timeout is irrelevant there.
pub(crate) fn read_greeter_request(conn: &UnixStream) -> Result<Request, FrameError> {
    // Idle phase: block with no deadline until a frame starts arriving.
    let mut pfd = libc::pollfd {
        fd: conn.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: one valid pollfd, count 1, infinite timeout; poll writes only revents.
        let rc = unsafe { libc::poll(&mut pfd, 1, -1) };
        if rc >= 0 {
            break;
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue; // EINTR wake — re-arm the wait.
        }
        return Err(FrameError::Io(err));
    }
    // Transfer phase: a message has begun; bound it so a mid-frame stall can't
    // pin the daemon.
    conn.set_read_timeout(Some(READ_TIMEOUT))?;
    let mut reader: &UnixStream = conn;
    read_frame(&mut reader)
}

/// Floor on how long a *failed* authentication takes to report. A wrong username
/// must not fail visibly faster than a wrong password, or the timing itself
/// leaks which accounts exist. Only the failure path is padded — success is not
/// slowed. Lockout/backoff proper is left to the PAM stack (`pam_faillock`).
pub(crate) const MIN_AUTH_FAILURE: Duration = Duration::from_secs(1);

/// The seat VT doord owns, or `-1` for none — read by the teardown signal
/// handler, which runs in async-signal context and so cannot consult [`Config`].
static OWNED_VTNR: AtomicI32 = AtomicI32::new(-1);

/// True only while doord holds the VT *at the greeter* (no live session). The
/// teardown handler resets the VT only in this state: during a live session the
/// user's compositor owns the VT and must not be disturbed (and that session has
/// no parent-death signal, so it rightly survives an admin restart of doord).
static GREETING: AtomicBool = AtomicBool::new(false);

/// Set by the teardown handler so the live-session wait loop notices an admin
/// `systemctl stop`/`disable` (`SIGTERM`/`SIGINT`) and, in normal code, frees the
/// seat for the next login manager before exiting. The handler cannot do that
/// itself: freeing the seat means scanning `/proc` and killing the compositor,
/// which is not async-signal-safe.
static TEARDOWN: AtomicBool = AtomicBool::new(false);

/// Install the teardown signal handler so an admin `systemctl stop`/`disable`
/// (`SIGTERM`, or `SIGINT` under a foreground run) leaves a usable text console.
/// Without it, stopping doord while it sits at the greeter kills the managed
/// compositor (via its parent-death signal) but never restores the VT it left in
/// graphics mode — the console looks frozen until a fallback DM starts. The
/// handler restores the VT (only when [`GREETING`]) and re-raises the signal with
/// the default disposition so the process still dies with the right semantics.
///
/// Registered only when doord owns a real VT; tests drive `handle_connection`
/// directly and never reach here, so the process signal disposition is untouched
/// under test.
fn install_teardown_handler(vtnr: u32) {
    OWNED_VTNR.store(vtnr as i32, Ordering::SeqCst);
    // SAFETY: `sigaction` installs `handle_teardown` (a plain `extern "C"`
    // function with no captured state) for SIGTERM/SIGINT. `sa_flags` omits
    // SA_RESTART so a pending teardown interrupts a blocked `accept`/`poll`; the
    // handler does only async-signal-safe work (atomics + open/ioctl/close +
    // signal/raise). The sigaction struct is zeroed then fully initialized.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_teardown as *const () as usize;
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
}

/// Async-signal-safe teardown handler (see [`install_teardown_handler`]). Resets
/// the owned VT to a usable text console when doord is caught at the greeter,
/// then restores the default disposition and re-raises so the process terminates
/// with normal signal semantics. Uses only async-signal-safe operations: atomic
/// loads, [`restore_text_vt_raw`] (open/ioctl/close, no allocation), `signal`,
/// and `raise`.
extern "C" fn handle_teardown(sig: libc::c_int) {
    TEARDOWN.store(true, Ordering::SeqCst);
    if GREETING.load(Ordering::SeqCst) {
        // At the greeter: no live session owns the seat. Reset the VT and die now
        // — the managed compositor dies with doord via its parent-death signal, so
        // nothing is left squatting the seat. Async-signal-safe throughout.
        let vtnr = OWNED_VTNR.load(Ordering::SeqCst);
        if vtnr >= 0 {
            restore_text_vt_raw(vtnr as u32);
        }
        // SAFETY: restore the default disposition and re-raise; both are
        // async-signal-safe. The process dies here without returning to the
        // interrupted syscall.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }
    // During a live session the compositor escapes into the per-user systemd
    // manager and holds the seat's DRM master; freeing it (scan /proc, kill the
    // holder) is not async-signal-safe. So only flag the teardown here and return —
    // the session-wait loop sees the flag and frees the seat in normal code.
}

/// Install a panic hook that frees the seat (and restores the VT) before the
/// process unwinds/aborts, so a doord panic during a live session cannot leave the
/// user's compositor squatting the seat's GPU and wedging the next login manager.
/// Chains to the previous hook so the default panic message is still printed.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("doord: panic — freeing the seat before exit");
        free_seat();
        let vtnr = OWNED_VTNR.load(Ordering::SeqCst);
        if vtnr >= 0 {
            restore_text_vt(vtnr as u32);
        }
        previous(info);
    }));
}

/// Own a started session until it ends, while staying responsive to an admin
/// teardown. Polls the session for exit on a short cadence; if a teardown signal
/// arrives first ([`TEARDOWN`]), the live compositor is squatting the seat's DRM
/// master and must be cleared for the next login manager — so free the seat,
/// restore a usable text console, and exit. (A `SIGTERM` cannot do this itself:
/// it would have to scan `/proc` and kill the holder, which is not
/// async-signal-safe — hence this normal-code path.)
///
/// A childless test handle has no real process to poll, so it falls back to the
/// blocking [`SessionChild::wait`], which returns immediately for it.
fn wait_for_session(mut child: SessionChild, seat: &crate::config::SeatTarget, session_id: &str) {
    if child.id().is_none() {
        let _ = child.wait();
        return;
    }
    loop {
        if TEARDOWN.load(Ordering::SeqCst) {
            eprintln!(
                "doord: teardown during live session '{session_id}'; freeing the seat for the next login manager"
            );
            free_seat();
            if let Some(vtnr) = seat.vtnr {
                restore_text_vt(vtnr);
            }
            std::process::exit(0);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                eprintln!("doord: session '{session_id}' exited ({status})");
                return;
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("doord: waiting on session '{session_id}' failed: {e}");
                return;
            }
        }
        thread::sleep(SESSION_WAIT_POLL);
    }
}

/// Free doord's seat by killing the process **group** of whatever holds the seat's
/// DRM device(s) open.
///
/// The user's compositor runs under the per-user systemd manager and *outlives*
/// the logind session, doord, and `loginctl terminate-session` alike — it keeps
/// the seat's DRM master, so any next login manager (sddm, or doord's own
/// re-greeted host) cannot acquire the GPU and is left on a blank, blinking VT.
///
/// Killing the holder's bare pid is *not* enough: KWin (and friends) run the
/// compositor under a supervisor — `kwin_wayland_wrapper` — that **respawns** the
/// compositor the instant it dies, which just bounces DRM (the desktop sees a
/// monitor unplug/replug) and never frees the seat. The wrapper shares the
/// compositor's process group, so signalling the whole **group** takes the
/// supervisor down too and nothing respawns. `SIGTERM` first (a clean compositor
/// exit that releases DRM), then `SIGKILL` any group still holding the card. A
/// no-op when nothing holds the card (clean boot, clean in-session logout).
///
/// Single-seat assumption: targets every `/dev/dri/card*` KMS node. A multi-seat
/// host would need to scope this to the seat's own GPU.
fn free_seat() {
    let cards = drm_card_nodes();
    if cards.is_empty() {
        return;
    }
    let holders = drm_card_holders(&cards);
    if holders.is_empty() {
        return;
    }
    let groups = holder_process_groups(&holders);
    eprintln!(
        "doord: freeing the seat — GPU held by pids {holders:?}; killing process group(s) {groups:?}"
    );
    for &pgid in &groups {
        signal_group(pgid, libc::SIGTERM);
    }
    thread::sleep(SEAT_FREE_GRACE);
    // Re-scan: SIGKILL the group of anything still — or freshly (a supervisor
    // respawn that slipped in during the grace) — holding the card.
    let stragglers = drm_card_holders(&cards);
    if !stragglers.is_empty() {
        for &pgid in &holder_process_groups(&stragglers) {
            signal_group(pgid, libc::SIGKILL);
        }
        thread::sleep(SEAT_FREE_KILL_SETTLE);
        let still = drm_card_holders(&cards);
        if !still.is_empty() {
            eprintln!(
                "doord: warning: the GPU is still held after SIGKILL by {still:?}; the seat may not be free"
            );
        }
    }
    // Deliberately *not* ending the seat's logind sessions here (e.g. via
    // `loginctl terminate-seat`): doing so leaves the VT momentarily session-less,
    // and logind's `autovt` then races a getty onto it before doord re-greets — a
    // visible "login:" flash on the seat console. Killing the compositor frees the
    // GPU/VT (what the next login manager needs); any session-bound leftover like
    // plasmashell has no compositor to draw to, holds no GPU, and is stopped by its
    // own StartLimit — invisible and harmless, not worth a getty flash to sweep.
}

/// The distinct process groups of `holders` (`getpgid` of each). Groups `<= 1`
/// (init / unknown) are dropped so we never signal the whole system.
fn holder_process_groups(holders: &[libc::pid_t]) -> Vec<libc::pid_t> {
    let mut groups = Vec::new();
    for &pid in holders {
        // SAFETY: getpgid of a pid; -1 (the process just exited) is filtered below.
        let pgid = unsafe { libc::getpgid(pid) };
        if pgid > 1 && !groups.contains(&pgid) {
            groups.push(pgid);
        }
    }
    groups
}

/// Send `sig` to every process in group `pgid` (`kill(-pgid)`), refusing to
/// signal doord's own group. Best effort — an `ESRCH` (group already gone) is fine.
fn signal_group(pgid: libc::pid_t, sig: libc::c_int) {
    // SAFETY: getpgid(0) is this process's own group; never signal it.
    let own = unsafe { libc::getpgid(0) };
    if pgid == own {
        return;
    }
    // SAFETY: a negative pid targets the process group; scalar args, no shared state.
    unsafe { libc::kill(-pgid, sig) };
}

/// The seat's DRM KMS device nodes (`/dev/dri/card*`). Render-only nodes
/// (`renderD*`) are excluded — they carry no DRM master / scanout, so a process
/// holding only those is not squatting the seat.
fn drm_card_nodes() -> Vec<PathBuf> {
    let mut nodes = Vec::new();
    if let Ok(entries) = fs::read_dir("/dev/dri") {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("card") {
                nodes.push(entry.path());
            }
        }
    }
    nodes
}

/// Pids with any of `cards` open, by scanning `/proc/<pid>/fd` symlinks — what
/// `fuser` does. doord's own pid is excluded (it never holds the card). Best
/// effort: pids that vanish or whose fd dir is unreadable mid-scan are skipped.
fn drm_card_holders(cards: &[PathBuf]) -> Vec<libc::pid_t> {
    let self_pid = std::process::id() as libc::pid_t;
    let mut holders = Vec::new();
    let Ok(procs) = fs::read_dir("/proc") else {
        return holders;
    };
    for entry in procs.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(target) = fs::read_link(fd.path()) {
                if cards.contains(&target) {
                    holders.push(pid);
                    break;
                }
            }
        }
    }
    holders
}

/// The daemon's main login loop (D-0008). When a greeter user is configured,
/// doord owns the greeter lifecycle: **greet** (launch the greeter), **serve**
/// the login, hand off (tear the greeter down before the session takes the VT),
/// wait for the session to end, and **re-greet**. Without a greeter user (dev),
/// it simply serves whatever greeter connects, forever.
///
/// Returns only on a fatal error setting up the listener; per-connection and
/// per-greeter errors are logged and the loop continues.
pub fn serve(
    config: &Config,
    logins: &dyn LoginFactory,
    spawner: Option<&Spawner>,
) -> io::Result<()> {
    let listener = bind(config)?;
    let manage_greeter = config.greeter_user.is_some();
    eprintln!(
        "doord: listening on {} (greeter uid {}, pam '{}', seat '{}' vt {:?}, managed greeter: {})",
        config.socket_path.display(),
        config.greeter_uid,
        config.pam_service,
        config.seat.seat,
        config.seat.vtnr,
        manage_greeter,
    );

    // When doord owns a real VT and the greeter, make an admin teardown leave a
    // usable console: a SIGTERM/SIGINT while at the greeter resets the VT on the
    // way out (the managed compositor dies with doord via its parent-death
    // signal but never restores the VT itself).
    if manage_greeter {
        if let Some(vtnr) = config.seat.vtnr {
            install_teardown_handler(vtnr);
        }
        // A panic must not leave the user's compositor squatting the seat's GPU.
        // The hook frees the seat (and restores the VT) before the process dies.
        install_panic_hook();
    }

    // Consecutive quick-failure streak; reset by any greeter that lasts a real
    // login. A sustained streak trips the give-up path rather than thrashing.
    let mut rapid_failures: u32 = 0;
    loop {
        // At the greeter: an admin teardown now should restore the VT (no live
        // session owns it). Cleared again the moment a session takes the seat.
        GREETING.store(true, Ordering::SeqCst);

        // Claim the seat before greeting: if a previous session's compositor is
        // still squatting the seat's DRM master (a doord crash/SIGKILL the
        // teardown path couldn't clean up, or a session that outlived its logind
        // session), the host could never take the master — it would die pre-connect
        // and trip the give-up path. Free the seat first so the greeter always
        // starts on a clean GPU. A no-op when nothing holds the card.
        if manage_greeter {
            free_seat();
        }

        // Greet: launch the greeter doord manages (re-greet on each loop). If the
        // launch fails, back off and retry rather than block on accept() with no
        // greeter to connect.
        let greeter = if manage_greeter {
            match launch_greeter(config, spawner) {
                Ok(handle) => {
                    match handle.pid() {
                        Some(pid) => eprintln!("doord: launched greeter (pid {pid})"),
                        None => eprintln!("doord: launched greeter"),
                    }
                    Some(RefCell::new(handle))
                }
                Err(e) => {
                    eprintln!("doord: could not launch greeter: {e}; retrying shortly");
                    rapid_failures += 1;
                    if rapid_failures >= GREETER_MAX_RAPID_FAILURES {
                        return give_up(config, rapid_failures);
                    }
                    thread::sleep(GREETER_RESPAWN_BACKOFF);
                    continue;
                }
            }
        } else {
            None
        };

        // Serve one greeter connection. Sequential by construction — one seat, one
        // greeter, no concurrency. While a greeter is managed, wait for it to
        // connect *and* watch the process: if it dies before connecting (the
        // pre-handshake wedge — e.g. the host can't take a held DRM master), don't
        // block on accept() forever; reset the VT and fall through to the
        // rapid-failure accounting so the streak trips the give-up path.
        let greeted_at = Instant::now();
        match accept_with_greeter_watch(&listener, greeter.as_ref()) {
            Ok(AcceptOutcome::Connected(stream)) => {
                if let Err(e) = handle_connection(stream, config, logins, spawner, greeter.as_ref())
                {
                    eprintln!("doord: connection ended: {e}");
                }
            }
            Ok(AcceptOutcome::GreeterDied) => {
                eprintln!(
                    "doord: greeter exited before connecting; resetting the VT and backing off"
                );
                if let Some(vtnr) = config.seat.vtnr {
                    restore_text_vt(vtnr);
                }
            }
            Err(e) => eprintln!("doord: accept failed: {e}"),
        }

        // Ensure the greeter is gone before re-greeting (idempotent: after a
        // login handoff it is already terminated; after a non-login disconnect or
        // a crash it is reaped here).
        if let Some(greeter) = greeter {
            greeter.borrow_mut().terminate();
            // Crash-loop guard: a managed greeter that barely lived (it never
            // carried a real login) must not be re-greeted in a tight spin. Count
            // the streak; once it crosses the threshold, give up (fail safe) so a
            // broken greeter can't wedge the GPU/VT by being relaunched forever.
            if greeted_at.elapsed() < GREETER_MIN_UPTIME {
                rapid_failures += 1;
                if rapid_failures >= GREETER_MAX_RAPID_FAILURES {
                    return give_up(config, rapid_failures);
                }
                eprintln!(
                    "doord: greeter exited quickly ({rapid_failures}/{GREETER_MAX_RAPID_FAILURES}); backing off before re-greet"
                );
                thread::sleep(GREETER_RESPAWN_BACKOFF);
            } else {
                // A greeter that carried a real login clears the streak.
                rapid_failures = 0;
            }
        }
    }
}

/// Abandon the login loop after the greeter has failed too many times in a row.
/// Restores the seat VT to a usable text console — a crashed or SIGKILLed
/// compositor leaves it in graphics mode with switching wedged — then returns so
/// the daemon exits *cleanly*. With `Type=simple` + `Restart=on-failure`, a clean
/// exit does **not** respawn: doord stops thrashing and the machine stays
/// recoverable from a text VT or SSH instead of locked out. The journal says why.
fn give_up(config: &Config, failures: u32) -> io::Result<()> {
    eprintln!(
        "doord: greeter failed {failures} times in quick succession; giving up to keep the \
         console usable. Check the greeter binary, the socket permissions, and the compositor, \
         then re-enable doord. Restoring the VT and exiting."
    );
    if let Some(vtnr) = config.seat.vtnr {
        restore_text_vt(vtnr);
    }
    Ok(())
}

/// Linux console/VT ioctls the `libc` crate does not expose for Linux. These are
/// stable kernel ABI (`<linux/kd.h>`, `<linux/vt.h>`).
mod vt_ioctl {
    pub const KDSETMODE: libc::Ioctl = 0x4B3A;
    pub const KD_TEXT: libc::c_int = 0x00;
    pub const VT_SETMODE: libc::Ioctl = 0x5602;
    pub const VT_ACTIVATE: libc::Ioctl = 0x5606;
    pub const VT_AUTO: libc::c_char = 0x00;

    /// Mirrors `struct vt_mode` from `<linux/vt.h>`.
    #[repr(C)]
    pub struct VtMode {
        pub mode: libc::c_char,
        pub waitv: libc::c_char,
        pub relsig: libc::c_short,
        pub acqsig: libc::c_short,
        pub frsig: libc::c_short,
    }
}

/// Restore VT `vtnr` to a switchable text console. A compositor (doorstep) that was
/// SIGKILLed never undoes its own VT setup: it leaves the VT in graphics mode
/// and — the part that actually freezes `Ctrl+Alt+Fn` — in process-controlled
/// switch mode (`VT_PROCESS`), where the kernel waits forever for a switch ack
/// from the dead process. We reset switching to kernel-driven (`VT_AUTO`) and the
/// console to text (`KD_TEXT`), then make the VT current so the user sees it.
/// doord runs as root, so it can open the VT; this is best-effort cleanup on the
/// give-up path, so a failure is logged, not fatal.
fn restore_text_vt(vtnr: u32) {
    let path = format!("/dev/tty{vtnr}");
    let c_path = match std::ffi::CString::new(path.clone()) {
        Ok(c) => c,
        Err(_) => return,
    };
    // SAFETY: c_path is a valid NUL-terminated path; open returns an owned fd or -1.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        eprintln!(
            "doord: could not open {path} to restore the console: {}",
            io::Error::last_os_error()
        );
        return;
    }
    // SAFETY: fd is a freshly opened VT we own; reset_vt_via_fd issues the VT
    // ioctls and we close fd exactly once after.
    unsafe {
        reset_vt_via_fd(fd, vtnr);
        libc::close(fd);
    }
}

/// Async-signal-safe variant of [`restore_text_vt`] for the teardown signal
/// handler: no allocation (the VT path is built on the stack), no stdio. Errors
/// are swallowed — a best-effort console restore on the way out.
///
/// # Safety
/// Must only call async-signal-safe operations (open/ioctl/close); used from a
/// signal handler.
fn restore_text_vt_raw(vtnr: u32) {
    // "/dev/tty" + up to 10 digits + NUL fits comfortably; vtnr is a small VT
    // number. Built in place so the handler allocates nothing.
    let mut buf = [0u8; 24];
    let path = vt_path_into(&mut buf, vtnr);
    // SAFETY: path points at a NUL-terminated C string in `buf` that outlives the
    // call; open/ioctl/close are async-signal-safe and fd is closed once.
    unsafe {
        let fd = libc::open(path, libc::O_RDWR | libc::O_NOCTTY);
        if fd < 0 {
            return;
        }
        reset_vt_via_fd(fd, vtnr);
        libc::close(fd);
    }
}

/// Write `"/dev/ttyN\0"` for VT `vtnr` into `buf` without allocating, returning a
/// pointer to it for `open(2)`. Async-signal-safe.
fn vt_path_into(buf: &mut [u8; 24], vtnr: u32) -> *const libc::c_char {
    const PREFIX: &[u8] = b"/dev/tty";
    let mut i = PREFIX.len();
    buf[..i].copy_from_slice(PREFIX);
    // Decimal digits of vtnr, least-significant first, then reversed into place.
    let mut digits = [0u8; 10];
    let mut d = 0;
    let mut n = vtnr;
    loop {
        digits[d] = b'0' + (n % 10) as u8;
        d += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    while d > 0 {
        d -= 1;
        buf[i] = digits[d];
        i += 1;
    }
    buf[i] = 0;
    buf.as_ptr() as *const libc::c_char
}

/// Issue the VT-restore ioctls on an already-open VT fd: kernel-driven switching
/// (`VT_AUTO`, undoing a dead compositor's `VT_PROCESS`), text mode (`KD_TEXT`),
/// and bring the VT to the foreground. Shared by [`restore_text_vt`] and
/// [`restore_text_vt_raw`].
///
/// # Safety
/// `fd` must be an open VT device fd owned by the caller; the caller closes it.
unsafe fn reset_vt_via_fd(fd: libc::c_int, vtnr: u32) {
    let auto = vt_ioctl::VtMode {
        mode: vt_ioctl::VT_AUTO,
        waitv: 0,
        relsig: 0,
        acqsig: 0,
        frsig: 0,
    };
    // SAFETY: each request matches its argument type per the kernel ABI
    // (VT_SETMODE takes a *const vt_mode; KDSETMODE and VT_ACTIVATE take an int).
    libc::ioctl(fd, vt_ioctl::VT_SETMODE, &auto as *const vt_ioctl::VtMode);
    libc::ioctl(fd, vt_ioctl::KDSETMODE, vt_ioctl::KD_TEXT);
    libc::ioctl(fd, vt_ioctl::VT_ACTIVATE, vtnr as libc::c_int);
}

/// A managed greeter process (the re-exec'd greeter worker, which runs
/// `doorstep -- door-greeter`), tracked so it can be torn down at the greeter→session
/// handoff and cleaned up before a re-greet. How it is tracked depends on who forked
/// it: in the direct/dev path it is the supervisor's own child; in the sandbox-split
/// path it is the spawner's child, so the supervisor watches its control-fd EOF
/// (the spawner reaps the pid) and signals it by pid.
struct GreeterHandle {
    backend: GreeterBackend,
}

enum GreeterBackend {
    /// Direct/dev: the greeter worker is the supervisor's child — wait/kill/reap it.
    Owned(Option<Child>),
    /// Sandbox split: the greeter worker is the *spawner's* child. `control`'s EOF
    /// signals its exit (it closes the fd when it exits); `pid` is retained to
    /// signal it at handoff. The spawner reaps the pid; the supervisor only watches
    /// the fd. `ended` latches once EOF is seen so later checks are cheap.
    Spawned {
        pid: libc::pid_t,
        control: UnixStream,
        ended: bool,
    },
}

impl GreeterHandle {
    /// Track a greeter worker forked directly by the supervisor (direct/dev path).
    fn owned(child: Child) -> Self {
        GreeterHandle {
            backend: GreeterBackend::Owned(Some(child)),
        }
    }

    /// Track a greeter worker forked by the spawner: observe its exit as EOF on the
    /// control fd (set non-blocking so [`reap_if_exited`](Self::reap_if_exited) can
    /// poll it), and keep its pid to signal it at handoff.
    fn spawned(pid: libc::pid_t, control: UnixStream) -> io::Result<Self> {
        control.set_nonblocking(true)?;
        Ok(GreeterHandle {
            backend: GreeterBackend::Spawned {
                pid,
                control,
                ended: false,
            },
        })
    }

    /// The greeter worker's pid, if known (for journaling at launch).
    fn pid(&self) -> Option<libc::pid_t> {
        match &self.backend {
            GreeterBackend::Owned(child) => child.as_ref().map(|c| c.id() as libc::pid_t),
            GreeterBackend::Spawned { pid, .. } => Some(*pid),
        }
    }

    /// Non-blocking check for a greeter that exited *before* connecting. Reports
    /// `true` once it has exited (or was already gone). Used by the greeter-watch so
    /// a pre-handshake death is noticed instead of hanging the daemon on `accept()`.
    fn reap_if_exited(&mut self) -> bool {
        match &mut self.backend {
            GreeterBackend::Owned(slot) => {
                let Some(child) = slot.as_mut() else {
                    return true;
                };
                match child.try_wait() {
                    Ok(Some(_)) => {
                        *slot = None;
                        true
                    }
                    Ok(None) => false,
                    // An errored wait can't be retried meaningfully; treat it as gone
                    // so the loop stops waiting rather than spinning on a broken handle.
                    Err(e) => {
                        eprintln!("doord: checking the greeter failed: {e}; treating it as exited");
                        *slot = None;
                        true
                    }
                }
            }
            GreeterBackend::Spawned { control, ended, .. } => {
                if *ended {
                    return true;
                }
                if control_hit_eof(control) {
                    *ended = true;
                    return true;
                }
                false
            }
        }
    }

    /// Terminate the greeter and **wait for it to exit**, so the seat's VT/DRM is
    /// released before the session takes it. `SIGTERM` first (the host releases the
    /// seat and exits cleanly), escalating to `SIGKILL` if it lingers. Idempotent:
    /// a second call (or one after the greeter already exited) just returns.
    fn terminate(&mut self) {
        match &mut self.backend {
            GreeterBackend::Owned(slot) => {
                let Some(mut child) = slot.take() else {
                    return;
                };
                let pid = child.id() as libc::pid_t;
                // SAFETY: pid is this child's; SIGTERM is a request to exit.
                unsafe { libc::kill(pid, libc::SIGTERM) };

                let deadline = Instant::now() + GREETER_TERM_GRACE;
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => return,
                        Ok(None) if Instant::now() < deadline => {
                            thread::sleep(Duration::from_millis(10))
                        }
                        Ok(None) => break,
                        Err(e) => {
                            eprintln!("doord: waiting on the greeter failed: {e}");
                            return;
                        }
                    }
                }
                // Still alive after the grace period — force it.
                // SAFETY: pid is this child's; it has not been reaped (try_wait
                // returned None).
                unsafe { libc::kill(pid, libc::SIGKILL) };
                let _ = child.wait();
            }
            GreeterBackend::Spawned {
                pid,
                control,
                ended,
            } => {
                if *ended {
                    return;
                }
                let pid = *pid;
                // SAFETY: pid is the spawner-forked greeter worker's; SIGTERM asks it
                // to exit (it forwards to doorstep, which releases the seat, then closes
                // its logind session). We observe completion as EOF on `control`.
                unsafe { libc::kill(pid, libc::SIGTERM) };
                if wait_for_control_eof(control, GREETER_TERM_GRACE) {
                    *ended = true;
                    return;
                }
                // Still alive after the grace period — force it. The spawner reaps.
                // SAFETY: pid is that greeter worker's; EOF has not been seen.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                wait_for_control_eof(control, GREETER_TERM_GRACE);
                *ended = true;
            }
        }
    }
}

/// Non-blocking check whether a control socket has hit EOF (the far process closed
/// it — i.e. exited). A hard read error is also treated as EOF so the caller stops
/// waiting on a broken fd. Requires the socket to be non-blocking.
fn control_hit_eof(control: &mut UnixStream) -> bool {
    use std::io::Read;
    let mut sink = [0u8; 64];
    match control.read(&mut sink) {
        Ok(0) => true,
        Ok(_) => false, // stray bytes (none are expected); still alive
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => false,
        Err(_) => true,
    }
}

/// Block until `control` hits EOF (the far process exited) or `timeout` elapses,
/// polling the fd rather than busy-spinning. Returns `true` if EOF was seen.
fn wait_for_control_eof(control: &mut UnixStream, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if control_hit_eof(control) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        poll_readable(control.as_raw_fd(), deadline - now);
    }
}

/// The result of waiting for a greeter connection while watching the greeter
/// process.
enum AcceptOutcome {
    /// The greeter connected; serve it.
    Connected(UnixStream),
    /// The greeter process exited before connecting (the pre-handshake wedge).
    GreeterDied,
}

/// Accept the greeter connection, but — when doord manages the greeter — do not
/// block on `accept()` indefinitely: concurrently watch the greeter process so a
/// pre-handshake death (it never connects) is detected and reported rather than
/// hanging the daemon forever (RC: the host failing to take a held DRM master would
/// otherwise wedge doord silently). Without a managed greeter (dev) there is no
/// process to watch, so it just blocks on `accept()`.
fn accept_with_greeter_watch(
    listener: &UnixListener,
    greeter: Option<&RefCell<GreeterHandle>>,
) -> io::Result<AcceptOutcome> {
    let Some(greeter) = greeter else {
        let (stream, _) = listener.accept()?;
        return Ok(AcceptOutcome::Connected(stream));
    };

    // Non-blocking accept + a bounded poll so we alternate between "did the
    // greeter connect?" and "is the greeter still alive?" without busy-spinning.
    // A socket accepted from a non-blocking listener is itself blocking on Linux
    // (O_NONBLOCK is not inherited through accept), so handle_connection's reads
    // behave normally.
    listener.set_nonblocking(true)?;
    let outcome = loop {
        match listener.accept() {
            Ok((stream, _)) => break Ok(AcceptOutcome::Connected(stream)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if greeter.borrow_mut().reap_if_exited() {
                    break Ok(AcceptOutcome::GreeterDied);
                }
                // Sleep until the listener is readable or the watch interval
                // elapses, then re-check the greeter. EINTR (a teardown signal)
                // also wakes us; the handler terminates the process, so we never
                // return to a stale wait.
                poll_readable(listener.as_raw_fd(), GREETER_WATCH_INTERVAL);
            }
            Err(e) => break Err(e),
        }
    };
    // Restore blocking semantics for the next iteration's dev/no-greeter path and
    // for cleanliness; ignore errors on a listener we are about to reuse anyway.
    let _ = listener.set_nonblocking(false);
    outcome
}

/// Block until `fd` is readable or `timeout` elapses. Best-effort: a poll error
/// or an `EINTR` wake simply returns, and the caller re-checks its conditions.
fn poll_readable(fd: libc::c_int, timeout: Duration) {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
    // SAFETY: a single valid pollfd is passed with count 1; poll writes only
    // revents. Any return value is acceptable — the caller re-checks state.
    unsafe {
        libc::poll(&mut pfd, 1, millis);
    }
}

/// Launch the greeter worker (D-0008). The greeter worker inherits the daemon's
/// environment (the `DOORD_*` config), which is how it learns the greeter user,
/// seat/VT, PAM service, and command.
///
/// With a **spawner** present (the sandbox split), the greeter worker is forked by
/// the pre-forked spawner — outside the supervisor's sandboxed lineage, so a future
/// supervisor seccomp/Landlock profile never confines the greeter compositor — and
/// its control fd is handed back so the supervisor observes its exit as EOF. Without
/// one (direct/dev), the supervisor forks it as its own child and tracks it by pid.
fn launch_greeter(config: &Config, spawner: Option<&Spawner>) -> io::Result<GreeterHandle> {
    let _ = config; // configuration travels via the inherited environment
    if let Some(spawner) = spawner {
        let child = spawner
            .request_greeter()
            .map_err(|e| io::Error::other(format!("spawner could not start the greeter: {e}")))?;
        return GreeterHandle::spawned(child.pid, child.control);
    }
    let child = Command::new("/proc/self/exe")
        .arg(worker::GREETER_WORKER_ARG)
        .spawn()?;
    Ok(GreeterHandle::owned(child))
}

/// Create the socket directory, remove any stale socket, bind, and lock down
/// permissions and ownership.
fn bind(config: &Config) -> io::Result<UnixListener> {
    let path = &config.socket_path;

    if let Some(dir) = path.parent() {
        let created = !dir.exists();
        if created {
            fs::create_dir_all(dir)?;
        }
        // The greeter runs as a *different*, unprivileged user, so it must be able
        // to traverse this directory to reach the socket inside it. The parent
        // dir's mode gates the path *before* the socket's own 0660 is consulted:
        // a 0700 root:root dir — which the unit's `RuntimeDirectory` pre-creates —
        // makes the socket unreachable no matter how the socket itself is chmod'd.
        // So when the greeter group is known (production) we group-own the dir to
        // it with group-search (0750), applied whether or not we created it
        // (RuntimeDirectory runs before us). In a dev run with no greeter group we
        // keep a dir we created private and leave a pre-existing shared dir (e.g.
        // /tmp) exactly as its owner set it.
        match config.greeter_gid {
            Some(gid) => {
                chown_group(dir, gid).unwrap_or_else(|e| {
                    eprintln!(
                        "doord: warning: could not chgrp {} to gid {gid}: {e}",
                        dir.display()
                    );
                });
                fs::set_permissions(dir, fs::Permissions::from_mode(0o750))?;
            }
            None if created => {
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            }
            None => {}
        }
    }

    // A leftover socket from a previous run would make bind() fail with EADDRINUSE.
    if path.exists() {
        fs::remove_file(path)?;
    }

    let listener = UnixListener::bind(path)?;

    // 0660: owner (root) and the greeter group, nobody else.
    fs::set_permissions(path, fs::Permissions::from_mode(0o660))?;

    // Hand group ownership to the greeter group when we know it and can do it
    // (root). Best-effort: in a non-root dev run this is skipped, and the
    // peercred check below is what actually enforces who may connect.
    if let Some(gid) = config.greeter_gid {
        chown_group(path, gid).unwrap_or_else(|e| {
            eprintln!("doord: warning: could not chgrp the socket to gid {gid}: {e}");
        });
    }

    Ok(listener)
}

/// Serve one greeter: authorize it, run the handshake, then answer requests
/// until it disconnects.
fn handle_connection(
    stream: UnixStream,
    config: &Config,
    logins: &dyn LoginFactory,
    spawner: Option<&Spawner>,
    managed_greeter: Option<&RefCell<GreeterHandle>>,
) -> Result<(), FrameError> {
    let cred = peer_cred(&stream)?;
    if cred.uid != config.greeter_uid {
        // Wrong peer: refuse before reading a single request byte.
        eprintln!(
            "doord: rejecting connection from uid {} (pid {}); only greeter uid {} is allowed",
            cred.uid, cred.pid, config.greeter_uid
        );
        return Ok(());
    }
    eprintln!(
        "doord: greeter connected (pid {}, uid {})",
        cred.pid, cred.uid
    );

    // Bound the handshake: a freshly-connected greeter must send its `Hello`
    // promptly (it is machine-paced, not waiting on a human). Every subsequent
    // greeter read goes through `read_greeter_request`, which manages its own
    // idle-vs-transfer deadline and overrides this.
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let mut conn = stream;

    if !handshake(&mut conn)? {
        return Ok(());
    }

    // The per-connection login owns the PAM transaction (auth → session). It is
    // handed its own clone of the connection so its PAM conversation can prompt
    // the greeter without contending with this loop's request reads (the two
    // never read concurrently: the loop is blocked inside `authenticate` while
    // the conversation runs). `Start` is honored only once the login reports an
    // authenticated user — bound by a completed PAM success, never by anything
    // the greeter names — and the login dies with the connection that earned it.
    let greeter = match conn.try_clone() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("doord: could not clone greeter connection for PAM: {e}");
            return Ok(());
        }
    };
    let mut login = logins.begin(greeter, spawner);

    loop {
        let request: Request = match read_greeter_request(&conn) {
            Ok(req) => req,
            // A clean disconnect surfaces as EOF on read_exact.
            Err(FrameError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                eprintln!("doord: greeter disconnected");
                return Ok(());
            }
            Err(e) => {
                // Never echo the offending bytes (they may be a mistyped secret).
                eprintln!("doord: dropping greeter after frame error: {e}");
                return Ok(());
            }
        };

        // BeginAuth and Start each do more than a single request → response:
        // BeginAuth runs a multi-step PAM conversation over the connection, and
        // Start launches and then owns the session. The rest are stateless.
        match request {
            Request::BeginAuth { username } => {
                match run_auth(&mut conn, login.as_mut(), &username)? {
                    AuthFlow::Continue => {}
                    AuthFlow::Break => return Ok(()),
                }
            }
            Request::Start { session_id } => {
                // The session-start gate: no auth, no spawn. A compromised greeter
                // cannot reach the privilege handoff without first driving a real
                // PAM success, and then only for the user that success bound.
                if login.user().is_none() {
                    eprintln!("doord: refusing Start: no authenticated user on this connection");
                    write_frame(
                        &mut conn,
                        &Response::Error {
                            message: "authenticate before starting a session".to_string(),
                        },
                    )?;
                } else if run_start(
                    &mut conn,
                    config,
                    login.as_mut(),
                    managed_greeter,
                    &session_id,
                )?
                .is_started()
                {
                    // The session is running and the greeter has stepped aside;
                    // this connection's work is done.
                    return Ok(());
                }
            }
            other => {
                let response = dispatch(&other, config);
                write_frame(&mut conn, &response)?;
            }
        }
    }
}

/// Outcome of an authentication conversation, as it affects the connection. The
/// authenticated identity itself lives in the login, not here — the loop only
/// needs to know whether to keep serving.
enum AuthFlow {
    /// Authenticated, or a retryable failure/cancel; keep serving. On success the
    /// login now reports a user, which the `Start` gate reads.
    Continue,
    /// The greeter vanished mid-conversation; tear the connection down.
    Break,
}

/// Outcome of a `Start` request.
enum StartFlow {
    /// The session launched; the connection is finished.
    Started,
    /// The launch was refused (unknown session, spawn error); keep serving.
    Failed,
}

impl StartFlow {
    fn is_started(&self) -> bool {
        matches!(self, StartFlow::Started)
    }
}

/// Run one authentication conversation to its terminal response. The [`Login`]
/// pumps prompts and replies over its own clone of the connection; we only send
/// the final verdict here, padding the failure path to [`MIN_AUTH_FAILURE`] so a
/// fast rejection can't be timed. On success the login is now bound to the user,
/// which the `Start` gate reads — the loop keeps serving either way.
fn run_auth(
    conn: &mut UnixStream,
    login: &mut dyn Login,
    username: &str,
) -> Result<AuthFlow, FrameError> {
    let started = Instant::now();
    let outcome = login.authenticate(username);

    match outcome {
        AuthOutcome::Success => {
            eprintln!("doord: authentication succeeded for '{username}'");
            write_frame(conn, &Response::AuthSuccess)?;
            Ok(AuthFlow::Continue)
        }
        AuthOutcome::Failure => {
            pad_failure(started);
            write_frame(
                conn,
                &Response::AuthFailure {
                    reason: "Authentication failed".to_string(),
                },
            )?;
            Ok(AuthFlow::Continue)
        }
        AuthOutcome::Cancelled => {
            write_frame(
                conn,
                &Response::AuthFailure {
                    reason: "Authentication cancelled".to_string(),
                },
            )?;
            Ok(AuthFlow::Continue)
        }
        AuthOutcome::Transport => {
            // The greeter is gone; nothing left to reply to.
            eprintln!("doord: greeter vanished mid-authentication");
            Ok(AuthFlow::Break)
        }
    }
}

/// Launch the chosen session for the already-authenticated login, performing the
/// greeter→session VT handoff (D-0008). Resolves the session id against current
/// discovery; if doord manages the greeter, it **tears the greeter down and waits
/// for the VT to free before** the session takes it; then it hands off to the
/// [`Login`] (which opens the logind session, drops privilege, execs) and owns the
/// running session until it exits. An unknown session is refused without touching
/// the greeter (the greeter keeps serving so the user can choose again).
fn run_start(
    conn: &mut UnixStream,
    config: &Config,
    login: &mut dyn Login,
    managed_greeter: Option<&RefCell<GreeterHandle>>,
    session_id: &str,
) -> Result<StartFlow, FrameError> {
    // For journaling only; the launch binds to the login's authenticated user,
    // not to anything derived from the request.
    let username = login.user().unwrap_or("?").to_string();

    // Re-discover so the picker and the launch agree on the same id, even if the
    // installed set changed while the greeter was up. The Exec stays daemon-side.
    let session = crate::sessions::discover(&config.session_dirs)
        .into_iter()
        .find(|s| s.id == session_id);

    let session = match session {
        Some(session) => session,
        None => {
            // Greeter is untouched — it keeps serving so the user can choose again.
            eprintln!("doord: refusing Start: no session with id '{session_id}'");
            write_frame(
                conn,
                &Response::Error {
                    message: "no such session".to_string(),
                },
            )?;
            return Ok(StartFlow::Failed);
        }
    };

    if let Some(greeter) = managed_greeter {
        // Managed handoff: free the VT before the session takes it. Tearing the
        // greeter down also drops this connection (we are serving the greeter) —
        // that is intended; its job is done. We therefore send the greeter
        // nothing and drive the session over the login's control channel.
        eprintln!(
            "doord: handoff '{username}' → session '{}': tearing down greeter, freeing the VT",
            session.id
        );
        greeter.borrow_mut().terminate();

        // The session is about to take the VT and owns it for its lifetime. From
        // here an admin teardown must *not* reset the VT out from under the live
        // compositor (and that session, having no parent-death signal, survives a
        // doord restart). The loop re-arms GREETING when it re-greets.
        GREETING.store(false, Ordering::SeqCst);

        match login.start(&session, &config.seat) {
            Ok(child) => {
                eprintln!("doord: started session '{}' for '{username}'", session.id);
                // Own the session until it ends — but stay responsive to an admin
                // teardown: a SIGTERM/SIGINT here means the seat must be freed for
                // the next login manager, which can't be done from the signal
                // handler. wait_for_session polls for both.
                wait_for_session(child, &config.seat, &session.id);
            }
            Err(e) => {
                // The greeter is already gone; the loop re-greets after we return.
                eprintln!("doord: session start failed after handoff for '{username}': {e}");
            }
        }
        // The connection (greeter) is finished either way; the loop re-greets.
        return Ok(StartFlow::Started);
    }

    // Unmanaged (dev): no greeter to tear down. Tell the greeter it started, then
    // own the session; a failure keeps the connection serving.
    match login.start(&session, &config.seat) {
        Ok(child) => {
            eprintln!("doord: started session '{}' for '{username}'", session.id);
            write_frame(conn, &Response::Started)?;
            match child.wait() {
                Ok(Some(status)) => {
                    eprintln!("doord: session '{}' exited ({status})", session.id)
                }
                Ok(None) => {}
                Err(e) => eprintln!("doord: waiting on session '{}' failed: {e}", session.id),
            }
            Ok(StartFlow::Started)
        }
        Err(e) => {
            eprintln!(
                "doord: could not start session '{}' for '{username}': {e}",
                session.id
            );
            write_frame(
                conn,
                &Response::Error {
                    message: "could not start the session".to_string(),
                },
            )?;
            Ok(StartFlow::Failed)
        }
    }
}

/// Sleep until at least [`MIN_AUTH_FAILURE`] has elapsed since `started`. Shared with
/// the reauth path so a denied reauth is padded exactly like a failed login — a fast
/// rejection must not be timeable into an account-existence oracle.
pub(crate) fn pad_failure(started: Instant) {
    let elapsed = started.elapsed();
    if elapsed < MIN_AUTH_FAILURE {
        thread::sleep(MIN_AUTH_FAILURE - elapsed);
    }
}

/// The mandatory first exchange: the greeter must send [`Request::Hello`] and
/// declare a version this daemon can speak. Anything else, or an incompatible
/// version, ends the connection. Returns `true` if the handshake succeeded.
fn handshake(conn: &mut UnixStream) -> Result<bool, FrameError> {
    let first: Request = read_frame(conn)?;
    match first {
        Request::Hello { protocol_version } if protocol_version == PROTOCOL_VERSION => {
            write_frame(
                conn,
                &Response::Welcome {
                    protocol_version: PROTOCOL_VERSION,
                },
            )?;
            Ok(true)
        }
        Request::Hello { protocol_version } => {
            eprintln!(
                "doord: rejecting greeter speaking protocol v{protocol_version}; this daemon speaks v{PROTOCOL_VERSION}"
            );
            write_frame(
                conn,
                &Response::Incompatible {
                    daemon_protocol_version: PROTOCOL_VERSION,
                },
            )?;
            Ok(false)
        }
        _ => {
            // A credential or any other request before Hello is a protocol
            // violation; refuse without acting on it.
            eprintln!("doord: greeter sent a request before the handshake; closing");
            write_frame(
                conn,
                &Response::Error {
                    message: "handshake required before any other request".to_string(),
                },
            )?;
            Ok(false)
        }
    }
}

/// Turn a (post-handshake) request into a response. The remaining privileged
/// actions (spawn, power) are not implemented yet; until they land, every
/// request that would touch them gets a clear, non-leaky refusal.
fn dispatch(request: &Request, config: &Config) -> Response {
    match request {
        // A second Hello is harmless; re-acknowledge.
        Request::Hello { .. } => Response::Welcome {
            protocol_version: PROTOCOL_VERSION,
        },
        Request::ListSessions => {
            // Re-scanned per request so a session installed while the greeter is
            // up appears without restarting the daemon. Only the greeter-facing
            // projection crosses the seam — the `Exec` stays in the daemon.
            let sessions = crate::sessions::discover(&config.session_dirs)
                .iter()
                .map(crate::sessions::DiscoveredSession::to_wire)
                .collect();
            Response::Sessions(sessions)
        }
        // BeginAuth and Start are handled by the stateful connection loop, not
        // here; reaching this arm is an internal routing bug.
        Request::BeginAuth { .. } | Request::Start { .. } => Response::Error {
            message: "internal: stateful request reached the stateless dispatch".to_string(),
        },
        // A reply or cancel with no conversation in progress is a stray message.
        Request::AuthReply { .. } | Request::CancelAuth => Response::Error {
            message: "no authentication in progress".to_string(),
        },
        Request::Power(_) => Response::Error {
            message: "not yet available: the privileged core is still being built".to_string(),
        },
    }
}

/// Read the peer's kernel-attested credentials via `SO_PEERCRED`. The uid here
/// is asserted by the kernel, not by anything the greeter sent, which is what
/// makes it a trustworthy authorization signal. Shared with the reauth listener,
/// where the same kernel-attested uid *is* the identity being reauthenticated.
pub(crate) fn peer_cred(stream: &UnixStream) -> io::Result<libc::ucred> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `cred`, which is a
    // correctly sized `ucred`; `len` is updated in place to the bytes written.
    let ret = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred)
}

/// `chgrp` the socket to `gid`, keeping the owner unchanged (`uid == -1`).
fn chown_group(path: &Path, gid: u32) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket path has a NUL byte"))?;
    // SAFETY: c_path is a valid NUL-terminated C string for the call's duration.
    let ret = unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SeatTarget;
    use crate::pam::testing::ScriptedLoginFactory;
    use protocol::{AuthPrompt, Secret};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    fn test_config() -> Config {
        Config {
            socket_path: PathBuf::from("/unused-in-pair-test.sock"),
            reauth_socket_path: PathBuf::from("/unused-in-pair-test-reauth.sock"),
            session_dirs: Vec::new(),
            // A socketpair reports the creating process's creds on SO_PEERCRED,
            // so authorize our own uid.
            greeter_uid: unsafe { libc::getuid() },
            greeter_gid: None,
            pam_service: "unused-in-pair-test".to_string(),
            seat: SeatTarget {
                seat: "seat0".to_string(),
                vtnr: None,
            },
            // Tests drive the connection directly; no managed greeter.
            greeter_user: None,
            greeter_pam_service: "unused".to_string(),
            greeter_cmd: vec!["true".to_string()],
        }
    }

    /// A scripted login factory accepting `hunter2`, recording launches into the
    /// shared log the test thread reads — so the connection's auth-gating and
    /// identity-binding can be asserted without privilege, PAM, or a real spawn.
    fn scripted_logins(calls: Arc<Mutex<Vec<(String, String)>>>) -> ScriptedLoginFactory {
        ScriptedLoginFactory {
            password: "hunter2".to_string(),
            calls,
        }
    }

    static SESSION_ROOT_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Create a throwaway data-dir root holding one session with the given id,
    /// returned as a one-element `session_dirs`. Leaked (not cleaned) — these are
    /// tiny and live under the temp dir; keeping the helper trivial matters more.
    fn session_dir_with(id: &str) -> Vec<PathBuf> {
        let n = SESSION_ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("doord-ipc-sesstest-{}-{}", std::process::id(), n));
        let wayland = root.join("wayland-sessions");
        std::fs::create_dir_all(&wayland).unwrap();
        std::fs::write(
            wayland.join(format!("{id}.desktop")),
            format!("[Desktop Entry]\nName={id}\nExec=/usr/bin/{id}\n"),
        )
        .unwrap();
        vec![root]
    }

    /// Drive a full handshake + auth conversation over an in-process socketpair
    /// against the scripted authenticator, returning the terminal response to
    /// the supplied password.
    fn run_conversation(password_typed: &str) -> Response {
        let (mut client, server) = UnixStream::pair().unwrap();
        let typed = password_typed.to_string();
        let handle = thread::spawn(move || {
            let cfg = test_config();
            let logins = scripted_logins(Arc::new(Mutex::new(Vec::new())));
            let _ = handle_connection(server, &cfg, &logins, None, None);
            let _ = typed; // captured to keep the closure's intent explicit
        });

        write_frame(
            &mut client,
            &Request::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        assert_eq!(
            read_frame::<_, Response>(&mut client).unwrap(),
            Response::Welcome {
                protocol_version: PROTOCOL_VERSION
            }
        );

        write_frame(
            &mut client,
            &Request::BeginAuth {
                username: "stephen".to_string(),
            },
        )
        .unwrap();
        // The conversation's first step is the password prompt.
        assert_eq!(
            read_frame::<_, Response>(&mut client).unwrap(),
            Response::Auth(AuthPrompt::Question {
                text: "Password:".to_string(),
                secret: true,
            })
        );

        write_frame(
            &mut client,
            &Request::AuthReply {
                response: Secret::new(password_typed.to_string()),
            },
        )
        .unwrap();
        let terminal: Response = read_frame(&mut client).unwrap();

        drop(client);
        handle.join().unwrap();
        terminal
    }

    /// Read back the NUL-terminated path `vt_path_into` wrote into `buf`.
    fn vt_path_string(vtnr: u32) -> String {
        let mut buf = [0u8; 24];
        let _ = vt_path_into(&mut buf, vtnr);
        let nul = buf.iter().position(|&b| b == 0).unwrap();
        String::from_utf8(buf[..nul].to_vec()).unwrap()
    }

    #[test]
    fn drm_card_nodes_are_kms_not_render() {
        // Read-only: whatever /dev/dri holds (or nothing, in a sandbox), every
        // returned node must be a `card*` KMS node and never a render-only
        // `renderD*` node — a process holding only the latter is not squatting the
        // seat, so it must never be a free_seat() target.
        for node in drm_card_nodes() {
            let name = node.file_name().unwrap().to_string_lossy().into_owned();
            assert!(name.starts_with("card"), "unexpected non-card node: {name}");
            assert!(
                !name.starts_with("renderD"),
                "render node leaked in: {name}"
            );
        }
    }

    #[test]
    fn vt_path_matches_format_without_allocating() {
        // The async-signal-safe hand-rolled path must equal the obvious format
        // for the VT numbers doord actually sees (single- and multi-digit).
        for vtnr in [0u32, 1, 2, 7, 12, 63] {
            assert_eq!(vt_path_string(vtnr), format!("/dev/tty{vtnr}"));
        }
    }

    #[test]
    fn correct_password_authenticates() {
        assert_eq!(run_conversation("hunter2"), Response::AuthSuccess);
    }

    #[test]
    fn wrong_password_is_refused() {
        assert!(matches!(
            run_conversation("wrong"),
            Response::AuthFailure { .. }
        ));
    }

    #[test]
    fn cancel_during_conversation_ends_it() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let handle = thread::spawn(move || {
            let cfg = test_config();
            let logins = scripted_logins(Arc::new(Mutex::new(Vec::new())));
            let _ = handle_connection(server, &cfg, &logins, None, None);
        });

        write_frame(
            &mut client,
            &Request::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        let _welcome: Response = read_frame(&mut client).unwrap();

        write_frame(
            &mut client,
            &Request::BeginAuth {
                username: "stephen".to_string(),
            },
        )
        .unwrap();
        let _prompt: Response = read_frame(&mut client).unwrap();

        // Cancel instead of answering.
        write_frame(&mut client, &Request::CancelAuth).unwrap();
        let terminal: Response = read_frame(&mut client).unwrap();
        assert!(matches!(terminal, Response::AuthFailure { .. }));

        drop(client);
        handle.join().unwrap();
    }

    #[test]
    fn start_before_auth_is_refused_and_never_launches() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let calls_in = calls.clone();
        let handle = thread::spawn(move || {
            let cfg = Config {
                session_dirs: session_dir_with("hyprland"),
                ..test_config()
            };
            let logins = scripted_logins(calls_in);
            let _ = handle_connection(server, &cfg, &logins, None, None);
        });

        // Handshake, then jump straight to Start without authenticating.
        write_frame(
            &mut client,
            &Request::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        let _welcome: Response = read_frame(&mut client).unwrap();

        write_frame(
            &mut client,
            &Request::Start {
                session_id: "hyprland".to_string(),
            },
        )
        .unwrap();
        let resp: Response = read_frame(&mut client).unwrap();
        assert!(
            matches!(resp, Response::Error { .. }),
            "Start without a prior auth must be refused, got {resp:?}"
        );

        drop(client);
        handle.join().unwrap();

        // The privilege handoff was never reached.
        assert!(
            calls.lock().unwrap().is_empty(),
            "no session may be launched without authentication"
        );
    }

    #[test]
    fn start_after_auth_launches_the_chosen_session_as_the_authed_user() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let calls_in = calls.clone();
        let handle = thread::spawn(move || {
            let cfg = Config {
                session_dirs: session_dir_with("hyprland"),
                ..test_config()
            };
            let logins = scripted_logins(calls_in);
            let _ = handle_connection(server, &cfg, &logins, None, None);
        });

        write_frame(
            &mut client,
            &Request::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        let _welcome: Response = read_frame(&mut client).unwrap();

        // Authenticate as "stephen".
        write_frame(
            &mut client,
            &Request::BeginAuth {
                username: "stephen".to_string(),
            },
        )
        .unwrap();
        let _prompt: Response = read_frame(&mut client).unwrap();
        write_frame(
            &mut client,
            &Request::AuthReply {
                response: Secret::new("hunter2".to_string()),
            },
        )
        .unwrap();
        assert_eq!(
            read_frame::<_, Response>(&mut client).unwrap(),
            Response::AuthSuccess
        );

        // Now Start succeeds and the daemon reports the session launched.
        write_frame(
            &mut client,
            &Request::Start {
                session_id: "hyprland".to_string(),
            },
        )
        .unwrap();
        assert_eq!(
            read_frame::<_, Response>(&mut client).unwrap(),
            Response::Started
        );

        drop(client);
        handle.join().unwrap();

        // Exactly the chosen session was launched, bound to the authenticated
        // user — not anything the greeter could otherwise have named.
        assert_eq!(
            *calls.lock().unwrap(),
            vec![("hyprland".to_string(), "stephen".to_string())]
        );
    }
}
