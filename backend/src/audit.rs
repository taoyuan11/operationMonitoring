use std::{fmt::Write as _, future::Future, io};

use async_stream::stream;
use axum::{
    Json,
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::Response,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Executor, FromRow, PgPool, Postgres, QueryBuilder};
use tracing::warn;
use uuid::Uuid;

use crate::{
    auth::require_admin,
    error::{AppError, AppResult},
    state::AppState,
    utils::now_ts,
};

pub const DEFAULT_AUDIT_RETENTION_DAYS: i64 = 180;
pub const MAX_AUDIT_RETENTION_DAYS: i64 = 3650;
pub const DEFAULT_AUDIT_PAGE_SIZE: i64 = 50;
pub const MAX_AUDIT_PAGE_SIZE: i64 = 200;
const MAX_EXPORT_ROWS: i64 = 100_000;
const EXPORT_BATCH_SIZE: i64 = 1_000;
const MAX_USER_AGENT_BYTES: usize = 512;
const AUDIT_SCHEMA_VERSION: &str = "2";

tokio::task_local! {
    static REQUEST_CONTEXT: AuditContext;
}

#[derive(Clone, Debug, Default)]
pub struct AuditContext {
    pub request_id: Option<String>,
    pub source_ip: Option<String>,
    pub user_agent: Option<String>,
}

impl AuditContext {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let request_id = headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| Uuid::parse_str(value).ok())
            .map(|value| value.to_string());
        let source_ip = headers
            .get("x-audit-client-ip")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let user_agent = headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .map(|value| truncate(value, MAX_USER_AGENT_BYTES));
        Self {
            request_id,
            source_ip,
            user_agent,
        }
    }
}

pub async fn with_context<F, T>(context: AuditContext, future: F) -> T
where
    F: Future<Output = T>,
{
    REQUEST_CONTEXT.scope(context, future).await
}

pub fn current_context() -> AuditContext {
    REQUEST_CONTEXT.try_with(Clone::clone).unwrap_or_default()
}

#[derive(Clone, Debug)]
pub struct AuditEventInput {
    pub category: String,
    pub kind: String,
    pub actor: String,
    pub user_id: Option<String>,
    pub action: String,
    pub target: String,
    pub detail: String,
    pub metadata: Value,
    pub instance_id: Option<String>,
    pub node_snapshot: Value,
    pub context: AuditContext,
    pub session_id: Option<String>,
    pub operation_id: Option<String>,
    pub status: String,
    pub error_code: Option<String>,
    pub error_reason: String,
    pub created_at: i64,
    pub completed_at: Option<i64>,
}

pub async fn insert_event<'e, E>(executor: E, event: &AuditEventInput) -> AppResult<String>
where
    E: Executor<'e, Database = Postgres>,
{
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO audit_events(
            id, category, kind, actor, user_id, action, target, detail, metadata,
            instance_id, node_snapshot, source_ip, user_agent, request_id,
            session_id, operation_id, status, error_code, error_reason,
            created_at, completed_at
        ) VALUES(
            $1, $2, $3, $4, $5, $6, $7, $8, $9,
            $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21
        )
        "#,
    )
    .bind(&id)
    .bind(&event.category)
    .bind(&event.kind)
    .bind(&event.actor)
    .bind(&event.user_id)
    .bind(&event.action)
    .bind(&event.target)
    .bind(truncate(&event.detail, 4 * 1024))
    .bind(&event.metadata)
    .bind(&event.instance_id)
    .bind(&event.node_snapshot)
    .bind(&event.context.source_ip)
    .bind(&event.context.user_agent)
    .bind(&event.context.request_id)
    .bind(&event.session_id)
    .bind(&event.operation_id)
    .bind(&event.status)
    .bind(&event.error_code)
    .bind(truncate(&event.error_reason, 4 * 1024))
    .bind(event.created_at)
    .bind(event.completed_at)
    .execute(executor)
    .await?;
    Ok(id)
}

