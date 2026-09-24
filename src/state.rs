//! Application-wide state: the persisted config plus the running streams.

use std::path::PathBuf;
use std::sync::Arc;

use cam_config::{CameraId, Config, ConfigError};
use cam_stream::{StreamManager, StreamShared};
use gpui::{App, AppContext, Context, Entity, Global, SharedString};

pub struct AppState {
    config: Config,
    config_path: PathBuf,
    pub streams: StreamManager,
    /// Problem worth showing to the user as a banner in the mosaic (e.g. broken config file).
    notice: Option<SharedString>,
}

struct GlobalAppState(Entity<AppState>);
impl Global for GlobalAppState {}

impl AppState {
    pub fn init(config: Config, config_path: PathBuf, streams: StreamManager, cx: &mut App) -> Entity<AppState> {
        streams.set_smooth_playback(config.smooth_playback);
        let state = cx.new(|_| AppState { config, config_path, streams, notice: None });
        state.update(cx, |state, _| state.sync_streams());
        cx.set_global(GlobalAppState(state.clone()));
        state
    }

    /// The single `AppState` entity. Observe it (`cx.observe`) to react to config changes.
    pub fn global(cx: &App) -> Entity<AppState> {
        cx.global::<GlobalAppState>().0.clone()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn stream(&self, id: CameraId) -> Option<Arc<StreamShared>> {
        self.streams.stream(id)
    }

    pub fn notice(&self) -> Option<&SharedString> {
        self.notice.as_ref()
    }

    pub fn set_notice(&mut self, notice: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.notice = Some(notice.into());
        cx.notify();
    }

    pub fn dismiss_notice(&mut self, cx: &mut Context<Self>) {
        if self.notice.take().is_some() {
            cx.notify();
        }
    }

    /// Applies `f` to a copy of the config, validates and saves it, then restarts/stops/starts
    /// streams as needed and notifies observers. On error nothing changes.
    pub fn update_config(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Config) -> Result<(), ConfigError>,
    ) -> Result<(), ConfigError> {
        let mut next = self.config.clone();
        f(&mut next)?;
        if next == self.config {
            return Ok(());
        }
        next.save(&self.config_path)?;
        let streams_changed = next.slots != self.config.slots || next.cameras != self.config.cameras;
        if next.smooth_playback != self.config.smooth_playback {
            self.streams.set_smooth_playback(next.smooth_playback);
        }
        self.config = next;
        if streams_changed {
            self.sync_streams();
        }
        cx.notify();
        Ok(())
    }

    /// Like `update_config` but for UI-only fields (e.g. window geometry) where a failed save
    /// should just be logged.
    pub fn update_config_quietly(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Config)) {
        if let Err(err) = self.update_config(cx, |config| {
            f(config);
            Ok(())
        }) {
            tracing::error!(%err, "failed to save config");
        }
    }

    fn sync_streams(&mut self) {
        let wanted: Vec<_> = self.config.assigned_cameras().into_iter().cloned().collect();
        self.streams.sync(&wanted);
    }
}
