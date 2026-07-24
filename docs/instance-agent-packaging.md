# 实例端 standalone 打包指南

本文说明如何为 Linux、Windows 和 macOS 打包实例端 `om-agent`。项目只发布 standalone 可执行文件，不生成 DEB、RPM、IPK、MSI 或 PKG 安装包。

## 1. 目标与产物

实例端源码位于 `instanceEnd/`，Cargo 二进制名称固定为 `om-agent`。构建脚本会把 release 产物复制到：

```text
instanceEnd/dist/standalone/
```

每个可执行文件都会同时生成同名 SHA-256 校验文件。发布或上传更新时，两者必须一起提供。

| 操作系统 | CPU/运行环境 | Rust target | 平台架构标识 | 产物后缀 |
| --- | --- | --- | --- | --- |
| Linux | x86_64 glibc 2.17+ | `x86_64-unknown-linux-gnu` | `x86_64` | `.bin` |
| Linux/OpenWrt | x86_64 musl | `x86_64-unknown-linux-musl` | `x86_64-musl` | `.bin` |
| Linux | ARM64 musl | `aarch64-unknown-linux-musl` | `aarch64` | `.bin` |
| Linux | ARMv7 glibc 2.17+ | `armv7-unknown-linux-gnueabihf` | `arm` | `.bin` |
| Linux | x86 glibc 2.17+ | `i686-unknown-linux-gnu` | `x86` | `.bin` |
| Windows | x64 | `x86_64-pc-windows-msvc` | `x64` | `.exe` |
| Windows | ARM64 | `aarch64-pc-windows-msvc` | `arm64` | `.exe` |
| Windows | x86 | `i686-pc-windows-msvc` | `x86` | `.exe` |
| macOS | Apple Silicon | `aarch64-apple-darwin` | `arm64` | `.bin` |
| macOS | Intel | `x86_64-apple-darwin` | `x86_64` | `.bin` |

产物名称包含 `instanceEnd/Cargo.toml` 中的版本号，例如：

```text
om-agent_0.1.5_linux_x86_64.bin
om-agent_0.1.5_linux_x86_64.bin.sha256
om-agent_0.1.5_windows_x64.exe
om-agent_0.1.5_windows_x64.exe.sha256
om-agent_0.1.5_macos_arm64.bin
om-agent_0.1.5_macos_arm64.bin.sha256
```

## 2. 打包前准备

### 2.1 更新版本号

修改 `instanceEnd/Cargo.toml`：

```toml
[package]
version = "0.1.5"
```

版本变更后更新并检查锁文件：

```bash
cd instanceEnd
cargo check
```

提交代码时应同时包含 `Cargo.toml` 和 `Cargo.lock` 的对应修改。

### 2.2 安装 Rust

Linux/macOS 推荐使用 rustup：

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup default stable
rustc --version
cargo --version
```

Windows 可从 <https://rustup.rs/> 下载 `rustup-init.exe`，安装完成后重新打开 PowerShell：

```powershell
rustup default stable
rustc --version
cargo --version
```

项目当前要求的最低 Rust 版本以 `instanceEnd/Cargo.toml` 的 `rust-version` 为准。

### 2.3 安装全部 Rust targets

在 Linux 或 macOS 的 Bash 中执行：

```bash
rustup target add \
  x86_64-unknown-linux-gnu \
  x86_64-unknown-linux-musl \
  aarch64-unknown-linux-musl \
  armv7-unknown-linux-gnueabihf \
  i686-unknown-linux-gnu \
  x86_64-pc-windows-msvc \
  aarch64-pc-windows-msvc \
  i686-pc-windows-msvc \
  aarch64-apple-darwin \
  x86_64-apple-darwin
