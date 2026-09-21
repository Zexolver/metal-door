//! Resolving an authenticated username to the system identity a session runs as.
//!
//! After PAM says "yes, this is `stephen`", the spawn path still needs the
//! concrete uid/gid/home/shell to drop to — facts that live in the passwd
//! database, not in anything the greeter sent. This module is the single place
//! that consults `getpwnam` and yields a [`TargetUser`] the privilege drop and
//! environment build consume.
//!
//! The username it resolves is always the one PAM authenticated, never one the
//! greeter supplied for the spawn: the greeter's `Start` carries only a session
//! id, so the identity here cannot be a greeter-chosen one. That binding is the
//! point — a compromised greeter must not be able to start a session *as someone
//! else*.

use std::ffi::CStr;
use std::io;

use crate::privdrop::TargetUser;

/// Look up `username` in the passwd database and return the identity to hand a
/// session off to. Errors if the user does not exist or the lookup fails — the
/// caller must then refuse to spawn rather than fall back to any default.
pub fn resolve(username: &str) -> io::Result<TargetUser> {
    let c_name = std::ffi::CString::new(username)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "username has a NUL byte"))?;

    // getpwnam_r writes into a caller-provided buffer; the right size is not
    // known up front, so grow on ERANGE. The suggested starting size comes from
    // sysconf, with a sane floor for systems that report no hint.
    let mut buf_len = match unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) } {
        n if n > 0 => n as usize,
        _ => 1024,
    };

    loop {
        let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let mut buf = vec![0 as libc::c_char; buf_len];

        // SAFETY: all pointers reference live, correctly-sized storage held by
        // this stack frame for the duration of the call; `c_name` is a valid C
        // string. getpwnam_r writes the entry into `passwd`/`buf` and sets
        // `result` to `&passwd` on success or null if no such user.
        let ret = unsafe {
            libc::getpwnam_r(
                c_name.as_ptr(),
                &mut passwd,
                buf.as_mut_ptr(),
                buf_len,
                &mut result,
            )
        };

        if ret == libc::ERANGE {
            // Buffer too small for this entry; double it and retry.
            buf_len = buf_len.saturating_mul(2);
            continue;
        }
        if ret != 0 {
            return Err(io::Error::from_raw_os_error(ret));
        }
        if result.is_null() {
            // ret == 0 with a null result means: no such user.
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no such user '{username}'"),
            ));
        }

        // SAFETY: these passwd fields point into `buf`, which outlives the reads
        // here; getpwnam_r guarantees them NUL-terminated.
        let name = unsafe { cstr_to_string(passwd.pw_name) };
        let home = unsafe { cstr_to_string(passwd.pw_dir) };
        let mut shell = unsafe { cstr_to_string(passwd.pw_shell) };
        // A passwd entry with an empty shell field conventionally means /bin/sh.
        if shell.is_empty() {
            shell = "/bin/sh".to_string();
        }

        return Ok(TargetUser {
            name,
            uid: passwd.pw_uid,
            gid: passwd.pw_gid,
            home,
            shell,
        });
    }
}

/// Look up a numeric uid in the passwd database and return the identity it names.
///
/// This is the reverse of [`resolve`], and it is how the reauth path binds a lock
/// screen to a single user: the daemon reads the connecting peer's kernel-attested
/// uid from `SO_PEERCRED` and resolves *that* uid here to the username PAM will
/// authenticate — never a name the client supplied. A uid with no passwd entry is a
/// `NotFound`, and the caller must then refuse rather than fall back to any default.
pub fn resolve_uid(uid: u32) -> io::Result<TargetUser> {
    let mut buf_len = match unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) } {
        n if n > 0 => n as usize,
        _ => 1024,
    };

    loop {
        let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let mut buf = vec![0 as libc::c_char; buf_len];

        // SAFETY: all pointers reference live, correctly-sized storage held by this
        // stack frame for the duration of the call. getpwuid_r writes the entry into
        // `passwd`/`buf` and sets `result` to `&passwd` on success or null if no such
        // uid.
        let ret =
            unsafe { libc::getpwuid_r(uid, &mut passwd, buf.as_mut_ptr(), buf_len, &mut result) };

        if ret == libc::ERANGE {
            buf_len = buf_len.saturating_mul(2);
            continue;
        }
        if ret != 0 {
            return Err(io::Error::from_raw_os_error(ret));
        }
        if result.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no passwd entry for uid {uid}"),
            ));
        }

        // SAFETY: these passwd fields point into `buf`, which outlives the reads here;
        // getpwuid_r guarantees them NUL-terminated.
        let name = unsafe { cstr_to_string(passwd.pw_name) };
        let home = unsafe { cstr_to_string(passwd.pw_dir) };
        let mut shell = unsafe { cstr_to_string(passwd.pw_shell) };
        if shell.is_empty() {
            shell = "/bin/sh".to_string();
        }

        return Ok(TargetUser {
            name,
            uid: passwd.pw_uid,
            gid: passwd.pw_gid,
            home,
            shell,
        });
    }
}

/// Copy a C string field out of a passwd entry into an owned `String`.
///
/// SAFETY: `ptr` must be a valid, NUL-terminated C string (or null). A null or
/// non-UTF-8 field yields an empty / lossy string rather than failing — a login
/// must not be blocked by an oddly-encoded gecos-adjacent field.
unsafe fn cstr_to_string(ptr: *const libc::c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    CStr::from_ptr(ptr).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_root_to_uid_zero() {
        // `root` exists on every Unix; its uid/gid are fixed at 0. (We only
        // resolve here — refusing to *drop* to uid 0 is privdrop's job.)
        let root = resolve("root").expect("root must resolve");
        assert_eq!(root.name, "root");
        assert_eq!(root.uid, 0);
        assert_eq!(root.gid, 0);
        assert!(!root.shell.is_empty());
    }

    #[test]
    fn unknown_user_is_not_found() {
        let err = resolve("doord-no-such-user-7f3a9c").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn username_with_nul_is_rejected() {
        let err = resolve("ro\0ot").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn resolve_uid_zero_is_root() {
        // uid 0 is `root` on every Unix — the reverse lookup the reauth path uses to
        // turn a peercred uid into the username PAM authenticates.
        let root = resolve_uid(0).expect("uid 0 must resolve");
        assert_eq!(root.name, "root");
        assert_eq!(root.uid, 0);
    }

    #[test]
    fn resolve_uid_round_trips_with_resolve() {
        // Whatever uid this test runs as must resolve to a name that resolves back to
        // the same uid — the binding reauth relies on (peercred uid → name → PAM).
        let me_uid = unsafe { libc::getuid() };
        let by_uid = resolve_uid(me_uid).expect("own uid must resolve");
        let by_name = resolve(&by_uid.name).expect("own name must resolve");
        assert_eq!(by_uid.uid, me_uid);
        assert_eq!(by_name.uid, me_uid);
        assert_eq!(by_uid.name, by_name.name);
    }

    #[test]
    fn resolve_uid_unknown_is_not_found() {
        // A very high uid with no passwd entry must be a clean NotFound, so the reauth
        // path refuses rather than authenticating some default identity.
        let err = resolve_uid(4_000_000_000).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
