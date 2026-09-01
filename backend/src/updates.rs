use std::collections::{HashMap, HashSet};
use std::path::Path as FsPath;
use std::time::Duration;

use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
};
use semver::Version;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Postgres, Transaction};
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
};
use tokio_util::io::ReaderStream;
use tracing::warn;
use uuid::Uuid;

use crate::{
    auth::require_admin,
    db::{agent_secret_matches, get_instance, write_action_log},
    error::{AppError, AppResult},
    files::content_disposition,
    models::{
        AgentArtifactRecord, AgentOutbound, AgentReleaseCoverage, AgentReleaseDetail,
        AgentReleaseRecord, AgentReleaseTargetsRequest, AgentRollbackCoverage, AgentRollbackOffer,
        AgentRollbackPackage, AgentRolloutCandidate, AgentUpdateAttemptRecord, AgentUpdateManifest,
        AgentUpdateOffer, CreateAgentReleaseRequest, InstanceRecord, MAX_AGENT_UPDATE_RETRY_COUNT,
        PublishAgentReleaseRequest, UpdateAttemptsQuery,
    },
    state::AppState,
    utils::now_ts,
};

const MAX_METADATA_BYTES: usize = 1024;
const MAX_CHECKSUM_FILE_BYTES: usize = 4096;
const MAX_STATUS_MESSAGE_BYTES: usize = 4096;
// Covers parent exit, target/rollback install and service-restart timeouts, health checks, and I/O.
const UPDATE_HANDOFF_TIMEOUT_SECONDS: i64 = 60 * 60;
const TERMINAL_ATTEMPT_STATUSES: [&str; 4] =
    ["succeeded", "rollback_succeeded", "failed", "cancelled"];
const ROLLOUT_CONTROL_LOCK_ID: i64 = 0x4f4d_524f_4c4c_4f55;

#[derive(FromRow)]
struct InstanceCapabilityRow {
    id: String,
    name: String,
    hostname: String,
    os: String,
    agent_version: String,
    package_type: String,
    native_arch: String,
    update_privileged: i64,
    rollback_supported: i64,
    rollback_version: String,
}

#[derive(FromRow)]
struct UpdateCandidate {
    release_id: String,
    version: String,
    artifact_id: String,
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
    status: String,
    rollout_state: String,
}

#[derive(FromRow)]
struct RollbackCandidate {
    attempt_id: String,
    release_id: String,
    instance_id: String,
    from_version: String,
    target_version: String,
    retry_count: i64,
    status: String,
    artifact_id: Option<String>,
    os: Option<String>,
    package_type: Option<String>,
    native_arch: Option<String>,
    sha256: Option<String>,
    size_bytes: Option<i64>,
}

#[derive(FromRow)]
struct PublishedRollbackArtifact {
    version: String,
    os: String,
    package_type: String,
    native_arch: String,
}

#[derive(Clone, Copy)]
struct RollbackQueueOutcome {
    inserted: bool,
    pending: bool,
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
        SELECT id, version, notes, status, rollout_state, rollout_updated_at,
               created_at, published_at
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
        "INSERT INTO agent_releases(id, version, notes, status, rollout_state, created_at, rollout_updated_at) VALUES($1, $2, $3, 'draft', 'draft', $4, $5)",
    )
    .bind(&id)
    .bind(&version)
    .bind(payload.notes.trim())
    .bind(created_at)
    .bind(created_at)
    .execute(&state.db)
    .await;
    let _ = map_unique_conflict(result, "该 Agent 版本已存在")?;

    write_action_log(
        &state.db,
        &admin.username,
        Some(&admin.user_id),
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
        "UPDATE agent_releases SET version = $1, notes = $2 WHERE id = $3 AND status = 'draft'",
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
        Some(&admin.user_id),
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
    delete_agent_release(&state, &admin.username, &admin.user_id, &release_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_agent_release(
    state: &AppState,
    actor: &str,
    user_id: &str,
    release_id: &str,
) -> AppResult<()> {
    let mut transaction = state.db.begin().await?;
    let release = sqlx::query_as::<_, AgentReleaseRecord>(
        "SELECT id, version, notes, status, rollout_state, rollout_updated_at, created_at, published_at FROM agent_releases WHERE id = $1 FOR UPDATE",
    )
    .bind(release_id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Agent 版本不存在"))?;
    let attempt_statuses = sqlx::query_as::<_, (String, bool)>(
        r#"
        SELECT u.status, i.id IS NOT NULL AS instance_exists
        FROM agent_update_attempts AS u
        LEFT JOIN instances AS i ON i.id = u.instance_id
        WHERE u.release_id = $1
        FOR UPDATE OF u
        "#,
    )
    .bind(release_id)
    .fetch_all(&mut *transaction)
    .await?;
    let active_attempts = attempt_statuses
        .iter()
        .filter(|(status, instance_exists)| {
            *instance_exists && !TERMINAL_ATTEMPT_STATUSES.contains(&status.as_str())
        })
        .count();
    if active_attempts > 0 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            format!("该版本仍有 {active_attempts} 个实例更新未结束，完成后才能删除"),
        ));
    }
    let rollback_dependencies: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM agent_update_attempts AS u
        JOIN instances AS i ON i.id = u.instance_id
        LEFT JOIN agent_artifacts AS a ON a.id = u.artifact_id
        WHERE u.operation = 'rollback'
          AND u.status NOT IN ('succeeded', 'rollback_succeeded', 'failed', 'cancelled')
          AND (u.release_id = $1 OR u.target_version = $2 OR a.release_id = $1)
        "#,
    )
    .bind(release_id)
    .bind(&release.version)
    .fetch_one(&mut *transaction)
    .await?;
    if rollback_dependencies > 0 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            format!("该版本仍被 {rollback_dependencies} 条回滚记录依赖"),
        ));
    }
    // Instances already running this version keep the executable they installed, so retiring the
    // distribution artifact cannot disturb them and must not block deletion. The one destructive
    // case is an instance that upgraded away from this version, still sits on that upgrade result,
    // and holds no local rollback baseline for it: the published artifact is then its only way
    // back, and `queue_rollback_for_upgrade` would fail once the artifact is gone. Mirror that
    // function's preconditions so the guard only counts instances that could really roll back.
    let sole_rollback_path: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(DISTINCT i.id)
        FROM instances i
        WHERE i.rollback_supported = 1
          AND i.update_privileged = 1
          AND i.approved = 1
          AND i.disabled = 0
          AND i.rollback_version <> $1
          AND EXISTS (
              SELECT 1 FROM agent_update_attempts u
              WHERE u.instance_id = i.id AND u.operation = 'upgrade'
                AND u.status = 'succeeded' AND u.from_version = $1
                AND i.agent_version = u.target_version
          )
        "#,
    )
    .bind(&release.version)
    .fetch_one(&mut *transaction)
    .await?;
    if release.status == "published" && sole_rollback_path > 0 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            format!(
                "仍有 {sole_rollback_path} 个实例只能回滚到该版本且本地没有回滚基线，删除后将无法回退"
            ),
        ));
    }
    let artifact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_artifacts WHERE release_id = $1")
            .bind(release_id)
            .fetch_one(&mut *transaction)
            .await?;

    let deleted = sqlx::query("DELETE FROM agent_releases WHERE id = $1")
        .bind(release_id)
        .execute(&mut *transaction)
        .await?;
    debug_assert_eq!(deleted.rows_affected(), 1);

    let release_kind = if release.status == "published" {
        "已发布"
    } else {
        "草稿"
    };
    write_action_log(
        &mut *transaction,
        actor,
        Some(user_id),
        "delete_agent_release",
        release_id,
        &format!(
            "永久删除{release_kind} Agent 版本 {}、{artifact_count} 个可执行文件及 SHA-256 校验文件、{} 条实例更新记录",
            release.version,
            attempt_statuses.len()
        ),
    )
    .await?;
    transaction.commit().await?;

    // The filesystem cannot participate in the database transaction. Once metadata and
    // its audit record are committed, storage cleanup is best effort and cannot resurrect
    // a release or make an orphaned artifact downloadable.
    let _ = remove_release_storage(state, release_id).await;
    Ok(())
}

pub async fn admin_upload_agent_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(release_id): Path<String>,
    multipart: Multipart,
) -> AppResult<(StatusCode, Json<AgentArtifactRecord>)> {
    let admin = require_admin(&state, &headers).await?;
    get_release(&state, &release_id).await?;
    let received = receive_artifact(&state, multipart).await?;
    let result = store_artifact(&state, &release_id, received).await;
    let artifact = result?;

    write_action_log(
        &state.db,
        &admin.username,
        Some(&admin.user_id),
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
    let artifact = get_artifact(&state, &artifact_id)
        .await?
        .filter(|artifact| artifact.release_id == release_id)
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Agent 可执行文件不存在"))?;

    let mut transaction = state.db.begin().await?;
    let deleted = sqlx::query(
        r#"
        DELETE FROM agent_artifacts
        WHERE id = $1 AND release_id = $2 AND status = 'draft'
        "#,
    )
    .bind(&artifact_id)
    .bind(&release_id)
    .execute(&mut *transaction)
    .await?;
    if deleted.rows_affected() != 1 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "已发布的可执行文件不能删除",
        ));
    }
    write_action_log(
        &mut *transaction,
        &admin.username,
        Some(&admin.user_id),
        "delete_agent_artifact",
        &artifact_id,
        "删除 Agent 可执行文件及 SHA-256 校验文件",
    )
    .await?;
    transaction.commit().await?;

    remove_stored_file(&state, &artifact.storage_path).await;
    remove_stored_file(&state, &format!("{}.sha256", artifact.storage_path)).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn admin_download_agent_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((release_id, artifact_id)): Path<(String, String)>,
) -> AppResult<Response> {
    let admin = require_admin(&state, &headers).await?;
    let artifact = get_artifact(&state, &artifact_id)
        .await?
        .filter(|artifact| artifact.release_id == release_id)
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Agent 更新包不存在"))?;
    let response =
        artifact_download_response(&state, &artifact, content_disposition(&artifact.file_name)?)
            .await?;
    write_action_log(
        &state.db,
        &admin.username,
        Some(&admin.user_id),
        "download_agent_artifact",
        &artifact.id,
        &format!(
            "下载 Agent 更新包 {}（{} 字节）",
            artifact.file_name, artifact.size_bytes
        ),
    )
    .await?;
    Ok(response)
}

pub async fn admin_publish_agent_release(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(release_id): Path<String>,
    payload: Option<Json<PublishAgentReleaseRequest>>,
) -> AppResult<Json<AgentReleaseDetail>> {
    let admin = require_admin(&state, &headers).await?;
    let requested_instance_ids = payload.unwrap_or_default().0.instance_ids;
    let instances = capability_instances(&state).await?;
    let now = now_ts();
    let mut transaction = state.db.begin().await?;
    let release = sqlx::query_as::<_, AgentReleaseRecord>(
        "SELECT id, version, notes, status, rollout_state, rollout_updated_at, created_at, published_at FROM agent_releases WHERE id = $1 FOR UPDATE",
    )
    .bind(&release_id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Agent 版本不存在"))?;
    let target = Version::parse(&release.version)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "版本号不是有效的 SemVer"))?;
    let initial_publish = release.status == "draft";
    if initial_publish && requested_instance_ids.is_empty() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "首次发布至少选择一个灰度实例",
        ));
    }
    if !initial_publish
        && matches!(
            release.rollout_state.as_str(),
            "rollback_active" | "rolled_back" | "rollback_partial"
        )
    {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "回滚中的版本不能继续发布更新包",
        ));
    }
    if initial_publish {
        ensure_no_other_controlled_release(&mut transaction, &release_id).await?;
    }
    let artifacts = sqlx::query_as::<_, AgentArtifactRecord>(
        r#"
        SELECT id, release_id, os, package_type, native_arch, file_name, size_bytes, sha256,
               storage_path, created_at, status, published_at
        FROM agent_artifacts
        WHERE release_id = $1 AND status = 'draft'
        ORDER BY os, package_type, native_arch
        FOR UPDATE
        "#,
    )
    .bind(&release_id)
    .fetch_all(&mut *transaction)
    .await?;
    if artifacts.is_empty() {
        let (status, message) = if release.status == "draft" {
            (StatusCode::BAD_REQUEST, "至少上传一个可执行文件后才能发布")
        } else {
            (StatusCode::CONFLICT, "没有待发布的新增可执行文件")
        };
        return Err(AppError::new(status, message));
    }

    let rollout_state = if initial_publish {
        "canary_active"
    } else {
        release.rollout_state.as_str()
    };
    sqlx::query(
        "UPDATE agent_releases SET status = 'published', rollout_state = $1, rollout_updated_at = $2, published_at = COALESCE(published_at, $3) WHERE id = $4",
    )
    .bind(rollout_state)
    .bind(now)
    .bind(now)
    .bind(&release_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE agent_artifacts SET status = 'published', published_at = $1 WHERE release_id = $2 AND status = 'draft'",
    )
    .bind(now)
    .bind(&release_id)
    .execute(&mut *transaction)
    .await?;

    let requested = requested_instance_ids.into_iter().collect::<HashSet<_>>();
    if initial_publish {
        for instance_id in &requested {
            let instance = instances
                .iter()
                .find(|instance| &instance.id == instance_id)
                .ok_or_else(|| {
                    AppError::new(StatusCode::BAD_REQUEST, "选择的实例不存在或已停用")
                })?;
            validate_rollout_instance(instance, &target, &artifacts)?;
            sqlx::query(
                r#"
                INSERT INTO agent_release_targets(release_id, instance_id, state, created_at, updated_at)
                VALUES($1, $2, 'included', $3, $4)
                ON CONFLICT(release_id, instance_id) DO UPDATE
                SET state = 'included', updated_at = EXCLUDED.updated_at
                "#,
            )
            .bind(&release_id)
            .bind(instance_id)
            .bind(now)
            .bind(now)
            .execute(&mut *transaction)
            .await?;
        }
    }

    let included_ids = sqlx::query_scalar::<_, String>(
        "SELECT instance_id FROM agent_release_targets WHERE release_id = $1 AND state = 'included'",
    )
    .bind(&release_id)
    .fetch_all(&mut *transaction)
    .await?
    .into_iter()
    .collect::<HashSet<_>>();
    let excluded_ids = sqlx::query_scalar::<_, String>(
        "SELECT instance_id FROM agent_release_targets WHERE release_id = $1 AND state = 'excluded'",
    )
    .bind(&release_id)
    .fetch_all(&mut *transaction)
    .await?
    .into_iter()
    .collect::<HashSet<_>>();
    let published_artifacts = sqlx::query_as::<_, AgentArtifactRecord>(
        r#"
        SELECT id, release_id, os, package_type, native_arch, file_name, size_bytes, sha256,
               storage_path, created_at, status, published_at
        FROM agent_artifacts WHERE release_id = $1 AND status = 'published'
        "#,
    )
    .bind(&release_id)
    .fetch_all(&mut *transaction)
    .await?;

    let mut notified_instance_ids = Vec::new();
    for instance in instances {
        let selected =
            rollout_selects_instance(rollout_state, &instance.id, &included_ids, &excluded_ids);
        if !selected {
            continue;
        }
        if !version_is_newer(&target, &instance.agent_version) {
            continue;
        }
        let Some(artifact) = published_artifacts.iter().find(|artifact| {
            target_matches(
                &instance.os,
                &instance.package_type,
                &instance.native_arch,
                artifact,
            )
        }) else {
            continue;
        };
        if queue_upgrade_attempt(&mut transaction, &release, &instance, artifact, now, false)
            .await?
            && matches!(rollout_state, "canary_active" | "full_active")
        {
            notified_instance_ids.push(instance.id);
        }
    }
    write_action_log(
        &mut *transaction,
        &admin.username,
        Some(&admin.user_id),
        "publish_agent_release",
        &release_id,
        &format!(
            "发布 Agent 版本 {} 的 {} 个可执行文件，灰度目标 {} 个",
            release.version,
            artifacts.len(),
            included_ids.len()
        ),
    )
    .await?;
    transaction.commit().await?;

    notify_instances(&state, notified_instance_ids).await;
    let published = get_release(&state, &release_id).await?;
    Ok(Json(load_release_detail(&state, published).await?))
}

