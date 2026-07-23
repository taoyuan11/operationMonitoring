# Docker Compose 部署指南

本文是 Operation Monitoring 的默认部署文档。默认使用 `docker-compose.with-db.yml`，项目名固定为 `operation-monitoring`，会创建 PostgreSQL 并构建后端 API 和前端 Nginx。仓库同时保留 `docker-compose.yml`，用于连接已有或托管的外部 PostgreSQL。

## 1. 部署架构

```text
浏览器 / Agent
      |
      |  HTTP(S)、WebSocket
      v
frontend (Nginx :80)
      |  /api、/uploads、WebSocket
      v
backend (Axum :13500) ---- postgres（Compose 服务）
      |
      +-- postgres-data    PostgreSQL 数据库
      +-- backend-db       认证密钥
      +-- backend-uploads   背景图片和上传资源
      +-- backend-updates   Agent 更新包
```

前端容器会将 `/api/`、`/uploads/` 和 WebSocket 请求转发到 Compose 网络中的 `backend:13500`，浏览器通常只需要访问前端地址。默认 PostgreSQL 服务名为 `postgres`，只加入 Compose 内网，不发布宿主机端口。后端宿主机端口仍默认发布为 `13500`，生产环境可以只绑定到回环地址或在防火墙中禁止公网访问。

如果使用 `docker-compose.yml` 外部数据库模式，后端会改为连接 `OM_DATABASE_URL` 指定的地址，其他前端、后端和文件卷配置保持不变。

Compose 项目名设置为 `operation-monitoring` 后，默认资源名称为：

| 资源 | 名称 |
| --- | --- |
| 服务 | `postgres`、`backend`、`frontend` |
| 网络 | `operation-monitoring_default` |
| PostgreSQL 数据卷 | `operation-monitoring_postgres-data` |
| 认证密钥卷 | `operation-monitoring_backend-db` |
| 上传资源卷 | `operation-monitoring_backend-uploads` |
| Agent 更新卷 | `operation-monitoring_backend-updates` |

不要依赖容器 ID 或临时容器文件保存数据。对自带数据库模式使用 `docker compose -f docker-compose.with-db.yml down` 不会删除上述卷；追加 `--volumes` 会删除它们。

## 2. 前置条件

在部署主机准备：

1. Docker Engine 和支持 Compose Specification 的 Docker Compose v2。
2. Git（如果从源码仓库部署）。
3. 足够的磁盘空间保存 PostgreSQL 数据和 Agent 更新包。
4. 对外提供前端访问的端口。默认是 `13501`；后端 API 默认是 `13500`。

默认自带数据库模式不需要预先安装 PostgreSQL。只有使用外部数据库模式时，数据库地址才不能写成容器内的 `127.0.0.1` 或 `localhost`，而应使用数据库 DNS 名称、私网 IP 或托管服务提供的地址。

### 外部 PostgreSQL 模式

`docker-compose.yml` 不创建数据库容器，适合使用已有或托管 PostgreSQL。该文件要求同时显式设置 `OM_DATABASE_URL` 和 `OM_DATABASE_PASSWORD`，缺少任意一项时 Compose 会在启动前报错。使用该模式前，数据库防火墙必须允许部署主机的出口地址，并正确配置 PostgreSQL 的 `pg_hba.conf`（自建实例）。

准备一个专用数据库和登录角色，并将数据库所有权交给该角色。以下 SQL 在 PostgreSQL 管理连接中执行，密码请替换为随机长密码：

```sql
CREATE ROLE operation_monitoring LOGIN PASSWORD 'replace-with-a-database-password';
CREATE DATABASE operation_monitoring OWNER operation_monitoring;
```

如果数据库已经存在，确认应用角色至少可以连接该数据库、在目标 schema 中建表和创建索引。后端首次启动会创建表和索引；如果数据库不存在，后端还会尝试连接维护库 `postgres` 并使用 `CREATEDB` 权限创建它。托管 PostgreSQL 通常不允许此操作，推荐提前执行上面的建库步骤。

将外部数据库连接写入 `.env` 后，使用基础 Compose 文件启动：

```bash
docker compose -f docker-compose.yml config --quiet
docker compose -f docker-compose.yml up -d --build
```

