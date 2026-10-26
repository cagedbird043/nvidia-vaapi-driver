#!/usr/bin/env python3
"""在真实 NVIDIA GPU 上逐帧比较 VA-API 与软件解码：参考帧、裁剪、多切片/分块、隐藏帧及尾帧。

用法：decode.py [--codec h264|hevc|vp9|av1 ...]，缺省跑全部格式。
"""
import argparse
import os
from pathlib import Path
import subprocess
import tempfile

# (name, codec, size, rate, frames, encoder arguments)
CASES = [
    ("h264-high", "h264", "1280x720", 30, 480,
     ["-c:v", "libx264", "-preset", "fast", "-profile:v", "high", "-bf", "3", "-g", "60"]),
    ("h264-main-slices", "h264", "854x480", 24, 96,
     ["-c:v", "libx264", "-preset", "fast", "-profile:v", "main", "-bf", "2", "-g", "48", "-x264-params", "slices=4"]),
    ("h264-baseline", "h264", "640x360", 30, 90,
     ["-c:v", "libx264", "-preset", "fast", "-profile:v", "baseline", "-bf", "0", "-g", "60"]),
    ("hevc-main", "hevc", "1280x720", 30, 240,
     ["-c:v", "libx265", "-preset", "fast", "-x265-params", "bframes=4:keyint=60:log-level=error"]),
    ("hevc-slices-crop", "hevc", "854x480", 24, 96,
     ["-c:v", "libx265", "-preset", "fast", "-x265-params", "slices=4:bframes=2:keyint=48:log-level=error"]),
    ("vp9-altref", "vp9", "1280x720", 30, 240,
     ["-c:v", "libvpx-vp9", "-deadline", "good", "-cpu-used", "4", "-b:v", "2M", "-g", "60",
      "-auto-alt-ref", "1", "-lag-in-frames", "16"]),
    ("vp9-tiles-crop", "vp9", "854x480", 24, 96,
     ["-c:v", "libvpx-vp9", "-deadline", "realtime", "-cpu-used", "8", "-b:v", "1M", "-g", "48", "-tile-columns", "1"]),
    ("av1-main", "av1", "1280x720", 30, 240,
     ["-c:v", "libsvtav1", "-preset", "8", "-g", "60"]),
    ("av1-tiles-crop", "av1", "854x480", 24, 96,
     ["-c:v", "libaom-av1", "-cpu-used", "8", "-b:v", "1M", "-g", "48", "-tile-columns", "1", "-tile-rows", "1"]),
]


def run(args, env):
    return subprocess.run(args, env=env, check=True, stdout=subprocess.PIPE, text=True).stdout


def frames(text):
    return [line.strip() for line in text.splitlines() if line and not line.startswith("#")]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--device", default="/dev/dri/renderD129")
    parser.add_argument("--driver", type=Path, default=Path(__file__).resolve().parents[1] / "target/release/librustnv_drv_video.so")
    parser.add_argument("--codec", action="append", choices=sorted({c[1] for c in CASES}))
    args = parser.parse_args()
    library = args.driver.resolve(strict=True)
    env = dict(os.environ)
    for key in ("LIBVA_DRIVERS_PATH", "LIBVA_DRIVER_NAME", "NVD_BACKEND", "NVD_LOG"):
        env.pop(key, None)
    ffmpeg = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin", "-xerror", "-y"]
    with tempfile.TemporaryDirectory(prefix="rustnv-decode-") as temporary:
        root = Path(temporary)
        (root / "rustnv_drv_video.so").symlink_to(library)
        hardware_env = dict(env, LIBVA_DRIVERS_PATH=str(root), LIBVA_DRIVER_NAME="rustnv")
        for name, codec, size, rate, count, encoder in CASES:
            if args.codec and codec not in args.codec:
                continue
            clip = str(root / f"{name}.mkv")
            run(ffmpeg + ["-f", "lavfi", "-i", f"testsrc2=size={size}:rate={rate}", "-frames:v", str(count),
                          "-pix_fmt", "yuv420p"] + encoder + [clip], env)
            reference = frames(run(ffmpeg + ["-i", clip, "-pix_fmt", "nv12", "-f", "framemd5", "-"], env))
            hardware = frames(run(ffmpeg + ["-hwaccel", "vaapi", "-hwaccel_device", args.device,
                                            "-hwaccel_output_format", "vaapi", "-i", clip,
                                            "-vf", "hwdownload,format=nv12", "-f", "framemd5", "-"], hardware_env))
            if len(reference) != count or len(hardware) != count:
                raise RuntimeError(f"{name}: expected {count} frames; software={len(reference)}, hardware={len(hardware)}")
            mismatch = next((i for i, (a, b) in enumerate(zip(reference, hardware)) if a != b), None)
            if mismatch is not None:
                raise RuntimeError(f"{name}: frame {mismatch} mismatch\n{reference[mismatch]}\n{hardware[mismatch]}")
            print(f"PASS {name} {size}: {count}/{count} frames bit-exact", flush=True)


if __name__ == "__main__":
    main()
