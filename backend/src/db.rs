use std::{fs, str::FromStr, time::Duration};

use anyhow::Context;
use axum::http::StatusCode;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
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

pub async fn connect_db(database_url: &str) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);

    if let Some(parent) = options
        .get_filename()
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create database directory {}", parent.display()))?;
    }

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
            update_privileged INTEGER NOT NULL DEFAULT 0,
            approved INTEGER NOT NULL DEFAULT 1,
            disabled INTEGER NOT NULL DEFAULT 0,
            first_seen INTEGER NOT NULL,
            last_seen INTEGER
        );
        "#,
    )
    .execute(db)
    .await?;

    ensure_instance_location_columns(db).await?;
    ensure_capability_columns(db, "instances").await?;

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
            update_privileged INTEGER NOT NULL DEFAULT 0,
            first_seen INTEGER NOT NULL,
            last_seen INTEGER NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;

    ensure_capability_columns(db, "pending_instances").await?;

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

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_releases (
            id TEXT PRIMARY KEY,
            version TEXT NOT NULL UNIQUE,
            notes TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT 'draft' CHECK(status IN ('draft', 'published')),
            created_at INTEGER NOT NULL,
            published_at INTEGER
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
            size_bytes INTEGER NOT NULL,
            sha256 TEXT NOT NULL,
            storage_path TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            UNIQUE(release_id, os, package_type, native_arch)
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
            artifact_id TEXT NOT NULL REFERENCES agent_artifacts(id) ON DELETE CASCADE,
            instance_id TEXT NOT NULL REFERENCES instances(id) ON DELETE CASCADE,
            from_version TEXT NOT NULL,
            target_version TEXT NOT NULL,
            status TEXT NOT NULL,
            message TEXT NOT NULL DEFAULT '',
            retry_count INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            completed_at INTEGER,
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

    sqlx::query("INSERT OR IGNORE INTO settings(key, value) VALUES('retention_days', '30');")
        .execute(db)
        .await?;

    Ok(())
}

async fn ensure_instance_location_columns(db: &SqlitePool) -> anyhow::Result<()> {
    let columns =
        sqlx::query_scalar::<_, String>("SELECT name FROM pragma_table_info('instances')")
            .fetch_all(db)
            .await?;

    for (name, definition) in [
        ("country_code", "TEXT NOT NULL DEFAULT ''"),
        ("country", "TEXT NOT NULL DEFAULT ''"),
        ("province_code", "TEXT NOT NULL DEFAULT ''"),
        ("province", "TEXT NOT NULL DEFAULT ''"),
        ("city", "TEXT NOT NULL DEFAULT ''"),
    ] {
        if !columns.iter().any(|column| column == name) {
            sqlx::query(&format!(
                "ALTER TABLE instances ADD COLUMN {name} {definition}"
            ))
            .execute(db)
            .await?;
        }
    }

    Ok(())
}