两种 Compose 文件使用同一个项目名，但不会自动迁移数据库。切换模式前先用当前文件执行 `down`，通过 `pg_dump`/`pg_restore` 迁移业务数据，再用目标文件启动；不要同时运行两套文件。

## 3. 配置环境变量

在仓库根目录执行：

```bash
if [ ! -f .env ]; then cp .env.example .env; fi
chmod 600 .env
```

默认自带数据库模式至少修改以下两个值：

```dotenv
OM_DATABASE_PASSWORD=replace-with-database-password
OM_ADMIN_PASSWORD=replace-with-a-long-random-bootstrap-password
```

`docker-compose.with-db.yml` 会把 `OM_DATABASE_PASSWORD` 同时作为 PostgreSQL 容器初始化密码和后端连接密码，并将后端连接 URL 设置为 Compose 内网中的 `postgres` 服务。`POSTGRES_DB`、`POSTGRES_USER` 和 `POSTGRES_IMAGE` 可以保留 `.env.example` 中的默认值。

PostgreSQL 官方镜像只在空数据卷首次启动时应用数据库名、用户和密码。数据库卷已经初始化后，直接修改 `POSTGRES_DB`、`POSTGRES_USER` 或 `OM_DATABASE_PASSWORD` 不会修改现有角色；密码轮换应先在 PostgreSQL 中执行 `ALTER ROLE`，再同步更新 `.env`。不要通过删除卷来应用新密码，除非已经备份并确认可以丢弃现有数据库。

使用外部 PostgreSQL 模式时，改为设置下面的连接 URL；密码仍通过 `OM_DATABASE_PASSWORD` 单独注入，不必写入 URL：

```dotenv
OM_DATABASE_URL=postgresql://operation_monitoring@db.example.com:5432/operation_monitoring?sslmode=require
OM_DATABASE_PASSWORD=replace-with-database-password
OM_ADMIN_PASSWORD=replace-with-a-long-random-bootstrap-password
```

如果用户名、数据库名或其他 URL 部分包含特殊字符，应按 PostgreSQL URL 规则进行编码。内网开发且数据库没有 TLS 时，可以去掉 `?sslmode=require`，生产环境优先使用数据库服务商要求的 TLS 参数。

`.env` 已被 `.gitignore` 忽略，不要提交、复制到镜像或贴入日志。部署账号应限制该文件权限，并使用 Secret 管理系统注入数据库和管理员密码。

### Compose 变量参考

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `OM_DATABASE_URL` | 外部模式必填 | 仅由 `docker-compose.yml` 外部数据库模式读取；自带数据库文件固定使用 `postgres` 服务地址。 |
| `OM_DATABASE_PASSWORD` | 两种模式均必填 | 自带模式用于初始化数据库；外部模式用于后端连接认证。 |
| `OM_ADMIN_PASSWORD` | `admin123` | 仅在管理员表为空时用于一次性初始化；生产首次启动前必须替换。 |
| `POSTGRES_IMAGE` | `postgres:16-alpine` | 自带数据库镜像。不要在已有数据卷上直接跨大版本升级。 |
| `POSTGRES_DB` | `operation_monitoring` | 自带数据库首次初始化的数据库名。 |
| `POSTGRES_USER` | `operation_monitoring` | 自带数据库首次初始化的用户。 |
| `OM_SECURE_COOKIES` | `false` | HTTPS/WSS 对外服务必须设为 `true`；本地 HTTP 调试保持 `false`。 |
| `FRONTEND_PORT` | `13501` | 宿主机到前端容器 80 端口的映射。可写成 `127.0.0.1:13501`。 |
| `BACKEND_PORT` | `13500` | 宿主机到后端容器 13500 端口的映射。生产可写成 `127.0.0.1:13500`。 |
| `OM_AGENT_PACKAGE_MAX_BYTES` | `268435456` | 单个 Agent 更新包上限，默认 256 MiB。 |
| `OM_FILE_TRANSFER_MAX_BYTES` | `1073741824` | 单个实例文件传输上限，默认 1 GiB。 |
| `NGINX_CLIENT_MAX_BODY_SIZE` | `1g` | 前端 Nginx 请求体上限，必须不小于两个后端文件限制中的较大值。 |
| `RUST_LOG` | `backend=info,tower_http=info` | 后端日志级别。 |

