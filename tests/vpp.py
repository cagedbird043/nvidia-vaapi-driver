#!/usr/bin/env python3
"""VideoProc（VAProfileNone + VAEntrypointVideoProc）回归。

探针（vpp_probe.c，vaPutImage 上传、vaGetImage 读回）：
- 每种 surface 格式的同格式拷贝逐字节一致；
- 颜色解析：固定标准忽略 Explicit 字段、Explicit 只换矩阵、单侧未指定按继承、双侧未指定按高度选矩阵、
  RGB 与 YUV 互转的矩阵与 range 默认值、601/709/2020 × full/limited；纯色画面与公式比较（误差 ≤ 1）；
- 色度位置（左对齐 / 居中）、output_region 外的背景色（含透明 alpha）；
- 拒绝：滤镜参数缓冲与 num_filters 返回 VA_STATUS_ERROR_UNSUPPORTED_FILTER，已知且不同色域（BT.709 → BT.2020）的 primaries /
  transfer 返回错误；SDR 时代的原色（BT.601 ↔ BT.709）视为同一色域，只换矩阵。
ffmpeg scale_vaapi（统一按居中色度位置，与 swscale 一致）：放大与同尺寸变格式对 swscale bilinear 逐字节一致，
1.5 倍缩小 Y 的 PSNR 不低于 DOWNSCALE_FLOOR，4K → 1080p、1080p → 360p、P010、4:4:4、RGB 输入输出、
CPU 上传的输入、缩放后送 h264_vaapi 编码。
用法：vpp.py [--device /dev/dri/renderD129] [--clip 自然画面 1080p 片源] [--only 名称 ...]
"""
import argparse
import os
from pathlib import Path
import random
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
DOWNSCALE_FLOOR = 45.0
EXACT = 60.0  # ffmpeg 报 inf；留出取整差异之外的余量
UNSUPPORTED_FILTER, UNIMPLEMENTED = "0x20", "0x14"
KR_KB = {"601": (0.299, 0.114), "709": (0.2126, 0.0722), "2020": (0.2627, 0.0593)}
# H.273 matrix_coefficients
MATRIX_CODE = {"601": 6, "709": 1, "2020": 9}
VA_SOURCE_RANGE_REDUCED, VA_SOURCE_RANGE_FULL = 1, 2
STD_NONE, STD_BT601, STD_BT709, STD_BT2020, STD_EXPLICIT = 0, 1, 2, 12, 13
LEFT, CENTER = 0x4 | 0x2, 0x8 | 0x2  # VA_CHROMA_SITING_HORIZONTAL_* | VERTICAL_CENTER

# fourcc: (位深, 每样本字节, 平面) — 平面 "420" 为亮度加交错色度，"444" 为三平面，"rgb" 为一个平面
FORMATS = {
    "NV12": (8, 1, "420"), "P010": (10, 2, "420"), "P012": (12, 2, "420"),
    "444P": (8, 1, "444"), "Q416": (10, 2, "444"),
    "BGRA": (8, 4, "rgb"), "BGRX": (8, 4, "rgb"), "ARGB": (8, 4, "rgb"), "RGBA": (8, 4, "rgb"), "RGBX": (8, 4, "rgb"),
}


