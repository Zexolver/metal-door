//! Where the daemon's runtime knobs come from.
//!
//! In production these are fixed by the install: a socket under `/run/doord` and
//! the greeter system user's uid/gid. They are overridable by environment for
//! development and testing, so the seam can be exercised without being root or
//! installing a system user — the *mechanism* (peercred, perms, framing) is
//! identical either way; only the identities differ.

use std::ffi::OsString;
use std::path::PathBuf;

/// Default production socket path. The parent dir is created `root:root 0700`.
const DEFAULT_SOCKET_PATH: &str = "/run/doord/door.sock";

/// Default PAM service name. Resolves to `/etc/pam.d/doord` (installed by door),
/// falling back to `/etc/pam.d/other` — which denies — if door's file is absent.
const DEFAULT_PAM_SERVICE: &str = "doord";

/// Default seat when neither door's own session environment nor config names one.
/// Single-seat is the v1 assumption; `seat0` is the only seat on a standard host.
const DEFAULT_SEAT: &str = "seat0";

/// Which seat and VT a spawned session is registered on. Handed to logind (via
/// `pam_systemd`) as `XDG_SEAT` / `XDG_VTNR` so the session lands where door
/// itself runs. The greeter never chooses these — they come from the daemon's
/// own logind session (the VT door was launched on), with config overrides for
/// development where door is not itself a logind session.
#[derive(Debug, Clone)]
pub struct SeatTarget {
    pub seat: String,
    /// VT number, when known. `None` on a dev run with no VT — the session is
    /// then registered without a `XDG_VTNR` and inherits no controlling tty.
    pub vtnr: Option<u32>,
}

/// Default reauth socket path. Lives in its own dir (not the greeter's `/run/doord`,
/// which is locked to the greeter group) because the reauth listener must be
/// reachable by any local session user — the peer-cred uid, not the socket mode, is
/// the authorization gate on this seam.
const DEFAULT_REAUTH_SOCKET_PATH: &str = "/run/doord-reauth/reauth.sock";

/// Resolved daemon configuration.
///
/// `Clone` so the daemon-lifetime reauth listener thread can own an immutable copy
/// rather than borrow across the thread boundary.
#[derive(Clone)]
pub struct Config {
    /// Pathname Unix socket to listen on (`DOORD_SOCKET`).
    pub socket_path: PathBuf,
    /// Pathname Unix socket the session-lock reauth listener binds
    /// (`DOORD_REAUTH_SOCKET`). World-connectable; the peer-cred uid is the gate.
    pub reauth_socket_path: PathBuf,
    /// Data-dir roots searched for `wayland-sessions/` and `xsessions/`
    /// (`DOORD_SESSION_DIRS`, `:`-separated; defaults to the freedesktop dirs).
    pub session_dirs: Vec<PathBuf>,
    /// The only uid permitted to connect, enforced via `SO_PEERCRED`
    /// (`DOORD_GREETER_UID`; defaults to the daemon's own uid for dev runs).
    pub greeter_uid: u32,
    /// Group to own the socket, if known (`DOORD_GREETER_GID`); `None` skips the
    /// chgrp, leaving the peercred check as the sole authorization gate.
    pub greeter_gid: Option<u32>,
    /// PAM service name to authenticate against (`DOORD_PAM_SERVICE`).
    pub pam_service: String,
    /// Seat and VT a spawned session is registered on (see [`SeatTarget`]).
    pub seat: SeatTarget,
    /// The greeter system user's name (`DOORD_GREETER_USER`), if configured.
    /// doord launches the greeter as this user in a passwordless logind session;
    /// `None` (dev) means no managed greeter is launched.
    pub greeter_user: Option<String>,
    /// PAM service for the *greeter's* passwordless session
    /// (`DOORD_GREETER_PAM_SERVICE`; default `door-greeter`). Distinct from the
    /// login service (`pam_service`) — this one authenticates no human.
    pub greeter_pam_service: String,
    /// The command doord execs as the greeter (`DOORD_GREETER_CMD`, whitespace-
    /// split; default `doorstep -- /usr/bin/door-greeter`).
    ///
    /// `doorstep` is door's own kiosk compositor (D-0022); it takes the same
    /// `HOST -- CLIENT` shape `cage` did, so setting this to
    /// `cage -- /usr/bin/door-greeter` is still a supported fallback.
    pub greeter_cmd: Vec<String>,
}

