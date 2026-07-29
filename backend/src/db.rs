use std::{str::FromStr, time::Duration};

use axum::http::StatusCode;
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tracing::error;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::{
        AgentRegisterRequest, InstanceRecord, InstanceSummary, MetricRecord, PendingInstanceSecret,
        SettingsRow,
    },
    state::AppState,
    utils::now_ts,
};

const MAX_PENDING_INSTANCES: i64 = 1_000;
const MAX_PENDING_INSTANCES_PER_SOURCE: i64 = 50;
const PENDING_INSTANCE_MAX_AGE: i64 = 7 * 24 * 60 * 60;
const PENDING_INSTANCE_TOUCH_INTERVAL: i64 = 5;

pub async fn connect_db(database_url: &str, password: Option<&str>) -> anyhow::Result<PgPool> {
    let mut options = PgConnectOptions::from_str(database_url)?;
    if let Some(password) = password {
        options = options.password(password);
    }

    match PgPoolOptions::new()
        .max_connections(8)
        .connect_with(options.clone())
        .await
    {
        Ok(pool) => Ok(pool),
        Err(error) if database_does_not_exist(&error) => {
            create_database(&options).await?;
            Ok(PgPoolOptions::new()
                .max_connections(8)
                .connect_with(options)
                .await?)
        }
        Err(error) => Err(error.into()),
    }
}

fn database_does_not_exist(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(database) if database.code().as_deref() == Some("3D000"))
}

async fn create_database(options: &PgConnectOptions) -> anyhow::Result<()> {
    let database = options
        .get_database()
        .filter(|database| !database.is_empty())
        .ok_or_else(|| anyhow::anyhow!("PostgreSQL connection URL must include a database name"))?;
    if database == "postgres" {
        anyhow::bail!("refusing to use the PostgreSQL maintenance database as application storage");
    }

    let maintenance = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone().database("postgres"))
        .await?;
    let quoted_database = database.replace('"', "\"\"");
    sqlx::query(&format!("CREATE DATABASE \"{quoted_database}\""))
        .execute(&maintenance)
        .await?;
    maintenance.close().await;
    Ok(())
}

