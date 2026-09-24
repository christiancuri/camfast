//! VideoToolbox hardware decoding on one dedicated thread per camera.
//!
//! The RTSP task hands access units over with [`DecoderHandle::try_send`], which never
//! blocks: when the queue is full the frame is dropped and the caller resynchronizes on the
//! next keyframe. Decoded `'420f'` IOSurface-backed buffers
//! are published straight from the VideoToolbox output callback into [`StreamShared`].
//!
//! With smoothing on, compressed access units are held (never decoded buffers) until their
//! playout time from [`Pacer`], so Wi-Fi bursts are shown at the camera's own cadence.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use core_foundation::base::TCFType;
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, CMVideoFormatDescriptionCreateFromHEVCParameterSets,
    kCMBlockBufferAssureMemoryNowFlag,
};
use objc2_core_video::{
    CVImageBuffer, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession,
    VTSessionSetProperty, kVTDecompressionPropertyKey_RealTime, kVTVideoDecoderBadDataErr, kVTVideoDecoderMalfunctionErr, kVTVideoDecoderReferenceMissingErr,
    kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::rtsp::{Codec, CodecParams};
use crate::timing::Pacer;
use crate::{StreamShared, StreamStatus};

/// Max access units queued between the RTSP task and the decode thread (2 s at 30 fps).
/// Wi-Fi jitter delivers frames in bursts; a shallow queue overflowed on every burst and froze
/// the picture until the next keyframe. HW decode drains a full queue in well under 100 ms,
/// so depth adds no steady-state latency — it only absorbs bursts.
pub(crate) const QUEUE_CAPACITY: usize = 60;
/// Capacity right after decoding (re)starts: creating the VT session takes ~70 ms and cameras
/// send their buffered GOP as a burst on PLAY, so a 4-deep queue would overflow and freeze
/// the picture until the next keyframe (2 s at GOP 60). HW decode drains this in ~100 ms.
pub(crate) const WARMUP_QUEUE_CAPACITY: usize = 90;
/// This many session recreations within [`RECREATE_WINDOW`] escalate to a full reconnect.
const MAX_RECREATES: usize = 3;
const RECREATE_WINDOW: Duration = Duration::from_secs(30);
/// While smoothing, an access unit held this long after arrival means the decoder fell
/// behind: decode the backlog immediately instead of growing latency.
const MAX_BACKLOG: Duration = Duration::from_secs(1);

/// `'420f'`: the only format GPUI's surface renderer accepts.
pub const PIXEL_FORMAT_420F: u32 = kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;

/// One encoded picture (4-byte length-prefixed NAL units) plus the parameters it depends on.
pub(crate) struct AccessUnit {
    pub data: Vec<u8>,
    /// Shared per RTSP session; replaced when the camera changes its parameter sets.
    pub params: Arc<CodecParams>,
    pub key: bool,
    /// RTSP session sequence number; a change means "start over from a keyframe".
    pub session: u32,
    /// Frames were lost or dropped right before this one.
    pub discontinuity: bool,
    /// RTP media timestamp, in seconds since the start of the RTSP session.
    pub media_ts: f64,
    /// When the frame came out of the RTSP demuxer.
    pub arrival: Instant,
}

enum Msg {
    Frame(AccessUnit),
    /// Stop decoding: invalidate the VT session and release the retained frame.
    Pause,
}

/// Problems the decode thread reports to the camera's supervisor.
#[derive(Debug)]
pub(crate) enum DecoderEvent {
    /// Reconnect with backoff.
    Failed { session: u32, reason: String },
    /// Don't retry (e.g. VideoToolbox produced a pixel format the renderer can't draw).
    Fatal { session: u32, reason: String },
}

/// Owner side of a camera's decode thread. Dropping it stops the thread (frames still queued
/// are discarded, not decoded); the VT session is invalidated on that thread.
pub(crate) struct DecoderHandle {
    tx: mpsc::Sender<Msg>,
    in_flight: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
}

impl Drop for DecoderHandle {
    fn drop(&mut self) {
        // Up to WARMUP_QUEUE_CAPACITY frames may be queued; decoding them after a stop/restart
        // would waste work and publish stale pictures into the (possibly reused) StreamShared.
        self.stopped.store(true, Ordering::Release);
    }
}

impl DecoderHandle {
    /// `smooth`: pace decoding at the camera's cadence (read live, before every access unit).
    pub fn spawn(
        shared: Arc<StreamShared>,
        events: UnboundedSender<DecoderEvent>,
        smooth: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let (counter, stop) = (in_flight.clone(), stopped.clone());
        std::thread::Builder::new()
            .name(format!("cam-decode-{}", shared.id()))
            .spawn(move || run(rx, counter, stop, smooth, shared, events))?;
        Ok(Self { tx, in_flight, stopped })
    }

    /// Queues an access unit without ever blocking. Returns `false` (and drops the frame)
    /// when `capacity` frames are already queued; the caller must then skip to the next keyframe.
    pub fn try_send(&self, au: AccessUnit, capacity: usize) -> bool {
        if self.in_flight.load(Ordering::Acquire) >= capacity {
            return false;
        }
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        if self.tx.send(Msg::Frame(au)).is_err() {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        true
    }

    pub fn pause(&self) {
        let _ = self.tx.send(Msg::Pause);
    }
}

fn run(
    rx: mpsc::Receiver<Msg>,
    in_flight: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    smooth: Arc<AtomicBool>,
    shared: Arc<StreamShared>,
    events: UnboundedSender<DecoderEvent>,
) {
    let mut decoder = Decoder::new(shared.clone(), events.clone());
    let mut pacer = Pacer::default();
    let mut smoothing = false;
    // Access units waiting for their playout time (smoothing only), with that time. They
    // still count in `in_flight`, so the RTSP side's queue bound covers them too.
    let mut held: VecDeque<(AccessUnit, Instant)> = VecDeque::with_capacity(WARMUP_QUEUE_CAPACITY);
    let dispatch = |decoder: &mut Decoder, msg: Msg| {
        let result = catch_unwind(AssertUnwindSafe(|| match msg {
            Msg::Frame(au) => decoder.decode(au),
            Msg::Pause => decoder.pause(),
        }));
        if result.is_err() {
            tracing::error!(camera = %shared.id(), "decoder panicked; resetting");
            let session = decoder.session_id;
            let broken = std::mem::replace(decoder, Decoder::new(shared.clone(), events.clone()));
            let _ = catch_unwind(AssertUnwindSafe(move || drop(broken)));
            let _ = events.send(DecoderEvent::Failed { session, reason: "Internal decoder error".into() });
        }
    };
    let release = |held: &mut VecDeque<(AccessUnit, Instant)>| {
        let (au, _) = held.pop_front()?;
        in_flight.fetch_sub(1, Ordering::AcqRel);
        (!stopped.load(Ordering::Acquire)).then_some(Msg::Frame(au))
    };

    loop {
        let enabled = smooth.load(Ordering::Relaxed);
        if enabled != smoothing {
            smoothing = enabled;
            pacer.reset();
        }
        // Decode what is due: everything once smoothing is off or the decoder fell behind.
        if let Some((oldest, _)) = held.front() {
            let now = Instant::now();
            let catch_up = now.saturating_duration_since(oldest.arrival) > MAX_BACKLOG;
            if catch_up {
                tracing::debug!(camera = %shared.id(), held = held.len(), "playout backlog; catching up");
                pacer.reset();
            }
            while held.front().is_some_and(|(_, due)| !smoothing || catch_up || *due <= now) {
                if let Some(msg) = release(&mut held) {
                    dispatch(&mut decoder, msg);
                }
            }
        }
        // Timed receive while holding: new access units, pause and stop stay responsive.
        let msg = match held.front() {
            None => match rx.recv() {
                Ok(msg) => msg,
                Err(_) => break,
            },
            Some((_, due)) => match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                Ok(msg) => msg,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };
        match msg {
            Msg::Frame(au) if smoothing && !stopped.load(Ordering::Acquire) => {
                let due = pacer.due(&au);
                held.push_back((au, due));
                if held.len() == 1 && due <= Instant::now() {
                    // Late (or no jitter to absorb): decode right away.
                    if let Some(msg) = release(&mut held) {
                        dispatch(&mut decoder, msg);
                    }
                }
            }
            Msg::Frame(au) => {
                in_flight.fetch_sub(1, Ordering::AcqRel);
                if !stopped.load(Ordering::Acquire) {
                    dispatch(&mut decoder, Msg::Frame(au));
                }
            }
            Msg::Pause => {
                // Held frames would be cleared by the pause anyway.
                in_flight.fetch_sub(held.len(), Ordering::AcqRel);
                held.clear();
                pacer.reset();
                dispatch(&mut decoder, Msg::Pause);
            }
        }
    }
    let _ = catch_unwind(AssertUnwindSafe(move || drop(decoder)));
}

/// Why frames are being skipped until the next keyframe.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Skip {
    /// New session / resume: the skipped frames are expected, not counted as dropped.
    Start,
    /// Decoder error: skipped frames count as dropped.
    Error,
}

// Outcomes recorded by the output callback for the frame being decoded.
const OUT_NONE: u32 = 0;
const OUT_PUBLISHED: u32 = 1;
const OUT_NO_IMAGE: u32 = 2;
const OUT_BAD_FORMAT: u32 = 3;
const OUT_PANIC: u32 = 4;

/// State reachable from the VT output callback (through the refcon). Everything is atomic
/// because VideoToolbox may invoke the callback from one of its own threads.
struct CallbackState {
    shared: Arc<StreamShared>,
    status: AtomicI32,
    outcome: AtomicU32,
    /// Pixel format of the rejected buffer when `outcome == OUT_BAD_FORMAT`.
    bad_format: AtomicU32,
    /// First buffer of the current VT session already verified as IOSurface-backed '420f'.
    format_ok: AtomicBool,
}

struct Decoder {
    shared: Arc<StreamShared>,
    events: UnboundedSender<DecoderEvent>,
    params: Option<Arc<CodecParams>>,
    format: Option<CFRetained<CMFormatDescription>>,
    vt: Option<CFRetained<VTDecompressionSession>>,
    /// Boxed so the refcon pointer given to VideoToolbox stays valid; outlives `vt` (see Drop).
    callback: Box<CallbackState>,
    skip: Option<Skip>,
    session_id: u32,
    /// We reported `Live` for the current run of decoded frames.
    live: bool,
    /// Stop decoding until the next RTSP session (after a fatal error or escalation).
    halted: bool,
    recreates: VecDeque<Instant>,
}

impl Decoder {
    fn new(shared: Arc<StreamShared>, events: UnboundedSender<DecoderEvent>) -> Self {
        let callback = Box::new(CallbackState {
            shared: shared.clone(),
            status: AtomicI32::new(0),
            outcome: AtomicU32::new(OUT_NONE),
            bad_format: AtomicU32::new(0),
            format_ok: AtomicBool::new(false),
        });
        Self {
            shared,
            events,
            params: None,
            format: None,
            vt: None,
            callback,
            skip: Some(Skip::Start),
            session_id: 0,
            live: false,
            halted: false,
            recreates: VecDeque::new(),
        }
    }

    fn pause(&mut self) {
        self.invalidate();
        self.skip = Some(Skip::Start);
        self.live = false;
        self.shared.clear_frame();
    }

    fn decode(&mut self, au: AccessUnit) {
        if au.session != self.session_id {
            self.session_id = au.session;
            self.skip = Some(Skip::Start);
            self.live = false;
            self.halted = false;
            self.recreates.clear();
        }
        if self.halted {
            return;
        }
        if au.discontinuity {
            // The RTSP side already skipped to a keyframe; make the next publish report Live.
            self.live = false;
            if !au.key {
                self.skip.get_or_insert(Skip::Error);
            }
        }
        if !self.params.as_ref().is_some_and(|p| Arc::ptr_eq(p, &au.params)) && !self.set_params(&au.params) {
            return;
        }
        if let Some(skip) = self.skip {
            if !au.key {
                if skip == Skip::Error {
                    self.shared.update_stats(|s| s.dropped_frames += 1);
                }
                return;
            }
            self.skip = None;
        }
        if self.vt.is_none()
            && let Err(status) = self.create_session()
        {
            tracing::warn!(camera = %self.shared.id(), status, "VTDecompressionSessionCreate failed");
            self.decoder_error(status, true);
            return;
        }
        match self.decode_frame(&au.data) {
            Ok(()) => {
                if self.callback.outcome.load(Ordering::Acquire) == OUT_PUBLISHED {
                    self.shared.observe_publish(au.arrival);
                }
            }
            Err(status) => {
                let recreate = status != kVTVideoDecoderBadDataErr && status != kVTVideoDecoderReferenceMissingErr;
                if recreate {
                    tracing::warn!(camera = %self.shared.id(), status, "decoder error; recreating session");
                } else {
                    tracing::debug!(camera = %self.shared.id(), status, "decoder error; waiting for keyframe");
                }
                self.decoder_error(status, recreate);
            }
        }
    }

    /// Returns false if the parameters are unusable (reported to the supervisor).
    fn set_params(&mut self, params: &Arc<CodecParams>) -> bool {
        let same_format = self.params.as_deref().is_some_and(|p| p.codec == params.codec && p.sets == params.sets);
        self.params = Some(params.clone());
        if same_format && self.format.is_some() {
            return true;
        }
        match create_format_description(params) {
            Ok(format) => {
                if let Some(vt) = &self.vt {
                    // SAFETY: both objects are valid CF objects.
                    if !unsafe { vt.can_accept_format_description(&format) } {
                        self.invalidate();
                    }
                }
                self.format = Some(format);
                true
            }
            Err(status) => {
                tracing::error!(camera = %self.shared.id(), status, "invalid parameter sets");
                self.format = None;
                self.invalidate();
                self.halt(DecoderEvent::Failed {
                    session: self.session_id,
                    reason: "Invalid video parameters".into(),
                });
                false
            }
        }
    }

    fn create_session(&mut self) -> Result<(), i32> {
        let format = self.format.as_ref().ok_or(-1)?;
        // SAFETY: the extern statics are valid CFStrings provided by the frameworks.
        let (hw_key, pf_key, io_key, metal_key, realtime_key) = unsafe {
            (
                kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder,
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferMetalCompatibilityKey,
                kVTDecompressionPropertyKey_RealTime,
            )
        };
        let yes = CFBoolean::new(true);
        let spec = CFDictionary::<CFString, CFType>::from_slices(&[hw_key], &[yes]);
        let pixel_format = CFNumber::new_i32(PIXEL_FORMAT_420F as i32);
        let io_surface = CFDictionary::<CFString, CFType>::empty();
        let attrs = CFDictionary::<CFString, CFType>::from_slices(
            &[pf_key, io_key, metal_key],
            &[&pixel_format, &io_surface, yes],
        );
        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(output_callback),
            decompressionOutputRefCon: (&*self.callback as *const CallbackState).cast_mut().cast(),
        };
        let mut out: *mut VTDecompressionSession = ptr::null_mut();
        // SAFETY: all arguments are valid; the record is copied by VideoToolbox and the refcon
        // (boxed CallbackState) outlives the session (invalidated before it is freed).
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                format,
                Some(spec.as_opaque()),
                Some(attrs.as_opaque()),
                &record,
                NonNull::from(&mut out),
            )
        };
        if status != 0 {
            return Err(status);
        }
        let vt = NonNull::new(out).ok_or(-1)?;
        // SAFETY: VTDecompressionSessionCreate follows the create rule (+1 retained).
        let vt = unsafe { CFRetained::from_raw(vt) };
        // SAFETY: valid session, key and value.
        let status = unsafe { VTSessionSetProperty(&vt, realtime_key, Some(yes)) };
        if status != 0 {
            tracing::debug!(status, "kVTDecompressionPropertyKey_RealTime not supported");
        }
        self.callback.format_ok.store(false, Ordering::Relaxed);
        self.vt = Some(vt);
        Ok(())
    }

    fn decode_frame(&mut self, data: &[u8]) -> Result<(), i32> {
        let (Some(vt), Some(format)) = (&self.vt, &self.format) else { return Err(-1) };
        let sample = create_sample_buffer(data, format)?;
        self.callback.status.store(0, Ordering::Relaxed);
        self.callback.outcome.store(OUT_NONE, Ordering::Relaxed);
        let mut info = VTDecodeInfoFlags(0);
        // SAFETY: valid session and sample buffer. No async flag: the output callback has run
        // by the time this returns.
        let status = unsafe { vt.decode_frame(&sample, VTDecodeFrameFlags(0), ptr::null_mut(), &mut info) };
        tracing::trace!(
            status,
            callback_status = self.callback.status.load(Ordering::Acquire),
            outcome = self.callback.outcome.load(Ordering::Acquire),
            info = info.0,
            len = data.len(),
            "decode_frame"
        );
        if status != 0 {
            return Err(status);
        }
        let status = self.callback.status.load(Ordering::Acquire);
        if status != 0 {
            return Err(status);
        }
        match self.callback.outcome.load(Ordering::Acquire) {
            OUT_PUBLISHED => {
                if !self.live {
                    self.live = true;
                    self.shared.transition(
                        |s| matches!(s, StreamStatus::Connecting | StreamStatus::WaitingKeyframe),
                        StreamStatus::Live,
                    );
                }
            }
            OUT_BAD_FORMAT => {
                let fourcc = self.callback.bad_format.load(Ordering::Relaxed);
                tracing::error!(
                    camera = %self.shared.id(),
                    format = %fourcc_str(fourcc),
                    "decoder output is not an IOSurface-backed '420f' buffer; refusing to publish"
                );
                self.invalidate();
                self.halt(DecoderEvent::Fatal {
                    session: self.session_id,
                    reason: format!("Unsupported image format ({})", fourcc_str(fourcc)),
                });
            }
            OUT_PANIC => return Err(kVTVideoDecoderMalfunctionErr),
            // Frame dropped by the decoder or no output (non-displayed picture): nothing to show.
            _ => {}
        }
        Ok(())
    }

    /// Handles a decode/creation error: resync on a keyframe and, if `recreate`, drop the VT
    /// session. Too many recreations in a short time escalate to a full RTSP reconnect.
    fn decoder_error(&mut self, status: i32, recreate: bool) {
        tracing::debug!(camera = %self.shared.id(), status, recreate, "decoder error; waiting for keyframe");
        self.skip = Some(Skip::Error);
        self.shared.update_stats(|s| s.dropped_frames += 1);
        if self.live {
            self.live = false;
            self.shared.transition(|s| *s == StreamStatus::Live, StreamStatus::WaitingKeyframe);
        }
        if !recreate {
            return;
        }
        self.invalidate();
        let now = Instant::now();
        while self.recreates.front().is_some_and(|t| now.duration_since(*t) > RECREATE_WINDOW) {
            self.recreates.pop_front();
        }
        self.recreates.push_back(now);
        if self.recreates.len() >= MAX_RECREATES {
            tracing::error!(camera = %self.shared.id(), status, "decoder keeps failing; reconnecting");
            self.halt(DecoderEvent::Failed { session: self.session_id, reason: "Decoder failure".into() });
        }
    }

    fn halt(&mut self, event: DecoderEvent) {
        self.halted = true;
        let _ = self.events.send(event);
    }

    fn invalidate(&mut self) {
        if let Some(vt) = self.vt.take() {
            // SAFETY: valid session; after this no more callbacks are delivered.
            unsafe { vt.invalidate() };
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // Must happen before `callback` is freed.
        self.invalidate();
    }
}

