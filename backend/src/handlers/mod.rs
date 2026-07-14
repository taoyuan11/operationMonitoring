use std::path::{Path as FsPath, PathBuf};

use axum::{
    Json,
    extract::{Multipart, Path, Query, State, ws::WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use tokio::fs;
use uuid::Uuid;

use crate::{
    auth::require_admin,
    db::{
        approve_pending_instance, get_instance, get_instance_optional, instance_summary,
        latest_metric, register_or_touch_pending, retention_days, setting_value, write_action_log,
    },
    error::{AppError, AppResult},
    jobs::{create_command_job, dispatch_command},
    models::{
        ActionLogRecord, AgentRegisterRequest, AgentRegisterResponse, AgentReportRequest,
        AgentWsQuery, AppearanceResponse, CommandJobRecord, CommandRecord, CreateCommandRequest,
        HealthResponse, InstanceRecord, InstanceSummary, ListQuery, MetricRecord, MetricsQuery,
        PendingInstance, SettingsRequest, SettingsResponse, UpdateInstanceRequest,
    },
    state::AppState,
    updates::confirm_update_version,
    utils::{non_empty_or, now_ts},
    ws::{agent_socket, terminal_socket},
};

const BACKGROUND_SETTING_KEY: &str = "background_image_path";
const BACKGROUND_DIR: &str = "backgrounds";
const MAX_BACKGROUND_BYTES: usize = 5 * 1024 * 1024;

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        now: now_ts(),
    })
}

pub async fn public_appearance(
    State(state): State<AppState>,
) -> AppResult<Json<AppearanceResponse>> {
    Ok(Json(AppearanceResponse {
        background_image_url: background_image_url(&state).await?,
    }))
}

pub async fn public_instances(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<InstanceSummary>>> {
    let records = sqlx::query_as::<_, InstanceRecord>(
        r#"
        SELECT id, secret, name, region, country_code, country, province_code, province, city,
               remark, hostname, os, arch, agent_version,
               package_type, native_arch, update_privileged,
               approved, disabled, first_seen, last_seen
        FROM instances
        WHERE approved = 1 AND disabled = 0
        ORDER BY LOWER(name) ASC
        "#,
    )
    .fetch_all(&state.db)
    .await?;

    let mut summaries = Vec::with_capacity(records.len());
    for record in records {
        let metrics = latest_metric(&state.db, &record.id).await?;
        let online = state.agents.read().await.contains_key(&record.id);
        summaries.push(instance_summary(record, metrics, online));
    }

    Ok(Json(summaries))
}

pub async fn public_metrics(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<MetricsQuery>,
) -> AppResult<Json<Vec<MetricRecord>>> {
    let to = query.to.unwrap_or_else(now_ts);
    let from = query.from.unwrap_or(to - 3600);
    let limit = query.limit.unwrap_or(720).clamp(1, 5000);

    let metrics = sqlx::query_as::<_, MetricRecord>(
        r#"
        SELECT ts, cpu_percent, memory_used, memory_total, disk_used, disk_total,
               network_rx, network_tx, gpu_percent, gpu_memory_used, gpu_memory_total,
               uptime_seconds, load_average
        FROM metrics
        WHERE instance_id = $1 AND ts BETWEEN $2 AND $3
        ORDER BY ts ASC
        LIMIT $4
        "#,
    )
    .bind(id)
    .bind(from)
    .bind(to)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(metrics))
}

pub async fn admin_pending_instances(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<PendingInstance>>> {
    require_admin(&state, &headers).await?;

    let rows = sqlx::query_as::<_, PendingInstance>(
        r#"
        SELECT id, hostname, os, arch, agent_version, package_type, native_arch,
               (update_privileged = 1) AS update_privileged, first_seen, last_seen
        FROM pending_instances
        ORDER BY last_seen DESC
        "#,
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(rows))
}

pub async fn admin_approve_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<AgentRegisterResponse>> {
    let admin = require_admin(&state, &headers).await?;

    approve_pending_instance(&state.db, &id)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "待审批实例不存在"))?;

    write_action_log(
        &state.db,
        &admin.username,
        "approve_instance",
        &id,
        "批准实例接入",
    )
    .await?;

    Ok(Json(AgentRegisterResponse {
        approved: true,
        disabled: false,
        message: "实例已批准".to_string(),
    }))
}

