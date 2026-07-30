use std::{
    collections::BTreeMap,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::FromRow;
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot, watch};
use tracing::warn;
use uuid::Uuid;

use crate::{
    auth::{AdminSessionGuard, require_admin},
    db::get_instance,
    error::{AppError, AppResult},
    models::{
        AgentOutbound, DockerComposeAction, DockerComposeTarget, DockerComposeValidation,
        DockerContainerAction, DockerContainerCreateSpec, DockerError, DockerErrorCode,
        DockerMountKind, DockerMountSpec, DockerNetworkCreateSpec, DockerPortSpec, DockerRequest,
        DockerResponse, DockerRestartPolicy, DockerStatus, DockerStatusState,
        DockerVolumeCreateSpec, TerminalClientMessage, TerminalServerMessage,
    },
    remote_desktop::ensure_same_origin,
    state::{
        AgentHandle, AppState, DockerExecSessionHandle, DockerLogEvent, DockerLogStreamHandle,
        DockerRequestFailure, PendingDockerRequest,
    },
    utils::now_ts,
};

pub(crate) const CAPABILITY: &str = "docker_manager_v1";
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(120);
const LONG_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_PENDING_REQUESTS_PER_INSTANCE: usize = 8;
const MAX_STREAMS_PER_INSTANCE: usize = 2;
const CANCEL_GRACE: Duration = Duration::from_secs(5);
const STREAM_BUFFER: usize = 64;
const SOCKET_SEND_TIMEOUT: Duration = Duration::from_secs(5);

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(admin_docker_status))
        .route(
            "/containers",
            get(admin_list_containers).post(admin_create_container),
        )
        .route(
            "/containers/{resource_id}",
            get(admin_inspect_container).delete(admin_remove_container),
        )
        .route(
            "/containers/{resource_id}/stats",
            get(admin_container_stats),
        )
        .route(
            "/containers/{resource_id}/actions/rename",
            post(admin_rename_container),
        )
        .route(
            "/containers/{resource_id}/actions/{action}",
            post(admin_container_action),
        )
        .route(
            "/containers/{resource_id}/logs/ws",
            get(admin_container_logs_ws),
        )
        .route(
            "/containers/{resource_id}/exec/ws",
            get(admin_container_exec_ws),
        )
        .route("/images", get(admin_list_images))
        .route("/images/pull", post(admin_pull_image))
        .route(
            "/images/{resource_id}",
            get(admin_inspect_image).delete(admin_remove_image),
        )
        .route("/images/{resource_id}/tag", post(admin_tag_image))
        .route(
            "/networks",
            get(admin_list_networks).post(admin_create_network),
        )
        .route(
            "/networks/{resource_id}",
            get(admin_inspect_network).delete(admin_remove_network),
        )
        .route(
            "/networks/{resource_id}/connect",
            post(admin_connect_network),
        )
        .route(
            "/networks/{resource_id}/disconnect",
            post(admin_disconnect_network),
        )
        .route(
            "/volumes",
            get(admin_list_volumes).post(admin_create_volume),
        )
        .route(
            "/volumes/{resource_id}",
            get(admin_inspect_volume).delete(admin_remove_volume),
        )
        .route("/compose/projects", get(admin_list_compose_projects))
        .route(
            "/compose/projects/{resource_id}",
            get(admin_inspect_compose_project),
        )
        .route("/compose/validate", post(admin_validate_compose))
        .route("/compose/deploy", post(admin_deploy_compose))
        .route(
            "/compose/projects/{resource_id}/actions/{action}",
            post(admin_compose_action),
        )
        .route("/system/df", get(admin_system_df))
        .route("/system/prune", post(admin_system_prune))
        .route("/prune/{resource}", post(admin_resource_prune))
}

async fn require_docker_write_admin(state: &AppState, headers: &HeaderMap) -> AppResult<String> {
    ensure_same_origin(headers, state.secure_cookies)?;
    Ok(require_admin(state, headers).await?.username)
}

#[derive(Debug, FromRow)]
struct DockerStatusRow {
    status: String,
    cli_version: Option<String>,
    engine_version: Option<String>,
    api_version: Option<String>,
    compose_version: Option<String>,
    diagnostic: String,
    checked_at: i64,
}

#[derive(Debug, Serialize)]
pub struct DockerStatusResponse {
    status: String,
    protocol_supported: bool,
    installed: bool,
    manageable: bool,
    online: bool,
    cli_version: Option<String>,
    engine_version: Option<String>,
    api_version: Option<String>,
    compose_version: Option<String>,
    diagnostic: Option<String>,
    checked_at: Option<i64>,
}

#[derive(Debug, Deserialize, Default)]
struct AllQuery {
    #[serde(default)]
    all: bool,
}

#[derive(Debug, Deserialize, Default)]
struct RemoveQuery {
    #[serde(default)]
    force: bool,
    #[serde(default)]
    remove_volumes: bool,
}

#[derive(Debug, Deserialize)]
struct ApiVolumeMount {
    name: String,
    target: String,
    #[serde(default, alias = "read_only")]
    readonly: bool,
}

#[derive(Debug, Deserialize)]
struct ApiBindMount {
    source: String,
    target: String,
    #[serde(default, alias = "read_only")]
    readonly: bool,
}

#[derive(Debug, Deserialize)]
struct CreateContainerRequest {
    #[serde(default)]
    name: String,
    image: String,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    environment: Vec<String>,
    #[serde(default)]
    ports: Vec<DockerPortSpec>,
    #[serde(default)]
    volumes: Vec<ApiVolumeMount>,
    #[serde(default)]
    bind_mounts: Vec<ApiBindMount>,
    network: Option<String>,
    restart_policy: Option<String>,
    restart_max_retries: Option<u32>,
    cpus: Option<f64>,
    memory_bytes: Option<u64>,
    #[serde(default, alias = "confirm_bind_write")]
    confirm_read_write_bind_mounts: bool,
}

#[derive(Debug, Deserialize, Default)]
struct ContainerActionRequest {
    timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RenameContainerRequest {
    name: String,
}

#[derive(Debug, Deserialize)]
struct PullImageRequest {
    #[serde(alias = "image")]
    reference: String,
}

#[derive(Debug, Deserialize)]
struct TagImageRequest {
    repository: String,
    tag: String,
}

#[derive(Debug, Deserialize)]
struct CreateNetworkRequest {
    name: String,
    driver: Option<String>,
    #[serde(default)]
    internal: bool,
    #[serde(default)]
    attachable: bool,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct NetworkMembershipRequest {
    container: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Deserialize)]
struct CreateVolumeRequest {
    name: String,
    driver: Option<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ComposeRequest {
    #[serde(default, alias = "project")]
    project_name: Option<String>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    profiles: Vec<String>,
    #[serde(default)]
    services: Vec<String>,
    #[serde(default, alias = "confirm_high_risk")]
    confirm_risks: bool,
    #[serde(default)]
    config_digest: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ComposeActionRequest {
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    profiles: Vec<String>,
    #[serde(default)]
    services: Vec<String>,
    config_digest: Option<String>,
    #[serde(default)]
    remove_volumes: bool,
    #[serde(default, alias = "confirm_high_risk")]
    confirm_risks: bool,
}

#[derive(Debug, Deserialize, Default)]
struct PruneRequest {
    #[serde(default)]
    all: bool,
    #[serde(default)]
    volumes: bool,
    #[serde(default)]
    confirm: bool,
}

#[derive(Debug, Deserialize)]
struct DockerLogsQuery {
    #[serde(default = "default_log_tail")]
    tail: u32,
    #[serde(default = "default_true")]
    follow: bool,
    since: Option<String>,
    #[serde(default)]
    _timestamps: bool,
}

#[derive(Debug, Deserialize)]
struct DockerExecQuery {
    #[serde(default = "default_shell")]
    shell: String,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
}

fn default_log_tail() -> u32 {
    200
}

fn default_true() -> bool {
    true
}

fn default_shell() -> String {
    "/bin/sh".to_string()
}

fn default_cols() -> u16 {
    120
}

fn default_rows() -> u16 {
    30
}

async fn admin_docker_status(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
) -> AppResult<Json<DockerStatusResponse>> {
    require_admin(&state, &headers).await?;
    get_instance(&state.db, &instance_id).await?;
    Ok(Json(docker_status_response(&state, &instance_id).await?))
}

async fn docker_status_response(
    state: &AppState,
    instance_id: &str,
) -> AppResult<DockerStatusResponse> {
    let agent = state.agents.read().await.get(instance_id).cloned();
    if let Some(agent) = agent {
        let protocol_supported = agent.capabilities.iter().any(|value| value == CAPABILITY);
        let current = if protocol_supported {
            agent.docker_status.read().await.clone()
        } else {
            None
        };
        return Ok(online_docker_status_response(
            protocol_supported,
            current.as_ref(),
        ));
    }

    let row = sqlx::query_as::<_, DockerStatusRow>(
        r#"
        SELECT status, cli_version, engine_version, api_version, compose_version,
               diagnostic, checked_at
        FROM instance_docker_status
        WHERE instance_id = $1
        "#,
    )
    .bind(instance_id)
    .fetch_optional(&state.db)
    .await?;
    Ok(offline_docker_status_response(row.as_ref()))
}

fn online_docker_status_response(
    protocol_supported: bool,
    current: Option<&DockerStatus>,
) -> DockerStatusResponse {
    let current = protocol_supported.then_some(current).flatten();
    let status = current
        .map(|status| docker_status_state(&status.state).to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let installed = !matches!(status.as_str(), "unknown" | "not_installed");
    DockerStatusResponse {
        manageable: protocol_supported && status == "ready",
        protocol_supported,
        installed,
        online: true,
        status,
        cli_version: current.and_then(|status| status.cli_version.clone()),
        engine_version: current.and_then(|status| status.engine_version.clone()),
        api_version: current.and_then(|status| status.api_version.clone()),
        compose_version: current.and_then(|status| status.compose_version.clone()),
        diagnostic: current
            .and_then(|status| status.message.as_deref())
            .filter(|message| !message.is_empty())
            .map(limited_message),
        checked_at: current.map(|status| status.checked_at),
    }
}

fn offline_docker_status_response(row: Option<&DockerStatusRow>) -> DockerStatusResponse {
    let status = row
        .map(|row| row.status.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let installed = !matches!(status.as_str(), "unknown" | "not_installed");

    DockerStatusResponse {
        status,
        protocol_supported: false,
        installed,
        manageable: false,
        online: false,
        cli_version: row.as_ref().and_then(|row| row.cli_version.clone()),
        engine_version: row.as_ref().and_then(|row| row.engine_version.clone()),
        api_version: row.as_ref().and_then(|row| row.api_version.clone()),
        compose_version: row.as_ref().and_then(|row| row.compose_version.clone()),
        diagnostic: row
            .as_ref()
            .and_then(|row| (!row.diagnostic.is_empty()).then(|| row.diagnostic.clone())),
        checked_at: row.map(|row| row.checked_at),
    }
}

async fn admin_list_containers(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<AllQuery>,
) -> AppResult<Json<Value>> {
    let actor = require_admin(&state, &headers).await?.username;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ContainerList { all: query.all },
    )
    .await
}

async fn admin_inspect_container(
    State(state): State<AppState>,
    Path((instance_id, container)): Path<(String, String)>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let actor = require_admin(&state, &headers).await?.username;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ContainerInspect { container },
    )
    .await
}

async fn admin_container_stats(
    State(state): State<AppState>,
    Path((instance_id, container)): Path<(String, String)>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let actor = require_admin(&state, &headers).await?.username;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ContainerStats {
            container: Some(container),
        },
    )
    .await
}

async fn admin_create_container(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<CreateContainerRequest>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    let spec = create_container_spec(payload)?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ContainerCreate { spec },
    )
    .await
}

async fn admin_container_action(
    State(state): State<AppState>,
    Path((instance_id, container, action)): Path<(String, String, String)>,
    headers: HeaderMap,
    payload: Option<Json<ContainerActionRequest>>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    let action = parse_container_action(&action)?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ContainerAction {
            container,
            action,
            timeout_seconds: payload.and_then(|Json(payload)| payload.timeout_seconds),
        },
    )
    .await
}

async fn admin_rename_container(
    State(state): State<AppState>,
    Path((instance_id, container)): Path<(String, String)>,
    headers: HeaderMap,
    Json(payload): Json<RenameContainerRequest>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ContainerRename {
            container,
            name: payload.name,
        },
    )
    .await
}

async fn admin_remove_container(
    State(state): State<AppState>,
    Path((instance_id, container)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<RemoveQuery>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ContainerRemove {
            container,
            force: query.force,
            remove_volumes: query.remove_volumes,
        },
    )
    .await
}

async fn admin_list_images(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<AllQuery>,
) -> AppResult<Json<Value>> {
    let actor = require_admin(&state, &headers).await?.username;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ImageList { all: query.all },
    )
    .await
}

async fn admin_inspect_image(
    State(state): State<AppState>,
    Path((instance_id, image)): Path<(String, String)>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let actor = require_admin(&state, &headers).await?.username;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ImageInspect { image },
    )
    .await
}

async fn admin_pull_image(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<PullImageRequest>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ImagePull {
            image: payload.reference,
        },
    )
    .await
}