/// VideoToolbox output callback. Never unwinds into VideoToolbox.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: i32,
    info: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: refcon is the boxed CallbackState, alive for the whole VT session.
    let state = unsafe { &*(refcon as *const CallbackState) };
    let result = catch_unwind(AssertUnwindSafe(|| {
        if status != 0 {
            state.status.store(status, Ordering::Release);
            return;
        }
        if image.is_null() {
            let _ = info; // FrameDropped or a non-output picture.
            state.outcome.store(OUT_NO_IMAGE, Ordering::Release);
            return;
        }
        // SAFETY: `image` is a valid CVPixelBuffer we don't own (get rule): wrapping retains
        // it once, and the wrapper releases it when the last clone is dropped.
        let buffer = unsafe {
            core_video::pixel_buffer::CVPixelBuffer::wrap_under_get_rule(image as core_video::pixel_buffer::CVPixelBufferRef)
        };
        if !state.format_ok.load(Ordering::Relaxed) {
            let format = buffer.get_pixel_format();
            if format != PIXEL_FORMAT_420F || !is_iosurface_backed(&buffer) {
                state.bad_format.store(format, Ordering::Relaxed);
                state.outcome.store(OUT_BAD_FORMAT, Ordering::Release);
                return;
            }
            state.format_ok.store(true, Ordering::Relaxed);
        }
        state.shared.publish_frame(buffer);
        state.outcome.store(OUT_PUBLISHED, Ordering::Release);
    }));
    if result.is_err() {
        state.outcome.store(OUT_PANIC, Ordering::Release);
    }
}

