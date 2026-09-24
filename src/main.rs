mod actions;
mod main_window;
mod state;
mod ui;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use cam_config::Config;
use cam_platform::PowerCallbacks;
use cam_stream::StreamManager;
use futures::StreamExt as _;
use gpui::{App, Global};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt as _, util::SubscriberInitExt as _};

use crate::state::AppState;

/// Platform handles that must live as long as the app.
struct Lifetime {
    _app_nap: cam_platform::AppNapGuard,
    _power: Option<cam_platform::PowerObserver>,
}
impl Global for Lifetime {}

enum PowerEvent {
    Sleep,
    Wake,
}

fn main() {
    init_tracing();
    install_panic_hook();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");

    // `CAMFAST_CONFIG` points at an alternative file (handy for testing).
    let config_path = std::env::var_os("CAMFAST_CONFIG").map_or_else(Config::default_path, Into::into);
    let (config, notice) = load_config(&config_path);
    let streams = match StreamManager::new() {
        Ok(streams) => streams,
        Err(err) => {
            tracing::error!(%err, "failed to start stream manager");
            std::process::exit(1);
        }
    };

    let app = gpui_platform::application();
    app.on_reopen(main_window::activate);
    app.run(move |cx: &mut App| {
        ui::components::init(cx);
        let state = AppState::init(config, config_path, streams, cx);
        if let Some(notice) = notice {
            state.update(cx, |state, cx| state.set_notice(notice, cx));
        }
        actions::init(cx);

        let main = match main_window::open(cx) {
            Ok(handle) => handle,
            Err(err) => {
                tracing::error!(%err, "failed to open main window");
                cx.quit();
                return;
            }
        };
        let main_id = main.window_id();
        cx.on_window_closed(move |cx, id| {
            if id == main_id {
                cx.quit();
            }
        })
        .detach();
        cx.on_app_quit(|cx| {
            main_window::flush_geometry(cx);
            tracing::info!("quitting");
            AppState::global(cx).update(cx, |state, _| state.streams.shutdown());
            async {}
        })
        .detach();
        cx.activate(true);

        let power = observe_power(cx);
        cx.set_global(Lifetime {
            _app_nap: cam_platform::prevent_app_nap("Live camera view"),
            _power: power,
        });

        if state.read(cx).config().cameras.is_empty() {
            ui::settings::open(cx);
        }
    });
}

/// Forwards sleep/wake to the stream manager. The platform callbacks only enqueue, so they never
/// re-enter GPUI while it is borrowed.
fn observe_power(cx: &mut App) -> Option<cam_platform::PowerObserver> {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let sleep_tx = tx.clone();
    let observer = cam_platform::observe_power(PowerCallbacks {
        will_sleep: Box::new(move || {
            let _ = sleep_tx.unbounded_send(PowerEvent::Sleep);
        }),
        did_wake: Box::new(move || {
            let _ = tx.unbounded_send(PowerEvent::Wake);
        }),
    });
    let observer = match observer {
        Ok(observer) => observer,
        Err(err) => {
            tracing::error!(%err, "failed to observe sleep/wake");
            return None;
        }
    };
    cx.spawn(async move |cx| {
        while let Some(event) = rx.next().await {
            cx.update(|cx| {
                let state = AppState::global(cx);
                let streams = &state.read(cx).streams;
                match event {
                    PowerEvent::Sleep => {
                        tracing::info!("system will sleep; suspending streams");
                        streams.suspend();
                    }
                    PowerEvent::Wake => {
                        tracing::info!("system woke; resuming streams");
                        streams.resume();
                    }
                }
            });
        }
    })
    .detach();
    Some(observer)
}

/// Loads the config. A broken file is moved aside (never overwritten) and the app starts fresh;
/// the returned notice tells the user where the old file went.
fn load_config(path: &Path) -> (Config, Option<String>) {
    let err = match Config::load(path) {
        Ok(config) => return (config, None),
        Err(err) => err,
    };
    tracing::error!(%err, path = %path.display(), "failed to load config");
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
    let backup = path.with_file_name(format!("config.toml.broken-{stamp}"));
    // TOML diagnostics span several lines (source excerpt); the banner only needs the first.
    let full = err.to_string();
    let err = full.lines().next().unwrap_or_default();
    let notice = match std::fs::rename(path, &backup) {
        Ok(()) => {
            tracing::warn!(backup = %backup.display(), "moved broken config aside; starting with defaults");
            format!("{err}. The file was kept as {} and a new configuration was created.", backup.display())
        }
        Err(rename_err) => {
            tracing::error!(%rename_err, "failed to move broken config aside");
            format!("{err}. Using the default configuration; changes may not be saved.")
        }
    };
    (Config::default(), Some(notice))
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,retina=warn"));
    let stderr = fmt::layer().with_writer(std::io::stderr);
    let log_dir = cam_config::log_dir();
    let _ = std::fs::create_dir_all(&log_dir);
    let file = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("camfast")
        .filename_suffix("log")
        .max_log_files(14)
        .build(&log_dir)
        .map_err(|err| eprintln!("file logging disabled: {err}"))
        .ok()
        .map(|appender| fmt::layer().with_ansi(false).with_writer(appender));
    tracing_subscriber::registry().with(filter).with(stderr).with(file).init();
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(%info, %backtrace, "panic");
        default_hook(info);
    }));
}
