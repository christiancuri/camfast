//! Frame timing: receive statistics (RTSP task) and playout pacing (decode thread).
//!
//! Both work from two clocks per frame: the RTP media timestamp (the camera's capture clock)
//! and the local arrival `Instant` (when the frame came out of the demuxer). Their difference
//! separates what the camera sends from what the Wi-Fi does to it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::decoder::AccessUnit;
use crate::rtsp::CodecParams;

/// Media-time window for [`RxStats::camera_fps`].
const CAMERA_FPS_WINDOW: f64 = 2.0;
/// Frames kept for the camera fps window (enough for 2 s at 120 fps).
const CAMERA_FPS_FRAMES: usize = 256;
/// One-second buckets averaged by [`RxStats::tick`] (so keyframes don't spike the bitrate).
const KBPS_BUCKETS: usize = 5;
/// A media-time step outside `0..=MAX_MEDIA_STEP` is a timestamp jump, not a frame interval.
const MAX_MEDIA_STEP: f64 = 2.0;

/// Fixed-capacity FIFO that overwrites its oldest element when full (no allocation).
struct Ring<T: Copy + Default, const N: usize> {
    items: [T; N],
    start: usize,
    len: usize,
}

impl<T: Copy + Default, const N: usize> Default for Ring<T, N> {
    fn default() -> Self {
        Self { items: [T::default(); N], start: 0, len: 0 }
    }
}

impl<T: Copy + Default, const N: usize> Ring<T, N> {
    fn push(&mut self, item: T) {
        if self.len == N {
            self.items[self.start] = item;
            self.start = (self.start + 1) % N;
        } else {
            self.items[(self.start + self.len) % N] = item;
            self.len += 1;
        }
    }

    fn front(&self) -> Option<T> {
        (self.len > 0).then(|| self.items[self.start])
    }

    fn back(&self) -> Option<T> {
        (self.len > 0).then(|| self.items[(self.start + self.len - 1) % N])
    }

    fn pop_front(&mut self) {
        if self.len > 0 {
            self.start = (self.start + 1) % N;
            self.len -= 1;
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn iter(&self) -> impl Iterator<Item = T> + '_ {
        (0..self.len).map(|i| self.items[(self.start + i) % N])
    }
}

/// Per-session receive statistics, fed with every video frame by the RTSP task.
#[derive(Default)]
pub(crate) struct RxStats {
    /// Media timestamps (s) of the frames within the last [`CAMERA_FPS_WINDOW`].
    media: Ring<f64, CAMERA_FPS_FRAMES>,
    /// Previous frame (arrival, media timestamp) for the jitter estimator.
    prev: Option<(Instant, f64)>,
    /// RFC 3550 interarrival jitter, in seconds.
    jitter: f64,
    /// (bytes, seconds) per stats tick.
    buckets: Ring<(u64, f64), KBPS_BUCKETS>,
    bytes: u64,
}

impl RxStats {
    /// Records one received frame. `discontinuity`: RTP packets were lost right before it.
    pub fn on_frame(&mut self, arrival: Instant, media_ts: f64, len: usize, discontinuity: bool) {
        self.bytes += len as u64;

        if self.media.back().is_some_and(|last| !(0.0..=MAX_MEDIA_STEP).contains(&(media_ts - last))) {
            self.media.clear();
        }
        self.media.push(media_ts);
        while self.media.front().is_some_and(|first| first < media_ts - CAMERA_FPS_WINDOW) {
            self.media.pop_front();
        }

        // D = (arrival_i - arrival_{i-1}) - (ts_i - ts_{i-1}); J += (|D| - J) / 16.
        if let Some((prev_arrival, prev_ts)) = self.prev.filter(|_| !discontinuity) {
            let media_step = media_ts - prev_ts;
            if (0.0..=MAX_MEDIA_STEP).contains(&media_step) {
                let d = arrival.saturating_duration_since(prev_arrival).as_secs_f64() - media_step;
                self.jitter += (d.abs() - self.jitter) / 16.0;
            }
        }
        self.prev = Some((arrival, media_ts));
    }

