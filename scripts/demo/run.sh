#!/usr/bin/env bash
# Starts the four demo cameras on localhost: one mediamtx RTSP server per camera
# (127.0.0.1:8554-8557, path cam/realmonitor, like a Dahua/Intelbras camera), each fed by an
# ffmpeg publisher that loops the stream built by fetch.sh. Everything runs in the background;
# stop it with scripts/demo/stop.sh.
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

for tool in mediamtx ffmpeg ffprobe; do
  command -v "$tool" >/dev/null || die "'$tool' not found (brew install mediamtx ffmpeg)"
done

mkdir -p "$RUN_DIR"

is_running() { # PIDFILE
  [[ -s "$1" ]] && kill -0 "$(cat "$1")" 2>/dev/null
}

start_camera() {
  local key="$1"
  camera_info "$key"
  local media="$MEDIA_DIR/$key.mp4" cfg="$RUN_DIR/$key.yml" pidfile="$RUN_DIR/$key.pid"
  local logfile="$RUN_DIR/$key.log"

  [[ -s "$media" ]] || die "$media is missing; run $DEMO_DIR/fetch.sh first"

  if is_running "$pidfile"; then
    log "$CAM_NAME: already running (pid $(cat "$pidfile"))"
    return
  fi
  if lsof -nP -iTCP:"$CAM_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    die "port $CAM_PORT is already in use (another RTSP server?); run $DEMO_DIR/stop.sh or free it"
  fi

  # Only plain RTSP over TCP on loopback: every other protocol, API and UDP listener is off so
  # the four instances never fight over ports. mediamtx supervises the publisher (runOnInit)
  # and restarts it if it exits; stopping mediamtx stops the publisher too.
  cat > "$cfg" <<EOF
logLevel: warn
logDestinations: [stdout]
api: false
metrics: false
pprof: false
playback: false
rtsp: true
rtspTransports: [tcp]
rtspEncryption: "no"
rtspAddress: $RTSP_HOST:$CAM_PORT
rtmp: false
hls: false
webrtc: false
srt: false
moq: false
paths:
  $RTSP_PATH:
    source: publisher
    runOnInit: >-
      ffmpeg -hide_banner -loglevel error -nostdin -re -stream_loop -1 -i "$media"
      -map 0:v -c copy -f rtsp -rtsp_transport tcp rtsp://$RTSP_HOST:$CAM_PORT/$RTSP_PATH
    runOnInitRestart: true
EOF

  nohup mediamtx "$cfg" > "$logfile" 2>&1 < /dev/null &
  echo $! > "$pidfile"
  log "$CAM_NAME: mediamtx started on $RTSP_HOST:$CAM_PORT (pid $!, log $logfile)"
}

wait_ready() {
  local key="$1" url deadline info
  camera_info "$key"
  url="$(rtsp_url "$CAM_PORT")"
  deadline=$((SECONDS + 30))
  while true; do
    if info=$(ffprobe -v error -rtsp_transport tcp -timeout 3000000 -select_streams v:0 \
        -show_entries stream=codec_name,width,height,avg_frame_rate -of csv=p=0 "$url" 2>/dev/null) \
        && [[ -n "$info" ]]; then
      log "$CAM_NAME: ready at $url ($info)"
      return
    fi
    if (( SECONDS >= deadline )); then
      die "$CAM_NAME did not come up within 30 s; see $RUN_DIR/$key.log"
    fi
    if ! is_running "$RUN_DIR/$key.pid"; then
      die "$CAM_NAME: mediamtx exited; see $RUN_DIR/$key.log"
    fi
    sleep 0.5
  done
}

for key in $CAMERAS; do start_camera "$key"; done
for key in $CAMERAS; do wait_ready "$key"; done

# CamFast writes its config back (window geometry, edits), so hand it a fresh copy instead of
# the tracked scripts/demo/config.toml.
cp "$DEMO_DIR/config.toml" "$RUN_DIR/config.toml"
chmod 600 "$RUN_DIR/config.toml"
REPO_DIR="$(cd "$DEMO_DIR/../.." && pwd)"
log "all 4 demo cameras are live. Start CamFast with the demo config:"
printf '\n  cd %q && CAMFAST_CONFIG=%q cargo run --release\n\n' "$REPO_DIR" "$RUN_DIR/config.toml"
log "stop them with: $DEMO_DIR/stop.sh"
