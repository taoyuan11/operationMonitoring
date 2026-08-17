# Operation Monitoring

一个自托管的远程资源监控与运维系统，用于小规模服务器和电脑实例的资源上报、生命周期管理、接入审批、告警通知、快捷命令、Web 终端和远程文件管理。

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
  api/            HTTP、文件传输与 WebSocket 请求封装
  components/     实例、命令、告警、审计、更新及远程操作组件
  composables/    控制台状态与业务动作
  data/           国家和地区等静态数据
  styles/         基础、布局、控件、看板、管理面板、弹窗、响应式样式
  types/          前端领域类型
  utils/          格式化与指标计算工具
front-end/tests/  Node test runner 回归测试
```

`App.vue` 负责页面路由和顶层装配。实例概览、接入审核、快捷命令、程序更新、告警中心、用户管理、统一审计和系统设置是独立管理视图；接口调用、状态管理和 UI 组件分别放在对应目录中。

## 后端分层

```text
backend/src/
  admin_auth.rs   管理员初始化、登录、用户与认证设备 API
  alerts.rs       告警规则、事件、维护窗口和通知投递
  audit.rs        管理操作审计查询与导出
  auth.rs         TOTP、密钥加密和管理员 session 校验
  config.rs       启动参数和环境变量
  db.rs           PostgreSQL 连接、建表、查询辅助、清理任务
  docker.rs       远端 Docker 管理 API
  error.rs        统一错误响应
  files.rs        远端文件管理与流式传输
  handlers/       HTTP API handler
  jobs.rs         命令任务创建、下发、完成
  models.rs       请求、响应、数据库行模型
  remote_desktop.rs  Windows 远程桌面会话中继
  request_security.rs  代理来源、协议与 Origin 校验
  state.rs        全局共享状态
  update_signature.rs Agent 更新签名密钥
  updates.rs      Agent 发布、灰度、更新和回滚
  utils.rs        通用工具
  ws.rs           Agent WebSocket 和 Web 终端
```

## 实例端分层

```text
instanceEnd/src/
  activity.rs     活动会话计数与更新协调
  command.rs      系统命令执行与超时截断
  config.rs       Agent 启动参数
  device_profile.rs  主机硬件和网络资料采集
  docker.rs       本机 Docker 探测与操作
  file_manager.rs  文件浏览与流式传输
  http.rs         审批前注册请求
  identity.rs     本地实例身份生成和读取
  install.rs      跨平台系统服务安装与卸载
  lifecycle.rs    实例进程启动、停止与状态管理
  logging.rs      Agent 与 updater 日志滚动
  metrics.rs      CPU、内存、磁盘、网络采集
  models.rs       Agent 与后端通信模型
  outbound.rs     Agent 出站请求并发控制
  profile.rs      主机基础信息
  pty_io.rs       PTY 原始字节读写
  remote_desktop/ Windows 桌面捕获和输入控制
  terminal.rs     跨平台 PTY/ConPTY 交互式 Shell
  time.rs         时间戳工具
  update.rs       standalone 自更新与回滚
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

脚本会校验 Compose 配置、构建镜像，并先等待后端完成数据库迁移和健康检查，再启动前端。
使用外部 PostgreSQL 时执行 `./deploy.sh deploy docker-compose.yml`。升级前完成数据备份，再执行
`./deploy.sh update <Compose 文件>`；更新会拒绝有本地修改的工作区，只允许切换到严格高于
当前后端版本的稳定 TAG。数据库较大时可以增加脚本等待时间，例如：
`OM_DEPLOY_BACKEND_TIMEOUT_SECONDS=3600 ./deploy.sh update docker-compose.yml`。
服务端版本升级不提供降级、回滚 API、命令或兼容保障，详见[Docker Compose 部署指南](docs/deployment.md)。

默认地址：

- 前端控制台：`http://localhost:13501`
- 后端 API：`http://localhost:13500`
- 健康检查：`http://localhost:13500/api/health`