fn create_format_description(params: &CodecParams) -> Result<CFRetained<CMFormatDescription>, i32> {
    if params.sets.iter().any(|s| s.is_empty()) {
        return Err(-1);
    }
    let pointers: Vec<NonNull<u8>> = params.sets.iter().map(|s| NonNull::from(&s[0])).collect();
    let sizes: Vec<usize> = params.sets.iter().map(Vec::len).collect();
    let mut out: *const CMFormatDescription = ptr::null();
    let pointers_ptr = NonNull::new(pointers.as_ptr().cast_mut()).ok_or(-1)?;
    let sizes_ptr = NonNull::new(sizes.as_ptr().cast_mut()).ok_or(-1)?;
    // SAFETY: pointers/sizes describe `sets.len()` valid parameter-set NAL units.
    let status = unsafe {
        match params.codec {
            Codec::H264 => CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None,
                pointers.len(),
                pointers_ptr,
                sizes_ptr,
                4,
                NonNull::from(&mut out),
            ),
            Codec::H265 => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                None,
                pointers.len(),
                pointers_ptr,
                sizes_ptr,
                4,
                None,
                NonNull::from(&mut out),
            ),
        }
    };
    if status != 0 {
        return Err(status);
    }
    let out = NonNull::new(out.cast_mut()).ok_or(-1)?;
    // SAFETY: create rule (+1 retained).
    Ok(unsafe { CFRetained::from_raw(out) })
}

