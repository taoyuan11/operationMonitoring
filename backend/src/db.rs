use std::{collections::HashMap, net::IpAddr, str::FromStr, time::Duration};

use axum::http::StatusCode;
use sha2::{Digest, Sha256};
use sqlx::{
    Executor, FromRow, PgPool, Postgres,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use subtle::ConstantTimeEq;
use tracing::error;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::{
        AgentRegisterRequest, DeviceProfile, InstanceAgentMetadata, InstanceRecord,
        InstanceSummary, MetricRecord, PendingInstanceSecret, SettingsRow,
    },
    state::AppState,
    utils::now_ts,
};

const MAX_PENDING_INSTANCES: i64 = 1_000;
const MAX_PENDING_INSTANCES_PER_SOURCE: i64 = 50;
const PENDING_INSTANCE_MAX_AGE: i64 = 7 * 24 * 60 * 60;
const PENDING_INSTANCE_TOUCH_INTERVAL: i64 = 5;
const MAX_DEVICE_PROFILE_BYTES: usize = 64 * 1024;
const MAX_DEVICE_PROFILE_STRING_BYTES: usize = 256;
const MAX_DEVICE_GPUS: usize = 16;
const MAX_DEVICE_DISKS: usize = 32;
const MAX_DEVICE_INTERFACES: usize = 32;
const MAX_DEVICE_INTERFACE_ADDRESSES: usize = 16;
const AGENT_SECRET_VERIFIER_PREFIX: &str = "v1$sha256$";

