#!/usr/bin/env python3
"""在真实 NVIDIA GPU 上逐帧比较 VA-API 与软件解码：参考帧、裁剪、多切片/分块、隐藏帧及尾帧。

10/12 位与 4:4:4 的样片经 va_dump（libavcodec VA-API 解码，vaGetImage 与 vaDeriveImage 各读一遍，不做像素格式转换）
读出，与 ffmpeg 软解同一像素格式的结果逐字节比较，并把 vaExportSurfaceHandle 导出的各层经 EGL 导入读回，与 vaGetImage 比较；8 位 4:2:0 直接用 ffmpeg hwdownload。
--c-driver 把同样的样片再交给系统安装的 C 版驱动，只列结果，失败不影响退出码。

用法：decode.py [--codec h264|hevc|vp9|av1 ...] [--c-driver] [--only 样片名 ...]，缺省跑全部格式。
"""
import argparse
import array
import os
from pathlib import Path
import subprocess
import sys
import tempfile

# (name, codec, size, rate, frames, encoder arguments[, source pix_fmt, VA-API image fourcc])
# 缺省是 yuv420p 经 ffmpeg 读 nv12；给出 fourcc 的样片经 va_dump 读原始平面。
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
    ("hevc-main10", "hevc", "1280x720", 30, 240,
     ["-c:v", "libx265", "-preset", "fast", "-x265-params", "bframes=4:keyint=60:log-level=error"], "yuv420p10le", "P010"),
    ("hevc-main10-slices-crop", "hevc", "850x478", 24, 96,
     ["-c:v", "libx265", "-preset", "fast", "-x265-params", "slices=4:bframes=3:keyint=48:log-level=error"], "yuv420p10le", "P010"),
    ("hevc-main12", "hevc", "854x480", 24, 96,
     ["-c:v", "libx265", "-preset", "fast", "-x265-params", "bframes=3:keyint=48:log-level=error"], "yuv420p12le", "P012"),
    ("hevc-444-8", "hevc", "850x478", 24, 96,
     ["-c:v", "libx265", "-preset", "fast", "-x265-params", "bframes=3:keyint=48:log-level=error"], "yuv444p", "444P"),
    ("hevc-444-10", "hevc", "1280x720", 30, 120,
     ["-c:v", "libx265", "-preset", "fast", "-x265-params", "bframes=3:keyint=60:log-level=error"], "yuv444p10le", "Q416"),
    ("hevc-444-12-slices", "hevc", "850x478", 24, 96,
     ["-c:v", "libx265", "-preset", "fast", "-x265-params", "slices=3:bframes=2:keyint=48:log-level=error"], "yuv444p12le", "Q416"),
    ("vp9-p2-10", "vp9", "1280x720", 30, 240,
     ["-c:v", "libvpx-vp9", "-profile:v", "2", "-deadline", "good", "-cpu-used", "4", "-b:v", "2M", "-g", "60",
      "-auto-alt-ref", "1", "-lag-in-frames", "16"], "yuv420p10le", "P010"),
    ("vp9-p2-12-tiles", "vp9", "850x478", 24, 96,
     ["-c:v", "libvpx-vp9", "-profile:v", "2", "-deadline", "good", "-cpu-used", "5", "-b:v", "1M", "-g", "48",
      "-tile-columns", "1"], "yuv420p12le", "P012"),
    ("av1-10", "av1", "1280x720", 30, 240,
     ["-c:v", "libsvtav1", "-preset", "8", "-g", "60"], "yuv420p10le", "P010"),
    ("av1-10-tiles-crop", "av1", "850x478", 24, 96,
     ["-c:v", "libaom-av1", "-cpu-used", "8", "-b:v", "1M", "-g", "48", "-tile-columns", "1", "-tile-rows", "1"],
     "yuv420p10le", "P010"),
]


def high_aligned(raw, bits):
    """软解的低位对齐 16 位样本移到高位，与硬解输出的容器布局一致。"""
    samples = array.array("H")
    samples.frombytes(raw)
    shift = 16 - bits
    if sys.byteorder == "big":
        samples.byteswap()
    shifted = array.array("H", (v << shift for v in samples))
    if sys.byteorder == "big":
        shifted.byteswap()
    return shifted.tobytes()


def compare_dump(ffmpeg, dump, clip, fourcc, pix_fmt, device, env, hardware_env, count, egl_probe, hint=()):
    """va_dump 以 vaGetImage 和 vaDeriveImage 读出的平面与软解逐字节比较，返回错误文字或 None。"""
    # P010/P012 are FFmpeg pixel formats (samples already in the high bits); Q416 has none, so the
    # planar 4:4:4 decode is shifted up the way the engine writes it.
    semi_planar = {"P010": "p010le", "P012": "p012le"}.get(fourcc)
    reference = subprocess.run(ffmpeg + ["-i", clip, "-pix_fmt", semi_planar or pix_fmt, "-f", "rawvideo", "-"], env=env,
                               check=True, stdout=subprocess.PIPE).stdout
    if fourcc == "Q416":
        reference = high_aligned(reference, int(pix_fmt[-4:-2]))
    for mode in ("getimage", "derive"):
        out = clip + f".{mode}.raw"
        result = subprocess.run([dump, device, clip, out, mode, fourcc, *hint], env=hardware_env, stderr=subprocess.PIPE, text=True)
        if result.returncode:
            return f"{mode}: va_dump failed: {result.stderr.strip().splitlines()[-1:]}"
        data = Path(out).read_bytes()
        Path(out).unlink()
        if len(data) != len(reference):
            return f"{mode}: {len(data)} bytes, expected {len(reference)}"
        if data != reference:
            frame = len(reference) // count
            index = next(i for i in range(len(data)) if data[i] != reference[i])
            return f"{mode}: first difference at frame {index // frame} byte {index % frame}"
    if egl_probe:
        # The exported dma-buf layers imported into EGL read back as vaGetImage returns them.
        result = subprocess.run([egl_probe, device, clip, fourcc], env=hardware_env, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True)
        if result.returncode:
            return f"EGL import: {(result.stdout + result.stderr).strip().splitlines()[-1:]}"
    return None


