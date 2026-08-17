use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    routing::{get, patch, post, put},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use hmac::{Hmac, KeyInit, Mac};
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    address::Address,
    message::{Mailbox, header::ContentType},
    transport::smtp::authentication::Credentials,
    transport::smtp::client::{Tls, TlsParameters},
};
use reqwest::{Client, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::{AssertSqlSafe, FromRow, PgPool, Postgres, Transaction};
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
const MAX_RULE_TARGET_INSTANCE_IDS: usize = 1_000;
const MAX_RULE_CHANNEL_IDS: usize = 100;
const MAX_HEADER_COUNT: usize = 32;
const MAX_HEADER_NAME_BYTES: usize = 128;
const MAX_HEADER_VALUE_BYTES: usize = 4_096;
const MAX_RESPONSE_BYTES: usize = 8 * 1024;
const WEBHOOK_TIMEOUT_SECONDS: u64 = 10;
const SMTP_TIMEOUT_SECONDS: u64 = 15;
const MAX_EMAIL_RECIPIENTS: usize = 100;
const MAX_EMAIL_FIELD_BYTES: usize = 512;
const MAX_NOTIFICATION_TEXT_BYTES: usize = 16 * 1024;
const MAX_WECOM_TEXT_BYTES: usize = 1_900;
const MAX_TELEGRAM_CHAT_ID_BYTES: usize = 256;
const STARTUP_RECONNECT_GRACE_SECONDS: i64 = 60;
const OFFLINE_RECOVERY_STABILITY_SECONDS: i64 = 60;
const DELIVERY_LEASE_SECONDS: i64 = 180;
const DELIVERY_WORKER_COUNT: usize = 4;
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
    channel_type: String,
    url_ciphertext: String,
    secret_ciphertext: Option<String>,
    headers_ciphertext: String,
    config_ciphertext: Option<String>,
    enabled: bool,
    created_at: i64,
    updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
struct ChannelResponse {
    id: String,
    name: String,
    channel_type: String,
    masked_url: String,
    header_names: Vec<String>,
    has_secret: bool,
    smtp_host: Option<String>,
    smtp_port: Option<u16>,
    security: Option<String>,
    username: Option<String>,
    from_address: Option<String>,
    from_name: Option<String>,
    recipients: Option<Vec<String>>,
    has_password: bool,
    chat_id: Option<String>,
    enabled: bool,
    created_at: i64,
    updated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct EmailChannelConfig {
    smtp_host: String,
    smtp_port: u16,
    security: String,
    username: Option<String>,
    password: Option<String>,
    from_address: String,
    from_name: Option<String>,
    recipients: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct TelegramChannelConfig {
    chat_id: String,
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
    channel_type: Option<String>,
    url: Option<String>,
    secret: Option<String>,
    #[serde(default)]
    clear_secret: bool,
    headers: Option<BTreeMap<String, String>>,
    smtp_host: Option<String>,
    smtp_port: Option<u16>,
    security: Option<String>,
    username: Option<String>,
    password: Option<String>,
    #[serde(default)]
    clear_password: bool,
    from_address: Option<String>,
    from_name: Option<String>,
    recipients: Option<Vec<String>>,
    chat_id: Option<String>,
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
            get(get_channel).put(update_channel).delete(delete_channel),
        )
        .route("/webhook-channels/{id}/test", post(test_channel))
        .route("/channels", get(list_channels).post(create_channel))
        .route(
            "/channels/{id}",
            get(get_channel).put(update_channel).delete(delete_channel),
        )
        .route("/channels/{id}/test", post(test_channel))
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
            metric TEXT NOT NULL CHECK(metric IN ('node_offline', 'cpu_percent', 'memory_percent', 'disk_percent', 'latency_ms', 'instance_expiring')),
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
        ALTER TABLE alert_rules DROP CONSTRAINT IF EXISTS alert_rules_metric_check;
        ALTER TABLE alert_rules DROP CONSTRAINT IF EXISTS alert_rules_check;
        ALTER TABLE alert_rules DROP CONSTRAINT IF EXISTS alert_rules_threshold_check;
        ALTER TABLE alert_rules ADD CONSTRAINT alert_rules_metric_check
            CHECK(metric IN ('node_offline', 'cpu_percent', 'memory_percent', 'disk_percent', 'latency_ms', 'instance_expiring'));
        ALTER TABLE alert_rules ADD CONSTRAINT alert_rules_threshold_check
            CHECK((metric = 'node_offline' AND threshold IS NULL) OR
                  (metric <> 'node_offline' AND threshold IS NOT NULL));
        CREATE TABLE IF NOT EXISTS alert_rule_targets (
            rule_id TEXT NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
            instance_id TEXT NOT NULL REFERENCES instances(id) ON DELETE CASCADE,
            PRIMARY KEY(rule_id, instance_id)
        );
        CREATE TABLE IF NOT EXISTS alert_webhook_channels (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            channel_type TEXT NOT NULL DEFAULT 'generic_webhook'
                CONSTRAINT alert_webhook_channels_type_check
                CHECK(channel_type IN ('generic_webhook', 'email', 'feishu', 'wecom', 'dingtalk', 'slack', 'msteams', 'telegram', 'discord')),
            url_ciphertext TEXT NOT NULL,
            secret_ciphertext TEXT,
            headers_ciphertext TEXT NOT NULL,
            config_ciphertext TEXT,
            enabled BOOLEAN NOT NULL DEFAULT TRUE,
            delivery_cooldown_until BIGINT,
            created_at BIGINT NOT NULL,
            updated_at BIGINT NOT NULL,
            deleted_at BIGINT
        );
        ALTER TABLE alert_webhook_channels
            ADD COLUMN IF NOT EXISTS channel_type TEXT NOT NULL DEFAULT 'generic_webhook';
        ALTER TABLE alert_webhook_channels
            ADD COLUMN IF NOT EXISTS config_ciphertext TEXT;
        ALTER TABLE alert_webhook_channels
            ADD COLUMN IF NOT EXISTS delivery_cooldown_until BIGINT;
        UPDATE alert_webhook_channels
            SET channel_type='generic_webhook' WHERE channel_type IS NULL;
        ALTER TABLE alert_webhook_channels
            ALTER COLUMN channel_type SET DEFAULT 'generic_webhook';
        ALTER TABLE alert_webhook_channels
            ALTER COLUMN channel_type SET NOT NULL;
        ALTER TABLE alert_webhook_channels
            DROP CONSTRAINT IF EXISTS alert_webhook_channels_type_check;
        ALTER TABLE alert_webhook_channels
            DROP CONSTRAINT IF EXISTS alert_webhook_channels_channel_type_check;
        ALTER TABLE alert_webhook_channels
            ADD CONSTRAINT alert_webhook_channels_type_check
            CHECK(channel_type IN ('generic_webhook', 'email', 'feishu', 'wecom', 'dingtalk', 'slack', 'msteams', 'telegram', 'discord'));
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
            recovery_since BIGINT,
            last_observed_at BIGINT,
            last_value DOUBLE PRECISION,
            last_sample_at BIGINT,
            match_count BIGINT NOT NULL DEFAULT 0,
            PRIMARY KEY(rule_id, instance_id)
        );
        ALTER TABLE alert_evaluation_states
            ADD COLUMN IF NOT EXISTS recovery_since BIGINT;
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
        CREATE INDEX IF NOT EXISTS idx_alert_deliveries_lifecycle_order
            ON alert_deliveries(event_id, channel_id, status, created_at, id);
        ALTER TABLE alert_deliveries ADD COLUMN IF NOT EXISTS lease_token TEXT;
        WITH ranked_deliveries AS (
            SELECT id,ROW_NUMBER() OVER(
                PARTITION BY event_id,channel_id,kind
                ORDER BY
                    CASE status
                        WHEN 'succeeded' THEN 1
                        WHEN 'processing' THEN 2
                        WHEN 'pending' THEN 3
                        WHEN 'failed' THEN 4
                        ELSE 5
                    END,
                    created_at,id
            ) AS duplicate_rank
            FROM alert_deliveries
            WHERE event_id IS NOT NULL AND status <> 'suppressed'
        )
        UPDATE alert_deliveries d SET
            status='suppressed',suppression_reason='duplicate_delivery_migrated',
            next_attempt_at=NULL,lease_until=NULL,lease_token=NULL,
            completed_at=COALESCE(d.completed_at,d.updated_at)
        FROM ranked_deliveries ranked
        WHERE d.id=ranked.id AND ranked.duplicate_rank > 1;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_alert_deliveries_deliverable_lifecycle
            ON alert_deliveries(event_id, channel_id, kind)
            WHERE event_id IS NOT NULL AND status <> 'suppressed';
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
    if payload.target_instance_ids.len() > MAX_RULE_TARGET_INSTANCE_IDS {
        return Err(AppError::bad_request(format!(
            "目标节点数量不能超过 {MAX_RULE_TARGET_INSTANCE_IDS} 个"
        )));
    }
    if payload.channel_ids.len() > MAX_RULE_CHANNEL_IDS {
        return Err(AppError::bad_request(format!(
            "通知渠道数量不能超过 {MAX_RULE_CHANNEL_IDS} 个"
        )));
    }
    let mut normalized = payload.clone();
    normalized.name = trimmed(&payload.name, MAX_NAME_BYTES, "规则名称")?;
    if !matches!(
        normalized.metric.as_str(),
        "node_offline"
            | "cpu_percent"
            | "memory_percent"
            | "disk_percent"
            | "latency_ms"
            | "instance_expiring"
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
    if normalized.metric == "instance_expiring" && normalized.duration_seconds != 0 {
        return Err(AppError::bad_request("实例到期规则必须立即触发"));
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
        if normalized.metric == "instance_expiring" && threshold.fract() != 0.0 {
            return Err(AppError::bad_request("实例到期阈值必须是整数天"));
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
    let missing_instance_id = sqlx::query_scalar::<_, String>(
        r#"
        SELECT requested.id
        FROM UNNEST($1::TEXT[]) WITH ORDINALITY AS requested(id, position)
        LEFT JOIN instances i ON i.id=requested.id AND i.approved=1
        WHERE i.id IS NULL
        ORDER BY requested.position
        LIMIT 1
        "#,
    )
    .bind(&payload.target_instance_ids)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(instance_id) = missing_instance_id {
        return Err(AppError::bad_request(format!(
            "节点 {instance_id} 不存在或未批准"
        )));
    }

    let missing_channel_id = sqlx::query_scalar::<_, String>(
        r#"
        SELECT requested.id
        FROM UNNEST($1::TEXT[]) WITH ORDINALITY AS requested(id, position)
        LEFT JOIN alert_webhook_channels c
            ON c.id=requested.id AND c.deleted_at IS NULL
        WHERE c.id IS NULL
        ORDER BY requested.position
        LIMIT 1
        "#,
    )
    .bind(&payload.channel_ids)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(channel_id) = missing_channel_id {
        return Err(AppError::bad_request(format!(
            "通知渠道 {channel_id} 不存在"
        )));
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
        sqlx::query(
            r#"
            INSERT INTO alert_rule_targets(rule_id, instance_id)
            SELECT $1, requested.instance_id
            FROM UNNEST($2::TEXT[]) AS requested(instance_id)
            "#,
        )
        .bind(rule_id)
        .bind(&payload.target_instance_ids)
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query("DELETE FROM alert_rule_channels WHERE rule_id = $1")
        .bind(rule_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO alert_rule_channels(rule_id, channel_id)
        SELECT $1, requested.channel_id
        FROM UNNEST($2::TEXT[]) AS requested(channel_id)
        "#,
    )
    .bind(rule_id)
    .bind(&payload.channel_ids)
    .execute(&mut **tx)
    .await?;
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
    recovery_since: Option<i64>,
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
    has_active_offline_event
        && matches!(
            metric,
            "cpu_percent" | "memory_percent" | "disk_percent" | "latency_ms"
        )
}

fn expiration_observation(expires_at: Option<i64>, now: i64, threshold_days: f64) -> (f64, bool) {
    let Some(expires_at) = expires_at else {
        return (0.0, false);
    };
    let remaining_days = expires_at.saturating_sub(now) as f64 / 86_400.0;
    (remaining_days, remaining_days <= threshold_days)
}

fn connection_observation(online: bool) -> (f64, bool) {
    if online { (0.0, false) } else { (1.0, true) }
}

fn startup_grace_defers_offline(online: bool, started_at: i64, now: i64) -> bool {
    !online && now.saturating_sub(started_at) < STARTUP_RECONNECT_GRACE_SECONDS
}

fn recovery_confirmation(
    metric: &str,
    has_active_event: bool,
    recovery_since: Option<i64>,
    observed_at: i64,
) -> (Option<i64>, bool) {
    if metric != "node_offline" || !has_active_event {
        return (None, true);
    }
    let recovery_since = recovery_since.unwrap_or(observed_at);
    (
        Some(recovery_since),
        observed_at.saturating_sub(recovery_since) >= OFFLINE_RECOVERY_STABILITY_SECONDS,
    )
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
            'os', os, 'arch', arch, 'agent_version', agent_version,
            'expires_at', expires_at
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
    channel_type: &str,
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
        ON CONFLICT(event_id,channel_id,kind)
            WHERE event_id IS NOT NULL AND status <> 'suppressed'
            DO NOTHING
        "#,
    )
    .bind(id)
    .bind(&event.id)
    .bind(channel_id)
    .bind(kind)
    .bind(status)
    .bind(delivery_payload(event, kind, actor, note))
    .bind(json!({
        "id": channel_id,
        "name": channel_name,
        "channel_type": channel_type,
    }))
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
    let channels = sqlx::query_as::<_, (String, String, String)>(
        r#"
        SELECT c.id, c.name, c.channel_type
        FROM alert_event_channels ec
        JOIN alert_webhook_channels c ON c.id = ec.channel_id
        WHERE ec.event_id = $1 AND c.enabled = TRUE AND c.deleted_at IS NULL
        ORDER BY c.id
        FOR SHARE OF c
        "#,
    )
    .bind(&event.id)
    .fetch_all(&mut **tx)
    .await?;
    for (channel_id, channel_name, channel_type) in channels {
        if kind == "alert.firing" && suppression.is_some() {
            let already_recorded: bool = sqlx::query_scalar(
                r#"
                    SELECT EXISTS(
                        SELECT 1 FROM alert_deliveries
                        WHERE event_id=$1 AND channel_id=$2 AND kind='alert.firing'
                    )
                    "#,
            )
            .bind(&event.id)
            .bind(&channel_id)
            .fetch_one(&mut **tx)
            .await?;
            if already_recorded {
                continue;
            }
        }
        insert_delivery_tx(
            tx,
            event,
            &channel_id,
            &channel_name,
            &channel_type,
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
        r#"
        UPDATE alert_deliveries SET status='suppressed',
            suppression_reason='event_resolved_before_delivery',
            next_attempt_at=NULL,lease_until=NULL,lease_token=NULL,
            completed_at=$1,updated_at=$1
        WHERE event_id=$2 AND kind='alert.firing' AND status='pending'
        "#,
    )
    .bind(now)
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
            WHERE s.rule_id=r.id AND r.metric NOT IN ('node_offline', 'instance_expiring')
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
            WHERE s.rule_id=r.id AND r.metric NOT IN ('node_offline', 'instance_expiring')
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
            rule_id,instance_id,rule_version,pending_since,recovery_since,
            last_observed_at,last_value,last_sample_at,match_count
        ) VALUES($1,$2,$3,NULL,NULL,NULL,NULL,NULL,0)
        ON CONFLICT(rule_id,instance_id) DO UPDATE SET
            rule_version=EXCLUDED.rule_version,pending_since=NULL,recovery_since=NULL,
            last_observed_at=NULL,last_value=NULL,last_sample_at=NULL,match_count=0
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
        SELECT pending_since,recovery_since,last_observed_at,last_value,last_sample_at,match_count
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
        let (recovery_since, recovery_confirmed) = recovery_confirmation(
            &rule.metric,
            active.is_some(),
            evaluation.recovery_since,
            observed_at,
        );
        sqlx::query(
            r#"
            UPDATE alert_evaluation_states SET pending_since=NULL,recovery_since=$1,
                last_observed_at=$2,last_value=$3,last_sample_at=$4,match_count=0
            WHERE rule_id=$5 AND instance_id=$6
            "#,
        )
        .bind(if recovery_confirmed {
            None
        } else {
            recovery_since
        })
        .bind(observed_at)
        .bind(value)
        .bind(sample_at)
        .bind(&rule.id)
        .bind(instance_id)
        .execute(&mut *tx)
        .await?;
        if recovery_confirmed && let Some(event) = active {
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
            UPDATE alert_evaluation_states SET recovery_since=NULL,last_observed_at=$1,
                last_value=$2,last_sample_at=$3,match_count=$4
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
        UPDATE alert_evaluation_states SET pending_since=$1,recovery_since=NULL,
            last_observed_at=$2,last_value=$3,last_sample_at=$4,match_count=$5
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
        WHERE r.enabled=TRUE AND r.metric NOT IN ('node_offline', 'instance_expiring')
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

async fn observe_expiration_at(
    state: &AppState,
    instance_id: &str,
    observed_at: i64,
) -> AppResult<()> {
    let expires_at = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT expires_at FROM instances WHERE id=$1 AND approved=1 AND disabled=0",
    )
    .bind(instance_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(expires_at) = expires_at else {
        return Ok(());
    };
    let rules = sqlx::query_as::<_, RuleRow>(
        r#"
        SELECT DISTINCT r.* FROM alert_rules r
        LEFT JOIN alert_rule_targets t ON t.rule_id=r.id
        WHERE r.enabled=TRUE AND r.metric='instance_expiring'
          AND (r.scope='all' OR t.instance_id=$1)
        ORDER BY r.id
        "#,
    )
    .bind(instance_id)
    .fetch_all(&state.db)
    .await?;
    for rule in rules {
        let (value, abnormal) =
            expiration_observation(expires_at, observed_at, rule.threshold.unwrap_or_default());
        process_rule_observation(
            state,
            &rule,
            instance_id,
            RuleObservation::Threshold { value, abnormal },
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
            if let Err(error) = observe_expiration_at(&state, &instance_id, now).await {
                warn!(?error, %instance_id, "failed to evaluate instance expiration alert");
            }
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
    let total: i64 = sqlx::query_scalar(AssertSqlSafe(count_sql))
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
    let items = sqlx::query_as::<_, EventRow>(AssertSqlSafe(list_sql))
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
    validate_webhook_url_with_policy(value, DestinationPolicy::PublicOnly)
}

fn validate_webhook_url_with_policy(value: &str, policy: DestinationPolicy) -> AppResult<String> {
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
    if url
        .host_str()
        .and_then(parse_url_ip)
        .is_some_and(|address| !destination_is_allowed(address, policy))
    {
        return Err(AppError::bad_request(
            "Webhook URL 不能指向本机、内网或保留地址",
        ));
    }
    if url.host_str().is_some_and(is_obviously_local_hostname) {
        return Err(AppError::bad_request("Webhook URL 不能使用本机或本地域名"));
    }
    Ok(url.to_string())
}

fn parse_url_ip(host: &str) -> Option<IpAddr> {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

fn is_obviously_local_hostname(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host == "metadata.google.internal"
        || host == "metadata"
}

fn is_public_destination(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    let first = octets[0];
    let second = octets[1];
    let blocked = address.is_unspecified()
        || address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_multicast()
        || address.is_broadcast()
        || (first == 0)
        || (first == 100 && (64..=127).contains(&second))
        || (first == 192 && second == 0 && octets[2] == 0)
        || (first == 192 && second == 0 && octets[2] == 2)
        || (first == 192 && second == 88 && octets[2] == 99)
        || (first == 198 && (18..=19).contains(&second))
        || (first == 198 && second == 51 && octets[2] == 100)
        || (first == 203 && second == 0 && octets[2] == 113)
        || (first >= 240);
    !blocked
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let [first, second, ..] = address.segments();
    // Public IPv6 unicast is allocated from 2000::/3. Starting from this
    // whitelist excludes ULA, link/site-local, multicast, NAT64, and other
    // special ranges; the remaining exclusions are special-use subnets inside
    // the global allocation.
    (first & 0xe000) == 0x2000
        && !(first == 0x2001 && second & 0xfe00 == 0) // IETF assignments.
        && !(first == 0x2001 && second == 0x0db8) // Documentation.
        && first != 0x2002 // 6to4 embeds an arbitrary IPv4 destination.
        && !(first == 0x3fff && second & 0xf000 == 0) // Documentation.
}

#[derive(Clone, Copy)]
enum DestinationPolicy {
    PublicOnly,
    #[cfg(test)]
    AllowLoopback,
}

fn destination_is_allowed(address: IpAddr, policy: DestinationPolicy) -> bool {
    match policy {
        DestinationPolicy::PublicOnly => is_public_destination(address),
        #[cfg(test)]
        DestinationPolicy::AllowLoopback => is_public_destination(address) || address.is_loopback(),
    }
}

async fn public_socket_addresses(
    host: &str,
    port: u16,
    policy: DestinationPolicy,
) -> AppResult<Vec<SocketAddr>> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addresses = if let Ok(address) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(address, port)]
    } else {
        tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| AppError::new(StatusCode::BAD_GATEWAY, "通知目标域名解析失败"))?
            .collect()
    };
    let mut unique = Vec::new();
    for address in addresses {
        if !destination_is_allowed(address.ip(), policy) {
            return Err(AppError::new(
                StatusCode::BAD_GATEWAY,
                "通知目标解析到了本机、内网或保留地址",
            ));
        }
        if !unique.contains(&address) {
            unique.push(address);
        }
    }
    if unique.is_empty() {
        return Err(AppError::new(
            StatusCode::BAD_GATEWAY,
            "通知目标没有可用的公网地址",
        ));
    }
    Ok(unique)
}

async fn secure_http_client(
    url: &Url,
    timeout: Duration,
    policy: DestinationPolicy,
) -> AppResult<Client> {
    let host = url
        .host_str()
        .ok_or_else(|| AppError::bad_request("Webhook URL 缺少主机名"))?;
    let port = url
        .port()
        .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
    let addresses = public_socket_addresses(host, port, policy).await?;
    let mut builder = Client::builder()
        .redirect(Policy::none())
        .no_proxy()
        .timeout(timeout);
    // Literal IP URLs do not perform DNS resolution. Domain destinations are
    // pinned to the complete address set that was validated above.
    if parse_url_ip(host).is_none() {
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    builder.build().map_err(|error| {
        AppError::new(
            StatusCode::BAD_GATEWAY,
            format!("通知 HTTP 客户端初始化失败: {error}"),
        )
    })
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

fn validate_channel_type(value: &str) -> AppResult<String> {
    let value = value.trim();
    if matches!(
        value,
        "generic_webhook"
            | "email"
            | "feishu"
            | "wecom"
            | "dingtalk"
            | "slack"
            | "msteams"
            | "telegram"
            | "discord"
    ) {
        Ok(value.to_string())
    } else {
        Err(AppError::bad_request("不支持的通知渠道类型"))
    }
}

fn channel_type_for_create(value: Option<&str>) -> AppResult<String> {
    validate_channel_type(value.unwrap_or("generic_webhook"))
}

fn channel_type_for_update(value: Option<&str>, existing: &str) -> AppResult<String> {
    let requested = validate_channel_type(value.unwrap_or(existing))?;
    if requested != existing {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "通知渠道类型创建后不可修改",
        ));
    }
    Ok(requested)
}

fn normalized_optional(
    value: Option<&str>,
    max_bytes: usize,
    label: &str,
) -> AppResult<Option<String>> {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    if value.is_some_and(|value| value.len() > max_bytes || value.contains(['\r', '\n'])) {
        return Err(AppError::bad_request(format!("{label}格式无效或过长")));
    }
    Ok(value.map(str::to_string))
}

fn normalized_password(value: Option<&str>) -> AppResult<Option<String>> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    if value.len() > MAX_EMAIL_FIELD_BYTES {
        return Err(AppError::bad_request("SMTP 密码过长"));
    }
    Ok(Some(value.to_string()))
}

fn validate_email_config(mut config: EmailChannelConfig) -> AppResult<EmailChannelConfig> {
    config.smtp_host = config.smtp_host.trim().to_string();
    if config.smtp_host.is_empty()
        || config.smtp_host.len() > 255
        || config.smtp_host.chars().any(char::is_whitespace)
    {
        return Err(AppError::bad_request("SMTP 主机格式无效"));
    }
    if config.smtp_port == 0 {
        return Err(AppError::bad_request("SMTP 端口必须大于 0"));
    }
    if config
        .smtp_host
        .parse::<IpAddr>()
        .ok()
        .is_some_and(|address| !is_public_destination(address))
        || is_obviously_local_hostname(&config.smtp_host)
    {
        return Err(AppError::bad_request(
            "SMTP 主机不能指向本机、内网或保留地址",
        ));
    }
    if !matches!(config.security.as_str(), "starttls" | "smtps") {
        return Err(AppError::bad_request(
            "SMTP security 必须是 starttls 或 smtps",
        ));
    }
    config.username = normalized_optional(
        config.username.as_deref(),
        MAX_EMAIL_FIELD_BYTES,
        "SMTP 用户名",
    )?;
    config.password = normalized_password(config.password.as_deref())?;
    if config.username.is_some() != config.password.is_some() {
        return Err(AppError::bad_request(
            "SMTP 用户名和密码必须同时配置或同时清除",
        ));
    }
    config.from_address = config.from_address.trim().to_string();
    config
        .from_address
        .parse::<Address>()
        .map_err(|_| AppError::bad_request("发件人地址格式无效"))?;
    config.from_name =
        normalized_optional(config.from_name.as_deref(), MAX_NAME_BYTES, "发件人名称")?;
    if config.recipients.is_empty() || config.recipients.len() > MAX_EMAIL_RECIPIENTS {
        return Err(AppError::bad_request(format!(
            "邮件收件人数量必须在 1 到 {MAX_EMAIL_RECIPIENTS} 之间"
        )));
    }
    let mut recipients = Vec::with_capacity(config.recipients.len());
    for recipient in config.recipients {
        let recipient = recipient.trim().to_string();
        recipient
            .parse::<Address>()
            .map_err(|_| AppError::bad_request(format!("收件人地址无效: {recipient}")))?;
        if !recipients
            .iter()
            .any(|known: &String| known.eq_ignore_ascii_case(&recipient))
        {
            recipients.push(recipient);
        }
    }
    config.recipients = recipients;
    Ok(config)
}

fn email_request_config(
    payload: &ChannelRequest,
    previous: Option<&EmailChannelConfig>,
) -> AppResult<EmailChannelConfig> {
    let required = |value: Option<&str>, previous: Option<&str>, label: &str| {
        value
            .or(previous)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| AppError::bad_request(format!("邮件渠道必须提供{label}")))
    };
    let username = normalized_optional(
        payload
            .username
            .as_deref()
            .or_else(|| previous.and_then(|config| config.username.as_deref())),
        MAX_EMAIL_FIELD_BYTES,
        "SMTP 用户名",
    )?;
    let password = if payload.clear_password {
        None
    } else {
        normalized_password(
            payload
                .password
                .as_deref()
                .or_else(|| previous.and_then(|config| config.password.as_deref())),
        )?
    };
    let username = if payload.clear_password {
        None
    } else {
        username
    };
    validate_email_config(EmailChannelConfig {
        smtp_host: required(
            payload.smtp_host.as_deref(),
            previous.map(|config| config.smtp_host.as_str()),
            " SMTP 主机",
        )?,
        smtp_port: payload
            .smtp_port
            .or_else(|| previous.map(|config| config.smtp_port))
            .ok_or_else(|| AppError::bad_request("邮件渠道必须提供 SMTP 端口"))?,
        security: required(
            payload.security.as_deref(),
            previous.map(|config| config.security.as_str()),
            " security",
        )?,
        username,
        password,
        from_address: required(
            payload.from_address.as_deref(),
            previous.map(|config| config.from_address.as_str()),
            "发件人地址",
        )?,
        from_name: payload
            .from_name
            .clone()
            .or_else(|| previous.and_then(|config| config.from_name.clone())),
        recipients: payload
            .recipients
            .clone()
            .or_else(|| previous.map(|config| config.recipients.clone()))
            .ok_or_else(|| AppError::bad_request("邮件渠道必须提供收件人"))?,
    })
}

fn email_fields_present(payload: &ChannelRequest) -> bool {
    payload.smtp_host.is_some()
        || payload.smtp_port.is_some()
        || payload.security.is_some()
        || payload.username.is_some()
        || payload.password.is_some()
        || payload.clear_password
        || payload.from_address.is_some()
        || payload.from_name.is_some()
        || payload.recipients.is_some()
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

fn decrypt_email_config(state: &AppState, row: &ChannelRow) -> AppResult<EmailChannelConfig> {
    let ciphertext = row
        .config_ciphertext
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("email channel configuration is missing"))?;
    let plaintext = decrypt_string(state, ciphertext)?;
    let config = serde_json::from_str::<EmailChannelConfig>(&plaintext)
        .map_err(|error| anyhow::anyhow!("invalid encrypted email channel config: {error}"))?;
    validate_email_config(config)
}

fn decrypt_telegram_config(state: &AppState, row: &ChannelRow) -> AppResult<TelegramChannelConfig> {
    let ciphertext = row
        .config_ciphertext
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Telegram channel configuration is missing"))?;
    let plaintext = decrypt_string(state, ciphertext)?;
    let config = serde_json::from_str::<TelegramChannelConfig>(&plaintext)
        .map_err(|error| anyhow::anyhow!("invalid encrypted Telegram channel config: {error}"))?;
    let chat_id = normalized_chat_id(Some(&config.chat_id))?
        .ok_or_else(|| anyhow::anyhow!("Telegram chat_id is missing"))?;
    Ok(TelegramChannelConfig { chat_id })
}

fn channel_response(state: &AppState, row: ChannelRow) -> AppResult<ChannelResponse> {
    let email = if row.channel_type == "email" {
        Some(decrypt_email_config(state, &row)?)
    } else {
        None
    };
    let telegram = if row.channel_type == "telegram" {
        Some(decrypt_telegram_config(state, &row)?)
    } else {
        None
    };
    let (masked_url, header_names) = if let Some(config) = email.as_ref() {
        (
            format!(
                "{}://{}:{}",
                config.security, config.smtp_host, config.smtp_port
            ),
            Vec::new(),
        )
    } else {
        let url = decrypt_string(state, &row.url_ciphertext)?;
        let headers = if row.channel_type == "generic_webhook" {
            decrypt_headers(state, &row.headers_ciphertext)?
                .into_keys()
                .collect()
        } else {
            Vec::new()
        };
        (mask_webhook_url(&url), headers)
    };
    Ok(ChannelResponse {
        id: row.id,
        name: row.name,
        channel_type: row.channel_type,
        masked_url,
        header_names,
        has_secret: row.secret_ciphertext.is_some(),
        smtp_host: email.as_ref().map(|config| config.smtp_host.clone()),
        smtp_port: email.as_ref().map(|config| config.smtp_port),
        security: email.as_ref().map(|config| config.security.clone()),
        username: email.as_ref().and_then(|config| config.username.clone()),
        from_address: email.as_ref().map(|config| config.from_address.clone()),
        from_name: email.as_ref().and_then(|config| config.from_name.clone()),
        recipients: email.as_ref().map(|config| config.recipients.clone()),
        has_password: email
            .as_ref()
            .is_some_and(|config| config.password.is_some()),
        chat_id: telegram.as_ref().map(|config| config.chat_id.clone()),
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
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "通知渠道不存在"))
}

async fn list_channels(
    State(state): State<AppState>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<ChannelResponse>>> {
    require_admin(&state, &headers).await?;
    let (page, page_size, offset) = normalize_page(query.page, query.page_size);
    let legacy = is_legacy_channel_uri(&uri);
    let total: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM alert_webhook_channels
        WHERE deleted_at IS NULL
          AND ($1::BOOLEAN = FALSE OR channel_type = 'generic_webhook')
        "#,
    )
    .bind(legacy)
    .fetch_one(&state.db)
    .await?;
    let rows = sqlx::query_as::<_, ChannelRow>(
        r#"
        SELECT * FROM alert_webhook_channels
        WHERE deleted_at IS NULL
          AND ($1::BOOLEAN = FALSE OR channel_type = 'generic_webhook')
        ORDER BY updated_at DESC,id LIMIT $2 OFFSET $3
        "#,
    )
    .bind(legacy)
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

async fn get_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> AppResult<Json<ChannelResponse>> {
    require_admin(&state, &headers).await?;
    let channel = load_channel(&state.db, &id).await?;
    ensure_channel_uri_supports(&uri, &channel.channel_type)?;
    Ok(Json(channel_response(&state, channel)?))
}

fn encrypted_json<T: Serialize>(state: &AppState, value: &T) -> AppResult<String> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| anyhow::anyhow!("failed to serialize channel config: {error}"))?;
    Ok(state.auth_cipher.encrypt(&encoded)?)
}

fn validate_specialized_http_fields(payload: &ChannelRequest, allow_secret: bool) -> AppResult<()> {
    if payload
        .headers
        .as_ref()
        .is_some_and(|headers| !headers.is_empty())
    {
        return Err(AppError::bad_request("平台机器人渠道不支持自定义请求头"));
    }
    if !allow_secret
        && payload
            .secret
            .as_deref()
            .is_some_and(|secret| !secret.trim().is_empty())
    {
        return Err(AppError::bad_request("该渠道类型不支持签名密钥"));
    }
    Ok(())
}

fn validate_platform_fields(payload: &ChannelRequest, allow_secret: bool) -> AppResult<()> {
    if email_fields_present(payload) {
        return Err(AppError::bad_request("平台机器人渠道不能设置 SMTP 字段"));
    }
    validate_specialized_http_fields(payload, allow_secret)?;
    if payload
        .chat_id
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return Err(AppError::bad_request("该渠道类型不支持 Telegram Chat ID"));
    }
    Ok(())
}

fn validate_email_only_fields(payload: &ChannelRequest) -> AppResult<()> {
    if payload
        .url
        .as_deref()
        .is_some_and(|url| !url.trim().is_empty())
        || payload
            .secret
            .as_deref()
            .is_some_and(|secret| !secret.trim().is_empty())
        || payload
            .headers
            .as_ref()
            .is_some_and(|headers| !headers.is_empty())
        || payload
            .chat_id
            .as_deref()
            .is_some_and(|chat_id| !chat_id.trim().is_empty())
    {
        return Err(AppError::bad_request(
            "邮件渠道不能设置 Webhook URL、签名密钥或自定义请求头",
        ));
    }
    Ok(())
}

fn normalized_secret(value: Option<&str>) -> AppResult<Option<&str>> {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    if value.is_some_and(|value| value.len() > MAX_HEADER_VALUE_BYTES) {
        return Err(AppError::bad_request("渠道签名密钥过长"));
    }
    Ok(value)
}

fn normalized_chat_id(value: Option<&str>) -> AppResult<Option<String>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.len() > MAX_TELEGRAM_CHAT_ID_BYTES
        || value.contains(['\r', '\n'])
        || value.chars().any(char::is_control)
    {
        return Err(AppError::bad_request("Telegram Chat ID 格式无效或过长"));
    }
    Ok(Some(value.to_string()))
}

