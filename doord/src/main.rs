//! doord — the privileged door daemon.
//!
//! This is the trusted computing base. It is the only part of door that runs
//! with privilege, and it owns every privileged responsibility end to end:
//!
//!   - the PAM authentication conversation,
//!   - seat / VT ownership and session bookkeeping via logind,
//!   - discovering startable sessions and spawning the chosen one with a
//!     sanitized environment,
//!   - the local IPC socket that the unprivileged greeter connects to.
//!
//! Everything arriving from the greeter is untrusted: the daemon validates it,
//! acts only on requests it permits, and drops privilege as early as it can.
//! Keeping this binary small is a security property, not a style preference —
//! every line here is attack surface that runs as root.
//!
//! Built so far: process hardening baseline; the authorized, framed IPC seam;
//! the PAM auth conversation; session discovery; and the full session handoff —
//! one PAM transaction spanning auth → logind session open (`pam_systemd`) →
//! fork → seat/VT controlling-tty handoff → privilege drop → exec, with the
//! logind session closed when it exits. The end-to-end live logind run on real
//! hardware is the remaining verification.

mod config;
mod fdpass;
mod hardening;
mod ipc;
mod pam;
mod privdrop;
mod reauth;
mod sessions;
mod spawn;
// M5 sandbox groundwork: the pre-forked spawner + its supervisor-side helpers. It
// creates both the session worker and the greeter worker outside the supervisor's
// lineage so a future supervisor self-sandbox never confines the desktop or the
// greeter compositor. Enabled by default via DOORD_SPAWNER (the shipped unit sets
// it); unset it to fall back to the direct in-lineage path.
mod spawner;
mod user;
mod worker;

use std::process::ExitCode;

use config::Config;
use pam::WorkerLoginFactory;

