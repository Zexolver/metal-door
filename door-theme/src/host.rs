//! Local host reads shared by door's auth surfaces (the greeter and the lock
//! screen): wall-clock time, the optional indicator probes (Caps Lock, keyboard
//! layout, battery), and the pre-auth asset vetting.
//!
//! Everything here is a purely local, unprivileged read — no daemon round-trip,
//! no privileged path, nothing a shoulder-surfer shouldn't already see. Both
//! surfaces render before (or without) an authenticated user, so these helpers
//! deliberately avoid date/time crates and read only world-readable files.

use std::path::{Path, PathBuf};

use crate::Theme;

/// Whether a logo path is an SVG (case-insensitive `.svg`) — chooses the vector
/// renderer over the raster one.
pub fn is_svg(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
}

/// The only directories an auth surface loads wallpaper/logo assets from —
/// the same root-owned trees door's own config is read from. A local unprivileged
/// user cannot write here, so an asset resolved into one of these is trustworthy;
/// a path anywhere else (a world-writable `/tmp`, a user's home) could be swapped
/// out from under the login screen before authentication, so it is refused.
const ASSET_ROOTS: [&str; 2] = ["/usr/share/door", "/etc/door"];

/// Decompression-bomb guard for raster assets: a decoded frame larger than this in
/// either dimension, or above the pixel budget, is refused rather than handed to the
/// image decoder (which would allocate for the full frame). 8K is ~33 MP, so this
/// clears any real display wallpaper while rejecting a hostile 100k×100k PNG.
const MAX_ASSET_DIM: u32 = 8192;
const MAX_ASSET_PIXELS: u64 = 40_000_000;

/// Vet a configured asset before an auth surface loads it. Returns the
/// canonical path if it is trusted, or `None` (with a logged explanation) if it is
/// refused — the caller then renders without it (a solid background / no logo).
///
/// Two gates: the path must resolve *inside* [`ASSET_ROOTS`] (canonicalized first,
/// so a symlink pointing out of the trusted tree is caught), and a raster asset must
/// decode within the size cap. A `DOORD_GREETER_CONFIG` dev/test config is an
/// explicit trusted-operator signal and may load assets from anywhere — but the size
/// cap still applies.
pub fn vet_asset(path: &Path, kind: &str) -> Option<PathBuf> {
    let real = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "door: refusing {kind} {}: cannot resolve the path ({e}); loading without it.",
                path.display()
            );
            return None;
        }
    };

    let dev = std::env::var_os(crate::ENV_CONFIG).is_some();
    if !dev && !ASSET_ROOTS.iter().any(|root| real.starts_with(root)) {
        eprintln!(
            "door: refusing {kind} {}: pre-auth assets may only be loaded from {}. \
             A path outside those root-owned directories could be replaced by a local \
             user before login, so it is not loaded — the surface falls back to none.",
            real.display(),
            ASSET_ROOTS.join(" or "),
        );
        return None;
    }

    // SVG has no meaningful raster dimensions; the trusted-path gate above is its
    // defense (and it is the higher-risk parser, so it is only ever loaded from root).
    // Probe only the header — `into_dimensions` reads the size without decoding the
    // full frame, so the bomb never gets allocated.
    if !is_svg(&real) {
        let dims = image::ImageReader::open(&real)
            .map_err(|e| e.to_string())
            .and_then(|r| r.with_guessed_format().map_err(|e| e.to_string()))
            .and_then(|r| r.into_dimensions().map_err(|e| e.to_string()));
        match dims {
            Ok((w, h))
                if w > MAX_ASSET_DIM
                    || h > MAX_ASSET_DIM
                    || (w as u64 * h as u64) > MAX_ASSET_PIXELS =>
            {
                eprintln!(
                    "door: refusing {kind} {}: {w}×{h} exceeds the {MAX_ASSET_DIM}px / {} MP \
                     asset cap (a decompression-bomb guard); loading without it.",
                    real.display(),
                    MAX_ASSET_PIXELS / 1_000_000,
                );
                return None;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!(
                    "door: refusing {kind} {}: not a decodable image ({e}); loading without it.",
                    real.display()
                );
                return None;
            }
        }
    }

    Some(real)
}

/// Vet a theme's wallpaper + logo once (when it is loaded or hot-reloaded), so the
/// per-frame `view` never re-stats or re-probes. Returns the trusted paths to render.
pub fn vet_theme_assets(theme: &Theme) -> (Option<PathBuf>, Option<PathBuf>) {
    let wallpaper = theme
        .wallpaper
        .as_ref()
        .and_then(|p| vet_asset(p, "wallpaper"));
    let logo = theme.logo.as_ref().and_then(|p| vet_asset(p, "logo"));
    (wallpaper, logo)
}