/// Copies one access unit into a CMBlockBuffer and wraps it in a ready CMSampleBuffer.
fn create_sample_buffer(data: &[u8], format: &CMFormatDescription) -> Result<CFRetained<CMSampleBuffer>, i32> {
    let len = data.len();
    let source = NonNull::new(data.as_ptr().cast_mut().cast::<c_void>()).filter(|_| len > 0).ok_or(-1)?;
    let mut block: *mut CMBlockBuffer = ptr::null_mut();
    // SAFETY: NULL memory block + default allocator: CoreMedia allocates `len` bytes now.
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            ptr::null_mut(),
            len,
            None,
            ptr::null(),
            0,
            len,
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut block),
        )
    };
    if status != 0 {
        return Err(status);
    }
    // SAFETY: create rule.
    let block = unsafe { CFRetained::from_raw(NonNull::new(block).ok_or(-1)?) };
    // SAFETY: `source` points to `len` readable bytes; the block holds `len` bytes.
    let status = unsafe { CMBlockBuffer::replace_data_bytes(source, &block, 0, len) };
    if status != 0 {
        return Err(status);
    }
    let mut sample: *mut CMSampleBuffer = ptr::null_mut();
    let sizes = [len];
    // SAFETY: one sample covering the whole block, no timing info.
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            1,
            0,
            ptr::null(),
            1,
            sizes.as_ptr(),
            NonNull::from(&mut sample),
        )
    };
    if status != 0 {
        return Err(status);
    }
    // SAFETY: create rule.
    Ok(unsafe { CFRetained::from_raw(NonNull::new(sample).ok_or(-1)?) })
}

/// True if `buffer` is backed by an IOSurface (required by `gpui::surface`).
///
/// Use this instead of core-video 0.5.2's `CVPixelBuffer::get_io_surface()`, which wraps a
/// Get-rule pointer under the Create rule and therefore over-releases the IOSurface.
pub fn is_iosurface_backed(buffer: &core_video::pixel_buffer::CVPixelBuffer) -> bool {
    // SAFETY: valid pixel buffer; the returned pointer is not retained and only null-checked.
    unsafe {
        !core_video::pixel_buffer_io_surface::CVPixelBufferGetIOSurface(buffer.as_concrete_TypeRef()).is_null()
    }
}

/// `'420f'`-style rendering of a CoreVideo pixel format code.
pub fn fourcc_str(code: u32) -> String {
    let bytes = code.to_be_bytes();
    if bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        format!("'{}'", bytes.iter().map(|b| *b as char).collect::<String>())
    } else {
        format!("0x{code:08x}")
    }
}
