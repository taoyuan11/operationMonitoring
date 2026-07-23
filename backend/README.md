# Operation Monitoring Backend

Rust、Axum 与 SQLx 实现的 Operation Monitoring API 服务。后端负责实例注册与监控数据、管理员认证、命令任务、文件传输、Agent 更新以及 WebSocket 会话，业务数据存储在 PostgreSQL 中。

## 默认部署方式

后端与前端默认通过仓库根目录的 `docker-compose.with-db.yml` 部署。Compose 会创建 PostgreSQL、构建后端镜像、挂载持久化卷，并等待数据库健康后启动后端。

```bash
cd ..
if [ ! -f .env ]; then cp .env.example .env; fi
# 编辑 .env，至少替换数据库密码和管理员初始化密码
docker compose -f docker-compose.with-db.yml up -d --build
docker compose -f docker-compose.with-db.yml ps
```

默认 PostgreSQL 只在 Compose 网络内开放。需要连接已有或托管 PostgreSQL 时，改用 `docker-compose.yml`，显式设置 `OM_DATABASE_URL` 和 `OM_DATABASE_PASSWORD`，并确保 URL 中的主机名能从后端容器访问。完整的数据库模式、HTTPS、持久化、备份、升级及故障处理步骤见[Docker Compose 部署指南](../docs/deployment.md)。

## 源码开发

本地开发需要先准备可访问的 PostgreSQL：

```bash
OM_DATABASE_URL='postgresql://operation_monitoring@127.0.0.1:5432/operation_monitoring' \
OM_DATABASE_PASSWORD='<数据库密码>' \
OM_ADMIN_PASSWORD='admin123' \
cargo run
```

接口默认监听 `0.0.0.0:13500`，健康检查地址为 `http://127.0.0.1:13500/api/health`。

提交后端变更前运行：

```bash
cargo fmt --check
cargo test
cargo check
```
