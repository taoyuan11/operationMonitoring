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
./deploy.sh deploy docker-compose.yml
```

两种 Compose 文件使用同一个项目名，但不会自动迁移数据库。切换模式前先用当前文件执行 `down`，通过 `pg_dump`/`pg_restore` 迁移业务数据，再用目标文件启动；不要同时运行两套文件。

## 3. 配置环境变量

可以在仓库根目录手动创建环境文件：

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
| `OM_ADMIN_PASSWORD` | 必填，无默认值 | 至少 16 字节，仅在管理员表为空时用于一次性初始化；Compose 启动时始终要求提供。 |
| `POSTGRES_IMAGE` | `postgres:16-alpine` | 自带数据库镜像。不要在已有数据卷上直接跨大版本升级。 |
| `POSTGRES_DB` | `operation_monitoring` | 自带数据库首次初始化的数据库名。 |
| `POSTGRES_USER` | `operation_monitoring` | 自带数据库首次初始化的用户。 |
| `OM_SECURE_COOKIES` | `false` | 未收到可信代理协议头时的 Cookie/Origin 协议回退值；纯 HTTPS 入口设为 `true`。可信代理请求会按 `X-Forwarded-Proto` 动态处理。 |
| `OM_TRUST_PROXY_HEADERS` | Compose 默认为 `true` | 登录限流和请求协议使用代理传入的 `X-Forwarded-For`、`X-Real-IP`、`X-Forwarded-Proto`；仅可在后端无法被绕过代理直连时启用。 |
| `OM_COMPOSE_NETWORK_CIDR` | `172.30.135.0/24` | Compose 专用网络；与现有网络冲突时必须连同网关和三个服务 IP 一起调整。 |
| `OM_COMPOSE_GATEWAY_IP` | `172.30.135.1` | Compose 网络固定网关；宿主机反向代理经此地址连接后端，Compose 会自动将其 `/32` 追加到可信代理列表。 |
| `OM_POSTGRES_IP` | `172.30.135.2` | 自带数据库模式中的 PostgreSQL 固定地址。 |
| `OM_FRONTEND_PROXY_IP` | `172.30.135.3` | 前端代理固定地址，必须包含在 `OM_TRUSTED_PROXY_CIDRS` 中。 |
| `OM_BACKEND_IP` | `172.30.135.4` | 后端固定地址，不得与 PostgreSQL 或前端代理重复。 |
| `OM_TRUSTED_PROXY_CIDRS` | `172.30.135.3/32` | 用户配置的可信代理网段；Compose 还会自动追加 `OM_COMPOSE_GATEWAY_IP/32` 以支持宿主机代理。 |
| `OM_ALLOW_LEGACY_AGENT_WS_AUTH` | `false` | 仅在把旧 Agent 升级到 `0.1.19` 的维护窗口临时接受查询串认证。 |
| `OM_UPDATE_SIGNING_KEY_FILE` | 空，禁用签名 | 容器内 Ed25519 私钥路径；HTTP 自动更新必须配置为 `/app/db/update-signing.key`。 |
| `OM_UPDATE_SIGNING_KEY_ID` | `default` | 更新签名 key ID；必须与构建 Agent 时嵌入的 ID 一致。 |
| `FRONTEND_PORT` | `13501` | 宿主机到前端容器 80 端口的映射。可写成 `127.0.0.1:13501`。 |
| `BACKEND_PORT` | `13500` | 后端仅映射到宿主机 `127.0.0.1`，供本机受控代理或诊断使用；远程 Agent 应连接前端代理地址。 |
| `OM_AGENT_PACKAGE_MAX_BYTES` | `268435456` | 单个 Agent 更新包上限，默认 256 MiB。 |
| `OM_FILE_TRANSFER_MAX_BYTES` | `1073741824` | 单个实例文件传输上限，默认 1 GiB。 |
| `NGINX_CLIENT_MAX_BODY_SIZE` | `1g` | 前端 Nginx 请求体上限，必须不小于两个后端文件限制中的较大值。 |
| `NGINX_TRUST_FORWARDED_PROTO` | `false` | 为 `true` 时仅保留可信入口传入的 `X-Forwarded-Proto: http/https`；前端端口必须只允许 Cloudflare Tunnel 或其他可信代理访问。 |
| `NGINX_TRUST_CF_CONNECTING_IP` | `false` | 为 `true` 时把 Cloudflare 的 `CF-Connecting-IP` 转换为后端使用的客户端地址头；仅可在 Cloudflare Tunnel 是唯一前端入口时启用。 |
| `RUST_LOG` | `backend=info,tower_http=info` | 后端日志级别。 |

`OM_BIND`、`OM_UPLOAD_DIR`、`OM_UPDATE_DIR` 和认证密钥文件路径由 Compose 在容器内固定设置，除非同步修改 Compose 和卷映射，否则不要在 `.env` 中覆盖。

## 4. 首次启动

从仓库根目录验证自带数据库 Compose 配置并启动：

```bash
./deploy.sh deploy docker-compose.with-db.yml
```

如果 `.env` 不存在，脚本会从 `.env.example` 创建该文件、将权限设置为 `600`，然后暂停部署。
至少替换数据库密码和管理员初始化密码后，重新执行同一命令。脚本内部会先运行
`docker compose config --quiet`，再构建、启动并显示服务状态。

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
BACKEND_PORT=13500
OM_SECURE_COOKIES=true
```

