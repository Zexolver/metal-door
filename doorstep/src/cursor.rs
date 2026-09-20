//! The pointer doorstep draws when the client has not supplied one.
//!
//! It is a bitmap compiled into the binary rather than an XCursor theme lookup.
//! A login screen runs before any user context exists, so reading themes out of
//! `~/.icons`, `/usr/share/icons` and `$XCURSOR_PATH` would mean parsing
//! attacker-influenceable files in the pre-auth path to draw an arrow. This is
//! the arrow.

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    utils::Transform,
};

/// `o` outline, `w` fill, `.` transparent. 12x18, the classic left-pointer.
const ARROW: [&str; 18] = [
    "o...........",
    "oo..........",
    "owo.........",
    "owwo........",
    "owwwo.......",
    "owwwwo......",
    "owwwwwo.....",
    "owwwwwwo....",
    "owwwwwwwo...",
    "owwwwwwwwo..",
    "owwwwwooooo.",
    "owwwwo......",
    "owwoowo.....",
    "owo..owo....",
    "oo...owo....",
    "o.....owo...",
    ".......owo..",
    "........oo..",
];

const WIDTH: i32 = 12;
const HEIGHT: i32 = 18;

/// Decode [`ARROW`] into premultiplied ARGB8888.
fn pixels() -> Vec<u8> {
    let mut out = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for row in ARROW {
        debug_assert_eq!(row.len(), WIDTH as usize);
        for cell in row.bytes() {
            // Little-endian ARGB8888 is stored B, G, R, A.
            out.extend_from_slice(match cell {
                b'o' => &[0x00, 0x00, 0x00, 0xff],
                b'w' => &[0xff, 0xff, 0xff, 0xff],
                _ => &[0x00, 0x00, 0x00, 0x00],
            });
        }
    }
    out
}

/// The built-in pointer as a render buffer. Cheap enough to build once at startup.
pub fn default_pointer() -> MemoryRenderBuffer {
    MemoryRenderBuffer::from_slice(
        &pixels(),
        Fourcc::Argb8888,
        (WIDTH, HEIGHT),
        1,
        Transform::Normal,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arrow_is_rectangular_and_decodes_to_argb() {
        assert!(ARROW.iter().all(|row| row.len() == WIDTH as usize));
        assert_eq!(ARROW.len(), HEIGHT as usize);
        let pixels = pixels();
        assert_eq!(pixels.len(), (WIDTH * HEIGHT * 4) as usize);
        // The hotspot pixel (0,0) is the arrow's tip: opaque outline.
        assert_eq!(&pixels[0..4], &[0x00, 0x00, 0x00, 0xff]);
        // ...and the top-right corner is fully transparent.
        assert_eq!(
            &pixels[(WIDTH as usize - 1) * 4..WIDTH as usize * 4],
            &[0, 0, 0, 0]
        );
    }
}
