# 设计与各格式要点

驱动名 `rustnv`（库文件 `rustnv_drv_video`，安装名 `nvidia_drv_video.so`）。通过共享 Rust RM 后端（[nvrm](https://github.com/cagedbird043/nvrm)）直接对 NVIDIA 开源内核模块发 ioctl，驱动 NVDEC / NVENC，不调用 CUDA、libnvcuvid 或 NVENC SDK。

## 架构

- 每个 context 一个 worker 线程持有 RM 会话；VA 回调把解码和导出拷贝排进队列后返回，`vaSyncSurface`、`vaDeriveImage`、
  `vaGetImage` 不持全局锁等待完成。worker 退出时未完成的工作置为失败，等待方不会卡死。
- surface 可以在解码前导出；导出的显存在 surface 生命周期内固定，每次解码后由 GPU copy engine 更新。显示路径没有 CPU 拷贝，
  有一次 GPU 拷贝。
- 解码 context 销毁后，存活的 surface 仍持有 worker 和画面。
- `src/backend/mod.rs` 管会话、导出和下载；`src/backend/{h264,hevc,vp9,av1,mpeg2,mpeg4,vc1,vp8}.rs` 各自把 VA 参数转换成 RM 的描述符输入。

RM 按 NVIDIA 驱动版本精确匹配 ABI，不认识的版本拒绝运行。Turing 及更新代的 NVDEC 全部启用，各代能力按 NVIDIA 驱动的代际规则
和引擎能力表判断；本驱动实测范围仅为 RTX 4060 Laptop / nvidia-open 615.78.08 / x86_64 glibc。

## 解码

- 逐行，支持非宏块/CTB/超块对齐的显示尺寸。10/12-bit 样本放在 16 位容器高位（P010/P012/Q416，与引擎输出一致）。
- 12-bit 4:2:0 的 DRM 描述符 fourcc 报 P016（与 C 版一致）；4:4:4 的分层描述符为三个 R8 或 R16 平面，合并描述符仅 444P（`YU24`）。
- HEVC 4:4:4/12-bit 经 range extension 描述符（`VAPictureParameterBufferHEVCExtension`）；NVDEC 无 AV1 12-bit、VP9/AV1 4:4:4、4:2:2。
- 解码 surface 作编码输入：8/10-bit 4:2:0 走 copy engine；4:4:4 surface 作编码输入不支持（NVENC 的 4:4:4 输入是交错 chroma，copy engine 一次只能搬一个平面）。
- 解码参数直接来自 VA-API，转换为 NVDEC 描述符；不重建参数集码流。
- 输出：NV12 CPU 下载；DRM PRIME2 的 R8 + RG88 分层或 NV12 合并描述符（块线性修饰符）。
- 解码所用的 GPU 与 C 版一致地选择：libva 打开的 DRM 设备是 NVIDIA 时用它，否则（别家 GPU 的节点或无 DRM fd）用第一块 NVIDIA 渲染节点；`NVD_GPU` 指定序号。

解码明确拒绝并返回错误的情况：4:2:2、VP9/AV1 4:4:4、AV1 12-bit、H.264 隔行/场编码、H.264 FMO、HEVC PCM 与分离色彩平面、
MPEG-4 的 sprite（GMC）、H.263 短头、data partitioning 与 reversible VLC（NVDEC 的 MPEG-4 描述符没有这些字段，stock 解 data partitioning 的结果也是错的）、
VP9 在分段图写入后改变编码尺寸、AV1 film grain、large-scale tile 与帧间 superres。

VP9 Profile 2、AV1 的 surface 位深由 `vaCreateSurfaces` 的 RTFormat 决定，context 在第一帧按目标 surface 重建解码器。Chrome 建 AV1 / VP9 Profile 2 的 config 时 RTFormat 只给 YUV420，随后才创建 10-bit surface；驱动与 C 版一样把 RTFormat 当提示，按 surface 格式解码。

### 旧格式（MPEG-2 / MPEG-4 / VC-1 / VP8）

这四种格式由 NVDEC 引擎自己解析宏块层，VA 参数只填序列/图像层语法：nvrm 的 `video/legacy.rs` 提供通用的 `LegacyCodec<F: Format>`
（VA surface 与会话 surface 的对应、前向/后向参考、场对），各格式只实现 `Format::build`（VA 参数 → NVDEC 描述符和方法序列）。
描述符布局、方法序列和侧缓冲规则取自 stock libnvcuvid 615.78.08 的 RM 追踪（nvrm 仓库 `tools/nvdec-trace`），不经 CUDA。关键事实：

- **MPEG-2**：引擎对每幅图像写两幅叠放的场图像（下场在上场之后）；copy engine 导出时按行交织成帧。场图像的第二场写进第一场所在的 surface。
  切片起点表放在描述符 +0x200，码流是带起始码的切片。
- **MPEG-4**：引擎自己解析 VOP 头，码流必须从 `00 00 01 b6` 开始。VA 客户端（libavcodec）只交出 VOP 头之后的宏块数据（`macroblock_offset` 是首字节内的位偏移），
  所以驱动按 VA 参数重建 VOP 头（类型、时间戳、取整、`intra_dc_vlc_thr`、扫描位、`vop_quant`、f_code），再把宏块数据按位移接上。
  VA 不带 VOP 时间戳，驱动用 `TRD`/`TRB` 推出一条时钟写进头里；实测 NVDEC 解带 `modulo_time_base` 的 I-VOP 会出错，所以 I-VOP 按 GOP 头之后的写法重新从 0 秒起算（与 ffmpeg 编码器的码流一致），P/B-VOP 照常带秒计数。
  描述符 +0x5c 起是 VOL 语法（时间增量位数、resync、尺寸）、量化矩阵（光栅序）和 B-VOP 的 `trd/trb`；`error_status` 的第 3 位在正确的 B-VOP 上也会出现（stock 相同），不当作错误。
- **VC-1 / WMV3**：引擎自己解析图像层（包括 bitplane），描述符只有序列和 entry point 语法；Simple/Main 没有起始码，引擎要求描述符里给帧起始码；
  Advanced 的每个切片前补 `00 00 01 0d/0c/0b`。输出同样是两幅叠放的场图像。
- **VP8**：引擎解析整个帧（头、概率、分区），描述符只有尺寸（按宏块对齐）、帧类型和第一分区长度；会话开始时需要把一张初始表（`vp8_init.bin`：RFC 6386 的默认系数、模式与运动矢量概率，按引擎布局排列，与 stock 写入 copy-engine 源缓冲的内容相同）写进侧缓冲。
  Chrome 整帧提交：`slice_data_offset` 越过帧头，`slice_data_size` 却是整帧大小；越界时按“从缓冲起始计”的大小读取。

### JPEG 不支持

C 版的 JPEG Baseline 经 libnvcuvid 的 JPEG 路径解码；nvrm 追踪显示 stock 的 JPEG 用内嵌的 CUDA 内核（`jpegparse`、`jpegdec_vld`、`jpegdec_idct`）完成，通道上只有计算与拷贝，没有 NVDEC / NVJPG 的方法。rustnv 不使用 CUDA，因此不提供 JPEG（`VAProfileJPEGBaseline`），这是与 C 版唯一的能力差距。

## 编码

- CQP、CBR、VBR；单个 L0 参考、I/P 帧；GOP / IDR 周期、强制关键帧、初始 QP、min/max QP。
- 最大尺寸 8192×8192；属性和 profile 对照系统 C 版，包含 HEVC features / block sizes 与 AV1 扩展属性。
- `vaCreateImage` + `vaPutImage`、`vaDeriveImage` + map / unmap 支持 CPU 上传。直接映射的线性缓冲在 surface 间复用，上传按 RM 的 NVENC tiled layout 写显存。
- 8-bit 444P 直接交给 RM 做 tiled chroma interleave。Q416 使用每个 context 复用的 chroma staging 缓冲，将两个 16-bit 平面交错后上传；luma 不做 staging 拷贝。
- H.264 / HEVC 忽略应用 packed SPS/PPS/slice，使用 RM 生成的参数集，在每个 IDR 前重复。AV1 接受 packed sequence 参数，RM 输出 in-band sequence OBU。
- coded buffer map 返回 `VACodedBufferSegment`（单段链、`VA_CODED_BUF_STATUS_SINGLE_NALU`）。提交当前为同步编码，因此 surface / buffer 同步查询在提交返回后已完成。
- GPU 从 libva 的 DRM fd 选择，与解码一致。解码 surface（8/10-bit 4:2:0、444P / Q416）作编码输入不经 CPU：surface 的导出 backing（8 行 GOB、单 GOB 块高的块线性）由 GPU copy engine 重排进 NVENC 输入槽，字节与 CPU tiler 一致。4:4:4 的 Cb、Cr 两个平面各用一次 remap 拷贝写进交错 chroma 的偶数 / 奇数分量，另一分量设为 NO_WRITE 保留原值（RM 实机测试 `chroma_444_interleave` 覆盖 1 / 2 字节样本、线性与块线性源、跨 GOB 尺寸与带 padding 的 pitch，结果与 CPU tiler 逐字节一致）。外部 DMA-BUF 的 NV12 / P010 / 444P / Q416 同走 copy engine。BGRA（ARGB / XRGB）由 copy engine 拷进显存线性 scratch，RM 的 compute 内核 `bgra_nv12` 按 BT.709 video range 转成线性 NV12（与 CPU 转换同一套定点系数与取整，逐字节一致），再由 copy engine 写进输入槽；只在已实测的 GPU（Ada）上启用，其余 GPU 回退到系统内存 staging + CPU 转换。
- RM 的帧率接口为整数，分数帧率四舍五入。RateControl / FrameRate misc 参数被消费；其余 misc（包含 HRD / QualityLevel）与当前 C 后端一样接受但不消费。

FFmpeg 经 `hwupload` 协商不了 H.264 444 和 HEVC Q416，这两种输入由回归脚本里的直接 VA 客户端测试，并检查输出色度/位深。

## 视频处理

`VAProfileNone` + `VAEntrypointVideoProc`，由 `src/vpp.rs` 的 worker 线程（`rustnv-vpp`）持有 RM 的 `video::vpp::Processor`。只在 compute 内核实测过的 GPU（Ada，compute class `0xc9c0`）上报告该入口。

- 格式：NV12、P010、P012、P016、444P、Q416 与 RGB32（BGRA / BGRX / ARGB / XRGB / RGBA / RGBX）之间任意组合；RGB32 surface 可直接创建，也可作 `vaGetImage` / `vaDeriveImage` 的来源。
- 管线：copy engine 把源平面（块线性或线性）拷进显存线性 scratch，compute 内核 `vpp` 做双线性缩放、`surface_region` 裁剪、色彩矩阵与位深转换，`output_region` 以外按 `output_background_color` 填充（RGB 输出带 alpha），再由 copy engine 写回目标 surface。同格式、同尺寸、无裁剪的请求只做 copy engine 拷贝，逐字节一致。
- `VA_FILTER_SCALING_*` 一律按双线性；`vaQueryVideoProcFilters` 返回空集，滤镜参数缓冲与 `num_filters` 非零都返回 `VA_STATUS_ERROR_UNSUPPORTED_FILTER`。不支持旋转、镜像、混合、额外输出与参考帧。
- 色彩分三步解析：
  1. 固定标准按 libva 隐含的 primaries / transfer / matrix，`VAProcColorStandardExplicit` 才读 `VAProcColorProperties` 的三个字段。
  2. 未指定的矩阵：RGB 侧为 identity；YUV 输出继承 YUV 输入的矩阵，输入为 RGB 时按输出高度选 BT.709（≥ 720 行）或 BT.601；YUV 输入按输入高度选。YUV 侧把 identity 矩阵也当未指定（FFmpeg 从 RGB 元数据复制它）。未指定的 range：YUV 为 limited，RGB 为 full。primaries / transfer 一端未指定则视为与另一端相同。
  3. 只有矩阵或 range 不同时做转换。BT.709、BT.601（525 / 625）、BT.470M、SMPTE 240M 与 generic film 的原色视为同一色域，相互之间只换矩阵（与其他 VA 驱动和 GStreamer `videoconvert` 一致，GStreamer 按画面高度协商 BT.601 / BT.709）；跨色域（如 BT.2020）或 transfer 不同（如 PQ → SDR）返回 `VA_STATUS_ERROR_UNIMPLEMENTED`。
- 色度位置按 `chroma_sample_location`：水平左对齐或居中，垂直顶、中、底。
- 完成状态挂在目标 surface 上：编码、导出、`vaSyncSurface`、读回都会先等它。

## 零拷贝与 DMA-BUF

| 输入来源 | 到 NVENC 的路径 |
|---|---|
| 解码 surface | 导出 backing → GPU copy engine（块线性 → NVENC 输入 tile）→ 输入槽 |
| DRM_PRIME_2 / DRM_PRIME（NV12、P010/P012/P016、444P、Q416） | 导入 dma-buf → copy engine → 输入槽（4:4:4 色度两次 remap 交错） |
| DRM_PRIME_2 / DRM_PRIME（ARGB、XRGB） | 导入 → copy engine → 显存 scratch → compute 内核 BT.709 → copy engine → 输入槽（未实测的 GPU：系统内存 staging + CPU 转换） |
| `vaPutImage` / `vaDeriveImage` | CPU 写显存 |

- `vaCreateSurfaces` 接受 `VASurfaceAttribMemoryType` = VA | DRM_PRIME | DRM_PRIME_2 与 `VASurfaceAttribExternalBufferDescriptor`（每次一个 surface）。旧版 DRM_PRIME 不带修饰符，与 C 版一样只在设置 `NVD_DRM_PRIME_ASSUME_LINEAR` 时按线性接受；`vaQuerySurfaceAttributes` 也只在此时报告它，平时报告 VA | DRM_PRIME_2 和外部缓冲描述符。
- 修饰符：线性，以及 NVIDIA 块线性（无压缩、Turing+ GOB、块高 ≤ 32 行，即 `0x0300000000606010`–`15` 一族）。其余修饰符返回 `UNSUPPORTED_MEMORY`。
- 导入的 surface 不能做解码目标，也不能 `vaPutImage`；可再经 `vaExportSurfaceHandle` 导出。`vaDestroySurfaces` 释放 import 与映射，1000 次 import/export/destroy 循环 RSS 与 fd 数不增长。
- 解码 surface 经 `vaExportSurfaceHandle` 导出时，拷贝入共享 backing 的操作在导出时排队，消费方需在导出后 `vaSyncSurface` 再读。

### Chrome 与 DRM PRIME

Chrome 156 中该类型只影响这一处。Chrome 只对认识的驱动用 DRM_PRIME_2 导入编码输入，其余用不带修饰符的旧版 DRM_PRIME，rustnv 无法确认其布局而拒绝，Chrome 也不会退回 `vaPutImage`。与 C 版相同，rustnv 在 Chrome GPU 进程（`--type=gpu-process`）里把 vendor 字符串加上 `Chromium fake libva driver compatible:` 前缀，使 Chrome 改走 DRM_PRIME_2。

### Chrome 与 GPU 沙箱

默认沙箱下，Chrome 在启用沙箱前初始化、查询并 `vaTerminate` 了驱动；解码时在 seccomp 沙箱内重新打开 VA display，
访问 `/sys/dev/char/<major>:<minor>/device/drm` 被 Chrome 的 broker 拒绝，报 `Could not get a valid VA display`，
驱动的初始化函数不会再被调用。这是 Chrome 对 NVIDIA 的 VA-API 沙箱策略不完整，驱动本身无法绕过。
C 版 nvidia-vaapi-driver 看似能在默认设置下工作，是因为它加载的 libcuda 在沙箱启用前就创建了线程，Chrome 报
`InitializeSandbox() called with multiple threads in process gpu-process` 后 GPU 进程没有 seccomp（`/proc/<pid>/status` 中
`Seccomp: 0`），效果等同于 `--disable-gpu-sandbox`。

## 已知限制

- 不支持 JPEG（见上）。VideoProc 没有滤镜，跨色域与 HDR → SDR 不做；Turing / Ampere / Hopper / Blackwell 的 `vpp` 内核随 RM 生成，但未在硬件上运行，这些 GPU 不报告 VideoProc。
- BGRA 的 GPU 转换只在实测过的 Ada（compute class `0xc9c0`）上启用；其他架构的内核随 RM 生成、QMD 打包有单测，但不在硬件上运行，回退到 CPU 转换。
- 同步在 CPU 侧：解码完成与 copy engine 完成都由 CPU 等待，未做 GPU 侧 fence 串联。
- 不支持带压缩（c≠0）的块线性修饰符和 Turing 之前的 GOB 排布。
- 编码为同步提交，单个 L0 参考、I/P 帧。
- Chrome 需要 `--disable-gpu-sandbox` 与 `--render-node-override`。
- 只在 RTX 4060 Laptop（Ada）/ nvidia-open 615.78.08 / x86_64 glibc 实测；RM 按 NVIDIA 驱动版本精确匹配 ABI。
