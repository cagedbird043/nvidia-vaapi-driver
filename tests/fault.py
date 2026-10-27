#!/usr/bin/env python3
"""解码状态失败的注入用例：失败帧不能被当成成功帧交给任何消费者。

从 AV1 片段取 60 帧，把第 FIRST_BAD 帧起若干帧的 tile 数据中段改成随机字节（OBU 语法保持完整，
解析器照常接受，送到 NVDEC 后引擎报 error_status）。对每种消费路径比较「损坏码流」与「干净码流」的输出：

- readback：`hwdownload`（vaDeriveImage / vaGetImage）读回的每一帧必须与干净码流同 pts 的帧逐字节一致；
- encode：解码 surface 经导出 backing 与 copy engine 送进 h264_vaapi，编码结果逐帧与干净码流的同一转码一致；
- decode-only：输出留在 GPU（ffmpeg 不调 vaSyncSurface，只看提交路径），失败状态不能让后续提交挂死。

readback 经 `vaSyncSurface` 后 `vaDeriveImage`，即同步路径。

每条路径还要求驱动报出错误（ffmpeg 退出码或日志），且在超时内结束（无挂死）。

`--stress` 另跑压力用例（H.264 1080p 片段）：
- long：同一进程解码 + 读回 24000 帧，峰值 RSS 与 10800 帧相比增长不超过 STRESS_RSS_SLACK_KB（ffmpeg 自身在前一万帧内
  也会涨约 20 MB，之后持平，所以基准取 10800 帧）；
- concurrent：4 个进程同时转码（解码 → h264_vaapi），全部成功；
- teardown：20 次在解码中途（0.2–0.7 s）用 SIGINT（正常关闭，GPU 工作未完成时销毁 context / surface）
  或 SIGKILL 结束 ffmpeg，每次都在超时内结束，随后的完整解码仍然成功。
"""
import argparse
import os
import random
import re
import signal
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

FIRST_BAD, LAST_BAD = 13, 20
FRAMES = 60


