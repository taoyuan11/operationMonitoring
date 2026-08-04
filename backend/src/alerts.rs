use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    routing::{get, patch, post, put},
};
use hmac::{Hmac, Mac};
use reqwest::{Client, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use tracing::{error, warn};
use uuid::Uuid;

use crate::{
    auth::{AdminPrincipal, require_admin},
    db::write_action_log,
    error::{AppError, AppResult},
    models::MetricPayload,
    state::AppState,
    utils::now_ts,
};

pub const DEFAULT_ALERT_RETENTION_DAYS: i64 = 180;
pub const MAX_ALERT_RETENTION_DAYS: i64 = 3650;
const DEFAULT_PAGE_SIZE: i64 = 50;
const MAX_PAGE_SIZE: i64 = 200;
const MAX_NAME_BYTES: usize = 120;
const MAX_REASON_BYTES: usize = 2_000;
const MAX_HEADER_COUNT: usize = 32;
const MAX_HEADER_NAME_BYTES: usize = 128;
const MAX_HEADER_VALUE_BYTES: usize = 4_096;
const MAX_RESPONSE_BYTES: usize = 8 * 1024;
const WEBHOOK_TIMEOUT_SECONDS: u64 = 10;
const STARTUP_RECONNECT_GRACE_SECONDS: i64 = 60;
const DELIVERY_LEASE_SECONDS: i64 = 30;
const DELIVERY_RETRY_DELAYS: [i64; 4] = [60, 5 * 60, 15 * 60, 60 * 60];

#[derive(Clone, Debug)]
struct MetricObservation {
    received_at: i64,
    cpu_percent: f64,
    memory_used: i64,
    memory_total: i64,
    disk_used: i64,
    disk_total: i64,
    latency_ms: Option<f64>,
    latency_sampled_at: Option<i64>,
}

#[derive(Clone, Debug, FromRow, Serialize)]
struct RuleRow {
    id: String,
    name: String,
    metric: String,
    threshold: Option<f64>,
    duration_seconds: i64,
    severity: String,
    scope: String,
    enabled: bool,
    version: i64,
    created_by: String,
    created_at: i64,
    updated_at: i64,
}

#[derive(Clone, Debug, FromRow, Serialize)]
struct EventRow {
    id: String,
    rule_id: String,
    instance_id: String,
    status: String,
    severity: String,
    metric: String,
    rule_snapshot: Value,
    node_snapshot: Value,
    threshold: Option<f64>,
    duration_seconds: i64,
    current_value: Option<f64>,
    first_observed_at: i64,
    fired_at: i64,
    last_observed_at: i64,
    match_count: i64,
    acknowledged_by: Option<String>,
    acknowledged_by_user_id: Option<String>,
    acknowledged_at: Option<i64>,
    acknowledge_note: String,
    resolved_at: Option<i64>,
    resolution_reason: String,
    suppressed: bool,
    suppression_reason: String,
}

#[derive(Clone, Debug, FromRow, Serialize)]
struct TimelineRow {
    id: String,
    event_id: String,
    kind: String,
    actor: String,
    note: String,
    value: Option<f64>,
    created_at: i64,
}

#[derive(Clone, Debug, FromRow, Serialize)]
struct MaintenanceRow {
    id: String,
    name: String,
    reason: String,
    scope: String,
    starts_at: i64,
    ends_at: i64,
    enabled: bool,
    created_by: String,
    created_by_user_id: Option<String>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
struct MaintenanceResponse {
    #[serde(flatten)]
    window: MaintenanceRow,
    target_ids: Vec<String>,
}

#[derive(Clone, Debug, FromRow)]
struct ChannelRow {
    id: String,
    name: String,
    url_ciphertext: String,
    secret_ciphertext: Option<String>,
    headers_ciphertext: String,
    enabled: bool,
    created_at: i64,
    updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
struct ChannelResponse {
    id: String,
    name: String,
    masked_url: String,
    header_names: Vec<String>,
    has_secret: bool,
    enabled: bool,
    created_at: i64,
    updated_at: i64,
}

#[derive(Clone, Debug, FromRow, Serialize)]
struct DeliveryRow {
    id: String,
    event_id: Option<String>,
    channel_id: String,
    kind: String,
    status: String,
    payload: Value,
    channel_snapshot: Value,
    suppression_reason: String,
    attempts_count: i64,
    cycle_attempts: i64,
    manual_retry_count: i64,
    next_attempt_at: Option<i64>,
    lease_until: Option<i64>,
    #[serde(skip_serializing)]
    lease_token: Option<String>,
    last_error: String,
    created_at: i64,
    updated_at: i64,
    completed_at: Option<i64>,
}

#[derive(Clone, Debug, FromRow, Serialize)]
struct DeliveryAttemptRow {
    id: String,
    delivery_id: String,
    attempt_number: i64,
    http_status: Option<i64>,
    duration_ms: i64,
    error: String,
    response_excerpt: String,
    created_at: i64,
}

#[derive(Clone, Debug, Deserialize)]
struct PageQuery {
    page: Option<i64>,
    page_size: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
struct Page<T> {
    items: Vec<T>,
    page: i64,
    page_size: i64,
    total: i64,
    pages: i64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RuleRequest {
    pub name: String,
    pub metric: String,
    pub threshold: Option<f64>,
    pub duration_seconds: i64,
    pub severity: String,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default)]
    pub target_instance_ids: Vec<String>,
    #[serde(default)]
    pub channel_ids: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize)]
struct RuleResponse {
    #[serde(flatten)]
    rule: RuleRow,
    target_instance_ids: Vec<String>,
    channel_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct EnabledRequest {
    enabled: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct AcknowledgeRequest {
    #[serde(default)]
    note: String,
}

#[derive(Clone, Debug, Deserialize)]
struct MaintenanceRequest {
    name: String,
    #[serde(default)]
    reason: String,
    scope: String,
    #[serde(default)]
    target_ids: Vec<String>,
    starts_at: i64,
    ends_at: i64,
    #[serde(default = "default_true")]
    enabled: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct ChannelRequest {
    name: String,
    url: Option<String>,
    secret: Option<String>,
    #[serde(default)]
    clear_secret: bool,
    headers: Option<BTreeMap<String, String>>,
    #[serde(default = "default_true")]
    enabled: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct EventQuery {
    page: Option<i64>,
    page_size: Option<i64>,
    status: Option<String>,
    severity: Option<String>,
    metric: Option<String>,
    instance_id: Option<String>,
    suppressed: Option<bool>,
    from: Option<i64>,
    to: Option<i64>,
    search: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct DeliveryQuery {
    page: Option<i64>,
    page_size: Option<i64>,
    status: Option<String>,
    kind: Option<String>,
    channel_id: Option<String>,
    event_id: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_scope() -> String {
    "all".to_string()
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/summary", get(summary))
        .route("/rules", get(list_rules).post(create_rule))
        .route(
            "/rules/{id}",
            get(get_rule).put(update_rule).delete(delete_rule),
        )
        .route("/rules/{id}/enabled", patch(set_rule_enabled))
        .route("/events", get(list_events))
        .route("/events/{id}", get(get_event))
        .route("/events/{id}/acknowledge", post(acknowledge_event))
        .route(
            "/maintenance-windows",
            get(list_maintenance).post(create_maintenance),
        )
        .route(
            "/maintenance-windows/{id}",
            put(update_maintenance).delete(delete_maintenance),
        )
        .route("/webhook-channels", get(list_channels).post(create_channel))
        .route(
            "/webhook-channels/{id}",
            put(update_channel).delete(delete_channel),
        )
        .route("/webhook-channels/{id}/test", post(test_channel))
        .route("/deliveries", get(list_deliveries))
        .route("/deliveries/{id}", get(get_delivery))
        .route("/deliveries/{id}/retry", post(retry_delivery))
}

pub async fn ensure_schema(db: &PgPool) -> anyhow::Result<()> {
    sqlx::raw_sql(
        r#"
        ALTER TABLE metrics ADD COLUMN IF NOT EXISTS received_at BIGINT;
        UPDATE metrics SET received_at = ts WHERE received_at IS NULL;
        ALTER TABLE metrics ALTER COLUMN received_at SET NOT NULL;
        ALTER TABLE metrics ALTER COLUMN received_at SET DEFAULT (EXTRACT(EPOCH FROM NOW())::BIGINT);
        ALTER TABLE metrics ADD COLUMN IF NOT EXISTS latency_sampled_at BIGINT;
        UPDATE metrics SET latency_sampled_at = received_at
        WHERE latency_ms IS NOT NULL AND latency_sampled_at IS NULL;
        CREATE INDEX IF NOT EXISTS idx_metrics_instance_received
            ON metrics(instance_id, received_at DESC, id DESC);

        CREATE TABLE IF NOT EXISTS alert_rules (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            metric TEXT NOT NULL CHECK(metric IN ('node_offline', 'cpu_percent', 'memory_percent', 'disk_percent', 'latency_ms')),
            threshold DOUBLE PRECISION,
            duration_seconds BIGINT NOT NULL CHECK(duration_seconds >= 0),
            severity TEXT NOT NULL CHECK(severity IN ('warning', 'critical')),
            scope TEXT NOT NULL DEFAULT 'all' CHECK(scope IN ('all', 'specific')),
            enabled BOOLEAN NOT NULL DEFAULT TRUE,
            version BIGINT NOT NULL DEFAULT 1,
            created_by TEXT NOT NULL DEFAULT '',
            created_at BIGINT NOT NULL,
            updated_at BIGINT NOT NULL,
            CHECK((metric = 'node_offline' AND threshold IS NULL) OR
                  (metric <> 'node_offline' AND threshold IS NOT NULL))
        );
        CREATE TABLE IF NOT EXISTS alert_rule_targets (
            rule_id TEXT NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
            instance_id TEXT NOT NULL REFERENCES instances(id) ON DELETE CASCADE,
            PRIMARY KEY(rule_id, instance_id)
        );
        CREATE TABLE IF NOT EXISTS alert_webhook_channels (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            url_ciphertext TEXT NOT NULL,
            secret_ciphertext TEXT,
            headers_ciphertext TEXT NOT NULL,
            enabled BOOLEAN NOT NULL DEFAULT TRUE,
            created_at BIGINT NOT NULL,
            updated_at BIGINT NOT NULL,
            deleted_at BIGINT
        );
        CREATE TABLE IF NOT EXISTS alert_rule_channels (
            rule_id TEXT NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
            channel_id TEXT NOT NULL REFERENCES alert_webhook_channels(id),
            PRIMARY KEY(rule_id, channel_id)
        );
        CREATE TABLE IF NOT EXISTS alert_evaluation_states (
            rule_id TEXT NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
            instance_id TEXT NOT NULL,
            rule_version BIGINT NOT NULL,
            pending_since BIGINT,
            last_observed_at BIGINT,
            last_value DOUBLE PRECISION,
            last_sample_at BIGINT,
            match_count BIGINT NOT NULL DEFAULT 0,
            PRIMARY KEY(rule_id, instance_id)
        );
        CREATE TABLE IF NOT EXISTS alert_events (
            id TEXT PRIMARY KEY,
            rule_id TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            status TEXT NOT NULL CHECK(status IN ('firing', 'acknowledged', 'resolved')),
            severity TEXT NOT NULL CHECK(severity IN ('warning', 'critical')),
            metric TEXT NOT NULL,
            rule_snapshot JSONB NOT NULL,
            node_snapshot JSONB NOT NULL,
            threshold DOUBLE PRECISION,
            duration_seconds BIGINT NOT NULL,
            current_value DOUBLE PRECISION,
            first_observed_at BIGINT NOT NULL,
            fired_at BIGINT NOT NULL,
            last_observed_at BIGINT NOT NULL,
            match_count BIGINT NOT NULL DEFAULT 1,
            acknowledged_by TEXT,
            acknowledged_by_user_id TEXT,
            acknowledged_at BIGINT,
            acknowledge_note TEXT NOT NULL DEFAULT '',
            resolved_at BIGINT,
            resolution_reason TEXT NOT NULL DEFAULT '',
            suppressed BOOLEAN NOT NULL DEFAULT FALSE,
            suppression_reason TEXT NOT NULL DEFAULT ''
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_alert_events_active
            ON alert_events(rule_id, instance_id) WHERE status <> 'resolved';
        CREATE INDEX IF NOT EXISTS idx_alert_events_status_time
            ON alert_events(status, fired_at DESC);
        CREATE INDEX IF NOT EXISTS idx_alert_events_instance
            ON alert_events(instance_id, fired_at DESC);
        CREATE TABLE IF NOT EXISTS alert_event_timeline (
            id TEXT PRIMARY KEY,
            event_id TEXT NOT NULL REFERENCES alert_events(id) ON DELETE CASCADE,
            kind TEXT NOT NULL,
            actor TEXT NOT NULL DEFAULT 'system',
            note TEXT NOT NULL DEFAULT '',
            value DOUBLE PRECISION,
            created_at BIGINT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_alert_timeline_event
            ON alert_event_timeline(event_id, created_at, id);
        CREATE TABLE IF NOT EXISTS alert_event_channels (
            event_id TEXT NOT NULL REFERENCES alert_events(id) ON DELETE CASCADE,
            channel_id TEXT NOT NULL,
            PRIMARY KEY(event_id, channel_id)
        );
        CREATE TABLE IF NOT EXISTS alert_maintenance_windows (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            reason TEXT NOT NULL DEFAULT '',
            scope TEXT NOT NULL CHECK(scope IN ('global', 'rule', 'node')),
            starts_at BIGINT NOT NULL,
            ends_at BIGINT NOT NULL,
            enabled BOOLEAN NOT NULL DEFAULT TRUE,
            created_by TEXT NOT NULL,
            created_by_user_id TEXT,
            created_at BIGINT NOT NULL,
            updated_at BIGINT NOT NULL,
            CHECK(ends_at > starts_at)
        );
        CREATE TABLE IF NOT EXISTS alert_maintenance_targets (
            window_id TEXT NOT NULL REFERENCES alert_maintenance_windows(id) ON DELETE CASCADE,
            target_id TEXT NOT NULL,
            PRIMARY KEY(window_id, target_id)
        );
        CREATE INDEX IF NOT EXISTS idx_alert_maintenance_active
            ON alert_maintenance_windows(enabled, starts_at, ends_at);
        CREATE TABLE IF NOT EXISTS alert_deliveries (
            id TEXT PRIMARY KEY,
            event_id TEXT REFERENCES alert_events(id) ON DELETE CASCADE,
            channel_id TEXT NOT NULL,
            kind TEXT NOT NULL CHECK(kind IN ('alert.firing', 'alert.acknowledged', 'alert.resolved', 'webhook.test')),
            status TEXT NOT NULL CHECK(status IN ('pending', 'processing', 'succeeded', 'failed', 'suppressed')),
            payload JSONB NOT NULL,
            channel_snapshot JSONB NOT NULL,
            suppression_reason TEXT NOT NULL DEFAULT '',
            attempts_count BIGINT NOT NULL DEFAULT 0,
            cycle_attempts BIGINT NOT NULL DEFAULT 0,
            manual_retry_count BIGINT NOT NULL DEFAULT 0,
            next_attempt_at BIGINT,
            lease_until BIGINT,
            lease_token TEXT,
            last_error TEXT NOT NULL DEFAULT '',
            created_at BIGINT NOT NULL,
            updated_at BIGINT NOT NULL,
            completed_at BIGINT
        );
        CREATE INDEX IF NOT EXISTS idx_alert_deliveries_work
            ON alert_deliveries(status, next_attempt_at, lease_until);
        CREATE INDEX IF NOT EXISTS idx_alert_deliveries_event
            ON alert_deliveries(event_id, created_at DESC);
        ALTER TABLE alert_deliveries ADD COLUMN IF NOT EXISTS lease_token TEXT;
        CREATE TABLE IF NOT EXISTS alert_delivery_attempts (
            id TEXT PRIMARY KEY,
            delivery_id TEXT NOT NULL REFERENCES alert_deliveries(id) ON DELETE CASCADE,
            attempt_number BIGINT NOT NULL,
            http_status BIGINT,
            duration_ms BIGINT NOT NULL,
            error TEXT NOT NULL DEFAULT '',
            response_excerpt TEXT NOT NULL DEFAULT '',
            created_at BIGINT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_alert_attempts_delivery
            ON alert_delivery_attempts(delivery_id, created_at, id);
        INSERT INTO settings(key, value) VALUES('alert_retention_days', '180')
        ON CONFLICT(key) DO NOTHING;
        "#,
    )
    .execute(db)
    .await?;
    Ok(())
}

fn normalize_page(page: Option<i64>, page_size: Option<i64>) -> (i64, i64, i64) {
    let page = page.unwrap_or(1).max(1);
    let page_size = page_size
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);
    (page, page_size, (page - 1) * page_size)
}

fn page_count(total: i64, page_size: i64) -> i64 {
    if total == 0 {
        0
    } else {
        (total + page_size - 1) / page_size
    }
}

fn trimmed(value: &str, max_bytes: usize, label: &str) -> AppResult<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > max_bytes {
        return Err(AppError::bad_request(format!(
            "{label}不能为空且不能超过 {max_bytes} 字节"
        )));
    }
    Ok(value.to_string())
}

fn validate_rule(payload: &RuleRequest) -> AppResult<RuleRequest> {
    let mut normalized = payload.clone();
    normalized.name = trimmed(&payload.name, MAX_NAME_BYTES, "规则名称")?;
    if !matches!(
        normalized.metric.as_str(),
        "node_offline" | "cpu_percent" | "memory_percent" | "disk_percent" | "latency_ms"
    ) {
        return Err(AppError::bad_request("不支持的告警指标"));
    }
    if !matches!(normalized.severity.as_str(), "warning" | "critical") {
        return Err(AppError::bad_request("严重级别必须是 warning 或 critical"));
    }
    if !matches!(normalized.scope.as_str(), "all" | "specific") {
        return Err(AppError::bad_request("规则范围必须是 all 或 specific"));
    }
    if normalized.scope == "specific" && normalized.target_instance_ids.is_empty() {
        return Err(AppError::bad_request("指定节点规则至少需要一个节点"));
    }
    if normalized.duration_seconds < 0 || normalized.duration_seconds > 365 * 24 * 3600 {
        return Err(AppError::bad_request("持续时间超出允许范围"));
    }
    if normalized.metric == "node_offline" {
        if normalized.threshold.is_some() {
            return Err(AppError::bad_request("节点离线规则不能设置阈值"));
        }
    } else {
        let Some(threshold) = normalized.threshold else {
            return Err(AppError::bad_request("阈值规则必须设置阈值"));
        };
        if !threshold.is_finite() || threshold < 0.0 {
            return Err(AppError::bad_request("阈值必须是非负有限数值"));
        }
        if matches!(
            normalized.metric.as_str(),
            "cpu_percent" | "memory_percent" | "disk_percent"
        ) && threshold > 100.0
        {
            return Err(AppError::bad_request("百分比阈值必须在 0 到 100 之间"));
        }
    }
    normalized.target_instance_ids.sort();
    normalized.target_instance_ids.dedup();
    if normalized.scope == "all" {
        normalized.target_instance_ids.clear();
    }
    normalized.channel_ids.sort();
    normalized.channel_ids.dedup();
    Ok(normalized)
}

async fn validate_rule_links(
    tx: &mut Transaction<'_, Postgres>,
    payload: &RuleRequest,
) -> AppResult<()> {
    for instance_id in &payload.target_instance_ids {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM instances WHERE id = $1 AND approved = 1)",
        )
        .bind(instance_id)
        .fetch_one(&mut **tx)
        .await?;
        if !exists {
            return Err(AppError::bad_request(format!(
                "节点 {instance_id} 不存在或未批准"
            )));
        }
    }
    for channel_id in &payload.channel_ids {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM alert_webhook_channels WHERE id = $1 AND deleted_at IS NULL)",
        )
        .bind(channel_id)
        .fetch_one(&mut **tx)
        .await?;
        if !exists {
            return Err(AppError::bad_request(format!(
                "Webhook 渠道 {channel_id} 不存在"
            )));
        }
    }
    Ok(())
}

async fn replace_rule_links(
    tx: &mut Transaction<'_, Postgres>,
    rule_id: &str,
    payload: &RuleRequest,
) -> AppResult<()> {
    sqlx::query("DELETE FROM alert_rule_targets WHERE rule_id = $1")
        .bind(rule_id)
        .execute(&mut **tx)
        .await?;
    if payload.scope == "specific" {
        for instance_id in &payload.target_instance_ids {
            sqlx::query("INSERT INTO alert_rule_targets(rule_id, instance_id) VALUES($1, $2)")
                .bind(rule_id)
                .bind(instance_id)
                .execute(&mut **tx)
                .await?;
        }
    }
    sqlx::query("DELETE FROM alert_rule_channels WHERE rule_id = $1")
        .bind(rule_id)
        .execute(&mut **tx)
        .await?;
    for channel_id in &payload.channel_ids {
        sqlx::query("INSERT INTO alert_rule_channels(rule_id, channel_id) VALUES($1, $2)")
            .bind(rule_id)
            .bind(channel_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn rule_response(db: &PgPool, rule: RuleRow) -> AppResult<RuleResponse> {
    let targets = sqlx::query_scalar::<_, String>(
        "SELECT instance_id FROM alert_rule_targets WHERE rule_id = $1 ORDER BY instance_id",
    )
    .bind(&rule.id)
    .fetch_all(db)
    .await?;
    let channels = sqlx::query_scalar::<_, String>(
        "SELECT channel_id FROM alert_rule_channels WHERE rule_id = $1 ORDER BY channel_id",
    )
    .bind(&rule.id)
    .fetch_all(db)
    .await?;
    Ok(RuleResponse {
        rule,
        target_instance_ids: targets,
        channel_ids: channels,
    })
}

async fn load_rule(db: &PgPool, id: &str) -> AppResult<RuleRow> {
    sqlx::query_as::<_, RuleRow>("SELECT * FROM alert_rules WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "告警规则不存在"))
}

async fn list_rules(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<RuleResponse>>> {
    require_admin(&state, &headers).await?;
    let (page, page_size, offset) = normalize_page(query.page, query.page_size);
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM alert_rules")
        .fetch_one(&state.db)
        .await?;
    let rows = sqlx::query_as::<_, RuleRow>(
        "SELECT * FROM alert_rules ORDER BY updated_at DESC, id LIMIT $1 OFFSET $2",
    )
    .bind(page_size)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        items.push(rule_response(&state.db, row).await?);
    }
    Ok(Json(Page {
        items,
        page,
        page_size,
        total,
        pages: page_count(total, page_size),
    }))
}

async fn get_rule(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<RuleResponse>> {
    require_admin(&state, &headers).await?;
    Ok(Json(
        rule_response(&state.db, load_rule(&state.db, &id).await?).await?,
    ))
}

async fn create_rule(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<RuleRequest>,
) -> AppResult<(StatusCode, Json<RuleResponse>)> {
    let admin = require_admin(&state, &headers).await?;
    let payload = validate_rule(&payload)?;
    let id = Uuid::new_v4().to_string();
    let now = now_ts();
    let mut tx = state.db.begin().await?;
    validate_rule_links(&mut tx, &payload).await?;
    sqlx::query(
        r#"
        INSERT INTO alert_rules(
            id, name, metric, threshold, duration_seconds, severity, scope,
            enabled, version, created_by, created_at, updated_at
        ) VALUES($1,$2,$3,$4,$5,$6,$7,$8,1,$9,$10,$10)
        "#,
    )
    .bind(&id)
    .bind(&payload.name)
    .bind(&payload.metric)
    .bind(payload.threshold)
    .bind(payload.duration_seconds)
    .bind(&payload.severity)
    .bind(&payload.scope)
    .bind(payload.enabled)
    .bind(&admin.username)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    replace_rule_links(&mut tx, &id, &payload).await?;
    audit_action_tx(&mut tx, &admin, "alert_rule_create", &id, "创建告警规则").await?;
    tx.commit().await?;
    let response = rule_response(&state.db, load_rule(&state.db, &id).await?).await?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn update_rule_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    payload: &RuleRequest,
    now: i64,
) -> AppResult<bool> {
    let old = sqlx::query_as::<_, RuleRow>("SELECT * FROM alert_rules WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "告警规则不存在"))?;
    let old_targets = sqlx::query_scalar::<_, String>(
        "SELECT instance_id FROM alert_rule_targets WHERE rule_id = $1 ORDER BY instance_id",
    )
    .bind(id)
    .fetch_all(&mut **tx)
    .await?;
    let condition_changed = old.metric != payload.metric
        || old.threshold != payload.threshold
        || old.duration_seconds != payload.duration_seconds
        || old.severity != payload.severity
        || old.scope != payload.scope
        || old_targets != payload.target_instance_ids;
    validate_rule_links(tx, payload).await?;
    if condition_changed {
        resolve_rule_events_tx(tx, id, now, "rule_conditions_changed").await?;
        sqlx::query("DELETE FROM alert_evaluation_states WHERE rule_id = $1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query(
        r#"
        UPDATE alert_rules SET name=$1, metric=$2, threshold=$3, duration_seconds=$4,
            severity=$5, scope=$6, enabled=$7,
            version=version + CASE WHEN $8 THEN 1 ELSE 0 END, updated_at=$9
        WHERE id=$10
        "#,
    )
    .bind(&payload.name)
    .bind(&payload.metric)
    .bind(payload.threshold)
    .bind(payload.duration_seconds)
    .bind(&payload.severity)
    .bind(&payload.scope)
    .bind(payload.enabled)
    .bind(condition_changed)
    .bind(now)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    replace_rule_links(tx, id, payload).await?;
    if old.enabled && !payload.enabled && !condition_changed {
        resolve_rule_events_tx(tx, id, now, "rule_disabled").await?;
        sqlx::query("DELETE FROM alert_evaluation_states WHERE rule_id = $1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(condition_changed)
}

async fn update_rule(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(payload): Json<RuleRequest>,
) -> AppResult<Json<RuleResponse>> {
    let admin = require_admin(&state, &headers).await?;
    let payload = validate_rule(&payload)?;
    let now = now_ts();
    let mut tx = state.db.begin().await?;
    update_rule_tx(&mut tx, &id, &payload, now).await?;
    audit_action_tx(&mut tx, &admin, "alert_rule_update", &id, "更新告警规则").await?;
    tx.commit().await?;
    Ok(Json(
        rule_response(&state.db, load_rule(&state.db, &id).await?).await?,
    ))
}

async fn set_rule_enabled(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(payload): Json<EnabledRequest>,
) -> AppResult<Json<RuleResponse>> {
    let admin = require_admin(&state, &headers).await?;
    let mut tx = state.db.begin().await?;
    let old = sqlx::query_as::<_, RuleRow>("SELECT * FROM alert_rules WHERE id=$1 FOR UPDATE")
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "告警规则不存在"))?;
    if old.enabled != payload.enabled {
        let now = now_ts();
        if !payload.enabled {
            resolve_rule_events_tx(&mut tx, &id, now, "rule_disabled").await?;
            sqlx::query("DELETE FROM alert_evaluation_states WHERE rule_id = $1")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE alert_rules SET enabled=$1, updated_at=$2 WHERE id=$3")
            .bind(payload.enabled)
            .bind(now)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        audit_action_tx(
            &mut tx,
            &admin,
            "alert_rule_enabled",
            &id,
            if payload.enabled {
                "启用告警规则"
            } else {
                "停用告警规则"
            },
        )
        .await?;
        tx.commit().await?;
    } else {
        tx.commit().await?;
    }
    Ok(Json(
        rule_response(&state.db, load_rule(&state.db, &id).await?).await?,
    ))
}

async fn delete_rule(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let admin = require_admin(&state, &headers).await?;
    let mut tx = state.db.begin().await?;
    sqlx::query_scalar::<_, String>("SELECT id FROM alert_rules WHERE id=$1 FOR UPDATE")
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "告警规则不存在"))?;
    resolve_rule_events_tx(&mut tx, &id, now_ts(), "rule_deleted").await?;
    remove_maintenance_target_tx(&mut tx, "rule", &id).await?;
    sqlx::query("DELETE FROM alert_rules WHERE id = $1")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    audit_action_tx(&mut tx, &admin, "alert_rule_delete", &id, "删除告警规则").await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn audit_action_tx(
    tx: &mut Transaction<'_, Postgres>,
    admin: &AdminPrincipal,
    action: &str,
    target: &str,
    detail: &str,
) -> AppResult<()> {
    write_action_log(
        &mut **tx,
        &admin.username,
        Some(&admin.user_id),
        action,
        target,
        detail,
    )
    .await?;
    Ok(())
}

async fn remove_maintenance_target_tx(
    tx: &mut Transaction<'_, Postgres>,
    scope: &str,
    target_id: &str,
) -> AppResult<()> {
    sqlx::query(
        r#"
        DELETE FROM alert_maintenance_targets t
        USING alert_maintenance_windows w
        WHERE t.window_id=w.id AND w.scope=$1 AND t.target_id=$2
        "#,
    )
    .bind(scope)
    .bind(target_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        r#"
        DELETE FROM alert_maintenance_windows w
        WHERE w.scope=$1 AND NOT EXISTS(
            SELECT 1 FROM alert_maintenance_targets t WHERE t.window_id=w.id
        )
        "#,
    )
    .bind(scope)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn remove_instance_maintenance_target(
    tx: &mut Transaction<'_, Postgres>,
    instance_id: &str,
) -> AppResult<()> {
    remove_maintenance_target_tx(tx, "node", instance_id).await
}

#[derive(Clone, Debug, FromRow)]
struct EvaluationRow {
    pending_since: Option<i64>,
    last_observed_at: Option<i64>,
    last_value: Option<f64>,
    last_sample_at: Option<i64>,
    match_count: i64,
}

#[derive(Clone, Copy, Debug)]
enum RuleObservation {
    Threshold { value: f64, abnormal: bool },
    Connection,
}

fn percentage(used: i64, total: i64) -> Option<f64> {
    if used < 0 || total <= 0 || used > total {
        return None;
    }
    let value = used as f64 / total as f64 * 100.0;
    value.is_finite().then_some(value)
}

fn metric_value(metric: &str, observation: &MetricObservation) -> Option<(f64, i64)> {
    match metric {
        "cpu_percent" => observation
            .cpu_percent
            .is_finite()
            .then_some(observation.cpu_percent)
            .filter(|value| (0.0..=100.0).contains(value))
            .map(|value| (value, observation.received_at)),
        "memory_percent" => percentage(observation.memory_used, observation.memory_total)
            .map(|value| (value, observation.received_at)),
        "disk_percent" => percentage(observation.disk_used, observation.disk_total)
            .map(|value| (value, observation.received_at)),
        "latency_ms" => observation
            .latency_ms
            .filter(|value| value.is_finite() && *value >= 0.0)
            .zip(observation.latency_sampled_at),
        _ => None,
    }
}

fn node_offline_suppresses(metric: &str, has_active_offline_event: bool) -> bool {
    has_active_offline_event && metric != "node_offline"
}

fn connection_observation(online: bool) -> (f64, bool) {
    if online { (0.0, false) } else { (1.0, true) }
}

fn startup_grace_defers_offline(online: bool, started_at: i64, now: i64) -> bool {
    !online && now.saturating_sub(started_at) < STARTUP_RECONNECT_GRACE_SECONDS
}

fn observation_is_stale(
    previous_sample_at: Option<i64>,
    previous_value: Option<f64>,
    sample_at: i64,
    value: f64,
) -> bool {
    previous_sample_at.is_some_and(|previous_sample_at| {
        sample_at < previous_sample_at
            || (sample_at == previous_sample_at && previous_value == Some(value))
    })
}

fn resolution_values(
    current_value: Option<f64>,
    last_observed_at: i64,
    recovery_observation: Option<(f64, i64)>,
) -> (Option<f64>, i64) {
    recovery_observation
        .map(|(value, observed_at)| (Some(value), observed_at))
        .unwrap_or((current_value, last_observed_at))
}

fn pending_should_fire(
    duration_seconds: i64,
    pending_since: Option<i64>,
    last_observed_at: Option<i64>,
    observed_at: i64,
) -> bool {
    duration_seconds == 0
        || (pending_since.is_some()
            && last_observed_at.is_some()
            && observed_at.saturating_sub(pending_since.unwrap_or(observed_at)) >= duration_seconds)
}

async fn node_snapshot_tx(
    tx: &mut Transaction<'_, Postgres>,
    instance_id: &str,
) -> AppResult<Value> {
    let snapshot = sqlx::query_scalar::<_, Value>(
        r#"
        SELECT jsonb_build_object(
            'id', id, 'name', name, 'hostname', hostname, 'region', region,
            'os', os, 'arch', arch, 'agent_version', agent_version
        ) FROM instances WHERE id = $1
        "#,
    )
    .bind(instance_id)
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or_else(|| json!({"id": instance_id}));
    Ok(snapshot)
}

fn rule_snapshot(rule: &RuleRow) -> Value {
    json!({
        "id": rule.id,
        "name": rule.name,
        "metric": rule.metric,
        "threshold": rule.threshold,
        "duration_seconds": rule.duration_seconds,
        "severity": rule.severity,
        "version": rule.version,
    })
}

async fn suppression_reason_tx(
    tx: &mut Transaction<'_, Postgres>,
    rule: &RuleRow,
    instance_id: &str,
    now: i64,
) -> AppResult<Option<String>> {
    let maintenance = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT w.name, w.reason
        FROM alert_maintenance_windows w
        WHERE w.enabled = TRUE AND w.starts_at <= $1 AND w.ends_at > $1
          AND (
            w.scope = 'global' OR
            (w.scope = 'rule' AND EXISTS(
                SELECT 1 FROM alert_maintenance_targets t
                WHERE t.window_id = w.id AND t.target_id = $2
            )) OR
            (w.scope = 'node' AND EXISTS(
                SELECT 1 FROM alert_maintenance_targets t
                WHERE t.window_id = w.id AND t.target_id = $3
            ))
          )
        ORDER BY w.starts_at, w.id
        LIMIT 1
        "#,
    )
    .bind(now)
    .bind(&rule.id)
    .bind(instance_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((name, reason)) = maintenance {
        return Ok(Some(if reason.is_empty() {
            format!("maintenance:{name}")
        } else {
            format!("maintenance:{name}: {reason}")
        }));
    }
    if node_offline_suppresses(&rule.metric, true) {
        let offline: Option<String> = sqlx::query_scalar(
            r#"
            SELECT id FROM alert_events
            WHERE instance_id = $1 AND metric = 'node_offline'
              AND status IN ('firing', 'acknowledged')
            ORDER BY fired_at LIMIT 1
            "#,
        )
        .bind(instance_id)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(event_id) = offline {
            return Ok(Some(format!("node_offline:{event_id}")));
        }
    }
    Ok(None)
}

fn delivery_payload(
    event: &EventRow,
    kind: &str,
    actor: Option<&str>,
    note: Option<&str>,
) -> Value {
    json!({
        "version": 1,
        "type": kind,
        "event": {
            "id": event.id,
            "status": event.status,
            "severity": event.severity,
            "metric": event.metric,
            "instance_id": event.instance_id,
            "current_value": event.current_value,
            "threshold": event.threshold,
            "duration_seconds": event.duration_seconds,
            "first_observed_at": event.first_observed_at,
            "fired_at": event.fired_at,
            "last_observed_at": event.last_observed_at,
            "acknowledged_at": event.acknowledged_at,
            "resolved_at": event.resolved_at,
            "resolution_reason": event.resolution_reason,
        },
        "rule": event.rule_snapshot,
        "node": event.node_snapshot,
        "actor": actor,
        "note": note,
    })
}

async fn insert_delivery_tx(
    tx: &mut Transaction<'_, Postgres>,
    event: &EventRow,
    channel_id: &str,
    channel_name: &str,
    kind: &str,
    suppression: Option<&str>,
    actor: Option<&str>,
    note: Option<&str>,
    now: i64,
) -> AppResult<()> {
    let id = Uuid::new_v4().to_string();
    let status = if suppression.is_some() {
        "suppressed"
    } else {
        "pending"
    };
    sqlx::query(
        r#"
        INSERT INTO alert_deliveries(
            id,event_id,channel_id,kind,status,payload,channel_snapshot,
            suppression_reason,next_attempt_at,created_at,updated_at,completed_at
        ) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$10,$11)
        "#,
    )
    .bind(id)
    .bind(&event.id)
    .bind(channel_id)
    .bind(kind)
    .bind(status)
    .bind(delivery_payload(event, kind, actor, note))
    .bind(json!({"id": channel_id, "name": channel_name}))
    .bind(suppression.unwrap_or_default())
    .bind(suppression.is_none().then_some(now))
    .bind(now)
    .bind(suppression.is_some().then_some(now))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn enqueue_event_lifecycle_tx(
    tx: &mut Transaction<'_, Postgres>,
    event: &EventRow,
    kind: &str,
    suppression: Option<&str>,
    actor: Option<&str>,
    note: Option<&str>,
    now: i64,
) -> AppResult<()> {
    let channels = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT c.id, c.name
        FROM alert_event_channels ec
        JOIN alert_webhook_channels c ON c.id = ec.channel_id
        WHERE ec.event_id = $1 AND c.enabled = TRUE AND c.deleted_at IS NULL
        ORDER BY c.id
        "#,
    )
    .bind(&event.id)
    .fetch_all(&mut **tx)
    .await?;
    for (channel_id, channel_name) in channels {
        if kind == "alert.firing" {
            let already_deliverable: bool = sqlx::query_scalar(
                r#"
                SELECT EXISTS(
                    SELECT 1 FROM alert_deliveries
                    WHERE event_id=$1 AND channel_id=$2 AND kind='alert.firing'
                      AND status <> 'suppressed'
                )
                "#,
            )
            .bind(&event.id)
            .bind(&channel_id)
            .fetch_one(&mut **tx)
            .await?;
            if already_deliverable {
                continue;
            }
            if suppression.is_some() {
                let already_suppressed: bool = sqlx::query_scalar(
                    r#"
                    SELECT EXISTS(
                        SELECT 1 FROM alert_deliveries
                        WHERE event_id=$1 AND channel_id=$2 AND kind='alert.firing'
                          AND status='suppressed'
                    )
                    "#,
                )
                .bind(&event.id)
                .bind(&channel_id)
                .fetch_one(&mut **tx)
                .await?;
                if already_suppressed {
                    continue;
                }
            }
        }
        insert_delivery_tx(
            tx,
            event,
            &channel_id,
            &channel_name,
            kind,
            suppression,
            actor,
            note,
            now,
        )
        .await?;
    }
    Ok(())
}

async fn resolve_event_tx(
    tx: &mut Transaction<'_, Postgres>,
    mut event: EventRow,
    now: i64,
    reason: &str,
    recovery_observation: Option<(f64, i64)>,
) -> AppResult<()> {
    if event.status == "resolved" {
        return Ok(());
    }
    (event.current_value, event.last_observed_at) = resolution_values(
        event.current_value,
        event.last_observed_at,
        recovery_observation,
    );
    sqlx::query(
        r#"
        UPDATE alert_events SET status='resolved', resolved_at=$1,
            resolution_reason=$2,current_value=$3,last_observed_at=$4,
            suppressed=FALSE,suppression_reason=''
        WHERE id=$5 AND status <> 'resolved'
        "#,
    )
    .bind(now)
    .bind(reason)
    .bind(event.current_value)
    .bind(event.last_observed_at)
    .bind(&event.id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO alert_event_timeline(id,event_id,kind,actor,note,value,created_at) VALUES($1,$2,'resolved','system',$3,$4,$5)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&event.id)
    .bind(reason)
    .bind(event.current_value)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    event.status = "resolved".to_string();
    event.resolved_at = Some(now);
    event.resolution_reason = reason.to_string();
    event.suppressed = false;
    event.suppression_reason.clear();
    enqueue_event_lifecycle_tx(
        tx,
        &event,
        "alert.resolved",
        None,
        Some("system"),
        Some(reason),
        now,
    )
    .await
}

async fn resolve_rule_events_tx(
    tx: &mut Transaction<'_, Postgres>,
    rule_id: &str,
    now: i64,
    reason: &str,
) -> AppResult<()> {
    let events = sqlx::query_as::<_, EventRow>(
        "SELECT * FROM alert_events WHERE rule_id=$1 AND status <> 'resolved' FOR UPDATE",
    )
    .bind(rule_id)
    .fetch_all(&mut **tx)
    .await?;
    for event in events {
        resolve_event_tx(tx, event, now, reason, None).await?;
    }
    Ok(())
}

pub async fn resolve_instance(
    tx: &mut Transaction<'_, Postgres>,
    instance_id: &str,
    reason: &str,
) -> AppResult<()> {
    let now = now_ts();
    let events = sqlx::query_as::<_, EventRow>(
        "SELECT * FROM alert_events WHERE instance_id=$1 AND status <> 'resolved' FOR UPDATE",
    )
    .bind(instance_id)
    .fetch_all(&mut **tx)
    .await?;
    for event in events {
        resolve_event_tx(tx, event, now, reason, None).await?;
    }
    sqlx::query("DELETE FROM alert_evaluation_states WHERE instance_id=$1")
        .bind(instance_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn reset_metric_pending_states(
    db: &PgPool,
    instance_id: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(instance_id) = instance_id {
        sqlx::query(
            r#"
            DELETE FROM alert_evaluation_states s USING alert_rules r
            WHERE s.rule_id=r.id AND r.metric <> 'node_offline'
              AND s.instance_id=$1 AND s.pending_since IS NOT NULL
              AND NOT EXISTS(
                  SELECT 1 FROM alert_events e
                  WHERE e.rule_id=s.rule_id AND e.instance_id=s.instance_id
                    AND e.status <> 'resolved'
              )
            "#,
        )
        .bind(instance_id)
        .execute(db)
        .await?;
    } else {
        sqlx::query(
            r#"
            DELETE FROM alert_evaluation_states s USING alert_rules r
            WHERE s.rule_id=r.id AND r.metric <> 'node_offline'
              AND s.pending_since IS NOT NULL
              AND NOT EXISTS(
                  SELECT 1 FROM alert_events e
                  WHERE e.rule_id=s.rule_id AND e.instance_id=s.instance_id
                    AND e.status <> 'resolved'
              )
            "#,
        )
        .execute(db)
        .await?;
    }
    Ok(())
}

async fn process_rule_observation(
    state: &AppState,
    rule: &RuleRow,
    instance_id: &str,
    observation: RuleObservation,
    observed_at: i64,
    sample_at: i64,
) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    let current = sqlx::query_as::<_, (bool, i64)>(
        "SELECT enabled,version FROM alert_rules WHERE id=$1 FOR SHARE",
    )
    .bind(&rule.id)
    .fetch_optional(&mut *tx)
    .await?;
    if current != Some((true, rule.version)) {
        tx.commit().await?;
        return Ok(());
    }
    let eligible = sqlx::query_scalar::<_, bool>(
        "SELECT (approved=1 AND disabled=0) FROM instances WHERE id=$1 FOR SHARE",
    )
    .bind(instance_id)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    if !eligible {
        sqlx::query("DELETE FROM alert_evaluation_states WHERE rule_id=$1 AND instance_id=$2")
            .bind(&rule.id)
            .bind(instance_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(());
    }
    sqlx::query(
        r#"
        INSERT INTO alert_evaluation_states(
            rule_id,instance_id,rule_version,pending_since,last_observed_at,
            last_value,last_sample_at,match_count
        ) VALUES($1,$2,$3,NULL,NULL,NULL,NULL,0)
        ON CONFLICT(rule_id,instance_id) DO UPDATE SET
            rule_version=EXCLUDED.rule_version,pending_since=NULL,last_observed_at=NULL,
            last_value=NULL,last_sample_at=NULL,match_count=0
        WHERE alert_evaluation_states.rule_version <> EXCLUDED.rule_version
        "#,
    )
    .bind(&rule.id)
    .bind(instance_id)
    .bind(rule.version)
    .execute(&mut *tx)
    .await?;
    let evaluation = sqlx::query_as::<_, EvaluationRow>(
        r#"
        SELECT pending_since,last_observed_at,last_value,last_sample_at,match_count
        FROM alert_evaluation_states
        WHERE rule_id=$1 AND instance_id=$2 FOR UPDATE
        "#,
    )
    .bind(&rule.id)
    .bind(instance_id)
    .fetch_one(&mut *tx)
    .await?;
    let (value, abnormal) = match observation {
        RuleObservation::Threshold { value, abnormal } => (value, abnormal),
        RuleObservation::Connection => {
            let online = state.agents.read().await.contains_key(instance_id);
            connection_observation(online)
        }
    };
    if observation_is_stale(
        evaluation.last_sample_at,
        evaluation.last_value,
        sample_at,
        value,
    ) {
        tx.commit().await?;
        return Ok(());
    }

    let active = sqlx::query_as::<_, EventRow>(
        r#"
        SELECT * FROM alert_events
        WHERE rule_id=$1 AND instance_id=$2 AND status <> 'resolved'
        FOR UPDATE
        "#,
    )
    .bind(&rule.id)
    .bind(instance_id)
    .fetch_optional(&mut *tx)
    .await?;

    if !abnormal {
        sqlx::query(
            r#"
            UPDATE alert_evaluation_states SET pending_since=NULL,last_observed_at=$1,
                last_value=$2,last_sample_at=$3,match_count=0
            WHERE rule_id=$4 AND instance_id=$5
            "#,
        )
        .bind(observed_at)
        .bind(value)
        .bind(sample_at)
        .bind(&rule.id)
        .bind(instance_id)
        .execute(&mut *tx)
        .await?;
        if let Some(event) = active {
            resolve_event_tx(
                &mut tx,
                event,
                observed_at,
                "condition_recovered",
                Some((value, observed_at)),
            )
            .await?;
        }
        tx.commit().await?;
        return Ok(());
    }

    let suppression = suppression_reason_tx(&mut tx, rule, instance_id, observed_at).await?;
    if let Some(mut event) = active {
        let match_count = event.match_count.saturating_add(1);
        sqlx::query(
            r#"
            UPDATE alert_events SET current_value=$1,last_observed_at=$2,match_count=$3,
                suppressed=$4,suppression_reason=$5 WHERE id=$6
            "#,
        )
        .bind(value)
        .bind(observed_at)
        .bind(match_count)
        .bind(suppression.is_some())
        .bind(suppression.as_deref().unwrap_or_default())
        .bind(&event.id)
        .execute(&mut *tx)
        .await?;
        event.current_value = Some(value);
        event.last_observed_at = observed_at;
        event.match_count = match_count;
        event.suppressed = suppression.is_some();
        event.suppression_reason = suppression.clone().unwrap_or_default();
        enqueue_event_lifecycle_tx(
            &mut tx,
            &event,
            "alert.firing",
            suppression.as_deref(),
            None,
            None,
            observed_at,
        )
        .await?;
        sqlx::query(
            r#"
            UPDATE alert_evaluation_states SET last_observed_at=$1,last_value=$2,
                last_sample_at=$3,match_count=$4
            WHERE rule_id=$5 AND instance_id=$6
            "#,
        )
        .bind(observed_at)
        .bind(value)
        .bind(sample_at)
        .bind(evaluation.match_count.saturating_add(1))
        .bind(&rule.id)
        .bind(instance_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(());
    }

    let pending_since = evaluation.pending_since.unwrap_or(observed_at);
    let match_count = evaluation.match_count.saturating_add(1);
    let should_fire = pending_should_fire(
        rule.duration_seconds,
        evaluation.pending_since,
        evaluation.last_observed_at,
        observed_at,
    );
    sqlx::query(
        r#"
        UPDATE alert_evaluation_states SET pending_since=$1,last_observed_at=$2,
            last_value=$3,last_sample_at=$4,match_count=$5
        WHERE rule_id=$6 AND instance_id=$7
        "#,
    )
    .bind(pending_since)
    .bind(observed_at)
    .bind(value)
    .bind(sample_at)
    .bind(match_count)
    .bind(&rule.id)
    .bind(instance_id)
    .execute(&mut *tx)
    .await?;
    if should_fire {
        let event_id = Uuid::new_v4().to_string();
        let snapshot = node_snapshot_tx(&mut tx, instance_id).await?;
        sqlx::query(
            r#"
            INSERT INTO alert_events(
                id,rule_id,instance_id,status,severity,metric,rule_snapshot,node_snapshot,
                threshold,duration_seconds,current_value,first_observed_at,fired_at,
                last_observed_at,match_count,suppressed,suppression_reason
            ) VALUES($1,$2,$3,'firing',$4,$5,$6,$7,$8,$9,$10,$11,$12,$12,$13,$14,$15)
            "#,
        )
        .bind(&event_id)
        .bind(&rule.id)
        .bind(instance_id)
        .bind(&rule.severity)
        .bind(&rule.metric)
        .bind(rule_snapshot(rule))
        .bind(snapshot)
        .bind(rule.threshold)
        .bind(rule.duration_seconds)
        .bind(value)
        .bind(pending_since)
        .bind(observed_at)
        .bind(match_count)
        .bind(suppression.is_some())
        .bind(suppression.as_deref().unwrap_or_default())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO alert_event_channels(event_id,channel_id)
            SELECT $1,rc.channel_id FROM alert_rule_channels rc
            JOIN alert_webhook_channels c ON c.id=rc.channel_id
            WHERE rc.rule_id=$2 AND c.deleted_at IS NULL
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(&event_id)
        .bind(&rule.id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO alert_event_timeline(id,event_id,kind,actor,note,value,created_at) VALUES($1,$2,'firing','system','',$3,$4)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&event_id)
        .bind(value)
        .bind(observed_at)
        .execute(&mut *tx)
        .await?;
        let event = sqlx::query_as::<_, EventRow>("SELECT * FROM alert_events WHERE id=$1")
            .bind(&event_id)
            .fetch_one(&mut *tx)
            .await?;
        enqueue_event_lifecycle_tx(
            &mut tx,
            &event,
            "alert.firing",
            suppression.as_deref(),
            None,
            None,
            observed_at,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn observe_metric(
    state: &AppState,
    instance_id: &str,
    metrics: &MetricPayload,
    latency_ms: Option<f64>,
    received_at: i64,
    latency_sampled_at: Option<i64>,
) -> AppResult<()> {
    let observation = MetricObservation {
        received_at,
        cpu_percent: metrics.cpu_percent,
        memory_used: metrics.memory_used,
        memory_total: metrics.memory_total,
        disk_used: metrics.disk_used,
        disk_total: metrics.disk_total,
        latency_ms,
        latency_sampled_at,
    };
    let rules = sqlx::query_as::<_, RuleRow>(
        r#"
        SELECT DISTINCT r.* FROM alert_rules r
        LEFT JOIN alert_rule_targets t ON t.rule_id=r.id
        WHERE r.enabled=TRUE AND r.metric <> 'node_offline'
          AND (r.scope='all' OR t.instance_id=$1)
        ORDER BY r.id
        "#,
    )
    .bind(instance_id)
    .fetch_all(&state.db)
    .await?;
    for rule in rules {
        let Some((value, sample_at)) = metric_value(&rule.metric, &observation) else {
            continue;
        };
        let abnormal = rule.threshold.is_some_and(|threshold| value >= threshold);
        process_rule_observation(
            state,
            &rule,
            instance_id,
            RuleObservation::Threshold { value, abnormal },
            received_at,
            sample_at,
        )
        .await?;
    }
    Ok(())
}

async fn observe_offline_at(
    state: &AppState,
    instance_id: &str,
    observed_at: i64,
) -> AppResult<()> {
    let eligible: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM instances WHERE id=$1 AND approved=1 AND disabled=0)",
    )
    .bind(instance_id)
    .fetch_one(&state.db)
    .await?;
    if !eligible {
        sqlx::query("DELETE FROM alert_evaluation_states WHERE instance_id=$1")
            .bind(instance_id)
            .execute(&state.db)
            .await?;
        return Ok(());
    }
    let rules = sqlx::query_as::<_, RuleRow>(
        r#"
        SELECT DISTINCT r.* FROM alert_rules r
        LEFT JOIN alert_rule_targets t ON t.rule_id=r.id
        WHERE r.enabled=TRUE AND r.metric='node_offline'
          AND (r.scope='all' OR t.instance_id=$1)
        ORDER BY r.id
        "#,
    )
    .bind(instance_id)
    .fetch_all(&state.db)
    .await?;
    for rule in rules {
        process_rule_observation(
            state,
            &rule,
            instance_id,
            RuleObservation::Connection,
            observed_at,
            observed_at,
        )
        .await?;
    }
    Ok(())
}

pub async fn observe_connection(
    state: &AppState,
    instance_id: &str,
    _online: bool,
) -> AppResult<()> {
    let currently_online = state.agents.read().await.contains_key(instance_id);
    if !currently_online {
        reset_metric_pending_states(&state.db, Some(instance_id)).await?;
    }
    observe_offline_at(state, instance_id, now_ts()).await
}

pub async fn alert_evaluation_loop(state: AppState) {
    let started_at = now_ts();
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let now = now_ts();
        let ids = match sqlx::query_scalar::<_, String>(
            "SELECT id FROM instances WHERE approved=1 AND disabled=0 ORDER BY id",
        )
        .fetch_all(&state.db)
        .await
        {
            Ok(ids) => ids,
            Err(error) => {
                error!(?error, "failed to enumerate nodes for alert evaluation");
                continue;
            }
        };
        for instance_id in ids {
            let online = state.agents.read().await.contains_key(&instance_id);
            if startup_grace_defers_offline(online, started_at, now) {
                continue;
            }
            if let Err(error) = observe_offline_at(&state, &instance_id, now).await {
                warn!(?error, %instance_id, "failed to evaluate offline alert");
            }
        }
        if let Err(error) = refresh_active_suppression_states(&state, now).await {
            warn!(?error, "failed to refresh active alert suppression state");
        }
    }
}

async fn refresh_active_suppression_states(state: &AppState, now: i64) -> AppResult<()> {
    let active = sqlx::query_as::<_, (String, String, String)>(
        r#"
        SELECT id,rule_id,instance_id FROM alert_events
        WHERE status <> 'resolved' ORDER BY id
        "#,
    )
    .fetch_all(&state.db)
    .await?;
    for (event_id, rule_id, instance_id) in active {
        let mut tx = state.db.begin().await?;
        let rule = sqlx::query_as::<_, RuleRow>("SELECT * FROM alert_rules WHERE id=$1")
            .bind(&rule_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(rule) = rule else {
            tx.commit().await?;
            continue;
        };
        let suppression = suppression_reason_tx(&mut tx, &rule, &instance_id, now).await?;
        sqlx::query(
            r#"
            UPDATE alert_events SET suppressed=$1,suppression_reason=$2
            WHERE id=$3 AND status <> 'resolved'
            "#,
        )
        .bind(suppression.is_some())
        .bind(suppression.as_deref().unwrap_or_default())
        .bind(&event_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    Ok(())
}

pub async fn evaluation_loop(state: AppState) {
    alert_evaluation_loop(state).await;
}

#[derive(Serialize)]
struct SummaryResponse {
    firing: i64,
    acknowledged: i64,
    suppressed: i64,
    resolved_24h: i64,
}

async fn summary(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<SummaryResponse>> {
    require_admin(&state, &headers).await?;
    let (firing, acknowledged, suppressed, resolved_24h) =
        sqlx::query_as::<_, (i64, i64, i64, i64)>(
            r#"
            SELECT
                COUNT(*) FILTER(WHERE status='firing' AND suppressed=FALSE)::BIGINT,
                COUNT(*) FILTER(WHERE status='acknowledged')::BIGINT,
                COUNT(*) FILTER(WHERE status <> 'resolved' AND suppressed=TRUE)::BIGINT,
                COUNT(*) FILTER(WHERE status='resolved' AND resolved_at >= $1)::BIGINT
            FROM alert_events
            "#,
        )
        .bind(now_ts() - 24 * 3600)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(SummaryResponse {
        firing,
        acknowledged,
        suppressed,
        resolved_24h,
    }))
}

fn validate_event_query(query: &EventQuery) -> AppResult<()> {
    if query
        .status
        .as_deref()
        .is_some_and(|value| !matches!(value, "firing" | "acknowledged" | "resolved"))
    {
        return Err(AppError::bad_request("事件状态筛选值无效"));
    }
    if query
        .severity
        .as_deref()
        .is_some_and(|value| !matches!(value, "warning" | "critical"))
    {
        return Err(AppError::bad_request("严重级别筛选值无效"));
    }
    if query.from.zip(query.to).is_some_and(|(from, to)| from > to) {
        return Err(AppError::bad_request("事件时间范围无效"));
    }
    Ok(())
}

async fn list_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<EventQuery>,
) -> AppResult<Json<Page<EventRow>>> {
    require_admin(&state, &headers).await?;
    validate_event_query(&query)?;
    let (page, page_size, offset) = normalize_page(query.page, query.page_size);
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("%{value}%"));
    let filter = r#"
        FROM alert_events e
        WHERE ($1::TEXT IS NULL OR e.status=$1)
          AND ($2::TEXT IS NULL OR e.severity=$2)
          AND ($3::TEXT IS NULL OR e.metric=$3)
          AND ($4::TEXT IS NULL OR e.instance_id=$4)
          AND ($5::BOOLEAN IS NULL OR e.suppressed=$5)
          AND ($6::BIGINT IS NULL OR e.fired_at >= $6)
          AND ($7::BIGINT IS NULL OR e.fired_at <= $7)
          AND ($8::TEXT IS NULL OR e.id ILIKE $8 OR e.instance_id ILIKE $8
               OR e.rule_snapshot->>'name' ILIKE $8 OR e.node_snapshot->>'name' ILIKE $8
               OR e.node_snapshot->>'hostname' ILIKE $8)
    "#;
    let count_sql = format!("SELECT COUNT(*) {filter}");
    let total: i64 = sqlx::query_scalar(&count_sql)
        .bind(query.status.as_deref())
        .bind(query.severity.as_deref())
        .bind(query.metric.as_deref())
        .bind(query.instance_id.as_deref())
        .bind(query.suppressed)
        .bind(query.from)
        .bind(query.to)
        .bind(search.as_deref())
        .fetch_one(&state.db)
        .await?;
    let list_sql = format!(
        "SELECT e.* {filter} ORDER BY CASE WHEN e.status='resolved' THEN 1 ELSE 0 END, e.fired_at DESC, e.id LIMIT $9 OFFSET $10"
    );
    let items = sqlx::query_as::<_, EventRow>(&list_sql)
        .bind(query.status.as_deref())
        .bind(query.severity.as_deref())
        .bind(query.metric.as_deref())
        .bind(query.instance_id.as_deref())
        .bind(query.suppressed)
        .bind(query.from)
        .bind(query.to)
        .bind(search.as_deref())
        .bind(page_size)
        .bind(offset)
        .fetch_all(&state.db)
        .await?;
    Ok(Json(Page {
        items,
        page,
        page_size,
        total,
        pages: page_count(total, page_size),
    }))
}

#[derive(Serialize)]
struct EventDetail {
    #[serde(flatten)]
    event: EventRow,
    timeline: Vec<TimelineRow>,
    deliveries: Vec<DeliveryRow>,
}

async fn event_detail(db: &PgPool, id: &str) -> AppResult<EventDetail> {
    let event = sqlx::query_as::<_, EventRow>("SELECT * FROM alert_events WHERE id=$1")
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "告警事件不存在"))?;
    let timeline = sqlx::query_as::<_, TimelineRow>(
        "SELECT * FROM alert_event_timeline WHERE event_id=$1 ORDER BY created_at,id",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    let deliveries = sqlx::query_as::<_, DeliveryRow>(
        "SELECT * FROM alert_deliveries WHERE event_id=$1 ORDER BY created_at DESC,id",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    Ok(EventDetail {
        event,
        timeline,
        deliveries,
    })
}

async fn get_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<EventDetail>> {
    require_admin(&state, &headers).await?;
    Ok(Json(event_detail(&state.db, &id).await?))
}

async fn acknowledge_event_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    username: &str,
    user_id: &str,
    note: &str,
    now: i64,
) -> AppResult<()> {
    let mut event =
        sqlx::query_as::<_, EventRow>("SELECT * FROM alert_events WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "告警事件不存在"))?;
    if event.status == "resolved" {
        return Err(AppError::new(StatusCode::CONFLICT, "已恢复事件不能确认"));
    }
    if event.status != "firing" {
        return Ok(());
    }
    sqlx::query(
        r#"
        UPDATE alert_events SET status='acknowledged',acknowledged_by=$1,
            acknowledged_by_user_id=$2,acknowledged_at=$3,acknowledge_note=$4
        WHERE id=$5
        "#,
    )
    .bind(username)
    .bind(user_id)
    .bind(now)
    .bind(note)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO alert_event_timeline(id,event_id,kind,actor,note,value,created_at) VALUES($1,$2,'acknowledged',$3,$4,$5,$6)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(id)
    .bind(username)
    .bind(note)
    .bind(event.current_value)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    event.status = "acknowledged".to_string();
    event.acknowledged_by = Some(username.to_string());
    event.acknowledged_by_user_id = Some(user_id.to_string());
    event.acknowledged_at = Some(now);
    event.acknowledge_note = note.to_string();
    enqueue_event_lifecycle_tx(
        tx,
        &event,
        "alert.acknowledged",
        None,
        Some(username),
        Some(note),
        now,
    )
    .await
}

async fn acknowledge_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(payload): Json<AcknowledgeRequest>,
) -> AppResult<Json<EventDetail>> {
    let admin = require_admin(&state, &headers).await?;
    let note = payload.note.trim().to_string();
    if note.len() > MAX_REASON_BYTES {
        return Err(AppError::bad_request("确认备注过长"));
    }
    let now = now_ts();
    let mut tx = state.db.begin().await?;
    acknowledge_event_tx(&mut tx, &id, &admin.username, &admin.user_id, &note, now).await?;
    audit_action_tx(
        &mut tx,
        &admin,
        "alert_event_acknowledge",
        &id,
        "确认告警事件",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(event_detail(&state.db, &id).await?))
}

fn validate_maintenance(payload: &MaintenanceRequest) -> AppResult<MaintenanceRequest> {
    let mut normalized = payload.clone();
    normalized.name = trimmed(&payload.name, MAX_NAME_BYTES, "维护窗口名称")?;
    normalized.reason = payload.reason.trim().to_string();
    if normalized.reason.len() > MAX_REASON_BYTES {
        return Err(AppError::bad_request("维护原因过长"));
    }
    if normalized.ends_at <= normalized.starts_at {
        return Err(AppError::bad_request("维护结束时间必须晚于开始时间"));
    }
    if !matches!(normalized.scope.as_str(), "global" | "rule" | "node") {
        return Err(AppError::bad_request("维护范围必须是 global、rule 或 node"));
    }
    normalized.target_ids.sort();
    normalized.target_ids.dedup();
    if normalized.scope == "global" && !normalized.target_ids.is_empty() {
        return Err(AppError::bad_request("全局维护不能指定目标"));
    }
    if normalized.scope != "global" && normalized.target_ids.is_empty() {
        return Err(AppError::bad_request("规则或节点维护至少需要一个目标"));
    }
    Ok(normalized)
}

async fn validate_maintenance_targets(
    tx: &mut Transaction<'_, Postgres>,
    payload: &MaintenanceRequest,
) -> AppResult<()> {
    for target in &payload.target_ids {
        let exists: bool = match payload.scope.as_str() {
            "rule" => {
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM alert_rules WHERE id=$1)")
                    .bind(target)
                    .fetch_one(&mut **tx)
                    .await?
            }
            "node" => {
                sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM instances WHERE id=$1 AND approved=1)",
                )
                .bind(target)
                .fetch_one(&mut **tx)
                .await?
            }
            _ => true,
        };
        if !exists {
            return Err(AppError::bad_request(format!("维护目标 {target} 不存在")));
        }
    }
    Ok(())
}

async fn replace_maintenance_targets(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    targets: &[String],
) -> AppResult<()> {
    sqlx::query("DELETE FROM alert_maintenance_targets WHERE window_id=$1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    for target in targets {
        sqlx::query("INSERT INTO alert_maintenance_targets(window_id,target_id) VALUES($1,$2)")
            .bind(id)
            .bind(target)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn maintenance_response(
    db: &PgPool,
    window: MaintenanceRow,
) -> AppResult<MaintenanceResponse> {
    let targets = sqlx::query_scalar::<_, String>(
        "SELECT target_id FROM alert_maintenance_targets WHERE window_id=$1 ORDER BY target_id",
    )
    .bind(&window.id)
    .fetch_all(db)
    .await?;
    Ok(MaintenanceResponse {
        window,
        target_ids: targets,
    })
}

async fn list_maintenance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<MaintenanceResponse>>> {
    require_admin(&state, &headers).await?;
    let (page, page_size, offset) = normalize_page(query.page, query.page_size);
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM alert_maintenance_windows")
        .fetch_one(&state.db)
        .await?;
    let rows = sqlx::query_as::<_, MaintenanceRow>(
        "SELECT * FROM alert_maintenance_windows ORDER BY starts_at DESC,id LIMIT $1 OFFSET $2",
    )
    .bind(page_size)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        items.push(maintenance_response(&state.db, row).await?);
    }
    Ok(Json(Page {
        items,
        page,
        page_size,
        total,
        pages: page_count(total, page_size),
    }))
}

async fn create_maintenance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<MaintenanceRequest>,
) -> AppResult<(StatusCode, Json<MaintenanceResponse>)> {
    let admin = require_admin(&state, &headers).await?;
    let payload = validate_maintenance(&payload)?;
    let id = Uuid::new_v4().to_string();
    let now = now_ts();
    let mut tx = state.db.begin().await?;
    validate_maintenance_targets(&mut tx, &payload).await?;
    sqlx::query(
        r#"
        INSERT INTO alert_maintenance_windows(
            id,name,reason,scope,starts_at,ends_at,enabled,created_by,
            created_by_user_id,created_at,updated_at
        ) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$10)
        "#,
    )
    .bind(&id)
    .bind(&payload.name)
    .bind(&payload.reason)
    .bind(&payload.scope)
    .bind(payload.starts_at)
    .bind(payload.ends_at)
    .bind(payload.enabled)
    .bind(&admin.username)
    .bind(&admin.user_id)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    replace_maintenance_targets(&mut tx, &id, &payload.target_ids).await?;
    audit_action_tx(
        &mut tx,
        &admin,
        "alert_maintenance_create",
        &id,
        "创建维护窗口",
    )
    .await?;
    tx.commit().await?;
    let row =
        sqlx::query_as::<_, MaintenanceRow>("SELECT * FROM alert_maintenance_windows WHERE id=$1")
            .bind(&id)
            .fetch_one(&state.db)
            .await?;
    Ok((
        StatusCode::CREATED,
        Json(maintenance_response(&state.db, row).await?),
    ))
}

async fn update_maintenance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(payload): Json<MaintenanceRequest>,
) -> AppResult<Json<MaintenanceResponse>> {
    let admin = require_admin(&state, &headers).await?;
    let payload = validate_maintenance(&payload)?;
    let now = now_ts();
    let mut tx = state.db.begin().await?;
    validate_maintenance_targets(&mut tx, &payload).await?;
    let result = sqlx::query(
        r#"
        UPDATE alert_maintenance_windows SET name=$1,reason=$2,scope=$3,
            starts_at=$4,ends_at=$5,enabled=$6,updated_at=$7 WHERE id=$8
        "#,
    )
    .bind(&payload.name)
    .bind(&payload.reason)
    .bind(&payload.scope)
    .bind(payload.starts_at)
    .bind(payload.ends_at)
    .bind(payload.enabled)
    .bind(now)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::new(StatusCode::NOT_FOUND, "维护窗口不存在"));
    }
    replace_maintenance_targets(&mut tx, &id, &payload.target_ids).await?;
    audit_action_tx(
        &mut tx,
        &admin,
        "alert_maintenance_update",
        &id,
        "更新维护窗口",
    )
    .await?;
    tx.commit().await?;
    let row =
        sqlx::query_as::<_, MaintenanceRow>("SELECT * FROM alert_maintenance_windows WHERE id=$1")
            .bind(&id)
            .fetch_one(&state.db)
            .await?;
    Ok(Json(maintenance_response(&state.db, row).await?))
}

async fn delete_maintenance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let admin = require_admin(&state, &headers).await?;
    let mut tx = state.db.begin().await?;
    let result = sqlx::query("DELETE FROM alert_maintenance_windows WHERE id=$1")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::new(StatusCode::NOT_FOUND, "维护窗口不存在"));
    }
    audit_action_tx(
        &mut tx,
        &admin,
        "alert_maintenance_delete",
        &id,
        "删除维护窗口",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn validate_webhook_url(value: &str) -> AppResult<String> {
    let value = value.trim();
    if value.len() > 4_096 {
        return Err(AppError::bad_request("Webhook URL 过长"));
    }
    let url = Url::parse(value).map_err(|_| AppError::bad_request("Webhook URL 格式无效"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(AppError::bad_request("Webhook URL 必须使用 HTTP 或 HTTPS"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::bad_request("Webhook URL 不能包含用户凭据"));
    }
    Ok(url.to_string())
}

fn validate_webhook_headers(
    headers: &BTreeMap<String, String>,
) -> AppResult<BTreeMap<String, String>> {
    if headers.len() > MAX_HEADER_COUNT {
        return Err(AppError::bad_request("Webhook 自定义请求头数量过多"));
    }
    let mut normalized = BTreeMap::new();
    for (name, value) in headers {
        let name = name.trim();
        if name.is_empty() || name.len() > MAX_HEADER_NAME_BYTES {
            return Err(AppError::bad_request("Webhook 请求头名称无效"));
        }
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "host"
                | "content-length"
                | "connection"
                | "proxy-connection"
                | "keep-alive"
                | "transfer-encoding"
                | "te"
                | "trailer"
                | "upgrade"
                | "content-type"
        ) || lower.starts_with("x-om-")
        {
            return Err(AppError::bad_request(format!(
                "Webhook 请求头 {name} 不允许覆盖"
            )));
        }
        HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| AppError::bad_request("Webhook 请求头名称无效"))?;
        if value.len() > MAX_HEADER_VALUE_BYTES {
            return Err(AppError::bad_request("Webhook 请求头值过长"));
        }
        HeaderValue::from_str(value).map_err(|_| AppError::bad_request("Webhook 请求头值无效"))?;
        normalized.insert(lower, value.to_string());
    }
    Ok(normalized)
}

fn mask_webhook_url(value: &str) -> String {
    let Ok(url) = Url::parse(value) else {
        return "***".to_string();
    };
    let Some(host) = url.host_str() else {
        return "***".to_string();
    };
    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    let path_hint = if url.path() == "/" || url.path().is_empty() {
        ""
    } else {
        "/..."
    };
    format!("{}://{host}{port}{path_hint}", url.scheme())
}

fn decrypt_string(state: &AppState, ciphertext: &str) -> AppResult<String> {
    let bytes = state.auth_cipher.decrypt(ciphertext)?;
    String::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("encrypted alert configuration is not UTF-8").into())
}

fn decrypt_headers(state: &AppState, ciphertext: &str) -> AppResult<BTreeMap<String, String>> {
    let plaintext = decrypt_string(state, ciphertext)?;
    Ok(serde_json::from_str(&plaintext)
        .map_err(|error| anyhow::anyhow!("invalid encrypted webhook headers: {error}"))?)
}

fn channel_response(state: &AppState, row: ChannelRow) -> AppResult<ChannelResponse> {
    let url = decrypt_string(state, &row.url_ciphertext)?;
    let headers = decrypt_headers(state, &row.headers_ciphertext)?;
    Ok(ChannelResponse {
        id: row.id,
        name: row.name,
        masked_url: mask_webhook_url(&url),
        header_names: headers.into_keys().collect(),
        has_secret: row.secret_ciphertext.is_some(),
        enabled: row.enabled,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

async fn load_channel(db: &PgPool, id: &str) -> AppResult<ChannelRow> {
    sqlx::query_as::<_, ChannelRow>(
        "SELECT * FROM alert_webhook_channels WHERE id=$1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Webhook 渠道不存在"))
}

async fn list_channels(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<ChannelResponse>>> {
    require_admin(&state, &headers).await?;
    let (page, page_size, offset) = normalize_page(query.page, query.page_size);
    let total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM alert_webhook_channels WHERE deleted_at IS NULL")
            .fetch_one(&state.db)
            .await?;
    let rows = sqlx::query_as::<_, ChannelRow>(
        r#"
        SELECT * FROM alert_webhook_channels WHERE deleted_at IS NULL
        ORDER BY updated_at DESC,id LIMIT $1 OFFSET $2
        "#,
    )
    .bind(page_size)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let items = rows
        .into_iter()
        .map(|row| channel_response(&state, row))
        .collect::<AppResult<Vec<_>>>()?;
    Ok(Json(Page {
        items,
        page,
        page_size,
        total,
        pages: page_count(total, page_size),
    }))
}

async fn create_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ChannelRequest>,
) -> AppResult<(StatusCode, Json<ChannelResponse>)> {
    let admin = require_admin(&state, &headers).await?;
    let name = trimmed(&payload.name, MAX_NAME_BYTES, "Webhook 名称")?;
    let url = validate_webhook_url(
        payload
            .url
            .as_deref()
            .ok_or_else(|| AppError::bad_request("创建 Webhook 时必须提供 URL"))?,
    )?;
    let custom_headers = validate_webhook_headers(&payload.headers.unwrap_or_default())?;
    let secret = payload
        .secret
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if secret.is_some_and(|value| value.len() > MAX_HEADER_VALUE_BYTES) {
        return Err(AppError::bad_request("Webhook HMAC 密钥过长"));
    }
    let id = Uuid::new_v4().to_string();
    let now = now_ts();
    let mut tx = state.db.begin().await?;
    sqlx::query(
        r#"
        INSERT INTO alert_webhook_channels(
            id,name,url_ciphertext,secret_ciphertext,headers_ciphertext,
            enabled,created_at,updated_at
        ) VALUES($1,$2,$3,$4,$5,$6,$7,$7)
        "#,
    )
    .bind(&id)
    .bind(name)
    .bind(state.auth_cipher.encrypt(url.as_bytes())?)
    .bind(
        secret
            .map(|value| state.auth_cipher.encrypt(value.as_bytes()))
            .transpose()?,
    )
    .bind(
        state.auth_cipher.encrypt(
            serde_json::to_string(&custom_headers)
                .map_err(anyhow::Error::from)?
                .as_bytes(),
        )?,
    )
    .bind(payload.enabled)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    audit_action_tx(
        &mut tx,
        &admin,
        "alert_webhook_create",
        &id,
        "创建 Webhook 渠道",
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(channel_response(
            &state,
            load_channel(&state.db, &id).await?,
        )?),
    ))
}

async fn update_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(payload): Json<ChannelRequest>,
) -> AppResult<Json<ChannelResponse>> {
    let admin = require_admin(&state, &headers).await?;
    let old = load_channel(&state.db, &id).await?;
    let name = trimmed(&payload.name, MAX_NAME_BYTES, "Webhook 名称")?;
    let url_ciphertext = if let Some(url) = payload.url.as_deref() {
        state
            .auth_cipher
            .encrypt(validate_webhook_url(url)?.as_bytes())?
    } else {
        old.url_ciphertext
    };
    let headers_ciphertext = if let Some(headers) = payload.headers.as_ref() {
        let headers = validate_webhook_headers(headers)?;
        state.auth_cipher.encrypt(
            serde_json::to_string(&headers)
                .map_err(anyhow::Error::from)?
                .as_bytes(),
        )?
    } else {
        old.headers_ciphertext
    };
    let secret_ciphertext = if payload.clear_secret {
        None
    } else if let Some(secret) = payload.secret.as_deref() {
        let secret = secret.trim();
        if secret.len() > MAX_HEADER_VALUE_BYTES {
            return Err(AppError::bad_request("Webhook HMAC 密钥过长"));
        }
        if secret.is_empty() {
            old.secret_ciphertext
        } else {
            Some(state.auth_cipher.encrypt(secret.as_bytes())?)
        }
    } else {
        old.secret_ciphertext
    };
    let mut tx = state.db.begin().await?;
    let result = sqlx::query(
        r#"
        UPDATE alert_webhook_channels SET name=$1,url_ciphertext=$2,
            secret_ciphertext=$3,headers_ciphertext=$4,enabled=$5,updated_at=$6
        WHERE id=$7 AND deleted_at IS NULL
        "#,
    )
    .bind(name)
    .bind(url_ciphertext)
    .bind(secret_ciphertext)
    .bind(headers_ciphertext)
    .bind(payload.enabled)
    .bind(now_ts())
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::new(StatusCode::NOT_FOUND, "Webhook 渠道不存在"));
    }
    audit_action_tx(
        &mut tx,
        &admin,
        "alert_webhook_update",
        &id,
        "更新 Webhook 渠道",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(channel_response(
        &state,
        load_channel(&state.db, &id).await?,
    )?))
}

async fn delete_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let admin = require_admin(&state, &headers).await?;
    let now = now_ts();
    let mut tx = state.db.begin().await?;
    let result = sqlx::query(
        r#"
        UPDATE alert_webhook_channels SET enabled=FALSE,deleted_at=$1,updated_at=$1
        WHERE id=$2 AND deleted_at IS NULL
        "#,
    )
    .bind(now)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::new(StatusCode::NOT_FOUND, "Webhook 渠道不存在"));
    }
    sqlx::query("DELETE FROM alert_rule_channels WHERE channel_id=$1")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        r#"
        UPDATE alert_deliveries SET status='failed',last_error='channel_deleted',
            next_attempt_at=NULL,lease_until=NULL,lease_token=NULL,
            completed_at=$1,updated_at=$1
        WHERE channel_id=$2 AND status IN ('pending','processing')
        "#,
    )
    .bind(now)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    audit_action_tx(
        &mut tx,
        &admin,
        "alert_webhook_delete",
        &id,
        "删除 Webhook 渠道",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn test_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<DeliveryRow>)> {
    let admin = require_admin(&state, &headers).await?;
    let channel = load_channel(&state.db, &id).await?;
    if !channel.enabled {
        return Err(AppError::new(StatusCode::CONFLICT, "Webhook 渠道已停用"));
    }
    let delivery_id = Uuid::new_v4().to_string();
    let now = now_ts();
    let mut tx = state.db.begin().await?;
    sqlx::query(
        r#"
        INSERT INTO alert_deliveries(
            id,event_id,channel_id,kind,status,payload,channel_snapshot,
            next_attempt_at,created_at,updated_at
        ) VALUES($1,NULL,$2,'webhook.test','pending',$3,$4,$5,$5,$5)
        "#,
    )
    .bind(&delivery_id)
    .bind(&id)
    .bind(json!({
        "version": 1,
        "type": "webhook.test",
        "created_at": now,
        "actor": {"username": admin.username, "user_id": admin.user_id},
    }))
    .bind(json!({"id": id, "name": channel.name}))
    .bind(now)
    .execute(&mut *tx)
    .await?;
    audit_action_tx(
        &mut tx,
        &admin,
        "alert_webhook_test",
        &id,
        "测试 Webhook 渠道",
    )
    .await?;
    tx.commit().await?;
    let delivery = sqlx::query_as::<_, DeliveryRow>("SELECT * FROM alert_deliveries WHERE id=$1")
        .bind(&delivery_id)
        .fetch_one(&state.db)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(delivery)))
}