pub async fn admin_agent_rollout_candidates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(release_id): Path<String>,
) -> AppResult<Json<Vec<AgentRolloutCandidate>>> {
    require_admin(&state, &headers).await?;
    let release = get_release(&state, &release_id).await?;
    let target = Version::parse(&release.version)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "版本号不是有效的 SemVer"))?;
    let artifacts = release_artifacts(&state, &release_id).await?;
    let selected = sqlx::query_scalar::<_, String>(
        "SELECT instance_id FROM agent_release_targets WHERE release_id = $1 AND state = 'included'",
    )
    .bind(&release_id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect::<HashSet<_>>();
    let excluded = sqlx::query_scalar::<_, String>(
        "SELECT instance_id FROM agent_release_targets WHERE release_id = $1 AND state = 'excluded'",
    )
    .bind(&release_id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect::<HashSet<_>>();
    let online = state
        .agents
        .read()
        .await
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    let active_attempts = sqlx::query_as::<_, (String, String, String)>(
        r#"
        SELECT instance_id, operation, status
        FROM agent_update_attempts
        WHERE status NOT IN ('succeeded', 'rollback_succeeded', 'failed', 'cancelled')
        ORDER BY updated_at DESC
        "#,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|(instance_id, operation, status)| (instance_id, (operation, status)))
    .collect::<HashMap<_, _>>();
    let candidates = capability_instances(&state)
        .await?
        .into_iter()
        .map(|instance| {
            let active = active_attempts.get(&instance.id);
            let reason = if excluded.contains(&instance.id) {
                Some("实例已从此版本排除，请使用重新升级")
            } else {
                rollout_ineligibility_reason(&instance, &target, &artifacts).or_else(|| {
                    active.map(|(operation, status)| {
                        if operation == "rollback" {
                            "实例正在执行回滚任务"
                        } else if status == "pending" {
                            "实例已有待下发的升级任务"
                        } else {
                            "实例正在执行升级任务"
                        }
                    })
                })
            };
            AgentRolloutCandidate {
                instance_id: instance.id.clone(),
                name: instance.name,
                hostname: instance.hostname,
                os: instance.os,
                package_type: instance.package_type,
                native_arch: instance.native_arch,
                agent_version: instance.agent_version,
                online: online.contains(&instance.id),
                update_privileged: instance.update_privileged == 1,
                selected: selected.contains(&instance.id),
                eligible: reason.is_none(),
                reason: reason.unwrap_or_default().to_string(),
                active_operation: active_attempts
                    .get(&instance.id)
                    .map(|(operation, _)| operation.clone()),
                active_status: active_attempts
                    .get(&instance.id)
                    .map(|(_, status)| status.clone()),
                rollback_supported: instance.rollback_supported == 1,
                rollback_version: (!instance.rollback_version.is_empty())
                    .then_some(instance.rollback_version),
            }
        })
        .collect();
    Ok(Json(candidates))
}

pub async fn admin_add_agent_rollout_targets(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(release_id): Path<String>,
    Json(payload): Json<AgentReleaseTargetsRequest>,
) -> AppResult<Json<AgentReleaseDetail>> {
    let admin = require_admin(&state, &headers).await?;
    if payload.instance_ids.is_empty() {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "至少选择一个实例"));
    }
    let release = get_release(&state, &release_id).await?;
    if !matches!(
        release.rollout_state.as_str(),
        "canary_active" | "canary_paused"
    ) {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "只有灰度版本可以继续添加批次",
        ));
    }
    let target = Version::parse(&release.version)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "版本号不是有效的 SemVer"))?;
    let artifacts = release_artifacts(&state, &release_id)
        .await?
        .into_iter()
        .filter(|artifact| artifact.status == "published")
        .collect::<Vec<_>>();
    let instances = capability_instances(&state).await?;
    let requested = payload.instance_ids.into_iter().collect::<HashSet<_>>();
    let excluded = sqlx::query_scalar::<_, String>(
        "SELECT instance_id FROM agent_release_targets WHERE release_id = $1 AND state = 'excluded'",
    )
    .bind(&release_id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect::<HashSet<_>>();
    if requested
        .iter()
        .any(|instance_id| excluded.contains(instance_id))
    {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "已排除的实例只能通过重新升级操作加入当前版本",
        ));
    }
    let now = now_ts();
    let mut transaction = state.db.begin().await?;
    let release = locked_release(&mut transaction, &release_id).await?;
    if !matches!(
        release.rollout_state.as_str(),
        "canary_active" | "canary_paused"
    ) {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "只有灰度版本可以继续添加批次",
        ));
    }
    let mut notified = Vec::new();
    for instance_id in requested {
        let instance = instances
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "选择的实例不存在或已停用"))?;
        validate_rollout_instance(instance, &target, &artifacts)?;
        sqlx::query(
            r#"
            INSERT INTO agent_release_targets(release_id, instance_id, state, created_at, updated_at)
            VALUES($1, $2, 'included', $3, $4)
            ON CONFLICT(release_id, instance_id) DO UPDATE
            SET state = 'included', updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(&release_id)
        .bind(&instance.id)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if let Some(artifact) = matching_artifact(instance, &artifacts)
            && queue_upgrade_attempt(&mut transaction, &release, instance, artifact, now, false)
                .await?
            && release.rollout_state == "canary_active"
        {
            notified.push(instance.id.clone());
        }
    }
    write_action_log(
        &mut *transaction,
        &admin.username,
        Some(&admin.user_id),
        "add_agent_rollout_targets",
        &release_id,
        "添加 Agent 灰度批次",
    )
    .await?;
    transaction.commit().await?;
    notify_instances(&state, notified).await;
    Ok(Json(
        load_release_detail(&state, get_release(&state, &release_id).await?).await?,
    ))
}

pub async fn admin_pause_agent_rollout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(release_id): Path<String>,
) -> AppResult<Json<AgentReleaseDetail>> {
    set_rollout_paused(&state, &headers, &release_id, true).await
}

pub async fn admin_resume_agent_rollout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(release_id): Path<String>,
) -> AppResult<Json<AgentReleaseDetail>> {
    set_rollout_paused(&state, &headers, &release_id, false).await
}

pub async fn admin_promote_agent_rollout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(release_id): Path<String>,
) -> AppResult<Json<AgentReleaseDetail>> {
    let admin = require_admin(&state, &headers).await?;
    let release = get_release(&state, &release_id).await?;
    let target = Version::parse(&release.version)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "版本号不是有效的 SemVer"))?;
    let artifacts = release_artifacts(&state, &release_id)
        .await?
        .into_iter()
        .filter(|artifact| artifact.status == "published")
        .collect::<Vec<_>>();
    let instances = capability_instances(&state).await?;
    let now = now_ts();
    let mut transaction = state.db.begin().await?;
    let release = locked_release(&mut transaction, &release_id).await?;
    let next_state = match release.rollout_state.as_str() {
        "canary_active" => "full_active",
        "canary_paused" => "full_paused",
        _ => {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "只有灰度版本可以晋级全量",
            ));
        }
    };
    let excluded_ids = sqlx::query_scalar::<_, String>(
        "SELECT instance_id FROM agent_release_targets WHERE release_id = $1 AND state = 'excluded'",
    )
    .bind(&release_id)
    .fetch_all(&mut *transaction)
    .await?
    .into_iter()
    .collect::<HashSet<_>>();
    sqlx::query(
        "UPDATE agent_releases SET rollout_state = $1, rollout_updated_at = $2 WHERE id = $3",
    )
    .bind(next_state)
    .bind(now)
    .bind(&release_id)
    .execute(&mut *transaction)
    .await?;
    let mut notified = Vec::new();
    for instance in &instances {
        if excluded_ids.contains(&instance.id) {
            continue;
        }
        if rollout_ineligibility_reason(instance, &target, &artifacts).is_some() {
            continue;
        }
        let Some(artifact) = matching_artifact(instance, &artifacts) else {
            continue;
        };
        if queue_upgrade_attempt(&mut transaction, &release, instance, artifact, now, false).await?
            && next_state == "full_active"
        {
            notified.push(instance.id.clone());
        }
    }
    write_action_log(
        &mut *transaction,
        &admin.username,
        Some(&admin.user_id),
        "promote_agent_rollout",
        &release_id,
        &format!("Agent {} 晋级全量发布", release.version),
    )
    .await?;
    transaction.commit().await?;
    notify_instances(&state, notified).await;
    Ok(Json(
        load_release_detail(&state, get_release(&state, &release_id).await?).await?,
    ))
}

pub async fn admin_rollback_agent_release(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(release_id): Path<String>,
) -> AppResult<Json<AgentReleaseDetail>> {
    let admin = require_admin(&state, &headers).await?;
    let now = now_ts();
    let mut transaction = state.db.begin().await?;
    let release = locked_release(&mut transaction, &release_id).await?;
    if !matches!(
        release.rollout_state.as_str(),
        "canary_active" | "canary_paused" | "full_active" | "full_paused" | "rollback_partial"
    ) {
        return Err(AppError::new(StatusCode::CONFLICT, "当前版本不能发起回滚"));
    }
    ensure_no_other_controlled_release(&mut transaction, &release_id).await?;
    sqlx::query(
        "UPDATE agent_releases SET rollout_state = 'rollback_active', rollout_updated_at = $1 WHERE id = $2",
    )
    .bind(now)
    .bind(&release_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        r#"
        UPDATE agent_update_attempts
        SET status = 'cancelled', message = '版本已进入回滚流程', updated_at = $1,
            completed_at = $2
        WHERE release_id = $3 AND operation = 'upgrade' AND status = 'pending'
        "#,
    )
    .bind(now)
    .bind(now)
    .bind(&release_id)
    .execute(&mut *transaction)
    .await?;
    write_action_log(
        &mut *transaction,
        &admin.username,
        Some(&admin.user_id),
        "rollback_agent_release",
        &release_id,
        &format!("回滚 Agent 版本 {}", release.version),
    )
    .await?;
    transaction.commit().await?;
    reconcile_release_rollback(&state, &release_id).await?;
    Ok(Json(
        load_release_detail(&state, get_release(&state, &release_id).await?).await?,
    ))
}

pub async fn admin_rollback_agent_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((release_id, instance_id)): Path<(String, String)>,
) -> AppResult<Json<AgentReleaseDetail>> {
    let admin = require_admin(&state, &headers).await?;
    let release = get_release(&state, &release_id).await?;
    if !matches!(
        release.rollout_state.as_str(),
        "canary_active" | "canary_paused" | "full_active" | "full_paused"
    ) {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "当前版本不能执行实例回滚",
        ));
    }
    let parent = latest_successful_upgrade(&state, &release_id, &instance_id).await?;
    let now = now_ts();
    let mut transaction = state.db.begin().await?;
    let release = locked_release(&mut transaction, &release_id).await?;
    if !matches!(
        release.rollout_state.as_str(),
        "canary_active" | "canary_paused" | "full_active" | "full_paused"
    ) {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "当前版本不能执行实例回滚",
        ));
    }
    let active_attempt: Option<(String, String)> = sqlx::query_as(
        r#"
        SELECT operation, status
        FROM agent_update_attempts
        WHERE instance_id = $1
          AND status NOT IN ('succeeded', 'rollback_succeeded', 'failed', 'cancelled')
        LIMIT 1
        FOR UPDATE
        "#,
    )
    .bind(&instance_id)
    .fetch_optional(&mut *transaction)
    .await?;
    if let Some((operation, status)) = active_attempt {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            format!("实例已有活动的 {operation} 任务（{status}），完成后才能回滚"),
        ));
    }
    sqlx::query(
        r#"
        INSERT INTO agent_release_targets(release_id, instance_id, state, created_at, updated_at)
        VALUES($1, $2, 'excluded', $3, $4)
        ON CONFLICT(release_id, instance_id) DO UPDATE
        SET state = 'excluded', updated_at = EXCLUDED.updated_at
        "#,
    )
    .bind(&release_id)
    .bind(&instance_id)
    .bind(now)
    .bind(now)
    .execute(&mut *transaction)
    .await?;
    let outcome = queue_rollback_for_upgrade_in_transaction(&mut transaction, &parent).await?;
    if !outcome.inserted {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "实例已有回滚记录或其他活动更新任务，请刷新后重试",
        ));
    }
    write_action_log(
        &mut *transaction,
        &admin.username,
        Some(&admin.user_id),
        "rollback_agent_instance",
        &instance_id,
        &format!("回滚实例的 Agent {}", release.version),
    )
    .await?;
    transaction.commit().await?;
    if outcome.pending {
        notify_instance(&state, &instance_id).await;
    }
    Ok(Json(
        load_release_detail(&state, get_release(&state, &release_id).await?).await?,
    ))
}

pub async fn admin_reupgrade_agent_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((release_id, instance_id)): Path<(String, String)>,
) -> AppResult<Json<AgentReleaseDetail>> {
    let admin = require_admin(&state, &headers).await?;
    let release = get_release(&state, &release_id).await?;
    if !matches!(
        release.rollout_state.as_str(),
        "canary_active" | "canary_paused" | "full_active" | "full_paused"
    ) {
        return Err(AppError::new(StatusCode::CONFLICT, "当前版本不能重新升级"));
    }
    let rolled_back: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_update_attempts WHERE release_id = $1 AND instance_id = $2 AND operation = 'rollback' AND status = 'rollback_succeeded'",
    )
    .bind(&release_id)
    .bind(&instance_id)
    .fetch_one(&state.db)
    .await?;
    if rolled_back == 0 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "该实例没有成功回滚记录",
        ));
    }
    let instance = get_instance(&state.db, &instance_id).await?;
    let target = Version::parse(&release.version)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "版本号不是有效的 SemVer"))?;
    let artifacts = release_artifacts(&state, &release_id)
        .await?
        .into_iter()
        .filter(|artifact| artifact.status == "published")
        .collect::<Vec<_>>();
    validate_rollout_instance_record(&instance, &target, &artifacts)?;
    let artifact = artifacts
        .iter()
        .find(|artifact| {
            target_matches(
                &instance.os,
                &instance.package_type,
                &instance.native_arch,
                artifact,
            )
        })
        .ok_or_else(|| AppError::new(StatusCode::CONFLICT, "没有适用于该实例的更新包"))?;
    let now = now_ts();
    let mut transaction = state.db.begin().await?;
    let release = locked_release(&mut transaction, &release_id).await?;
    if !matches!(
        release.rollout_state.as_str(),
        "canary_active" | "canary_paused" | "full_active" | "full_paused"
    ) {
        return Err(AppError::new(StatusCode::CONFLICT, "当前版本不能重新升级"));
    }
    sqlx::query(
        r#"
        INSERT INTO agent_release_targets(release_id, instance_id, state, created_at, updated_at)
        VALUES($1, $2, 'included', $3, $4)
        ON CONFLICT(release_id, instance_id) DO UPDATE
        SET state = 'included', updated_at = EXCLUDED.updated_at
        "#,
    )
    .bind(&release_id)
    .bind(&instance_id)
    .bind(now)
    .bind(now)
    .execute(&mut *transaction)
    .await?;
    let capability = capability_row_from_instance(&instance);
    let queued =
        queue_upgrade_attempt(&mut transaction, &release, &capability, artifact, now, true).await?;
    if !queued {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "实例已有活动更新任务或当前版本已不再适用，请刷新后重试",
        ));
    }
    write_action_log(
        &mut *transaction,
        &admin.username,
        Some(&admin.user_id),
        "reupgrade_agent_instance",
        &instance_id,
        &format!("重新升级实例到 Agent {}", release.version),
    )
    .await?;
    transaction.commit().await?;
    if queued
        && matches!(
            release.rollout_state.as_str(),
            "canary_active" | "full_active"
        )
    {
        notify_instance(&state, &instance_id).await;
    }
    Ok(Json(
        load_release_detail(&state, get_release(&state, &release_id).await?).await?,
    ))
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
            SELECT id, release_id, artifact_id, instance_id, operation, parent_attempt_id,
                   from_version, target_version,
                   status, message, retry_count, created_at, updated_at, completed_at
            FROM agent_update_attempts AS u
            WHERE u.release_id = $1
              AND EXISTS (SELECT 1 FROM instances AS i WHERE i.id = u.instance_id)
            ORDER BY u.updated_at DESC
            "#,
        )
        .bind(release_id)
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as::<_, AgentUpdateAttemptRecord>(
            r#"
            SELECT id, release_id, artifact_id, instance_id, operation, parent_attempt_id,
                   from_version, target_version,
                   status, message, retry_count, created_at, updated_at, completed_at
            FROM agent_update_attempts AS u
            WHERE EXISTS (SELECT 1 FROM instances AS i WHERE i.id = u.instance_id)
            ORDER BY u.updated_at DESC LIMIT 1000
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
    let observed_attempt = get_attempt(&state, &attempt_id).await?;
    let now = now_ts();
    let mut transaction = state.db.begin().await?;
    // Rollout controls lock the release before touching attempts. Using the same order makes a
    // concurrent retry either commit before rollback cancellation or observe the rollback state.
    let release = locked_release(&mut transaction, &observed_attempt.release_id).await?;
    let attempt = locked_attempt(&mut transaction, &attempt_id).await?;
    if attempt.release_id != release.id {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "更新记录所属版本已发生变化，请刷新后重试",
        ));
    }
    let retryable = match attempt.operation.as_str() {
        "upgrade" => matches!(attempt.status.as_str(), "failed" | "rollback_succeeded"),
        "rollback" => attempt.status == "failed",
        _ => false,
    };
    if !retryable {
        return Err(AppError::new(StatusCode::CONFLICT, "当前更新记录不能重试"));
    }
    let rollout_allows_retry = match attempt.operation.as_str() {
        "upgrade" => matches!(
            release.rollout_state.as_str(),
            "canary_active" | "canary_paused" | "full_active" | "full_paused"
        ),
        "rollback" => matches!(
            release.rollout_state.as_str(),
            "canary_active"
                | "canary_paused"
                | "full_active"
                | "full_paused"
                | "rollback_active"
                | "rollback_partial"
        ),
        _ => false,
    };
    if !rollout_allows_retry {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "当前版本的发布流程不允许重试该更新记录",
        ));
    }
    if attempt.retry_count >= MAX_AGENT_UPDATE_RETRY_COUNT {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            format!("更新记录最多只能重试 {MAX_AGENT_UPDATE_RETRY_COUNT} 次"),
        ));
    }
    let rollback_artifact_id = if attempt.operation == "upgrade" {
        require_latest_retry_candidate(&state, &attempt).await?;
        None
    } else {
        let instance = locked_instance(&mut transaction, &attempt.instance_id).await?;
        if instance.agent_version != attempt.from_version {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "实例当前版本与回滚记录不一致，不能重试",
            ));
        }
        if instance.rollback_supported != 1 {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "当前 Agent 不支持主动回滚",
            ));
        }
        let artifact =
            rollback_artifact_for_instance(&state, &instance, &attempt.target_version).await?;
        if artifact.is_none() && instance.rollback_version != attempt.target_version {
            return Err(AppError::new(StatusCode::CONFLICT, "仍然没有可用的回滚包"));
        }
        artifact.map(|artifact| artifact.id)
    };
    if attempt.operation == "rollback" && release.rollout_state == "rollback_partial" {
        ensure_no_other_controlled_release(&mut transaction, &attempt.release_id).await?;
        let activated = sqlx::query(
            "UPDATE agent_releases SET rollout_state = 'rollback_active', rollout_updated_at = $1 WHERE id = $2 AND rollout_state = 'rollback_partial'",
        )
        .bind(now)
        .bind(&attempt.release_id)
        .execute(&mut *transaction)
        .await?;
        if activated.rows_affected() != 1 {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "版本回滚状态已发生变化，请刷新后重试",
            ));
        }
    }
    let active_attempt: Option<(String, String)> = sqlx::query_as(
        r#"
        SELECT operation, status
        FROM agent_update_attempts
        WHERE instance_id = $1 AND id <> $2
          AND status NOT IN ('succeeded', 'rollback_succeeded', 'failed', 'cancelled')
        ORDER BY updated_at DESC
        LIMIT 1
        FOR UPDATE
        "#,
    )
    .bind(&attempt.instance_id)
    .bind(&attempt_id)
    .fetch_optional(&mut *transaction)
    .await?;
    if let Some((operation, status)) = active_attempt {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            format!("实例已有活动的 {operation} 任务（{status}），完成后才能重试"),
        ));
    }
    let retried = sqlx::query(
        r#"
        UPDATE agent_update_attempts
        SET artifact_id = COALESCE($1, artifact_id), status = 'pending', message = '', retry_count = retry_count + 1,
            updated_at = $2, completed_at = NULL
        WHERE id = $3 AND status = $4 AND retry_count = $5 AND retry_count < $6
        "#,
    )
    .bind(rollback_artifact_id.as_deref())
    .bind(now)
    .bind(&attempt_id)
    .bind(&attempt.status)
    .bind(attempt.retry_count)
    .bind(MAX_AGENT_UPDATE_RETRY_COUNT)
    .execute(&mut *transaction)
    .await;
    let retried = map_unique_conflict(retried, "实例已有其他活动更新任务，完成后才能重试")?;
    if retried.rows_affected() != 1 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "更新状态已发生变化，请刷新后重试",
        ));
    }
    write_action_log(
        &mut *transaction,
        &admin.username,
        Some(&admin.user_id),
        "retry_agent_update",
        &attempt_id,
        &format!("重试实例 {} 的 Agent 更新", attempt.instance_id),
    )
    .await?;
    transaction.commit().await?;
    notify_retried_attempt(&state, &attempt.instance_id, &attempt_id).await;
    Ok(Json(get_attempt(&state, &attempt_id).await?))
}

