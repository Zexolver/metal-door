//! The production backend: DRM/KMS on the greeter VT.
//!
//! Scoped down from Smithay's `anvil` reference to what a login screen needs:
//! one GPU, one renderer, no multi-GPU copy paths, no DRM leasing, no syncobj
//! timeline import, no XWayland. Every connected output on the primary card is
//! lit — the greeter lives on the first one and the rest hold black, rather than
//! whatever the firmware left in the scanout buffer.

use std::{collections::HashMap, io, path::Path, time::Duration};

use smithay::{
    backend::{
        allocator::{
            dmabuf::Dmabuf,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Fourcc,
        },
        drm::{
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
            DrmDevice, DrmDeviceFd, DrmError, DrmEvent, DrmEventMetadata, DrmNode, NodeType,
        },
        egl::{EGLContext, EGLDisplay},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            element::{
                default_primary_scanout_output_compare, memory::MemoryRenderBuffer,
                RenderElementStates,
            },
            gles::GlesRenderer,
            ImportDma, ImportMemWl,
        },
        session::{
            libseat::{LibSeatSession, LibSeatSessionNotifier},
            Event as SessionEvent, Session,
        },
        udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent},
        SwapBuffersError,
    },
    desktop::utils::{
        send_frames_surface_tree, surface_presentation_feedback_flags_from_states,
        surface_primary_scanout_output, update_surface_primary_scanout_output,
        with_surfaces_surface_tree, OutputPresentationFeedback,
    },
    input::keyboard::LedState,
    output::{Mode as WlMode, Output, PhysicalProperties},
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            LoopHandle,
        },
        drm::control::{connector, crtc, ModeTypeFlags},
        input::{DeviceCapability, Libinput},
        rustix::fs::OFlags,
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
    },
    utils::{DeviceFd, Monotonic, Time},
    wayland::presentation::Refresh,
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::{backend::BackendData, cursor, render, state::Doorstep};

/// Widely supported, and in this order: 10-bit first, then 8-bit.
const COLOR_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];

type DoorstepDrmOutput = DrmOutput<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    Option<OutputPresentationFeedback>,
    DrmDeviceFd,
>;

/// Identifies which output a DRM event belongs to.
#[derive(Debug, PartialEq, Eq)]
struct OutputId {
    node: DrmNode,
    crtc: crtc::Handle,
}

struct SurfaceData {
    drm_output: DoorstepDrmOutput,
    global: Option<smithay::reexports::wayland_server::backend::GlobalId>,
}

struct DeviceData {
    drm_output_manager: DrmOutputManager<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        Option<OutputPresentationFeedback>,
        DrmDeviceFd,
    >,
    drm_scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, SurfaceData>,
}

pub struct UdevData {
    pub session: LibSeatSession,
    /// Taken by [`start`]: session pause/resume events cannot be wired into the
    /// event loop until the compositor state they dispatch against exists.
    notifier: Option<LibSeatSessionNotifier>,
    libinput: Libinput,
    primary_gpu: DrmNode,
    renderer: Option<GlesRenderer>,
    device: Option<(DrmNode, DeviceData)>,
    pointer_buffer: MemoryRenderBuffer,
    /// Physical keyboards, so caps-lock actually lights the key. The greeter
    /// shows a caps-lock indicator (D-0012); the LED should agree with it.
    keyboards: Vec<smithay::reexports::input::Device>,
}

impl UdevData {
    pub fn seat_name(&self) -> String {
        self.session.seat()
    }

    pub fn change_vt(&mut self, vt: i32) {
        if let Err(err) = self.session.change_vt(vt) {
            tracing::warn!("failed to switch to VT {vt}: {err}");
        }
    }

    pub fn update_led_state(&mut self, led_state: LedState) {
        for keyboard in &mut self.keyboards {
            keyboard.led_update(led_state.into());
        }
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        match self.renderer.as_mut() {
            Some(renderer) => renderer.import_dmabuf(dmabuf, None).is_ok(),
            None => false,
        }
    }
}

