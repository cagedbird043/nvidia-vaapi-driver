# Rust NVIDIA VA-API 解码驱动

独立的宿主机 VA-API 解码驱动，通过共享 Rust RM 后端直接使用 NVIDIA NVDEC，不调用 CUDA 或 libnvcuvid。驱动名为 `rustnv`，
在单个测试进程中选择，不安装到系统。

## 支持范围

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

明确拒绝并返回错误的情况：10/12-bit、非 4:2:0、隔行/场编码、H.264 FMO、HEVC PCM 与分离色彩平面、
VP9 在分段图写入后改变编码尺寸、AV1 film grain、large-scale tile 与帧间 superres。不支持编码、CPU 图像上传和视频后处理。
mpv 的可选 CPU 上传探测会记录一次 `vaPutImage` 不支持，硬解显示不依赖该功能。

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

依赖 Rust、Clang/libclang、libva 开发头文件；回归还需要 FFmpeg（libx264、libx265、libvpx、libsvtav1、libaom）。
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