async fn list_deliveries(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DeliveryQuery>,
) -> AppResult<Json<Page<DeliveryRow>>> {
    require_admin(&state, &headers).await?;
    if query.status.as_deref().is_some_and(|value| {
        !matches!(
            value,
            "pending" | "processing" | "succeeded" | "failed" | "suppressed"
        )
    }) {
        return Err(AppError::bad_request("投递状态筛选值无效"));
    }
    let (page, page_size, offset) = normalize_page(query.page, query.page_size);
    let filter = r#"
        FROM alert_deliveries d
        WHERE ($1::TEXT IS NULL OR d.status=$1)
          AND ($2::TEXT IS NULL OR d.kind=$2)
          AND ($3::TEXT IS NULL OR d.channel_id=$3)
          AND ($4::TEXT IS NULL OR d.event_id=$4)
    "#;
    let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) {filter}"))
        .bind(query.status.as_deref())
        .bind(query.kind.as_deref())
        .bind(query.channel_id.as_deref())
        .bind(query.event_id.as_deref())
        .fetch_one(&state.db)
        .await?;
    let items = sqlx::query_as::<_, DeliveryRow>(&format!(
        "SELECT d.* {filter} ORDER BY d.created_at DESC,d.id LIMIT $5 OFFSET $6"
    ))
    .bind(query.status.as_deref())
    .bind(query.kind.as_deref())
    .bind(query.channel_id.as_deref())
    .bind(query.event_id.as_deref())
    .bind(page_size)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page {
        items,
        page,
        page_size,
        total,
        pages: page_count(total, page_size),
    }))
}

