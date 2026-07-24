# Operation Monitoring

一个自托管的远程资源监控系统 MVP，用于小规模服务器、电脑实例的资源上报、审批管理、快捷命令、Web 终端和远程文件管理。

## 文档

- [Docker Compose 部署指南](docs/deployment.md)：默认部署方式、PostgreSQL、HTTPS、持久化、备份、升级与故障排查。
- [实例端 standalone 打包指南](docs/instance-agent-packaging.md)：跨平台构建、校验与分发。

## 项目结构

```text
operationMonitoring/
  front-end/      Vue 3 + Vite 前端控制台
  backend/        Rust + Axum + SQLx 后端服务
  instanceEnd/    Rust 实例端 Agent
  docs/           部署与实例端打包文档
  docker-compose.with-db.yml  默认自带 PostgreSQL 部署
  docker-compose.yml          外部 PostgreSQL 部署
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
  db.rs           PostgreSQL 连接、建表、查询辅助、清理任务
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

Windows 网页终端优先使用 ConPTY。Windows Server 2016 没有系统级 ConPTY API，Agent 会自动
切换到隐藏窗口的管道终端，仍可执行交互式 `cmd.exe` 命令；该兼容模式不支持真实 PTY 的
窗口大小同步和部分控制台专用快捷键。若兼容终端启动失败，终端会把具体原因返回到页面，
并写入 Agent 日志，不会再因为打开终端导致 Agent 进程退出。

## 默认部署：Docker Compose

生产和试用环境默认使用 `docker-compose.with-db.yml`，同时启动 PostgreSQL、后端与前端。Compose 会初始化业务数据库，并持久化数据库、认证密钥、背景图片和 Agent 更新包。

```bash
./deploy.sh deploy docker-compose.with-db.yml
# 首次执行会生成 .env 并暂停；编辑密码等配置后重新执行同一命令
```

脚本会校验 Compose 配置、构建镜像并在后台启动服务。使用外部 PostgreSQL 时执行
`./deploy.sh deploy docker-compose.yml`。升级前完成数据备份，再执行
`./deploy.sh update <Compose 文件>`；更新会拒绝有本地修改的工作区，并切换到远端版本号最高的稳定 TAG。

默认地址：

- 前端控制台：`http://localhost:13501`
- 后端 API：`http://localhost:13500`
- 健康检查：`http://localhost:13500/api/health`

首次初始化只使用一次 `OM_ADMIN_PASSWORD` 创建管理员；登录后需绑定 Authenticator 并使用 TOTP。生产环境应通过 HTTPS/WSS 访问，将 `OM_SECURE_COOKIES` 设为 `true`，并按部署场景限制宿主机端口暴露。实例端通常连接前端代理地址，前端会转发 API、上传和 WebSocket。

PostgreSQL 默认只在 Compose 网络内开放。需要使用已有或托管 PostgreSQL 时，改用 `docker-compose.yml`，并同时显式设置 `OM_DATABASE_URL` 和 `OM_DATABASE_PASSWORD`。Compose 项目名固定为 `operation-monitoring`，详细的两种数据库模式、反向代理、备份恢复、升级、管理员认证恢复和排障步骤见[Docker Compose 部署指南](docs/deployment.md)。

## 源码开发

源码启动仅用于开发和调试。先准备可访问的 PostgreSQL，再启动后端：

```bash
cd backend
OM_DATABASE_URL='postgresql://operation_monitoring@127.0.0.1:5432/operation_monitoring' \
OM_DATABASE_PASSWORD='<数据库密码>' \
OM_ADMIN_PASSWORD=admin123 \
cargo run
```

启动前端开发服务器：

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

## Windows 网页远程桌面

Windows 10/11 和带 Desktop Experience 的 Windows Server 2016 及以上版本，在更新到包含
`remote_desktop_v1` 能力的 Agent 后，可以从实例详情的“操作”页直接打开远程桌面。画面和
键鼠输入通过 Agent 主动建立的专用 WebSocket 传输，不需要开放 3389 端口，也不依赖 Windows
RDP、Guacamole、STUN 或 TURN。

安装为 Windows 服务时，Agent 服务会在当前活动的登录用户会话中启动同一程序的受限桌面
helper；非服务方式运行 Agent 时则使用当前用户会话。第一版每台实例只允许一名管理员独占控制主显示器，
画面上限为 1920×1080、目标 8–12 FPS。系统服务模式支持锁屏、Windows 登录界面和 UAC
安全桌面，并可发送 Ctrl+Alt+Del；非服务方式运行时会在安全桌面暂停，返回普通桌面后自动恢复。
多个非控制台活动会话、Windows Server Core、剪贴板、音频、录屏和移动端触控暂不支持。

远程桌面包含实时画面和控制输入，生产部署必须通过 HTTPS/WSS 访问前端和后端。后端会记录
会话管理员、实例、开始时间、结束时间和结束原因，但不会保存画面或键鼠内容。