pub async fn finish_event(
    db: &PgPool,
    id: &str,
    status: &str,
    error_code: Option<&str>,
    error_reason: &str,
) -> AppResult<()> {
    sqlx::query(
        r#"
        UPDATE audit_events
        SET status = $1, error_code = $2, error_reason = $3,
            completed_at = $4
        WHERE id = $5 AND status = 'running'
        "#,
    )
    .bind(status)
    .bind(error_code)
    .bind(truncate(error_reason, 4 * 1024))
    .bind(now_ts())
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn finish_session_event(
    db: &PgPool,
    session_id: &str,
    status: &str,
    error_code: Option<&str>,
    error_reason: &str,
) -> AppResult<()> {
    sqlx::query(
        r#"
        UPDATE audit_events
        SET status = $1, error_code = $2, error_reason = $3, completed_at = $4
        WHERE session_id = $5 AND status = 'running'
        "#,
    )
    .bind(status)
    .bind(error_code)
    .bind(truncate(error_reason, 4 * 1024))
    .bind(now_ts())
    .bind(session_id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn instance_snapshot(db: &PgPool, instance_id: &str) -> Value {
    sqlx::query_as::<_, (String, String, String, String, String, String, String)>(
        "SELECT id, name, hostname, os, arch, region, agent_version FROM instances WHERE id = $1",
    )
    .bind(instance_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .map(|(id, name, hostname, os, arch, region, agent_version)| {
        json!({
            "id": id,
            "name": name,
            "hostname": hostname,
            "os": os,
            "arch": arch,
            "region": region,
            "agent_version": agent_version,
        })
    })
    .unwrap_or_else(|| json!({ "id": instance_id }))
}

pub fn truncate(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

pub async fn ensure_schema(db: &PgPool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS audit_events (
            id TEXT PRIMARY KEY,
            category TEXT NOT NULL DEFAULT 'admin',
            kind TEXT NOT NULL DEFAULT 'operation',
            actor TEXT NOT NULL DEFAULT '',
            user_id TEXT,
            action TEXT NOT NULL DEFAULT '',
            target TEXT NOT NULL DEFAULT '',
            detail TEXT NOT NULL DEFAULT '',
            metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
            instance_id TEXT,
            node_snapshot JSONB NOT NULL DEFAULT '{}'::jsonb,
            source_ip TEXT,
            user_agent TEXT,
            request_id TEXT,
            session_id TEXT,
            operation_id TEXT,
            status TEXT NOT NULL DEFAULT 'success' CHECK(status IN ('running', 'success', 'partial_success', 'failed', 'cancelled')),
            error_code TEXT,
            error_reason TEXT NOT NULL DEFAULT '',
            created_at BIGINT NOT NULL,
            completed_at BIGINT
        );
        "#,
    )
    .execute(db)
    .await?;
    for statement in [
        "ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS category TEXT NOT NULL DEFAULT 'admin'",
        "ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS kind TEXT NOT NULL DEFAULT 'operation'",
        "ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS user_id TEXT",
        "ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS metadata JSONB NOT NULL DEFAULT '{}'::jsonb",
        "ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS instance_id TEXT",
        "ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS node_snapshot JSONB NOT NULL DEFAULT '{}'::jsonb",
        "ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS source_ip TEXT",
        "ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS user_agent TEXT",
        "ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS request_id TEXT",
        "ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS session_id TEXT",
        "ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS operation_id TEXT",
        "ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'success'",
        "ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS error_code TEXT",
        "ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS error_reason TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS completed_at BIGINT",
    ] {
        sqlx::query(statement).execute(db).await?;
    }
    sqlx::query(
        r#"
        CREATE OR REPLACE FUNCTION operation_monitoring_parse_json(value TEXT)
        RETURNS JSONB LANGUAGE plpgsql IMMUTABLE AS $fn$
        BEGIN
            RETURN COALESCE(NULLIF(BTRIM(value), '')::jsonb, '{}'::jsonb);
        EXCEPTION WHEN others THEN
            RETURN '{}'::jsonb;
        END;
        $fn$;
        "#,
    )
    .execute(db)
    .await?;
    for column in ["metadata", "node_snapshot"] {
        sqlx::query(&format!(
            "ALTER TABLE audit_events ALTER COLUMN {column} DROP DEFAULT"
        ))
        .execute(db)
        .await?;
        sqlx::query(&format!(
            r#"
            DO $$
            BEGIN
                IF EXISTS (
                    SELECT 1
                    FROM pg_attribute
                    WHERE attrelid = 'audit_events'::regclass
                      AND attname = '{column}'
                      AND atttypid <> 'jsonb'::regtype
                      AND NOT attisdropped
                ) THEN
                    EXECUTE 'ALTER TABLE audit_events ALTER COLUMN {column} TYPE JSONB USING operation_monitoring_parse_json({column}::text)';
                END IF;
            END
            $$;
            "#
        ))
        .execute(db)
        .await?;
        sqlx::query(&format!(
            "UPDATE audit_events SET {column} = '{{}}'::jsonb WHERE {column} IS NULL"
        ))
        .execute(db)
        .await?;
        sqlx::query(&format!(
            "ALTER TABLE audit_events ALTER COLUMN {column} SET DEFAULT '{{}}'::jsonb"
        ))
        .execute(db)
        .await?;
        sqlx::query(&format!(
            "ALTER TABLE audit_events ALTER COLUMN {column} SET NOT NULL"
        ))
        .execute(db)
        .await?;
    }
    sqlx::query("ALTER TABLE audit_events DROP CONSTRAINT IF EXISTS audit_events_user_id_fkey")
        .execute(db)
        .await?;
    sqlx::query(
        "UPDATE audit_events SET status = CASE status WHEN 'succeeded' THEN 'success' WHEN 'completed' THEN 'success' WHEN 'partial' THEN 'partial_success' WHEN 'canceled' THEN 'cancelled' ELSE status END WHERE status IN ('succeeded', 'completed', 'partial', 'canceled')",
    )
    .execute(db)
    .await?;
    sqlx::query(
        "UPDATE audit_events SET status = 'failed', error_code = COALESCE(error_code, 'legacy_status'), error_reason = CASE WHEN error_reason = '' THEN '历史状态已归一化' ELSE error_reason END WHERE status NOT IN ('running', 'success', 'partial_success', 'failed', 'cancelled')",
    )
    .execute(db)
    .await?;
    sqlx::query("UPDATE audit_events SET category = 'admin' WHERE category = 'operation'")
        .execute(db)
        .await?;
    sqlx::query(
        r#"
        DO $$
        BEGIN
            IF NOT EXISTS (
                SELECT 1 FROM pg_constraint
                WHERE conrelid = 'audit_events'::regclass
                  AND conname = 'audit_events_status_check'
            ) THEN
                ALTER TABLE audit_events
                    ADD CONSTRAINT audit_events_status_check
                    CHECK(status IN ('running', 'success', 'partial_success', 'failed', 'cancelled'));
            END IF;
        END
        $$;
        "#,
    )
    .execute(db)
    .await?;
    for statement in [
        "CREATE INDEX IF NOT EXISTS idx_audit_events_created ON audit_events(created_at DESC, id DESC)",
        "CREATE INDEX IF NOT EXISTS idx_audit_events_user_created ON audit_events(user_id, created_at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_audit_events_action_created ON audit_events(category, action, created_at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_audit_events_instance_created ON audit_events(instance_id, created_at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_audit_events_status_created ON audit_events(status, created_at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_audit_events_request ON audit_events(request_id)",
        "CREATE INDEX IF NOT EXISTS idx_audit_events_source_ip ON audit_events(source_ip)",
    ] {
        sqlx::query(statement).execute(db).await?;
    }

    let migrated: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'audit_schema_version'")
            .fetch_optional(db)
            .await?;
    let action_logs_exists = legacy_table_exists(db, "action_logs").await?;
    let ssh_sessions_exists = legacy_table_exists(db, "ssh_sessions").await?;
    let desktop_sessions_exists = legacy_table_exists(db, "desktop_sessions").await?;
    let docker_sessions_exists = legacy_table_exists(db, "docker_exec_sessions").await?;

    // Older installations may still have the action log and Docker metadata columns from
    // before the unified audit migration. Normalize those tables only when they exist; fresh
    // installations never create these compatibility tables.
    if action_logs_exists {
        for statement in [
            "ALTER TABLE action_logs ADD COLUMN IF NOT EXISTS user_id TEXT",
            "ALTER TABLE action_logs ADD COLUMN IF NOT EXISTS source_ip TEXT",
            "ALTER TABLE action_logs ADD COLUMN IF NOT EXISTS user_agent TEXT",
            "ALTER TABLE action_logs ADD COLUMN IF NOT EXISTS request_id TEXT",
        ] {
            sqlx::query(statement).execute(db).await?;
        }
    }
    if docker_sessions_exists {
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
    }
    let legacy_exists = action_logs_exists
        || ssh_sessions_exists
        || desktop_sessions_exists
        || docker_sessions_exists;
    if migrated.as_deref() != Some(AUDIT_SCHEMA_VERSION) || legacy_exists {
        let mut migration = db.begin().await?;
        if !action_logs_exists {
            sqlx::query(
                "CREATE TEMP TABLE action_logs (id TEXT PRIMARY KEY, actor TEXT NOT NULL, action TEXT NOT NULL, target TEXT NOT NULL, detail TEXT NOT NULL, created_at BIGINT NOT NULL, user_id TEXT, source_ip TEXT, user_agent TEXT, request_id TEXT) ON COMMIT DROP",
            )
            .execute(&mut *migration)
            .await?;
        }
        if !ssh_sessions_exists {
            sqlx::query(
                "CREATE TEMP TABLE ssh_sessions (id TEXT PRIMARY KEY, instance_id TEXT NOT NULL, actor TEXT NOT NULL, started_at BIGINT NOT NULL, ended_at BIGINT) ON COMMIT DROP",
            )
            .execute(&mut *migration)
            .await?;
        }
        if !desktop_sessions_exists {
            sqlx::query(
                "CREATE TEMP TABLE desktop_sessions (id TEXT PRIMARY KEY, instance_id TEXT NOT NULL, actor TEXT NOT NULL, started_at BIGINT NOT NULL, ended_at BIGINT, end_reason TEXT) ON COMMIT DROP",
            )
            .execute(&mut *migration)
            .await?;
        }
        if !docker_sessions_exists {
            sqlx::query(
                "CREATE TEMP TABLE docker_exec_sessions (id TEXT PRIMARY KEY, instance_id TEXT, instance_snapshot TEXT NOT NULL DEFAULT '', request_id TEXT NOT NULL, actor TEXT NOT NULL, operation TEXT NOT NULL, target TEXT NOT NULL DEFAULT '', metadata TEXT NOT NULL DEFAULT '{}', status TEXT NOT NULL, error_code TEXT, error_message TEXT NOT NULL DEFAULT '', requested_at BIGINT NOT NULL, completed_at BIGINT) ON COMMIT DROP",
            )
            .execute(&mut *migration)
            .await?;
        }
        if action_logs_exists {
            sqlx::query(
                r#"
            INSERT INTO audit_events(
                id, category, kind, actor, user_id, action, target, detail,
                instance_id, node_snapshot, source_ip, user_agent, request_id,
                created_at, completed_at, status
            )
            SELECT 'legacy-action:' || l.id,
                   CASE WHEN l.action IN ('login', 'initialize_auth', 'reset_admin_auth') THEN 'auth' ELSE 'admin' END,
                   'operation', l.actor,
                   COALESCE(l.user_id, u.id), l.action, l.target, l.detail,
                   CASE WHEN i.id IS NULL THEN NULL ELSE i.id END,
                   CASE WHEN i.id IS NULL THEN '{}'::jsonb ELSE jsonb_build_object(
                       'id', i.id, 'name', i.name, 'hostname', i.hostname, 'region', i.region,
                       'os', i.os, 'arch', i.arch, 'agent_version', i.agent_version
                   ) END,
                   l.source_ip, l.user_agent, l.request_id,
                   l.created_at, l.created_at,
                   'success'
            FROM action_logs l
            LEFT JOIN admin_users u ON u.username = l.actor
            LEFT JOIN instances i ON i.id = l.target
            WHERE l.action NOT IN ('desktop_start', 'desktop_end')
            ON CONFLICT (id) DO NOTHING
            "#,
            )
            .execute(&mut *migration)
            .await?;
        }
        if ssh_sessions_exists {
            sqlx::query(
                r#"
            INSERT INTO audit_events(
                id, category, kind, actor, user_id, action, target, instance_id, node_snapshot, session_id,
                created_at, completed_at, status, error_code, error_reason
            )
            SELECT 'legacy-ssh:' || s.id, 'terminal', 'session', s.actor, u.id,
                   'terminal_session', s.instance_id, s.instance_id,
                   CASE WHEN i.id IS NULL THEN jsonb_build_object('id', s.instance_id)
                        ELSE jsonb_build_object(
                            'id', i.id, 'name', i.name, 'hostname', i.hostname, 'region', i.region,
                            'os', i.os, 'arch', i.arch, 'agent_version', i.agent_version
                        ) END,
                   s.id,
                   s.started_at, s.ended_at,
                   CASE WHEN s.ended_at IS NULL THEN 'running' ELSE 'success' END,
                   NULL, ''
            FROM ssh_sessions s
            LEFT JOIN instances i ON i.id = s.instance_id
            LEFT JOIN admin_users u ON u.username = s.actor
            ON CONFLICT (id) DO NOTHING
            "#,
            )
            .execute(&mut *migration)
            .await?;
        }
        if desktop_sessions_exists {
            sqlx::query(
                r#"
            INSERT INTO audit_events(
                id, category, kind, actor, user_id, action, target, instance_id, node_snapshot, session_id,
                detail, created_at, completed_at, status, error_code, error_reason
            )
            SELECT 'legacy-desktop:' || s.id, 'desktop', 'session', s.actor, u.id,
                   'desktop_session', s.instance_id, s.instance_id,
                   CASE WHEN i.id IS NULL THEN jsonb_build_object('id', s.instance_id)
                        ELSE jsonb_build_object(
                            'id', i.id, 'name', i.name, 'hostname', i.hostname, 'region', i.region,
                            'os', i.os, 'arch', i.arch, 'agent_version', i.agent_version
                        ) END,
                   s.id,
                   COALESCE(NULLIF(s.end_reason, ''), end_log.reason, ''),
                   s.started_at, COALESCE(s.ended_at, end_log.created_at),
                   CASE WHEN COALESCE(s.ended_at, end_log.created_at) IS NULL THEN 'running'
                        WHEN COALESCE(NULLIF(s.end_reason, ''), end_log.reason, '') IN ('', 'client_closed', 'agent_closed') THEN 'success'
                        ELSE 'failed' END,
                   CASE WHEN COALESCE(NULLIF(s.end_reason, ''), end_log.reason, '') IN ('', 'client_closed', 'agent_closed') THEN NULL ELSE COALESCE(NULLIF(s.end_reason, ''), end_log.reason) END,
                   COALESCE(NULLIF(s.end_reason, ''), end_log.reason, '')
            FROM desktop_sessions s
            LEFT JOIN instances i ON i.id = s.instance_id
            LEFT JOIN admin_users u ON u.username = s.actor
            LEFT JOIN LATERAL (
                SELECT l.created_at,
                       COALESCE(NULLIF(split_part(l.detail, '：', 2), ''), NULLIF(split_part(l.detail, ':', 2), '')) AS reason
                FROM action_logs l
                WHERE l.action = 'desktop_end'
                  AND l.target = s.instance_id
                  AND l.detail LIKE '%' || s.id || '%'
                ORDER BY l.created_at DESC, l.id DESC
                LIMIT 1
            ) end_log ON TRUE
            ON CONFLICT (id) DO NOTHING
            "#,
            )
            .execute(&mut *migration)
            .await?;
        }
        if action_logs_exists {
            // Some older versions wrote desktop start/end as action logs in
            // addition to the session table. Keep one merged session event,
            // and recover action-only sessions when the session table is absent.
            sqlx::query(
                r#"
                WITH starts AS (
                    SELECT DISTINCT ON (l.target, regexp_replace(l.detail, '^.*[[:space:]]', ''))
                           l.*, regexp_replace(l.detail, '^.*[[:space:]]', '') AS session_key
                    FROM action_logs l
                    WHERE l.action = 'desktop_start'
                    ORDER BY l.target, regexp_replace(l.detail, '^.*[[:space:]]', ''), l.created_at, l.id
                )
                INSERT INTO audit_events(
                    id, category, kind, actor, user_id, action, target, detail,
                    instance_id, node_snapshot, session_id, created_at, completed_at,
                    status, error_code, error_reason
                )
                SELECT 'legacy-desktop:' || s.session_key,
                       'desktop', 'session', s.actor, COALESCE(s.user_id, u.id),
                       'desktop_session', s.target, s.detail,
                       s.target,
                       CASE WHEN i.id IS NULL THEN jsonb_build_object('id', s.target)
                            ELSE jsonb_build_object(
                                'id', i.id, 'name', i.name, 'hostname', i.hostname, 'region', i.region,
                                'os', i.os, 'arch', i.arch, 'agent_version', i.agent_version
                            ) END,
                       s.session_key, s.created_at, end_log.created_at,
                       CASE WHEN end_log.created_at IS NULL THEN 'running'
                            WHEN COALESCE(end_log.reason, '') IN ('', 'client_closed', 'agent_closed') THEN 'success'
                            ELSE 'failed' END,
                       CASE WHEN end_log.created_at IS NULL OR COALESCE(end_log.reason, '') IN ('', 'client_closed', 'agent_closed') THEN NULL ELSE end_log.reason END,
                       COALESCE(end_log.reason, '')
                FROM starts s
                LEFT JOIN instances i ON i.id = s.target
                LEFT JOIN admin_users u ON u.username = s.actor
                LEFT JOIN LATERAL (
                    SELECT l.created_at,
                           COALESCE(NULLIF(split_part(l.detail, '：', 2), ''), NULLIF(split_part(l.detail, ':', 2), '')) AS reason
                    FROM action_logs l
                    WHERE l.action = 'desktop_end'
                      AND l.target = s.target
                      AND l.detail LIKE '%' || s.session_key || '%'
                    ORDER BY l.created_at DESC, l.id DESC
                    LIMIT 1
                ) end_log ON TRUE
                WHERE NOT EXISTS (
                    SELECT 1 FROM desktop_sessions d WHERE d.id = s.session_key
                )
                ON CONFLICT (id) DO NOTHING
                "#,
            )
            .execute(&mut *migration)
            .await?;
        }
        if docker_sessions_exists {
            sqlx::query(
                r#"
            INSERT INTO audit_events(
                id, category, kind, actor, user_id, action, target, detail, metadata,
                instance_id, node_snapshot, session_id, operation_id, created_at, completed_at,
                status, error_code, error_reason
            )
            SELECT 'legacy-docker:' || d.id, 'docker',
                   CASE WHEN d.operation = 'container_exec' THEN 'session' ELSE 'operation' END,
                   d.actor, u.id, d.operation, d.target, '',
                   CASE WHEN NULLIF(d.metadata, '') IS NULL THEN '{}'::jsonb
                        ELSE operation_monitoring_parse_json(d.metadata) END,
                   d.instance_id,
                   CASE WHEN i.id IS NULL THEN jsonb_build_object('id', COALESCE(NULLIF(d.instance_snapshot, ''), d.instance_id))
                        ELSE jsonb_build_object(
                            'id', i.id, 'name', i.name, 'hostname', i.hostname, 'region', i.region,
                            'os', i.os, 'arch', i.arch, 'agent_version', i.agent_version
                        ) END,
                   CASE WHEN d.operation = 'container_exec' THEN d.id ELSE NULL END,
                   d.request_id, d.requested_at, d.completed_at,
                   CASE d.status WHEN 'completed' THEN 'success' WHEN 'succeeded' THEN 'success'
                        WHEN 'partial' THEN 'partial_success' WHEN 'canceled' THEN 'cancelled'
                        WHEN 'running' THEN 'running' WHEN 'success' THEN 'success'
                        WHEN 'partial_success' THEN 'partial_success' WHEN 'failed' THEN 'failed'
                        ELSE 'failed' END,
                   d.error_code, d.error_message
            FROM docker_exec_sessions d
            LEFT JOIN instances i ON i.id = d.instance_id
            LEFT JOIN admin_users u ON u.username = d.actor
            ON CONFLICT (id) DO NOTHING
            "#,
            )
            .execute(&mut *migration)
            .await?;
        }
        sqlx::query(
            "INSERT INTO settings(key, value) VALUES('audit_schema_version', $1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(AUDIT_SCHEMA_VERSION)
        .execute(&mut *migration)
        .await?;
        sqlx::query(
            "DROP TABLE IF EXISTS action_logs, ssh_sessions, desktop_sessions, docker_exec_sessions",
        )
        .execute(&mut *migration)
        .await?;
        migration.commit().await?;
    }

    // Legacy rows are restored as `running` during the one-time migration. Only
    // mark running events as interrupted when this is a subsequent restart.
    let restart_running = migrated.is_some() && !legacy_exists;
    if restart_running {
        let restarted_at = now_ts();
        sqlx::query(
            "UPDATE audit_events SET status = 'failed', error_code = 'backend_restarted', error_reason = '后端服务重启', completed_at = $1 WHERE status = 'running' AND completed_at IS NULL",
        )
        .bind(restarted_at)
        .execute(db)
        .await?;
    }
    sqlx::query(
        "INSERT INTO settings(key, value) VALUES('audit_retention_days', $1) ON CONFLICT(key) DO NOTHING",
    )
    .bind(DEFAULT_AUDIT_RETENTION_DAYS.to_string())
    .execute(db)
    .await?;
    Ok(())
}

async fn legacy_table_exists(db: &PgPool, table: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = $1)",
    )
    .bind(table)
    .fetch_one(db)
    .await?)
}

pub async fn retention_days(db: &PgPool) -> AppResult<i64> {
    let value: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'audit_retention_days'")
            .fetch_optional(db)
            .await?;
    Ok(value
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(DEFAULT_AUDIT_RETENTION_DAYS)
        .clamp(1, MAX_AUDIT_RETENTION_DAYS))
}

pub async fn cleanup(db: &PgPool, cutoff: i64) -> AppResult<()> {
    sqlx::query("DELETE FROM audit_events WHERE created_at < $1 AND status <> 'running'")
        .bind(cutoff)
        .execute(db)
        .await?;
    Ok(())
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct AuditQuery {
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
    pub user_id: Option<String>,
    pub actor: Option<String>,
    pub category: Option<String>,
    pub action: Option<String>,
    pub instance_id: Option<String>,
    pub status: Option<String>,
    pub source_ip: Option<String>,
    pub request_id: Option<String>,
    pub keyword: Option<String>,
    pub limit: Option<i64>,
    pub format: Option<String>,
}

#[derive(Debug, Serialize, FromRow, Clone)]
pub struct AuditEventRecord {
    pub id: String,
    pub user_id: Option<String>,
    pub actor: String,
    pub category: String,
    pub kind: String,
    pub action: String,
    pub target: String,
    pub detail: String,
    pub metadata: Value,
    pub instance_id: Option<String>,
    pub node_snapshot: Value,
    pub source_ip: Option<String>,
    pub user_agent: Option<String>,
    pub request_id: Option<String>,
    pub session_id: Option<String>,
    pub operation_id: Option<String>,
    pub status: String,
    pub error_code: Option<String>,
    pub error_reason: String,
    pub created_at: i64,
    pub completed_at: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct AuditPage {
    pub items: Vec<AuditEventRecord>,
    pub page: i64,
    pub page_size: i64,
    pub total: i64,
    pub pages: i64,
}

fn add_filters<'a>(builder: &mut QueryBuilder<'a, Postgres>, query: &'a AuditQuery) {
    if let Some(value) = query.from {
        builder.push(" AND created_at >= ").push_bind(value);
    }
    if let Some(value) = query.to {
        builder.push(" AND created_at < ").push_bind(value);
    }
    for (column, value) in [
        ("user_id", query.user_id.as_deref()),
        ("actor", query.actor.as_deref()),
        ("category", query.category.as_deref()),
        ("action", query.action.as_deref()),
        ("instance_id", query.instance_id.as_deref()),
        ("status", query.status.as_deref()),
        ("source_ip", query.source_ip.as_deref()),
        ("request_id", query.request_id.as_deref()),
    ] {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            builder
                .push(" AND ")
                .push(column)
                .push(" = ")
                .push_bind(value.trim());
        }
    }
    if let Some(value) = query
        .keyword
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let pattern = format!("%{}%", value.trim());
        builder
            .push(" AND (actor ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR action ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR target ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR detail ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR user_id ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR category ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR instance_id ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR request_id ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR metadata::text ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR node_snapshot::text ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR error_reason ILIKE ")
            .push_bind(pattern)
            .push(")");
    }
}

pub async fn query_page(db: &PgPool, query: &AuditQuery) -> AppResult<AuditPage> {
    let page = query.page.unwrap_or(1).max(1);
    let max_page_size = if query.format.is_some() {
        MAX_EXPORT_ROWS
    } else {
        MAX_AUDIT_PAGE_SIZE
    };
    let page_size = query
        .page_size
        .or(query.limit)
        .unwrap_or(DEFAULT_AUDIT_PAGE_SIZE)
        .clamp(1, max_page_size);
    let mut count = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM audit_events WHERE TRUE");
    add_filters(&mut count, query);
    let total: i64 = count.build_query_scalar().fetch_one(db).await?;

    let mut rows = QueryBuilder::<Postgres>::new(
        "SELECT id, user_id, actor, category, kind, action, target, detail, metadata, instance_id, node_snapshot, source_ip, user_agent, request_id, session_id, operation_id, status, error_code, error_reason, created_at, completed_at FROM audit_events WHERE TRUE",
    );
    add_filters(&mut rows, query);
    rows.push(" ORDER BY created_at DESC, id DESC LIMIT ")
        .push_bind(page_size)
        .push(" OFFSET ")
        .push_bind((page - 1).saturating_mul(page_size));
    let items = rows
        .build_query_as::<AuditEventRecord>()
        .fetch_all(db)
        .await?;
    let pages = if total == 0 {
        0
    } else {
        (total + page_size - 1) / page_size
    };
    Ok(AuditPage {
        items,
        page,
        page_size,
        total,
        pages,
    })
}

async fn query_export_batch(
    db: &PgPool,
    query: &AuditQuery,
    before: Option<(i64, String)>,
    exclude_id: Option<&str>,
    limit: i64,
) -> AppResult<Vec<AuditEventRecord>> {
    let mut rows = QueryBuilder::<Postgres>::new(
        "SELECT id, user_id, actor, category, kind, action, target, detail, metadata, instance_id, node_snapshot, source_ip, user_agent, request_id, session_id, operation_id, status, error_code, error_reason, created_at, completed_at FROM audit_events WHERE TRUE",
    );
    add_filters(&mut rows, query);
    if let Some(exclude_id) = exclude_id {
        rows.push(" AND id <> ").push_bind(exclude_id.to_string());
    }
    if let Some((created_at, id)) = before {
        rows.push(" AND (created_at < ")
            .push_bind(created_at)
            .push(" OR (created_at = ")
            .push_bind(created_at)
            .push(" AND id < ")
            .push_bind(id)
            .push("))");
    }
    rows.push(" ORDER BY created_at DESC, id DESC LIMIT ")
        .push_bind(limit.clamp(1, EXPORT_BATCH_SIZE));
    Ok(rows
        .build_query_as::<AuditEventRecord>()
        .fetch_all(db)
        .await?)
}

struct ExportAuditGuard {
    db: PgPool,
    event_id: Option<String>,
}

impl ExportAuditGuard {
    fn new(db: PgPool, event_id: String) -> Self {
        Self {
            db,
            event_id: Some(event_id),
        }
    }

    async fn finish(
        &mut self,
        status: &str,
        error_code: Option<&str>,
        error_reason: &str,
    ) -> AppResult<()> {
        let Some(event_id) = self.event_id.as_deref() else {
            return Ok(());
        };
        finish_event(&self.db, event_id, status, error_code, error_reason).await?;
        self.event_id = None;
        Ok(())
    }
}

impl Drop for ExportAuditGuard {
    fn drop(&mut self) {
        let Some(event_id) = self.event_id.take() else {
            return;
        };
        let db = self.db.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            if let Err(error) = finish_event(
                &db,
                &event_id,
                "cancelled",
                Some("client_disconnected"),
                "审计导出连接已断开",
            )
            .await
            {
                warn!(?error, %event_id, "failed to finalize disconnected audit export");
            }
        });
    }
}