`OM_BIND`、`OM_UPLOAD_DIR`、`OM_UPDATE_DIR` 和认证密钥文件路径由 Compose 在容器内固定设置，除非同步修改 Compose 和卷映射，否则不要在 `.env` 中覆盖。

## 4. 首次启动

从仓库根目录验证自带数据库 Compose 配置并启动：

```bash
docker compose -f docker-compose.with-db.yml config --quiet
docker compose -f docker-compose.with-db.yml up -d --build
docker compose -f docker-compose.with-db.yml ps
```

除“外部 PostgreSQL 模式”小节外，本文后续 Compose 命令均针对默认的 `docker-compose.with-db.yml`。外部数据库部署执行相同操作时，将文件名替换为 `docker-compose.yml`，数据库备份和恢复则使用外部平台提供的工具。

首次构建会下载 PostgreSQL、Rust、Node 和 Nginx 基础镜像，耗时取决于网络。PostgreSQL 健康后后端才会启动，后端健康后前端才会启动。确认三个服务均正常后，访问：

- 前端控制台：`http://服务器地址:13501`
- 后端健康检查：`http://服务器地址:13500/api/health`

首次登录使用 `.env` 中的 `OM_ADMIN_PASSWORD`。系统会要求创建用户名、使用 Authenticator 扫描二维码并确认 6 位 TOTP；完成后密码初始化入口关闭，后续使用用户名和 TOTP 登录。管理员可以在用户管理页面添加多个用户和认证设备。

常用状态和日志命令：

```bash
docker compose -f docker-compose.with-db.yml ps
docker compose -f docker-compose.with-db.yml logs --tail=200 postgres
docker compose -f docker-compose.with-db.yml logs --tail=200 backend
docker compose -f docker-compose.with-db.yml logs --tail=200 frontend
docker compose -f docker-compose.with-db.yml logs -f
```

## 5. 对外发布 HTTPS

生产环境应在独立的 TLS 终止层（云负载均衡、Caddy、Nginx 或 Traefik）后发布前端，只将前端端口暴露给该代理。设置：

```dotenv
FRONTEND_PORT=127.0.0.1:13501
BACKEND_PORT=127.0.0.1:13500
OM_SECURE_COOKIES=true
```

反向代理必须把 HTTP、上传请求和 WebSocket 一起转发到 `http://127.0.0.1:13501`，保留原始 `Host`，并传递 `Upgrade`/`Connection` 标头。下面是 Nginx 主机配置的最小示例；证书路径和域名按实际环境替换：

```nginx
map $http_upgrade $connection_upgrade {
    default upgrade;
    '' close;
}

server {
    listen 443 ssl;
    server_name monitor.example.com;

    ssl_certificate /etc/letsencrypt/live/monitor.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/monitor.example.com/privkey.pem;
    client_max_body_size 1g;

    location / {
        proxy_pass http://127.0.0.1:13501;
        proxy_http_version 1.1;
        proxy_set_header Host $http_host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto https;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_request_buffering off;
        proxy_buffering off;
        proxy_read_timeout 604800s;
        proxy_send_timeout 604800s;
    }
}
```

外部代理的 `client_max_body_size` 必须不小于 `NGINX_CLIENT_MAX_BODY_SIZE`。前端容器内部已经为 API 和上传关闭缓冲并配置长 WebSocket 超时，但外部代理仍必须保留这些设置。HTTPS 和 Cookie 配置修改后重新创建后端与前端容器：

```bash
docker compose -f docker-compose.with-db.yml up -d --force-recreate backend frontend
```

Agent 的 `OM_SERVER` 使用完整的外部地址，例如 `https://monitor.example.com`，不要追加 `/api`。

## 6. 持久化和密钥

默认自带数据库模式的业务表保存在 PostgreSQL 数据卷中；外部数据库模式的业务表由外部 PostgreSQL 负责持久化。Compose 卷保存以下内容：

- `postgres-data`：自带 PostgreSQL 的完整数据目录；仅 `docker-compose.with-db.yml` 使用。
- `backend-db`：默认保存 `/app/db/auth-secret.key`。这是加密 Authenticator 密钥所需的主密钥，不是业务数据库。
- `backend-uploads`：背景图片及上传资源。
- `backend-updates`：Agent 更新包。

