# Shared settings for the CamFast demo cameras. Sourced by fetch.sh, run.sh and stop.sh.
# Compatible with the stock macOS bash 3.2 (no associative arrays).

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/camfast-demo"
SRC_DIR="$CACHE_DIR/source"   # original downloads
WORK_DIR="$CACHE_DIR/work"    # intermediate files (OSD frames, pre-processed video)
MEDIA_DIR="$CACHE_DIR/media"  # final camera-like streams served over RTSP
RUN_DIR="$CACHE_DIR/run"      # mediamtx configs, logs and PIDs

RTSP_HOST=127.0.0.1
RTSP_PATH=cam/realmonitor

# Fixed, plausible OSD start time: 21-09-2026 17:32:17 (rendered as UTC).
OSD_START_EPOCH=1790011937

# Mosaic order: top-left, top-right, bottom-left, bottom-right.
CAMERAS="front_door driveway backyard garage"

log() { printf '\033[1m[camfast-demo]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[camfast-demo]\033[0m %s\n' "$*" >&2; exit 1; }

# camera_info KEY sets, for that camera:
#   CAM_NAME      OSD / CamFast name
#   CAM_PORT      RTSP port of its mediamtx instance
#   CAM_W CAM_H   output resolution
#   CAM_URL       direct download URL of the source clip
#   CAM_TRIM      seconds of the source clip to keep (empty = all)
#   CAM_LOOPS     how many times the clip is repeated inside one stream loop (to reach 20-40 s)
#   CAM_MAXRATE   bitrate cap in kbit/s, like a camera's CBR/VBR ceiling
#   CAM_MASKS     privacy masks (licence plates), one track per line: "w h t,cx,cy t,cx,cy ..."
#                 = a w x h blurred box (output pixels) centred on (cx,cy), moving linearly
#                 between the keyframes (t in seconds); only shown between the first and last t.
camera_info() {
  CAM_TRIM=""
  CAM_LOOPS=1
  CAM_MASKS=""
  case "$1" in
    front_door)
      CAM_NAME="Front Door"; CAM_PORT=8554; CAM_W=2560; CAM_H=1440; CAM_MAXRATE=8000
      CAM_URL="https://videos.pexels.com/video-files/6034753/6034753-uhd_4096_2160_24fps.mp4"
      CAM_LOOPS=2
      ;;
    driveway)
      CAM_NAME="Driveway"; CAM_PORT=8555; CAM_W=1920; CAM_H=1080; CAM_MAXRATE=5000
      CAM_URL="https://videos.pexels.com/video-files/29635453/12751174_1920_1080_24fps.mp4"
      CAM_MASKS="
110 46 0,985,566 2,945,568 4,890,570 5,863,576 6,836,573 7,801,574 8,761,571 9,719,570 10,689,570 11,649,571
160 60 11,649,571 12,620,571 13,585,571 14,572,573 15,572,579 16,588,581 17,615,581 18,636,581 18.5,668,584 19,679,581 20.5,695,581
120 50 11.5,945,595 20.5,945,595"
      ;;
    backyard)
      CAM_NAME="Backyard"; CAM_PORT=8556; CAM_W=1920; CAM_H=1080; CAM_MAXRATE=5000
      CAM_URL="https://videos.pexels.com/video-files/3852659/3852659-uhd_3840_2160_30fps.mp4"
      CAM_LOOPS=2
      ;;
    garage)
      CAM_NAME="Garage"; CAM_PORT=8557; CAM_W=2560; CAM_H=1440; CAM_MAXRATE=8000
      CAM_URL="https://videos.pexels.com/video-files/18320892/18320892-uhd_3840_2160_30fps.mp4"
      CAM_MASKS="
110 40 0,1082,846 0.5,1066,858 1,1008,868 1.5,976,878 2,912,894 2.5,874,922 3,855,948
300 90 3,855,948 3.5,913,974 4,1159,1043 4.5,1679,1210 4.8,2075,1290
110 40 16,1100,828 17,1098,830 18.5,1078,836 19,1056,852 20,1024,862 21,986,887 22,1050,932 22.5,1098,958
170 55 22.5,1098,958 23,1142,986 23.5,1194,1025
240 70 23.5,1194,1025 24,1179,1056 24.5,1398,1105
300 90 24.5,1398,1105 25,1726,1190 25.5,2190,1314 25.6,2333,1340 25.7,2400,1375 25.8,2410,1395 26.3,2410,1395
100 44 0,330,798 10,298,798 20,293,795 30,340,780
100 44 0,530,875 10,497,870 20,493,868 30,545,855
100 44 0,1590,908 10,1557,895 20,1560,890 30,1595,890"
      ;;
    *) die "unknown camera: $1" ;;
  esac
}

rtsp_url() { # PORT [SUBTYPE]
  printf 'rtsp://%s:%s/%s?channel=1&subtype=%s' "$RTSP_HOST" "$1" "$RTSP_PATH" "${2:-0}"
}
