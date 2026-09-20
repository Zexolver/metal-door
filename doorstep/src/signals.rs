//! `SIGTERM`/`SIGINT` → the event loop, through a self-pipe.
//!
//! `doord` ends a greet by sending the greeter `SIGTERM` and then *waiting for
//! the VT to be released* before the session worker takes DRM master (D-0008).
//! Dying on the default disposition would skip libseat's teardown and leave the
//! seat held, so the signal has to arrive as an ordinary event-loop event that
//! runs the shutdown path.

use std::os::unix::io::{FromRawFd, OwnedFd, RawFd};

use smithay::reexports::calloop::{generic::Generic, Interest, LoopHandle, Mode, PostAction};

use crate::state::Doorstep;

static mut WRITE_FD: RawFd = -1;

/// Async-signal-safe: a single byte down the pipe, nothing else.
extern "C" fn handler(_signal: libc::c_int) {
    let fd = unsafe { WRITE_FD };
    if fd >= 0 {
        let byte: u8 = 1;
        // Errors are deliberately ignored: there is nothing safe to do about
        // them here, and a full pipe already means a pending wakeup.
        unsafe { libc::write(fd, &byte as *const u8 as *const libc::c_void, 1) };
    }
}

/// Install the handlers and wire the read end into `loop_handle`.
pub fn install(
    loop_handle: &LoopHandle<'static, Doorstep>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid two-element array for the duration of the call.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    // SAFETY: both ends were just created by `pipe2` and are owned here.
    let read_end = unsafe { OwnedFd::from_raw_fd(read_fd) };
    unsafe { WRITE_FD = write_fd };

    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: `handler` is async-signal-safe (one `write`).
        if unsafe { libc::signal(signal, handler as *const () as libc::sighandler_t) }
            == libc::SIG_ERR
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }

    loop_handle.insert_source(
        Generic::new(read_end, Interest::READ, Mode::Level),
        |_, _, state: &mut Doorstep| {
            tracing::info!("signalled: asking the hosted client to exit");
            state.shutdown();
            Ok(PostAction::Remove)
        },
    )?;

    Ok(())
}