查询状态或停止实例端：

```bash
./target/release/om-agent status
./target/release/om-agent stop
```

需要查看实例端日志时，使用 `log`：

```bash
./target/release/om-agent log
```

实例端不提供 `run` 命令。`log` 会先显示当前日志内容，再持续输出新增日志，按 `Ctrl+C` 退出；它不会启动第二个实例端进程。系统安装会自动读取对应平台的服务日志：Linux/OpenWrt 为 `/var/log/om-agent/agent.log`，macOS 为 `/Library/Logs/OperationMonitoring/agent.log`，Windows 为 `C:\ProgramData\OperationMonitoring\logs\agent.log`。显式传入 `OM_AGENT_LOG_FILE` 或 `--log-file` 时优先使用指定文件；Unix 系统日志由 root 创建且当前用户无读取权限时，请使用 `sudo om-agent log`。

开发时也可以通过 Cargo 执行相同命令：

```bash
cargo run -- start --server http://127.0.0.1:13500
cargo run -- status
cargo run -- log
cargo run -- stop
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

完整的环境准备、各操作系统打包步骤、交叉编译依赖、目标架构对照、产物校验和常见故障处理，请参阅：[实例端 standalone 打包指南](docs/instance-agent-packaging.md)。

打包前先修改 `instanceEnd/Cargo.toml` 中的版本号，并同步 `Cargo.lock`。Cargo 二进制名称固定为 `om-agent`。

在 Linux 或 macOS 的 Bash 环境中，可以构建单个目标，也可以依次构建全部 10 个支持目标：

```bash
cd instanceEnd
./scripts/build-standalone.sh <rust-target> <linux|windows|macos> <native-architecture>
./scripts/build-standalone.sh all
```

例如：

```bash
# Linux x86_64（glibc）
./scripts/build-standalone.sh x86_64-unknown-linux-gnu linux x86_64

# OpenWrt x86_64（musl）
./scripts/build-standalone.sh x86_64-unknown-linux-musl linux x86_64-musl

# macOS Apple Silicon
./scripts/build-standalone.sh aarch64-apple-darwin macos arm64