pub async fn agent_update_manifest(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<AgentUpdateManifest>> {
    let instance = authenticate_agent_headers(&state, &headers).await?;
    let rollback = find_rollback_for_instance(&state, &instance).await?;
    let update = if rollback.is_none() {
        find_update_for_instance(&state, &instance).await?
    } else {
        None
    };
    Ok(Json(AgentUpdateManifest { update, rollback }))
}

pub async fn agent_download_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(artifact_id): Path<String>,
) -> AppResult<Response> {
    let artifact = authorized_artifact_download(&state, &headers, &artifact_id).await?;
    let extension = FsPath::new(&artifact.file_name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("bin");
    let disposition = HeaderValue::from_str(&format!(
        "attachment; filename=\"agent-update-{}.{}\"",
        artifact.id, extension
    ))
    .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "下载文件名格式无效"))?;
    artifact_download_response(&state, &artifact, disposition).await
}

async fn artifact_download_response(
    state: &AppState,
    artifact: &AgentArtifactRecord,
    disposition: HeaderValue,
) -> AppResult<Response> {
    let path = safe_storage_path(state, &artifact.storage_path)?;
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
               a.size_bytes, a.sha256, a.storage_path, a.created_at, a.status, a.published_at
        FROM agent_artifacts a
        JOIN agent_releases r ON r.id = a.release_id
        WHERE a.id = $1 AND r.status = 'published' AND a.status = 'published'
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
        "SELECT COUNT(*) FROM agent_update_attempts WHERE artifact_id = $1 AND instance_id = $2",
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

pub async fn record_connection_update_status(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    attempt_id: Option<&str>,
    release_id: &str,
    artifact_id: &str,
    version: &str,
    retry_count: i64,
    status: &str,
    message: Option<&str>,
) -> AppResult<bool> {
    let agents = state.agents.read().await;
    let is_current_connection = agents
        .get(instance_id)
        .is_some_and(|agent| agent.connection_id == connection_id);
    if !is_current_connection {
        return Ok(false);
    }

    record_update_status_for_attempt(
        state,
        instance_id,
        attempt_id,
        release_id,
        artifact_id,
        version,
        retry_count,
        status,
        message,
    )
    .await?;
    drop(agents);
    reconcile_release_rollback(state, release_id).await?;
    Ok(true)
}

#[cfg(test)]
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
    record_update_status_for_attempt(
        state,
        instance_id,
        None,
        release_id,
        artifact_id,
        version,
        retry_count,
        status,
        message,
    )
    .await?;
    reconcile_release_rollback(state, release_id).await
}

async fn record_update_status_for_attempt(
    state: &AppState,
    instance_id: &str,
    attempt_id: Option<&str>,
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

    let mut transaction = state.db.begin().await?;
    let current = if let Some(attempt_id) = attempt_id {
        sqlx::query_as::<_, (String, String, String)>(
            r#"
            SELECT id, status, from_version FROM agent_update_attempts
            WHERE id = $1 AND instance_id = $2 AND operation = 'upgrade'
              AND release_id = $3 AND artifact_id = $4 AND target_version = $5
              AND retry_count = $6
            FOR UPDATE
            "#,
        )
        .bind(attempt_id)
        .bind(instance_id)
        .bind(release_id)
        .bind(artifact_id)
        .bind(version)
        .bind(retry_count)
        .fetch_optional(&mut *transaction)
        .await?
    } else {
        sqlx::query_as::<_, (String, String, String)>(
            r#"
            SELECT id, status, from_version FROM agent_update_attempts
            WHERE instance_id = $1 AND release_id = $2 AND artifact_id = $3
              AND target_version = $4 AND retry_count = $5 AND operation = 'upgrade'
            ORDER BY updated_at DESC LIMIT 1 FOR UPDATE
            "#,
        )
        .bind(instance_id)
        .bind(release_id)
        .bind(artifact_id)
        .bind(version)
        .bind(retry_count)
        .fetch_optional(&mut *transaction)
        .await?
    }
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Agent 更新任务不存在或信息不匹配"))?;
    let (stored_attempt_id, current_status, from_version) = current;
    if current_status == status && TERMINAL_ATTEMPT_STATUSES.contains(&status) {
        if status == "succeeded" {
            sqlx::query("UPDATE instances SET agent_version = $1 WHERE id = $2")
                .bind(version)
                .bind(instance_id)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        return Ok(());
    }
    if !update_status_transition_allowed(&current_status, status) {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "Agent 更新状态不允许回退或离开终态",
        ));
    }

    let now = now_ts();
    let completed_at = TERMINAL_ATTEMPT_STATUSES.contains(&status).then_some(now);
    sqlx::query(
        r#"
        UPDATE agent_update_attempts
        SET status = $1, message = $2, updated_at = $3, completed_at = $4
        WHERE id = $5
        "#,
    )
    .bind(status)
    .bind(message)
    .bind(now)
    .bind(completed_at)
    .bind(&stored_attempt_id)
    .execute(&mut *transaction)
    .await?;
    if status == "succeeded" {
        sqlx::query("UPDATE instances SET agent_version = $1 WHERE id = $2")
            .bind(version)
            .bind(instance_id)
            .execute(&mut *transaction)
            .await?;
    } else if status == "rollback_succeeded" {
        sqlx::query("UPDATE instances SET agent_version = $1 WHERE id = $2")
            .bind(&from_version)
            .bind(instance_id)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(())
}

pub async fn record_connection_rollback_status(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    attempt_id: &str,
    retry_count: i64,
    status: &str,
    message: Option<&str>,
) -> AppResult<bool> {
    let agents = state.agents.read().await;
    let is_current_connection = agents
        .get(instance_id)
        .is_some_and(|agent| agent.connection_id == connection_id);
    if !is_current_connection {
        return Ok(false);
    }
    if !valid_agent_status(status) || status == "succeeded" {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "未知的 Agent 回滚状态",
        ));
    }
    let message = message.unwrap_or_default();
    if message.len() > MAX_STATUS_MESSAGE_BYTES {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Agent 回滚状态信息过长",
        ));
    }
    let mut transaction = state.db.begin().await?;
    let current = sqlx::query_as::<_, (String, String, String)>(
        r#"
        SELECT status, release_id, target_version FROM agent_update_attempts
        WHERE id = $1 AND instance_id = $2 AND operation = 'rollback' AND retry_count = $3
        FOR UPDATE
        "#,
    )
    .bind(attempt_id)
    .bind(instance_id)
    .bind(retry_count)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Agent 回滚任务不存在或信息不匹配"))?;
    if current.0 != status && !update_status_transition_allowed(&current.0, status) {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "Agent 回滚状态不允许回退或离开终态",
        ));
    }
    let now = now_ts();
    let completed_at = TERMINAL_ATTEMPT_STATUSES.contains(&status).then_some(now);
    sqlx::query(
        "UPDATE agent_update_attempts SET status = $1, message = $2, updated_at = $3, completed_at = $4 WHERE id = $5",
    )
    .bind(status)
    .bind(message)
    .bind(now)
    .bind(completed_at)
    .bind(attempt_id)
    .execute(&mut *transaction)
    .await?;
    if status == "rollback_succeeded" {
        sqlx::query(
            "UPDATE instances SET agent_version = $1, rollback_supported = 0, rollback_version = '' WHERE id = $2",
        )
            .bind(&current.2)
            .bind(instance_id)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    drop(agents);
    reconcile_release_rollback(state, &current.1).await?;
    Ok(true)
}

pub async fn confirm_update_version(
    state: &AppState,
    instance_id: &str,
    agent_version: &str,
) -> AppResult<()> {
    let now = now_ts();
    let mut transaction = state.db.begin().await?;
    let confirmed_operations = sqlx::query_scalar::<_, String>(
        r#"
        UPDATE agent_update_attempts
        SET status = CASE WHEN operation = 'rollback' THEN 'rollback_succeeded' ELSE 'succeeded' END,
            message = '', updated_at = $1, completed_at = $2
        WHERE instance_id = $3 AND target_version = $4
          AND (
              status NOT IN ('succeeded', 'rollback_succeeded', 'failed', 'cancelled')
              OR (status = 'cancelled' AND operation = 'upgrade' AND EXISTS (
                  SELECT 1 FROM agent_releases r
                  WHERE r.id = agent_update_attempts.release_id AND r.rollout_state = 'rollback_active'
              ))
          )
        RETURNING operation
        "#,
    )
    .bind(now)
    .bind(now)
    .bind(instance_id)
    .bind(agent_version)
    .fetch_all(&mut *transaction)
    .await?;
    if confirmed_operations
        .iter()
        .any(|operation| operation == "rollback")
    {
        sqlx::query(
            "UPDATE instances SET rollback_supported = 0, rollback_version = '' WHERE id = $1",
        )
        .bind(instance_id)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    let rollback_releases = sqlx::query_scalar::<_, String>(
        "SELECT id FROM agent_releases WHERE rollout_state = 'rollback_active'",
    )
    .fetch_all(&state.db)
    .await?;
    for release_id in rollback_releases {
        reconcile_release_rollback(state, &release_id).await?;
    }
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
        let rollback_releases = sqlx::query_scalar::<_, String>(
            "SELECT id FROM agent_releases WHERE rollout_state = 'rollback_active'",
        )
        .fetch_all(&state.db)
        .await;
        if let Ok(release_ids) = rollback_releases {
            for release_id in release_ids {
                if let Err(error) = reconcile_release_rollback(&state, &release_id).await {
                    warn!(?error, %release_id, "failed to reconcile Agent rollback");
                }
            }
        }
    }
}

async fn expire_restart_attempts(state: &AppState) -> AppResult<u64> {
    let now = now_ts();
    let result = sqlx::query(
        r#"
        UPDATE agent_update_attempts
        SET status = 'failed', message = '更新进程启动后 60 分钟内未完成重连', updated_at = $1, completed_at = $2
        WHERE status IN ('installing', 'awaiting_restart') AND updated_at < $3
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
            sha256: data_encoding::HEXLOWER.encode(digest.finalize().as_slice()),
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
    let id = Uuid::new_v4().to_string();
    let extension = expected_extension(&received.os);
    let relative_path = format!("{release_id}/{id}.{extension}");
    let checksum_relative_path = format!("{relative_path}.sha256");
    let final_dir = state.update_dir.join(release_id);
    let final_path = state.update_dir.join(&relative_path);
    let checksum_final_path = state.update_dir.join(&checksum_relative_path);
    if let Err(error) = fs::create_dir_all(&final_dir).await {
        let _ = fs::remove_file(&received.temp_path).await;
        return Err(error.into());
    }
    if let Err(error) = fs::rename(&received.temp_path, &final_path).await {
        let _ = fs::remove_file(&received.temp_path).await;
        return Err(error.into());
    }
    if let Err(error) = fs::write(&checksum_final_path, &received.checksum_contents).await {
        let _ = fs::remove_file(&final_path).await;
        let _ = fs::remove_file(&checksum_final_path).await;
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
        status: "draft".to_string(),
        published_at: None,
    };
    let persisted: AppResult<()> = async {
        let mut transaction = state.db.begin().await?;
        let release_exists = sqlx::query_scalar::<_, String>(
            "SELECT id FROM agent_releases WHERE id = $1 FOR SHARE",
        )
        .bind(release_id)
        .fetch_optional(&mut *transaction)
        .await?;
        if release_exists.is_none() {
            return Err(AppError::new(StatusCode::NOT_FOUND, "Agent 版本不存在"));
        }
        let result = sqlx::query(
            r#"
            INSERT INTO agent_artifacts(id, release_id, os, package_type, native_arch, file_name,
                                        size_bytes, sha256, storage_path, created_at, status)
            VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'draft')
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
        .execute(&mut *transaction)
        .await;
        let inserted = map_unique_conflict(result, "该版本已包含相同目标的可执行文件")?;
        debug_assert_eq!(inserted.rows_affected(), 1);
        transaction.commit().await?;
        Ok(())
    }
    .await;
    if let Err(error) = persisted {
        let _ = fs::remove_file(&final_path).await;
        let _ = fs::remove_file(&checksum_final_path).await;
        return Err(error);
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

fn update_status_transition_allowed(current: &str, next: &str) -> bool {
    if TERMINAL_ATTEMPT_STATUSES.contains(&current) {
        return current == next;
    }
    if TERMINAL_ATTEMPT_STATUSES.contains(&next) {
        return true;
    }

    let progress = |status| match status {
        "pending" => Some(0),
        "waiting" => Some(1),
        "downloading" => Some(2),
        "verifying" => Some(3),
        "waiting_idle" => Some(4),
        "installing" => Some(5),
        "awaiting_restart" => Some(6),
        _ => None,
    };
    progress(current)
        .zip(progress(next))
        .is_some_and(|(current, next)| next >= current)
}

async fn get_release(state: &AppState, release_id: &str) -> AppResult<AgentReleaseRecord> {
    sqlx::query_as::<_, AgentReleaseRecord>(
        "SELECT id, version, notes, status, rollout_state, rollout_updated_at, created_at, published_at FROM agent_releases WHERE id = $1",
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
               storage_path, created_at, status, published_at
        FROM agent_artifacts WHERE release_id = $1 ORDER BY os, package_type, native_arch
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
               storage_path, created_at, status, published_at
        FROM agent_artifacts WHERE id = $1
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
    let instances = capability_instances(state).await?;
    let attempts = sqlx::query_as::<_, AgentUpdateAttemptRecord>(
        r#"
        SELECT id, release_id, artifact_id, instance_id, operation, parent_attempt_id,
               from_version, target_version,
               status, message, retry_count, created_at, updated_at, completed_at
        FROM agent_update_attempts AS u
        WHERE u.release_id = $1
          AND EXISTS (SELECT 1 FROM instances AS i WHERE i.id = u.instance_id)
        ORDER BY u.updated_at DESC
        "#,
    )
    .bind(&release.id)
    .fetch_all(&state.db)
    .await?;
    let mut eligible_instances = 0;
    let mut covered_instances = 0;
    let mut missing_artifact_instances = 0;
    let mut unprivileged_instances = 0;
    for instance in &instances {
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
    let selected_instances: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_release_targets WHERE release_id = $1 AND state = 'included'",
    )
    .bind(&release.id)
    .fetch_one(&state.db)
    .await?;
    let rollback_artifacts = sqlx::query_as::<_, PublishedRollbackArtifact>(
        r#"
        SELECT r.version, a.os, a.package_type, a.native_arch
        FROM agent_releases r
        JOIN agent_artifacts a ON a.release_id = r.id
        WHERE r.status = 'published' AND a.status = 'published'
        "#,
    )
    .fetch_all(&state.db)
    .await?;
    let mut covered_rollback_instances = HashSet::new();
    let succeeded = attempts
        .iter()
        .filter(|attempt| {
            attempt.operation == "upgrade"
                && attempt.status == "succeeded"
                && covered_rollback_instances.insert(attempt.instance_id.as_str())
        })
        .collect::<Vec<_>>();
    let succeeded_upgrades = succeeded.len() as i64;
    let mut rollback_supported = 0;
    let mut server_package_available = 0;
    let mut local_package_available = 0;
    let mut unavailable = 0;
    let rollback_outcomes = latest_rollback_outcomes(&attempts);
    for attempt in &succeeded {
        if rollback_outcomes
            .get(&attempt.id)
            .is_some_and(|status| status == "rollback_succeeded")
        {
            continue;
        }
        let Some(instance) = instances
            .iter()
            .find(|instance| instance.id == attempt.instance_id)
        else {
            unavailable += 1;
            continue;
        };
        let supported = instance.rollback_supported == 1;
        let privileged = instance.update_privileged == 1;
        let server = rollback_artifacts.iter().any(|artifact| {
            artifact.version == attempt.from_version
                && artifact.package_type == instance.package_type
                && artifact.native_arch == instance.native_arch
                && update_target_os(&instance.package_type, &instance.os)
                    == Some(artifact.os.as_str())
        });
        let local = instance.rollback_version == attempt.from_version;
        rollback_supported += i64::from(supported);
        server_package_available += i64::from(server);
        local_package_available += i64::from(local);
        unavailable += i64::from(!supported || !privileged || (!server && !local));
    }
    let active_rollbacks = attempts
        .iter()
        .filter(|attempt| {
            attempt.operation == "rollback"
                && !TERMINAL_ATTEMPT_STATUSES.contains(&attempt.status.as_str())
        })
        .count() as i64;
    let failed_rollbacks = rollback_outcomes
        .values()
        .filter(|status| status.as_str() == "failed")
        .count() as i64;
    Ok(AgentReleaseDetail {
        release,
        artifacts,
        attempts,
        coverage: AgentReleaseCoverage {
            eligible_instances,
            covered_instances,
            missing_artifact_instances,
            unprivileged_instances,
            selected_instances,
        },
        rollback_coverage: AgentRollbackCoverage {
            succeeded_upgrades,
            rollback_supported,
            server_package_available,
            local_package_available,
            unavailable,
            active_rollbacks,
            failed_rollbacks,
        },
    })
}

async fn capability_instances(state: &AppState) -> AppResult<Vec<InstanceCapabilityRow>> {
    Ok(sqlx::query_as::<_, InstanceCapabilityRow>(
        r#"
        SELECT id, name, hostname, os, agent_version, package_type, native_arch,
               update_privileged, rollback_supported, rollback_version
        FROM instances WHERE approved = 1 AND disabled = 0
        "#,
    )
    .fetch_all(&state.db)
    .await?)
}

fn latest_rollback_outcomes(attempts: &[AgentUpdateAttemptRecord]) -> HashMap<String, String> {
    let mut outcomes: HashMap<String, (i64, i64, String, String)> = HashMap::new();
    for attempt in attempts {
        if attempt.operation != "rollback" {
            continue;
        }
        let key = attempt
            .parent_attempt_id
            .clone()
            .unwrap_or_else(|| format!("instance:{}", attempt.instance_id));
        let replace = outcomes
            .get(&key)
            .is_none_or(|(updated_at, created_at, id, _)| {
                (attempt.updated_at, attempt.created_at, attempt.id.as_str())
                    > (*updated_at, *created_at, id.as_str())
            });
        if replace {
            outcomes.insert(
                key,
                (
                    attempt.updated_at,
                    attempt.created_at,
                    attempt.id.clone(),
                    attempt.status.clone(),
                ),
            );
        }
    }
    outcomes
        .into_iter()
        .map(|(key, (_, _, _, status))| (key, status))
        .collect()
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

fn publish_attempt_state(
    instance: &InstanceCapabilityRow,
    now: i64,
) -> (&'static str, &'static str, Option<i64>) {
    if instance.update_privileged == 1 {
        ("pending", "", None)
    } else {
        (
            "failed",
            "Agent 进程没有替换当前可执行文件所需的权限",
            Some(now),
        )
    }
}

fn matching_artifact<'a>(
    instance: &InstanceCapabilityRow,
    artifacts: &'a [AgentArtifactRecord],
) -> Option<&'a AgentArtifactRecord> {
    artifacts.iter().find(|artifact| {
        target_matches(
            &instance.os,
            &instance.package_type,
            &instance.native_arch,
            artifact,
        )
    })
}

fn rollout_selects_instance(
    rollout_state: &str,
    instance_id: &str,
    included_ids: &HashSet<String>,
    excluded_ids: &HashSet<String>,
) -> bool {
    (matches!(rollout_state, "full_active" | "full_paused") && !excluded_ids.contains(instance_id))
        || included_ids.contains(instance_id)
}

fn rollout_ineligibility_reason<'a>(
    instance: &InstanceCapabilityRow,
    target: &Version,
    artifacts: &[AgentArtifactRecord],
) -> Option<&'a str> {
    if instance.update_privileged != 1 {
        return Some("Agent 没有自动更新权限");
    }
    if update_target_os(&instance.package_type, &instance.os).is_none()
        || instance.native_arch.is_empty()
    {
        return Some("Agent 不是受管 standalone 安装");
    }
    if !version_is_newer(target, &instance.agent_version) {
        return Some("实例版本不低于目标版本");
    }
    if matching_artifact(instance, artifacts).is_none() {
        return Some("缺少匹配系统和架构的更新包");
    }
    None
}

fn validate_rollout_instance(
    instance: &InstanceCapabilityRow,
    target: &Version,
    artifacts: &[AgentArtifactRecord],
) -> AppResult<()> {
    if let Some(reason) = rollout_ineligibility_reason(instance, target, artifacts) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            format!("实例 {} 不可加入灰度：{reason}", instance.name),
        ));
    }
    Ok(())
}