pub async fn admin_audit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AuditQuery>,
) -> AppResult<Json<AuditPage>> {
    require_admin(&state, &headers).await?;
    let mut page_query = query;
    page_query.format = None;
    Ok(Json(query_page(&state.db, &page_query).await?))
}

pub async fn admin_audit_export(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AuditQuery>,
) -> AppResult<Response> {
    let admin = require_admin(&state, &headers).await?;
    let format = query.format.clone().unwrap_or_else(|| "csv".to_string());
    if !matches!(format.as_str(), "csv" | "json") {
        return Err(AppError::bad_request("导出格式必须是 csv 或 json"));
    }
    let export_event_id = insert_event(
        &state.db,
        &AuditEventInput {
            category: "security".to_string(),
            kind: "operation".to_string(),
            actor: admin.username,
            user_id: Some(admin.user_id),
            action: "audit_export".to_string(),
            target: format.clone(),
            detail: "导出审计记录".to_string(),
            metadata: serde_json::to_value(&query).unwrap_or_else(|_| json!({})),
            instance_id: None,
            node_snapshot: json!({}),
            context: AuditContext::from_headers(&headers),
            session_id: None,
            operation_id: None,
            status: "running".to_string(),
            error_code: None,
            error_reason: String::new(),
            created_at: now_ts(),
            completed_at: None,
        },
    )
    .await?;

    let json_export = format == "json";
    let content_type = if json_export {
        "application/json; charset=utf-8"
    } else {
        "text/csv; charset=utf-8"
    };
    let export_db = state.db.clone();
    let export_query = query;
    let excluded_event_id = export_event_id.clone();
    let body_stream = stream! {
        let mut audit_guard = ExportAuditGuard::new(export_db.clone(), export_event_id);
        if json_export {
            yield Ok::<Vec<u8>, io::Error>(b"[".to_vec());
        } else {
            yield Ok::<Vec<u8>, io::Error>(CSV_HEADER.as_bytes().to_vec());
        }

        let mut cursor: Option<(i64, String)> = None;
        let mut emitted = 0_i64;
        let mut first_json_item = true;
        while emitted < MAX_EXPORT_ROWS {
            let limit = (MAX_EXPORT_ROWS - emitted).min(EXPORT_BATCH_SIZE);
            let items = match query_export_batch(
                &export_db,
                &export_query,
                cursor.clone(),
                Some(&excluded_event_id),
                limit,
            )
            .await
            {
                Ok(items) => items,
                Err(error) => {
                    let error_message = error.message.clone();
                    if let Err(audit_error) = audit_guard
                        .finish("failed", Some(&error.code), &error_message)
                        .await
                    {
                        warn!(?audit_error, "failed to finalize failed audit export");
                    }
                    yield Err(io::Error::other(error_message));
                    return;
                }
            };
            if items.is_empty() {
                break;
            }
            cursor = items
                .last()
                .map(|item| (item.created_at, item.id.clone()));
            emitted += items.len() as i64;

            let mut chunk = String::new();
            if json_export {
                for item in &items {
                    if !first_json_item {
                        chunk.push(',');
                    }
                    first_json_item = false;
                    match serde_json::to_string(item) {
                        Ok(encoded) => chunk.push_str(&encoded),
                        Err(error) => {
                            let error_message = error.to_string();
                            if let Err(audit_error) = audit_guard
                                .finish(
                                    "failed",
                                    Some("serialization_failed"),
                                    &error_message,
                                )
                                .await
                            {
                                warn!(?audit_error, "failed to finalize failed audit export");
                            }
                            yield Err(io::Error::other(error));
                            return;
                        }
                    }
                }
            } else {
                for item in &items {
                    append_csv_row(&mut chunk, item);
                }
            }
            yield Ok(chunk.into_bytes());
            if items.len() < limit as usize {
                break;
            }
        }

        if json_export {
            yield Ok(b"]".to_vec());
        }
        if let Err(error) = audit_guard.finish("success", None, "").await {
            warn!(?error, "failed to finalize successful audit export");
            yield Err(io::Error::other(error.message));
        }
    };

    let mut response = Response::new(Body::from_stream(body_stream));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"audit-{0}.{format}\"", now_ts())
            .parse()
            .unwrap(),
    );
    Ok(response)
}