/// Take the seat and find the GPU. No devices are opened yet — that needs the
/// display handle, so it happens in [`start`].
pub fn init() -> Result<UdevData, Box<dyn std::error::Error>> {
    let (session, notifier) = LibSeatSession::new()?;
    let seat = session.seat();
    let gpu = primary_gpu(&seat)?
        .and_then(|path| DrmNode::from_path(path).ok())
        .or_else(|| {
            all_gpus(&seat)
                .ok()?
                .into_iter()
                .find_map(|path| DrmNode::from_path(path).ok())
        })
        .ok_or("no GPU found on this seat")?;
    tracing::info!("primary GPU: {gpu}");

    let mut libinput =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput
        .udev_assign_seat(&seat)
        .map_err(|_| "failed to assign the libinput seat")?;

    Ok(UdevData {
        session,
        notifier: Some(notifier),
        libinput,
        primary_gpu: gpu,
        renderer: None,
        device: None,
        pointer_buffer: cursor::default_pointer(),
        keyboards: Vec::new(),
    })
}

/// Wire up input, session and udev events, open the GPU, and light the outputs.
pub fn start(
    state: &mut Doorstep,
    loop_handle: &LoopHandle<'static, Doorstep>,
) -> Result<(), Box<dyn std::error::Error>> {
    let BackendData::Udev(data) = &mut state.backend else {
        return Err("udev::start called on a non-udev backend".into());
    };
    let seat = data.session.seat();
    let primary = data.primary_gpu;
    let libinput_context = data.libinput.clone();
    let notifier = data
        .notifier
        .take()
        .ok_or("session notifier already consumed")?;

    loop_handle.insert_source(
        LibinputInputBackend::new(data.libinput.clone()),
        |mut event, _, state: &mut Doorstep| {
            // Keyboard hotplug is tracked only to keep the LEDs in sync; the
            // events themselves carry nothing a single-client kiosk acts on.
            match &mut event {
                InputEvent::DeviceAdded { device }
                    if device.has_capability(DeviceCapability::Keyboard) =>
                {
                    if let Some(keyboard) = state.seat.get_keyboard() {
                        device.led_update(keyboard.led_state().into());
                    }
                    if let BackendData::Udev(data) = &mut state.backend {
                        data.keyboards.push(device.clone());
                    }
                }
                InputEvent::DeviceRemoved { device } => {
                    if let BackendData::Udev(data) = &mut state.backend {
                        data.keyboards.retain(|tracked| tracked != device);
                    }
                }
                _ => {}
            }
            state.process_input_event(event);
        },
    )?;

    let mut paused_libinput = libinput_context;
    loop_handle.insert_source(
        notifier,
        move |event, _, state: &mut Doorstep| match event {
            SessionEvent::PauseSession => {
                // A VT switch away: stop reading devices and let go of KMS.
                paused_libinput.suspend();
                if let BackendData::Udev(data) = &mut state.backend {
                    if let Some((_, device)) = data.device.as_mut() {
                        device.drm_output_manager.pause();
                    }
                }
            }
            SessionEvent::ActivateSession => {
                if let Err(err) = paused_libinput.resume() {
                    tracing::warn!("failed to resume libinput: {err:?}");
                }
                if let BackendData::Udev(data) = &mut state.backend {
                    if let Some((_, device)) = data.device.as_mut() {
                        if let Err(err) = device.drm_output_manager.activate(false) {
                            tracing::warn!("failed to reactivate DRM: {err}");
                        }
                    }
                }
                state.render_all(state.clock.now());
            }
        },
    )?;

    let udev_backend = UdevBackend::new(&seat)?;
    let device_path = udev_backend
        .device_list()
        .find(|(device_id, _)| {
            DrmNode::from_dev_id(*device_id)
                .map(|node| node.dev_id() == primary.dev_id() || same_card(node, primary))
                .unwrap_or(false)
        })
        .map(|(_, path)| path.to_path_buf())
        .or_else(|| {
            // Fall back to the first card udev knows about: a machine with one
            // GPU whose render node we picked above still needs its card node.
            udev_backend
                .device_list()
                .next()
                .map(|(_, path)| path.to_path_buf())
        })
        .ok_or("no DRM device to open")?;

    state.add_device(&device_path, loop_handle)?;

    // Hotplug: connectors coming and going on the card we already own.
    loop_handle.insert_source(
        udev_backend,
        move |event, _, state: &mut Doorstep| match event {
            UdevEvent::Changed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    state.device_changed(node);
                }
            }
            UdevEvent::Added { .. } | UdevEvent::Removed { .. } => {
                // doorstep drives exactly one card, chosen at startup. Adopting a
                // card mid-greet would mean re-homing the greeter's only surface.
            }
        },
    )?;

    Ok(())
}