fn is_legacy_channel_uri(uri: &axum::http::Uri) -> bool {
    uri.path()
        .split('/')
        .any(|segment| segment == "webhook-channels")
}

fn ensure_channel_uri_supports(uri: &axum::http::Uri, channel_type: &str) -> AppResult<()> {
    if is_legacy_channel_uri(uri) && channel_type != "generic_webhook" {
        return Err(AppError::bad_request(
            "旧版 Webhook 接口仅支持 generic_webhook 渠道",
        ));
    }
    Ok(())
}

async fn create_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Json(payload): Json<ChannelRequest>,
) -> AppResult<(StatusCode, Json<ChannelResponse>)> {
    let admin = require_admin(&state, &headers).await?;
    let name = trimmed(&payload.name, MAX_NAME_BYTES, "通知渠道名称")?;
    let channel_type = channel_type_for_create(payload.channel_type.as_deref())?;
    ensure_channel_uri_supports(&uri, &channel_type)?;
    let empty_headers = BTreeMap::new();
    let (url, secret, custom_headers, config_ciphertext) =
        match channel_type.as_str() {
            "generic_webhook" => {
                if email_fields_present(&payload) {
                    return Err(AppError::bad_request("Webhook 渠道不能设置 SMTP 字段"));
                }
                if payload
                    .chat_id
                    .as_deref()
                    .is_some_and(|chat_id| !chat_id.trim().is_empty())
                {
                    return Err(AppError::bad_request(
                        "Webhook 渠道不能设置 Telegram Chat ID",
                    ));
                }
                let url = validate_webhook_url(
                    payload
                        .url
                        .as_deref()
                        .ok_or_else(|| AppError::bad_request("创建 Webhook 时必须提供 URL"))?,
                )?;
                let headers =
                    validate_webhook_headers(payload.headers.as_ref().unwrap_or(&empty_headers))?;
                let secret = if payload.clear_secret {
                    None
                } else {
                    normalized_secret(payload.secret.as_deref())?
                };
                (url, secret, headers, None)
            }
            "feishu" => {
                validate_platform_fields(&payload, true)?;
                let url = validate_webhook_url(
                    payload
                        .url
                        .as_deref()
                        .ok_or_else(|| AppError::bad_request("创建飞书渠道时必须提供 URL"))?,
                )?;
                let secret = if payload.clear_secret {
                    None
                } else {
                    normalized_secret(payload.secret.as_deref())?
                };
                (url, secret, BTreeMap::new(), None)
            }
            "wecom" => {
                validate_platform_fields(&payload, false)?;
                let url = validate_webhook_url(
                    payload
                        .url
                        .as_deref()
                        .ok_or_else(|| AppError::bad_request("创建企业微信渠道时必须提供 URL"))?,
                )?;
                (url, None, BTreeMap::new(), None)
            }
            "dingtalk" => {
                validate_platform_fields(&payload, true)?;
                let url = validate_webhook_url(
                    payload
                        .url
                        .as_deref()
                        .ok_or_else(|| AppError::bad_request("创建钉钉渠道时必须提供 URL"))?,
                )?;
                let secret = if payload.clear_secret {
                    None
                } else {
                    normalized_secret(payload.secret.as_deref())?
                };
                (url, secret, BTreeMap::new(), None)
            }
            "slack" => {
                validate_platform_fields(&payload, false)?;
                let url = validate_webhook_url(
                    payload
                        .url
                        .as_deref()
                        .ok_or_else(|| AppError::bad_request("创建 Slack 渠道时必须提供 URL"))?,
                )?;
                (url, None, BTreeMap::new(), None)
            }
            "msteams" => {
                validate_platform_fields(&payload, false)?;
                let url = validate_webhook_url(payload.url.as_deref().ok_or_else(|| {
                    AppError::bad_request("创建 Microsoft Teams 渠道时必须提供 URL")
                })?)?;
                (url, None, BTreeMap::new(), None)
            }
            "discord" => {
                validate_platform_fields(&payload, false)?;
                let url =
                    validate_webhook_url(payload.url.as_deref().ok_or_else(|| {
                        AppError::bad_request("创建 Discord 渠道时必须提供 URL")
                    })?)?;
                (url, None, BTreeMap::new(), None)
            }
            "telegram" => {
                if email_fields_present(&payload) {
                    return Err(AppError::bad_request("Telegram 渠道不能设置 SMTP 字段"));
                }
                validate_specialized_http_fields(&payload, false)?;
                let url =
                    validate_webhook_url(payload.url.as_deref().ok_or_else(|| {
                        AppError::bad_request("创建 Telegram 渠道时必须提供 URL")
                    })?)?;
                let chat_id = normalized_chat_id(payload.chat_id.as_deref())?
                    .ok_or_else(|| AppError::bad_request("创建 Telegram 渠道时必须提供 Chat ID"))?;
                (
                    url,
                    None,
                    BTreeMap::new(),
                    Some(encrypted_json(&state, &TelegramChannelConfig { chat_id })?),
                )
            }
            "email" => {
                validate_email_only_fields(&payload)?;
                let config = email_request_config(&payload, None)?;
                (
                    String::new(),
                    None,
                    BTreeMap::new(),
                    Some(encrypted_json(&state, &config)?),
                )
            }
            _ => unreachable!("validated channel type"),
        };
    let id = Uuid::new_v4().to_string();
    let now = now_ts();
    let mut tx = state.db.begin().await?;
    sqlx::query(
        r#"
        INSERT INTO alert_webhook_channels(
            id,name,channel_type,url_ciphertext,secret_ciphertext,headers_ciphertext,
            config_ciphertext,enabled,created_at,updated_at
        ) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$9)
        "#,
    )
    .bind(&id)
    .bind(name)
    .bind(&channel_type)
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
    .bind(config_ciphertext)
    .bind(payload.enabled)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    audit_action_tx(&mut tx, &admin, "alert_webhook_create", &id, "创建通知渠道").await?;
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
    OriginalUri(uri): OriginalUri,
    Json(payload): Json<ChannelRequest>,
) -> AppResult<Json<ChannelResponse>> {
    let admin = require_admin(&state, &headers).await?;
    let old = load_channel(&state.db, &id).await?;
    ensure_channel_uri_supports(&uri, &old.channel_type)?;
    channel_type_for_update(payload.channel_type.as_deref(), &old.channel_type)?;
    let name = trimmed(&payload.name, MAX_NAME_BYTES, "通知渠道名称")?;
    let (url_ciphertext, secret_ciphertext, headers_ciphertext, config_ciphertext) =
        match old.channel_type.as_str() {
            "generic_webhook" => {
                if email_fields_present(&payload) {
                    return Err(AppError::bad_request("Webhook 渠道不能设置 SMTP 字段"));
                }
                if payload
                    .chat_id
                    .as_deref()
                    .is_some_and(|chat_id| !chat_id.trim().is_empty())
                {
                    return Err(AppError::bad_request(
                        "Webhook 渠道不能设置 Telegram Chat ID",
                    ));
                }
                let url = payload
                    .url
                    .as_deref()
                    .map(validate_webhook_url)
                    .transpose()?;
                let headers = payload
                    .headers
                    .as_ref()
                    .map(validate_webhook_headers)
                    .transpose()?;
                let secret = normalized_secret(payload.secret.as_deref())?;
                (
                    url.map(|url| state.auth_cipher.encrypt(url.as_bytes()))
                        .transpose()?
                        .unwrap_or(old.url_ciphertext),
                    if payload.clear_secret {
                        None
                    } else {
                        secret
                            .map(|secret| state.auth_cipher.encrypt(secret.as_bytes()))
                            .transpose()?
                            .or(old.secret_ciphertext)
                    },
                    headers
                        .map(|headers| encrypted_json(&state, &headers))
                        .transpose()?
                        .unwrap_or(old.headers_ciphertext),
                    None,
                )
            }
            "feishu" => {
                validate_platform_fields(&payload, true)?;
                let url = payload
                    .url
                    .as_deref()
                    .map(validate_webhook_url)
                    .transpose()?;
                let secret = normalized_secret(payload.secret.as_deref())?;
                (
                    url.map(|url| state.auth_cipher.encrypt(url.as_bytes()))
                        .transpose()?
                        .unwrap_or(old.url_ciphertext),
                    if payload.clear_secret {
                        None
                    } else {
                        secret
                            .map(|secret| state.auth_cipher.encrypt(secret.as_bytes()))
                            .transpose()?
                            .or(old.secret_ciphertext)
                    },
                    encrypted_json(&state, &BTreeMap::<String, String>::new())?,
                    None,
                )
            }
            "wecom" | "slack" | "msteams" | "discord" => {
                validate_platform_fields(&payload, false)?;
                let url = payload
                    .url
                    .as_deref()
                    .map(validate_webhook_url)
                    .transpose()?;
                (
                    url.map(|url| state.auth_cipher.encrypt(url.as_bytes()))
                        .transpose()?
                        .unwrap_or(old.url_ciphertext),
                    None,
                    encrypted_json(&state, &BTreeMap::<String, String>::new())?,
                    None,
                )
            }
            "dingtalk" => {
                validate_platform_fields(&payload, true)?;
                let url = payload
                    .url
                    .as_deref()
                    .map(validate_webhook_url)
                    .transpose()?;
                let secret = normalized_secret(payload.secret.as_deref())?;
                (
                    url.map(|url| state.auth_cipher.encrypt(url.as_bytes()))
                        .transpose()?
                        .unwrap_or(old.url_ciphertext),
                    if payload.clear_secret {
                        None
                    } else {
                        secret
                            .map(|secret| state.auth_cipher.encrypt(secret.as_bytes()))
                            .transpose()?
                            .or(old.secret_ciphertext)
                    },
                    encrypted_json(&state, &BTreeMap::<String, String>::new())?,
                    None,
                )
            }
            "telegram" => {
                if email_fields_present(&payload) {
                    return Err(AppError::bad_request("Telegram 渠道不能设置 SMTP 字段"));
                }
                validate_specialized_http_fields(&payload, false)?;
                let url = payload
                    .url
                    .as_deref()
                    .map(validate_webhook_url)
                    .transpose()?;
                let old_config = decrypt_telegram_config(&state, &old)?;
                let chat_id = normalized_chat_id(payload.chat_id.as_deref())?
                    .or(Some(old_config.chat_id))
                    .ok_or_else(|| AppError::bad_request("Telegram 渠道必须提供 Chat ID"))?;
                (
                    url.map(|url| state.auth_cipher.encrypt(url.as_bytes()))
                        .transpose()?
                        .unwrap_or(old.url_ciphertext),
                    None,
                    encrypted_json(&state, &BTreeMap::<String, String>::new())?,
                    Some(encrypted_json(&state, &TelegramChannelConfig { chat_id })?),
                )
            }
            "email" => {
                validate_email_only_fields(&payload)?;
                let old_config = decrypt_email_config(&state, &old)?;
                let config = if email_fields_present(&payload) {
                    email_request_config(&payload, Some(&old_config))?
                } else {
                    old_config
                };
                (
                    old.url_ciphertext,
                    None,
                    old.headers_ciphertext,
                    Some(encrypted_json(&state, &config)?),
                )
            }
            _ => return Err(AppError::bad_request("通知渠道类型无效")),
        };
    let mut tx = state.db.begin().await?;
    let result = sqlx::query(
        r#"
        UPDATE alert_webhook_channels SET name=$1,url_ciphertext=$2,
            secret_ciphertext=$3,headers_ciphertext=$4,config_ciphertext=$5,
            enabled=$6,updated_at=$7
        WHERE id=$8 AND deleted_at IS NULL
        "#,
    )
    .bind(name)
    .bind(url_ciphertext)
    .bind(secret_ciphertext)
    .bind(headers_ciphertext)
    .bind(config_ciphertext)
    .bind(payload.enabled)
    .bind(now_ts())
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::new(StatusCode::NOT_FOUND, "通知渠道不存在"));
    }
    if !payload.enabled {
        sqlx::query(
            r#"
            UPDATE alert_deliveries SET status='failed',last_error='channel_disabled',
                next_attempt_at=NULL,lease_until=NULL,lease_token=NULL,
                completed_at=$1,updated_at=$1
            WHERE channel_id=$2 AND status='pending'
            "#,
        )
        .bind(now_ts())
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    }
    audit_action_tx(&mut tx, &admin, "alert_webhook_update", &id, "更新通知渠道").await?;
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
    OriginalUri(uri): OriginalUri,
) -> AppResult<StatusCode> {
    let admin = require_admin(&state, &headers).await?;
    let channel = load_channel(&state.db, &id).await?;
    ensure_channel_uri_supports(&uri, &channel.channel_type)?;
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
        return Err(AppError::new(StatusCode::NOT_FOUND, "通知渠道不存在"));
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
        WHERE channel_id=$2 AND status='pending'
        "#,
    )
    .bind(now)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    audit_action_tx(&mut tx, &admin, "alert_webhook_delete", &id, "删除通知渠道").await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn test_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(StatusCode, Json<DeliveryRow>)> {
    let admin = require_admin(&state, &headers).await?;
    let mut tx = state.db.begin().await?;
    let channel = sqlx::query_as::<_, ChannelRow>(
        "SELECT * FROM alert_webhook_channels WHERE id=$1 AND deleted_at IS NULL FOR SHARE",
    )
    .bind(&id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "通知渠道不存在"))?;
    ensure_channel_uri_supports(&uri, &channel.channel_type)?;
    if !channel.enabled {
        return Err(AppError::new(StatusCode::CONFLICT, "通知渠道已停用"));
    }
    let delivery_id = Uuid::new_v4().to_string();
    let now = now_ts();
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
    .bind(json!({
        "id": id,
        "name": channel.name,
        "channel_type": channel.channel_type,
    }))
    .bind(now)
    .execute(&mut *tx)
    .await?;
    audit_action_tx(&mut tx, &admin, "alert_webhook_test", &id, "测试通知渠道").await?;
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
    let total: i64 = sqlx::query_scalar(AssertSqlSafe(format!("SELECT COUNT(*) {filter}")))
        .bind(query.status.as_deref())
        .bind(query.kind.as_deref())
        .bind(query.channel_id.as_deref())
        .bind(query.event_id.as_deref())
        .fetch_one(&state.db)
        .await?;
    let items = sqlx::query_as::<_, DeliveryRow>(AssertSqlSafe(format!(
        "SELECT d.* {filter} ORDER BY d.created_at DESC,d.id LIMIT $5 OFFSET $6"
    )))
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
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "通知投递不存在"))?;
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
    audit_action_tx(&mut tx, &admin, "alert_delivery_retry", &id, "重试通知投递").await?;
    tx.commit().await?;
    Ok(Json(delivery_detail(&state.db, &id).await?))
}

