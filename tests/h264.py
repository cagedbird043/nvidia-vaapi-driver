#!/usr/bin/env python3
"""在真实 NVIDIA GPU 上比较 VA-API 与软件解码，包含参考帧、裁剪、多切片及尾帧。"""
import argparse
import os
from pathlib import Path
import subprocess
import tempfile


def run(args, env):
    return subprocess.run(args, env=env, check=True, stdout=subprocess.PIPE, text=True).stdout


def frames(text):
    return [line.strip() for line in text.splitlines() if line and not line.startswith("#")]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--device", default="/dev/dri/renderD129")
    parser.add_argument("--driver", type=Path, default=Path(__file__).resolve().parents[1] / "target/release/librustnv_drv_video.so")
    args = parser.parse_args()
    library = args.driver.resolve(strict=True)
    env = dict(os.environ)
    for key in ("LIBVA_DRIVERS_PATH", "LIBVA_DRIVER_NAME", "NVD_BACKEND", "NVD_LOG"):
        env.pop(key, None)
    ffmpeg = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin", "-xerror", "-y"]
    cases = [
        ("high", "1280x720", 30, 480, 3, []),
        ("main", "854x480", 24, 96, 2, ["-x264-params", "slices=4"]),
        ("baseline", "640x360", 30, 90, 0, []),
    ]
    with tempfile.TemporaryDirectory(prefix="rustnv-h264-") as temporary:
        root = Path(temporary)
        (root / "rustnv_drv_video.so").symlink_to(library)
        hardware_env = dict(env, LIBVA_DRIVERS_PATH=str(root), LIBVA_DRIVER_NAME="rustnv")
        for profile, size, rate, count, bframes, extra in cases:
            clip = str(root / f"{profile}.mp4")
            run(ffmpeg + ["-f", "lavfi", "-i", f"testsrc2=size={size}:rate={rate}",
                          "-frames:v", str(count), "-c:v", "libx264", "-preset", "fast",
                          "-profile:v", profile, "-bf", str(bframes), "-g", str(rate * 2),
                          "-pix_fmt", "yuv420p"] + extra + [clip], env)
            reference = frames(run(ffmpeg + ["-i", clip, "-pix_fmt", "nv12", "-f", "framemd5", "-"], env))
            hardware = frames(run(ffmpeg + ["-hwaccel", "vaapi", "-hwaccel_device", args.device,
                                            "-hwaccel_output_format", "vaapi", "-i", clip,
                                            "-vf", "hwdownload,format=nv12", "-f", "framemd5", "-"], hardware_env))
            if len(reference) != count or len(hardware) != count:
                raise RuntimeError(f"{profile}: expected {count} frames; software={len(reference)}, hardware={len(hardware)}")
            mismatch = next((i for i, (a, b) in enumerate(zip(reference, hardware)) if a != b), None)
            if mismatch is not None:
                raise RuntimeError(f"{profile}: frame {mismatch} mismatch\n{reference[mismatch]}\n{hardware[mismatch]}")
            print(f"PASS {profile} {size}: {count}/{count} frames bit-exact", flush=True)


if __name__ == "__main__":
    main()