/// Two nodes belonging to the same physical card (primary vs. render node).
fn same_card(a: DrmNode, b: DrmNode) -> bool {
    match (
        a.node_with_type(NodeType::Primary),
        b.node_with_type(NodeType::Primary),
    ) {
        (Some(Ok(a)), Some(Ok(b))) => a.dev_id() == b.dev_id(),
        _ => false,
    }
}

impl Doorstep {
    fn udev(&mut self) -> Option<&mut UdevData> {
        match &mut self.backend {
            BackendData::Udev(data) => Some(data),
            #[cfg(feature = "nested")]
            _ => None,
        }
    }

    fn add_device(
        &mut self,
        path: &Path,
        loop_handle: &LoopHandle<'static, Doorstep>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(data) = self.udev() else {
            return Ok(());
        };

        let fd = data.session.open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));
        let (drm, notifier) = DrmDevice::new(fd.clone(), true)?;
        let gbm = GbmDevice::new(fd)?;
        let node = DrmNode::from_dev_id(drm.device_id())?;

        let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
        let egl_context = EGLContext::new(&egl_display)?;
        let render_formats: FormatSet = egl_context.dmabuf_render_formats().clone();
        let renderer = unsafe { GlesRenderer::new(egl_context)? };

        // Never removed: doorstep drives exactly one card from startup until the
        // process exits, and the event loop goes away with it.
        loop_handle.insert_source(notifier, move |event, metadata, state: &mut Doorstep| {
            match event {
                DrmEvent::VBlank(crtc) => state.frame_submitted(node, crtc, metadata),
                DrmEvent::Error(err) => tracing::warn!("DRM error: {err:?}"),
            }
        })?;

        let drm_output_manager = DrmOutputManager::new(
            drm,
            GbmAllocator::new(
                gbm.clone(),
                GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
            ),
            GbmFramebufferExporter::new(
                gbm.clone(),
                node.node_with_type(NodeType::Render).and_then(|n| n.ok()),
            ),
            Some(gbm),
            COLOR_FORMATS.iter().copied(),
            render_formats,
        );

        let shm_formats = renderer.shm_formats();
        let dmabuf_formats = renderer.dmabuf_formats();

        let Some(data) = self.udev() else {
            return Ok(());
        };
        data.renderer = Some(renderer);
        data.device = Some((
            node,
            DeviceData {
                drm_output_manager,
                drm_scanner: DrmScanner::new(),
                surfaces: HashMap::new(),
            },
        ));

        self.shm_state.update_formats(shm_formats);
        self.dmabuf_global = Some(
            self.dmabuf_state
                .create_global::<Doorstep>(&self.display_handle, dmabuf_formats),
        );

        self.device_changed(node);
        Ok(())
    }

    /// Rescan connectors and bring up (or tear down) their outputs.
    fn device_changed(&mut self, node: DrmNode) {
        let scan = {
            let Some(data) = self.udev() else { return };
            let Some((device_node, device)) = data.device.as_mut() else {
                return;
            };
            if *device_node != node {
                return;
            }
            match device
                .drm_scanner
                .scan_connectors(device.drm_output_manager.device())
            {
                Ok(scan) => scan.into_iter().collect::<Vec<_>>(),
                Err(err) => {
                    tracing::warn!("failed to scan connectors: {err}");
                    return;
                }
            }
        };

        for event in scan {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_connected(node, connector, crtc),
                DrmScanEvent::Disconnected {
                    crtc: Some(crtc), ..
                } => self.connector_disconnected(node, crtc),
                _ => {}
            }
        }

        self.refit_windows();
    }

    fn connector_connected(
        &mut self,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );

        let mode_index = connector
            .modes()
            .iter()
            .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
            .unwrap_or(0);
        let Some(drm_mode) = connector.modes().get(mode_index).copied() else {
            tracing::warn!("connector {name} reports no modes; skipping");
            return;
        };
        let wl_mode = WlMode::from(drm_mode);
        let (width, height) = connector.size().unwrap_or((0, 0));

        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: (width as i32, height as i32).into(),
                subpixel: connector.subpixel().into(),
                // EDID make/model would mean parsing display-supplied bytes with
                // a C library before login. The connector name is enough.
                make: "Unknown".into(),
                model: "Unknown".into(),
            },
        );
        let global = output.create_global::<Doorstep>(&self.display_handle);
        let position = (
            self.space
                .outputs()
                .filter_map(|o| self.space.output_geometry(o))
                .map(|geometry| geometry.size.w)
                .sum::<i32>(),
            0,
        );
        output.set_preferred(wl_mode);
        output.change_current_state(Some(wl_mode), None, None, Some(position.into()));
        self.space.map_output(&output, position);
        output
            .user_data()
            .insert_if_missing(|| OutputId { node, crtc });

        let planes = {
            let Some(data) = self.udev() else { return };
            let Some((_, device)) = data.device.as_ref() else {
                return;
            };
            device.drm_output_manager.device().planes(&crtc).ok()
        };

        let drm_output = {
            let BackendData::Udev(data) = &mut self.backend else {
                return;
            };
            let UdevData {
                renderer, device, ..
            } = &mut **data;
            let (Some(renderer), Some((_, device))) = (renderer.as_mut(), device.as_mut()) else {
                return;
            };
            match device
                .drm_output_manager
                .initialize_output::<_, render::OutputElements<GlesRenderer>>(
                    crtc,
                    drm_mode,
                    &[connector.handle()],
                    &output,
                    planes,
                    renderer,
                    &DrmOutputRenderElements::default(),
                ) {
                Ok(drm_output) => drm_output,
                Err(err) => {
                    tracing::warn!("failed to bring up {name}: {err}");
                    return;
                }
            }
        };

        tracing::info!("lit {name} at {}x{}", drm_mode.size().0, drm_mode.size().1);
        if let Some(data) = self.udev() {
            if let Some((_, device)) = data.device.as_mut() {
                device.surfaces.insert(
                    crtc,
                    SurfaceData {
                        drm_output,
                        global: Some(global),
                    },
                );
            }
        }

        self.refit_windows();
        let now = self.clock.now();
        self.render(node, crtc, now);
    }

    fn connector_disconnected(&mut self, node: DrmNode, crtc: crtc::Handle) {
        let removed = self
            .udev()
            .and_then(|data| data.device.as_mut())
            .and_then(|(_, device)| device.surfaces.remove(&crtc));

        if let Some(surface) = removed {
            if let Some(global) = surface.global {
                self.display_handle.remove_global::<Doorstep>(global);
            }
        }

        let output = self
            .space
            .outputs()
            .find(|output| output.user_data().get::<OutputId>() == Some(&OutputId { node, crtc }))
            .cloned();
        if let Some(output) = output {
            self.space.unmap_output(&output);
        }
        self.refit_windows();
    }

    fn output_for(&self, node: DrmNode, crtc: crtc::Handle) -> Option<Output> {
        self.space
            .outputs()
            .find(|output| output.user_data().get::<OutputId>() == Some(&OutputId { node, crtc }))
            .cloned()
    }

    pub(crate) fn render_all(&mut self, frame_target: Time<Monotonic>) {
        let targets: Vec<(DrmNode, crtc::Handle)> = match &self.backend {
            BackendData::Udev(data) => data
                .device
                .as_ref()
                .map(|(node, device)| device.surfaces.keys().map(|crtc| (*node, *crtc)).collect())
                .unwrap_or_default(),
            #[cfg(feature = "nested")]
            _ => Vec::new(),
        };
        for (node, crtc) in targets {
            self.render(node, crtc, frame_target);
        }
    }

    /// Compose one output and queue it for the next vblank.
    fn render(&mut self, node: DrmNode, crtc: crtc::Handle, frame_target: Time<Monotonic>) {
        let Some(output) = self.output_for(node, crtc) else {
            return;
        };

        let result = {
            let Doorstep {
                backend,
                space,
                cursor_status,
                seat,
                ..
            } = self;
            let BackendData::Udev(data) = backend else {
                return;
            };
            let UdevData {
                renderer,
                device,
                pointer_buffer,
                ..
            } = &mut **data;
            let (Some(renderer), Some((_, device))) = (renderer.as_mut(), device.as_mut()) else {
                return;
            };
            let Some(surface) = device.surfaces.get_mut(&crtc) else {
                return;
            };

            let pointer_location = seat
                .get_pointer()
                .map(|pointer| pointer.current_location())
                .unwrap_or_default();
            let elements = render::output_elements(
                space,
                cursor_status,
                pointer_location,
                &output,
                renderer,
                pointer_buffer,
            );

            surface
                .drm_output
                .render_frame(
                    renderer,
                    &elements,
                    render::CLEAR_COLOR,
                    FrameFlags::DEFAULT,
                )
                .map(|frame| (!frame.is_empty, frame.states))
                .map_err(|err| match err {
                    smithay::backend::drm::compositor::RenderFrameError::PrepareFrame(err) => {
                        SwapBuffersError::from(err)
                    }
                    smithay::backend::drm::compositor::RenderFrameError::RenderFrame(
                        smithay::backend::renderer::damage::Error::Rendering(err),
                    ) => SwapBuffersError::from(err),
                    other => SwapBuffersError::ContextLost(Box::new(other)),
                })
        };

        match result {
            Ok((rendered, states)) => {
                self.update_scanout_output(&output, &states);
                if rendered {
                    let feedback = self.presentation_feedback(&output, &states);
                    let queued = match self.surface_mut(crtc) {
                        Some(surface) => surface.drm_output.queue_frame(Some(feedback)),
                        None => Ok(()),
                    };
                    if let Err(err) = queued {
                        // No vblank will arrive for a frame that was never queued,
                        // and the vblank is what schedules the next render — so
                        // re-arm here or the screen freezes for good.
                        tracing::warn!("failed to queue a frame: {err}");
                        self.schedule_repaint(node, crtc, &output, frame_target);
                    }
                } else {
                    // Nothing changed. Re-test for damage in a frame's time
                    // instead of spinning on an idle screen.
                    self.schedule_repaint(node, crtc, &output, frame_target);
                }
                self.send_frames(&output, &states);
            }
            Err(SwapBuffersError::TemporaryFailure(err)) => {
                let inactive = matches!(
                    err.downcast_ref::<DrmError>(),
                    Some(DrmError::DeviceInactive)
                );
                let denied = matches!(
                    err.downcast_ref::<DrmError>(),
                    Some(DrmError::Access(access)) if access.source.kind() == io::ErrorKind::PermissionDenied
                );
                if !inactive && !denied {
                    tracing::warn!("temporary render failure: {err}");
                }
                // Both cases mean "we do not hold the VT right now"; the session
                // resume path kicks rendering off again.
            }
            Err(err) => tracing::warn!("render failed: {err}"),
        }
    }

    fn surface_mut(&mut self, crtc: crtc::Handle) -> Option<&mut SurfaceData> {
        match &mut self.backend {
            BackendData::Udev(data) => data
                .device
                .as_mut()
                .and_then(|(_, device)| device.surfaces.get_mut(&crtc)),
            #[cfg(feature = "nested")]
            _ => None,
        }
    }

    fn schedule_repaint(
        &mut self,
        node: DrmNode,
        crtc: crtc::Handle,
        output: &Output,
        frame_target: Time<Monotonic>,
    ) {
        let Some(refresh) = output.current_mode().map(|mode| mode.refresh) else {
            return;
        };
        let next = frame_target + Duration::from_millis(1_000_000 / refresh.max(1) as u64);
        let delay = Duration::from(next).saturating_sub(self.clock.now().into());
        let _ = self.loop_handle.insert_source(
            Timer::from_duration(delay),
            move |_, _, state: &mut Doorstep| {
                state.render(node, crtc, next);
                TimeoutAction::Drop
            },
        );
    }

    /// A page flip completed: report presentation and schedule the next frame.
    fn frame_submitted(
        &mut self,
        node: DrmNode,
        crtc: crtc::Handle,
        metadata: &mut Option<DrmEventMetadata>,
    ) {
        let Some(output) = self.output_for(node, crtc) else {
            return;
        };
        let Some(refresh) = output.current_mode().map(|mode| mode.refresh) else {
            return;
        };
        let frame_duration = Duration::from_secs_f64(1_000f64 / refresh as f64);

        let hardware_time = metadata.as_ref().and_then(|metadata| match metadata.time {
            smithay::backend::drm::DrmEventTime::Monotonic(time) if !time.is_zero() => Some(time),
            _ => None,
        });
        let sequence = metadata
            .as_ref()
            .map(|metadata| metadata.sequence)
            .unwrap_or(0);
        let (clock, flags) = match hardware_time {
            Some(time) => (
                time.into(),
                wp_presentation_feedback::Kind::Vsync
                    | wp_presentation_feedback::Kind::HwClock
                    | wp_presentation_feedback::Kind::HwCompletion,
            ),
            None => (self.clock.now(), wp_presentation_feedback::Kind::Vsync),
        };

        let submitted = self
            .surface_mut(crtc)
            .map(|surface| surface.drm_output.frame_submitted());

        match submitted {
            Some(Ok(feedback)) => {
                if let Some(mut feedback) = feedback.flatten() {
                    feedback.presented(
                        clock,
                        Refresh::fixed(frame_duration),
                        sequence as u64,
                        flags,
                    );
                }
                // Repaint a little before the next vblank: the client gets most
                // of the frame to draw, we get the tail.
                let next = clock + frame_duration;
                let delay = frame_duration.mul_f64(0.6);
                let _ = self.loop_handle.insert_source(
                    Timer::from_duration(delay),
                    move |_, _, state: &mut Doorstep| {
                        state.render(node, crtc, next);
                        TimeoutAction::Drop
                    },
                );
            }
            Some(Err(err)) => tracing::warn!("frame submission failed: {err}"),
            None => {}
        }
    }

    fn update_scanout_output(&mut self, output: &Output, states: &RenderElementStates) {
        for window in self.space.elements() {
            window.with_surfaces(|surface, surface_states| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    surface_states,
                    states,
                    default_primary_scanout_output_compare,
                );
            });
        }
        if let smithay::input::pointer::CursorImageStatus::Surface(surface) = &self.cursor_status {
            with_surfaces_surface_tree(surface, |surface, surface_states| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    surface_states,
                    states,
                    default_primary_scanout_output_compare,
                );
            });
        }
    }

    fn presentation_feedback(
        &self,
        output: &Output,
        states: &RenderElementStates,
    ) -> OutputPresentationFeedback {
        let mut feedback = OutputPresentationFeedback::new(output);
        for window in self.space.elements() {
            if self.space.outputs_for_element(window).contains(output) {
                window.take_presentation_feedback(
                    &mut feedback,
                    surface_primary_scanout_output,
                    |surface, _| surface_presentation_feedback_flags_from_states(surface, states),
                );
            }
        }
        feedback
    }

    fn send_frames(&mut self, output: &Output, _states: &RenderElementStates) {
        let time = self.start_time.elapsed();
        for window in self.space.elements() {
            window.send_frame(
                output,
                time,
                Some(Duration::ZERO),
                surface_primary_scanout_output,
            );
        }
        if let smithay::input::pointer::CursorImageStatus::Surface(surface) = &self.cursor_status {
            send_frames_surface_tree(surface, output, time, Some(Duration::ZERO), |_, _| None);
        }
    }
}

/// Drop the device cleanly when doorstep exits: releasing the DRM master is what
/// lets `doord`'s session worker take the VT (D-0008).
impl Drop for UdevData {
    fn drop(&mut self) {
        self.device = None;
        self.renderer = None;
    }
}
