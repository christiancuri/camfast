//! 2x2 mosaic window content.

use std::time::Duration;

use cam_config::{CameraId, SLOT_COUNT};
use cam_stream::StreamStatus;
use gpui::{
    AnyElement, App, ClickEvent, Context, ElementId, Entity, FocusHandle, FontWeight, IntoElement, ParentElement,
    Pixels, Point, Render, SharedString, Styled, Subscription, Task, Window, actions, anchored, deferred, div,
    prelude::*, px,
};

use crate::actions::CloseWindow;
use crate::main_window;
use crate::state::AppState;
use crate::ui::theme;
use crate::ui::tile::{CameraTile, TileEvent};

actions!(mosaic, [ExitFocus]);

/// Height of the (transparent) macOS titlebar area drawn by the app.
pub const TITLEBAR_HEIGHT: Pixels = px(28.);
const GAP: Pixels = px(2.);

/// Human-readable position of a mosaic slot.
pub fn slot_label(slot: usize) -> &'static str {
    match slot {
        0 => "Top left",
        1 => "Top right",
        2 => "Bottom left",
        _ => "Bottom right",
    }
}

struct MenuState {
    slot: usize,
    position: Point<Pixels>,
}

pub struct Mosaic {
    tiles: Vec<Entity<CameraTile>>,
    /// Tile expanded to the whole window (double-click), if any.
    focused: Option<usize>,
    menu: Option<MenuState>,
    show_stats: bool,
    title: SharedString,
    focus_handle: FocusHandle,
    save_geometry: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Mosaic {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tiles: Vec<_> = (0..SLOT_COUNT).map(|slot| cx.new(|_| CameraTile::new(slot))).collect();
        let state = AppState::global(cx);

        let mut subscriptions = vec![
            cx.observe(&state, |this, _, cx| this.sync_tiles(cx)),
            cx.observe_window_bounds(window, |this, window, cx| {
                // Debounced: moving/resizing fires this on every frame.
                let bounds = window.window_bounds().get_bounds();
                this.save_geometry = Some(cx.spawn(async move |_, cx| {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    cx.update(|cx| main_window::save_geometry(bounds, cx));
                }));
            }),
            // Covers minimize, full occlusion, other Space and display sleep.
            window.observe_window_visibility(|visibility, _, cx| {
                let visible = visibility.is_visible();
                tracing::debug!(visible, "main window visibility changed");
                AppState::global(cx).read(cx).streams.set_window_visible(visible);
            }),
        ];
        for (slot, tile) in tiles.iter().enumerate() {
            subscriptions.push(cx.subscribe_in(tile, window, move |this, _, event, window, cx| match event {
                TileEvent::DoubleClicked => {
                    let next = if this.focused == Some(slot) { None } else { Some(slot) };
                    this.set_focused(next, window, cx);
                }
                TileEvent::ContextMenu(position) => {
                    this.menu = Some(MenuState { slot, position: *position });
                    cx.notify();
                }
            }));
        }

        window.on_window_should_close(cx, |window, cx| {
            main_window::save_geometry(window.window_bounds().get_bounds(), cx);
            true
        });

        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        let mut this = Self {
            tiles,
            focused: None,
            menu: None,
            show_stats: false,
            title: "CamFast".into(),
            focus_handle,
            save_geometry: None,
            _subscriptions: subscriptions,
        };
        this.sync_tiles(cx);
        this
    }

    pub fn show_stats(&self) -> bool {
        self.show_stats
    }

    pub fn toggle_stats(&mut self, cx: &mut Context<Self>) {
        self.show_stats = !self.show_stats;
        for tile in &self.tiles {
            tile.update(cx, |tile, cx| tile.set_show_stats(self.show_stats, cx));
        }
    }

    /// Points every tile at the camera/stream currently assigned to its slot.
    fn sync_tiles(&mut self, cx: &mut Context<Self>) {
        let state = AppState::global(cx);
        let assignments: Vec<_> = {
            let state = state.read(cx);
            let config = state.config();
            config
                .slots
                .iter()
                .map(|id| {
                    let camera = id.and_then(|id| config.camera(id));
                    let stream = camera.and_then(|camera| state.stream(camera.id));
                    (camera.map(|c| (c.id, SharedString::from(c.name.clone()))), stream)
                })
                .collect()
        };
        for (tile, (camera, stream)) in self.tiles.iter().zip(assignments) {
            tile.update(cx, |tile, cx| tile.set_camera(camera, stream, cx));
        }
        cx.notify();
    }

    fn set_focused(&mut self, focused: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.focused = focused;
        self.menu = None;
        self.title = focused
            .and_then(|slot| self.tiles[slot].read(cx).camera_name())
            .map_or_else(|| "CamFast".into(), |name| format!("CamFast — {name}").into());
        window.set_window_title(&self.title);
        cx.notify();
    }

