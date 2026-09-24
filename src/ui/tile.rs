//! One camera tile: video surface + status overlay.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cam_config::CameraId;
use cam_stream::{StreamShared, StreamStatus};
use gpui::{
    AnyElement, App, Context, EventEmitter, FontWeight, Hsla, IntoElement, MouseButton, MouseDownEvent, ObjectFit,
    ParentElement, Pixels, Point, Render, SharedString, Styled, Task, Window, div, prelude::*, px, surface,
};

use crate::state::AppState;
use crate::ui::theme;

/// Mouse gestures on a tile; handled by the mosaic.
pub enum TileEvent {
    DoubleClicked,
    ContextMenu(Point<Pixels>),
}

pub struct CameraTile {
    slot: usize,
    camera: Option<(CameraId, SharedString)>,
    stream: Option<Arc<StreamShared>>,
    show_stats: bool,
    /// Re-renders the tile whenever the stream publishes a frame or changes status.
    _watch: Option<Task<()>>,
    /// 1 Hz refresh of the "Reconnecting in Ns" countdown; only alive while reconnecting.
    countdown: Option<Task<()>>,
}

impl EventEmitter<TileEvent> for CameraTile {}

impl CameraTile {
    pub fn new(slot: usize) -> Self {
        Self { slot, camera: None, stream: None, show_stats: false, _watch: None, countdown: None }
    }

    pub fn camera_name(&self) -> Option<SharedString> {
        self.camera.as_ref().map(|(_, name)| name.clone())
    }

    pub fn status(&self) -> Option<StreamStatus> {
        self.stream.as_ref().map(|s| s.status())
    }

    /// Points the tile at another camera/stream. Keeps the current watcher when nothing changed.
    pub fn set_camera(
        &mut self,
        camera: Option<(CameraId, SharedString)>,
        stream: Option<Arc<StreamShared>>,
        cx: &mut Context<Self>,
    ) {
        let same_stream = match (&self.stream, &stream) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if self.camera == camera && same_stream {
            return;
        }
        self.camera = camera;
        if !same_stream {
            self.stream = stream;
            self.countdown = None;
            self._watch = self.stream.as_ref().map(|stream| {
                let changed = stream.changed();
                cx.spawn(async move |this, cx| {
                    while changed.recv().await.is_ok() {
                        if this.update(cx, |this, cx| this.stream_changed(cx)).is_err() {
                            break;
                        }
                    }
                })
            });
            self.stream_changed(cx);
        }
        cx.notify();
    }

    pub fn set_show_stats(&mut self, show: bool, cx: &mut Context<Self>) {
        if self.show_stats != show {
            self.show_stats = show;
            cx.notify();
        }
    }

    fn stream_changed(&mut self, cx: &mut Context<Self>) {
        let reconnecting = matches!(self.status(), Some(StreamStatus::Reconnecting { .. }));
        if reconnecting && self.countdown.is_none() {
            self.countdown = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        } else if !reconnecting {
            self.countdown = None;
        }
        cx.notify();
    }
}

fn status_color(status: &StreamStatus) -> Hsla {
    match status {
        StreamStatus::Live => theme::success().into(),
        StreamStatus::Connecting | StreamStatus::WaitingKeyframe | StreamStatus::Reconnecting { .. } => {
            theme::warning().into()
        }
        StreamStatus::Paused => theme::text_muted().into(),
        StreamStatus::AuthFailed | StreamStatus::Error(_) => theme::danger().into(),
    }
}

