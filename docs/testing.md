# 回归测试与验证结果

所有脚本都不安装或修改系统驱动：它们在子进程内用 `LIBVA_DRIVERS_PATH` / `LIBVA_DRIVER_NAME` 选择 rustnv。先 `cargo build --release`；脚本默认加载 `target/release/librustnv_drv_video.so`（`--driver` 可改），默认设备 `/dev/dri/renderD129`（`--device` 可改，用 `vainfo`/`ls /dev/dri` 确认 NVIDIA 的渲染节点）。

脚本需要 Python 3、C 编译器、FFmpeg（libx264、libx265、libvpx、libsvtav1、libaom）、`vainfo`；对照 C 版时还需要另行安装的 C 版 `nvidia_drv_video.so`（与本驱动的安装名相同，二者不能同时装在 `/usr/lib/dri`；`tests/encode.py` 的 `--reference-library` 指向放有 C 版 `.so` 的目录，默认取系统驱动目录）。`tests/` 下的 `*.c` 是脚本自动编译的辅助程序（`va_dump.c`、`va_rt420.c`、`dmabuf_egl.c`、`dmabuf_import.c`、`dmabuf_export.c`）。

## 脚本

| 脚本 | 内容 |
|---|---|
| `tests/decode.py` | 全部解码格式的逐帧比较（`--codec av1` 单个格式；`--c-driver` 同时跑 C 版；`--fetch-samples` 下载 VC-1 样片） |
| `tests/encode.py` | 编码回归、属性对照、零拷贝输入、稳定性（`--quick --skip-stability` 快速） |
| `tests/env.py` | 环境变量与兼容行为 |
| `tests/gstreamer.py` | GStreamer `va` 元素解码六种格式、编码三种格式、`vapostproc` 缩放与变格式（需要 `GST_VA_ALL_DRIVERS=1`，脚本已设置） |
| `tests/vpp.py` | VideoProc：直接 VA 客户端 `tests/vpp_probe.c` 的像素级单项与 ffmpeg `scale_vaapi` 对照（`--only probe` / `--only ffmpeg`） |
| `tests/fault.py` | 解码状态失败注入：坏帧不交给读回、编码与同步；`--stress` 加长跑、并发 4 进程、中途中断 |
| `tests/chrome.py` | Chrome WebCodecs：硬解、硬编、硬解直送硬编（含裁剪）的每一帧按画面里的帧号校验，不能是旧帧或错帧；`--driver-dir system` 测已安装驱动 |

`cargo test --release` 只跑无需 GPU 的单元测试。

## 解码回归

```sh
python3 tests/decode.py                # 全部格式
python3 tests/decode.py --codec av1    # 单个格式
```

脚本生成临时测试视频，逐帧比较软件解码与 VA-API 下载结果的 NV12 哈希，并检查包含尾帧在内的精确帧数，失败即退出。

2026-10-26 实测：

| 用例 | 覆盖 | 结果 |
|---|---|---|
| h264-high 1280×720 | 3 B 帧、surface 池循环 | 480/480 逐字节一致 |
| h264-main-slices 854×480 | 2 B 帧、4 slices、宏块补齐 | 96/96 |
| h264-baseline 640×360 | 无 B 帧 | 90/90 |
| hevc-main 1280×720 | 4 B 帧 | 240/240 |
| hevc-slices-crop 854×480 | 4 slices、CTB 补齐 | 96/96 |
| vp9-altref 1280×720 | 隐藏 altref 帧 | 240/240 |
| vp9-tiles-crop 854×480 | tile 列、超块补齐 | 96/96 |
| av1-main 1280×720 | SVT-AV1 | 240/240 |
| av1-tiles-crop 854×480 | libaom、2×2 tiles | 96/96 |