def raw_size(fourcc, w, h):
    _, bps, kind = FORMATS[fourcc]
    return {"420": w * h * bps * 3 // 2, "444": w * h * bps * 3, "rgb": w * h * 4}[kind]


def random_raw(fourcc, w, h, seed):
    bits, bps, _ = FORMATS[fourcc]
    rng = random.Random(seed)
    n = raw_size(fourcc, w, h)
    if bps != 2:
        return bytes(rng.getrandbits(8) for _ in range(n))
    out = bytearray()
    for _ in range(n // 2):
        out += ((rng.getrandbits(bits)) << (16 - bits)).to_bytes(2, "little")
    return bytes(out)


def flat_yuv(fourcc, w, h, yuv):
    """Flat 4:2:0 / 4:4:4 picture of codes (Y, Cb, Cr)."""
    bits, bps, kind = FORMATS[fourcc]
    sample = lambda v: (v << (16 - bits)).to_bytes(2, "little") if bps == 2 else bytes([v])
    y, u, v = (sample(c) for c in yuv)
    if kind == "420":
        return y * (w * h) + (u + v) * (w * h // 4)
    return y * (w * h) + u * (w * h) + v * (w * h)


def flat_rgb(fourcc, w, h, rgb, alpha=255):
    r, g, b = rgb
    px = bytes([r, g, b, alpha]) if fourcc.startswith("RGB") else bytes([b, g, r, alpha])
    return px * (w * h)


def rgb_at(fourcc, raw, w, x, y):
    i = (y * w + x) * 4
    p = raw[i:i + 4]
    return (p[0], p[1], p[2], p[3]) if fourcc.startswith("RGB") else (p[2], p[1], p[0], p[3])


def yuv_at(fourcc, raw, w, h, x, y):
    bits, bps, kind = FORMATS[fourcc]
    get = lambda i: (int.from_bytes(raw[i:i + 2], "little") >> (16 - bits)) if bps == 2 else raw[i]
    luma = get((y * w + x) * bps)
    if kind == "420":
        c = w * h * bps + ((y // 2) * w + (x // 2) * 2) * bps
        return luma, get(c), get(c + bps)
    return luma, get(w * h * bps + (y * w + x) * bps), get(2 * w * h * bps + (y * w + x) * bps)


def to_rgb(yuv, matrix, full, bits=8, exact=False):
    """Codes (Y, Cb, Cr) -> full-range 8-bit R, G, B (H.273)."""
    kr, kb = KR_KB[matrix]
    m = (1 << bits) - 1
    u = 1 << (bits - 8)
    if full:
        y, pb, pr = yuv[0] / m, (yuv[1] - (1 << (bits - 1))) / m, (yuv[2] - (1 << (bits - 1))) / m
    else:
        y, pb, pr = (yuv[0] - 16 * u) / (219 * u), (yuv[1] - 128 * u) / (224 * u), (yuv[2] - 128 * u) / (224 * u)
    r = y + 2 * (1 - kr) * pr
    b = y + 2 * (1 - kb) * pb
    g = (y - kr * r - kb * b) / (1 - kr - kb)
    if exact:  # R'G'B' in 0..1, unclipped: a YUV -> YUV conversion keeps out-of-gamut values
        return (r, g, b)
    return tuple(min(255, max(0, round(c * 255))) for c in (r, g, b))


def to_yuv(rgb, matrix, full, bits=8):
    """Full-range 8-bit R, G, B -> codes (Y, Cb, Cr)."""
    kr, kb = KR_KB[matrix]
    r, g, b = rgb if isinstance(rgb[0], float) else (c / 255 for c in rgb)
    y = kr * r + (1 - kr - kb) * g + kb * b
    pb, pr = (b - y) / (2 * (1 - kb)), (r - y) / (2 * (1 - kr))
    m = (1 << bits) - 1
    u = 1 << (bits - 8)
    if full:
        v = (y * m, pb * m + (1 << (bits - 1)), pr * m + (1 << (bits - 1)))
    else:
        v = (16 * u + 219 * u * y, 128 * u + 224 * u * pb, 128 * u + 224 * u * pr)
    return tuple(min(m, max(0, int(c + 0.5))) for c in v)


def close(a, b, tolerance=1):
    return all(abs(x - y) <= tolerance for x, y in zip(a, b))


class Probe:
    def __init__(self, exe, device, env, tmp):
        self.exe, self.device, self.env, self.tmp = exe, device, env, Path(tmp)

    def run(self, in_fourcc, iw, ih, raw, out_fourcc, ow, oh, *options):
        src, dst = self.tmp / "in.raw", self.tmp / "out.raw"
        src.write_bytes(raw)
        dst.unlink(missing_ok=True)
        r = subprocess.run([self.exe, self.device, in_fourcc, str(iw), str(ih), str(src), out_fourcc, str(ow), str(oh), str(dst), *options],
                           env=self.env, capture_output=True, text=True, timeout=60)
        status = r.stdout.strip().splitlines()[-1] if r.stdout.strip() else f"crash {r.returncode} {r.stderr.strip()}"
        return status, dst.read_bytes() if status == "OK" else None


def props(loc=0, rng=0, prim=0, trc=0, mat=0):
    return f"{loc},{rng},{prim},{trc},{mat}"


def probe_cases(p):
    """[(名称, 错误或 None)]"""
    results = []

    def case(name, check):
        try:
            error = check()
        except Exception as e:  # noqa: BLE001 — 报告而不是中断其余用例
            error = f"exception {e!r}"
        results.append((name, error))

    def copy(fourcc, w, h):
        raw = random_raw(fourcc, w, h, w * 7 + h)
        status, out = p.run(fourcc, w, h, raw, fourcc, w, h)
        if status != "OK":
            return status
        return None if out == raw else "output differs from input"

    for fourcc in FORMATS:
        for w, h in [(64, 48), (70, 38)]:
            case(f"copy {fourcc} {w}x{h}", lambda f=fourcc, w=w, h=h: copy(f, w, h))

    colours = [(180, 60, 200), (20, 128, 128), (235, 240, 16), (81, 90, 240)]

    def yuv_to_rgb(matrix, full, in_fourcc="NV12", height=48, explicit=True, expect_matrix=None, expect_full=None, extra=()):
        bits = FORMATS[in_fourcc][0]
        for yuv8 in colours:
            yuv = tuple(c << (bits - 8) for c in yuv8)
            options = [*extra]
            if explicit:
                options += [f"in_std={STD_EXPLICIT}", "in_props=" + props(LEFT, VA_SOURCE_RANGE_FULL if full else VA_SOURCE_RANGE_REDUCED, 2, 2, MATRIX_CODE[matrix])]
            status, out = p.run(in_fourcc, 64, height, flat_yuv(in_fourcc, 64, height, yuv), "BGRA", 64, height, *options)
            if status != "OK":
                return status
            want = to_rgb(yuv, expect_matrix or matrix, full if expect_full is None else expect_full, bits)
            got = rgb_at("BGRA", out, 64, 10, 10)
            if not close(got[:3], want) or got[3] != 255:
                return f"{yuv} -> {got}, want {want}"
        return None

    for matrix in KR_KB:
        for full in (False, True):
            case(f"NV12 -> BGRA {matrix} {'full' if full else 'limited'}", lambda m=matrix, f=full: yuv_to_rgb(m, f))
    case("P010 -> BGRA 2020 limited", lambda: yuv_to_rgb("2020", False, "P010"))
    case("Q416 -> BGRA 709 full", lambda: yuv_to_rgb("709", True, "Q416"))
    # 双侧未指定：YUV 输入按输入高度选矩阵（< 720 行 BT.601，≥ 720 行 BT.709），YUV limited、RGB full。
    case("unspecified YUV 480 rows -> BT.601", lambda: yuv_to_rgb("601", False, height=480, explicit=False))
    case("unspecified YUV 720 rows -> BT.709", lambda: yuv_to_rgb("709", False, height=720, explicit=False))
    # 固定标准下 Explicit 字段被忽略：填垃圾值结果不变。
    garbage = ["in_props=" + props(LEFT, 0, 9, 16, 9), "out_props=" + props(0, 0, 9, 16, 9)]
    case("fixed BT.709 ignores explicit fields",
         lambda: yuv_to_rgb("709", False, height=480, explicit=False, extra=[f"in_std={STD_BT709}", f"out_std={STD_BT709}", *garbage]))

    def rgb_to_yuv(height, matrix, out_fourcc="NV12", options=(), full=False):
        for rgb in [(255, 0, 0), (10, 200, 60), (128, 128, 128), (0, 0, 255)]:
            status, out = p.run("BGRA", 64, height, flat_rgb("BGRA", 64, height, rgb), out_fourcc, 64, height, *options)
            if status != "OK":
                return status
            bits = FORMATS[out_fourcc][0]
            want = to_yuv(rgb, matrix, full, bits)
            got = yuv_at(out_fourcc, out, 64, height, 10, 10)
            if not close(got, want):
                return f"{rgb} -> {got}, want {want}"
        return None

    # RGB 输入、YUV 输出未指定矩阵：按输出高度选；RGB 侧未指定 range 按 full。
    case("BGRA -> NV12 unspecified, 718 rows -> BT.601", lambda: rgb_to_yuv(718, "601"))
    case("BGRA -> NV12 unspecified, 720 rows -> BT.709", lambda: rgb_to_yuv(720, "709"))
    case("BGRA -> P010 explicit BT.2020 full",
         lambda: rgb_to_yuv(64, "2020", "P010", [f"out_std={STD_EXPLICIT}", "out_props=" + props(LEFT, VA_SOURCE_RANGE_FULL, 9, 14, 9)], True))

    def matrix_change():
        # Explicit：primaries / transfer 一致，只换矩阵与 range（709 limited -> 601 full）。
        options = [f"in_std={STD_EXPLICIT}", "in_props=" + props(LEFT, VA_SOURCE_RANGE_REDUCED, 1, 1, 1),
                   f"out_std={STD_EXPLICIT}", "out_props=" + props(LEFT, VA_SOURCE_RANGE_FULL, 1, 1, 6)]
        for yuv in colours:
            status, out = p.run("NV12", 64, 48, flat_yuv("NV12", 64, 48, yuv), "NV12", 64, 48, *options)
            if status != "OK":
                return status
            want = to_yuv(to_rgb(yuv, "709", False, exact=True), "601", True)
            got = yuv_at("NV12", out, 64, 48, 10, 10)
            if not close(got, want):
                return f"{yuv} -> {got}, want {want}"
        return None

    case("explicit matrix and range change", matrix_change)

    def inherit():
        # 输入 BT.2020 PQ（Explicit），输出未指定：继承，不当作颜色变换，画面不变。
        raw = flat_yuv("P010", 64, 48, (500, 400, 600))
        status, out = p.run("P010", 64, 48, raw, "P010", 64, 48, f"in_std={STD_EXPLICIT}", "in_props=" + props(LEFT, 1, 9, 16, 9))
        if status != "OK":
            return status
        return None if out == raw else "picture changed"

    case("one side unspecified inherits", inherit)

    def refused(expect, *options):
        status, _ = p.run("NV12", 64, 48, flat_yuv("NV12", 64, 48, (100, 120, 140)), "NV12", 64, 48, *options)
        return None if status.startswith("FAIL") and status.endswith(expect) else f"status {status}, want {expect}"

    def sdr_primaries():
        yuv = (180, 60, 200)
        status, out = p.run("NV12", 64, 48, flat_yuv("NV12", 64, 48, yuv), "NV12", 64, 48, f"in_std={STD_BT601}", f"out_std={STD_BT709}")
        if status != "OK":
            return status
        want = to_yuv(to_rgb(yuv, "601", False, exact=True), "709", False)
        got = yuv_at("NV12", out, 64, 48, 10, 10)
        return None if close(got, want) else f"{yuv} -> {got}, want {want}"

    case("BT.601 -> BT.709 is a matrix change", sdr_primaries)
    case("BT.709 -> BT.2020 (primaries) refused", lambda: refused(UNIMPLEMENTED, f"in_std={STD_BT709}", f"out_std={STD_BT2020}"))
    case("PQ -> SDR (transfer) refused", lambda: refused(UNIMPLEMENTED, f"in_std={STD_EXPLICIT}", "in_props=" + props(0, 0, 9, 16, 9),
                                                         f"out_std={STD_EXPLICIT}", "out_props=" + props(0, 0, 9, 14, 9)))
    case("filter parameter buffer refused", lambda: refused(UNSUPPORTED_FILTER, "filter=1"))
    case("num_filters refused", lambda: refused(UNSUPPORTED_FILTER, "filters=1"))

    def siting():
        # 色度隔列 64/192：左对齐时偶数列正好落在色度样本上，居中时是两样本的平均。
        w, h = 64, 16
        luma = bytes([128]) * (w * h)
        chroma = b"".join(bytes([64 if k % 2 == 0 else 192, 128]) for k in range(w // 2)) * (h // 2)
        for loc, want in [(LEFT, (64, 192)), (CENTER, (96, 160))]:
            status, out = p.run("NV12", w, h, luma + chroma, "444P", w, h, f"in_std={STD_EXPLICIT}", "in_props=" + props(loc, 1, 1, 1, 1))
            if status != "OK":
                return status
            got = (yuv_at("444P", out, w, h, 8, 4)[1], yuv_at("444P", out, w, h, 10, 4)[1])
            if not close(got, want):
                return f"siting {loc:#x}: Cb at x=8,10 {got}, want {want}"
        return None

    case("chroma siting left / centre", siting)

    def region(out_fourcc, bg, inside_want, outside_want):
        w, h = 64, 48
        status, out = p.run("BGRA", w, h, flat_rgb("BGRA", w, h, (200, 100, 50)), out_fourcc, w, h,
                            "dst=8,6,32,20", f"bg={bg:#010x}")
        if status != "OK":
            return status
        read = (lambda x, y: rgb_at(out_fourcc, out, w, x, y)) if FORMATS[out_fourcc][2] == "rgb" else (lambda x, y: yuv_at(out_fourcc, out, w, h, x, y))
        for (x, y), want in [((20, 15), inside_want), ((2, 2), outside_want), ((50, 40), outside_want), ((39, 25), inside_want), ((40, 26), outside_want)]:
            got = read(x, y)
            if not close(got, want):
                return f"({x},{y}) {got}, want {want}"
        return None

    case("output region, transparent background (RGBA)", lambda: region("RGBA", 0x00102030, (200, 100, 50, 255), (0x10, 0x20, 0x30, 0)))
    case("output region, opaque white background (NV12)", lambda: region("NV12", 0xffffffff, to_yuv((200, 100, 50), "601", False), (235, 128, 128)))
    return results


def psnr(a, b, w, h, pix_fmt, component="average"):
    out = subprocess.run(["ffmpeg", "-hide_banner", "-f", "rawvideo", "-pix_fmt", pix_fmt, "-s", f"{w}x{h}", "-i", a,
                          "-f", "rawvideo", "-pix_fmt", pix_fmt, "-s", f"{w}x{h}", "-i", b,
                          "-lavfi", "[0]format=gbrp[x];[1]format=gbrp[y];[x][y]psnr" if pix_fmt in ("bgra", "rgba") else "psnr",
                          "-f", "null", "-"], capture_output=True, text=True).stderr
    m = re.search(rf"{component}:(\S+)", out)
    return float("inf") if m and m.group(1) == "inf" else float(m.group(1)) if m else 0.0


def ffmpeg_cases(device, env, tmp, clip):
    tmp = Path(tmp)
    hw = ["-init_hw_device", f"vaapi=va:{device}", "-hwaccel", "vaapi", "-hwaccel_device", "va", "-hwaccel_output_format", "vaapi"]
    up = ["-init_hw_device", f"vaapi=va:{device}", "-filter_hw_device", "va"]
    q = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y"]
    clean = {k: v for k, v in env.items() if not k.startswith("LIBVA_")}
    run = lambda cmd, e=env: subprocess.run(cmd, env=e, capture_output=True, text=True, timeout=600)
    results = []
    sources = {}

    def source(name, args):
        if name not in sources:
            path = tmp / name
            r = run(q + args + [str(path)], clean)
            if r.returncode != 0:
                raise RuntimeError(r.stderr.strip()[-300:])
            sources[name] = str(path)
        return sources[name]

    def compare(name, src, vf_hw, vf_sw, w, h, pix_fmt, floor, frames=60, hwargs=None, component="average"):
        o, r = tmp / "o.raw", tmp / "r.raw"
        a = run(q + (hwargs or hw) + ["-i", src, "-frames:v", str(frames), "-vf", vf_hw, "-f", "rawvideo", str(o)])
        if a.returncode != 0:
            return results.append((name, a.stderr.strip()[-300:]))
        b = run(q + ["-i", src, "-frames:v", str(frames), "-vf", vf_sw, "-pix_fmt", pix_fmt, "-f", "rawvideo", str(r)], clean)
        if b.returncode != 0:
            return results.append((name, "reference: " + b.stderr.strip()[-300:]))
        frame = {"nv12": w * h * 3 // 2, "p010le": w * h * 3, "bgra": w * h * 4, "rgba": w * h * 4}[pix_fmt]
        if o.stat().st_size != frames * frame:
            return results.append((name, f"{o.stat().st_size} bytes, want {frames} frames of {frame}"))
        value = psnr(str(o), str(r), w, h, pix_fmt, component)
        results.append((name, None if value >= floor else f"PSNR {component} {value:.2f} < {floor}"))
        print(f"  {name}: PSNR {component} {value:.2f}")

    natural = clip if clip and Path(clip).exists() else source("testsrc2.mp4", ["-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=30", "-frames:v", "60", "-c:v", "libx264", "-crf", "16"])
    center = "setparams=chroma_location=center,"
    compare("1080p -> 1440p (upscale) = swscale", natural, center + "scale_vaapi=w=2560:h=1440,hwdownload,format=nv12", "scale=2560:1440:flags=bilinear", 2560, 1440, "nv12", EXACT)
    compare("1080p NV12 -> BGRA = swscale", natural, center + "scale_vaapi=format=bgra:out_range=full,hwdownload,format=bgra",
            "scale=in_color_matrix=bt709:in_range=tv:out_range=pc:flags=bilinear+accurate_rnd+full_chroma_int+full_chroma_inp,format=bgra", 1920, 1080, "bgra", EXACT, 30)
    compare("1080p NV12 -> P010 = swscale", natural, "scale_vaapi=format=p010,hwdownload,format=p010le", "scale=flags=bilinear+accurate_rnd", 1920, 1080, "p010le", EXACT)
    compare("1080p -> 720p (Y)", natural, center + "scale_vaapi=w=1280:h=720,hwdownload,format=nv12", "scale=1280:720:flags=bilinear", 1280, 720, "nv12", DOWNSCALE_FLOOR, component="y")
    compare("1080p -> 360p", natural, center + "scale_vaapi=w=640:h=360,hwdownload,format=nv12", "scale=640:360:flags=bilinear", 640, 360, "nv12", 33.0)
    four_k = source("src4k.mp4", ["-i", natural, "-frames:v", "30", "-vf", "scale=3840:2160", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "16"])
    compare("4K -> 1080p", four_k, center + "scale_vaapi=w=1920:h=1080,hwdownload,format=nv12", "scale=1920:1080:flags=bilinear", 1920, 1080, "nv12", 33.0, 30)
    ten = source("src10.mp4", ["-i", natural, "-frames:v", "30", "-c:v", "libx265", "-pix_fmt", "yuv420p10le", "-preset", "ultrafast", "-x265-params", "log-level=error"])
    compare("HEVC 10-bit P010 -> 720p NV12", ten, center + "scale_vaapi=w=1280:h=720:format=nv12,hwdownload,format=nv12", "scale=1280:720:flags=bilinear", 1280, 720, "nv12", 40.0, 30)
    full = source("src444.mp4", ["-i", natural, "-frames:v", "30", "-c:v", "libx265", "-pix_fmt", "yuv444p", "-preset", "ultrafast", "-x265-params", "log-level=error"])
    compare("HEVC 4:4:4 -> NV12", full, center + "scale_vaapi=format=nv12,hwdownload,format=nv12", "scale=flags=bilinear+accurate_rnd", 1920, 1080, "nv12", 40.0, 30)
    compare("CPU upload NV12 -> 360p", natural, "format=nv12," + center + "hwupload,scale_vaapi=w=640:h=360,hwdownload,format=nv12", "scale=640:360:flags=bilinear", 640, 360, "nv12", 33.0, 30, up)
    compare("CPU upload BGRA -> NV12", natural, "format=bgra,hwupload,scale_vaapi=format=nv12:out_color_matrix=bt709:out_range=limited,hwdownload,format=nv12",
            "format=bgra,scale=out_color_matrix=bt709:out_range=tv:flags=bilinear+accurate_rnd,format=nv12", 1920, 1080, "nv12", 40.0, 30, up)
    compare("1080p -> RGBA 720p", natural, center + "scale_vaapi=w=1280:h=720:format=rgba:out_range=full,hwdownload,format=rgba",
            "scale=1280:720:in_color_matrix=bt709:in_range=tv:out_range=pc:flags=bilinear+accurate_rnd+full_chroma_int+full_chroma_inp,format=rgba", 1280, 720, "rgba", 35.0, 30)

    # 缩放后的 surface 直接进编码器。
    out = tmp / "enc.mp4"
    a = run(q + hw + ["-i", natural, "-frames:v", "60", "-vf", "scale_vaapi=w=1280:h=720", "-c:v", "h264_vaapi", "-qp", "20", str(out)])
    if a.returncode != 0:
        results.append(("scale_vaapi -> h264_vaapi", a.stderr.strip()[-300:]))
    else:
        n = run(["ffprobe", "-v", "error", "-count_frames", "-show_entries", "stream=nb_read_frames,width,height", "-of", "csv=p=0", str(out)], clean).stdout.strip()
        results.append(("scale_vaapi -> h264_vaapi", None if n == "1280,720,60" else f"stream {n}"))
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--device", default="/dev/dri/renderD129")
    parser.add_argument("--driver-dir", default=str(ROOT / "target/release"))
    parser.add_argument("--clip", default=str(Path.home() / "Projects/waydroid-a17/ref/video/h264-1080p60.mp4"))
    parser.add_argument("--only", choices=["probe", "ffmpeg"])
    args = parser.parse_args()
    env = {k: v for k, v in os.environ.items() if k not in ("NVD_LOG", "NVD_LOG_VERBOSE")}
    env.update(LIBVA_DRIVERS_PATH=args.driver_dir, LIBVA_DRIVER_NAME="rustnv")
    with tempfile.TemporaryDirectory(prefix="rustnv-vpp-") as tmp:
        results = []
        if args.only in (None, "probe"):
            exe = str(Path(tmp) / "vpp_probe")
            flags = subprocess.run(["pkg-config", "--cflags", "--libs", "libva", "libva-drm"], capture_output=True, text=True, check=True).stdout.split()
            subprocess.run(["gcc", "-O1", "-o", exe, str(Path(__file__).with_name("vpp_probe.c")), *flags], check=True)
            results += probe_cases(Probe(exe, args.device, env, tmp))
        if args.only in (None, "ffmpeg"):
            results += ffmpeg_cases(args.device, env, tmp, args.clip)
    failed = [(n, e) for n, e in results if e]
    for name, error in results:
        print(f"{'FAIL' if error else 'ok  '} {name}{': ' + error if error else ''}")
    print(f"{len(results) - len(failed)}/{len(results)} passed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
