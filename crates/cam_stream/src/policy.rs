//! Pure reconnection / visibility / error-classification policy. Time is injected so the
//! logic is unit-testable without a runtime.

use std::time::{Duration, Instant};

pub(crate) const BACKOFF_BASE: Duration = Duration::from_secs(1);
pub(crate) const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Full jitter can yield ~0 ms; never hammer a camera faster than this.
pub(crate) const BACKOFF_FLOOR: Duration = Duration::from_millis(250);
/// Streaming this long without interruption resets the backoff attempt counter.
pub(crate) const STABLE_AFTER: Duration = Duration::from_secs(60);
/// Window hidden this long: stop decoding (RTSP stays connected).
pub(crate) const PAUSE_DECODE_AFTER: Duration = Duration::from_secs(2);
/// Window hidden this long: disconnect RTSP too.
pub(crate) const DISCONNECT_AFTER: Duration = Duration::from_secs(5 * 60);

/// Exponential backoff (1 s ×2 up to 30 s) with full jitter.
#[derive(Debug, Default)]
pub(crate) struct Backoff {
    attempt: u32,
}

impl Backoff {
    /// Number of consecutive failed attempts so far.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Registers a failure and returns the delay before the next attempt. `jitter` ∈ [0, 1].
    pub fn next_delay(&mut self, jitter: f64) -> Duration {
        self.attempt = self.attempt.saturating_add(1);
        let exp = (self.attempt - 1).min(16);
        let cap = BACKOFF_BASE.saturating_mul(1 << exp).min(BACKOFF_MAX);
        cap.mul_f64(jitter.clamp(0.0, 1.0)).max(BACKOFF_FLOOR)
    }

    /// Like [`Self::next_delay`] with a random jitter.
    pub fn next_random_delay(&mut self) -> Duration {
        self.next_delay(rand::random::<f64>())
    }
}

/// What the pipelines should be doing given the window visibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Activity {
    /// Receive and decode.
    Decode,
    /// Stay connected but discard frames (no decoder, no retained buffers).
    Discard,
    /// Disconnect RTSP.
    Disconnect,
}

/// Debounced window-visibility state: a hide only takes effect after being continuous for
/// [`PAUSE_DECODE_AFTER`], so flapping (Mission Control, Spaces, minimize/restore) costs nothing.
/// Becoming visible takes effect immediately.
#[derive(Debug, Default)]
pub(crate) struct VisibilityTracker {
    hidden_since: Option<Instant>,
}

impl VisibilityTracker {
    pub fn set_visible(&mut self, visible: bool, now: Instant) {
        if visible {
            self.hidden_since = None;
        } else if self.hidden_since.is_none() {
            self.hidden_since = Some(now);
        }
    }

    pub fn activity(&self, now: Instant) -> Activity {
        match self.hidden_since {
            None => Activity::Decode,
            Some(since) => {
                let hidden = now.saturating_duration_since(since);
                if hidden >= DISCONNECT_AFTER {
                    Activity::Disconnect
                } else if hidden >= PAUSE_DECODE_AFTER {
                    Activity::Discard
                } else {
                    Activity::Decode
                }
            }
        }
    }

    /// When [`Self::activity`] will next change on its own, if ever.
    pub fn next_change(&self, now: Instant) -> Option<Instant> {
        let since = self.hidden_since?;
        [since + PAUSE_DECODE_AFTER, since + DISCONNECT_AFTER].into_iter().find(|t| *t > now)
    }
}

/// How the supervisor must react to a failed or ended session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureKind {
    /// Reconnect with backoff.
    Retry,
    /// Credentials rejected: never retry on our own (5 failures lock the camera user).
    Auth,
    /// Not fixable by retrying (unsupported codec, bad URL...).
    Fatal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Failure {
    pub kind: FailureKind,
    /// Short reason for the UI.
    pub reason: String,
}

impl Failure {
    pub fn retry(reason: impl Into<String>) -> Self {
        Self { kind: FailureKind::Retry, reason: reason.into() }
    }

    pub fn fatal(reason: impl Into<String>) -> Self {
        Self { kind: FailureKind::Fatal, reason: reason.into() }
    }

    pub fn auth() -> Self {
        Self { kind: FailureKind::Auth, reason: "Invalid username or password".into() }
    }
}