高位深与 4:4:4（`tests/decode.py`，10 例，`--c-driver` 同时跑 C 版）：10/12-bit 与 4:4:4 样片用 `tests/va_dump.c`
（libavcodec VA-API，`vaGetImage` 与 `vaDeriveImage` 各读一遍，不转换像素格式）与 ffmpeg 软解逐字节比较，Q416 与软解左移到高位后比较；
再由 `tests/dmabuf_egl.c` 把 `vaExportSurfaceHandle` 导出的分层经 EGL（GBM，NVIDIA）导入读回，与 `vaGetImage` 比较。

| 用例 | 覆盖 | 结果 |
|---|---|---|
| hevc-main10 1280×720 | 4 B 帧 | 240/240 逐字节一致，EGL 一致 |
| hevc-main10-slices-crop 850×478 | 4 slices、非对齐 | 96/96 |
| hevc-main12 854×480 | 12-bit | 96/96 |
| hevc-444-8 850×478 | 444P | 96/96 |
| hevc-444-10 1280×720 | Q416 | 120/120 |
| hevc-444-12-slices 850×478 | 3 slices、Q416 | 96/96 |
| vp9-p2-10 1280×720 | altref | 240/240 |
| vp9-p2-12-tiles 850×478 | tile 列 | 96/96 |
| av1-10 1280×720 | SVT-AV1 | 240/240 |
| av1-10-tiles-crop 850×478 | libaom、2×2 tiles | 96/96 |

C 版对同样样片（va_dump，同一读法）：所有 10/12-bit、444 样片都在 vaGetImage 或 vaDeriveImage 上失败（色度平面首字节起不一致，或 `vaDeriveImage` 返回失败），只记录。

高位深吞吐（ffmpeg `-hwaccel vaapi -f null`，片源循环）：

| 片源 | rustnv | C 版 |
|---|---|---|
| HEVC Main10 1080p | 1997 fps | 2043 fps |
| HEVC Main10 4K | 529 fps | 499 fps |
| AV1 10-bit 1080p | 1498 fps | 1497 fps |
| AV1 10-bit 4K | 428 fps | 428 fps |

HEVC Main10 1080p 18000 帧无错误，进程 RSS 332 MB 持平（C 版 352 MB）。mpv `--hwdec=vaapi --vo=gpu-next`（Wayland）播放 HEVC Main10 与 AV1 10-bit，
截图与软解截图 PSNR 为 inf（逐像素一致）。

浏览器（1080p、120 帧的 HEVC Main10 / VP9 Profile 2 / AV1 10-bit，临时 profile，`LIBVA_TRACE` 统计 `vaEndPicture`）：

| 浏览器 | 格式 | rustnv | C 版 |
|---|---|---|---|
| Chrome 156 | HEVC Main10 | 硬解 120 次 | 120 次 |
| Chrome 156 | VP9 Profile 2 | 130 次 | 130 次 |
| Chrome 156 | AV1 10-bit | 120 次 | 119 次 |
| Firefox（`media.ffmpeg.vaapi.enabled`、`media.hardware-video-decoding.force-enabled`、`MOZ_DISABLE_RDD_SANDBOX=1`） | HEVC Main10 / VP9 Profile 2 / AV1 10-bit | 509 / 550 / 508 次，无错误 | 490 / 529 / 490 次 |

Chrome 参数：`--ozone-platform=wayland --disable-gpu-sandbox --ignore-gpu-blocklist --render-node-override=/dev/dri/renderD129
--enable-features=VaapiVideoDecoder,VaapiOnNvidiaGPUs,VaapiIgnoreDriverChecks,AcceleratedVideoDecodeLinuxGL,AcceleratedVideoDecodeLinuxZeroCopyGL,PlatformHEVCDecoderSupport`。
未带 `--render-node-override` 时 Chrome 跳过 nvidia-drm 设备，GPU 进程不加载任何 VA 驱动（rustnv 与 C 版相同），回退软件解码。
Chrome 建 AV1 / VP9 Profile 2 的 config 时 RTFormat 只给 YUV420，随后才创建 10-bit surface；rustnv 与 C 版一样把 RTFormat 当提示，按 surface 格式解码
（`tests/va_rt420.c` 用 `LD_PRELOAD` 复现这个调用顺序，`tests/decode.py` 的 `av1-10`、`vp9-p2-10` 用例带上它）。