`OM_SECURE_COOKIES=true` 是没有可信协议头时的安全回退值，并不会再禁用现有 HTTP
入口。来自可信前端或宿主机代理的请求会按其覆盖写入的 `X-Forwarded-Proto` 分别校验
`http`/`https` Origin，并只为 HTTPS 登录响应增加 `Secure`。因此同一后端可以同时服务
HTTP/WS 和 HTTPS/WSS。两种入口使用不同名称的会话 Cookie，避免同一浏览器中的
HTTPS Cookie 阻止 HTTP 登录 Cookie 写入；HTTP 仍只适合可信网络或本机维护。

反向代理应把 `/api/` 和 `/uploads/` 直接转发到回环地址上的后端，将其他请求转发
到前端。这样后端既能获得代理覆盖写入的真实客户端地址，又不会暴露受信任代理头
接口。WebSocket 必须保留原始 `Host` 并传递 `Upgrade`/`Connection` 标头。下面是
Nginx 主机配置的最小示例；证书路径和域名按实际环境替换：

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

    location /api/ {
        proxy_pass http://127.0.0.1:13500;
        proxy_http_version 1.1;
        proxy_set_header Host $http_host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_request_buffering off;
        proxy_buffering off;
        proxy_read_timeout 604800s;
        proxy_send_timeout 604800s;
    }

    location /uploads/ {
        proxy_pass http://127.0.0.1:13500;
        proxy_set_header Host $http_host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_request_buffering off;
        proxy_buffering off;
    }

    location / {
        proxy_pass http://127.0.0.1:13501;
        proxy_set_header Host $http_host;
    }
}
```

外部代理的 `client_max_body_size` 必须不小于 `NGINX_CLIENT_MAX_BODY_SIZE`，API 和上传
路径也必须保持关闭缓冲及长 WebSocket 超时。HTTPS 和 Cookie 配置修改后重新创建
后端与前端容器：

```bash
docker compose -f docker-compose.with-db.yml up -d --force-recreate backend frontend
```

Agent 的 `OM_SERVER` 使用完整的外部地址，例如 `https://monitor.example.com`，不要追加 `/api`。

### Cloudflare Tunnel

Cloudflare Tunnel 会在浏览器与 `cloudflared` 之间终止 HTTPS，再用 HTTP 连接内网前端。
Cloudflare 会通过 `X-Forwarded-Proto` 告知访客实际使用的协议；要让管理员登录的 Origin
校验通过，在 `.env` 中启用：

```dotenv
OM_SECURE_COOKIES=true
NGINX_TRUST_FORWARDED_PROTO=true
NGINX_TRUST_CF_CONNECTING_IP=true
```

`NGINX_TRUST_FORWARDED_PROTO=true` 会让前端 Nginx 只接受值为 `http` 或 `https` 的入口协议，
其他值仍回退到 Nginx 与后端之间的本地协议。`NGINX_TRUST_CF_CONNECTING_IP=true` 会把
Cloudflare 提供的访客地址覆盖写入发给后端的 `X-Real-IP` 和 `X-Forwarded-For`，并删除
原始 `CF-Connecting-IP`。其他情况下，前端 Nginx 会覆盖而不是追加客户端提供的
`X-Forwarded-For`，避免伪造可信代理链。启用后，不能把前端宿主机端口直接暴露给公网；
如果 `cloudflared` 运行在宿主机上，请使用：

```dotenv
FRONTEND_PORT=127.0.0.1:13501
```

如果 `cloudflared` 运行在容器中，应使用仅该容器可访问的 Docker 网络或防火墙规则，并保持
前端端口不对不可信客户端开放。修改配置后重新构建并创建前端和后端容器：

```bash
docker compose -f docker-compose.with-db.yml up -d --build --force-recreate backend frontend
```

## 6. 持久化和密钥

默认自带数据库模式的业务表保存在 PostgreSQL 数据卷中；外部数据库模式的业务表由外部 PostgreSQL 负责持久化。Compose 卷保存以下内容：

