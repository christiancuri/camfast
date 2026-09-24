//! App-level actions, key bindings and the menu bar.

use gpui::{App, KeyBinding, Menu, MenuItem, actions};

use crate::main_window;
use crate::ui::mosaic::{self, ExitFocus};

actions!(camfast, [OpenSettings, ToggleStats, HideApp, Quit, Minimize, CloseWindow]);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-i", ToggleStats, None),
        KeyBinding::new("cmd-h", HideApp, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("escape", ExitFocus, Some("Mosaic")),
    ]);

    // Handlers that touch a window are deferred: actions are dispatched while GPUI has the
    // active window checked out, so updating it synchronously would fail.
    // Deferred too: with the settings window focused, updating it synchronously fails and
    // `open` would create a second settings window.
    cx.on_action(|_: &OpenSettings, cx| cx.defer(crate::ui::settings::open));
    cx.on_action(|_: &ToggleStats, cx| {
        cx.defer(|cx| {
            if let Some(handle) = main_window::handle(cx) {
                handle.update(cx, |mosaic, _, cx| mosaic.toggle_stats(cx)).ok();
            }
            set_menus(cx);
        })
    });
    cx.on_action(|_: &HideApp, cx| cx.hide());
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Minimize, cx| {
        cx.defer(|cx| {
            if let Some(window) = cx.active_window() {
                window.update(cx, |_, window, _| window.minimize_window()).ok();
            }
        })
    });
    // Fallback for windows that don't handle it themselves (the mosaic does, then the app quits).
    cx.on_action(|_: &CloseWindow, cx| {
        cx.defer(|cx| {
            if let Some(window) = cx.active_window() {
                window.update(cx, |_, window, _| window.remove_window()).ok();
            }
        })
    });

    set_menus(cx);
}

pub fn set_menus(cx: &mut App) {
    let stats = mosaic::stats_visible(cx);
    cx.set_menus([
        Menu::new("CamFast").items([
            MenuItem::action("Cameras…", OpenSettings),
            MenuItem::action("Show Statistics", ToggleStats).checked(stats),
            MenuItem::separator(),
            MenuItem::action("Hide CamFast", HideApp),
            MenuItem::action("Quit CamFast", Quit),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Close", CloseWindow),
        ]),
    ]);
}