当前 Compose 使用 `OM_AUTH_KEY_FILE`，第一次启动会在 `backend-db` 卷中生成密钥。数据库备份必须同时备份该卷；只恢复 PostgreSQL 而丢失密钥，已有 Authenticator 将无法解密。

后端也支持通过 `OM_AUTH_SECRET_KEY` 注入 Base64 编码的 32 字节主密钥，但当前 Compose 默认不映射该可选变量。若要改用外部 Secret，先在 `backend.environment` 中显式映射非空的 `OM_AUTH_SECRET_KEY`，再生成密钥：

```bash
openssl rand -base64 32
```

将输出作为 `OM_AUTH_SECRET_KEY` 写入 Secret 管理系统，并确保后续每次启动使用同一个值。密钥一旦丢失不能从数据库内容推导出来。

查看实际卷名：

```bash
docker volume ls --filter label=com.docker.compose.project=operation-monitoring
```

## 7. 备份和恢复

备份前先停止后端，避免数据库和文件卷处于不一致状态；前端可以继续运行但会暂时无法访问 API：

```bash
docker compose -f docker-compose.with-db.yml stop backend
mkdir -p backups
```

自带数据库模式直接在 PostgreSQL 容器内执行 `pg_dump`：

```bash
docker compose -f docker-compose.with-db.yml exec -T postgres \
  sh -c 'pg_dump --username="$POSTGRES_USER" --dbname="$POSTGRES_DB" --format=custom' \
  > backups/operation_monitoring-$(date +%Y%m%d-%H%M%S).dump
```

不要在 PostgreSQL 运行时直接打包 `postgres-data` 卷作为唯一备份；优先使用上面的逻辑备份或存储平台提供的一致性快照。外部数据库模式使用数据库平台备份机制或主机上的 `pg_dump`。例如，已通过 `.pgpass` 配置凭据时（连接地址替换为实际值）：

```bash
pg_dump --format=custom \
  --file=backups/operation_monitoring-$(date +%Y%m%d-%H%M%S).dump \
  'postgresql://operation_monitoring@db.example.com:5432/operation_monitoring?sslmode=require'
```

将数据库以外的三个卷打包到受保护的备份目录（下面的命名与本 Compose 项目名一致）：

```bash
docker run --rm \
  -v operation-monitoring_backend-db:/data:ro \
  -v "$PWD/backups:/backup" \
  alpine:3.22 tar -czf /backup/backend-db-$(date +%Y%m%d-%H%M%S).tar.gz -C /data .

docker run --rm \
  -v operation-monitoring_backend-uploads:/data:ro \
  -v "$PWD/backups:/backup" \
  alpine:3.22 tar -czf /backup/backend-uploads-$(date +%Y%m%d-%H%M%S).tar.gz -C /data .

docker run --rm \
  -v operation-monitoring_backend-updates:/data:ro \
  -v "$PWD/backups:/backup" \
  alpine:3.22 tar -czf /backup/backend-updates-$(date +%Y%m%d-%H%M%S).tar.gz -C /data .
```

确认备份文件已复制到独立存储后启动服务：

```bash
docker compose -f docker-compose.with-db.yml start backend
docker compose -f docker-compose.with-db.yml ps
```

恢复时先停止后端，使用 `pg_restore` 将逻辑备份恢复到已清空的目标数据库，再将文件卷内容恢复到同名卷。卷恢复属于覆盖操作，务必先保留当前卷快照，并在维护窗口验证数据库和认证密钥来自同一备份时间点。恢复完成后执行 `docker compose -f docker-compose.with-db.yml up -d` 并访问健康检查。

## 8. 升级和回滚

升级前备份 PostgreSQL 与三个卷，然后在仓库根目录执行：

```bash
git pull --ff-only
docker compose -f docker-compose.with-db.yml pull postgres
docker compose -f docker-compose.with-db.yml build --pull
docker compose -f docker-compose.with-db.yml up -d --remove-orphans
docker compose -f docker-compose.with-db.yml ps
```

外部数据库模式跳过 `pull postgres`。后端启动时会执行所需的表结构补齐。升级后检查健康接口、管理员登录、Agent WebSocket、文件上传和更新包下载。回滚代码前应确认新版本没有不可逆的数据结构变化；必要时先恢复升级前的 PostgreSQL 备份，再切回旧代码并重新构建镜像。

