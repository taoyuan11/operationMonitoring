use std::path::Path as FsPath;
use std::time::Duration;

use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use semver::Version;
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
};
use tokio_util::io::ReaderStream;
use tracing::warn;
use uuid::Uuid;

use crate::{
    auth::require_admin,
    db::{get_instance, write_action_log},
    error::{AppError, AppResult},
    models::{
        AgentArtifactRecord, AgentOutbound, AgentReleaseCoverage, AgentReleaseDetail,
        AgentReleaseRecord, AgentUpdateAttemptRecord, AgentUpdateManifest, AgentUpdateOffer,
        CreateAgentReleaseRequest, InstanceRecord, UpdateAttemptsQuery,
    },
    state::AppState,
    utils::now_ts,
};

const MAX_METADATA_BYTES: usize = 1024;
const MAX_CHECKSUM_FILE_BYTES: usize = 4096;
const MAX_STATUS_MESSAGE_BYTES: usize = 4096;
// Covers parent exit, target/rollback install and service-restart timeouts, health checks, and I/O.
const UPDATE_HANDOFF_TIMEOUT_SECONDS: i64 = 60 * 60;
const TERMINAL_ATTEMPT_STATUSES: [&str; 3] = ["succeeded", "rollback_succeeded", "failed"];

#[derive(FromRow)]
struct InstanceCapabilityRow {
    id: String,
    os: String,
    agent_version: String,
    package_type: String,
    native_arch: String,
    update_privileged: i64,
}

#[derive(FromRow)]
struct UpdateCandidate {
    release_id: String,
    version: String,
    artifact_id: String,
    package_type: String,
    native_arch: String,
    sha256: String,
    size_bytes: i64,
}

#[derive(FromRow)]
struct RetriedUpdateCandidate {
    attempt_id: String,
    release_id: String,
    version: String,
    artifact_id: String,
    os: String,
    package_type: String,
    native_arch: String,
    sha256: String,
    size_bytes: i64,
    retry_count: i64,
}

struct ReceivedArtifact {
    os: String,
    package_type: String,
    native_arch: String,
    file_name: String,
    size_bytes: i64,
    sha256: String,
    checksum_file_name: String,
    checksum_contents: String,
    first_bytes: Vec<u8>,
    temp_path: std::path::PathBuf,
}

pub async fn admin_agent_releases(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<AgentReleaseDetail>>> {
    require_admin(&state, &headers).await?;
    let releases = sqlx::query_as::<_, AgentReleaseRecord>(
        r#"
        SELECT id, version, notes, status, created_at, published_at
        FROM agent_releases
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(&state.db)
    .await?;

    let mut details = Vec::with_capacity(releases.len());
    for release in releases {
        details.push(load_release_detail(&state, release).await?);
    }
    Ok(Json(details))
}

pub async fn admin_create_agent_release(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateAgentReleaseRequest>,
) -> AppResult<(StatusCode, Json<AgentReleaseDetail>)> {
    let admin = require_admin(&state, &headers).await?;
    let version = validate_version(&payload.version)?;
    let id = Uuid::new_v4().to_string();
    let created_at = now_ts();
    let result = sqlx::query(
        "INSERT INTO agent_releases(id, version, notes, status, created_at) VALUES(?, ?, ?, 'draft', ?)",
    )
    .bind(&id)
    .bind(&version)
    .bind(payload.notes.trim())
    .bind(created_at)
    .execute(&state.db)
    .await;
    let _ = map_unique_conflict(result, "该 Agent 版本已存在")?;

    write_action_log(
        &state.db,
        &admin.username,
        "create_agent_release",
        &id,
        &format!("创建 Agent 版本 {version}"),
    )
    .await?;
    let release = get_release(&state, &id).await?;
    Ok((
        StatusCode::CREATED,
        Json(load_release_detail(&state, release).await?),
    ))
}

pub async fn admin_update_agent_release(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(release_id): Path<String>,
    Json(payload): Json<CreateAgentReleaseRequest>,
) -> AppResult<Json<AgentReleaseDetail>> {
    let admin = require_admin(&state, &headers).await?;
    require_draft_release(&state, &release_id).await?;
    let version = validate_version(&payload.version)?;
    let result = sqlx::query(
        "UPDATE agent_releases SET version = ?, notes = ? WHERE id = ? AND status = 'draft'",
    )
    .bind(&version)
    .bind(payload.notes.trim())
    .bind(&release_id)
    .execute(&state.db)
    .await;
    let updated = map_unique_conflict(result, "该 Agent 版本已存在")?;
    if updated.rows_affected() != 1 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "已发布的 Agent 版本不能修改",
        ));
    }

    write_action_log(
        &state.db,
        &admin.username,
        "update_agent_release",
        &release_id,
        &format!("更新 Agent 草稿 {version}"),
    )
    .await?;
    let release = get_release(&state, &release_id).await?;
    Ok(Json(load_release_detail(&state, release).await?))
}

pub async fn admin_delete_agent_release(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(release_id): Path<String>,
) -> AppResult<StatusCode> {
    let admin = require_admin(&state, &headers).await?;
    require_draft_release(&state, &release_id).await?;
    let deleted = sqlx::query("DELETE FROM agent_releases WHERE id = ? AND status = 'draft'")
        .bind(&release_id)
        .execute(&state.db)
        .await?;
    if deleted.rows_affected() != 1 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "已发布的 Agent 版本不能删除",
        ));
    }
    let _ = fs::remove_dir_all(state.update_dir.join(&release_id)).await;
    write_action_log(
        &state.db,
        &admin.username,
        "delete_agent_release",
        &release_id,
        "删除 Agent 更新草稿",
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn admin_upload_agent_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(release_id): Path<String>,
    multipart: Multipart,
) -> AppResult<(StatusCode, Json<AgentArtifactRecord>)> {
    let admin = require_admin(&state, &headers).await?;
    require_draft_release(&state, &release_id).await?;
    let received = receive_artifact(&state, multipart).await?;
    let result = store_artifact(&state, &release_id, received).await;
    let artifact = result?;

    write_action_log(
        &state.db,
        &admin.username,
        "upload_agent_artifact",
        &artifact.id,
        &format!(
            "上传 {} {} {} Agent 可执行文件及 SHA-256 校验文件",
            artifact.os, artifact.package_type, artifact.native_arch
        ),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(artifact)))
}

