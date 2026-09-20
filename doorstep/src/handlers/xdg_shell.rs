//! `xdg_shell`, reduced to what a kiosk can honour.
//!
//! There is one window and it is fullscreen. Move, resize, maximize, minimize and
//! "leave fullscreen" are therefore not refusals to implement — they are requests
//! whose only correct answer here is the state the client already has. Each is
//! answered with a configure re-asserting fullscreen, which is what the protocol
//! asks a compositor that will not comply to do.

use smithay::{
    delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{find_popup_root_surface, get_popup_toplevel_coords, PopupKind, Window},
    reexports::{
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
            shell::server::xdg_toplevel,
        },
        wayland_server::protocol::{wl_seat, wl_surface::WlSurface},
    },
    utils::Serial,
    wayland::{
        compositor::with_states,
        shell::xdg::{
            decoration::XdgDecorationHandler, PopupSurface, PositionerState, ToplevelSurface,
            XdgShellHandler, XdgShellState, XdgToplevelSurfaceData,
        },
    },
};

use crate::state::Doorstep;

impl XdgShellHandler for Doorstep {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface);
        self.fullscreen(&window);
        if let Some(keyboard) = self.seat.get_keyboard() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(
                self,
                window.toplevel().map(|t| t.wl_surface().clone()),
                serial,
            );
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let window = self
            .space
            .elements()
            .find(|window| window.toplevel().map(|t| t.wl_surface()) == Some(surface.wl_surface()))
            .cloned();
        if let Some(window) = window {
            self.space.unmap_elem(&window);
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, _surface: ToplevelSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
    }

    fn resize_request(
        &mut self,
        _surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        _serial: Serial,
        _edges: xdg_toplevel::ResizeEdge,
    ) {
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.reassert_fullscreen(&surface);
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.reassert_fullscreen(&surface);
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<wl_output::WlOutput>,
    ) {
        self.reassert_fullscreen(&surface);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.reassert_fullscreen(&surface);
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        // Minimizing the login screen would leave the machine showing nothing and
        // accepting keystrokes. Answer with the state we are keeping.
        self.reassert_fullscreen(&surface);
    }
}
delegate_xdg_shell!(Doorstep);

use smithay::reexports::wayland_server::protocol::wl_output;

impl XdgDecorationHandler for Doorstep {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // Server-side, and the server draws none. This is the structural fix for
        // the black-strip class of bugs (v0.1.7): under cage the client fell back
        // to drawing its own title bar because nothing answered this protocol.
        toplevel
            .with_pending_state(|state| state.decoration_mode = Some(DecorationMode::ServerSide));
        toplevel.send_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        toplevel
            .with_pending_state(|state| state.decoration_mode = Some(DecorationMode::ServerSide));
        toplevel.send_pending_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel
            .with_pending_state(|state| state.decoration_mode = Some(DecorationMode::ServerSide));
        toplevel.send_pending_configure();
    }
}
delegate_xdg_decoration!(Doorstep);

impl Doorstep {
    /// Re-send the fullscreen configure a kiosk always answers with.
    fn reassert_fullscreen(&mut self, surface: &ToplevelSurface) {
        let window = self
            .space
            .elements()
            .find(|window| window.toplevel().map(|t| t.wl_surface()) == Some(surface.wl_surface()))
            .cloned();
        if let Some(window) = window {
            self.fullscreen(&window);
        }
        surface.send_pending_configure();
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self
            .space
            .elements()
            .find(|window| window.toplevel().map(|t| t.wl_surface()) == Some(&root))
        else {
            return;
        };
        let Some(output) = self.primary_output() else {
            return;
        };
        let (Some(output_geometry), Some(window_geometry)) = (
            self.space.output_geometry(&output),
            self.space.element_geometry(window),
        ) else {
            return;
        };

        let mut target = output_geometry;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geometry.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}

/// Called from `CompositorHandler::commit`: send the initial configure a surface
/// is waiting on before it may attach a buffer.
pub fn handle_commit(state: &mut Doorstep, surface: &WlSurface) {
    let window = state
        .space
        .elements()
        .find(|window| window.toplevel().map(|t| t.wl_surface()) == Some(surface))
        .cloned();

    if let Some(window) = window {
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .expect("a mapped toplevel has toplevel data")
                .lock()
                .unwrap()
                .initial_configure_sent
        });

        if !initial_configure_sent {
            // The very first configure already carries fullscreen + the output
            // size, so the client never renders at a guessed size and then jumps.
            state.fullscreen(&window);
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_configure();
            }
        }
    }

    state.popups.commit(surface);
    if let Some(PopupKind::Xdg(popup)) = state.popups.find_popup(surface) {
        if !popup.is_initial_configure_sent() {
            let _ = popup.send_configure();
        }
    }
}
