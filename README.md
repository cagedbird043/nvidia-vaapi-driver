# nvidia-vaapi-driver（Rust）

[English](#english) · 中文

用 Rust 写的 NVIDIA VA-API 驱动：直接对 NVIDIA 开源内核模块发 ioctl 驱动 NVDEC / NVENC，**不使用 CUDA**，不加载 libcuda / libnvcuvid。本仓库原是 [elFarto/nvidia-vaapi-driver](https://github.com/elFarto/nvidia-vaapi-driver) 的 fork，现已用 Rust 重写；RM 后端来自 [nvrm](https://github.com/cagedbird043/nvrm)（MIT）。

## 能力

| | 格式 |
|---|---|
| 解码 | H.264；HEVC（Main / Main10 / Main12 / Main444 / Main444_10 / Main444_12）；VP9 Profile 0 / 2；AV1 Profile 0（8 / 10-bit）；MPEG-2；MPEG-4 Part 2（SP / ASP）；VC-1；VP8 |
| 编码 | H.264（Constrained Baseline / Main / High，含 4:4:4）；HEVC（Main / Main10 / Main444 / Main444_10）；AV1 Profile 0；CQP / CBR / VBR |
| 导出 | `vaExportSurfaceHandle`（DRM PRIME 2，块线性修饰符），解码画面零 CPU 拷贝 |
| 导入 | DRM PRIME / PRIME 2 的 dma-buf 作编码输入（NV12 / P010 / P012 / BGRA），经 GPU copy engine 送入 NVENC |

解码最大尺寸：H.264 / VP8 4096，MPEG-2 4080，MPEG-4 / VC-1 2048，HEVC / VP9 / AV1 8192。编码最大 8192×8192。各格式细节见 [docs/design.md](docs/design.md)。

## 兼容性

- 目标：Turing 及更新架构的 NVIDIA GPU，使用 **nvidia-open** 内核模块（专有模块不在测试范围）。
- 只在 RTX 4060 Laptop（Ada）/ nvidia-open 615.78.08 / x86_64 glibc 上实测。
- RM 按 NVIDIA 驱动版本精确匹配内核 ABI，不认识的版本会拒绝运行，所以支持的驱动版本取决于所用 nvrm 版本。

## 安装

**Arch Linux（AUR）**：

```sh
yay -S nvrm-vaapi-driver
```

该包 `provides` `libva-nvidia-driver`，并与 `libva-nvidia-driver`、`libva-nvidia-driver-cagedbird` 冲突。

**手动**：依赖 Rust、Clang / libclang、libva 开发头文件。

```sh
cargo build --release
sudo install -Dm755 target/release/librustnv_drv_video.so /usr/lib/dri/nvidia_drv_video.so
```

libva 对 `nvidia-drm` 设备自动加载名为 `nvidia` 的驱动，不需要额外设置。

不安装试用：

```sh
ln -sfn librustnv_drv_video.so target/release/rustnv_drv_video.so
export LIBVA_DRIVERS_PATH="$PWD/target/release" LIBVA_DRIVER_NAME=rustnv
vainfo --display drm --device /dev/dri/renderD129
```

## 使用

先用 `vainfo --display drm --device /dev/dri/renderD129` 确认驱动可用（`renderD129` 换成 NVIDIA 的渲染节点）。

**mpv**

```sh
mpv --hwdec=vaapi --hwdec-software-fallback=no --vo=gpu --gpu-api=opengl video.mp4
```

mpv 默认的 hwdec 白名单不含 MPEG-2 / MPEG-4，要硬解它们加 `--hwdec-codecs=all`。

**Firefox**：在 `about:config` 设 `media.ffmpeg.vaapi.enabled = true`（部分版本还需要 `media.hardware-video-decoding.force-enabled = true`）。RDD 进程里会出现 `rustnv-nvdec` 线程。

**Chrome / Chromium**：需要关闭 GPU 沙箱，并指定 NVIDIA 渲染节点（否则 Chrome 跳过 nvidia-drm 设备，回退软件解码）：

```sh
google-chrome-stable --ozone-platform=wayland --disable-gpu-sandbox --ignore-gpu-blocklist \
  --render-node-override=/dev/dri/renderD129 \
  --enable-features=VaapiVideoDecoder,VaapiOnNvidiaGPUs,VaapiIgnoreDriverChecks,AcceleratedVideoDecodeLinuxGL,AcceleratedVideoDecodeLinuxZeroCopyGL,PlatformHEVCDecoderSupport
```

**ffmpeg**

```sh
ffmpeg -hwaccel vaapi -hwaccel_device /dev/dri/renderD129 -hwaccel_output_format vaapi -i in.mkv -f null -
ffmpeg -init_hw_device vaapi=va:/dev/dri/renderD129 -filter_hw_device va \
  -f lavfi -i testsrc2=size=1920x1080:rate=30 -vf format=nv12,hwupload \
  -frames:v 120 -c:v h264_vaapi -profile:v high -rc_mode CQP -qp 26 out.mkv
```

**GStreamer**：`va` 插件默认只注册已知驱动，需要 `GST_VA_ALL_DRIVERS=1`：

```sh
GST_VA_ALL_DRIVERS=1 gst-launch-1.0 filesrc location=in.mp4 ! parsebin ! vah264dec ! fakesink
```

## 环境变量

| 变量 | 作用 |
|---|---|
| `NVD_LOG` | `1` 输出日志到 stdout，其他值为追加写入的文件路径；默认静默 |
| `NVD_LOG_VERBOSE` | 非 `0` 时额外记录每个 VA 调用（需 `NVD_LOG`） |
| `NVD_GPU` | 序号：忽略 libva 的 DRM fd，取第 N 个 NVIDIA 渲染节点（renderD128 起） |
| `NVD_MAX_INSTANCES` | 进程内同时打开的 VA display 上限，超出返回 `VA_STATUS_ERROR_HW_BUSY` |
| `NVD_DRM_PRIME_ASSUME_LINEAR` | 旧版 DRM_PRIME 描述符（不带修饰符）按线性接受 |

与 C 版其余环境变量的对应关系见 [docs/compare-c.md](docs/compare-c.md)。

## 已知限制

- 不支持 JPEG：C 版的 JPEG 走 CUDA 内核，本驱动不用 CUDA。
- 不支持 VideoProc（`vaCreateConfig(VAProfileNone, VAEntrypointVideoProc)` 返回 `UNSUPPORTED_PROFILE`），`scale_vaapi` 等 VPP 滤镜不可用；C 版的 VideoProc 同样只有最小实现。
- Chrome 需要 `--disable-gpu-sandbox` 与 `--render-node-override`：默认沙箱下 Chrome 重新打开 VA display 被其 broker 拒绝，驱动无法绕过。
- 4:2:2 解码、VP9 / AV1 4:4:4、AV1 12-bit、H.264 隔行与 FMO 等 NVDEC 不支持的情形会返回错误。
- BGRA 输入的 GPU 色彩转换只在实测过的 Ada 上启用，其他架构回退到 CPU 转换。
- 视频帧同步在 CPU 侧完成，编码提交为同步。

完整列表见 [docs/design.md](docs/design.md)。

## 与 C 版对比

RTX 4060 Laptop / nvidia-open 615.78.08，ffmpeg VA-API，输出留在 GPU，每项取两轮最快（fps 含进程启动）：

| 用例 | C 版 | 本驱动 |
|---|---:|---:|
| H.264 4K 解码 | 149 fps | 166 fps |
| HEVC 4K 解码 | 304 fps | 400 fps |
| AV1 4K 解码 | 343 fps | 480 fps |
| H.264 1080p 编码（CQP） | 214 fps | 390 fps |
| HEVC 1080p 编码（CQP） | 203 fps | 455 fps |
| 进程峰值 RSS（HEVC 4K 解码） | 595 MB | 291 MB |
| `vainfo` 初始化 | 431 ms | 2 ms |

另外 MPEG-4 Part 2 可以硬解（C 版不能），GStreamer `va` 插件可用。完整对照见 [docs/compare-c.md](docs/compare-c.md)。

## 文档

- [docs/design.md](docs/design.md)：架构、各格式要点、已知限制
- [docs/testing.md](docs/testing.md)：回归脚本用法与验证结果
- [docs/compare-c.md](docs/compare-c.md)：与 C 版逐项对照
- [docs/development.md](docs/development.md)：构建、本地联调 nvrm、`Cargo.lock` 注意事项

## 致谢

- [elFarto/nvidia-vaapi-driver](https://github.com/elFarto/nvidia-vaapi-driver)：上游项目，本仓库的来源与行为基线（MIT）。
- [mickys/nvenc](https://github.com/mickys/nvenc)（MIT）：NVENC 编码核心的移植来源（经 nvrm）。
- NVIDIA 公开的开源内核模块头文件（MIT）。
- RFC 6386（VP8 默认概率表）、libvpx（VP9 表，BSD-3-Clause）、Mesa（AV1 CDF 默认值，MIT）。

## 许可

MIT，见 [COPYING](COPYING)。nvrm 的第三方部分许可见其仓库的 `LICENSE`。

---

# English

A VA-API driver for NVIDIA GPUs written in Rust. It talks directly to the open NVIDIA kernel modules through ioctls to drive NVDEC / NVENC, **without CUDA**, libcuda or libnvcuvid. This repository started as a fork of [elFarto/nvidia-vaapi-driver](https://github.com/elFarto/nvidia-vaapi-driver) and has since been rewritten in Rust. The RM backend is [nvrm](https://github.com/cagedbird043/nvrm) (MIT).

## Capabilities

| | Formats |
|---|---|
| Decode | H.264; HEVC (Main / Main10 / Main12 / Main444 / Main444_10 / Main444_12); VP9 Profile 0 / 2; AV1 Profile 0 (8 / 10-bit); MPEG-2; MPEG-4 Part 2 (SP / ASP); VC-1; VP8 |
| Encode | H.264 (Constrained Baseline / Main / High incl. 4:4:4); HEVC (Main / Main10 / Main444 / Main444_10); AV1 Profile 0; CQP / CBR / VBR |
| Export | `vaExportSurfaceHandle` (DRM PRIME 2, block-linear modifiers), no CPU copy for decoded frames |
| Import | DRM PRIME / PRIME 2 dma-bufs as encoder input (NV12 / P010 / P012 / BGRA), fed to NVENC by the GPU copy engine |

Maximum decode size: H.264 / VP8 4096, MPEG-2 4080, MPEG-4 / VC-1 2048, HEVC / VP9 / AV1 8192. Maximum encode size is 8192×8192. Details: [docs/design.md](docs/design.md) (Chinese).

## Compatibility

- Target: Turing and newer GPUs with the **nvidia-open** kernel modules (the proprietary modules are not tested).
- Tested only on RTX 4060 Laptop (Ada) / nvidia-open 615.78.08 / x86_64 glibc.
- RM matches the kernel ABI to the exact NVIDIA driver version and refuses unknown versions, so the supported driver versions depend on the nvrm revision in use.

## Installation

**Arch Linux (AUR)**:

```sh
yay -S nvrm-vaapi-driver
```

The package provides `libva-nvidia-driver` and conflicts with `libva-nvidia-driver` and `libva-nvidia-driver-cagedbird`.

**Manual**: needs Rust, Clang / libclang and the libva development headers.

```sh
cargo build --release
sudo install -Dm755 target/release/librustnv_drv_video.so /usr/lib/dri/nvidia_drv_video.so
```

libva loads the driver named `nvidia` for `nvidia-drm` devices automatically; no extra configuration is needed.

Try it without installing:

```sh
ln -sfn librustnv_drv_video.so target/release/rustnv_drv_video.so
export LIBVA_DRIVERS_PATH="$PWD/target/release" LIBVA_DRIVER_NAME=rustnv
vainfo --display drm --device /dev/dri/renderD129
```

## Usage

Check with `vainfo --display drm --device /dev/dri/renderD129` first (use your NVIDIA render node).

- **mpv**: `mpv --hwdec=vaapi --hwdec-software-fallback=no --vo=gpu --gpu-api=opengl video.mp4`. Add `--hwdec-codecs=all` for MPEG-2 / MPEG-4.
- **Firefox**: set `media.ffmpeg.vaapi.enabled = true` in `about:config` (some versions also need `media.hardware-video-decoding.force-enabled = true`).
- **Chrome / Chromium**: the GPU sandbox must be disabled and the NVIDIA render node given explicitly; see the command in the Chinese section above.
- **ffmpeg**: `-hwaccel vaapi -hwaccel_device /dev/dri/renderD129 -hwaccel_output_format vaapi` for decoding; `-init_hw_device vaapi=va:/dev/dri/renderD129 -filter_hw_device va … -vf format=nv12,hwupload -c:v h264_vaapi` for encoding.
- **GStreamer**: set `GST_VA_ALL_DRIVERS=1` so the `va` plugin registers elements for this driver.

## Environment variables

| Variable | Effect |
|---|---|
| `NVD_LOG` | `1` logs to stdout, any other value is a file to append to; silent by default |
| `NVD_LOG_VERBOSE` | non-`0` also logs every VA call (requires `NVD_LOG`) |
| `NVD_GPU` | index: ignore libva's DRM fd and use the N-th NVIDIA render node (from renderD128) |
| `NVD_MAX_INSTANCES` | cap on simultaneously open VA displays per process; beyond it `VA_STATUS_ERROR_HW_BUSY` |
| `NVD_DRM_PRIME_ASSUME_LINEAR` | accept legacy DRM_PRIME descriptors (no modifier) as linear |

## Known limitations

- No JPEG (the C driver decodes it with CUDA kernels; this driver does not use CUDA).
- No VideoProc: `scale_vaapi` and other VPP filters are unavailable.
- Chrome needs `--disable-gpu-sandbox` and `--render-node-override`.
- Formats NVDEC cannot decode (4:2:2, VP9 / AV1 4:4:4, AV1 12-bit, interlaced H.264, FMO, …) return errors.
- GPU colour conversion of BGRA encoder input is enabled only on the tested Ada GPU; other architectures fall back to the CPU.
- Frame synchronisation is CPU-side and encode submission is synchronous.

## Performance vs. the C driver

See the table in the Chinese section above and [docs/compare-c.md](docs/compare-c.md).

## Acknowledgements

elFarto/nvidia-vaapi-driver (upstream, MIT); mickys/nvenc (MIT); NVIDIA's open kernel-module headers (MIT); RFC 6386, libvpx (BSD-3-Clause) and Mesa (MIT) for VP8 / VP9 / AV1 tables.

## License

MIT, see [COPYING](COPYING).