pub async fn admin_reject_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<AgentRegisterResponse>> {
    let admin = require_admin(&state, &headers).await?;

    sqlx::query("DELETE FROM pending_instances WHERE id = $1")
        .bind(&id)
        .execute(&state.db)
        .await?;
    write_action_log(
        &state.db,
        &admin.username,
        "reject_instance",
        &id,
        "拒绝实例接入",
    )
    .await?;

    Ok(Json(AgentRegisterResponse {
        approved: false,
        disabled: false,
        message: "实例已拒绝".to_string(),
    }))
}

pub async fn admin_update_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(payload): Json<UpdateInstanceRequest>,
) -> AppResult<Json<InstanceSummary>> {
    let admin = require_admin(&state, &headers).await?;

    let current = get_instance(&state.db, &id).await?;
    let name = non_empty_or(payload.name, current.name);
    let location_changed = payload.country_code.is_some()
        || payload.country.is_some()
        || payload.province_code.is_some()
        || payload.province.is_some()
        || payload.city.is_some();
    let country_code = payload.country_code.unwrap_or(current.country_code);
    let country = payload.country.unwrap_or(current.country);
    let province_code = payload.province_code.unwrap_or(current.province_code);
    let province = payload.province.unwrap_or(current.province);
    let city = payload.city.unwrap_or(current.city);
    let region = if location_changed {
        [country.as_str(), province.as_str(), city.as_str()]
            .into_iter()
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" / ")
    } else {
        payload.region.unwrap_or(current.region)
    };
    let remark = payload.remark.unwrap_or(current.remark);

    if country.trim().is_empty() && (!province.trim().is_empty() || !city.trim().is_empty()) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "省份和城市必须隶属于国家",
        ));
    }
    if province.trim().is_empty() && !city.trim().is_empty() {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "城市必须隶属于省份"));
    }
    if !country_code.is_empty()
        && (country_code.len() != 2 || !country_code.bytes().all(|byte| byte.is_ascii_alphabetic()))
    {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "国家代码格式无效"));
    }

    sqlx::query(
        "UPDATE instances SET name = $1, region = $2, country_code = $3, country = $4, \
         province_code = $5, province = $6, city = $7, remark = $8 WHERE id = $9",
    )
    .bind(&name)
    .bind(&region)
    .bind(country_code.trim().to_ascii_uppercase())
    .bind(country.trim())
    .bind(province_code.trim())
    .bind(province.trim())
    .bind(city.trim())
    .bind(&remark)
    .bind(&id)
    .execute(&state.db)
    .await?;
    write_action_log(
        &state.db,
        &admin.username,
        "update_instance",
        &id,
        "编辑实例资料",
    )
    .await?;

    let updated = get_instance(&state.db, &id).await?;
    let metrics = latest_metric(&state.db, &updated.id).await?;
    let online = state.agents.read().await.contains_key(&updated.id);
    Ok(Json(instance_summary(updated, metrics, online)))
}

pub async fn admin_disable_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<AgentRegisterResponse>> {
    let admin = require_admin(&state, &headers).await?;

    sqlx::query("UPDATE instances SET disabled = 1 WHERE id = $1")
        .bind(&id)
        .execute(&state.db)
        .await?;
    state.agents.write().await.remove(&id);
    write_action_log(
        &state.db,
        &admin.username,
        "disable_instance",
        &id,
        "停用实例",
    )
    .await?;

    Ok(Json(AgentRegisterResponse {
        approved: true,
        disabled: true,
        message: "实例已停用".to_string(),
    }))
}

pub async fn admin_delete_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<AgentRegisterResponse>> {
    let admin = require_admin(&state, &headers).await?;

    sqlx::query("DELETE FROM metrics WHERE instance_id = $1")
        .bind(&id)
        .execute(&state.db)
        .await?;
    sqlx::query("DELETE FROM command_jobs WHERE instance_id = $1")
        .bind(&id)
        .execute(&state.db)
        .await?;
    sqlx::query("DELETE FROM instances WHERE id = $1")
        .bind(&id)
        .execute(&state.db)
        .await?;
    state.agents.write().await.remove(&id);
    write_action_log(
        &state.db,
        &admin.username,
        "delete_instance",
        &id,
        "删除实例和历史指标",
    )
    .await?;

    Ok(Json(AgentRegisterResponse {
        approved: false,
        disabled: true,
        message: "实例已删除".to_string(),
    }))
}

