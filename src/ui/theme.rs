//! Colors shared by the mosaic and the settings window (dark UI, like the camera's web UI).

use gpui::{Hsla, Rgba, rgb, rgba};

pub fn background() -> Rgba {
    rgb(0x0f1113)
}
pub fn surface() -> Rgba {
    rgb(0x1b1f23)
}
pub fn surface_hover() -> Rgba {
    rgb(0x262b30)
}
pub fn border() -> Rgba {
    rgb(0x30363d)
}
pub fn text() -> Rgba {
    rgb(0xf0f3f6)
}
pub fn text_muted() -> Rgba {
    rgb(0x8b949e)
}
pub fn accent() -> Rgba {
    rgb(0x2f81f7)
}
pub fn danger() -> Rgba {
    rgb(0xf85149)
}
pub fn success() -> Rgba {
    rgb(0x3fb950)
}
pub fn warning() -> Rgba {
    rgb(0xd29922)
}
/// Hairline between mosaic tiles (tiles themselves are pure black).
pub fn tile_gap() -> Rgba {
    rgb(0x1f2327)
}
pub fn overlay_scrim() -> Rgba {
    rgba(0x00000099)
}
pub fn label_scrim() -> Hsla {
    gpui::hsla(0.0, 0.0, 0.0, 0.55)
}
