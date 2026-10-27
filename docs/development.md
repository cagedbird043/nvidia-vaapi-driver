# 开发与构建

## 依赖

- Rust（edition 2021）、Clang / libclang（`build.rs` 用 bindgen 读 libva 头）、libva 开发头文件（需有 `va/va_enc_av1.h`，并提供 `vaSyncSurface2`；开发环境为 libva 2.24）。
- 回归脚本另需 C 编译器、FFmpeg、`vainfo`，见 [testing.md](testing.md)。

## 构建

```sh
cargo build --release
```

产物是 `target/release/librustnv_drv_video.so`。要让 libva 按驱动名找到它：

```sh
ln -sfn librustnv_drv_video.so target/release/rustnv_drv_video.so
LIBVA_DRIVERS_PATH="$PWD/target/release" LIBVA_DRIVER_NAME=rustnv vainfo --display drm --device /dev/dri/renderD129
```

安装到系统时重命名为 `/usr/lib/dri/nvidia_drv_video.so`（libva 对 `nvidia-drm` 设备自动加载 `nvidia` 驱动）。

`cargo test --release` 只运行无需 GPU 的单元测试（`src/import.rs`、`src/backend/mpeg4.rs`）；需要 GPU 的验证全部由 `tests/*.py` 完成。

## nvrm 依赖

RM 后端来自 [nvrm](https://github.com/cagedbird043/nvrm)（包名 `nv-bionic-native`，在 `Cargo.toml` 中重命名为 `nvrm`），按 commit 钉选：

```toml
nvrm = { package = "nv-bionic-native", git = "https://github.com/cagedbird043/nvrm", rev = "…", default-features = false }
```

升级 RM：改 `rev`，执行 `cargo update -p nv-bionic-native`，提交 `Cargo.toml` 与 `Cargo.lock`。

### 本地联调 nvrm

在 `.cargo/config.toml`（已被 `.gitignore` 忽略，不提交）里用 `[patch]` 把依赖指向本地 nvrm 目录：

```toml
[patch."https://github.com/cagedbird043/nvrm"]
nv-bionic-native = { path = "/path/to/nvrm" }
```

### Cargo.lock 注意事项

启用 `[patch]` 后，cargo 会去掉 `Cargo.lock` 里 nvrm 条目的 `source` 行，`git status` 里 `Cargo.lock` 显示已修改。**不要提交这个改动**：提交前 `git checkout Cargo.lock`，或临时移走 `.cargo/config.toml` 后重新生成。打包使用 `--locked`，依赖提交进仓库的锁文件。

## 打包

AUR 包 `nvrm-vaapi-driver` 的 PKGBUILD 在单独的仓库维护。打包时 `cargo fetch --locked`、`cargo build --frozen --release`，把 `librustnv_drv_video.so` 安装为 `/usr/lib/dri/nvidia_drv_video.so`。
