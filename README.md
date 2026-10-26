# Rust NVIDIA VA-API 驱动

独立的宿主机 VA-API 驱动，通过共享 Rust RM 后端直接使用 NVIDIA NVDEC / NVENC，不调用 CUDA、libnvcuvid 或 NVENC SDK。驱动名为 `rustnv`，
在单个测试进程中选择，不安装到系统。

## 支持范围

### 解码（VAEntrypointVLD）

| 格式 | VA profile | surface / 图像格式 |
|---|---|---|
| MPEG-2 | Simple / Main | NV12 |
| MPEG-4 part 2 | Simple / Advanced Simple / Main（无 sprite） | NV12 |
| VC-1 / WMV3 | Simple / Main / Advanced | NV12 |
| VP8 | Version 0–3 | NV12 |
| H.264 | Constrained Baseline / Main / High | NV12 |
| HEVC | Main / Main10 / Main12 | NV12 / P010 / P012 |
| HEVC | Main444 / Main444_10 / Main444_12 | 444P / Q416 / Q416 |
| VP9 | Profile 0 / Profile 2（10、12-bit） | NV12 / P010、P012 |
| AV1 | Profile 0（8、10-bit） | NV12 / P010 |

与系统里的 C 驱动的 VLD 列表相比，唯一的能力差距是 JPEG Baseline（见“旧格式”）。C 驱动的 `vainfo` 虽然列出 MPEG-4 的三个 profile，
但 `vaCreateConfig` 对它们返回 `VA_STATUS_ERROR_UNSUPPORTED_PROFILE`（源码里 MPEG-4 已被注释禁用，`mpeg4.c` 自述“VA-API 给的信息不足以喂 NVDEC”），
所以 C 驱动实际不能解 MPEG-4，rustnv 能；VC-1 Complex profile 没有对应的 VA profile，两个驱动都不涉及；C 源码的 MPEG-4 条目还含 H.263 Baseline（同被禁用），rustnv 不提供。
VP9 Profile 2、AV1 的 surface 位深由 `vaCreateSurfaces` 的 RTFormat 决定，context 在第一帧按目标 surface 重建解码器。

已验证的覆盖见下方“硬件回归”。

- 逐行，支持非宏块/CTB/超块对齐的显示尺寸。10/12-bit 样本放在 16 位容器高位（P010/P012/Q416，与引擎输出一致）。
- 12-bit 4:2:0 的 DRM 描述符 fourcc 报 P016（与 C 驱动一致）；4:4:4 的分层描述符为三个 R8 或 R16 平面，合并描述符仅 444P（`YU24`）。
- HEVC 4:4:4/12-bit 经 range extension 描述符（`VAPictureParameterBufferHEVCExtension`）；NVDEC 无 AV1 12-bit、VP9/AV1 4:4:4、4:2:2。
- 解码 surface 作编码输入：8/10-bit 4:2:0 走 copy engine；4:4:4 surface 作编码输入不支持（NVENC 的 4:4:4 输入是交错 chroma，copy engine 一次只能搬一个平面，见“遗留”）。
- 解码参数直接来自 VA-API，转换为 NVDEC 描述符；不重建参数集码流。
- 输出：NV12 CPU 下载；DRM PRIME2 的 R8 + RG88 分层或 NV12 合并描述符（块线性修饰符）。
- 只在 libva 打开的那块 NVIDIA GPU 上解码；初始化时由 DRM fd 识别设备，非 nvidia 驱动的节点直接拒绝。

解码明确拒绝并返回错误的情况：4:2:2、VP9/AV1 4:4:4、AV1 12-bit、H.264 隔行/场编码、H.264 FMO、HEVC PCM 与分离色彩平面、
MPEG-4 的 sprite（GMC）、H.263 短头、data partitioning 与 reversible VLC（NVDEC 的 MPEG-4 描述符没有这些字段，stock 解 data partitioning 的结果也是错的）、
VP9 在分段图写入后改变编码尺寸、AV1 film grain、large-scale tile 与帧间 superres。不支持视频后处理。

### 编码（VAEntrypointEncSlice）

| 格式 | VA profile | 输入 |
|---|---|---|
| H.264 | Constrained Baseline / Main | NV12 |
| H.264 | High | NV12 / 444P |
| HEVC | Main / Main10 | NV12 / P010 |
| HEVC | Main444 / Main444_10 | 444P / Q416 |
| AV1 | Profile 0 | NV12 / P010 |

