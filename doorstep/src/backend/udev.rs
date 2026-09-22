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
        drm::{
            control::{connector, crtc, Device as ControlDevice, ModeTypeFlags},
            Device as DrmDeviceTrait,
        },
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
    /// Kept for diagnostics; `device_path` is what actually gets opened.
    #[allow(dead_code)]
    primary_gpu: DrmNode,
    /// The card node [`select_drm_device`] settled on.
    device_path: std::path::PathBuf,
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
/// Explain, before libseat does it in errno, why this process cannot drive a VT.
///
/// Both of the ways to get this wrong produce errors that name neither the cause
/// nor the fix: no runtime dir surfaces as a socket bind failure, and no seat
/// surfaces as `EOPNOTSUPP` out of libseat. doorstep is a thing people run by
/// hand on a TTY while testing, so it should say what is actually wrong.
fn check_session_environment() -> Result<(), String> {
    let mut problems = Vec::new();

    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        problems.push(
            "XDG_RUNTIME_DIR is not set, so there is nowhere to put the Wayland socket \
             (running under `sudo`/`su` clears it)",
        );
    }

    // A remote session gets a runtime dir but never a seat, so this is the check
    // that catches "ran it over SSH".
    let has_seat = std::env::var("XDG_SEAT")
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !has_seat {
        problems.push(
            "XDG_SEAT is not set, so this session owns no seat and cannot take the \
             display (an SSH session never can; `sudo` drops it too)",
        );
    }

    if problems.is_empty() {
        return Ok(());
    }

    Err(format!(
        "doorstep needs to run as your own user, in the logind session on the VT it \
         should draw on — not under sudo and not over SSH.\n  - {}",
        problems.join("\n  - ")
    ))
}

pub fn init() -> Result<UdevData, Box<dyn std::error::Error>> {
    if let Err(problem) = check_session_environment() {
        return Err(problem.into());
    }

    let (session, notifier) = LibSeatSession::new().map_err(|err| {
        format!(
            "could not take a seat via libseat ({err}). The session is not one logind \
             will hand the display to: check `loginctl show-session $XDG_SESSION_ID \
             -p Seat -p Active -p Remote`."
        )
    })?;
    let seat = session.seat();
    let (gpu, device_path) = select_drm_device(&mut session.clone(), &seat)?;
    tracing::info!("primary GPU: {gpu} ({})", device_path.display());

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
        device_path,
        renderer: None,
        device: None,
        pointer_buffer: cursor::default_pointer(),
        keyboards: Vec::new(),
    })
}

/// What a probe of one DRM node found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DeviceProbe {
    crtcs: usize,
    connectors: usize,
    /// Connectors reporting a display actually plugged in.
    connected: usize,
    driver: Option<String>,
}

impl DeviceProbe {
    /// A node that can modeset at all. Render-only nodes have neither.
    fn drives_a_display(&self) -> bool {
        self.crtcs > 0 && self.connectors > 0
    }
}

/// Probe one DRM node.
///
/// On a PC the GPU and the display controller are the same device, so any node
/// will do. On most ARM SoCs they are not: `panfrost`/`lima`/`v3d` render but
/// have no modesetting at all, while a separate `rockchip-drm`/`sun4i-drm`/`vc4`
/// owns the outputs — and which of them becomes `card0` is just probe order.
/// Asking a render-only node for its connectors fails with `EOPNOTSUPP`, so
/// picking by order lands on the wrong device roughly half the time.
///
/// (Mesa's `kmsro` pairs a display-only node with the SoC's render node behind
/// GBM/EGL, so choosing the KMS node here is right on split SoCs too — doorstep
/// does not need its own multi-GPU path for them.)
fn probe_device(session: &mut LibSeatSession, path: &Path) -> Result<DeviceProbe, String> {
    let fd = session
        .open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .map_err(|err| format!("{err}"))?;
    // The fd closes when it drops at the end of this scope.
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let driver = DrmDeviceTrait::get_driver(&fd)
        .ok()
        .map(|info| info.name().to_string_lossy().into_owned());

    let resources = ControlDevice::resource_handles(&fd).map_err(|err| format!("{err}"))?;

    // `force_probe: false` — asking the kernel to re-probe every connector can
    // take seconds per output, and a stale "disconnected" only costs this node
    // its preference, never its eligibility.
    let connected = resources
        .connectors()
        .iter()
        .filter(|handle| {
            ControlDevice::get_connector(&fd, **handle, false)
                .map(|conn| conn.state() == connector::State::Connected)
                .unwrap_or(false)
        })
        .count();

    Ok(DeviceProbe {
        crtcs: resources.crtcs().len(),
        connectors: resources.connectors().len(),
        connected,
        driver,
    })
}