#[derive(FromRow)]
struct InstanceMetricRecord {
    instance_id: String,
    #[sqlx(flatten)]
    metric: MetricRecord,
}

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
            rollback_supported BIGINT NOT NULL DEFAULT 0,
            rollback_version TEXT NOT NULL DEFAULT '',
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
        CREATE TABLE IF NOT EXISTS instance_agent_metadata (
            instance_id TEXT PRIMARY KEY REFERENCES instances(id) ON DELETE CASCADE,
            capabilities TEXT NOT NULL DEFAULT '',
            device_profile TEXT NOT NULL DEFAULT '',
            observed_ip TEXT NOT NULL DEFAULT '',
            device_profile_updated_at BIGINT,
            updated_at BIGINT NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;
    sqlx::query(
        "ALTER TABLE instance_agent_metadata ADD COLUMN IF NOT EXISTS device_profile_updated_at BIGINT",
    )
    .execute(db)
    .await?;
    sqlx::query(
        "UPDATE instance_agent_metadata SET device_profile_updated_at = updated_at WHERE device_profile <> '' AND device_profile_updated_at IS NULL",
    )
    .execute(db)
    .await?;

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
    migrate_agent_secret_verifiers(db).await?;

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
            started_at BIGINT,
            completed_at BIGINT,
            output TEXT NOT NULL DEFAULT '',
            exit_code BIGINT,
            agent_connection_id TEXT
        );
        "#,
    )
    .execute(db)
    .await?;
    sqlx::query("ALTER TABLE command_jobs ADD COLUMN IF NOT EXISTS started_at BIGINT")
        .execute(db)
        .await?;
    sqlx::query("ALTER TABLE command_jobs ADD COLUMN IF NOT EXISTS agent_connection_id TEXT")
        .execute(db)
        .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_command_jobs_running_started ON command_jobs(status, started_at) WHERE status = 'running'",
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
            rollout_state TEXT NOT NULL DEFAULT 'draft' CHECK(rollout_state IN (
                'draft', 'canary_active', 'canary_paused', 'full_active', 'full_paused',
                'rollback_active', 'rolled_back', 'rollback_partial'
            )),
            rollout_updated_at BIGINT,
            created_at BIGINT NOT NULL,
            published_at BIGINT
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        "ALTER TABLE agent_releases ADD COLUMN IF NOT EXISTS rollout_state TEXT NOT NULL DEFAULT 'full_active'",
    )
    .execute(db)
    .await?;
    sqlx::query("ALTER TABLE agent_releases ADD COLUMN IF NOT EXISTS rollout_updated_at BIGINT")
        .execute(db)
        .await?;
    sqlx::query(
        r#"
        UPDATE agent_releases
        SET rollout_state = CASE WHEN status = 'draft' THEN 'draft' ELSE rollout_state END,
            rollout_updated_at = COALESCE(rollout_updated_at, published_at, created_at)
        "#,
    )
    .execute(db)
    .await?;
    sqlx::query(
        r#"
        DO $$
        BEGIN
            IF NOT EXISTS (
                SELECT 1 FROM pg_constraint
                WHERE conrelid = 'agent_releases'::regclass
                  AND conname = 'agent_releases_rollout_state_check'
            ) THEN
                ALTER TABLE agent_releases
                ADD CONSTRAINT agent_releases_rollout_state_check CHECK (
                    rollout_state IN (
                        'draft', 'canary_active', 'canary_paused', 'full_active',
                        'full_paused', 'rollback_active', 'rolled_back', 'rollback_partial'
                    )
                );
            END IF;
        END
        $$
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
        CREATE TABLE IF NOT EXISTS agent_release_targets (
            release_id TEXT NOT NULL REFERENCES agent_releases(id) ON DELETE CASCADE,
            instance_id TEXT NOT NULL REFERENCES instances(id) ON DELETE CASCADE,
            state TEXT NOT NULL CHECK(state IN ('included', 'excluded')),
            created_at BIGINT NOT NULL,
            updated_at BIGINT NOT NULL,
            PRIMARY KEY(release_id, instance_id)
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_update_attempts (
            id TEXT PRIMARY KEY,
            release_id TEXT NOT NULL REFERENCES agent_releases(id) ON DELETE CASCADE,
            artifact_id TEXT REFERENCES agent_artifacts(id) ON DELETE CASCADE,
            instance_id TEXT NOT NULL REFERENCES instances(id) ON DELETE CASCADE,
            operation TEXT NOT NULL DEFAULT 'upgrade' CHECK(operation IN ('upgrade', 'rollback')),
            parent_attempt_id TEXT REFERENCES agent_update_attempts(id) ON DELETE SET NULL,
            from_version TEXT NOT NULL,
            target_version TEXT NOT NULL,
            status TEXT NOT NULL,
            message TEXT NOT NULL DEFAULT '',
            retry_count BIGINT NOT NULL DEFAULT 0,
            created_at BIGINT NOT NULL,
            updated_at BIGINT NOT NULL,
            completed_at BIGINT
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        "ALTER TABLE agent_update_attempts ADD COLUMN IF NOT EXISTS operation TEXT NOT NULL DEFAULT 'upgrade'",
    )
    .execute(db)
    .await?;
    sqlx::query(
        "ALTER TABLE agent_update_attempts ADD COLUMN IF NOT EXISTS parent_attempt_id TEXT",
    )
    .execute(db)
    .await?;
    sqlx::query(
        r#"
        DO $$
        BEGIN
            IF NOT EXISTS (
                SELECT 1 FROM pg_constraint
                WHERE conrelid = 'agent_update_attempts'::regclass
                  AND conname = 'agent_update_attempts_operation_check'
            ) THEN
                ALTER TABLE agent_update_attempts
                ADD CONSTRAINT agent_update_attempts_operation_check
                CHECK (operation IN ('upgrade', 'rollback'));
            END IF;
            IF NOT EXISTS (
                SELECT 1 FROM pg_constraint
                WHERE conrelid = 'agent_update_attempts'::regclass
                  AND conname = 'agent_update_attempts_parent_attempt_id_fkey'
            ) THEN
                ALTER TABLE agent_update_attempts
                ADD CONSTRAINT agent_update_attempts_parent_attempt_id_fkey
                FOREIGN KEY (parent_attempt_id) REFERENCES agent_update_attempts(id)
                ON DELETE SET NULL;
            END IF;
        END
        $$
        "#,
    )
    .execute(db)
    .await?;
    sqlx::query("ALTER TABLE agent_update_attempts ALTER COLUMN artifact_id DROP NOT NULL")
        .execute(db)
        .await?;
    sqlx::query(
        "ALTER TABLE agent_update_attempts DROP CONSTRAINT IF EXISTS agent_update_attempts_release_id_instance_id_key",
    )
    .execute(db)
    .await?;
    sqlx::query(
        r#"
        WITH superseded AS (
            SELECT id
            FROM (
                SELECT id, row_number() OVER (
                    PARTITION BY instance_id ORDER BY updated_at DESC, created_at DESC, id DESC
                ) AS position
                FROM agent_update_attempts
                WHERE status NOT IN ('succeeded', 'rollback_succeeded', 'failed', 'cancelled')
            ) ranked
            WHERE position > 1
        )
        UPDATE agent_update_attempts
        SET status = 'cancelled', message = '由数据库迁移取消的重复活动任务',
            completed_at = COALESCE(completed_at, updated_at)
        WHERE id IN (SELECT id FROM superseded)
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_attempts_one_active_per_instance
        ON agent_update_attempts(instance_id)
        WHERE status NOT IN ('succeeded', 'rollback_succeeded', 'failed', 'cancelled');
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        WITH duplicate_rollbacks AS (
            SELECT id
            FROM (
                SELECT id, row_number() OVER (
                    PARTITION BY parent_attempt_id ORDER BY updated_at DESC, created_at DESC, id DESC
                ) AS position
                FROM agent_update_attempts
                WHERE operation = 'rollback' AND parent_attempt_id IS NOT NULL
            ) ranked
            WHERE position > 1
        )
        UPDATE agent_update_attempts
        SET parent_attempt_id = NULL,
            message = CASE
                WHEN message = '' THEN '由数据库迁移解除的重复回滚父任务关联'
                ELSE message
            END
        WHERE id IN (SELECT id FROM duplicate_rollbacks)
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_attempts_one_rollback_per_parent
        ON agent_update_attempts(parent_attempt_id)
        WHERE operation = 'rollback' AND parent_attempt_id IS NOT NULL;
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
    let now = now_ts();
    sqlx::query(
        r#"
        UPDATE command_jobs
        SET status = 'failed', completed_at = $1, exit_code = -1,
            output = '后端服务重启，命令任务已终止'
        WHERE status IN ('queued', 'running')
        "#,
    )
    .bind(now)
    .execute(db)
    .await?;

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
            &["created_at", "started_at", "completed_at", "exit_code"][..],
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
        (
            "agent_releases",
            &["created_at", "published_at", "rollout_updated_at"][..],
        ),
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
        ("rollback_supported", "BIGINT NOT NULL DEFAULT 0"),
        ("rollback_version", "TEXT NOT NULL DEFAULT ''"),
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
    let secret_verifier = agent_secret_verifier(&payload.secret);
    let mut tx = db.begin().await?;
    let mut instance = sqlx::query_as::<_, InstanceRecord>(
        r#"
        SELECT id, secret, name, region, country_code, country, province_code, province, city,
               remark, hostname, os, arch, agent_version,
               package_type, native_arch, update_privileged, rollback_supported, rollback_version,
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
                       package_type, native_arch, update_privileged, rollback_supported, rollback_version,
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
                rollback_supported = COALESCE($9, 0),
                rollback_version = CASE
                    WHEN $9 = 1 THEN COALESCE($10, '')
                    ELSE ''
                END,
                last_seen = $11
            WHERE id = $12
            "#,
        )
        .bind(&secret_verifier)
        .bind(&payload.hostname)
        .bind(&payload.os)
        .bind(&payload.arch)
        .bind(&payload.agent_version)
        .bind(payload.package_type.as_deref())
        .bind(payload.native_arch.as_deref())
        .bind(payload.update_privileged.map(i64::from))
        .bind(payload.rollback_supported.map(i64::from))
        .bind(payload.rollback_version.as_deref())
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
        .bind(&secret_verifier)
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
        .bind(&secret_verifier)
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
    if let Some(value) = payload.rollback_version.as_deref() {
        validate_agent_field("rollback_version", value, 64, true)?;
    }
    if payload.rollback_supported != Some(true)
        && payload
            .rollback_version
            .as_deref()
            .is_some_and(|value| !value.is_empty())
    {
        return Err(AppError::bad_request(
            "不支持回滚的 Agent 不能上报本地回滚版本",
        ));
    }
    if let Some(profile) = payload.device_profile.as_ref() {
        validate_device_profile(profile)?;
    }
    Ok(())
}

fn validate_device_profile(profile: &DeviceProfile) -> AppResult<()> {
    if profile.schema_version != 1 {
        return Err(AppError::bad_request("设备资料版本不受支持"));
    }
    if profile.collected_at <= 0
        || profile.memory_total < 0
        || profile.storage_total < 0
        || profile.cpu.logical_cores > 65_536
        || profile.gpus.len() > MAX_DEVICE_GPUS
        || profile.disks.len() > MAX_DEVICE_DISKS
        || profile.network_interfaces.len() > MAX_DEVICE_INTERFACES
    {
        return Err(AppError::bad_request("设备资料格式无效"));
    }
    for value in [
        &profile.system.os_name,
        &profile.system.os_version,
        &profile.system.kernel_version,
        &profile.system.architecture,
        &profile.cpu.model,
        &profile.cpu.vendor,
    ] {
        validate_device_text(value, true)?;
    }
    for gpu in &profile.gpus {
        validate_device_text(&gpu.name, false)?;
        validate_device_text(&gpu.vendor, true)?;
        if gpu.memory_total.is_some_and(|value| value < 0) {
            return Err(AppError::bad_request("GPU 设备资料格式无效"));
        }
    }
    for disk in &profile.disks {
        validate_device_text(&disk.name, true)?;
        validate_device_text(&disk.mount_point, true)?;
        validate_device_text(&disk.file_system, true)?;
        validate_device_text(&disk.kind, true)?;
        if disk.total_bytes < 0 {
            return Err(AppError::bad_request("磁盘设备资料格式无效"));
        }
    }
    for interface in &profile.network_interfaces {
        validate_device_text(&interface.name, false)?;
        if interface.ipv4.len() > MAX_DEVICE_INTERFACE_ADDRESSES
            || interface.ipv6.len() > MAX_DEVICE_INTERFACE_ADDRESSES
        {
            return Err(AppError::bad_request("网卡地址数量超过限制"));
        }
        if let Some(mac) = interface.mac_address.as_deref()
            && !valid_mac_address(mac)
        {
            return Err(AppError::bad_request("MAC 地址格式无效"));
        }
        for address in &interface.ipv4 {
            if !address.parse::<IpAddr>().is_ok_and(|address| {
                address.is_ipv4() && !address.is_loopback() && !address.is_unspecified()
            }) {
                return Err(AppError::bad_request("IPv4 地址格式无效"));
            }
        }
        for address in &interface.ipv6 {
            if !address.parse::<IpAddr>().is_ok_and(|address| {
                address.is_ipv6() && !address.is_loopback() && !address.is_unspecified()
            }) {
                return Err(AppError::bad_request("IPv6 地址格式无效"));
            }
        }
    }
    let encoded =
        serde_json::to_vec(profile).map_err(|_| AppError::bad_request("设备资料无法序列化"))?;
    if encoded.len() > MAX_DEVICE_PROFILE_BYTES {
        return Err(AppError::bad_request("设备资料超过大小限制"));
    }
    Ok(())
}

fn validate_device_text(value: &str, allow_empty: bool) -> AppResult<()> {
    if (!allow_empty && value.trim().is_empty())
        || value.len() > MAX_DEVICE_PROFILE_STRING_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(AppError::bad_request("设备资料文本格式无效"));
    }
    Ok(())
}

fn valid_mac_address(value: &str) -> bool {
    let bytes = value
        .split(':')
        .map(|part| {
            (part.len() == 2)
                .then(|| u8::from_str_radix(part, 16).ok())
                .flatten()
        })
        .collect::<Option<Vec<_>>>();
    let Some(bytes) = bytes.filter(|bytes| bytes.len() == 6) else {
        return false;
    };
    bytes.iter().any(|byte| *byte != 0)
        && bytes.iter().any(|byte| *byte != u8::MAX)
        && bytes[0] & 1 == 0
}

fn agent_secret_is_authorized(current_secret: &str, payload: &AgentRegisterRequest) -> bool {
    agent_secret_matches(current_secret, &payload.secret)
        || payload
            .previous_secret
            .as_deref()
            .is_some_and(|secret| agent_secret_matches(current_secret, secret))
}

pub fn agent_secret_verifier(secret: &str) -> String {
    format!(
        "{AGENT_SECRET_VERIFIER_PREFIX}{:x}",
        Sha256::digest(secret.as_bytes())
    )
}

pub fn agent_secret_matches(stored: &str, presented: &str) -> bool {
    if let Some(expected) = agent_secret_digest(stored) {
        let actual = format!("{:x}", Sha256::digest(presented.as_bytes()));
        return bool::from(expected.as_bytes().ct_eq(actual.as_bytes()));
    }
    bool::from(stored.as_bytes().ct_eq(presented.as_bytes()))
}

fn agent_secret_digest(stored: &str) -> Option<&str> {
    stored
        .strip_prefix(AGENT_SECRET_VERIFIER_PREFIX)
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

async fn migrate_agent_secret_verifiers(db: &PgPool) -> anyhow::Result<()> {
    for table in ["instances", "pending_instances"] {
        let rows =
            sqlx::query_as::<_, (String, String)>(&format!("SELECT id, secret FROM {table}"))
                .fetch_all(db)
                .await?;
        for (id, secret) in rows {
            if agent_secret_digest(&secret).is_some() {
                continue;
            }
            sqlx::query(&format!(
                "UPDATE {table} SET secret = $1 WHERE id = $2 AND secret = $3"
            ))
            .bind(agent_secret_verifier(&secret))
            .bind(id)
            .bind(secret)
            .execute(db)
            .await?;
        }
    }
    Ok(())
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
               package_type, native_arch, update_privileged, rollback_supported, rollback_version,
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

pub async fn upsert_agent_device_profile(
    db: &PgPool,
    instance_id: &str,
    profile: Option<&DeviceProfile>,
    observed_ip: IpAddr,
) -> AppResult<()> {
    let encoded_profile = profile
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| AppError::bad_request("设备资料无法序列化"))?;
    let now = now_ts();
    let profile_updated_at = profile.map(|_| now);
    sqlx::query(
        r#"
        INSERT INTO instance_agent_metadata(
            instance_id, capabilities, device_profile, observed_ip,
            device_profile_updated_at, updated_at
        )
        VALUES($1, '', COALESCE($2, ''), $3, $4, $5)
        ON CONFLICT(instance_id) DO UPDATE SET
            device_profile = COALESCE($2, instance_agent_metadata.device_profile),
            observed_ip = EXCLUDED.observed_ip,
            device_profile_updated_at = COALESCE($4, instance_agent_metadata.device_profile_updated_at),
            updated_at = EXCLUDED.updated_at
        "#,
    )
    .bind(instance_id)
    .bind(encoded_profile)
    .bind(observed_ip.to_string())
    .bind(profile_updated_at)
    .bind(now)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn upsert_agent_capabilities(
    db: &PgPool,
    instance_id: &str,
    capabilities: &[String],
) -> AppResult<()> {
    sqlx::query(
        r#"
        INSERT INTO instance_agent_metadata(
            instance_id, capabilities, device_profile, observed_ip,
            device_profile_updated_at, updated_at
        )
        VALUES($1, $2, '', '', NULL, $3)
        ON CONFLICT(instance_id) DO UPDATE SET
            capabilities = EXCLUDED.capabilities,
            updated_at = EXCLUDED.updated_at
        "#,
    )
    .bind(instance_id)
    .bind(capabilities.join(","))
    .bind(now_ts())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn instance_agent_metadata(
    db: &PgPool,
    instance_id: &str,
) -> AppResult<Option<InstanceAgentMetadata>> {
    Ok(sqlx::query_as::<_, InstanceAgentMetadata>(
        r#"
        SELECT device_profile, observed_ip, device_profile_updated_at
        FROM instance_agent_metadata
        WHERE instance_id = $1
        "#,
    )
    .bind(instance_id)
    .fetch_optional(db)
    .await?)
}

pub async fn instance_capabilities(
    db: &PgPool,
    instance_ids: &[String],
) -> AppResult<HashMap<String, Vec<String>>> {
    if instance_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT instance_id, capabilities
        FROM instance_agent_metadata
        WHERE instance_id = ANY($1)
        "#,
    )
    .bind(instance_ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(instance_id, encoded)| {
            let capabilities = encoded
                .split(',')
                .filter(|capability| !capability.is_empty())
                .map(str::to_string)
                .collect();
            (instance_id, capabilities)
        })
        .collect())
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

pub async fn latest_metrics(
    db: &PgPool,
    instance_ids: &[String],
) -> AppResult<HashMap<String, MetricRecord>> {
    if instance_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, InstanceMetricRecord>(
        r#"
        SELECT DISTINCT ON (instance_id)
               instance_id, ts, cpu_percent, memory_used, memory_total, disk_used, disk_total,
               network_rx, network_tx, gpu_percent, gpu_memory_used, gpu_memory_total,
               uptime_seconds, load_average, latency_ms
        FROM metrics
        WHERE instance_id = ANY($1)
        ORDER BY instance_id, ts DESC
        "#,
    )
    .bind(instance_ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.instance_id, row.metric))
        .collect())
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

pub async fn write_action_log<'e, E>(
    executor: E,
    actor: &str,
    action: &str,
    target: &str,
    detail: &str,
) -> AppResult<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(
        "INSERT INTO action_logs(id, actor, action, target, detail, created_at) VALUES($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(actor)
    .bind(action)
    .bind(target)
    .bind(detail)
    .bind(now_ts())
    .execute(executor)
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
            rollback_supported: Some(true),
            rollback_version: Some("0.0.9".to_string()),
            device_profile: None,
        }
    }

    fn device_profile() -> DeviceProfile {
        DeviceProfile {
            schema_version: 1,
            collected_at: 100,
            system: crate::models::DeviceSystemInfo {
                os_name: "Linux".to_string(),
                os_version: "6.8".to_string(),
                kernel_version: "6.8.0".to_string(),
                architecture: "x86_64".to_string(),
            },
            cpu: crate::models::DeviceCpuInfo {
                model: "CPU".to_string(),
                vendor: "Vendor".to_string(),
                physical_cores: Some(2),
                logical_cores: 4,
                frequency_mhz: Some(3200),
            },
            memory_total: 1024,
            storage_total: 2048,
            gpus: Vec::new(),
            disks: Vec::new(),
            network_interfaces: vec![crate::models::DeviceNetworkInterface {
                name: "eth0".to_string(),
                mac_address: Some("AA:BB:CC:DD:EE:FF".to_string()),
                ipv4: vec!["192.0.2.1".to_string()],
                ipv6: Vec::new(),
            }],
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
    fn validates_device_profile_limits_and_network_privacy_inputs() {
        let mut payload = registration_payload();
        payload.device_profile = Some(device_profile());
        assert!(validate_agent_registration(&payload).is_ok());

        payload.device_profile.as_mut().unwrap().network_interfaces[0].ipv4[0] =
            "127.0.0.1".to_string();
        assert!(validate_agent_registration(&payload).is_err());

        payload.device_profile = Some(device_profile());
        payload.device_profile.as_mut().unwrap().network_interfaces[0].ipv4[0] =
            "0.0.0.0".to_string();
        assert!(validate_agent_registration(&payload).is_err());

        payload.device_profile = Some(device_profile());
        payload.device_profile.as_mut().unwrap().network_interfaces[0].mac_address =
            Some("FF:FF:FF:FF:FF:FF".to_string());
        assert!(validate_agent_registration(&payload).is_err());
    }

    #[test]
    fn secret_rotation_requires_the_current_secret() {
        let mut payload = registration_payload();
        payload.secret = "new-agent-secret".to_string();
        let current = agent_secret_verifier("current-agent-secret");
        assert!(!agent_secret_is_authorized(&current, &payload));

        payload.previous_secret = Some("wrong-agent-secret".to_string());
        assert!(!agent_secret_is_authorized(&current, &payload));

        payload.previous_secret = Some("current-agent-secret".to_string());
        assert!(agent_secret_is_authorized(&current, &payload));
    }

    #[test]
    fn agent_secret_verifiers_are_versioned_and_constant_time_comparable() {
        let verifier = agent_secret_verifier("550e8400-e29b-41d4-a716-446655440001");
        assert!(verifier.starts_with(AGENT_SECRET_VERIFIER_PREFIX));
        assert!(!verifier.contains("550e8400"));
        assert!(agent_secret_matches(
            &verifier,
            "550e8400-e29b-41d4-a716-446655440001"
        ));
        assert!(!agent_secret_matches(&verifier, "different-secret"));
        assert!(agent_secret_matches("legacy-secret", "legacy-secret"));
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
    async fn agent_metadata_preserves_profiles_and_replaces_explicit_capabilities() {
        let db = PgPoolOptions::new()
            .max_connections(1)
            .connect("postgresql://localhost/postgres")
            .await
            .expect("connect database");
        init_db(&db).await.expect("initialize database");
        let instance_id = format!("metadata-test-{}", Uuid::new_v4());
        sqlx::query(
            r#"
            INSERT INTO instances(id, secret, name, hostname, os, arch, agent_version,
                                  approved, disabled, first_seen)
            VALUES($1, 'secret', 'Metadata test', 'host', 'linux', 'x86_64', '0.1.21',
                   1, 0, $2)
            "#,
        )
        .bind(&instance_id)
        .bind(now_ts())
        .execute(&db)
        .await
        .expect("insert test instance");

        let profile = device_profile();
        upsert_agent_device_profile(
            &db,
            &instance_id,
            Some(&profile),
            "192.0.2.10".parse().unwrap(),
        )
        .await
        .expect("store device profile");
        sqlx::query(
            "UPDATE instance_agent_metadata SET device_profile_updated_at = 42 WHERE instance_id = $1",
        )
        .bind(&instance_id)
        .execute(&db)
        .await
        .expect("set stable profile timestamp");
        upsert_agent_capabilities(
            &db,
            &instance_id,
            &[
                "file_manager_v1".to_string(),
                "docker_manager_v1".to_string(),
            ],
        )
        .await
        .expect("store capabilities");

        upsert_agent_device_profile(&db, &instance_id, None, "192.0.2.11".parse().unwrap())
            .await
            .expect("refresh legacy agent metadata");
        let metadata = instance_agent_metadata(&db, &instance_id)
            .await
            .expect("read metadata")
            .expect("metadata exists");
        assert_eq!(
            serde_json::from_str::<DeviceProfile>(&metadata.device_profile).unwrap(),
            profile
        );
        assert_eq!(metadata.observed_ip, "192.0.2.11");
        assert_eq!(metadata.device_profile_updated_at, Some(42));
        assert_eq!(
            instance_capabilities(&db, std::slice::from_ref(&instance_id))
                .await
                .expect("read capabilities")[&instance_id],
            ["file_manager_v1", "docker_manager_v1"]
        );

        upsert_agent_capabilities(&db, &instance_id, &[])
            .await
            .expect("clear capabilities");
        assert!(
            instance_capabilities(&db, std::slice::from_ref(&instance_id))
                .await
                .expect("read cleared capabilities")[&instance_id]
                .is_empty()
        );

        sqlx::query("DELETE FROM instances WHERE id = $1")
            .bind(&instance_id)
            .execute(&db)
            .await
            .expect("delete test instance");
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
        assert_eq!(stored_secret, agent_secret_verifier(&original.secret));
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
        assert_ne!(record.secret, "secret");
        assert!(agent_secret_matches(&record.secret, "secret"));
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
            rollback_supported: Some(false),
            rollback_version: None,
            device_profile: None,
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
