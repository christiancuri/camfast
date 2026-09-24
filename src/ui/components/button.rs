//! Push button with primary/secondary/danger looks and a disabled state.

use gpui::{App, ClickEvent, ElementId, IntoElement, RenderOnce, SharedString, Window, div, prelude::*, px};

use crate::ui::theme;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonKind {
    Primary,
    #[default]
    Secondary,
    Danger,
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    kind: ButtonKind,
    disabled: bool,
    tab_index: Option<isize>,
    on_click: Option<ClickHandler>,
}

pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button {
        id: id.into(),
        label: label.into(),
        kind: ButtonKind::Secondary,
        disabled: false,
        tab_index: None,
        on_click: None,
    }
}

impl Button {
    pub fn kind(mut self, kind: ButtonKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Makes the button reachable with Tab; Enter/Space activate it.
    pub fn tab_index(mut self, index: isize) -> Self {
        self.tab_index = Some(index);
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(f));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let (bg, hover_bg, fg, border) = match self.kind {
            ButtonKind::Primary => (theme::accent(), theme::accent().blend(gpui::rgba(0xffffff22)), theme::text(), theme::accent()),
            ButtonKind::Secondary => (theme::surface(), theme::surface_hover(), theme::text(), theme::border()),
            ButtonKind::Danger => (theme::surface(), theme::danger().opacity(0.2), theme::danger(), theme::danger().opacity(0.6)),
        };
        let disabled = self.disabled;
        let on_click = self.on_click;

        div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(px(28.))
            .px_3()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(bg)
            .text_color(fg)
            .text_size(px(13.))
            .whitespace_nowrap()
            .child(self.label)
            .when_some(self.tab_index.filter(|_| !disabled), |this, index| {
                this.tab_index(index).focus(|s| s.border_color(theme::accent()))
            })
            .map(|this| {
                if disabled {
                    this.opacity(0.45)
                } else {
                    this.cursor_pointer()
                        .hover(move |s| s.bg(hover_bg))
                        .active(|s| s.opacity(0.8))
                        .when_some(on_click, |this, f| this.on_click(move |event, window, cx| f(event, window, cx)))
                }
            })
    }
}
