#!/bin/bash
# End-to-end decode/encode matrix through ffmpeg on the NVIDIA render node.
#
# Decode: a clip made with a software encoder is decoded through VA-API with
# hardware output frames and compared frame by frame against software decode.
# Encode: a test pattern is encoded through VA-API, then decoded in software
# and compared against the source with PSNR.
#
# Usage: tests/hw-matrix.sh [driver-dir]
#   driver-dir   directory holding nvidia_drv_video.so (default: ./build)
# Environment:
#   VAAPI_DEVICE   render node (default: first NVIDIA render node)
#   SIZE           frame size (default: 1920x1080)
#   FRAMES         frame count (default: 60)
#   RATE_FRAMES    frame count for CBR/VBR bitrate checks (default: 300); rate
#                  control needs several seconds before the average settles
#   MIN_PSNR       minimum average PSNR for encodes, dB (default: 35)

set -u

driver_dir=$(realpath "${1:-build}")
[ -f "$driver_dir/nvidia_drv_video.so" ] || { echo "no nvidia_drv_video.so in $driver_dir" >&2; exit 2; }

if [ -z "${VAAPI_DEVICE:-}" ]; then
    for node in /dev/dri/renderD*; do
        vendor=$(cat "/sys/class/drm/$(basename "$node")/device/vendor" 2>/dev/null)
        if [ "$vendor" = 0x10de ]; then VAAPI_DEVICE=$node; break; fi
    done
fi
[ -n "${VAAPI_DEVICE:-}" ] || { echo "no NVIDIA render node found" >&2; exit 2; }

SIZE=${SIZE:-1920x1080}
FRAMES=${FRAMES:-60}
RATE_FRAMES=${RATE_FRAMES:-300}
MIN_PSNR=${MIN_PSNR:-35}

export LIBVA_DRIVER_NAME=nvidia LIBVA_DRIVERS_PATH=$driver_dir
export NVD_BACKEND=${NVD_BACKEND:-direct}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

ff() { ffmpeg -hide_banner -nostdin -y -loglevel error "$@"; }

pass=0 fail=0 skip=0
report() {
    printf '%-6s %-28s %s\n' "$1" "$2" "$3"
    case $1 in PASS) pass=$((pass + 1)) ;; FAIL) fail=$((fail + 1)) ;; *) skip=$((skip + 1)) ;; esac
}

src() { # src <pix_fmt> -> raw test pattern of $FRAMES frames
    local out=$tmp/src-$1-$FRAMES.nut
    [ -f "$out" ] || ff -f lavfi -i "testsrc2=size=$SIZE:rate=30" -frames:v "$FRAMES" \
        -pix_fmt "$1" -c:v ffv1 "$out"
    echo "$out"
}

# decode <name> <profile substring in vainfo> <sw pix_fmt> <hw download fmt> <encoder args...>
decode() {
    local name=$1 profile=$2 pixfmt=$3 dlfmt=$4; shift 4
    if ! grep -q "$profile[[:space:]]*:[[:space:]]*VAEntrypointVLD" "$tmp/vainfo"; then
        report SKIP "decode $name" "$profile not advertised"; return
    fi
    local clip=$tmp/dec-$name.mkv
    if ! ff -i "$(src "$pixfmt")" "$@" "$clip" 2>"$tmp/err"; then
        report SKIP "decode $name" "software encoder failed: $(head -c 120 "$tmp/err")"; return
    fi
    ff -i "$clip" -vf "format=$dlfmt" -f framemd5 "$tmp/sw.md5"
    if ! ff -hwaccel vaapi -hwaccel_device "$VAAPI_DEVICE" -hwaccel_output_format vaapi \
        -i "$clip" -vf "hwdownload,format=$dlfmt" -f framemd5 "$tmp/hw.md5" 2>"$tmp/err"; then
        report FAIL "decode $name" "$(grep -v '^$' "$tmp/err" | tail -1)"; return
    fi
    local n diff
    n=$(grep -vc '^#' "$tmp/hw.md5")
    diff=$(diff <(grep -v '^#' "$tmp/sw.md5" | awk -F, '{print $NF}') \
        <(grep -v '^#' "$tmp/hw.md5" | awk -F, '{print $NF}') | grep -c '^>')
    if [ "$n" -eq "$FRAMES" ] && [ "$diff" -eq 0 ]; then
        report PASS "decode $name" "$n frames bit-exact with software"
    else
        report FAIL "decode $name" "$n/$FRAMES frames, $diff differ from software"
    fi
}