/// Choose among probed nodes: a node with something plugged in beats one with
/// only unused outputs, and otherwise the earlier candidate wins.
///
/// The "connected" preference is not an ARM concern — it is what keeps a muxless
/// amd64 laptop from picking the discrete GPU, whose connectors exist but have
/// no panel behind them, over the integrated one that drives the screen.
fn best_candidate(probes: &[(std::path::PathBuf, DeviceProbe)]) -> Option<usize> {
    let usable: Vec<usize> = (0..probes.len())
        .filter(|&i| probes[i].1.drives_a_display())
        .collect();
    usable
        .iter()
        .copied()
        .find(|&i| probes[i].1.connected > 0)
        .or_else(|| usable.first().copied())
}

/// Pick the DRM node doorstep will drive. `DOORSTEP_DRM_DEVICE` overrides the
/// search entirely.
fn select_drm_device(
    session: &mut LibSeatSession,
    seat: &str,
) -> Result<(DrmNode, std::path::PathBuf), Box<dyn std::error::Error>> {
    if let Some(forced) = std::env::var_os("DOORSTEP_DRM_DEVICE") {
        let path = std::path::PathBuf::from(forced);
        let node = DrmNode::from_path(&path).map_err(|err| {
            format!(
                "DOORSTEP_DRM_DEVICE={} is not a DRM node: {err}",
                path.display()
            )
        })?;
        match probe_device(session, &path) {
            Ok(probe) if probe.drives_a_display() => {}
            Ok(_) => tracing::warn!(
                "DOORSTEP_DRM_DEVICE={} reports no connectors or CRTCs; using it anyway",
                path.display()
            ),
            Err(err) => tracing::warn!(
                "could not probe DOORSTEP_DRM_DEVICE={}: {err}; using it anyway",
                path.display()
            ),
        }
        return Ok((node, path));
    }

    // The seat's primary GPU first — on amd64 that is the boot_vga device and
    // almost always the right answer, so probing must not reorder it away.
    // ARM has no boot_vga, so there the order is just udev's.
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(Some(primary)) = primary_gpu(seat) {
        candidates.push(primary);
    }
    if let Ok(all) = all_gpus(seat) {
        for path in all {
            if !candidates.contains(&path) {
                candidates.push(path);
            }
        }
    }

    if candidates.is_empty() {
        return Err(format!(
            "no GPU on seat `{seat}` — nothing in /dev/dri that logind will lease out"
        )
        .into());
    }

    let mut probes = Vec::new();
    let mut rejected = Vec::new();
    for path in candidates {
        if DrmNode::from_path(&path).is_err() {
            rejected.push(format!("{}: not a DRM node", path.display()));
            continue;
        }
        match probe_device(session, &path) {
            Ok(probe) => {
                tracing::debug!(
                    "probed {} (driver {}): {} crtcs, {} connectors, {} connected",
                    path.display(),
                    probe.driver.as_deref().unwrap_or("unknown"),
                    probe.crtcs,
                    probe.connectors,
                    probe.connected
                );
                if !probe.drives_a_display() {
                    rejected.push(format!(
                        "{} (driver {}): render-only, no connectors or CRTCs",
                        path.display(),
                        probe.driver.as_deref().unwrap_or("unknown")
                    ));
                }
                probes.push((path, probe));
            }
            Err(err) => rejected.push(format!("{}: {err}", path.display())),
        }
    }

    match best_candidate(&probes) {
        Some(index) => {
            let (path, probe) = &probes[index];
            let node = DrmNode::from_path(path)?;
            if probe.connected == 0 {
                tracing::warn!(
                    "{} has no connected output; using it anyway as nothing else qualifies",
                    path.display()
                );
            }
            tracing::info!(
                "using {} (driver {}, {} connected of {} connectors)",
                path.display(),
                probe.driver.as_deref().unwrap_or("unknown"),
                probe.connected,
                probe.connectors
            );
            Ok((node, path.clone()))
        }
        None => Err(format!(
            "none of this seat's DRM devices can drive a display:\n  - {}\n\
             Set DOORSTEP_DRM_DEVICE=/dev/dri/cardN to force one.",
            rejected.join("\n  - ")
        )
        .into()),
    }
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
    let libinput_context = data.libinput.clone();
    let device_path = data.device_path.clone();
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

#[cfg(test)]
mod tests {
    use super::{best_candidate, DeviceProbe};
    use std::path::PathBuf;

    fn probe(crtcs: usize, connectors: usize, connected: usize, driver: &str) -> DeviceProbe {
        DeviceProbe {
            crtcs,
            connectors,
            connected,
            driver: Some(driver.to_string()),
        }
    }

    fn candidates(entries: &[(&str, DeviceProbe)]) -> Vec<(PathBuf, DeviceProbe)> {
        entries
            .iter()
            .map(|(path, probe)| (PathBuf::from(path), probe.clone()))
            .collect()
    }

    #[test]
    fn arm_soc_skips_the_render_only_node() {
        // The failure this selection exists for: panfrost probes first and owns
        // card0, but every output hangs off the display controller on card1.
        let probes = candidates(&[
            ("/dev/dri/card0", probe(0, 0, 0, "panfrost")),
            ("/dev/dri/card1", probe(2, 3, 1, "rockchip-drm")),
        ]);
        let picked = best_candidate(&probes).expect("a KMS node is available");
        assert_eq!(probes[picked].0, PathBuf::from("/dev/dri/card1"));
    }

    #[test]
    fn amd64_keeps_the_primary_gpu_first() {
        // A single-GPU PC must land on the node udev named first, exactly as it
        // did before any of this probing existed.
        let probes = candidates(&[("/dev/dri/card0", probe(4, 5, 2, "amdgpu"))]);
        assert_eq!(best_candidate(&probes), Some(0));
    }

    #[test]
    fn a_plugged_in_output_beats_an_idle_one() {
        // Muxless laptop: the discrete GPU has connectors but no panel behind
        // them, and picking it would light nothing.
        let probes = candidates(&[
            ("/dev/dri/card0", probe(4, 4, 0, "nvidia-drm")),
            ("/dev/dri/card1", probe(3, 4, 1, "i915")),
        ]);
        let picked = best_candidate(&probes).expect("a connected node is available");
        assert_eq!(probes[picked].0, PathBuf::from("/dev/dri/card1"));
    }

    #[test]
    fn a_kms_node_with_nothing_plugged_in_is_still_better_than_none() {
        // Headless or a display asleep at startup: still the only thing that can
        // modeset, so use it rather than refusing to start.
        let probes = candidates(&[
            ("/dev/dri/card0", probe(0, 0, 0, "v3d")),
            ("/dev/dri/card1", probe(2, 2, 0, "vc4")),
        ]);
        let picked = best_candidate(&probes).expect("a KMS node is available");
        assert_eq!(probes[picked].0, PathBuf::from("/dev/dri/card1"));
    }

    #[test]
    fn render_only_everywhere_selects_nothing() {
        let probes = candidates(&[
            ("/dev/dri/card0", probe(0, 0, 0, "panfrost")),
            ("/dev/dri/card1", probe(0, 0, 0, "lima")),
        ]);
        assert_eq!(best_candidate(&probes), None);
    }

    #[test]
    fn crtcs_without_connectors_does_not_count_as_modesetting() {
        assert!(!probe(2, 0, 0, "odd").drives_a_display());
        assert!(!probe(0, 2, 0, "odd").drives_a_display());
        assert!(probe(1, 1, 0, "ok").drives_a_display());
    }
}
