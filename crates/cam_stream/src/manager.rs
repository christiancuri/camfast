use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cam_config::{CameraConfig, CameraId};
use futures::StreamExt;
use retina::client::SessionGroup;
use retina::codec::CodecItem;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout, timeout_at};

use crate::decoder::{AccessUnit, DecoderEvent, DecoderHandle, QUEUE_CAPACITY, WARMUP_QUEUE_CAPACITY};
use crate::policy::{Activity, Backoff, Failure, FailureKind, STABLE_AFTER, VisibilityTracker};
use crate::nal::{HevcPicture, hevc_picture};
use crate::rtsp::{self, CONNECT_TIMEOUT, Codec, CodecParams, FRAME_TIMEOUT, TEARDOWN_WAIT};
use crate::timing::RxStats;
use crate::{StreamShared, StreamStatus};

/// How long to wait for the previous session of a camera to be torn down before reconnecting.
const TEARDOWN_BEFORE_CONNECT: Duration = Duration::from_secs(5);
/// Larger decoder queue allowed right after decoding (re)starts on a keyframe.
const WARMUP: Duration = Duration::from_secs(3);
/// Upper bound for `Drop` to let supervisors send their TEARDOWNs.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Result of a one-shot connection test from the settings window.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeInfo {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: Option<f32>,
}

/// App-wide state every supervisor follows.
#[derive(Clone, Copy, Debug)]
struct Global {
    activity: Activity,
    /// System sleep: everything disconnected.
    suspended: bool,
    /// Bumped on system wake: supervisors reset their backoff and reconnect now.
    wake_epoch: u64,
}

impl Global {
    fn wants_connection(&self) -> bool {
        !self.suspended && self.activity != Activity::Disconnect
    }
}

/// Per-camera control.
#[derive(Clone, Copy, Debug, Default)]
struct Control {
    stop: bool,
    /// Bumped by `retry_now` (skip backoff / re-arm AuthFailed and Error).
    retry_epoch: u64,
}

struct Camera {
    config: CameraConfig,
    shared: Arc<StreamShared>,
    group: Arc<SessionGroup>,
    /// App-wide "Smooth playback" flag, read live by the decode thread.
    smooth: Arc<AtomicBool>,
}

struct Entry {
    config: CameraConfig,
    shared: Arc<StreamShared>,
    group: Arc<SessionGroup>,
    control: watch::Sender<Control>,
    task: JoinHandle<()>,
}

/// Owns the tokio runtime and one supervisor per camera placed in the mosaic.
///
/// All methods are cheap and non-blocking; they may be called from the GPUI main thread.
/// RTSP runs on a private 2-worker tokio runtime; each camera decodes on its own thread.
pub struct StreamManager {
    runtime: Option<Runtime>,
    streams: HashMap<CameraId, Entry>,
    /// Supervisors of cameras removed from the mosaic that may still be tearing down. If the
    /// camera comes back before they finish, the new supervisor waits for them (one session
    /// per camera) and reuses their session group.
    retired: HashMap<CameraId, (Arc<SessionGroup>, JoinHandle<()>)>,
    global: watch::Sender<Global>,
    visible: watch::Sender<bool>,
    smooth: Arc<AtomicBool>,
}