async fn admin_tag_image(
    State(state): State<AppState>,
    Path((instance_id, image)): Path<(String, String)>,
    headers: HeaderMap,
    Json(payload): Json<TagImageRequest>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    let repository = payload.repository.trim();
    let tag = payload.tag.trim();
    if repository.is_empty() || tag.is_empty() || tag.contains(':') {
        return Err(AppError::bad_request("镜像仓库和标签格式无效"));
    }
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ImageTag {
            source: image,
            target: format!("{repository}:{tag}"),
        },
    )
    .await
}

async fn admin_remove_image(
    State(state): State<AppState>,
    Path((instance_id, image)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<RemoveQuery>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ImageRemove {
            image,
            force: query.force,
        },
    )
    .await
}

async fn admin_list_networks(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let actor = require_admin(&state, &headers).await?.username;
    execute_json(&state, &instance_id, &actor, DockerRequest::NetworkList).await
}

async fn admin_inspect_network(
    State(state): State<AppState>,
    Path((instance_id, network)): Path<(String, String)>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let actor = require_admin(&state, &headers).await?.username;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::NetworkInspect { network },
    )
    .await
}

async fn admin_create_network(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<CreateNetworkRequest>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::NetworkCreate {
            spec: DockerNetworkCreateSpec {
                name: payload.name,
                driver: payload.driver,
                internal: payload.internal,
                attachable: payload.attachable,
                labels: payload.labels,
            },
        },
    )
    .await
}

async fn admin_connect_network(
    State(state): State<AppState>,
    Path((instance_id, network)): Path<(String, String)>,
    headers: HeaderMap,
    Json(payload): Json<NetworkMembershipRequest>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::NetworkConnect {
            network,
            container: payload.container,
            aliases: payload.aliases,
        },
    )
    .await
}

async fn admin_disconnect_network(
    State(state): State<AppState>,
    Path((instance_id, network)): Path<(String, String)>,
    headers: HeaderMap,
    Json(payload): Json<NetworkMembershipRequest>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::NetworkDisconnect {
            network,
            container: payload.container,
            force: payload.force,
        },
    )
    .await
}

async fn admin_remove_network(
    State(state): State<AppState>,
    Path((instance_id, network)): Path<(String, String)>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::NetworkRemove { network },
    )
    .await
}

async fn admin_list_volumes(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let actor = require_admin(&state, &headers).await?.username;
    execute_json(&state, &instance_id, &actor, DockerRequest::VolumeList).await
}

async fn admin_inspect_volume(
    State(state): State<AppState>,
    Path((instance_id, volume)): Path<(String, String)>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let actor = require_admin(&state, &headers).await?.username;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::VolumeInspect { volume },
    )
    .await
}

async fn admin_create_volume(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<CreateVolumeRequest>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::VolumeCreate {
            spec: DockerVolumeCreateSpec {
                name: payload.name,
                driver: payload.driver,
                labels: payload.labels,
            },
        },
    )
    .await
}

async fn admin_remove_volume(
    State(state): State<AppState>,
    Path((instance_id, volume)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<RemoveQuery>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::VolumeRemove {
            volume,
            force: query.force,
        },
    )
    .await
}

async fn admin_list_compose_projects(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let actor = require_admin(&state, &headers).await?.username;
    execute_json(&state, &instance_id, &actor, DockerRequest::ComposeList).await
}

async fn admin_inspect_compose_project(
    State(state): State<AppState>,
    Path((instance_id, project)): Path<(String, String)>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let actor = require_admin(&state, &headers).await?.username;
    let target = resolve_compose_target(
        &state,
        &instance_id,
        &actor,
        project,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
    .await?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ComposeInspect { target },
    )
    .await
}

async fn admin_validate_compose(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<ComposeRequest>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    validate_compose_files(&payload.files)?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ComposeValidate {
            target: compose_target(&payload),
        },
    )
    .await
}

async fn admin_deploy_compose(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<ComposeRequest>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    validate_compose_files(&payload.files)?;
    let target = compose_target(&payload);
    let digest = payload
        .config_digest
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::bad_request("部署前必须先校验 Compose 配置"))?;
    validate_config_digest(digest)?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ComposeDeploy {
            target,
            config_digest: digest.to_string(),
            confirm_high_risk: payload.confirm_risks,
        },
    )
    .await
}

async fn compose_action_digest(
    state: &AppState,
    instance_id: &str,
    actor: &str,
    action: DockerComposeAction,
    target: &DockerComposeTarget,
    supplied: Option<&str>,
) -> AppResult<String> {
    if let Some(digest) = supplied_compose_action_digest(action, supplied)? {
        return Ok(digest);
    }
    let response = execute_request(
        state,
        instance_id,
        actor,
        DockerRequest::ComposeValidate {
            target: target.clone(),
        },
    )
    .await?;
    let DockerResponse::ComposeValidation { validation } = response else {
        return Err(unexpected_response());
    };
    validate_config_digest(&validation.config_digest)?;
    Ok(validation.config_digest)
}

fn supplied_compose_action_digest(
    action: DockerComposeAction,
    supplied: Option<&str>,
) -> AppResult<Option<String>> {
    if action != DockerComposeAction::Up {
        return Ok(None);
    }
    let digest = supplied
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::bad_request("Compose up 前必须先校验配置"))?;
    validate_config_digest(digest)?;
    Ok(Some(digest.to_string()))
}

fn validate_config_digest(digest: &str) -> AppResult<()> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(AppError::bad_request("Compose 配置摘要格式无效"));
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::bad_request("Compose 配置摘要格式无效"));
    }
    Ok(())
}

async fn admin_compose_action(
    State(state): State<AppState>,
    Path((instance_id, project, action)): Path<(String, String, String)>,
    headers: HeaderMap,
    payload: Option<Json<ComposeActionRequest>>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    let payload = payload.map(|Json(payload)| payload).unwrap_or_default();
    let action = parse_compose_action(&action)?;
    let target = resolve_compose_target(
        &state,
        &instance_id,
        &actor,
        project,
        payload.files,
        payload.profiles,
        payload.services,
    )
    .await?;
    let config_digest = compose_action_digest(
        &state,
        &instance_id,
        &actor,
        action,
        &target,
        payload.config_digest.as_deref(),
    )
    .await?;
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::ComposeAction {
            target,
            action,
            config_digest,
            remove_volumes: payload.remove_volumes,
            confirm_high_risk: payload.confirm_risks,
        },
    )
    .await
}

async fn admin_system_df(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let actor = require_admin(&state, &headers).await?.username;
    execute_json(&state, &instance_id, &actor, DockerRequest::SystemDf).await
}

async fn admin_system_prune(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<PruneRequest>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    if !payload.confirm {
        return Err(AppError::bad_request("系统清理需要显式确认"));
    }
    execute_json(
        &state,
        &instance_id,
        &actor,
        DockerRequest::SystemPrune {
            containers: true,
            images: true,
            networks: true,
            volumes: payload.volumes,
            all_images: payload.all,
        },
    )
    .await
}

async fn admin_resource_prune(
    State(state): State<AppState>,
    Path((instance_id, resource)): Path<(String, String)>,
    headers: HeaderMap,
    Json(payload): Json<PruneRequest>,
) -> AppResult<Json<Value>> {
    let actor = require_docker_write_admin(&state, &headers).await?;
    if !payload.confirm {
        return Err(AppError::bad_request("资源清理需要显式确认"));
    }
    let request = match resource.as_str() {
        "containers" => DockerRequest::SystemPrune {
            containers: true,
            images: false,
            networks: false,
            volumes: false,
            all_images: false,
        },
        "images" if payload.all => DockerRequest::SystemPrune {
            containers: false,
            images: true,
            networks: false,
            volumes: false,
            all_images: true,
        },
        "images" => DockerRequest::ImagePrune,
        "networks" => DockerRequest::NetworkPrune,
        "volumes" => DockerRequest::VolumePrune,
        _ => return Err(AppError::bad_request("不支持的 Docker 清理类型")),
    };
    execute_json(&state, &instance_id, &actor, request).await
}

fn create_container_spec(payload: CreateContainerRequest) -> AppResult<DockerContainerCreateSpec> {
    let mut environment = BTreeMap::new();
    for entry in payload.environment {
        let Some((key, value)) = entry.split_once('=') else {
            return Err(AppError::bad_request("环境变量必须使用 KEY=VALUE 格式"));
        };
        if key.is_empty() {
            return Err(AppError::bad_request("环境变量名称不能为空"));
        }
        environment.insert(key.to_string(), value.to_string());
    }

    let mut mounts = Vec::with_capacity(payload.volumes.len() + payload.bind_mounts.len());
    mounts.extend(payload.volumes.into_iter().map(|mount| DockerMountSpec {
        kind: DockerMountKind::Volume,
        source: mount.name,
        target: mount.target,
        read_only: mount.readonly,
    }));
    mounts.extend(
        payload
            .bind_mounts
            .into_iter()
            .map(|mount| DockerMountSpec {
                kind: DockerMountKind::Bind,
                source: mount.source,
                target: mount.target,
                read_only: mount.readonly,
            }),
    );

    let restart_policy = match payload.restart_policy.as_deref() {
        None | Some("") => None,
        Some("no") => Some(DockerRestartPolicy::No),
        Some("always") => Some(DockerRestartPolicy::Always),
        Some("unless-stopped" | "unless_stopped") => Some(DockerRestartPolicy::UnlessStopped),
        Some("on-failure" | "on_failure") => Some(DockerRestartPolicy::OnFailure),
        Some(_) => return Err(AppError::bad_request("不支持的容器重启策略")),
    };

    Ok(DockerContainerCreateSpec {
        name: (!payload.name.trim().is_empty()).then(|| payload.name.trim().to_string()),
        image: payload.image,
        command: payload.command,
        environment,
        ports: payload.ports,
        mounts,
        network: payload.network,
        restart_policy,
        restart_max_retries: payload.restart_max_retries,
        cpus: payload.cpus,
        memory_bytes: payload.memory_bytes,
        confirm_bind_write: payload.confirm_read_write_bind_mounts,
    })
}

fn parse_container_action(value: &str) -> AppResult<DockerContainerAction> {
    match value {
        "start" => Ok(DockerContainerAction::Start),
        "stop" => Ok(DockerContainerAction::Stop),
        "restart" => Ok(DockerContainerAction::Restart),
        "kill" => Ok(DockerContainerAction::Kill),
        "pause" => Ok(DockerContainerAction::Pause),
        "unpause" => Ok(DockerContainerAction::Unpause),
        _ => Err(AppError::bad_request("不支持的容器操作")),
    }
}

fn parse_compose_action(value: &str) -> AppResult<DockerComposeAction> {
    match value {
        "pull" => Ok(DockerComposeAction::Pull),
        "up" => Ok(DockerComposeAction::Up),
        "start" => Ok(DockerComposeAction::Start),
        "stop" => Ok(DockerComposeAction::Stop),
        "restart" => Ok(DockerComposeAction::Restart),
        "down" => Ok(DockerComposeAction::Down),
        _ => Err(AppError::bad_request("不支持的 Compose 操作")),
    }
}

fn validate_compose_files(files: &[String]) -> AppResult<()> {
    if files.is_empty() || files.len() > 8 {
        return Err(AppError::bad_request("Compose 文件数量必须为 1 到 8 个"));
    }
    if files.iter().any(|file| file.trim().is_empty()) {
        return Err(AppError::bad_request("Compose 文件路径不能为空"));
    }
    Ok(())
}

fn compose_target(payload: &ComposeRequest) -> DockerComposeTarget {
    DockerComposeTarget {
        project: payload.project_name.clone(),
        files: payload.files.clone(),
        profiles: payload.profiles.clone(),
        services: payload.services.clone(),
    }
}

async fn resolve_compose_target(
    state: &AppState,
    instance_id: &str,
    actor: &str,
    project: String,
    files: Vec<String>,
    profiles: Vec<String>,
    services: Vec<String>,
) -> AppResult<DockerComposeTarget> {
    let files = if files.is_empty() {
        let response =
            execute_request(state, instance_id, actor, DockerRequest::ComposeList).await?;
        let DockerResponse::Data { data } = response else {
            return Err(unexpected_response());
        };
        compose_project_files(&data, &project).ok_or_else(|| {
            AppError::new(StatusCode::NOT_FOUND, "Compose 项目不存在或缺少配置文件")
        })?
    } else {
        files
    };
    validate_compose_files(&files)?;
    Ok(DockerComposeTarget {
        project: Some(project),
        files,
        profiles,
        services,
    })
}

fn compose_project_files(value: &Value, project: &str) -> Option<Vec<String>> {
    let projects = value.as_array()?;
    let project = projects.iter().find_map(|value| {
        let value = value.as_object()?;
        (string_field(value, &["Name", "name"]) == project).then_some(value)
    })?;
    let files = split_csv_field(project, &["ConfigFiles", "config_files"]);
    (!files.is_empty()).then_some(files)
}

async fn execute_json(
    state: &AppState,
    instance_id: &str,
    actor: &str,
    request: DockerRequest,
) -> AppResult<Json<Value>> {
    let view = response_view(&request);
    let value = response_value(execute_request(state, instance_id, actor, request).await?);
    Ok(Json(normalize_response(view, value)))
}

#[derive(Clone, Copy)]
enum ResponseView {
    Raw,
    Inspect,
    ContainerList,
    ContainerStats,
    ImageList,
    NetworkList,
    VolumeList,
    ComposeList,
    SystemDf,
}