## 9. 管理员认证恢复

如果唯一管理员丢失所有 Authenticator，必须先停止正常后端，再使用同一 Compose 配置执行显式重置：

```bash
docker compose -f docker-compose.with-db.yml stop backend
docker compose -f docker-compose.with-db.yml run --rm backend \
  --reset-admin-auth \
  --confirm-reset-admin-auth RESET-ADMIN-AUTH
docker compose -f docker-compose.with-db.yml up -d backend
```

该命令会删除管理员和认证设备，但不会删除业务表、操作日志、上传资源或 Agent 更新包。重置后下一次登录会重新开放一次性密码初始化；完成绑定后立即删除命令输出和临时凭据。

## 10. 修改上传限制

默认 Agent 包上限为 256 MiB，实例文件传输上限为 1 GiB。提高任一后端限制时，同时提高前端容器和外部 TLS 代理的请求体上限。例如：

```dotenv
OM_AGENT_PACKAGE_MAX_BYTES=536870912
OM_FILE_TRANSFER_MAX_BYTES=1073741824
NGINX_CLIENT_MAX_BODY_SIZE=1100m
```

应用修改：

```bash
docker compose -f docker-compose.with-db.yml up -d --build --force-recreate backend frontend
```

如果只修改 `NGINX_CLIENT_MAX_BODY_SIZE`，也需要重新创建前端容器，以便 Nginx 模板重新渲染。

## 11. 常见问题

### 后端一直 unhealthy

先看日志：

```bash
docker compose -f docker-compose.with-db.yml logs --tail=200 backend
```

自带数据库模式先检查 PostgreSQL 日志和健康状态：

```bash
docker compose -f docker-compose.with-db.yml ps postgres
docker compose -f docker-compose.with-db.yml logs --tail=200 postgres
```

如果数据卷已经存在，确认 `.env` 中的数据库名、用户和密码与首次初始化时一致。外部数据库模式则检查 `OM_DATABASE_URL` 是否使用容器可达的主机名、数据库防火墙和 `pg_hba.conf` 是否允许连接、TLS 参数是否匹配，以及数据库账号是否有建表权限。容器内的 `127.0.0.1` 不是数据库宿主机。

### 前端返回 502 或无法启动

确认后端健康后再看前端日志：

```bash
docker compose -f docker-compose.with-db.yml ps
docker compose -f docker-compose.with-db.yml logs --tail=200 frontend
```

前端依赖 `backend` 的健康检查；后端未完成数据库初始化时，Compose 不会启动前端。

### 上传返回 413

按“外部代理 >= 前端 Nginx >= 后端限制”的顺序检查 `client_max_body_size`、`NGINX_CLIENT_MAX_BODY_SIZE`、`OM_AGENT_PACKAGE_MAX_BYTES` 和 `OM_FILE_TRANSFER_MAX_BYTES`。

### WebSocket 或远程桌面断开

确认外部代理传递 `Upgrade` 和 `Connection` 标头、保留 `Host`，并将读写超时设置为至少数小时。远程桌面和终端生产环境必须使用 HTTPS/WSS；浏览器通过 HTTP 访问时不要启用 `OM_SECURE_COOKIES=true`。

### 端口已被占用

在 `.env` 中修改 `FRONTEND_PORT` 或 `BACKEND_PORT`，例如 `FRONTEND_PORT=8080`，然后执行：

```bash
docker compose -f docker-compose.with-db.yml up -d
```

如果只希望本机反向代理访问，使用 `FRONTEND_PORT=127.0.0.1:13501` 和 `BACKEND_PORT=127.0.0.1:13500`。

## 12. 停止和卸载

暂停服务但保留数据：

```bash
docker compose -f docker-compose.with-db.yml stop
```

删除容器、网络和默认资源但保留卷：

```bash
docker compose -f docker-compose.with-db.yml down
```

只有在已经完成 PostgreSQL 和卷备份、并确认不再需要本地数据时才使用：

```bash
docker compose -f docker-compose.with-db.yml down --volumes
```

该命令会永久删除自带 PostgreSQL 数据、认证密钥、上传资源和更新包卷。使用外部数据库模式时，外部 PostgreSQL 不会由 Compose 删除。