pub async fn admin_get_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<SettingsResponse>> {
    require_admin(&state, &headers).await?;
    Ok(Json(SettingsResponse {
        retention_days: retention_days(&state.db).await?,
        background_image_url: background_image_url(&state).await?,
    }))
}

pub async fn admin_put_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<SettingsRequest>,
) -> AppResult<Json<SettingsResponse>> {
    let admin = require_admin(&state, &headers).await?;
    let days = payload.retention_days.clamp(1, 365);
    sqlx::query(
        "INSERT INTO settings(key, value) VALUES('retention_days', $1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(days.to_string())
    .execute(&state.db)
    .await?;
    write_action_log(
        &state.db,
        &admin.username,
        "update_settings",
        "retention_days",
        &format!("指标保留天数设置为 {}", days),
    )
    .await?;
    Ok(Json(SettingsResponse {
        retention_days: days,
        background_image_url: background_image_url(&state).await?,
    }))
}

pub async fn admin_upload_background_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> AppResult<Json<SettingsResponse>> {
    let admin = require_admin(&state, &headers).await?;

    let mut image: Option<(String, Vec<u8>)> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "图片上传失败"))?
    {
        if field.name() != Some("image") {
            continue;
        }

        let content_type = field.content_type().unwrap_or("").to_string();
        let extension = background_extension(&content_type)
            .ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "仅支持 PNG、JPEG、WebP 图片"))?;
        let bytes = field
            .bytes()
            .await
            .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "图片读取失败"))?;
        if bytes.is_empty() {
            return Err(AppError::new(StatusCode::BAD_REQUEST, "图片不能为空"));
        }
        if bytes.len() > MAX_BACKGROUND_BYTES {
            return Err(AppError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "图片不能超过 5 MB",
            ));
        }
        if !background_signature_matches(extension, &bytes) {
            return Err(AppError::new(StatusCode::BAD_REQUEST, "图片文件格式不正确"));
        }
        image = Some((extension.to_string(), bytes.to_vec()));
        break;
    }

    let Some((extension, bytes)) = image else {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "请选择背景图片"));
    };

    let old_relative_path = setting_value(&state.db, BACKGROUND_SETTING_KEY).await?;
    let upload_dir = state.upload_dir.join(BACKGROUND_DIR);
    fs::create_dir_all(&upload_dir).await?;

    let filename = format!("{}.{}", Uuid::new_v4(), extension);
    let relative_path = PathBuf::from(BACKGROUND_DIR).join(&filename);
    let file_path = state.upload_dir.join(&relative_path);
    fs::write(&file_path, bytes).await?;

    let relative_path_text = relative_path.to_string_lossy().replace('\\', "/");
    sqlx::query(
        "INSERT INTO settings(key, value) VALUES($1, $2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(BACKGROUND_SETTING_KEY)
    .bind(&relative_path_text)
    .execute(&state.db)
    .await?;

    if let Some(old_path) = old_relative_path {
        remove_background_file(&state.upload_dir, &old_path).await;
    }

    write_action_log(
        &state.db,
        &admin.username,
        "update_background",
        "appearance",
        "更新站点背景图",
    )
    .await?;

    Ok(Json(SettingsResponse {
        retention_days: retention_days(&state.db).await?,
        background_image_url: path_to_upload_url(&relative_path_text),
    }))
}

