//! Headless end-to-end check of the streaming pipeline against one camera.
//!
//! ```sh
//! CAM_USER=demo CAM_PASS=... cargo run -p cam_stream --example probe -- 192.168.1.150 20 [--sub] [--smooth]
//! ```
//!
//! Runs the one-shot `probe()` first, then the full `StreamManager` pipeline for N seconds,
//! printing status transitions and per-second stats, verifying every sampled frame is an
//! IOSurface-backed `'420f'` buffer, and reporting the process CPU time at the end.
//!
//! Every second it also prints the standard deviation of the intervals between published
//! frames (`iv_sd`, what the eye sees as stutter) and the arrival → publish delay. `--smooth`
//! turns on playout smoothing.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cam_config::{CameraConfig, Secret, StreamKind};
use cam_stream::{PIXEL_FORMAT_420F, StreamManager, fourcc_str, is_iosurface_backed};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,retina=warn".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let mut host = None;
    let mut seconds = 20u64;
    let mut sub = false;
    let mut smooth = false;
    let mut port = 554u16;
    for arg in args.by_ref() {
        match arg.as_str() {
            "--sub" => sub = true,
            "--smooth" => smooth = true,
            a if a.starts_with("--port=") => port = a["--port=".len()..].parse()?,
            a if host.is_none() => host = Some(a.to_owned()),
            a => seconds = a.parse()?,
        }
    }
    let host = host.ok_or_else(|| anyhow::anyhow!("usage: probe <host> [seconds] [--sub] [--smooth] [--port=N]"))?;

    let mut camera = CameraConfig::new("probe", host);
    camera.port = port;
    camera.stream = if sub { StreamKind::Sub } else { StreamKind::Main };
    camera.username = std::env::var("CAM_USER").unwrap_or_default();
    camera.password = Secret::new(std::env::var("CAM_PASS").unwrap_or_default());

    let mut manager = StreamManager::new()?;

    // The probe future is awaited from a non-tokio executor, like GPUI does.
    let started = Instant::now();
    match futures::executor::block_on(manager.probe(camera.clone())) {
        Ok(info) => println!("probe: {info:?} ({:.2} s)", started.elapsed().as_secs_f32()),
        Err(e) => {
            println!("probe failed: {e}");
            if e.contains("password") {
                return Ok(()); // never hammer a camera with bad credentials
            }
        }
    }

    manager.set_smooth_playback(smooth);
    manager.sync(std::slice::from_ref(&camera));
    let shared = manager.stream(camera.id).expect("stream registered");
    // (published at, arrived at) of every published frame since the last report.
    let publishes: Arc<Mutex<Vec<(Instant, Instant)>>> = Arc::new(Mutex::new(Vec::with_capacity(1024)));
    let sink = publishes.clone();
    shared.set_publish_observer(move |arrival| sink.lock().unwrap().push((Instant::now(), arrival)));
    let mut last_publish: Option<Instant> = None;
    let mut summary = Summary::default();
    let wall = Instant::now();
    let mut last_status = None;
    let mut next_report = wall + Duration::from_secs(1);
    let mut bad_frames = 0u64;
    let mut checked_frames = 0u64;
    while wall.elapsed() < Duration::from_secs(seconds) {
        let status = shared.status();
        if last_status.as_ref() != Some(&status) {
            println!("[{:>6.2}s] status: {status:?}", wall.elapsed().as_secs_f32());
            last_status = Some(status);
        }
        if Instant::now() >= next_report {
            next_report += Duration::from_secs(1);
            let stats = shared.stats();
            let frame = shared.latest_frame();
            let (format, iosurface) = match &frame {
                Some(f) => {
                    let format = f.get_pixel_format();
                    let iosurface = is_iosurface_backed(f);
                    checked_frames += 1;
                    if format != PIXEL_FORMAT_420F || !iosurface {
                        bad_frames += 1;
                    }
                    (fourcc_str(format), iosurface)
                }
                None => ("-".to_owned(), false),
            };
            let batch = std::mem::take(&mut *publishes.lock().unwrap());
            let mut intervals = Vec::with_capacity(batch.len());
            let mut delays = Vec::with_capacity(batch.len());
            for (published, arrival) in batch {
                if let Some(prev) = last_publish.replace(published) {
                    intervals.push(ms(published - prev));
                }
                delays.push(ms(published.saturating_duration_since(arrival)));
            }
            let (_, iv_sd) = mean_sd(&intervals);
            let (delay, _) = mean_sd(&delays);
            if wall.elapsed() > WARMUP {
                summary.add(&stats, iv_sd, &intervals, &delays);
            }
            println!(
                "[{:>6.2}s] cam_fps={:>4.1} fps={:>4.1} jitter={:>4.1}ms iv_sd={:>5.1}ms delay={:>5.1}ms kbps={:>5} {}x{} {} fmt={} iosurface={} published={} dropped={} reconnects={} err={:?}",
                wall.elapsed().as_secs_f32(),
                stats.camera_fps,
                stats.fps,
                stats.jitter_ms,
                iv_sd,
                delay,
                stats.kbps,
                stats.width,
                stats.height,
                stats.codec,
                format,
                iosurface,
                shared.frames_published(),
                stats.dropped_frames,
                stats.reconnects,
                stats.last_error,
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let elapsed = wall.elapsed();
    let published = shared.frames_published();
    drop(shared);
    let shutdown = Instant::now();
    drop(manager);
    println!("shutdown took {:.2} s", shutdown.elapsed().as_secs_f32());

    let cpu = cpu_time();
    let total = started.elapsed();
    println!(
        "frames published: {published} ({:.1} avg fps over {:.1} s); sampled {checked_frames} frames, {bad_frames} not IOSurface '420f'",
        published as f64 / elapsed.as_secs_f64(),
        elapsed.as_secs_f64(),
    );
    println!(
        "process CPU time: {:.2} s over {:.1} s wall = {:.1}% of one core",
        cpu.as_secs_f64(),
        total.as_secs_f64(),
        100.0 * cpu.as_secs_f64() / total.as_secs_f64(),
    );
    summary.print(smooth);
    if bad_frames > 0 {
        anyhow::bail!("published frames that GPUI cannot render");
    }
    Ok(())
}

/// Seconds excluded from the summary (connection + initial GOP burst).
const WARMUP: Duration = Duration::from_secs(5);

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn mean_sd(values: &[f64]) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
    (mean, var.sqrt())
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// Steady-state totals over the run (after [`WARMUP`]).
#[derive(Default)]
struct Summary {
    camera_fps: Vec<f64>,
    fps: Vec<f64>,
    jitter: Vec<f64>,
    iv_sd: Vec<f64>,
    intervals: Vec<f64>,
    delays: Vec<f64>,
}

impl Summary {
    fn add(&mut self, stats: &cam_stream::StatsSnapshot, iv_sd: f64, intervals: &[f64], delays: &[f64]) {
        self.camera_fps.push(stats.camera_fps as f64);
        self.fps.push(stats.fps as f64);
        self.jitter.push(stats.jitter_ms as f64);
        self.iv_sd.push(iv_sd);
        self.intervals.extend_from_slice(intervals);
        self.delays.extend_from_slice(delays);
    }

    fn print(&mut self, smooth: bool) {
        let range = |v: &[f64]| {
            let (mean, _) = mean_sd(v);
            let min = v.iter().copied().fold(f64::INFINITY, f64::min);
            let max = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            format!("{mean:.1} [{min:.1}–{max:.1}]")
        };
        let (_, iv_sd_all) = mean_sd(&self.intervals);
        self.delays.sort_by(f64::total_cmp);
        let (delay_mean, _) = mean_sd(&self.delays);
        println!("summary (smooth={smooth}, {} s after warmup):", self.fps.len());
        println!("  camera_fps {}", range(&self.camera_fps));
        println!("  fps        {}", range(&self.fps));
        println!("  jitter_ms  {}", range(&self.jitter));
        println!("  iv_sd_ms   per-second {}; all intervals {iv_sd_all:.1}", range(&self.iv_sd));
        println!(
            "  arrival→publish ms: mean {delay_mean:.1}, p50 {:.1}, p95 {:.1}, max {:.1}",
            percentile(&self.delays, 0.5),
            percentile(&self.delays, 0.95),
            percentile(&self.delays, 1.0),
        );
    }
}

/// User + system CPU time of this process.
fn cpu_time() -> Duration {
    // SAFETY: getrusage fills the zeroed struct.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(usage.ru_utime) + tv(usage.ru_stime)
}