fn capability_row_from_instance(instance: &InstanceRecord) -> InstanceCapabilityRow {
    InstanceCapabilityRow {
        id: instance.id.clone(),
        name: instance.name.clone(),
        hostname: instance.hostname.clone(),
        os: instance.os.clone(),
        agent_version: instance.agent_version.clone(),
        package_type: instance.package_type.clone(),
        native_arch: instance.native_arch.clone(),
        update_privileged: instance.update_privileged,
        rollback_supported: instance.rollback_supported,
        rollback_version: instance.rollback_version.clone(),
    }
}

fn validate_rollout_instance_record(
    instance: &InstanceRecord,
    target: &Version,
    artifacts: &[AgentArtifactRecord],
) -> AppResult<()> {
    validate_rollout_instance(&capability_row_from_instance(instance), target, artifacts)
}

async fn queue_upgrade_attempt(
    transaction: &mut Transaction<'_, Postgres>,
    release: &AgentReleaseRecord,
    instance: &InstanceCapabilityRow,
    artifact: &AgentArtifactRecord,
    now: i64,
    explicit_reupgrade: bool,
) -> AppResult<bool> {
    let target = Version::parse(&release.version)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "版本号不是有效的 SemVer"))?;
    if !version_is_newer(&target, &instance.agent_version) {
        return Ok(false);
    }
    if !explicit_reupgrade {
        let attempted: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM agent_update_attempts
                WHERE release_id = $1 AND instance_id = $2 AND operation = 'upgrade'
            )
            "#,
        )
        .bind(&release.id)
        .bind(&instance.id)
        .fetch_one(&mut **transaction)
        .await?;
        if attempted {
            return Ok(false);
        }
    }
    let (status, message, completed_at) = publish_attempt_state(instance, now);
    let inserted = sqlx::query(
        r#"
        INSERT INTO agent_update_attempts(
            id, release_id, artifact_id, instance_id, operation, from_version,
            target_version, status, message, retry_count, created_at, updated_at, completed_at
        ) VALUES($1, $2, $3, $4, 'upgrade', $5, $6, $7, $8, 0, $9, $10, $11)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&release.id)
    .bind(&artifact.id)
    .bind(&instance.id)
    .bind(&instance.agent_version)
    .bind(&release.version)
    .bind(status)
    .bind(message)
    .bind(now)
    .bind(now)
    .bind(completed_at)
    .execute(&mut **transaction)
    .await?;
    Ok(inserted.rows_affected() == 1 && status == "pending")
}

async fn locked_release(
    transaction: &mut Transaction<'_, Postgres>,
    release_id: &str,
) -> AppResult<AgentReleaseRecord> {
    sqlx::query_as::<_, AgentReleaseRecord>(
        "SELECT id, version, notes, status, rollout_state, rollout_updated_at, created_at, published_at FROM agent_releases WHERE id = $1 FOR UPDATE",
    )
    .bind(release_id)
    .fetch_optional(&mut **transaction)
    .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Agent 版本不存在"))
}

async fn locked_attempt(
    transaction: &mut Transaction<'_, Postgres>,
    attempt_id: &str,
) -> AppResult<AgentUpdateAttemptRecord> {
    sqlx::query_as::<_, AgentUpdateAttemptRecord>(
        r#"
        SELECT id, release_id, artifact_id, instance_id, operation, parent_attempt_id,
               from_version, target_version, status, message, retry_count, created_at,
               updated_at, completed_at
        FROM agent_update_attempts WHERE id = $1 FOR UPDATE
        "#,
    )
    .bind(attempt_id)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Agent 更新记录不存在"))
}

async fn locked_instance(
    transaction: &mut Transaction<'_, Postgres>,
    instance_id: &str,
) -> AppResult<InstanceRecord> {
    sqlx::query_as::<_, InstanceRecord>(
        r#"
        SELECT id, secret, name, region, country_code, country, province_code, province, city,
               remark, hostname, os, arch, agent_version, package_type, native_arch,
               update_privileged, rollback_supported, rollback_version, approved, disabled,
               first_seen, last_seen, expires_at
        FROM instances WHERE id = $1 FOR UPDATE
        "#,
    )
    .bind(instance_id)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "实例不存在"))
}

async fn ensure_no_other_controlled_release(
    transaction: &mut Transaction<'_, Postgres>,
    release_id: &str,
) -> AppResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ROLLOUT_CONTROL_LOCK_ID)
        .execute(&mut **transaction)
        .await?;
    let active: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM agent_releases
        WHERE id <> $1 AND rollout_state IN (
            'canary_active', 'canary_paused', 'full_paused', 'rollback_active'
        )
        "#,
    )
    .bind(release_id)
    .fetch_one(&mut **transaction)
    .await?;
    if active > 0 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "已有其他 Agent 版本处于灰度、暂停或回滚流程",
        ));
    }
    Ok(())
}

async fn set_rollout_paused(
    state: &AppState,
    headers: &HeaderMap,
    release_id: &str,
    paused: bool,
) -> AppResult<Json<AgentReleaseDetail>> {
    let admin = require_admin(state, headers).await?;
    let now = now_ts();
    let mut transaction = state.db.begin().await?;
    let release = locked_release(&mut transaction, release_id).await?;
    let next_state = match (release.rollout_state.as_str(), paused) {
        ("canary_active", true) => "canary_paused",
        ("full_active", true) => "full_paused",
        ("canary_paused", false) => "canary_active",
        ("full_paused", false) => "full_active",
        _ => {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                if paused {
                    "当前版本不能暂停"
                } else {
                    "当前版本没有暂停"
                },
            ));
        }
    };
    if paused {
        ensure_no_other_controlled_release(&mut transaction, release_id).await?;
    }
    sqlx::query(
        "UPDATE agent_releases SET rollout_state = $1, rollout_updated_at = $2 WHERE id = $3",
    )
    .bind(next_state)
    .bind(now)
    .bind(release_id)
    .execute(&mut *transaction)
    .await?;
    write_action_log(
        &mut *transaction,
        &admin.username,
        Some(&admin.user_id),
        if paused {
            "pause_agent_rollout"
        } else {
            "resume_agent_rollout"
        },
        release_id,
        if paused {
            "暂停 Agent 发布"
        } else {
            "恢复 Agent 发布"
        },
    )
    .await?;
    transaction.commit().await?;
    if !paused {
        let instance_ids = sqlx::query_scalar::<_, String>(
            "SELECT instance_id FROM agent_update_attempts WHERE release_id = $1 AND operation = 'upgrade' AND status = 'pending'",
        )
        .bind(release_id)
        .fetch_all(&state.db)
        .await?;
        notify_instances(state, instance_ids).await;
    }
    Ok(Json(
        load_release_detail(state, get_release(state, release_id).await?).await?,
    ))
}

async fn latest_successful_upgrade(
    state: &AppState,
    release_id: &str,
    instance_id: &str,
) -> AppResult<AgentUpdateAttemptRecord> {
    let attempt = sqlx::query_as::<_, AgentUpdateAttemptRecord>(
        r#"
        SELECT id, release_id, artifact_id, instance_id, operation, parent_attempt_id,
               from_version, target_version, status, message, retry_count, created_at,
               updated_at, completed_at
        FROM agent_update_attempts
        WHERE release_id = $1 AND instance_id = $2 AND operation = 'upgrade'
          AND status = 'succeeded'
        ORDER BY completed_at DESC NULLS LAST, updated_at DESC
        LIMIT 1
        "#,
    )
    .bind(release_id)
    .bind(instance_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::CONFLICT, "该实例没有成功升级记录"))?;
    let instance = get_instance(&state.db, instance_id).await?;
    if instance.agent_version != attempt.target_version {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "实例当前版本与升级记录不一致",
        ));
    }
    Ok(attempt)
}

async fn rollback_artifact_for_instance(
    state: &AppState,
    instance: &InstanceRecord,
    version: &str,
) -> AppResult<Option<AgentArtifactRecord>> {
    let Some(target_os) = update_target_os(&instance.package_type, &instance.os) else {
        return Ok(None);
    };
    Ok(sqlx::query_as::<_, AgentArtifactRecord>(
        r#"
        SELECT a.id, a.release_id, a.os, a.package_type, a.native_arch, a.file_name,
               a.size_bytes, a.sha256, a.storage_path, a.created_at, a.status, a.published_at
        FROM agent_artifacts a
        JOIN agent_releases r ON r.id = a.release_id
        WHERE r.version = $1 AND r.status = 'published' AND a.status = 'published'
          AND a.os = $2 AND a.package_type = $3 AND a.native_arch = $4
        LIMIT 1
        "#,
    )
    .bind(version)
    .bind(target_os)
    .bind(&instance.package_type)
    .bind(&instance.native_arch)
    .fetch_optional(&state.db)
    .await?)
}

async fn queue_rollback_for_upgrade(
    state: &AppState,
    upgrade: &AgentUpdateAttemptRecord,
) -> AppResult<bool> {
    let mut transaction = state.db.begin().await?;
    let outcome = queue_rollback_for_upgrade_in_transaction(&mut transaction, upgrade).await?;
    transaction.commit().await?;
    Ok(outcome.pending)
}

async fn queue_rollback_for_upgrade_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    upgrade: &AgentUpdateAttemptRecord,
) -> AppResult<RollbackQueueOutcome> {
    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_update_attempts WHERE parent_attempt_id = $1 AND operation = 'rollback'",
    )
    .bind(&upgrade.id)
    .fetch_one(&mut **transaction)
    .await?;
    if exists > 0 {
        return Ok(RollbackQueueOutcome {
            inserted: false,
            pending: false,
        });
    }
    let active_attempt: Option<(String, String)> = sqlx::query_as(
        r#"
        SELECT operation, status
        FROM agent_update_attempts
        WHERE instance_id = $1 AND id <> $2
          AND status NOT IN ('succeeded', 'rollback_succeeded', 'failed', 'cancelled')
        LIMIT 1
        "#,
    )
    .bind(&upgrade.instance_id)
    .bind(&upgrade.id)
    .fetch_optional(&mut **transaction)
    .await?;
    if active_attempt.is_some() {
        return Ok(RollbackQueueOutcome {
            inserted: false,
            pending: false,
        });
    }
    let instance = sqlx::query_as::<_, InstanceRecord>(
        r#"
        SELECT id, secret, name, region, country_code, country, province_code, province, city,
               remark, hostname, os, arch, agent_version, package_type, native_arch,
               update_privileged, rollback_supported, rollback_version, approved, disabled,
               first_seen, last_seen, expires_at
        FROM instances WHERE id = $1
        "#,
    )
    .bind(&upgrade.instance_id)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "实例不存在"))?;
    let artifact = if let Some(target_os) = update_target_os(&instance.package_type, &instance.os) {
        sqlx::query_as::<_, AgentArtifactRecord>(
            r#"
            SELECT a.id, a.release_id, a.os, a.package_type, a.native_arch, a.file_name,
                   a.size_bytes, a.sha256, a.storage_path, a.created_at, a.status, a.published_at
            FROM agent_artifacts a
            JOIN agent_releases r ON r.id = a.release_id
            WHERE r.version = $1 AND r.status = 'published' AND a.status = 'published'
              AND a.os = $2 AND a.package_type = $3 AND a.native_arch = $4
            LIMIT 1
            "#,
        )
        .bind(&upgrade.from_version)
        .bind(target_os)
        .bind(&instance.package_type)
        .bind(&instance.native_arch)
        .fetch_optional(&mut **transaction)
        .await?
    } else {
        None
    };
    let local_available = instance.rollback_version == upgrade.from_version;
    let supported = instance.rollback_supported == 1;
    let source_matches = instance.agent_version == upgrade.target_version;
    let (status, message, completed_at) = if instance.approved != 1 || instance.disabled == 1 {
        ("failed", "实例已停用或未获批准", Some(now_ts()))
    } else if !source_matches {
        ("failed", "实例当前版本与本次升级结果不一致", Some(now_ts()))
    } else if !supported {
        ("failed", "当前 Agent 不支持主动回滚", Some(now_ts()))
    } else if instance.update_privileged != 1 {
        (
            "failed",
            "Agent 进程没有替换当前可执行文件所需的权限",
            Some(now_ts()),
        )
    } else if artifact.is_none() && !local_available {
        ("failed", "没有服务端旧版本包或本地回滚基线", Some(now_ts()))
    } else {
        ("pending", "", None)
    };
    let now = now_ts();
    let inserted = sqlx::query(
        r#"
        INSERT INTO agent_update_attempts(
            id, release_id, artifact_id, instance_id, operation, parent_attempt_id,
            from_version, target_version, status, message, retry_count, created_at,
            updated_at, completed_at
        ) VALUES($1, $2, $3, $4, 'rollback', $5, $6, $7, $8, $9, 0, $10, $11, $12)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&upgrade.release_id)
    .bind(artifact.as_ref().map(|artifact| artifact.id.as_str()))
    .bind(&upgrade.instance_id)
    .bind(&upgrade.id)
    .bind(&upgrade.target_version)
    .bind(&upgrade.from_version)
    .bind(status)
    .bind(message)
    .bind(now)
    .bind(now)
    .bind(completed_at)
    .execute(&mut **transaction)
    .await;
    let inserted = match inserted {
        Ok(inserted) => inserted,
        Err(error)
            if error
                .as_database_error()
                .is_some_and(|error| error.is_unique_violation()) =>
        {
            return Ok(RollbackQueueOutcome {
                inserted: false,
                pending: false,
            });
        }
        Err(error) => return Err(error.into()),
    };
    let inserted = inserted.rows_affected() == 1;
    Ok(RollbackQueueOutcome {
        inserted,
        pending: inserted && status == "pending",
    })
}

async fn reconcile_release_rollback(state: &AppState, release_id: &str) -> AppResult<()> {
    let release = get_release(state, release_id).await?;
    if release.rollout_state != "rollback_active" {
        return Ok(());
    }
    let upgrades = sqlx::query_as::<_, AgentUpdateAttemptRecord>(
        r#"
        SELECT DISTINCT ON (instance_id)
               id, release_id, artifact_id, instance_id, operation, parent_attempt_id,
               from_version, target_version, status, message, retry_count, created_at,
               updated_at, completed_at
        FROM agent_update_attempts
        WHERE release_id = $1 AND operation = 'upgrade' AND status = 'succeeded'
        ORDER BY instance_id, completed_at DESC NULLS LAST, updated_at DESC
        "#,
    )
    .bind(release_id)
    .fetch_all(&state.db)
    .await?;
    let mut notify = Vec::new();
    for upgrade in &upgrades {
        if queue_rollback_for_upgrade(state, upgrade).await? {
            notify.push(upgrade.instance_id.clone());
        }
    }
    notify_instances(state, notify).await;
    let active: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM agent_update_attempts
        WHERE release_id = $1
          AND status NOT IN ('succeeded', 'rollback_succeeded', 'failed', 'cancelled')
        "#,
    )
    .bind(release_id)
    .fetch_one(&state.db)
    .await?;
    let unqueued: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM (
            SELECT DISTINCT ON (instance_id) id
            FROM agent_update_attempts
            WHERE release_id = $1 AND operation = 'upgrade' AND status = 'succeeded'
            ORDER BY instance_id, completed_at DESC NULLS LAST, updated_at DESC
        ) AS upgrade
        WHERE NOT EXISTS (
              SELECT 1 FROM agent_update_attempts AS rollback
              WHERE rollback.parent_attempt_id = upgrade.id
                AND rollback.operation = 'rollback'
          )
        "#,
    )
    .bind(release_id)
    .fetch_one(&state.db)
    .await?;
    if active == 0 && unqueued == 0 {
        let failed: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM (
                SELECT DISTINCT ON (parent_attempt_id) status
                FROM agent_update_attempts
                WHERE release_id = $1 AND operation = 'rollback'
                  AND parent_attempt_id IS NOT NULL
                ORDER BY parent_attempt_id, updated_at DESC, created_at DESC, id DESC
            ) AS latest
            WHERE status = 'failed'
            "#,
        )
        .bind(release_id)
        .fetch_one(&state.db)
        .await?;
        sqlx::query(
            "UPDATE agent_releases SET rollout_state = $1, rollout_updated_at = $2 WHERE id = $3 AND rollout_state = 'rollback_active'",
        )
        .bind(if failed > 0 { "rollback_partial" } else { "rolled_back" })
        .bind(now_ts())
        .bind(release_id)
        .execute(&state.db)
        .await?;
    }
    Ok(())
}

