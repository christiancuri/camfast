# Demo cameras

Four fake IP cameras on localhost for trying CamFast (and taking screenshots) without real
cameras. Each one is a [mediamtx](https://github.com/bluenviron/mediamtx) RTSP server that
answers at `rtsp://127.0.0.1:<port>/cam/realmonitor?channel=1&subtype=0`, like a
Dahua/Intelbras camera, and loops freely licensed stock footage (see [CREDITS.md](CREDITS.md))
re-encoded to look like a camera stream: H.264 High, 30 fps, no B-frames, GOP 60, a date/time
and camera-name OSD, licence plates blurred.

| Mosaic position | Camera     | Port | Resolution |
|-----------------|------------|------|------------|
| top-left        | Front Door | 8554 | 2560×1440  |
| top-right       | Driveway   | 8555 | 1920×1080  |
| bottom-left     | Backyard   | 8556 | 1920×1080  |
| bottom-right    | Garage     | 8557 | 2560×1440  |

## Requirements

macOS with `brew install ffmpeg mediamtx` and the Xcode command line tools (`swiftc`, used to
render the OSD because Homebrew's ffmpeg has no `drawtext`).

## Usage

```sh
scripts/demo/fetch.sh && scripts/demo/run.sh
CAMFAST_CONFIG=$PWD/scripts/demo/config.toml cargo run --release
scripts/demo/stop.sh
```

- `fetch.sh` downloads the clips (~250 MB) and builds the streams in
  `${XDG_CACHE_HOME:-~/.cache}/camfast-demo/` (about a minute and a half on Apple silicon). It
  skips streams that already exist; `--force` rebuilds them, and camera keys (`front_door`,
  `driveway`, `backyard`, `garage`) limit it to those cameras.
- `run.sh` starts the four servers in the background, waits until every stream answers and
  prints the command to launch CamFast. It also copies `config.toml` to
  `~/.cache/camfast-demo/run/config.toml` and suggests that copy: CamFast writes its config back
  (window position, edits), which would otherwise modify the tracked file.
- `stop.sh` stops the servers and their ffmpeg publishers.

`config.toml` has the four cameras (no credentials needed) with fixed ids, all four mosaic
slots assigned and `smooth_playback = false`. Logs are in `~/.cache/camfast-demo/run/`.

The privacy-mask tracks for the licence plates live in `common.sh`; run
`DEMO_DEBUG_MASKS=1 scripts/demo/fetch.sh --force <camera>` to draw them as red outlines when
adjusting them.