```

安装 target 只会提供 Rust 标准库。交叉编译含 C、汇编或系统库的依赖时，还需要后文所述的 Zig、cargo-zigbuild、cargo-xwin 或原生系统工具链。

## 3. 在 macOS 上打包

macOS 是当前最方便的一站式打包主机：可以原生构建 macOS，并通过 Zig/cargo-zigbuild 构建 Linux，通过 cargo-xwin 构建 Windows。

### 3.1 安装依赖

先安装 Xcode Command Line Tools：

```bash
xcode-select --install
```

使用 Homebrew 安装 Zig：

```bash
brew install zig
```

安装 Cargo 交叉编译工具：

```bash
cargo install cargo-zigbuild
cargo install --locked cargo-xwin
```

确认工具可用：

```bash
clang --version
zig version
cargo zigbuild --version
cargo xwin --version
```

当系统没有 `llvm-lib` 时，项目内置的 `instanceEnd/scripts/xwin-tools/llvm-lib` 会自动使用 `zig ar` 生成 Windows COFF 静态库，无需额外安装整套 LLVM。

### 3.2 构建全部平台

从项目根目录执行：

```bash
cd instanceEnd
./scripts/build-standalone.sh all
```

脚本会依次尝试全部 10 个目标。单个目标失败后会继续构建其他目标，最后统一列出失败项；只要存在失败，脚本退出状态就是非零。

正常结束时会显示：

```text
All 10 supported platform builds succeeded.
```

### 3.3 只构建 macOS

Apple Silicon：

```bash
cd instanceEnd
./scripts/build-standalone.sh aarch64-apple-darwin macos arm64
```

Intel：

```bash
cd instanceEnd
./scripts/build-standalone.sh x86_64-apple-darwin macos x86_64
```

Apple Silicon Mac 可以通过 Apple 提供的工具链交叉生成 Intel macOS 二进制。Intel Mac 同理可构建 Apple Silicon target，但应在对应真实硬件或 CI runner 上进行最终运行验证。

### 3.4 只构建 Windows

```bash
cd instanceEnd
./scripts/build-standalone.sh x86_64-pc-windows-msvc windows x64
./scripts/build-standalone.sh aarch64-pc-windows-msvc windows arm64
./scripts/build-standalone.sh i686-pc-windows-msvc windows x86
```

跨平台 Windows MSVC 目标会自动选择 `cargo-xwin`。首次执行可能下载并缓存 Windows SDK/MSVC sysroot，因此耗时较长并占用数 GB 磁盘空间。

### 3.5 只构建 Linux

```bash
cd instanceEnd
./scripts/build-standalone.sh x86_64-unknown-linux-gnu linux x86_64
./scripts/build-standalone.sh x86_64-unknown-linux-musl linux x86_64-musl
./scripts/build-standalone.sh aarch64-unknown-linux-musl linux aarch64
./scripts/build-standalone.sh armv7-unknown-linux-gnueabihf linux arm
./scripts/build-standalone.sh i686-unknown-linux-gnu linux x86
```

GNU/Linux target 会自动使用 `cargo zigbuild` 并追加 glibc 2.17 最低版本约束；其他非本机 Linux target 会在检测到 Zig 和 cargo-zigbuild 后自动使用 `cargo zigbuild`。

## 4. 在 Linux 上打包

### 4.1 安装基础依赖

Debian/Ubuntu 示例：

```bash
sudo apt update
sudo apt install -y build-essential clang curl pkg-config
```

Fedora/RHEL 示例：

```bash
sudo dnf install -y gcc gcc-c++ clang curl pkgconf-pkg-config
```

然后安装 Rust，并根据所需目标执行 `rustup target add`。

### 4.2 构建 Linux x86_64 glibc

```bash
cd instanceEnd
./scripts/build-standalone.sh x86_64-unknown-linux-gnu linux x86_64
```

该目标始终以 glibc 2.17 为最低兼容基线，默认使用 cargo-zigbuild，即使构建机本身也是 x86_64 glibc Linux。这样生成的文件可以用于 CentOS 7，并避免发布产物意外依赖构建机上的较新 glibc。构建前必须按下一节安装 Zig 和 cargo-zigbuild。

### 4.3 构建 musl 和其他 Linux 架构

推荐安装 Zig 与 cargo-zigbuild。Zig 的安装方式因发行版而异，也可以从 <https://ziglang.org/download/> 下载官方版本。

```bash
cargo install cargo-zigbuild
zig version
cargo zigbuild --version
```

随后执行目标对应命令：

```bash
cd instanceEnd
./scripts/build-standalone.sh x86_64-unknown-linux-musl linux x86_64-musl
./scripts/build-standalone.sh aarch64-unknown-linux-musl linux aarch64
./scripts/build-standalone.sh armv7-unknown-linux-gnueabihf linux arm
./scripts/build-standalone.sh i686-unknown-linux-gnu linux x86
```

### 4.4 OpenWrt 注意事项

目标矩阵中的 `x86_64-unknown-linux-musl` 适合常见 x86_64 musl/OpenWrt 环境，但仍应在目标设备上验证内核、libc 和 CPU 兼容性。

MIPS、MIPSel 或其他未列入脚本矩阵的 OpenWrt 平台不能仅靠修改 Rust target 完成。必须使用与固件版本、CPU、浮点 ABI 和 libc 完全匹配的 OpenWrt SDK，并配置对应编译器和链接器。使用 SDK 自带链接器时，可以关闭自动 Zig 选择：

```bash
cd instanceEnd
OM_STANDALONE_BUILDER=cargo ./scripts/build-standalone.sh <rust-target> linux <native-architecture>
```

只有脚本目标矩阵中已经登记的 target 才能直接通过现有脚本构建；新增协议架构标识时，还应同步修改后端和前端的更新匹配逻辑。

### 4.5 从 Linux 构建 Windows

安装 Clang、Zig 和 cargo-xwin：

```bash
cargo install --locked cargo-xwin
clang --version
zig version
cargo xwin --version
```

然后执行：

```bash
cd instanceEnd
./scripts/build-standalone.sh x86_64-pc-windows-msvc windows x64
./scripts/build-standalone.sh aarch64-pc-windows-msvc windows arm64
./scripts/build-standalone.sh i686-pc-windows-msvc windows x86
```

Linux 无法通过普通 Cargo/MSVC target 直接链接 Windows 程序，必须使用 cargo-xwin 或完整的兼容 MSVC 工具链。

### 4.6 从 Linux 构建 macOS

不建议在普通 Linux 主机上直接构建正式 macOS 产物。macOS SDK 和 Apple 工具链受许可限制，正式产物应使用真实 Mac 或 macOS CI runner 构建、签名和公证。

## 5. 在 Windows 上打包

Windows 原生构建推荐使用 `instanceEnd/scripts/build-standalone.cmd`，它会调用 PowerShell 脚本，并规避常见的 PowerShell 执行策略问题。

### 5.1 安装 Visual Studio Build Tools

安装 Visual Studio 2022 Build Tools，并勾选：

- Desktop development with C++（使用 C++ 的桌面开发）
- MSVC v143 x64/x86 build tools
- Windows 10 或 Windows 11 SDK
- 如需原生构建 Windows ARM64，再安装 MSVC ARM64 build tools

安装完成后建议使用“Developer PowerShell for VS 2022”，或者重新打开普通 PowerShell，确认：

```powershell
rustc --version
cargo --version
```

### 5.2 一次构建三个 Windows 架构

从项目根目录执行：

```powershell
cd instanceEnd
.\scripts\build-standalone.cmd
```

无参数时会依次构建 Windows x64、x86 和 ARM64，并自动安装缺失的 Windows Rust targets。

### 5.3 只构建一个 Windows 架构

x64：

```powershell
cd instanceEnd
.\scripts\build-standalone.cmd x86_64-pc-windows-msvc
```

x86：

```powershell
cd instanceEnd
.\scripts\build-standalone.cmd i686-pc-windows-msvc
```

ARM64：

```powershell
cd instanceEnd
.\scripts\build-standalone.cmd aarch64-pc-windows-msvc
```

### 5.4 Windows ARM64 使用 cargo-xwin

如果本机没有安装 MSVC ARM64 工具链，可以安装 LLVM 和 cargo-xwin：

```powershell
cargo install --locked cargo-xwin
clang --version
lld-link --version
cargo xwin --version
```

构建脚本检测到 cargo-xwin 后，会优先将 Windows ARM64 目标交给它。首次构建会下载 Windows SDK/MSVC sysroot。

### 5.5 直接执行 PowerShell 脚本

一般应优先使用 `.cmd`。如需直接运行 `.ps1`：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\build-standalone.ps1
```

