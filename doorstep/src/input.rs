//! Input, forwarded to the one client — with exactly one exception.
//!
//! The exception is `Ctrl+Alt+F<n>`. door's documented recovery path ("switch to
//! a TTY, log in, and revert") has to work *from the login screen*, and on a VT
//! in graphics mode the kernel no longer handles those keys — the compositor
//! must. `--no-vt-switch` turns it off for kiosk deployments that want the
//! machine sealed.

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, Event, InputBackend, InputEvent, KeyboardKeyEvent,
        PointerAxisEvent, PointerButtonEvent, PointerMotionEvent, TouchEvent as _,
    },
    input::{
        keyboard::{keysyms, FilterResult},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
        touch::{DownEvent, MotionEvent as TouchMotionEvent, UpEvent},
    },
    utils::SERIAL_COUNTER,
};

use crate::state::Doorstep;

impl Doorstep {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let vt_switch = self.vt_switch;
                let Some(keyboard) = self.seat.get_keyboard() else {
                    return;
                };

                let target_vt = keyboard.input::<Option<i32>, _>(
                    self,
                    event.key_code(),
                    event.state(),
                    serial,
                    time,
                    |_, _, handle| {
                        let raw = handle.modified_sym().raw();
                        if vt_switch
                            && (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12)
                                .contains(&raw)
                        {
                            // Intercept: the client never sees the chord, and no
                            // key is left latched on the VT we are leaving.
                            FilterResult::Intercept(Some(
                                (raw - keysyms::KEY_XF86Switch_VT_1 + 1) as i32,
                            ))
                        } else {
                            FilterResult::Forward
                        }
                    },
                );

                if let Some(Some(vt)) = target_vt {
                    self.backend.change_vt(vt);
                }
            }
            InputEvent::PointerMotion { event } => {
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let location = self.clamp_to_outputs(pointer.current_location() + event.delta());
                let serial = SERIAL_COUNTER.next_serial();
                let under = self.surface_under(location);
                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let Some(output) = self.primary_output() else {
                    return;
                };
                let Some(geometry) = self.space.output_geometry(&output) else {
                    return;
                };
                let location = event.position_transformed(geometry.size) + geometry.loc.to_f64();
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                let under = self.surface_under(location);
                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerButton { event } => {
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                pointer.button(
                    self,
                    &ButtonEvent {
                        button: event.button_code(),
                        state: event.state(),
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerAxis { event } => {
                let source = event.source();
                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                for axis in [Axis::Horizontal, Axis::Vertical] {
                    let amount = event
                        .amount(axis)
                        .unwrap_or_else(|| event.amount_v120(axis).unwrap_or(0.0) * 15.0 / 120.0);
                    if amount != 0.0 {
                        frame = frame.value(axis, amount);
                        if let Some(v120) = event.amount_v120(axis) {
                            frame = frame.v120(axis, v120 as i32);
                        }
                    }
                    if source == AxisSource::Finger && event.amount(axis) == Some(0.0) {
                        frame = frame.stop(axis);
                    }
                }
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            InputEvent::TouchDown { event } => {
                let Some(touch) = self.seat.get_touch() else {
                    return;
                };
                let Some(location) = self.touch_location(&event) else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                let under = self.surface_under(location);
                touch.down(
                    self,
                    under,
                    &DownEvent {
                        slot: event.slot(),
                        location,
                        serial,
                        time: event.time_msec(),
                    },
                );
                touch.frame(self);
            }
            InputEvent::TouchMotion { event } => {
                let Some(touch) = self.seat.get_touch() else {
                    return;
                };
                let Some(location) = self.touch_location(&event) else {
                    return;
                };
                let under = self.surface_under(location);
                touch.motion(
                    self,
                    under,
                    &TouchMotionEvent {
                        slot: event.slot(),
                        location,
                        time: event.time_msec(),
                    },
                );
                touch.frame(self);
            }
            InputEvent::TouchUp { event } => {
                let Some(touch) = self.seat.get_touch() else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                touch.up(
                    self,
                    &UpEvent {
                        slot: event.slot(),
                        serial,
                        time: event.time_msec(),
                    },
                );
                touch.frame(self);
            }
            InputEvent::TouchCancel { .. } => {
                if let Some(touch) = self.seat.get_touch() {
                    touch.cancel(self);
                }
            }
            InputEvent::TouchFrame { .. } => {
                if let Some(touch) = self.seat.get_touch() {
                    touch.frame(self);
                }
            }
            _ => {}
        }
    }

    fn touch_location<E: AbsolutePositionEvent<I>, I: InputBackend>(
        &self,
        event: &E,
    ) -> Option<smithay::utils::Point<f64, smithay::utils::Logical>> {
        let output = self.primary_output()?;
        let geometry = self.space.output_geometry(&output)?;
        Some(event.position_transformed(geometry.size) + geometry.loc.to_f64())
    }

    /// Keep the pointer on a lit output: a relative device can otherwise walk it
    /// off the edge into a region nothing renders.
    fn clamp_to_outputs(
        &self,
        location: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> smithay::utils::Point<f64, smithay::utils::Logical> {
        if self
            .space
            .outputs()
            .filter_map(|output| self.space.output_geometry(output))
            .any(|geometry| geometry.to_f64().contains(location))
        {
            return location;
        }

        let Some(output) = self.primary_output() else {
            return location;
        };
        let Some(geometry) = self.space.output_geometry(&output) else {
            return location;
        };
        let max = geometry.loc.to_f64() + geometry.size.to_f64().to_point();
        (
            location.x.clamp(geometry.loc.x as f64, max.x - 1.0),
            location.y.clamp(geometry.loc.y as f64, max.y - 1.0),
        )
            .into()
    }
}