性能（ffmpeg VA-API 解码吞吐，与C 版 nvidia-vaapi-driver 对比）：

| 片源 | rustnv | C 版 |
|---|---|---|
| H.264 1080p60 | 773 fps，CPU 0.30 ms/帧 | 680 fps，0.56 ms/帧 |
| H.264 4K60 | 216 fps，CPU 0.91 ms/帧 | 203 fps，1.35 ms/帧 |

mpv 实时播放 H.264 4K60 两者都是 60 fps、0 丢帧。浏览器：Firefox 158 播放 H.264/HEVC/VP9/AV1 由 rustnv 解码；
Chrome 156（关闭 GPU 沙箱，带 `--render-node-override`，见 [design.md](design.md)）四种格式由 rustnv 解码，GPU 进程不加载 `libcuda`/`libnvcuvid`。

## 旧格式验证

校验方式（`tests/decode.py`）：

| 格式 | 判定 | 样片 |
|---|---|---|
| MPEG-2 | 对软解 PSNR ≥ 45 dB；与 C 版逐字节一致 | 渐进、B 帧、隔行（场图像与帧图像）、自定义量化矩阵、非对齐、1080p |
| MPEG-4 | 与 libnvcuvid（`mpeg4_cuvid`）逐字节一致；对软解 PSNR ≥ 40 dB | SP、ASP+B-VOP、qpel+4MV、隔行（`+ildct+ilme`，tff）、alternate scan、`-mpeg_quant 1` 自定义矩阵、resync marker、330×250 非对齐、120 帧长 GOP（跨秒时间戳）、720p、`testsrc2` |
| VC-1 | 对软解逐字节一致，Advanced profile 样片对软解 PSNR ≥ 45 dB（色度有极小差异，C 版相同）；与 C 版逐字节一致 | samples.ffmpeg.org 上的 8 个样片，缓存在 `~/.cache/rustnv-samples`，缺失则跳过，`--fetch-samples` 经 http 下载 |
| VP8 | 对软解逐字节一致；与 C 版逐字节一致 | altref、8 切片、1080p、全关键帧 |

VC-1 样片（`http://samples.ffmpeg.org/` 下）：`V-codecs/WMV9/interlaced/480i30__codec_WVC1__mode_2__framerate_29.970__type_2__preproc_17.wmv`、
`ffmpeg-bugs/roundup/issue948/{planet-vc1-bframe-bitexact,saylent-vc1-bitexact}.mkv`、`ffmpeg-bugs/trac/ticket2723/wmv9adv.wmv`、
`ffmpeg-bugs/trac/ticket2993/vc1.wmv`、`archive/all/asf+wmv3+wmav2++audio_stutter.wmv`、`ffmpeg-bugs/trac/ticket5274/advanced_profile{,_2}.vc1`。

MPEG-4 与软解的差异：在 mandelbrot（自然渐变）内容上最低 43.7 dB（隔行）；`testsrc2`（锐利、快速移动的测试图）的 B-VOP 有少数宏块与 libavcodec 解出不同，
stock libnvcuvid 的输出与 rustnv 逐字节相同，所以差异在 NVDEC 引擎与 libavcodec 之间，不在描述符；该样片只以 libnvcuvid 为准。

C 版对照：MPEG-2、VP8、VC-1 与 rustnv 逐字节一致；MPEG-4 在 C 版上不可用（`vaCreateConfig` 失败）。含损坏帧的 VC-1 样片（`asf+wmv3+0x0162++async_av.wmv`、
`WMV9Drifting.asf`）上 rustnv 检查引擎状态，报 NVDEC 错误并丢弃 1–3 帧（输出帧数少于软解）；C 版照常输出全部帧，内容与 rustnv 不同。这两个样片不在回归里。

旧格式 1080p 解码吞吐（ffmpeg `-hwaccel vaapi -f null`，3300 帧，两次计时相减去掉启动）与 3300 帧以上的连续解码（`-v warning` 无任何错误输出，RSS 热身后增量）：

