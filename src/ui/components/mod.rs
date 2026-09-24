//! Small reusable widgets (GPUI has no built-in text input or dropdown).

pub mod button;
pub mod checkbox;
pub mod dropdown;
pub mod text_input;

pub use button::{ButtonKind, button};
pub use checkbox::checkbox;
pub use dropdown::{Dropdown, DropdownEvent, DropdownItem};
pub use text_input::{TextInput, TextInputEvent};

use gpui::App;

/// Registers key bindings/actions used by the components. Called once at startup.
pub fn init(cx: &mut App) {
    text_input::init(cx);
    dropdown::init(cx);
}