/// Centered message (and optional detail line) shown when the stream is not live.
fn status_message(status: &StreamStatus) -> (SharedString, Option<SharedString>) {
    match status {
        StreamStatus::Live => ("".into(), None),
        StreamStatus::Connecting => ("Connecting…".into(), None),
        StreamStatus::WaitingKeyframe => ("Waiting for video…".into(), None),
        StreamStatus::Reconnecting { attempt, retry_at, reason } => {
            let secs = retry_at.saturating_duration_since(Instant::now()).as_secs_f32().ceil() as u64;
            let title = if secs > 0 {
                format!("Reconnecting in {secs}s (attempt {attempt})")
            } else {
                format!("Reconnecting… (attempt {attempt})")
            };
            let reason = (!reason.is_empty()).then(|| SharedString::from(reason.clone()));
            (title.into(), reason)
        }
        StreamStatus::Paused => ("Paused".into(), None),
        StreamStatus::AuthFailed => ("Invalid username or password".into(), None),
        StreamStatus::Error(err) => ("Error".into(), Some(err.clone().into())),
    }
}

impl Render for CameraTile {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .id(("tile", self.slot))
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(gpui::black())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, event: &MouseDownEvent, _, cx| {
                    if event.click_count == 2 {
                        cx.emit(TileEvent::DoubleClicked);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|_, event: &MouseDownEvent, _, cx| {
                    cx.emit(TileEvent::ContextMenu(event.position));
                    cx.stop_propagation();
                }),
            );

        let Some((_, name)) = self.camera.clone() else {
            return root.child(centered(
                "No camera".into(),
                Some("Right-click to choose a camera".into()),
            ));
        };

        let (status, frame) = match &self.stream {
            Some(stream) => (stream.status(), stream.latest_frame()),
            None => (StreamStatus::Connecting, None),
        };
        let live = status == StreamStatus::Live;
        let has_frame = frame.is_some();

        root.when_some(frame, |this, frame| {
            this.child(surface(frame).object_fit(ObjectFit::Contain).size_full())
        })
        .when(!live, |this| {
            let (title, detail) = status_message(&status);
            this.when(has_frame, |this| this.child(div().absolute().inset_0().bg(theme::overlay_scrim())))
                .child(centered(title, detail))
        })
        .when(self.show_stats, |this| this.child(self.render_stats(cx)))
        .child(
            div()
                .absolute()
                .bottom_2()
                .left_2()
                .flex()
                .items_center()
                .gap_1p5()
                .px_2()
                .py_0p5()
                .rounded_full()
                .bg(theme::label_scrim())
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::text())
                .child(div().size_2().rounded_full().bg(status_color(&status)))
                .child(name),
        )
    }
}

impl CameraTile {
    fn render_stats(&self, cx: &App) -> AnyElement {
        let Some(stream) = &self.stream else {
            return div().into_any_element();
        };
        let stats = stream.stats();
        let smoothing = AppState::global(cx).read(cx).config().smooth_playback;
        let mut lines = vec![format!("{:.0} fps · {:.1} Mbps", stats.camera_fps, stats.kbps as f32 / 1000.0)];
        if stats.width > 0 {
            let codec = if stats.codec.is_empty() { String::new() } else { format!(" · {}", stats.codec) };
            lines.push(format!("{}×{}{codec}", stats.width, stats.height));
        }
        let smoothed = if smoothing { " · smoothed" } else { "" };
        lines.push(format!("network jitter ±{:.0} ms{smoothed}", stats.jitter_ms));
        let reconnects = if stats.reconnects == 1 { "reconnect" } else { "reconnects" };
        lines.push(format!("{} {reconnects} · {} dropped", stats.reconnects, stats.dropped_frames));
        if let Some(err) = stats.last_error {
            lines.push(format!("Last error: {err}"));
        }
        div()
            .absolute()
            .top_2()
            .right_2()
            .max_w(px(320.))
            .flex()
            .flex_col()
            .items_end()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(theme::label_scrim())
            .text_xs()
            .text_color(theme::text())
            .children(lines.into_iter().map(|line| div().child(line)))
            .into_any_element()
    }
}

fn centered(title: SharedString, detail: Option<SharedString>) -> impl IntoElement {
    div()
        .absolute()
        .inset_0()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_1()
        .px_4()
        .child(div().text_sm().font_weight(FontWeight::MEDIUM).text_color(theme::text()).child(title))
        .when_some(detail, |this, detail| {
            this.child(div().text_xs().text_color(theme::text_muted()).text_center().child(detail))
        })
}
