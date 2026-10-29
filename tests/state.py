#!/usr/bin/env python3
"""Surface 画面位置的状态转移（tests/surface_state.c）。

解码、vaPutImage、VideoProc 输出、可写导出后的 EGL 写入两两先后发生，之后 vaGetImage、vaDeriveImage
与 VideoProc（编码走的同一条 GPU 读取路径）都必须读到后发生的那一个；可写导出后的 vaPutImage 必须被拒绝。
默认加载 debug 构建：它在每次 VA 调用后检查 surface 状态不变量，违反即 abort，所以先 `cargo build`。
用法：state.py [--device /dev/dri/renderD129] [--driver-dir target/debug]
"""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--device", default="/dev/dri/renderD129")
    parser.add_argument("--driver-dir", type=Path, default=ROOT / "target/debug")
    args = parser.parse_args()
    library = args.driver_dir / "librustnv_drv_video.so"
    if not library.exists():
        sys.exit(f"{library} missing: cargo build first")
    with tempfile.TemporaryDirectory(prefix="rustnv-state-") as tmp:
        # libva looks for <name>_drv_video.so.
        (Path(tmp) / "rustnv_drv_video.so").symlink_to(library.resolve())
        exe = Path(tmp) / "surface_state"
        flags = subprocess.run(["pkg-config", "--cflags", "--libs", "egl", "glesv2", "gbm", "libavcodec", "libavutil", "libva"],
                               capture_output=True, text=True, check=True).stdout.split()
        subprocess.run(["gcc", "-O1", "-Wall", "-o", str(exe), str(Path(__file__).with_name("surface_state.c")), *flags], check=True)
        env = {k: v for k, v in os.environ.items() if k not in ("NVD_LOG", "NVD_LOG_VERBOSE")}
        env.update(LIBVA_DRIVERS_PATH=tmp, LIBVA_DRIVER_NAME="rustnv")
        r = subprocess.run([str(exe), args.device], env=env, capture_output=True, text=True, timeout=120)
    lines = r.stdout.strip().splitlines()
    print("\n".join(lines))
    if r.returncode != 0:
        # An invariant abort cuts stdout short; its reason is on stderr.
        print("\n".join(l for l in r.stderr.splitlines() if l.startswith("rustnv:") or "FAIL" in l) or f"exit {r.returncode}")
    passed = sum(l.startswith("ok ") for l in lines)
    print(f"{passed}/{len(lines)} passed" + ("" if r.returncode == 0 else f", exit {r.returncode}"))
    sys.exit(0 if r.returncode == 0 else 1)


if __name__ == "__main__":
    main()