| 格式 | rustnv | C 版 | rustnv RSS 增量 |
|---|---|---|---|
| MPEG-2 | 860 fps | 812 fps | 0 KiB |
| MPEG-4 | 871 fps | 不可用 | 12 KiB |
| VC-1（saylent 拼接 3359 帧） | 774 fps | 776 fps | 0 KiB |
| VP8 | 904 fps | 932 fps | 0 KiB |

另有 VC-1 `vc1.wmv` 160×120 连续 4123 帧无错误。mpv `--hwdec=vaapi --hwdec-codecs=all --vo=gpu`（mpv 默认白名单不含 MPEG-2/MPEG-4）播放 MPEG-2、MPEG-4、VC-1、VP8，
`vaEndPicture` 每 120 帧约 120 次，无丢帧；`--hwdec=vaapi-copy` 截图与软解 PSNR：MPEG-2 70.7 dB、MPEG-4 54.2 dB、VP8 inf。
浏览器播 1080p VP8 webm（120 帧，libva trace 统计 `vaEndPicture` 成功次数）：

| 浏览器 | rustnv | C 版 |
|---|---|---|
| Chrome 156 | 硬解 120 帧，0 错误；截帧对软解 50.6 dB | `vaEndPicture` 报 invalid parameter（同样的整帧提交），退回软解 |
| Firefox | 硬解 120 帧，0 错误 | 硬解 120 帧 |

Chrome 只对 H.264/VP8/VP9/AV1/HEVC 走 VA-API，MPEG-2/4、VC-1 不会进驱动。

## 编码回归

```sh
LIBVA_DRIVERS_PATH="$PWD/target/release" LIBVA_DRIVER_NAME=rustnv \
  ffmpeg -init_hw_device vaapi=va:/dev/dri/renderD129 -filter_hw_device va \
  -f lavfi -i testsrc2=size=1920x1080:rate=30 -vf format=nv12,hwupload \
  -frames:v 120 -c:v h264_vaapi -profile:v high -rc_mode CQP -qp 26 out.mkv
python3 tests/encode.py
python3 tests/encode.py --quick --skip-stability
```

脚本默认在独立进程内分别选择系统 `nvidia` 和本仓库构建的 `rustnv`，不安装或修改系统驱动。`--driver` 是 libva 驱动名，`--library` 是驱动目录（`LIBVA_DRIVERS_PATH`，默认 `target/release`，其中需有 `rustnv_drv_video.so` 别名：`ln -sfn librustnv_drv_video.so target/release/rustnv_drv_video.so`）。可用 `--driver` / `--library`、`--reference-driver` / `--reference-library`、`--device` 调整。
结果保存在 `/tmp/rustnv-encode-regression/results.json`；每一侧分别保存完整编码 `vainfo -a` 和逐 profile 的原始属性值（包含不支持的属性）。
覆盖三个尺寸、三种 RC、各 120 帧、非 GOP 边界的强制关键帧、软件严格解码、精确帧数/色度/位深、原始像素 PSNR、目标码率 ±10%、码流 `trace_headers` 证明 QP 上下界。
直接 VA 客户端覆盖 H.264 444 / HEVC 444 10-bit 的 derive 和 put 两种上传路径；另有解码→编码转码和 3000 帧 1080p /proc RSS 采样。
退出码只判 rustnv：属性与 profile 须与 C 逐值相同，rustnv 的全部用例须满足断言；C 版自身的失败只记录在 JSON 里作对照。

2026-10-26 / RTX 4060 Laptop / nvidia-open 615.78.08，C 与 rustnv 的八个编码 profile、全部 57 个属性槽逐值相同：