- `postgres-data`：自带 PostgreSQL 的完整数据目录；仅 `docker-compose.with-db.yml` 使用。
- `backend-db`：默认保存 `/app/db/auth-secret.key`，启用更新签名后也保存
  `/app/db/update-signing.key`。
- `backend-uploads`：背景图片及上传资源。
- `backend-updates`：Agent 更新包。

当前 Compose 使用 `OM_AUTH_KEY_FILE`，第一次启动会在 `backend-db` 卷中生成密钥。数据库备份必须同时备份该卷；只恢复 PostgreSQL 而丢失密钥，已有 Authenticator 将无法解密。

后端也支持通过 `OM_AUTH_SECRET_KEY` 注入 Base64 编码的 32 字节主密钥，但当前 Compose 默认不映射该可选变量。若要改用外部 Secret，先在 `backend.environment` 中显式映射非空的 `OM_AUTH_SECRET_KEY`，再生成密钥：

```bash
openssl rand -base64 32
```

将输出作为 `OM_AUTH_SECRET_KEY` 写入 Secret 管理系统，并确保后续每次启动使用同一个值。密钥一旦丢失不能从数据库内容推导出来。

### Agent 更新签名

Agent `0.1.22` 起支持 Ed25519 更新签名。HTTP 自动更新必须启用签名；HTTPS 在 Agent
未嵌入公钥时可以依赖 TLS，但正式产物仍建议嵌入公钥。为 Compose 的 `backend-db`
卷创建私钥（命令在文件已存在时会拒绝覆盖）：

```bash
docker compose -f docker-compose.with-db.yml run --rm --no-deps \
  --entrypoint sh backend -c \
  'umask 077 && test ! -e /app/db/update-signing.key && head -c 32 /dev/urandom | base64 > /app/db/update-signing.key'
```

外部数据库模式把文件名改为 `docker-compose.yml`。随后在 `.env` 中启用该密钥：

```dotenv
OM_UPDATE_SIGNING_KEY_FILE=/app/db/update-signing.key
OM_UPDATE_SIGNING_KEY_ID=release-v1
```

重建后端并从启动日志读取公钥：

```bash
docker compose -f docker-compose.with-db.yml up -d --force-recreate backend
docker compose -f docker-compose.with-db.yml logs backend \
  | grep 'agent update signing enabled'
```

日志中的 `public_key` 是标准 Base64 公钥。构建所有 Agent 产物时传入同一个 key ID：

```bash
cd instanceEnd
OM_UPDATE_PUBLIC_KEY='<public_key>' \
OM_UPDATE_PUBLIC_KEY_ID='release-v1' \
./scripts/build-standalone.sh all
```

这两个 Agent 变量只在编译时读取。嵌入公钥的 Agent 会在 HTTP 和 HTTPS 下都拒绝
未签名、密钥 ID 不匹配或元数据被篡改的更新。后端配置了私钥但文件缺失、权限允许
组内或其他用户读取、内容无效时会拒绝启动。私钥必须随 `backend-db` 备份；丢失或
直接更换后，已嵌入旧公钥的 Agent 将无法自动更新。当前版本只支持一个公钥，不要在
没有过渡版本的情况下轮换签名密钥。
两个 Agent 变量必须同时设置或同时省略，编译会验证公钥和 key ID。服务端同时发送
兼容 `0.1.22` 的 v1 签名和绑定任务 ID、实例 ID、发布 ID、产物 ID、下载路径及重试代次的
v2 签名；`0.1.23` 在 HTTP 下强制验证 v2，在 HTTPS 下仍可验证旧服务端的 v1 签名。

直接运行后端时可在宿主机生成密钥，并将绝对路径传给后端：

```bash
umask 077
openssl rand -base64 32 > /secure/path/update-signing.key
chmod 600 /secure/path/update-signing.key
OM_UPDATE_SIGNING_KEY_FILE=/secure/path/update-signing.key \
OM_UPDATE_SIGNING_KEY_ID=release-v1 \
cargo run
```

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

恢复时先停止后端，使用 `pg_restore` 将逻辑备份恢复到已清空的目标数据库，再将文件卷内容恢复到同名卷。该流程仅用于数据库或存储损坏后的灾难恢复，不是应用版本回滚，也不保证恢复后的数据可被任意旧版本读取。卷恢复属于覆盖操作，务必先保留当前卷快照，并在维护窗口验证数据库和认证密钥来自同一备份时间点。恢复完成后执行 `docker compose -f docker-compose.with-db.yml up -d` 并访问健康检查。

## 8. 服务升级（仅向前）

升级前备份 PostgreSQL 与三个卷作为灾难恢复材料，然后在仓库根目录执行：