fn response_view(request: &DockerRequest) -> ResponseView {
    match request {
        DockerRequest::ContainerList { .. } => ResponseView::ContainerList,
        DockerRequest::ContainerInspect { .. }
        | DockerRequest::ImageInspect { .. }
        | DockerRequest::NetworkInspect { .. }
        | DockerRequest::VolumeInspect { .. } => ResponseView::Inspect,
        DockerRequest::ContainerStats { .. } => ResponseView::ContainerStats,
        DockerRequest::ImageList { .. } => ResponseView::ImageList,
        DockerRequest::NetworkList => ResponseView::NetworkList,
        DockerRequest::VolumeList => ResponseView::VolumeList,
        DockerRequest::ComposeList => ResponseView::ComposeList,
        DockerRequest::SystemDf => ResponseView::SystemDf,
        _ => ResponseView::Raw,
    }
}

fn normalize_response(view: ResponseView, value: Value) -> Value {
    match view {
        ResponseView::Raw => value,
        ResponseView::Inspect => unwrap_single(value),
        ResponseView::ContainerList => normalize_list(value, normalize_container),
        ResponseView::ContainerStats => {
            let value = unwrap_single(value);
            value
                .as_object()
                .map(normalize_container_stats)
                .unwrap_or(value)
        }
        ResponseView::ImageList => normalize_list(value, normalize_image),
        ResponseView::NetworkList => normalize_list(value, normalize_network),
        ResponseView::VolumeList => normalize_list(value, normalize_volume),
        ResponseView::ComposeList => normalize_list(value, normalize_compose_project),
        ResponseView::SystemDf => normalize_system_df(value),
    }
}

fn unwrap_single(value: Value) -> Value {
    match value {
        Value::Array(mut values) if values.len() == 1 => values.remove(0),
        value => value,
    }
}

fn normalize_list(value: Value, normalize: fn(&Map<String, Value>) -> Value) -> Value {
    let values = match value {
        Value::Array(values) => values,
        Value::Null => Vec::new(),
        value => vec![value],
    };
    Value::Array(
        values
            .into_iter()
            .filter_map(|value| value.as_object().map(normalize))
            .collect(),
    )
}

fn normalize_container(source: &Map<String, Value>) -> Value {
    json!({
        "id": string_field(source, &["id", "ID"]),
        "name": string_field(source, &["name", "Names"]),
        "image": string_field(source, &["image", "Image"]),
        "command": string_field(source, &["command", "Command"]),
        "created": field(source, &["created", "CreatedAt"]),
        "state": string_field(source, &["state", "State"]).to_ascii_lowercase(),
        "status": string_field(source, &["status", "Status"]),
        "ports": parse_ports(&string_field(source, &["Ports", "ports"])),
        "mounts": split_csv_field(source, &["Mounts", "mounts"]),
        "networks": split_csv_field(source, &["Networks", "networks"]),
        "labels": parse_labels(&string_field(source, &["Labels", "labels"])),
    })
}

fn normalize_container_stats(source: &Map<String, Value>) -> Value {
    let (memory_usage, memory_limit) = parse_pair_bytes(&string_field(source, &["MemUsage"]));
    let (network_rx, network_tx) = parse_pair_bytes(&string_field(source, &["NetIO"]));
    let (block_read, block_write) = parse_pair_bytes(&string_field(source, &["BlockIO"]));
    json!({
        "container_id": string_field(source, &["Container", "ID", "Name"]),
        "cpu_percent": parse_percent(&string_field(source, &["CPUPerc"])),
        "memory_usage": memory_usage,
        "memory_limit": memory_limit,
        "memory_percent": parse_percent(&string_field(source, &["MemPerc"])),
        "network_rx": network_rx,
        "network_tx": network_tx,
        "block_read": block_read,
        "block_write": block_write,
        "pids": string_field(source, &["PIDs"]).parse::<u64>().ok(),
    })
}

fn normalize_image(source: &Map<String, Value>) -> Value {
    let repository = string_field(source, &["Repository"]);
    let tag = string_field(source, &["Tag"]);
    let repo_tags = if repository.is_empty() || repository == "<none>" {
        Vec::new()
    } else {
        vec![format!("{repository}:{tag}")]
    };
    let digest = string_field(source, &["Digest"]);
    json!({
        "id": string_field(source, &["id", "ID"]),
        "repo_tags": repo_tags,
        "repo_digests": if digest.is_empty() || digest == "<none>" { Vec::new() } else { vec![digest] },
        "created": field(source, &["created", "CreatedAt"]),
        "size": parse_bytes(&string_field(source, &["Size"])),
        "shared_size": parse_bytes(&string_field(source, &["SharedSize"])),
        "virtual_size": parse_bytes(&string_field(source, &["VirtualSize"])),
    })
}

fn normalize_network(source: &Map<String, Value>) -> Value {
    json!({
        "id": string_field(source, &["id", "ID"]),
        "name": string_field(source, &["name", "Name"]),
        "driver": string_field(source, &["driver", "Driver"]),
        "scope": string_field(source, &["scope", "Scope"]),
        "internal": bool_field(source, &["internal", "Internal"]),
        "attachable": bool_field(source, &["attachable", "Attachable"]),
        "labels": parse_labels(&string_field(source, &["Labels", "labels"])),
    })
}

fn normalize_volume(source: &Map<String, Value>) -> Value {
    json!({
        "name": string_field(source, &["name", "Name"]),
        "driver": string_field(source, &["driver", "Driver"]),
        "mountpoint": string_field(source, &["mountpoint", "Mountpoint"]),
        "scope": string_field(source, &["scope", "Scope"]),
        "labels": parse_labels(&string_field(source, &["Labels", "labels"])),
    })
}

fn normalize_compose_project(source: &Map<String, Value>) -> Value {
    let config_files = split_csv_field(source, &["ConfigFiles", "config_files"]);
    json!({
        "name": string_field(source, &["name", "Name"]),
        "status": string_field(source, &["status", "Status"]),
        "config_files": config_files,
        "working_dir": string_field(source, &["working_dir", "WorkingDir"]),
    })
}

fn normalize_system_df(value: Value) -> Value {
    if let Value::Array(rows) = &value {
        let mut image_count = None;
        let mut container_count = None;
        let mut volume_count = None;
        let mut images_size = None;
        let mut containers_size = None;
        let mut volumes_size = None;
        let mut reclaimable_size = 0_u64;
        for row in rows.iter().filter_map(Value::as_object) {
            let resource_type = string_field(row, &["Type", "type"]);
            let count = string_field(row, &["TotalCount", "total_count"])
                .parse::<u64>()
                .ok();
            let size = number_or_bytes(row, &["Size", "size"]);
            reclaimable_size = reclaimable_size
                .saturating_add(number_or_bytes(row, &["Reclaimable", "reclaimable"]).unwrap_or(0));
            match resource_type.as_str() {
                "Images" => {
                    image_count = count;
                    images_size = size;
                }
                "Containers" => {
                    container_count = count;
                    containers_size = size;
                }
                "Local Volumes" | "Volumes" => {
                    volume_count = count;
                    volumes_size = size;
                }
                _ => {}
            }
        }
        return json!({
            "image_count": image_count,
            "container_count": container_count,
            "volume_count": volume_count,
            "images_size": images_size,
            "containers_size": containers_size,
            "volumes_size": volumes_size,
            "reclaimable_size": reclaimable_size,
            "rows": rows,
        });
    }
    let value = unwrap_single(value);
    let Some(source) = value.as_object() else {
        return value;
    };
    let images = field(source, &["images", "Images"]);
    let containers = field(source, &["containers", "Containers"]);
    let volumes = field(source, &["volumes", "Volumes", "LocalVolumes"]);
    let build_cache = field(source, &["build_cache", "BuildCache"]);
    json!({
        "layers_size": number_or_bytes(source, &["layers_size", "LayersSize"]),
        "images": images,
        "containers": containers,
        "volumes": volumes,
        "build_cache": build_cache,
    })
}

fn field(source: &Map<String, Value>, names: &[&str]) -> Value {
    names
        .iter()
        .find_map(|name| source.get(*name))
        .cloned()
        .unwrap_or(Value::Null)
}

fn string_field(source: &Map<String, Value>, names: &[&str]) -> String {
    match field(source, names) {
        Value::String(value) => value,
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        _ => String::new(),
    }
}

fn bool_field(source: &Map<String, Value>, names: &[&str]) -> bool {
    match field(source, names) {
        Value::Bool(value) => value,
        Value::String(value) => value.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn split_csv_field(source: &Map<String, Value>, names: &[&str]) -> Vec<String> {
    match field(source, names) {
        Value::Array(values) => values
            .into_iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect(),
        Value::String(value) => value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_labels(value: &str) -> BTreeMap<String, String> {
    value
        .split(',')
        .filter_map(|label| label.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

fn parse_ports(value: &str) -> Vec<Value> {
    value
        .split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let (mapping, protocol) = entry.rsplit_once('/').unwrap_or((entry, "tcp"));
            let (host, container) = mapping
                .split_once("->")
                .map_or((None, mapping), |(host, container)| (Some(host), container));
            let private_port = container.parse::<u16>().ok()?;
            let (ip, public_port) = host.map_or((None, None), |host| {
                host.rsplit_once(':')
                    .map_or((None, host.parse::<u16>().ok()), |(ip, port)| {
                        (
                            (!ip.is_empty()).then(|| ip.trim_matches(['[', ']']).to_string()),
                            port.parse::<u16>().ok(),
                        )
                    })
            });
            Some(json!({
                "ip": ip,
                "public_port": public_port,
                "private_port": private_port,
                "type": protocol,
            }))
        })
        .collect()
}

fn parse_percent(value: &str) -> Option<f64> {
    value.trim().trim_end_matches('%').parse().ok()
}

fn parse_pair_bytes(value: &str) -> (Option<u64>, Option<u64>) {
    let Some((left, right)) = value.split_once('/') else {
        return (parse_bytes(value), None);
    };
    (parse_bytes(left), parse_bytes(right))
}

fn parse_bytes(value: &str) -> Option<u64> {
    let value = value.trim().split_whitespace().next()?;
    let split = value
        .find(|character: char| !(character.is_ascii_digit() || character == '.'))
        .unwrap_or(value.len());
    let number = value[..split].parse::<f64>().ok()?;
    let unit = value[split..].trim().to_ascii_lowercase();
    let multiplier = match unit.as_str() {
        "" | "b" => 1.0,
        "kb" => 1_000.0,
        "mb" => 1_000_000.0,
        "gb" => 1_000_000_000.0,
        "tb" => 1_000_000_000_000.0,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * multiplier).round() as u64)
}

fn number_or_bytes(source: &Map<String, Value>, names: &[&str]) -> Option<u64> {
    match field(source, names) {
        Value::Number(value) => value.as_u64(),
        Value::String(value) => parse_bytes(&value),
        _ => None,
    }
}

fn response_value(response: DockerResponse) -> Value {
    match response {
        DockerResponse::Data { data } | DockerResponse::OperationComplete { data } => data,
        DockerResponse::ComposeValidation { validation } => compose_validation_value(validation),
        DockerResponse::Error { .. } => unreachable!("errors are mapped before HTTP serialization"),
    }
}

fn compose_validation_value(validation: DockerComposeValidation) -> Value {
    json!({
        "valid": true,
        "project_name": validation.project,
        "services": validation.services,
        "service_summaries": validation.service_summaries,
        "config_summary": validation.config_summary,
        "warnings": validation.warnings,
        "config_digest": validation.config_digest,
    })
}

struct DockerRequestCleanup {
    state: AppState,
    instance_id: String,
    connection_id: Uuid,
    request_id: String,
    failure_code: &'static str,
    failure_message: &'static str,
    armed: bool,
}

impl DockerRequestCleanup {
    fn release(&mut self) {
        self.armed = false;
    }

    fn timeout(&mut self) {
        self.failure_code = "timeout";
        self.failure_message = "Docker 操作超时";
    }
}

impl Drop for DockerRequestCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let state = self.state.clone();
        let instance_id = self.instance_id.clone();
        let connection_id = self.connection_id;
        let request_id = self.request_id.clone();
        let failure_code = self.failure_code;
        let failure_message = self.failure_message;
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let matches = state
                .docker_requests
                .read()
                .await
                .get(&request_id)
                .is_some_and(|pending| {
                    pending.instance_id == instance_id
                        && pending.agent_connection_id == connection_id
                });
            if !matches {
                return;
            }
            if let Some(agent) = state
                .agents
                .read()
                .await
                .get(&instance_id)
                .filter(|agent| agent.connection_id == connection_id)
                .cloned()
            {
                let _ = agent.tx.send(AgentOutbound::DockerCancel {
                    request_id: request_id.clone(),
                });
            }
            tokio::time::sleep(CANCEL_GRACE).await;
            let pending = {
                let mut requests = state.docker_requests.write().await;
                let matches = requests.get(&request_id).is_some_and(|pending| {
                    pending.instance_id == instance_id
                        && pending.agent_connection_id == connection_id
                });
                matches.then(|| requests.remove(&request_id)).flatten()
            };
            if let Some(pending) = pending {
                let _ = pending.tx.send(Err(DockerRequestFailure::Disconnected));
                finish_audit_best_effort(
                    &state,
                    pending.audit_id.as_deref(),
                    "failed",
                    Some(failure_code),
                    failure_message,
                )
                .await;
            }
        });
    }
}

async fn acquire_request_slot(
    state: &AppState,
    instance_id: &str,
) -> AppResult<OwnedSemaphorePermit> {
    let slots = {
        let mut slots = state.docker_request_slots.lock().await;
        slots
            .entry(instance_id.to_string())
            .or_insert_with(|| {
                Arc::new(tokio::sync::Semaphore::new(
                    MAX_PENDING_REQUESTS_PER_INSTANCE,
                ))
            })
            .clone()
    };
    slots
        .try_acquire_owned()
        .map_err(|_| AppError::new(StatusCode::CONFLICT, "实例 Docker 请求繁忙，请稍后重试"))
}