    /// Rate the camera is sending at, from media timestamps (independent of network jitter).
    pub fn camera_fps(&self) -> f32 {
        match (self.media.front(), self.media.back()) {
            (Some(first), Some(last)) if last > first => ((self.media.len - 1) as f64 / (last - first)) as f32,
            _ => 0.0,
        }
    }

    pub fn jitter_ms(&self) -> f32 {
        (self.jitter * 1000.0) as f32
    }

    /// Closes a stats period of `secs` seconds; returns the bitrate over the last few periods.
    pub fn tick(&mut self, secs: f64) -> u32 {
        self.buckets.push((std::mem::take(&mut self.bytes), secs));
        let (bytes, secs) = self.buckets.iter().fold((0u64, 0.0), |(b, s), (bb, ss)| (b + bb, s + ss));
        if secs <= 0.0 { 0 } else { (bytes as f64 * 8.0 / 1000.0 / secs).round() as u32 }
    }
}

/// Extra latency the smoothing adds on top of the least-delayed frame, to absorb jitter.
pub(crate) const PLAYOUT_DELAY: Duration = Duration::from_millis(150);
/// How fast the baseline drifts later (s per s) to follow clock drift and route changes.
const BASELINE_CREEP: f64 = 0.0005;
/// A frame this much later than the baseline predicts means the timestamps jumped: resync.
const RESYNC_LATE: f64 = 1.0;

/// Maps media timestamps to local playout instants for the decode thread's smoothing.
///
/// The baseline `offset` is the minimum observed `arrival - media_ts` (the least-delayed
/// frame), creeping slowly upward; a frame is due at `media_ts + offset + PLAYOUT_DELAY`,
/// which is never later than its own arrival + `PLAYOUT_DELAY`.
#[derive(Default)]
pub(crate) struct Pacer {
    /// Local time origin: the arrival of the first frame after a reset.
    epoch: Option<Instant>,
    /// Baseline, seconds relative to `epoch`.
    offset: f64,
    /// Arrival (s since `epoch`) of the previous frame, for the creep.
    last_arrival: f64,
    session: u32,
    params: Option<Arc<CodecParams>>,
}

impl Pacer {
    pub fn reset(&mut self) {
        self.epoch = None;
    }

    /// When `au` should be decoded. Resets the baseline on a new session, a parameter change
    /// or a discontinuity (loss / keyframe resync).
    pub fn due(&mut self, au: &AccessUnit) -> Instant {
        if !self.params.as_ref().is_some_and(|p| Arc::ptr_eq(p, &au.params)) {
            self.params = Some(au.params.clone());
            self.reset();
        }
        if au.session != self.session || au.discontinuity {
            self.session = au.session;
            self.reset();
        }
        self.target(au.arrival, au.media_ts)
    }