首次初始化只使用一次 `OM_ADMIN_PASSWORD` 创建管理员；登录后需绑定 Authenticator 并使用 TOTP。生产环境应通过 HTTPS/WSS 访问，将 `OM_SECURE_COOKIES` 设为 `true` 作为协议回退，并按部署场景限制宿主机端口暴露。可信代理提供 `X-Forwarded-Proto` 时，后端会按每个请求分别支持 HTTP/WS 和 HTTPS/WSS。实例端通常连接前端代理地址，前端会转发 API、上传和 WebSocket。

PostgreSQL 默认只在 Compose 网络内开放。需要使用已有或托管 PostgreSQL 时，改用 `docker-compose.yml`，并同时显式设置 `OM_DATABASE_URL` 和 `OM_DATABASE_PASSWORD`。Compose 项目名固定为 `operation-monitoring`，详细的两种数据库模式、反向代理、备份恢复、升级、管理员认证恢复和排障步骤见[Docker Compose 部署指南](docs/deployment.md)。

## 源码开发

源码启动仅用于开发和调试。先准备可访问的 PostgreSQL，再启动后端：

```bash
cd backend
OM_DATABASE_URL='postgresql://operation_monitoring@127.0.0.1:5432/operation_monitoring' \
OM_DATABASE_PASSWORD='<数据库密码>' \
OM_ADMIN_PASSWORD='development-bootstrap-password' \
cargo run
```

启动前端开发服务器：

```bash
cd front-end
pnpm install
pnpm dev
```

开发服务器仅监听 `127.0.0.1`，默认访问地址为 `http://127.0.0.1:5173`。需要从其他设备调试时，应通过受控反向代理访问，不要将带管理能力的开发服务器直接暴露到公网。

构建并在后台启动实例端：

```bash
cd instanceEnd
cargo build --release
./target/release/om-agent start --server http://127.0.0.1:13500
```

`start` 会在后台启动实例端并立即释放命令行，标准输出和错误输出会写入命令返回的日志路径。Windows 使用同目录下的 `om-agent.exe`，后台子进程不会创建控制台窗口。

## 控制台与实例生命周期

控制台首页公开展示已批准且未禁用的实例及最新指标。管理员登录后可进入独立的接入审核、
快捷命令、程序更新、告警中心、用户管理、统一审计和系统设置页面。快捷命令页用于维护命令
白名单和查看最近任务，实际执行入口位于实例的操作面板；任意命令仍以 Agent 服务账号权限
在目标主机上运行。

管理员可在实例编辑弹窗中设置到期时间，或选择“长期有效”清除到期时间。实例卡片会向访客和
管理员展示绝对到期时间及剩余时间；到期仅作为生命周期标记和告警依据，不会自动禁用、断开或
删除实例。需要提醒时，在告警中心创建“实例即将到期”规则，并设置非负整数天阈值。后端会
周期检查全部或指定实例，剩余天数小于等于阈值时立即创建事件；清除到期时间或将日期延后到
阈值之外后，活动事件会自动恢复。

## 实例 Docker 管理

实例详情中的“容器”页签用于管理单台 Linux 或 OpenWrt 主机上的 Docker；Windows 和 macOS
不探测也不展示该功能。该功能要求实例端 Agent `0.1.18` 或更高版本，并要求主机已安装
Docker CLI `20.10` 或更高版本。Compose 管理依赖 Docker Compose v2 插件；未安装插件时仅禁用
Compose 视图，不影响容器、镜像、网络、存储卷和系统空间管理。

Docker 命令由 Agent 在实例本机执行，权限、Docker context 和 credential store 均继承 Agent
服务账号。部署前应确保该账号能够访问目标 Docker daemon，并在需要拉取私有镜像时预先为该账号
配置 Docker 登录凭据；管理端不会接收、传输或保存 Registry 凭据。后端不挂载远端 Docker
socket，Docker 面板和接口仅对已登录管理员开放。

Agent 会定期检测 Docker。Docker 已安装但 daemon 不可达、权限不足或版本不受支持时仍保留
“容器”页签，显示版本与诊断信息并禁用操作；实例离线时保留最后一次检测结果，但不展示离线前的
容器、镜像、网络或存储卷清单。升级时应先部署兼容的 backend 和 front-end，再发布 Agent
`0.1.18`；旧 Agent 不会显示 Docker 管理面板。

## 实例硬件配置

