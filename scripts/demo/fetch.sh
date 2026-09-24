#!/usr/bin/env bash
# Downloads the demo footage (freely licensed, see CREDITS.md) and turns each clip into a
# security-camera-like stream: H.264 High, no B-frames, GOP 60 at 30 fps, licence plates
# blurred and an IP-camera OSD (date/time top-right, camera name bottom-left) burned in.
#
# Usage: scripts/demo/fetch.sh [--force] [camera...]
#   --force   rebuild the streams even if they already exist in the cache.
#   camera    only build these (front_door, driveway, backyard, garage); default: all.
#
# Environment: DEMO_DEBUG_MASKS=1 draws the privacy masks as red outlines instead of blurring
# (handy when adjusting the mask tracks in common.sh).
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

FORCE=0
SELECTED=""
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
    -h|--help) sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) die "unknown option: $arg" ;;
    *) camera_info "$arg"; SELECTED="$SELECTED $arg" ;;
  esac
done

for tool in curl ffmpeg ffprobe swiftc; do
  command -v "$tool" >/dev/null || die "'$tool' not found (brew install ffmpeg; xcode-select --install for swiftc)"
done

mkdir -p "$SRC_DIR" "$WORK_DIR" "$MEDIA_DIR" "$CACHE_DIR/bin"

# The OSD is rendered with AppKit (Homebrew's ffmpeg has no drawtext/libfreetype).
OSD_BIN="$CACHE_DIR/bin/osd"
if [[ ! -x "$OSD_BIN" || "$DEMO_DIR/osd.swift" -nt "$OSD_BIN" ]]; then
  log "compiling the OSD renderer"
  swiftc -O -o "$OSD_BIN" "$DEMO_DIR/osd.swift"
fi

# Prints a filtergraph fragment that blurs the masks in CAM_MASKS, reading [$1] and
# writing [$2]. Each mask track becomes one crop -> blur -> overlay, positioned by a
# piecewise-linear expression of the frame time (one filter chain per track, not per
# keyframe: long split/overlay chains make ffmpeg crawl).
mask_filters() {
  local cur="$1" out="$2" i=0 graph="" w h x y t0 t1 effect
  while read -r w h keys; do
    [[ -z "${keys:-}" ]] && continue
    # "t,cx,cy t,cx,cy ..." -> "<x expr> <y expr> <first t> <last t>" (top-left corner).
    read -r x y t0 t1 < <(awk -v w="$w" -v h="$h" -v keys="$keys" 'BEGIN {
      n = split(keys, k, " ")
      for (i = 1; i <= n; i++) { split(k[i], f, ","); T[i] = f[1]; X[i] = f[2] - w / 2; Y[i] = f[3] - h / 2 }
      ex = X[n]; ey = Y[n]
      for (i = n - 1; i >= 1; i--) {
        r = "(t-" T[i] ")/(" T[i + 1] - T[i] ")"
        ex = "if(lt(t," T[i + 1] ")," X[i] "+(" X[i + 1] - X[i] ")*" r "," ex ")"
        ey = "if(lt(t," T[i + 1] ")," Y[i] "+(" Y[i + 1] - Y[i] ")*" r "," ey ")"
      }
      print ex, ey, T[1], T[n]
    }')
    if [[ "${DEMO_DEBUG_MASKS:-0}" == 1 ]]; then
      effect="drawbox=c=red:t=3"
    else
      effect="gblur=sigma=$(( h / 4 + 3 )):steps=3"
    fi
    graph+="[$cur]split[mb$i][ms$i];"
    graph+="[ms$i]crop=w=$w:h=$h:x='$x':y='$y',$effect[mk$i];"
    graph+="[mb$i][mk$i]overlay=x='$x':y='$y':enable='between(t,$t0,$t1)'[mv$i];"
    cur="mv$i"
    i=$((i + 1))
  done <<< "$CAM_MASKS"
  graph+="[$cur]null[$out]"
  printf '%s' "$graph"
}

