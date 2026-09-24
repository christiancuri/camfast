//! RTSP session setup (retina) and the one-shot probe.

use std::sync::Arc;
use std::time::Duration;

use cam_config::CameraConfig;
use futures::StreamExt;
use retina::client::{
    Credentials, Demuxed, PlayOptions, Session, SessionGroup, SessionOptions, SetupOptions, TeardownPolicy,
    Transport,
};
use retina::codec::{CodecItem, FrameFormat, ParametersRef, VideoParameters, VideoParametersCodec};
use tokio::time::{Instant, timeout, timeout_at};

use crate::ProbeInfo;
use crate::policy::{Failure, FailureKind, classify_rtsp_error};

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const TEARDOWN_WAIT: Duration = Duration::from_secs(2);

const USER_AGENT: &str = concat!("camfast/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Codec {
    H264,
    H265,
}

impl Codec {
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::H265 => "H.265",
        }
    }
}

/// Everything the decoder needs to build a `CMVideoFormatDescription`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CodecParams {
    pub codec: Codec,
    /// Raw parameter-set NAL units (with NAL header, no start code / length prefix):
    /// H.264 `[sps, pps]`, H.265 `[vps, sps, pps]`.
    pub sets: Vec<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

impl CodecParams {
    pub fn from_video(params: &VideoParameters) -> Option<Self> {
        let (codec, sets) = match params.codec_params() {
            VideoParametersCodec::H264 { sps, pps } => (Codec::H264, vec![sps.to_vec(), pps.to_vec()]),
            VideoParametersCodec::H265 { vps, sps, pps } => {
                (Codec::H265, vec![vps.to_vec(), sps.to_vec(), pps.to_vec()])
            }
            _ => return None,
        };
        let (width, height) = params.pixel_dimensions();
        Some(Self { codec, sets, width, height })
    }
}

pub(crate) fn video_params(demuxed: &Demuxed, stream: usize) -> Option<&VideoParameters> {
    match demuxed.streams().get(stream)?.parameters()? {
        ParametersRef::Video(v) => Some(v),
        _ => None,
    }
}

/// A playing session with only the video stream set up.
pub(crate) struct Connected {
    pub demuxed: Demuxed,
    pub stream: usize,
    pub codec: Codec,
    /// `a=framerate` from the SDP, if the camera sends it.
    pub sdp_fps: Option<f32>,
}

/// DESCRIBE → SETUP (video only, TCP interleaved, 4-byte length-prefixed NALs) → PLAY.
/// Must run inside the manager's runtime (dropping the session spawns its TEARDOWN there).
pub(crate) async fn connect(config: &CameraConfig, group: Arc<SessionGroup>) -> Result<Connected, Failure> {
    let url = url::Url::parse(&config.rtsp_url()).map_err(|_| Failure::fatal("Invalid address"))?;
    let creds = (!config.username.is_empty()).then(|| Credentials {
        username: config.username.clone(),
        password: config.password.expose().to_owned(),
    });
    let options = SessionOptions::default()
        .creds(creds)
        .session_group(group)
        .teardown(TeardownPolicy::Auto)
        .user_agent(USER_AGENT.to_owned());
    let mut session = Session::describe(url, options).await.map_err(rtsp_failure)?;
    let (stream, codec, sdp_fps) = pick_video(session.streams())?;
    session
        .setup(
            stream,
            SetupOptions::default()
                .transport(Transport::Tcp(Default::default()))
                .frame_format(FrameFormat::MP4),
        )
        .await
        .map_err(rtsp_failure)?;
    let playing = session.play(PlayOptions::default()).await.map_err(rtsp_failure)?;
    let demuxed = playing.demuxed().map_err(rtsp_failure)?;
    Ok(Connected { demuxed, stream, codec, sdp_fps })
}

pub(crate) fn rtsp_failure(e: retina::Error) -> Failure {
    let failure = classify_rtsp_error(e.status_code(), &e.to_string());
    if failure.kind == FailureKind::Auth {
        tracing::warn!("camera rejected the credentials");
    } else {
        tracing::debug!(error = %e, "rtsp error");
    }
    failure
}