Agent `0.1.21` 起会在注册和重连时上报设备配置。硬件采集包括操作系统、CPU 型号和核心数、
内存总量、GPU 型号与显存、存储总容量，以及管理员可见的内核版本、磁盘明细、网卡、IP 和
MAC 地址。采集失败只会产生部分资料，不会阻断 Agent 连接；旧 Agent 的资料会显示为等待
更新或重连。

游客可以在实例详情中查看 CPU/GPU 型号等硬件摘要；内核、磁盘挂载点、网卡地址和后端观察到的
连接 IP 只通过管理员接口展示。设备资料会限制大小并过滤回环网卡和无效 MAC，不会出现在公开
实例列表或指标响应中。升级时应先部署兼容的 backend 和 front-end，再发布 Agent `0.1.21`。

## Windows 网页远程桌面

Windows 10/11 和带 Desktop Experience 的 Windows Server 2016 及以上版本，在更新到包含
`remote_desktop_v1` 能力的 Agent 后，可以从实例详情的“操作”页直接打开远程桌面。画面和
键鼠输入通过 Agent 主动建立的专用 WebSocket 传输，不需要开放 3389 端口，也不依赖 Windows
RDP、Guacamole、STUN 或 TURN。

包含 `remote_desktop_audio_v1` 能力的 Agent 还可传输 Windows 默认播放设备的系统声音。
声音使用 48 kHz 双声道 Opus，并要求 Chrome 或 Edge 支持 WebCodecs `AudioDecoder`；不支持
该能力或解码器的浏览器仍可正常使用纯画面。每次新开远程桌面均默认静音，只有管理员主动点击
工具栏扬声器后才会开始采集和播放。同一弹窗内切换画质或重连会保留启声意图，关闭弹窗后恢复
默认静音。声音能力随 Windows x64 和 ARM64 Agent 提供；i686 Agent 暂时仅支持远程桌面画面。

安装为 Windows 服务时，Agent 服务会在当前活动的登录用户会话中启动同一程序的受限桌面
helper；非服务方式运行 Agent 时则使用当前用户会话。第一版每台实例只允许一名管理员独占控制主显示器，
画质默认使用“均衡”档，并可在远程桌面工具栏切换；切换时会自动重连，浏览器会记住最后一次选择。

| 画质 | 最大分辨率 | 自适应 FPS | JPEG（初始 / 自适应范围） |
| --- | ---: | ---: | ---: |
| 省流 | 960×540 | 4–6 | 35 / 25–40 |
| 均衡 | 1280×720 | 6–8 | 50 / 30–55 |
| 清晰 | 1600×900 | 8–10 | 60 / 40–65 |
| 原画 | 1920×1080 | 8–12 | 70 / 50–75 |

所有档位都会保持显示器宽高比且不会放大低分辨率画面。“原画”用于保留原有的 1080p 高带宽体验，
仍采用有损 JPEG，不代表显示器原生 2K/4K 分辨率或无损编码。系统服务模式支持锁屏、Windows 登录界面和 UAC
安全桌面，并可发送 Ctrl+Alt+Del；非服务方式运行时会在安全桌面暂停，返回普通桌面后自动恢复。
进入安全桌面、切换到非默认桌面、撤销本地同意或结束会话时，系统声音会立即停止并清空缓冲。
首版只采集默认播放设备的系统声音，不采集麦克风，也不提供设备选择、音量调节或录音。多个非控制台
活动会话、Windows Server Core、剪贴板、录屏和移动端触控暂不支持。

远程桌面包含实时画面、系统声音和控制输入，生产部署必须通过 HTTPS/WSS 访问前端和后端。
后端会记录会话管理员、实例、开始时间、结束时间、结束原因和协商的音频编码，但不会保存画面、
音频或键鼠内容。升级时应先部署兼容的 backend 和 front-end，再发布带
`remote_desktop_audio_v1` 能力的 Agent；旧 Agent 会继续提供纯画面。

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

Agent `0.1.22` 起支持 Ed25519 更新签名。用于 HTTP 自动更新的正式产物必须在编译时
嵌入后端对应的公钥；同一公钥也会让 HTTPS 更新强制验签：