async fn execute_request(
    state: &AppState,
    instance_id: &str,
    actor: &str,
    request: DockerRequest,
) -> AppResult<DockerResponse> {
    let audit = request_audit_metadata(&request);
    let request_id = Uuid::new_v4().to_string();
    let (agent, audit_id) = if audit.mutation {
        // Preserve the existing 404 contract and the audit table's instance FK.
        let instance = get_instance(&state.db, instance_id).await?;
        let audit_id = start_audit(
            state,
            instance_id,
            &request_id,
            actor,
            audit.operation,
            &audit.target,
            &audit.metadata,
        )
        .await?;
        let agent = match connected_manageable_agent(state, instance_id, instance.disabled).await {
            Ok(agent) => agent,
            Err(error) => {
                finish_audit_best_effort(
                    state,
                    Some(&audit_id),
                    "failed",
                    Some(error.audit_code),
                    "",
                )
                .await;
                return Err(error.error);
            }
        };
        (agent, Some(audit_id))
    } else {
        (manageable_agent(state, instance_id).await?, None)
    };
    let permit = match acquire_request_slot(state, instance_id).await {
        Ok(permit) => permit,
        Err(error) => {
            finish_audit_best_effort(state, audit_id.as_deref(), "failed", Some("busy"), "").await;
            return Err(error);
        }
    };
    let timeout = request_timeout(&request);
    let (tx, rx) = oneshot::channel();
    let mut cleanup = DockerRequestCleanup {
        state: state.clone(),
        instance_id: instance_id.to_string(),
        connection_id: agent.connection_id,
        request_id: request_id.clone(),
        failure_code: "client_disconnected",
        failure_message: "客户端已断开",
        armed: true,
    };

    let send_failed = {
        let agents = state.agents.read().await;
        let current = agents
            .get(instance_id)
            .filter(|current| current.connection_id == agent.connection_id)
            .cloned();
        let Some(current) = current else {
            drop(agents);
            finish_audit_best_effort(
                state,
                audit_id.as_deref(),
                "failed",
                Some("offline"),
                "实例连接已断开",
            )
            .await;
            cleanup.release();
            return Err(AppError::new(StatusCode::CONFLICT, "实例不在线"));
        };
        state.docker_requests.write().await.insert(
            request_id.clone(),
            PendingDockerRequest {
                instance_id: instance_id.to_string(),
                agent_connection_id: agent.connection_id,
                tx,
                audit_id,
                _permit: permit,
            },
        );
        current
            .tx
            .send(AgentOutbound::DockerRequest {
                request_id: request_id.clone(),
                request,
            })
            .is_err()
    };
    if send_failed {
        if let Some(pending) = state.docker_requests.write().await.remove(&request_id) {
            finish_audit_best_effort(
                state,
                pending.audit_id.as_deref(),
                "failed",
                Some("offline"),
                "实例连接已断开",
            )
            .await;
        }
        cleanup.release();
        return Err(AppError::new(StatusCode::CONFLICT, "实例不在线"));
    }

    let response = match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(Ok(response))) => response,
        Ok(Ok(Err(DockerRequestFailure::Disconnected))) | Ok(Err(_)) => {
            cleanup.release();
            return Err(AppError::new(StatusCode::CONFLICT, "实例连接已断开"));
        }
        Err(_) => {
            cleanup.timeout();
            return Err(AppError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "Docker 操作超时",
            ));
        }
    };
    cleanup.release();

    if let DockerResponse::Error { error } = response {
        return Err(map_docker_error(error));
    }
    Ok(response)
}

async fn manageable_agent(
    state: &AppState,
    instance_id: &str,
) -> AppResult<crate::state::AgentHandle> {
    let instance = get_instance(&state.db, instance_id).await?;
    connected_manageable_agent(state, instance_id, instance.disabled)
        .await
        .map_err(|error| error.error)
}

struct DockerManageabilityError {
    error: AppError,
    audit_code: &'static str,
}

impl DockerManageabilityError {
    fn new(audit_code: &'static str, message: &'static str) -> Self {
        Self {
            error: AppError::new(StatusCode::CONFLICT, message),
            audit_code,
        }
    }
}

async fn connected_manageable_agent(
    state: &AppState,
    instance_id: &str,
    disabled: i64,
) -> Result<crate::state::AgentHandle, DockerManageabilityError> {
    if disabled == 1 {
        return Err(DockerManageabilityError::new("disabled", "实例已停用"));
    }
    let Some(agent) = state.agents.read().await.get(instance_id).cloned() else {
        return Err(DockerManageabilityError::new("offline", "实例不在线"));
    };
    if !agent.capabilities.iter().any(|value| value == CAPABILITY) {
        return Err(DockerManageabilityError::new(
            "unsupported",
            "实例 Agent 不支持 Docker 管理协议",
        ));
    }
    let status = agent
        .docker_status
        .read()
        .await
        .as_ref()
        .map(|status| status.state);
    if status != Some(DockerStatusState::Ready) {
        let audit_code = match status {
            Some(DockerStatusState::NotInstalled) => "not_installed",
            Some(DockerStatusState::DaemonUnreachable) | None => "daemon_unavailable",
            Some(DockerStatusState::PermissionDenied) => "permission_denied",
            Some(DockerStatusState::UnsupportedVersion) => "unsupported_version",
            Some(DockerStatusState::Error) => "internal",
            Some(DockerStatusState::Ready) => unreachable!(),
        };
        return Err(DockerManageabilityError::new(
            audit_code,
            "实例 Docker 当前不可管理",
        ));
    }
    Ok(agent)
}

fn request_timeout(request: &DockerRequest) -> Duration {
    match request {
        DockerRequest::ImagePull { .. }
        | DockerRequest::ImagePrune
        | DockerRequest::NetworkPrune
        | DockerRequest::VolumePrune
        | DockerRequest::ComposeDeploy { .. }
        | DockerRequest::ComposeAction {
            action: DockerComposeAction::Pull | DockerComposeAction::Up | DockerComposeAction::Down,
            ..
        }
        | DockerRequest::SystemPrune { .. } => LONG_TIMEOUT,
        DockerRequest::ContainerCreate { .. }
        | DockerRequest::ContainerAction { .. }
        | DockerRequest::ContainerRename { .. }
        | DockerRequest::ContainerRemove { .. }
        | DockerRequest::ImageTag { .. }
        | DockerRequest::ImageRemove { .. }
        | DockerRequest::NetworkCreate { .. }
        | DockerRequest::NetworkConnect { .. }
        | DockerRequest::NetworkDisconnect { .. }
        | DockerRequest::NetworkRemove { .. }
        | DockerRequest::VolumeCreate { .. }
        | DockerRequest::VolumeRemove { .. }
        | DockerRequest::ComposeAction { .. } => LIFECYCLE_TIMEOUT,
        _ => READ_TIMEOUT,
    }
}

struct DockerAuditMetadata {
    operation: &'static str,
    target: String,
    metadata: Value,
    mutation: bool,
}

fn audit_metadata(
    operation: &'static str,
    target: String,
    metadata: Value,
    mutation: bool,
) -> DockerAuditMetadata {
    DockerAuditMetadata {
        operation,
        target,
        metadata,
        mutation,
    }
}

fn safe_audit_target(value: &str) -> String {
    let value = value.trim();
    let valid = !value.is_empty()
        && value.len() <= 256
        && !value.contains("://")
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'/' | b'@')
        });
    if valid {
        value.to_string()
    } else if value.is_empty() {
        String::new()
    } else {
        "[redacted]".to_string()
    }
}

fn safe_image_audit_target(value: &str) -> String {
    let value = value.trim();
    if let Some(digest) = value.strip_prefix("sha256:")
        && digest.len() == 64
        && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return value.to_string();
    }
    "[image-reference]".to_string()
}

fn request_audit_metadata(request: &DockerRequest) -> DockerAuditMetadata {
    match request {
        DockerRequest::ContainerCreate { spec } => audit_metadata(
            "container_create",
            spec.name
                .as_deref()
                .map(safe_audit_target)
                .unwrap_or_default(),
            json!({
                "ports": spec.ports.len(),
                "mounts": spec.mounts.len(),
                "read_write_bind_confirmed": spec.confirm_bind_write,
            }),
            true,
        ),
        DockerRequest::ContainerAction {
            container, action, ..
        } => audit_metadata(
            match action {
                DockerContainerAction::Start => "container_start",
                DockerContainerAction::Stop => "container_stop",
                DockerContainerAction::Restart => "container_restart",
                DockerContainerAction::Kill => "container_kill",
                DockerContainerAction::Pause => "container_pause",
                DockerContainerAction::Unpause => "container_unpause",
            },
            safe_audit_target(container),
            json!({ "timeout_seconds": request_container_timeout(request) }),
            true,
        ),
        DockerRequest::ContainerRename { container, name } => audit_metadata(
            "container_rename",
            safe_audit_target(container),
            json!({ "new_name": safe_audit_target(name) }),
            true,
        ),
        DockerRequest::ContainerRemove {
            container,
            force,
            remove_volumes,
        } => audit_metadata(
            "container_remove",
            safe_audit_target(container),
            json!({ "force": force, "remove_volumes": remove_volumes }),
            true,
        ),
        DockerRequest::ImagePull { image } => audit_metadata(
            "image_pull",
            safe_image_audit_target(image),
            json!({}),
            true,
        ),
        DockerRequest::ImageTag { source, .. } => audit_metadata(
            "image_tag",
            safe_image_audit_target(source),
            json!({ "target": "[image-reference]" }),
            true,
        ),
        DockerRequest::ImageRemove { image, force } => audit_metadata(
            "image_remove",
            safe_image_audit_target(image),
            json!({ "force": force }),
            true,
        ),
        DockerRequest::ImagePrune => audit_metadata("image_prune", String::new(), json!({}), true),
        DockerRequest::NetworkCreate { spec } => audit_metadata(
            "network_create",
            safe_audit_target(&spec.name),
            json!({ "internal": spec.internal, "attachable": spec.attachable }),
            true,
        ),
        DockerRequest::NetworkConnect {
            network,
            container,
            aliases,
        } => audit_metadata(
            "network_connect",
            safe_audit_target(network),
            json!({
                "container": safe_audit_target(container),
                "alias_count": aliases.len(),
            }),
            true,
        ),
        DockerRequest::NetworkDisconnect {
            network,
            container,
            force,
        } => audit_metadata(
            "network_disconnect",
            safe_audit_target(network),
            json!({ "container": safe_audit_target(container), "force": force }),
            true,
        ),
        DockerRequest::NetworkRemove { network } => audit_metadata(
            "network_remove",
            safe_audit_target(network),
            json!({}),
            true,
        ),
        DockerRequest::NetworkPrune => {
            audit_metadata("network_prune", String::new(), json!({}), true)
        }
        DockerRequest::VolumeCreate { spec } => audit_metadata(
            "volume_create",
            safe_audit_target(&spec.name),
            json!({}),
            true,
        ),
        DockerRequest::VolumeRemove { volume, force } => audit_metadata(
            "volume_remove",
            safe_audit_target(volume),
            json!({ "force": force }),
            true,
        ),
        DockerRequest::VolumePrune => {
            audit_metadata("volume_prune", String::new(), json!({}), true)
        }
        DockerRequest::ComposeDeploy {
            target,
            config_digest,
            confirm_high_risk,
        } => audit_metadata(
            "compose_deploy",
            target
                .project
                .as_deref()
                .map(safe_audit_target)
                .unwrap_or_default(),
            json!({
                "config_digest": config_digest,
                "service_count": target.services.len(),
                "profile_count": target.profiles.len(),
                "high_risk_confirmed": confirm_high_risk,
            }),
            true,
        ),
        DockerRequest::ComposeAction {
            target,
            action,
            config_digest,
            remove_volumes,
            confirm_high_risk,
        } => audit_metadata(
            match action {
                DockerComposeAction::Pull => "compose_pull",
                DockerComposeAction::Up => "compose_up",
                DockerComposeAction::Start => "compose_start",
                DockerComposeAction::Stop => "compose_stop",
                DockerComposeAction::Restart => "compose_restart",
                DockerComposeAction::Down => "compose_down",
            },
            target
                .project
                .as_deref()
                .map(safe_audit_target)
                .unwrap_or_default(),
            json!({
                "config_digest": config_digest,
                "service_count": target.services.len(),
                "profile_count": target.profiles.len(),
                "remove_volumes": remove_volumes,
                "high_risk_confirmed": confirm_high_risk,
            }),
            true,
        ),
        DockerRequest::SystemPrune {
            containers,
            images,
            networks,
            volumes,
            all_images,
        } => audit_metadata(
            "system_prune",
            String::new(),
            json!({
                "containers": containers,
                "images": images,
                "networks": networks,
                "volumes": volumes,
                "all_images": all_images,
            }),
            true,
        ),
        DockerRequest::ContainerList { .. } => {
            audit_metadata("container_list", String::new(), json!({}), false)
        }
        DockerRequest::ContainerInspect { container } => audit_metadata(
            "container_inspect",
            safe_audit_target(container),
            json!({}),
            false,
        ),
        DockerRequest::ContainerStats { container } => audit_metadata(
            "container_stats",
            container
                .as_deref()
                .map(safe_audit_target)
                .unwrap_or_default(),
            json!({}),
            false,
        ),
        DockerRequest::ImageList { .. } => {
            audit_metadata("image_list", String::new(), json!({}), false)
        }
        DockerRequest::ImageInspect { image } => audit_metadata(
            "image_inspect",
            safe_image_audit_target(image),
            json!({}),
            false,
        ),
        DockerRequest::NetworkList => {
            audit_metadata("network_list", String::new(), json!({}), false)
        }
        DockerRequest::NetworkInspect { network } => audit_metadata(
            "network_inspect",
            safe_audit_target(network),
            json!({}),
            false,
        ),
        DockerRequest::VolumeList => audit_metadata("volume_list", String::new(), json!({}), false),
        DockerRequest::VolumeInspect { volume } => audit_metadata(
            "volume_inspect",
            safe_audit_target(volume),
            json!({}),
            false,
        ),
        DockerRequest::ComposeList => {
            audit_metadata("compose_list", String::new(), json!({}), false)
        }
        DockerRequest::ComposeInspect { target } => audit_metadata(
            "compose_inspect",
            target
                .project
                .as_deref()
                .map(safe_audit_target)
                .unwrap_or_default(),
            json!({}),
            false,
        ),
        DockerRequest::ComposeValidate { target } => audit_metadata(
            "compose_validate",
            target
                .project
                .as_deref()
                .map(safe_audit_target)
                .unwrap_or_default(),
            json!({}),
            false,
        ),
        DockerRequest::SystemDf => audit_metadata("system_df", String::new(), json!({}), false),
    }
}