#[derive(Serialize)]
struct DeliveryDetail {
    #[serde(flatten)]
    delivery: DeliveryRow,
    attempts: Vec<DeliveryAttemptRow>,
}

async fn delivery_detail(db: &PgPool, id: &str) -> AppResult<DeliveryDetail> {
    let delivery = sqlx::query_as::<_, DeliveryRow>("SELECT * FROM alert_deliveries WHERE id=$1")
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Webhook 投递不存在"))?;
    let attempts = sqlx::query_as::<_, DeliveryAttemptRow>(
        "SELECT * FROM alert_delivery_attempts WHERE delivery_id=$1 ORDER BY created_at,id",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    Ok(DeliveryDetail { delivery, attempts })
}

async fn get_delivery(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<DeliveryDetail>> {
    require_admin(&state, &headers).await?;
    Ok(Json(delivery_detail(&state.db, &id).await?))
}

async fn retry_delivery(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<DeliveryDetail>> {
    let admin = require_admin(&state, &headers).await?;
    let now = now_ts();
    let mut tx = state.db.begin().await?;
    reset_failed_delivery_tx(&mut tx, &id, now).await?;
    audit_action_tx(
        &mut tx,
        &admin,
        "alert_delivery_retry",
        &id,
        "重试 Webhook 投递",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(delivery_detail(&state.db, &id).await?))
}

async fn reset_failed_delivery_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    now: i64,
) -> AppResult<()> {
    let delivery = sqlx::query_as::<_, (String, String)>(
        "SELECT status,channel_id FROM alert_deliveries WHERE id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Webhook 投递不存在"))?;
    if delivery.0 != "failed" {
        return Err(AppError::new(StatusCode::CONFLICT, "只有失败投递可以重试"));
    }
    let channel = sqlx::query_as::<_, (bool, Option<i64>)>(
        "SELECT enabled,deleted_at FROM alert_webhook_channels WHERE id=$1 FOR SHARE",
    )
    .bind(&delivery.1)
    .fetch_optional(&mut **tx)
    .await?;
    match channel {
        None | Some((_, Some(_))) => {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "Webhook 渠道已删除，无法重试",
            ));
        }
        Some((false, None)) => {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "Webhook 渠道已停用，无法重试",
            ));
        }
        Some((true, None)) => {}
    }
    let result = sqlx::query(
        r#"
        UPDATE alert_deliveries SET status='pending',cycle_attempts=0,
            manual_retry_count=manual_retry_count+1,next_attempt_at=$1,
            lease_until=NULL,lease_token=NULL,last_error='',completed_at=NULL,updated_at=$1
        WHERE id=$2 AND status='failed'
        "#,
    )
    .bind(now)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "投递状态已发生变化，请刷新后重试",
        ));
    }
    Ok(())
}