    fn exit_focus(&mut self, _: &ExitFocus, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        } else if self.focused.is_some() {
            self.set_focused(None, window, cx);
        }
    }

    fn assign(&mut self, slot: usize, camera: Option<CameraId>, cx: &mut Context<Self>) {
        AppState::global(cx).update(cx, |state, cx| {
            if let Err(err) = state.update_config(cx, |config| {
                config.assign(slot, camera);
                Ok(())
            }) {
                tracing::error!(%err, slot, "failed to assign camera");
                state.set_notice(format!("Could not save the configuration: {err}"), cx);
            }
        });
    }

    fn render_grid(&self) -> AnyElement {
        if let Some(slot) = self.focused {
            return div().flex_1().min_h_0().child(self.tiles[slot].clone()).into_any_element();
        }
        let cell = |slot: usize| div().flex_1().min_w_0().h_full().child(self.tiles[slot].clone());
        let row = |a: usize, b: usize| div().flex().flex_1().min_h_0().gap(GAP).child(cell(a)).child(cell(b));
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap(GAP)
            .bg(theme::tile_gap())
            .child(row(0, 1))
            .child(row(2, 3))
            .into_any_element()
    }

    fn render_notice(&self, notice: SharedString, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_1p5()
            .bg(theme::danger().opacity(0.18))
            .border_b_1()
            .border_color(theme::danger().opacity(0.5))
            .text_sm()
            .text_color(theme::text())
            .child(div().flex_1().child(notice))
            .child(
                div()
                    .id("dismiss-notice")
                    .px_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(theme::text_muted())
                    .hover(|s| s.bg(theme::surface_hover()).text_color(theme::text()))
                    .child("Dismiss")
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                        AppState::global(cx).update(cx, |state, cx| state.dismiss_notice(cx));
                    })),
            )
    }

    fn render_menu(&self, menu: &MenuState, cx: &mut Context<Self>) -> impl IntoElement {
        let slot = menu.slot;
        let (current, cameras) = {
            let state = AppState::global(cx);
            let config = state.read(cx).config();
            let mut cameras: Vec<_> = config
                .cameras
                .iter()
                .map(|camera| {
                    let detail = config
                        .slot_of(camera.id)
                        .filter(|other| *other != slot)
                        .map(|other| SharedString::from(format!("(in {})", slot_label(other).to_lowercase())));
                    (camera.id, SharedString::from(camera.name.clone()), detail)
                })
                .collect();
            cameras.sort_by_cached_key(|(_, name, _)| name.trim().to_lowercase());
            (config.slots[slot], cameras)
        };

        let mut items: Vec<AnyElement> = Vec::with_capacity(cameras.len() + 6);
        items.push(menu_item("none", "No camera", current.is_none(), None, cx, move |this, _, cx| {
            this.assign(slot, None, cx)
        }));
        items.push(separator());
        if cameras.is_empty() {
            items.push(
                // Indented like item labels (px_3 + check column w_3 + gap_2).
                div().pl_8().pr_3().py_1().text_color(theme::text_muted()).child("No cameras added yet").into_any_element(),
            );
        }
        for (id, name, detail) in cameras {
            items.push(menu_item(
                ElementId::Name(format!("camera-{}", id.0).into()),
                name,
                current == Some(id),
                detail,
                cx,
                move |this, _, cx| this.assign(slot, Some(id), cx),
            ));
        }
        items.push(separator());
        let retryable = matches!(
            self.tiles[slot].read(cx).status(),
            Some(StreamStatus::Reconnecting { .. } | StreamStatus::AuthFailed | StreamStatus::Error(_))
        );
        if let (true, Some(id)) = (retryable, current) {
            items.push(menu_item("retry", "Reconnect Now", false, None, cx, move |_, _, cx| {
                AppState::global(cx).read(cx).streams.retry_now(id);
            }));
        }
        items.push(menu_item("settings", "Configure Cameras…", false, None, cx, |_, _, cx| {
            crate::ui::settings::open(cx)
        }));

        deferred(
            anchored().position(menu.position).snap_to_window_with_margin(px(8.)).child(
                div()
                    .id("tile-menu")
                    .occlude()
                    .min_w(px(240.))
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(theme::border())
                    .bg(theme::surface())
                    .shadow_lg()
                    .text_sm()
                    .text_color(theme::text())
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .children(items),
            ),
        )
        .with_priority(1)
    }
}

fn separator() -> AnyElement {
    div().my_1().h(px(1.)).bg(theme::border()).into_any_element()
}

fn menu_item(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    checked: bool,
    detail: Option<SharedString>,
    cx: &mut Context<Mosaic>,
    on_select: impl Fn(&mut Mosaic, &mut Window, &mut Context<Mosaic>) + 'static,
) -> AnyElement {
    div()
        .id(id.into())
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .cursor_pointer()
        .hover(|s| s.bg(theme::surface_hover()))
        .child(div().w_3().text_color(theme::accent()).child(if checked { "✓" } else { "" }))
        .child(label.into())
        .when_some(detail, |this, detail| this.child(div().text_color(theme::text_muted()).child(detail)))
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.menu = None;
            on_select(this, window, cx);
            cx.notify();
        }))
        .into_any_element()
}

impl Render for Mosaic {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let notice = AppState::global(cx).read(cx).notice().cloned();
        div()
            .key_context("Mosaic")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::exit_focus))
            .on_action(|_: &CloseWindow, window, cx| {
                main_window::save_geometry(window.window_bounds().get_bounds(), cx);
                window.remove_window();
            })
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::black())
            .child(
                // Transparent titlebar area: AppKit drags the window from here.
                div()
                    .h(TITLEBAR_HEIGHT)
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::background())
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::text_muted())
                    .child(self.title.clone()),
            )
            .when_some(notice, |this, notice| this.child(self.render_notice(notice, cx)))
            .child(self.render_grid())
            .when_some(self.menu.as_ref(), |this, menu| this.child(self.render_menu(menu, cx)))
    }
}

/// Whether the given app has the mosaic window's stats overlay on (for the menu checkmark).
pub fn stats_visible(cx: &App) -> bool {
    main_window::handle(cx).and_then(|handle| handle.read(cx).ok()).is_some_and(Mosaic::show_stats)
}
