# Operation Monitoring

一个自托管的远程资源监控系统 MVP，用于小规模服务器、电脑实例的资源上报、审批管理、快捷命令和 Web 终端操作。

## 项目结构

```text
operationMonitoring/
  front-end/      Vue 3 + Vite 前端控制台
  backend/        Rust + Axum + SQLx 后端服务
  instanceEnd/    Rust 实例端 Agent
  需求.md          产品需求草案
  执行计划.md      MVP 执行计划
```

## 前端分层

```text
front-end/src/
  api/            HTTP 请求封装
  components/     页面组件：顶部栏、实例看板、管理面板、弹窗
  composables/    控制台状态与业务动作
  styles/         基础、布局、控件、看板、管理面板、弹窗、响应式样式
  types/          前端领域类型
  utils/          格式化与指标计算工具
```

`App.vue` 只负责装配页面，接口调用、状态管理和 UI 组件已经拆分，后续加历史图表、告警、设置页时可以直接在对应目录扩展。

## 后端分层

```text
backend/src/
  auth.rs         TOTP、密钥加密和管理员 session 校验
  admin_auth.rs   管理员初始化、登录、用户与认证设备 API
  config.rs       启动参数和环境变量
  db.rs           SQLite 连接、建表、查询辅助、清理任务
  error.rs        统一错误响应
  handlers/       HTTP API handler
  jobs.rs         命令任务创建、下发、完成
  models.rs       请求、响应、数据库行模型
  state.rs        全局共享状态
  utils.rs        通用工具
  ws.rs           Agent WebSocket 和 Web 终端
```

## 实例端分层

```text
instanceEnd/src/
  command.rs      系统命令执行与超时截断
  config.rs       Agent 启动参数
  http.rs         审批前注册请求
  identity.rs     本地实例身份生成和读取
  lifecycle.rs    实例进程启动、停止与状态管理
  metrics.rs      CPU、内存、磁盘、网络采集
  models.rs       Agent 与后端通信模型
  profile.rs      主机基础信息
  terminal.rs     跨平台 PTY/ConPTY 交互式 Shell
  time.rs         时间戳工具
  ws.rs           Agent WebSocket、指标上报、命令与终端复用通道
```

## 本地启动

启动后端：

```bash
cd backend
OM_DATABASE_PASSWORD='<数据库密码>' OM_ADMIN_PASSWORD=admin123 cargo run
```

启动前端：

```bash
cd front-end
pnpm install
pnpm dev
```

构建并在后台启动实例端：

```bash
cd instanceEnd
cargo build --release
./target/release/om-agent start --server http://127.0.0.1:13500
```

`start` 会在后台启动实例端并立即释放命令行，标准输出和错误输出会写入命令返回的日志路径。Windows 使用同目录下的 `om-agent.exe`，后台子进程不会创建控制台窗口。

查询状态或停止实例端：

```bash
./target/release/om-agent status
./target/release/om-agent stop
```

需要在前台运行实例端并直接向终端打印日志时，使用 `log`：

```bash
./target/release/om-agent log --server http://127.0.0.1:13500
```

实例端不提供 `run` 命令。`log` 会持续运行，按 `Ctrl+C` 退出；同一状态目录下不能同时运行前台和后台实例端。未配置 `OM_AGENT_LOG_FILE` 时日志直接打印到终端，配置后则写入对应的滚动日志文件。

开发时也可以通过 Cargo 执行相同命令：

```bash
cargo run -- start --server http://127.0.0.1:13500
cargo run -- status
cargo run -- log
cargo run -- stop
```

## Docker Compose 部署

安装 Docker 和 Docker Compose 后，在项目根目录构建并启动前端与后端：

```bash
docker compose up -d --build
```

默认可通过以下地址访问：

- 前端控制台：`http://localhost`
- 后端 API：`http://localhost:13500`
- 健康检查：`http://localhost:13500/api/health`

