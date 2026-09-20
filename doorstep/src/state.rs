//! The compositor state and, more importantly, doorstep's kiosk policy.
//!
//! Three rules define this compositor, and each one is enforced here rather than
//! by convention:
//!
//! 1. **One process, ours.** The socket lives in the runtime dir; a connection is
//!    admitted only if its kernel-attested peer uid is our own *and* its peer pid
//!    is the process we launched. Anything else is closed before it is handed to
//!    `wayland-server`, so it never reaches a global.
//!
//!    Note the unit: *process*, not *connection*. A toolkit legitimately opens
//!    more than one — the greeter's `iced`/`wgpu` stack opens a second for its GPU
//!    surface — so a naive first-client-wins rule breaks the real greeter while
//!    protecting nothing extra. The pid gate is the tighter rule anyway: cage
//!    admits any local connection at all.
//! 2. **One window, fullscreen, undecorated.** Every toplevel is configured to the
//!    output size with the fullscreen state set; decorations are answered
//!    server-side and then not drawn.
//! 3. **The client's lifetime is the compositor's.** When the hosted command exits,
//!    doorstep exits with its status — the contract `doord` already relies on.

use std::{
    ffi::OsString,
    os::unix::net::UnixStream,
    process::{Child, Command},
    sync::Arc,
    time::{Duration, Instant},
};

use smithay::{
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{pointer::CursorImageStatus, Seat, SeatState},
    reexports::{
        calloop::{generic::Generic, Interest, LoopHandle, LoopSignal, Mode, PostAction},
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Display, DisplayHandle,
        },
    },
    utils::{Logical, Point},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        dmabuf::{DmabufGlobal, DmabufState},
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        presentation::PresentationState,
        selection::data_device::DataDeviceState,
        shell::xdg::{decoration::XdgDecorationState, XdgShellState},
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
    },
};

use crate::backend::BackendData;

/// How long a hosted client gets to answer `SIGTERM` before `SIGKILL`.
pub const SIGTERM_GRACE: Duration = Duration::from_secs(2);

/// How many simultaneous connections the hosted process may hold. A toolkit
/// opens a handful (iced/wgpu opens a second for its GPU surface); this is a
/// bound on a wedged client, not a policy knob.
pub const MAX_CLIENT_CONNECTIONS: usize = 8;

/// Per-client data. Only the hosted process's connections ever carry it.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

pub struct Doorstep {
    pub start_time: Instant,
    pub display_handle: DisplayHandle,
    // Both are the udev backend's: it schedules repaints off the loop and stamps
    // presentation feedback with the clock. A nested-only build carries neither.
    #[cfg_attr(not(feature = "udev"), allow(dead_code))]
    pub loop_handle: LoopHandle<'static, Doorstep>,
    pub loop_signal: LoopSignal,
    #[cfg_attr(not(feature = "udev"), allow(dead_code))]
    pub clock: smithay::utils::Clock<smithay::utils::Monotonic>,

    pub space: Space<Window>,
    pub popups: PopupManager,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    // These four are never read after construction: holding them *is* what keeps
    // their globals advertised. Dropping one silently removes the protocol.
    #[allow(dead_code)]
    pub xdg_decoration_state: XdgDecorationState,
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,
    #[allow(dead_code)]
    pub presentation_state: PresentationState,
    #[allow(dead_code)]
    pub fractional_scale_state: FractionalScaleManagerState,
    pub dmabuf_state: DmabufState,
    pub dmabuf_global: Option<DmabufGlobal>,

    pub seat: Seat<Self>,
    /// Kept for diagnostics; the seat itself carries the name on the wire.
    #[allow(dead_code)]
    pub seat_name: String,
    pub cursor_status: CursorImageStatus,

    /// The socket clients connect to (`WAYLAND_DISPLAY` for the child).
    pub socket_name: OsString,
    /// The only uid allowed to connect: our own.
    pub allowed_uid: u32,
    /// The pid connections must come from: the hosted command's. `None` before it
    /// is spawned, which is also when nothing has any business connecting.
    pub hosted_pid: Option<i32>,
    /// How many connections that process currently holds, bounded by
    /// [`MAX_CLIENT_CONNECTIONS`] so a wedged client cannot exhaust our fds.
    pub connections: usize,

    /// The hosted command. `None` once it has been reaped.
    pub child: Option<Child>,
    /// What doorstep will exit with — the child's status, mirrored.
    pub exit_code: i32,