fn webhook_signature(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let mut encoded = String::with_capacity(digest.len() * 2 + 7);
    encoded.push_str("sha256=");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

async fn claim_delivery(db: &PgPool, now: i64) -> AppResult<Option<DeliveryRow>> {
    let lease_token = Uuid::new_v4().to_string();
    let row = sqlx::query_as::<_, DeliveryRow>(
        r#"
        WITH candidate AS (
            SELECT id FROM alert_deliveries
            WHERE (
                status='pending' AND COALESCE(next_attempt_at,0) <= $1
            ) OR (
                status='processing' AND COALESCE(lease_until,0) <= $1
            )
            ORDER BY COALESCE(next_attempt_at,created_at),created_at,id
            FOR UPDATE SKIP LOCKED
            LIMIT 1
        )
        UPDATE alert_deliveries d SET status='processing',lease_until=$2,
            lease_token=$3,updated_at=$1
        FROM candidate c WHERE d.id=c.id
        RETURNING d.*
        "#,
    )
    .bind(now)
    .bind(now + DELIVERY_LEASE_SECONDS)
    .bind(lease_token)
    .fetch_optional(db)
    .await?;
    Ok(row)
}

async fn start_delivery_attempt(
    db: &PgPool,
    delivery: &DeliveryRow,
    now: i64,
) -> AppResult<Option<DeliveryRow>> {
    let Some(lease_token) = delivery.lease_token.as_deref() else {
        return Ok(None);
    };
    Ok(sqlx::query_as::<_, DeliveryRow>(
        r#"
        UPDATE alert_deliveries SET attempts_count=attempts_count+1,
            cycle_attempts=cycle_attempts+1,updated_at=$1
        WHERE id=$2 AND status='processing' AND lease_until > $1 AND lease_token=$3
        RETURNING *
        "#,
    )
    .bind(now)
    .bind(&delivery.id)
    .bind(lease_token)
    .fetch_optional(db)
    .await?)
}

async fn current_delivery_suppression(
    db: &PgPool,
    delivery: &DeliveryRow,
    now: i64,
) -> AppResult<Option<String>> {
    if delivery.kind != "alert.firing" {
        return Ok(None);
    }
    let Some(event_id) = delivery.event_id.as_deref() else {
        return Ok(None);
    };
    let Some((rule_id, instance_id, metric)) = sqlx::query_as::<_, (String, String, String)>(
        "SELECT rule_id,instance_id,metric FROM alert_events WHERE id=$1",
    )
    .bind(event_id)
    .fetch_optional(db)
    .await?
    else {
        return Ok(None);
    };
    let maintenance = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT w.name,w.reason FROM alert_maintenance_windows w
        WHERE w.enabled=TRUE AND w.starts_at <= $1 AND w.ends_at > $1
          AND (
            w.scope='global' OR
            (w.scope='rule' AND EXISTS(
                SELECT 1 FROM alert_maintenance_targets t
                WHERE t.window_id=w.id AND t.target_id=$2
            )) OR
            (w.scope='node' AND EXISTS(
                SELECT 1 FROM alert_maintenance_targets t
                WHERE t.window_id=w.id AND t.target_id=$3
            ))
          )
        ORDER BY w.starts_at,w.id LIMIT 1
        "#,
    )
    .bind(now)
    .bind(&rule_id)
    .bind(&instance_id)
    .fetch_optional(db)
    .await?;
    if let Some((name, reason)) = maintenance {
        return Ok(Some(if reason.is_empty() {
            format!("maintenance:{name}")
        } else {
            format!("maintenance:{name}: {reason}")
        }));
    }
    if node_offline_suppresses(&metric, true) {
        let offline_id = sqlx::query_scalar::<_, String>(
            r#"
            SELECT id FROM alert_events
            WHERE instance_id=$1 AND metric='node_offline'
              AND status IN ('firing','acknowledged')
            ORDER BY fired_at,id LIMIT 1
            "#,
        )
        .bind(&instance_id)
        .fetch_optional(db)
        .await?;
        if let Some(offline_id) = offline_id {
            return Ok(Some(format!("node_offline:{offline_id}")));
        }
    }
    Ok(None)
}

