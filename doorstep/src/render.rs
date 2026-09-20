//! Building the frame: the greeter's surface, plus a pointer.

use smithay::{
    backend::renderer::{
        element::{
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
            Kind,
        },
        Color32F, ImportDmaWl, ImportMem, ImportMemWl, Renderer,
    },
    desktop::space::{space_render_elements, SpaceRenderElements},
    desktop::{Space, Window},
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    output::Output,
    utils::{IsAlive, Logical, Point, Scale},
};
use std::sync::Mutex;

/// Black. A login screen letterboxes onto black, and every other output on the
/// card shows black rather than whatever the firmware left in the scanout buffer.
pub const CLEAR_COLOR: Color32F = Color32F::new(0.0, 0.0, 0.0, 1.0);

smithay::backend::renderer::element::render_elements! {
    pub DoorstepElement<R, E> where R: ImportMemWl + ImportDmaWl + ImportMem;
    Space = SpaceRenderElements<R, E>,
    Pointer = MemoryRenderBufferRenderElement<R>,
    PointerSurface = WaylandSurfaceRenderElement<R>,
}

/// What both backends actually build: window surfaces plus a pointer.
pub type OutputElements<R> = DoorstepElement<R, WaylandSurfaceRenderElement<R>>;

/// Everything to draw on `output`, front to back.
///
/// Takes the state apart rather than `&mut Doorstep` so a caller can hold a
/// renderer borrowed out of `Doorstep::backend` at the same time.
pub fn output_elements<R>(
    space: &Space<Window>,
    cursor_status: &mut CursorImageStatus,
    pointer_location: Point<f64, Logical>,
    output: &Output,
    renderer: &mut R,
    pointer_buffer: &MemoryRenderBuffer,
) -> Vec<OutputElements<R>>
where
    R: Renderer + ImportMemWl + ImportDmaWl + ImportMem,
    R::TextureId: Clone + Send + 'static,
{
    let mut elements: Vec<OutputElements<R>> = Vec::new();

    if let Some(output_geometry) = space.output_geometry(output) {
        if output_geometry.to_f64().contains(pointer_location) {
            let scale = Scale::from(output.current_scale().fractional_scale());

            // A client can hand back a cursor surface and then die; fall back to
            // the built-in arrow rather than rendering a dead surface.
            if let CursorImageStatus::Surface(surface) = &*cursor_status {
                if !surface.alive() {
                    *cursor_status = CursorImageStatus::default_named();
                }
            }

            let relative = pointer_location - output_geometry.loc.to_f64();
            match &*cursor_status {
                CursorImageStatus::Hidden => {}
                CursorImageStatus::Named(_) => {
                    if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                        renderer,
                        relative.to_physical(scale),
                        pointer_buffer,
                        None,
                        None,
                        None,
                        Kind::Cursor,
                    ) {
                        elements.push(OutputElements::Pointer(element));
                    }
                }
                CursorImageStatus::Surface(surface) => {
                    let hotspot = smithay::wayland::compositor::with_states(surface, |states| {
                        states
                            .data_map
                            .get::<Mutex<CursorImageAttributes>>()
                            .map(|attributes| attributes.lock().unwrap().hotspot)
                            .unwrap_or_default()
                    });
                    let position = (relative - hotspot.to_f64())
                        .to_physical(scale)
                        .to_i32_round();
                    elements.extend(
                        render_elements_from_surface_tree(
                            renderer,
                            surface,
                            position,
                            scale,
                            1.0,
                            Kind::Cursor,
                        )
                        .into_iter()
                        .map(OutputElements::PointerSurface),
                    );
                }
            }
        }
    }

    if let Ok(space_elements) = space_render_elements(renderer, [space], output, 1.0) {
        elements.extend(space_elements.into_iter().map(OutputElements::Space));
    }

    elements
}