| 属性 | C / rustnv |
|---|---|
| RateControl | CQP / CBR / VBR（22） |
| PackedHeaders | H.264 / HEVC：sequence / picture / slice / raw（23）；AV1：sequence（1） |
| MaxRefFrames / MaxSlices | 1 / 1 |
| PredictionDirection | previous（1） |
| MaxPictureWidth / Height | 8192 / 8192 |
| EncQualityRange | VA_ATTRIB_NOT_SUPPORTED |
| EncHEVCFeatures / BlockSizes | 285561920 / 1573082 |
| EncAV1 / Ext1 / Ext2 | 0 / 1024 / 8140 |

RTFormat：H.264 constrained baseline / main 与 HEVC main 为 420；H.264 high 为 420 + 444；HEVC main10 为 420_10；main444 为 444；main444_10 为 444_10；AV1 profile0 为 420 + 420_10。未列出的属性与不适用的 HEVC / AV1 属性均返回 `VA_ATTRIB_NOT_SUPPORTED`。

### 实测结果（2026-10-26）

完整记录：`python3 tests/encode.py --output /tmp/rustnv-encode-final`。
下表为 1920×1080、120 帧；PSNR 为 CQP 的原始像素比较，CBR/VBR 目标均为 4 Mbit/s。每格按 **rustnv / C** 顺序：

| profile / 输入 | CQP PSNR（dB） | CBR（Mbit/s） | VBR（Mbit/s） |
|---|---:|---:|---:|
| H.264 constrained baseline / NV12 | 47.32 / 47.97 | 3.817 / 3.979 | 3.741 / 3.979 |
| H.264 main / NV12 | 47.32 / 47.97 | 3.847 / 4.116 | 3.739 / 4.116 |
| H.264 high / NV12 | 47.29 / 47.96 | 3.852 / 4.051 | 3.734 / 4.051 |
| H.264 high / 444P（直接 VA） | 48.11 / 45.26 | 4.263 / 4.323 | 4.187 / 4.323 |
| HEVC main / NV12 | 47.81 / 47.92 | 3.813 / 4.292 | 3.717 / 4.292 |
| HEVC main10 / P010 | 47.34 / 47.49 | 3.782 / 4.337 | 3.687 / 4.337 |
| HEVC main444 / 444P | 47.56 / 44.29 | 4.088 / 4.643 | 3.968 / 4.643 |
| HEVC main444_10 / Q416（直接 VA） | 47.48 / 44.16 | 4.087 / 4.729 | 3.976 / 4.729 |
| AV1 profile0 / NV12 | 47.75 / 50.18 | 4.171 / 4.367 | 4.096 / 4.367 |
| AV1 profile0 / P010 | 47.98 / 50.64 | 4.172 / 4.367 | 4.091 / 4.367 |

三个尺寸合计：rustnv 的 72 个 FFmpeg 用例全部满足帧数、严格软件解码、色度/位深、关键帧、PSNR 和码率断言；直接 VA 444 的 36 个用例全部满足断言。

C 版作为对照有自己的失败（只记录，不影响退出码）：AV1 忽略第 45 帧强制关键帧（实际 keyframes 为 0/60，rustnv 为 0/45/105）；部分 444 CBR/VBR 超出目标 ±10%；HEVC Q416 无法协商；对尚未解码的 444 surface 调用 derive 失败（put 上传成功，rustnv 两条上传路径都成功）。

两边三个 codec × CBR/VBR 的 min=max=35 都由 `trace_headers` 证明 120 帧量化值恰为 35。FFmpeg 在 CQP 下不提交 qmin/qmax misc，CQP 的上下界由调用者提交 RateControl misc 时生效。
转码两边均为 120 帧、软件严格解码成功；rustnv 的解码 surface 转码无 CPU 回读。

| 连续 3000 帧 1080p | 热身后 RSS | 结束前 RSS | 增量 |
|---|---:|---:|---:|
| rustnv | 133872 KiB（第 330 帧） | 133908 KiB（第 2961 帧） | 36 KiB |
| C | 336192 KiB（第 324 帧） | 336264 KiB（第 2966 帧） | 72 KiB |