def decode_case(ffmpeg, dump, clip, fourcc, pix_fmt, device, env, hardware_env, count, egl_probe=None, shim=None):
    """8 位 4:2:0 经 ffmpeg hwdownload；其余格式经 va_dump。返回错误文字或 None。"""
    if fourcc is not None:
        error = compare_dump(ffmpeg, dump, clip, fourcc, pix_fmt, device, env, hardware_env, count, egl_probe)
        if error is None and fourcc == "P010" and clip.endswith(("av1-10.mkv", "vp9-p2-10.mkv")) and shim:
            # Chrome's call order: the config asks for RTFormat YUV420, the surfaces are 10-bit.
            error = compare_dump(ffmpeg, dump, clip, fourcc, pix_fmt, device, env, dict(hardware_env, LD_PRELOAD=shim), count, None)
            error = error and f"RTFormat YUV420 config with 10-bit surfaces: {error}"
        return error
    reference = frames(run(ffmpeg + ["-i", clip, "-pix_fmt", "nv12", "-f", "framemd5", "-"], env))
    result = subprocess.run(ffmpeg + ["-hwaccel", "vaapi", "-hwaccel_device", device, "-hwaccel_output_format", "vaapi",
                                      "-i", clip, "-vf", "hwdownload,format=nv12", "-f", "framemd5", "-"],
                            env=hardware_env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    if result.returncode:
        return f"ffmpeg failed: {result.stderr.strip().splitlines()[-1:]}"
    hardware = frames(result.stdout)
    if len(reference) != count or len(hardware) != count:
        return f"expected {count} frames; software={len(reference)}, hardware={len(hardware)}"
    mismatch = next((i for i, (a, b) in enumerate(zip(reference, hardware)) if a != b), None)
    return None if mismatch is None else f"frame {mismatch} mismatch"


def run(args, env):
    return subprocess.run(args, env=env, check=True, stdout=subprocess.PIPE, text=True).stdout


def frames(text):
    return [line.strip() for line in text.splitlines() if line and not line.startswith("#")]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--device", default="/dev/dri/renderD129")
    parser.add_argument("--driver", type=Path, default=Path(__file__).resolve().parents[1] / "target/release/librustnv_drv_video.so")
    parser.add_argument("--codec", action="append", choices=sorted({c[1] for c in CASES}))
    parser.add_argument("--c-driver", action="store_true", help="the same clips through the installed C driver (reported only)")
    parser.add_argument("--only", action="append", help="case name (repeatable)")
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
        c_env = dict(env, LIBVA_DRIVER_NAME="nvidia")
        dump = str(root / "va_dump")
        flags = run(["pkg-config", "--cflags", "--libs", "libavcodec", "libavformat", "libavutil", "libva"], env).split()
        subprocess.run(["gcc", "-O1", "-o", dump, str(Path(__file__).with_name("va_dump.c"))] + flags, check=True)
        shim = str(root / "va_rt420.so")
        subprocess.run(["gcc", "-O1", "-shared", "-fPIC", "-o", shim, str(Path(__file__).with_name("va_rt420.c")), "-ldl"], check=True)
        egl_probe = str(root / "dmabuf_egl")
        libs = run(["pkg-config", "--cflags", "--libs", "egl", "glesv2", "gbm", "libdrm", "libavcodec", "libavformat",
                    "libavutil", "libva"], env).split()
        subprocess.run(["gcc", "-O1", "-o", egl_probe, str(Path(__file__).with_name("dmabuf_egl.c"))] + libs, check=True)
        for name, codec, size, rate, count, encoder, *extra in CASES:
            if args.codec and codec not in args.codec or args.only and name not in args.only:
                continue
            pix_fmt, fourcc = extra or ("yuv420p", None)
            clip = str(root / f"{name}.mkv")
            run(ffmpeg + ["-f", "lavfi", "-i", f"testsrc2=size={size}:rate={rate}", "-frames:v", str(count),
                          "-pix_fmt", pix_fmt] + encoder + [clip], env)
            for label, driver_env in (("rustnv", hardware_env), ("C", c_env)):
                if label == "C" and not args.c_driver:
                    continue
                error = decode_case(ffmpeg, dump, clip, fourcc, pix_fmt, args.device, env, driver_env, count,
                                    egl_probe if label == "rustnv" else None, shim if label == "rustnv" else None)
                if error and label == "rustnv":
                    raise RuntimeError(f"{name}: {error}")
                print(f"{'FAIL' if error else 'PASS'} {label:6} {name} {size}: "
                      + (error or f"{count}/{count} frames bit-exact"), flush=True)


if __name__ == "__main__":
    main()
