//! Dropdown (select) and the floating list it uses, which is also reusable as a context menu.

use std::rc::Rc;

use gpui::{
    App, ClickEvent, Context, Div, ElementId, EventEmitter, FocusHandle, Focusable, KeyBinding, SharedString, Stateful,
    Window, actions, anchored, deferred, div, point, prelude::*, px,
};

use crate::ui::theme;

actions!(dropdown, [Next, Prev, Confirm, Close]);

const KEY_CONTEXT: &str = "Dropdown";
const HEIGHT: f32 = 30.;

pub(super) fn init(cx: &mut App) {
    let ctx = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("down", Next, ctx),
        KeyBinding::new("up", Prev, ctx),
        KeyBinding::new("enter", Confirm, ctx),
        KeyBinding::new("space", Confirm, ctx),
        KeyBinding::new("escape", Close, ctx),
    ]);
}

/// One row of a floating menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItem {
    pub label: SharedString,
    /// Muted text after the label, e.g. "(in Top right)".
    pub hint: Option<SharedString>,
    /// Draws a check mark in front of the label.
    pub selected: bool,
}

impl MenuItem {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self { label: label.into(), hint: None, selected: false }
    }

    pub fn hint(mut self, hint: impl Into<SharedString>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
}

/// Floating list of rows. Wrap it in `deferred(anchored()...)` and add `on_mouse_down_out` to
/// close it. `highlighted` is the keyboard-highlighted row.
pub fn menu_panel(
    id: impl Into<ElementId>,
    items: Vec<MenuItem>,
    highlighted: Option<usize>,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let on_select = Rc::new(on_select);
    div()
        .id(id)
        // Without occlusion a mouse-down on a row also hits the focusable elements underneath,
        // which steals focus from the dropdown and closes it before the click completes.
        .occlude()
        .flex()
        .flex_col()
        .min_w(px(200.))
        .max_h(px(280.))
        .overflow_y_scroll()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(theme::border())
        .bg(theme::surface())
        .shadow_lg()
        .text_size(px(13.))
        .text_color(theme::text())
        .children(items.into_iter().enumerate().map(|(ix, item)| {
            let on_select = on_select.clone();
            div()
                .id(("item", ix))
                .flex()
                .items_center()
                .gap_2()
                .h(px(26.))
                .px_2()
                .mx_1()
                .rounded_sm()
                .cursor_pointer()
                .whitespace_nowrap()
                .when(highlighted == Some(ix), |this| this.bg(theme::surface_hover()))
                .hover(|s| s.bg(theme::surface_hover()))
                .child(div().w(px(14.)).flex_none().text_color(theme::accent()).when(item.selected, |this| this.child("✓")))
                .child(item.label)
                .when_some(item.hint, |this, hint| this.child(div().text_color(theme::text_muted()).child(hint)))
                .on_click(move |_, window, cx| on_select(ix, window, cx))
        }))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropdownItem {
    pub label: SharedString,
    pub hint: Option<SharedString>,
}

impl DropdownItem {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self { label: label.into(), hint: None }
    }

    pub fn hint(mut self, hint: impl Into<SharedString>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropdownEvent {
    /// The user picked the item at this index (also emitted when re-picking the current one).
    Selected(usize),
}

pub struct Dropdown {
    focus_handle: FocusHandle,
    items: Vec<DropdownItem>,
    selected: usize,
    open: bool,
    highlighted: Option<usize>,
    /// Set when the popover closed because of a mouse-down outside it, so the mouse-up half of
    /// that same click on the trigger does not reopen it.
    suppress_click: bool,
}

impl EventEmitter<DropdownEvent> for Dropdown {}

impl Focusable for Dropdown {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Dropdown {
    pub fn new(cx: &mut Context<Self>, items: Vec<DropdownItem>, selected: usize, tab_index: isize) -> Self {
        let selected = selected.min(items.len().saturating_sub(1));
        Self {
            focus_handle: cx.focus_handle().tab_index(tab_index).tab_stop(true),
            items,
            selected,
            open: false,
            highlighted: None,
            suppress_click: false,
        }
    }

    /// Replaces the options (does not emit an event).
    pub fn set_items(&mut self, items: Vec<DropdownItem>, selected: usize, cx: &mut Context<Self>) {
        let selected = selected.min(items.len().saturating_sub(1));
        if items == self.items && selected == self.selected {
            return;
        }
        self.items = items;
        self.selected = selected;
        self.highlighted = None;
        cx.notify();
    }

    fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.open = open && !self.items.is_empty();
        self.highlighted = self.open.then_some(self.selected);
        cx.notify();
    }

    fn choose(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.items.len() {
            self.selected = index;
            cx.emit(DropdownEvent::Selected(index));
        }
        self.set_open(false, cx);
    }

    fn next(&mut self, _: &Next, _: &mut Window, cx: &mut Context<Self>) {
        if !self.open {
            self.set_open(true, cx);
            return;
        }
        let last = self.items.len().saturating_sub(1);
        self.highlighted = Some(self.highlighted.map_or(0, |i| (i + 1).min(last)));
        cx.notify();
    }

    fn prev(&mut self, _: &Prev, _: &mut Window, cx: &mut Context<Self>) {
        if !self.open {
            cx.propagate();
            return;
        }
        self.highlighted = Some(self.highlighted.map_or(0, |i| i.saturating_sub(1)));
        cx.notify();
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            match self.highlighted {
                Some(i) => self.choose(i, cx),
                None => self.set_open(false, cx),
            }
        } else {
            self.set_open(true, cx);
        }
    }

    fn close(&mut self, _: &Close, _: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.set_open(false, cx);
        } else {
            cx.propagate();
        }
    }

    fn on_trigger_click(&mut self, event: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        // Enter/Space are handled by the actions above; keyboard "clicks" would double-toggle.
        if matches!(event, ClickEvent::Keyboard(_)) {
            return;
        }
        if std::mem::take(&mut self.suppress_click) {
            return;
        }
        self.set_open(!self.open, cx);
    }
}

