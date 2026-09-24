//! Labeled checkbox. Stateless: the caller owns the value and reacts in `on_change`.

use gpui::{App, ElementId, SharedString, Stateful, Window, div, prelude::*, px};

use crate::ui::theme;

pub fn checkbox(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    checked: bool,
    on_change: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> Stateful<gpui::Div> {
    let box_bg = if checked { theme::accent() } else { theme::surface() };
    let box_border = if checked { theme::accent() } else { theme::border() };
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .focus(|s| s.text_color(theme::accent()))
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(16.))
                .rounded_sm()
                .border_1()
                .border_color(box_border)
                .bg(box_bg)
                .text_color(theme::text())
                .text_size(px(11.))
                .when(checked, |this| this.child("✓")),
        )
        .child(div().text_size(px(13.)).child(label.into()))
        .on_click(move |_, window, cx| on_change(&!checked, window, cx))
}
