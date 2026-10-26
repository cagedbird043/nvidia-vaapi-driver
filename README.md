# Rust NVIDIA VA-API 驱动

独立的宿主机 VA-API 驱动，通过共享 Rust RM 后端直接使用 NVIDIA NVDEC / NVENC，不调用 CUDA、libnvcuvid 或 NVENC SDK。驱动名为 `rustnv`，
在单个测试进程中选择，不安装到系统。

## 支持范围

### 解码（VAEntrypointVLD）

| 格式 | VA profile |
|---|---|
| H.264 | Baseline / Constrained Baseline / Main / High |
| HEVC | Main |
| VP9 | Profile 0 |
| AV1 | Profile 0 |

已验证的覆盖见下方“硬件回归”。

- 全部为 8-bit 4:2:0 逐行，支持非宏块/CTB/超块对齐的显示尺寸。
- 解码参数直接来自 VA-API，转换为 NVDEC 描述符；不重建参数集码流。
- 输出：NV12 CPU 下载；DRM PRIME2 的 R8 + RG88 分层或 NV12 合并描述符（块线性修饰符）。
- 只在 libva 打开的那块 NVIDIA GPU 上解码；初始化时由 DRM fd 识别设备，非 nvidia 驱动的节点直接拒绝。

解码明确拒绝并返回错误的情况：10/12-bit、非 4:2:0、隔行/场编码、H.264 FMO、HEVC PCM 与分离色彩平面、
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
- GPU 从 libva 的 DRM fd 选择，与解码一致。解码 surface 作编码输入不经 CPU：surface 的导出 backing（8 行 GOB、单 GOB 块高的块线性）由 GPU copy engine 重排进 NVENC 输入槽，字节与 CPU tiler 一致。外部 DMA-BUF 的 NV12 / P010 同走 copy engine；BGRA 先经 copy engine 拷入系统内存 staging，再由 CPU 按 BT.709 转换（见“遗留”）。
- RM 的帧率接口为整数，分数帧率四舍五入。RateControl / FrameRate misc 参数被消费；其余 misc（包含 HRD / QualityLevel）与当前 C 后端一样接受但不消费。

FFmpeg 经 `hwupload` 协商不了 H.264 444 和 HEVC Q416，这两种输入由回归脚本里的直接 VA 客户端测试，并检查输出色度/位深。

## 设计

- 每个 context 一个 worker 线程持有 RM 会话；VA 回调把解码和导出拷贝排进队列后返回，`vaSyncSurface`、`vaDeriveImage`、
  `vaGetImage` 不持全局锁等待完成。worker 退出时未完成的工作置为失败，等待方不会卡死。
- surface 可以在解码前导出；导出的显存在 surface 生命周期内固定，每次解码后由 GPU copy engine 更新。显示路径没有 CPU 拷贝，
  有一次 GPU 拷贝。
- 解码 context 销毁后，存活的 surface 仍持有 worker 和画面。
- `src/backend/mod.rs` 管会话、导出和下载；`src/backend/{h264,hevc,vp9,av1}.rs` 各自把 VA 参数转换成 RM 的描述符输入。

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

性能（ffmpeg VA-API 解码吞吐，与系统里的 C 版 nvidia-vaapi-driver 对比）：

| 片源 | rustnv | C 驱动 |
|---|---|---|
| H.264 1080p60 | 773 fps，CPU 0.30 ms/帧 | 680 fps，0.56 ms/帧 |
| H.264 4K60 | 216 fps，CPU 0.91 ms/帧 | 203 fps，1.35 ms/帧 |

mpv 实时播放 H.264 4K60 两者都是 60 fps、0 丢帧。浏览器：Firefox 158 播放 H.264/HEVC/VP9/AV1 由 rustnv 解码；
Chrome 156（关闭 GPU 沙箱）四种格式由 rustnv 解码，GPU 进程不加载 `libcuda`/`libnvcuvid`。

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
`cargo build --release` 无警告；`tests/decode.py` 的 9 组全部逐字节一致；RM `cargo test --lib video::` 为 60 passed / 20 ignored；176 组 C/Rust NVENC 对照的 setup 和码流全部逐字节一致。

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