/// Battery charge for the optional indicator — a percent and whether it's charging,
/// from a purely local `/sys/class/power_supply` read (no daemon, no privilege).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
}

/// Whether Caps Lock is on, from the keyboard's `capslock` LED under
/// `/sys/class/leds/*::capslock/brightness` (e.g. `input3::capslock`). A purely
/// local read — no daemon, no privileged path, and it discloses nothing sensitive.
/// `None` when no such LED exists (e.g. some laptops) so the caller can hide the
/// hint rather than assert a state it cannot know.
pub fn caps_lock_on() -> Option<bool> {
    let entries = std::fs::read_dir("/sys/class/leds").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().ends_with("::capslock") {
            let brightness = std::fs::read_to_string(entry.path().join("brightness")).ok()?;
            return Some(brightness.trim() != "0");
        }
    }
    None
}

/// The configured keyboard layout code (e.g. `us`, `de`) from purely local sources,
/// in priority order: `XKB_DEFAULT_LAYOUT` (the compositor's launch env), then the
/// localectl-managed `/etc/X11/xorg.conf.d/00-keyboard.conf` (`XkbLayout`), then
/// `/etc/vconsole.conf` (`XKBLAYOUT=`). Returns the *primary* (first) layout of a
/// comma-separated list. No daemon, no privileged path — every source is world-
/// readable. This reports the *configured* layout: a Wayland client can't see a live
/// per-keystroke layout switch through iced, so the indicator names what the seat is
/// set to, which is the thing that trips a login on an unexpected layout.
pub fn kb_layout() -> Option<String> {
    fn primary(value: &str) -> Option<String> {
        let first = value.trim().trim_matches('"').split(',').next()?.trim();
        (!first.is_empty()).then(|| first.to_string())
    }
    // 1. The compositor's launch environment, if it set an explicit layout.
    if let Ok(env) = std::env::var("XKB_DEFAULT_LAYOUT") {
        if let Some(l) = primary(&env) {
            return Some(l);
        }
    }
    // 2. The localectl-managed X11/Wayland keymap file: `Option "XkbLayout" "us,de"`.
    if let Ok(conf) = std::fs::read_to_string("/etc/X11/xorg.conf.d/00-keyboard.conf") {
        for line in conf.lines() {
            let line = line.trim();
            if line.starts_with("Option") && line.contains("XkbLayout") {
                // The value is the last quoted token on the line.
                if let Some(v) = line.rsplit('"').nth(1) {
                    if let Some(l) = primary(v) {
                        return Some(l);
                    }
                }
            }
        }
    }
    // 3. The virtual-console config's xkb layout (`XKBLAYOUT="us"`).
    if let Ok(vc) = std::fs::read_to_string("/etc/vconsole.conf") {
        for line in vc.lines() {
            if let Some(v) = line.trim().strip_prefix("XKBLAYOUT=") {
                if let Some(l) = primary(v) {
                    return Some(l);
                }
            }
        }
    }
    None
}

/// Battery charge as `(percent, charging?)` from the first real battery under
/// `/sys/class/power_supply/*` (the `type` file reads `Battery`, skipping AC/USB
/// supplies). A purely local, world-readable sysfs read — no daemon, no privilege.
/// `None` on a machine with no battery, so a desktop asserts nothing.
pub fn battery() -> Option<Battery> {
    let entries = std::fs::read_dir("/sys/class/power_supply").ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        let is_battery = std::fs::read_to_string(p.join("type"))
            .map(|s| s.trim() == "Battery")
            .unwrap_or(false);
        if !is_battery {
            continue;
        }
        let Some(percent) = std::fs::read_to_string(p.join("capacity"))
            .ok()
            .and_then(|s| s.trim().parse::<u8>().ok())
        else {
            continue;
        };
        let charging = std::fs::read_to_string(p.join("status"))
            .map(|s| matches!(s.trim(), "Charging" | "Full"))
            .unwrap_or(false);
        return Some(Battery {
            percent: percent.min(100),
            charging,
        });
    }
    None
}

/// Local `tm` for the current epoch second, or `None` if the conversion fails.
/// `localtime_r` fills a caller-owned struct — no date/time crate on the pre-auth
/// surface.
pub fn local_tm() -> Option<libc::tm> {
    // SAFETY: `time(NULL)` returns epoch seconds; `localtime_r` fills our owned `tm`
    // and returns null on failure. No shared state, no allocation.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            None
        } else {
            Some(tm)
        }
    }
}