pub async fn admin_delete_agent_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((release_id, artifact_id)): Path<(String, String)>,
) -> AppResult<StatusCode> {
    let admin = require_admin(&state, &headers).await?;
    require_draft_release(&state, &release_id).await?;
    let artifact = get_artifact(&state, &artifact_id)
        .await?
        .filter(|artifact| artifact.release_id == release_id)
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Agent 可执行文件不存在"))?;

    let deleted = sqlx::query(
        r#"
        DELETE FROM agent_artifacts
        WHERE id = ? AND release_id = ?
          AND EXISTS (
              SELECT 1 FROM agent_releases
              WHERE id = ? AND status = 'draft'
          )
        "#,
    )
    .bind(&artifact_id)
    .bind(&release_id)
    .bind(&release_id)
    .execute(&state.db)
    .await?;
    if deleted.rows_affected() != 1 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "已发布版本的可执行文件不能删除",
        ));
    }
    remove_stored_file(&state, &artifact.storage_path).await;
    remove_stored_file(&state, &format!("{}.sha256", artifact.storage_path)).await;
    write_action_log(
        &state.db,
        &admin.username,
        "delete_agent_artifact",
        &artifact_id,
        "删除 Agent 可执行文件及 SHA-256 校验文件",
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn admin_publish_agent_release(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(release_id): Path<String>,
) -> AppResult<Json<AgentReleaseDetail>> {
    let admin = require_admin(&state, &headers).await?;
    let release = require_draft_release(&state, &release_id).await?;
    let target = Version::parse(&release.version)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "版本号不是有效的 SemVer"))?;
    let instances = capability_instances(&state).await?;
    let now = now_ts();
    let mut transaction = state.db.begin().await?;
    let updated = sqlx::query(
        "UPDATE agent_releases SET status = 'published', published_at = ? WHERE id = ? AND status = 'draft'",
    )
    .bind(now)
    .bind(&release_id)
    .execute(&mut *transaction)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(AppError::new(StatusCode::CONFLICT, "Agent 版本已发布"));
    }
    let artifacts = sqlx::query_as::<_, AgentArtifactRecord>(
        r#"
        SELECT id, release_id, os, package_type, native_arch, file_name, size_bytes, sha256,
               storage_path, created_at
        FROM agent_artifacts WHERE release_id = ? ORDER BY os, package_type, native_arch
        "#,
    )
    .bind(&release_id)
    .fetch_all(&mut *transaction)
    .await?;
    if artifacts.is_empty() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "至少上传一个可执行文件后才能发布",
        ));
    }

    for instance in instances {
        if instance.update_privileged != 1 || !version_is_newer(&target, &instance.agent_version) {
            continue;
        }
        let Some(artifact) = artifacts.iter().find(|artifact| {
            target_matches(
                &instance.os,
                &instance.package_type,
                &instance.native_arch,
                artifact,
            )
        }) else {
            continue;
        };
        sqlx::query(
            r#"
            INSERT OR IGNORE INTO agent_update_attempts(
                id, release_id, artifact_id, instance_id, from_version, target_version,
                status, message, retry_count, created_at, updated_at
            ) VALUES(?, ?, ?, ?, ?, ?, 'pending', '', 0, ?, ?)
            "#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&release_id)
        .bind(&artifact.id)
        .bind(&instance.id)
        .bind(&instance.agent_version)
        .bind(&release.version)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;

    write_action_log(
        &state.db,
        &admin.username,
        "publish_agent_release",
        &release_id,
        &format!("发布 Agent 版本 {}", release.version),
    )
    .await?;
    notify_release_instances(&state, &release_id).await;
    let published = get_release(&state, &release_id).await?;
    Ok(Json(load_release_detail(&state, published).await?))
}

pub async fn admin_agent_update_attempts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UpdateAttemptsQuery>,
) -> AppResult<Json<Vec<AgentUpdateAttemptRecord>>> {
    require_admin(&state, &headers).await?;
    let attempts = if let Some(release_id) = query.release_id {
        sqlx::query_as::<_, AgentUpdateAttemptRecord>(
            r#"
            SELECT id, release_id, artifact_id, instance_id, from_version, target_version,
                   status, message, retry_count, created_at, updated_at, completed_at
            FROM agent_update_attempts WHERE release_id = ? ORDER BY updated_at DESC
            "#,
        )
        .bind(release_id)
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as::<_, AgentUpdateAttemptRecord>(
            r#"
            SELECT id, release_id, artifact_id, instance_id, from_version, target_version,
                   status, message, retry_count, created_at, updated_at, completed_at
            FROM agent_update_attempts ORDER BY updated_at DESC LIMIT 1000
            "#,
        )
        .fetch_all(&state.db)
        .await?
    };
    Ok(Json(attempts))
}

pub async fn admin_retry_agent_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(attempt_id): Path<String>,
) -> AppResult<Json<AgentUpdateAttemptRecord>> {
    let admin = require_admin(&state, &headers).await?;
    let attempt = get_attempt(&state, &attempt_id).await?;
    if !matches!(attempt.status.as_str(), "failed" | "rollback_succeeded") {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "只有失败或已回滚的更新可以重试",
        ));
    }
    require_latest_retry_candidate(&state, &attempt).await?;
    let now = now_ts();
    let retried = sqlx::query(
        r#"
        UPDATE agent_update_attempts
        SET status = 'pending', message = '', retry_count = retry_count + 1,
            updated_at = ?, completed_at = NULL
        WHERE id = ? AND status = ? AND retry_count = ?
        "#,
    )
    .bind(now)
    .bind(&attempt_id)
    .bind(&attempt.status)
    .bind(attempt.retry_count)
    .execute(&state.db)
    .await?;
    if retried.rows_affected() != 1 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "更新状态已发生变化，请刷新后重试",
        ));
    }
    write_action_log(
        &state.db,
        &admin.username,
        "retry_agent_update",
        &attempt_id,
        &format!("重试实例 {} 的 Agent 更新", attempt.instance_id),
    )
    .await?;
    notify_retried_attempt(&state, &attempt.instance_id, &attempt_id).await;
    Ok(Json(get_attempt(&state, &attempt_id).await?))
}

pub async fn agent_update_manifest(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<AgentUpdateManifest>> {
    let instance = authenticate_agent_headers(&state, &headers).await?;
    let update = find_update_for_instance(&state, &instance).await?;
    Ok(Json(AgentUpdateManifest { update }))
}

pub async fn agent_download_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(artifact_id): Path<String>,
) -> AppResult<Response> {
    let artifact = authorized_artifact_download(&state, &headers, &artifact_id).await?;

    let path = safe_storage_path(&state, &artifact.storage_path)?;
    let file = File::open(&path).await.map_err(|error| {
        warn!(?error, path = %path.display(), "agent artifact file is missing");
        AppError::new(StatusCode::NOT_FOUND, "Agent 可执行文件文件不存在")
    })?;
    let actual_size = file.metadata().await?.len();
    if actual_size != artifact.size_bytes as u64 {
        return Err(AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Agent 可执行文件大小校验失败",
        ));
    }
    let extension = FsPath::new(&artifact.file_name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("bin");
    let disposition = format!(
        "attachment; filename=\"agent-update-{}.{}\"",
        artifact.id, extension
    );
    let body = Body::from_stream(ReaderStream::new(file));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, artifact.size_bytes)
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(body)
        .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "下载响应生成失败"))
}

pub async fn agent_download_artifact_checksum(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(artifact_id): Path<String>,
) -> AppResult<Response> {
    let artifact = authorized_artifact_download(&state, &headers, &artifact_id).await?;
    let checksum_storage_path = format!("{}.sha256", artifact.storage_path);
    let path = safe_storage_path(&state, &checksum_storage_path)?;
    let file = File::open(&path).await.map_err(|error| {
        warn!(?error, path = %path.display(), "agent artifact checksum file is missing");
        AppError::new(StatusCode::NOT_FOUND, "Agent SHA-256 校验文件不存在")
    })?;
    let size = file.metadata().await?.len();
    if size == 0 || size > MAX_CHECKSUM_FILE_BYTES as u64 {
        return Err(AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Agent SHA-256 校验文件大小无效",
        ));
    }
    let disposition = format!(
        "attachment; filename=\"agent-update-{}.sha256\"",
        artifact.id
    );
    let body = Body::from_stream(ReaderStream::new(file));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CONTENT_LENGTH, size)
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(body)
        .map_err(|_| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "SHA-256 下载响应生成失败",
            )
        })
}

async fn authorized_artifact_download(
    state: &AppState,
    headers: &HeaderMap,
    artifact_id: &str,
) -> AppResult<AgentArtifactRecord> {
    let instance = authenticate_agent_headers(state, headers).await?;
    let artifact = sqlx::query_as::<_, AgentArtifactRecord>(
        r#"
        SELECT a.id, a.release_id, a.os, a.package_type, a.native_arch, a.file_name,
               a.size_bytes, a.sha256, a.storage_path, a.created_at
        FROM agent_artifacts a
        JOIN agent_releases r ON r.id = a.release_id
        WHERE a.id = ? AND r.status = 'published'
        "#,
    )
    .bind(artifact_id)
    .fetch_optional(&state.db)
    .await?
    .filter(|artifact| {
        instance.update_privileged == 1
            && target_matches(
                &instance.os,
                &instance.package_type,
                &instance.native_arch,
                artifact,
            )
    })
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "没有适用于该实例的可执行文件"))?;

    let attempt_exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_update_attempts WHERE artifact_id = ? AND instance_id = ?",
    )
    .bind(artifact_id)
    .bind(&instance.id)
    .fetch_one(&state.db)
    .await?;
    if attempt_exists == 0 {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "该实例没有待执行的更新",
        ));
    }

    Ok(artifact)
}

pub async fn offer_update_on_connect(state: &AppState, instance_id: &str) {
    notify_instance(state, instance_id).await;
}