# encode <name> <profile substring> <upload fmt> <encoder> <encoder args...>
# With EXPECT_KBPS set, the average bitrate must land within 25% of it.
encode() {
    local name=$1 profile=$2 upfmt=$3 enc=$4; shift 4
    if ! grep -q "$profile[[:space:]]*:[[:space:]]*VAEntrypointEncSlice" "$tmp/vainfo"; then
        report SKIP "encode $name" "$profile not advertised"; return
    fi
    # mkv: matroska needs the codec extradata at init, so this also checks
    # the global header path (AV1: packed sequence header support).
    local out=$tmp/enc-$name.mkv source
    source=$(src "$upfmt")
    if ! ff -vaapi_device "$VAAPI_DEVICE" -i "$source" -vf "format=$upfmt,hwupload" \
        -c:v "$enc" "$@" "$out" 2>"$tmp/err"; then
        report FAIL "encode $name" "$(grep -v '^$' "$tmp/err" | tail -1)"; return
    fi
    local n psnr
    n=$(ffprobe -v error -count_frames -select_streams v -show_entries stream=nb_read_frames -of csv=p=0 "$out")
    # Pair frames by index: container timestamps drift against the 1/30 source
    # timebase and make psnr compare wrong frames.
    psnr=$(ffmpeg -hide_banner -nostdin -i "$out" -i "$source" \
        -lavfi "[0]settb=1/30,setpts=N[a];[1]settb=1/30,setpts=N[b];[a][b]psnr" -f null - 2>&1 |
        sed -n 's/.* average:\([0-9.inf]*\).*/\1/p' | tail -1)
    local size=$(( $(stat -c %s "$out") / 1024 ))
    local kbps=$(( size * 8 * 1024 * 30 / FRAMES / 1000 ))
    local ratefail=
    if [ -n "${EXPECT_KBPS:-}" ] &&
        [ $((kbps * 4)) -lt $((EXPECT_KBPS * 3)) -o $((kbps * 4)) -gt $((EXPECT_KBPS * 5)) ]; then
        ratefail=" (expected ~${EXPECT_KBPS} kbps)"
    fi
    if [ "$n" = "$FRAMES" ] && [ -z "$ratefail" ] &&
        awk -v p="$psnr" -v m="$MIN_PSNR" 'BEGIN { exit !(p == "inf" || p + 0 >= m) }'; then
        report PASS "encode $name" "$n frames, PSNR ${psnr} dB, ${kbps} kbps"
    else
        report FAIL "encode $name" "$n/$FRAMES frames, PSNR ${psnr:-?} dB, ${kbps} kbps$ratefail"
    fi
}

vainfo --display drm --device "$VAAPI_DEVICE" >"$tmp/vainfo" 2>&1 ||
    { cat "$tmp/vainfo"; echo "vainfo failed" >&2; exit 1; }
echo "device $VAAPI_DEVICE, $(sed -n 's/.*Driver version: //p' "$tmp/vainfo"), ${SIZE}, $FRAMES frames"

decode h264         'VAProfileH264High'      yuv420p     nv12 -c:v libx264 -profile:v high
decode hevc         'VAProfileHEVCMain '     yuv420p     nv12 -c:v libx265 -x265-params log-level=error
decode hevc-10bit   'VAProfileHEVCMain10'    yuv420p10le p010 -c:v libx265 -x265-params log-level=error
decode vp8          'VAProfileVP8Version0_3' yuv420p     nv12 -c:v libvpx -b:v 5M
decode vp9          'VAProfileVP9Profile0'   yuv420p     nv12 -c:v libvpx-vp9 -b:v 5M -row-mt 1
decode vp9-10bit    'VAProfileVP9Profile2'   yuv420p10le p010 -c:v libvpx-vp9 -b:v 5M -row-mt 1
# cyclic-refresh AQ turns on segmentation with per-segment quantizers
decode vp9-segments 'VAProfileVP9Profile0'   yuv420p     nv12 -c:v libvpx-vp9 -b:v 2M -deadline realtime -cpu-used 8 -aq-mode 3
decode av1          'VAProfileAV1Profile0'   yuv420p     nv12 -c:v libsvtav1 -preset 10
decode av1-10bit    'VAProfileAV1Profile0'   yuv420p10le p010 -c:v libsvtav1 -preset 10

for rc in CQP CBR VBR; do
    frames=$FRAMES
    case $rc in
        CQP) rcargs=(-rc_mode CQP -qp 24); EXPECT_KBPS= ;;
        CBR) rcargs=(-rc_mode CBR -b:v 8M); EXPECT_KBPS=8000; frames=$RATE_FRAMES ;;
        VBR) rcargs=(-rc_mode VBR -b:v 4M -maxrate 8M); EXPECT_KBPS=4000; frames=$RATE_FRAMES ;;
    esac
    FRAMES=$frames encode "h264-$rc" 'VAProfileH264High'    nv12 h264_vaapi "${rcargs[@]}"
    FRAMES=$frames encode "hevc-$rc" 'VAProfileHEVCMain '   nv12 hevc_vaapi "${rcargs[@]}"
    FRAMES=$frames encode "av1-$rc"  'VAProfileAV1Profile0' nv12 av1_vaapi  "${rcargs[@]}"
done
EXPECT_KBPS=
encode hevc-10bit-CQP   'VAProfileHEVCMain10'  p010 hevc_vaapi -profile:v main10 -rc_mode CQP -qp 24
encode av1-10bit-CQP    'VAProfileAV1Profile0' p010 av1_vaapi  -rc_mode CQP -qp 24
encode h264-bframes     'VAProfileH264High'    nv12 h264_vaapi -rc_mode CQP -qp 24 -bf 2
encode hevc-bframes     'VAProfileHEVCMain '   nv12 hevc_vaapi -rc_mode CQP -qp 24 -bf 2

echo "pass $pass, fail $fail, skip $skip"
[ "$fail" -eq 0 ]