pub async fn init_db(db: &PgPool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS instances (
            id TEXT PRIMARY KEY,
            secret TEXT NOT NULL,
            name TEXT NOT NULL,
            region TEXT NOT NULL DEFAULT '',
            country_code TEXT NOT NULL DEFAULT '',
            country TEXT NOT NULL DEFAULT '',
            province_code TEXT NOT NULL DEFAULT '',
            province TEXT NOT NULL DEFAULT '',
            city TEXT NOT NULL DEFAULT '',
            remark TEXT NOT NULL DEFAULT '',
            hostname TEXT NOT NULL DEFAULT '',
            os TEXT NOT NULL DEFAULT '',
            arch TEXT NOT NULL DEFAULT '',
            agent_version TEXT NOT NULL DEFAULT '',
            package_type TEXT NOT NULL DEFAULT '',
            native_arch TEXT NOT NULL DEFAULT '',
            update_privileged BIGINT NOT NULL DEFAULT 0,
            approved BIGINT NOT NULL DEFAULT 1,
            disabled BIGINT NOT NULL DEFAULT 0,
            first_seen BIGINT NOT NULL,
            last_seen BIGINT
        );
        "#,
    )
    .execute(db)
    .await?;

    ensure_instance_location_columns(db).await?;
    ensure_capability_columns(db, "instances").await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS instance_docker_status (
            instance_id TEXT PRIMARY KEY REFERENCES instances(id) ON DELETE CASCADE,
            status TEXT NOT NULL,
            cli_version TEXT,
            engine_version TEXT,
            api_version TEXT,
            compose_version TEXT,
            diagnostic TEXT NOT NULL DEFAULT '',
            checked_at BIGINT NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS docker_exec_sessions (
            id TEXT PRIMARY KEY,
            instance_id TEXT REFERENCES instances(id) ON DELETE SET NULL,
            instance_snapshot TEXT NOT NULL DEFAULT '',
            request_id TEXT NOT NULL UNIQUE,
            actor TEXT NOT NULL,
            operation TEXT NOT NULL,
            target TEXT NOT NULL DEFAULT '',
            metadata TEXT NOT NULL DEFAULT '{}',
            status TEXT NOT NULL,
            error_code TEXT,
            error_message TEXT NOT NULL DEFAULT '',
            requested_at BIGINT NOT NULL,
            completed_at BIGINT
        );
        "#,
    )
    .execute(db)
    .await?;
    sqlx::query(
        "ALTER TABLE docker_exec_sessions ADD COLUMN IF NOT EXISTS instance_snapshot TEXT NOT NULL DEFAULT ''",
    )
    .execute(db)
    .await?;
    sqlx::query(
        "ALTER TABLE docker_exec_sessions ADD COLUMN IF NOT EXISTS metadata TEXT NOT NULL DEFAULT '{}'",
    )
    .execute(db)
    .await?;
    sqlx::query(
        "UPDATE docker_exec_sessions SET instance_snapshot = instance_id WHERE instance_snapshot = '' AND instance_id IS NOT NULL",
    )
    .execute(db)
    .await?;
    sqlx::query("ALTER TABLE docker_exec_sessions ALTER COLUMN instance_id DROP NOT NULL")
        .execute(db)
        .await?;
    sqlx::query(
        "ALTER TABLE docker_exec_sessions DROP CONSTRAINT IF EXISTS docker_exec_sessions_instance_id_fkey",
    )
    .execute(db)
    .await?;
    sqlx::query(
        r#"
        DO $$
        BEGIN
            IF NOT EXISTS (
                SELECT 1 FROM pg_constraint
                WHERE conname = 'docker_exec_sessions_instance_id_fkey_set_null'
                  AND conrelid = 'docker_exec_sessions'::regclass
            ) THEN
                ALTER TABLE docker_exec_sessions
                    ADD CONSTRAINT docker_exec_sessions_instance_id_fkey_set_null
                    FOREIGN KEY(instance_id) REFERENCES instances(id) ON DELETE SET NULL;
            END IF;
        END
        $$;
        "#,
    )
    .execute(db)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_docker_exec_instance_requested ON docker_exec_sessions(instance_id, requested_at DESC);",
    )
    .execute(db)
    .await?;
    sqlx::query(
        "UPDATE docker_exec_sessions SET status = 'failed', error_code = 'backend_restarted', error_message = '后端服务重启', completed_at = $1 WHERE status = 'running'",
    )
    .bind(now_ts())
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS pending_instances (
            id TEXT PRIMARY KEY,
            secret TEXT NOT NULL,
            hostname TEXT NOT NULL,
            os TEXT NOT NULL,
            arch TEXT NOT NULL,
            agent_version TEXT NOT NULL,
            package_type TEXT NOT NULL DEFAULT '',
            native_arch TEXT NOT NULL DEFAULT '',
            update_privileged BIGINT NOT NULL DEFAULT 0,
            first_seen BIGINT NOT NULL,
            last_seen BIGINT NOT NULL,
            source_key TEXT NOT NULL DEFAULT ''
        );
        "#,
    )
    .execute(db)
    .await?;
    sqlx::query(
        "ALTER TABLE pending_instances ADD COLUMN IF NOT EXISTS source_key TEXT NOT NULL DEFAULT ''",
    )
    .execute(db)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_pending_instances_source ON pending_instances(source_key)",
    )
    .execute(db)
    .await?;

    ensure_capability_columns(db, "pending_instances").await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS metrics (
            id BIGSERIAL PRIMARY KEY,
            instance_id TEXT NOT NULL,
            ts BIGINT NOT NULL,
            cpu_percent DOUBLE PRECISION NOT NULL,
            memory_used BIGINT NOT NULL,
            memory_total BIGINT NOT NULL,
            disk_used BIGINT NOT NULL,
            disk_total BIGINT NOT NULL,
            network_rx BIGINT NOT NULL,
            network_tx BIGINT NOT NULL,
            gpu_percent DOUBLE PRECISION,
            gpu_memory_used BIGINT,
            gpu_memory_total BIGINT,
            uptime_seconds BIGINT NOT NULL,
            load_average DOUBLE PRECISION,
            latency_ms DOUBLE PRECISION
        );
        "#,
    )
    .execute(db)
    .await?;

    ensure_metric_columns(db).await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_metrics_instance_ts ON metrics(instance_id, ts DESC);",
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS commands (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            command TEXT NOT NULL,
            confirm_text TEXT NOT NULL DEFAULT '',
            enabled BIGINT NOT NULL DEFAULT 1,
            created_at BIGINT NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS command_jobs (
            id TEXT PRIMARY KEY,
            command_id TEXT,
            instance_id TEXT NOT NULL,
            command TEXT NOT NULL,
            status TEXT NOT NULL,
            requested_by TEXT NOT NULL,
            created_at BIGINT NOT NULL,
            completed_at BIGINT,
            output TEXT NOT NULL DEFAULT '',
            exit_code BIGINT
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS ssh_sessions (
            id TEXT PRIMARY KEY,
            instance_id TEXT NOT NULL,
            actor TEXT NOT NULL,
            started_at BIGINT NOT NULL,
            ended_at BIGINT
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS desktop_sessions (
            id TEXT PRIMARY KEY,
            instance_id TEXT NOT NULL,
            actor TEXT NOT NULL,
            started_at BIGINT NOT NULL,
            ended_at BIGINT,
            end_reason TEXT
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_desktop_sessions_instance_started ON desktop_sessions(instance_id, started_at DESC);",
    )
    .execute(db)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_desktop_sessions_ended ON desktop_sessions(ended_at) WHERE ended_at IS NOT NULL;",
    )
    .execute(db)
    .await?;

    sqlx::query(
        "UPDATE desktop_sessions SET ended_at = $1, end_reason = 'backend_restarted' WHERE ended_at IS NULL",
    )
    .bind(now_ts())
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS action_logs (
            id TEXT PRIMARY KEY,
            actor TEXT NOT NULL,
            action TEXT NOT NULL,
            target TEXT NOT NULL,
            detail TEXT NOT NULL,
            created_at BIGINT NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS admin_users (
            id TEXT PRIMARY KEY,
            username TEXT NOT NULL,
            username_normalized TEXT NOT NULL UNIQUE,
            enabled BIGINT NOT NULL DEFAULT 1,
            created_at BIGINT NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS authenticator_devices (
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL REFERENCES admin_users(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            secret_ciphertext TEXT NOT NULL,
            created_at BIGINT NOT NULL,
            last_used_at BIGINT,
            last_totp_counter BIGINT
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        "ALTER TABLE authenticator_devices ADD COLUMN IF NOT EXISTS last_totp_counter BIGINT",
    )
    .execute(db)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_authenticator_devices_user ON authenticator_devices(user_id);",
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS admin_enrollments (
            id TEXT PRIMARY KEY,
            target_user_id TEXT REFERENCES admin_users(id) ON DELETE CASCADE,
            username TEXT NOT NULL,
            username_normalized TEXT NOT NULL,
            device_name TEXT NOT NULL,
            secret_ciphertext TEXT NOT NULL,
            created_by_user_id TEXT REFERENCES admin_users(id) ON DELETE SET NULL,
            created_at BIGINT NOT NULL,
            expires_at BIGINT NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_admin_enrollments_expiry ON admin_enrollments(expires_at);",
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_releases (
            id TEXT PRIMARY KEY,
            version TEXT NOT NULL UNIQUE,
            notes TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT 'draft' CHECK(status IN ('draft', 'published')),
            created_at BIGINT NOT NULL,
            published_at BIGINT
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_artifacts (
            id TEXT PRIMARY KEY,
            release_id TEXT NOT NULL REFERENCES agent_releases(id) ON DELETE CASCADE,
            os TEXT NOT NULL,
            package_type TEXT NOT NULL,
            native_arch TEXT NOT NULL,
            file_name TEXT NOT NULL,
            size_bytes BIGINT NOT NULL,
            sha256 TEXT NOT NULL,
            storage_path TEXT NOT NULL,
            created_at BIGINT NOT NULL,
            status TEXT NOT NULL DEFAULT 'draft' CHECK(status IN ('draft', 'published')),
            published_at BIGINT,
            UNIQUE(release_id, os, package_type, native_arch)
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("ALTER TABLE agent_artifacts ADD COLUMN IF NOT EXISTS published_at BIGINT")
        .execute(db)
        .await?;
    sqlx::query(
        r#"
        DO $$
        BEGIN
            IF NOT EXISTS (
                SELECT 1
                FROM information_schema.columns
                WHERE table_schema = current_schema()
                  AND table_name = 'agent_artifacts'
                  AND column_name = 'status'
            ) THEN
                ALTER TABLE agent_artifacts
                    ADD COLUMN status TEXT NOT NULL DEFAULT 'draft'
                    CHECK(status IN ('draft', 'published'));
                UPDATE agent_artifacts AS artifact
                SET status = 'published',
                    published_at = COALESCE(release.published_at, artifact.created_at)
                FROM agent_releases AS release
                WHERE artifact.release_id = release.id
                  AND release.status = 'published';
            END IF;
        END
        $$
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_update_attempts (
            id TEXT PRIMARY KEY,
            release_id TEXT NOT NULL REFERENCES agent_releases(id) ON DELETE CASCADE,
            artifact_id TEXT NOT NULL REFERENCES agent_artifacts(id) ON DELETE CASCADE,
            instance_id TEXT NOT NULL REFERENCES instances(id) ON DELETE CASCADE,
            from_version TEXT NOT NULL,
            target_version TEXT NOT NULL,
            status TEXT NOT NULL,
            message TEXT NOT NULL DEFAULT '',
            retry_count BIGINT NOT NULL DEFAULT 0,
            created_at BIGINT NOT NULL,
            updated_at BIGINT NOT NULL,
            completed_at BIGINT,
            UNIQUE(release_id, instance_id)
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_agent_attempts_instance_updated ON agent_update_attempts(instance_id, updated_at DESC);",
    )
    .execute(db)
    .await?;

    sqlx::query(
        "INSERT INTO settings(key, value) VALUES('retention_days', '30') ON CONFLICT(key) DO NOTHING;",
    )
        .execute(db)
        .await?;

    for (key, value) in [("theme_mode", "auto"), ("accent_color", "#3bbf9b")] {
        sqlx::query("INSERT INTO settings(key, value) VALUES($1, $2) ON CONFLICT(key) DO NOTHING;")
            .bind(key)
            .bind(value)
            .execute(db)
            .await?;
    }

    ensure_bigint_columns(db).await?;

    Ok(())
}

async fn ensure_bigint_columns(db: &PgPool) -> anyhow::Result<()> {
    for (table, columns) in [
        (
            "instances",
            &[
                "update_privileged",
                "approved",
                "disabled",
                "first_seen",
                "last_seen",
            ][..],
        ),
        (
            "pending_instances",
            &["update_privileged", "first_seen", "last_seen"][..],
        ),
        (
            "metrics",
            &[
                "id",
                "ts",
                "memory_used",
                "memory_total",
                "disk_used",
                "disk_total",
                "network_rx",
                "network_tx",
                "gpu_memory_used",
                "gpu_memory_total",
                "uptime_seconds",
            ][..],
        ),
        ("commands", &["enabled", "created_at"][..]),
        (
            "command_jobs",
            &["created_at", "completed_at", "exit_code"][..],
        ),
        ("ssh_sessions", &["started_at", "ended_at"][..]),
        ("desktop_sessions", &["started_at", "ended_at"][..]),
        ("action_logs", &["created_at"][..]),
        ("admin_users", &["enabled", "created_at"][..]),
        (
            "authenticator_devices",
            &["created_at", "last_used_at", "last_totp_counter"][..],
        ),
        ("admin_enrollments", &["created_at", "expires_at"][..]),
        ("agent_releases", &["created_at", "published_at"][..]),
        (
            "agent_artifacts",
            &["size_bytes", "created_at", "published_at"][..],
        ),
        (
            "agent_update_attempts",
            &["retry_count", "created_at", "updated_at", "completed_at"][..],
        ),
        ("instance_docker_status", &["checked_at"][..]),
        (
            "docker_exec_sessions",
            &["requested_at", "completed_at"][..],
        ),
    ] {
        for column in columns {
            let data_type: Option<String> = sqlx::query_scalar(
                r#"
                SELECT data_type
                FROM information_schema.columns
                WHERE table_schema = current_schema()
                  AND table_name = $1
                  AND column_name = $2
                "#,
            )
            .bind(table)
            .bind(column)
            .fetch_optional(db)
            .await?;

            if matches!(data_type.as_deref(), Some("integer" | "smallint")) {
                sqlx::query(&format!(
                    "ALTER TABLE {table} ALTER COLUMN {column} TYPE BIGINT USING {column}::BIGINT"
                ))
                .execute(db)
                .await?;
            }
        }
    }

    Ok(())
}

async fn ensure_instance_location_columns(db: &PgPool) -> anyhow::Result<()> {
    for (name, definition) in [
        ("country_code", "TEXT NOT NULL DEFAULT ''"),
        ("country", "TEXT NOT NULL DEFAULT ''"),
        ("province_code", "TEXT NOT NULL DEFAULT ''"),
        ("province", "TEXT NOT NULL DEFAULT ''"),
        ("city", "TEXT NOT NULL DEFAULT ''"),
    ] {
        sqlx::query(&format!(
            "ALTER TABLE instances ADD COLUMN IF NOT EXISTS {name} {definition}"
        ))
        .execute(db)
        .await?;
    }

    Ok(())
}

async fn ensure_metric_columns(db: &PgPool) -> anyhow::Result<()> {
    sqlx::query("ALTER TABLE metrics ADD COLUMN IF NOT EXISTS latency_ms DOUBLE PRECISION")
        .execute(db)
        .await?;

    Ok(())
}

async fn ensure_capability_columns(db: &PgPool, table: &str) -> anyhow::Result<()> {
    for (name, definition) in [
        ("package_type", "TEXT NOT NULL DEFAULT ''"),
        ("native_arch", "TEXT NOT NULL DEFAULT ''"),
        ("update_privileged", "BIGINT NOT NULL DEFAULT 0"),
    ] {
        sqlx::query(&format!(
            "ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {name} {definition}"
        ))
        .execute(db)
        .await?;
    }

    Ok(())
}

pub async fn register_or_touch_pending(
    db: &PgPool,
    payload: &AgentRegisterRequest,
    source_key: &str,
) -> AppResult<()> {
    validate_agent_registration(payload)?;
    let mut tx = db.begin().await?;
    let mut instance = sqlx::query_as::<_, InstanceRecord>(
        r#"
        SELECT id, secret, name, region, country_code, country, province_code, province, city,
               remark, hostname, os, arch, agent_version,
               package_type, native_arch, update_privileged,
               approved, disabled, first_seen, last_seen
        FROM instances
        WHERE id = $1
        FOR UPDATE
        "#,
    )
    .bind(&payload.instance_id)
    .fetch_optional(&mut *tx)
    .await?;

    let mut pending_secret: Option<String> = None;
    if instance.is_none() {
        pending_secret =
            sqlx::query_scalar("SELECT secret FROM pending_instances WHERE id = $1 FOR UPDATE")
                .bind(&payload.instance_id)
                .fetch_optional(&mut *tx)
                .await?;
        if pending_secret.is_none() {
            sqlx::query("LOCK TABLE pending_instances IN SHARE ROW EXCLUSIVE MODE")
                .execute(&mut *tx)
                .await?;
            instance = sqlx::query_as::<_, InstanceRecord>(
                r#"
                SELECT id, secret, name, region, country_code, country, province_code, province, city,
                       remark, hostname, os, arch, agent_version,
                       package_type, native_arch, update_privileged,
                       approved, disabled, first_seen, last_seen
                FROM instances
                WHERE id = $1
                FOR UPDATE
                "#,
            )
            .bind(&payload.instance_id)
            .fetch_optional(&mut *tx)
            .await?;
            if instance.is_none() {
                pending_secret = sqlx::query_scalar(
                    "SELECT secret FROM pending_instances WHERE id = $1 FOR UPDATE",
                )
                .bind(&payload.instance_id)
                .fetch_optional(&mut *tx)
                .await?;
            }
        }
    }

    if let Some(instance) = instance {
        if !agent_secret_is_authorized(&instance.secret, payload) {
            return Err(AppError::new(StatusCode::UNAUTHORIZED, "实例密钥不匹配"));
        }
        sqlx::query(
            r#"
            UPDATE instances
            SET secret = $1, hostname = $2, os = $3, arch = $4, agent_version = $5,
                package_type = COALESCE($6, package_type),
                native_arch = COALESCE($7, native_arch),
                update_privileged = COALESCE($8, update_privileged),
                last_seen = $9
            WHERE id = $10
            "#,
        )
        .bind(&payload.secret)
        .bind(&payload.hostname)
        .bind(&payload.os)
        .bind(&payload.arch)
        .bind(&payload.agent_version)
        .bind(payload.package_type.as_deref())
        .bind(payload.native_arch.as_deref())
        .bind(payload.update_privileged.map(i64::from))
        .bind(now_ts())
        .bind(&payload.instance_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM pending_instances WHERE id = $1")
            .bind(&payload.instance_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(());
    }

    if let Some(secret) = pending_secret {
        if !agent_secret_is_authorized(&secret, payload) {
            return Err(AppError::new(StatusCode::UNAUTHORIZED, "实例密钥不匹配"));
        }
        sqlx::query(
            r#"
            UPDATE pending_instances
            SET secret = $1, hostname = $2, os = $3, arch = $4, agent_version = $5,
                package_type = $6, native_arch = $7, update_privileged = $8, last_seen = $9
            WHERE id = $10 AND (secret != $1 OR last_seen <= $11)
            "#,
        )
        .bind(&payload.secret)
        .bind(&payload.hostname)
        .bind(&payload.os)
        .bind(&payload.arch)
        .bind(&payload.agent_version)
        .bind(payload.package_type.as_deref().unwrap_or_default())
        .bind(payload.native_arch.as_deref().unwrap_or_default())
        .bind(i64::from(payload.update_privileged.unwrap_or(false)))
        .bind(now_ts())
        .bind(&payload.instance_id)
        .bind(now_ts() - PENDING_INSTANCE_TOUCH_INTERVAL)
        .execute(&mut *tx)
        .await?;
    } else {
        let now = now_ts();
        sqlx::query("DELETE FROM pending_instances WHERE first_seen < $1")
            .bind(now - PENDING_INSTANCE_MAX_AGE)
            .execute(&mut *tx)
            .await?;
        let source_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pending_instances WHERE source_key = $1")
                .bind(source_key)
                .fetch_one(&mut *tx)
                .await?;
        if source_count >= MAX_PENDING_INSTANCES_PER_SOURCE {
            return Err(AppError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "该来源的待审批实例数量已达上限",
            ));
        }
        let pending_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_instances")
            .fetch_one(&mut *tx)
            .await?;
        if pending_count >= MAX_PENDING_INSTANCES {
            sqlx::query(
                r#"
                DELETE FROM pending_instances
                WHERE id = (
                    SELECT id FROM pending_instances
                    ORDER BY last_seen ASC, first_seen ASC, id ASC
                    LIMIT 1
                )
                "#,
            )
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            r#"
            INSERT INTO pending_instances(id, secret, hostname, os, arch, agent_version,
                                          package_type, native_arch, update_privileged,
                                          first_seen, last_seen, source_key)
            VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            "#,
        )
        .bind(&payload.instance_id)
        .bind(&payload.secret)
        .bind(&payload.hostname)
        .bind(&payload.os)
        .bind(&payload.arch)
        .bind(&payload.agent_version)
        .bind(payload.package_type.as_deref().unwrap_or_default())
        .bind(payload.native_arch.as_deref().unwrap_or_default())
        .bind(i64::from(payload.update_privileged.unwrap_or(false)))
        .bind(now)
        .bind(now)
        .bind(source_key)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    Ok(())
}

fn validate_agent_registration(payload: &AgentRegisterRequest) -> AppResult<()> {
    validate_agent_identifier(&payload.instance_id)?;
    validate_agent_secret(&payload.secret)?;
    if let Some(value) = payload.previous_secret.as_deref() {
        validate_agent_secret(value)?;
    }
    validate_agent_field("hostname", &payload.hostname, 255, false)?;
    validate_agent_field("os", &payload.os, 64, false)?;
    validate_agent_field("arch", &payload.arch, 64, false)?;
    validate_agent_field("agent_version", &payload.agent_version, 64, false)?;
    if let Some(value) = payload.package_type.as_deref() {
        validate_agent_field("package_type", value, 64, true)?;
    }
    if let Some(value) = payload.native_arch.as_deref() {
        validate_agent_field("native_arch", value, 64, true)?;
    }
    Ok(())
}

fn agent_secret_is_authorized(current_secret: &str, payload: &AgentRegisterRequest) -> bool {
    payload.secret == current_secret || payload.previous_secret.as_deref() == Some(current_secret)
}

fn validate_agent_identifier(value: &str) -> AppResult<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(AppError::bad_request("实例 ID 格式无效"));
    }
    Ok(())
}

fn validate_agent_secret(value: &str) -> AppResult<()> {
    if value.is_empty() || value.len() > 256 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(AppError::bad_request("实例密钥格式无效"));
    }
    Ok(())
}

fn validate_agent_field(
    name: &str,
    value: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> AppResult<()> {
    if (!allow_empty && value.trim().is_empty())
        || value.len() > max_bytes
        || value.chars().any(char::is_control)
    {
        return Err(AppError::bad_request(format!("实例字段 {name} 格式无效")));
    }
    Ok(())
}

pub async fn approve_pending_instance(
    db: &PgPool,
    id: &str,
) -> AppResult<Option<PendingInstanceSecret>> {
    let mut tx = db.begin().await?;
    let pending = sqlx::query_as::<_, PendingInstanceSecret>(
        r#"
        SELECT id, secret, hostname, os, arch, agent_version, package_type, native_arch,
               update_privileged, first_seen, last_seen
        FROM pending_instances
        WHERE id = $1
        FOR UPDATE
        "#,
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(pending) = pending else {
        tx.commit().await?;
        return Ok(None);
    };

    let inserted = sqlx::query(
        r#"
        INSERT INTO instances(id, secret, name, region, country_code, country, province_code,
                              province, city, remark, hostname, os, arch, agent_version,
                              package_type, native_arch, update_privileged, approved, disabled,
                              first_seen, last_seen)
        VALUES($1, $2, $3, '', '', '', '', '', '', '', $4, $5, $6, $7, $8, $9, $10, 1, 0, $11, $12)
        ON CONFLICT(id) DO NOTHING
        "#,
    )
    .bind(&pending.id)
    .bind(&pending.secret)
    .bind(&pending.hostname)
    .bind(&pending.hostname)
    .bind(&pending.os)
    .bind(&pending.arch)
    .bind(&pending.agent_version)
    .bind(&pending.package_type)
    .bind(&pending.native_arch)
    .bind(pending.update_privileged)
    .bind(pending.first_seen)
    .bind(pending.last_seen)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "同 ID 实例已存在，无法审批待审批记录",
        ));
    }

    sqlx::query("DELETE FROM pending_instances WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(Some(pending))
}

pub async fn get_instance(db: &PgPool, id: &str) -> AppResult<InstanceRecord> {
    get_instance_optional(db, id)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "实例不存在"))
}

pub async fn get_instance_optional(db: &PgPool, id: &str) -> AppResult<Option<InstanceRecord>> {
    let record = sqlx::query_as::<_, InstanceRecord>(
        r#"
        SELECT id, secret, name, region, country_code, country, province_code, province, city,
               remark, hostname, os, arch, agent_version,
               package_type, native_arch, update_privileged,
               approved, disabled, first_seen, last_seen
        FROM instances
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    Ok(record)
}

pub async fn latest_metric(db: &PgPool, instance_id: &str) -> AppResult<Option<MetricRecord>> {
    let metric = sqlx::query_as::<_, MetricRecord>(
        r#"
        SELECT ts, cpu_percent, memory_used, memory_total, disk_used, disk_total,
               network_rx, network_tx, gpu_percent, gpu_memory_used, gpu_memory_total,
               uptime_seconds, load_average, latency_ms
        FROM metrics
        WHERE instance_id = $1
        ORDER BY ts DESC
        LIMIT 1
        "#,
    )
    .bind(instance_id)
    .fetch_optional(db)
    .await?;
    Ok(metric)
}

pub fn normalize_metric_timestamp(timestamp: i64, received_at: i64) -> AppResult<i64> {
    if timestamp <= 0 || received_at <= 0 {
        return Err(AppError::bad_request("指标时间戳无效"));
    }
    Ok(timestamp.min(received_at))
}

pub fn instance_summary(
    record: InstanceRecord,
    metrics: Option<MetricRecord>,
    online: bool,
    capabilities: Vec<String>,
) -> InstanceSummary {
    InstanceSummary {
        id: record.id,
        name: record.name,
        region: record.region,
        country_code: record.country_code,
        country: record.country,
        province_code: record.province_code,
        province: record.province,
        city: record.city,
        remark: record.remark,
        hostname: record.hostname,
        os: record.os,
        arch: record.arch,
        agent_version: record.agent_version,
        capabilities,
        online,
        first_seen: record.first_seen,
        last_seen: record.last_seen,
        metrics,
    }
}

pub async fn retention_days(db: &PgPool) -> AppResult<i64> {
    let row =
        sqlx::query_as::<_, SettingsRow>("SELECT value FROM settings WHERE key = 'retention_days'")
            .fetch_optional(db)
            .await?;
    Ok(row
        .and_then(|row| row.value.parse::<i64>().ok())
        .unwrap_or(30)
        .clamp(1, 365))
}

pub async fn setting_value(db: &PgPool, key: &str) -> AppResult<Option<String>> {
    let row = sqlx::query_as::<_, SettingsRow>("SELECT value FROM settings WHERE key = $1")
        .bind(key)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|row| row.value).filter(|value| !value.is_empty()))
}

pub async fn cleanup_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    loop {
        interval.tick().await;
        match retention_days(&state.db).await {
            Ok(days) => {
                let cutoff = now_ts() - days * 24 * 3600;
                if let Err(error) = sqlx::query("DELETE FROM metrics WHERE ts < $1")
                    .bind(cutoff)
                    .execute(&state.db)
                    .await
                {
                    error!(?error, "failed to clean old metrics");
                }
                if let Err(error) = sqlx::query(
                    "DELETE FROM desktop_sessions WHERE ended_at IS NOT NULL AND ended_at < $1",
                )
                .bind(cutoff)
                .execute(&state.db)
                .await
                {
                    error!(?error, "failed to clean old desktop sessions");
                }
                if let Err(error) =
                    sqlx::query("DELETE FROM pending_instances WHERE first_seen < $1")
                        .bind(now_ts() - PENDING_INSTANCE_MAX_AGE)
                        .execute(&state.db)
                        .await
                {
                    error!(?error, "failed to clean stale pending instances");
                }
            }
            Err(error) => error!(?error, "failed to read retention setting"),
        }
    }
}

pub async fn write_action_log(
    db: &PgPool,
    actor: &str,
    action: &str,
    target: &str,
    detail: &str,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO action_logs(id, actor, action, target, detail, created_at) VALUES($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(actor)
    .bind(action)
    .bind(target)
    .bind(detail)
    .bind(now_ts())
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registration_payload() -> AgentRegisterRequest {
        AgentRegisterRequest {
            instance_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            secret: "550e8400-e29b-41d4-a716-446655440001".to_string(),
            previous_secret: None,
            hostname: "host-1".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            agent_version: "0.1.0".to_string(),
            package_type: Some("standalone".to_string()),
            native_arch: Some("x86_64".to_string()),
            update_privileged: Some(true),
        }
    }

    #[test]
    fn validates_bounded_agent_registration_fields() {
        assert!(validate_agent_registration(&registration_payload()).is_ok());

        let mut invalid_id = registration_payload();
        invalid_id.instance_id = "../agent".to_string();
        assert!(validate_agent_registration(&invalid_id).is_err());

        let mut oversized_hostname = registration_payload();
        oversized_hostname.hostname = "h".repeat(256);
        assert!(validate_agent_registration(&oversized_hostname).is_err());

        let mut control_character = registration_payload();
        control_character.os = "linux\nforged-log".to_string();
        assert!(validate_agent_registration(&control_character).is_err());
    }

    #[test]
    fn secret_rotation_requires_the_current_secret() {
        let mut payload = registration_payload();
        payload.secret = "new-agent-secret".to_string();
        assert!(!agent_secret_is_authorized(
            "current-agent-secret",
            &payload
        ));

        payload.previous_secret = Some("wrong-agent-secret".to_string());
        assert!(!agent_secret_is_authorized(
            "current-agent-secret",
            &payload
        ));

        payload.previous_secret = Some("current-agent-secret".to_string());
        assert!(agent_secret_is_authorized("current-agent-secret", &payload));
    }

    #[test]
    fn metric_timestamps_cannot_extend_into_the_future() {
        assert_eq!(normalize_metric_timestamp(900, 1_000).unwrap(), 900);
        assert_eq!(normalize_metric_timestamp(1_001, 1_000).unwrap(), 1_000);
        assert_eq!(normalize_metric_timestamp(i64::MAX, 1_000).unwrap(), 1_000);
        assert!(normalize_metric_timestamp(0, 1_000).is_err());
        assert!(normalize_metric_timestamp(i64::MIN, 1_000).is_err());
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn pending_instance_secret_cannot_be_replaced() {
        let db = PgPoolOptions::new()
            .max_connections(1)
            .connect("postgresql://localhost/postgres")
            .await
            .expect("connect database");
        init_db(&db).await.expect("initialize database");
        let original = registration_payload();
        register_or_touch_pending(&db, &original, "test-source")
            .await
            .expect("create pending instance");

        let mut replacement = registration_payload();
        replacement.secret = "different-agent-secret".to_string();
        let error = register_or_touch_pending(&db, &replacement, "test-source")
            .await
            .expect_err("pending secret must be immutable");
        assert_eq!(error.status, StatusCode::UNAUTHORIZED);

        let stored_secret: String =
            sqlx::query_scalar("SELECT secret FROM pending_instances WHERE id = $1")
                .bind(&original.instance_id)
                .fetch_one(&db)
                .await
                .expect("load pending secret");
        assert_eq!(stored_secret, original.secret);
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn init_db_migrates_existing_instance_locations() {
        let db = PgPoolOptions::new()
            .max_connections(1)
            .connect("postgresql://localhost/postgres")
            .await
            .expect("connect in-memory database");

        sqlx::query(
            r#"
            CREATE TABLE instances (
                id TEXT PRIMARY KEY,
                secret TEXT NOT NULL,
                name TEXT NOT NULL,
                region TEXT NOT NULL DEFAULT '',
                remark TEXT NOT NULL DEFAULT '',
                hostname TEXT NOT NULL DEFAULT '',
                os TEXT NOT NULL DEFAULT '',
                arch TEXT NOT NULL DEFAULT '',
                agent_version TEXT NOT NULL DEFAULT '',
                approved INTEGER NOT NULL DEFAULT 1,
                disabled INTEGER NOT NULL DEFAULT 0,
                first_seen INTEGER NOT NULL,
                last_seen INTEGER
            )
            "#,
        )
        .execute(&db)
        .await
        .expect("create legacy instances table");
        sqlx::query(
            "INSERT INTO instances(id, secret, name, region, first_seen) VALUES('old', 'secret', 'Old', '上海', 1)",
        )
        .execute(&db)
        .await
        .expect("insert legacy instance");

        init_db(&db).await.expect("migrate database");

        let record = get_instance(&db, "old")
            .await
            .expect("load migrated instance");
        assert_eq!(record.region, "上海");
        assert_eq!(record.country_code, "");
        assert_eq!(record.country, "");
        assert_eq!(record.province_code, "");
        assert_eq!(record.province, "");
        assert_eq!(record.city, "");

        let approved_type: String = sqlx::query_scalar(
            r#"
            SELECT data_type
            FROM information_schema.columns
            WHERE table_schema = current_schema()
              AND table_name = 'instances'
              AND column_name = 'approved'
            "#,
        )
        .fetch_one(&db)
        .await
        .expect("load migrated column type");
        assert_eq!(approved_type, "bigint");
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn approved_instance_is_not_recreated_as_pending_by_concurrent_registration() {
        let db = PgPoolOptions::new()
            .max_connections(4)
            .connect("postgresql://localhost/postgres")
            .await
            .expect("connect in-memory database");
        init_db(&db).await.expect("initialize database");
        let payload = AgentRegisterRequest {
            instance_id: "agent-1".to_string(),
            secret: "secret-1".to_string(),
            previous_secret: None,
            hostname: "host-1".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            agent_version: "0.1.0".to_string(),
            package_type: Some("standalone".to_string()),
            native_arch: Some("x86_64".to_string()),
            update_privileged: Some(true),
        };

        register_or_touch_pending(&db, &payload, "test-source")
            .await
            .expect("create pending instance");
        let (approved, registered) = tokio::join!(
            approve_pending_instance(&db, &payload.instance_id),
            register_or_touch_pending(&db, &payload, "test-source"),
        );
        approved.expect("approve instance");
        registered.expect("register instance");

        assert!(
            get_instance_optional(&db, &payload.instance_id)
                .await
                .expect("load instance")
                .is_some()
        );
        let pending_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pending_instances WHERE id = $1")
                .bind(&payload.instance_id)
                .fetch_one(&db)
                .await
                .expect("count pending instances");
        assert_eq!(pending_count, 0);
    }
}