async fn mark_delivery_suppressed(
    db: &PgPool,
    delivery: &DeliveryRow,
    reason: &str,
    now: i64,
) -> AppResult<()> {
    let Some(lease_token) = delivery.lease_token.as_deref() else {
        return Ok(());
    };
    let mut tx = db.begin().await?;
    let updated = sqlx::query(
        r#"
        UPDATE alert_deliveries SET status='suppressed',suppression_reason=$1,
            next_attempt_at=NULL,lease_until=NULL,lease_token=NULL,
            completed_at=$2,updated_at=$2
        WHERE id=$3 AND status='processing' AND lease_token=$4
        "#,
    )
    .bind(reason)
    .bind(now)
    .bind(&delivery.id)
    .bind(lease_token)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        tx.commit().await?;
        return Ok(());
    }
    if let Some(event_id) = delivery.event_id.as_deref() {
        sqlx::query(
            r#"
            UPDATE alert_events SET suppressed=TRUE,suppression_reason=$1
            WHERE id=$2 AND status <> 'resolved'
            "#,
        )
        .bind(reason)
        .bind(event_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

struct WebhookAttemptOutcome {
    succeeded: bool,
    http_status: Option<i64>,
    duration_ms: i64,
    error: String,
    response_excerpt: String,
}

async fn send_webhook(
    client: &Client,
    state: &AppState,
    delivery: &DeliveryRow,
) -> WebhookAttemptOutcome {
    let started = Instant::now();
    let outcome = async {
        let channel = load_channel(&state.db, &delivery.channel_id).await?;
        if !channel.enabled {
            return Err(AppError::new(StatusCode::CONFLICT, "Webhook 渠道已停用"));
        }
        let url = decrypt_string(state, &channel.url_ciphertext)?;
        let url = validate_webhook_url(&url)?;
        let headers = decrypt_headers(state, &channel.headers_ciphertext)?;
        let body = serde_json::to_vec(&delivery.payload)
            .map_err(|error| anyhow::anyhow!("failed to serialize webhook payload: {error}"))?;
        let timestamp = now_ts();
        let mut request = client
            .post(url)
            .header("content-type", "application/json")
            .header("x-om-timestamp", timestamp.to_string())
            .header("x-om-delivery-id", &delivery.id)
            .body(body.clone());
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if let Some(ciphertext) = channel.secret_ciphertext.as_deref() {
            let secret = state.auth_cipher.decrypt(ciphertext)?;
            request = request.header(
                "x-om-signature",
                webhook_signature(&secret, timestamp, &body),
            );
        }
        let mut response = request.send().await.map_err(|error| {
            AppError::new(
                StatusCode::BAD_GATEWAY,
                format!("webhook request failed: {}", error.without_url()),
            )
        })?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            AppError::new(
                StatusCode::BAD_GATEWAY,
                format!("failed to read webhook response: {}", error.without_url()),
            )
        })? {
            let remaining = MAX_RESPONSE_BYTES.saturating_sub(bytes.len());
            if remaining == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            if bytes.len() >= MAX_RESPONSE_BYTES {
                break;
            }
        }
        let response_excerpt =
            crate::audit::truncate(&String::from_utf8_lossy(&bytes), MAX_RESPONSE_BYTES);
        Ok::<_, AppError>((status, response_excerpt))
    }
    .await;
    let duration_ms = started.elapsed().as_millis().min(i64::MAX as u128) as i64;
    match outcome {
        Ok((status, response_excerpt)) => WebhookAttemptOutcome {
            succeeded: status.is_success(),
            http_status: Some(i64::from(status.as_u16())),
            duration_ms,
            error: if status.is_success() {
                String::new()
            } else {
                format!("http_status_{}", status.as_u16())
            },
            response_excerpt,
        },
        Err(error) => WebhookAttemptOutcome {
            succeeded: false,
            http_status: None,
            duration_ms,
            error: crate::audit::truncate(&error.message, 4 * 1024),
            response_excerpt: String::new(),
        },
    }
}