pub async fn record_update_status(
    state: &AppState,
    instance_id: &str,
    release_id: &str,
    artifact_id: &str,
    version: &str,
    retry_count: i64,
    status: &str,
    message: Option<&str>,
) -> AppResult<()> {
    if !valid_agent_status(status) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "未知的 Agent 更新状态",
        ));
    }
    let message = message.unwrap_or_default();
    if message.len() > MAX_STATUS_MESSAGE_BYTES {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Agent 更新状态信息过长",
        ));
    }
    let completed_at = TERMINAL_ATTEMPT_STATUSES
        .contains(&status)
        .then_some(now_ts());
    let result = sqlx::query(
        r#"
        UPDATE agent_update_attempts
        SET status = ?, message = ?, updated_at = ?, completed_at = ?
        WHERE instance_id = ? AND release_id = ? AND artifact_id = ? AND target_version = ?
          AND retry_count = ?
        "#,
    )
    .bind(status)
    .bind(message)
    .bind(now_ts())
    .bind(completed_at)
    .bind(instance_id)
    .bind(release_id)
    .bind(artifact_id)
    .bind(version)
    .bind(retry_count)
    .execute(&state.db)
    .await?;
    if result.rows_affected() != 1 {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            "Agent 更新任务不存在或信息不匹配",
        ));
    }
    if status == "succeeded" {
        sqlx::query("UPDATE instances SET agent_version = ? WHERE id = ?")
            .bind(version)
            .bind(instance_id)
            .execute(&state.db)
            .await?;
    }
    Ok(())
}

pub async fn confirm_update_version(
    state: &AppState,
    instance_id: &str,
    agent_version: &str,
) -> AppResult<()> {
    let now = now_ts();
    sqlx::query(
        r#"
        UPDATE agent_update_attempts
        SET status = 'succeeded', message = '', updated_at = ?, completed_at = ?
        WHERE instance_id = ? AND target_version = ?
          AND status NOT IN ('succeeded', 'rollback_succeeded', 'failed')
        "#,
    )
    .bind(now)
    .bind(now)
    .bind(instance_id)
    .bind(agent_version)
    .execute(&state.db)
    .await?;
    Ok(())
}

pub async fn update_timeout_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if let Err(error) = expire_restart_attempts(&state).await {
            warn!(?error, "failed to expire timed-out agent updates");
        }
    }
}

async fn expire_restart_attempts(state: &AppState) -> AppResult<u64> {
    let now = now_ts();
    let result = sqlx::query(
        r#"
        UPDATE agent_update_attempts
        SET status = 'failed', message = '更新进程启动后 60 分钟内未完成重连', updated_at = ?, completed_at = ?
        WHERE status IN ('installing', 'awaiting_restart') AND updated_at < ?
        "#,
    )
    .bind(now)
    .bind(now)
    .bind(now - UPDATE_HANDOFF_TIMEOUT_SECONDS)
    .execute(&state.db)
    .await?;
    Ok(result.rows_affected())
}

async fn receive_artifact(
    state: &AppState,
    mut multipart: Multipart,
) -> AppResult<ReceivedArtifact> {
    let temp_dir = state.update_dir.join(".tmp");
    fs::create_dir_all(&temp_dir).await?;
    let temp_path = temp_dir.join(format!("{}.upload", Uuid::new_v4()));
    let result = async {
        let mut os = None;
        let mut package_type = None;
        let mut native_arch = None;
        let mut file_name = None;
        let mut checksum_file_name = None;
        let mut checksum_contents = None;
        let mut size_bytes = 0_i64;
        let mut first_bytes = Vec::with_capacity(16);
        let mut digest = Sha256::new();
        let mut received_file = false;

        while let Some(mut field) = multipart
            .next_field()
            .await
            .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "无法读取上传表单"))?
        {
            let name = field.name().unwrap_or_default().to_string();
            match name.as_str() {
                "os" | "package_type" | "native_arch" => {
                    let value = field.text().await.map_err(|_| {
                        AppError::new(StatusCode::BAD_REQUEST, "无法读取可执行文件元数据")
                    })?;
                    if value.len() > MAX_METADATA_BYTES {
                        return Err(AppError::new(
                            StatusCode::BAD_REQUEST,
                            "可执行文件元数据过长",
                        ));
                    }
                    match name.as_str() {
                        "os" => os = Some(value),
                        "package_type" => package_type = Some(value),
                        "native_arch" => native_arch = Some(value),
                        _ => unreachable!(),
                    }
                }
                "file" => {
                    if received_file {
                        return Err(AppError::new(
                            StatusCode::BAD_REQUEST,
                            "只能上传一个可执行文件",
                        ));
                    }
                    let supplied_name = field
                        .file_name()
                        .and_then(|name| FsPath::new(name).file_name())
                        .and_then(|name| name.to_str())
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| {
                            AppError::new(StatusCode::BAD_REQUEST, "可执行文件文件名无效")
                        })?
                        .to_string();
                    let mut file = File::create(&temp_path).await?;
                    while let Some(chunk) = field
                        .chunk()
                        .await
                        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "可执行文件上传中断"))?
                    {
                        size_bytes =
                            size_bytes.checked_add(chunk.len() as i64).ok_or_else(|| {
                                AppError::new(StatusCode::PAYLOAD_TOO_LARGE, "可执行文件过大")
                            })?;
                        if size_bytes as usize > state.agent_package_max_bytes {
                            return Err(AppError::new(
                                StatusCode::PAYLOAD_TOO_LARGE,
                                "可执行文件超过大小限制",
                            ));
                        }
                        if first_bytes.len() < 16 {
                            let take = (16 - first_bytes.len()).min(chunk.len());
                            first_bytes.extend_from_slice(&chunk[..take]);
                        }
                        digest.update(&chunk);
                        file.write_all(&chunk).await?;
                    }
                    file.flush().await?;
                    file.sync_all().await?;
                    file_name = Some(supplied_name);
                    received_file = true;
                }
                "checksum_file" => {
                    if checksum_file_name.is_some() {
                        return Err(AppError::new(
                            StatusCode::BAD_REQUEST,
                            "只能上传一个 SHA-256 校验文件",
                        ));
                    }
                    let supplied_name = field
                        .file_name()
                        .and_then(|name| FsPath::new(name).file_name())
                        .and_then(|name| name.to_str())
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| {
                            AppError::new(StatusCode::BAD_REQUEST, "SHA-256 校验文件名无效")
                        })?
                        .to_string();
                    let mut contents = Vec::new();
                    while let Some(chunk) = field.chunk().await.map_err(|_| {
                        AppError::new(StatusCode::BAD_REQUEST, "无法读取 SHA-256 校验文件")
                    })? {
                        if contents.len() + chunk.len() > MAX_CHECKSUM_FILE_BYTES {
                            return Err(AppError::new(
                                StatusCode::PAYLOAD_TOO_LARGE,
                                "SHA-256 校验文件过大",
                            ));
                        }
                        contents.extend_from_slice(&chunk);
                    }
                    let contents = String::from_utf8(contents).map_err(|_| {
                        AppError::new(StatusCode::BAD_REQUEST, "SHA-256 校验文件必须是文本文件")
                    })?;
                    checksum_file_name = Some(supplied_name);
                    checksum_contents = Some(contents);
                }
                _ => {
                    return Err(AppError::new(
                        StatusCode::BAD_REQUEST,
                        "上传表单包含未知字段",
                    ));
                }
            }
        }
        if !received_file || size_bytes == 0 {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "可执行文件文件不能为空",
            ));
        }
        if checksum_file_name.is_none() {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "缺少 .sha256 校验文件",
            ));
        }
        Ok(ReceivedArtifact {
            os: os.ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "缺少目标系统"))?,
            package_type: package_type
                .ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "缺少可执行文件类型"))?,
            native_arch: native_arch
                .ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "缺少原生架构"))?,
            file_name: file_name.expect("file name exists after received_file check"),
            size_bytes,
            sha256: format!("{:x}", digest.finalize()),
            checksum_file_name: checksum_file_name.expect("checksum file name exists after check"),
            checksum_contents: checksum_contents.expect("checksum contents exists after check"),
            first_bytes,
            temp_path: temp_path.clone(),
        })
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temp_path).await;
    }
    result
}