默认密码 `admin123` 仅用于第一次启动时创建首位管理员。首次登录后必须填写
用户名、使用手机 Authenticator 扫描二维码并输入 6 位代码；确认成功后密码入口会
永久关闭，之后统一使用“用户名 + Authenticator 代码”登录。兼容 Google
Authenticator、Microsoft Authenticator、1Password 等标准 TOTP 应用。

生产环境必须在首次初始化前通过 `OM_ADMIN_PASSWORD` 设置强密码；也可以覆盖前后端端口：

```bash
OM_ADMIN_PASSWORD='replace-with-a-strong-password' \
OM_DATABASE_PASSWORD='<数据库密码>' \
FRONTEND_PORT=8080 \
BACKEND_PORT=13500 \
docker compose up -d --build
```

Agent 可以连接前端代理地址（例如 `http://服务器地址:8080`）或后端直连地址
（例如 `http://服务器地址:13500`）。前端代理支持 API、上传资源和 WebSocket。

业务数据保存在外部 PostgreSQL 中。管理员认证密钥、背景图片和 Agent 更新包分别保存在
`backend-db`、`backend-uploads` 和 `backend-updates` 命名卷中，重新创建容器不会删除这些文件。
删除命名卷前请先备份。常用管理命令：

```bash
docker compose ps
docker compose logs -f
docker compose down
```

后台“用户管理”页面可以现场添加管理员和多台 Authenticator。创建、停用、删除、
撤销设备等敏感操作都需要当前管理员再次输入自己的 6 位代码。二维码只在创建页面
临时显示，刷新后不能恢复；未确认的注册可取消后重新生成。

TOTP 密钥使用 AES-256-GCM 加密。默认加密主密钥保存在 SQLite 同一命名卷内的
`/app/db/auth-secret.key`，备份数据库时必须同时备份该文件；丢失后现有
Authenticator 无法恢复。也可以通过 `OM_AUTH_SECRET_KEY` 提供 Base64 编码的
32 字节主密钥，此时应由外部 Secret 管理系统保存。

若唯一管理员遗失全部设备，只能在后端主机上停止正常服务后执行显式恢复：

```bash
cd backend
cargo run -- --reset-admin-auth \
  --confirm-reset-admin-auth RESET-ADMIN-AUTH
```

Docker 部署使用挂载了同一数据库卷的一次性后端容器：

```bash
docker compose stop backend
docker compose run --rm backend --reset-admin-auth \
  --confirm-reset-admin-auth RESET-ADMIN-AUTH
docker compose up -d backend
```

恢复命令会删除所有管理员与认证设备并立即退出；下一次正常启动重新开放一次性
密码初始化。操作日志和业务数据不会删除。

默认允许上传 256 MiB 的 Agent 包。修改后端限制时，应同时调整 Nginx 请求体限制：

```bash
OM_AGENT_PACKAGE_MAX_BYTES=536870912 \
NGINX_CLIENT_MAX_BODY_SIZE=600m \
docker compose up -d --build
```

## 实例端一键安装

实例端二进制支持显式的系统级安装命令。安装过程会询问并校验后端地址，自动通过 `sudo` 或 Windows UAC 请求管理员权限，将程序复制到系统命令目录、注册开机自启并立即启动：

```bash
./om-agent install
```

批量部署可使用无人值守模式；该模式必须显式指定后端地址：

```bash
./om-agent install --non-interactive --yes --server https://monitor.example.com
```

- Linux：安装到 `/usr/local/bin` 并注册 systemd。
- OpenWrt：安装到 `/usr/bin` 并注册 procd。
- macOS：安装到 `/usr/local/bin` 并注册 LaunchDaemon。
- Windows：安装到 `%ProgramFiles%\OM Agent`，注册 Windows Service，并加入机器级 `PATH`。

重复执行 `install` 会修复程序、配置和服务定义，同时保留已有实例身份。旧版 `operation-monitoring-agent` 的命令、安装路径和显示名会迁移为 `om-agent`，已有身份和更新状态保持不变；内部兼容标识会继续供旧 updater 和回滚版本使用。`uninstall` 默认删除新旧服务、PATH 项、程序、身份、配置、日志和更新缓存：

```bash
om-agent uninstall
om-agent uninstall --yes # 无人值守
```