/// Maps a retina error (its RTSP status code, if any, and its message) to a policy decision.
pub(crate) fn classify_rtsp_error(status: Option<u16>, message: &str) -> Failure {
    match status {
        Some(401 | 403) => return Failure::auth(),
        Some(404) => return Failure::retry("Stream not found (404)"),
        Some(453) => return Failure::retry("Camera connection limit reached"),
        Some(code) if code >= 300 => return Failure::retry(format!("Camera responded with error {code}")),
        _ => {}
    }
    let lower = message.to_ascii_lowercase();
    let reason = if lower.starts_with("unable to connect") {
        if lower.contains("refused") {
            "Connection refused"
        } else if lower.contains("lookup") || lower.contains("nodename") || lower.contains("resolve") {
            "Address not found"
        } else if lower.contains("unreachable") {
            "Camera unreachable"
        } else {
            "No connection to the camera"
        }
    } else if lower.starts_with("error reading") || lower.starts_with("error writing") {
        "Connection lost"
    } else if lower.starts_with("timeout") {
        "Timed out"
    } else {
        "RTSP protocol error"
    };
    Failure::retry(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_cap() {
        let mut b = Backoff::default();
        let delays: Vec<u64> = (0..8).map(|_| b.next_delay(1.0).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(b.attempt(), 8);
    }

    #[test]
    fn backoff_full_jitter_with_floor() {
        let mut b = Backoff::default();
        assert_eq!(b.next_delay(0.0), BACKOFF_FLOOR);
        assert_eq!(b.next_delay(0.5), Duration::from_secs(1));
        assert_eq!(b.next_delay(0.25), Duration::from_secs(1));
        for _ in 0..100 {
            let d = b.next_random_delay();
            assert!(d >= BACKOFF_FLOOR && d <= BACKOFF_MAX);
        }
    }

    #[test]
    fn backoff_reset_and_no_overflow() {
        let mut b = Backoff::default();
        for _ in 0..10_000 {
            b.next_delay(1.0);
        }
        assert_eq!(b.next_delay(1.0), BACKOFF_MAX);
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert_eq!(b.next_delay(1.0), BACKOFF_BASE);
    }

    #[test]
    fn visibility_debounce() {
        let t0 = Instant::now();
        let mut v = VisibilityTracker::default();
        assert_eq!(v.activity(t0), Activity::Decode);
        assert_eq!(v.next_change(t0), None);

        v.set_visible(false, t0);
        assert_eq!(v.activity(t0 + Duration::from_millis(1999)), Activity::Decode);
        assert_eq!(v.next_change(t0), Some(t0 + PAUSE_DECODE_AFTER));
        // Repeated "hidden" notifications don't restart the timer.
        v.set_visible(false, t0 + Duration::from_secs(1));
        assert_eq!(v.activity(t0 + PAUSE_DECODE_AFTER), Activity::Discard);
        assert_eq!(v.next_change(t0 + PAUSE_DECODE_AFTER), Some(t0 + DISCONNECT_AFTER));
        assert_eq!(v.activity(t0 + DISCONNECT_AFTER), Activity::Disconnect);
        assert_eq!(v.next_change(t0 + DISCONNECT_AFTER), None);

        // Visible resumes immediately.
        v.set_visible(true, t0 + DISCONNECT_AFTER);
        assert_eq!(v.activity(t0 + DISCONNECT_AFTER), Activity::Decode);
    }

    #[test]
    fn visibility_flapping_never_pauses() {
        let t0 = Instant::now();
        let mut v = VisibilityTracker::default();
        for i in 0..20u64 {
            let t = t0 + Duration::from_millis(i * 1500);
            v.set_visible(i % 2 == 1, t);
            assert_eq!(v.activity(t + Duration::from_millis(1400)), Activity::Decode);
        }
    }

    #[test]
    fn classify_errors() {
        assert_eq!(classify_rtsp_error(Some(401), "x").kind, FailureKind::Auth);
        assert_eq!(classify_rtsp_error(Some(403), "x").kind, FailureKind::Auth);
        let f = classify_rtsp_error(Some(404), "x");
        assert_eq!((f.kind, f.reason.as_str()), (FailureKind::Retry, "Stream not found (404)"));
        assert_eq!(classify_rtsp_error(Some(503), "x").kind, FailureKind::Retry);
        assert_eq!(
            classify_rtsp_error(None, "Unable to connect to RTSP server: Connection refused (os error 61)").reason,
            "Connection refused"
        );
        assert_eq!(classify_rtsp_error(None, "Error reading from RTSP peer: EOF").reason, "Connection lost");
        assert_eq!(classify_rtsp_error(None, "Timeout").reason, "Timed out");
        assert_eq!(classify_rtsp_error(None, "whatever").kind, FailureKind::Retry);
    }
}