const CSV_HEADER: &str = "id,user_id,actor,category,kind,action,target,detail,metadata,instance_id,node_snapshot,source_ip,user_agent,request_id,session_id,operation_id,status,error_code,error_reason,created_at,completed_at\n";

fn append_csv_row(output: &mut String, item: &AuditEventRecord) {
    let metadata = serde_json::to_string(&item.metadata).unwrap_or_else(|_| "{}".to_string());
    let node_snapshot =
        serde_json::to_string(&item.node_snapshot).unwrap_or_else(|_| "{}".to_string());
    let fields = [
        item.id.as_str(),
        item.user_id.as_deref().unwrap_or(""),
        item.actor.as_str(),
        item.category.as_str(),
        item.kind.as_str(),
        item.action.as_str(),
        item.target.as_str(),
        item.detail.as_str(),
        metadata.as_str(),
        item.instance_id.as_deref().unwrap_or(""),
        node_snapshot.as_str(),
        item.source_ip.as_deref().unwrap_or(""),
        item.user_agent.as_deref().unwrap_or(""),
        item.request_id.as_deref().unwrap_or(""),
        item.session_id.as_deref().unwrap_or(""),
        item.operation_id.as_deref().unwrap_or(""),
        item.status.as_str(),
        item.error_code.as_deref().unwrap_or(""),
        item.error_reason.as_str(),
    ];
    for (index, field) in fields.into_iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str(&csv_escape(field));
    }
    let _ = writeln!(
        output,
        ",{},{}",
        item.created_at,
        item.completed_at
            .map(|value| value.to_string())
            .unwrap_or_default()
    );
}