async fn reset_failed_delivery_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    now: i64,
) -> AppResult<()> {
    let delivery = sqlx::query_as::<_, (String, String, Option<String>, String)>(
        "SELECT status,channel_id,event_id,kind FROM alert_deliveries WHERE id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "通知投递不存在"))?;
    if delivery.0 != "failed" {
        return Err(AppError::new(StatusCode::CONFLICT, "只有失败投递可以重试"));
    }
    if let Some(event_id) = delivery.2.as_deref() {
        let event_status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM alert_events WHERE id=$1 FOR SHARE",
        )
        .bind(event_id)
        .fetch_optional(&mut **tx)
        .await?;
        if lifecycle_has_advanced(&delivery.3, event_status.as_deref()) {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "事件已进入后续生命周期，无法重试旧投递",
            ));
        }
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
                "通知渠道已删除，无法重试",
            ));
        }
        Some((false, None)) => {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "通知渠道已停用，无法重试",
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

fn lifecycle_has_advanced(delivery_kind: &str, event_status: Option<&str>) -> bool {
    matches!(
        (delivery_kind, event_status),
        ("alert.firing", Some("acknowledged" | "resolved"))
            | ("alert.acknowledged", Some("resolved"))
    )
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
            SELECT queued.id FROM alert_deliveries queued
            JOIN alert_webhook_channels c ON c.id=queued.channel_id
            WHERE (
                (queued.status='pending' AND COALESCE(queued.next_attempt_at,0) <= $1)
                OR (queued.status='processing' AND COALESCE(queued.lease_until,0) <= $1)
            )
              AND COALESCE(c.delivery_cooldown_until,0) <= $1
              AND NOT EXISTS (
                SELECT 1 FROM alert_deliveries in_flight
                WHERE in_flight.channel_id=queued.channel_id
                  AND in_flight.status='processing'
                  AND COALESCE(in_flight.lease_until,0) > $1
                  AND in_flight.id <> queued.id
              )
              AND NOT EXISTS (
                SELECT 1 FROM alert_deliveries earlier
                WHERE queued.event_id IS NOT NULL
                  AND earlier.event_id=queued.event_id
                  AND earlier.channel_id=queued.channel_id
                  AND earlier.status IN ('pending','processing')
                  AND earlier.id <> queued.id
                  AND ROW(
                    earlier.created_at,
                    CASE earlier.kind
                      WHEN 'alert.firing' THEN 1
                      WHEN 'alert.acknowledged' THEN 2
                      WHEN 'alert.resolved' THEN 3
                      ELSE 4
                    END,
                    earlier.id
                  ) < ROW(
                    queued.created_at,
                    CASE queued.kind
                      WHEN 'alert.firing' THEN 1
                      WHEN 'alert.acknowledged' THEN 2
                      WHEN 'alert.resolved' THEN 3
                      ELSE 4
                    END,
                    queued.id
                  )
            )
            ORDER BY COALESCE(queued.next_attempt_at,queued.created_at),queued.created_at,queued.id
            FOR UPDATE OF queued,c SKIP LOCKED
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

async fn release_delivery_claim(
    db: &PgPool,
    delivery: &DeliveryRow,
    retry_at: i64,
) -> AppResult<()> {
    let Some(lease_token) = delivery.lease_token.as_deref() else {
        return Ok(());
    };
    sqlx::query(
        r#"
        UPDATE alert_deliveries SET status='pending',next_attempt_at=$1,
            lease_until=NULL,lease_token=NULL,updated_at=$1
        WHERE id=$2 AND status='processing' AND lease_token=$3
        "#,
    )
    .bind(retry_at)
    .bind(&delivery.id)
    .bind(lease_token)
    .execute(db)
    .await?;
    Ok(())
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
    let Some((rule_id, instance_id, metric, event_status)) =
        sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT rule_id,instance_id,metric,status FROM alert_events WHERE id=$1",
        )
        .bind(event_id)
        .fetch_optional(db)
        .await?
    else {
        return Ok(None);
    };
    if event_status == "resolved" {
        return Ok(Some("event_resolved_before_delivery".to_string()));
    }
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

fn payload_string(payload: &Value, section: &str, field: &str) -> Option<String> {
    let value = payload.get(section)?.get(field)?;
    match value {
        Value::String(value) if !value.is_empty() => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn lifecycle_label(kind: &str) -> &str {
    match kind {
        "alert.firing" => "告警触发",
        "alert.acknowledged" => "告警已确认",
        "alert.resolved" => "告警已恢复",
        "webhook.test" => "通知渠道测试",
        _ => "告警通知",
    }
}

fn metric_label(metric: &str) -> &str {
    match metric {
        "node_offline" => "节点离线",
        "cpu_percent" => "CPU 使用率",
        "memory_percent" => "内存使用率",
        "disk_percent" => "磁盘使用率",
        "latency_ms" => "网络延迟",
        "instance_expiring" => "实例即将到期",
        _ => metric,
    }
}

fn format_expiration_days(value: f64) -> String {
    if value < 0.0 {
        format!("已到期 {:.1} 天", value.abs())
    } else {
        format!("剩余 {value:.1} 天")
    }
}

fn notification_metric_values(payload: &Value, metric: &str) -> (String, String) {
    if metric != "instance_expiring" {
        return (
            payload_string(payload, "event", "current_value").unwrap_or_else(|| "未知".into()),
            payload_string(payload, "event", "threshold").unwrap_or_else(|| "不适用".into()),
        );
    }
    let current = payload
        .get("event")
        .and_then(|event| event.get("current_value"))
        .and_then(Value::as_f64)
        .map(format_expiration_days)
        .unwrap_or_else(|| "未知".to_string());
    let threshold = payload
        .get("event")
        .and_then(|event| event.get("threshold"))
        .and_then(Value::as_f64)
        .map(|days| format!("剩余时间 <= {days:.0} 天"))
        .unwrap_or_else(|| "不适用".to_string());
    (current, threshold)
}

fn single_line(value: &str, max_bytes: usize) -> String {
    let normalized = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    crate::audit::truncate(normalized.trim(), max_bytes)
}

fn notification_text(payload: &Value) -> String {
    let kind = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("alert.notification");
    if kind == "webhook.test" {
        return "operationMonitoring 通知渠道测试\n测试投递已由管理员发起。".to_string();
    }

    let label = lifecycle_label(kind);
    let node = payload_string(payload, "node", "name")
        .or_else(|| payload_string(payload, "node", "hostname"))
        .or_else(|| payload_string(payload, "event", "instance_id"))
        .unwrap_or_else(|| "未知节点".to_string());
    let rule = payload_string(payload, "rule", "name").unwrap_or_else(|| "未知规则".to_string());
    let metric = payload_string(payload, "event", "metric").unwrap_or_else(|| "unknown".into());
    let severity = payload_string(payload, "event", "severity").unwrap_or_else(|| "unknown".into());
    let (current, threshold) = notification_metric_values(payload, &metric);

    let mut lines = vec![
        format!("operationMonitoring {label}"),
        format!("节点: {node}"),
        format!("规则: {rule}"),
        format!("级别: {severity}"),
        format!("指标: {}", metric_label(&metric)),
        format!("当前值: {current}"),
        format!("阈值: {threshold}"),
    ];
    if let Some(actor) = payload
        .get("actor")
        .and_then(|actor| actor.as_str().or_else(|| actor.get("username")?.as_str()))
    {
        lines.push(format!("操作者: {actor}"));
    }
    if let Some(note) = payload
        .get("note")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        lines.push(format!("备注: {note}"));
    }
    if let Some(reason) = payload_string(payload, "event", "resolution_reason") {
        lines.push(format!("恢复原因: {reason}"));
    }
    crate::audit::truncate(&lines.join("\n"), MAX_NOTIFICATION_TEXT_BYTES)
}

fn email_notification_content(payload: &Value) -> (String, String) {
    let kind = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("alert.notification");
    if kind == "webhook.test" {
        return (
            "[operationMonitoring] 通知渠道测试".to_string(),
            notification_text(payload),
        );
    }
    let severity = payload_string(payload, "event", "severity").unwrap_or_else(|| "unknown".into());
    let node = payload_string(payload, "node", "name")
        .or_else(|| payload_string(payload, "node", "hostname"))
        .or_else(|| payload_string(payload, "event", "instance_id"))
        .unwrap_or_else(|| "未知节点".to_string());
    let rule = payload_string(payload, "rule", "name").unwrap_or_else(|| "未知规则".to_string());
    let subject = format!(
        "[operationMonitoring][{severity}] {}: {node} / {rule}",
        lifecycle_label(kind)
    );
    (single_line(&subject, 240), notification_text(payload))
}

fn feishu_signature(secret: &str, timestamp: i64) -> String {
    let key = format!("{timestamp}\n{secret}");
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(&[]);
    BASE64_STANDARD.encode(mac.finalize().into_bytes())
}

fn feishu_body(payload: &Value, secret: Option<&str>, timestamp: i64) -> Value {
    let mut body = json!({
        "msg_type": "text",
        "content": {"text": notification_text(payload)},
    });
    if let Some(secret) = secret {
        let object = body.as_object_mut().expect("Feishu payload is an object");
        object.insert(
            "timestamp".to_string(),
            Value::String(timestamp.to_string()),
        );
        object.insert(
            "sign".to_string(),
            Value::String(feishu_signature(secret, timestamp)),
        );
    }
    body
}

fn dingtalk_signature(secret: &str, timestamp: i64) -> String {
    let message = format!("{timestamp}\n{secret}");
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(message.as_bytes());
    BASE64_STANDARD.encode(mac.finalize().into_bytes())
}

fn dingtalk_timestamp(timestamp: i64) -> i64 {
    timestamp.saturating_mul(1_000)
}

fn dingtalk_body(payload: &Value) -> Value {
    json!({
        "msgtype": "text",
        "text": {"content": notification_text(payload)},
    })
}

fn wecom_body(payload: &Value) -> Value {
    let content = notification_text(payload);
    json!({
        "msgtype": "text",
        "text": {"content": crate::audit::truncate(&content, MAX_WECOM_TEXT_BYTES)},
    })
}

fn slack_body(payload: &Value) -> Value {
    json!({"text": notification_text(payload)})
}

fn msteams_body(payload: &Value) -> Value {
    let kind = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("alert.notification");
    json!({
        "@type": "MessageCard",
        "@context": "http://schema.org/extensions",
        "summary": format!("operationMonitoring {}", lifecycle_label(kind)),
        "themeColor": if kind == "alert.firing" { "D64545" } else { "2E7D5B" },
        "text": notification_text(payload),
    })
}

fn telegram_body(payload: &Value, chat_id: &str) -> Value {
    json!({
        "chat_id": chat_id,
        "text": crate::audit::truncate(&notification_text(payload), 4 * 1024),
        "disable_web_page_preview": true,
    })
}

fn discord_body(payload: &Value) -> Value {
    json!({
        "content": crate::audit::truncate(&notification_text(payload), 2_000),
        "allowed_mentions": {"parse": []},
    })
}

fn response_code(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    })
}

fn channel_response_error(channel_type: &str, status: StatusCode, body: &[u8]) -> Option<String> {
    if !status.is_success() {
        return Some(format!("http_status_{}", status.as_u16()));
    }
    match channel_type {
        "generic_webhook" => None,
        "feishu" => {
            let Ok(value) = serde_json::from_slice::<Value>(body) else {
                return Some("feishu_invalid_response".to_string());
            };
            let code = response_code(&value, "code");
            let status_code = response_code(&value, "StatusCode");
            match code.or(status_code) {
                Some(0) => None,
                Some(code) => Some(format!("feishu_code_{code}")),
                None => Some("feishu_invalid_response".to_string()),
            }
        }
        "wecom" => {
            let Ok(value) = serde_json::from_slice::<Value>(body) else {
                return Some("wecom_invalid_response".to_string());
            };
            match response_code(&value, "errcode") {
                Some(0) => None,
                Some(code) => Some(format!("wecom_errcode_{code}")),
                None => Some("wecom_invalid_response".to_string()),
            }
        }
        "dingtalk" => {
            let Ok(value) = serde_json::from_slice::<Value>(body) else {
                return Some("dingtalk_invalid_response".to_string());
            };
            match response_code(&value, "errcode") {
                Some(0) => None,
                Some(code) => Some(format!("dingtalk_errcode_{code}")),
                None => Some("dingtalk_invalid_response".to_string()),
            }
        }
        "telegram" => {
            let Ok(value) = serde_json::from_slice::<Value>(body) else {
                return Some("telegram_invalid_response".to_string());
            };
            match value.get("ok").and_then(Value::as_bool) {
                Some(true) => None,
                Some(false) => response_code(&value, "error_code")
                    .map(|code| format!("telegram_error_code_{code}"))
                    .or_else(|| Some("telegram_api_error".to_string())),
                None => Some("telegram_invalid_response".to_string()),
            }
        }
        "slack" | "msteams" => None,
        "discord" => None,
        _ => Some("unsupported_http_channel_type".to_string()),
    }
}

async fn send_http_channel(
    state: &AppState,
    channel: &ChannelRow,
    delivery: &DeliveryRow,
    timeout: Duration,
    policy: DestinationPolicy,
) -> AppResult<(StatusCode, String, Option<String>)> {
    let url = decrypt_string(state, &channel.url_ciphertext)?;
    let url = validate_webhook_url_with_policy(&url, policy)?;
    let parsed_url = Url::parse(&url).map_err(|_| AppError::bad_request("Webhook URL 格式无效"))?;
    let client = secure_http_client(&parsed_url, timeout, policy).await?;
    let timestamp = now_ts();
    let mut request = client.post(&url).header("content-type", "application/json");
    let body = match channel.channel_type.as_str() {
        "generic_webhook" => {
            let body = serde_json::to_vec(&delivery.payload)
                .map_err(|error| anyhow::anyhow!("failed to serialize webhook payload: {error}"))?;
            request = request
                .header("x-om-timestamp", timestamp.to_string())
                .header("x-om-delivery-id", &delivery.id);
            for (name, value) in decrypt_headers(state, &channel.headers_ciphertext)? {
                request = request.header(name, value);
            }
            if let Some(ciphertext) = channel.secret_ciphertext.as_deref() {
                let secret = state.auth_cipher.decrypt(ciphertext)?;
                request = request.header(
                    "x-om-signature",
                    webhook_signature(&secret, timestamp, &body),
                );
            }
            body
        }
        "feishu" => {
            let secret = channel
                .secret_ciphertext
                .as_deref()
                .map(|ciphertext| decrypt_string(state, ciphertext))
                .transpose()?;
            serde_json::to_vec(&feishu_body(
                &delivery.payload,
                secret.as_deref(),
                timestamp,
            ))
            .map_err(|error| anyhow::anyhow!("failed to serialize Feishu payload: {error}"))?
        }
        "wecom" => serde_json::to_vec(&wecom_body(&delivery.payload))
            .map_err(|error| anyhow::anyhow!("failed to serialize WeCom payload: {error}"))?,
        "dingtalk" => {
            if let Some(ciphertext) = channel.secret_ciphertext.as_deref() {
                let secret = decrypt_string(state, ciphertext)?;
                // DingTalk's signed robot API expects milliseconds, while the
                // generic delivery timestamp remains Unix seconds.
                let signed_timestamp = dingtalk_timestamp(timestamp);
                let mut signed_url = Url::parse(&url)
                    .map_err(|_| AppError::bad_request("钉钉 Webhook URL 格式无效"))?;
                signed_url
                    .query_pairs_mut()
                    .append_pair("timestamp", &signed_timestamp.to_string())
                    .append_pair("sign", &dingtalk_signature(&secret, signed_timestamp));
                request = client
                    .post(signed_url)
                    .header("content-type", "application/json");
            }
            serde_json::to_vec(&dingtalk_body(&delivery.payload))
                .map_err(|error| anyhow::anyhow!("failed to serialize DingTalk payload: {error}"))?
        }
        "slack" => serde_json::to_vec(&slack_body(&delivery.payload))
            .map_err(|error| anyhow::anyhow!("failed to serialize Slack payload: {error}"))?,
        "msteams" => serde_json::to_vec(&msteams_body(&delivery.payload)).map_err(|error| {
            anyhow::anyhow!("failed to serialize Microsoft Teams payload: {error}")
        })?,
        "telegram" => {
            let config = decrypt_telegram_config(state, channel)?;
            serde_json::to_vec(&telegram_body(&delivery.payload, &config.chat_id))
                .map_err(|error| anyhow::anyhow!("failed to serialize Telegram payload: {error}"))?
        }
        "discord" => serde_json::to_vec(&discord_body(&delivery.payload))
            .map_err(|error| anyhow::anyhow!("failed to serialize Discord payload: {error}"))?,
        _ => return Err(AppError::bad_request("该渠道不是 HTTP 通知渠道")),
    };
    let mut response = request.body(body).send().await.map_err(|error| {
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
    let error = channel_response_error(&channel.channel_type, status, &bytes);
    Ok((status, response_excerpt, error))
}

async fn send_email_channel(
    state: &AppState,
    channel: &ChannelRow,
    delivery: &DeliveryRow,
) -> AppResult<String> {
    let config = decrypt_email_config(state, channel)?;
    let addresses = public_socket_addresses(
        &config.smtp_host,
        config.smtp_port,
        DestinationPolicy::PublicOnly,
    )
    .await?;
    let connect_host = addresses
        .first()
        .map(|address| address.ip().to_string())
        .ok_or_else(|| AppError::new(StatusCode::BAD_GATEWAY, "SMTP 主机没有可用的公网地址"))?;
    let (subject, body) = email_notification_content(&delivery.payload);
    let from_address = config
        .from_address
        .parse::<Address>()
        .map_err(|_| AppError::new(StatusCode::BAD_GATEWAY, "smtp_message_build_failed"))?;
    let mut message = Message::builder()
        .from(Mailbox::new(config.from_name.clone(), from_address))
        .subject(subject)
        .message_id(Some(format!(
            "<{}@operation-monitoring.local>",
            delivery.id
        )))
        .header(ContentType::TEXT_PLAIN);
    for recipient in &config.recipients {
        let address = recipient
            .parse::<Address>()
            .map_err(|_| AppError::new(StatusCode::BAD_GATEWAY, "smtp_message_build_failed"))?;
        message = message.to(Mailbox::new(None, address));
    }
    let message = message
        .body(body)
        .map_err(|_| AppError::new(StatusCode::BAD_GATEWAY, "smtp_message_build_failed"))?;
    let tls_parameters = TlsParameters::new(config.smtp_host.clone()).map_err(|error| {
        AppError::new(
            StatusCode::BAD_GATEWAY,
            format!("SMTP TLS 参数无效: {error}"),
        )
    })?;
    let mut transport = match config.security.as_str() {
        "starttls" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(connect_host)
            .tls(Tls::Required(tls_parameters)),
        "smtps" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(connect_host)
            .tls(Tls::Wrapper(tls_parameters)),
        _ => return Err(AppError::bad_request("SMTP security 配置无效")),
    }
    .port(config.smtp_port)
    .timeout(Some(Duration::from_secs(SMTP_TIMEOUT_SECONDS)));
    if let (Some(username), Some(password)) = (config.username, config.password) {
        transport = transport.credentials(Credentials::new(username, password));
    }
    let transport = transport.build::<Tokio1Executor>();
    match tokio::time::timeout(
        Duration::from_secs(SMTP_TIMEOUT_SECONDS),
        transport.send(message),
    )
    .await
    {
        Ok(Ok(_)) => Ok("smtp_accepted".to_string()),
        Ok(Err(error)) => Err(AppError::new(
            StatusCode::BAD_GATEWAY,
            smtp_delivery_error_code(&error),
        )),
        Err(_) => Err(AppError::new(StatusCode::GATEWAY_TIMEOUT, "smtp_timeout")),
    }
}

fn smtp_delivery_error_code(error: &lettre::transport::smtp::Error) -> String {
    if error.is_timeout() {
        "smtp_timeout".to_string()
    } else if error.is_tls() {
        "smtp_tls_failed".to_string()
    } else if error.is_transient() {
        error
            .status()
            .map(|status| format!("smtp_transient_{status}"))
            .unwrap_or_else(|| "smtp_transient".to_string())
    } else if let Some(status) = error.status() {
        format!("smtp_status_{status}")
    } else if error.is_permanent() {
        "smtp_permanent".to_string()
    } else {
        "smtp_send_failed".to_string()
    }
}

async fn send_webhook(state: &AppState, delivery: &DeliveryRow) -> WebhookAttemptOutcome {
    send_webhook_with_options(
        state,
        delivery,
        Duration::from_secs(WEBHOOK_TIMEOUT_SECONDS),
        DestinationPolicy::PublicOnly,
    )
    .await
}

async fn send_webhook_with_options(
    state: &AppState,
    delivery: &DeliveryRow,
    timeout: Duration,
    policy: DestinationPolicy,
) -> WebhookAttemptOutcome {
    let started = Instant::now();
    let outcome = async {
        let channel = load_channel(&state.db, &delivery.channel_id).await?;
        if !channel.enabled {
            return Err(AppError::new(StatusCode::CONFLICT, "通知渠道已停用"));
        }
        if channel.channel_type == "email" {
            let response_excerpt = send_email_channel(state, &channel, delivery).await?;
            Ok::<_, AppError>((None, response_excerpt, None))
        } else {
            let (status, response_excerpt, error) =
                send_http_channel(state, &channel, delivery, timeout, policy).await?;
            Ok((Some(i64::from(status.as_u16())), response_excerpt, error))
        }
    }
    .await;
    let duration_ms = started.elapsed().as_millis().min(i64::MAX as u128) as i64;
    match outcome {
        Ok((http_status, response_excerpt, error)) => WebhookAttemptOutcome {
            succeeded: error.is_none(),
            http_status,
            duration_ms,
            error: error.unwrap_or_default(),
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

#[cfg(test)]
async fn send_webhook_to_loopback(
    state: &AppState,
    delivery: &DeliveryRow,
    timeout: Duration,
) -> WebhookAttemptOutcome {
    send_webhook_with_options(state, delivery, timeout, DestinationPolicy::AllowLoopback).await
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
    let channel_state = sqlx::query_as::<_, (bool, Option<i64>, String)>(
        "SELECT enabled,deleted_at,channel_type FROM alert_webhook_channels WHERE id=$1 FOR SHARE",
    )
    .bind(&delivery.channel_id)
    .fetch_optional(&mut *tx)
    .await?;
    let inactive_channel_error = match channel_state.as_ref() {
        Some((_, Some(_), _)) | None => Some("channel_deleted"),
        Some((false, None, _)) => Some("channel_disabled"),
        Some((true, None, _)) => None,
    };
    let event_resolved = if !outcome.succeeded && delivery.kind == "alert.firing" {
        if let Some(event_id) = delivery.event_id.as_deref() {
            sqlx::query_scalar::<_, bool>(
                "SELECT status='resolved' FROM alert_events WHERE id=$1 FOR SHARE",
            )
            .bind(event_id)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(false)
        } else {
            false
        }
    } else {
        false
    };
    let channel_cooldown_delay = channel_state
        .as_ref()
        .and_then(|(_, deleted_at, channel_type)| {
            deleted_at.is_none().then(|| {
                transient_channel_cooldown_delay(
                    channel_type,
                    &outcome.error,
                    delivery.cycle_attempts,
                )
            })
        })
        .flatten();
    if let Some(delay) = channel_cooldown_delay {
        let resume_at = now.saturating_add(delay);
        sqlx::query(
            r#"
            UPDATE alert_webhook_channels SET
                delivery_cooldown_until=GREATEST(COALESCE(delivery_cooldown_until,$1),$1)
            WHERE id=$2
            "#,
        )
        .bind(resume_at)
        .bind(&delivery.channel_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            UPDATE alert_deliveries SET
                next_attempt_at=GREATEST(COALESCE(next_attempt_at,$1),$1),updated_at=$2
            WHERE channel_id=$3 AND status='pending'
            "#,
        )
        .bind(resume_at)
        .bind(now)
        .bind(&delivery.channel_id)
        .execute(&mut *tx)
        .await?;
    }
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
    } else if event_resolved {
        sqlx::query(
            r#"
            UPDATE alert_deliveries SET status='suppressed',
                suppression_reason='event_resolved_before_delivery',
                next_attempt_at=NULL,lease_until=NULL,lease_token=NULL,
                last_error=$1,completed_at=$2,updated_at=$2
            WHERE id=$3 AND status='processing' AND lease_token=$4
            "#,
        )
        .bind(&outcome.error)
        .bind(now)
        .bind(&delivery.id)
        .bind(lease_token)
        .execute(&mut *tx)
        .await?;
    } else if let Some(channel_error) = inactive_channel_error {
        sqlx::query(
            r#"
            UPDATE alert_deliveries SET status='failed',next_attempt_at=NULL,
                lease_until=NULL,lease_token=NULL,last_error=$1,completed_at=$2,updated_at=$2
            WHERE id=$3 AND status='processing' AND lease_token=$4
            "#,
        )
        .bind(channel_error)
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

fn transient_channel_cooldown_delay(
    channel_type: &str,
    error: &str,
    cycle_attempts: i64,
) -> Option<i64> {
    let transient_smtp_error = error == "smtp_timeout" || error.starts_with("smtp_transient");
    if channel_type != "email" || !transient_smtp_error {
        return None;
    }
    retry_delay_after(cycle_attempts).or_else(|| DELIVERY_RETRY_DELAYS.last().copied())
}

async fn notification_delivery_worker(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        for _ in 0..16 {
            let delivery = match claim_delivery(&state.db, now_ts()).await {
                Ok(Some(delivery)) => delivery,
                Ok(None) => break,
                Err(error) => {
                    warn!(?error, "failed to claim alert notification delivery");
                    break;
                }
            };
            match current_delivery_suppression(&state.db, &delivery, now_ts()).await {
                Ok(Some(reason)) => {
                    if let Err(error) =
                        mark_delivery_suppressed(&state.db, &delivery, &reason, now_ts()).await
                    {
                        warn!(?error, delivery_id=%delivery.id, "failed to suppress notification delivery");
                    }
                    continue;
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(?error, delivery_id=%delivery.id, "failed to recheck notification suppression");
                    if let Err(error) =
                        release_delivery_claim(&state.db, &delivery, now_ts() + 1).await
                    {
                        warn!(?error, delivery_id=%delivery.id, "failed to release notification claim");
                    }
                    continue;
                }
            }
            let Some(delivery) = (match start_delivery_attempt(&state.db, &delivery, now_ts()).await
            {
                Ok(delivery) => delivery,
                Err(error) => {
                    warn!(?error, delivery_id=%delivery.id, "failed to start notification attempt");
                    continue;
                }
            }) else {
                continue;
            };
            let outcome = send_webhook(&state, &delivery).await;
            if let Err(error) = finish_delivery_attempt(&state.db, &delivery, outcome).await {
                warn!(?error, delivery_id=%delivery.id, "failed to record notification attempt");
            }
        }
    }
}

pub async fn webhook_delivery_loop(state: AppState) {
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..DELIVERY_WORKER_COUNT {
        workers.spawn(notification_delivery_worker(state.clone()));
    }
    while let Some(result) = workers.join_next().await {
        error!(?result, "alert notification worker stopped unexpectedly");
        workers.spawn(notification_delivery_worker(state.clone()));
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
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
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
        sqlx::query(AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&bootstrap)
            .await
            .expect("drop isolated alert schema");
        bootstrap.close().await;
    }

    async fn create_alert_prerequisites(db: &PgPool) {
        for statement in [
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            "CREATE TABLE instances (id TEXT PRIMARY KEY, name TEXT NOT NULL DEFAULT '', hostname TEXT NOT NULL DEFAULT '', region TEXT NOT NULL DEFAULT '', os TEXT NOT NULL DEFAULT '', arch TEXT NOT NULL DEFAULT '', agent_version TEXT NOT NULL DEFAULT '', approved BIGINT NOT NULL DEFAULT 1, disabled BIGINT NOT NULL DEFAULT 0, expires_at BIGINT)",
            "CREATE TABLE metrics (id BIGSERIAL PRIMARY KEY, instance_id TEXT NOT NULL, ts BIGINT NOT NULL, latency_ms DOUBLE PRECISION)",
        ] {
            sqlx::query(statement)
                .execute(db)
                .await
                .expect("create alert prerequisite");
        }
    }

    fn test_state(db: PgPool) -> AppState {
        let database_url = std::env::var("OM_TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgresql://localhost/postgres".to_string());
        AppState::new(
            db,
            Cli {
                bind: "127.0.0.1:0".parse().expect("test bind address"),
                database_url,
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
                id,name,channel_type,url_ciphertext,secret_ciphertext,headers_ciphertext,
                config_ciphertext,enabled,created_at,updated_at
            ) VALUES($1,'Test webhook','generic_webhook',$2,$3,$4,NULL,TRUE,100,100)
            ON CONFLICT(id) DO UPDATE SET
                channel_type='generic_webhook',
                url_ciphertext=EXCLUDED.url_ciphertext,
                secret_ciphertext=EXCLUDED.secret_ciphertext,
                headers_ciphertext=EXCLUDED.headers_ciphertext,
                config_ciphertext=NULL,
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
            channel_snapshot: json!({
                "id": channel_id,
                "name": "Test webhook",
                "channel_type": "generic_webhook",
            }),
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

        let expiration = RuleRequest {
            name: "expiring".to_string(),
            metric: "instance_expiring".to_string(),
            threshold: Some(7.0),
            duration_seconds: 0,
            severity: "warning".to_string(),
            scope: "all".to_string(),
            target_instance_ids: vec![],
            channel_ids: vec![],
            enabled: true,
        };
        assert!(validate_rule(&expiration).is_ok());
        let mut invalid_expiration = expiration.clone();
        invalid_expiration.threshold = Some(1.5);
        assert!(validate_rule(&invalid_expiration).is_err());
        invalid_expiration.threshold = Some(7.0);
        invalid_expiration.duration_seconds = 1;
        assert!(validate_rule(&invalid_expiration).is_err());
    }

    #[test]
    fn limits_rule_link_arrays_before_deduplication() {
        let base = RuleRequest {
            name: "offline".to_string(),
            metric: "node_offline".to_string(),
            threshold: None,
            duration_seconds: 60,
            severity: "critical".to_string(),
            scope: "specific".to_string(),
            target_instance_ids: vec!["node-1".to_string()],
            channel_ids: vec![],
            enabled: true,
        };

        let mut at_target_limit = base.clone();
        at_target_limit.target_instance_ids =
            vec!["node-1".to_string(); MAX_RULE_TARGET_INSTANCE_IDS];
        let normalized = validate_rule(&at_target_limit).expect("accept target ID limit");
        assert_eq!(normalized.target_instance_ids, vec!["node-1".to_string()]);

        let mut too_many_targets = base.clone();
        too_many_targets.target_instance_ids =
            vec!["node-1".to_string(); MAX_RULE_TARGET_INSTANCE_IDS + 1];
        let error = validate_rule(&too_many_targets).expect_err("reject excessive target IDs");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(
            error
                .message
                .contains(&MAX_RULE_TARGET_INSTANCE_IDS.to_string())
        );

        let mut at_channel_limit = base.clone();
        at_channel_limit.channel_ids = vec!["channel-1".to_string(); MAX_RULE_CHANNEL_IDS];
        let normalized = validate_rule(&at_channel_limit).expect("accept channel ID limit");
        assert_eq!(normalized.channel_ids, vec!["channel-1".to_string()]);

        let mut too_many_channels = base;
        too_many_channels.channel_ids = vec!["channel-1".to_string(); MAX_RULE_CHANNEL_IDS + 1];
        let error = validate_rule(&too_many_channels).expect_err("reject excessive channel IDs");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.contains(&MAX_RULE_CHANNEL_IDS.to_string()));
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
    fn legacy_channels_default_to_generic_and_channel_type_is_immutable() {
        let payload: ChannelRequest = serde_json::from_value(json!({
            "name": "legacy",
            "url": "https://hooks.example.test/notify"
        }))
        .expect("deserialize legacy channel request");
        assert_eq!(
            channel_type_for_create(payload.channel_type.as_deref()).unwrap(),
            "generic_webhook"
        );
        assert_eq!(channel_type_for_update(None, "email").unwrap(), "email");
        assert!(channel_type_for_update(Some("wecom"), "feishu").is_err());
    }

    #[test]
    fn validates_email_tls_credentials_and_preserves_password_bytes() {
        let config = EmailChannelConfig {
            smtp_host: "smtp.example.test".to_string(),
            smtp_port: 587,
            security: "starttls".to_string(),
            username: Some("mailer".to_string()),
            password: Some("  password with spaces  ".to_string()),
            from_address: "alerts@example.test".to_string(),
            from_name: Some("Operations".to_string()),
            recipients: vec![
                "admin@example.test".to_string(),
                "ADMIN@example.test".to_string(),
            ],
        };
        let validated = validate_email_config(config.clone()).expect("valid email config");
        assert_eq!(validated.password, config.password);
        assert_eq!(validated.recipients, vec!["admin@example.test"]);

        let mut invalid = config.clone();
        invalid.security = "plaintext".to_string();
        assert!(validate_email_config(invalid).is_err());
        let mut invalid = config;
        invalid.password = None;
        assert!(validate_email_config(invalid).is_err());
    }

    #[test]
    fn notification_targets_reject_non_public_addresses() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "fc00::1",
            "fd00::1",
            "fe80::1",
            "fec0::1",
            "2001:db8::1",
            "3fff::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(
                !is_public_destination(address.parse().unwrap()),
                "address should be blocked: {address}"
            );
        }
        assert!(is_public_destination("8.8.8.8".parse().unwrap()));
        assert!(is_public_destination("192.0.1.1".parse().unwrap()));
        assert!(is_public_destination(
            "2001:4860:4860::8888".parse().unwrap()
        ));
        assert!(validate_webhook_url("http://127.0.0.1/hook").is_err());
        assert!(validate_webhook_url("http://localhost/hook").is_err());
        assert!(validate_webhook_url("http://[::1]/hook").is_err());
    }

    #[tokio::test]
    async fn runtime_notification_resolution_rejects_loopback() {
        assert!(
            public_socket_addresses("127.0.0.1", 80, DestinationPolicy::PublicOnly)
                .await
                .is_err()
        );
        assert!(
            public_socket_addresses("::1", 80, DestinationPolicy::PublicOnly)
                .await
                .is_err()
        );
    }

    #[test]
    fn platform_adapters_use_expected_payloads_and_signatures() {
        let payload = json!({
            "type": "alert.firing",
            "event": {
                "severity": "critical",
                "metric": "cpu_percent",
                "instance_id": "node-1",
                "current_value": 95.5,
                "threshold": 90
            },
            "rule": {"name": "High CPU"},
            "node": {"name": "API node"}
        });
        let feishu = feishu_body(&payload, Some("test"), 1_700_000_000);
        assert_eq!(feishu["msg_type"], "text");
        assert_eq!(feishu["timestamp"], "1700000000");
        assert_eq!(
            feishu["sign"],
            "eSJQnOl8XqPTMPHWz9e5IzeHS/tqoc68g2967ekIPmg="
        );
        assert!(
            feishu["content"]["text"]
                .as_str()
                .unwrap()
                .contains("API node")
        );

        let wecom = wecom_body(&payload);
        assert_eq!(wecom["msgtype"], "text");
        assert!(
            wecom["text"]["content"]
                .as_str()
                .unwrap()
                .contains("High CPU")
        );

        let long_payload = json!({
            "type": "alert.acknowledged",
            "event": {"instance_id": "node-1"},
            "note": "告".repeat(2_000),
        });
        let long_wecom = wecom_body(&long_payload);
        let content = long_wecom["text"]["content"].as_str().unwrap();
        assert!(content.len() <= MAX_WECOM_TEXT_BYTES);
        assert!(content.is_char_boundary(content.len()));
    }

    #[test]
    fn common_platform_adapters_use_expected_payloads() {
        let payload = json!({
            "type": "alert.firing",
            "event": {
                "severity": "critical",
                "metric": "cpu_percent",
                "instance_id": "node-1",
                "current_value": 95.5,
                "threshold": 90
            },
            "rule": {"name": "High CPU"},
            "node": {"name": "API node"}
        });
        let ding = dingtalk_body(&payload);
        assert_eq!(ding["msgtype"], "text");
        assert!(
            ding["text"]["content"]
                .as_str()
                .unwrap()
                .contains("API node")
        );
        assert_eq!(
            dingtalk_signature("secret", 1_700_000_000),
            "vli3/GfD4kJTUyxxBD82mmmq4rnrfhhcCjvn2rP9x7o="
        );
        assert_eq!(
            dingtalk_timestamp(1_700_000_000),
            1_700_000_000_000,
            "DingTalk timestamps are millisecond Unix timestamps"
        );

        let slack = slack_body(&payload);
        assert!(slack["text"].as_str().unwrap().contains("High CPU"));

        let teams = msteams_body(&payload);
        assert_eq!(teams["@type"], "MessageCard");
        assert_eq!(teams["themeColor"], "D64545");

        let telegram = telegram_body(&payload, "-100123");
        assert_eq!(telegram["chat_id"], "-100123");
        assert!(telegram["text"].as_str().unwrap().contains("critical"));

        let discord = discord_body(&payload);
        assert!(discord["content"].as_str().unwrap().contains("CPU 使用率"));
        assert_eq!(discord["allowed_mentions"]["parse"], json!([]));
    }

    #[test]
    fn common_platform_business_codes_control_delivery_success() {
        assert_eq!(
            channel_response_error("dingtalk", StatusCode::OK, br#"{"errcode":0}"#),
            None
        );
        assert_eq!(
            channel_response_error("dingtalk", StatusCode::OK, br#"{"errcode":310000}"#),
            Some("dingtalk_errcode_310000".to_string())
        );
        assert_eq!(
            channel_response_error("telegram", StatusCode::OK, br#"{"ok":true}"#),
            None
        );
        assert_eq!(
            channel_response_error(
                "telegram",
                StatusCode::OK,
                br#"{"ok":false,"error_code":400}"#,
            ),
            Some("telegram_error_code_400".to_string())
        );
        assert_eq!(channel_response_error("slack", StatusCode::OK, b"ok"), None);
        assert_eq!(
            channel_response_error("slack", StatusCode::OK, b"invalid_payload"),
            None
        );
        assert_eq!(
            channel_response_error("msteams", StatusCode::OK, br#"{"accepted":true}"#),
            None
        );
        assert_eq!(
            channel_response_error("discord", StatusCode::NO_CONTENT, b""),
            None
        );
    }

    #[test]
    fn validates_common_channel_types_and_telegram_chat_id() {
        for channel_type in ["dingtalk", "slack", "msteams", "telegram", "discord"] {
            assert_eq!(validate_channel_type(channel_type).unwrap(), channel_type);
        }
        assert_eq!(
            normalized_chat_id(Some(" -100123 ")).unwrap(),
            Some("-100123".to_string())
        );
        assert!(normalized_chat_id(Some("bad\nid")).is_err());
        assert!(normalized_chat_id(Some(&"x".repeat(MAX_TELEGRAM_CHAT_ID_BYTES + 1))).is_err());
    }

    #[test]
    fn platform_business_codes_control_delivery_success() {
        assert_eq!(
            channel_response_error("feishu", StatusCode::OK, br#"{"code":0}"#),
            None
        );
        assert_eq!(
            channel_response_error(
                "feishu",
                StatusCode::OK,
                br#"{"code":19001,"StatusCode":0}"#,
            ),
            Some("feishu_code_19001".to_string())
        );
        assert_eq!(
            channel_response_error("feishu", StatusCode::OK, br#"{"StatusCode":0}"#),
            None
        );
        assert_eq!(
            channel_response_error("feishu", StatusCode::OK, br#"{"code":19001}"#),
            Some("feishu_code_19001".to_string())
        );
        assert_eq!(
            channel_response_error("wecom", StatusCode::OK, br#"{"errcode":0}"#),
            None
        );
        assert_eq!(
            channel_response_error("wecom", StatusCode::OK, br#"{"errcode":40013}"#),
            Some("wecom_errcode_40013".to_string())
        );
        assert_eq!(
            channel_response_error("generic_webhook", StatusCode::NO_CONTENT, b""),
            None
        );
    }

    #[test]
    fn email_content_is_readable_lifecycle_text() {
        let payload = json!({
            "type": "alert.resolved",
            "event": {
                "severity": "warning",
                "metric": "memory_percent",
                "instance_id": "node-1",
                "current_value": 72.5,
                "threshold": 90,
                "resolution_reason": "condition_recovered"
            },
            "rule": {"name": "Memory pressure"},
            "node": {"name": "Database node"}
        });
        let (subject, body) = email_notification_content(&payload);
        assert!(subject.contains("告警已恢复"));
        assert!(subject.contains("Database node"));
        assert!(body.contains("规则: Memory pressure"));
        assert!(body.contains("恢复原因: condition_recovered"));
        assert!(!body.starts_with('{'));
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
        assert!(!node_offline_suppresses("instance_expiring", true));
        assert!(!node_offline_suppresses("cpu_percent", false));
    }

    #[test]
    fn expiration_observation_uses_remaining_whole_day_threshold_direction() {
        assert_eq!(expiration_observation(None, 100, 7.0), (0.0, false));
        assert_eq!(
            expiration_observation(Some(100 + 8 * 86_400), 100, 7.0),
            (8.0, false)
        );
        assert_eq!(
            expiration_observation(Some(100 + 7 * 86_400), 100, 7.0),
            (7.0, true)
        );
        assert_eq!(
            expiration_observation(Some(99), 100, 0.0),
            (-1.0 / 86_400.0, true)
        );
    }

    #[test]
    fn expiration_notifications_use_readable_units() {
        let text = notification_text(&json!({
            "type": "alert.firing",
            "event": {
                "severity": "warning",
                "metric": "instance_expiring",
                "instance_id": "node-1",
                "current_value": -2.0,
                "threshold": 7
            },
            "rule": {"name": "Expiry reminder"},
            "node": {"name": "API node", "expires_at": 1_700_000_000_i64}
        }));
        assert!(text.contains("指标: 实例即将到期"));
        assert!(text.contains("当前值: 已到期 2.0 天"));
        assert!(text.contains("阈值: 剩余时间 <= 7 天"));
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
    fn offline_recovery_requires_a_stable_online_window() {
        assert_eq!(
            recovery_confirmation("node_offline", true, None, 100),
            (Some(100), false),
        );
        assert_eq!(
            recovery_confirmation("node_offline", true, Some(100), 159),
            (Some(100), false),
        );
        assert_eq!(
            recovery_confirmation("node_offline", true, Some(100), 160),
            (Some(100), true),
        );
        assert_eq!(
            recovery_confirmation("cpu_percent", true, None, 100),
            (None, true),
        );
        assert_eq!(
            recovery_confirmation("node_offline", false, None, 100),
            (None, true),
        );
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

    #[test]
    fn transient_smtp_failures_cool_down_only_the_failing_channel() {
        assert_eq!(
            transient_channel_cooldown_delay("email", "smtp_transient_4.7.0", 1),
            Some(60),
        );
        assert_eq!(
            transient_channel_cooldown_delay("email", "smtp_timeout", 5),
            Some(60 * 60),
        );
        assert_eq!(
            transient_channel_cooldown_delay("email", "smtp_status_5.7.1", 1),
            None,
        );
        assert_eq!(
            transient_channel_cooldown_delay("generic_webhook", "smtp_transient", 1),
            None,
        );
    }

    #[test]
    fn stale_lifecycle_deliveries_cannot_be_manually_replayed() {
        assert!(!lifecycle_has_advanced("alert.firing", Some("firing")));
        assert!(lifecycle_has_advanced("alert.firing", Some("acknowledged")));
        assert!(lifecycle_has_advanced("alert.firing", Some("resolved")));
        assert!(lifecycle_has_advanced(
            "alert.acknowledged",
            Some("resolved")
        ));
        assert!(!lifecycle_has_advanced("alert.resolved", Some("resolved")));
        assert!(!lifecycle_has_advanced("webhook.test", None));
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn expiration_rule_fires_and_resolves_when_expiration_changes() {
        let (db, bootstrap, schema) = isolated_test_pool("om_alerts_expiration").await;
        create_alert_prerequisites(&db).await;
        ensure_schema(&db).await.expect("create alert schema");
        insert_test_instance(&db, "node-expiration", "Expiring Node").await;
        sqlx::query(
            r#"
            INSERT INTO alert_rules(
                id,name,metric,threshold,duration_seconds,severity,scope,
                enabled,version,created_by,created_at,updated_at
            ) VALUES(
                'rule-expiration','Expiry reminder','instance_expiring',7,0,
                'warning','all',TRUE,1,'test',100,100
            )
            "#,
        )
        .execute(&db)
        .await
        .expect("insert expiration rule");
        let state = test_state(db.clone());
        let now = 1_700_000_000_i64;

        sqlx::query("UPDATE instances SET expires_at=$1 WHERE id='node-expiration'")
            .bind(now + 8 * 86_400)
            .execute(&db)
            .await
            .expect("set distant expiration");
        observe_expiration_at(&state, "node-expiration", now)
            .await
            .expect("evaluate distant expiration");
        let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM alert_events")
            .fetch_one(&db)
            .await
            .expect("count expiration events");
        assert_eq!(events, 0);

        let near_expiration = now + 7 * 86_400;
        sqlx::query("UPDATE instances SET expires_at=$1 WHERE id='node-expiration'")
            .bind(near_expiration)
            .execute(&db)
            .await
            .expect("set near expiration");
        observe_expiration_at(&state, "node-expiration", now + 1)
            .await
            .expect("evaluate near expiration");
        let (status, snapshot) = sqlx::query_as::<_, (String, Value)>(
            "SELECT status,node_snapshot FROM alert_events WHERE instance_id='node-expiration'",
        )
        .fetch_one(&db)
        .await
        .expect("load firing expiration event");
        assert_eq!(status, "firing");
        assert_eq!(snapshot["expires_at"], near_expiration);

        sqlx::query("UPDATE instances SET expires_at=$1 WHERE id='node-expiration'")
            .bind(now + 10 * 86_400)
            .execute(&db)
            .await
            .expect("postpone expiration");
        observe_expiration_at(&state, "node-expiration", now + 2)
            .await
            .expect("evaluate postponed expiration");
        let status: String = sqlx::query_scalar(
            "SELECT status FROM alert_events WHERE instance_id='node-expiration'",
        )
        .fetch_one(&db)
        .await
        .expect("load resolved expiration event");
        assert_eq!(status, "resolved");

        sqlx::query("UPDATE instances SET expires_at=$1 WHERE id='node-expiration'")
            .bind(now - 1)
            .execute(&db)
            .await
            .expect("expire instance");
        observe_expiration_at(&state, "node-expiration", now + 3)
            .await
            .expect("evaluate expired instance");
        sqlx::query("UPDATE instances SET expires_at=NULL WHERE id='node-expiration'")
            .execute(&db)
            .await
            .expect("clear expiration");
        observe_expiration_at(&state, "node-expiration", now + 4)
            .await
            .expect("evaluate cleared expiration");
        let unresolved: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM alert_events WHERE instance_id='node-expiration' AND status <> 'resolved'",
        )
        .fetch_one(&db)
        .await
        .expect("count unresolved expiration events");
        assert_eq!(unresolved, 0);

        drop_test_schema(db, bootstrap, schema).await;
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

        let success = send_webhook_to_loopback(&state, &delivery, Duration::from_secs(2)).await;
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
        let failed = send_webhook_to_loopback(&state, &delivery, Duration::from_secs(2)).await;
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
        let redirected = send_webhook_to_loopback(&state, &delivery, Duration::from_secs(2)).await;
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
        let truncated = send_webhook_to_loopback(&state, &delivery, Duration::from_secs(2)).await;
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
        let timed_out =
            send_webhook_to_loopback(&state, &delivery, Duration::from_millis(50)).await;
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
    async fn postgres_schema_migrates_legacy_channel_type_check() {
        let (db, bootstrap, schema) = isolated_test_pool("om_alerts_channel_migration").await;
        create_alert_prerequisites(&db).await;
        sqlx::raw_sql(
            r#"
            CREATE TABLE alert_webhook_channels (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                channel_type TEXT NOT NULL DEFAULT 'generic_webhook'
                    CHECK(channel_type IN ('generic_webhook', 'email', 'feishu', 'wecom')),
                url_ciphertext TEXT NOT NULL,
                secret_ciphertext TEXT,
                headers_ciphertext TEXT NOT NULL,
                config_ciphertext TEXT,
                enabled BOOLEAN NOT NULL DEFAULT TRUE,
                created_at BIGINT NOT NULL,
                updated_at BIGINT NOT NULL,
                deleted_at BIGINT
            );
            INSERT INTO alert_webhook_channels(
                id,name,channel_type,url_ciphertext,headers_ciphertext,
                enabled,created_at,updated_at
            ) VALUES('legacy-feishu','Legacy Feishu','feishu','url','headers',TRUE,100,100)
            "#,
        )
        .execute(&db)
        .await
        .expect("create legacy four-type notification channel table");

        let legacy_constraint: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM pg_constraint
                WHERE conrelid='alert_webhook_channels'::regclass
                  AND conname='alert_webhook_channels_channel_type_check'
            )
            "#,
        )
        .fetch_one(&db)
        .await
        .expect("inspect legacy automatically named channel constraint");
        assert!(legacy_constraint);

        ensure_schema(&db)
            .await
            .expect("migrate legacy notification channel constraint");
        for channel_type in ["dingtalk", "slack", "msteams", "telegram", "discord"] {
            sqlx::query(
                r#"
                INSERT INTO alert_webhook_channels(
                    id,name,channel_type,url_ciphertext,headers_ciphertext,
                    enabled,created_at,updated_at
                ) VALUES($1,$1,$1,'url','headers',TRUE,200,200)
                "#,
            )
            .bind(channel_type)
            .execute(&db)
            .await
            .unwrap_or_else(|error| panic!("insert migrated {channel_type} channel: {error}"));
        }
        ensure_schema(&db)
            .await
            .expect("rerun migrated notification channel schema idempotently");

        let constraints = sqlx::query_scalar::<_, String>(
            r#"
            SELECT conname FROM pg_constraint
            WHERE conrelid='alert_webhook_channels'::regclass AND contype='c'
            ORDER BY conname
            "#,
        )
        .fetch_all(&db)
        .await
        .expect("load migrated channel constraints");
        assert_eq!(
            constraints,
            vec!["alert_webhook_channels_type_check".to_string()]
        );

        drop_test_schema(db, bootstrap, schema).await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn lifecycle_deliveries_are_claimed_in_order_and_release_without_attempts() {
        let (db, bootstrap, schema) = isolated_test_pool("om_alerts_delivery_order").await;
        create_alert_prerequisites(&db).await;
        ensure_schema(&db).await.expect("create alert schema");
        let state = test_state(db.clone());
        insert_test_instance(&db, "node-order", "Ordered Node").await;
        insert_test_rule(&db, "rule-order").await;
        set_test_channel(
            &state,
            "channel-order",
            "http://127.0.0.1/unused",
            None,
            &BTreeMap::new(),
        )
        .await;
        let observed_at = now_ts();
        sqlx::query(
            r#"
            INSERT INTO alert_events(
                id,rule_id,instance_id,status,severity,metric,rule_snapshot,node_snapshot,
                threshold,duration_seconds,current_value,first_observed_at,fired_at,last_observed_at
            ) VALUES(
                'event-order','rule-order','node-order','firing','critical','cpu_percent',
                '{}'::JSONB,'{}'::JSONB,90,0,95,$1,$1,$1
            )
            "#,
        )
        .bind(observed_at)
        .execute(&db)
        .await
        .expect("insert event for ordered lifecycle deliveries");
        sqlx::query(
            r#"
            INSERT INTO alert_deliveries(
                id,event_id,channel_id,kind,status,payload,channel_snapshot,
                next_attempt_at,created_at,updated_at
            ) VALUES
                ('z-firing','event-order','channel-order','alert.firing','pending','{}'::JSONB,'{}'::JSONB,$1,$1,$1),
                ('a-ack','event-order','channel-order','alert.acknowledged','pending','{}'::JSONB,'{}'::JSONB,$1,$1,$1),
                ('m-resolved','event-order','channel-order','alert.resolved','pending','{}'::JSONB,'{}'::JSONB,$1,$1,$1)
            "#,
        )
        .bind(observed_at)
        .execute(&db)
        .await
        .expect("insert ordered lifecycle deliveries");

        let (first, competing) = tokio::join!(
            claim_delivery(&db, observed_at),
            claim_delivery(&db, observed_at),
        );
        let first = first.expect("claim firing delivery");
        let competing = competing.expect("run competing claim");
        assert_ne!(first.is_some(), competing.is_some());
        let first = first
            .or(competing)
            .expect("one worker claims firing delivery");
        assert_eq!(first.kind, "alert.firing");
        assert_eq!(
            first.lease_until,
            Some(observed_at + DELIVERY_LEASE_SECONDS)
        );
        assert!(
            claim_delivery(&db, observed_at)
                .await
                .expect("check blocked acknowledgement")
                .is_none()
        );
        let first = start_delivery_attempt(&db, &first, observed_at)
            .await
            .expect("start firing attempt")
            .expect("firing lease remains active");
        finish_delivery_attempt(
            &db,
            &first,
            WebhookAttemptOutcome {
                succeeded: true,
                http_status: Some(204),
                duration_ms: 1,
                error: String::new(),
                response_excerpt: String::new(),
            },
        )
        .await
        .expect("finish firing attempt");

        let acknowledgement = claim_delivery(&db, observed_at)
            .await
            .expect("claim acknowledgement")
            .expect("acknowledgement becomes available");
        assert_eq!(acknowledgement.kind, "alert.acknowledged");
        let acknowledgement = start_delivery_attempt(&db, &acknowledgement, observed_at)
            .await
            .expect("start acknowledgement attempt")
            .expect("acknowledgement lease remains active");
        finish_delivery_attempt(
            &db,
            &acknowledgement,
            WebhookAttemptOutcome {
                succeeded: true,
                http_status: Some(204),
                duration_ms: 1,
                error: String::new(),
                response_excerpt: String::new(),
            },
        )
        .await
        .expect("finish acknowledgement attempt");

        let resolved = claim_delivery(&db, observed_at)
            .await
            .expect("claim resolution")
            .expect("resolution becomes available");
        assert_eq!(resolved.kind, "alert.resolved");
        release_delivery_claim(&db, &resolved, observed_at + 1)
            .await
            .expect("release resolution after a pre-send database failure");
        let released = sqlx::query_as::<_, (String, i64, i64, Option<String>)>(
            "SELECT status,attempts_count,next_attempt_at,lease_token FROM alert_deliveries WHERE id=$1",
        )
        .bind(&resolved.id)
        .fetch_one(&db)
        .await
        .expect("load released resolution delivery");
        assert_eq!(released, ("pending".to_string(), 0, observed_at + 1, None));

        let resolved = claim_delivery(&db, observed_at + 1)
            .await
            .expect("reclaim released resolution")
            .expect("released resolution becomes available");
        let resolved = start_delivery_attempt(&db, &resolved, observed_at + 1)
            .await
            .expect("start released resolution")
            .expect("released resolution lease remains active");
        finish_delivery_attempt(
            &db,
            &resolved,
            WebhookAttemptOutcome {
                succeeded: true,
                http_status: Some(204),
                duration_ms: 1,
                error: String::new(),
                response_excerpt: String::new(),
            },
        )
        .await
        .expect("finish released resolution");

        insert_test_instance(&db, "node-recovery", "Recovery Node").await;
        sqlx::query(
            r#"
            INSERT INTO alert_events(
                id,rule_id,instance_id,status,severity,metric,rule_snapshot,node_snapshot,
                threshold,duration_seconds,current_value,first_observed_at,fired_at,last_observed_at
            ) VALUES(
                'event-recovery','rule-order','node-recovery','firing','critical','cpu_percent',
                '{}'::JSONB,'{}'::JSONB,90,0,95,$1,$1,$1
            )
            "#,
        )
        .bind(observed_at + 2)
        .execute(&db)
        .await
        .expect("insert recovering event");
        sqlx::query(
            "INSERT INTO alert_event_channels(event_id,channel_id) VALUES('event-recovery','channel-order')",
        )
        .execute(&db)
        .await
        .expect("link recovery event channel");
        sqlx::query(
            r#"
            INSERT INTO alert_deliveries(
                id,event_id,channel_id,kind,status,payload,channel_snapshot,
                attempts_count,cycle_attempts,next_attempt_at,created_at,updated_at
            ) VALUES(
                'retrying-firing','event-recovery','channel-order','alert.firing','pending',
                '{}'::JSONB,'{}'::JSONB,3,3,$1,$1,$1
            )
            "#,
        )
        .bind(observed_at + 2)
        .execute(&db)
        .await
        .expect("insert firing delivery near final retry");
        let firing = claim_delivery(&db, observed_at + 2)
            .await
            .expect("claim retrying firing")
            .expect("retrying firing becomes available");
        let firing = start_delivery_attempt(&db, &firing, observed_at + 2)
            .await
            .expect("start fourth firing attempt")
            .expect("firing lease remains active");
        finish_delivery_attempt(
            &db,
            &firing,
            WebhookAttemptOutcome {
                succeeded: false,
                http_status: Some(500),
                duration_ms: 1,
                error: "http_status_500".to_string(),
                response_excerpt: "failure".to_string(),
            },
        )
        .await
        .expect("schedule final firing retry");
        let deferred_until: i64 = sqlx::query_scalar(
            "SELECT next_attempt_at FROM alert_deliveries WHERE id='retrying-firing'",
        )
        .fetch_one(&db)
        .await
        .expect("load deferred firing retry");
        assert!(deferred_until > observed_at + 60 * 60 - 5);

        let mut recovery_tx = db.begin().await.expect("begin event recovery");
        let event = sqlx::query_as::<_, EventRow>(
            "SELECT * FROM alert_events WHERE id='event-recovery' FOR UPDATE",
        )
        .fetch_one(&mut *recovery_tx)
        .await
        .expect("lock recovering event");
        resolve_event_tx(
            &mut recovery_tx,
            event,
            observed_at + 3,
            "condition_recovered",
            Some((40.0, observed_at + 3)),
        )
        .await
        .expect("resolve event with deferred firing");
        recovery_tx.commit().await.expect("commit event recovery");
        let superseded = sqlx::query_as::<_, (String, String, Option<i64>)>(
            "SELECT status,suppression_reason,next_attempt_at FROM alert_deliveries WHERE id='retrying-firing'",
        )
        .fetch_one(&db)
        .await
        .expect("load superseded firing delivery");
        assert_eq!(
            superseded,
            (
                "suppressed".to_string(),
                "event_resolved_before_delivery".to_string(),
                None,
            )
        );
        let recovery = claim_delivery(&db, observed_at + 3)
            .await
            .expect("claim recovery without waiting for stale firing")
            .expect("recovery delivery becomes available immediately");
        assert_eq!(recovery.kind, "alert.resolved");
        assert_eq!(recovery.event_id.as_deref(), Some("event-recovery"));
        release_delivery_claim(&db, &recovery, observed_at + 4)
            .await
            .expect("release recovery test delivery");

        drop_test_schema(db, bootstrap, schema).await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn offline_flaps_share_one_event_until_online_state_is_stable() {
        let (db, bootstrap, schema) = isolated_test_pool("om_alerts_offline_flap").await;
        create_alert_prerequisites(&db).await;
        ensure_schema(&db).await.expect("create alert schema");
        let state = test_state(db.clone());
        insert_test_instance(&db, "node-flap", "Flapping Node").await;
        set_test_channel(
            &state,
            "channel-flap",
            "http://127.0.0.1/unused",
            None,
            &BTreeMap::new(),
        )
        .await;
        sqlx::query(
            r#"
            INSERT INTO alert_rules(
                id,name,metric,threshold,duration_seconds,severity,scope,
                enabled,version,created_by,created_at,updated_at
            ) VALUES(
                'rule-flap','Node offline','node_offline',NULL,0,'critical','all',
                TRUE,1,'test',100,100
            )
            "#,
        )
        .execute(&db)
        .await
        .expect("insert offline rule");
        sqlx::query(
            "INSERT INTO alert_rule_channels(rule_id,channel_id) VALUES('rule-flap','channel-flap')",
        )
        .execute(&db)
        .await
        .expect("link offline channel");
        let rule = sqlx::query_as::<_, RuleRow>("SELECT * FROM alert_rules WHERE id='rule-flap'")
            .fetch_one(&db)
            .await
            .expect("load offline rule");
        let observed_at = now_ts();

        process_rule_observation(
            &state,
            &rule,
            "node-flap",
            RuleObservation::Threshold {
                value: 1.0,
                abnormal: true,
            },
            observed_at,
            observed_at,
        )
        .await
        .expect("fire initial offline event");
        process_rule_observation(
            &state,
            &rule,
            "node-flap",
            RuleObservation::Threshold {
                value: 0.0,
                abnormal: false,
            },
            observed_at + 1,
            observed_at + 1,
        )
        .await
        .expect("begin online recovery window");
        process_rule_observation(
            &state,
            &rule,
            "node-flap",
            RuleObservation::Threshold {
                value: 1.0,
                abnormal: true,
            },
            observed_at + 2,
            observed_at + 2,
        )
        .await
        .expect("merge flap into active event");

        let active_events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM alert_events WHERE rule_id='rule-flap' AND status <> 'resolved'",
        )
        .fetch_one(&db)
        .await
        .expect("count active flap events");
        let firing_deliveries: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM alert_deliveries WHERE channel_id='channel-flap' AND kind='alert.firing' AND status <> 'suppressed'",
        )
        .fetch_one(&db)
        .await
        .expect("count deduplicated flap deliveries");
        let recovery_since: Option<i64> = sqlx::query_scalar(
            "SELECT recovery_since FROM alert_evaluation_states WHERE rule_id='rule-flap' AND instance_id='node-flap'",
        )
        .fetch_one(&db)
        .await
        .expect("load reset recovery window");
        assert_eq!(active_events, 1);
        assert_eq!(firing_deliveries, 1);
        assert_eq!(recovery_since, None);

        for offset in [3, 62] {
            process_rule_observation(
                &state,
                &rule,
                "node-flap",
                RuleObservation::Threshold {
                    value: 0.0,
                    abnormal: false,
                },
                observed_at + offset,
                observed_at + offset,
            )
            .await
            .expect("observe online recovery window");
        }
        let still_active: bool = sqlx::query_scalar(
            "SELECT status <> 'resolved' FROM alert_events WHERE rule_id='rule-flap'",
        )
        .fetch_one(&db)
        .await
        .expect("check unconfirmed recovery");
        assert!(still_active);

        process_rule_observation(
            &state,
            &rule,
            "node-flap",
            RuleObservation::Threshold {
                value: 0.0,
                abnormal: false,
            },
            observed_at + 63,
            observed_at + 63,
        )
        .await
        .expect("confirm stable recovery");
        let resolved: bool = sqlx::query_scalar(
            "SELECT status='resolved' FROM alert_events WHERE rule_id='rule-flap'",
        )
        .fetch_one(&db)
        .await
        .expect("check stable recovery");
        assert!(resolved);

        drop_test_schema(db, bootstrap, schema).await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn smtp_transient_failure_defers_its_channel_without_blocking_other_channels() {
        let (db, bootstrap, schema) = isolated_test_pool("om_alerts_smtp_cooldown").await;
        create_alert_prerequisites(&db).await;
        ensure_schema(&db).await.expect("create alert schema");
        let state = test_state(db.clone());
        for channel_id in ["channel-email", "channel-other"] {
            set_test_channel(
                &state,
                channel_id,
                "http://127.0.0.1/unused",
                None,
                &BTreeMap::new(),
            )
            .await;
        }
        sqlx::query(
            "UPDATE alert_webhook_channels SET channel_type='email' WHERE id='channel-email'",
        )
        .execute(&db)
        .await
        .expect("mark test channel as email");
        let observed_at = now_ts();
        sqlx::query(
            r#"
            INSERT INTO alert_deliveries(
                id,event_id,channel_id,kind,status,payload,channel_snapshot,
                next_attempt_at,created_at,updated_at
            ) VALUES
                ('a-email-first',NULL,'channel-email','webhook.test','pending','{}','{}',$1,$1,$1),
                ('b-email-second',NULL,'channel-email','webhook.test','pending','{}','{}',$1,$1,$1),
                ('c-other',NULL,'channel-other','webhook.test','pending','{}','{}',$1,$1,$1)
            "#,
        )
        .bind(observed_at)
        .execute(&db)
        .await
        .expect("insert channel isolation deliveries");

        let email = claim_delivery(&db, observed_at)
            .await
            .expect("claim first email")
            .expect("first email is ready");
        assert_eq!(email.id, "a-email-first");
        let other = claim_delivery(&db, observed_at)
            .await
            .expect("claim other channel")
            .expect("other channel remains available");
        assert_eq!(other.id, "c-other");
        let other = start_delivery_attempt(&db, &other, observed_at)
            .await
            .expect("start other channel attempt")
            .expect("other channel lease is active");
        finish_delivery_attempt(
            &db,
            &other,
            WebhookAttemptOutcome {
                succeeded: true,
                http_status: Some(204),
                duration_ms: 1,
                error: String::new(),
                response_excerpt: String::new(),
            },
        )
        .await
        .expect("finish other channel delivery");

        let email = start_delivery_attempt(&db, &email, observed_at)
            .await
            .expect("start email attempt")
            .expect("email lease is active");
        let before_failure = now_ts();
        finish_delivery_attempt(
            &db,
            &email,
            WebhookAttemptOutcome {
                succeeded: false,
                http_status: None,
                duration_ms: 1,
                error: "smtp_transient_4.7.0".to_string(),
                response_excerpt: String::new(),
            },
        )
        .await
        .expect("record transient SMTP failure");

        let queued = sqlx::query_as::<_, (String, i64, i64)>(
            "SELECT status,attempts_count,next_attempt_at FROM alert_deliveries WHERE id='b-email-second'",
        )
        .fetch_one(&db)
        .await
        .expect("load deferred email delivery");
        assert_eq!(queued.0, "pending");
        assert_eq!(queued.1, 0);
        assert!(queued.2 >= before_failure + 60);
        sqlx::query(
            r#"
            INSERT INTO alert_deliveries(
                id,event_id,channel_id,kind,status,payload,channel_snapshot,
                next_attempt_at,created_at,updated_at
            ) VALUES(
                'd-email-new',NULL,'channel-email','webhook.test','pending','{}','{}',$1,$1,$1
            )
            "#,
        )
        .bind(before_failure + 1)
        .execute(&db)
        .await
        .expect("insert email delivery during cooldown");
        assert!(
            claim_delivery(&db, before_failure + 1)
                .await
                .expect("check SMTP cooldown")
                .is_none()
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

        let success = send_webhook_to_loopback(&state, &delivery, Duration::from_millis(100)).await;
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
        let large = send_webhook_to_loopback(&state, &delivery, Duration::from_millis(100)).await;
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
        let redirected =
            send_webhook_to_loopback(&state, &delivery, Duration::from_millis(100)).await;
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
        let timed_out =
            send_webhook_to_loopback(&state, &delivery, Duration::from_millis(100)).await;
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
        let failure =
            send_webhook_to_loopback(&state, &failed_delivery, Duration::from_millis(100)).await;
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