- CQP、CBR、VBR；单个 L0 参考、I/P 帧；GOP / IDR 周期、强制关键帧、初始 QP、min/max QP。
- 最大尺寸 8192×8192；属性和 profile 对照系统 C 驱动，包含 HEVC features / block sizes 与 AV1 扩展属性。
- `vaCreateImage` + `vaPutImage`、`vaDeriveImage` + map / unmap 支持 CPU 上传。直接映射的线性缓冲在 surface 间复用，上传按 RM 的 NVENC tiled layout 写显存。
- 8-bit 444P 直接交给 RM 做 tiled chroma interleave。Q416 使用每个 context 复用的 chroma staging 缓冲，将两个 16-bit 平面交错后上传；luma 不做 staging 拷贝。
- H.264 / HEVC 忽略应用 packed SPS/PPS/slice，使用 RM 生成的参数集，在每个 IDR 前重复。AV1 接受 packed sequence 参数，RM 输出 in-band sequence OBU。
- coded buffer map 返回 `VACodedBufferSegment`（单段链、`VA_CODED_BUF_STATUS_SINGLE_NALU`）。提交当前为同步编码，因此 surface / buffer 同步查询在提交返回后已完成。
- GPU 从 libva 的 DRM fd 选择，与解码一致。解码 surface（8/10-bit 4:2:0）作编码输入不经 CPU：surface 的导出 backing（8 行 GOB、单 GOB 块高的块线性）由 GPU copy engine 重排进 NVENC 输入槽，字节与 CPU tiler 一致。外部 DMA-BUF 的 NV12 / P010 同走 copy engine；BGRA 先经 copy engine 拷入系统内存 staging，再由 CPU 按 BT.709 转换（见“遗留”）。
- RM 的帧率接口为整数，分数帧率四舍五入。RateControl / FrameRate misc 参数被消费；其余 misc（包含 HRD / QualityLevel）与当前 C 后端一样接受但不消费。

FFmpeg 经 `hwupload` 协商不了 H.264 444 和 HEVC Q416，这两种输入由回归脚本里的直接 VA 客户端测试，并检查输出色度/位深。

## 设计

- 每个 context 一个 worker 线程持有 RM 会话；VA 回调把解码和导出拷贝排进队列后返回，`vaSyncSurface`、`vaDeriveImage`、
  `vaGetImage` 不持全局锁等待完成。worker 退出时未完成的工作置为失败，等待方不会卡死。
- surface 可以在解码前导出；导出的显存在 surface 生命周期内固定，每次解码后由 GPU copy engine 更新。显示路径没有 CPU 拷贝，
  有一次 GPU 拷贝。
- 解码 context 销毁后，存活的 surface 仍持有 worker 和画面。
- `src/backend/mod.rs` 管会话、导出和下载；`src/backend/{h264,hevc,vp9,av1,mpeg2,mpeg4,vc1,vp8}.rs` 各自把 VA 参数转换成 RM 的描述符输入。

RM 按 NVIDIA 驱动版本精确匹配 ABI，不认识的版本拒绝运行。Turing 及更新代的 NVDEC 全部启用，各代能力按 NVIDIA 驱动的代际规则
和引擎能力表判断；本驱动实测范围仅为 RTX 4060 Laptop / nvidia-open 615.78.08 / x86_64 glibc。

## 构建

