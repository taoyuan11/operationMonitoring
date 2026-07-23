# Operation Monitoring Frontend

Vue 3、TypeScript 与 Vite 实现的 Operation Monitoring 管理控制台。生产镜像使用 Nginx 提供静态文件，并将 `/api`、`/uploads` 和 WebSocket 请求转发到后端服务。

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

生产构建与类型检查：

```bash
pnpm build
```