两边均连续编码 3000 帧且严格解码无错误；RSS 在热身后基本稳定，上表保留实际测得的微小增量。
`cargo build --release` 无警告；`tests/decode.py` 的 9 组全部逐字节一致；nvrm `cargo test --lib video::` 为 61 passed / 20 ignored；176 组 C/Rust NVENC 对照的 setup 和码流全部逐字节一致。

## 零拷贝输入

| 对照（640x360 / 1280x720 / 1920x1080 / 642x362） | rustnv | C 版 |
|---|---|---|
| NV12 线性 / 块线性 import，与 `vaPutImage` 码流逐字节一致 | 是（H.264、HEVC；块线性 `…6011`–`…6015` 全部） | 是 |
| P010 import（HEVC Main10） | 是 | `vaCreateSurfaces` 失败 |
| BGRA import | 是（线性与块线性码流相同；对 BT.709 参考 PSNR 约 38.6 dB；GPU 转换与 CPU 转换的码流逐字节一致） | `vaEndPicture` 失败（本测试程序下） |
| 444P / Q416 线性 / 块线性 import（H.264 444、HEVC Main444、Main444_10），与 `vaPutImage` 码流逐字节一致 | 是（656×368、642×362、1280×720、1920×1080） | 未测 |
| 解码 surface 直接编码（HEVC Main10、AV1 10-bit、HEVC 444 8-bit），与同一解码 `hwupload` 码流逐字节一致 | 是 | 未测 |
| 解码 → 编码 CPU 回读 | 无 | 无 |
| 解码 surface 导出再 DRM_PRIME_2 导入编码 | 与直接转码逐字节一致 | 未测 |

1080p 3000 帧 H.264 硬转码耗时：rustnv 6.0 s，C 版 7.0 s；600 帧 user CPU：rustnv 0.25 s，C 版 0.26 s。12000 帧 RSS 150 → 161 MiB 后持平。

BGRA 外部 dma-buf 编码（H.264，300 帧，同一帧只上传一次，`IMP_STATIC=1 tests/dmabuf_import.c`，计时整个进程）：

| 尺寸 / 源布局 | GPU 转换：耗时 / user CPU | CPU 转换（0.1.0）：耗时 / user CPU |
|---|---|---|
| 1920×1080 线性 | 0.94 s / 0.05 s | 1.66 s / 1.30 s |
| 1920×1080 块线性 | 0.95 s / 0.04 s | 1.75 s / 1.33 s |
| 3840×2160 线性 | 2.48 s / 0.07 s | 5.21 s / 4.74 s |
| 3840×2160 块线性 | 2.65 s / 0.07 s | 5.21 s / 4.69 s |

两种转换的输出码流逐字节一致。RM 的实机测试 `bgra_to_nv12_matches_cpu` 直接比对 NVENC 输入槽：极值、纯色、色度棋盘格、随机图（X 字节为垃圾值），1×1 到 1920×1080 的奇偶尺寸，线性（含带 padding 的 pitch）与块线性源，180 组逐字节一致。

测试：`tests/encode.py` 的 `zero_copy` 编译 `tests/dmabuf_import.c`（Vulkan 分配外部 NV12/P010/444P/Q416/BGRA，线性与块线性）与 `tests/dmabuf_export.c`（libav VA-API 解码后导出再导入），逐字节对照同内容 `vaPutImage`、BGRA PSNR 门限、1000 次循环 RSS/fd。FFmpeg 的 VA-API 解码没有 Q416 像素格式（10-bit 4:4:4 硬解在 `vaBeginPicture` 即失败），Q416 解码 surface 的布局由 Q416 import 覆盖。RM 的实机测试 `cargo test --release --lib chroma_444_interleave -- --ignored` 直接比对 4:4:4 色度交错的 copy engine 输出与 CPU tiler。

### Chrome 硬件编码（Chrome 156，Wayland 窗口，`--disable-gpu-sandbox`，临时 profile）