依赖 Rust、Clang/libclang、libva 开发头文件；回归还需要 C 编译器、FFmpeg（libx264、libx265、libvpx、libsvtav1、libaom）。
RM 来自 [nvrm](https://github.com/cagedbird043/nvrm)，`Cargo.toml` 按 commit 钉选。本地联调 RM 时在 `.cargo/config.toml`
（不提交）里用 `[patch."https://github.com/cagedbird043/nvrm"]` 把 `nv-bionic-native` 指到本地 RM 目录；此时 cargo
会去掉 `Cargo.lock` 里 nvrm 的 `source` 行，提交前不带 patch 重新生成。

```sh
cargo build --release
ln -sfn librustnv_drv_video.so target/release/rustnv_drv_video.so
```

## 使用

```sh
export LIBVA_DRIVERS_PATH="$PWD/target/release" LIBVA_DRIVER_NAME=rustnv
mpv --hwdec=vaapi --hwdec-software-fallback=no --vo=gpu --gpu-api=opengl video.mp4
```

Firefox：`media.ffmpeg.vaapi.enabled=true` 即可，RDD 进程里会出现 `rustnv-nvdec` 线程。

Chrome 目前需要关闭 GPU 沙箱：

```sh
google-chrome-stable --disable-gpu-sandbox --render-node-override=/dev/dri/renderD129 \
  --enable-features=VaapiVideoDecoder,VaapiOnNvidiaGPUs,VaapiIgnoreDriverChecks,AcceleratedVideoDecodeLinuxGL,AcceleratedVideoDecodeLinuxZeroCopyGL
```

默认沙箱下，Chrome 在启用沙箱前初始化、查询并 `vaTerminate` 了驱动；解码时在 seccomp 沙箱内重新打开 VA display，
访问 `/sys/dev/char/<major>:<minor>/device/drm` 被 Chrome 的 broker 拒绝，报 `Could not get a valid VA display`，
驱动的初始化函数不会再被调用。这是 Chrome 对 NVIDIA 的 VA-API 沙箱策略不完整，驱动本身无法绕过。
C 版 nvidia-vaapi-driver 看似能在默认设置下工作，是因为它加载的 libcuda 在沙箱启用前就创建了线程，Chrome 报
`InitializeSandbox() called with multiple threads in process gpu-process` 后 GPU 进程没有 seccomp（`/proc/<pid>/status` 中
`Seccomp: 0`），效果等同于 `--disable-gpu-sandbox`。

## 硬件回归

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

高位深与 4:4:4（`tests/decode.py`，10 例，`--c-driver` 同时跑 C 驱动）：10/12-bit 与 4:4:4 样片用 `tests/va_dump.c`
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

C 驱动对同样样片（va_dump，同一读法）：所有 10/12-bit、444 样片都在 vaGetImage 或 vaDeriveImage 上失败（色度平面首字节起不一致，或 `vaDeriveImage` 返回失败），只记录。

高位深吞吐（ffmpeg `-hwaccel vaapi -f null`，片源循环）：

| 片源 | rustnv | C 驱动 |
|---|---|---|
| HEVC Main10 1080p | 1997 fps | 2043 fps |
| HEVC Main10 4K | 529 fps | 499 fps |
| AV1 10-bit 1080p | 1498 fps | 1497 fps |
| AV1 10-bit 4K | 428 fps | 428 fps |

HEVC Main10 1080p 18000 帧无错误，进程 RSS 332 MB 持平（C 驱动 352 MB）。mpv `--hwdec=vaapi --vo=gpu-next`（Wayland）播放 HEVC Main10 与 AV1 10-bit，
截图与软解截图 PSNR 为 inf（逐像素一致）。

浏览器（1080p、120 帧的 HEVC Main10 / VP9 Profile 2 / AV1 10-bit，临时 profile，`LIBVA_TRACE` 统计 `vaEndPicture`）：

| 浏览器 | 格式 | rustnv | C 驱动 |
|---|---|---|---|
| Chrome 156 | HEVC Main10 | 硬解 120 次 | 120 次 |
| Chrome 156 | VP9 Profile 2 | 130 次 | 130 次 |
| Chrome 156 | AV1 10-bit | 120 次 | 119 次 |
| Firefox（`media.ffmpeg.vaapi.enabled`、`media.hardware-video-decoding.force-enabled`、`MOZ_DISABLE_RDD_SANDBOX=1`） | HEVC Main10 / VP9 Profile 2 / AV1 10-bit | 509 / 550 / 508 次，无错误 | 490 / 529 / 490 次 |

Chrome 参数：`--ozone-platform=wayland --disable-gpu-sandbox --ignore-gpu-blocklist --render-node-override=/dev/dri/renderD129
--enable-features=VaapiVideoDecoder,VaapiOnNvidiaGPUs,VaapiIgnoreDriverChecks,AcceleratedVideoDecodeLinuxGL,AcceleratedVideoDecodeLinuxZeroCopyGL,PlatformHEVCDecoderSupport`。
未带 `--render-node-override` 时 Chrome 跳过 nvidia-drm 设备，GPU 进程不加载任何 VA 驱动（rustnv 与 C 驱动相同），回退软件解码。
Chrome 建 AV1 / VP9 Profile 2 的 config 时 RTFormat 只给 YUV420，随后才创建 10-bit surface；rustnv 与 C 驱动一样把 RTFormat 当提示，按 surface 格式解码
（`tests/va_rt420.c` 用 `LD_PRELOAD` 复现这个调用顺序，`tests/decode.py` 的 `av1-10`、`vp9-p2-10` 用例带上它）。

性能（ffmpeg VA-API 解码吞吐，与系统里的 C 版 nvidia-vaapi-driver 对比）：

| 片源 | rustnv | C 驱动 |
|---|---|---|
| H.264 1080p60 | 773 fps，CPU 0.30 ms/帧 | 680 fps，0.56 ms/帧 |
| H.264 4K60 | 216 fps，CPU 0.91 ms/帧 | 203 fps，1.35 ms/帧 |

mpv 实时播放 H.264 4K60 两者都是 60 fps、0 丢帧。浏览器：Firefox 158 播放 H.264/HEVC/VP9/AV1 由 rustnv 解码；
Chrome 156（关闭 GPU 沙箱，带 `--render-node-override`，见下）四种格式由 rustnv 解码，GPU 进程不加载 `libcuda`/`libnvcuvid`。

## 旧格式（MPEG-2 / MPEG-4 / VC-1 / VP8）

这四种格式由 NVDEC 引擎自己解析宏块层，VA 参数只填序列/图像层语法：RM 的 `video/legacy.rs` 提供通用的 `LegacyCodec<F: Format>`
（VA surface 与会话 surface 的对应、前向/后向参考、场对），各格式只实现 `Format::build`（VA 参数 → NVDEC 描述符和方法序列）。
描述符布局、方法序列和侧缓冲规则取自 stock libnvcuvid 615.78.08 的 RM 追踪（RM 仓库 `tools/nvdec-trace`），不经 CUDA。关键事实：

- **MPEG-2**：引擎对每幅图像写两幅叠放的场图像（下场在上场之后）；copy engine 导出时按行交织成帧。场图像的第二场写进第一场所在的 surface。
  切片起点表放在描述符 +0x200，码流是带起始码的切片。
- **MPEG-4**：引擎自己解析 VOP 头，码流必须从 `00 00 01 b6` 开始。VA 客户端（libavcodec）只交出 VOP 头之后的宏块数据（`macroblock_offset` 是首字节内的位偏移），
  所以驱动按 VA 参数重建 VOP 头（类型、时间戳、取整、`intra_dc_vlc_thr`、扫描位、`vop_quant`、f_code），再把宏块数据按位移接上。
  VA 不带 VOP 时间戳，驱动用 `TRD`/`TRB` 推出一条时钟写进头里；实测 NVDEC 解带 `modulo_time_base` 的 I-VOP 会出错，所以 I-VOP 按 GOP 头之后的写法重新从 0 秒起算（与 ffmpeg 编码器的码流一致），P/B-VOP 照常带秒计数。
  描述符 +0x5c 起是 VOL 语法（时间增量位数、resync、尺寸）、量化矩阵（光栅序）和 B-VOP 的 `trd/trb`；`error_status` 的第 3 位在正确的 B-VOP 上也会出现（stock 相同），不当作错误。
- **VC-1 / WMV3**：引擎自己解析图像层（包括 bitplane），描述符只有序列和 entry point 语法；Simple/Main 没有起始码，引擎要求描述符里给帧起始码；
  Advanced 的每个切片前补 `00 00 01 0d/0c/0b`。输出同样是两幅叠放的场图像。
- **VP8**：引擎解析整个帧（头、概率、分区），描述符只有尺寸、帧类型和第一分区长度；会话开始时需要把一张固定初始表（`vp8_init.bin`，取自 stock 写入的 copy-engine 源缓冲）写进侧缓冲。

校验方式（`tests/decode.py`，原 19 例之外的新用例）：

| 格式 | 判定 | 样片 |
|---|---|---|
| MPEG-2 | 对软解 PSNR ≥ 45 dB；与 C 驱动逐字节一致 | 渐进、B 帧、隔行（场图像与帧图像）、自定义量化矩阵、非对齐、1080p |
| MPEG-4 | 与 libnvcuvid（`mpeg4_cuvid`）逐字节一致；对软解 PSNR ≥ 40 dB | SP、ASP+B-VOP、qpel+4MV、隔行（`+ildct+ilme`，tff）、alternate scan、`-mpeg_quant 1` 自定义矩阵、resync marker、330×250 非对齐、120 帧长 GOP（跨秒时间戳）、720p、`testsrc2` |
| VC-1 | 对软解逐字节一致，Advanced profile 样片对软解 PSNR ≥ 45 dB（色度有极小差异，C 驱动相同）；与 C 驱动逐字节一致 | samples.ffmpeg.org 上的 8 个样片，缓存在 `~/.cache/rustnv-samples`，缺失则跳过，`--fetch-samples` 经 http 下载 |
| VP8 | 对软解逐字节一致；与 C 驱动逐字节一致 | altref、8 切片、1080p、全关键帧 |

VC-1 样片（`http://samples.ffmpeg.org/` 下）：`V-codecs/WMV9/interlaced/480i30__codec_WVC1__mode_2__framerate_29.970__type_2__preproc_17.wmv`、
`ffmpeg-bugs/roundup/issue948/{planet-vc1-bframe-bitexact,saylent-vc1-bitexact}.mkv`、`ffmpeg-bugs/trac/ticket2723/wmv9adv.wmv`、
`ffmpeg-bugs/trac/ticket2993/vc1.wmv`、`archive/all/asf+wmv3+wmav2++audio_stutter.wmv`、`ffmpeg-bugs/trac/ticket5274/advanced_profile{,_2}.vc1`。

MPEG-4 与软解的差异：在 mandelbrot（自然渐变）内容上最低 43.7 dB（隔行）；`testsrc2`（锐利、快速移动的测试图）的 B-VOP 有少数宏块与 libavcodec 解出不同，
stock libnvcuvid 的输出与 rustnv 逐字节相同，所以差异在 NVDEC 引擎与 libavcodec 之间，不在描述符；该样片只以 libnvcuvid 为准。

C 驱动对照：MPEG-2、VP8、VC-1 与 rustnv 逐字节一致；MPEG-4 在 C 驱动上不可用（`vaCreateConfig` 失败）。含损坏帧的 VC-1 样片（`asf+wmv3+0x0162++async_av.wmv`、
`WMV9Drifting.asf`）上 rustnv 检查引擎状态，报 NVDEC 错误并丢弃 1–3 帧（输出帧数少于软解）；C 驱动照常输出全部帧，内容与 rustnv 不同。这两个样片不在回归里。

旧格式 1080p 解码吞吐（ffmpeg `-hwaccel vaapi -f null`，3300 帧，两次计时相减去掉启动）与 3300 帧以上的连续解码（`-v warning` 无任何错误输出，RSS 热身后增量）：

| 格式 | rustnv | C 驱动 | rustnv RSS 增量 |
|---|---|---|---|
| MPEG-2 | 860 fps | 812 fps | 0 KiB |
| MPEG-4 | 871 fps | 不可用 | 12 KiB |
| VC-1（saylent 拼接 3359 帧） | 774 fps | 776 fps | 0 KiB |
| VP8 | 904 fps | 932 fps | 0 KiB |

另有 VC-1 `vc1.wmv` 160×120 连续 4123 帧无错误。mpv `--hwdec=vaapi --hwdec-codecs=all --vo=gpu`（mpv 默认白名单不含 MPEG-2/MPEG-4）播放 MPEG-2、MPEG-4、VC-1、VP8，
`vaEndPicture` 每 120 帧约 120 次，无丢帧；`--hwdec=vaapi-copy` 截图与软解 PSNR：MPEG-2 70.7 dB、MPEG-4 54.2 dB、VP8 inf。
Chrome 156 播放 1080p VP8 webm（rustnv 与 C 驱动）8 秒：无解码错误、0 丢帧（rustnv 238 帧、C 210 帧）；Chrome 只对 H.264/VP8/VP9/AV1/HEVC 走 VA-API，MPEG-2/4、VC-1 不会进驱动。

### JPEG 不支持

C 驱动的 JPEG Baseline 经 libnvcuvid 的 JPEG 路径解码；RM 追踪显示 stock 的 JPEG 用内嵌的 CUDA 内核（`jpegparse`、`jpegdec_vld`、`jpegdec_idct`）完成，
通道上只有计算与拷贝，没有 NVDEC / NVJPG 的方法。rustnv 不使用 CUDA，因此不提供 JPEG（`VAProfileJPEGBaseline`），这是与 C 驱动唯一的能力差距。

## 编码回归

```sh
LIBVA_DRIVERS_PATH="$PWD/target/release" LIBVA_DRIVER_NAME=rustnv \
  ffmpeg -init_hw_device vaapi=va:/dev/dri/renderD129 -filter_hw_device va \
  -f lavfi -i testsrc2=size=1920x1080:rate=30 -vf format=nv12,hwupload \
  -frames:v 120 -c:v h264_vaapi -profile:v high -rc_mode CQP -qp 26 out.mkv
python3 tests/encode.py
python3 tests/encode.py --quick --skip-stability
```

脚本默认在独立进程内分别选择系统 `nvidia` 和本目录的 `rustnv`，不安装或修改系统驱动。可用 `--driver` / `--library`、`--reference-driver` / `--reference-library`、`--device` 调整。
结果保存在 `/tmp/rustnv-encode-regression/results.json`；每一侧分别保存完整编码 `vainfo -a` 和逐 profile 的原始属性值（包含不支持的属性）。
覆盖三个尺寸、三种 RC、各 120 帧、非 GOP 边界的强制关键帧、软件严格解码、精确帧数/色度/位深、原始像素 PSNR、目标码率 ±10%、码流 `trace_headers` 证明 QP 上下界。
直接 VA 客户端覆盖 H.264 444 / HEVC 444 10-bit 的 derive 和 put 两种上传路径；另有解码→编码转码和 3000 帧 1080p /proc RSS 采样。
退出码只判 rustnv：属性与 profile 须与 C 逐值相同，rustnv 的全部用例须满足断言；C 驱动自身的失败只记录在 JSON 里作对照。

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

C 驱动作为对照有自己的失败（只记录，不影响退出码）：AV1 忽略第 45 帧强制关键帧（实际 keyframes 为 0/60，rustnv 为 0/45/105）；部分 444 CBR/VBR 超出目标 ±10%；HEVC Q416 无法协商；对尚未解码的 444 surface 调用 derive 失败（put 上传成功，rustnv 两条上传路径都成功）。

两边三个 codec × CBR/VBR 的 min=max=35 都由 `trace_headers` 证明 120 帧量化值恰为 35。FFmpeg 在 CQP 下不提交 qmin/qmax misc，CQP 的上下界由调用者提交 RateControl misc 时生效。
转码两边均为 120 帧、软件严格解码成功；rustnv 的解码 surface 转码无 CPU 回读。

| 连续 3000 帧 1080p | 热身后 RSS | 结束前 RSS | 增量 |
|---|---:|---:|---:|
| rustnv | 133872 KiB（第 330 帧） | 133908 KiB（第 2961 帧） | 36 KiB |
| C | 336192 KiB（第 324 帧） | 336264 KiB（第 2966 帧） | 72 KiB |

两边均连续编码 3000 帧且严格解码无错误；RSS 在热身后基本稳定，上表保留实际测得的微小增量。
`cargo build --release` 无警告；`tests/decode.py` 的 9 组全部逐字节一致；RM `cargo test --lib video::` 为 61 passed / 20 ignored；176 组 C/Rust NVENC 对照的 setup 和码流全部逐字节一致。

## 零拷贝编码输入（2c）

| 输入来源 | 到 NVENC 的路径 |
|---|---|
| 解码 surface | 导出 backing → GPU copy engine（块线性 → NVENC 输入 tile）→ 输入槽 |
| DRM_PRIME_2 / DRM_PRIME（NV12、P010/P012/P016） | 导入 dma-buf → copy engine → 输入槽 |
| DRM_PRIME_2 / DRM_PRIME（ARGB、XRGB） | 导入 → copy engine → 系统内存 staging → CPU BT.709 转换 → 输入槽 |
| `vaPutImage` / `vaDeriveImage` | CPU 写显存（未变） |

- `vaCreateSurfaces` 接受 `VASurfaceAttribMemoryType` = VA | DRM_PRIME | DRM_PRIME_2 与 `VASurfaceAttribExternalBufferDescriptor`（每次一个 surface）。旧版 DRM_PRIME 不带修饰符，与 C 驱动一样只在设置 `NVD_DRM_PRIME_ASSUME_LINEAR` 时按线性接受；`vaQuerySurfaceAttributes` 也只在此时报告它，平时报告 VA | DRM_PRIME_2 和外部缓冲描述符。
- Chrome：它只对认识的驱动用 DRM_PRIME_2 导入编码输入，其余用不带修饰符的旧版 DRM_PRIME，rustnv 无法确认其布局而拒绝，Chrome 也不会退回 `vaPutImage`。与 C 驱动相同，rustnv 在 Chrome GPU 进程里把 vendor 字符串加上 `Chromium fake libva driver compatible:` 前缀，使 Chrome 改走 DRM_PRIME_2（Chrome 156 中该类型只影响这一处）。
- 修饰符：线性，以及 NVIDIA 块线性（无压缩、Turing+ GOB、块高 ≤ 32 行，即 `0x0300000000606010`–`15` 一族）。其余修饰符返回 `UNSUPPORTED_MEMORY`。
- 导入的 surface 不能做解码目标，也不能 `vaPutImage`；可再经 `vaExportSurfaceHandle` 导出。`vaDestroySurfaces` 释放 import 与映射，1000 次 import/export/destroy 循环 RSS 与 fd 数不增长。
- 解码 surface 经 `vaExportSurfaceHandle` 导出时，拷贝入共享 backing 的操作在导出时排队，消费方需在导出后 `vaSyncSurface` 再读。

| 对照（640x360 / 1280x720 / 1920x1080 / 642x362） | rustnv | C 驱动 |
|---|---|---|
| NV12 线性 / 块线性 import，与 `vaPutImage` 码流逐字节一致 | 是（H.264、HEVC；块线性 `…6011`–`…6015` 全部） | 是 |
| P010 import（HEVC Main10） | 是 | `vaCreateSurfaces` 失败 |
| BGRA import | 是（线性与块线性码流相同；对 BT.709 参考 PSNR 约 38.6 dB） | `vaEndPicture` 失败（本测试程序下） |
| 解码 → 编码 CPU 回读 | 无 | 无 |
| 解码 surface 导出再 DRM_PRIME_2 导入编码 | 与直接转码逐字节一致 | 未测 |

1080p 3000 帧 H.264 硬转码耗时：旧 rustnv（CPU 回读）288 s，现 rustnv 6.0 s，C 驱动 7.0 s；600 帧 user CPU 49.8 s → 0.25 s（C 0.26 s）。12000 帧 RSS 150 → 161 MiB 后持平。与旧驱动的 FFmpeg 转码（H.264/HEVC/AV1，8-bit，多尺寸；HEVC Main10 与 AV1 经 `hwupload` p010）输出逐字节一致。

测试：`tests/encode.py` 的 `zero_copy` 编译 `tests/dmabuf_import.c`（Vulkan 分配外部 NV12/P010/BGRA，线性与块线性）与 `tests/dmabuf_export.c`（libav VA-API 解码后导出再导入），逐字节对照同内容 `vaPutImage`、BGRA PSNR 门限、1000 次循环 RSS/fd。

### 遗留
- 4:4:4 解码 surface 不能直接作编码输入（NVENC 要交错 chroma，copy engine 的 remap 一次只写一个平面）；4:4:4 编码输入仍走 `vaPutImage` / derive 上传。
- BGRA 色彩转换仍在 CPU：RM 没有计算通道，copy engine 只能搬运。C 驱动这一路径本身在测试中失败。
- 同步在 CPU 侧：解码完成与 copy engine 完成都由 CPU 等待，未做 GPU 侧 fence 串联。
- 不支持带压缩（c≠0）的块线性修饰符和 Turing 之前的 GOB 排布。

### Chrome 硬件编码（Chrome 156，Wayland 窗口，`--disable-gpu-sandbox`，临时 profile）

| 测试页项目 | rustnv | C 驱动 |
|---|---|---|
| WebCodecs H.264 Baseline / High（`prefer-hardware`，720p） | 硬件编码，各 60 帧、2 个关键帧 | 相同 |
| WebCodecs HEVC / AV1 | Chrome 不提供 | 相同 |
| WebRTC H.264 / AV1 | Chrome 用软件编码器（OpenH264 / libaom），两驱动相同 | 相同 |
| 硬件编码 720p 30 帧经软件解码与源画面 PSNR | 平均 28.10 dB、最低 27.96 dB | 平均 28.10 dB、最低 27.96 dB |

Chrome 经 DRM_PRIME_2 导入 NV12（C 驱动下的 libva trace：两个 object 指向同一缓冲、修饰符 0 即线性、pitch 1280、UV 偏移 983040），由 copy engine 送入 NVENC，不经 CPU。软件编码同一画面 PSNR 约 36.9 dB；两驱动硬件结果完全相同，差距来自 Chrome 的 RGB→NV12 输入与码率，不在驱动。