    pub vt_switch: bool,
    pub backend: BackendData,
}

impl Doorstep {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        display: Display<Doorstep>,
        loop_handle: LoopHandle<'static, Doorstep>,
        loop_signal: LoopSignal,
        backend: BackendData,
        seat_name: String,
        vt_switch: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, Vec::new());
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let presentation_state = PresentationState::new::<Self>(&dh, libc::CLOCK_MONOTONIC as u32);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&dh);
        let dmabuf_state = DmabufState::new();

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, seat_name.clone());
        // The greeter is a login screen: a keyboard is not optional, and the
        // xkb config comes from the environment (`XKB_DEFAULT_LAYOUT` and
        // friends) exactly as it does under cage, so a non-US layout still
        // types the right password.
        seat.add_keyboard(Default::default(), 200, 25)?;
        seat.add_pointer();
        seat.add_touch();

        let socket = ListeningSocketSource::new_auto()?;
        let socket_name = socket.socket_name().to_os_string();

        loop_handle.insert_source(socket, |stream, _, state: &mut Doorstep| {
            state.admit_client(stream);
        })?;

        loop_handle.insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // SAFETY: the display is owned by the event source and never moved out.
                unsafe { display.get_mut().dispatch_clients(state)? };
                Ok(PostAction::Continue)
            },
        )?;

        Ok(Self {
            start_time: Instant::now(),
            display_handle: dh,
            loop_handle,
            loop_signal,
            clock: smithay::utils::Clock::new(),
            space: Space::default(),
            popups: PopupManager::default(),
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            viewporter_state,
            presentation_state,
            fractional_scale_state,
            dmabuf_state,
            dmabuf_global: None,
            seat,
            seat_name,
            cursor_status: CursorImageStatus::default_named(),
            socket_name,
            allowed_uid: unsafe { libc::getuid() },
            hosted_pid: None,
            connections: 0,
            child: None,
            exit_code: 0,
            vt_switch,
            backend,
        })
    }

    /// Rule 1. Admit the socket peer only if it is our own uid and the process we
    /// launched; otherwise drop the stream, which closes it.
    fn admit_client(&mut self, stream: UnixStream) {
        let peer = match peer_credentials(&stream) {
            Ok(cred) => cred,
            Err(err) => {
                tracing::warn!("refusing a client with no peer credentials: {err}");
                return;
            }
        };

        if peer.uid != self.allowed_uid {
            tracing::warn!(
                "refusing uid {} on the greeter socket (only {} may connect)",
                peer.uid,
                self.allowed_uid
            );
            return;
        }

        match self.hosted_pid {
            Some(hosted) if hosted == peer.pid => {}
            Some(hosted) => {
                tracing::warn!(
                    "refusing pid {}: doorstep hosts pid {hosted} and nothing else",
                    peer.pid
                );
                return;
            }
            None => {
                tracing::warn!(
                    "refusing pid {}: nothing is hosted yet, so nothing should be connecting",
                    peer.pid
                );
                return;
            }
        }

        if self.connections >= MAX_CLIENT_CONNECTIONS {
            tracing::warn!(
                "refusing pid {}: already holding {} connections",
                peer.pid,
                self.connections
            );
            return;
        }

        match self
            .display_handle
            .insert_client(stream, Arc::new(ClientState::default()))
        {
            Ok(_) => {
                self.connections += 1;
                tracing::info!(
                    "greeter connected (pid {}, connection {})",
                    peer.pid,
                    self.connections
                );
            }
            Err(err) => tracing::warn!("failed to insert client: {err}"),
        }
    }

    /// Spawn the hosted command with `WAYLAND_DISPLAY` pointing at our socket.
    ///
    /// Called once the backend is up, so the client never races a missing output.
    pub fn spawn_child(&mut self, command: &[String]) -> std::io::Result<()> {
        let mut cmd = Command::new(&command[0]);
        cmd.args(&command[1..]);
        cmd.env("WAYLAND_DISPLAY", &self.socket_name);
        // doorstep starts no X server, and a stale DISPLAY would send a toolkit
        // looking for one that door deliberately does not provide.
        cmd.env_remove("DISPLAY");
        let child = cmd.spawn()?;
        self.hosted_pid = Some(child.id() as i32);
        tracing::info!(
            "hosting `{}` (pid {}) on {}",
            command.join(" "),
            child.id(),
            self.socket_name.to_string_lossy()
        );
        self.child = Some(child);
        Ok(())
    }

    /// Rule 3. Reap the child if it has exited and stop the loop when it has.
    ///
    /// Called on every event-loop tick; `try_wait` does not block.
    pub fn poll_child(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                self.exit_code = exit_code_of(&status);
                tracing::info!("hosted client exited with {}", self.exit_code);
                self.child = None;
                self.loop_signal.stop();
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!("failed to wait on the hosted client: {err}");
                self.child = None;
                self.loop_signal.stop();
            }
        }
    }

    /// Ask the hosted client to exit, then stop. `doord` sends doorstep a SIGTERM
    /// at the greeter→session handoff and waits for the VT to be released, so the
    /// child has to go down with us.
    pub fn shutdown(&mut self) {
        if let Some(child) = self.child.as_mut() {
            // SAFETY: `child` is alive until reaped; SIGTERM to a live pid is safe.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        }
        self.loop_signal.stop();
    }

    /// Wait for the hosted client to go, escalating to `SIGKILL` if it will not.
    ///
    /// doorstep is holding DRM master while this runs, and `doord` is waiting for
    /// the VT (D-0008), so "wait forever for a client that ignores SIGTERM" is not
    /// an option: that is the blank-VT wedge the daemon's own timeout exists to
    /// break. Bound it here too rather than relying on being killed.
    pub fn reap(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };

        let deadline = Instant::now() + SIGTERM_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.exit_code = exit_code_of(&status);
                    return;
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!("failed to wait on the hosted client: {err}");
                    return;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        tracing::warn!(
            "hosted client {} ignored SIGTERM for {:?}; killing it so the VT is released",
            child.id(),
            SIGTERM_GRACE
        );
        let _ = child.kill();
        match child.wait() {
            Ok(status) => self.exit_code = exit_code_of(&status),
            Err(err) => tracing::warn!("failed to reap the hosted client: {err}"),
        }
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(surface, offset)| (surface, (offset + location).to_f64()))
            })
    }

    /// The output the greeter lives on: the first one mapped. cage fullscreens a
    /// single output too, so this keeps the greeter's one-surface assumption
    /// (D-0006/D-0007) intact; any other outputs render the clear colour.
    pub fn primary_output(&self) -> Option<smithay::output::Output> {
        self.space.outputs().next().cloned()
    }

    /// Rule 2. Configure `window` fullscreen at the primary output's size.
    pub fn fullscreen(&mut self, window: &Window) {
        let Some(output) = self.primary_output() else {
            return;
        };
        let Some(geometry) = self.space.output_geometry(&output) else {
            return;
        };
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.size = Some(geometry.size);
                state.bounds = Some(geometry.size);
                state
                    .states
                    .set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen);
                state
                    .states
                    .set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated);
            });
        }
        self.space.map_element(window.clone(), geometry.loc, true);
    }

    /// Re-apply the fullscreen geometry to every window — after a mode change or
    /// a hotplug that moved the primary output.
    pub fn refit_windows(&mut self) {
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        for window in windows {
            self.fullscreen(&window);
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_pending_configure();
            }
        }
    }
}