async fn ensure_capability_columns(db: &SqlitePool, table: &str) -> anyhow::Result<()> {
    let columns =
        sqlx::query_scalar::<_, String>(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .fetch_all(db)
            .await?;

    for (name, definition) in [
        ("package_type", "TEXT NOT NULL DEFAULT ''"),
        ("native_arch", "TEXT NOT NULL DEFAULT ''"),
        ("update_privileged", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !columns.iter().any(|column| column == name) {
            sqlx::query(&format!(
                "ALTER TABLE {table} ADD COLUMN {name} {definition}"
            ))
            .execute(db)
            .await?;
        }
    }

    Ok(())
}

pub async fn register_or_touch_pending(
    db: &SqlitePool,
    payload: &AgentRegisterRequest,
) -> AppResult<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let instance = sqlx::query_as::<_, InstanceRecord>(
        r#"
        SELECT id, secret, name, region, country_code, country, province_code, province, city,
               remark, hostname, os, arch, agent_version,
               package_type, native_arch, update_privileged,
               approved, disabled, first_seen, last_seen
        FROM instances
        WHERE id = ?
        "#,
    )
    .bind(&payload.instance_id)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some(instance) = instance {
        if instance.secret != payload.secret {
            return Err(AppError::new(StatusCode::UNAUTHORIZED, "实例密钥不匹配"));
        }
        sqlx::query(
            r#"
            UPDATE instances
            SET hostname = ?, os = ?, arch = ?, agent_version = ?,
                package_type = COALESCE(?, package_type),
                native_arch = COALESCE(?, native_arch),
                update_privileged = COALESCE(?, update_privileged),
                last_seen = ?
            WHERE id = ?
            "#,
        )
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
        sqlx::query("DELETE FROM pending_instances WHERE id = ?")
            .bind(&payload.instance_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(());
    }

    let now = now_ts();
    sqlx::query(
        r#"
        INSERT INTO pending_instances(id, secret, hostname, os, arch, agent_version, package_type,
                                      native_arch, update_privileged, first_seen, last_seen)
        VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            secret = excluded.secret,
            hostname = excluded.hostname,
            os = excluded.os,
            arch = excluded.arch,
            agent_version = excluded.agent_version,
            package_type = excluded.package_type,
            native_arch = excluded.native_arch,
            update_privileged = excluded.update_privileged,
            last_seen = excluded.last_seen
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
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(())
}

pub async fn approve_pending_instance(
    db: &SqlitePool,
    id: &str,
) -> AppResult<Option<PendingInstanceSecret>> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let pending = sqlx::query_as::<_, PendingInstanceSecret>(
        r#"
        SELECT id, secret, hostname, os, arch, agent_version, package_type, native_arch,
               update_privileged, first_seen, last_seen
        FROM pending_instances
        WHERE id = ?
        "#,
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(pending) = pending else {
        tx.commit().await?;
        return Ok(None);
    };

    sqlx::query(
        r#"
        INSERT INTO instances(id, secret, name, region, country_code, country, province_code,
                              province, city, remark, hostname, os, arch, agent_version,
                              package_type, native_arch, update_privileged, approved, disabled,
                              first_seen, last_seen)
        VALUES(?, ?, ?, '', '', '', '', '', '', '', ?, ?, ?, ?, ?, ?, ?, 1, 0, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            secret = excluded.secret,
            hostname = excluded.hostname,
            os = excluded.os,
            arch = excluded.arch,
            agent_version = excluded.agent_version,
            package_type = excluded.package_type,
            native_arch = excluded.native_arch,
            update_privileged = excluded.update_privileged,
            approved = 1,
            disabled = 0,
            last_seen = excluded.last_seen
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

    sqlx::query("DELETE FROM pending_instances WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(Some(pending))
}

pub async fn get_instance(db: &SqlitePool, id: &str) -> AppResult<InstanceRecord> {
    get_instance_optional(db, id)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "实例不存在"))
}

pub async fn get_instance_optional(db: &SqlitePool, id: &str) -> AppResult<Option<InstanceRecord>> {
    let record = sqlx::query_as::<_, InstanceRecord>(
        r#"
        SELECT id, secret, name, region, country_code, country, province_code, province, city,
               remark, hostname, os, arch, agent_version,
               package_type, native_arch, update_privileged,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn init_db_migrates_existing_instance_locations() {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
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
    }

    #[tokio::test]
    async fn approved_instance_is_not_recreated_as_pending_by_concurrent_registration() {
        let db = SqlitePoolOptions::new()
            .max_connections(4)
            .connect("sqlite::memory:?cache=shared")
            .await
            .expect("connect in-memory database");
        init_db(&db).await.expect("initialize database");
        let payload = AgentRegisterRequest {
            instance_id: "agent-1".to_string(),
            secret: "secret-1".to_string(),
            hostname: "host-1".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            agent_version: "0.1.0".to_string(),
            package_type: Some("standalone".to_string()),
            native_arch: Some("x86_64".to_string()),
            update_privileged: Some(true),
        };

        register_or_touch_pending(&db, &payload)
            .await
            .expect("create pending instance");
        let (approved, registered) = tokio::join!(
            approve_pending_instance(&db, &payload.instance_id),
            register_or_touch_pending(&db, &payload),
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
            sqlx::query_scalar("SELECT COUNT(*) FROM pending_instances WHERE id = ?")
                .bind(&payload.instance_id)
                .fetch_one(&db)
                .await
                .expect("count pending instances");
        assert_eq!(pending_count, 0);
    }
}