impl StreamManager {
    pub fn new() -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("cam-rtsp")
            .enable_all()
            .build()?;
        let (global, _) = watch::channel(Global { activity: Activity::Decode, suspended: false, wake_epoch: 0 });
        let (visible, visible_rx) = watch::channel(true);
        runtime.spawn(visibility_controller(visible_rx, global.clone()));
        Ok(Self {
            runtime: Some(runtime),
            streams: HashMap::new(),
            retired: HashMap::new(),
            global,
            visible,
            smooth: Arc::new(AtomicBool::new(false)),
        })
    }

    fn runtime(&self) -> &Runtime {
        self.runtime.as_ref().expect("runtime lives until drop")
    }

    /// Makes the running set match `cameras` (the cameras currently in the mosaic): starts
    /// new ones, stops removed ones and restarts ones whose connection settings changed (which
    /// also re-arms a camera in `AuthFailed`/`Error`).
    pub fn sync(&mut self, cameras: &[CameraConfig]) {
        self.retired.retain(|_, (_, task)| !task.is_finished());
        let removed: Vec<CameraId> =
            self.streams.keys().filter(|id| !cameras.iter().any(|c| c.id == **id)).copied().collect();
        for id in removed {
            if let Some(entry) = self.streams.remove(&id) {
                entry.control.send_modify(|c| c.stop = true);
                entry.shared.clear_frame();
                entry.shared.set_status(StreamStatus::Paused);
                self.retired.insert(id, (entry.group, entry.task));
            }
        }
        for camera in cameras {
            match self.streams.remove(&camera.id) {
                None => {
                    let shared = Arc::new(StreamShared::new(camera.id));
                    let (group, previous) = match self.retired.remove(&camera.id) {
                        Some((group, task)) => (group, Some(task)),
                        None => (Arc::new(SessionGroup::default().named(format!("cam-{}", camera.id))), None),
                    };
                    let entry = self.spawn_camera(camera, shared, group, previous);
                    self.streams.insert(camera.id, entry);
                }
                Some(entry) if entry.config.connection_differs(camera) => {
                    tracing::info!(camera = %camera.id, "connection settings changed; restarting stream");
                    entry.control.send_modify(|c| c.stop = true);
                    entry.shared.clear_frame();
                    entry.shared.set_status(StreamStatus::Connecting);
                    let new = self.spawn_camera(camera, entry.shared, entry.group, Some(entry.task));
                    self.streams.insert(camera.id, new);
                }
                Some(mut entry) => {
                    // Only non-connection fields (the name) changed: nothing to retry. Re-arming an
                    // AuthFailed camera here would spend one of the camera's 5 login attempts.
                    entry.config = camera.clone();
                    self.streams.insert(camera.id, entry);
                }
            }
        }
    }

    fn spawn_camera(
        &self,
        config: &CameraConfig,
        shared: Arc<StreamShared>,
        group: Arc<SessionGroup>,
        previous: Option<JoinHandle<()>>,
    ) -> Entry {
        let (control, control_rx) = watch::channel(Control::default());
        let camera = Arc::new(Camera {
            config: config.clone(),
            shared: shared.clone(),
            group: group.clone(),
            smooth: self.smooth.clone(),
        });
        let task = self.runtime().spawn(guardian(camera, control_rx, self.global.subscribe(), previous));
        Entry { config: config.clone(), shared, group, control, task }
    }

    pub fn stream(&self, id: CameraId) -> Option<Arc<StreamShared>> {
        self.streams.get(&id).map(|e| e.shared.clone())
    }

    /// Window visibility. Hidden > 2 s: stop decoding (keep RTSP). Hidden > 5 min: disconnect.
    /// Visible again: resume immediately.
    pub fn set_window_visible(&self, visible: bool) {
        self.visible.send_if_modified(|v| std::mem::replace(v, visible) != visible);
    }

    /// Smoothing ("Smooth playback"): hold each frame until its playout time on the camera's
    /// own clock plus ~150 ms, absorbing network jitter. Applies live to every stream, current
    /// and future, without reconnecting.
    pub fn set_smooth_playback(&self, enabled: bool) {
        self.smooth.store(enabled, Ordering::Relaxed);
    }

    /// System is going to sleep: tear down every session cleanly.
    pub fn suspend(&self) {
        self.global.send_modify(|g| g.suspended = true);
    }

    /// System woke up: reconnect everything immediately (backoff reset).
    pub fn resume(&self) {
        self.global.send_modify(|g| {
            g.suspended = false;
            g.wake_epoch += 1;
        });
    }

    /// Skip the remaining backoff delay of a camera and reconnect now. Also re-arms a camera
    /// in `AuthFailed`/`Error` (a single new attempt).
    pub fn retry_now(&self, id: CameraId) {
        if let Some(entry) = self.streams.get(&id) {
            entry.control.send_modify(|c| c.retry_epoch += 1);
        }
    }

    /// One-shot DESCRIBE/SETUP/PLAY against `camera` to validate settings. Never retries
    /// (5 wrong passwords lock the camera user for 30 minutes).
    ///
    /// The work runs on the manager's runtime; the returned future can be awaited from any
    /// executor (e.g. GPUI's). Resolves within ~12 s.
    pub fn probe(&self, camera: CameraConfig) -> impl Future<Output = Result<ProbeInfo, String>> + Send + 'static {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.runtime().spawn(async move {
            let _ = tx.send(rtsp::probe(camera).await);
        });
        async move { rx.await.unwrap_or_else(|_| Err("Connection test interrupted".to_owned())) }
    }
}

