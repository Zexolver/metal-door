//! The two ways doorstep reaches a screen.
//!
//! [`udev`] is production: DRM/KMS, GBM, libinput and libseat on the greeter VT.
//! [`nested`] is a window on someone else's compositor, for development and the
//! screenshot harness. They share everything above this module.

#[cfg(feature = "nested")]
pub mod nested;
#[cfg(feature = "udev")]
pub mod udev;

use smithay::{
    backend::allocator::dmabuf::Dmabuf, reexports::wayland_server::protocol::wl_surface::WlSurface,
};

/// Backend-owned state, held by [`crate::state::Doorstep`].
pub enum BackendData {
    #[cfg(feature = "udev")]
    Udev(Box<udev::UdevData>),
    #[cfg(feature = "nested")]
    Nested(Box<nested::NestedData>),
}

impl BackendData {
    /// Import a client dmabuf into the backend's renderer. The greeter's GPU sky
    /// depends on this succeeding.
    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        match self {
            #[cfg(feature = "udev")]
            BackendData::Udev(data) => data.import_dmabuf(dmabuf),
            #[cfg(feature = "nested")]
            BackendData::Nested(data) => data.import_dmabuf(dmabuf),
        }
    }

    /// Hand the VT over. Only the udev backend owns a session that can.
    pub fn change_vt(&mut self, vt: i32) {
        match self {
            #[cfg(feature = "udev")]
            BackendData::Udev(data) => data.change_vt(vt),
            #[cfg(feature = "nested")]
            BackendData::Nested(_) => {
                tracing::debug!("ignoring a VT switch to {vt}: doorstep is nested, not on a VT");
            }
        }
    }

    /// Push keyboard LED state (caps lock, num lock) back to the physical
    /// keyboards. Only real devices have LEDs, so the nested backend has none.
    pub fn update_led_state(&mut self, led_state: smithay::input::keyboard::LedState) {
        match self {
            #[cfg(feature = "udev")]
            BackendData::Udev(data) => data.update_led_state(led_state),
            #[cfg(feature = "nested")]
            BackendData::Nested(_) => {
                let _ = led_state;
            }
        }
    }

    /// The seat name to advertise on `wl_seat`.
    pub fn seat_name(&self) -> String {
        match self {
            #[cfg(feature = "udev")]
            BackendData::Udev(data) => data.seat_name(),
            #[cfg(feature = "nested")]
            BackendData::Nested(_) => "nested".to_string(),
        }
    }

    /// Called on every surface commit. Both backends render on their own clock
    /// (vblank, or the host compositor's frame callbacks), so there is nothing to
    /// wake — the hook exists so that stays a deliberate choice rather than an
    /// omission.
    pub fn on_commit(&mut self, _surface: &WlSurface) {}
}