这种方式安装的实例会上报 `standalone` 更新类型。发布更新时 Windows 必须同时上传 `.exe` 及其同名 `.exe.sha256`，Linux/macOS 必须同时上传 `.bin` 及其同名 `.bin.sha256`；后端会在保存前核对校验文件内容。更新时 Agent 会先下载受认证保护的 `.sha256` 文件，再校验发布记录、校验文件和实际下载文件中的摘要完全一致，校验通过后才交给独立 updater 替换自身，并在健康检查失败时恢复旧二进制。项目不再生成、分发或接受 DEB、RPM、IPK、MSI、PKG；所有平台统一使用 standalone 可执行文件。

## 打包与分发实例端 standalone 可执行文件

项目只发布独立可执行文件，不再生成或分发 DEB、RPM、IPK、MSI、PKG。控制台的程序更新接口也只接受 `package_type=standalone`。首次安装、开机自启、系统命令注册和后续卸载均由可执行文件自身的 `install` / `uninstall` 命令完成，因此不再需要原生安装包。

### 构建产物

先修改 `instanceEnd/Cargo.toml` 中的版本号，再为每个目标系统和 CPU 架构单独构建。Cargo 二进制名称固定为 `om-agent`。

在 Bash 环境中使用：

```bash
cd instanceEnd
./scripts/build-standalone.sh <rust-target> <linux|windows|macos> <native-architecture>
./scripts/build-standalone.sh all
```

常用示例：

```bash
# Linux x86_64（glibc）
./scripts/build-standalone.sh x86_64-unknown-linux-gnu linux x86_64

# OpenWrt x86_64（musl）
./scripts/build-standalone.sh x86_64-unknown-linux-musl linux x86_64-musl

# Linux / OpenWrt aarch64（musl）
./scripts/build-standalone.sh aarch64-unknown-linux-musl linux aarch64

# macOS Apple Silicon
./scripts/build-standalone.sh aarch64-apple-darwin macos arm64

# macOS Intel
./scripts/build-standalone.sh x86_64-apple-darwin macos x86_64
```

Windows 在 PowerShell 中使用：

```powershell
cd instanceEnd
.\scripts\build-standalone.ps1 -RustTarget x86_64-pc-windows-msvc -NativeArchitecture x64
.\scripts\build-standalone.ps1 all
```

`all` 会依次尝试控制台支持的全部 10 个系统/架构组合，并允许在任意目标失败后继续构建：Linux glibc `x86_64`、Linux/OpenWrt musl `x86_64-musl`、Linux `aarch64`、`arm`、`x86`，Windows `x64`、`arm64`、`x86`，以及 macOS `arm64`、`x86_64`。Bash 和 PowerShell 脚本使用相同的目标矩阵。全部尝试结束后，脚本会汇总每个失败项的系统、架构、Rust target 和首条构建错误；只要存在失败，最终退出状态即为非零。

脚本在原生目标上执行 `cargo build --locked --release --target ... --bin om-agent`。从当前主机交叉编译 Linux 目标时，如果系统已安装 Zig 和 `cargo-zigbuild`，脚本会自动改用 `cargo zigbuild`，避免 `ring` 等含 C/汇编代码的依赖因缺少目标链接器而失败。随后脚本将产物复制到 `instanceEnd/dist/standalone/`，并生成同名 `.sha256` 文件：

```text
om-agent_0.1.0_linux_x86_64.bin
om-agent_0.1.0_linux_x86_64.bin.sha256
om-agent_0.1.0_linux_x86_64-musl.bin
om-agent_0.1.0_linux_x86_64-musl.bin.sha256
om-agent_0.1.0_macos_arm64.bin
om-agent_0.1.0_macos_arm64.bin.sha256
om-agent_0.1.0_windows_x64.exe
om-agent_0.1.0_windows_x64.exe.sha256
```

交叉编译前需要安装相应 Rust target 和工具链。例如：

