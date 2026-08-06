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

默认 PostgreSQL 只在 Compose 网络内开放。需要连接已有或托管 PostgreSQL 时，改用 `docker-compose.yml`，显式设置 `OM_DATABASE_URL` 和 `OM_DATABASE_PASSWORD`，并确保 URL 中的主机名能从后端容器访问。后端服务版本只支持向前升级，不提供版本降级或回滚操作；完整的数据库模式、HTTPS、持久化、备份、升级及故障处理步骤见[Docker Compose 部署指南](../docs/deployment.md)。

## 源码开发

本地开发需要先准备可访问的 PostgreSQL：

```bash
OM_DATABASE_URL='postgresql://operation_monitoring@127.0.0.1:5432/operation_monitoring' \
OM_DATABASE_PASSWORD='<数据库密码>' \
OM_ADMIN_PASSWORD='development-bootstrap-password' \
OM_TRUST_PROXY_HEADERS=false \
OM_TRUSTED_PROXY_CIDRS='127.0.0.1/32,::1/128' \
OM_ALLOW_LEGACY_AGENT_WS_AUTH=false \
cargo run
```

接口默认监听 `0.0.0.0:13500`，健康检查地址为 `http://127.0.0.1:13500/api/health`。
只有后端无法被客户端绕过反向代理直接访问时，才可设置
`OM_TRUST_PROXY_HEADERS=true`。连接来源还必须命中 `OM_TRUSTED_PROXY_CIDRS` 中
逗号分隔的 CIDR，后端才会使用代理提供的 `X-Forwarded-For` 或 `X-Real-IP` 进行
登录限流；多级代理链会从右向左剥离可信代理。
`OM_ALLOW_LEGACY_AGENT_WS_AUTH` 仅可在将旧 Agent 升级到 `0.1.19` 的维护窗口临时
启用，平时必须保持关闭，避免实例密钥重新进入 URL 和代理日志。

提交后端变更前运行：

```bash
cargo fmt --check
cargo test
cargo check
```

需要执行依赖 PostgreSQL 的 ignored 测试时，使用专用空数据库并统一通过
`OM_TEST_DATABASE_URL` 指定连接地址：

```bash
OM_TEST_DATABASE_URL=postgresql://localhost/operation_monitoring_test \
  cargo test -- --ignored --test-threads=1
```