impl Config {
    /// Build the configuration from the environment, falling back to
    /// development-friendly defaults so an unprivileged `cargo run` still serves.
    pub fn from_env() -> Self {
        let socket_path = env_os("DOORD_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_PATH));

        let reauth_socket_path = env_os("DOORD_REAUTH_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_REAUTH_SOCKET_PATH));

        // `:`-separated data-dir roots, mirroring how XDG paths are written.
        // Empty segments are dropped so a trailing `:` is harmless.
        let session_dirs = env_os("DOORD_SESSION_DIRS")
            .map(|raw| {
                std::env::split_paths(&raw)
                    .filter(|p| !p.as_os_str().is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|dirs: &Vec<PathBuf>| !dirs.is_empty())
            .unwrap_or_else(|| {
                crate::sessions::DEFAULT_DATA_DIRS
                    .iter()
                    .map(PathBuf::from)
                    .collect()
            });

        // The greeter user may be named (`DOORD_GREETER_USER`) rather than given
        // as a numeric uid — systemd units carry a name, not a uid. When set, it
        // resolves both uid and gid; an explicit `DOORD_GREETER_UID`/`_GID` still
        // overrides. Default (dev): whoever launched the daemon, so a `cargo run`
        // authorizes its own test greeter without configuration.
        let greeter_user = std::env::var("DOORD_GREETER_USER")
            .ok()
            .filter(|v| !v.is_empty());
        let named_greeter = greeter_user
            .as_deref()
            .and_then(|name| crate::user::resolve(name).ok());
        let greeter_uid = env_u32("DOORD_GREETER_UID")
            .or(named_greeter.as_ref().map(|u| u.uid))
            .unwrap_or_else(current_uid);
        let greeter_gid = env_u32("DOORD_GREETER_GID").or(named_greeter.as_ref().map(|u| u.gid));

        let greeter_pam_service = std::env::var("DOORD_GREETER_PAM_SERVICE")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "door-greeter".to_string());

        // The greeter command, whitespace-split (no shell quoting — door owns it).
        let greeter_cmd = std::env::var("DOORD_GREETER_CMD")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| v.split_whitespace().map(str::to_string).collect::<Vec<_>>())
            .unwrap_or_else(|| {
                ["doorstep", "--", "/usr/bin/door-greeter"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            });

        let pam_service = std::env::var("DOORD_PAM_SERVICE")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_PAM_SERVICE.to_string());

        // Seat/VT: door registers a session where it itself runs. Prefer an
        // explicit override, then the daemon's own logind session environment
        // (set when door was launched on its VT), then the single-seat default.
        let seat = std::env::var("DOORD_SEAT")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var("XDG_SEAT").ok().filter(|v| !v.is_empty()))
            .unwrap_or_else(|| DEFAULT_SEAT.to_string());
        let vtnr = env_u32("DOORD_VTNR").or_else(|| env_u32("XDG_VTNR"));

        Config {
            socket_path,
            reauth_socket_path,
            session_dirs,
            greeter_uid,
            greeter_gid,
            pam_service,
            seat: SeatTarget { seat, vtnr },
            greeter_user,
            greeter_pam_service,
            greeter_cmd,
        }
    }
}

fn env_os(key: &str) -> Option<OsString> {
    std::env::var_os(key).filter(|v| !v.is_empty())
}

fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

fn current_uid() -> u32 {
    // SAFETY: getuid is always safe; it takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}