pub async fn admin_delete_background_image(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<SettingsResponse>> {
    let admin = require_admin(&state, &headers).await?;
    let old_relative_path = setting_value(&state.db, BACKGROUND_SETTING_KEY).await?;
    sqlx::query("DELETE FROM settings WHERE key = $1")
        .bind(BACKGROUND_SETTING_KEY)
        .execute(&state.db)
        .await?;
    if let Some(old_path) = old_relative_path {
        remove_background_file(&state.upload_dir, &old_path).await;
    }
    write_action_log(
        &state.db,
        &admin.username,
        "clear_background",
        "appearance",
        "清除站点背景图",
    )
    .await?;
    Ok(Json(SettingsResponse {
        retention_days: retention_days(&state.db).await?,
        background_image_url: None,
    }))
}

async fn background_image_url(state: &AppState) -> AppResult<Option<String>> {
    Ok(setting_value(&state.db, BACKGROUND_SETTING_KEY)
        .await?
        .and_then(|path| path_to_upload_url(&path)))
}

fn path_to_upload_url(path: &str) -> Option<String> {
    let normalized = path.trim().replace('\\', "/");
    if normalized.is_empty() || normalized.contains("..") || normalized.starts_with('/') {
        return None;
    }
    Some(format!("/uploads/{}", normalized))
}

fn background_extension(content_type: &str) -> Option<&'static str> {
    match content_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

fn background_signature_matches(extension: &str, bytes: &[u8]) -> bool {
    match extension {
        "png" => bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
        "jpg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "webp" => bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    }
}

async fn remove_background_file(upload_dir: &FsPath, relative_path: &str) {
    if path_to_upload_url(relative_path).is_none() {
        return;
    }
    let path = upload_dir.join(relative_path);
    let _ = fs::remove_file(path).await;
}

pub async fn admin_commands(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<CommandRecord>>> {
    require_admin(&state, &headers).await?;
    let commands = sqlx::query_as::<_, CommandRecord>(
        r#"
        SELECT id, name, command, confirm_text, enabled, created_at
        FROM commands
        WHERE enabled = 1
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(commands))
}

pub async fn admin_create_command(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateCommandRequest>,
) -> AppResult<Json<CommandRecord>> {
    let admin = require_admin(&state, &headers).await?;
    let name = payload.name.trim();
    let command = payload.command.trim();
    if name.is_empty() || command.is_empty() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "命令名称和内容不能为空",
        ));
    }

    let record = CommandRecord {
        id: Uuid::new_v4().to_string(),
        name: name.to_string(),
        command: command.to_string(),
        confirm_text: payload.confirm_text.unwrap_or_default(),
        enabled: 1,
        created_at: now_ts(),
    };

    sqlx::query(
        "INSERT INTO commands(id, name, command, confirm_text, enabled, created_at) VALUES($1, $2, $3, $4, 1, $5)",
    )
    .bind(&record.id)
    .bind(&record.name)
    .bind(&record.command)
    .bind(&record.confirm_text)
    .bind(record.created_at)
    .execute(&state.db)
    .await?;

    write_action_log(
        &state.db,
        &admin.username,
        "create_command",
        &record.id,
        &record.name,
    )
    .await?;
    Ok(Json(record))
}

pub async fn admin_disable_command(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<AgentRegisterResponse>> {
    let admin = require_admin(&state, &headers).await?;
    sqlx::query("UPDATE commands SET enabled = 0 WHERE id = $1")
        .bind(&id)
        .execute(&state.db)
        .await?;
    write_action_log(
        &state.db,
        &admin.username,
        "disable_command",
        &id,
        "停用快捷操作",
    )
    .await?;
    Ok(Json(AgentRegisterResponse {
        approved: true,
        disabled: true,
        message: "快捷操作已停用".to_string(),
    }))
}

pub async fn admin_run_whitelist_command(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((instance_id, command_id)): Path<(String, String)>,
) -> AppResult<Json<CommandJobRecord>> {
    let admin = require_admin(&state, &headers).await?;

    let command = sqlx::query_as::<_, CommandRecord>(
        "SELECT id, name, command, confirm_text, enabled, created_at FROM commands WHERE id = $1 AND enabled = 1",
    )
    .bind(&command_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "快捷操作不存在"))?;

    let job = create_command_job(
        &state,
        Some(command.id),
        &instance_id,
        &command.command,
        &admin.username,
    )
    .await?;
    dispatch_command(&state, &job.id, &instance_id, &command.command).await?;
    write_action_log(
        &state.db,
        &admin.username,
        "run_command",
        &instance_id,
        &format!("执行快捷操作：{}", command.name),
    )
    .await?;
    Ok(Json(job))
}

pub async fn admin_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> AppResult<Json<Vec<CommandJobRecord>>> {
    require_admin(&state, &headers).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let jobs = sqlx::query_as::<_, CommandJobRecord>(
        r#"
        SELECT id, command_id, instance_id, command, status, requested_by, created_at,
               completed_at, output, exit_code
        FROM command_jobs
        ORDER BY created_at DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(jobs))
}

pub async fn admin_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> AppResult<Json<Vec<ActionLogRecord>>> {
    require_admin(&state, &headers).await?;
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let logs = sqlx::query_as::<_, ActionLogRecord>(
        r#"
        SELECT id, actor, action, target, detail, created_at
        FROM action_logs
        ORDER BY created_at DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(logs))
}