/// Kernel-attested identity of a socket peer.
///
/// `UnixStream::peer_cred` is still unstable, and this is the gate that keeps a
/// second process off the greeter's compositor, so it is worth the `getsockopt`.
struct PeerCredentials {
    uid: u32,
    pid: i32,
}

fn peer_credentials(stream: &UnixStream) -> std::io::Result<PeerCredentials> {
    use std::os::unix::io::AsRawFd;

    let mut ucred = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `ucred`/`len` are valid for the size we pass, and the fd is owned
    // by `stream` for the duration of the call.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut ucred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(PeerCredentials {
        uid: ucred.uid,
        pid: ucred.pid,
    })
}

/// Mirror a child's exit status into a process exit code, the way a shell does.
pub fn exit_code_of(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => code,
        (None, Some(signal)) => 128 + signal,
        (None, None) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::exit_code_of;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    #[test]
    fn exit_codes_mirror_the_child() {
        assert_eq!(exit_code_of(&ExitStatus::from_raw(0)), 0);
        // waitpid encodes a normal exit in the high byte.
        assert_eq!(exit_code_of(&ExitStatus::from_raw(3 << 8)), 3);
        // ...and a signal death in the low seven bits: SIGTERM is 15.
        assert_eq!(exit_code_of(&ExitStatus::from_raw(15)), 128 + 15);
    }
}