async fn authenticate_agent_headers(
    state: &AppState,
    headers: &HeaderMap,
) -> AppResult<InstanceRecord> {
    let instance_id = agent_header(headers, "x-agent-id")?;
    let secret = agent_header(headers, "x-agent-secret")?;
    let instance = get_instance(&state.db, instance_id).await?;
    if !agent_secret_matches(&instance.secret, secret) {
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
    if let Some(offer) = active_upgrade_offer(state, instance, None).await? {
        return Ok(Some(offer));
    }
    let Some(candidate) = select_update_candidate(state, instance).await? else {
        return Ok(None);
    };

    let now = now_ts();
    let attempt_id = Uuid::new_v4().to_string();
    let inserted: Option<String> = sqlx::query_scalar(
        r#"
        INSERT INTO agent_update_attempts(
            id, release_id, artifact_id, instance_id, operation, from_version, target_version,
            status, message, retry_count, created_at, updated_at
        )
        SELECT $1, $2, $3, $4, 'upgrade', $5, $6, 'pending', '', 0, $7, $8
        WHERE NOT EXISTS (
            SELECT 1 FROM agent_update_attempts
            WHERE release_id = $2 AND instance_id = $4 AND operation = 'upgrade'
        )
        ON CONFLICT DO NOTHING
        RETURNING id
        "#,
    )
    .bind(&attempt_id)
    .bind(&candidate.release_id)
    .bind(&candidate.artifact_id)
    .bind(&instance.id)
    .bind(&instance.agent_version)
    .bind(&candidate.version)
    .bind(now)
    .bind(now)
    .fetch_optional(&state.db)
    .await?;
    if inserted.is_none() {
        return Ok(None);
    }
    active_upgrade_offer(state, instance, Some(&attempt_id)).await
}

async fn active_upgrade_offer(
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
               a.sha256, a.size_bytes, u.retry_count, u.status, r.rollout_state
        FROM agent_update_attempts u
        JOIN agent_releases r ON r.id = u.release_id
        JOIN agent_artifacts a ON a.id = u.artifact_id
        WHERE u.instance_id = $1 AND u.operation = 'upgrade'
          AND u.status NOT IN ('succeeded', 'rollback_succeeded', 'failed', 'cancelled')
          AND r.status = 'published' AND a.status = 'published'
        ORDER BY u.updated_at DESC
        "#,
    )
    .bind(&instance.id)
    .fetch_all(&state.db)
    .await?;
    let candidate = candidates.into_iter().find(|candidate| {
        if attempt_id.is_some_and(|attempt_id| candidate.attempt_id != attempt_id) {
            return false;
        }
        true
    });
    let Some(mut candidate) = candidate else {
        return Ok(None);
    };
    if !active_offer_matches_instance(instance, &candidate) {
        if !matches!(
            candidate.status.as_str(),
            "pending" | "waiting" | "downloading" | "verifying" | "waiting_idle"
        ) {
            return Ok(None);
        }
        let target_os =
            update_target_os(&instance.package_type, &instance.os).expect("validated package type");
        let replacement = sqlx::query_as::<_, (String, String, String, String, String, i64)>(
            r#"
            SELECT id, os, package_type, native_arch, sha256, size_bytes
            FROM agent_artifacts
            WHERE release_id = $1 AND status = 'published' AND lower(os) = lower($2)
              AND package_type = $3 AND native_arch = $4
            LIMIT 1
            "#,
        )
        .bind(&candidate.release_id)
        .bind(target_os)
        .bind(&instance.package_type)
        .bind(&instance.native_arch)
        .fetch_optional(&state.db)
        .await?;
        let Some((artifact_id, os, package_type, native_arch, sha256, size_bytes)) = replacement
        else {
            return Ok(None);
        };
        let reconciled_status = if candidate.status == "pending" {
            "pending"
        } else {
            "waiting"
        };
        let reconciled = sqlx::query(
            r#"
            UPDATE agent_update_attempts
            SET artifact_id = $1, status = $2,
                message = '实例能力变化，已重新匹配更新包', updated_at = $3
            WHERE id = $4 AND status = $5
              AND status IN ('pending', 'waiting', 'downloading', 'verifying', 'waiting_idle')
            "#,
        )
        .bind(&artifact_id)
        .bind(reconciled_status)
        .bind(now_ts())
        .bind(&candidate.attempt_id)
        .bind(&candidate.status)
        .execute(&state.db)
        .await?;
        if reconciled.rows_affected() != 1 {
            return Ok(None);
        }
        candidate.artifact_id = artifact_id;
        candidate.os = os;
        candidate.package_type = package_type;
        candidate.native_arch = native_arch;
        candidate.sha256 = sha256;
        candidate.size_bytes = size_bytes;
        candidate.status = reconciled_status.to_string();
    }
    if candidate.status == "pending"
        && !matches!(
            candidate.rollout_state.as_str(),
            "canary_active" | "full_active"
        )
    {
        return Ok(None);
    }
    if candidate.status == "pending" {
        let delivered = sqlx::query(
            r#"
            UPDATE agent_update_attempts AS u
            SET status = 'waiting', updated_at = $1
            FROM agent_releases AS r
            WHERE u.id = $2 AND u.release_id = r.id AND u.status = 'pending'
              AND r.rollout_state IN ('canary_active', 'full_active')
            "#,
        )
        .bind(now_ts())
        .bind(&candidate.attempt_id)
        .execute(&state.db)
        .await?;
        if delivered.rows_affected() != 1 {
            return Ok(None);
        }
    }
    signed_offer(
        state,
        AgentUpdateOffer {
            attempt_id: Some(candidate.attempt_id),
            instance_id: Some(instance.id.clone()),
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
            target_os: Some(candidate.os),
            signature_key_id: None,
            signature: None,
            signature_v2: None,
            retry_count: candidate.retry_count,
        },
    )
    .map(Some)
}

fn active_offer_matches_instance(
    instance: &InstanceRecord,
    candidate: &RetriedUpdateCandidate,
) -> bool {
    update_target_os(&instance.package_type, &instance.os) == Some(candidate.os.as_str())
        && candidate.package_type == instance.package_type
        && candidate.native_arch == instance.native_arch
}

async fn find_rollback_for_instance(
    state: &AppState,
    instance: &InstanceRecord,
) -> AppResult<Option<AgentRollbackOffer>> {
    if instance.rollback_supported != 1 || instance.update_privileged != 1 {
        return Ok(None);
    }
    let candidate = sqlx::query_as::<_, RollbackCandidate>(
        r#"
        SELECT u.id AS attempt_id, u.release_id, u.instance_id, u.from_version,
               u.target_version, u.retry_count, u.status, u.artifact_id,
               a.os, a.package_type, a.native_arch, a.sha256, a.size_bytes
        FROM agent_update_attempts u
        LEFT JOIN agent_artifacts a ON a.id = u.artifact_id AND a.status = 'published'
        WHERE u.instance_id = $1 AND u.operation = 'rollback'
          AND u.status NOT IN ('succeeded', 'rollback_succeeded', 'failed', 'cancelled')
        ORDER BY u.updated_at DESC
        LIMIT 1
        "#,
    )
    .bind(&instance.id)
    .fetch_optional(&state.db)
    .await?;
    let Some(candidate) = candidate else {
        return Ok(None);
    };
    if instance.agent_version != candidate.from_version {
        return Ok(None);
    }
    if candidate.status == "pending" {
        let delivered = sqlx::query(
            "UPDATE agent_update_attempts SET status = 'waiting', updated_at = $1 WHERE id = $2 AND status = 'pending'",
        )
        .bind(now_ts())
        .bind(&candidate.attempt_id)
        .execute(&state.db)
        .await?;
        if delivered.rows_affected() != 1 {
            return Ok(None);
        }
    }
    let package = match (
        candidate.artifact_id,
        candidate.os,
        candidate.package_type,
        candidate.native_arch,
        candidate.sha256,
        candidate.size_bytes,
    ) {
        (
            Some(artifact_id),
            Some(os),
            Some(package_type),
            Some(native_arch),
            Some(sha256),
            Some(size_bytes),
        ) => Some(AgentRollbackPackage {
            download_url: format!("/api/agent/update/artifacts/{artifact_id}/download"),
            artifact_id,
            sha256,
            size_bytes,
            package_type,
            native_arch,
            target_os: os,
        }),
        _ => None,
    };
    let mut offer = AgentRollbackOffer {
        attempt_id: candidate.attempt_id,
        release_id: candidate.release_id,
        instance_id: candidate.instance_id,
        from_version: candidate.from_version,
        target_version: candidate.target_version,
        retry_count: candidate.retry_count,
        package,
        signature_key_id: None,
        signature: None,
    };
    if let Some(signer) = state.update_signer.as_deref() {
        signer.sign_rollback_offer(&mut offer)?;
    }
    Ok(Some(offer))
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
        SELECT r.id AS release_id, r.version, a.id AS artifact_id
        FROM agent_releases r
        JOIN agent_artifacts a ON a.release_id = r.id
        LEFT JOIN agent_release_targets t
          ON t.release_id = r.id AND t.instance_id = $4
        WHERE r.status = 'published' AND a.status = 'published'
          AND lower(a.os) = lower($1)
          AND a.package_type = $2 AND a.native_arch = $3
          AND (
              r.rollout_state = 'full_active'
              OR (r.rollout_state = 'canary_active' AND t.state = 'included')
          )
          AND COALESCE(t.state, '') <> 'excluded'
        "#,
    )
    .bind(update_target_os(&instance.package_type, &instance.os).expect("validated package type"))
    .bind(&instance.package_type)
    .bind(&instance.native_arch)
    .bind(&instance.id)
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
        || Some(candidate.artifact_id) != attempt.artifact_id
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

fn signed_offer(state: &AppState, mut offer: AgentUpdateOffer) -> AppResult<AgentUpdateOffer> {
    if let Some(signer) = state.update_signer.as_deref() {
        signer.sign_offer(&mut offer)?;
    }
    Ok(offer)
}

fn outbound_offer(offer: AgentUpdateOffer) -> AgentOutbound {
    AgentOutbound::UpdateAvailable {
        attempt_id: offer.attempt_id,
        instance_id: offer.instance_id,
        release_id: offer.release_id,
        version: offer.version,
        artifact_id: offer.artifact_id,
        download_url: offer.download_url,
        sha256: offer.sha256,
        size_bytes: offer.size_bytes,
        package_type: offer.package_type,
        native_arch: offer.native_arch,
        target_os: offer.target_os,
        signature_key_id: offer.signature_key_id,
        signature: offer.signature,
        signature_v2: offer.signature_v2,
        retry_count: offer.retry_count,
    }
}

async fn notify_instance(state: &AppState, instance_id: &str) {
    let Ok(instance) = get_instance(&state.db, instance_id).await else {
        return;
    };
    let Some(handle) = state.agents.read().await.get(instance_id).cloned() else {
        return;
    };
    if let Ok(Some(rollback)) = find_rollback_for_instance(state, &instance).await {
        let _ = handle
            .tx
            .send(AgentOutbound::RollbackAvailable { offer: rollback });
        return;
    }
    if let Ok(Some(offer)) = find_update_for_instance(state, &instance).await {
        let _ = handle.tx.send(outbound_offer(offer));
    }
}

async fn notify_retried_attempt(state: &AppState, instance_id: &str, attempt_id: &str) {
    let Ok(instance) = get_instance(&state.db, instance_id).await else {
        return;
    };
    let attempt = get_attempt(state, attempt_id).await.ok();
    if attempt
        .as_ref()
        .is_some_and(|attempt| attempt.operation == "rollback")
    {
        notify_instance(state, instance_id).await;
    } else if let Ok(Some(offer)) = active_upgrade_offer(state, &instance, Some(attempt_id)).await {
        let Some(handle) = state.agents.read().await.get(instance_id).cloned() else {
            return;
        };
        let _ = handle.tx.send(outbound_offer(offer));
    }
}

async fn notify_instances(state: &AppState, instance_ids: Vec<String>) {
    for instance_id in instance_ids {
        notify_instance(state, &instance_id).await;
    }
}