impl StreamManager {
    /// Stops every stream, giving RTSP TEARDOWNs up to ~2.5 s, then stops the runtime.
    /// Idempotent; also runs on drop. Call it on app quit (GPUI never drops its globals).
    pub fn shutdown(&mut self) {
        let Some(runtime) = self.runtime.take() else { return };
        let tasks: Vec<JoinHandle<()>> = self
            .streams
            .drain()
            .map(|(_, entry)| {
                entry.control.send_modify(|c| c.stop = true);
                entry.task
            })
            .chain(self.retired.drain().map(|(_, (_, task))| task))
            .collect();
        let (tx, rx) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            let _ = timeout(SHUTDOWN_GRACE, futures::future::join_all(tasks)).await;
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(SHUTDOWN_GRACE + Duration::from_millis(500));
        runtime.shutdown_timeout(Duration::from_millis(500));
    }
}

impl Drop for StreamManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Turns raw visibility changes into the debounced [`Activity`] seen by every supervisor.
async fn visibility_controller(mut visible: watch::Receiver<bool>, global: watch::Sender<Global>) {
    let mut tracker = VisibilityTracker::default();
    loop {
        let now = std::time::Instant::now();
        let activity = tracker.activity(now);
        global.send_if_modified(|g| std::mem::replace(&mut g.activity, activity) != activity);
        let next = tracker.next_change(now);
        tokio::select! {
            changed = visible.changed() => {
                if changed.is_err() {
                    return;
                }
                let v = *visible.borrow_and_update();
                tracker.set_visible(v, std::time::Instant::now());
            }
            _ = sleep_until(next.map(Instant::from_std).unwrap_or_else(far_future)), if next.is_some() => {}
        }
    }
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(86_400)
}

/// Runs a camera's supervisor, restarting it (with backoff) if it panics.
async fn guardian(
    camera: Arc<Camera>,
    control: watch::Receiver<Control>,
    global: watch::Receiver<Global>,
    previous: Option<JoinHandle<()>>,
) {
    if let Some(previous) = previous {
        // One session per camera: let the old supervisor finish its TEARDOWN first.
        let _ = timeout(TEARDOWN_BEFORE_CONNECT, previous).await;
    }
    let mut backoff = Backoff::default();
    loop {
        let task = tokio::spawn(supervise(camera.clone(), control.clone(), global.clone()));
        match task.await {
            Ok(()) => return,
            Err(e) if e.is_panic() => {
                tracing::error!(camera = %camera.config.id, "stream supervisor panicked; restarting");
                if control.borrow().stop {
                    return;
                }
                let delay = backoff.next_random_delay();
                camera.shared.update_stats(|s| {
                    s.reconnects += 1;
                    s.last_error = Some("Internal error".into());
                });
                camera.shared.set_status(StreamStatus::Reconnecting {
                    attempt: backoff.attempt(),
                    retry_at: std::time::Instant::now() + delay,
                    reason: "Internal error".into(),
                });
                let mut control = control.clone();
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = control.wait_for(|c| c.stop) => return,
                }
            }
            Err(_) => return,
        }
    }
}