impl Render for Dropdown {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        if self.open && !focused {
            self.open = false;
        }
        let current = self.items.get(self.selected).map(|item| item.label.clone()).unwrap_or_else(|| "—".into());
        let entity = cx.entity();

        div()
            .id("dropdown")
            .relative()
            .w_full()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::next))
            .on_action(cx.listener(Self::prev))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::close))
            .child(
                div()
                    .id("trigger")
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .w_full()
                    .h(px(HEIGHT))
                    .px(px(8.))
                    .rounded_md()
                    .border_1()
                    .border_color(if focused { theme::accent() } else { theme::border() })
                    .bg(theme::background())
                    .hover(|s| s.bg(theme::surface()))
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(theme::text())
                    .whitespace_nowrap()
                    .child(div().overflow_hidden().text_ellipsis().child(current))
                    .child(div().flex_none().text_color(theme::text_muted()).text_size(px(10.)).child(if self.open { "▲" } else { "▼" }))
                    .on_click(cx.listener(Self::on_trigger_click))
                    .on_mouse_up_out(gpui::MouseButton::Left, cx.listener(|this, _, _, _| this.suppress_click = false)),
            )
            .when(self.open, |this| {
                let items = self
                    .items
                    .iter()
                    .enumerate()
                    .map(|(ix, item)| {
                        let row = MenuItem::new(item.label.clone()).selected(ix == self.selected);
                        match &item.hint {
                            Some(hint) => row.hint(hint.clone()),
                            None => row,
                        }
                    })
                    .collect();
                let on_select = {
                    let entity = entity.clone();
                    move |ix, _: &mut Window, cx: &mut App| entity.update(cx, |this, cx| this.choose(ix, cx))
                };
                this.child(
                    deferred(
                        anchored().offset(point(px(0.), px(HEIGHT + 4.))).snap_to_window_with_margin(px(8.)).child(
                            menu_panel("dropdown-menu", items, self.highlighted, on_select).on_mouse_down_out(cx.listener(
                                |this, _, _, cx| {
                                    this.suppress_click = true;
                                    this.set_open(false, cx);
                                },
                            )),
                        ),
                    )
                    .priority(1),
                )
            })
    }
}