fn main() -> ExitCode {
    // Re-exec modes (the daemon forks itself into these): the per-login session
    // worker (D-0005) holds the PAM transaction and is the logind session leader;
    // the greeter worker (D-0008) holds a passwordless greeter session and runs
    // the greeter host. Neither opens the IPC listener.
    match std::env::args().nth(1).as_deref() {
        Some(worker::WORKER_ARG) => return worker::main(),
        Some(worker::GREETER_WORKER_ARG) => return worker::run_greeter(),
        _ => {}
    }

    // Lock down the process before opening any attack surface.
    hardening::apply_baseline();

    let config = Config::from_env();

    // Child creation: with DOORD_SPAWNER (the shipped default), a spawner is forked
    // here — after the baseline but before serving — that owns creation of both the
    // greeter worker and the per-login session worker, so a future supervisor
    // self-sandbox is never inherited by the desktop or the greeter compositor.
    // Fails open to the direct in-lineage path if the spawner can't be forked; unset
    // the flag to select that path deliberately.
    let spawner = if std::env::var_os("DOORD_SPAWNER").is_some() {
        match spawner::fork_spawner(pam::spawn_raw) {
            Ok(sock) => {
                eprintln!(
                    "doord: spawner mode — the greeter and session workers are created by a \
                     pre-forked helper"
                );
                Some(spawner::Spawner::new(sock))
            }
            Err(e) => {
                eprintln!(
                    "doord: could not fork the spawner ({e}); using the direct in-lineage path"
                );
                None
            }
        }
    } else {
        None
    };
    // The session-lock reauth listener's PAM runs in workers forked off a *dedicated*
    // reauth spawner (production) — outside the supervisor's sandbox, exactly like the
    // login path — or directly (dev). Fork that spawner here, while the process is still
    // single-threaded and before any sandbox: it forks/execs, which the supervisor's own
    // seccomp filter forbids. (The listener thread itself is spawned further below —
    // after Landlock, before seccomp — so it inherits the Landlock domain and is then
    // covered by the TSYNC seccomp install; see there.)
    let reauth_spawner: std::sync::Arc<std::sync::Mutex<Option<spawner::Spawner>>> =
        if spawner.is_some() {
            match spawner::fork_spawner(pam::spawn_raw) {
                Ok(sock) => {
                    std::sync::Arc::new(std::sync::Mutex::new(Some(spawner::Spawner::new(sock))))
                }
                Err(e) => {
                    eprintln!(
                        "doord: could not fork the reauth spawner ({e}); reauth will use the \
                         direct in-lineage worker path"
                    );
                    std::sync::Arc::new(std::sync::Mutex::new(None))
                }
            }
        } else {
            std::sync::Arc::new(std::sync::Mutex::new(None))
        };

    // Establish confinement *around* the reauth listener thread, in an order dictated by
    // two facts: a Landlock ruleset and a seccomp filter are both inherited across
    // `clone`, and the supervisor seccomp allowlist has neither `clone` nor the Landlock
    // syscalls. So: create the reauth socket dir → apply Landlock → spawn the reauth
    // thread (it inherits the Landlock domain; it never issues a Landlock syscall itself,
    // which under the filter it could not) → install the seccomp filter with TSYNC so it
    // covers the already-existing reauth thread. Landlock precedes seccomp so the
    // supervisor's own Landlock syscalls run before the filter that would deny them.

    // Create /run/doord-reauth before Landlock seeds it as a PathFd: a missing dir would
    // make the ruleset install fail and silently un-sandbox the whole supervisor. The
    // shipped unit also pre-creates it via RuntimeDirectory; this is the dev/non-systemd
    // belt-and-suspenders. Best-effort — a failure (an unprivileged dev run that cannot
    // write the runtime dir) is logged; the listener's own bind reports the rest.
    if let Err(e) = reauth::ensure_socket_dir(&config) {
        eprintln!(
            "doord: warning: could not prepare the reauth socket dir for {}: {e}",
            config.reauth_socket_path.display()
        );
    }

    // Confine the supervisor's filesystem reach (Landlock) FIRST — before the reauth
    // thread is spawned, so that thread inherits the ruleset (Landlock has no all-threads
    // apply; inheritance across `clone` is how a sibling thread is confined), and before
    // seccomp, so the supervisor's own Landlock syscalls are not denied by the filter.
    // Spawner-only: the desktop/greeter forked off the spawner never inherits the ruleset.
    // Flag-gated + default-off; best-effort (an old kernel degrades to NotEnforced).
    // `DOORD_NO_SANDBOX=1` forces off.
    if spawner.is_some() {
        let mode = hardening::LandlockMode::from_env();
        match hardening::apply_landlock(mode) {
            Ok(status) if mode != hardening::LandlockMode::Off => {
                eprintln!("doord: supervisor Landlock ruleset installed ({mode:?}, {status:?})");
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!(
                    "doord: warning: could not apply the Landlock ruleset ({mode:?}); \
                     continuing without a path sandbox: {e}"
                );
            }
        }
    } else if hardening::LandlockMode::from_env() != hardening::LandlockMode::Off {
        eprintln!(
            "doord: DOORD_LANDLOCK set but not in spawner mode; skipping — a supervisor \
             path ruleset on the direct in-lineage path would confine the desktop session"
        );
    }

    // Spawn the reauth listener now — after Landlock (so it inherits the path ruleset) and
    // before seccomp (whose allowlist has no `clone`) — but hold it on a release gate: the
    // thread blocks on `rx.recv()` (nothing but a futex wait — `futex` is allowlisted, and
    // the wake happens post-filter) and touches the socket only once released. The main
    // thread releases it *after* the TSYNC install below, so the reauth thread's very first
    // socket syscall (the `bind`/`listen` that makes the socket connectable, then `accept`)
    // already runs under the all-threads filter — the connectable window is never
    // seccomp-unconfined, no scheduler race. A channel (not a `Barrier`) so a failed spawn
    // can't deadlock the release: `send` on a dropped receiver is a harmless error.
    let reauth_release: Option<std::sync::mpsc::Sender<()>> = {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let reauth_config = config.clone();
        let factory = reauth::WorkerReauthFactory::new(reauth_spawner);
        let spawned = std::thread::Builder::new()
            .name("doord-reauth".to_string())
            .spawn(move || {
                // Park until the all-threads seccomp filter is installed. Issue no
                // meaningful syscall (only the futex wait) before this returns.
                if rx.recv().is_err() {
                    return; // released nothing (startup aborted) — never serve
                }
                if let Err(e) = reauth::serve_reauth(&reauth_config, &factory) {
                    eprintln!(
                        "doord: reauth listener could not start on {}: {e}",
                        reauth_config.reauth_socket_path.display()
                    );
                }
            });
        match spawned {
            Ok(_handle) => Some(tx), // detach the thread; keep the release handle
            Err(e) => {
                eprintln!(
                    "doord: could not spawn the reauth listener thread: {e}; reauth is disabled"
                );
                None
            }
        }
    };

    // Confine the supervisor's syscall surface (seccomp) LAST, with TSYNC (all threads),
    // so it covers both the greeter loop (this thread) and the reauth listener thread
    // without needing a post-filter `clone`. Spawner-only: on the direct in-lineage path
    // the supervisor itself forks the session, so a filter — inherited across fork and
    // preserved across execve — would confine the desktop and break every login.
    // Best-effort: a failed install (or `off`) leaves the supervisor on the baseline, and
    // says so. `DOORD_SECCOMP=log` records unlisted syscalls without blocking; `enforce`
    // fails them with EPERM; `DOORD_NO_SANDBOX=1` forces off.
    if spawner.is_some() {
        let mode = hardening::SeccompMode::from_env();
        match hardening::apply_seccomp(mode) {
            Ok(()) if mode != hardening::SeccompMode::Off => {
                eprintln!("doord: supervisor seccomp filter installed ({mode:?}, all threads)");
            }
            Ok(()) => {}
            Err(e) => {
                eprintln!(
                    "doord: warning: could not apply the seccomp filter ({mode:?}); \
                     continuing with the baseline only: {e}"
                );
            }
        }
    } else if hardening::SeccompMode::from_env() != hardening::SeccompMode::Off {
        eprintln!(
            "doord: DOORD_SECCOMP set but not in spawner mode; skipping — a supervisor \
             filter on the direct in-lineage path would confine the desktop session"
        );
    }

    // Release the reauth listener: the all-threads filter is now installed, so everything
    // the thread does from here — bind/listen/accept and the PAM relay — runs fully
    // seccomp-confined. (A dropped receiver, from a failed spawn, makes this a no-op.)
    if let Some(release) = reauth_release {
        let _ = release.send(());
    }

    let logins = WorkerLoginFactory::new();

    match ipc::serve(&config, &logins, spawner.as_ref()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "doord: fatal: could not serve on {}: {e}",
                config.socket_path.display()
            );
            ExitCode::FAILURE
        }
    }
}
