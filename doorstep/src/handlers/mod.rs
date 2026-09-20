//! Protocol handlers.
//!
//! The list of globals doorstep implements *is* its threat model, so it is worth
//! reading as one. Present: `wl_compositor`, `wl_subcompositor`, `wl_shm`,
//! `wl_seat`, `wl_output`/`xdg_output`, `xdg_shell`, `xdg-decoration`,
//! `linux-dmabuf`, `viewporter`, `fractional-scale`, `presentation-time` and
//! `wl_data_device_manager` (which, with a single client, can only ever move data
//! from the greeter to itself).
//!
//! Absent on purpose: XWayland, `wlr-layer-shell`, `ext-session-lock`,
//! screencopy/`ext-image-copy`, `foreign-toplevel`, gamma control, virtual
//! keyboard/pointer, input method, text input, tablet, primary selection,
//! `xdg-activation`, DRM lease, and security contexts. A login screen needs none
//! of them, and each is a pre-auth attack surface door would have to defend.

mod compositor;
mod xdg_shell;

use smithay::{
    delegate_data_device, delegate_dmabuf, delegate_fractional_scale, delegate_output,
    delegate_presentation, delegate_seat, delegate_viewporter,
    input::{Seat, SeatHandler, SeatState},
    reexports::wayland_server::{protocol::wl_surface::WlSurface, Resource},
    wayland::{
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        fractional_scale::FractionalScaleHandler,
        output::OutputHandler,
        selection::{
            data_device::{
                set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
                ServerDndGrabHandler,
            },
            SelectionHandler,
        },
    },
};

use crate::state::Doorstep;

impl SeatHandler for Doorstep {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Doorstep> {
        &mut self.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        self.cursor_status = image;
    }

    fn led_state_changed(
        &mut self,
        _seat: &Seat<Self>,
        led_state: smithay::input::keyboard::LedState,
    ) {
        self.backend.update_led_state(led_state);
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|surface| dh.get_client(surface.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}
delegate_seat!(Doorstep);

impl SelectionHandler for Doorstep {
    type SelectionUserData = ();
}
impl DataDeviceHandler for Doorstep {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
impl ClientDndGrabHandler for Doorstep {}
impl ServerDndGrabHandler for Doorstep {}
delegate_data_device!(Doorstep);

impl OutputHandler for Doorstep {}
delegate_output!(Doorstep);

delegate_viewporter!(Doorstep);
delegate_presentation!(Doorstep);

impl FractionalScaleHandler for Doorstep {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        // One output, one window: the greeter's scale is the primary output's.
        if let Some(output) = self.primary_output() {
            smithay::wayland::compositor::with_states(&surface, |states| {
                smithay::wayland::fractional_scale::with_fractional_scale(states, |fractional| {
                    fractional.set_preferred_scale(output.current_scale().fractional_scale());
                });
            });
        }
    }
}
delegate_fractional_scale!(Doorstep);

impl DmabufHandler for Doorstep {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        // The greeter renders its sky with wgpu, so this is the hot path, not an
        // optional extra: a rejected import means a black login screen.
        if self.backend.import_dmabuf(&dmabuf) {
            let _ = notifier.successful::<Doorstep>();
        } else {
            notifier.failed();
        }
    }
}
delegate_dmabuf!(Doorstep);