async fn store_artifact(
    state: &AppState,
    release_id: &str,
    mut received: ReceivedArtifact,
) -> AppResult<AgentArtifactRecord> {
    let validation = validate_artifact_metadata(&mut received);
    if let Err(error) = validation {
        let _ = fs::remove_file(&received.temp_path).await;
        return Err(error);
    }
    let duplicate: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM agent_artifacts
        WHERE release_id = ? AND os = ? AND package_type = ? AND native_arch = ?
        "#,
    )
    .bind(release_id)
    .bind(&received.os)
    .bind(&received.package_type)
    .bind(&received.native_arch)
    .fetch_one(&state.db)
    .await?;
    if duplicate > 0 {
        let _ = fs::remove_file(&received.temp_path).await;
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "该版本已包含相同目标的可执行文件",
        ));
    }

    let id = Uuid::new_v4().to_string();
    let extension = expected_extension(&received.os);
    let relative_path = format!("{release_id}/{id}.{extension}");
    let checksum_relative_path = format!("{relative_path}.sha256");
    let final_dir = state.update_dir.join(release_id);
    let final_path = state.update_dir.join(&relative_path);
    let checksum_final_path = state.update_dir.join(&checksum_relative_path);
    fs::create_dir_all(&final_dir).await?;
    fs::rename(&received.temp_path, &final_path).await?;
    if let Err(error) = fs::write(&checksum_final_path, &received.checksum_contents).await {
        let _ = fs::remove_file(&final_path).await;
        return Err(error.into());
    }

    let artifact = AgentArtifactRecord {
        id,
        release_id: release_id.to_string(),
        os: received.os,
        package_type: received.package_type,
        native_arch: received.native_arch,
        file_name: received.file_name,
        size_bytes: received.size_bytes,
        sha256: received.sha256,
        storage_path: relative_path,
        created_at: now_ts(),
    };
    let result = sqlx::query(
        r#"
        INSERT INTO agent_artifacts(id, release_id, os, package_type, native_arch, file_name,
                                    size_bytes, sha256, storage_path, created_at)
        SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
        FROM agent_releases
        WHERE id = ? AND status = 'draft'
        "#,
    )
    .bind(&artifact.id)
    .bind(&artifact.release_id)
    .bind(&artifact.os)
    .bind(&artifact.package_type)
    .bind(&artifact.native_arch)
    .bind(&artifact.file_name)
    .bind(artifact.size_bytes)
    .bind(&artifact.sha256)
    .bind(&artifact.storage_path)
    .bind(artifact.created_at)
    .bind(release_id)
    .execute(&state.db)
    .await;
    let inserted = match result {
        Ok(inserted) => inserted,
        Err(error) => {
            let _ = fs::remove_file(&final_path).await;
            let _ = fs::remove_file(&checksum_final_path).await;
            if error
                .as_database_error()
                .is_some_and(|error| error.is_unique_violation())
            {
                return Err(AppError::new(
                    StatusCode::CONFLICT,
                    "该版本已包含相同目标的可执行文件",
                ));
            }
            return Err(error.into());
        }
    };
    if inserted.rows_affected() != 1 {
        let _ = fs::remove_file(&final_path).await;
        let _ = fs::remove_file(&checksum_final_path).await;
        require_draft_release(state, release_id).await?;
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "Agent 版本已在上传过程中发布",
        ));
    }
    Ok(artifact)
}

fn validate_artifact_metadata(received: &mut ReceivedArtifact) -> AppResult<()> {
    received.os = received.os.trim().to_ascii_lowercase();
    received.package_type = received.package_type.trim().to_ascii_lowercase();
    received.native_arch = received.native_arch.trim().to_string();
    if received.package_type != "standalone" {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "仅支持 standalone 可执行文件",
        ));
    }
    if !matches!(received.os.as_str(), "linux" | "windows" | "macos") {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "不支持的目标系统"));
    }
    if received.native_arch.is_empty()
        || received.native_arch.len() > 64
        || !received
            .native_arch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "原生架构名称无效"));
    }
    let extension = FsPath::new(&received.file_name)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "可执行文件扩展名无效"))?;
    if extension != expected_extension(&received.os) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "可执行文件扩展名与目标系统不匹配",
        ));
    }
    if !package_signature_matches(&received.os, &received.first_bytes) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "可执行文件签名与目标系统不匹配",
        ));
    }
    validate_checksum_file(
        &received.file_name,
        &received.checksum_file_name,
        &received.checksum_contents,
        &received.sha256,
    )?;
    Ok(())
}

fn validate_checksum_file(
    file_name: &str,
    checksum_file_name: &str,
    checksum_contents: &str,
    actual_sha256: &str,
) -> AppResult<()> {
    let expected_file_name = format!("{file_name}.sha256");
    if !checksum_file_name.eq_ignore_ascii_case(&expected_file_name) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "SHA-256 校验文件名必须与可执行文件匹配",
        ));
    }
    let mut fields = checksum_contents.split_whitespace();
    let supplied_sha256 = fields.next().unwrap_or_default();
    if supplied_sha256.len() != 64
        || !supplied_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !supplied_sha256.eq_ignore_ascii_case(actual_sha256)
    {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "SHA-256 校验文件内容与可执行文件不匹配",
        ));
    }
    if let Some(supplied_name) = fields.next() {
        if supplied_name.trim_start_matches('*') != file_name {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "SHA-256 校验文件中的文件名不匹配",
            ));
        }
        if fields.next().is_some() {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "SHA-256 校验文件格式无效",
            ));
        }
    }
    Ok(())
}

fn expected_extension(os: &str) -> &'static str {
    if os == "windows" { "exe" } else { "bin" }
}

fn package_signature_matches(os: &str, bytes: &[u8]) -> bool {
    match os {
        "windows" => bytes.starts_with(b"MZ"),
        "linux" => bytes.starts_with(&[0x7f, b'E', b'L', b'F']),
        "macos" => {
            bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe])
                || bytes.starts_with(&[0xfe, 0xed, 0xfa, 0xcf])
                || bytes.starts_with(&[0xca, 0xfe, 0xba, 0xbe])
                || bytes.starts_with(&[0xca, 0xfe, 0xba, 0xbf])
        }
        _ => false,
    }
}

fn validate_version(raw: &str) -> AppResult<String> {
    let version = raw.trim();
    let parsed = Version::parse(version)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "版本号必须是有效的 SemVer"))?;
    if parsed.to_string() != version {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "版本号必须使用规范的 SemVer 格式",
        ));
    }
    Ok(version.to_string())
}

fn version_is_newer(target: &Version, current: &str) -> bool {
    Version::parse(current)
        .map(|current| target > &current)
        .unwrap_or(true)
}

fn valid_agent_status(status: &str) -> bool {
    matches!(
        status,
        "waiting"
            | "downloading"
            | "verifying"
            | "waiting_idle"
            | "installing"
            | "awaiting_restart"
            | "succeeded"
            | "rollback_succeeded"
            | "failed"
    )
}

async fn get_release(state: &AppState, release_id: &str) -> AppResult<AgentReleaseRecord> {
    sqlx::query_as::<_, AgentReleaseRecord>(
        "SELECT id, version, notes, status, created_at, published_at FROM agent_releases WHERE id = ?",
    )
    .bind(release_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Agent 版本不存在"))
}

async fn require_draft_release(
    state: &AppState,
    release_id: &str,
) -> AppResult<AgentReleaseRecord> {
    let release = get_release(state, release_id).await?;
    if release.status != "draft" {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "已发布的 Agent 版本不能修改",
        ));
    }
    Ok(release)
}

async fn release_artifacts(
    state: &AppState,
    release_id: &str,
) -> AppResult<Vec<AgentArtifactRecord>> {
    Ok(sqlx::query_as::<_, AgentArtifactRecord>(
        r#"
        SELECT id, release_id, os, package_type, native_arch, file_name, size_bytes, sha256,
               storage_path, created_at
        FROM agent_artifacts WHERE release_id = ? ORDER BY os, package_type, native_arch
        "#,
    )
    .bind(release_id)
    .fetch_all(&state.db)
    .await?)
}