该执行策略只作用于本次 PowerShell 子进程，不会永久修改系统策略。

### 5.6 从 Windows 构建全部平台

可以执行：

```powershell
cd instanceEnd
.\scripts\build-standalone.cmd all
```

但 Windows 主机还需要为 Linux 交叉目标安装 Zig 和 cargo-zigbuild；正式 macOS 产物仍建议在 Mac 上构建。因此，生产环境更推荐分别使用 Windows、Linux、macOS runner 构建各自的原生目标，或者统一在配置完整的 macOS 构建机上执行 Bash `all`。

## 6. 构建器选择

Bash 和 PowerShell 脚本都支持 `OM_STANDALONE_BUILDER`：

| 值 | 行为 |
| --- | --- |
| `auto` | 默认。GNU/Linux 目标固定使用 cargo-zigbuild 和 glibc 2.17 基线，其他 Linux 交叉目标优先使用 cargo-zigbuild，Windows MSVC 交叉目标优先使用 cargo-xwin |
| `cargo` | 强制使用普通 `cargo build`，适合原生工具链或自行配置好链接器的环境；GNU/Linux 目标不允许使用此模式 |
| `zigbuild` | 强制使用 `cargo zigbuild` |
| `xwin` | 强制使用 `cargo xwin build` |

Linux/macOS Bash 示例：

```bash
OM_STANDALONE_BUILDER=xwin ./scripts/build-standalone.sh x86_64-pc-windows-msvc windows x64
OM_STANDALONE_BUILDER=zigbuild ./scripts/build-standalone.sh aarch64-unknown-linux-musl linux aarch64
```

