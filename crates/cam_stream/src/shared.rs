use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use cam_config::CameraId;
use core_video::pixel_buffer::CVPixelBuffer;
use parking_lot::Mutex;

/// Connection state of one camera, as shown in its mosaic tile.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamStatus {
    /// RTSP handshake in progress (or waiting for the first keyframe of a new session).
    Connecting,
    /// Frames are being decoded and displayed.
    Live,
    /// Connected but the picture is frozen until the next keyframe (packet loss / decoder error).
    WaitingKeyframe,
    /// Waiting before the next reconnection attempt.
    Reconnecting { attempt: u32, retry_at: Instant, reason: String },
    /// Decoding paused because the window is hidden (RTSP may still be connected).
    Paused,
    /// The camera rejected the credentials. Not retried until the camera config changes.
    AuthFailed,
    /// Non-retryable problem (e.g. unsupported codec). Not retried until the config changes.
    Error(String),
}

/// Point-in-time metrics for the optional stats overlay.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatsSnapshot {
    /// Frames published (decoded and shown) during the last second.
    pub fps: f32,
    /// Rate the camera is sending at, from RTP timestamps over ~2 s (unaffected by jitter).
    pub camera_fps: f32,
    /// RFC 3550 interarrival jitter of the video frames, in milliseconds.
    pub jitter_ms: f32,
    /// Received video bitrate, averaged over ~5 s.
    pub kbps: u32,
    pub width: u32,
    pub height: u32,
    pub codec: String,
    pub reconnects: u32,
    pub dropped_frames: u64,
    pub last_error: Option<String>,
}

/// State shared between a camera's pipeline threads and its UI tile.
///
/// The same `Arc<StreamShared>` lives as long as the camera stays in the mosaic, across
/// reconnections and config-driven restarts, so UI handles never go stale.
pub struct StreamShared {
    id: CameraId,
    frame: Mutex<Option<SendPixelBuffer>>,
    status: Mutex<StreamStatus>,
    stats: Mutex<StatsSnapshot>,
    frames_published: AtomicU64,
    fps_x100: AtomicU32,
    changed_tx: async_channel::Sender<()>,
    changed_rx: async_channel::Receiver<()>,
    publish_observer: OnceLock<Box<PublishObserver>>,
}

type PublishObserver = dyn Fn(Instant) + Send + Sync;

/// CVPixelBuffer is a CoreFoundation object with an atomic refcount; moving it between
/// threads is safe. `core-video` just doesn't declare it.
struct SendPixelBuffer(CVPixelBuffer);
unsafe impl Send for SendPixelBuffer {}

impl StreamShared {
    pub fn new(id: CameraId) -> Self {
        let (changed_tx, changed_rx) = async_channel::bounded(1);
        Self {
            id,
            frame: Mutex::new(None),
            status: Mutex::new(StreamStatus::Connecting),
            stats: Mutex::new(StatsSnapshot::default()),
            frames_published: AtomicU64::new(0),
            fps_x100: AtomicU32::new(0),
            changed_tx,
            changed_rx,
            publish_observer: OnceLock::new(),
        }
    }

    pub fn id(&self) -> CameraId {
        self.id
    }

    /// Latest decoded frame (IOSurface-backed NV12 full range, ready for `gpui::surface`).
    pub fn latest_frame(&self) -> Option<CVPixelBuffer> {
        self.frame.lock().as_ref().map(|f| f.0.clone())
    }

    pub fn status(&self) -> StreamStatus {
        self.status.lock().clone()
    }

    pub fn stats(&self) -> StatsSnapshot {
        let mut stats = self.stats.lock().clone();
        stats.fps = self.fps_x100.load(Ordering::Relaxed) as f32 / 100.0;
        stats
    }

    /// Total frames published since creation (monotonic; handy for tests and fps).
    pub fn frames_published(&self) -> u64 {
        self.frames_published.load(Ordering::Relaxed)
    }

    /// Coalesced change signal: at most one pending notification, so a slow UI never
    /// accumulates work. Intended for a single consumer (the camera's tile).
    pub fn changed(&self) -> async_channel::Receiver<()> {
        self.changed_rx.clone()
    }

    fn notify(&self) {
        let _ = self.changed_tx.try_send(());
    }

    /// Replaces the latest frame ("latest wins"); the previous buffer returns to the VT pool.
    pub fn publish_frame(&self, buffer: CVPixelBuffer) {
        *self.frame.lock() = Some(SendPixelBuffer(buffer));
        self.frames_published.fetch_add(1, Ordering::Relaxed);
        self.notify();
    }

    /// Drops the retained frame (e.g. while paused) so the decoder's buffers can be freed.
    pub fn clear_frame(&self) {
        if self.frame.lock().take().is_some() {
            self.notify();
        }
    }

    pub fn set_status(&self, status: StreamStatus) {
        let mut current = self.status.lock();
        if *current != status {
            *current = status;
            drop(current);
            self.notify();
        }
    }

    /// Sets `status` only if the current status satisfies `from` (lets the decoder report
    /// `Live`/`WaitingKeyframe` without overwriting a supervisor-owned state like `Paused`).
    pub(crate) fn transition(&self, from: impl FnOnce(&StreamStatus) -> bool, status: StreamStatus) -> bool {
        let mut current = self.status.lock();
        if *current == status || !from(&current) {
            return false;
        }
        *current = status;
        drop(current);
        self.notify();
        true
    }

    pub fn set_fps(&self, fps: f32) {
        self.fps_x100.store((fps * 100.0).round() as u32, Ordering::Relaxed);
    }

    pub fn update_stats(&self, f: impl FnOnce(&mut StatsSnapshot)) {
        f(&mut self.stats.lock());
    }

    /// Diagnostics: `f` runs on the decode thread right after each published frame, with the
    /// frame's network arrival time. Can be set once; returns false if already set.
    pub fn set_publish_observer(&self, f: impl Fn(Instant) + Send + Sync + 'static) -> bool {
        self.publish_observer.set(Box::new(f)).is_ok()
    }

    pub(crate) fn observe_publish(&self, arrival: Instant) {
        if let Some(f) = self.publish_observer.get() {
            f(arrival);
        }
    }
}
