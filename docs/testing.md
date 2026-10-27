# 回归测试与验证结果

所有脚本都不安装或修改系统驱动：它们在子进程内用 `LIBVA_DRIVERS_PATH` / `LIBVA_DRIVER_NAME` 选择 rustnv。先 `cargo build --release`；脚本默认加载 `target/release/librustnv_drv_video.so`（`--driver` 可改），默认设备 `/dev/dri/renderD129`（`--device` 可改，用 `vainfo`/`ls /dev/dri` 确认 NVIDIA 的渲染节点）。

脚本需要 Python 3、C 编译器、FFmpeg（libx264、libx265、libvpx、libsvtav1、libaom）、`vainfo`；对照 C 版时还需要另行安装的 C 版 `nvidia_drv_video.so`（与本驱动的安装名相同，二者不能同时装在 `/usr/lib/dri`；`tests/encode.py` 的 `--reference-library` 指向放有 C 版 `.so` 的目录，默认取系统驱动目录）。`tests/` 下的 `*.c` 是脚本自动编译的辅助程序（`va_dump.c`、`va_rt420.c`、`dmabuf_egl.c`、`dmabuf_import.c`、`dmabuf_export.c`）。

## 脚本

| 脚本 | 内容 |
|---|---|
| `tests/decode.py` | 全部解码格式的逐帧比较（`--codec av1` 单个格式；`--c-driver` 同时跑 C 版；`--fetch-samples` 下载 VC-1 样片） |
| `tests/encode.py` | 编码回归、属性对照、零拷贝输入、稳定性（`--quick --skip-stability` 快速） |
| `tests/env.py` | 环境变量与兼容行为 |
| `tests/gstreamer.py` | GStreamer `va` 元素解码六种格式、编码三种格式（需要 `GST_VA_ALL_DRIVERS=1`，脚本已设置） |

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
| BGRA import | 是（线性与块线性码流相同；对 BT.709 参考 PSNR 约 38.6 dB） | `vaEndPicture` 失败（本测试程序下） |
| 444P / Q416 线性 / 块线性 import（H.264 444、HEVC Main444、Main444_10），与 `vaPutImage` 码流逐字节一致 | 是（656×368、642×362、1280×720、1920×1080） | 未测 |
| 解码 surface 直接编码（HEVC Main10、AV1 10-bit、HEVC 444 8-bit），与同一解码 `hwupload` 码流逐字节一致 | 是 | 未测 |
| 解码 → 编码 CPU 回读 | 无 | 无 |
| 解码 surface 导出再 DRM_PRIME_2 导入编码 | 与直接转码逐字节一致 | 未测 |

1080p 3000 帧 H.264 硬转码耗时：rustnv 6.0 s，C 版 7.0 s；600 帧 user CPU：rustnv 0.25 s，C 版 0.26 s。12000 帧 RSS 150 → 161 MiB 后持平。

测试：`tests/encode.py` 的 `zero_copy` 编译 `tests/dmabuf_import.c`（Vulkan 分配外部 NV12/P010/444P/Q416/BGRA，线性与块线性）与 `tests/dmabuf_export.c`（libav VA-API 解码后导出再导入），逐字节对照同内容 `vaPutImage`、BGRA PSNR 门限、1000 次循环 RSS/fd。FFmpeg 的 VA-API 解码没有 Q416 像素格式（10-bit 4:4:4 硬解在 `vaBeginPicture` 即失败），Q416 解码 surface 的布局由 Q416 import 覆盖。RM 的实机测试 `cargo test --release --lib chroma_444_interleave -- --ignored` 直接比对 4:4:4 色度交错的 copy engine 输出与 CPU tiler。

### Chrome 硬件编码（Chrome 156，Wayland 窗口，`--disable-gpu-sandbox`，临时 profile）

| 测试页项目 | rustnv | C 版 |
|---|---|---|
| WebCodecs H.264 Baseline / High（`prefer-hardware`，720p） | 硬件编码，各 60 帧、2 个关键帧 | 相同 |
| WebCodecs HEVC / AV1 | Chrome 不提供 | 相同 |
| WebRTC H.264 / AV1 | Chrome 用软件编码器（OpenH264 / libaom），两驱动相同 | 相同 |
| 硬件编码 720p 30 帧经软件解码与源画面 PSNR | 平均 28.10 dB、最低 27.96 dB | 平均 28.10 dB、最低 27.96 dB |

Chrome 经 DRM_PRIME_2 导入 NV12（C 版下的 libva trace：两个 object 指向同一缓冲、修饰符 0 即线性、pitch 1280、UV 偏移 983040），由 copy engine 送入 NVENC，不经 CPU。软件编码同一画面 PSNR 约 36.9 dB；两驱动硬件结果完全相同，差距来自 Chrome 的 RGB→NV12 输入与码率，不在驱动。

## 环境变量与其余回归

`tests/env.py` 覆盖环境变量与兼容行为：默认静默、`NVD_LOG` / `NVD_LOG_VERBOSE`、`NVD_GPU`、别家 GPU 的显示设备回落、`NVD_MAX_INSTANCES`、解码最大尺寸属性、`vaCreateConfig` 忽略多余属性、`vaSyncSurface2`。`tests/gstreamer.py` 用 GStreamer `va` 元素解码六种格式、编码三种格式。`tests/decode.py` 增加 HEVC / VP9 / AV1 的 8192×4320 用例。
