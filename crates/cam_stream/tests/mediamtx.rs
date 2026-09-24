//! End-to-end tests against a local mediamtx server fed by ffmpeg (simulated cameras with
//! fault injection). Ignored by default because they need external tools and take ~2 min:
//!
//! ```sh
//! brew install mediamtx ffmpeg
//! cargo test -p cam_stream --test mediamtx -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! `MEDIAMTX` / `FFMPEG` env vars override the binary paths.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use cam_config::{CameraConfig, Secret};
use cam_stream::{PIXEL_FORMAT_420F, StreamManager, StreamShared, StreamStatus, is_iosurface_backed};

/// Tests share the machine's GPU/CPU and ffmpeg encoders; run them one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,retina=warn".into()),
        )
        .with_test_writer()
        .try_init();
}

fn tool(env: &str, default: &str) -> PathBuf {
    std::env::var_os(env).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(default))
}

/// Kills the child process on drop.
struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Server {
    _proc: Proc,
    _dir: PathBuf,
}

/// Starts mediamtx on `port`. With `reader`, reading requires those credentials.
fn mediamtx(port: u16, reader: Option<(&str, &str)>) -> Server {
    let dir = std::env::temp_dir().join(format!("cam_stream_mediamtx_{port}"));
    std::fs::create_dir_all(&dir).unwrap();
    let users = match reader {
        None => "  - user: any\n    permissions:\n      - action: publish\n      - action: read\n".to_owned(),
        Some((user, pass)) => format!(
            "  - user: any\n    permissions:\n      - action: publish\n  - user: {user}\n    pass: {pass}\n    permissions:\n      - action: read\n"
        ),
    };
    let config = format!(
        "logLevel: warn\nrtsp: true\nrtspTransports: [tcp]\nrtspAddress: 127.0.0.1:{port}\n\
         rtmp: false\nhls: false\nwebrtc: false\nsrt: false\napi: false\nmetrics: false\n\
         pprof: false\nplayback: false\nauthInternalUsers:\n{users}paths:\n  all_others:\n"
    );
    let path = dir.join("mediamtx.yml");
    std::fs::write(&path, config).unwrap();
    let child = Command::new(tool("MEDIAMTX", "/opt/homebrew/opt/mediamtx/bin/mediamtx"))
        .arg(&path)
        .stdout(Stdio::null())
        .spawn()
        .expect("mediamtx not found (brew install mediamtx)");
    wait_for_port(port);
    Server { _proc: Proc(child), _dir: dir }
}