```bash
rustup target add x86_64-unknown-linux-gnu x86_64-unknown-linux-musl aarch64-unknown-linux-musl
rustup target add armv7-unknown-linux-gnueabihf i686-unknown-linux-gnu
rustup target add aarch64-apple-darwin x86_64-apple-darwin
rustup target add x86_64-pc-windows-msvc aarch64-pc-windows-msvc i686-pc-windows-msvc
cargo install cargo-zigbuild
# macOS: brew install zig
```

部分目标不能仅靠 `rustup target add` 完成：从 macOS 构建 Linux glibc 或 musl 目标通常需要 Zig/`cargo-zigbuild`；OpenWrt MIPS/MIPSel 需要使用与固件版本、CPU 和 libc ABI 匹配的 OpenWrt SDK 编译。使用 SDK 已配置好的 Cargo 链接器时，设置 `OM_STANDALONE_BUILDER=cargo` 可关闭 Zig 自动选择。也可以设置 `OM_STANDALONE_BUILDER=zigbuild` 强制使用 Zig。OpenWrt x86_64 使用 `linux / x86_64-musl / standalone`，普通 glibc Linux x86_64 使用 `linux / x86_64 / standalone`；Agent 会按该原生架构标识精确匹配更新，避免两种 libc 的产物互相覆盖或误发。其他 OpenWrt 架构仍必须确保二进制与设备 ABI 完全兼容。

### 首次分发和安装

将对应平台的 `.bin` 或 `.exe` 直接提供给目标机器。Unix 平台下载后添加执行权限，再运行安装命令：

```bash
chmod +x om-agent_0.1.0_linux_x86_64.bin
./om-agent_0.1.0_linux_x86_64.bin install
```

无人值守部署：

```bash
./om-agent_0.1.0_linux_x86_64.bin install \
  --non-interactive --yes \
  --server https://monitor.example.com
```

Windows 请在 PowerShell 或命令提示符中运行下载的 `.exe`：

```powershell
.\om-agent_0.1.0_windows_x64.exe install
```

安装命令会自动请求管理员权限、复制到系统目录、注册开机自启并让 `om-agent` 在命令行全局可用：

- Linux：`/usr/local/bin/om-agent` + systemd。
- OpenWrt：`/usr/bin/om-agent` + procd。
- macOS：`/usr/local/bin/om-agent` + LaunchDaemon。
- Windows：`%ProgramFiles%\OM Agent` + Windows Service + 机器级 `PATH`。

安装完成后可直接执行：

```bash
om-agent status
om-agent uninstall
```

### 发布实例端更新

1. 在控制台“程序更新”页面创建 SemVer 版本草稿。
2. 为需要覆盖的每个系统和架构上传对应 standalone 可执行文件。
3. 目标系统选择 `linux`、`windows` 或 `macos`；分发格式固定为 `standalone`。
4. Windows 文件扩展名必须为 `.exe`；Linux 和 macOS 必须为 `.bin`。
5. 原生架构必须与 Agent 上报值一致，例如 Linux `x86_64`/`aarch64`、Windows `x64`/`arm64`、macOS `arm64`/`x86_64`。
6. 检查覆盖率后发布版本；后端不会构建、转换或重命名上传内容。

Agent 会流式下载文件，校验大小、平台文件签名和 SHA-256，等待快捷命令与终端会话结束，再通过独立 updater 替换已安装程序并重启服务。新版本未能在健康检查期限内连接后端时，updater 会恢复上一版本可执行文件。自动更新要求 Agent 由 `install` 命令以系统服务方式安装并以管理员权限运行；直接通过 `start` 或 `log` 启动的开发实例不会声明自动更新能力。

生产分发应使用 HTTPS/WSS，并对 standalone 产物执行平台代码签名：Windows 对 `.exe` 使用 Authenticode，macOS 对二进制进行 Developer ID 签名和公证。后端的 SHA-256 校验用于传输完整性，不能替代平台代码签名。

### 从旧原生安装迁移

旧的 DEB、RPM、IPK、MSI、PKG 安装不会再获得匹配更新。迁移前应停止旧服务并备份实例身份与配置，然后移除旧包，下载匹配平台和架构的 standalone 文件并运行 `install`。为避免在控制台产生新实例，迁移时应保留原身份文件和原 `OM_SERVER` 配置；确认新服务上线后再清理旧包管理器残留。

