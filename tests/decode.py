#!/usr/bin/env python3
"""在真实 NVIDIA GPU 上逐帧比较 VA-API 与软件解码：参考帧、裁剪、多切片/分块、隐藏帧及尾帧。

10/12 位与 4:4:4 的样片经 va_dump（libavcodec VA-API 解码，vaGetImage 与 vaDeriveImage 各读一遍，不做像素格式转换）
读出，与 ffmpeg 软解同一像素格式的结果逐字节比较，并把 vaExportSurfaceHandle 导出的各层经 EGL 导入读回，与 vaGetImage 比较；8 位 4:2:0 直接用 ffmpeg hwdownload。
--c-driver 把同样的样片再交给系统安装的 C 版驱动，只列结果，失败不影响退出码。

MPEG-2/MPEG-4 的 8 位 4:2:0 样片不要求与软解逐字节一致（IDCT 与 B-VOP 直接模式的实现不同）：MPEG-2 与 C 版驱动输出逐字节比较
（C 版不支持的格式只记录），并要求对软解的 PSNR 不低于 PSNR_FLOOR；MPEG-4 在 ffmpeg 带 mpeg4_cuvid（libnvcuvid）时必须与它逐字节一致，
对软解只要求 PSNR_FLOOR_MPEG4（C 版的 MPEG-4 已禁用，--c-driver 只会记录失败）。VP8 与软解逐字节一致。
VC-1 样片取自 samples.ffmpeg.org（VC1_SAMPLES），缓存在 ~/.cache/rustnv-samples：缺失则跳过并提示，--fetch-samples 经 http 下载；
"exact" 样片与软解逐字节一致，"psnr" 样片（Advanced profile 的色度有极小差异，C 版相同）要求 PSNR 不低于 PSNR_FLOOR。

用法：decode.py [--codec h264|hevc|vp9|av1|mpeg2|mpeg4|vc1|vp8 ...] [--c-driver] [--fetch-samples] [--only 样片名 ...]，缺省跑全部格式。
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


# 对软解 PSNR 的下限（dB）：MPEG-2 的 IDCT 与软解有舍入差异。
PSNR_FLOOR = 45.0
# MPEG-4：自然渐变内容（mandelbrot）的帧间最小值在 43 dB 以上；隔行样片的最小值最低。
PSNR_FLOOR_MPEG4 = 40.0
SAMPLES = Path.home() / ".cache/rustnv-samples"
SAMPLE_URL = "http://samples.ffmpeg.org/"

# (name, codec, size, rate, frames, encoder arguments, mode[, lavfi source])；mode "exact" 对软解逐字节，"psnr" 对软解 PSNR（C 版逐字节只记录），
# "stock" 只与 libnvcuvid 逐字节（不可用则只要求解得出帧）；缺省源 testsrc2。
LEGACY_CASES = [
    ("mpeg2-progressive", "mpeg2", "352x288", 25, 50, ["-c:v", "mpeg2video", "-bf", "0", "-g", "12"], "psnr"),
    ("mpeg2-bframes", "mpeg2", "352x288", 25, 50, ["-c:v", "mpeg2video", "-bf", "2", "-g", "12"], "psnr"),
    ("mpeg2-interlaced", "mpeg2", "352x288", 25, 50,
     ["-c:v", "mpeg2video", "-vf", "setfield=tff", "-flags", "+ildct+ilme", "-bf", "2", "-g", "12"], "psnr"),
    ("mpeg2-matrix", "mpeg2", "352x288", 25, 50,
     ["-c:v", "mpeg2video", "-bf", "2", "-g", "12", "-intra_matrix",
      ",".join(str(8 + i) for i in range(64)), "-inter_matrix", ",".join(str(10 + i // 2) for i in range(64))], "psnr"),
    ("mpeg2-unaligned", "mpeg2", "350x286", 25, 50, ["-c:v", "mpeg2video", "-bf", "2", "-g", "12"], "psnr"),
    ("mpeg2-1080p", "mpeg2", "1920x1080", 25, 50, ["-c:v", "mpeg2video", "-bf", "2", "-g", "12"], "psnr"),
    ("mpeg4-simple", "mpeg4", "352x288", 25, 50, ["-c:v", "mpeg4", "-bf", "0", "-g", "12", "-qscale", "4"], "psnr", "mandelbrot"),
    ("mpeg4-bframes", "mpeg4", "352x288", 25, 50, ["-c:v", "mpeg4", "-bf", "2", "-g", "12", "-qscale", "4"], "psnr", "mandelbrot"),
    ("mpeg4-qpel", "mpeg4", "352x288", 25, 50,
     ["-c:v", "mpeg4", "-bf", "2", "-g", "12", "-qscale", "4", "-flags", "+qpel+mv4"], "psnr", "mandelbrot"),
    ("mpeg4-interlaced", "mpeg4", "352x288", 25, 50,
     ["-c:v", "mpeg4", "-vf", "setfield=tff", "-flags", "+ildct+ilme", "-bf", "2", "-g", "12", "-qscale", "4"], "psnr", "mandelbrot"),
    ("mpeg4-alternate-scan", "mpeg4", "352x288", 25, 50,
     ["-c:v", "mpeg4", "-flags", "+ildct+ilme", "-alternate_scan", "1", "-bf", "0", "-g", "12", "-qscale", "4"], "psnr", "mandelbrot"),
    ("mpeg4-mpeg-quant", "mpeg4", "352x288", 25, 50,
     ["-c:v", "mpeg4", "-bf", "2", "-g", "12", "-qscale", "6", "-mpeg_quant", "1", "-intra_matrix",
      ",".join(str(8 + i // 4) for i in range(64)), "-inter_matrix", ",".join(str(20 + i // 4) for i in range(64))], "psnr", "mandelbrot"),
    ("mpeg4-resync", "mpeg4", "352x288", 25, 50, ["-c:v", "mpeg4", "-bf", "2", "-g", "12", "-qscale", "4", "-ps", "600"], "psnr", "mandelbrot"),
    ("mpeg4-unaligned", "mpeg4", "330x250", 25, 50, ["-c:v", "mpeg4", "-bf", "2", "-g", "12", "-qscale", "4"], "psnr", "mandelbrot"),
    # No GOP header after the first: P/B time stamps cross second boundaries (modulo_time_base).
    ("mpeg4-long-gop", "mpeg4", "352x288", 25, 120, ["-c:v", "mpeg4", "-bf", "2", "-g", "250", "-qscale", "4"], "psnr", "mandelbrot"),
    ("mpeg4-720p", "mpeg4", "1280x720", 25, 50, ["-c:v", "mpeg4", "-bf", "2", "-g", "12", "-qscale", "8"], "psnr", "mandelbrot"),
    # Sharp, fast test pattern: a few macroblocks of B-VOPs decode differently from libavcodec in the engine (libnvcuvid
    # identical), so only libnvcuvid is the judge here.
    ("mpeg4-testsrc2", "mpeg4", "352x288", 25, 50, ["-c:v", "mpeg4", "-bf", "2", "-g", "12", "-qscale", "4"], "stock"),
    ("vp8-altref", "vp8", "350x286", 25, 100,
     ["-c:v", "libvpx", "-b:v", "500k", "-auto-alt-ref", "1", "-lag-in-frames", "10", "-g", "30"], "exact"),
    ("vp8-slices", "vp8", "854x480", 24, 96, ["-c:v", "libvpx", "-b:v", "1M", "-slices", "8", "-g", "48"], "exact"),
    ("vp8-1080p", "vp8", "1920x1080", 25, 50, ["-c:v", "libvpx", "-b:v", "4M", "-g", "25"], "exact"),
    ("vp8-keyframes", "vp8", "320x240", 25, 30, ["-c:v", "libvpx", "-g", "1"], "exact"),
]

# (samples.ffmpeg.org 下的路径, 模式, 最多解几帧)；"exact" 与软解逐字节，"psnr" 对软解 PSNR_FLOOR。
VC1_SAMPLES = [
    ("V-codecs/WMV9/interlaced/480i30__codec_WVC1__mode_2__framerate_29.970__type_2__preproc_17.wmv", "exact", 100),
    ("ffmpeg-bugs/roundup/issue948/planet-vc1-bframe-bitexact.mkv", "exact", 100),
    ("ffmpeg-bugs/roundup/issue948/saylent-vc1-bitexact.mkv", "exact", 60),
    ("ffmpeg-bugs/trac/ticket2723/wmv9adv.wmv", "exact", 100),
    ("ffmpeg-bugs/trac/ticket2993/vc1.wmv", "exact", 100),
    ("archive/all/asf+wmv3+wmav2++audio_stutter.wmv", "exact", 100),
    ("ffmpeg-bugs/trac/ticket5274/advanced_profile.vc1", "psnr", 40),
    ("ffmpeg-bugs/trac/ticket5274/advanced_profile_2.vc1", "psnr", 46),
]


def decode_raw(ffmpeg, clip, device, env, limit=None):
    """ffmpeg VA-API 解码并以 nv12 读回；失败返回 (None, 错误文字)。"""
    count = ["-frames:v", str(limit)] if limit else []
    result = subprocess.run(ffmpeg + ["-hwaccel", "vaapi", "-hwaccel_device", device, "-hwaccel_output_format", "vaapi",
                                      "-i", clip, "-vf", "hwdownload,format=nv12"] + count + ["-f", "rawvideo", "-"],
                            env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if result.returncode:
        return None, f"ffmpeg failed: {result.stderr.decode().strip().splitlines()[-1:]}"
    return result.stdout, None


def min_psnr(root, size, reference, data):
    """逐帧 PSNR 的最小值（dB）；逐字节相同为无穷。"""
    if reference == data:
        return float("inf")
    a, b = root / "ref.nv12", root / "hw.nv12"
    a.write_bytes(reference)
    b.write_bytes(data)
    log = subprocess.run(["ffmpeg", "-hide_banner", "-nostdin", "-f", "rawvideo", "-pix_fmt", "nv12", "-s", size, "-i", str(a),
                          "-f", "rawvideo", "-pix_fmt", "nv12", "-s", size, "-i", str(b), "-lavfi", "psnr", "-f", "null", "-"],
                         stderr=subprocess.PIPE, text=True).stderr
    value = [l for l in log.splitlines() if "PSNR" in l and "min:" in l][-1].split("min:")[1].split()[0]
    return float(value)


def stock_decode(ffmpeg, clip, decoder, env):
    """libnvcuvid（ffmpeg 的 *_cuvid 解码器）解出的 nv12；不可用返回 None。"""
    result = subprocess.run(ffmpeg + ["-c:v", decoder, "-i", clip, "-fps_mode", "passthrough", "-pix_fmt", "nv12",
                                      "-f", "rawvideo", "-"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    return None if result.returncode else result.stdout


def legacy_case(ffmpeg, root, clip, size, mode, device, env, hardware_env, c_env, codec=None, limit=None):
    """返回 (rustnv 错误或 None, 说明, C 版结果文字或 None)。"""
    count = ["-frames:v", str(limit)] if limit else []
    reference = run_bytes(ffmpeg + ["-i", clip] + count + ["-pix_fmt", "nv12", "-f", "rawvideo", "-"], env)
    hardware, error = decode_raw(ffmpeg, clip, device, hardware_env, limit)
    if error:
        return error, "", None
    if len(hardware) != len(reference):
        return f"{len(hardware)} bytes, expected {len(reference)}", "", None
    c_text = None
    if c_env is not None:
        c_data, c_error = decode_raw(ffmpeg, clip, device, c_env, limit)
        c_text = c_error or ("identical to rustnv" if c_data == hardware else "differs from rustnv")
    stock = ""
    if codec == "mpeg4":
        data = stock_decode(ffmpeg, clip, "mpeg4_cuvid", env)
        if data is None:
            stock = ", libnvcuvid unavailable"
        elif data != hardware:
            return "differs from libnvcuvid (mpeg4_cuvid)", "", c_text
        else:
            stock = ", bit-exact with libnvcuvid"
    if mode == "stock":
        return None, "frames decoded" + (stock or ", bit-exact with libnvcuvid"), c_text
    if mode == "exact":
        if hardware != reference:
            index = next(i for i in range(len(hardware)) if hardware[i] != reference[i])
            return f"first difference at byte {index}", "", c_text
        return None, "bit-exact with software", c_text
    psnr = min_psnr(root, size, reference, hardware)
    floor = PSNR_FLOOR_MPEG4 if codec == "mpeg4" else PSNR_FLOOR
    if psnr < floor:
        return f"PSNR {psnr:.2f} dB below {floor}", "", c_text
    return None, f"min PSNR {psnr:.2f} dB vs software{stock}", c_text


def run_bytes(args, env):
    return subprocess.run(args, env=env, check=True, stdout=subprocess.PIPE).stdout


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
    parser.add_argument("--codec", action="append", choices=sorted({c[1] for c in CASES + LEGACY_CASES} | {"vc1"}))
    parser.add_argument("--c-driver", action="store_true", help="the same clips through the installed C driver (reported only)")
    parser.add_argument("--only", action="append", help="case name (repeatable)")
    parser.add_argument("--fetch-samples", action="store_true", help="download missing VC-1 samples over http")
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
        for name, codec, size, rate, count, encoder, mode, *extra in LEGACY_CASES:
            if args.codec and codec not in args.codec or args.only and name not in args.only:
                continue
            clip = str(root / f"{name}.mkv")
            run(ffmpeg + ["-f", "lavfi", "-i", f"{extra[0] if extra else 'testsrc2'}=size={size}:rate={rate}", "-frames:v", str(count),
                          "-pix_fmt", "yuv420p"] + encoder + [clip], env)
            error, note, c_text = legacy_case(ffmpeg, root, clip, size, mode, args.device, env, hardware_env,
                                              c_env if args.c_driver else None, codec)
            if error:
                raise RuntimeError(f"{name}: {error}")
            print(f"PASS rustnv {name} {size}: {note}" + (f" (C driver: {c_text})" if c_text else ""), flush=True)
        if not args.codec or "vc1" in args.codec:
            for remote, mode, limit in VC1_SAMPLES:
                name = Path(remote).name
                if args.only and name not in args.only:
                    continue
                clip = SAMPLES / name
                if not clip.exists() and args.fetch_samples:
                    SAMPLES.mkdir(parents=True, exist_ok=True)
                    subprocess.run(["curl", "-sS", "-L", "--retry", "8", "--max-time", "300", "-o", str(clip) + ".part",
                                    SAMPLE_URL + remote.replace(" ", "%20")], check=True)
                    Path(str(clip) + ".part").rename(clip)
                if not clip.exists():
                    print(f"SKIP rustnv {name}: not in {SAMPLES} (--fetch-samples downloads {SAMPLE_URL}{remote})", flush=True)
                    continue
                size = run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height",
                            "-of", "csv=s=x:p=0", str(clip)], env).strip()
                error, note, c_text = legacy_case(ffmpeg, root, str(clip), size, mode, args.device, env, hardware_env,
                                                  c_env if args.c_driver else None, "vc1", limit)
                if error:
                    raise RuntimeError(f"{name}: {error}")
                print(f"PASS rustnv {name} {size}: {note}" + (f" (C driver: {c_text})" if c_text else ""), flush=True)


if __name__ == "__main__":
    main()