async fn finish_delivery_attempt(
    db: &PgPool,
    delivery: &DeliveryRow,
    outcome: WebhookAttemptOutcome,
) -> AppResult<()> {
    let Some(lease_token) = delivery.lease_token.as_deref() else {
        return Ok(());
    };
    let now = now_ts();
    let mut tx = db.begin().await?;
    let current_lease = sqlx::query_as::<_, (Option<String>,)>(
        r#"
        SELECT lease_token FROM alert_deliveries
        WHERE id=$1 AND status='processing' FOR UPDATE
        "#,
    )
    .bind(&delivery.id)
    .fetch_optional(&mut *tx)
    .await?;
    if current_lease.as_ref().and_then(|(token,)| token.as_deref()) != Some(lease_token) {
        tx.commit().await?;
        return Ok(());
    }
    sqlx::query(
        r#"
        INSERT INTO alert_delivery_attempts(
            id,delivery_id,attempt_number,http_status,duration_ms,error,
            response_excerpt,created_at
        ) VALUES($1,$2,$3,$4,$5,$6,$7,$8)
        "#,
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&delivery.id)
    .bind(delivery.attempts_count)
    .bind(outcome.http_status)
    .bind(outcome.duration_ms)
    .bind(&outcome.error)
    .bind(&outcome.response_excerpt)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    if outcome.succeeded {
        sqlx::query(
            r#"
            UPDATE alert_deliveries SET status='succeeded',next_attempt_at=NULL,
                lease_until=NULL,lease_token=NULL,last_error='',completed_at=$1,updated_at=$1
            WHERE id=$2 AND status='processing' AND lease_token=$3
            "#,
        )
        .bind(now)
        .bind(&delivery.id)
        .bind(lease_token)
        .execute(&mut *tx)
        .await?;
    } else if let Some(delay) = retry_delay_after(delivery.cycle_attempts) {
        sqlx::query(
            r#"
            UPDATE alert_deliveries SET status='pending',next_attempt_at=$1,
                lease_until=NULL,lease_token=NULL,last_error=$2,updated_at=$3
            WHERE id=$4 AND status='processing' AND lease_token=$5
            "#,
        )
        .bind(now + delay)
        .bind(&outcome.error)
        .bind(now)
        .bind(&delivery.id)
        .bind(lease_token)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            r#"
            UPDATE alert_deliveries SET status='failed',next_attempt_at=NULL,
                lease_until=NULL,lease_token=NULL,last_error=$1,completed_at=$2,updated_at=$2
            WHERE id=$3 AND status='processing' AND lease_token=$4
            "#,
        )
        .bind(&outcome.error)
        .bind(now)
        .bind(&delivery.id)
        .bind(lease_token)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn retry_delay_after(cycle_attempts: i64) -> Option<i64> {
    if !(1..5).contains(&cycle_attempts) {
        return None;
    }
    Some(DELIVERY_RETRY_DELAYS[(cycle_attempts - 1) as usize])
}

pub async fn webhook_delivery_loop(state: AppState) {
    let client = match Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(WEBHOOK_TIMEOUT_SECONDS))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            error!(?error, "failed to initialize alert webhook client");
            return;
        }
    };
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        for _ in 0..16 {
            let delivery = match claim_delivery(&state.db, now_ts()).await {
                Ok(Some(delivery)) => delivery,
                Ok(None) => break,
                Err(error) => {
                    warn!(?error, "failed to claim alert webhook delivery");
                    break;
                }
            };
            match current_delivery_suppression(&state.db, &delivery, now_ts()).await {
                Ok(Some(reason)) => {
                    if let Err(error) =
                        mark_delivery_suppressed(&state.db, &delivery, &reason, now_ts()).await
                    {
                        warn!(?error, delivery_id=%delivery.id, "failed to suppress webhook delivery");
                    }
                    continue;
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(?error, delivery_id=%delivery.id, "failed to recheck webhook suppression");
                    let Some(delivery) = (match start_delivery_attempt(
                        &state.db,
                        &delivery,
                        now_ts(),
                    )
                    .await
                    {
                        Ok(delivery) => delivery,
                        Err(error) => {
                            warn!(?error, delivery_id=%delivery.id, "failed to start webhook attempt");
                            continue;
                        }
                    }) else {
                        continue;
                    };
                    let outcome = WebhookAttemptOutcome {
                        succeeded: false,
                        http_status: None,
                        duration_ms: 0,
                        error: "suppression_check_failed".to_string(),
                        response_excerpt: String::new(),
                    };
                    if let Err(error) = finish_delivery_attempt(&state.db, &delivery, outcome).await
                    {
                        warn!(?error, delivery_id=%delivery.id, "failed to record webhook attempt");
                    }
                    continue;
                }
            }
            let Some(delivery) = (match start_delivery_attempt(&state.db, &delivery, now_ts()).await
            {
                Ok(delivery) => delivery,
                Err(error) => {
                    warn!(?error, delivery_id=%delivery.id, "failed to start webhook attempt");
                    continue;
                }
            }) else {
                continue;
            };
            let outcome = send_webhook(&client, &state, &delivery).await;
            if let Err(error) = finish_delivery_attempt(&state.db, &delivery, outcome).await {
                warn!(?error, delivery_id=%delivery.id, "failed to record webhook attempt");
            }
        }
    }
}

pub async fn delivery_loop(state: AppState) {
    webhook_delivery_loop(state).await;
}

pub async fn retention_days(db: &PgPool) -> AppResult<i64> {
    let value = sqlx::query_scalar::<_, String>(
        "SELECT value FROM settings WHERE key='alert_retention_days'",
    )
    .fetch_optional(db)
    .await?;
    Ok(value
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(DEFAULT_ALERT_RETENTION_DAYS)
        .clamp(1, MAX_ALERT_RETENTION_DAYS))
}