## 连接与终端说明

- 实例审批完成后，指标、快捷命令和交互式终端都复用同一条 Agent WebSocket 长连接。
- 后端以内存中的 WebSocket 连接状态判断实例在线；连接关闭或心跳超时后立即判定离线，不再依赖“最后上报时间 + 固定阈值”。
- Web 终端使用系统 PTY/ConPTY，支持持续 Shell 上下文、`cd`、环境变量、交互程序、方向键、Tab、`Ctrl+C` 和窗口尺寸变化。
- 浏览器与 Agent 之间的终端数据按原始字节进行 Base64 封装，Shell 统一按 UTF-8 工作；Windows `cmd.exe` 会切换到代码页 65001，避免中文经 JSON 转发时损坏。

## 常用环境变量

后端：

```bash
OM_BIND=127.0.0.1:13500
OM_DATABASE_URL=postgresql://root@192.168.100.1:5432/operation_monitoring
OM_DATABASE_PASSWORD=<数据库密码>
OM_ADMIN_PASSWORD=admin123
OM_AUTH_KEY_FILE=db/auth-secret.key
# OM_AUTH_SECRET_KEY=<Base64 编码的 32 字节主密钥>
OM_SECURE_COOKIES=false
OM_UPLOAD_DIR=uploads
OM_UPDATE_DIR=updates
OM_AGENT_PACKAGE_MAX_BYTES=268435456
```

未设置 `OM_DATABASE_URL` 时，后端默认连接
`postgresql://root@192.168.100.1:5432/operation_monitoring`。如果该数据库不存在且
连接用户具有 `CREATEDB` 权限，后端会自动创建它。数据库密码必须通过
`OM_DATABASE_PASSWORD` 注入，不要将密码写入配置文件或提交到仓库。首次启动时，
后端会在目标 PostgreSQL 数据库中自动创建所需表和索引。

`OM_ADMIN_PASSWORD` 仅在管理员表为空时有效，完成首位管理员绑定后即使仍保留该
变量也会被忽略。`OM_SECURE_COOKIES` 在 HTTPS/WSS 生产部署中应设为 `true`；
直接使用 HTTP 本地开发时保持 `false`。会话固定有效 7 天，后端重启会要求重新登录。

实例端：

```bash
OM_SERVER=http://127.0.0.1:13500
OM_AGENT_ID_FILE=/path/to/identity.json
OM_REPORT_INTERVAL=5
OM_AGENT_STATE_DIR=/path/to/runtime
OM_AGENT_LOG_FILE=/path/to/agent.log
OM_AGENT_LOG_MAX_BYTES=10485760
OM_AGENT_LOG_HISTORY=3
OM_AGENT_UPDATE_DIR=/path/to/persistent/updates
```

实例端日志默认在单个文件达到 10 MiB 时滚动，保留 `agent.log.1` 至
`agent.log.3` 三个历史文件，超过保留数量的旧日志会直接删除。updater 日志使用相同的
大小和保留策略。将 `OM_AGENT_LOG_HISTORY` 设为 `0` 可在滚动时直接丢弃旧日志。

同一状态目录只允许一个实例端进程运行。若要在一台机器上运行多个实例端，请为每个进程设置不同的 `OM_AGENT_STATE_DIR`、`OM_AGENT_ID_FILE` 和 `OM_AGENT_UPDATE_DIR`。更新目录保存可执行文件、回滚基线、状态和 updater 日志，不能放在重启后会清空的临时目录中。OpenWrt standalone 安装默认使用 `/var/lib/om-agent/updates`。

## 验证命令

```bash
cd front-end && pnpm build
cd backend && cargo check
cd instanceEnd && cargo check
```

## 后续增强

- 增加历史趋势图和指标明细页。
- 增加告警规则、通知渠道和阈值配置。
- 增加登录失败限制、密码哈希和更细粒度权限。
- 增加 CI 中的多平台 standalone 可执行文件构建、签名和发布流水线。