fn wait_for_port(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "mediamtx did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Publishes an ffmpeg test pattern at the Intelbras path (`/cam/realmonitor`), GOP 60, no B-frames.
fn publish(port: u16, encoder: &str, size: &str) -> Proc {
    let mut cmd = Command::new(tool("FFMPEG", "ffmpeg"));
    cmd.args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size={size}:rate=30"))
        .args(["-pix_fmt", "yuv420p", "-c:v", encoder, "-g", "60", "-bf", "0"]);
    match encoder {
        "libx264" => {
            cmd.args(["-preset", "ultrafast", "-tune", "zerolatency"]);
        }
        "libx265" => {
            cmd.args(["-preset", "ultrafast", "-tune", "zerolatency", "-x265-params", "bframes=0:keyint=60:log-level=error"]);
        }
        _ => {}
    }
    cmd.args(["-f", "rtsp", "-rtsp_transport", "tcp"])
        .arg(format!("rtsp://127.0.0.1:{port}/cam/realmonitor"))
        .stdin(Stdio::null());
    Proc(cmd.spawn().expect("ffmpeg not found"))
}

fn camera(port: u16) -> CameraConfig {
    let mut c = CameraConfig::new("sim", "127.0.0.1");
    c.port = port;
    c
}

fn wait_until(shared: &StreamShared, timeout: Duration, what: &str, pred: impl Fn(&StreamStatus) -> bool) {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    loop {
        let status = shared.status();
        if last.as_ref() != Some(&status) {
            eprintln!("  status: {status:?}");
            last = Some(status.clone());
        }
        if pred(&status) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}; last status {status:?}, stats {:?}", shared.stats());
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_live(shared: &StreamShared, timeout: Duration) {
    wait_until(shared, timeout, "Live", |s| *s == StreamStatus::Live);
}

/// Waits for `frames` more frames and checks they are renderable by GPUI.
fn assert_decoding(shared: &StreamShared, frames: u64, width: usize, height: usize) {
    let start = shared.frames_published();
    let t0 = Instant::now();
    while shared.frames_published() < start + frames {
        assert!(t0.elapsed() < Duration::from_secs(20), "stalled at {} frames", shared.frames_published() - start);
        std::thread::sleep(Duration::from_millis(20));
    }
    let fps = frames as f64 / t0.elapsed().as_secs_f64();
    let frame = shared.latest_frame().expect("frame published");
    assert_eq!(frame.get_pixel_format(), PIXEL_FORMAT_420F);
    assert!(is_iosurface_backed(&frame));
    assert_eq!((frame.get_width(), frame.get_height()), (width, height));
    assert!(fps > 20.0, "only {fps:.1} fps");
    eprintln!("  decoded {frames} frames at {fps:.1} fps, stats {:?}", shared.stats());
}

fn start(port: u16) -> (StreamManager, Arc<StreamShared>, CameraConfig) {
    let mut manager = StreamManager::new().unwrap();
    let cam = camera(port);
    manager.sync(std::slice::from_ref(&cam));
    let shared = manager.stream(cam.id).unwrap();
    (manager, shared, cam)
}

#[test]
#[ignore = "needs mediamtx + ffmpeg"]
fn decodes_h264_1440p_and_probe() {
    let _serial = serial();
    init_tracing();
    let port = 18554;
    let _server = mediamtx(port, None);
    let _publisher = publish(port, "libx264", "2560x1440");
    std::thread::sleep(Duration::from_secs(2));

    let (manager, shared, cam) = start(port);
    wait_live(&shared, Duration::from_secs(15));
    assert_decoding(&shared, 90, 2560, 1440);
    let stats = shared.stats();
    assert_eq!((stats.width, stats.height, stats.codec.as_str()), (2560, 1440, "H.264"));

    let info = futures::executor::block_on(manager.probe(cam)).unwrap();
    assert_eq!((info.codec.as_str(), info.width, info.height), ("H.264", 2560, 1440));
    let fps = info.fps.unwrap();
    assert!((fps - 30.0).abs() < 2.0, "probe fps {fps}");
}

#[test]
#[ignore = "needs mediamtx + ffmpeg with libx265"]
fn decodes_h265() {
    let _serial = serial();
    init_tracing();
    let port = 18555;
    let _server = mediamtx(port, None);
    let _publisher = publish(port, "libx265", "1920x1080");
    std::thread::sleep(Duration::from_secs(3));

    let (_manager, shared, _) = start(port);
    wait_live(&shared, Duration::from_secs(20));
    assert_decoding(&shared, 60, 1920, 1080);
    assert_eq!(shared.stats().codec, "H.265");
}

#[test]
#[ignore = "needs mediamtx + ffmpeg"]
fn reconnects_after_server_killed() {
    let _serial = serial();
    init_tracing();
    let port = 18556;
    let server = mediamtx(port, None);
    let publisher = publish(port, "libx264", "1280x720");
    std::thread::sleep(Duration::from_secs(2));

    let (_manager, shared, _) = start(port);
    wait_live(&shared, Duration::from_secs(15));
    assert_decoding(&shared, 30, 1280, 720);

    eprintln!("killing mediamtx");
    drop(publisher);
    drop(server);
    wait_until(&shared, Duration::from_secs(15), "Reconnecting", |s| matches!(s, StreamStatus::Reconnecting { .. }));
    std::thread::sleep(Duration::from_secs(4));
    // Last good frame stays on screen while reconnecting.
    assert!(shared.latest_frame().is_some());

    eprintln!("restarting mediamtx");
    let _server = mediamtx(port, None);
    let _publisher = publish(port, "libx264", "1280x720");
    wait_live(&shared, Duration::from_secs(45));
    assert_decoding(&shared, 30, 1280, 720);
    assert!(shared.stats().reconnects >= 1);
}

#[test]
#[ignore = "needs mediamtx + ffmpeg"]
fn missing_path_backs_off_then_recovers() {
    let _serial = serial();
    init_tracing();
    let port = 18557;
    let _server = mediamtx(port, None);

    // Nothing published yet: 404 → backoff with growing attempts.
    let (manager, shared, cam) = start(port);
    wait_until(&shared, Duration::from_secs(20), "attempt >= 3", |s| {
        matches!(s, StreamStatus::Reconnecting { attempt, reason, .. } if *attempt >= 3 && reason.contains("404"))
    });

    let publisher = publish(port, "libx264", "1280x720");
    std::thread::sleep(Duration::from_secs(1));
    manager.retry_now(cam.id);
    wait_live(&shared, Duration::from_secs(20));
    assert_decoding(&shared, 30, 1280, 720);

    // Stream path removed mid-stream: back to backoff, then recovers when it returns.
    eprintln!("stopping publisher");
    drop(publisher);
    wait_until(&shared, Duration::from_secs(20), "Reconnecting", |s| matches!(s, StreamStatus::Reconnecting { .. }));
    let _publisher = publish(port, "libx264", "1280x720");
    wait_live(&shared, Duration::from_secs(45));
}

#[test]
#[ignore = "needs mediamtx + ffmpeg"]
fn auth_failure_is_not_retried() {
    let _serial = serial();
    init_tracing();
    let port = 18558;
    let _server = mediamtx(port, Some(("viewer", "right")));
    let _publisher = publish(port, "libx264", "1280x720");
    std::thread::sleep(Duration::from_secs(2));

    let mut manager = StreamManager::new().unwrap();
    let mut cam = camera(port);
    cam.username = "viewer".into();
    cam.password = Secret::new("wrong");
    manager.sync(std::slice::from_ref(&cam));
    let shared = manager.stream(cam.id).unwrap();
    wait_until(&shared, Duration::from_secs(15), "AuthFailed", |s| *s == StreamStatus::AuthFailed);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(shared.status(), StreamStatus::AuthFailed);
    assert_eq!(shared.stats().reconnects, 0);

    // Fixing the password (config change) restarts it, keeping the same shared handle.
    cam.password = Secret::new("right");
    manager.sync(std::slice::from_ref(&cam));
    assert!(Arc::ptr_eq(&shared, &manager.stream(cam.id).unwrap()));
    wait_live(&shared, Duration::from_secs(15));
}

#[test]
#[ignore = "needs mediamtx + ffmpeg"]
fn visibility_and_sleep() {
    let _serial = serial();
    init_tracing();
    let port = 18559;
    let _server = mediamtx(port, None);
    let _publisher = publish(port, "libx264", "1280x720");
    std::thread::sleep(Duration::from_secs(2));

    let (manager, shared, _) = start(port);
    wait_live(&shared, Duration::from_secs(15));

    // Brief hide (< 2 s) changes nothing.
    manager.set_window_visible(false);
    std::thread::sleep(Duration::from_millis(1000));
    manager.set_window_visible(true);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(shared.status(), StreamStatus::Live);

    // Hidden > 2 s: paused, frame released, no decoding.
    manager.set_window_visible(false);
    wait_until(&shared, Duration::from_secs(5), "Paused", |s| *s == StreamStatus::Paused);
    std::thread::sleep(Duration::from_millis(300));
    assert!(shared.latest_frame().is_none());
    let published = shared.frames_published();
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(shared.frames_published(), published);

    // Visible: resumes from the next keyframe (GOP 2 s) without reconnecting.
    let reconnects = shared.stats().reconnects;
    manager.set_window_visible(true);
    wait_live(&shared, Duration::from_secs(5));
    assert_eq!(shared.stats().reconnects, reconnects);

    // System sleep / wake.
    manager.suspend();
    wait_until(&shared, Duration::from_secs(5), "Paused", |s| *s == StreamStatus::Paused);
    manager.resume();
    wait_live(&shared, Duration::from_secs(10));
}

#[test]
#[ignore = "needs mediamtx + ffmpeg"]
fn remove_and_readd_while_tearing_down() {
    let _serial = serial();
    init_tracing();
    let port = 18560;
    let _server = mediamtx(port, None);
    let _publisher = publish(port, "libx264", "1280x720");
    std::thread::sleep(Duration::from_secs(2));

    let (mut manager, shared, cam) = start(port);
    wait_live(&shared, Duration::from_secs(15));

    // Removed and immediately placed back: the new supervisor waits for the old one's TEARDOWN.
    manager.sync(&[]);
    assert!(manager.stream(cam.id).is_none());
    manager.sync(std::slice::from_ref(&cam));
    let shared = manager.stream(cam.id).unwrap();
    wait_live(&shared, Duration::from_secs(15));
    assert_decoding(&shared, 30, 1280, 720);
    assert_eq!(shared.stats().reconnects, 0);
}

/// Mean arrival → publish delay over the next `window`.
fn mean_delay(delays: &Mutex<Vec<Duration>>, window: Duration) -> Duration {
    delays.lock().unwrap().clear();
    std::thread::sleep(window);
    let delays = delays.lock().unwrap();
    assert!(!delays.is_empty(), "no frames published");
    delays.iter().sum::<Duration>() / delays.len() as u32
}

#[test]
#[ignore = "needs mediamtx + ffmpeg"]
fn smoothing_toggles_live() {
    let _serial = serial();
    init_tracing();
    let port = 18561;
    let _server = mediamtx(port, None);
    let _publisher = publish(port, "libx264", "1280x720");
    std::thread::sleep(Duration::from_secs(2));

    // Enabled before the stream exists: applies to streams started later.
    let mut manager = StreamManager::new().unwrap();
    manager.set_smooth_playback(true);
    let cam = camera(port);
    manager.sync(std::slice::from_ref(&cam));
    let shared = manager.stream(cam.id).unwrap();
    let delays = Arc::new(Mutex::new(Vec::new()));
    let sink = delays.clone();
    assert!(shared.set_publish_observer(move |arrival| sink.lock().unwrap().push(arrival.elapsed())));
    wait_live(&shared, Duration::from_secs(15));
    assert_decoding(&shared, 60, 1280, 720);

    std::thread::sleep(Duration::from_secs(2));
    let stats = shared.stats();
    assert!((stats.camera_fps - 30.0).abs() < 1.0, "camera fps {}", stats.camera_fps);
    assert!(stats.jitter_ms < 20.0, "jitter {}", stats.jitter_ms);
    assert!(stats.kbps > 0);

    let smoothed = mean_delay(&delays, Duration::from_secs(2));
    eprintln!("  smoothing on: mean delay {smoothed:?}");
    assert!(smoothed > Duration::from_millis(90) && smoothed < Duration::from_millis(250), "{smoothed:?}");

    // Live toggle, no reconnect.
    manager.set_smooth_playback(false);
    std::thread::sleep(Duration::from_millis(500));
    let direct = mean_delay(&delays, Duration::from_secs(2));
    eprintln!("  smoothing off: mean delay {direct:?}");
    assert!(direct < Duration::from_millis(60), "{direct:?}");
    assert_decoding(&shared, 30, 1280, 720);

    manager.set_smooth_playback(true);
    std::thread::sleep(Duration::from_millis(500));
    let smoothed = mean_delay(&delays, Duration::from_secs(2));
    eprintln!("  smoothing on again: mean delay {smoothed:?}");
    assert!(smoothed > Duration::from_millis(90), "{smoothed:?}");
    assert_eq!(shared.stats().reconnects, 0);
    assert_eq!(shared.status(), StreamStatus::Live);
}