async fn get_attempt(state: &AppState, attempt_id: &str) -> AppResult<AgentUpdateAttemptRecord> {
    sqlx::query_as::<_, AgentUpdateAttemptRecord>(
        r#"
        SELECT id, release_id, artifact_id, instance_id, operation, parent_attempt_id,
               from_version, target_version,
               status, message, retry_count, created_at, updated_at, completed_at
        FROM agent_update_attempts WHERE id = $1
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

async fn remove_release_storage(state: &AppState, release_id: &str) -> AppResult<()> {
    let relative = FsPath::new(release_id);
    if relative.components().count() != 1 {
        return Err(AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Agent 版本存储路径无效",
        ));
    }
    let path = safe_storage_path(state, release_id)?;
    match fs::remove_dir_all(&path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            warn!(?error, path = %path.display(), "failed to remove agent release storage");
            Err(error.into())
        }
    }
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
    result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>,
    message: &'static str,
) -> AppResult<sqlx::postgres::PgQueryResult> {
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

    use sqlx::postgres::PgPoolOptions;

    use crate::{
        auth::{AuthCipher, SESSION_COOKIE, insert_session},
        config::Cli,
        db::{IsolatedTestDatabase, init_db},
    };

    struct TestResources {
        database: IsolatedTestDatabase,
        root: std::path::PathBuf,
    }

    impl TestResources {
        async fn cleanup(self) {
            self.database.cleanup().await;
            match fs::remove_dir_all(&self.root).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove temporary update directory: {error}"),
            }
        }
    }

    fn test_cli(database_url: String, root: &FsPath) -> Cli {
        Cli {
            bind: "127.0.0.1:0".parse::<SocketAddr>().expect("bind address"),
            database_url,
            database_password: None,
            admin_password: Some("test-password-value".to_string()),
            auth_secret_key: None,
            auth_key_file: root.join("auth-secret.key"),
            secure_cookies: false,
            trust_proxy_headers: false,
            trusted_proxy_cidrs: Vec::new(),
            allow_legacy_agent_ws_auth: false,
            reset_admin_auth: false,
            confirm_reset_admin_auth: None,
            upload_dir: root.join("uploads"),
            update_dir: root.join("updates"),
            update_signing_key_file: None,
            update_signing_key_id: "default".to_string(),
            agent_package_max_bytes: 1024 * 1024,
            file_transfer_max_bytes: 1024 * 1024,
        }
    }

    async fn test_state() -> (AppState, TestResources) {
        let database = IsolatedTestDatabase::connect("om_update_test", 4).await;
        let database_url = database.database_url().to_string();
        let db = database.pool();
        init_db(&db).await.expect("initialize database");
        let root = std::env::temp_dir().join(format!("om-backend-update-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&root)
            .await
            .expect("create temporary update directory");
        let state = AppState::new(
            db,
            test_cli(database_url, &root),
            AuthCipher::from_key(&[7_u8; 32]).expect("create test auth cipher"),
            None,
        );
        let resources = TestResources { database, root };
        (state, resources)
    }

    async fn admin_headers(state: &AppState) -> HeaderMap {
        let user_id = Uuid::new_v4().to_string();
        let device_id = Uuid::new_v4().to_string();
        let token = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO admin_users(id, username, username_normalized, created_at) VALUES($1, 'test-admin', 'test-admin', 1)",
        )
        .bind(&user_id)
        .execute(&state.db)
        .await
        .expect("insert test administrator");
        sqlx::query(
            "INSERT INTO authenticator_devices(id, user_id, name, secret_ciphertext, created_at) VALUES($1, $2, 'test-device', 'test-secret', 1)",
        )
        .bind(&device_id)
        .bind(&user_id)
        .execute(&state.db)
        .await
        .expect("insert test authenticator");
        insert_session(
            state,
            token.clone(),
            user_id,
            "test-admin".to_string(),
            device_id,
            None,
        )
        .await;

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("{SESSION_COOKIE}={token}")
                .parse()
                .expect("valid test session cookie"),
        );
        headers
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
            ) VALUES($1, 'secret', $2, $3, $4, 'x86_64', '1.0.0', $5, $6, 1, 1)
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
            "INSERT INTO agent_releases(id, version, status, rollout_state, created_at, published_at) VALUES($1, $2, 'published', 'full_active', 1, 1)",
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
                sha256, storage_path, created_at, status, published_at
            ) VALUES($1, $2, 'linux', $3, $4, 'agent.bin', 8, 'digest', 'stored.bin', 1,
                     'published', 1)
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

    async fn insert_draft_release(
        state: &AppState,
        version: &str,
        native_arch: &str,
    ) -> (String, String) {
        let release_id = format!("release-{version}");
        let artifact_id = format!("artifact-{version}-{native_arch}");
        sqlx::query(
            "INSERT INTO agent_releases(id, version, status, rollout_state, created_at) VALUES($1, $2, 'draft', 'draft', 1)",
        )
        .bind(&release_id)
        .bind(version)
        .execute(&state.db)
        .await
        .expect("insert draft release");
        sqlx::query(
            r#"
            INSERT INTO agent_artifacts(
                id, release_id, os, package_type, native_arch, file_name, size_bytes,
                sha256, storage_path, created_at, status
            ) VALUES($1, $2, 'linux', 'standalone', $3, 'agent.bin', 8,
                     'digest', 'stored.bin', 1, 'draft')
            "#,
        )
        .bind(&artifact_id)
        .bind(&release_id)
        .bind(native_arch)
        .execute(&state.db)
        .await
        .expect("insert draft artifact");
        (release_id, artifact_id)
    }

    async fn set_instance_version_and_rollback(
        state: &AppState,
        instance_id: &str,
        version: &str,
        rollback_supported: bool,
        rollback_version: &str,
    ) {
        sqlx::query(
            "UPDATE instances SET agent_version = $1, rollback_supported = $2, rollback_version = $3 WHERE id = $4",
        )
        .bind(version)
        .bind(i64::from(rollback_supported))
        .bind(rollback_version)
        .bind(instance_id)
        .execute(&state.db)
        .await
        .expect("set instance version and rollback capability");
    }

    async fn insert_upgrade_attempt(
        state: &AppState,
        release_id: &str,
        artifact_id: &str,
        instance_id: &str,
        from_version: &str,
        target_version: &str,
        status: &str,
    ) -> String {
        let attempt_id = Uuid::new_v4().to_string();
        let completed_at = TERMINAL_ATTEMPT_STATUSES.contains(&status).then_some(1_i64);
        sqlx::query(
            r#"
            INSERT INTO agent_update_attempts(
                id, release_id, artifact_id, instance_id, operation, from_version,
                target_version, status, created_at, updated_at, completed_at
            ) VALUES($1, $2, $3, $4, 'upgrade', $5, $6, $7, 1, 1, $8)
            "#,
        )
        .bind(&attempt_id)
        .bind(release_id)
        .bind(artifact_id)
        .bind(instance_id)
        .bind(from_version)
        .bind(target_version)
        .bind(status)
        .bind(completed_at)
        .execute(&state.db)
        .await
        .expect("insert upgrade attempt");
        attempt_id
    }

    struct StoredReleaseFixture {
        release_id: String,
        version: String,
        artifact_id: String,
        instance_id: String,
        release_dir: std::path::PathBuf,
    }

    async fn insert_stored_release(
        state: &AppState,
        release_status: &str,
        attempt_status: Option<&str>,
    ) -> StoredReleaseFixture {
        let release_id = Uuid::new_v4().to_string();
        let artifact_id = Uuid::new_v4().to_string();
        let instance_id = Uuid::new_v4().to_string();
        let version_seed = Uuid::new_v4().as_u128();
        let version = format!(
            "{}.{}.{}",
            version_seed >> 64,
            (version_seed >> 32) & u32::MAX as u128,
            version_seed & u32::MAX as u128
        );
        let published_at = (release_status == "published").then_some(1_i64);
        sqlx::query(
            "INSERT INTO agent_releases(id, version, status, rollout_state, created_at, published_at) VALUES($1, $2, $3, CASE WHEN $3 = 'published' THEN 'full_active' ELSE 'draft' END, 1, $4)",
        )
        .bind(&release_id)
        .bind(&version)
        .bind(release_status)
        .bind(published_at)
        .execute(&state.db)
        .await
        .expect("insert release for deletion");

        insert_instance(state, &instance_id, "linux", "standalone", "x86_64").await;
        let storage_path = format!("{release_id}/{artifact_id}.bin");
        sqlx::query(
            r#"
            INSERT INTO agent_artifacts(
                id, release_id, os, package_type, native_arch, file_name, size_bytes,
                sha256, storage_path, created_at, status, published_at
            ) VALUES($1, $2, 'linux', 'standalone', 'x86_64', 'agent.bin', 7,
                     'digest', $3, 1, $4, $5)
            "#,
        )
        .bind(&artifact_id)
        .bind(&release_id)
        .bind(&storage_path)
        .bind(release_status)
        .bind(published_at)
        .execute(&state.db)
        .await
        .expect("insert artifact for deletion");

        if let Some(attempt_status) = attempt_status {
            let completed_at = TERMINAL_ATTEMPT_STATUSES
                .contains(&attempt_status)
                .then_some(1_i64);
            sqlx::query(
                r#"
                INSERT INTO agent_update_attempts(
                    id, release_id, artifact_id, instance_id, from_version, target_version,
                    status, created_at, updated_at, completed_at
                ) VALUES($1, $2, $3, $4, '1.0.0', $5, $6, 1, 1, $7)
                "#,
            )
            .bind(Uuid::new_v4().to_string())
            .bind(&release_id)
            .bind(&artifact_id)
            .bind(&instance_id)
            .bind(&version)
            .bind(attempt_status)
            .bind(completed_at)
            .execute(&state.db)
            .await
            .expect("insert update attempt for deletion");
        }

        let release_dir = state.update_dir.join(&release_id);
        fs::create_dir_all(&release_dir)
            .await
            .expect("create release storage");
        fs::write(state.update_dir.join(&storage_path), b"program")
            .await
            .expect("write stored program");
        fs::write(
            state.update_dir.join(format!("{storage_path}.sha256")),
            b"digest  agent.bin\n",
        )
        .await
        .expect("write stored checksum");

        StoredReleaseFixture {
            release_id,
            version,
            artifact_id,
            instance_id,
            release_dir,
        }
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
    fn outbound_update_offer_preserves_instance_binding() {
        let outbound = outbound_offer(AgentUpdateOffer {
            attempt_id: Some("attempt-1".to_string()),
            instance_id: Some("instance-1".to_string()),
            release_id: "release-1".to_string(),
            version: "1.2.3".to_string(),
            artifact_id: "artifact-1".to_string(),
            download_url: "/api/agent/update/artifacts/artifact-1/download".to_string(),
            sha256: "a".repeat(64),
            size_bytes: 42,
            package_type: "standalone".to_string(),
            native_arch: "x86_64".to_string(),
            target_os: Some("linux".to_string()),
            signature_key_id: Some("release-v1".to_string()),
            signature: Some("legacy-signature".to_string()),
            signature_v2: Some("v2-signature".to_string()),
            retry_count: 0,
        });
        let AgentOutbound::UpdateAvailable {
            attempt_id,
            instance_id,
            ..
        } = outbound
        else {
            panic!("expected update_available message");
        };
        assert_eq!(attempt_id.as_deref(), Some("attempt-1"));
        assert_eq!(instance_id.as_deref(), Some("instance-1"));
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
    fn update_attempt_statuses_only_move_forward_or_to_a_terminal_state() {
        let progress = [
            "pending",
            "waiting",
            "downloading",
            "verifying",
            "waiting_idle",
            "installing",
            "awaiting_restart",
        ];
        for (index, current) in progress.iter().enumerate() {
            assert!(update_status_transition_allowed(current, current));
            for next in progress.iter().skip(index) {
                assert!(update_status_transition_allowed(current, next));
            }
            for previous in progress.iter().take(index) {
                assert!(!update_status_transition_allowed(current, previous));
            }
            for terminal in TERMINAL_ATTEMPT_STATUSES {
                assert!(update_status_transition_allowed(current, terminal));
            }
        }

        for terminal in TERMINAL_ATTEMPT_STATUSES {
            assert!(update_status_transition_allowed(terminal, terminal));
            assert!(!update_status_transition_allowed(terminal, "waiting"));
            for other in TERMINAL_ATTEMPT_STATUSES {
                if terminal != other {
                    assert!(!update_status_transition_allowed(terminal, other));
                }
            }
        }
        assert!(!update_status_transition_allowed("unknown", "waiting"));
        assert!(!update_status_transition_allowed("waiting", "unknown"));
    }

    #[test]
    fn records_an_unprivileged_release_target_as_failed() {
        let instance = InstanceCapabilityRow {
            id: "unprivileged-agent".to_string(),
            name: "Unprivileged Agent".to_string(),
            hostname: "unprivileged-agent".to_string(),
            os: "linux".to_string(),
            agent_version: "1.0.0".to_string(),
            package_type: "standalone".to_string(),
            native_arch: "x86_64".to_string(),
            update_privileged: 0,
            rollback_supported: 0,
            rollback_version: String::new(),
        };

        assert_eq!(
            publish_attempt_state(&instance, 123),
            (
                "failed",
                "Agent 进程没有替换当前可执行文件所需的权限",
                Some(123),
            )
        );
    }

    #[test]
    fn full_rollout_keeps_instance_level_exclusions_until_explicit_reupgrade() {
        let included = HashSet::from(["canary-instance".to_string()]);
        let excluded = HashSet::from(["excluded-instance".to_string()]);

        assert!(rollout_selects_instance(
            "canary_active",
            "canary-instance",
            &included,
            &excluded,
        ));
        assert!(!rollout_selects_instance(
            "canary_active",
            "other-instance",
            &included,
            &excluded,
        ));
        assert!(rollout_selects_instance(
            "full_active",
            "other-instance",
            &included,
            &excluded,
        ));
        assert!(!rollout_selects_instance(
            "full_active",
            "excluded-instance",
            &included,
            &excluded,
        ));
        assert!(!rollout_selects_instance(
            "full_paused",
            "excluded-instance",
            &included,
            &excluded,
        ));
    }

    fn attempt(
        id: &str,
        parent_attempt_id: Option<&str>,
        status: &str,
        updated_at: i64,
    ) -> AgentUpdateAttemptRecord {
        AgentUpdateAttemptRecord {
            id: id.to_string(),
            release_id: "release-1".to_string(),
            artifact_id: None,
            instance_id: "instance-1".to_string(),
            operation: "rollback".to_string(),
            parent_attempt_id: parent_attempt_id.map(str::to_string),
            from_version: "2.0.0".to_string(),
            target_version: "1.0.0".to_string(),
            status: status.to_string(),
            message: String::new(),
            retry_count: 0,
            created_at: updated_at,
            updated_at,
            completed_at: Some(updated_at),
        }
    }

    #[test]
    fn rollback_summary_uses_the_latest_outcome_per_parent_upgrade() {
        let attempts = vec![
            attempt("old-failure", Some("upgrade-1"), "failed", 10),
            attempt("new-success", Some("upgrade-1"), "rollback_succeeded", 20),
            attempt("other-failure", Some("upgrade-2"), "failed", 30),
        ];

        let outcomes = latest_rollback_outcomes(&attempts);
        assert_eq!(
            outcomes.get("upgrade-1").map(String::as_str),
            Some("rollback_succeeded")
        );
        assert_eq!(
            outcomes.get("upgrade-2").map(String::as_str),
            Some("failed")
        );
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
    async fn storage_setup_failure_removes_the_temporary_artifact() {
        let root = std::env::temp_dir().join(format!("om-artifact-cleanup-{}", Uuid::new_v4()));
        fs::create_dir_all(&root)
            .await
            .expect("create artifact cleanup directory");
        let update_dir = root.join("updates");
        fs::write(&update_dir, b"not a directory")
            .await
            .expect("block update directory creation");
        let temporary = root.join("pending.upload");
        fs::write(&temporary, b"\x7fELFpayload")
            .await
            .expect("write temporary artifact");
        let db = PgPoolOptions::new()
            .connect_lazy("postgresql://localhost/unused")
            .expect("create lazy test pool");
        let state = AppState::new(
            db,
            test_cli("postgresql://localhost/unused".to_string(), &root),
            AuthCipher::from_key(&[7_u8; 32]).expect("create test auth cipher"),
            None,
        );
        let checksum = "0".repeat(64);
        let received = ReceivedArtifact {
            os: "linux".to_string(),
            package_type: "standalone".to_string(),
            native_arch: "x86_64".to_string(),
            file_name: "agent.bin".to_string(),
            size_bytes: 11,
            sha256: checksum.clone(),
            checksum_file_name: "agent.bin.sha256".to_string(),
            checksum_contents: format!("{checksum}  agent.bin\n"),
            first_bytes: b"\x7fELFpayload".to_vec(),
            temp_path: temporary.clone(),
        };

        let result = store_artifact(&state, "release-test", received).await;
        assert!(result.is_err(), "invalid update storage must fail");
        assert!(!temporary.exists());
        fs::remove_dir_all(&root)
            .await
            .expect("remove artifact cleanup directory");
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn admin_can_download_a_draft_artifact_with_its_original_file_name() {
        let (state, resources) = test_state().await;
        let fixture = insert_stored_release(&state, "draft", None).await;
        let headers = admin_headers(&state).await;

        let response = admin_download_agent_artifact(
            State(state.clone()),
            headers,
            Path((fixture.release_id.clone(), fixture.artifact_id.clone())),
        )
        .await
        .expect("download draft artifact");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_DISPOSITION)
                .and_then(|value| value.to_str().ok()),
            Some("attachment; filename=\"agent.bin\"; filename*=UTF-8''agent.bin")
        );
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("read artifact response body");
        assert_eq!(body.as_ref(), b"program");

        let audit_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'download_agent_artifact' AND target = $1",
        )
        .bind(&fixture.artifact_id)
        .fetch_one(&state.db)
        .await
        .expect("load download audit event");
        assert_eq!(audit_count, 1);
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn canary_pause_resume_and_full_rollout_cover_only_the_expected_instances() {
        let (state, resources) = test_state().await;
        let headers = admin_headers(&state).await;
        for instance_id in ["canary-agent", "batch-agent", "full-agent"] {
            insert_instance(&state, instance_id, "ubuntu", "standalone", "amd64").await;
        }
        let (release_id, _) = insert_draft_release(&state, "2.0.0", "amd64").await;

        let initial = admin_publish_agent_release(
            State(state.clone()),
            headers.clone(),
            Path(release_id.clone()),
            Some(Json(PublishAgentReleaseRequest {
                instance_ids: vec!["canary-agent".to_string()],
            })),
        )
        .await
        .expect("publish initial canary")
        .0;
        assert_eq!(initial.release.rollout_state, "canary_active");
        assert_eq!(initial.coverage.selected_instances, 1);
        let initial_targets: Vec<String> = sqlx::query_scalar(
            "SELECT instance_id FROM agent_update_attempts WHERE release_id = $1 ORDER BY instance_id",
        )
        .bind(&release_id)
        .fetch_all(&state.db)
        .await
        .expect("load initial canary attempts");
        assert_eq!(initial_targets, ["canary-agent"]);
        let canary = get_instance(&state.db, "canary-agent")
            .await
            .expect("load initial canary instance");
        let handed_off = find_update_for_instance(&state, &canary)
            .await
            .expect("hand off initial canary")
            .expect("initial canary offer");
        assert_eq!(handed_off.version, "2.0.0");
        assert_eq!(handed_off.instance_id.as_deref(), Some("canary-agent"));

        let _ = set_rollout_paused(&state, &headers, &release_id, true)
            .await
            .expect("pause canary");
        assert!(
            find_update_for_instance(&state, &canary)
                .await
                .expect("resume an already handed-off task while paused")
                .is_some(),
            "pause must not interrupt a task that already reached waiting"
        );
        let _ = admin_add_agent_rollout_targets(
            State(state.clone()),
            headers.clone(),
            Path(release_id.clone()),
            Json(AgentReleaseTargetsRequest {
                instance_ids: vec!["batch-agent".to_string()],
            }),
        )
        .await
        .expect("add a paused canary batch");
        let paused_attempt: String = sqlx::query_scalar(
            "SELECT status FROM agent_update_attempts WHERE release_id = $1 AND instance_id = 'batch-agent'",
        )
        .bind(&release_id)
        .fetch_one(&state.db)
        .await
        .expect("load paused batch attempt");
        assert_eq!(paused_attempt, "pending");
        let batch = get_instance(&state.db, "batch-agent")
            .await
            .expect("load paused batch instance");
        assert!(
            find_update_for_instance(&state, &batch)
                .await
                .expect("check paused offer")
                .is_none()
        );

        let _ = set_rollout_paused(&state, &headers, &release_id, false)
            .await
            .expect("resume canary");
        let resumed = find_update_for_instance(&state, &batch)
            .await
            .expect("check resumed offer")
            .expect("resumed batch offer");
        assert_eq!(resumed.version, "2.0.0");

        let promoted = admin_promote_agent_rollout(
            State(state.clone()),
            headers.clone(),
            Path(release_id.clone()),
        )
        .await
        .expect("promote canary to full rollout")
        .0;
        assert_eq!(promoted.release.rollout_state, "full_active");
        let full_targets: Vec<String> = sqlx::query_scalar(
            "SELECT instance_id FROM agent_update_attempts WHERE release_id = $1 ORDER BY instance_id",
        )
        .bind(&release_id)
        .fetch_all(&state.db)
        .await
        .expect("load full rollout attempts");
        assert_eq!(full_targets, ["batch-agent", "canary-agent", "full-agent"]);

        insert_instance(&state, "new-agent", "ubuntu", "standalone", "amd64").await;
        let new_instance = get_instance(&state.db, "new-agent")
            .await
            .expect("load newly registered instance");
        let new_offer = find_update_for_instance(&state, &new_instance)
            .await
            .expect("select full rollout for new instance")
            .expect("new instance receives full rollout");
        assert_eq!(new_offer.release_id, release_id);
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn batch_rollback_waits_for_an_in_progress_upgrade_then_queues_its_rollback() {
        let (state, resources) = test_state().await;
        let headers = admin_headers(&state).await;
        insert_instance(&state, "in-progress-agent", "ubuntu", "standalone", "amd64").await;
        insert_release(&state, "1.0.0", "standalone", "amd64").await;
        insert_release(&state, "2.0.0", "standalone", "amd64").await;
        set_instance_version_and_rollback(&state, "in-progress-agent", "1.0.0", true, "1.0.0")
            .await;
        let instance = get_instance(&state.db, "in-progress-agent")
            .await
            .expect("load in-progress instance");
        let offer = find_update_for_instance(&state, &instance)
            .await
            .expect("create active upgrade")
            .expect("active upgrade offer");
        sqlx::query("UPDATE agent_update_attempts SET status = 'downloading' WHERE id = $1")
            .bind(offer.attempt_id.as_deref().expect("attempt id"))
            .execute(&state.db)
            .await
            .expect("mark upgrade as downloading");

        let rollback = admin_rollback_agent_release(
            State(state.clone()),
            headers,
            Path("release-2.0.0".to_string()),
        )
        .await
        .expect("start batch rollback")
        .0;
        assert_eq!(rollback.release.rollout_state, "rollback_active");
        let still_downloading: String =
            sqlx::query_scalar("SELECT status FROM agent_update_attempts WHERE id = $1")
                .bind(offer.attempt_id.as_deref().expect("attempt id"))
                .fetch_one(&state.db)
                .await
                .expect("load in-progress upgrade status");
        assert_eq!(still_downloading, "downloading");

        record_update_status(
            &state,
            &instance.id,
            &offer.release_id,
            &offer.artifact_id,
            &offer.version,
            offer.retry_count,
            "succeeded",
            None,
        )
        .await
        .expect("finish the in-progress upgrade");
        let queued: (String, String, String) = sqlx::query_as(
            "SELECT operation, status, target_version FROM agent_update_attempts WHERE parent_attempt_id = $1",
        )
        .bind(offer.attempt_id.as_deref().expect("attempt id"))
        .fetch_one(&state.db)
        .await
        .expect("load automatically queued rollback");
        assert_eq!(
            queued,
            (
                "rollback".to_string(),
                "pending".to_string(),
                "1.0.0".to_string(),
            )
        );
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn controlled_releases_and_active_instance_tasks_cannot_compete() {
        let (state, resources) = test_state().await;
        insert_instance(&state, "contended-agent", "ubuntu", "standalone", "amd64").await;
        insert_release(&state, "2.0.0", "standalone", "amd64").await;
        let instance = get_instance(&state.db, "contended-agent")
            .await
            .expect("load contended instance");
        let offer = find_update_for_instance(&state, &instance)
            .await
            .expect("create first release attempt")
            .expect("first release offer");
        insert_release(&state, "3.0.0", "standalone", "amd64").await;
        let pinned = find_update_for_instance(&state, &instance)
            .await
            .expect("load active offer")
            .expect("active task remains available");
        assert_eq!(pinned.release_id, offer.release_id);
        let active_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_update_attempts WHERE instance_id = $1 AND status NOT IN ('succeeded', 'rollback_succeeded', 'failed', 'cancelled')",
        )
        .bind(&instance.id)
        .fetch_one(&state.db)
        .await
        .expect("count active tasks");
        assert_eq!(active_count, 1);

        sqlx::query(
            "UPDATE agent_releases SET rollout_state = 'canary_active' WHERE id = 'release-3.0.0'",
        )
        .execute(&state.db)
        .await
        .expect("mark newer release as controlled");
        let mut transaction = state.db.begin().await.expect("start control transaction");
        let error = ensure_no_other_controlled_release(&mut transaction, "release-2.0.0")
            .await
            .expect_err("another controlled release must be rejected");
        assert_eq!(error.status, StatusCode::CONFLICT);
        transaction
            .rollback()
            .await
            .expect("rollback control check");

        sqlx::query(
            "UPDATE agent_releases SET rollout_state = 'full_active' WHERE id = 'release-3.0.0'",
        )
        .execute(&state.db)
        .await
        .expect("finish newer release control flow");
        let mut transaction = state
            .db
            .begin()
            .await
            .expect("start stable baseline transaction");
        ensure_no_other_controlled_release(&mut transaction, "release-2.0.0")
            .await
            .expect("stable full releases may coexist with one controlled release");
        transaction
            .rollback()
            .await
            .expect("rollback stable baseline check");
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn batch_rollback_uses_local_baselines_and_finishes_partial_when_some_are_unavailable() {
        let (state, resources) = test_state().await;
        for instance_id in ["local-baseline-agent", "unsupported-agent"] {
            insert_instance(&state, instance_id, "ubuntu", "standalone", "amd64").await;
        }
        insert_release(&state, "2.0.0", "standalone", "amd64").await;
        set_instance_version_and_rollback(&state, "local-baseline-agent", "2.0.0", true, "1.0.0")
            .await;
        set_instance_version_and_rollback(&state, "unsupported-agent", "2.0.0", false, "").await;
        let local_parent = insert_upgrade_attempt(
            &state,
            "release-2.0.0",
            "artifact-2.0.0-amd64",
            "local-baseline-agent",
            "1.0.0",
            "2.0.0",
            "succeeded",
        )
        .await;
        let unsupported_parent = insert_upgrade_attempt(
            &state,
            "release-2.0.0",
            "artifact-2.0.0-amd64",
            "unsupported-agent",
            "1.0.0",
            "2.0.0",
            "succeeded",
        )
        .await;
        sqlx::query("UPDATE agent_releases SET rollout_state = 'rollback_active' WHERE id = 'release-2.0.0'")
            .execute(&state.db)
            .await
            .expect("activate batch rollback");

        reconcile_release_rollback(&state, "release-2.0.0")
            .await
            .expect("queue batch rollback");
        let local: (String, Option<String>, String) = sqlx::query_as(
            "SELECT status, artifact_id, target_version FROM agent_update_attempts WHERE parent_attempt_id = $1",
        )
        .bind(&local_parent)
        .fetch_one(&state.db)
        .await
        .expect("load local baseline rollback");
        assert_eq!(local, ("pending".to_string(), None, "1.0.0".to_string()));
        let unsupported: (String, String) = sqlx::query_as(
            "SELECT status, message FROM agent_update_attempts WHERE parent_attempt_id = $1",
        )
        .bind(&unsupported_parent)
        .fetch_one(&state.db)
        .await
        .expect("load unsupported rollback");
        assert_eq!(unsupported.0, "failed");
        assert!(unsupported.1.contains("不支持主动回滚"));

        let local_attempt_id: String =
            sqlx::query_scalar("SELECT id FROM agent_update_attempts WHERE parent_attempt_id = $1")
                .bind(&local_parent)
                .fetch_one(&state.db)
                .await
                .expect("load local rollback attempt id");
        sqlx::query(
            "UPDATE agent_update_attempts SET status = 'rollback_succeeded', completed_at = 2 WHERE id = $1",
        )
        .bind(local_attempt_id)
        .execute(&state.db)
        .await
        .expect("complete local rollback");
        reconcile_release_rollback(&state, "release-2.0.0")
            .await
            .expect("finish batch rollback");
        let rollout_state: String = sqlx::query_scalar(
            "SELECT rollout_state FROM agent_releases WHERE id = 'release-2.0.0'",
        )
        .fetch_one(&state.db)
        .await
        .expect("load partial rollback state");
        assert_eq!(rollout_state, "rollback_partial");
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn instance_rollback_excludes_then_reupgrade_creates_a_new_upgrade_history() {
        let (state, resources) = test_state().await;
        let headers = admin_headers(&state).await;
        insert_instance(&state, "reupgrade-agent", "ubuntu", "standalone", "amd64").await;
        insert_release(&state, "1.0.0", "standalone", "amd64").await;
        insert_release(&state, "2.0.0", "standalone", "amd64").await;
        set_instance_version_and_rollback(&state, "reupgrade-agent", "2.0.0", true, "1.0.0").await;
        insert_upgrade_attempt(
            &state,
            "release-2.0.0",
            "artifact-2.0.0-amd64",
            "reupgrade-agent",
            "1.0.0",
            "2.0.0",
            "succeeded",
        )
        .await;

        let _ = admin_rollback_agent_instance(
            State(state.clone()),
            headers.clone(),
            Path(("release-2.0.0".to_string(), "reupgrade-agent".to_string())),
        )
        .await
        .expect("start instance rollback");
        let target_state: String = sqlx::query_scalar(
            "SELECT state FROM agent_release_targets WHERE release_id = 'release-2.0.0' AND instance_id = 'reupgrade-agent'",
        )
        .fetch_one(&state.db)
        .await
        .expect("load instance exclusion");
        assert_eq!(target_state, "excluded");
        let rollback_attempt_id: String = sqlx::query_scalar(
            "SELECT id FROM agent_update_attempts WHERE release_id = 'release-2.0.0' AND instance_id = 'reupgrade-agent' AND operation = 'rollback'",
        )
        .fetch_one(&state.db)
        .await
        .expect("load instance rollback attempt");
        sqlx::query(
            "UPDATE agent_update_attempts SET status = 'rollback_succeeded', completed_at = 2 WHERE id = $1",
        )
        .bind(&rollback_attempt_id)
        .execute(&state.db)
        .await
        .expect("complete instance rollback");
        set_instance_version_and_rollback(&state, "reupgrade-agent", "1.0.0", true, "2.0.0").await;

        let _ = admin_reupgrade_agent_instance(
            State(state.clone()),
            headers,
            Path(("release-2.0.0".to_string(), "reupgrade-agent".to_string())),
        )
        .await
        .expect("explicitly reupgrade rolled back instance");
        let included: String = sqlx::query_scalar(
            "SELECT state FROM agent_release_targets WHERE release_id = 'release-2.0.0' AND instance_id = 'reupgrade-agent'",
        )
        .fetch_one(&state.db)
        .await
        .expect("load rejoined target");
        assert_eq!(included, "included");
        let upgrades: Vec<String> = sqlx::query_scalar(
            "SELECT status FROM agent_update_attempts WHERE release_id = 'release-2.0.0' AND instance_id = 'reupgrade-agent' AND operation = 'upgrade' ORDER BY created_at, id",
        )
        .fetch_all(&state.db)
        .await
        .expect("load upgrade history");
        assert_eq!(upgrades.len(), 2);
        assert!(upgrades.contains(&"succeeded".to_string()));
        assert!(upgrades.contains(&"pending".to_string()));
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn release_deletion_is_blocked_by_a_local_baseline_target_version_dependency() {
        let (state, resources) = test_state().await;
        insert_instance(&state, "baseline-agent", "ubuntu", "standalone", "amd64").await;
        insert_release(&state, "1.0.0", "standalone", "amd64").await;
        insert_release(&state, "2.0.0", "standalone", "amd64").await;
        set_instance_version_and_rollback(&state, "baseline-agent", "2.0.0", true, "1.0.0").await;
        let parent = insert_upgrade_attempt(
            &state,
            "release-2.0.0",
            "artifact-2.0.0-amd64",
            "baseline-agent",
            "1.0.0",
            "2.0.0",
            "succeeded",
        )
        .await;
        sqlx::query(
            "INSERT INTO agent_update_attempts(id, release_id, artifact_id, instance_id, operation, parent_attempt_id, from_version, target_version, status, created_at, updated_at) VALUES($1, 'release-2.0.0', NULL, 'baseline-agent', 'rollback', $2, '2.0.0', '1.0.0', 'pending', 2, 2)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(parent)
        .execute(&state.db)
        .await
        .expect("insert local baseline rollback dependency");

        let error = delete_agent_release(&state, "test-admin", "test-admin-id", "release-1.0.0")
            .await
            .expect_err("target-version rollback dependency blocks deletion");
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert!(error.message.contains("回滚记录依赖"));
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn instances_running_a_published_version_do_not_block_its_deletion() {
        let (state, resources) = test_state().await;
        insert_instance(&state, "running-agent", "linux", "standalone", "x86_64").await;
        insert_release(&state, "1.0.0", "standalone", "x86_64").await;
        set_instance_version_and_rollback(&state, "running-agent", "1.0.0", true, "0.9.0").await;

        delete_agent_release(&state, "test-admin", "test-admin-id", "release-1.0.0")
            .await
            .expect("a version that instances merely run must stay deletable");
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_releases WHERE id = 'release-1.0.0'")
                .fetch_one(&state.db)
                .await
                .expect("count deleted release");
        assert_eq!(remaining, 0);
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn deletion_is_blocked_when_a_published_version_is_the_only_rollback_path() {
        let (state, resources) = test_state().await;
        insert_instance(&state, "upgraded-agent", "linux", "standalone", "x86_64").await;
        insert_release(&state, "1.0.0", "standalone", "x86_64").await;
        insert_release(&state, "2.0.0", "standalone", "x86_64").await;
        // No local baseline for 1.0.0, so only the published artifact can bring the agent back.
        set_instance_version_and_rollback(&state, "upgraded-agent", "2.0.0", true, "").await;
        insert_upgrade_attempt(
            &state,
            "release-2.0.0",
            "artifact-2.0.0-x86_64",
            "upgraded-agent",
            "1.0.0",
            "2.0.0",
            "succeeded",
        )
        .await;

        let error = delete_agent_release(&state, "test-admin", "test-admin-id", "release-1.0.0")
            .await
            .expect_err("removing the only rollback path must be refused");
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert!(error.message.contains("1 个实例只能回滚到该版本"));
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn a_local_rollback_baseline_keeps_the_superseded_version_deletable() {
        let (state, resources) = test_state().await;
        insert_instance(&state, "baselined-agent", "linux", "standalone", "x86_64").await;
        insert_release(&state, "1.0.0", "standalone", "x86_64").await;
        insert_release(&state, "2.0.0", "standalone", "x86_64").await;
        set_instance_version_and_rollback(&state, "baselined-agent", "2.0.0", true, "1.0.0").await;
        insert_upgrade_attempt(
            &state,
            "release-2.0.0",
            "artifact-2.0.0-x86_64",
            "baselined-agent",
            "1.0.0",
            "2.0.0",
            "succeeded",
        )
        .await;

        delete_agent_release(&state, "test-admin", "test-admin-id", "release-1.0.0")
            .await
            .expect("a local baseline makes the published artifact redundant");
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn migration_preserves_published_rollouts_and_upgrade_history() {
        let test_db = IsolatedTestDatabase::connect("om_update_migration", 4).await;
        let db = test_db.pool();
        init_db(&db)
            .await
            .expect("initialize legacy fixture base tables");
        sqlx::query("DROP TABLE agent_release_targets")
            .execute(&db)
            .await
            .expect("remove new rollout target table");
        sqlx::query("DROP TABLE agent_update_attempts")
            .execute(&db)
            .await
            .expect("remove new attempts table");
        sqlx::query("ALTER TABLE agent_releases DROP COLUMN rollout_updated_at")
            .execute(&db)
            .await
            .expect("remove rollout timestamp column");
        sqlx::query("ALTER TABLE agent_releases DROP COLUMN rollout_state")
            .execute(&db)
            .await
            .expect("remove rollout state column");
        sqlx::query(
            r#"
            CREATE TABLE agent_update_attempts (
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
            )
            "#,
        )
        .execute(&db)
        .await
        .expect("create legacy attempts table");
        sqlx::query(
            "INSERT INTO instances(id, secret, name, first_seen) VALUES('legacy-agent', 'secret', 'legacy-agent', 1)",
        )
        .execute(&db)
        .await
        .expect("insert legacy instance");
        sqlx::query(
            "INSERT INTO agent_releases(id, version, status, created_at, published_at) VALUES('legacy-release', '1.2.3', 'published', 1, 2)",
        )
        .execute(&db)
        .await
        .expect("insert legacy release");
        sqlx::query(
            r#"
            INSERT INTO agent_artifacts(
                id, release_id, os, package_type, native_arch, file_name, size_bytes,
                sha256, storage_path, created_at, status, published_at
            ) VALUES('legacy-artifact', 'legacy-release', 'linux', 'standalone', 'amd64',
                     'agent.bin', 8, 'digest', 'stored.bin', 1, 'published', 2)
            "#,
        )
        .execute(&db)
        .await
        .expect("insert legacy artifact");
        sqlx::query(
            "INSERT INTO agent_update_attempts(id, release_id, artifact_id, instance_id, from_version, target_version, status, created_at, updated_at, completed_at) VALUES('legacy-attempt', 'legacy-release', 'legacy-artifact', 'legacy-agent', '1.0.0', '1.2.3', 'succeeded', 1, 2, 2)",
        )
        .execute(&db)
        .await
        .expect("insert legacy attempt");

        init_db(&db).await.expect("migrate legacy update tables");
        let rollout_state: String = sqlx::query_scalar(
            "SELECT rollout_state FROM agent_releases WHERE id = 'legacy-release'",
        )
        .fetch_one(&db)
        .await
        .expect("load migrated rollout state");
        let operation: String = sqlx::query_scalar(
            "SELECT operation FROM agent_update_attempts WHERE id = 'legacy-attempt'",
        )
        .fetch_one(&db)
        .await
        .expect("load migrated attempt operation");
        assert_eq!(rollout_state, "full_active");
        assert_eq!(operation, "upgrade");
        sqlx::query(
            "INSERT INTO agent_update_attempts(id, release_id, artifact_id, instance_id, operation, from_version, target_version, status, created_at, updated_at, completed_at) VALUES('second-history', 'legacy-release', 'legacy-artifact', 'legacy-agent', 'upgrade', '1.0.0', '1.2.3', 'failed', 3, 3, 3)",
        )
        .execute(&db)
        .await
        .expect("legacy uniqueness constraint was removed");
        test_db.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn deletes_published_release_storage_history_and_keeps_audit_log() {
        let (state, resources) = test_state().await;
        let fixture = insert_stored_release(&state, "published", Some("succeeded")).await;

        delete_agent_release(&state, "test-admin", "test-admin-id", &fixture.release_id)
            .await
            .expect("delete published release");

        let release_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_releases WHERE id = $1")
                .bind(&fixture.release_id)
                .fetch_one(&state.db)
                .await
                .expect("count releases");
        let artifact_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_artifacts WHERE id = $1")
                .bind(&fixture.artifact_id)
                .fetch_one(&state.db)
                .await
                .expect("count artifacts");
        let attempt_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_update_attempts WHERE release_id = $1")
                .bind(&fixture.release_id)
                .fetch_one(&state.db)
                .await
                .expect("count attempts");
        assert_eq!((release_count, artifact_count, attempt_count), (0, 0, 0));
        assert!(!fixture.release_dir.exists());

        let detail: String = sqlx::query_scalar(
            "SELECT detail FROM audit_events WHERE action = 'delete_agent_release' AND target = $1 ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(&fixture.release_id)
        .fetch_one(&state.db)
        .await
        .expect("load deletion audit log");
        assert!(detail.contains(&fixture.version));
        assert!(detail.contains("1 个可执行文件"));
        assert!(detail.contains("1 条实例更新记录"));

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-agent-id",
            fixture.instance_id.parse().expect("agent id header"),
        );
        headers.insert(
            "x-agent-secret",
            "secret".parse().expect("agent secret header"),
        );
        let error = match authorized_artifact_download(&state, &headers, &fixture.artifact_id).await
        {
            Ok(_) => panic!("deleted artifact must not remain downloadable"),
            Err(error) => error,
        };
        assert_eq!(error.status, StatusCode::NOT_FOUND);
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn terminal_rollback_history_does_not_permanently_block_release_deletion() {
        let (state, resources) = test_state().await;
        let fixture = insert_stored_release(&state, "published", Some("succeeded")).await;
        let parent_attempt_id: String = sqlx::query_scalar(
            "SELECT id FROM agent_update_attempts WHERE release_id = $1 AND instance_id = $2",
        )
        .bind(&fixture.release_id)
        .bind(&fixture.instance_id)
        .fetch_one(&state.db)
        .await
        .expect("load parent upgrade attempt");
        sqlx::query(
            r#"
            INSERT INTO agent_update_attempts(
                id, release_id, artifact_id, instance_id, operation, parent_attempt_id,
                from_version, target_version, status, created_at, updated_at, completed_at
            ) VALUES($1, $2, NULL, $3, 'rollback', $4, $5, '1.0.0', 'failed', 2, 2, 2)
            "#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&fixture.release_id)
        .bind(&fixture.instance_id)
        .bind(parent_attempt_id)
        .bind(&fixture.version)
        .execute(&state.db)
        .await
        .expect("insert failed rollback history");

        delete_agent_release(&state, "test-admin", "test-admin-id", &fixture.release_id)
            .await
            .expect("terminal rollback history must not block release deletion");
        assert!(!fixture.release_dir.exists());
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn orphaned_attempt_from_a_deleted_instance_does_not_block_release_deletion() {
        let (state, resources) = test_state().await;
        let fixture = insert_stored_release(&state, "published", Some("pending")).await;
        sqlx::query(
            "ALTER TABLE agent_update_attempts DROP CONSTRAINT agent_update_attempts_instance_id_fkey",
        )
        .execute(&state.db)
        .await
        .expect("simulate a legacy database without instance cascade cleanup");
        sqlx::query("DELETE FROM instances WHERE id = $1")
            .bind(&fixture.instance_id)
            .execute(&state.db)
            .await
            .expect("delete instance while retaining legacy attempt");
        let orphaned_attempts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_update_attempts WHERE release_id = $1")
                .bind(&fixture.release_id)
                .fetch_one(&state.db)
                .await
                .expect("count orphaned attempts");
        assert_eq!(orphaned_attempts, 1);

        let release = get_release(&state, &fixture.release_id)
            .await
            .expect("load release with orphaned attempt");
        let detail = load_release_detail(&state, release)
            .await
            .expect("load release detail");
        assert!(detail.attempts.is_empty());
        delete_agent_release(&state, "test-admin", "test-admin-id", &fixture.release_id)
            .await
            .expect("orphaned instance attempt must not block release deletion");
        assert!(!fixture.release_dir.exists());
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn rejects_release_deletion_for_every_nonterminal_attempt_status() {
        let (state, resources) = test_state().await;
        for status in [
            "pending",
            "waiting",
            "downloading",
            "verifying",
            "waiting_idle",
            "installing",
            "awaiting_restart",
        ] {
            let fixture = insert_stored_release(&state, "published", Some(status)).await;
            let error =
                delete_agent_release(&state, "test-admin", "test-admin-id", &fixture.release_id)
                    .await
                    .expect_err("active update must block release deletion");
            assert_eq!(error.status, StatusCode::CONFLICT, "status {status}");
            assert!(error.message.contains("1 个实例更新未结束"));

            let release_count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM agent_releases WHERE id = $1")
                    .bind(&fixture.release_id)
                    .fetch_one(&state.db)
                    .await
                    .expect("count blocked release");
            let attempt_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM agent_update_attempts WHERE release_id = $1",
            )
            .bind(&fixture.release_id)
            .fetch_one(&state.db)
            .await
            .expect("count blocked attempt");
            assert_eq!((release_count, attempt_count), (1, 1));
            assert!(fixture.release_dir.exists());

            sqlx::query(
                "UPDATE agent_update_attempts SET status = 'failed', completed_at = 1 WHERE release_id = $1",
            )
            .bind(&fixture.release_id)
            .execute(&state.db)
            .await
            .expect("finish blocked attempt for cleanup");
            delete_agent_release(&state, "test-admin", "test-admin-id", &fixture.release_id)
                .await
                .expect("clean blocked release fixture");
        }
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn draft_deletion_tolerates_missing_storage_and_missing_release_is_not_found() {
        let (state, resources) = test_state().await;
        let fixture = insert_stored_release(&state, "draft", None).await;
        fs::remove_dir_all(&fixture.release_dir)
            .await
            .expect("remove fixture storage before deletion");

        delete_agent_release(&state, "test-admin", "test-admin-id", &fixture.release_id)
            .await
            .expect("delete draft with already missing storage");
        let error =
            delete_agent_release(&state, "test-admin", "test-admin-id", &fixture.release_id)
                .await
                .expect_err("deleted release must no longer exist");
        assert_eq!(error.status, StatusCode::NOT_FOUND);
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn storage_cleanup_failure_does_not_roll_back_release_deletion() {
        let (state, resources) = test_state().await;
        let fixture = insert_stored_release(&state, "draft", None).await;
        fs::remove_dir_all(&fixture.release_dir)
            .await
            .expect("remove release directory before failure injection");
        fs::write(&fixture.release_dir, b"not a directory")
            .await
            .expect("replace release directory with a file");

        delete_agent_release(&state, "test-admin", "test-admin-id", &fixture.release_id)
            .await
            .expect("database deletion must survive storage cleanup failure");

        let release_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_releases WHERE id = $1")
                .bind(&fixture.release_id)
                .fetch_one(&state.db)
                .await
                .expect("count deleted releases");
        let artifact_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_artifacts WHERE release_id = $1")
                .bind(&fixture.release_id)
                .fetch_one(&state.db)
                .await
                .expect("count deleted artifacts");
        let audit_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'delete_agent_release' AND target = $1",
        )
        .bind(&fixture.release_id)
        .fetch_one(&state.db)
        .await
        .expect("count committed audit records");

        assert_eq!((release_count, artifact_count, audit_count), (0, 0, 1));
        assert!(fixture.release_dir.is_file());
        fs::remove_file(&fixture.release_dir)
            .await
            .expect("clean failure injection file");
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn deleting_latest_release_leaves_the_next_published_candidate_available() {
        let (state, resources) = test_state().await;
        let instance_id = Uuid::new_v4().to_string();
        insert_instance(&state, &instance_id, "linux", "standalone", "amd64").await;
        let version_seed = (Uuid::new_v4().as_u128() % 1_000_000_000) as u64 + 10_000;
        let fallback_version = format!("{version_seed}.1.0");
        let deleted_version = format!("{version_seed}.2.0");
        insert_release(&state, &fallback_version, "standalone", "amd64").await;
        insert_release(&state, &deleted_version, "standalone", "amd64").await;

        delete_agent_release(
            &state,
            "test-admin",
            "test-admin-id",
            &format!("release-{deleted_version}"),
        )
        .await
        .expect("delete latest release");
        let instance = get_instance(&state.db, &instance_id)
            .await
            .expect("load candidate instance");
        let offer = find_update_for_instance(&state, &instance)
            .await
            .expect("find fallback update")
            .expect("fallback release remains available");
        assert_eq!(offer.version, fallback_version);
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn selects_highest_matching_release_and_suppresses_failed_attempt() {
        let (state, resources) = test_state().await;
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
            "UPDATE agent_update_attempts SET status = 'pending', completed_at = NULL WHERE instance_id = $1 AND release_id = $2",
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
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn requires_an_exact_native_architecture_match() {
        let (state, resources) = test_state().await;
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
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn reconciles_pre_handoff_attempt_after_offline_capability_change() {
        let (state, resources) = test_state().await;
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
                sha256, storage_path, created_at, status, published_at
            ) VALUES('artifact-2.1.0-arm64', 'release-2.1.0', 'linux', 'standalone', 'arm64',
                     'agent-arm64.bin', 8, 'digest', 'stored-arm64.bin', 1, 'published', 1)
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
            "UPDATE agent_update_attempts SET status = 'downloading' WHERE release_id = $1 AND instance_id = $2",
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
            "SELECT artifact_id FROM agent_update_attempts WHERE release_id = $1 AND instance_id = $2",
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
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn normalizes_openwrt_standalone_target_to_linux() {
        let (state, resources) = test_state().await;
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
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn expires_agents_that_do_not_reconnect_after_installation() {
        let (state, resources) = test_state().await;
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
        sqlx::query("UPDATE agent_update_attempts SET updated_at = $1 WHERE instance_id = $2")
            .bind(now_ts() - UPDATE_HANDOFF_TIMEOUT_SECONDS - 1)
            .bind(&instance.id)
            .execute(&state.db)
            .await
            .expect("age attempt");

        assert_eq!(expire_restart_attempts(&state).await.expect("expire"), 1);
        let status: String =
            sqlx::query_scalar("SELECT status FROM agent_update_attempts WHERE instance_id = $1")
                .bind(&instance.id)
                .fetch_one(&state.db)
                .await
                .expect("read attempt");
        assert_eq!(status, "failed");
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn retry_rechecks_the_locked_release_state() {
        let (state, resources) = test_state().await;
        let headers = admin_headers(&state).await;
        insert_instance(
            &state,
            "rollback-race-agent",
            "ubuntu",
            "standalone",
            "amd64",
        )
        .await;
        insert_release(&state, "2.0.0", "standalone", "amd64").await;
        let attempt_id = insert_upgrade_attempt(
            &state,
            "release-2.0.0",
            "artifact-2.0.0-amd64",
            "rollback-race-agent",
            "1.0.0",
            "2.0.0",
            "failed",
        )
        .await;
        sqlx::query(
            "UPDATE agent_releases SET rollout_state = 'rollback_active' WHERE id = 'release-2.0.0'",
        )
        .execute(&state.db)
        .await
        .expect("activate rollback before retry");

        let error =
            match admin_retry_agent_update(State(state.clone()), headers, Path(attempt_id.clone()))
                .await
            {
                Ok(_) => panic!("rollback-active release must reject an upgrade retry"),
                Err(error) => error,
            };
        assert_eq!(error.status, StatusCode::CONFLICT);
        let stored: (String, i64) =
            sqlx::query_as("SELECT status, retry_count FROM agent_update_attempts WHERE id = $1")
                .bind(attempt_id)
                .fetch_one(&state.db)
                .await
                .expect("load rejected retry");
        assert_eq!(stored, ("failed".to_string(), 0));
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn rollback_retry_rejects_an_instance_that_changed_version() {
        let (state, resources) = test_state().await;
        let headers = admin_headers(&state).await;
        insert_instance(
            &state,
            "stale-rollback-agent",
            "ubuntu",
            "standalone",
            "amd64",
        )
        .await;
        insert_release(&state, "1.0.0", "standalone", "amd64").await;
        insert_release(&state, "2.0.0", "standalone", "amd64").await;
        set_instance_version_and_rollback(&state, "stale-rollback-agent", "3.0.0", true, "1.0.0")
            .await;
        let parent_id = insert_upgrade_attempt(
            &state,
            "release-2.0.0",
            "artifact-2.0.0-amd64",
            "stale-rollback-agent",
            "1.0.0",
            "2.0.0",
            "succeeded",
        )
        .await;
        let attempt_id = Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO agent_update_attempts(
                id, release_id, artifact_id, instance_id, operation, parent_attempt_id,
                from_version, target_version, status, retry_count, created_at, updated_at,
                completed_at
            ) VALUES($1, 'release-2.0.0', NULL, 'stale-rollback-agent', 'rollback', $2,
                     '2.0.0', '1.0.0', 'failed', 0, 2, 2, 2)
            "#,
        )
        .bind(&attempt_id)
        .bind(parent_id)
        .execute(&state.db)
        .await
        .expect("insert failed rollback attempt");

        let error =
            match admin_retry_agent_update(State(state.clone()), headers, Path(attempt_id.clone()))
                .await
            {
                Ok(_) => panic!("changed source version must reject rollback retry"),
                Err(error) => error,
            };
        assert_eq!(error.status, StatusCode::CONFLICT);
        let status: String =
            sqlx::query_scalar("SELECT status FROM agent_update_attempts WHERE id = $1")
                .bind(attempt_id)
                .fetch_one(&state.db)
                .await
                .expect("load stale rollback attempt");
        assert_eq!(status, "failed");
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn retry_count_cannot_exceed_the_websocket_protocol_limit() {
        let (state, resources) = test_state().await;
        let headers = admin_headers(&state).await;
        insert_instance(&state, "retry-limit-agent", "ubuntu", "standalone", "amd64").await;
        insert_release(&state, "2.0.0", "standalone", "amd64").await;
        let attempt_id = insert_upgrade_attempt(
            &state,
            "release-2.0.0",
            "artifact-2.0.0-amd64",
            "retry-limit-agent",
            "1.0.0",
            "2.0.0",
            "failed",
        )
        .await;
        sqlx::query("UPDATE agent_update_attempts SET retry_count = $1 WHERE id = $2")
            .bind(MAX_AGENT_UPDATE_RETRY_COUNT)
            .bind(&attempt_id)
            .execute(&state.db)
            .await
            .expect("set retry count to protocol limit");

        let error =
            match admin_retry_agent_update(State(state.clone()), headers, Path(attempt_id.clone()))
                .await
            {
                Ok(_) => panic!("retry beyond the protocol limit must fail"),
                Err(error) => error,
            };
        assert_eq!(error.status, StatusCode::CONFLICT);
        let stored: (String, i64) =
            sqlx::query_as("SELECT status, retry_count FROM agent_update_attempts WHERE id = $1")
                .bind(attempt_id)
                .fetch_one(&state.db)
                .await
                .expect("load capped retry");
        assert_eq!(stored, ("failed".to_string(), MAX_AGENT_UPDATE_RETRY_COUNT));
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn rejects_retry_when_a_newer_matching_release_exists() {
        let (state, resources) = test_state().await;
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
            SELECT id, release_id, artifact_id, instance_id, operation, parent_attempt_id,
                   from_version, target_version,
                   status, message, retry_count, created_at, updated_at, completed_at
            FROM agent_update_attempts WHERE instance_id = $1
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
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn failed_upgrade_is_not_automatically_requeued_for_the_same_release() {
        let (state, resources) = test_state().await;
        insert_instance(
            &state,
            "no-automatic-retry-agent",
            "ubuntu",
            "standalone",
            "amd64",
        )
        .await;
        insert_release(&state, "1.6.0", "standalone", "amd64").await;
        let instance = get_instance(&state.db, "no-automatic-retry-agent")
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
            offer.retry_count,
            "failed",
            Some("installation failed"),
        )
        .await
        .expect("record failed update");

        assert!(
            find_update_for_instance(&state, &instance)
                .await
                .expect("check automatic retry")
                .is_none(),
            "a failed release must wait for an explicit administrator retry"
        );
        let attempts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_update_attempts WHERE release_id = $1 AND instance_id = $2 AND operation = 'upgrade'",
        )
        .bind(&offer.release_id)
        .bind(&instance.id)
        .fetch_one(&state.db)
        .await
        .expect("count upgrade history");
        assert_eq!(attempts, 1);
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn ignores_status_from_an_older_retry_generation() {
        let (state, resources) = test_state().await;
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
            "UPDATE agent_update_attempts SET retry_count = 1, status = 'pending' WHERE instance_id = $1",
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
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn terminal_update_status_cannot_regress_and_version_is_committed() {
        let (state, resources) = test_state().await;
        insert_instance(
            &state,
            "terminal-state-agent",
            "ubuntu",
            "standalone",
            "amd64",
        )
        .await;
        insert_release(&state, "6.0.0", "standalone", "amd64").await;
        let instance = get_instance(&state.db, "terminal-state-agent")
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
            "succeeded",
            Some("installed"),
        )
        .await
        .expect("record success");
        let regression = record_update_status(
            &state,
            &instance.id,
            &offer.release_id,
            &offer.artifact_id,
            &offer.version,
            0,
            "waiting",
            Some("late stale status"),
        )
        .await
        .expect_err("terminal attempt must not regress");
        assert_eq!(regression.status, StatusCode::CONFLICT);
        record_update_status(
            &state,
            &instance.id,
            &offer.release_id,
            &offer.artifact_id,
            &offer.version,
            0,
            "succeeded",
            Some("duplicate terminal status"),
        )
        .await
        .expect("duplicate terminal status is idempotent");

        let (status, message, completed_at) =
            sqlx::query_as::<_, (String, String, Option<i64>)>(
                "SELECT status, message, completed_at FROM agent_update_attempts WHERE instance_id = $1",
            )
            .bind(&instance.id)
            .fetch_one(&state.db)
            .await
            .expect("load terminal attempt");
        assert_eq!(status, "succeeded");
        assert_eq!(message, "installed");
        assert!(completed_at.is_some());
        let agent_version: String =
            sqlx::query_scalar("SELECT agent_version FROM instances WHERE id = $1")
                .bind(&instance.id)
                .fetch_one(&state.db)
                .await
                .expect("load committed agent version");
        assert_eq!(agent_version, offer.version);
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn explicit_retry_remains_pinned_when_a_new_release_is_published() {
        let (state, resources) = test_state().await;
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
            "UPDATE agent_update_attempts SET status = 'pending', retry_count = 1 WHERE instance_id = $1",
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
        resources.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn stores_a_draft_artifact_after_publication_and_offers_it_only_after_publishing() {
        let (state, resources) = test_state().await;
        insert_release(&state, "5.0.0", "standalone", "amd64").await;
        insert_instance(&state, "late-arm-agent", "ubuntu", "standalone", "arm64").await;
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

        let artifact = store_artifact(&state, "release-5.0.0", received)
            .await
            .expect("published releases accept new draft targets");
        assert_eq!(artifact.status, "draft");
        assert_eq!(artifact.published_at, None);
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_artifacts WHERE release_id = 'release-5.0.0'",
        )
        .fetch_one(&state.db)
        .await
        .expect("count artifacts");
        assert_eq!(
            count, 2,
            "the new target is stored alongside the published one"
        );

        let instance = get_instance(&state.db, "late-arm-agent")
            .await
            .expect("load late target instance");
        assert!(
            find_update_for_instance(&state, &instance)
                .await
                .expect("check draft package visibility")
                .is_none(),
            "draft packages must not be offered"
        );

        sqlx::query(
            "UPDATE agent_artifacts SET status = 'published', published_at = 2 WHERE id = $1",
        )
        .bind(&artifact.id)
        .execute(&state.db)
        .await
        .expect("publish newly added target");
        let offer = find_update_for_instance(&state, &instance)
            .await
            .expect("check published package visibility")
            .expect("published package is offered");
        assert_eq!(offer.artifact_id, artifact.id);
        assert_eq!(offer.version, "5.0.0");
        resources.cleanup().await;
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