def corrupt(src: Path, dst: Path):
    d = bytearray(src.read_bytes())
    off, n = 32, 0
    rnd = random.Random(7)
    while off + 12 <= len(d):
        size = struct.unpack_from("<I", d, off)[0]
        if FIRST_BAD <= n <= LAST_BAD and size > 200:
            for i in range(off + 12 + size * 55 // 100, off + 12 + size * 85 // 100):
                d[i] = rnd.randrange(256)
        off += 12 + size
        n += 1
    dst.write_bytes(d)


def framemd5(path: Path) -> dict:
    out = {}
    for line in path.read_text().splitlines():
        if line.startswith("#"):
            continue
        f = [x.strip() for x in line.split(",")]
        out[int(f[2])] = f[5]
    return out


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--device", default="/dev/dri/renderD129")
    p.add_argument("--driver-dir", type=Path, default=Path(__file__).resolve().parents[1] / "target/release")
    p.add_argument("--clip", type=Path, default=Path.home() / "Projects/waydroid-a17/ref/video/av1-1080p60.mp4")
    p.add_argument("--stress", action="store_true", help="also run the long / concurrent / teardown cases")
    p.add_argument("--stress-clip", type=Path, default=Path.home() / "Projects/waydroid-a17/ref/video/h264-1080p60.mp4")
    args = p.parse_args()
    env = dict(os.environ, LIBVA_DRIVERS_PATH=str(args.driver_dir), LIBVA_DRIVER_NAME="rustnv")
    for k in [k for k in env if k.startswith("NVD_")]:
        del env[k]
    hw = ["-hwaccel", "vaapi", "-hwaccel_device", args.device, "-hwaccel_output_format", "vaapi"]
    failures = []

    def report(name, ok, detail):
        print(("PASS " if ok else "FAIL ") + f"{name}: {detail}", flush=True)
        if not ok:
            failures.append(name)

    def ffmpeg(*a, timeout=120):
        try:
            return subprocess.run(["ffmpeg", "-nostdin", "-y", "-v", "error", *a], env=env, capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            return None

    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        clean, bad = tmp / "clean.ivf", tmp / "bad.ivf"
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", str(args.clip), "-frames:v", str(FRAMES), "-c", "copy", "-an", "-f", "ivf", str(clean)], check=True)
        corrupt(clean, bad)

        def run(name, inp, out, *chain):
            r = ffmpeg(*hw, "-i", str(inp), *chain, "-f", "framemd5", str(out))
            if r is None:
                report(name, False, "hung (timeout)")
                return None
            return r

        # readback
        chain = ["-vf", "hwdownload,format=nv12"]
        rc, rb = run("readback", clean, tmp / "c.md5", *chain), run("readback", bad, tmp / "b.md5", *chain)
        if rc is not None and rb is not None:
            c, b = framemd5(tmp / "c.md5"), framemd5(tmp / "b.md5")
            wrong = [pts for pts, h in b.items() if c.get(pts) != h]
            errored = rb.returncode != 0 or "error" in rb.stderr.lower()
            report("readback", len(c) == FRAMES and not wrong and errored and len(b) <= FIRST_BAD,
                   f"{len(b)} frames delivered (clean {len(c)}), {len(wrong)} differ from clean, driver error reported: {errored}")

        # encode：解码 surface 直接作编码输入；编码结果再软解比较
        for name, inp, out in (("clean", clean, tmp / "c.mkv"), ("bad", bad, tmp / "b.mkv")):
            r = ffmpeg(*hw, "-i", str(inp), "-c:v", "h264_vaapi", "-qp", "20", "-bf", "0", str(out))
            if r is None:
                report("encode", False, f"{name} hung (timeout)")
                break
            if name == "bad":
                enc_err = r.returncode != 0 or "error" in r.stderr.lower()
        else:
            ok = True
            for f in ("c", "b"):
                ok &= subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", str(tmp / f"{f}.mkv"), "-f", "framemd5", str(tmp / f"{f}e.md5")]).returncode == 0
            c, b = framemd5(tmp / "ce.md5"), framemd5(tmp / "be.md5")
            first = sorted(b)
            wrong = [i for i, pts in enumerate(first) if c.get(sorted(c)[i] if i < len(c) else -1) != b[pts]]
            report("encode", ok and len(c) == FRAMES and not wrong and enc_err and len(b) <= FIRST_BAD,
                   f"{len(b)} frames encoded (clean {len(c)}), {len(wrong)} differ from the clean transcode, driver error reported: {enc_err}")

        # decode-only：只提交，不同步
        r = ffmpeg(*hw, "-i", str(bad), "-f", "null", "-")
        if r is None:
            report("decode-only", False, "hung (timeout)")
        else:
            errored = r.returncode != 0 or "error" in r.stderr.lower()
            report("decode-only", errored, f"finished, driver error reported: {errored}")

    if args.stress:
        stress(args, env, hw, report)
    sys.exit(1 if failures else 0)


STRESS_RSS_SLACK_KB = 16 * 1024


def stress(args, env, hw, report):
    clip = str(args.stress_clip)

    def peak_rss(loops):
        cmd = ["/usr/bin/time", "-f", "MAXRSS %M", "ffmpeg", "-nostdin", "-v", "error", "-stream_loop", str(loops), *hw, "-i", clip,
               "-an", "-vf", "hwdownload,format=nv12", "-f", "null", "-"]
        try:
            r = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=600)
        except subprocess.TimeoutExpired:
            return None, "hung (timeout)"
        m = re.search(r"MAXRSS (\d+)", r.stderr)
        return (int(m.group(1)) if r.returncode == 0 and m else None), r.stderr[-300:]

    short, _ = peak_rss(8)
    long, err = peak_rss(19)
    ok = short is not None and long is not None and long - short <= STRESS_RSS_SLACK_KB
    report("long", ok, f"peak RSS 10800 frames {short} KiB, 24000 frames {long} KiB" if ok or long else err)

    procs = [subprocess.Popen(["ffmpeg", "-nostdin", "-v", "error", *hw, "-i", clip, "-an", "-c:v", "h264_vaapi", "-qp", "26", "-f", "null", "-"],
                              env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE) for _ in range(4)]
    codes = []
    for pr in procs:
        try:
            pr.wait(timeout=300)
            codes.append(pr.returncode)
        except subprocess.TimeoutExpired:
            pr.kill()
            codes.append("hung")
    report("concurrent", codes == [0] * 4, f"exit codes {codes}")

    rnd = random.Random(11)
    hung, broken = 0, 0
    for i in range(20):
        pr = subprocess.Popen(["ffmpeg", "-nostdin", "-v", "error", *hw, "-i", clip, "-an", "-vf", "hwdownload,format=nv12", "-f", "null", "-"],
                              env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(rnd.uniform(0.2, 0.7))
        pr.send_signal(signal.SIGINT if i % 2 == 0 else signal.SIGKILL)
        try:
            pr.wait(timeout=30)
        except subprocess.TimeoutExpired:
            pr.kill()
            hung += 1
        r = subprocess.run(["ffmpeg", "-nostdin", "-v", "error", *hw, "-i", clip, "-frames:v", "120", "-an", "-f", "null", "-"],
                           env=env, capture_output=True, timeout=120)
        broken += r.returncode != 0
    report("teardown", hung == 0 and broken == 0, f"20 interrupted runs: {hung} hung, {broken} later decodes failed")


if __name__ == "__main__":
    main()