```bash
cd instanceEnd
OM_UPDATE_PUBLIC_KEY='<后端启动日志中的 Base64 公钥>' \
OM_UPDATE_PUBLIC_KEY_ID='release-v1' \
./scripts/build-standalone.sh <rust-target> <linux|windows|macos> <native-architecture>
```

`OM_UPDATE_PUBLIC_KEY` 和 `OM_UPDATE_PUBLIC_KEY_ID` 是编译期参数，不是 Agent 运行时
配置。未嵌入公钥的 Agent 只允许通过 HTTPS 自动更新；嵌入公钥后，HTTP 和 HTTPS
都会要求后端使用匹配的密钥签名。后端签名密钥的生成和 Compose 配置见
[部署指南](docs/deployment.md#agent-更新签名)。
两个变量必须同时设置或同时省略；Cargo 会在编译时校验 Base64、32 字节 Ed25519
公钥和 key ID，配置不完整或无效时直接终止构建。

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

Windows x64/ARM64 的无显示器/无播放设备正式产物必须在原生 Windows 构建机上生成。先按
[`instanceEnd/windows-drivers/README.md`](instanceEnd/windows-drivers/README.md) 完成 WDK、HLK、
Driver Verifier 和 Hardware Partner Center 签名流程，再设置 `OM_WINDOWS_DRIVER_BUNDLE_DIR`
为包含 `bundle-lock.json` 的 Microsoft 签名驱动目录。脚本会自动启用
`bundled-windows-drivers` feature，并用 `OM_SIGNTOOL_PATH`（未设置时使用 `signtool.exe`）执行
`/kp` 校验。最终 EXE 的 Authenticode 签名使用证书存储中的
`OM_WINDOWS_SIGNING_CERTIFICATE_SHA1` 和 RFC 3161 地址 `OM_WINDOWS_TIMESTAMP_URL`；只有签名及
`/pa` 校验成功后才生成 SHA-256。未设置驱动目录的普通开发构建不嵌入驱动，保持
physical-only；Windows x86 始终不嵌入虚拟设备驱动。

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

将对应平台的 `.bin` 或 `.exe` 直接提供给目标机器。以下命令中的 `<version>` 应替换为
本次发布的 SemVer。Unix 平台下载后添加执行权限，再运行安装命令：

```bash
AGENT_VERSION='<version>'
chmod +x "om-agent_${AGENT_VERSION}_linux_x86_64.bin"
"./om-agent_${AGENT_VERSION}_linux_x86_64.bin" install
```

无人值守部署：

```bash
AGENT_VERSION='<version>'
"./om-agent_${AGENT_VERSION}_linux_x86_64.bin" install \
  --non-interactive --yes \
  --server https://monitor.example.com
```

Windows 请在 PowerShell 或命令提示符中运行下载的 `.exe`：

```powershell
$AgentVersion = '<version>'
& ".\om-agent_${AgentVersion}_windows_x64.exe" install
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

首次发布必须在实例选择弹窗中手动勾选首批灰度目标。离线实例可以入选，重新上线后会收到任务；灰度期间可以继续添加批次。版本卡会显示 `灰度中`、`灰度已暂停`、`全量`、`全量已暂停`、`回滚中`、`已回滚` 或 `部分回滚` 状态。

暂停只阻止尚未下发的任务，已经进入等待、下载、校验、安装或等待重连的任务会继续完成。灰度晋级全量后，当前及以后符合平台、权限和版本条件且未被实例级排除的实例会自动获取该版本。任一时刻只有一个版本可以处于灰度、暂停或受控回滚流程；旧的全量版本仍作为稳定基线。

版本级回滚会停止该版本继续分发，取消尚未下发的升级，并覆盖本次发布成功升级的实例。回滚优先使用匹配平台的已发布旧包，其次使用 Agent 本地保留的一代基线；旧 Agent 或两种路径都不可用的实例会保留明确失败原因，其余实例继续执行。成功升级记录提供实例级“回滚”，回滚成功记录提供“重新升级”；实例级回滚会排除该实例，只有管理员明确重新升级才会重新加入当前版本。

草稿和已发布版本都可以从控制台申请删除，但仍被当前实例、活动任务或回滚记录依赖的发布及产物会被后端保护。只有依赖解除后才会删除；删除会同时清除该版本的可执行文件、`.sha256` 校验文件、产物元数据和实例更新记录，仅保留管理员操作日志，且无法恢复。已经安装该版本的实例不会因为删除自动回退。

Agent 会流式下载文件，校验大小、平台文件签名和 SHA-256，等待快捷命令与终端会话结束，再通过独立 updater 替换已安装程序并重启服务。普通升级失败时会恢复上一版本；回滚目标健康检查失败时会恢复回滚前版本并将任务标记失败。更新状态目录始终保留当前包和一代回滚基线，下一次成功升级或回滚时安全轮换并清理更旧缓存。自动更新要求 Agent 由 `install` 命令以系统服务方式安装并以管理员权限运行；直接通过 `start` 或 `log` 启动的开发实例不会声明自动更新能力。

部署时应先升级 backend 和 front-end，再发布带有 `agent_rollback_v1` capability 的 Agent。旧 Agent 继续支持灰度、暂停、恢复、晋级和普通升级调度，但控制台会标记其不支持主动回滚；主动回滚只对支持该协议且具备服务端旧包或本地基线的实例生效。

从仓库标签 `0.1.2`（Agent `0.1.20`）可以直接升级到当前协议：旧 Agent 会忽略新增的
签名字段，实例 ID、原始实例密钥和更新状态文件也保持兼容。历史版本本身不具备验签
代码，因此它通过纯 HTTP 执行的第一次远程升级无法获得签名保护；这一次必须使用
HTTPS，或离线核对产物后执行本地 `om-agent update`。升级到嵌入公钥的 `0.1.22`
后，后续 HTTP 与 HTTPS 自动更新都可验证签名。服务端会同时提供兼容 `0.1.22` 的
产物签名和 `0.1.23` 使用的完整元数据签名；`0.1.23` 在 HTTP 下拒绝缺少完整元数据
签名的更新。未携带 `agent_rollback_v1` 的旧 Agent
不会执行主动回滚，也不会伪造回滚能力；升级后重新注册或上报指标即可恢复真实能力和本地基线版本。

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
- 终端工作区支持跨实例标签页；每个实例最多同时运行 8 个相互隔离的会话。关闭单个标签仅结束对应会话，关闭工作区会结束其中全部活动会话。
- 新版 Agent 支持自动 Shell、实时检测到的 Shell 和自定义 Shell 可执行文件。自定义值只能是可执行文件名或绝对路径，不接受命令参数，并始终以 Agent 服务账户的权限运行。
- 浏览器与 Agent 之间的终端数据按原始字节进行 Base64 封装，Shell 统一按 UTF-8 工作；Windows `cmd.exe` 会切换到代码页 65001，避免中文经 JSON 转发时损坏。

## 常用环境变量

后端：

```bash
OM_BIND=127.0.0.1:13500
OM_DATABASE_URL=postgresql://operation_monitoring@127.0.0.1:5432/operation_monitoring
OM_DATABASE_PASSWORD=<数据库密码>
OM_ADMIN_PASSWORD=<至少16字节的随机初始化密码>
OM_AUTH_KEY_FILE=auth/auth-secret.key
# OM_AUTH_SECRET_KEY=<Base64 编码的 32 字节主密钥>
OM_SECURE_COOKIES=false
OM_TRUST_PROXY_HEADERS=false
OM_TRUSTED_PROXY_CIDRS=127.0.0.1/32,::1/128
OM_ALLOW_LEGACY_AGENT_WS_AUTH=false
OM_UPLOAD_DIR=uploads
OM_UPDATE_DIR=updates
# OM_UPDATE_SIGNING_KEY_FILE=/absolute/path/update-signing.key
OM_UPDATE_SIGNING_KEY_ID=default
OM_AGENT_PACKAGE_MAX_BYTES=268435456
OM_FILE_TRANSFER_MAX_BYTES=1073741824
# Set true when Cloudflare Tunnel/HTTPS proxy is the only frontend ingress.
NGINX_TRUST_FORWARDED_PROTO=false
# Set true only when Cloudflare Tunnel is the only frontend ingress.
NGINX_TRUST_CF_CONNECTING_IP=false
```

未设置 `OM_DATABASE_URL` 时，后端默认连接
`postgresql://root@127.0.0.1:5432/operation_monitoring`。如果该数据库不存在且
连接用户具有 `CREATEDB` 权限，后端会自动创建它。数据库密码必须通过
`OM_DATABASE_PASSWORD` 注入，不要将密码写入配置文件或提交到仓库。首次启动时，
后端会在目标 PostgreSQL 数据库中自动创建所需表和索引。

上述 URL 是后端进程直接启动时的默认值。`docker-compose.with-db.yml` 会自动改用 Compose 内网中的 `postgres` 服务；外部数据库用的 `docker-compose.yml` 则要求显式设置 `OM_DATABASE_URL` 和 `OM_DATABASE_PASSWORD`，避免错误连接到后端容器自身或使用空密码。

`OM_ADMIN_PASSWORD` 在管理员表为空时必须显式设置且至少包含 16 字节，且不能保留
`.env.example` 中公开的占位值；完成首位管理员绑定后，直接启动后端时可以移除该变量。
Compose 为避免误用始终要求提供它。
`OM_SECURE_COOKIES` 是请求未经过可信协议代理时的回退值，在 HTTPS/WSS 生产部署中
应设为 `true`，直接使用 HTTP 本地开发时保持 `false`。连接端命中可信代理网段且代理
覆盖写入 `X-Forwarded-Proto` 时，后端会按当前请求动态校验 Origin，并只在 HTTPS
登录响应中增加 `Secure`。HTTP 与 HTTPS 使用不同名称的会话 Cookie，因此同一部署和
同一浏览器可同时保留 HTTP/WS 和 HTTPS/WSS。会话固定有效 7 天，后端重启会要求
重新登录。

`OM_TRUST_PROXY_HEADERS` 控制登录限流和协议判定是否读取反向代理提供的
`X-Forwarded-For`、`X-Real-IP`、`X-Forwarded-Proto`；默认关闭。客户端地址会从
`X-Forwarded-For` 右侧开始逐跳剥离可信代理，遇到首个不可信地址即停止。
启用时，只有连接端地址命中 `OM_TRUSTED_PROXY_CIDRS` 中逗号分隔的 IP/CIDR 才会
采信该请求头。应按实际代理网络配置最小范围，并确保客户端不能绕过代理直连后端。
Compose 将后端宿主端口固定绑定到 `127.0.0.1`，并默认把 PostgreSQL、前端代理和
后端分别固定为 `172.30.135.2`、`172.30.135.3` 和 `172.30.135.4`，网络网关固定为
`172.30.135.1`。后端信任前端代理地址，Compose 还会自动追加网关 `/32`，供宿主机
反向代理安全传递客户端地址和协议。若该网段与现有网络冲突，必须同时调整
`OM_COMPOSE_NETWORK_CIDR`、`OM_COMPOSE_GATEWAY_IP`、
`OM_POSTGRES_IP`、`OM_FRONTEND_PROXY_IP`、`OM_BACKEND_IP` 和
`OM_TRUSTED_PROXY_CIDRS`，并确保网关和三个服务地址互不相同且都位于所选网段内。
登录限流会同时按来源地址和数据库中的真实管理员账号计数；不存在的用户名不会占用
账号限流容量。

Docker 前端默认用 Nginx 与后端之间的连接协议覆盖 `X-Forwarded-Proto`。使用 Cloudflare
Tunnel 时，浏览器的 HTTPS 会先在 Tunnel 终止，内网连接通常是 HTTP；此时应将
`NGINX_TRUST_FORWARDED_PROTO=true`，让前端保留 Cloudflare 传入的 `http`/`https` 协议，
同时设置 `NGINX_TRUST_CF_CONNECTING_IP=true`，将 Cloudflare 提供的访客地址转换为
后端使用的通用代理头；其他情况下前端 Nginx 会覆盖客户端提供的 XFF 链。启用这两个
开关时，前端端口必须只允许 `cloudflared` 访问。
纯 HTTPS 入口还应将
`OM_SECURE_COOKIES=true` 作为无协议头时的安全回退。

Agent `0.1.19` 起通过 `Authorization` 请求头认证 WebSocket，实例密钥不再出现在
URL。`OM_ALLOW_LEGACY_AGENT_WS_AUTH` 默认必须保持 `false`；它只用于升级旧 Agent
的短暂维护窗口，启用期间后端会接受旧查询串认证并输出弃用警告。迁移完成后应立即
关闭并重建后端。

`OM_UPDATE_SIGNING_KEY_FILE` 指向只允许文件所有者读取的 Base64 编码 32 字节 Ed25519
私钥。设置后，后端会对 HTTP 清单和 WebSocket 推送中的更新元数据签名，并在启动日志
中输出可嵌入 Agent 的公钥。`OM_UPDATE_SIGNING_KEY_ID` 必须与构建 Agent 时的
`OM_UPDATE_PUBLIC_KEY_ID` 一致。未配置签名器时，旧 Agent 和未嵌入公钥的 HTTPS
Agent 仍可更新，但嵌入公钥的 Agent 会拒绝所有未签名更新。服务端同时生成兼容旧
Agent 的 v1 签名和绑定任务 ID、实例 ID、发布 ID、产物 ID、下载路径及重试代次的 v2
签名。

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
OM_REMOTE_DESKTOP_CONSENT=required
OM_WINDOWS_VIRTUAL_DEVICES=auto
```

`OM_REMOTE_DESKTOP_CONSENT` 可设为 `required`（默认）或 `unattended`。无人值守模式是
Windows 机器级安全授权，仅受管 LocalSystem 服务会实际启用；非交互安装还必须显式传入
`--yes`。`OM_WINDOWS_VIRTUAL_DEVICES` 可设为 `auto`（Windows 默认）或 `disabled`；普通
开发构建没有 Microsoft 签名 bundle 时会明确降级为 physical-only，并且不会修改设备。

实例端日志默认在单个文件达到 10 MiB 时滚动，保留 `agent.log.1` 至
`agent.log.3` 三个历史文件，超过保留数量的旧日志会直接删除。updater 日志使用相同的
大小和保留策略。将 `OM_AGENT_LOG_HISTORY` 设为 `0` 可在滚动时直接丢弃旧日志。

同一状态目录只允许一个实例端进程运行。若要在一台机器上运行多个实例端，请为每个进程设置不同的 `OM_AGENT_STATE_DIR`、`OM_AGENT_ID_FILE` 和 `OM_AGENT_UPDATE_DIR`。更新目录保存可执行文件、回滚基线、状态和 updater 日志，不能放在重启后会清空的临时目录中。OpenWrt standalone 安装默认使用 `/var/lib/om-agent/updates`。

以 root、LocalSystem 或提升权限的管理员身份运行 Agent 时，上述自定义路径及
`OM_AGENT_LOG_FILE` 的所有祖先必须由特权账户拥有，且不能授予普通用户写入、删除或
修改 ACL 的权限。身份和 updater 文件不能是符号链接或 Windows 重解析点；不满足条件时
Agent 会拒绝启动或更新。不要将这些路径放在 `/tmp`、普通用户主目录、共享目录或可由
普通用户替换子项的 Windows 目录中。

## 验证命令

```bash
cd front-end && pnpm test && pnpm build
cd backend && cargo fmt --check && cargo test && cargo check
cd instanceEnd && cargo fmt --check && cargo test && cargo check
```

## 告警与事件闭环

管理员登录后可从“告警中心”创建节点离线、CPU、内存、聚合磁盘占用率、通信延迟和
实例即将到期规则。规则可作用于全部节点或指定节点，并使用 `warning`、`critical` 两个
严重级别。资源和离线规则的持续时间按后端接收观察的时间计算；实例到期规则使用非负整数天
作为阈值，进入提醒周期后立即触发，不使用持续时间。无效或缺失指标不会触发资源告警，也不会
被当作恢复。节点在线状态仍以授权 Agent WebSocket 为准，旧 `/api/agent/report` 只参与资源
阈值判断，不会使节点进入在线状态。

同一规则和节点在恢复前只保留一个事件。事件从“告警中”进入“已确认”，条件恢复后
自动进入“已恢复”；确认人、备注、状态时间线、规则与节点快照均会保留。一次性维护
窗口可覆盖全局、指定规则或指定节点，窗口内继续记录事件但静默首报。节点离线告警
生效时也会抑制同节点的资源首报；维护结束或节点重新上线后，新的有效观察确认异常仍
存在时会补发首报。告警及投递记录默认保留 180 天，活动事件不会被自动清理。

每条规则可以选择零到多个通知渠道，当前支持：

- 通用 Webhook：发送版本化 JSON，可配置自定义请求头和 HMAC-SHA256 密钥。
- SMTP 邮件：使用固定的中文主题和纯文本正文，支持多个收件人。
- 飞书自定义机器人：发送文本消息，并支持机器人“签名校验”密钥。
- 企业微信群机器人：发送文本消息到群机器人 Webhook。
- 钉钉自定义机器人：发送文本消息，支持机器人加签（毫秒 `timestamp`、`sign` 查询参数）。
- Slack Incoming Webhook：发送纯文本消息。
- Microsoft Teams Workflow Webhook：发送 MessageCard 文本卡片。
- Telegram Bot API：发送文本消息，需要填写 Bot API `sendMessage` URL 和 Chat ID。
- Discord Webhook：发送文本消息，并禁用 `@everyone` 等隐式提及。

统一渠道管理接口位于 `/api/admin/alerts/channels`；原有
`/api/admin/alerts/webhook-channels` 继续保留，并且只操作通用 Webhook，旧客户端无需
同步升级。

所有渠道共用 `alert.firing`、`alert.acknowledged`、`alert.resolved` 和
`webhook.test` 四类生命周期、持久化投递队列、失败重试及尝试历史。通用 Webhook
的事件负载包含规则与节点的触发时快照、观察值、阈值、持续时间、操作者及恢复原因，
例如：

```json
{
  "version": 1,
  "type": "alert.resolved",
  "event": {
    "id": "...",
    "status": "resolved",
    "metric": "cpu_percent",
    "current_value": 42.1,
    "threshold": 90.0,
    "duration_seconds": 300,
    "resolution_reason": "condition_recovered"
  },
  "rule": { "id": "...", "name": "CPU 使用率过高", "version": 1 },
  "node": { "id": "...", "name": "核心节点", "hostname": "core-01" },
  "actor": "system",
  "note": "condition_recovered"
}
```

通用 Webhook 配置签名密钥后，请求包含：

```text
X-OM-Timestamp: <Unix 秒>
X-OM-Delivery-ID: <稳定投递 ID>
X-OM-Signature: sha256=<HMAC-SHA256(secret, timestamp + "." + raw_body)>
```

SMTP 仅支持必须升级到 TLS 的 STARTTLS 和隐式 TLS（SMTPS），不提供明文或跳过证书
校验模式。SMTP 密码、Webhook URL、签名密钥、自定义请求头以及邮件配置均使用管理员
认证密钥加密保存；API 不返回密码、签名密钥、完整机器人 token 或请求头值。飞书、企业
微信、钉钉和 Telegram 投递会同时校验 HTTP 状态与平台业务响应，避免将 HTTP 200 的拒绝
响应记为成功；通用 Webhook、Slack、Microsoft Teams 和 Discord 按 HTTP 2xx 记为成功。

HTTP 通知禁用重定向并限制总超时，失败后按固定退避重试；持久化 worker 使用带
fencing 令牌的租约认领，同一事件和渠道的首报、确认、恢复严格串行，进程恢复后不会让
过期 worker 覆盖新投递结果。所有尝试均可在告警中心查询。全部 HTTP 通知渠道都允许
内网 HTTP/HTTPS 地址，这意味着告警管理员拥有后端出站访问能力，应只向可信目标投递，
并避免在响应中返回敏感信息。该功能不改变 Agent 通信协议，不需要为了启用告警升级
实例端。

## 后续增强

- 增加周期维护窗口、节点分组以及短信、电话等通知渠道。
- 增加角色划分和更细粒度的管理员权限。
- 增加 CI 中的多平台 standalone 可执行文件构建、签名和发布流水线。
