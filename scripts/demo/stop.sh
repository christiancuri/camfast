#!/usr/bin/env bash
# Stops the demo cameras started by run.sh (mediamtx instances and their ffmpeg publishers).
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

stopped=0
for key in $CAMERAS; do
  camera_info "$key"
  pidfile="$RUN_DIR/$key.pid"
  [[ -s "$pidfile" ]] || continue
  pid="$(cat "$pidfile")"
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.1
    done
    if kill -0 "$pid" 2>/dev/null; then
      kill -KILL "$pid" 2>/dev/null || true
    fi
    log "$CAM_NAME: stopped (pid $pid)"
    stopped=$((stopped + 1))
  fi
  rm -f "$pidfile"
done

# mediamtx signals its ffmpeg publishers on shutdown; make sure they are gone (they are the
# only processes reading from MEDIA_DIR), escalating to SIGKILL after ~3 s.
publisher="-i $MEDIA_DIR/"
if pgrep -f -- "$publisher" >/dev/null 2>&1; then
  pkill -TERM -f -- "$publisher" 2>/dev/null || true
  for _ in $(seq 1 30); do
    pgrep -f -- "$publisher" >/dev/null 2>&1 || break
    sleep 0.1
  done
  pkill -KILL -f -- "$publisher" 2>/dev/null || true
  log "stopped ffmpeg publishers"
fi

if (( stopped == 0 )); then
  log "no demo cameras were running"
else
  log "all demo cameras stopped"
fi
