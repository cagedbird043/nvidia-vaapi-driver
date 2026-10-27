#!/usr/bin/env python3
"""GStreamer `va` 插件（gst-plugins-bad）经 rustnv 解码与编码的回归。

解码：va*dec 输出读回为 NV12，与 ffmpeg 软解逐帧比较（MPEG-2 要求 PSNR 不低于 PSNR_FLOOR）。
编码：videotestsrc 经 va*enc 输出，软件解码后帧数必须一致、对源的平均 PSNR 不低于 ENC_PSNR_FLOOR。
插件只在 GST_VA_ALL_DRIVERS=1 时接受非 Intel/AMD 驱动；使用临时 GST_REGISTRY，不碰用户缓存。
没有 gst-launch-1.0 或 va 插件时退出 0 并注明跳过。
用法：gstreamer.py [--device /dev/dri/renderD129] [--driver .../librustnv_drv_video.so] [--only 名称 ...]
"""
import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

PSNR_FLOOR = 45.0
ENC_PSNR_FLOOR = 30.0
SIZE, FRAMES = (1280, 720), 60

# (名称, 编码器, 容器, ffmpeg 编码参数, 解复用与解析, VA 元素后缀, 比较方式)
DECODE = [
    ("h264", "libx264", "mp4", ["-preset", "fast", "-bf", "3", "-g", "30"], "qtdemux ! h264parse", "h264dec", "exact"),
    ("hevc", "libx265", "mp4", ["-preset", "fast", "-x265-params", "bframes=3:keyint=30:log-level=error"], "qtdemux ! h265parse", "h265dec", "exact"),
    ("vp9", "libvpx-vp9", "webm", ["-deadline", "good", "-cpu-used", "5", "-g", "30", "-auto-alt-ref", "1", "-lag-in-frames", "16"], "matroskademux ! vp9parse", "vp9dec", "exact"),
    ("vp8", "libvpx", "webm", ["-b:v", "2M", "-g", "30"], "matroskademux", "vp8dec", "exact"),
    # libsvtav1 的关键帧在 VA 参数里不带参考索引（GStreamer 填 255）。
    ("av1", "libsvtav1", "mp4", ["-preset", "8", "-g", "30"], "qtdemux ! av1parse", "av1dec", "exact"),
    ("mpeg2", "mpeg2video", "ts", ["-bf", "2", "-g", "12", "-b:v", "4M"], "tsdemux ! mpegvideoparse", "mpeg2dec", "psnr"),
]
ENCODE = [("h264", "h264enc", "h264parse", "h264"), ("hevc", "h265enc", "h265parse", "hevc"), ("av1", "av1enc", "av1parse", "mkv")]
# vapostproc：(名称, VA 内存上的输出 caps, 读回格式, 输出尺寸, 对 zscale bilinear 的 PSNR 下限)。两级 vapostproc：
# 第一级在 VA surface 上缩放 / 变格式，第二级读回系统内存。GStreamer 按画面高度协商 colorimetry（≥ 720 行 BT.709，
# 否则 BT.601），NV12 默认色度左对齐；swscale 不认左对齐，参考用 zscale。缩小时 zscale 的滤波器覆盖更宽，下限按实测留余量。
POSTPROC = [
    ("postproc scale 720p -> 360p", "video/x-raw(memory:VAMemory),format=NV12", "nv12", (640, 360), 34.0),
    ("postproc scale 720p -> 1080p", "video/x-raw(memory:VAMemory),format=NV12", "nv12", (1920, 1080), 55.0),
    ("postproc NV12 -> BGRA", "video/x-raw(memory:VAMemory),format=BGRA", "bgra", (1280, 720), 60.0),
    ("postproc decoded h264", "video/x-raw(memory:VAMemory),format=NV12", "nv12", (960, 540), 35.0),
]


def run(cmd, env, timeout=300):
    return subprocess.run(cmd, shell=isinstance(cmd, str), env=env, capture_output=True, timeout=timeout)