| 测试页项目 | rustnv | C 版 |
|---|---|---|
| WebCodecs H.264 Baseline / High（`prefer-hardware`，720p） | 硬件编码，各 60 帧、2 个关键帧 | 相同 |
| WebCodecs HEVC / AV1 | Chrome 不提供 | 相同 |
| WebRTC H.264 / AV1 | Chrome 用软件编码器（OpenH264 / libaom），两驱动相同 | 相同 |
| 硬件编码 720p 30 帧经软件解码与源画面 PSNR | 平均 28.10 dB、最低 27.96 dB | 平均 28.10 dB、最低 27.96 dB |

Chrome 经 DRM_PRIME_2 导入 NV12（C 版下的 libva trace：两个 object 指向同一缓冲、修饰符 0 即线性、pitch 1280、UV 偏移 983040），由 copy engine 送入 NVENC，不经 CPU。软件编码同一画面 PSNR 约 36.9 dB；两驱动硬件结果完全相同，差距来自 Chrome 的 RGB→NV12 输入与码率，不在驱动。

## 视频处理

```sh
python3 tests/vpp.py
python3 tests/gstreamer.py --only postproc
```

`tests/vpp.py` 的 probe 部分编译 `tests/vpp_probe.c`，逐项与测试内的 H.273 公式参考比较（纯色画面误差 ≤ 1）：

- 12 种格式各两种尺寸（含 70×38 奇数行宽）的同格式拷贝逐字节一致；
- NV12 / P010 / Q416 → BGRA，BT.601 / 709 / 2020 × full / limited；
- 颜色解析：固定标准忽略填了垃圾值的 Explicit 字段；Explicit 只换矩阵与 range；一端未指定按继承；两端未指定按高度（480 / 720 行）选矩阵；RGB → YUV 按输出高度（718 / 720 行）选矩阵；BT.601 → BT.709 只换矩阵；
- 拒绝：滤镜参数缓冲、`num_filters` 非零返回 `VA_STATUS_ERROR_UNSUPPORTED_FILTER`，BT.709 → BT.2020、PQ → SDR 返回 `VA_STATUS_ERROR_UNIMPLEMENTED`；
- 色度左对齐 / 居中，`output_region` 外的背景（RGBA 透明、NV12 不透明白）。

ffmpeg 部分（`setparams=chroma_location=center`，与 swscale 的假设一致）：

| 用例 | 结果 |
|---|---|
| 1080p → 1440p、NV12 → BGRA、NV12 → P010 | 与 swscale bilinear 逐字节一致 |
| 1080p → 720p（Y） | PSNR ≈ 46 dB |
| 1080p → 360p、4K → 1080p | 33.8 / 45.9 dB（缩小较多时 swscale 的滤波器覆盖更宽） |
| HEVC 10-bit P010 → 720p NV12、HEVC 4:4:4 → NV12 | 45.3 / 49.4 dB |
| CPU 上传 NV12 → 360p、BGRA → NV12 | 33.8 / 49.1 dB |
| 1080p → RGBA 720p | 45.3 dB |
| `scale_vaapi` 后接 `h264_vaapi` | 通过 |

GStreamer `vapostproc`（两级：VA surface 上处理，再读回）对照 zscale bilinear（GStreamer 按高度协商 BT.601 / 709，NV12 色度左对齐，swscale 不支持左对齐）：720p → 360p 37.0 dB，720p → 1080p 63.0 dB，NV12 → BGRA 103.3 dB，硬解 H.264 → 540p 38.6 dB。libva-utils `vavpp` 640×480 → 320×240 通过。

RM 的实机测试 `cargo test --release --lib vpp -- --ignored` 覆盖每种格式的 copy engine 拷贝、内核与 CPU 参考（32 种组合 × 5 种几何，误差 ≤ 1）和单位变换逐字节一致。

