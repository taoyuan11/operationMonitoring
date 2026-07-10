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
  auth.rs         管理员 session 校验
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
OM_ADMIN_PASSWORD=admin123 cargo run
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
./target/release/instanceEnd start --server http://127.0.0.1:13500
```

`start` 会在后台启动实例端并立即释放命令行，标准输出和错误输出会写入命令返回的日志路径。Windows 使用同目录下的 `instanceEnd.exe`，后台子进程不会创建控制台窗口。

查询状态或停止实例端：

```bash
./target/release/instanceEnd status
./target/release/instanceEnd stop
```

需要在前台运行实例端并直接向终端打印日志时，使用 `log`：

```bash
./target/release/instanceEnd log --server http://127.0.0.1:13500
```

实例端不提供 `run` 命令。`log` 会持续运行，按 `Ctrl+C` 退出；同一状态目录下不能同时运行前台和后台实例端。

开发时也可以通过 Cargo 执行相同命令：

```bash
cargo run -- start --server http://127.0.0.1:13500
cargo run -- status
cargo run -- log
cargo run -- stop
```

前端开发服务器已在 `front-end/vite.config.ts` 中代理 `/api` 到 `http://127.0.0.1:13500`，WebSocket 也会透传。

## 连接与终端说明

- 实例审批完成后，指标、快捷命令和交互式终端都复用同一条 Agent WebSocket 长连接。
- 后端以内存中的 WebSocket 连接状态判断实例在线；连接关闭或心跳超时后立即判定离线，不再依赖“最后上报时间 + 固定阈值”。
- Web 终端使用系统 PTY/ConPTY，支持持续 Shell 上下文、`cd`、环境变量、交互程序、方向键、Tab、`Ctrl+C` 和窗口尺寸变化。
- 浏览器与 Agent 之间的终端数据按原始字节进行 Base64 封装，Shell 统一按 UTF-8 工作；Windows `cmd.exe` 会切换到代码页 65001，避免中文经 JSON 转发时损坏。

## 常用环境变量

后端：

```bash
OM_BIND=127.0.0.1:13500
OM_DATABASE_URL=sqlite://db/operation-monitoring.db
OM_ADMIN_PASSWORD=admin123
```

未设置 `OM_DATABASE_URL` 时，后端会在启动进程的当前工作目录下自动创建
`db/operation-monitoring.db`；SQLite 产生的 WAL、SHM 等运行时文件也位于该目录，
不会作为项目文件提交。

实例端：

```bash
OM_SERVER=http://127.0.0.1:13500
OM_AGENT_ID_FILE=/path/to/identity.json
OM_REPORT_INTERVAL=5
OM_AGENT_STATE_DIR=/path/to/runtime
OM_AGENT_LOG_FILE=/path/to/agent.log
```

同一状态目录只允许一个实例端进程运行。若要在一台机器上运行多个实例端，请为每个进程设置不同的 `OM_AGENT_STATE_DIR` 和 `OM_AGENT_ID_FILE`。

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
- 增加 Agent 安装脚本、服务注册和自动升级。
