//! The nested backend: doorstep as a window on someone else's compositor.
//!
//! Development and the screenshot harness only. `doord` never sets
//! `WAYLAND_DISPLAY` for the greeter, so production never selects this path, and
//! `--no-default-features --features udev` leaves it out of the binary entirely.

use std::time::Duration;

use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        renderer::{
            damage::OutputDamageTracker, element::memory::MemoryRenderBuffer, gles::GlesRenderer,
            ImportDma,
        },
        winit::{self, Error as WinitError, WinitEvent, WinitEventLoop, WinitGraphicsBackend},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::LoopHandle,
    utils::{Rectangle, Transform},
};

use crate::{backend::BackendData, cursor, render, state::Doorstep};

pub struct NestedData {
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    pub damage_tracker: OutputDamageTracker,
    pub output: Output,
    pub pointer_buffer: MemoryRenderBuffer,
}

impl NestedData {
    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        self.backend.renderer().import_dmabuf(dmabuf, None).is_ok()
    }
}

/// Open the window and build the backend state. The event source is inserted
/// later, by [`start`], once the compositor state exists.
pub fn init() -> Result<(NestedData, WinitEventLoop), WinitError> {
    let (backend, winit) = winit::init()?;

    let mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };
    let output = Output::new(
        "doorstep".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "door".into(),
            model: "nested".into(),
        },
    );
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    let damage_tracker = OutputDamageTracker::from_output(&output);

    Ok((
        NestedData {
            backend,
            damage_tracker,
            output,
            pointer_buffer: cursor::default_pointer(),
        },
        winit,
    ))
}

/// Publish the output, advertise dmabuf, and drive frames off winit's redraws.
pub fn start(
    state: &mut Doorstep,
    loop_handle: &LoopHandle<'static, Doorstep>,
    winit: WinitEventLoop,
) -> Result<(), Box<dyn std::error::Error>> {
    let BackendData::Nested(data) = &mut state.backend else {
        return Err("nested::start called on a non-nested backend".into());
    };
    let output = data.output.clone();

    // Advertise the host's dmabuf formats, so a wgpu client (the greeter's sky)
    // can hand us GPU buffers instead of going through shared memory.
    let formats = data.backend.renderer().dmabuf_formats();
    output.create_global::<Doorstep>(&state.display_handle);
    state.space.map_output(&output, (0, 0));
    state.dmabuf_global = Some(
        state
            .dmabuf_state
            .create_global::<Doorstep>(&state.display_handle, formats),
    );

    loop_handle.insert_source(winit, move |event, _, state: &mut Doorstep| match event {
        WinitEvent::Resized { size, .. } => {
            output.change_current_state(
                Some(Mode {
                    size,
                    refresh: 60_000,
                }),
                None,
                None,
                None,
            );
            state.space.map_output(&output, (0, 0));
            state.refit_windows();
        }
        WinitEvent::Input(event) => state.process_input_event(event),
        WinitEvent::Redraw => {
            if let Err(err) = redraw(state, &output) {
                tracing::warn!("nested redraw failed: {err}");
            }
        }
        WinitEvent::CloseRequested => state.shutdown(),
        _ => {}
    })?;

    Ok(())
}

fn redraw(state: &mut Doorstep, output: &Output) -> Result<(), Box<dyn std::error::Error>> {
    let Doorstep {
        backend,
        space,
        cursor_status,
        seat,
        start_time,
        display_handle,
        popups,
        ..
    } = state;

    let BackendData::Nested(data) = backend else {
        return Ok(());
    };
    let NestedData {
        backend: winit_backend,
        damage_tracker,
        pointer_buffer,
        ..
    } = &mut **data;

    let pointer_location = seat
        .get_pointer()
        .map(|pointer| pointer.current_location())
        .unwrap_or_default();
    let size = winit_backend.window_size();

    {
        let (renderer, mut framebuffer) = winit_backend.bind()?;
        let elements = render::output_elements(
            space,
            cursor_status,
            pointer_location,
            output,
            renderer,
            pointer_buffer,
        );
        damage_tracker.render_output(
            renderer,
            &mut framebuffer,
            0,
            &elements,
            render::CLEAR_COLOR,
        )?;
    }
    winit_backend.submit(Some(&[Rectangle::from_size(size)]))?;

    for window in space.elements() {
        window.send_frame(
            output,
            start_time.elapsed(),
            Some(Duration::ZERO),
            |_, _| Some(output.clone()),
        );
    }
    space.refresh();
    popups.cleanup();
    let _ = display_handle.flush_clients();

    winit_backend.window().request_redraw();
    Ok(())
}