build_camera() {
  local key="$1"
  camera_info "$key"
  local src="$SRC_DIR/$key.mp4" prep="$WORK_DIR/$key-prep.mp4" osd_dir="$WORK_DIR/$key-osd"
  local out="$MEDIA_DIR/$key.mp4"

  if [[ -s "$out" && "$FORCE" == 0 ]]; then
    log "$CAM_NAME: already built ($out)"
    return
  fi

  if [[ ! -s "$src" ]]; then
    log "$CAM_NAME: downloading $CAM_URL"
    curl -fL --retry 3 --progress-bar -A "camfast-demo/1.0" -o "$src.part" "$CAM_URL"
    mv "$src.part" "$src"
  fi

  # 1. Normalise: 30 fps, fill the target frame (scale + centre crop), blur plates and give
  #    it a slightly flatter, cooler "CCTV" grade with a touch of sensor noise.
  log "$CAM_NAME: pre-processing to ${CAM_W}x${CAM_H} @ 30 fps"
  local trim=() graph
  [[ -n "$CAM_TRIM" ]] && trim=(-t "$CAM_TRIM")
  graph="[0:v:0]fps=30,scale=$CAM_W:$CAM_H:force_original_aspect_ratio=increase:flags=lanczos"
  graph+=",crop=$CAM_W:$CAM_H,setsar=1,setpts=PTS-STARTPTS[base];"
  graph+="$(mask_filters base masked);"
  graph+="[masked]eq=contrast=1.04:saturation=0.88:gamma=0.98,noise=alls=3:allf=t,format=yuv420p[v]"
  ffmpeg -nostdin -hide_banner -loglevel error -stats -y ${trim[@]+"${trim[@]}"} -i "$src" \
    -filter_complex "$graph" -map '[v]' -an \
    -c:v libx264 -preset veryfast -crf 12 -g 30 -pix_fmt yuv420p "$prep"

  # 2. OSD frames, one per second of the final loop.
  local clip_len total seconds
  clip_len=$(ffprobe -v error -show_entries format=duration -of default=nw=1:nk=1 "$prep")
  total=$(awk -v d="$clip_len" -v n="$CAM_LOOPS" 'BEGIN { printf "%.3f", d * n }')
  seconds=$(awk -v t="$total" 'BEGIN { printf "%d", t + 2 }')
  log "$CAM_NAME: rendering OSD (${total}s loop)"
  rm -rf "$osd_dir"
  "$OSD_BIN" "$osd_dir" "$CAM_W" "$CAM_H" "$CAM_NAME" "$OSD_START_EPOCH" "$seconds"

  # 3. Final camera-like encode: H.264 High, no B-frames, fixed GOP 60, SPS/PPS repeated on
  #    every IDR (as IP cameras do) and a capped bitrate.
  log "$CAM_NAME: encoding final stream"
  ffmpeg -nostdin -hide_banner -loglevel error -stats -y \
    -stream_loop $((CAM_LOOPS - 1)) -i "$prep" \
    -framerate 1 -i "$osd_dir/osd_%04d.png" \
    -filter_complex "[0:v][1:v]overlay=0:0:eof_action=repeat,format=yuv420p[v]" \
    -map '[v]' -an -t "$total" -r 30 \
    -c:v libx264 -profile:v high -preset medium -crf 21 \
    -maxrate "${CAM_MAXRATE}k" -bufsize "$((CAM_MAXRATE * 2))k" \
    -bf 0 -g 60 -keyint_min 60 -sc_threshold 0 -x264-params repeat-headers=1 \
    -pix_fmt yuv420p -movflags +faststart "$out.part.mp4"
  mv "$out.part.mp4" "$out"
  rm -f "$prep"
  log "$CAM_NAME: done -> $out"
}

for key in ${SELECTED:-$CAMERAS}; do
  build_camera "$key"
done

log "all streams ready in $MEDIA_DIR"
log "next: $DEMO_DIR/run.sh"
