use std::{str::FromStr, time::Duration};

use axum::http::StatusCode;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use tracing::error;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::{AgentRegisterRequest, InstanceRecord, InstanceSummary, MetricRecord, SettingsRow},
    state::AppState,
    utils::now_ts,
};

pub async fn connect_db(database_url: &str) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);

    Ok(SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?)
}

pub async fn init_db(db: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS instances (
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
        );
        "#,
    )
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
            first_seen INTEGER NOT NULL,
            last_seen INTEGER NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS metrics (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            instance_id TEXT NOT NULL,
            ts INTEGER NOT NULL,
            cpu_percent REAL NOT NULL,
            memory_used INTEGER NOT NULL,
            memory_total INTEGER NOT NULL,
            disk_used INTEGER NOT NULL,
            disk_total INTEGER NOT NULL,
            network_rx INTEGER NOT NULL,
            network_tx INTEGER NOT NULL,
            gpu_percent REAL,
            gpu_memory_used INTEGER,
            gpu_memory_total INTEGER,
            uptime_seconds INTEGER NOT NULL,
            load_average REAL
        );
        "#,
    )
    .execute(db)
    .await?;

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
            enabled INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL
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
            created_at INTEGER NOT NULL,
            completed_at INTEGER,
            output TEXT NOT NULL DEFAULT '',
            exit_code INTEGER
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
            started_at INTEGER NOT NULL,
            ended_at INTEGER
        );
        "#,
    )
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
            created_at INTEGER NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query("INSERT OR IGNORE INTO settings(key, value) VALUES('retention_days', '30');")
        .execute(db)
        .await?;

    Ok(())
}

pub async fn register_or_touch_pending(
    db: &SqlitePool,
    payload: &AgentRegisterRequest,
) -> AppResult<()> {
    if let Some(instance) = get_instance_optional(db, &payload.instance_id).await? {
        if instance.secret != payload.secret {
            return Err(AppError::new(StatusCode::UNAUTHORIZED, "实例密钥不匹配"));
        }
        return Ok(());
    }

    let now = now_ts();
    sqlx::query(
        r#"
        INSERT INTO pending_instances(id, secret, hostname, os, arch, agent_version, first_seen, last_seen)
        VALUES(?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            secret = excluded.secret,
            hostname = excluded.hostname,
            os = excluded.os,
            arch = excluded.arch,
            agent_version = excluded.agent_version,
            last_seen = excluded.last_seen
        "#,
    )
    .bind(&payload.instance_id)
    .bind(&payload.secret)
    .bind(&payload.hostname)
    .bind(&payload.os)
    .bind(&payload.arch)
    .bind(&payload.agent_version)
    .bind(now)
    .bind(now)
    .execute(db)
    .await?;

    Ok(())
}

pub async fn get_instance(db: &SqlitePool, id: &str) -> AppResult<InstanceRecord> {
    get_instance_optional(db, id)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "实例不存在"))
}

pub async fn get_instance_optional(db: &SqlitePool, id: &str) -> AppResult<Option<InstanceRecord>> {
    let record = sqlx::query_as::<_, InstanceRecord>(
        r#"
        SELECT id, secret, name, region, remark, hostname, os, arch, agent_version,
               approved, disabled, first_seen, last_seen
        FROM instances
        WHERE id = ?
        "#,
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    Ok(record)
}

pub async fn latest_metric(db: &SqlitePool, instance_id: &str) -> AppResult<Option<MetricRecord>> {
    let metric = sqlx::query_as::<_, MetricRecord>(
        r#"
        SELECT ts, cpu_percent, memory_used, memory_total, disk_used, disk_total,
               network_rx, network_tx, gpu_percent, gpu_memory_used, gpu_memory_total,
               uptime_seconds, load_average
        FROM metrics
        WHERE instance_id = ?
        ORDER BY ts DESC
        LIMIT 1
        "#,
    )
    .bind(instance_id)
    .fetch_optional(db)
    .await?;
    Ok(metric)
}

pub fn instance_summary(
    record: InstanceRecord,
    metrics: Option<MetricRecord>,
    online: bool,
) -> InstanceSummary {
    InstanceSummary {
        id: record.id,
        name: record.name,
        region: record.region,
        remark: record.remark,
        hostname: record.hostname,
        os: record.os,
        arch: record.arch,
        agent_version: record.agent_version,
        online,
        first_seen: record.first_seen,
        last_seen: record.last_seen,
        metrics,
    }
}

pub async fn retention_days(db: &SqlitePool) -> AppResult<i64> {
    let row =
        sqlx::query_as::<_, SettingsRow>("SELECT value FROM settings WHERE key = 'retention_days'")
            .fetch_optional(db)
            .await?;
    Ok(row
        .and_then(|row| row.value.parse::<i64>().ok())
        .unwrap_or(30)
        .clamp(1, 365))
}

pub async fn setting_value(db: &SqlitePool, key: &str) -> AppResult<Option<String>> {
    let row = sqlx::query_as::<_, SettingsRow>("SELECT value FROM settings WHERE key = ?")
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
                if let Err(error) = sqlx::query("DELETE FROM metrics WHERE ts < ?")
                    .bind(cutoff)
                    .execute(&state.db)
                    .await
                {
                    error!(?error, "failed to clean old metrics");
                }
            }
            Err(error) => error!(?error, "failed to read retention setting"),
        }
    }
}

pub async fn write_action_log(
    db: &SqlitePool,
    actor: &str,
    action: &str,
    target: &str,
    detail: &str,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO action_logs(id, actor, action, target, detail, created_at) VALUES(?, ?, ?, ?, ?, ?)",
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
