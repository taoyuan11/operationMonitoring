# Operation Monitoring Frontend

Vue 3、TypeScript 与 Vite 实现的 Operation Monitoring 控制台。公开视图展示实例状态和生命周期，管理员视图提供接入审核、快捷命令、程序更新、告警、用户、审计和设置页面，以及终端、文件、Docker 和 Windows 远程桌面操作。生产镜像使用 Nginx 提供静态文件，并将 `/api`、`/uploads` 和 WebSocket 请求转发到后端服务。

## 默认部署方式

前端、后端与 PostgreSQL 默认通过仓库根目录的 `docker-compose.with-db.yml` 一起部署：

```bash
cd ..
if [ ! -f .env ]; then cp .env.example .env; fi
# 编辑 .env，至少替换数据库密码和管理员初始化密码
docker compose -f docker-compose.with-db.yml up -d --build
docker compose -f docker-compose.with-db.yml ps
```

默认前端地址为 `http://127.0.0.1:13501`。外部数据库模式改用 `docker-compose.yml`，并显式设置 `OM_DATABASE_URL` 和 `OM_DATABASE_PASSWORD`。端口、HTTPS 反向代理、上传限制、升级与排障说明见[Docker Compose 部署指南](../docs/deployment.md)。

## 源码开发

源码模式仅用于开发，Vite 会将 API 与 WebSocket 请求代理到 `http://127.0.0.1:13500`：

```bash
pnpm install
pnpm dev
```

开发和预览服务器仅监听 `127.0.0.1`；开发模式默认访问 `http://127.0.0.1:5173`。跨主机调试应使用受控反向代理，不要将 Vite 开发服务器直接暴露到公网。

实例编辑弹窗支持设置或清除到期时间；实例卡片会显示绝对时间、剩余时间或已到期时长。
快捷命令使用独立的 `#/commands` 管理页面，命令执行入口仍位于实例操作面板。

提交前运行 Node 回归测试、TypeScript 类型检查和生产构建：

```bash
pnpm test
pnpm build
```

`pnpm test` 执行 `tests/*.test.mjs`，当前覆盖告警筛选与通知渠道请求、实例到期格式化、
Agent 产物匹配和终端消息处理。涉及布局或交互的改动还应在窄屏和宽屏视口手动检查。
