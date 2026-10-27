# 与 C 版对照

对象：[elFarto/nvidia-vaapi-driver](https://github.com/elFarto/nvidia-vaapi-driver) 的 C 版（direct 后端，经 CUDA / libnvcuvid）。2026-10-27 在 RTX 4060 Laptop / nvidia-open 615.78.08 上实测；rustnv 为 `LIBVA_DRIVER_NAME=rustnv`，C 版为 `LIBVA_DRIVER_NAME=nvidia`。

## 入口

`__vaDriverInit_1_0` 填写的 `VADriverVTable` 槽，逐项：

| 槽 | C 版 | rustnv |
|---|---|---|
| Terminate、QueryConfigProfiles/Entrypoints/Attributes、CreateConfig/DestroyConfig、GetConfigAttributes | 实现 | 实现；profile 列表无 JPEG（不支持）；`VAProfileNone` 只列一次，入口为 `VAEntrypointVideoProc`（C 版把它列了两次且 `vaQueryConfigEntrypoints` 对它返回 UNSUPPORTED_PROFILE，GStreamer `va` 插件因此注册不出任何元素） |
| CreateSurfaces/2、DestroySurfaces、CreateContext/DestroyContext、CreateBuffer、BufferSetNumElements、MapBuffer/UnmapBuffer/DestroyBuffer、BufferInfo、Begin/Render/EndPicture、QuerySurfaceAttributes、ExportSurfaceHandle | 实现 | 实现 |
| SyncSurface | 实现 | 实现 |
| SyncSurface2、SyncBuffer、MapBuffer2 | 不在 vtable（libva 返回 UNIMPLEMENTED） | 实现；`vaSyncSurface2` 的超时：0 只查询，`UINT64_MAX` 一直等，到期仍在运行返回 `VA_STATUS_ERROR_TIMEDOUT` |
| QuerySurfaceStatus | UNIMPLEMENTED | 实现（Rendering / Ready） |
| QuerySurfaceError | UNIMPLEMENTED | UNIMPLEMENTED |
| QueryImageFormats / CreateImage | NV12、NV16、P010、P210、P012、P016、ARGB、444P、Q416 | NV12、P010、P012、P016、444P、Q416、BGRA、BGRX、ARGB、XRGB、RGBA、RGBX。NV16、P210 只对 4:2:2 surface 有意义，本 GPU 的 NVDEC 没有 4:2:2 解码 |
| DeriveImage | 恒返回 `OPERATION_FAILED` | 实现（解码 surface 读回；编码输入 surface 直接映射） |
| GetImage | 实现（忽略 x、y，从原点拷贝；surface 没有 context 时返回 INVALID_CONTEXT） | 实现，检查区域，必须与图像同尺寸；surface 与图像格式需一致（P010/P012 可读入 P016 图像） |
| PutImage | 仅同尺寸、同格式，缩放返回 UNIMPLEMENTED | 仅整幅 surface（用作编码输入），否则 INVALID_PARAMETER |
| PutSurface、SetImagePalette、LockSurface/UnlockSurface、Acquire/ReleaseBufferHandle、CreateMFContext 等 MF*、CreateBuffer2、QueryProcessingRate | UNIMPLEMENTED | UNIMPLEMENTED |
| QuerySubpictureFormats | 0 个格式，成功 | 同 |
| CreateSubpicture 等其余 subpicture 槽 | UNIMPLEMENTED | UNIMPLEMENTED |
| QueryDisplayAttributes | 0 个，成功 | 同；Get/SetDisplayAttributes 为 UNIMPLEMENTED，两者一致 |
| vaCopy | 不在 vtable | UNIMPLEMENTED |
| `vtable_vpp`（QueryVideoProcFilters / FilterCaps / PipelineCaps） | 实现，仅对 `VAProfileNone` + `VAEntrypointVideoProc` 的 context：无滤镜，`VA_STATUS_ERROR_UNSUPPORTED_FILTER` | 实现：无滤镜，滤镜参数返回 `VA_STATUS_ERROR_UNSUPPORTED_FILTER`；PipelineCaps 报告 12 种输入 / 输出格式、颜色标准与 chroma siting，不报旋转、镜像、混合 |

C 版的 VideoProc 是最小实现：只做同尺寸、原点为 0 的同格式拷贝，以及 NV12/P010/P012 到 ARGB 的色彩转换（CUDA 内核）。缩放、变格式、滤镜都失败（`ffmpeg -vf scale_vaapi=w=640:h=360` 报 “Failed to render parameter buffer”，`format=p010` 同样）。rustnv 覆盖这两项，并加上双线性缩放、裁剪、目的区域与背景色、NV12 / P010 / P012 / P016 / 444P / Q416 / RGB32 之间互转与 BT.601 / 709 / 2020 矩阵转换（copy engine + compute 内核，不用 CUDA），见 [design.md](design.md#视频处理)。

`VADriverContext` 字段：

| 字段 | C 版 | rustnv |
|---|---|---|
| max_profiles | 32 | 29 |
| max_entrypoints | 3 | 2 |
| max_attributes | 8 | 64 |
| max_image_formats | 9 | 6 |
| max_subpic_formats / max_display_attributes | 1 / 1 | 1 / 1 |
| str_vendor | `VA-API NVDEC/NVENC driver [direct backend]`，Chromium GPU 进程加前缀 `Chromium fake libva driver compatible: ` | `Rust NVIDIA RM NVDEC/NVENC driver`，同样的 Chromium 前缀判断（`--type=gpu-process`） |

`vainfo -a` 逐 profile / entrypoint 比较（`/dev/dri/renderD129`）：除 vendor 串和 JPEG Baseline 外属性完全一致，包括 MPEG-4 的 `MaxPictureWidth/Height`（C 版报 0，因为 MPEG-4 在 C 版被禁用；rustnv 报 2048）；解码最大尺寸为 H.264 / VP8 4096、MPEG-2 4080、MPEG-4 / VC-1 2048、HEVC / VP9 / AV1 8192（8192×4320 的 HEVC / VP9 / AV1 逐字节通过 `tests/decode.py`）。profile 的出现顺序不同，`vainfo` 按驱动返回的顺序打印。`vaCreateConfig` 与 C 版一样只看 `VAConfigAttribRTFormat`，其余属性忽略。

## 环境变量与兼容行为

C 版读取的变量，与 rustnv 的处理：

| 变量 | C 版作用 | rustnv |
|---|---|---|
| `NVD_LOG` | `1` 输出到 stdout，其余值为追加写的文件路径 | 同；默认静默。行格式 `[rustnv] 秒.纳秒 [pid-tid] 文件:行 消息`，记录初始化选中的 GPU、实例数、解码 context、每个失败的 VA 调用及状态 |
| `NVD_LOG_VERBOSE` | 非 `0` 时输出调试日志 | 同：额外输出每个 VA 调用的名字（需 `NVD_LOG`） |
| `NVD_GPU` | 序号：忽略 libva 的 DRM fd，取第 N 个 NVIDIA 渲染节点（renderD128 起），同时是 CUDA 设备号 | 同：序号对应第 N 个 NVIDIA 渲染节点；超出范围初始化失败 |
| 设备选择（未设 `NVD_GPU`） | libva 的 DRM fd 属于 NVIDIA 就用它；否则用第一块 NVIDIA 渲染节点 | 同 |
| `NVD_MAX_INSTANCES` | 进程内同时打开的 VA display 上限，超出返回 `VA_STATUS_ERROR_HW_BUSY`，`vaTerminate` 释放 | 同 |
| `NVD_DRM_PRIME_ASSUME_LINEAR` | 旧 DRM_PRIME 描述符按线性接受 | 同 |
| `NVD_ENABLE_CLIENT_PACKED_HEADERS` / `NVD_DISABLE_CLIENT_PACKED_HEADERS` | 默认忽略应用 packed SPS/PPS，使用 NVENC 生成的参数集；前者打开把客户端头拼进码流的兼容路径（C 版自注“脆弱”） | 固定为默认行为（RM 生成的参数集），不提供拼接路径 |
| `NVD_BACKEND`（`direct` / `egl`） | 选导出后端 | 无后端之分，变量被忽略，不报错 |
| Chromium vendor 前缀 | GPU 进程（`--type=gpu-process`）的 vendor 串加前缀，Chrome 据此用 DRM_PRIME_2 导入编码输入 | 同 |
| `NVD_DECODE_SURFACE_COUNT`、`NVD_SINGLE_BUFFER`、`NVD_MAX_DETACHED_BACKING_IMAGE(S/_BYTES)` | CUDA 解码器 surface 数、backing 缓存 | 不适用：rustnv 的 surface 池由应用绑定的 render target 数决定，没有 backing 缓存 |
| `NVD_USE_PRIMARY_CUDA_CONTEXT`、`NVD_DISABLE_CUDA_INIT_BACKOFF`、`NVD_FORCE_INIT`、`NVD_ENCODE_PROBE_CACHE`、`NVD_STATS(_LOG)`、`NVD_*DMABUF_CUDA_IMPORT`、`NVD_EXPERIMENTAL_*`、`NVD_DIRECT_IMPORT_*`、`NVD_*GPU_COPY*`、`NVD_FORCE_CPU_COPY_IMPORT` | CUDA 上下文、CUDA 导入路径、探测缓存、统计与实验开关 | 不适用：没有 CUDA；NVENC 能力由 RM 直接查询，无需缓存 |

## 应用矩阵

| 应用 | 测试 | C 版 | rustnv |
|---|---|---|---|
| `vainfo -a` | 全量文本 | 见上，除 vendor 串、JPEG、顺序外一致 | 同 |
| ffmpeg（`-hwaccel vaapi`） | 解码 H.264 / HEVC / HEVC Main10 / VP9 / AV1 / MPEG-2 / VP8 | 通过 | 通过 |
| ffmpeg | 解码 MPEG-4 part 2 | ffmpeg 静默回落软解（CPU 约 4 s / 3000 帧） | 硬解 |
| ffmpeg | 编码 `h264_vaapi` / `hevc_vaapi` / `av1_vaapi`，CQP 与 CBR，1080p / 4K，含硬解到硬编的转码 | 通过 | 通过 |
| ffmpeg | 8192×4320 解码 HEVC / VP9 / AV1 | 通过（逐字节等于软解） | 通过 |
| ffmpeg | `scale_vaapi` 缩放 / 变格式 | 失败（Failed to render parameter buffer） | 通过；`tests/vpp.py`：放大与同尺寸变格式与 swscale bilinear 逐字节一致，1080p → 720p PSNR ≈ 46 dB |
| mpv `--hwdec=vaapi-copy` 与 `vaapi --vo=gpu` | H.264 / HEVC / HEVC10 / VP9 / AV1 / VP8 / MPEG-2 / HEVC 4K | “Using hardware decoding” | 相同 |
| GStreamer 1.28 `va` 插件（`GST_VA_ALL_DRIVERS=1`） | 注册元素 | 0 个（上述 `VAProfileNone` 问题） | vah264dec / vah265dec / vavp9dec / vavp8dec / vaav1dec / vampeg2dec / vah264enc / vah265enc / vaav1enc / vapostproc；无 vacompositor、vadeinterlace |
| GStreamer | `gst-launch-1.0` 解码六种格式、编码三种格式、`vapostproc` 缩放与变格式 | 无元素，不可用 | `tests/gstreamer.py`：解码与软解逐帧一致（MPEG-2 PSNR 68.7 dB），编码输出软解后帧数与 PSNR 合格；`vapostproc` 输出与 zscale 参考比较（NV12 → BGRA 103 dB） |
| Firefox（`media.ffmpeg.vaapi.enabled`，临时 profile） | H.264 / HEVC / VP9 / AV1 / VP8 `<video>` 播放 | “VA-API FFmpeg init successful”，逐帧 VA-API 解码（测试时设了 `MOZ_DISABLE_RDD_SANDBOX=1`） | 同样成功，未设该变量 |
| Chrome 156（`--disable-gpu-sandbox`，临时 profile） | 同五种格式播放；WebCodecs H.264 Baseline / High `prefer-hardware` 编码 60 帧 | 通过，码流 300229 B | 通过，码流 301622 B（Baseline）；HEVC / VP9 / AV1 编码 Chrome 不提供 |
| libva-utils `avcenc` / `h264encode` / `hevcencode` | 无窗口系统（`env -u WAYLAND_DISPLAY -u DISPLAY`）的 drm 显示 | 输出 0 字节（初始化失败） | 正常输出码流 |
| libva-utils `vp8enc` / `mpeg2vaenc` / `vp9enc` | 编码 | 无此入口 | 无此入口 |
| libva-utils `vavpp` | VideoProc（`FILTER_TYPE: VAProcFilterNone`，640×480 → 320×240） | 未测 | 通过 |
| libva-utils `vacopy`、`jpegenc`、`loadjpeg`、`putsurface` | vaCopy / JPEG / vaPutSurface | vaCopy 不在 vtable；JPEG 走 CUDA；PutSurface 未实现 | 不可用（vaCopy、JPEG、PutSurface 未实现） |

GStreamer 在 AV1 关键帧上把 `ref_frame_idx` 填 255；驱动对关键帧不校验也不使用该字段。

## 性能与资源

ffmpeg 解码到 `-f null`，输出保持在 GPU（`hwaccel_output_format vaapi`）；1080p 循环 2400 帧，4K 循环 480 帧；编码为 `hwupload` 的 NV12 原始输入（1080p 300 帧、4K 90 帧）；转码为硬解到硬编（CQP，qp 26）；每项两轮取最快。fps 含进程启动；CPU 为用户加系统秒数；RSS 为进程峰值；GPU 为 `nvidia-smi` 整卡 `memory.used` 相对运行前的峰值增量（MiB，包含桌面等其他进程的波动）。

解码：

| 用例 | fps C | fps rustnv | CPU s C | CPU s rustnv | RSS MB C | RSS MB rustnv | GPU MiB C | GPU MiB rustnv |
|---|---|---|---|---|---|---|---|---|
| H.264 1080p | 587 | 619 | 1.32 | 0.85 | 408 | 354 | 368 | 293 |
| H.264 4K | 149 | 166 | 1.08 | 0.71 | 695 | 496 | 770 | 742 |
| HEVC 1080p | 635 | 688 | 1.25 | 0.81 | 358 | 307 | 319 | 281 |
| HEVC 4K | 304 | 400 | 0.80 | 0.42 | 595 | 291 | 793 | 694 |
| VP9 1080p | 927 | 1062 | 1.14 | 0.66 | 421 | 241 | 349 | 284 |
| VP9 4K | 257 | 316 | 0.87 | 0.45 | 449 | 141 | 794 | 621 |
| AV1 1080p | 741 | 854 | 0.95 | 0.44 | 312 | 200 | 278 | 240 |
| AV1 4K | 343 | 480 | 0.64 | 0.23 | 402 | 78 | 646 | 584 |
| HEVC Main10 1080p | 1290 | 1678 | 1.01 | 0.49 | 350 | 138 | 458 | 481 |
| MPEG-2 720p | 1194 | 1583 | 1.08 | 0.53 | 296 | 163 | 173 | 122 |
| VP8 720p | 1463 | 1775 | 0.98 | 0.39 | 288 | 102 | 194 | 122 |
| MPEG-4 720p | 3030（C 版软解） | 1875 | 3.97 | 0.39 | 329 | 129 | 99 | 123 |

编码：

| 用例 | fps C | fps rustnv | CPU s C | CPU s rustnv | RSS MB C | RSS MB rustnv | GPU MiB C | GPU MiB rustnv |
|---|---|---|---|---|---|---|---|---|
| H.264 1080p CQP | 214 | 390 | 0.92 | 0.82 | 360 | 148 | 136 | 10 |
| H.264 1080p CBR | 214 | 400 | 0.92 | 0.83 | 364 | 163 | 140 | 11 |
| HEVC 1080p CQP | 203 | 455 | 0.93 | 0.80 | 363 | 146 | 134 | 10 |
| HEVC 1080p CBR | 203 | 462 | 0.95 | 0.80 | 370 | 146 | 134 | 10 |
| AV1 1080p CQP | 214 | 385 | 0.94 | 0.82 | 368 | 157 | 135 | 12 |
| AV1 1080p CBR | 231 | 455 | 0.91 | 0.82 | 370 | 176 | 135 | 12 |
| H.264 4K CQP | 62 | 110 | 0.97 | 0.93 | 550 | 467 | 211 | 32 |
| H.264 4K CBR | 63 | 110 | 0.96 | 0.92 | 548 | 490 | 210 | 32 |
| HEVC 4K CQP | 58 | 127 | 0.98 | 0.94 | 579 | 455 | 202 | 34 |
| HEVC 4K CBR | 59 | 130 | 0.96 | 0.94 | 577 | 466 | 203 | 34 |
| AV1 4K CQP | 61 | 94 | 0.98 | 0.93 | 596 | 426 | 204 | 39 |
| AV1 4K CBR | 70 | 129 | 0.94 | 0.92 | 578 | 526 | 203 | 38 |

转码（硬解到硬编）：

| 用例 | fps C | fps rustnv | CPU s C | CPU s rustnv | RSS MB C | RSS MB rustnv | GPU MiB C | GPU MiB rustnv |
|---|---|---|---|---|---|---|---|---|
| H.264→H.264 1080p | 265 | 331 | 0.96 | 0.93 | 413 | 195 | 335 | 406 |
| H.264→HEVC 1080p | 243 | 337 | 0.99 | 0.93 | 420 | 195 | 333 | 406 |
| H.264→AV1 1080p | 268 | 319 | 0.98 | 0.96 | 420 | 212 | 334 | 408 |
| H.264→H.264 4K | 80 | 94 | 1.05 | 1.00 | 690 | 444 | 852 | 1056 |
| H.264→HEVC 4K | 73 | 101 | 1.07 | 0.99 | 708 | 445 | 831 | 1082 |
| H.264→AV1 4K | 86 | 95 | 1.07 | 1.02 | 717 | 478 | 882 | 1086 |
| HEVC→H.264 1080p | 283 | 353 | 0.89 | 0.88 | 380 | 145 | 345 | 396 |
| HEVC→HEVC 1080p | 258 | 361 | 0.92 | 0.88 | 386 | 146 | 343 | 396 |
| HEVC→AV1 1080p | 280 | 323 | 0.92 | 0.92 | 387 | 159 | 344 | 398 |
| AV1→H.264 1080p | 261 | 370 | 0.81 | 0.73 | 330 | 90 | 306 | 311 |
| AV1→HEVC 1080p | 244 | 405 | 0.83 | 0.70 | 339 | 91 | 306 | 310 |
| AV1→AV1 1080p | 276 | 382 | 0.82 | 0.74 | 338 | 107 | 303 | 313 |

驱动初始化（`vainfo` 整个进程，10 次中位数）：C 版 431 ms，rustnv 2 ms。rustnv 的 `vaInitialize` 不打开 RM，RM 设备在第一个 context / 导出时才打开；C 版在初始化时创建 CUDA context 并探测 NVENC 能力。

吞吐在所有解码、编码和转码项上不低于 C 版。唯一高于 C 版 10% 以上的是转码的显存：解码 surface 用作编码输入时，rustnv 除解码器自己的参考池外，还为每个 surface 建立固定的导出 backing（GPU copy engine 每帧更新，编码器读它），一个 surface 占两份；C 版在 CUDA 里直接读解码输出。H.264 / HEVC 转码里 1080p 多约 50–70 MiB，4K 多约 200–250 MiB；AV1 解码的转码几乎持平。这是设计带来的取舍（见 [design.md](design.md)：显示与编码共用固定的导出 backing）。纯解码（导出 backing 在需要时才分配）和纯编码里 rustnv 的显存都更低。

回归脚本见 [testing.md](testing.md)：`tests/env.py` 覆盖默认静默、`NVD_LOG` / `NVD_LOG_VERBOSE`、`NVD_GPU`、别家 GPU 的显示设备回落、`NVD_MAX_INSTANCES`、解码最大尺寸属性、`vaCreateConfig` 忽略多余属性、`vaSyncSurface2`。