async fn get_artifact(
    state: &AppState,
    artifact_id: &str,
) -> AppResult<Option<AgentArtifactRecord>> {
    Ok(sqlx::query_as::<_, AgentArtifactRecord>(
        r#"
        SELECT id, release_id, os, package_type, native_arch, file_name, size_bytes, sha256,
               storage_path, created_at
        FROM agent_artifacts WHERE id = ?
        "#,
    )
    .bind(artifact_id)
    .fetch_optional(&state.db)
    .await?)
}

async fn load_release_detail(
    state: &AppState,
    release: AgentReleaseRecord,
) -> AppResult<AgentReleaseDetail> {
    let artifacts = release_artifacts(state, &release.id).await?;
    let attempts = sqlx::query_as::<_, AgentUpdateAttemptRecord>(
        r#"
        SELECT id, release_id, artifact_id, instance_id, from_version, target_version,
               status, message, retry_count, created_at, updated_at, completed_at
        FROM agent_update_attempts WHERE release_id = ? ORDER BY updated_at DESC
        "#,
    )
    .bind(&release.id)
    .fetch_all(&state.db)
    .await?;
    let instances = capability_instances(state).await?;
    let mut eligible_instances = 0;
    let mut covered_instances = 0;
    let mut missing_artifact_instances = 0;
    let mut unprivileged_instances = 0;
    for instance in instances {
        if update_target_os(&instance.package_type, &instance.os).is_none()
            || instance.native_arch.is_empty()
        {
            continue;
        }
        eligible_instances += 1;
        if instance.update_privileged != 1 {
            unprivileged_instances += 1;
        }
        if artifacts.iter().any(|artifact| {
            target_matches(
                &instance.os,
                &instance.package_type,
                &instance.native_arch,
                artifact,
            )
        }) {
            covered_instances += 1;
        } else {
            missing_artifact_instances += 1;
        }
    }
    Ok(AgentReleaseDetail {
        release,
        artifacts,
        attempts,
        coverage: AgentReleaseCoverage {
            eligible_instances,
            covered_instances,
            missing_artifact_instances,
            unprivileged_instances,
        },
    })
}

async fn capability_instances(state: &AppState) -> AppResult<Vec<InstanceCapabilityRow>> {
    Ok(sqlx::query_as::<_, InstanceCapabilityRow>(
        r#"
        SELECT id, os, agent_version, package_type, native_arch, update_privileged
        FROM instances WHERE approved = 1 AND disabled = 0
        "#,
    )
    .fetch_all(&state.db)
    .await?)
}

fn target_matches(
    _reported_os: &str,
    package_type: &str,
    native_arch: &str,
    artifact: &AgentArtifactRecord,
) -> bool {
    update_target_os(package_type, _reported_os).is_some_and(|os| artifact.os == os)
        && artifact.package_type == package_type
        && artifact.native_arch == native_arch
}

fn update_target_os<'a>(package_type: &str, reported_os: &'a str) -> Option<&'a str> {
    if package_type != "standalone" {
        return None;
    }
    match reported_os {
        "windows" => Some("windows"),
        "macos" => Some("macos"),
        _ => Some("linux"),
    }
}

async fn authenticate_agent_headers(
    state: &AppState,
    headers: &HeaderMap,
) -> AppResult<InstanceRecord> {
    let instance_id = agent_header(headers, "x-agent-id")?;
    let secret = agent_header(headers, "x-agent-secret")?;
    let instance = get_instance(&state.db, instance_id).await?;
    if instance.secret != secret {
        return Err(AppError::new(StatusCode::UNAUTHORIZED, "实例密钥不匹配"));
    }
    if instance.approved != 1 || instance.disabled == 1 {
        return Err(AppError::new(StatusCode::FORBIDDEN, "实例未获准更新"));
    }
    Ok(instance)
}

fn agent_header<'a>(headers: &'a HeaderMap, name: &str) -> AppResult<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::new(StatusCode::UNAUTHORIZED, "缺少实例更新认证信息"))
}

async fn find_update_for_instance(
    state: &AppState,
    instance: &InstanceRecord,
) -> AppResult<Option<AgentUpdateOffer>> {
    if let Some(offer) = retried_offer_for_instance(state, instance, None).await? {
        return Ok(Some(offer));
    }
    let Some(candidate) = select_update_candidate(state, instance).await? else {
        return Ok(None);
    };

    let now = now_ts();
    sqlx::query(
        r#"
        INSERT INTO agent_update_attempts(
            id, release_id, artifact_id, instance_id, from_version, target_version,
            status, message, retry_count, created_at, updated_at
        ) VALUES(?, ?, ?, ?, ?, ?, 'pending', '', 0, ?, ?)
        ON CONFLICT(release_id, instance_id) DO UPDATE SET
            artifact_id = excluded.artifact_id,
            from_version = excluded.from_version,
            target_version = excluded.target_version,
            message = '',
            updated_at = excluded.updated_at,
            completed_at = NULL
        WHERE agent_update_attempts.status IN (
            'pending', 'waiting', 'downloading', 'verifying', 'waiting_idle'
        )
        "#,
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&candidate.release_id)
    .bind(&candidate.artifact_id)
    .bind(&instance.id)
    .bind(&instance.agent_version)
    .bind(&candidate.version)
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await?;
    let (artifact_id, target_version, status, retry_count) =
        sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT artifact_id, target_version, status, retry_count FROM agent_update_attempts WHERE release_id = ? AND instance_id = ?",
    )
    .bind(&candidate.release_id)
    .bind(&instance.id)
    .fetch_one(&state.db)
    .await?;
    if artifact_id != candidate.artifact_id
        || target_version != candidate.version
        || TERMINAL_ATTEMPT_STATUSES.contains(&status.as_str())
        || matches!(status.as_str(), "installing" | "awaiting_restart")
    {
        return Ok(None);
    }
    Ok(Some(candidate_offer(candidate, retry_count)))
}

async fn retried_offer_for_instance(
    state: &AppState,
    instance: &InstanceRecord,
    attempt_id: Option<&str>,
) -> AppResult<Option<AgentUpdateOffer>> {
    if instance.update_privileged != 1
        || update_target_os(&instance.package_type, &instance.os).is_none()
        || instance.native_arch.is_empty()
    {
        return Ok(None);
    }
    let candidates = sqlx::query_as::<_, RetriedUpdateCandidate>(
        r#"
        SELECT u.id AS attempt_id, r.id AS release_id, r.version,
               a.id AS artifact_id, a.os, a.package_type, a.native_arch,
               a.sha256, a.size_bytes, u.retry_count
        FROM agent_update_attempts u
        JOIN agent_releases r ON r.id = u.release_id
        JOIN agent_artifacts a ON a.id = u.artifact_id
        WHERE u.instance_id = ? AND u.status = 'pending' AND u.retry_count > 0
          AND r.status = 'published'
        ORDER BY u.updated_at DESC
        "#,
    )
    .bind(&instance.id)
    .fetch_all(&state.db)
    .await?;
    let current = Version::parse(&instance.agent_version).ok();
    let candidate = candidates.into_iter().find(|candidate| {
        if attempt_id.is_some_and(|attempt_id| candidate.attempt_id != attempt_id) {
            return false;
        }
        if update_target_os(&candidate.package_type, &candidate.os) != Some(candidate.os.as_str())
            || candidate.package_type != instance.package_type
            || candidate.native_arch != instance.native_arch
        {
            return false;
        }
        Version::parse(&candidate.version)
            .is_ok_and(|version| current.as_ref().is_none_or(|current| version > *current))
    });
    Ok(candidate.map(|candidate| AgentUpdateOffer {
        download_url: format!(
            "/api/agent/update/artifacts/{}/download",
            candidate.artifact_id
        ),
        release_id: candidate.release_id,
        version: candidate.version,
        artifact_id: candidate.artifact_id,
        sha256: candidate.sha256,
        size_bytes: candidate.size_bytes,
        package_type: candidate.package_type,
        native_arch: candidate.native_arch,
        retry_count: candidate.retry_count,
    }))
}