def psnr(raw_a, raw_b, size, pix_fmt="nv12"):
    graph = "[0]format=gbrp[a];[1]format=gbrp[b];[a][b]psnr" if pix_fmt == "bgra" else "psnr"
    out = subprocess.run(["ffmpeg", "-hide_banner", "-f", "rawvideo", "-pix_fmt", pix_fmt, "-s", size, "-i", raw_a,
                          "-f", "rawvideo", "-pix_fmt", pix_fmt, "-s", size, "-i", raw_b, "-lavfi", graph, "-f", "null", "-"],
                         capture_output=True, text=True).stderr
    match = re.search(r"average:(\S+)", out)
    return float("inf") if match and match.group(1) == "inf" else float(match.group(1)) if match else 0.0

def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--device", default="/dev/dri/renderD129")
    parser.add_argument("--driver", type=Path, default=Path(__file__).resolve().parents[1] / "target/release/librustnv_drv_video.so")
    parser.add_argument("--only", action="append")
    args = parser.parse_args()
    if not shutil.which("gst-launch-1.0"):
        print("SKIP gst-launch-1.0 not installed")
        return
    node = Path(args.device).name
    w, h = SIZE
    size = f"{w}x{h}"
    frame_bytes = w * h * 3 // 2
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        (tmp / "drv").mkdir()
        (tmp / "drv/rustnv_drv_video.so").symlink_to(args.driver.resolve())
        env = dict(os.environ, GST_VA_ALL_DRIVERS="1", GST_REGISTRY=str(tmp / "registry.bin"),
                   LIBVA_DRIVERS_PATH=str(tmp / "drv"), LIBVA_DRIVER_NAME="rustnv")
        for key in [k for k in env if k.startswith("NVD_")]:
            del env[key]
        listing = run("gst-inspect-1.0 va", env).stdout.decode()
        names = set(re.findall(r"^\s+(va\w+):", listing, re.M))
        if not names:
            print("SKIP GStreamer va plugin not installed")
            return

        def element(suffix):
            for candidate in (f"va{node}{suffix}", f"va{suffix}"):
                if candidate in names:
                    return candidate
            return None

        source = tmp / "source.nv12"
        run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i", f"testsrc2=size={size}:rate=30", "-vf", "noise=alls=6:allf=t",
             "-frames:v", str(FRAMES), "-pix_fmt", "nv12", "-f", "rawvideo", str(source)], env)
        failures = []

        def report(name, ok, detail):
            print(("PASS " if ok else "FAIL ") + f"{name}: {detail}")
            if not ok:
                failures.append(name)

        for name, encoder, container, extra, demux, suffix, mode in DECODE:
            if args.only and name not in args.only:
                continue
            dec = element(suffix)
            if dec is None:
                report(f"decode {name}", False, f"no GStreamer element for {suffix}")
                continue
            clip = tmp / f"clip.{container}"
            run(["ffmpeg", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "nv12", "-s", size, "-r", "30", "-i", str(source),
                 "-c:v", encoder, "-pix_fmt", "yuv420p"] + extra + [str(clip)], env)
            reference = tmp / "reference.nv12"
            run(["ffmpeg", "-v", "error", "-y", "-i", str(clip), "-pix_fmt", "nv12", "-f", "rawvideo", str(reference)], env)
            result = tmp / "result.nv12"
            result.unlink(missing_ok=True)
            done = run(f"gst-launch-1.0 -q filesrc location={clip} ! {demux} ! {dec} ! videoconvert ! video/x-raw,format=NV12 ! filesink location={result}", env)
            if done.returncode or not result.exists():
                report(f"decode {name}", False, done.stderr.decode().strip()[-200:])
                continue
            ref, got = reference.read_bytes(), result.read_bytes()
            count = len(ref) // frame_bytes
            if len(got) != len(ref):
                report(f"decode {name}", False, f"{len(got) // frame_bytes} of {count} frames")
            elif mode == "exact":
                bad = sum(1 for i in range(count) if got[i * frame_bytes:(i + 1) * frame_bytes] != ref[i * frame_bytes:(i + 1) * frame_bytes])
                report(f"decode {name}", bad == 0, f"{count - bad}/{count} frames bit-exact")
            else:
                value = psnr(str(result), str(reference), size)
                report(f"decode {name}", value >= PSNR_FLOOR, f"{count} frames, PSNR {value:.1f} dB")

        for name, suffix, parser_name, extension in ENCODE:
            if args.only and f"encode-{name}" not in args.only and name not in args.only:
                continue
            enc = element(suffix)
            if enc is None:
                report(f"encode {name}", False, f"no GStreamer element for {suffix}")
                continue
            out = tmp / f"out.{extension}"
            out.unlink(missing_ok=True)
            tail = {"h264": "video/x-h264,stream-format=byte-stream", "hevc": "video/x-h265,stream-format=byte-stream", "av1": "matroskamux"}[name]
            done = run(f"gst-launch-1.0 -q filesrc location={source} blocksize={frame_bytes} ! "
                       f"rawvideoparse format=nv12 width={w} height={h} framerate=30/1 ! {enc} ! {parser_name} ! {tail} ! filesink location={out}", env)
            if done.returncode or not out.exists() or out.stat().st_size == 0:
                report(f"encode {name}", False, done.stderr.decode().strip()[-200:])
                continue
            decoded = tmp / "decoded.nv12"
            run(["ffmpeg", "-v", "error", "-y", "-i", str(out), "-pix_fmt", "nv12", "-f", "rawvideo", str(decoded)], env)
            frames = decoded.stat().st_size // frame_bytes if decoded.exists() else 0
            value = psnr(str(decoded), str(source), size) if frames == FRAMES else 0.0
            report(f"encode {name}", frames == FRAMES and value >= ENC_PSNR_FLOOR, f"{frames}/{FRAMES} frames, PSNR {value:.1f} dB")

        post = element("postproc")
        for name, caps, pix_fmt, scale, floor in POSTPROC:
            if args.only and "postproc" not in args.only and name not in args.only:
                continue
            if post is None:
                report(name, False, "no vapostproc element")
                continue
            out_w, out_h = scale
            result, reference = tmp / "post.raw", tmp / "post-ref.raw"
            result.unlink(missing_ok=True)
            upstream = (f"filesrc location={source} blocksize={frame_bytes} ! rawvideoparse format=nv12 width={w} height={h} framerate=30/1"
                        if name != "postproc decoded h264" else f"filesrc location={tmp / 'clip.mp4'} ! qtdemux ! h264parse ! {element('h264dec')}")
            if name == "postproc decoded h264":
                run(["ffmpeg", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "nv12", "-s", size, "-r", "30", "-i", str(source),
                     "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "12", str(tmp / "clip.mp4")], env)
            done = run(f"gst-launch-1.0 -q {upstream} ! {post} ! '{caps},width={out_w},height={out_h}' ! {post} ! "
                       f"video/x-raw,{caps.split(',', 1)[1]},width={out_w},height={out_h} ! filesink location={result}", env)
            if done.returncode or not result.exists():
                report(name, False, done.stderr.decode().strip()[-200:])
                continue
            matrix = "709" if out_h >= 720 else "170m"
            target = "range=full,format=gbrp" if pix_fmt == "bgra" else f"matrix={matrix}:range=limited:chromal=left,format=nv12"
            run(["ffmpeg", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "nv12", "-s", size, "-i", str(source),
                 "-vf", f"setparams=chroma_location=left,zscale=w={out_w}:h={out_h}:matrixin=709:rangein=limited:chromalin=left:filter=bilinear:"
                        + target, "-pix_fmt", pix_fmt, "-f", "rawvideo", str(reference)], env)
            frame = out_w * out_h * (4 if pix_fmt == "bgra" else 3) // (1 if pix_fmt == "bgra" else 2)
            frames = result.stat().st_size // frame
            value = psnr(str(result), str(reference), f"{out_w}x{out_h}", pix_fmt) if frames == FRAMES else 0.0
            report(name, frames == FRAMES and value >= floor, f"{frames}/{FRAMES} frames, PSNR {value:.1f} dB")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