fn request_container_timeout(request: &DockerRequest) -> Option<u64> {
    match request {
        DockerRequest::ContainerAction {
            timeout_seconds, ..
        } => *timeout_seconds,
        _ => None,
    }
}

async fn start_audit(
    state: &AppState,
    instance_id: &str,
    request_id: &str,
    actor: &str,
    operation: &str,
    target: &str,
    metadata: &Value,
) -> AppResult<String> {
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        r#"
        INSERT INTO docker_exec_sessions(
            id, instance_id, instance_snapshot, request_id, actor, operation,
            target, metadata, status, requested_at
        ) VALUES($1, $2, $2, $3, $4, $5, $6, $7, 'running', $8)
        "#,
    )
    .bind(&id)
    .bind(instance_id)
    .bind(request_id)
    .bind(actor)
    .bind(operation)
    .bind(limited_message(target))
    .bind(serde_json::to_string(metadata).unwrap_or_else(|_| "{}".to_string()))
    .bind(now_ts())
    .execute(&state.db)
    .await?;
    Ok(id)
}

async fn finish_audit(
    state: &AppState,
    id: &str,
    status: &str,
    error_code: Option<&str>,
) -> AppResult<()> {
    sqlx::query(
        r#"
        UPDATE docker_exec_sessions
        SET status = $1, error_code = $2, error_message = $3, completed_at = $4
        WHERE id = $5 AND status = 'running'
        "#,
    )
    .bind(status)
    .bind(error_code)
    .bind(error_code.map(audit_error_message).unwrap_or_default())
    .bind(now_ts())
    .bind(id)
    .execute(&state.db)
    .await?;
    Ok(())
}

async fn finish_audit_best_effort(
    state: &AppState,
    id: Option<&str>,
    status: &str,
    error_code: Option<&str>,
    _message: &str,
) {
    let Some(id) = id else {
        return;
    };
    if let Err(error) = finish_audit(state, id, status, error_code).await {
        warn!(?error, audit_id = %id, "failed to finalize Docker audit");
    }
}

fn audit_error_message(code: &str) -> &'static str {
    match code {
        "client_disconnected" => "客户端连接已断开",
        "offline" => "实例连接已断开",
        "disabled" => "实例已停用",
        "timeout" => "Docker 操作超时",
        "cancelled" => "Docker 操作已取消",
        "backpressure" => "浏览器读取速度不足，流已关闭",
        "exec_open_failed" => "容器终端启动失败",
        "exec_nonzero_exit" => "容器终端异常退出",
        "exec_failed" => "容器终端执行失败",
        "invalid_request" => "Docker 请求无效",
        "not_found" => "Docker 资源不存在",
        "permission_denied" => "Docker 权限不足",
        "not_installed" => "Docker 未安装",
        "daemon_unavailable" => "Docker daemon 不可用",
        "unsupported_version" => "Docker 版本不受支持",
        "busy" => "Docker Agent 正忙",
        "conflict" => "Docker 资源状态冲突",
        "output_too_large" => "Docker 输出超过限制",
        "command_failed" => "Docker 命令执行失败",
        "partial_failure" => "Docker 操作部分成功，部分阶段失败",
        "operation_failed" => "Docker 操作的所有阶段均失败",
        "unsupported" => "Docker 操作不受支持",
        "internal" => "Docker Agent 内部错误",
        _ => "Docker 操作失败",
    }
}

fn limited_message(value: &str) -> String {
    value.chars().take(1024).collect()
}

fn docker_error_code(code: &DockerErrorCode) -> &'static str {
    match code {
        DockerErrorCode::InvalidRequest => "invalid_request",
        DockerErrorCode::NotFound => "not_found",
        DockerErrorCode::PermissionDenied => "permission_denied",
        DockerErrorCode::NotInstalled => "not_installed",
        DockerErrorCode::DaemonUnavailable => "daemon_unavailable",
        DockerErrorCode::UnsupportedVersion => "unsupported_version",
        DockerErrorCode::Busy => "busy",
        DockerErrorCode::Conflict => "conflict",
        DockerErrorCode::Timeout => "timeout",
        DockerErrorCode::Cancelled => "cancelled",
        DockerErrorCode::OutputTooLarge => "output_too_large",
        DockerErrorCode::CommandFailed => "command_failed",
        DockerErrorCode::Unsupported => "unsupported",
        DockerErrorCode::Internal => "internal",
    }
}

fn map_docker_error(error: DockerError) -> AppError {
    let status = match error.code {
        DockerErrorCode::InvalidRequest => StatusCode::BAD_REQUEST,
        DockerErrorCode::NotFound => StatusCode::NOT_FOUND,
        DockerErrorCode::PermissionDenied => StatusCode::FORBIDDEN,
        DockerErrorCode::NotInstalled
        | DockerErrorCode::DaemonUnavailable
        | DockerErrorCode::UnsupportedVersion => StatusCode::SERVICE_UNAVAILABLE,
        DockerErrorCode::Busy | DockerErrorCode::Conflict | DockerErrorCode::Cancelled => {
            StatusCode::CONFLICT
        }
        DockerErrorCode::Timeout => StatusCode::GATEWAY_TIMEOUT,
        DockerErrorCode::Unsupported => StatusCode::NOT_IMPLEMENTED,
        DockerErrorCode::OutputTooLarge
        | DockerErrorCode::CommandFailed
        | DockerErrorCode::Internal => StatusCode::BAD_GATEWAY,
    };
    AppError::new(status, limited_message(&error.message))
}

fn unexpected_response() -> AppError {
    AppError::new(StatusCode::BAD_GATEWAY, "Agent 返回了非预期的 Docker 响应")
}