async fn select_update_candidate(
    state: &AppState,
    instance: &InstanceRecord,
) -> AppResult<Option<UpdateCandidate>> {
    if instance.update_privileged != 1
        || update_target_os(&instance.package_type, &instance.os).is_none()
        || instance.native_arch.is_empty()
    {
        return Ok(None);
    }
    let candidates = sqlx::query_as::<_, UpdateCandidate>(
        r#"
        SELECT r.id AS release_id, r.version, a.id AS artifact_id, a.package_type,
               a.native_arch, a.sha256, a.size_bytes
        FROM agent_releases r
        JOIN agent_artifacts a ON a.release_id = r.id
        WHERE r.status = 'published' AND lower(a.os) = lower(?)
          AND a.package_type = ? AND a.native_arch = ?
        "#,
    )
    .bind(update_target_os(&instance.package_type, &instance.os).expect("validated package type"))
    .bind(&instance.package_type)
    .bind(&instance.native_arch)
    .fetch_all(&state.db)
    .await?;
    let current = Version::parse(&instance.agent_version).ok();
    Ok(candidates
        .into_iter()
        .filter_map(|candidate| {
            let parsed = Version::parse(&candidate.version).ok()?;
            if current.as_ref().is_some_and(|current| parsed <= *current) {
                return None;
            }
            Some((parsed, candidate))
        })
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, candidate)| candidate))
}

async fn require_latest_retry_candidate(
    state: &AppState,
    attempt: &AgentUpdateAttemptRecord,
) -> AppResult<()> {
    let instance = get_instance(&state.db, &attempt.instance_id).await?;
    let candidate = select_update_candidate(state, &instance)
        .await?
        .ok_or_else(|| {
            AppError::new(
                StatusCode::CONFLICT,
                "该实例当前没有可重试的更高版本可执行文件",
            )
        })?;
    if candidate.release_id != attempt.release_id
        || candidate.artifact_id != attempt.artifact_id
        || candidate.version != attempt.target_version
    {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            format!(
                "该更新已被 Agent {} 取代，请使用最新版本",
                candidate.version
            ),
        ));
    }
    Ok(())
}

fn candidate_offer(candidate: UpdateCandidate, retry_count: i64) -> AgentUpdateOffer {
    AgentUpdateOffer {
        download_url: format!(
            "/api/agent/update/artifacts/{}/download",
            candidate.artifact_id
        ),
        release_id: candidate.release_id,
        version: candidate.version,
        artifact_id: candidate.artifact_id,
        sha256: candidate.sha256,
        size_bytes: candidate.size_bytes,
        package_type: candidate.package_type,
        native_arch: candidate.native_arch,
        retry_count,
    }
}

fn outbound_offer(offer: AgentUpdateOffer) -> AgentOutbound {
    AgentOutbound::UpdateAvailable {
        release_id: offer.release_id,
        version: offer.version,
        artifact_id: offer.artifact_id,
        download_url: offer.download_url,
        sha256: offer.sha256,
        size_bytes: offer.size_bytes,
        package_type: offer.package_type,
        native_arch: offer.native_arch,
        retry_count: offer.retry_count,
    }
}

async fn notify_instance(state: &AppState, instance_id: &str) {
    let Ok(instance) = get_instance(&state.db, instance_id).await else {
        return;
    };
    let Ok(Some(offer)) = find_update_for_instance(state, &instance).await else {
        return;
    };
    let Some(handle) = state.agents.read().await.get(instance_id).cloned() else {
        return;
    };
    let _ = handle.tx.send(outbound_offer(offer));
}

async fn notify_retried_attempt(state: &AppState, instance_id: &str, attempt_id: &str) {
    let Ok(instance) = get_instance(&state.db, instance_id).await else {
        return;
    };
    let Ok(Some(offer)) = retried_offer_for_instance(state, &instance, Some(attempt_id)).await
    else {
        return;
    };
    let Some(handle) = state.agents.read().await.get(instance_id).cloned() else {
        return;
    };
    let _ = handle.tx.send(outbound_offer(offer));
}

async fn notify_release_instances(state: &AppState, release_id: &str) {
    let instance_ids = sqlx::query_scalar::<_, String>(
        "SELECT instance_id FROM agent_update_attempts WHERE release_id = ? AND status = 'pending'",
    )
    .bind(release_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    for instance_id in instance_ids {
        notify_instance(state, &instance_id).await;
    }
}

async fn get_attempt(state: &AppState, attempt_id: &str) -> AppResult<AgentUpdateAttemptRecord> {
    sqlx::query_as::<_, AgentUpdateAttemptRecord>(
        r#"
        SELECT id, release_id, artifact_id, instance_id, from_version, target_version,
               status, message, retry_count, created_at, updated_at, completed_at
        FROM agent_update_attempts WHERE id = ?
        "#,
    )
    .bind(attempt_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Agent 更新记录不存在"))
}

fn safe_storage_path(state: &AppState, relative: &str) -> AppResult<std::path::PathBuf> {
    let path = FsPath::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Agent 可执行文件存储路径无效",
        ));
    }
    Ok(state.update_dir.join(path))
}

async fn remove_stored_file(state: &AppState, relative: &str) {
    let Ok(path) = safe_storage_path(state, relative) else {
        return;
    };
    if let Err(error) = fs::remove_file(&path).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        warn!(?error, path = %path.display(), "failed to remove agent artifact");
    }
}

