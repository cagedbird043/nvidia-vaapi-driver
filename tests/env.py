#!/usr/bin/env python3
"""与 C 版驱动对齐的环境变量与入口行为回归（只用 rustnv，无需 C 驱动）。

NVD_LOG / NVD_LOG_VERBOSE（默认静默）、NVD_GPU、NVD_MAX_INSTANCES、非 NVIDIA 显示设备时回落到 NVIDIA 渲染节点、
解码最大尺寸属性（H.264 4096，HEVC/VP9/AV1 8192）、vaCreateConfig 忽略 RTFormat 以外的属性。
用法：env.py [--device /dev/dri/renderD129] [--driver .../librustnv_drv_video.so]
"""
import argparse
import ctypes as c
import os
from pathlib import Path
import subprocess
import sys
import tempfile

VAProfileH264Main, VAProfileHEVCMain, VAProfileVP9Profile0, VAProfileAV1Profile0 = 6, 17, 19, 32
VAEntrypointVLD = 1
VAConfigAttribRTFormat, VAConfigAttribDecSliceMode, VAConfigAttribMaxPictureWidth = 0, 6, 18
VA_STATUS_ERROR_HW_BUSY = 0x22
VA_RT_FORMAT_YUV420 = 1


class Attribute(c.Structure):
    _fields_ = [("type", c.c_int), ("value", c.c_uint)]


def libs():
    va = c.CDLL("libva.so.2")
    drm = c.CDLL("libva-drm.so.2")
    drm.vaGetDisplayDRM.argtypes = [c.c_int]
    drm.vaGetDisplayDRM.restype = c.c_void_p
    va.vaInitialize.argtypes = [c.c_void_p, c.POINTER(c.c_int), c.POINTER(c.c_int)]
    va.vaTerminate.argtypes = [c.c_void_p]
    va.vaGetConfigAttributes.argtypes = [c.c_void_p, c.c_int, c.c_int, c.POINTER(Attribute), c.c_int]
    va.vaCreateConfig.argtypes = [c.c_void_p, c.c_int, c.c_int, c.POINTER(Attribute), c.c_int, c.POINTER(c.c_uint)]
    va.vaDestroyConfig.argtypes = [c.c_void_p, c.c_uint]
    return va, drm


def child_instances(device):
    """打开两个 VA display：NVD_MAX_INSTANCES=1 时第二个必须得到 HW_BUSY，关掉第一个后又能打开。"""
    va, drm = libs()
    fds = [os.open(device, os.O_RDWR) for _ in range(3)]
    displays = [drm.vaGetDisplayDRM(fd) for fd in fds]
    ver = (c.c_int(), c.c_int())
    first = va.vaInitialize(displays[0], *map(c.byref, ver))
    second = va.vaInitialize(displays[1], *map(c.byref, ver))
    va.vaTerminate(displays[0])
    third = va.vaInitialize(displays[2], *map(c.byref, ver))
    print(first, second, third)
    sys.exit(0)


def child_config(device):
    """各 profile 的最大尺寸属性，以及携带 DecSliceMode 的 vaCreateConfig。"""
    va, drm = libs()
    display = drm.vaGetDisplayDRM(os.open(device, os.O_RDWR))
    ver = (c.c_int(), c.c_int())
    assert va.vaInitialize(display, *map(c.byref, ver)) == 0
    sizes = {}
    for name, profile in (("h264", VAProfileH264Main), ("hevc", VAProfileHEVCMain), ("vp9", VAProfileVP9Profile0), ("av1", VAProfileAV1Profile0)):
        attr = (Attribute * 1)(Attribute(VAConfigAttribMaxPictureWidth, 0))
        assert va.vaGetConfigAttributes(display, profile, VAEntrypointVLD, attr, 1) == 0
        sizes[name] = attr[0].value
    attrs = (Attribute * 2)(Attribute(VAConfigAttribRTFormat, VA_RT_FORMAT_YUV420), Attribute(VAConfigAttribDecSliceMode, 1))
    config = c.c_uint()
    status = va.vaCreateConfig(display, VAProfileH264Main, VAEntrypointVLD, attrs, 2, c.byref(config))
    if status == 0:
        va.vaDestroyConfig(display, config)
    print(sizes["h264"], sizes["hevc"], sizes["vp9"], sizes["av1"], status)
    sys.exit(0)


def child_sync(device):
    """vaSyncSurface2 on an idle surface: 0 (query only) and u64::MAX (wait) both succeed at once."""
    va, drm = libs()
    display = drm.vaGetDisplayDRM(os.open(device, os.O_RDWR))
    ver = (c.c_int(), c.c_int())
    assert va.vaInitialize(display, *map(c.byref, ver)) == 0
    surface = c.c_uint()
    va.vaCreateSurfaces.argtypes = [c.c_void_p, c.c_uint, c.c_uint, c.c_uint, c.POINTER(c.c_uint), c.c_uint, c.c_void_p, c.c_uint]
    assert va.vaCreateSurfaces(display, VA_RT_FORMAT_YUV420, 64, 64, c.byref(surface), 1, None, 0) == 0
    va.vaSyncSurface2.argtypes = [c.c_void_p, c.c_uint, c.c_uint64]
    print(va.vaSyncSurface2(display, surface, 0), va.vaSyncSurface2(display, surface, 2**64 - 1), va.vaSyncSurface2(display, surface, 1000))
    sys.exit(0)