pub async fn store_docker_status(
    state: &AppState,
    instance_id: &str,
    status: &DockerStatus,
) -> AppResult<()> {
    sqlx::query(
        r#"
        INSERT INTO instance_docker_status(
            instance_id, status, cli_version, engine_version, api_version,
            compose_version, diagnostic, checked_at
        ) VALUES($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT(instance_id) DO UPDATE SET
            status = EXCLUDED.status,
            cli_version = EXCLUDED.cli_version,
            engine_version = EXCLUDED.engine_version,
            api_version = EXCLUDED.api_version,
            compose_version = EXCLUDED.compose_version,
            diagnostic = EXCLUDED.diagnostic,
            checked_at = EXCLUDED.checked_at
        WHERE EXCLUDED.checked_at >= instance_docker_status.checked_at
        "#,
    )
    .bind(instance_id)
    .bind(docker_status_state(&status.state))
    .bind(status.cli_version.as_deref())
    .bind(status.engine_version.as_deref())
    .bind(status.api_version.as_deref())
    .bind(status.compose_version.as_deref())
    .bind(limited_message(
        status.message.as_deref().unwrap_or_default(),
    ))
    .bind(status.checked_at)
    .execute(&state.db)
    .await?;
    Ok(())
}

pub async fn update_current_docker_status(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    status: Option<DockerStatus>,
) -> AppResult<()> {
    let agents = state.agents.read().await;
    let Some(agent) = agents
        .get(instance_id)
        .filter(|agent| agent.connection_id == connection_id)
    else {
        return Ok(());
    };
    let protocol_supported = agent
        .capabilities
        .iter()
        .any(|capability| capability == CAPABILITY);
    let status = protocol_supported.then_some(status).flatten();
    *agent.docker_status.write().await = status.clone();
    match docker_status_persistence(protocol_supported, status.is_some()) {
        DockerStatusPersistence::Store => {
            store_docker_status(
                state,
                instance_id,
                status.as_ref().expect("store requires a Docker status"),
            )
            .await?;
        }
        DockerStatusPersistence::Preserve => {}
        DockerStatusPersistence::Clear => {
            sqlx::query("DELETE FROM instance_docker_status WHERE instance_id = $1")
                .bind(instance_id)
                .execute(&state.db)
                .await?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DockerStatusPersistence {
    Store,
    Preserve,
    Clear,
}

fn docker_status_persistence(
    protocol_supported: bool,
    status_present: bool,
) -> DockerStatusPersistence {
    match (protocol_supported, status_present) {
        (true, true) => DockerStatusPersistence::Store,
        (true, false) => DockerStatusPersistence::Preserve,
        (false, _) => DockerStatusPersistence::Clear,
    }
}

fn docker_status_state(status: &DockerStatusState) -> &'static str {
    match status {
        DockerStatusState::NotInstalled => "not_installed",
        DockerStatusState::DaemonUnreachable => "daemon_unreachable",
        DockerStatusState::PermissionDenied => "permission_denied",
        DockerStatusState::UnsupportedVersion => "unsupported_version",
        DockerStatusState::Ready => "ready",
        DockerStatusState::Error => "error",
    }
}

pub async fn handle_docker_response(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    request_id: &str,
    response: DockerResponse,
) {
    let pending = {
        let mut requests = state.docker_requests.write().await;
        let matches = requests.get(request_id).is_some_and(|pending| {
            pending.instance_id == instance_id && pending.agent_connection_id == connection_id
        });
        matches.then(|| requests.remove(request_id)).flatten()
    };
    let Some(pending) = pending else {
        warn!(%instance_id, %connection_id, %request_id, "ignored unmatched Docker response");
        return;
    };
    let audit_outcome = docker_response_audit_outcome(&response);
    let audit_id = pending.audit_id.clone();
    let _ = pending.tx.send(Ok(response));
    finish_audit_best_effort(
        state,
        audit_id.as_deref(),
        audit_outcome.0,
        audit_outcome.1,
        "",
    )
    .await;
}

fn docker_response_audit_outcome(
    response: &DockerResponse,
) -> (&'static str, Option<&'static str>) {
    match response {
        DockerResponse::Error { error } => ("failed", Some(docker_error_code(&error.code))),
        DockerResponse::OperationComplete { data }
            if data.get("partial_success").and_then(Value::as_bool) == Some(true) =>
        {
            ("partial_success", Some("partial_failure"))
        }
        DockerResponse::OperationComplete { data }
            if data.get("completed").and_then(Value::as_bool) == Some(false) =>
        {
            ("failed", Some("operation_failed"))
        }
        _ => ("succeeded", None),
    }
}

pub async fn handle_docker_log_chunk(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    request_id: &str,
    sequence: u64,
    data: String,
    cursor: Option<String>,
) {
    let handle = state
        .docker_log_streams
        .read()
        .await
        .get(request_id)
        .filter(|handle| {
            handle.instance_id == instance_id && handle.agent_connection_id == connection_id
        })
        .cloned();
    let Some(handle) = handle else {
        warn!(%instance_id, %connection_id, %request_id, "ignored unmatched Docker log chunk");
        return;
    };
    if handle
        .tx
        .try_send(DockerLogEvent::Chunk {
            sequence,
            data,
            cursor,
        })
        .is_err()
    {
        let removed = {
            let mut streams = state.docker_log_streams.write().await;
            let matches = streams.get(request_id).is_some_and(|current| {
                current.instance_id == instance_id && current.agent_connection_id == connection_id
            });
            matches.then(|| streams.remove(request_id)).flatten()
        };
        if let Some(removed) = removed {
            removed
                .close_tx
                .send_replace(Some(DockerLogEvent::Backpressure));
        }
        if let Some(agent) = state
            .agents
            .read()
            .await
            .get(instance_id)
            .filter(|agent| agent.connection_id == connection_id)
            .cloned()
        {
            let _ = agent.tx.send(AgentOutbound::DockerLogCancel {
                request_id: request_id.to_string(),
            });
        }
    }
}

pub async fn handle_docker_log_closed(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    request_id: &str,
    error: Option<DockerError>,
) {
    let handle = {
        let mut streams = state.docker_log_streams.write().await;
        let matches = streams.get(request_id).is_some_and(|handle| {
            handle.instance_id == instance_id && handle.agent_connection_id == connection_id
        });
        matches.then(|| streams.remove(request_id)).flatten()
    };
    if let Some(handle) = handle {
        handle
            .close_tx
            .send_replace(Some(DockerLogEvent::Closed { error }));
    }
}

pub async fn handle_docker_exec_event(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    session_id: &str,
    event: TerminalServerMessage,
    remove: bool,
) {
    let handle = state
        .docker_exec_sessions
        .read()
        .await
        .get(session_id)
        .filter(|handle| {
            handle.instance_id == instance_id && handle.agent_connection_id == connection_id
        })
        .cloned();
    let Some(handle) = handle else {
        warn!(%instance_id, %connection_id, %session_id, "ignored unmatched Docker exec event");
        return;
    };
    if matches!(event, TerminalServerMessage::Ready) {
        handle.opened.store(true, Ordering::SeqCst);
    }
    if remove {
        let removed = {
            let mut sessions = state.docker_exec_sessions.write().await;
            let matches = sessions.get(session_id).is_some_and(|current| {
                current.instance_id == instance_id && current.agent_connection_id == connection_id
            });
            matches.then(|| sessions.remove(session_id)).flatten()
        };
        if let Some(removed) = removed {
            let (status, error_code) =
                exec_audit_outcome(removed.opened.load(Ordering::SeqCst), &event);
            removed.close_tx.send_replace(Some(event));
            finish_audit_best_effort(state, Some(&removed.audit_id), status, error_code, "").await;
        }
        return;
    }
    if handle.tx.try_send(event).is_err() {
        let removed = state.docker_exec_sessions.write().await.remove(session_id);
        if let Some(removed) = removed {
            removed
                .close_tx
                .send_replace(Some(TerminalServerMessage::Closed {
                    exit_code: None,
                    reason: Some("终端输出积压，连接已关闭".to_string()),
                }));
            finish_audit_best_effort(
                state,
                Some(&removed.audit_id),
                "failed",
                Some("backpressure"),
                "",
            )
            .await;
        }
        if let Some(agent) = state
            .agents
            .read()
            .await
            .get(instance_id)
            .filter(|agent| agent.connection_id == connection_id)
            .cloned()
        {
            let _ = agent.tx.send(AgentOutbound::DockerExecClose {
                session_id: session_id.to_string(),
            });
        }
    }
}

fn exec_audit_outcome(
    opened: bool,
    event: &TerminalServerMessage,
) -> (&'static str, Option<&'static str>) {
    let TerminalServerMessage::Closed { exit_code, reason } = event else {
        return ("failed", Some("exec_failed"));
    };
    if !opened {
        return ("failed", Some("exec_open_failed"));
    }
    if reason.as_deref().is_some_and(|reason| !reason.is_empty()) {
        return ("failed", Some("exec_failed"));
    }
    match exit_code {
        Some(0) => ("succeeded", None),
        Some(_) => ("failed", Some("exec_nonzero_exit")),
        None => ("failed", Some("exec_failed")),
    }
}

pub async fn close_connection_docker(state: &AppState, instance_id: &str, connection_id: Uuid) {
    let request_ids = state
        .docker_requests
        .read()
        .await
        .iter()
        .filter(|(_, pending)| {
            pending.instance_id == instance_id && pending.agent_connection_id == connection_id
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let pending = {
        let mut requests = state.docker_requests.write().await;
        request_ids
            .into_iter()
            .filter_map(|request_id| requests.remove(&request_id))
            .collect::<Vec<_>>()
    };
    for pending in pending {
        let audit_id = pending.audit_id.clone();
        let _ = pending.tx.send(Err(DockerRequestFailure::Disconnected));
        finish_audit_best_effort(state, audit_id.as_deref(), "failed", Some("offline"), "").await;
    }

    let log_ids = state
        .docker_log_streams
        .read()
        .await
        .iter()
        .filter(|(_, handle)| {
            handle.instance_id == instance_id && handle.agent_connection_id == connection_id
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let logs = {
        let mut streams = state.docker_log_streams.write().await;
        log_ids
            .into_iter()
            .filter_map(|request_id| streams.remove(&request_id))
            .collect::<Vec<_>>()
    };
    for handle in logs {
        handle
            .close_tx
            .send_replace(Some(DockerLogEvent::Disconnected));
    }

    let session_ids = state
        .docker_exec_sessions
        .read()
        .await
        .iter()
        .filter(|(_, handle)| {
            handle.instance_id == instance_id && handle.agent_connection_id == connection_id
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let sessions = {
        let mut sessions = state.docker_exec_sessions.write().await;
        session_ids
            .into_iter()
            .filter_map(|session_id| sessions.remove(&session_id))
            .collect::<Vec<_>>()
    };
    for handle in sessions {
        handle
            .close_tx
            .send_replace(Some(TerminalServerMessage::Closed {
                exit_code: None,
                reason: Some("实例连接已断开".to_string()),
            }));
        finish_audit_best_effort(state, Some(&handle.audit_id), "failed", Some("offline"), "")
            .await;
    }
}

pub async fn cancel_instance_docker(
    state: &AppState,
    instance_id: &str,
    agent: Option<&AgentHandle>,
) {
    let request_ids = state
        .docker_requests
        .read()
        .await
        .iter()
        .filter(|(_, pending)| pending.instance_id == instance_id)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let log_ids = state
        .docker_log_streams
        .read()
        .await
        .iter()
        .filter(|(_, handle)| handle.instance_id == instance_id)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let session_ids = state
        .docker_exec_sessions
        .read()
        .await
        .iter()
        .filter(|(_, handle)| handle.instance_id == instance_id)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();

    if let Some(agent) = agent {
        for request_id in &request_ids {
            let _ = agent.tx.send(AgentOutbound::DockerCancel {
                request_id: request_id.clone(),
            });
        }
        for request_id in &log_ids {
            let _ = agent.tx.send(AgentOutbound::DockerLogCancel {
                request_id: request_id.clone(),
            });
        }
        for session_id in &session_ids {
            let _ = agent.tx.send(AgentOutbound::DockerExecClose {
                session_id: session_id.clone(),
            });
        }
    }

    let pending = {
        let mut requests = state.docker_requests.write().await;
        request_ids
            .into_iter()
            .filter_map(|id| requests.remove(&id))
            .collect::<Vec<_>>()
    };
    for pending in pending {
        let audit_id = pending.audit_id.clone();
        let _ = pending.tx.send(Err(DockerRequestFailure::Disconnected));
        finish_audit_best_effort(state, audit_id.as_deref(), "failed", Some("cancelled"), "").await;
    }
    let logs = {
        let mut streams = state.docker_log_streams.write().await;
        log_ids
            .into_iter()
            .filter_map(|id| streams.remove(&id))
            .collect::<Vec<_>>()
    };
    for handle in logs {
        handle
            .close_tx
            .send_replace(Some(DockerLogEvent::Disconnected));
    }
    let sessions = {
        let mut sessions = state.docker_exec_sessions.write().await;
        session_ids
            .into_iter()
            .filter_map(|id| sessions.remove(&id))
            .collect::<Vec<_>>()
    };
    for handle in sessions {
        handle
            .close_tx
            .send_replace(Some(TerminalServerMessage::Closed {
                exit_code: None,
                reason: Some("实例已被管理员停用".to_string()),
            }));
        finish_audit_best_effort(
            state,
            Some(&handle.audit_id),
            "failed",
            Some("cancelled"),
            "",
        )
        .await;
    }
    state.docker_request_slots.lock().await.remove(instance_id);
    state.docker_stream_slots.lock().await.remove(instance_id);
}

async fn acquire_stream_slot(
    state: &AppState,
    instance_id: &str,
) -> AppResult<OwnedSemaphorePermit> {
    let slots = {
        let mut slots = state.docker_stream_slots.lock().await;
        slots
            .entry(instance_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(MAX_STREAMS_PER_INSTANCE)))
            .clone()
    };
    slots
        .try_acquire_owned()
        .map_err(|_| AppError::new(StatusCode::CONFLICT, "实例 Docker 日志或终端会话繁忙"))
}

async fn admin_container_logs_ws(
    State(state): State<AppState>,
    Path((instance_id, container)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<DockerLogsQuery>,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    ensure_same_origin(&headers, state.secure_cookies)?;
    let admin = require_admin(&state, &headers).await?;
    manageable_agent(&state, &instance_id).await?;
    let permit = acquire_stream_slot(&state, &instance_id).await?;
    let session_guard = admin.session_guard();
    Ok(ws.on_upgrade(move |socket| {
        docker_logs_socket(
            state,
            instance_id,
            container,
            session_guard,
            query,
            socket,
            permit,
        )
    }))
}

async fn docker_logs_socket(
    state: AppState,
    instance_id: String,
    container: String,
    session_guard: AdminSessionGuard,
    query: DockerLogsQuery,
    mut socket: WebSocket,
    _permit: OwnedSemaphorePermit,
) {
    let Some(agent) = state.agents.read().await.get(&instance_id).cloned() else {
        send_ws_event(
            &mut socket,
            &json!({"type": "error", "message": "实例不在线"}),
        )
        .await;
        return;
    };
    let request_id = Uuid::new_v4().to_string();
    let (tx, mut rx) = mpsc::channel(STREAM_BUFFER);
    let (close_tx, mut close_rx) = watch::channel(None);
    if !send_ws_event(&mut socket, &json!({"type": "opening"})).await {
        return;
    }
    let send_failed = {
        let agents = state.agents.read().await;
        let current = agents
            .get(&instance_id)
            .filter(|current| current.connection_id == agent.connection_id)
            .cloned();
        let Some(current) = current else {
            send_ws_event(
                &mut socket,
                &json!({"type": "error", "code": "offline", "message": "实例连接已断开"}),
            )
            .await;
            return;
        };
        state.docker_log_streams.write().await.insert(
            request_id.clone(),
            DockerLogStreamHandle {
                instance_id: instance_id.clone(),
                agent_connection_id: agent.connection_id,
                tx,
                close_tx,
            },
        );
        current
            .tx
            .send(AgentOutbound::DockerLogStart {
                request_id: request_id.clone(),
                container,
                tail: query.tail.clamp(1, 2000),
                follow: query.follow,
                since: query.since,
            })
            .is_err()
    };
    if send_failed {
        state.docker_log_streams.write().await.remove(&request_id);
        send_ws_event(
            &mut socket,
            &json!({"type": "error", "code": "offline", "message": "实例连接已断开"}),
        )
        .await;
        return;
    }
    if !send_ws_event(&mut socket, &json!({"type": "ready"})).await {
        state.docker_log_streams.write().await.remove(&request_id);
        let _ = agent.tx.send(AgentOutbound::DockerLogCancel { request_id });
        let _ = tokio::time::timeout(SOCKET_SEND_TIMEOUT, socket.close()).await;
        return;
    }
    let authorization = session_guard.wait_until_invalid(state.clone());
    tokio::pin!(authorization);

    loop {
        tokio::select! {
            _ = &mut authorization => {
                send_ws_event(
                    &mut socket,
                    &json!({"type": "closed", "reason": "authorization_revoked"}),
                )
                .await;
                break;
            }
            event = rx.recv() => {
                match event {
                    Some(DockerLogEvent::Chunk { sequence, data, cursor }) => {
                        if !send_ws_event(&mut socket, &json!({
                            "type": "chunk",
                            "encoding": "utf8",
                            "sequence": sequence,
                            "data": data,
                            "cursor": cursor,
                        })).await {
                            break;
                        }
                    }
                    Some(event @ DockerLogEvent::Closed { .. })
                    | Some(event @ DockerLogEvent::Disconnected)
                    | Some(event @ DockerLogEvent::Backpressure) => {
                        send_docker_log_terminal_event(&mut socket, &event).await;
                        break;
                    }
                    None => {
                        send_docker_log_terminal_event(
                            &mut socket,
                            &DockerLogEvent::Disconnected,
                        )
                        .await;
                        break;
                    }
                }
            }
            _ = close_rx.changed() => {
                while let Ok(event) = rx.try_recv() {
                    if let DockerLogEvent::Chunk { sequence, data, cursor } = event {
                        if !send_ws_event(&mut socket, &json!({
                            "type": "chunk",
                            "encoding": "utf8",
                            "sequence": sequence,
                            "data": data,
                            "cursor": cursor,
                        })).await {
                            break;
                        }
                    }
                }
                let event = close_rx
                    .borrow()
                    .clone()
                    .unwrap_or(DockerLogEvent::Disconnected);
                send_docker_log_terminal_event(&mut socket, &event).await;
                break;
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
    let removed = state.docker_log_streams.write().await.remove(&request_id);
    if removed.is_some()
        && let Some(agent) = state
            .agents
            .read()
            .await
            .get(&instance_id)
            .filter(|current| current.connection_id == agent.connection_id)
            .cloned()
    {
        let _ = agent.tx.send(AgentOutbound::DockerLogCancel { request_id });
    }
    let _ = tokio::time::timeout(SOCKET_SEND_TIMEOUT, socket.close()).await;
}

async fn send_docker_log_terminal_event(socket: &mut WebSocket, event: &DockerLogEvent) {
    send_ws_event(socket, &docker_log_terminal_message(event)).await;
}

fn docker_log_terminal_message(event: &DockerLogEvent) -> Value {
    match event {
        DockerLogEvent::Closed { error: Some(error) } => json!({
            "type": "error",
            "code": docker_error_code(&error.code),
            "retryable": error.retryable,
            "message": limited_message(&error.message),
        }),
        DockerLogEvent::Closed { error: None } => json!({"type": "closed"}),
        DockerLogEvent::Backpressure => json!({
            "type": "error",
            "code": "backpressure",
            "retryable": true,
            "message": "浏览器读取速度不足，日志流已暂停",
        }),
        DockerLogEvent::Disconnected | DockerLogEvent::Chunk { .. } => json!({
            "type": "closed",
            "retryable": true,
            "reason": "实例连接已断开",
        }),
    }
}

async fn admin_container_exec_ws(
    State(state): State<AppState>,
    Path((instance_id, container)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<DockerExecQuery>,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    ensure_same_origin(&headers, state.secure_cookies)?;
    let admin = require_admin(&state, &headers).await?;
    manageable_agent(&state, &instance_id).await?;
    if !matches!(query.shell.as_str(), "/bin/sh" | "/bin/bash" | "/bin/ash") {
        return Err(AppError::bad_request("不支持的容器终端 shell"));
    }
    let permit = acquire_stream_slot(&state, &instance_id).await?;
    let actor = admin.username.clone();
    let session_guard = admin.session_guard();
    Ok(ws.on_upgrade(move |socket| {
        docker_exec_socket(
            state,
            instance_id,
            container,
            actor,
            session_guard,
            query,
            socket,
            permit,
        )
    }))
}

async fn docker_exec_socket(
    state: AppState,
    instance_id: String,
    container: String,
    actor: String,
    session_guard: AdminSessionGuard,
    query: DockerExecQuery,
    socket: WebSocket,
    _permit: OwnedSemaphorePermit,
) {
    let Some(agent) = state.agents.read().await.get(&instance_id).cloned() else {
        let mut socket = socket;
        send_terminal_event(
            &mut socket,
            &TerminalServerMessage::Error {
                message: "实例不在线".to_string(),
            },
        )
        .await;
        return;
    };
    let session_id = Uuid::new_v4().to_string();
    let audit_id = match start_audit(
        &state,
        &instance_id,
        &session_id,
        &actor,
        "container_exec",
        &safe_audit_target(&container),
        &json!({ "shell": query.shell }),
    )
    .await
    {
        Ok(id) => id,
        Err(error) => {
            warn!(?error, %instance_id, "failed to create Docker exec audit");
            let mut socket = socket;
            send_terminal_event(
                &mut socket,
                &TerminalServerMessage::Error {
                    message: "无法创建容器终端审计记录".to_string(),
                },
            )
            .await;
            return;
        }
    };
    let (tx, mut rx) = mpsc::channel(STREAM_BUFFER);
    let (close_tx, mut close_rx) = watch::channel(None);
    let opened = Arc::new(AtomicBool::new(false));
    let (mut sender, mut receiver) = socket.split();
    if send_split_terminal_event(&mut sender, &TerminalServerMessage::Opening)
        .await
        .is_err()
    {
        finish_audit_best_effort(
            &state,
            Some(&audit_id),
            "failed",
            Some("client_disconnected"),
            "",
        )
        .await;
        return;
    }
    let current = {
        let agents = state.agents.read().await;
        agents
            .get(&instance_id)
            .filter(|current| current.connection_id == agent.connection_id)
            .cloned()
    };
    let Some(current) = current else {
        finish_audit_best_effort(&state, Some(&audit_id), "failed", Some("offline"), "").await;
        let _ = send_split_terminal_event(
            &mut sender,
            &TerminalServerMessage::Error {
                message: "实例连接已断开".to_string(),
            },
        )
        .await;
        let _ = tokio::time::timeout(SOCKET_SEND_TIMEOUT, sender.close()).await;
        return;
    };
    let send_failed = {
        state.docker_exec_sessions.write().await.insert(
            session_id.clone(),
            DockerExecSessionHandle {
                instance_id: instance_id.clone(),
                agent_connection_id: agent.connection_id,
                tx,
                close_tx,
                opened,
                audit_id: audit_id.clone(),
            },
        );
        current
            .tx
            .send(AgentOutbound::DockerExecOpen {
                session_id: session_id.clone(),
                container,
                shell: query.shell,
                cols: query.cols.clamp(20, 500),
                rows: query.rows.clamp(5, 300),
            })
            .is_err()
    };
    if send_failed {
        state.docker_exec_sessions.write().await.remove(&session_id);
        finish_audit_best_effort(&state, Some(&audit_id), "failed", Some("offline"), "").await;
        let _ = tokio::time::timeout(SOCKET_SEND_TIMEOUT, sender.close()).await;
        return;
    }
    let authorization = session_guard.wait_until_invalid(state.clone());
    tokio::pin!(authorization);
    let mut failure_code = "client_disconnected";

    loop {
        tokio::select! {
            _ = &mut authorization => {
                failure_code = "authorization_revoked";
                let _ = send_split_terminal_event(
                    &mut sender,
                    &TerminalServerMessage::Error {
                        message: "管理员会话已失效".to_string(),
                    },
                )
                .await;
                break;
            }
            event = rx.recv() => {
                let Some(event) = event else {
                    let final_event = close_rx.borrow().clone();
                    if let Some(event) = final_event {
                        let _ = send_split_terminal_event(&mut sender, &event).await;
                    }
                    break;
                };
                let terminal_closed = matches!(event, TerminalServerMessage::Closed { .. });
                if send_split_terminal_event(&mut sender, &event).await.is_err() {
                    break;
                }
                if terminal_closed {
                    break;
                }
            }
            _ = close_rx.changed() => {
                while let Ok(event) = rx.try_recv() {
                    if send_split_terminal_event(&mut sender, &event).await.is_err() {
                        break;
                    }
                }
                let final_event = close_rx.borrow().clone();
                if let Some(event) = final_event {
                    let _ = send_split_terminal_event(&mut sender, &event).await;
                }
                break;
            }
            incoming = receiver.next() => {
                let Some(incoming) = incoming else { break; };
                match incoming {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<TerminalClientMessage>(&text) {
                            Ok(TerminalClientMessage::Input { data }) if data.len() <= 64 * 1024 => {
                                if agent.tx.send(AgentOutbound::DockerExecInput {
                                    session_id: session_id.clone(),
                                    data,
                                }).is_err() {
                                    break;
                                }
                            }
                            Ok(TerminalClientMessage::Resize { cols, rows }) => {
                                let _ = agent.tx.send(AgentOutbound::DockerExecResize {
                                    session_id: session_id.clone(),
                                    cols: cols.clamp(20, 500),
                                    rows: rows.clamp(5, 300),
                                });
                            }
                            _ => {}
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
        }
    }
    let removed = state.docker_exec_sessions.write().await.remove(&session_id);
    if let Some(removed) = removed {
        if let Some(current) = state
            .agents
            .read()
            .await
            .get(&instance_id)
            .filter(|current| current.connection_id == agent.connection_id)
            .cloned()
        {
            let _ = current.tx.send(AgentOutbound::DockerExecClose {
                session_id: session_id.clone(),
            });
        }
        finish_audit_best_effort(
            &state,
            Some(&removed.audit_id),
            "failed",
            Some(failure_code),
            "",
        )
        .await;
    }
    let _ = tokio::time::timeout(SOCKET_SEND_TIMEOUT, sender.close()).await;
}

async fn send_ws_event(socket: &mut WebSocket, event: &Value) -> bool {
    socket_send_with_timeout(socket.send(Message::Text(event.to_string().into())))
        .await
        .is_ok()
}

async fn send_terminal_event(socket: &mut WebSocket, event: &TerminalServerMessage) {
    if let Ok(text) = serde_json::to_string(event) {
        let _ = socket_send_with_timeout(socket.send(Message::Text(text.into()))).await;
    }
    let _ = tokio::time::timeout(SOCKET_SEND_TIMEOUT, socket.close()).await;
}

async fn send_split_terminal_event(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    event: &TerminalServerMessage,
) -> Result<(), ()> {
    let text = serde_json::to_string(event)
        .unwrap_or_else(|_| r#"{"type":"error","message":"终端消息序列化失败"}"#.to_string());
    socket_send_with_timeout(sender.send(Message::Text(text.into()))).await
}

async fn socket_send_with_timeout<F, E>(send: F) -> Result<(), ()>
where
    F: Future<Output = Result<(), E>>,
{
    send_with_timeout(SOCKET_SEND_TIMEOUT, send).await
}

async fn send_with_timeout<F, E>(timeout: Duration, send: F) -> Result<(), ()>
where
    F: Future<Output = Result<(), E>>,
{
    tokio::time::timeout(timeout, send)
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, path::PathBuf};

    use axum::http::header;
    use sqlx::postgres::PgPoolOptions;

    use super::*;
    use crate::{
        auth::AuthCipher,
        config::Cli,
        db::init_db,
        models::{
            DockerComposeConfigSummary, DockerComposeServiceSummary, DockerPortProtocol,
            DockerStatusState,
        },
    };

    fn origin_headers(host: &str, origin: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.parse().expect("valid Host header"));
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, origin.parse().expect("valid Origin header"));
        }
        headers
    }

    #[test]
    fn docker_websockets_require_an_origin() {
        let error = ensure_same_origin(&origin_headers("console.example.com", None), true)
            .expect_err("missing Origin must be rejected before authentication");
        assert_eq!(error.status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn docker_websockets_reject_wrong_scheme_or_host() {
        let wrong_scheme =
            origin_headers("console.example.com", Some("http://console.example.com"));
        assert!(ensure_same_origin(&wrong_scheme, true).is_err());

        let wrong_host = origin_headers("console.example.com", Some("https://evil.example"));
        assert!(ensure_same_origin(&wrong_host, true).is_err());
    }

    #[test]
    fn docker_websockets_accept_the_configured_same_origin() {
        let secure = origin_headers(
            "console.example.com:13500",
            Some("https://console.example.com:13500"),
        );
        assert!(ensure_same_origin(&secure, true).is_ok());

        let insecure = origin_headers("127.0.0.1:13500", Some("http://127.0.0.1:13500"));
        assert!(ensure_same_origin(&insecure, false).is_ok());
    }

    #[tokio::test]
    async fn docker_browser_sends_are_bounded_by_a_timeout() {
        let pending_send = std::future::pending::<Result<(), ()>>();
        assert!(
            send_with_timeout(Duration::from_millis(1), pending_send)
                .await
                .is_err()
        );
        assert!(
            send_with_timeout(
                Duration::from_millis(1),
                std::future::ready(Ok::<_, ()>(()))
            )
            .await
            .is_ok()
        );
    }

    #[test]
    fn docker_wire_messages_match_the_agent_shape() {
        let request = AgentOutbound::DockerRequest {
            request_id: "request-1".to_string(),
            request: DockerRequest::ContainerAction {
                container: "web".to_string(),
                action: DockerContainerAction::Restart,
                timeout_seconds: Some(30),
            },
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({
                "type": "docker_request",
                "request_id": "request-1",
                "request": {
                    "operation": "container_action",
                    "container": "web",
                    "action": "restart",
                    "timeout_seconds": 30
                }
            })
        );

        let status = DockerStatus {
            state: DockerStatusState::Ready,
            cli_version: Some("27.0.1".to_string()),
            engine_version: Some("27.0.1".to_string()),
            api_version: Some("1.46".to_string()),
            compose_version: None,
            message: None,
            checked_at: 123,
        };
        assert_eq!(serde_json::to_value(status).unwrap()["state"], "ready");

        let digest = format!("sha256:{}", "a".repeat(64));
        let request = DockerRequest::ComposeAction {
            target: DockerComposeTarget {
                project: Some("demo".to_string()),
                files: vec!["/srv/demo/compose.yml".to_string()],
                profiles: vec!["worker".to_string()],
                services: vec!["web".to_string()],
            },
            action: DockerComposeAction::Up,
            config_digest: digest.clone(),
            remove_volumes: false,
            confirm_high_risk: true,
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({
                "operation": "compose_action",
                "target": {
                    "project": "demo",
                    "files": ["/srv/demo/compose.yml"],
                    "profiles": ["worker"],
                    "services": ["web"]
                },
                "action": "up",
                "config_digest": digest,
                "remove_volumes": false,
                "confirm_high_risk": true
            })
        );

        let validation = DockerComposeValidation {
            project: Some("demo".to_string()),
            services: vec!["web".to_string()],
            service_summaries: vec![DockerComposeServiceSummary {
                name: "web".to_string(),
                image: Some("nginx:alpine".to_string()),
                ports: vec!["8080:80/tcp".to_string()],
                mounts: vec!["data:/var/lib/data".to_string()],
                networks: vec!["default".to_string()],
                profiles: Vec::new(),
            }],
            config_summary: DockerComposeConfigSummary {
                service_count: 1,
                network_count: 1,
                volume_count: 1,
                config_count: 0,
                secret_count: 0,
            },
            warnings: Vec::new(),
            config_digest: format!("sha256:{}", "b".repeat(64)),
        };
        let encoded = serde_json::to_value(DockerResponse::ComposeValidation { validation })
            .expect("serialize Compose validation");
        assert_eq!(encoded["result"], "compose_validation");
        assert_eq!(encoded["validation"]["config_summary"]["service_count"], 1);
        assert_eq!(encoded["validation"]["service_summaries"][0]["name"], "web");
    }

    #[test]
    fn frontend_container_input_maps_to_the_agent_whitelist() {
        let payload: CreateContainerRequest = serde_json::from_value(json!({
            "name": "web",
            "image": "nginx:alpine",
            "command": ["nginx", "-g", "daemon off;"],
            "environment": ["MODE=production"],
            "ports": [{
                "host_ip": null,
                "host_port": 8080,
                "container_port": 80,
                "protocol": "tcp"
            }],
            "volumes": [{"name": "data", "target": "/data", "readonly": true}],
            "bind_mounts": [],
            "network": null,
            "restart_policy": "unless-stopped",
            "cpus": 1.5,
            "memory_bytes": 268435456,
            "confirm_read_write_bind_mounts": false
        }))
        .unwrap();
        let spec = create_container_spec(payload).unwrap();
        assert_eq!(
            spec.environment.get("MODE").map(String::as_str),
            Some("production")
        );
        assert_eq!(
            spec.restart_policy,
            Some(DockerRestartPolicy::UnlessStopped)
        );
        assert_eq!(spec.ports[0].protocol, DockerPortProtocol::Tcp);
        assert_eq!(spec.mounts[0].kind, DockerMountKind::Volume);
    }

    #[test]
    fn compose_paths_are_bounded() {
        assert!(validate_compose_files(&[]).is_err());
        assert!(validate_compose_files(&vec!["compose.yml".to_string(); 8]).is_ok());
        assert!(validate_compose_files(&vec!["compose.yml".to_string(); 9]).is_err());
    }

    #[test]
    fn compose_action_digest_policy_requires_preview_only_for_up() {
        let digest = format!("sha256:{}", "c".repeat(64));
        assert_eq!(
            supplied_compose_action_digest(DockerComposeAction::Up, Some(&digest)).unwrap(),
            Some(digest)
        );
        assert!(supplied_compose_action_digest(DockerComposeAction::Up, None).is_err());
        assert!(
            supplied_compose_action_digest(DockerComposeAction::Up, Some("sha256:bad")).is_err()
        );
        assert_eq!(
            supplied_compose_action_digest(
                DockerComposeAction::Stop,
                Some("client-digest-is-ignored")
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn docker_timeouts_match_operation_classes() {
        assert_eq!(
            request_timeout(&DockerRequest::ContainerList { all: true }),
            READ_TIMEOUT
        );
        assert_eq!(
            request_timeout(&DockerRequest::ContainerAction {
                container: "web".to_string(),
                action: DockerContainerAction::Restart,
                timeout_seconds: None,
            }),
            LIFECYCLE_TIMEOUT
        );
        assert_eq!(
            request_timeout(&DockerRequest::ImagePull {
                image: "nginx:alpine".to_string(),
            }),
            LONG_TIMEOUT
        );
        assert_eq!(
            request_timeout(&DockerRequest::ComposeAction {
                target: DockerComposeTarget {
                    project: Some("demo".to_string()),
                    files: Vec::new(),
                    profiles: Vec::new(),
                    services: Vec::new(),
                },
                action: DockerComposeAction::Down,
                config_digest: format!("sha256:{}", "d".repeat(64)),
                remove_volumes: true,
                confirm_high_risk: true,
            }),
            LONG_TIMEOUT
        );
    }

    #[test]
    fn docker_audit_metadata_excludes_secrets_and_records_destructive_flags() {
        let payload: CreateContainerRequest = serde_json::from_value(json!({
            "name": "web",
            "image": "registry.example.invalid/private/app:latest",
            "environment": ["DATABASE_PASSWORD=super-secret"],
            "ports": [],
            "volumes": [],
            "bind_mounts": [],
            "confirm_read_write_bind_mounts": false
        }))
        .unwrap();
        let create = request_audit_metadata(&DockerRequest::ContainerCreate {
            spec: create_container_spec(payload).unwrap(),
        });
        let audit_text = format!("{} {}", create.target, create.metadata);
        assert_eq!(create.operation, "container_create");
        assert!(!audit_text.contains("super-secret"));
        assert!(!audit_text.contains("DATABASE_PASSWORD"));
        assert!(!audit_text.contains("registry.example.invalid"));

        let remove = request_audit_metadata(&DockerRequest::ContainerRemove {
            container: "web".to_string(),
            force: true,
            remove_volumes: true,
        });
        assert_eq!(remove.metadata["force"], true);
        assert_eq!(remove.metadata["remove_volumes"], true);

        let pull = request_audit_metadata(&DockerRequest::ImagePull {
            image: "registry.example.invalid/private/app:latest".to_string(),
        });
        assert_eq!(pull.target, "[image-reference]");
        assert_eq!(audit_error_message("command_failed"), "Docker 命令执行失败");
        assert_eq!(
            audit_error_message("partial_failure"),
            "Docker 操作部分成功，部分阶段失败"
        );
        assert_eq!(
            audit_error_message("operation_failed"),
            "Docker 操作的所有阶段均失败"
        );

        let read = request_audit_metadata(&DockerRequest::ContainerList { all: true });
        assert!(!read.mutation);
    }

    #[test]
    fn docker_operation_audit_distinguishes_success_partial_and_failure() {
        let succeeded = DockerResponse::OperationComplete {
            data: json!({"completed": true}),
        };
        assert_eq!(
            docker_response_audit_outcome(&succeeded),
            ("succeeded", None)
        );

        let partial = DockerResponse::OperationComplete {
            data: json!({
                "completed": false,
                "partial_success": true,
                "succeeded_stages": 2,
                "failed_stages": 1,
            }),
        };
        assert_eq!(
            docker_response_audit_outcome(&partial),
            ("partial_success", Some("partial_failure"))
        );

        let failed = DockerResponse::OperationComplete {
            data: json!({
                "completed": false,
                "partial_success": false,
                "succeeded_stages": 0,
                "failed_stages": 3,
            }),
        };
        assert_eq!(
            docker_response_audit_outcome(&failed),
            ("failed", Some("operation_failed"))
        );
    }

    #[test]
    fn docker_log_disconnect_is_retryable_but_normal_completion_is_not() {
        let disconnected = docker_log_terminal_message(&DockerLogEvent::Disconnected);
        assert_eq!(disconnected["type"], "closed");
        assert_eq!(disconnected["retryable"], true);

        let completed = docker_log_terminal_message(&DockerLogEvent::Closed { error: None });
        assert_eq!(completed["type"], "closed");
        assert!(completed.get("retryable").is_none());
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn offline_mutation_is_failed_audit_but_reads_and_missing_instances_are_not_audited() {
        let db = PgPoolOptions::new()
            .max_connections(1)
            .connect("postgresql://localhost/postgres")
            .await
            .expect("connect database");
        init_db(&db).await.expect("initialize database");
        let state = AppState::new(
            db,
            Cli {
                bind: "127.0.0.1:0".parse::<SocketAddr>().expect("bind address"),
                database_url: "postgresql://localhost/postgres".to_string(),
                database_password: None,
                admin_password: Some("test-password-value".to_string()),
                auth_secret_key: None,
                auth_key_file: PathBuf::from("unused-test-auth-key"),
                secure_cookies: false,
                trust_proxy_headers: false,
                trusted_proxy_cidrs: Vec::new(),
                allow_legacy_agent_ws_auth: false,
                reset_admin_auth: false,
                confirm_reset_admin_auth: None,
                upload_dir: PathBuf::from("unused-uploads"),
                update_dir: PathBuf::from("unused-updates"),
                agent_package_max_bytes: 1024,
                file_transfer_max_bytes: 1024,
            },
            AuthCipher::from_key(&[5_u8; 32]).expect("create auth cipher"),
        );
        let suffix = Uuid::new_v4();
        let instance_id = format!("docker-audit-{suffix}");
        let missing_id = format!("missing-docker-audit-{suffix}");
        let actor = format!("docker-audit-actor-{suffix}");
        sqlx::query(
            "INSERT INTO instances(id, secret, name, first_seen) VALUES($1, 'secret', 'Docker audit test', $2)",
        )
        .bind(&instance_id)
        .bind(now_ts())
        .execute(&state.db)
        .await
        .expect("insert test instance");

        let error = execute_request(
            &state,
            &instance_id,
            &actor,
            DockerRequest::ContainerAction {
                container: "web".to_string(),
                action: DockerContainerAction::Start,
                timeout_seconds: None,
            },
        )
        .await
        .expect_err("offline mutation must fail");
        assert_eq!(error.status, StatusCode::CONFLICT);
        let audit: (String, Option<String>) =
            sqlx::query_as("SELECT status, error_code FROM docker_exec_sessions WHERE actor = $1")
                .bind(&actor)
                .fetch_one(&state.db)
                .await
                .expect("load failed audit");
        assert_eq!(audit.0, "failed");
        assert_eq!(audit.1.as_deref(), Some("offline"));

        execute_request(
            &state,
            &instance_id,
            &actor,
            DockerRequest::ContainerList { all: true },
        )
        .await
        .expect_err("offline read must fail without an audit");
        let missing_error = execute_request(
            &state,
            &missing_id,
            &actor,
            DockerRequest::ContainerAction {
                container: "web".to_string(),
                action: DockerContainerAction::Start,
                timeout_seconds: None,
            },
        )
        .await
        .expect_err("missing instance must retain the 404 contract");
        assert_eq!(missing_error.status, StatusCode::NOT_FOUND);
        let audit_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM docker_exec_sessions WHERE actor = $1")
                .bind(&actor)
                .fetch_one(&state.db)
                .await
                .expect("count audits");
        assert_eq!(audit_count, 1);

        sqlx::query("DELETE FROM docker_exec_sessions WHERE actor = $1")
            .bind(&actor)
            .execute(&state.db)
            .await
            .expect("delete test audits");
        sqlx::query("DELETE FROM instances WHERE id = $1")
            .bind(&instance_id)
            .execute(&state.db)
            .await
            .expect("delete test instance");
        state.db.close().await;
    }

    #[test]
    fn docker_exec_audit_distinguishes_open_and_exit_outcomes() {
        assert_eq!(
            exec_audit_outcome(
                false,
                &TerminalServerMessage::Closed {
                    exit_code: None,
                    reason: Some("permission denied".to_string()),
                }
            ),
            ("failed", Some("exec_open_failed"))
        );
        assert_eq!(
            exec_audit_outcome(
                true,
                &TerminalServerMessage::Closed {
                    exit_code: Some(0),
                    reason: None,
                }
            ),
            ("succeeded", None)
        );
        assert_eq!(
            exec_audit_outcome(
                true,
                &TerminalServerMessage::Closed {
                    exit_code: Some(7),
                    reason: None,
                }
            ),
            ("failed", Some("exec_nonzero_exit"))
        );
        assert_eq!(
            exec_audit_outcome(
                true,
                &TerminalServerMessage::Closed {
                    exit_code: Some(0),
                    reason: Some("daemon closed the stream".to_string()),
                }
            ),
            ("failed", Some("exec_failed"))
        );
    }

    #[test]
    fn stale_persisted_status_is_never_used_for_an_online_old_agent() {
        let ready = DockerStatus {
            state: DockerStatusState::Ready,
            cli_version: Some("27.0.1".to_string()),
            engine_version: Some("27.0.1".to_string()),
            api_version: Some("1.46".to_string()),
            compose_version: Some("2.29.1".to_string()),
            message: None,
            checked_at: 123,
        };
        let status = online_docker_status_response(false, Some(&ready));
        assert_eq!(status.status, "unknown");
        assert!(!status.protocol_supported);
        assert!(!status.installed);
        assert!(!status.manageable);
        assert!(status.cli_version.is_none());

        let pending_probe = online_docker_status_response(true, None);
        assert_eq!(pending_probe.status, "unknown");
        assert!(pending_probe.protocol_supported);
        assert!(!pending_probe.installed);
        assert!(!pending_probe.manageable);
    }

    #[test]
    fn missing_probe_preserves_persistence_only_for_capable_agents() {
        assert_eq!(
            docker_status_persistence(true, true),
            DockerStatusPersistence::Store
        );
        assert_eq!(
            docker_status_persistence(true, false),
            DockerStatusPersistence::Preserve
        );
        assert_eq!(
            docker_status_persistence(false, false),
            DockerStatusPersistence::Clear
        );
        assert_eq!(
            docker_status_persistence(false, true),
            DockerStatusPersistence::Clear
        );
    }

    #[test]
    fn offline_response_retains_the_last_persisted_probe() {
        let row = DockerStatusRow {
            status: "daemon_unreachable".to_string(),
            cli_version: Some("27.0.1".to_string()),
            engine_version: None,
            api_version: None,
            compose_version: Some("2.29.1".to_string()),
            diagnostic: "daemon is unavailable".to_string(),
            checked_at: 456,
        };
        let status = offline_docker_status_response(Some(&row));
        assert_eq!(status.status, "daemon_unreachable");
        assert!(status.installed);
        assert!(!status.online);
        assert!(!status.protocol_supported);
        assert!(!status.manageable);
        assert_eq!(status.cli_version.as_deref(), Some("27.0.1"));
        assert_eq!(status.checked_at, Some(456));
    }

    #[test]
    fn cleared_docker_status_stays_unknown_after_disconnect() {
        let status = offline_docker_status_response(None);
        assert_eq!(status.status, "unknown");
        assert!(!status.online);
        assert!(!status.protocol_supported);
        assert!(!status.installed);
        assert!(!status.manageable);
        assert!(status.checked_at.is_none());
    }

    #[test]
    fn error_mapping_is_stable() {
        assert_eq!(
            map_docker_error(DockerError {
                code: DockerErrorCode::NotFound,
                message: "missing".to_string(),
                retryable: false,
                exit_code: Some(1),
            })
            .status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            map_docker_error(DockerError {
                code: DockerErrorCode::Timeout,
                message: "slow".to_string(),
                retryable: true,
                exit_code: None,
            })
            .status,
            StatusCode::GATEWAY_TIMEOUT
        );
    }
}