Windows PowerShell 示例：

```powershell
$env:OM_STANDALONE_BUILDER = 'xwin'
.\scripts\build-standalone.cmd aarch64-pc-windows-msvc
Remove-Item Env:OM_STANDALONE_BUILDER
```

## 7. 校验产物

### 7.1 Linux

```bash
cd instanceEnd/dist/standalone
sha256sum -c om-agent_0.1.5_linux_x86_64.bin.sha256
file om-agent_0.1.5_linux_x86_64.bin
readelf --version-info om-agent_0.1.5_linux_x86_64.bin \
  | grep -o 'GLIBC_[0-9.]*' \
  | sort -V \
  | tail -n 1
```

GNU/Linux 产物的最后一条命令应输出 `GLIBC_2.17` 或更低版本。Bash 打包脚本也会在复制产物前执行等价检查，超过该基线时构建失败。

### 7.2 macOS

```bash
cd instanceEnd/dist/standalone
shasum -a 256 -c om-agent_0.1.5_macos_arm64.bin.sha256
file om-agent_0.1.5_macos_arm64.bin
```

### 7.3 Windows

```powershell
cd instanceEnd\dist\standalone
Get-FileHash .\om-agent_0.1.5_windows_x64.exe -Algorithm SHA256
Get-Content .\om-agent_0.1.5_windows_x64.exe.sha256
```

应确认计算结果与 `.sha256` 文件中的摘要完全相同。

交叉编译成功只说明文件已生成。发布前还应在对应操作系统和 CPU 上至少验证：

```text
om-agent --help
om-agent --version
```

并进行一次连接后端、指标上报、命令执行、终端和更新流程的基本检查。

## 8. 发布前检查清单

1. `instanceEnd/Cargo.toml` 版本号正确，`Cargo.lock` 已同步。
2. 在 `instanceEnd/` 执行 `cargo fmt --check`、`cargo test` 和 `cargo check`。
3. 所有需要发布的平台构建成功。
4. 每个可执行文件都有同名 `.sha256` 文件。
5. SHA-256 校验通过。
6. 在真实目标平台完成最基本的启动与连接验证。
7. Windows `.exe` 使用 Authenticode 签名。
8. macOS 二进制使用 Developer ID 签名并完成公证。
9. 上传更新时，操作系统、架构标识、版本号和 standalone 类型填写正确。
10. 不提交 `dist/` 产物、密码、`.env`、数据库、日志或实例身份文件。

SHA-256 只能验证文件完整性，不能替代 Windows 或 macOS 的平台代码签名。

## 9. 常见问题

### `assert.h file not found` 或 Windows SDK 头文件缺失

通常是从 macOS/Linux 对 MSVC target 执行了普通 `cargo build`。安装 cargo-xwin、Clang 和 Zig，然后保留默认 `OM_STANDALONE_BUILDER=auto`：

```bash
cargo install --locked cargo-xwin
./scripts/build-standalone.sh x86_64-pc-windows-msvc windows x64
```

### `failed to find tool llvm-lib`

当前 Bash 脚本在找不到系统 `llvm-lib` 时会自动把项目内置包装器加入 PATH，并使用 `zig ar`。确认 Zig 可用：

```bash
zig version
```

### `cargo-zigbuild is required`

```bash
cargo install cargo-zigbuild
```

同时确认 Zig 已安装并在 PATH 中。

### `rust target ... is not installed`

```bash
rustup target add <rust-target>
```

例如：

```bash
rustup target add aarch64-pc-windows-msvc
```

### Windows 提示找不到 `link.exe`

安装 Visual Studio Build Tools 的 C++ 工作负载和 Windows SDK，并从 Developer PowerShell 重新执行；或者安装 LLVM/cargo-xwin 后强制使用 xwin。

### macOS 产物在另一台 Mac 上被系统阻止

正式分发前需要 Developer ID 签名和 Apple 公证。临时本地测试可以检查隔离属性和签名状态，但不要把绕过 Gatekeeper 当作生产发布方案。

### 构建成功但目标设备无法运行

重点检查 CPU 架构、32/64 位、glibc/musl、ARM 浮点 ABI、最低系统版本以及 OpenWrt 固件 ABI。`linux/x86_64` 与 `linux/x86_64-musl` 是不同更新目标，不能混用。

### 清理构建缓存

仅在确认不需要增量缓存时执行：

```bash
cd instanceEnd
cargo clean
```

这会删除所有 target 的编译缓存，下一次完整构建会明显变慢，但不会删除 `dist/standalone/` 中已经复制出的发布文件。
