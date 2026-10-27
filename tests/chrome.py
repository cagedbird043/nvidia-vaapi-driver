#!/usr/bin/env python3
"""Chrome WebCodecs 用例：解码出的每一帧都是该帧，不是旧帧或别的帧。

Chrome 只在导出 surface 时调一次 vaSyncSurface，之后靠 dma-buf 本身判断解码是否完成；导出 backing 里的画面
必须在 vaEndPicture 返回时就绪。页面 `tests/web/webcodecs.html` 在每帧画面里画 8 位帧号，用例：

- source：软件解码，校验帧号读取本身；
- decode round 0-2：硬解（多轮，新解码器复用 surface id，旧画面会显示成上一轮的帧）；
- canvas -> encode：canvas 帧硬编，软解校验；
- decode -> encode、decode -> crop -> encode：硬解帧逐帧直接交给硬编（Chrome 经导出 backing 送入 NVENC），软解校验。

需要 Wayland 会话与 google-chrome-stable；Chrome 用临时 profile，不读用户配置。
"""
import argparse
import functools
import http.server
import os
import re
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

FEATURES = ",".join([
    "VaapiOnNvidiaGPUs", "VaapiIgnoreDriverChecks", "AcceleratedVideoEncoder", "VaapiVideoEncoder",
    "AcceleratedVideoDecodeLinuxGL", "AcceleratedVideoDecodeLinuxZeroCopyGL",
])


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--device", default="/dev/dri/renderD129")
    p.add_argument("--driver-dir", type=Path, default=Path(__file__).resolve().parents[1] / "target/release",
                   help="directory with rustnv_drv_video.so; 'system' uses the installed driver")
    p.add_argument("--chrome", default="google-chrome-stable")
    p.add_argument("--timeout", type=int, default=150)
    args = p.parse_args()

    env = {k: v for k, v in os.environ.items() if not k.startswith(("NVD_", "LIBVA_"))}
    if str(args.driver_dir) != "system":
        env.update(LIBVA_DRIVERS_PATH=str(args.driver_dir.resolve()), LIBVA_DRIVER_NAME="rustnv")

    class Quiet(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *a):
            pass

    handler = functools.partial(Quiet, directory=str(Path(__file__).parent / "web"))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_address[1]}/webcodecs.html"

    with tempfile.TemporaryDirectory() as profile:
        cmd = [args.chrome, f"--user-data-dir={profile}", "--no-first-run", "--no-default-browser-check",
               "--ozone-platform=wayland", "--disable-gpu-sandbox", f"--render-node-override={args.device}",
               "--ignore-gpu-blocklist", f"--enable-features={FEATURES}", "--enable-logging=stderr", url]
        proc = subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
                                errors="replace")
        cases, done = [], False
        timer = threading.Timer(args.timeout, proc.kill)
        timer.start()
        for line in proc.stderr:
            m = re.search(r'"WCTEST (CASE .*|DONE)", source', line)
            if not m:
                continue
            if m.group(1) == "DONE":
                done = True
                break
            cases.append(m.group(1)[5:])
            print(cases[-1], flush=True)
        timer.cancel()
        proc.terminate()
        proc.wait()
    server.shutdown()

    failed = [c for c in cases if " FAIL " in c or c.endswith(" FAIL")]
    if not done:
        print(f"FAIL page did not finish within {args.timeout} s")
    print(f"{len(cases) - len(failed)}/{len(cases)} passed")
    sys.exit(0 if done and cases and not failed else 1)


if __name__ == "__main__":
    main()