/// How a session ended.
enum End {
    Stopped,
    /// Suspended or hidden long enough: disconnected on purpose.
    Paused,
    Failed { failure: Failure, streaming_since: Option<Instant> },
}

/// Per-camera loop: connect, stream, and reconnect with backoff until stopped.
async fn supervise(camera: Arc<Camera>, mut control: watch::Receiver<Control>, mut global: watch::Receiver<Global>) {
    let shared = &camera.shared;
    let id = camera.config.id;
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let decoder = match DecoderHandle::spawn(shared.clone(), events_tx, camera.smooth.clone()) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(camera = %id, error = %e, "cannot spawn decode thread");
            shared.set_status(StreamStatus::Error("Could not start the decoder".into()));
            return;
        }
    };
    let mut backoff = Backoff::default();
    let mut wake_epoch = global.borrow().wake_epoch;
    let mut session_seq = 0u32;

    'outer: loop {
        if control.borrow_and_update().stop {
            break;
        }
        let g = *global.borrow_and_update();
        if g.wake_epoch != wake_epoch {
            wake_epoch = g.wake_epoch;
            backoff.reset();
        }
        if !g.wants_connection() {
            decoder.pause();
            shared.set_fps(0.0);
            shared.set_status(StreamStatus::Paused);
            tokio::select! {
                r = control.changed() => if r.is_err() { break },
                r = global.changed() => if r.is_err() { break },
            }
            continue;
        }

        let _ = timeout(TEARDOWN_BEFORE_CONNECT, camera.group.await_teardown()).await;
        if control.borrow().stop {
            break;
        }
        session_seq = session_seq.wrapping_add(1);
        shared.set_status(StreamStatus::Connecting);
        let end = run_session(&camera, &decoder, &mut events, session_seq, &mut control, &mut global).await;
        shared.set_fps(0.0);
        shared.update_stats(|s| {
            s.kbps = 0;
            s.camera_fps = 0.0;
            s.jitter_ms = 0.0;
        });
        let (failure, streaming_since) = match end {
            End::Stopped => break,
            End::Paused => continue,
            End::Failed { failure, streaming_since } => (failure, streaming_since),
        };
        if control.borrow().stop {
            break;
        }
        shared.update_stats(|s| s.last_error = Some(failure.reason.clone()));
        match failure.kind {
            FailureKind::Auth | FailureKind::Fatal => {
                tracing::warn!(camera = %id, reason = %failure.reason, "stream failed; not retrying until config changes");
                shared.set_status(if failure.kind == FailureKind::Auth {
                    StreamStatus::AuthFailed
                } else {
                    StreamStatus::Error(failure.reason)
                });
                // Only a config change (restart) or retry_now re-arms the camera.
                loop {
                    tokio::select! {
                        r = control.changed() => {
                            if r.is_err() || control.borrow_and_update().stop {
                                break 'outer;
                            }
                            break;
                        }
                        r = global.changed() => if r.is_err() { break 'outer },
                    }
                }
                backoff.reset();
            }
            FailureKind::Retry => {
                if streaming_since.is_some_and(|t| t.elapsed() >= STABLE_AFTER) {
                    backoff.reset();
                }
                let delay = backoff.next_random_delay();
                let retry_at = Instant::now() + delay;
                tracing::info!(camera = %id, reason = %failure.reason, attempt = backoff.attempt(), ?delay, "reconnecting");
                shared.update_stats(|s| s.reconnects += 1);
                shared.set_status(StreamStatus::Reconnecting {
                    attempt: backoff.attempt(),
                    retry_at: retry_at.into_std(),
                    reason: failure.reason,
                });
                loop {
                    tokio::select! {
                        _ = sleep_until(retry_at) => break,
                        r = control.changed() => {
                            if r.is_err() || control.borrow_and_update().stop {
                                break 'outer;
                            }
                            break; // retry_now
                        }
                        r = global.changed() => {
                            if r.is_err() {
                                break 'outer;
                            }
                            let g = *global.borrow_and_update();
                            if g.wake_epoch != wake_epoch {
                                wake_epoch = g.wake_epoch;
                                backoff.reset();
                                break;
                            }
                            if !g.wants_connection() {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    // Stopped: the session (if any) was dropped in run_session, which spawned its TEARDOWN.
    drop(decoder);
    let _ = timeout(TEARDOWN_WAIT, camera.group.await_teardown()).await;
}

/// Connects and pumps frames into the decoder until the session ends.
async fn run_session(
    camera: &Camera,
    decoder: &DecoderHandle,
    events: &mut mpsc::UnboundedReceiver<DecoderEvent>,
    session: u32,
    control: &mut watch::Receiver<Control>,
    global: &mut watch::Receiver<Global>,
) -> End {
    let shared = &camera.shared;
    let id = camera.config.id;
    let failed = |failure: Failure, streaming_since: Option<Instant>| End::Failed { failure, streaming_since };

    // Drop events from previous sessions.
    while events.try_recv().is_ok() {}

    let connect = tokio::select! {
        r = timeout(CONNECT_TIMEOUT, rtsp::connect(&camera.config, camera.group.clone())) => r,
        r = control.wait_for(|c| c.stop) => {
            let _ = r;
            return End::Stopped;
        }
        r = global.wait_for(|g| !g.wants_connection()) => {
            return if r.is_ok() { End::Paused } else { End::Stopped };
        }
    };
    let mut conn = match connect {
        Ok(Ok(conn)) => conn,
        Ok(Err(failure)) => return failed(failure, None),
        Err(_) => return failed(Failure::retry("Timed out while connecting"), None),
    };
    tracing::info!(camera = %id, codec = conn.codec.label(), "rtsp session playing");
    shared.update_stats(|s| s.codec = conn.codec.label().to_owned());

    let mut params = new_params(shared, &conn.demuxed, conn.stream);
    let mut decoding = global.borrow().activity == Activity::Decode;
    if !decoding {
        decoder.pause();
        shared.set_status(StreamStatus::Paused);
    }
    // Skip to the next keyframe; `count_skipped` is false for the expected skip at start/resume.
    let mut skip_until_key = true;
    let mut count_skipped = false;
    let mut warmup_until = Instant::now();
    let mut skip_rasl = false;
    let mut discontinuity = false;
    let mut streaming_since: Option<Instant> = None;
    let mut last_frame = Instant::now();

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut tick_at = Instant::now();
    let mut published_at_tick = shared.frames_published();
    let mut rx = RxStats::default();
    let mut dropped = 0u64;

    loop {
        tokio::select! {
            biased;
            r = control.changed() => {
                if r.is_err() || control.borrow_and_update().stop {
                    return End::Stopped;
                }
            }
            r = global.changed() => {
                if r.is_err() {
                    return End::Stopped;
                }
                let g = *global.borrow_and_update();
                if !g.wants_connection() {
                    return End::Paused;
                }
                let decode = g.activity == Activity::Decode;
                if decode != decoding {
                    decoding = decode;
                    if decoding {
                        tracing::debug!(camera = %id, "window visible; resuming decode");
                        skip_until_key = true;
                        count_skipped = false;
                        shared.set_status(StreamStatus::WaitingKeyframe);
                    } else {
                        tracing::debug!(camera = %id, "window hidden; pausing decode");
                        decoder.pause();
                        shared.set_status(StreamStatus::Paused);
                    }
                }
            }
            Some(event) = events.recv() => {
                match event {
                    DecoderEvent::Failed { session: s, reason } if s == session => {
                        return failed(Failure::retry(reason), streaming_since);
                    }
                    DecoderEvent::Fatal { session: s, reason } if s == session => {
                        return failed(Failure::fatal(reason), streaming_since);
                    }
                    _ => {}
                }
            }
            _ = tick.tick() => {
                let now = Instant::now();
                let secs = now.duration_since(tick_at).as_secs_f64().max(1e-3);
                let published = shared.frames_published();
                shared.set_fps(((published - published_at_tick) as f64 / secs) as f32);
                let kbps = rx.tick(secs);
                let new_drops = std::mem::take(&mut dropped);
                shared.update_stats(|s| {
                    s.kbps = kbps;
                    s.camera_fps = rx.camera_fps();
                    s.jitter_ms = rx.jitter_ms();
                    s.dropped_frames += new_drops;
                });
                tick_at = now;
                published_at_tick = published;
            }
            item = timeout_at(last_frame + FRAME_TIMEOUT, conn.demuxed.next()) => {
                let item = match item {
                    Err(_) => return failed(Failure::retry("No video for 10 s"), streaming_since),
                    Ok(None) => return failed(Failure::retry("Camera ended the stream"), streaming_since),
                    Ok(Some(Err(e))) => return failed(rtsp::rtsp_failure(e), streaming_since),
                    Ok(Some(Ok(item))) => item,
                };
                let CodecItem::VideoFrame(frame) = item else { continue };
                if frame.stream_id() != conn.stream {
                    continue;
                }
                last_frame = Instant::now();
                streaming_since.get_or_insert(last_frame);
                let arrival = last_frame.into_std();
                let media_ts = frame.timestamp().elapsed_secs();
                rx.on_frame(arrival, media_ts, frame.data().len(), frame.loss() > 0);
                if (frame.has_new_parameters() || params.is_none())
                    && let Some(p) = new_params(shared, &conn.demuxed, conn.stream)
                {
                    params = Some(p);
                }
                let Some(params) = &params else { continue };
                if !decoding {
                    continue;
                }
                let picture = match conn.codec {
                    Codec::H264 => HevcPicture::Other,
                    Codec::H265 => hevc_picture(frame.data()),
                };
                let key = frame.is_random_access_point()
                    || matches!(picture, HevcPicture::ClosedIrap | HevcPicture::Cra);
                if frame.loss() > 0 {
                    discontinuity = true;
                    if !key && !skip_until_key {
                        tracing::debug!(camera = %shared.id(), lost = frame.loss(), "rtp loss; waiting for keyframe");
                        skip_until_key = true;
                        count_skipped = true;
                        shared.transition(|s| *s == StreamStatus::Live, StreamStatus::WaitingKeyframe);
                    }
                }
                if skip_until_key {
                    if !key {
                        dropped += u64::from(count_skipped);
                        continue;
                    }
                    skip_until_key = false;
                    // Leading pictures of a CRA we start on reference frames we never had.
                    skip_rasl = picture == HevcPicture::Cra;
                    if !count_skipped {
                        warmup_until = last_frame + WARMUP;
                    }
                } else if skip_rasl {
                    if picture == HevcPicture::Rasl {
                        continue;
                    }
                    skip_rasl = false;
                }
                let capacity = if last_frame < warmup_until { WARMUP_QUEUE_CAPACITY } else { QUEUE_CAPACITY };
                let au = AccessUnit {
                    data: frame.into_data(),
                    params: params.clone(),
                    key,
                    session,
                    discontinuity: std::mem::take(&mut discontinuity),
                    media_ts,
                    arrival,
                };
                if !decoder.try_send(au, capacity) {
                    tracing::debug!(camera = %shared.id(), capacity, "decode queue full; waiting for keyframe");
                    dropped += 1;
                    skip_until_key = true;
                    count_skipped = true;
                    discontinuity = true;
                    shared.transition(|s| *s == StreamStatus::Live, StreamStatus::WaitingKeyframe);
                }
            }
        }
    }
}

fn new_params(shared: &StreamShared, demuxed: &retina::client::Demuxed, stream: usize) -> Option<Arc<CodecParams>> {
    let params = CodecParams::from_video(rtsp::video_params(demuxed, stream)?)?;
    shared.update_stats(|s| {
        s.width = params.width;
        s.height = params.height;
    });
    Some(Arc::new(params))
}