    fn target(&mut self, arrival: Instant, media_ts: f64) -> Instant {
        let epoch = *self.epoch.get_or_insert(arrival);
        let arrival_s = arrival.saturating_duration_since(epoch).as_secs_f64();
        let sample = arrival_s - media_ts;
        if arrival == epoch {
            self.offset = sample;
        } else {
            let crept = self.offset + BASELINE_CREEP * (arrival_s - self.last_arrival).max(0.0);
            self.offset = if sample - crept > RESYNC_LATE { sample } else { crept.min(sample) };
        }
        self.last_arrival = arrival_s;
        let target = media_ts + self.offset + PLAYOUT_DELAY.as_secs_f64();
        epoch + Duration::from_secs_f64(target.max(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    fn assert_near(a: Instant, b: Instant) {
        let diff = a.max(b).duration_since(a.min(b));
        assert!(diff < Duration::from_micros(10), "{a:?} vs {b:?}");
    }

    #[test]
    fn ring_overwrites_oldest() {
        let mut ring = Ring::<u32, 3>::default();
        for i in 1..=5 {
            ring.push(i);
        }
        assert_eq!(ring.iter().collect::<Vec<_>>(), [3, 4, 5]);
        ring.pop_front();
        assert_eq!((ring.front(), ring.back()), (Some(4), Some(5)));
    }

    #[test]
    fn camera_fps_ignores_bursty_arrivals() {
        let t0 = Instant::now();
        let mut stats = RxStats::default();
        for i in 0..120u64 {
            // Frames arrive in bursts of 4 every 133 ms, but are stamped at 30 fps.
            let arrival = t0 + ms(i / 4 * 133);
            stats.on_frame(arrival, i as f64 / 30.0, 1000, false);
        }
        assert!((stats.camera_fps() - 30.0).abs() < 0.01, "{}", stats.camera_fps());
        assert!(stats.jitter_ms() > 20.0, "{}", stats.jitter_ms());
    }

    #[test]
    fn jitter_is_zero_for_steady_arrivals_and_skips_discontinuities() {
        let t0 = Instant::now();
        let mut stats = RxStats::default();
        for i in 0..60u64 {
            stats.on_frame(t0 + Duration::from_micros(i * 33_333), i as f64 / 30.0, 1000, false);
        }
        assert!(stats.jitter_ms() < 0.1);
        // A 5 s gap flagged as loss must not register as jitter.
        stats.on_frame(t0 + Duration::from_secs(7), 7.0, 1000, true);
        assert!(stats.jitter_ms() < 0.1);
        // Timestamps restarting (new camera clock) reset the fps window.
        stats.on_frame(t0 + ms(7033), 0.0, 1000, false);
        assert_eq!(stats.camera_fps(), 0.0);
    }

    #[test]
    fn kbps_is_averaged_over_the_window() {
        let t0 = Instant::now();
        let mut stats = RxStats::default();
        stats.on_frame(t0, 0.0, 500_000, false); // keyframe second
        assert_eq!(stats.tick(1.0), 4000);
        for i in 1..=4 {
            stats.on_frame(t0 + Duration::from_secs(i), i as f64, 125_000, false);
            stats.tick(1.0);
        }
        stats.on_frame(t0 + Duration::from_secs(5), 5.0, 125_000, false);
        assert_eq!(stats.tick(1.0), 1000); // keyframe bucket fell out of the window
    }

    #[test]
    fn pacer_delays_least_delayed_frame_and_evens_out_bursts() {
        let t0 = Instant::now();
        let mut pacer = Pacer::default();
        let mut due = Vec::new();
        for i in 0..30u64 {
            let arrival = t0 + ms(i / 3 * 100) + ms(if i % 3 == 0 { 0 } else { 1 });
            due.push(pacer.target(arrival, i as f64 / 30.0));
        }
        assert_near(due[0], t0 + PLAYOUT_DELAY);
        // Once the least-delayed frame (last of the first burst) set the baseline, frames are
        // due exactly one frame interval apart.
        for (i, d) in due.iter().enumerate().skip(3) {
            let step = d.duration_since(due[i - 1]).as_secs_f64();
            assert!((step - 1.0 / 30.0).abs() < 0.002, "frame {i}: {step}");
        }
    }

    #[test]
    fn pacer_never_holds_longer_than_playout_delay_and_resyncs_on_jumps() {
        let t0 = Instant::now();
        let mut pacer = Pacer::default();
        pacer.target(t0, 10.0);
        // Timestamps jump backwards by 10 s: the frame would be 10 s late; resync instead.
        let arrival = t0 + ms(33);
        assert_near(pacer.target(arrival, 0.0), arrival + PLAYOUT_DELAY);
        // A frame arriving early (less delay than the baseline) is due at arrival + delay.
        let arrival = t0 + ms(40);
        assert_near(pacer.target(arrival, 0.5), arrival + PLAYOUT_DELAY);
    }

    #[test]
    fn pacer_baseline_creeps_up() {
        let t0 = Instant::now();
        let mut pacer = Pacer::default();
        pacer.target(t0, 0.0);
        // Every later frame arrives 20 ms later than the first one's timing predicts.
        let mut late = Duration::ZERO;
        for i in 1..=100 * 30u64 {
            let arrival = t0 + Duration::from_secs_f64(i as f64 / 30.0) + ms(20);
            late = (arrival + PLAYOUT_DELAY).saturating_duration_since(pacer.target(arrival, i as f64 / 30.0));
        }
        // After 100 s the baseline moved ~20 ms (capped by the frames' own delay).
        assert!(late < ms(1), "{late:?}");
    }
}