pub async fn agent_register(
    State(state): State<AppState>,
    Json(payload): Json<AgentRegisterRequest>,
) -> AppResult<Json<AgentRegisterResponse>> {
    register_or_touch_pending(&state.db, &payload).await?;

    let Some(instance) = get_instance_optional(&state.db, &payload.instance_id).await? else {
        return Ok(Json(AgentRegisterResponse {
            approved: false,
            disabled: false,
            message: "实例等待管理员审批".to_string(),
        }));
    };

    if instance.secret != payload.secret {
        return Err(AppError::new(StatusCode::UNAUTHORIZED, "实例密钥不匹配"));
    }

    Ok(Json(AgentRegisterResponse {
        approved: instance.approved == 1,
        disabled: instance.disabled == 1,
        message: if instance.disabled == 1 {
            "实例已被停用".to_string()
        } else {
            "实例已批准".to_string()
        },
    }))
}

pub async fn agent_report(
    State(state): State<AppState>,
    Json(payload): Json<AgentReportRequest>,
) -> AppResult<Json<AgentRegisterResponse>> {
    let register_payload = AgentRegisterRequest {
        instance_id: payload.instance_id.clone(),
        secret: payload.secret.clone(),
        hostname: payload.hostname.clone(),
        os: payload.os.clone(),
        arch: payload.arch.clone(),
        agent_version: payload.agent_version.clone(),
        package_type: payload.package_type.clone(),
        native_arch: payload.native_arch.clone(),
        update_privileged: payload.update_privileged,
    };
    register_or_touch_pending(&state.db, &register_payload).await?;

    let Some(instance) = get_instance_optional(&state.db, &payload.instance_id).await? else {
        return Ok(Json(AgentRegisterResponse {
            approved: false,
            disabled: false,
            message: "实例等待管理员审批".to_string(),
        }));
    };

    if instance.secret != payload.secret {
        return Err(AppError::new(StatusCode::UNAUTHORIZED, "实例密钥不匹配"));
    }
    if instance.disabled == 1 {
        return Ok(Json(AgentRegisterResponse {
            approved: true,
            disabled: true,
            message: "实例已被停用".to_string(),
        }));
    }

    sqlx::query(
        r#"
        UPDATE instances
        SET hostname = $1, os = $2, arch = $3, agent_version = $4,
            package_type = COALESCE($5, package_type),
            native_arch = COALESCE($6, native_arch),
            update_privileged = COALESCE($7, update_privileged), last_seen = $8
        WHERE id = $9
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
    .execute(&state.db)
    .await?;

    confirm_update_version(&state, &payload.instance_id, &payload.agent_version).await?;

    sqlx::query(
        r#"
        INSERT INTO metrics(instance_id, ts, cpu_percent, memory_used, memory_total,
                            disk_used, disk_total, network_rx, network_tx, gpu_percent,
                            gpu_memory_used, gpu_memory_total, uptime_seconds, load_average)
        VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        "#,
    )
    .bind(&payload.instance_id)
    .bind(payload.metrics.ts)
    .bind(payload.metrics.cpu_percent)
    .bind(payload.metrics.memory_used)
    .bind(payload.metrics.memory_total)
    .bind(payload.metrics.disk_used)
    .bind(payload.metrics.disk_total)
    .bind(payload.metrics.network_rx)
    .bind(payload.metrics.network_tx)
    .bind(payload.metrics.gpu_percent)
    .bind(payload.metrics.gpu_memory_used)
    .bind(payload.metrics.gpu_memory_total)
    .bind(payload.metrics.uptime_seconds)
    .bind(payload.metrics.load_average)
    .execute(&state.db)
    .await?;

    Ok(Json(AgentRegisterResponse {
        approved: true,
        disabled: false,
        message: "指标已接收".to_string(),
    }))
}

pub async fn agent_ws(
    State(state): State<AppState>,
    Query(query): Query<AgentWsQuery>,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    let instance = get_instance(&state.db, &query.instance_id).await?;
    if instance.secret != query.secret {
        return Err(AppError::new(StatusCode::UNAUTHORIZED, "实例密钥不匹配"));
    }
    if instance.disabled == 1 {
        return Err(AppError::new(StatusCode::FORBIDDEN, "实例已停用"));
    }

    Ok(ws.on_upgrade(move |socket| agent_socket(state, query.instance_id, socket)))
}

pub async fn admin_terminal_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance_id): Path<String>,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    let admin = require_admin(&state, &headers).await?;
    get_instance(&state.db, &instance_id).await?;
    Ok(ws.on_upgrade(move |socket| terminal_socket(state, instance_id, admin.username, socket)))
}