def nvidia_nodes():
    nodes = []
    for node in sorted(Path("/dev/dri").glob("renderD*")):
        rdev = node.stat().st_rdev
        uevent = Path(f"/sys/dev/char/{os.major(rdev)}:{os.minor(rdev)}/device/uevent").read_text()
        nodes.append((str(node), "DRIVER=nvidia" in uevent.splitlines()))
    return nodes


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--device", default="/dev/dri/renderD129")
    parser.add_argument("--driver", type=Path, default=Path(__file__).resolve().parents[1] / "target/release/librustnv_drv_video.so")
    parser.add_argument("--child")
    args = parser.parse_args()
    if args.child == "instances":
        child_instances(args.device)
    if args.child == "config":
        child_config(args.device)
    if args.child == "sync":
        child_sync(args.device)

    with tempfile.TemporaryDirectory() as tmp:
        link = Path(tmp) / "rustnv_drv_video.so"
        link.symlink_to(args.driver.resolve())
        base = {k: v for k, v in os.environ.items() if not k.startswith("NVD_")}
        base.update(LIBVA_DRIVERS_PATH=tmp, LIBVA_DRIVER_NAME="rustnv")
        failures = []

        def check(name, ok, detail=""):
            print(("PASS " if ok else "FAIL ") + name + (f": {detail}" if detail and not ok else ""))
            if not ok:
                failures.append(name)

        def vainfo(device, **extra):
            return subprocess.run(["vainfo", "--display", "drm", "--device", device], env={**base, **extra}, capture_output=True, text=True)

        quiet = vainfo(args.device)
        check("default is silent", quiet.returncode == 0 and "[rustnv]" not in quiet.stdout + quiet.stderr, quiet.stdout + quiet.stderr)

        out = vainfo(args.device, NVD_LOG="1")
        check("NVD_LOG=1 logs to stdout", "[rustnv]" in out.stdout and "initialising on PCI" in out.stdout, out.stdout)
        check("NVD_LOG=1 without verbose has no per-call trace", "query_profiles" not in out.stdout)

        logfile = Path(tmp) / "nvd.log"
        vainfo(args.device, NVD_LOG=str(logfile), NVD_LOG_VERBOSE="1")
        text = logfile.read_text() if logfile.exists() else ""
        check("NVD_LOG=<path> appends to the file with NVD_LOG_VERBOSE trace", "initialising on PCI" in text and "query_profiles" in text, text)
        before = len(text)
        vainfo(args.device, NVD_LOG=str(logfile))
        check("NVD_LOG appends", len(logfile.read_text()) > before)

        bad = vainfo(args.device, NVD_GPU="99")
        check("NVD_GPU beyond the last NVIDIA GPU fails init", bad.returncode != 0)
        nodes = nvidia_nodes()
        nvidia = [n for n, is_nvidia in nodes if is_nvidia]
        good = vainfo(args.device, NVD_GPU="0")
        check("NVD_GPU=0 selects the first NVIDIA GPU", good.returncode == 0 and "VAEntrypointVLD" in good.stdout, good.stderr)
        other = [n for n, is_nvidia in nodes if not is_nvidia]
        if other:
            res = vainfo(other[0], NVD_LOG="1")
            check("display on another vendor's GPU falls back to the first NVIDIA GPU",
                  res.returncode == 0 and f"NVIDIA GPU 0 is {nvidia[0]}" in res.stdout, res.stdout + res.stderr)
        else:
            print("SKIP fallback from another vendor's GPU: no non-NVIDIA render node")

        inst = subprocess.run([sys.executable, __file__, "--child", "instances", "--device", args.device], env={**base, "NVD_MAX_INSTANCES": "1"}, capture_output=True, text=True)
        check("NVD_MAX_INSTANCES=1 refuses the second display with HW_BUSY and frees the slot on vaTerminate",
              inst.stdout.split() == ["0", str(VA_STATUS_ERROR_HW_BUSY), "0"], inst.stdout + inst.stderr)
        inst = subprocess.run([sys.executable, __file__, "--child", "instances", "--device", args.device], env=base, capture_output=True, text=True)
        check("no instance limit by default", inst.stdout.split() == ["0", "0", "0"], inst.stdout + inst.stderr)

        cfg = subprocess.run([sys.executable, __file__, "--child", "config", "--device", args.device], env=base, capture_output=True, text=True)
        check("decode max picture size 4096 (H.264) / 8192 (HEVC, VP9, AV1) and extra config attributes ignored",
              cfg.stdout.split() == ["4096", "8192", "8192", "8192", "0"], cfg.stdout + cfg.stderr)

        sync = subprocess.run([sys.executable, __file__, "--child", "sync", "--device", args.device], env=base, capture_output=True, text=True)
        check("vaSyncSurface2 on an idle surface succeeds for every timeout", sync.stdout.split() == ["0", "0", "0"], sync.stdout + sync.stderr)
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
