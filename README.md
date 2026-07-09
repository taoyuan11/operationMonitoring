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
  http.rs         注册与指标上报
  identity.rs     本地实例身份生成和读取
  metrics.rs      CPU、内存、磁盘、网络采集
  models.rs       Agent 与后端通信模型
  profile.rs      主机基础信息
  time.rs         时间戳工具
  ws.rs           Agent 持久连接和命令回传
```

## 本地启动

启动后端：

```bash
cd backend
OM_ADMIN_USER=admin OM_ADMIN_PASSWORD=admin123 cargo run
```

启动前端：

```bash
cd front-end
pnpm install
pnpm dev
```

启动实例端：

```bash
cd instanceEnd
cargo run -- --server http://127.0.0.1:13500
```

前端开发服务器已在 `front-end/vite.config.ts` 中代理 `/api` 到 `http://127.0.0.1:13500`，WebSocket 也会透传。

## 常用环境变量

后端：

```bash
OM_BIND=127.0.0.1:13500
OM_DATABASE_URL=sqlite://operation-monitoring.db
OM_ADMIN_USER=admin
OM_ADMIN_PASSWORD=admin123
```

实例端：

```bash
OM_SERVER=http://127.0.0.1:13500
OM_AGENT_ID_FILE=/path/to/identity.json
OM_REPORT_INTERVAL=5
```

## 验证命令

```bash
cd front-end && pnpm build
cd backend && cargo check
cd instanceEnd && cargo check
```

## 后续增强

- 增加历史趋势图和指标明细页。
- 增加告警规则、通知渠道和阈值配置。
- 增加真实交互式 PTY，替代当前命令式 Web 终端。
- 增加登录失败限制、密码哈希和更细粒度权限。
- 增加 Agent 安装脚本、服务注册和自动升级。