fn csv_escape(value: &str) -> String {
    if value
        .bytes()
        .any(|byte| matches!(byte, b',' | b'"' | b'\n' | b'\r'))
    {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    async fn isolated_test_pool(prefix: &str) -> (PgPool, PgPool, String) {
        let database_url = std::env::var("OM_TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgresql://localhost/postgres".to_string());
        let schema = format!("{prefix}_{}", Uuid::new_v4().simple());
        let bootstrap = PgPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .expect("connect test database");
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&bootstrap)
            .await
            .expect("create isolated audit schema");

        let connection_schema = schema.clone();
        let db = PgPoolOptions::new()
            .max_connections(4)
            .after_connect(move |connection, _metadata| {
                let schema = connection_schema.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(schema)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .expect("connect isolated audit schema");
        (db, bootstrap, schema)
    }

    async fn drop_test_schema(db: PgPool, bootstrap: PgPool, schema: String) {
        db.close().await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&bootstrap)
            .await
            .expect("drop isolated audit schema");
        bootstrap.close().await;
    }

    async fn create_audit_prerequisites(db: &PgPool) {
        for statement in [
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            "CREATE TABLE instances (id TEXT PRIMARY KEY, name TEXT NOT NULL, hostname TEXT NOT NULL, region TEXT NOT NULL, os TEXT NOT NULL, arch TEXT NOT NULL, agent_version TEXT NOT NULL)",
            "CREATE TABLE admin_users (id TEXT PRIMARY KEY, username TEXT NOT NULL)",
        ] {
            sqlx::query(statement)
                .execute(db)
                .await
                .expect("create audit prerequisite");
        }
    }

    fn test_event(actor: &str, action: &str, created_at: i64, status: &str) -> AuditEventInput {
        AuditEventInput {
            category: "admin".to_string(),
            kind: "operation".to_string(),
            actor: actor.to_string(),
            user_id: Some("user-1".to_string()),
            action: action.to_string(),
            target: "node-1".to_string(),
            detail: "test detail".to_string(),
            metadata: json!({}),
            instance_id: Some("node-1".to_string()),
            node_snapshot: json!({ "id": "node-1", "name": "Node One" }),
            context: AuditContext {
                request_id: Some(Uuid::new_v4().to_string()),
                source_ip: Some("203.0.113.10".to_string()),
                user_agent: Some("audit-test".to_string()),
            },
            session_id: None,
            operation_id: None,
            status: status.to_string(),
            error_code: None,
            error_reason: String::new(),
            created_at,
            completed_at: (status != "running").then_some(created_at),
        }
    }

    #[test]
    fn truncation_preserves_utf8() {
        assert_eq!(truncate("你好世界", 7), "你好");
    }

    #[test]
    fn csv_quotes_special_values() {
        assert_eq!(csv_escape("a,b\"c"), "\"a,b\"\"c\"");
    }

    #[test]
    fn csv_export_includes_all_context_and_escaped_values() {
        let row = AuditEventRecord {
            id: "event-1".to_string(),
            user_id: Some("user-1".to_string()),
            actor: "admin".to_string(),
            category: "file".to_string(),
            kind: "operation".to_string(),
            action: "download".to_string(),
            target: "report,final.csv".to_string(),
            detail: "line one\nline \"two\"".to_string(),
            metadata: json!({ "path": "report,final.csv" }),
            instance_id: Some("node-1".to_string()),
            node_snapshot: json!({ "id": "node-1", "name": "Node One" }),
            source_ip: Some("203.0.113.10".to_string()),
            user_agent: Some("audit-test".to_string()),
            request_id: Some("request-1".to_string()),
            session_id: None,
            operation_id: Some("operation-1".to_string()),
            status: "success".to_string(),
            error_code: None,
            error_reason: String::new(),
            created_at: 100,
            completed_at: Some(101),
        };
        let mut csv = CSV_HEADER.to_string();
        append_csv_row(&mut csv, &row);
        assert!(csv.starts_with(CSV_HEADER));
        assert!(csv.contains("\"report,final.csv\""));
        assert!(csv.contains("\"line one\nline \"\"two\"\"\""));
        assert!(csv.contains("request-1"));
        assert!(csv.ends_with(",100,101\n"));
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn migrates_legacy_audits_deduplicates_desktops_and_preserves_snapshots() {
        let (db, bootstrap, schema) = isolated_test_pool("om_audit_migration").await;
        create_audit_prerequisites(&db).await;
        for statement in [
            "CREATE TABLE action_logs (id TEXT PRIMARY KEY, actor TEXT NOT NULL, action TEXT NOT NULL, target TEXT NOT NULL, detail TEXT NOT NULL, created_at BIGINT NOT NULL)",
            "CREATE TABLE ssh_sessions (id TEXT PRIMARY KEY, instance_id TEXT NOT NULL, actor TEXT NOT NULL, started_at BIGINT NOT NULL, ended_at BIGINT)",
            "CREATE TABLE desktop_sessions (id TEXT PRIMARY KEY, instance_id TEXT NOT NULL, actor TEXT NOT NULL, started_at BIGINT NOT NULL, ended_at BIGINT, end_reason TEXT)",
            "CREATE TABLE docker_exec_sessions (id TEXT PRIMARY KEY, instance_id TEXT, request_id TEXT NOT NULL, actor TEXT NOT NULL, operation TEXT NOT NULL, target TEXT NOT NULL DEFAULT '', status TEXT NOT NULL, error_code TEXT, error_message TEXT NOT NULL DEFAULT '', requested_at BIGINT NOT NULL, completed_at BIGINT)",
        ] {
            sqlx::query(statement)
                .execute(&db)
                .await
                .expect("create legacy audit table");
        }
        sqlx::query("INSERT INTO admin_users(id, username) VALUES('user-1', 'alice')")
            .execute(&db)
            .await
            .expect("insert legacy actor");
        sqlx::query("INSERT INTO instances(id, name, hostname, region, os, arch, agent_version) VALUES('node-1', 'Node One', 'host-1', 'cn-east', 'linux', 'x86_64', '1.2.3')")
            .execute(&db)
            .await
            .expect("insert legacy node");
        for (id, action, detail, created_at) in [
            ("action-1", "update_instance", "updated", 1_000_i64),
            (
                "desktop-start-1",
                "desktop_start",
                "启动远程桌面会话 desktop-1",
                1_010,
            ),
            (
                "desktop-end-1",
                "desktop_end",
                "结束远程桌面会话 desktop-1：client_closed",
                1_020,
            ),
            (
                "desktop-start-2",
                "desktop_start",
                "启动远程桌面会话 desktop-only",
                1_030,
            ),
            (
                "desktop-end-2",
                "desktop_end",
                "结束远程桌面会话 desktop-only：agent_disconnected",
                1_040,
            ),
        ] {
            sqlx::query("INSERT INTO action_logs(id, actor, action, target, detail, created_at) VALUES($1, 'alice', $2, 'node-1', $3, $4)")
                .bind(id)
                .bind(action)
                .bind(detail)
                .bind(created_at)
                .execute(&db)
                .await
                .expect("insert legacy action");
        }
        sqlx::query("INSERT INTO desktop_sessions(id, instance_id, actor, started_at) VALUES('desktop-1', 'node-1', 'alice', 1010)")
            .execute(&db)
            .await
            .expect("insert legacy desktop session");
        sqlx::query("INSERT INTO ssh_sessions(id, instance_id, actor, started_at) VALUES('ssh-1', 'node-1', 'alice', 1050)")
            .execute(&db)
            .await
            .expect("insert legacy terminal session");
        sqlx::query("INSERT INTO docker_exec_sessions(id, instance_id, request_id, actor, operation, target, status, requested_at) VALUES('docker-1', 'node-1', 'agent-operation-1', 'alice', 'container_exec', 'web', 'running', 1060)")
            .execute(&db)
            .await
            .expect("insert legacy Docker session");

        ensure_schema(&db).await.expect("migrate legacy audits");

        let event_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
            .fetch_one(&db)
            .await
            .expect("count migrated events");
        assert_eq!(event_count, 5);
        let desktop: (String, Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT status, completed_at, error_code FROM audit_events WHERE session_id = 'desktop-1'",
        )
        .fetch_one(&db)
        .await
        .expect("load merged desktop event");
        assert_eq!(desktop, ("success".to_string(), Some(1_020), None));
        let action_only: (String, Option<String>) = sqlx::query_as(
            "SELECT status, error_code FROM audit_events WHERE session_id = 'desktop-only'",
        )
        .fetch_one(&db)
        .await
        .expect("load action-only desktop event");
        assert_eq!(
            action_only,
            ("failed".to_string(), Some("agent_disconnected".to_string()))
        );
        let running: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE status = 'running'")
                .fetch_one(&db)
                .await
                .expect("count restored running sessions");
        assert_eq!(running, 2);
        let snapshot: Value =
            sqlx::query_scalar("SELECT node_snapshot FROM audit_events WHERE session_id = 'ssh-1'")
                .fetch_one(&db)
                .await
                .expect("load immutable node snapshot");
        assert_eq!(snapshot["name"], "Node One");
        let user_id: Option<String> = sqlx::query_scalar(
            "SELECT user_id FROM audit_events WHERE id = 'legacy-action:action-1'",
        )
        .fetch_one(&db)
        .await
        .expect("load migrated user association");
        assert_eq!(user_id.as_deref(), Some("user-1"));
        for table in [
            "action_logs",
            "ssh_sessions",
            "desktop_sessions",
            "docker_exec_sessions",
        ] {
            assert!(
                !legacy_table_exists(&db, table)
                    .await
                    .expect("check legacy table")
            );
        }

        sqlx::query("DELETE FROM instances WHERE id = 'node-1'")
            .execute(&db)
            .await
            .expect("delete migrated node");
        let preserved_snapshot: Value = sqlx::query_scalar(
            "SELECT node_snapshot FROM audit_events WHERE session_id = 'desktop-1'",
        )
        .fetch_one(&db)
        .await
        .expect("load snapshot after node deletion");
        assert_eq!(preserved_snapshot["hostname"], "host-1");

        ensure_schema(&db)
            .await
            .expect("recover running audits after backend restart");
        let restarted: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_events WHERE error_code = 'backend_restarted' AND status = 'failed'",
        )
        .fetch_one(&db)
        .await
        .expect("count restarted sessions");
        assert_eq!(restarted, 2);

        drop_test_schema(db, bootstrap, schema).await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn filters_pages_exports_and_cleans_audits_independently() {
        let (db, bootstrap, schema) = isolated_test_pool("om_audit_query").await;
        create_audit_prerequisites(&db).await;
        ensure_schema(&db).await.expect("initialize audit schema");

        let explicit_context = AuditContext {
            request_id: Some("00000000-0000-4000-8000-000000000099".to_string()),
            source_ip: Some("198.51.100.9".to_string()),
            user_agent: Some("explicit-user-test".to_string()),
        };
        let explicit_id = with_context(
            explicit_context.clone(),
            crate::db::write_action_log(
                &db,
                "renamed-admin",
                Some("deleted-user-id"),
                "update_settings",
                "settings",
                "explicit user association",
            ),
        )
        .await
        .expect("insert explicitly associated event");
        let explicit_row: (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT user_id, request_id, source_ip, user_agent FROM audit_events WHERE id = $1",
        )
        .bind(&explicit_id)
        .fetch_one(&db)
        .await
        .expect("load explicitly associated event");
        assert_eq!(
            explicit_row,
            (
                Some("deleted-user-id".to_string()),
                explicit_context.request_id,
                explicit_context.source_ip,
                explicit_context.user_agent,
            )
        );
        sqlx::query("DELETE FROM audit_events WHERE id = $1")
            .bind(explicit_id)
            .execute(&db)
            .await
            .expect("remove explicit association fixture");

        let mut matching = test_event("alice", "update_instance", 1_000, "success");
        matching.detail = "unique audit needle".to_string();
        matching.metadata = json!({ "scope": "settings" });
        matching.context.request_id = Some("00000000-0000-4000-8000-000000000001".to_string());
        let matching_id = insert_event(&db, &matching)
            .await
            .expect("insert matching event");
        let second_id = insert_event(
            &db,
            &test_event("alice", "delete_instance", 1_000, "failed"),
        )
        .await
        .expect("insert second event");
        let third_id = insert_event(&db, &test_event("bob", "logout", 1_000, "success"))
            .await
            .expect("insert third event");

        let filtered = query_page(
            &db,
            &AuditQuery {
                from: Some(1_000),
                to: Some(1_001),
                page: Some(1),
                page_size: Some(50),
                user_id: Some("user-1".to_string()),
                actor: Some("alice".to_string()),
                category: Some("admin".to_string()),
                action: Some("update_instance".to_string()),
                instance_id: Some("node-1".to_string()),
                status: Some("success".to_string()),
                source_ip: Some("203.0.113.10".to_string()),
                request_id: matching.context.request_id.clone(),
                keyword: Some("unique audit needle".to_string()),
                ..AuditQuery::default()
            },
        )
        .await
        .expect("query combined audit filters");
        assert_eq!(filtered.total, 1);
        assert_eq!(filtered.items[0].id, matching_id);

        let page_one = query_page(
            &db,
            &AuditQuery {
                page: Some(1),
                page_size: Some(2),
                ..AuditQuery::default()
            },
        )
        .await
        .expect("query first audit page");
        let page_two = query_page(
            &db,
            &AuditQuery {
                page: Some(2),
                page_size: Some(2),
                ..AuditQuery::default()
            },
        )
        .await
        .expect("query second audit page");
        assert_eq!((page_one.total, page_one.pages), (3, 2));
        let mut expected_ids = vec![matching_id.clone(), second_id.clone(), third_id.clone()];
        expected_ids.sort_by(|left, right| right.cmp(left));
        assert_eq!(
            page_one
                .items
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>(),
            expected_ids[..2]
        );
        assert_eq!(page_two.items[0].id, expected_ids[2]);

        sqlx::query(
            r#"
            INSERT INTO audit_events(
                id, category, kind, actor, action, target, status, created_at, completed_at
            )
            SELECT 'bulk-' || lpad(value::text, 4, '0'), 'admin', 'operation',
                   'bulk', 'export', '', 'success', 2000, 2000
            FROM generate_series(1, 1001) AS value
            "#,
        )
        .execute(&db)
        .await
        .expect("insert export batch");
        let batch = query_export_batch(&db, &AuditQuery::default(), None, None, MAX_EXPORT_ROWS)
            .await
            .expect("query bounded export batch");
        assert_eq!(batch.len(), EXPORT_BATCH_SIZE as usize);

        sqlx::query("UPDATE audit_events SET created_at = 10, completed_at = 10 WHERE id = $1")
            .bind(&matching_id)
            .execute(&db)
            .await
            .expect("age completed event");
        sqlx::query("UPDATE audit_events SET created_at = 10, completed_at = NULL, status = 'running' WHERE id = $1")
            .bind(&second_id)
            .execute(&db)
            .await
            .expect("age running event");
        cleanup(&db, 100).await.expect("clean old audits");
        let retained: Vec<String> =
            sqlx::query_scalar("SELECT id FROM audit_events WHERE id = ANY($1) ORDER BY id")
                .bind(vec![
                    matching_id.clone(),
                    second_id.clone(),
                    third_id.clone(),
                ])
                .fetch_all(&db)
                .await
                .expect("load retained events");
        let mut expected_retained = vec![second_id.clone(), third_id];
        expected_retained.sort();
        assert_eq!(retained, expected_retained);

        sqlx::query("UPDATE settings SET value = '99999' WHERE key = 'audit_retention_days'")
            .execute(&db)
            .await
            .expect("set oversized retention");
        assert_eq!(retention_days(&db).await.expect("read retention"), 3_650);

        let export_event =
            insert_event(&db, &test_event("alice", "audit_export", 3_000, "running"))
                .await
                .expect("insert running export");
        drop(ExportAuditGuard::new(db.clone(), export_event.clone()));
        for _ in 0..50 {
            let status: String =
                sqlx::query_scalar("SELECT status FROM audit_events WHERE id = $1")
                    .bind(&export_event)
                    .fetch_one(&db)
                    .await
                    .expect("load disconnected export status");
            if status == "cancelled" {
                break;
            }
            tokio::task::yield_now().await;
        }
        let disconnected_export: (String, Option<String>) =
            sqlx::query_as("SELECT status, error_code FROM audit_events WHERE id = $1")
                .bind(&export_event)
                .fetch_one(&db)
                .await
                .expect("load finalized disconnected export");
        assert_eq!(
            disconnected_export,
            (
                "cancelled".to_string(),
                Some("client_disconnected".to_string())
            )
        );

        drop_test_schema(db, bootstrap, schema).await;
    }
}
