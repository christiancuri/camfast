//! The main (mosaic) window: opening, geometry persistence and lookup.

use cam_config::WindowGeometry;
use gpui::{
    App, AppContext, Bounds, Global, Pixels, TitlebarOptions, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowOptions, point, px, size,
};

use crate::state::AppState;
use crate::ui::mosaic::{Mosaic, TITLEBAR_HEIGHT};

const DEFAULT_SIZE: (f32, f32) = (1280., 760.);
const MIN_SIZE: (f32, f32) = (640., 400.);

struct MainWindow(WindowHandle<Mosaic>);
impl Global for MainWindow {}

pub fn handle(cx: &App) -> Option<WindowHandle<Mosaic>> {
    cx.try_global::<MainWindow>().map(|main| main.0)
}

pub fn open(cx: &mut App) -> anyhow::Result<WindowHandle<Mosaic>> {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(initial_bounds(cx))),
        titlebar: Some(TitlebarOptions {
            title: Some("CamFast".into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        window_min_size: Some(size(px(MIN_SIZE.0), px(MIN_SIZE.1))),
        window_background: WindowBackgroundAppearance::Opaque,
        app_id: Some("camfast".into()),
        ..Default::default()
    };
    let handle = cx.open_window(options, |window, cx| cx.new(|cx| Mosaic::new(window, cx)))?;
    cx.set_global(MainWindow(handle));
    Ok(handle)
}

/// Brings the main window to the front (dock click, "CamFast" relaunch).
pub fn activate(cx: &mut App) {
    cx.activate(true);
    match handle(cx) {
        Some(handle) => {
            handle.update(cx, |_, window, _| window.activate_window()).ok();
        }
        None => {
            if let Err(err) = open(cx) {
                tracing::error!(%err, "failed to reopen main window");
            }
        }
    }
}

/// Persists the window frame (restore bounds when maximized/fullscreen).
pub fn save_geometry(bounds: Bounds<Pixels>, cx: &mut App) {
    let geometry = WindowGeometry {
        x: bounds.origin.x.as_f32().round(),
        y: bounds.origin.y.as_f32().round(),
        width: bounds.size.width.as_f32().round(),
        height: bounds.size.height.as_f32().round(),
    };
    if geometry.width < 1. || geometry.height < 1. {
        return;
    }
    AppState::global(cx).update(cx, |state, cx| {
        state.update_config_quietly(cx, |config| config.window = Some(geometry))
    });
}

/// Saves the main window geometry right away (e.g. on quit), if the window is still open.
pub fn flush_geometry(cx: &mut App) {
    let Some(handle) = handle(cx) else { return };
    if let Ok(bounds) = handle.update(cx, |_, window, _| window.window_bounds().get_bounds()) {
        save_geometry(bounds, cx);
    }
}

/// Saved geometry if it is sane and its titlebar lands on the primary display, else a
/// centered default. GPUI window coordinates are relative to the display the window is on and
/// the config has no display id, so restoring always targets the primary display.
fn initial_bounds(cx: &App) -> Bounds<Pixels> {
    let default = || Bounds::centered(None, size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)), cx);
    let Some(g) = AppState::global(cx).read(cx).config().window else {
        return default();
    };
    let Some(display) = cx.primary_display() else {
        return default();
    };
    if ![g.x, g.y, g.width, g.height].iter().all(|v| v.is_finite()) {
        return default();
    }
    let area = display.visible_bounds();
    // Not `clamp`: it panics when the visible area is smaller than the minimum size.
    let width = g.width.min(area.size.width.as_f32()).max(MIN_SIZE.0);
    let height = g.height.min(area.size.height.as_f32()).max(MIN_SIZE.1);
    let bounds = Bounds::new(point(px(g.x), px(g.y)), size(px(width), px(height)));

    // Require a grabbable piece of the titlebar to be visible.
    let titlebar = Bounds::new(bounds.origin, size(bounds.size.width, TITLEBAR_HEIGHT));
    let visible = titlebar.intersect(&display.bounds());
    if visible.size.width < px(120.) || visible.size.height < TITLEBAR_HEIGHT / 2. {
        tracing::info!(?g, "saved window position is off-screen; centering");
        return Bounds::centered(None, bounds.size, cx);
    }
    bounds
}