# Windows x64（从 Linux/macOS 交叉编译）
./scripts/build-standalone.sh x86_64-pc-windows-msvc windows x64
```

Windows 原生打包推荐使用 `.cmd` 入口。无参数时会依次构建 Windows x64、x86 和 ARM64：

```powershell
cd instanceEnd
.\scripts\build-standalone.cmd
```

也可以只构建一个 Windows target：

```powershell
.\scripts\build-standalone.cmd x86_64-pc-windows-msvc
.\scripts\build-standalone.cmd aarch64-pc-windows-msvc
.\scripts\build-standalone.cmd i686-pc-windows-msvc
```

脚本默认自动选择构建器：GNU/Linux 目标固定使用 `cargo-zigbuild` 并以 glibc 2.17 为最低兼容基线，其他 Linux 交叉目标在工具可用时也使用 `cargo-zigbuild`，Windows MSVC 交叉目标使用 `cargo-xwin`。因此执行 `all` 前必须安装 Zig 和 cargo-zigbuild。如果系统缺少 `llvm-lib`，Bash 脚本会使用项目内置包装器和 `zig ar` 完成 Windows 静态库归档。也可以通过 `OM_STANDALONE_BUILDER=cargo|zigbuild|xwin` 强制选择构建器，但 GNU/Linux 目标不允许覆盖为其他构建器，以免绕过 glibc 2.17 基线。

`all` 会依次尝试 Linux 5 个目标、Windows 3 个目标和 macOS 2 个目标。单个目标失败后仍会继续构建，最后统一汇总失败原因。产物和同名 SHA-256 文件位于 `instanceEnd/dist/standalone/`：

```text
om-agent_<version>_linux_x86_64.bin
om-agent_<version>_linux_x86_64.bin.sha256
om-agent_<version>_windows_x64.exe
om-agent_<version>_windows_x64.exe.sha256
om-agent_<version>_macos_arm64.bin
om-agent_<version>_macos_arm64.bin.sha256
```

发布时必须同时上传可执行文件及其 `.sha256` 文件。Linux glibc `x86_64` 与 musl `x86_64-musl` 是不同更新目标，不能混用。完整目标矩阵和 OpenWrt SDK 注意事项见[打包指南](docs/instance-agent-packaging.md)。

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
- Windows：`%ProgramFiles%\OM Agent` + Windows Service + 机器级 `PATH`，并在
  Windows 系统命令目录创建 `om-agent.exe` 全局命令入口（64 位系统同时覆盖 64 位和
  WOW64 命令搜索目录）。该入口不依赖 Explorer 或终端刷新，兼容 Windows Server Core、
  RDP 长期会话及 Windows Server 2016。

Windows 安装或自动更新后，当前 CMD/PowerShell 可直接使用 `om-agent`。如需诊断，
可分别运行 `where om-agent`、`"%ProgramFiles%\OM Agent\om-agent.exe" status`。

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

自动更新无法使用时，可先通过实例文件管理上传匹配系统与架构的新 Agent，再从前端终端或命令执行器调用本地强制更新：

```bash
om-agent update /path/to/new/om-agent
```

Windows 路径含空格时需要加引号，例如 `om-agent update "C:\Temp\om-agent new.exe"`。`update` 只接受本地 standalone 可执行文件，要求 Agent 以 root/管理员权限安装运行；它会先复制并执行 `--version` 预检，再交给独立 updater。命令显示 `has been handed off` 后会退出，服务随后重启。强制更新不受管理端版本策略限制，允许重装、升级或降级；存在旧程序时仍会保留回滚基线，并在新版本未能通过健康检查时自动恢复。此命令是自动更新失效后的恢复手段，不应与另一个正在运行的 updater 并发执行。

生产分发应使用 HTTPS/WSS，并对 standalone 产物执行平台代码签名：Windows 对 `.exe` 使用 Authenticode，macOS 对二进制进行 Developer ID 签名和公证。后端的 SHA-256 校验用于传输完整性，不能替代平台代码签名。

### 从旧原生安装迁移

旧的 DEB、RPM、IPK、MSI、PKG 安装不会再获得匹配更新。迁移前应停止旧服务并备份实例身份与配置，然后移除旧包，下载匹配平台和架构的 standalone 文件并运行 `install`。为避免在控制台产生新实例，迁移时应保留原身份文件和原 `OM_SERVER` 配置；确认新服务上线后再清理旧包管理器残留。

## 连接与终端说明

- 实例审批完成后，指标、快捷命令和交互式终端都复用同一条 Agent WebSocket 长连接。
- 管理员可从实例详情面板浏览 Agent 权限范围内的整机文件系统，执行流式上传、下载、新建目录、重命名、同盘移动和永久删除。文件内容不会写入后端磁盘。
- 后端以内存中的 WebSocket 连接状态判断实例在线；连接关闭或心跳超时后立即判定离线，不再依赖“最后上报时间 + 固定阈值”。
- Web 终端使用系统 PTY/ConPTY，支持持续 Shell 上下文、`cd`、环境变量、交互程序、方向键、Tab、`Ctrl+C` 和窗口尺寸变化。
- 浏览器与 Agent 之间的终端数据按原始字节进行 Base64 封装，Shell 统一按 UTF-8 工作；Windows `cmd.exe` 会切换到代码页 65001，避免中文经 JSON 转发时损坏。

## 常用环境变量

后端：

```bash
OM_BIND=127.0.0.1:13500
OM_DATABASE_URL=postgresql://operation_monitoring@127.0.0.1:5432/operation_monitoring
OM_DATABASE_PASSWORD=<数据库密码>
OM_ADMIN_PASSWORD=admin123
OM_AUTH_KEY_FILE=auth/auth-secret.key
# OM_AUTH_SECRET_KEY=<Base64 编码的 32 字节主密钥>
OM_SECURE_COOKIES=false
OM_UPLOAD_DIR=uploads
OM_UPDATE_DIR=updates
OM_AGENT_PACKAGE_MAX_BYTES=268435456
OM_FILE_TRANSFER_MAX_BYTES=1073741824
```

未设置 `OM_DATABASE_URL` 时，后端默认连接
`postgresql://root@127.0.0.1:5432/operation_monitoring`。如果该数据库不存在且
连接用户具有 `CREATEDB` 权限，后端会自动创建它。数据库密码必须通过
`OM_DATABASE_PASSWORD` 注入，不要将密码写入配置文件或提交到仓库。首次启动时，
后端会在目标 PostgreSQL 数据库中自动创建所需表和索引。

上述 URL 是后端进程直接启动时的默认值。`docker-compose.with-db.yml` 会自动改用 Compose 内网中的 `postgres` 服务；外部数据库用的 `docker-compose.yml` 则要求显式设置 `OM_DATABASE_URL` 和 `OM_DATABASE_PASSWORD`，避免错误连接到后端容器自身或使用空密码。

`OM_ADMIN_PASSWORD` 仅在管理员表为空时有效，完成首位管理员绑定后即使仍保留该
变量也会被忽略。`OM_SECURE_COOKIES` 在 HTTPS/WSS 生产部署中应设为 `true`；
直接使用 HTTP 本地开发时保持 `false`。会话固定有效 7 天，后端重启会要求重新登录。

`OM_FILE_TRANSFER_MAX_BYTES` 限制单个远程上传或下载文件的大小，默认 1 GiB。反向代理的请求体上限必须不小于该值；Docker 前端默认将 `NGINX_CLIENT_MAX_BODY_SIZE` 设置为 `1g`，并关闭 API 请求与响应缓冲以保持流式传输。远程文件操作拥有与 Agent 服务进程相同的系统权限，生产环境应严格保护管理员账号和 TOTP 设备。

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