pub async fn cleanup_old_alerts(db: &PgPool, cutoff: i64) -> AppResult<()> {
    let mut tx = db.begin().await?;
    sqlx::query(
        r#"
        DELETE FROM alert_deliveries
        WHERE event_id IS NULL AND created_at < $1
          AND status IN ('succeeded','failed','suppressed')
        "#,
    )
    .bind(cutoff)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM alert_events WHERE status='resolved' AND resolved_at < $1")
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM alert_maintenance_windows WHERE ends_at < $1")
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use axum::{body::Bytes, response::IntoResponse};
    use reqwest::redirect::Policy;
    use sqlx::postgres::PgPoolOptions;
    use tokio::sync::Mutex;

    use super::*;
    use crate::{auth::AuthCipher, config::Cli};

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
            .expect("create isolated alert schema");

        let connection_schema = schema.clone();
        let db = PgPoolOptions::new()
            .max_connections(8)
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
            .expect("connect isolated alert schema");
        (db, bootstrap, schema)
    }

    async fn drop_test_schema(db: PgPool, bootstrap: PgPool, schema: String) {
        db.close().await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&bootstrap)
            .await
            .expect("drop isolated alert schema");
        bootstrap.close().await;
    }

    async fn create_alert_prerequisites(db: &PgPool) {
        for statement in [
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            "CREATE TABLE instances (id TEXT PRIMARY KEY, name TEXT NOT NULL DEFAULT '', hostname TEXT NOT NULL DEFAULT '', region TEXT NOT NULL DEFAULT '', os TEXT NOT NULL DEFAULT '', arch TEXT NOT NULL DEFAULT '', agent_version TEXT NOT NULL DEFAULT '', approved BIGINT NOT NULL DEFAULT 1, disabled BIGINT NOT NULL DEFAULT 0)",
            "CREATE TABLE metrics (id BIGSERIAL PRIMARY KEY, instance_id TEXT NOT NULL, ts BIGINT NOT NULL, latency_ms DOUBLE PRECISION)",
        ] {
            sqlx::query(statement)
                .execute(db)
                .await
                .expect("create alert prerequisite");
        }
    }

    fn test_state(db: PgPool) -> AppState {
        AppState::new(
            db,
            Cli {
                bind: "127.0.0.1:0".parse().expect("test bind address"),
                database_url: "postgresql://localhost/postgres".to_string(),
                database_password: None,
                admin_password: Some("test-bootstrap-password".to_string()),
                auth_secret_key: None,
                auth_key_file: PathBuf::from("unused-alert-test-key"),
                secure_cookies: false,
                trust_proxy_headers: false,
                trusted_proxy_cidrs: Vec::new(),
                allow_legacy_agent_ws_auth: false,
                reset_admin_auth: false,
                confirm_reset_admin_auth: None,
                upload_dir: PathBuf::from("unused-alert-test-uploads"),
                update_dir: PathBuf::from("unused-alert-test-updates"),
                update_signing_key_file: None,
                update_signing_key_id: "test".to_string(),
                agent_package_max_bytes: 1024,
                file_transfer_max_bytes: 1024,
            },
            AuthCipher::from_key(&[11_u8; 32]).expect("create alert test cipher"),
            None,
        )
    }

    async fn insert_test_instance(db: &PgPool, id: &str, name: &str) {
        sqlx::query(
            r#"
            INSERT INTO instances(id,name,hostname,region,os,arch,agent_version,approved,disabled)
            VALUES($1,$2,'test-host','test-region','linux','x86_64','1.0.0',1,0)
            "#,
        )
        .bind(id)
        .bind(name)
        .execute(db)
        .await
        .expect("insert alert test instance");
    }

    async fn insert_test_rule(db: &PgPool, id: &str) -> RuleRow {
        sqlx::query(
            r#"
            INSERT INTO alert_rules(
                id,name,metric,threshold,duration_seconds,severity,scope,
                enabled,version,created_by,created_at,updated_at
            ) VALUES($1,'High CPU','cpu_percent',90,0,'critical','all',TRUE,1,'test',100,100)
            "#,
        )
        .bind(id)
        .execute(db)
        .await
        .expect("insert alert test rule");
        sqlx::query_as::<_, RuleRow>("SELECT * FROM alert_rules WHERE id=$1")
            .bind(id)
            .fetch_one(db)
            .await
            .expect("load alert test rule")
    }

    async fn set_test_channel(
        state: &AppState,
        id: &str,
        url: &str,
        secret: Option<&str>,
        headers: &BTreeMap<String, String>,
    ) {
        let url_ciphertext = state
            .auth_cipher
            .encrypt(url.as_bytes())
            .expect("encrypt test webhook URL");
        let secret_ciphertext = secret
            .map(|value| state.auth_cipher.encrypt(value.as_bytes()))
            .transpose()
            .expect("encrypt test webhook secret");
        let headers_ciphertext = state
            .auth_cipher
            .encrypt(
                serde_json::to_string(headers)
                    .expect("serialize test webhook headers")
                    .as_bytes(),
            )
            .expect("encrypt test webhook headers");
        sqlx::query(
            r#"
            INSERT INTO alert_webhook_channels(
                id,name,url_ciphertext,secret_ciphertext,headers_ciphertext,
                enabled,created_at,updated_at
            ) VALUES($1,'Test webhook',$2,$3,$4,TRUE,100,100)
            ON CONFLICT(id) DO UPDATE SET
                url_ciphertext=EXCLUDED.url_ciphertext,
                secret_ciphertext=EXCLUDED.secret_ciphertext,
                headers_ciphertext=EXCLUDED.headers_ciphertext,
                enabled=TRUE,deleted_at=NULL,updated_at=EXCLUDED.updated_at
            "#,
        )
        .bind(id)
        .bind(url_ciphertext)
        .bind(secret_ciphertext)
        .bind(headers_ciphertext)
        .execute(&state.db)
        .await
        .expect("store test webhook channel");
    }

    fn test_delivery(channel_id: &str) -> DeliveryRow {
        DeliveryRow {
            id: "delivery-test".to_string(),
            event_id: None,
            channel_id: channel_id.to_string(),
            kind: "webhook.test".to_string(),
            status: "processing".to_string(),
            payload: json!({"version": 1, "type": "webhook.test", "value": "payload"}),
            channel_snapshot: json!({"id": channel_id, "name": "Test webhook"}),
            suppression_reason: String::new(),
            attempts_count: 1,
            cycle_attempts: 1,
            manual_retry_count: 0,
            next_attempt_at: None,
            lease_until: Some(now_ts() + 60),
            lease_token: Some("test-lease-token".to_string()),
            last_error: String::new(),
            created_at: now_ts(),
            updated_at: now_ts(),
            completed_at: None,
        }
    }

    #[derive(Clone, Default)]
    struct WebhookCapture {
        requests: Arc<Mutex<Vec<(HeaderMap, Vec<u8>)>>>,
        redirect_target_hits: Arc<AtomicUsize>,
    }

    async fn capture_webhook(
        State(capture): State<WebhookCapture>,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        capture.requests.lock().await.push((headers, body.to_vec()));
        StatusCode::NO_CONTENT
    }

    async fn failing_webhook() -> impl IntoResponse {
        (StatusCode::INTERNAL_SERVER_ERROR, "receiver failure")
    }

    async fn slow_webhook() -> StatusCode {
        tokio::time::sleep(Duration::from_millis(250)).await;
        StatusCode::NO_CONTENT
    }

    async fn redirect_webhook() -> impl IntoResponse {
        (
            StatusCode::FOUND,
            [(axum::http::header::LOCATION, "/redirect-target")],
        )
    }

    async fn redirect_target(State(capture): State<WebhookCapture>) -> StatusCode {
        capture.redirect_target_hits.fetch_add(1, Ordering::SeqCst);
        StatusCode::NO_CONTENT
    }

    async fn large_webhook_response() -> String {
        "x".repeat(MAX_RESPONSE_BYTES + 1024)
    }

    async fn spawn_webhook_receiver(
        capture: WebhookCapture,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/capture", post(capture_webhook))
            .route("/fail", post(failing_webhook))
            .route("/slow", post(slow_webhook))
            .route("/redirect", post(redirect_webhook))
            .route("/redirect-target", post(redirect_target))
            .route("/large", post(large_webhook_response))
            .with_state(capture);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind webhook receiver");
        let address = listener
            .local_addr()
            .expect("read webhook receiver address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve webhook receiver");
        });
        (format!("http://{address}"), server)
    }

    #[test]
    fn computes_percentages_and_rejects_invalid_totals() {
        assert_eq!(percentage(50, 200), Some(25.0));
        assert_eq!(percentage(0, 0), None);
        assert_eq!(percentage(-1, 100), None);
        assert_eq!(percentage(101, 100), None);
    }

    #[test]
    fn metric_values_use_distinct_latency_sample_time() {
        let observation = MetricObservation {
            received_at: 200,
            cpu_percent: 10.0,
            memory_used: 20,
            memory_total: 100,
            disk_used: 30,
            disk_total: 100,
            latency_ms: Some(42.0),
            latency_sampled_at: Some(150),
        };
        assert_eq!(metric_value("cpu_percent", &observation), Some((10.0, 200)));
        assert_eq!(metric_value("latency_ms", &observation), Some((42.0, 150)));
    }

    #[test]
    fn validates_rule_threshold_shapes() {
        let valid_offline = RuleRequest {
            name: "offline".to_string(),
            metric: "node_offline".to_string(),
            threshold: None,
            duration_seconds: 60,
            severity: "critical".to_string(),
            scope: "all".to_string(),
            target_instance_ids: vec![],
            channel_ids: vec![],
            enabled: true,
        };
        assert!(validate_rule(&valid_offline).is_ok());
        let mut invalid = valid_offline.clone();
        invalid.threshold = Some(1.0);
        assert!(validate_rule(&invalid).is_err());
        invalid.metric = "cpu_percent".to_string();
        invalid.threshold = Some(f64::NAN);
        assert!(validate_rule(&invalid).is_err());
    }

    #[test]
    fn maintenance_interval_is_half_open() {
        fn active(starts_at: i64, ends_at: i64, now: i64) -> bool {
            starts_at <= now && ends_at > now
        }
        assert!(!active(100, 200, 99));
        assert!(active(100, 200, 100));
        assert!(active(100, 200, 199));
        assert!(!active(100, 200, 200));
    }

    #[test]
    fn webhook_signatures_are_stable_and_body_sensitive() {
        assert_eq!(
            webhook_signature(b"secret", 1_700_000_000, br#"{"ok":true}"#),
            "sha256=c1afc7c2df3db0690d7d75954610ed1a1d959ce96355ccb8c0a8bc09fd0cfc27"
        );
        assert_ne!(
            webhook_signature(b"secret", 1_700_000_000, br#"{"ok":true}"#),
            webhook_signature(b"secret", 1_700_000_000, br#"{"ok":false}"#)
        );
    }

    #[test]
    fn masks_webhook_queries_and_paths() {
        assert_eq!(
            mask_webhook_url("https://hooks.example.test/secret/path?token=sensitive"),
            "https://hooks.example.test/..."
        );
    }

    #[test]
    fn active_offline_event_suppresses_only_resource_events() {
        assert!(node_offline_suppresses("cpu_percent", true));
        assert!(node_offline_suppresses("latency_ms", true));
        assert!(!node_offline_suppresses("node_offline", true));
        assert!(!node_offline_suppresses("cpu_percent", false));
    }

    #[test]
    fn zero_duration_fires_on_first_abnormal_sample() {
        assert!(pending_should_fire(0, None, None, 100));
        assert!(!pending_should_fire(60, None, None, 100));
        assert!(!pending_should_fire(60, Some(100), Some(100), 159));
        assert!(pending_should_fire(60, Some(100), Some(100), 160));
    }

    #[test]
    fn same_second_connection_change_overrides_repeated_state() {
        let (offline_value, offline_abnormal) = connection_observation(false);
        let (online_value, online_abnormal) = connection_observation(true);

        assert!(offline_abnormal);
        assert!(!online_abnormal);
        assert!(observation_is_stale(
            Some(100),
            Some(offline_value),
            100,
            offline_value,
        ));
        assert!(!observation_is_stale(
            Some(100),
            Some(offline_value),
            100,
            online_value,
        ));
        assert!(observation_is_stale(
            Some(101),
            Some(online_value),
            100,
            offline_value,
        ));
    }

    #[test]
    fn startup_reconnect_grace_defers_only_offline_nodes_for_sixty_seconds() {
        assert!(startup_grace_defers_offline(false, 100, 159));
        assert!(!startup_grace_defers_offline(false, 100, 160));
        assert!(!startup_grace_defers_offline(true, 100, 101));
    }

    #[test]
    fn recovery_observation_replaces_last_abnormal_value_and_time() {
        assert_eq!(
            resolution_values(Some(95.0), 100, Some((42.0, 120))),
            (Some(42.0), 120),
        );
        assert_eq!(resolution_values(Some(95.0), 100, None), (Some(95.0), 100),);
    }

    #[test]
    fn delivery_retry_schedule_has_five_total_attempts() {
        assert_eq!(retry_delay_after(1), Some(60));
        assert_eq!(retry_delay_after(2), Some(5 * 60));
        assert_eq!(retry_delay_after(3), Some(15 * 60));
        assert_eq!(retry_delay_after(4), Some(60 * 60));
        assert_eq!(retry_delay_after(5), None);
        assert_eq!(retry_delay_after(0), None);
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn webhook_transport_records_complete_safe_attempt_details() {
        let (db, bootstrap, schema) = isolated_test_pool("om_alerts_webhook").await;
        create_alert_prerequisites(&db).await;
        ensure_schema(&db).await.expect("create alert schema");
        let state = test_state(db.clone());
        let capture = WebhookCapture::default();
        let (base_url, server) = spawn_webhook_receiver(capture.clone()).await;
        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .expect("build webhook test client");
        let mut custom_headers = BTreeMap::new();
        custom_headers.insert("x-test-header".to_string(), "header-value".to_string());
        set_test_channel(
            &state,
            "transport-channel",
            &format!("{base_url}/capture"),
            Some("transport-secret"),
            &custom_headers,
        )
        .await;
        let delivery = test_delivery("transport-channel");

        let success = send_webhook(&client, &state, &delivery).await;
        assert!(success.succeeded);
        assert_eq!(success.http_status, Some(204));
        let requests = capture.requests.lock().await;
        assert_eq!(requests.len(), 1);
        let (headers, body) = &requests[0];
        assert_eq!(
            headers
                .get("x-test-header")
                .and_then(|value| value.to_str().ok()),
            Some("header-value"),
        );
        assert_eq!(
            headers
                .get("x-om-delivery-id")
                .and_then(|value| value.to_str().ok()),
            Some(delivery.id.as_str()),
        );
        let timestamp = headers
            .get("x-om-timestamp")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok())
            .expect("signed webhook timestamp");
        let expected_signature = webhook_signature(b"transport-secret", timestamp, body);
        assert_eq!(
            headers
                .get("x-om-signature")
                .and_then(|value| value.to_str().ok()),
            Some(expected_signature.as_str()),
        );
        assert_eq!(
            serde_json::from_slice::<Value>(body).expect("decode captured webhook body"),
            delivery.payload,
        );
        drop(requests);

        set_test_channel(
            &state,
            "transport-channel",
            &format!("{base_url}/fail"),
            None,
            &BTreeMap::new(),
        )
        .await;
        let failed = send_webhook(&client, &state, &delivery).await;
        assert!(!failed.succeeded);
        assert_eq!(failed.http_status, Some(500));
        assert_eq!(failed.error, "http_status_500");
        assert_eq!(failed.response_excerpt, "receiver failure");

        set_test_channel(
            &state,
            "transport-channel",
            &format!("{base_url}/redirect"),
            None,
            &BTreeMap::new(),
        )
        .await;
        let redirected = send_webhook(&client, &state, &delivery).await;
        assert!(!redirected.succeeded);
        assert_eq!(redirected.http_status, Some(302));
        assert_eq!(capture.redirect_target_hits.load(Ordering::SeqCst), 0);

        set_test_channel(
            &state,
            "transport-channel",
            &format!("{base_url}/large"),
            None,
            &BTreeMap::new(),
        )
        .await;
        let truncated = send_webhook(&client, &state, &delivery).await;
        assert!(truncated.succeeded);
        assert_eq!(truncated.response_excerpt.len(), MAX_RESPONSE_BYTES);

        set_test_channel(
            &state,
            "transport-channel",
            &format!("{base_url}/slow"),
            None,
            &BTreeMap::new(),
        )
        .await;
        let timeout_client = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_millis(50))
            .build()
            .expect("build timeout webhook client");
        let timed_out = send_webhook(&timeout_client, &state, &delivery).await;
        assert!(!timed_out.succeeded);
        assert_eq!(timed_out.http_status, None);
        assert!(timed_out.error.contains("webhook request failed"));
        assert!(!timed_out.error.contains(&base_url));

        sqlx::query(
            r#"
            INSERT INTO alert_deliveries(
                id,event_id,channel_id,kind,status,payload,channel_snapshot,
                attempts_count,cycle_attempts,last_error,created_at,updated_at,completed_at
            ) VALUES(
                'manual-retry',NULL,'transport-channel','webhook.test','failed',
                '{}'::JSONB,'{}'::JSONB,5,5,'http_status_500',100,100,100
            )
            "#,
        )
        .execute(&db)
        .await
        .expect("insert failed test delivery");
        sqlx::query(
            r#"
            INSERT INTO alert_delivery_attempts(
                id,delivery_id,attempt_number,http_status,duration_ms,error,response_excerpt,created_at
            ) VALUES('manual-attempt','manual-retry',5,500,10,'http_status_500','failure',100)
            "#,
        )
        .execute(&db)
        .await
        .expect("insert existing delivery attempt");
        let mut retry_tx = db.begin().await.expect("begin manual retry");
        reset_failed_delivery_tx(&mut retry_tx, "manual-retry", 200)
            .await
            .expect("reset failed test delivery");
        retry_tx.commit().await.expect("commit manual retry");
        let reset = sqlx::query_as::<_, (String, i64, i64, i64, String)>(
            r#"
            SELECT status,attempts_count,cycle_attempts,manual_retry_count,last_error
            FROM alert_deliveries WHERE id='manual-retry'
            "#,
        )
        .fetch_one(&db)
        .await
        .expect("load reset test delivery");
        assert_eq!(reset, ("pending".to_string(), 5, 0, 1, String::new()));
        let attempt_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM alert_delivery_attempts WHERE delivery_id='manual-retry'",
        )
        .fetch_one(&db)
        .await
        .expect("count preserved attempt history");
        assert_eq!(attempt_count, 1);

        sqlx::query("UPDATE alert_deliveries SET status='failed' WHERE id='manual-retry'")
            .execute(&db)
            .await
            .expect("fail delivery again");
        sqlx::query("UPDATE alert_webhook_channels SET enabled=FALSE WHERE id='transport-channel'")
            .execute(&db)
            .await
            .expect("disable retry channel");
        let mut disabled_tx = db.begin().await.expect("begin disabled retry");
        let disabled = reset_failed_delivery_tx(&mut disabled_tx, "manual-retry", 300)
            .await
            .expect_err("disabled channel retry must fail");
        assert_eq!(disabled.status, StatusCode::CONFLICT);
        assert!(disabled.message.contains("已停用"));
        disabled_tx
            .rollback()
            .await
            .expect("rollback disabled retry");

        server.abort();
        let _ = server.await;
        drop_test_schema(db, bootstrap, schema).await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn acknowledgement_and_rule_edits_preserve_lifecycle_contracts() {
        let (db, bootstrap, schema) = isolated_test_pool("om_alerts_lifecycle").await;
        create_alert_prerequisites(&db).await;
        ensure_schema(&db)
            .await
            .expect("create lifecycle test schema");
        let state = test_state(db.clone());
        insert_test_instance(&db, "node-lifecycle", "Lifecycle Node").await;
        let rule = insert_test_rule(&db, "rule-lifecycle").await;
        set_test_channel(
            &state,
            "channel-lifecycle",
            "http://127.0.0.1/unused",
            None,
            &BTreeMap::new(),
        )
        .await;
        sqlx::query(
            "INSERT INTO alert_rule_channels(rule_id,channel_id) VALUES('rule-lifecycle','channel-lifecycle')",
        )
        .execute(&db)
        .await
        .expect("link lifecycle channel");
        let observed_at = now_ts();
        process_rule_observation(
            &state,
            &rule,
            "node-lifecycle",
            RuleObservation::Threshold {
                value: 95.0,
                abnormal: true,
            },
            observed_at,
            observed_at,
        )
        .await
        .expect("fire lifecycle event");
        let event_id: String = sqlx::query_scalar(
            "SELECT id FROM alert_events WHERE rule_id='rule-lifecycle' AND instance_id='node-lifecycle'",
        )
        .fetch_one(&db)
        .await
        .expect("load lifecycle event");

        let mut acknowledge_tx = db.begin().await.expect("begin event acknowledgement");
        acknowledge_event_tx(
            &mut acknowledge_tx,
            &event_id,
            "operator",
            "user-1",
            "investigating",
            observed_at + 1,
        )
        .await
        .expect("acknowledge lifecycle event");
        acknowledge_tx
            .commit()
            .await
            .expect("commit event acknowledgement");
        let acknowledged = sqlx::query_as::<_, (String, Option<String>, String)>(
            "SELECT status,acknowledged_by,acknowledge_note FROM alert_events WHERE id=$1",
        )
        .bind(&event_id)
        .fetch_one(&db)
        .await
        .expect("load acknowledged event");
        assert_eq!(
            acknowledged,
            (
                "acknowledged".to_string(),
                Some("operator".to_string()),
                "investigating".to_string(),
            )
        );

        let notification_only = RuleRequest {
            name: "Renamed high CPU".to_string(),
            metric: "cpu_percent".to_string(),
            threshold: Some(90.0),
            duration_seconds: 0,
            severity: "critical".to_string(),
            scope: "all".to_string(),
            target_instance_ids: vec![],
            channel_ids: vec![],
            enabled: true,
        };
        let mut rename_tx = db.begin().await.expect("begin notification-only edit");
        let changed = update_rule_tx(
            &mut rename_tx,
            "rule-lifecycle",
            &notification_only,
            observed_at + 2,
        )
        .await
        .expect("apply notification-only rule edit");
        assert!(!changed);
        rename_tx
            .commit()
            .await
            .expect("commit notification-only edit");
        let unchanged = sqlx::query_as::<_, (i64, String, String, i64)>(
            r#"
            SELECT r.version,r.name,e.status,
                (SELECT COUNT(*) FROM alert_evaluation_states s WHERE s.rule_id=r.id)::BIGINT
            FROM alert_rules r JOIN alert_events e ON e.rule_id=r.id
            WHERE r.id='rule-lifecycle'
            "#,
        )
        .fetch_one(&db)
        .await
        .expect("load notification-only edit result");
        assert_eq!(
            unchanged,
            (
                1,
                "Renamed high CPU".to_string(),
                "acknowledged".to_string(),
                1,
            )
        );

        let condition_edit = RuleRequest {
            threshold: Some(91.0),
            ..notification_only
        };
        let mut condition_tx = db.begin().await.expect("begin condition edit");
        let changed = update_rule_tx(
            &mut condition_tx,
            "rule-lifecycle",
            &condition_edit,
            observed_at + 3,
        )
        .await
        .expect("apply condition rule edit");
        assert!(changed);
        condition_tx.commit().await.expect("commit condition edit");
        let resolved = sqlx::query_as::<_, (i64, String, String, i64)>(
            r#"
            SELECT r.version,e.status,e.resolution_reason,
                (SELECT COUNT(*) FROM alert_evaluation_states s WHERE s.rule_id=r.id)::BIGINT
            FROM alert_rules r JOIN alert_events e ON e.rule_id=r.id
            WHERE r.id='rule-lifecycle'
            "#,
        )
        .fetch_one(&db)
        .await
        .expect("load condition edit result");
        assert_eq!(
            resolved,
            (
                2,
                "resolved".to_string(),
                "rule_conditions_changed".to_string(),
                0,
            )
        );
        let lifecycle_kinds = sqlx::query_as::<_, (String, i64)>(
            r#"
            SELECT kind,COUNT(*)::BIGINT FROM alert_deliveries
            WHERE event_id=$1 GROUP BY kind ORDER BY kind
            "#,
        )
        .bind(&event_id)
        .fetch_all(&db)
        .await
        .expect("load lifecycle outbox kinds");
        assert_eq!(
            lifecycle_kinds,
            vec![
                ("alert.acknowledged".to_string(), 1),
                ("alert.firing".to_string(), 1),
                ("alert.resolved".to_string(), 1),
            ]
        );

        drop_test_schema(db, bootstrap, schema).await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn postgres_schema_evaluation_outbox_lease_snapshots_and_cleanup_are_consistent() {
        let (db, bootstrap, schema) = isolated_test_pool("om_alerts_domain").await;
        create_alert_prerequisites(&db).await;
        sqlx::query("INSERT INTO metrics(instance_id,ts,latency_ms) VALUES('legacy-node',123,42)")
            .execute(&db)
            .await
            .expect("insert legacy metric");

        ensure_schema(&db).await.expect("create alert schema");
        ensure_schema(&db)
            .await
            .expect("recreate alert schema idempotently");
        let migrated_metric = sqlx::query_as::<_, (i64, Option<i64>)>(
            "SELECT received_at,latency_sampled_at FROM metrics WHERE instance_id='legacy-node'",
        )
        .fetch_one(&db)
        .await
        .expect("load migrated metric");
        assert_eq!(migrated_metric, (123, Some(123)));

        let state = test_state(db.clone());
        insert_test_instance(&db, "node-1", "Node One").await;
        let rule = insert_test_rule(&db, "rule-1").await;
        set_test_channel(
            &state,
            "channel-1",
            "http://127.0.0.1/unused",
            None,
            &BTreeMap::new(),
        )
        .await;
        sqlx::query(
            "INSERT INTO alert_rule_channels(rule_id,channel_id) VALUES('rule-1','channel-1')",
        )
        .execute(&db)
        .await
        .expect("link test webhook channel");

        let observed_at = now_ts();
        let (first, second) = tokio::join!(
            process_rule_observation(
                &state,
                &rule,
                "node-1",
                RuleObservation::Threshold {
                    value: 95.0,
                    abnormal: true,
                },
                observed_at,
                observed_at,
            ),
            process_rule_observation(
                &state,
                &rule,
                "node-1",
                RuleObservation::Threshold {
                    value: 95.0,
                    abnormal: true,
                },
                observed_at,
                observed_at,
            ),
        );
        first.expect("first concurrent evaluation");
        second.expect("second concurrent evaluation");

        let event_id: String = sqlx::query_scalar(
            "SELECT id FROM alert_events WHERE rule_id='rule-1' AND instance_id='node-1'",
        )
        .fetch_one(&db)
        .await
        .expect("load concurrent event");
        let event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM alert_events WHERE rule_id='rule-1' AND instance_id='node-1'",
        )
        .fetch_one(&db)
        .await
        .expect("count concurrent events");
        let firing_delivery_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM alert_deliveries WHERE event_id=$1 AND kind='alert.firing'",
        )
        .bind(&event_id)
        .fetch_one(&db)
        .await
        .expect("count transactional firing deliveries");
        assert_eq!(event_count, 1);
        assert_eq!(firing_delivery_count, 1);

        let duplicate = sqlx::query(
            r#"
            INSERT INTO alert_events(
                id,rule_id,instance_id,status,severity,metric,rule_snapshot,node_snapshot,
                threshold,duration_seconds,current_value,first_observed_at,fired_at,last_observed_at
            ) VALUES(
                'duplicate-active','rule-1','node-1','firing','critical','cpu_percent',
                '{}'::JSONB,'{}'::JSONB,90,0,96,$1,$1,$1
            )
            "#,
        )
        .bind(observed_at)
        .execute(&db)
        .await
        .expect_err("active event uniqueness must reject a duplicate");
        let duplicate_code = duplicate
            .as_database_error()
            .and_then(|error| error.code())
            .map(|code| code.into_owned());
        assert_eq!(duplicate_code.as_deref(), Some("23505"));

        let (first_claim, second_claim) = tokio::join!(
            claim_delivery(&db, observed_at),
            claim_delivery(&db, observed_at),
        );
        let first_claim = first_claim.expect("first lease claim");
        let second_claim = second_claim.expect("second lease claim");
        assert_ne!(first_claim.is_some(), second_claim.is_some());
        let stale_claim = first_claim.or(second_claim).expect("one claimed delivery");
        sqlx::query("UPDATE alert_deliveries SET lease_until=$1 WHERE id=$2")
            .bind(observed_at - 1)
            .bind(&stale_claim.id)
            .execute(&db)
            .await
            .expect("expire first delivery lease");
        let reclaimed = claim_delivery(&db, observed_at)
            .await
            .expect("reclaim expired delivery")
            .expect("expired delivery was reclaimed");
        assert_ne!(stale_claim.lease_token, reclaimed.lease_token);
        assert!(
            start_delivery_attempt(&db, &stale_claim, observed_at)
                .await
                .expect("fence stale attempt start")
                .is_none()
        );
        let active_claim = start_delivery_attempt(&db, &reclaimed, observed_at)
            .await
            .expect("start current delivery attempt")
            .expect("current lease owns delivery");
        finish_delivery_attempt(
            &db,
            &stale_claim,
            WebhookAttemptOutcome {
                succeeded: true,
                http_status: Some(204),
                duration_ms: 1,
                error: String::new(),
                response_excerpt: String::new(),
            },
        )
        .await
        .expect("discard stale delivery completion");
        let after_stale = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT status,lease_token FROM alert_deliveries WHERE id=$1",
        )
        .bind(&active_claim.id)
        .fetch_one(&db)
        .await
        .expect("load delivery after stale completion");
        assert_eq!(after_stale.0, "processing");
        assert_eq!(after_stale.1, active_claim.lease_token);
        finish_delivery_attempt(
            &db,
            &active_claim,
            WebhookAttemptOutcome {
                succeeded: true,
                http_status: Some(204),
                duration_ms: 1,
                error: String::new(),
                response_excerpt: String::new(),
            },
        )
        .await
        .expect("complete current delivery lease");
        let completed = sqlx::query_as::<_, (String, i64)>(
            r#"
            SELECT d.status,COUNT(a.id)::BIGINT
            FROM alert_deliveries d
            LEFT JOIN alert_delivery_attempts a ON a.delivery_id=d.id
            WHERE d.id=$1 GROUP BY d.status
            "#,
        )
        .bind(&active_claim.id)
        .fetch_one(&db)
        .await
        .expect("load fenced delivery completion");
        assert_eq!(completed, ("succeeded".to_string(), 1));

        sqlx::query(
            r#"
            INSERT INTO alert_maintenance_windows(
                id,name,reason,scope,starts_at,ends_at,enabled,created_by,created_at,updated_at
            ) VALUES('node-window','Node window','','node',$1,$2,TRUE,'test',$1,$1)
            "#,
        )
        .bind(observed_at)
        .bind(observed_at + 3600)
        .execute(&db)
        .await
        .expect("insert node maintenance window");
        sqlx::query(
            "INSERT INTO alert_maintenance_targets(window_id,target_id) VALUES('node-window','node-1')",
        )
        .execute(&db)
        .await
        .expect("insert node maintenance target");
        let mut delete_tx = db.begin().await.expect("begin node deletion");
        resolve_instance(&mut delete_tx, "node-1", "instance_deleted")
            .await
            .expect("resolve events before node deletion");
        remove_instance_maintenance_target(&mut delete_tx, "node-1")
            .await
            .expect("remove deleted node from maintenance windows");
        sqlx::query("DELETE FROM instances WHERE id='node-1'")
            .execute(&mut *delete_tx)
            .await
            .expect("delete alert test node");
        delete_tx.commit().await.expect("commit node deletion");
        let resolved_snapshot = sqlx::query_as::<_, (String, Value)>(
            "SELECT status,node_snapshot FROM alert_events WHERE id=$1",
        )
        .bind(&event_id)
        .fetch_one(&db)
        .await
        .expect("load resolved event snapshot");
        assert_eq!(resolved_snapshot.0, "resolved");
        assert_eq!(resolved_snapshot.1["name"], "Node One");
        let empty_node_window_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM alert_maintenance_windows WHERE id='node-window')",
        )
        .fetch_one(&db)
        .await
        .expect("check empty node maintenance cleanup");
        assert!(!empty_node_window_exists);

        insert_test_instance(&db, "node-2", "Node Two").await;
        process_rule_observation(
            &state,
            &rule,
            "node-2",
            RuleObservation::Threshold {
                value: 97.0,
                abnormal: true,
            },
            observed_at + 1,
            observed_at + 1,
        )
        .await
        .expect("create active event retained by cleanup");
        let active_event_id: String = sqlx::query_scalar(
            "SELECT id FROM alert_events WHERE rule_id='rule-1' AND instance_id='node-2'",
        )
        .fetch_one(&db)
        .await
        .expect("load retained active event");
        sqlx::query(
            r#"
            INSERT INTO alert_deliveries(
                id,event_id,channel_id,kind,status,payload,channel_snapshot,
                created_at,updated_at,completed_at
            ) VALUES(
                'old-test-delivery',NULL,'channel-1','webhook.test','succeeded',
                '{}'::JSONB,'{}'::JSONB,$1,$1,$1
            )
            "#,
        )
        .bind(observed_at)
        .execute(&db)
        .await
        .expect("insert old test delivery");
        sqlx::query(
            r#"
            INSERT INTO alert_maintenance_windows(
                id,name,reason,scope,starts_at,ends_at,enabled,created_by,created_at,updated_at
            ) VALUES('old-window','Old window','','global',$1,$2,TRUE,'test',$1,$1)
            "#,
        )
        .bind(observed_at - 10)
        .bind(observed_at)
        .execute(&db)
        .await
        .expect("insert old maintenance window");

        cleanup_old_alerts(&db, now_ts() + 10)
            .await
            .expect("clean old alert records");
        let resolved_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM alert_events WHERE id=$1)")
                .bind(&event_id)
                .fetch_one(&db)
                .await
                .expect("check resolved event cleanup");
        let active_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM alert_events WHERE id=$1)")
                .bind(&active_event_id)
                .fetch_one(&db)
                .await
                .expect("check active event retention");
        let old_test_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM alert_deliveries WHERE id='old-test-delivery')",
        )
        .fetch_one(&db)
        .await
        .expect("check test delivery cleanup");
        let old_window_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM alert_maintenance_windows WHERE id='old-window')",
        )
        .fetch_one(&db)
        .await
        .expect("check maintenance cleanup");
        assert!(!resolved_exists);
        assert!(active_exists);
        assert!(!old_test_exists);
        assert!(!old_window_exists);

        insert_test_instance(&db, "node-3", "Node Three").await;
        sqlx::query(
            r#"
            INSERT INTO alert_rules(
                id,name,metric,threshold,duration_seconds,severity,scope,
                enabled,version,created_by,created_at,updated_at
            ) VALUES(
                'offline-rule','Offline','node_offline',NULL,60,'critical','all',
                TRUE,1,'test',100,100
            )
            "#,
        )
        .execute(&db)
        .await
        .expect("insert offline reset rule");
        for rule_id in ["rule-1", "offline-rule"] {
            sqlx::query(
                r#"
                INSERT INTO alert_evaluation_states(
                    rule_id,instance_id,rule_version,pending_since,last_observed_at,
                    last_value,last_sample_at,match_count
                ) VALUES($1,'node-3',1,100,100,1,100,1)
                "#,
            )
            .bind(rule_id)
            .execute(&db)
            .await
            .expect("insert pending reset state");
        }
        reset_metric_pending_states(&db, None)
            .await
            .expect("reset resource pending states on startup");
        let remaining_pending = sqlx::query_scalar::<_, String>(
            "SELECT rule_id FROM alert_evaluation_states WHERE instance_id='node-3' ORDER BY rule_id",
        )
        .fetch_all(&db)
        .await
        .expect("load pending states after startup reset");
        assert_eq!(remaining_pending, vec!["offline-rule".to_string()]);

        drop_test_schema(db, bootstrap, schema).await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn webhook_transport_enforces_signing_limits_redirects_timeouts_and_manual_retry() {
        let (db, bootstrap, schema) = isolated_test_pool("om_alerts_webhook").await;
        create_alert_prerequisites(&db).await;
        ensure_schema(&db)
            .await
            .expect("create webhook test schema");
        let state = test_state(db.clone());

        let capture = WebhookCapture::default();
        let app = Router::new()
            .route("/ok", axum::routing::post(capture_webhook))
            .route("/fail", axum::routing::post(failing_webhook))
            .route("/slow", axum::routing::post(slow_webhook))
            .route("/redirect", axum::routing::post(redirect_webhook))
            .route("/redirect-target", axum::routing::post(redirect_target))
            .route("/large", axum::routing::post(large_webhook_response))
            .with_state(capture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind webhook receiver");
        let address = listener
            .local_addr()
            .expect("read webhook receiver address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve webhook receiver");
        });

        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_millis(100))
            .build()
            .expect("build webhook test client");
        let mut custom_headers = BTreeMap::new();
        custom_headers.insert("x-test-token".to_string(), "configured".to_string());
        let delivery = test_delivery("channel-webhook");
        set_test_channel(
            &state,
            "channel-webhook",
            &format!("http://{address}/ok?token=hidden"),
            Some("signing-secret"),
            &custom_headers,
        )
        .await;

        let success = send_webhook(&client, &state, &delivery).await;
        assert!(success.succeeded);
        assert_eq!(success.http_status, Some(204));
        let captured = capture.requests.lock().await;
        let (headers, body) = captured.first().expect("captured webhook request");
        assert_eq!(body, &serde_json::to_vec(&delivery.payload).unwrap());
        assert_eq!(
            headers
                .get("x-om-delivery-id")
                .and_then(|value| value.to_str().ok()),
            Some(delivery.id.as_str())
        );
        assert_eq!(
            headers
                .get("x-test-token")
                .and_then(|value| value.to_str().ok()),
            Some("configured")
        );
        let timestamp = headers
            .get("x-om-timestamp")
            .expect("webhook timestamp")
            .to_str()
            .expect("valid webhook timestamp")
            .parse::<i64>()
            .expect("numeric webhook timestamp");
        let expected_signature = webhook_signature(b"signing-secret", timestamp, body);
        assert_eq!(
            headers
                .get("x-om-signature")
                .and_then(|value| value.to_str().ok()),
            Some(expected_signature.as_str())
        );
        drop(captured);

        set_test_channel(
            &state,
            "channel-webhook",
            &format!("http://{address}/large"),
            Some("signing-secret"),
            &custom_headers,
        )
        .await;
        let large = send_webhook(&client, &state, &delivery).await;
        assert!(large.succeeded);
        assert_eq!(large.response_excerpt.len(), MAX_RESPONSE_BYTES);

        set_test_channel(
            &state,
            "channel-webhook",
            &format!("http://{address}/redirect"),
            None,
            &BTreeMap::new(),
        )
        .await;
        let redirected = send_webhook(&client, &state, &delivery).await;
        assert!(!redirected.succeeded);
        assert_eq!(redirected.http_status, Some(302));
        assert_eq!(capture.redirect_target_hits.load(Ordering::SeqCst), 0);

        set_test_channel(
            &state,
            "channel-webhook",
            &format!("http://{address}/slow?secret=hidden"),
            None,
            &BTreeMap::new(),
        )
        .await;
        let timed_out = send_webhook(&client, &state, &delivery).await;
        assert!(!timed_out.succeeded);
        assert_eq!(timed_out.http_status, None);
        assert!(!timed_out.error.contains("secret=hidden"));

        set_test_channel(
            &state,
            "channel-webhook",
            &format!("http://{address}/fail"),
            None,
            &BTreeMap::new(),
        )
        .await;
        let mut failed_delivery = delivery;
        failed_delivery.attempts_count = 5;
        failed_delivery.cycle_attempts = 5;
        sqlx::query(
            r#"
            INSERT INTO alert_deliveries(
                id,event_id,channel_id,kind,status,payload,channel_snapshot,
                attempts_count,cycle_attempts,lease_until,lease_token,created_at,updated_at
            ) VALUES($1,NULL,$2,'webhook.test','processing',$3,$4,5,5,$5,$6,$7,$7)
            "#,
        )
        .bind(&failed_delivery.id)
        .bind(&failed_delivery.channel_id)
        .bind(&failed_delivery.payload)
        .bind(&failed_delivery.channel_snapshot)
        .bind(now_ts() + 60)
        .bind(failed_delivery.lease_token.as_deref())
        .bind(now_ts())
        .execute(&db)
        .await
        .expect("insert failed delivery candidate");
        let failure = send_webhook(&client, &state, &failed_delivery).await;
        assert_eq!(failure.http_status, Some(500));
        finish_delivery_attempt(&db, &failed_delivery, failure)
            .await
            .expect("record terminal delivery failure");
        let mut retry_tx = db.begin().await.expect("begin manual retry");
        reset_failed_delivery_tx(&mut retry_tx, &failed_delivery.id, now_ts())
            .await
            .expect("reset failed delivery");
        retry_tx.commit().await.expect("commit manual retry");
        let retried = sqlx::query_as::<_, (String, i64, i64, i64)>(
            "SELECT status,attempts_count,cycle_attempts,manual_retry_count FROM alert_deliveries WHERE id=$1",
        )
        .bind(&failed_delivery.id)
        .fetch_one(&db)
        .await
        .expect("load manually retried delivery");
        let attempt_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM alert_delivery_attempts WHERE delivery_id=$1")
                .bind(&failed_delivery.id)
                .fetch_one(&db)
                .await
                .expect("count preserved delivery attempts");
        assert_eq!(retried, ("pending".to_string(), 5, 0, 1));
        assert_eq!(attempt_count, 1);

        server.abort();
        let _ = server.await;
        drop_test_schema(db, bootstrap, schema).await;
    }
}