Chrome 156（Linux）只在 native 输入（GpuMemoryBuffer）且 `source_rect != encode_size` 时调用 `BlitSurface()`。实测未能触发：WebCodecs 在 Linux 上 `IsGpuFrameResizeSupported()` 为 false，裁剪 / 缩放在 renderer 里做；WebRTC 用真实摄像头（1280×720，`scaleResolutionDownBy: 2`）加 `--video-capture-use-gpu-memory-buffer` 时，H.264 由 VAVEA 硬编、硬解，但浏览器进程判定本机不支持 NV12 GpuMemoryBuffer（`IsGpuMemoryBufferNV12Supported()`），给 renderer 追加 `--disable-video-capture-use-gpu-memory-buffer`，VEA 仍以 I420 + SharedMemory 初始化，这一判定与本驱动无关；屏幕共享在 `IsZeroCopyEnabled()` 里直接排除；空间层 SVC 只支持 VP9，NVENC 没有 VP9 编码。

## 解码状态的延迟检查

```sh
python3 tests/fault.py --stress
```

失败注入：AV1 片段第 13–20 帧的 tile 数据中段改成随机字节（OBU 语法完整，引擎报 `error_status 0xd`）。读回（`hwdownload`，经 `vaSyncSurface`）与编码输入两条路径交出的每一帧都与干净码流同 pts 的帧逐字节一致，坏帧一帧也不交出，ffmpeg 报错；只解码时不挂死。压力：24000 帧解码 + 读回的峰值 RSS 比 10800 帧只多约 4 MB（ffmpeg 自身前一万帧内也涨约 20 MB）；4 个进程同时转码全部成功；20 次在解码中途 SIGINT / SIGKILL，无挂死，之后的解码正常。

性能（1080p 片段 1200 帧，4K 为 HEVC 600 帧；ffmpeg 输出留在 GPU，读回为 `hwdownload,format=nv12`；前后两版交替各跑 4–8 轮取最快）：

| 用例 | 解码后 CPU 等待 | 延迟检查 | 变化 |
|---|---:|---:|---:|
| H.264 / HEVC / AV1 1080p 解码 | 751 / 1683 / 1803 fps | 804 / 1893 / 2016 fps | +7% / +12% / +12% |
| HEVC 4K 解码 | 465 fps | 491 fps | +6% |
| H.264 / HEVC / AV1 1080p 解码 + 读回 | 435 / 704 / 696 fps | 776 / 905 / 926 fps | +79% / +28% / +33% |
| HEVC 4K 解码 + 读回 | 193 fps | 216 fps | +12% |
| H.264 1080p → AV1 | 490 fps | 607 fps | +24% |
| HEVC 1080p → H.264、HEVC 4K → AV1 | 501 / 184 fps | 500 / 184 fps | 持平（编码同步提交是瓶颈） |

CPU 时间不变或略降（读回 1.35 → 1.18 s）。读回的内容与旧版、软解逐帧一致（H.264 / HEVC / AV1 / VP9）。

## Chrome WebCodecs 帧序

```sh
python3 tests/chrome.py
```

Chrome 只在导出 surface 时调一次 `vaSyncSurface`，之后直接用 NVIDIA 的 GL 读导出的 dma-buf，GL 不等隐式 fence。0.2.0 的 `vaEndPicture` 只把导出拷贝排进队列，
新解码器复用 surface id 时偶尔读到上一轮留下的画面（第 2、3 轮硬解各有 1–3 帧是错帧）；改为已导出 surface 的 `vaEndPicture` 等拷贝完成后
8 项全部通过（每项 60 帧、帧号全对），3 轮硬解、硬解直送硬编、两种裁剪都没有错帧。页面画面经 Chrome 的 GL 转换后与参考图只有约 25 dB
（AMD 780M + radeonsi 相同），所以用帧号而不是 PSNR 判断。

## 环境变量与其余回归

`tests/env.py` 覆盖环境变量与兼容行为：默认静默、`NVD_LOG` / `NVD_LOG_VERBOSE`、`NVD_GPU`、别家 GPU 的显示设备回落、`NVD_MAX_INSTANCES`、解码最大尺寸属性、`vaCreateConfig` 忽略多余属性、`vaSyncSurface2`。`tests/gstreamer.py` 用 GStreamer `va` 元素解码六种格式、编码三种格式。`tests/decode.py` 增加 HEVC / VP9 / AV1 的 8192×4320 用例。