/// Local wall-clock time. A `format` (a `strftime` string) wins when set; otherwise
/// the built-in `HH:MM` (24-hour) / `H:MM AM/PM` (12-hour), with optional seconds.
pub fn now_hm(clock_24h: bool, seconds: bool, format: Option<&str>) -> String {
    let Some(tm) = local_tm() else {
        return String::new();
    };
    if let Some(fmt) = format {
        return strftime(&tm, fmt);
    }
    if clock_24h {
        if seconds {
            format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
        } else {
            format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
        }
    } else {
        let h12 = match tm.tm_hour % 12 {
            0 => 12,
            h => h,
        };
        let meridiem = if tm.tm_hour < 12 { "AM" } else { "PM" };
        if seconds {
            format!("{}:{:02}:{:02} {}", h12, tm.tm_min, tm.tm_sec, meridiem)
        } else {
            format!("{}:{:02} {}", h12, tm.tm_min, meridiem)
        }
    }
}

/// Format a `tm` via the C library's `strftime`. Empty on a malformed format (an
/// interior NUL) or if the result overflows the buffer — the caller keeps the last
/// good clock. No date/time crate on the pre-auth surface.
pub fn strftime(tm: &libc::tm, fmt: &str) -> String {
    let Ok(cfmt) = std::ffi::CString::new(fmt) else {
        return String::new();
    };
    let mut buf = [0u8; 128];
    // SAFETY: strftime writes at most buf.len() bytes (including the NUL) into buf,
    // and only reads the borrowed `tm` and our NUL-terminated format string.
    let n = unsafe {
        libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            cfmt.as_ptr(),
            tm,
        )
    };
    if n == 0 {
        return String::new();
    }
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// Local time as minutes since midnight (for the day/night window). Noon on failure.
pub fn now_minutes() -> u32 {
    match local_tm() {
        Some(tm) => (tm.tm_hour.clamp(0, 23) as u32) * 60 + (tm.tm_min.clamp(0, 59) as u32),
        None => 12 * 60,
    }
}

/// Local calendar month, 1–12 (for the seasonal scene selector). June on failure.
pub fn now_month() -> u32 {
    match local_tm() {
        Some(tm) => (tm.tm_mon.clamp(0, 11) as u32) + 1,
        None => 6,
    }
}

/// Local date as `Weekday, Month D` (e.g. `Friday, June 27`).
pub fn now_date() -> String {
    const DAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    match local_tm() {
        Some(tm) => {
            let day = DAYS.get(tm.tm_wday as usize).copied().unwrap_or("");
            let month = MONTHS.get(tm.tm_mon as usize).copied().unwrap_or("");
            format!("{day}, {month} {}", tm.tm_mday)
        }
        None => String::new(),
    }
}

/// Clock-hand angles (radians, clockwise from 12 o'clock) for the analog face.
/// Seconds carry a sub-second fraction from `gettimeofday`, so the second hand
/// sweeps smoothly when the surface is animating (and ticks at 1 Hz when it isn't).
pub fn clock_angles() -> (f32, f32, f32) {
    let Some(tm) = local_tm() else {
        return (0.0, 0.0, 0.0);
    };
    // Sub-second fraction for the smooth sweep. SAFETY: fills our owned timeval.
    let frac = unsafe {
        let mut tv: libc::timeval = std::mem::zeroed();
        if libc::gettimeofday(&mut tv, std::ptr::null_mut()) == 0 {
            (tv.tv_usec as f32) / 1_000_000.0
        } else {
            0.0
        }
    };
    let tau = std::f32::consts::TAU;
    let sec = tm.tm_sec as f32 + frac;
    let min = tm.tm_min as f32 + sec / 60.0;
    let hour = (tm.tm_hour % 12) as f32 + min / 60.0;
    (hour / 12.0 * tau, min / 60.0 * tau, sec / 60.0 * tau)
}

#[cfg(test)]
mod asset_vetting_tests {
    use super::vet_asset;
    use std::path::Path;

    /// A perfectly valid image that happens to live outside the trusted roots
    /// (`/tmp`) must be refused pre-auth — a local user could have written it.
    #[test]
    fn refuses_a_valid_image_outside_the_trusted_roots() {
        // SAFETY: single-threaded test; no other test touches this var. Ensure we are
        // in production mode (no dev-config bypass) so the allowlist is enforced.
        unsafe { std::env::remove_var(crate::ENV_CONFIG) };
        let p = std::env::temp_dir().join(format!("door-vet-{}.png", std::process::id()));
        image::RgbaImage::new(2, 2)
            .save(&p)
            .expect("write test png");
        assert!(
            vet_asset(&p, "wallpaper").is_none(),
            "a valid image under /tmp must be refused by the trusted-roots allowlist"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// A path that does not resolve is refused (canonicalize fails) — never loaded.
    #[test]
    fn refuses_a_nonexistent_asset() {
        assert!(vet_asset(Path::new("/door/nope/missing.png"), "logo").is_none());
    }
}