fn pick_video(streams: &[retina::client::Stream]) -> Result<(usize, Codec, Option<f32>), Failure> {
    let mut unsupported = None;
    for (i, stream) in streams.iter().enumerate() {
        if stream.media() != "video" {
            continue;
        }
        let codec = match stream.encoding_name().to_ascii_lowercase().as_str() {
            "h264" => Codec::H264,
            "h265" => Codec::H265,
            other => {
                unsupported.get_or_insert_with(|| other.to_uppercase());
                continue;
            }
        };
        return Ok((i, codec, stream.framerate().filter(|f| *f > 0.0)));
    }
    Err(match unsupported {
        Some(name) => Failure::fatal(format!("Unsupported video codec: {name}")),
        None => Failure::fatal("Camera has no video stream"),
    })
}

/// One-shot connection test: connect, wait for the first frame, estimate fps, tear down.
/// Never retries.
pub(crate) async fn probe(config: CameraConfig) -> Result<ProbeInfo, String> {
    let group = Arc::new(SessionGroup::default().named(format!("probe-{}", config.id)));
    let result = probe_inner(&config, group.clone()).await;
    // The session was dropped inside probe_inner; give its TEARDOWN a moment.
    let _ = timeout(TEARDOWN_WAIT, group.await_teardown()).await;
    result.map_err(|f| f.reason)
}

async fn probe_inner(config: &CameraConfig, group: Arc<SessionGroup>) -> Result<ProbeInfo, Failure> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let mut conn = timeout_at(deadline, connect(config, group))
        .await
        .map_err(|_| Failure::retry("Timed out while connecting"))??;

    const SAMPLE_FRAMES: usize = 15;
    let mut first_ts: Option<f64> = None;
    let mut last_ts = 0.0;
    let mut frames = 0usize;
    let mut info: Option<ProbeInfo> = None;
    let mut sps_fps = None;
    loop {
        let item = match timeout_at(deadline, conn.demuxed.next()).await {
            Ok(Some(Ok(item))) => item,
            Ok(Some(Err(e))) if info.is_none() => return Err(rtsp_failure(e)),
            Ok(None) if info.is_none() => return Err(Failure::retry("Camera ended the stream")),
            Err(_) if info.is_none() => return Err(Failure::retry("No frames received in 10 s")),
            _ => break,
        };
        let CodecItem::VideoFrame(frame) = item else { continue };
        if frame.stream_id() != conn.stream {
            continue;
        }
        if info.is_none() {
            let Some(params) = video_params(&conn.demuxed, conn.stream) else { continue };
            let (width, height) = params.pixel_dimensions();
            sps_fps = params.frame_rate().filter(|(n, d)| *n > 0 && *d > 0).map(|(n, d)| d as f32 / n as f32);
            info = Some(ProbeInfo { codec: conn.codec.label().to_owned(), width, height, fps: None });
        }
        let ts = frame.timestamp().elapsed_secs();
        first_ts.get_or_insert(ts);
        last_ts = ts;
        frames += 1;
        if frames >= SAMPLE_FRAMES {
            break;
        }
    }
    let conn_sdp_fps = conn.sdp_fps;
    drop(conn);

    // Prefer the measured rate: some cameras (Intelbras VIP 1430) advertise a stale
    // `a=framerate` in the SDP (20 while streaming 30).
    let mut info = info.expect("loop only exits early with an error before the first frame");
    let measured = first_ts
        .filter(|_| frames >= 2)
        .map(|first| (last_ts - first) / (frames - 1) as f64)
        .filter(|dt| *dt > 0.0)
        .map(|dt| (1.0 / dt) as f32);
    tracing::debug!(?measured, sdp = ?conn_sdp_fps, sps = ?sps_fps, "probe frame rate");
    info.fps = measured.or(conn_sdp_fps).or(sps_fps).map(|f| (f * 10.0).round() / 10.0);
    Ok(info)
}