```bash
./deploy.sh update docker-compose.with-db.yml
```

外部数据库模式改用 `./deploy.sh update docker-compose.yml`。更新脚本会拒绝存在本地修改的
Git 工作区，从 `origin` 选择版本号最高、格式为 `主版本.次版本.修订号` 的稳定 TAG，并以
detached HEAD 方式切换到该版本。脚本会比较当前 `backend/Cargo.toml` 版本，只允许目标版本
严格更高；目标版本相同或更低时直接拒绝，不提供降级或回滚入口。脚本不会删除 Compose
命名卷，也不会自动恢复旧容器、旧代码或旧数据库。

后端启动时会执行所需的表结构补齐。升级后检查健康接口、管理员登录、Agent WebSocket、
文件上传和更新包下载。服务端版本升级是单向变更：后端不提供版本回滚 API、命令、自动
回退或旧版本兼容保障。若新版本出现问题，只能查看日志、修复配置或数据问题并重新部署
当前或更高版本；请不要将恢复备份、检出旧 TAG 或替换旧镜像当作受支持的回滚操作。

### 从标签 0.1.2 升级

仓库标签 `0.1.2` 中的 Agent 版本为 `0.1.20`，可直接连接并升级到当前版本。新增的
`target_os`、`signature_key_id`、`signature` 和 `signature_v2` 都是可选 JSON 字段，旧 Agent 会忽略；
实例身份、原始实例密钥和更新状态文件无需转换。当前后端首次启动会把数据库中的实例
密钥转换为单向 verifier，旧 Agent 仍发送原密钥并可正常认证。这个数据库转换不支持
旧后端读取，因此本次迁移是单向的；`0.1.2` 等旧后端不属于受支持的回退目标。

`0.1.20` 本身没有更新验签代码，所以它通过纯 HTTP 执行的第一次远程升级无法验证
Ed25519 签名，即使新后端已经附带签名字段也一样。该次升级必须通过 HTTPS 完成，或
离线核对二进制后使用本地 `om-agent update`。升级到编译时嵌入公钥的 `0.1.22` 后，
后续 HTTP 和 HTTPS 自动更新都会强制验签。新后端保留 v1 签名，因此 `0.1.22` 可以
直接升级到 `0.1.23`；升级完成后，Agent 在 HTTP 下只接受完整的 v2 元数据签名。

### Agent 0.1.19 认证迁移

Agent `0.1.19` 开始把 WebSocket 实例密钥放入 `Authorization` 请求头。升级包含该
变更的后端时，旧 Agent 默认会被拒绝。要使用控制台自动更新完成滚动迁移：

1. 在 `.env` 临时设置 `OM_ALLOW_LEGACY_AGENT_WS_AUTH=true`，升级并重建后端。
2. 在控制台发布 `0.1.19` 或更高版本的各平台 Agent，让所有在线实例完成更新并重新连接。
3. 确认实例版本和在线状态后，将该变量恢复为 `false`，再次重建后端。
4. 检查后端不再出现旧查询串认证弃用警告，并按日志保留策略清理维护窗口中的代理访问日志。

此开关不会回退新版 Agent；它只为旧连接增加临时兼容路径。不要长期启用。

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

确认外部代理传递 `Upgrade` 和 `Connection` 标头、保留 `Host`、覆盖写入准确的
`X-Forwarded-Proto`，并将读写超时设置为至少数小时。代理连接后端时显示的来源地址
必须包含在有效的 `OM_TRUSTED_PROXY_CIDRS` 中。远程桌面和终端在不可信网络中必须
使用 HTTPS/WSS；可信代理按每个请求选择协议，`OM_SECURE_COOKIES` 只作为回退值。

### 端口已被占用

在 `.env` 中修改 `FRONTEND_PORT` 或 `BACKEND_PORT`，例如 `FRONTEND_PORT=8080`，然后执行：

```bash
docker compose -f docker-compose.with-db.yml up -d
```

如果只希望本机反向代理访问，使用 `FRONTEND_PORT=127.0.0.1:13501`。
`BACKEND_PORT` 只接受端口号，Compose 始终把它绑定到 `127.0.0.1`。

如果 Docker 报错 `failed to set up container networking: Address already in use`，说明
Compose 内部服务 IP 冲突，而不是宿主机端口冲突。确认 `OM_COMPOSE_GATEWAY_IP`、`OM_POSTGRES_IP`、
`OM_FRONTEND_PROXY_IP` 和 `OM_BACKEND_IP` 互不相同且位于
`OM_COMPOSE_NETWORK_CIDR` 内；修改后重新创建相关容器即可，不能通过删除数据卷处理。

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