fn map_unique_conflict(
    result: Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error>,
    message: &'static str,
) -> AppResult<sqlx::sqlite::SqliteQueryResult> {
    match result {
        Ok(result) => Ok(result),
        Err(error)
            if error
                .as_database_error()
                .is_some_and(|error| error.is_unique_violation()) =>
        {
            Err(AppError::new(StatusCode::CONFLICT, message))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    use sqlx::sqlite::SqlitePoolOptions;

    use crate::{auth::AuthCipher, config::Cli, db::init_db};

    async fn test_state() -> AppState {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        init_db(&db).await.expect("initialize database");
        let root = std::env::temp_dir().join(format!("om-backend-update-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&root)
            .await
            .expect("create temporary update directory");
        AppState::new(
            db,
            Cli {
                bind: "127.0.0.1:0".parse::<SocketAddr>().expect("bind address"),
                database_url: "sqlite::memory:".to_string(),
                admin_password: "test".to_string(),
                auth_secret_key: None,
                auth_key_file: root.join("auth-secret.key"),
                secure_cookies: false,
                reset_admin_auth: false,
                confirm_reset_admin_auth: None,
                upload_dir: root.join("uploads"),
                update_dir: root.join("updates"),
                agent_package_max_bytes: 1024 * 1024,
            },
            AuthCipher::from_key(&[7_u8; 32]).expect("create test auth cipher"),
        )
    }

    async fn insert_instance(
        state: &AppState,
        id: &str,
        os: &str,
        package_type: &str,
        native_arch: &str,
    ) {
        sqlx::query(
            r#"
            INSERT INTO instances(
                id, secret, name, hostname, os, arch, agent_version, package_type,
                native_arch, update_privileged, first_seen
            ) VALUES(?, 'secret', ?, ?, ?, 'x86_64', '1.0.0', ?, ?, 1, 1)
            "#,
        )
        .bind(id)
        .bind(id)
        .bind(id)
        .bind(os)
        .bind(package_type)
        .bind(native_arch)
        .execute(&state.db)
        .await
        .expect("insert instance");
    }

    async fn insert_release(
        state: &AppState,
        version: &str,
        package_type: &str,
        native_arch: &str,
    ) {
        let release_id = format!("release-{version}");
        let artifact_id = format!("artifact-{version}-{native_arch}");
        sqlx::query(
            "INSERT INTO agent_releases(id, version, status, created_at, published_at) VALUES(?, ?, 'published', 1, 1)",
        )
        .bind(&release_id)
        .bind(version)
        .execute(&state.db)
        .await
        .expect("insert release");
        sqlx::query(
            r#"
            INSERT INTO agent_artifacts(
                id, release_id, os, package_type, native_arch, file_name, size_bytes,
                sha256, storage_path, created_at
            ) VALUES(?, ?, 'linux', ?, ?, 'agent.bin', 8, 'digest', 'stored.bin', 1)
            "#,
        )
        .bind(artifact_id)
        .bind(release_id)
        .bind(package_type)
        .bind(native_arch)
        .execute(&state.db)
        .await
        .expect("insert artifact");
    }

    #[test]
    fn accepts_canonical_semver_and_rejects_noncanonical_versions() {
        assert_eq!(validate_version("1.2.3").expect("valid version"), "1.2.3");
        assert_eq!(
            validate_version("2.0.0-rc.1+build.7").expect("valid prerelease"),
            "2.0.0-rc.1+build.7"
        );
        assert!(validate_version("v1.2.3").is_err());
        assert!(validate_version("1.2").is_err());
        assert!(validate_version("01.2.3").is_err());
    }

    #[test]
    fn recognizes_supported_standalone_executable_signatures() {
        assert!(package_signature_matches(
            "linux",
            &[0x7f, b'E', b'L', b'F']
        ));
        assert!(package_signature_matches("windows", b"MZbinary"));
        assert!(package_signature_matches(
            "macos",
            &[0xcf, 0xfa, 0xed, 0xfe]
        ));
        assert_eq!(update_target_os("standalone", "ubuntu"), Some("linux"));
        assert_eq!(update_target_os("standalone", "openwrt"), Some("linux"));
        assert_eq!(update_target_os("standalone", "windows"), Some("windows"));
        assert_eq!(update_target_os("standalone", "macos"), Some("macos"));
        assert_eq!(update_target_os("legacy", "ubuntu"), None);
        assert!(!package_signature_matches("linux", b"not an executable"));
    }

    #[test]
    fn compares_agent_versions_with_semver_precedence() {
        let release = Version::parse("1.10.0").expect("version");
        assert!(version_is_newer(&release, "1.9.9"));
        assert!(!version_is_newer(&release, "1.10.0"));
        assert!(!version_is_newer(&release, "2.0.0"));
        assert!(version_is_newer(&release, "legacy"));
    }

    #[test]
    fn storage_paths_reject_parent_components() {
        assert_eq!(FsPath::new("release/artifact.bin").components().count(), 2);
        assert!(
            FsPath::new("../secret")
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        );
    }

    #[tokio::test]
    async fn selects_highest_matching_release_and_suppresses_failed_attempt() {
        let state = test_state().await;
        insert_instance(&state, "amd64-agent", "ubuntu", "standalone", "amd64").await;
        insert_release(&state, "1.9.0", "standalone", "amd64").await;
        insert_release(&state, "1.10.0", "standalone", "amd64").await;

        let instance = get_instance(&state.db, "amd64-agent")
            .await
            .expect("load instance");
        let offer = find_update_for_instance(&state, &instance)
            .await
            .expect("find update")
            .expect("matching update");
        assert_eq!(offer.version, "1.10.0");

        record_update_status(
            &state,
            &instance.id,
            &offer.release_id,
            &offer.artifact_id,
            &offer.version,
            0,
            "failed",
            Some("update process exited"),
        )
        .await
        .expect("record failure");
        assert!(
            find_update_for_instance(&state, &instance)
                .await
                .expect("check suppressed update")
                .is_none()
        );

        sqlx::query(
            "UPDATE agent_update_attempts SET status = 'pending', completed_at = NULL WHERE instance_id = ? AND release_id = ?",
        )
        .bind(&instance.id)
        .bind(&offer.release_id)
        .execute(&state.db)
        .await
        .expect("reset attempt like administrator retry");
        assert_eq!(
            find_update_for_instance(&state, &instance)
                .await
                .expect("check retried update")
                .expect("retried offer")
                .version,
            "1.10.0"
        );
    }

    #[tokio::test]
    async fn requires_an_exact_native_architecture_match() {
        let state = test_state().await;
        insert_instance(&state, "arm-agent", "ubuntu", "standalone", "arm64").await;
        insert_release(&state, "2.0.0", "standalone", "amd64").await;
        let instance = get_instance(&state.db, "arm-agent")
            .await
            .expect("load instance");

        assert!(
            find_update_for_instance(&state, &instance)
                .await
                .expect("find update")
                .is_none()
        );
    }

    #[tokio::test]
    async fn reconciles_pre_handoff_attempt_after_offline_capability_change() {
        let state = test_state().await;
        insert_instance(&state, "changed-agent", "ubuntu", "standalone", "amd64").await;
        insert_release(&state, "2.1.0", "standalone", "amd64").await;

        let original = get_instance(&state.db, "changed-agent")
            .await
            .expect("load original instance capability");
        let original_offer = find_update_for_instance(&state, &original)
            .await
            .expect("create publication-time attempt")
            .expect("original architecture is covered");
        assert_eq!(original_offer.native_arch, "amd64");

        sqlx::query(
            r#"
            INSERT INTO agent_artifacts(
                id, release_id, os, package_type, native_arch, file_name, size_bytes,
                sha256, storage_path, created_at
            ) VALUES('artifact-2.1.0-arm64', 'release-2.1.0', 'linux', 'standalone', 'arm64',
                     'agent-arm64.bin', 8, 'digest', 'stored-arm64.bin', 1)
            "#,
        )
        .execute(&state.db)
        .await
        .expect("insert newly matching artifact");
        sqlx::query("UPDATE instances SET native_arch = 'arm64' WHERE id = 'changed-agent'")
            .execute(&state.db)
            .await
            .expect("refresh instance capability on reconnect");

        let reconnected = get_instance(&state.db, "changed-agent")
            .await
            .expect("load refreshed instance capability");
        let reconciled = find_update_for_instance(&state, &reconnected)
            .await
            .expect("reconcile pending attempt")
            .expect("new architecture is covered");
        assert_eq!(reconciled.artifact_id, "artifact-2.1.0-arm64");
        assert_eq!(reconciled.native_arch, "arm64");

        sqlx::query(
            "UPDATE agent_update_attempts SET status = 'downloading' WHERE release_id = ? AND instance_id = ?",
        )
        .bind(&reconciled.release_id)
        .bind(&reconnected.id)
        .execute(&state.db)
        .await
        .expect("simulate a disconnect during package download");
        sqlx::query("UPDATE instances SET native_arch = 'amd64' WHERE id = 'changed-agent'")
            .execute(&state.db)
            .await
            .expect("refresh capability after a second reconnect");

        let resumed_instance = get_instance(&state.db, "changed-agent")
            .await
            .expect("load capability after interrupted download");
        let resumed = find_update_for_instance(&state, &resumed_instance)
            .await
            .expect("reconcile interrupted pre-handoff attempt")
            .expect("original architecture remains covered");
        assert_eq!(resumed.artifact_id, original_offer.artifact_id);
        assert_eq!(resumed.native_arch, "amd64");

        let stored_artifact: String = sqlx::query_scalar(
            "SELECT artifact_id FROM agent_update_attempts WHERE release_id = ? AND instance_id = ?",
        )
        .bind(&resumed.release_id)
        .bind(&resumed_instance.id)
        .fetch_one(&state.db)
        .await
        .expect("load reconciled attempt");
        assert_eq!(stored_artifact, resumed.artifact_id);
        record_update_status(
            &state,
            &resumed_instance.id,
            &resumed.release_id,
            &resumed.artifact_id,
            &resumed.version,
            resumed.retry_count,
            "verifying",
            None,
        )
        .await
        .expect("reconciled offer status must match its stored attempt");
    }

    #[tokio::test]
    async fn normalizes_openwrt_standalone_target_to_linux() {
        let state = test_state().await;
        insert_instance(
            &state,
            "router",
            "openwrt",
            "standalone",
            "aarch64_cortex-a53",
        )
        .await;
        insert_release(&state, "3.0.0", "standalone", "aarch64_cortex-a53").await;
        let instance = get_instance(&state.db, "router")
            .await
            .expect("load instance");

        assert_eq!(
            find_update_for_instance(&state, &instance)
                .await
                .expect("find update")
                .expect("matching standalone update")
                .version,
            "3.0.0"
        );
    }

    #[tokio::test]
    async fn expires_agents_that_do_not_reconnect_after_installation() {
        let state = test_state().await;
        insert_instance(&state, "timed-out-agent", "ubuntu", "standalone", "amd64").await;
        insert_release(&state, "4.0.0", "standalone", "amd64").await;
        let instance = get_instance(&state.db, "timed-out-agent")
            .await
            .expect("load instance");
        let offer = find_update_for_instance(&state, &instance)
            .await
            .expect("find update")
            .expect("matching update");
        record_update_status(
            &state,
            &instance.id,
            &offer.release_id,
            &offer.artifact_id,
            &offer.version,
            0,
            "awaiting_restart",
            None,
        )
        .await
        .expect("record restart state");
        sqlx::query("UPDATE agent_update_attempts SET updated_at = ? WHERE instance_id = ?")
            .bind(now_ts() - UPDATE_HANDOFF_TIMEOUT_SECONDS - 1)
            .bind(&instance.id)
            .execute(&state.db)
            .await
            .expect("age attempt");

        assert_eq!(expire_restart_attempts(&state).await.expect("expire"), 1);
        let status: String =
            sqlx::query_scalar("SELECT status FROM agent_update_attempts WHERE instance_id = ?")
                .bind(&instance.id)
                .fetch_one(&state.db)
                .await
                .expect("read attempt");
        assert_eq!(status, "failed");
    }

    #[tokio::test]
    async fn rejects_retry_when_a_newer_matching_release_exists() {
        let state = test_state().await;
        insert_instance(&state, "retry-agent", "ubuntu", "standalone", "amd64").await;
        insert_release(&state, "1.5.0", "standalone", "amd64").await;
        let instance = get_instance(&state.db, "retry-agent")
            .await
            .expect("load instance");
        let offer = find_update_for_instance(&state, &instance)
            .await
            .expect("find first update")
            .expect("first update");
        record_update_status(
            &state,
            &instance.id,
            &offer.release_id,
            &offer.artifact_id,
            &offer.version,
            0,
            "failed",
            Some("update process failed"),
        )
        .await
        .expect("record failure");
        let attempt = sqlx::query_as::<_, AgentUpdateAttemptRecord>(
            r#"
            SELECT id, release_id, artifact_id, instance_id, from_version, target_version,
                   status, message, retry_count, created_at, updated_at, completed_at
            FROM agent_update_attempts WHERE instance_id = ?
            "#,
        )
        .bind(&instance.id)
        .fetch_one(&state.db)
        .await
        .expect("load failed attempt");

        insert_release(&state, "2.0.0", "standalone", "amd64").await;

        let error = require_latest_retry_candidate(&state, &attempt)
            .await
            .expect_err("superseded retry must fail");
        assert_eq!(error.status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn ignores_status_from_an_older_retry_generation() {
        let state = test_state().await;
        insert_instance(&state, "generation-agent", "ubuntu", "standalone", "amd64").await;
        insert_release(&state, "2.5.0", "standalone", "amd64").await;
        let instance = get_instance(&state.db, "generation-agent")
            .await
            .expect("load instance");
        let offer = find_update_for_instance(&state, &instance)
            .await
            .expect("find update")
            .expect("matching update");
        sqlx::query(
            "UPDATE agent_update_attempts SET retry_count = 1, status = 'pending' WHERE instance_id = ?",
        )
        .bind(&instance.id)
        .execute(&state.db)
        .await
        .expect("advance retry generation");

        let error = record_update_status(
            &state,
            &instance.id,
            &offer.release_id,
            &offer.artifact_id,
            &offer.version,
            0,
            "failed",
            Some("late failure from generation zero"),
        )
        .await
        .expect_err("stale generation must not update the current attempt");
        assert_eq!(error.status, StatusCode::NOT_FOUND);

        record_update_status(
            &state,
            &instance.id,
            &offer.release_id,
            &offer.artifact_id,
            &offer.version,
            1,
            "failed",
            Some("current generation failed"),
        )
        .await
        .expect("current generation status is accepted");
    }

    #[tokio::test]
    async fn explicit_retry_remains_pinned_when_a_new_release_is_published() {
        let state = test_state().await;
        insert_instance(
            &state,
            "pinned-retry-agent",
            "ubuntu",
            "standalone",
            "amd64",
        )
        .await;
        insert_release(&state, "1.5.0", "standalone", "amd64").await;
        let instance = get_instance(&state.db, "pinned-retry-agent")
            .await
            .expect("load instance");
        let first = find_update_for_instance(&state, &instance)
            .await
            .expect("find first update")
            .expect("first update");
        sqlx::query(
            "UPDATE agent_update_attempts SET status = 'pending', retry_count = 1 WHERE instance_id = ?",
        )
        .bind(&instance.id)
        .execute(&state.db)
        .await
        .expect("mark explicit retry");
        insert_release(&state, "2.0.0", "standalone", "amd64").await;

        let retry = find_update_for_instance(&state, &instance)
            .await
            .expect("find pinned retry")
            .expect("pinned retry offer");
        assert_eq!(retry.artifact_id, first.artifact_id);
        assert_eq!(retry.version, "1.5.0");
        assert_eq!(retry.retry_count, 1);
    }

    #[tokio::test]
    async fn refuses_to_store_an_artifact_after_its_release_is_published() {
        let state = test_state().await;
        insert_release(&state, "5.0.0", "standalone", "amd64").await;
        let temporary = state.update_dir.join("late-upload.bin");
        fs::create_dir_all(&state.update_dir)
            .await
            .expect("create update directory");
        fs::write(&temporary, b"\x7fELFpayload")
            .await
            .expect("write temporary package");
        let received = ReceivedArtifact {
            os: "linux".to_string(),
            package_type: "standalone".to_string(),
            native_arch: "arm64".to_string(),
            file_name: "agent.bin".to_string(),
            size_bytes: 15,
            sha256: "0".repeat(64),
            checksum_file_name: "agent.bin.sha256".to_string(),
            checksum_contents: format!("{}  agent.bin\n", "0".repeat(64)),
            first_bytes: b"\x7fELFpayload".to_vec(),
            temp_path: temporary,
        };

        let error = match store_artifact(&state, "release-5.0.0", received).await {
            Err(error) => error,
            Ok(_) => panic!("published releases are immutable"),
        };
        assert_eq!(error.status, StatusCode::CONFLICT);
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_artifacts WHERE release_id = 'release-5.0.0'",
        )
        .fetch_one(&state.db)
        .await
        .expect("count artifacts");
        assert_eq!(count, 1, "the pre-existing published artifact is unchanged");
    }

    #[test]
    fn accepts_matching_sha256_sidecar_formats() {
        let digest = "a".repeat(64);
        assert!(
            validate_checksum_file(
                "om-agent.bin",
                "om-agent.bin.sha256",
                &format!("{digest}  om-agent.bin\n"),
                &digest,
            )
            .is_ok()
        );
        assert!(
            validate_checksum_file(
                "om-agent.exe",
                "OM-AGENT.EXE.SHA256",
                &format!("{digest} *om-agent.exe\r\n"),
                &digest,
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_mismatched_sha256_sidecars() {
        let digest = "a".repeat(64);
        assert!(
            validate_checksum_file(
                "om-agent.bin",
                "other.bin.sha256",
                &format!("{digest}  om-agent.bin\n"),
                &digest,
            )
            .is_err()
        );
        assert!(
            validate_checksum_file(
                "om-agent.bin",
                "om-agent.bin.sha256",
                &format!("{}  om-agent.bin\n", "b".repeat(64)),
                &digest,
            )
            .is_err()
        );
        assert!(
            validate_checksum_file(
                "om-agent.bin",
                "om-agent.bin.sha256",
                &format!("{digest}  other.bin\n"),
                &digest,
            )
            .is_err()
        );
        assert!(
            validate_checksum_file(
                "om-agent.bin",
                "om-agent.bin.sha256",
                &format!("{digest}  om-agent.bin unexpected\n"),
                &digest,
            )
            .is_err()
        );
    }
}
