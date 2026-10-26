# Rust NVIDIA VA-API H.264 驱动

独立的宿主机 VA-API 驱动，通过共享 Rust RM 后端直接使用 NVIDIA NVDEC。驱动名为 `rustnv`，在单个测试进程中选择，不安装到系统。

## 支持范围

- H.264 Baseline / Constrained Baseline / Main / High，逐行、8-bit、4:2:0。
- P/B 参考帧、多切片、宏块补齐与显示尺寸裁剪。
- NV12 CPU 下载，以及 DRM PRIME2 的 R8 + RG88 分层或 NV12 合并描述符。
- 解码参数直接来自 VA-API，转换为 NVDEC 描述符；不重建 SPS/PPS 码流，不调用 CUDA 或 libnvcuvid。
- 解码 context 销毁后，存活的 VA surface 仍持有 worker 和画面资源。
- surface 可以在解码前导出。导出的 backing 在 surface 生命周期内固定；后续解码通过 GPU copy engine 更新它，随后同步完成。显示路径无 CPU 画面回拷，但包含一次 GPU 拷贝。
- 每个 context 的 GPU 对象由专属线程持有；VA 回调通过消息调用。当前 EndPicture 同步等待解码完成。

这不是现有驱动的全功能替代品：不支持编码、其他编解码格式、隔行、10-bit、FMO、分段 slice，以及 CPU 图像上传/视频后处理。未实现的入口明确返回错误。mpv 的可选 CPU 上传探测会记录一次 `vaPutImage` 不支持，已验证的硬解显示不依赖该功能。

RM 使用精确 NVIDIA 驱动 ABI 匹配；未知驱动版本会拒绝运行。本次运行验证范围仅为 RTX 4060 Laptop / nvidia-open 615.78.08 / x86_64 glibc。

## 构建

依赖 Rust、Clang/libclang、libva 开发头文件；运行验证还需要 FFmpeg（含 libx264）和 mpv。共享 RM 源码位于相邻的 `../waydroid-nvidia/nvidia-prolapse/driver-source/native`，需要包含本工作区的外部解码器与输出分配接口改动。

在本目录执行：

```sh
cargo build --release
ln -sfn librustnv_drv_video.so target/release/rustnv_drv_video.so
```

## 播放

```sh
LIBVA_DRIVERS_PATH="$PWD/target/release" LIBVA_DRIVER_NAME=rustnv \
  mpv --no-config --hwdec=vaapi --hwdec-software-fallback=no \
  --vo=gpu --gpu-api=opengl --gpu-context=wayland video.mp4
```

应显示 `Using hardware decoding (vaapi)` 和 `VO: [gpu] ... vaapi[nv12]`。上述环境变量只作用于该进程。渲染使用宿主 NVIDIA EGL/GL，解码不加载 `libcuda`、`libnvcuvid`。

## 硬件回归

```sh
python3 tests/h264.py --device /dev/dri/renderD129
```

脚本生成临时测试视频，比较软件与 VA-API 下载后的每一帧 NV12 哈希，检查包含尾帧的精确帧数；失败即退出，结束自动清理样本。`--driver` 可指定其它构建的 `.so`。

2026-10-26 实测：

| 样本 | 覆盖 | 结果 |
|---|---|---|
| High 1280×720 | 3 B 帧、重复关键帧、surface 池循环 | 480/480 帧逐字节一致 |
| Main 854×480 | 2 B 帧、4 slices、宏块 padding | 96/96 帧逐字节一致 |
| Baseline 640×360 | 无 B 帧 | 90/90 帧逐字节一致 |

mpv Wayland/OpenGL 已实播 High 与 Main，检查了跳转、循环和切换视频；观察时 `hwdec-current=vaapi`，解码/显示丢帧均为 0。原有原生 H.264 码流接口另外通过 60 帧逐字节比对。
