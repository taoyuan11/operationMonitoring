use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use serde::Serialize;
use sqlx::FromRow;
use uuid::Uuid;

use crate::{
    auth::require_admin,
    db::get_instance,
    error::AppResult,
    models::{
        RemoteAccessDeviceStatus, RemoteAccessFallbackMode, RemoteAccessStatus,
        RemoteDesktopAccessMode,
    },
    state::AppState,
};

pub(crate) const STATUS_CAPABILITY: &str = "remote_access_status_v1";
const DESKTOP_CAPABILITY: &str = "remote_desktop_v1";

#[derive(Debug, FromRow)]
struct RemoteAccessStatusRow {
    status: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct RemoteAccessStatusResponse {
    protocol_supported: bool,
    status_supported: bool,
    online: bool,
    access_mode: Option<RemoteDesktopAccessMode>,
    fallback_mode: Option<RemoteAccessFallbackMode>,
    display: Option<RemoteAccessDeviceStatus>,
    audio: Option<RemoteAccessDeviceStatus>,
    reboot_required: Option<bool>,
    checked_at: Option<i64>,
}

pub async fn admin_remote_access_status(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
) -> AppResult<Json<RemoteAccessStatusResponse>> {
    require_admin(&state, &headers).await?;
    get_instance(&state.db, &instance_id).await?;
    Ok(Json(
        remote_access_status_response(&state, &instance_id).await?,
    ))
}

async fn remote_access_status_response(
    state: &AppState,
    instance_id: &str,
) -> AppResult<RemoteAccessStatusResponse> {
    if let Some(agent) = state.agents.read().await.get(instance_id).cloned() {
        let protocol_supported = has_capability(&agent.capabilities, DESKTOP_CAPABILITY);
        let status_supported = has_capability(&agent.capabilities, STATUS_CAPABILITY);
        let status = if status_supported {
            agent.remote_access_status.read().await.clone()
        } else {
            None
        };
        return Ok(build_status_response(
            protocol_supported,
            status_supported,
            true,
            status,
        ));
    }

    let row = sqlx::query_as::<_, RemoteAccessStatusRow>(
        "SELECT status FROM instance_remote_access_status WHERE instance_id = $1",
    )
    .bind(instance_id)
    .fetch_optional(&state.db)
    .await?;
    let capabilities: String = sqlx::query_scalar(
        "SELECT COALESCE(capabilities, '') FROM instance_agent_metadata WHERE instance_id = $1",
    )
    .bind(instance_id)
    .fetch_optional(&state.db)
    .await?
    .unwrap_or_default();
    let capabilities = capabilities
        .split(',')
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let protocol_supported = capabilities.contains(&DESKTOP_CAPABILITY);
    let status_supported = capabilities.contains(&STATUS_CAPABILITY);
    let status = if status_supported {
        row.map(|row| serde_json::from_str(&row.status))
            .transpose()
            .map_err(anyhow::Error::from)?
    } else {
        None
    };
    Ok(build_status_response(
        protocol_supported,
        status_supported,
        false,
        status,
    ))
}

fn build_status_response(
    protocol_supported: bool,
    status_supported: bool,
    online: bool,
    status: Option<RemoteAccessStatus>,
) -> RemoteAccessStatusResponse {
    RemoteAccessStatusResponse {
        protocol_supported,
        status_supported,
        online,
        access_mode: status.as_ref().map(|status| status.access_mode),
        fallback_mode: status.as_ref().map(|status| status.fallback_mode),
        display: status.as_ref().map(|status| status.display.clone()),
        audio: status.as_ref().map(|status| status.audio.clone()),
        reboot_required: status.as_ref().map(|status| status.reboot_required),
        checked_at: status.map(|status| status.checked_at),
    }
}

pub async fn update_current_remote_access_status(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
    status: Option<RemoteAccessStatus>,
) -> AppResult<()> {
    let agents = state.agents.read().await;
    let Some(agent) = agents
        .get(instance_id)
        .filter(|agent| agent.connection_id == connection_id)
    else {
        return Ok(());
    };
    let protocol_supported = has_capability(&agent.capabilities, STATUS_CAPABILITY);
    let status = protocol_supported.then_some(status).flatten();
    *agent.remote_access_status.write().await = status.clone();
    match status_persistence(protocol_supported, status.is_some()) {
        StatusPersistence::Store => {
            let status = status
                .as_ref()
                .expect("store requires a remote access status");
            let encoded = serde_json::to_string(status).map_err(anyhow::Error::from)?;
            sqlx::query(
                r#"
                INSERT INTO instance_remote_access_status(instance_id, status, checked_at)
                VALUES($1, $2, $3)
                ON CONFLICT(instance_id) DO UPDATE SET
                    status = EXCLUDED.status,
                    checked_at = EXCLUDED.checked_at
                WHERE EXCLUDED.checked_at >= instance_remote_access_status.checked_at
                "#,
            )
            .bind(instance_id)
            .bind(encoded)
            .bind(status.checked_at)
            .execute(&state.db)
            .await?;
        }
        StatusPersistence::Preserve => {}
        StatusPersistence::Clear => {
            sqlx::query("DELETE FROM instance_remote_access_status WHERE instance_id = $1")
                .bind(instance_id)
                .execute(&state.db)
                .await?;
        }
    }
    Ok(())
}

fn has_capability(capabilities: &[String], expected: &str) -> bool {
    capabilities.iter().any(|value| value == expected)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusPersistence {
    Store,
    Preserve,
    Clear,
}

fn status_persistence(protocol_supported: bool, status_present: bool) -> StatusPersistence {
    match (protocol_supported, status_present) {
        (true, true) => StatusPersistence::Store,
        (true, false) => StatusPersistence::Preserve,
        (false, _) => StatusPersistence::Clear,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        RemoteAccessAvailability, RemoteAccessDeviceSource, RemoteAccessDriverState,
    };

    fn test_status() -> RemoteAccessStatus {
        let device = RemoteAccessDeviceStatus {
            availability: RemoteAccessAvailability::Ready,
            source: RemoteAccessDeviceSource::Virtual,
            driver_state: RemoteAccessDriverState::Active,
            driver_version: Some("1.2.3".to_string()),
            code: None,
        };
        RemoteAccessStatus {
            access_mode: RemoteDesktopAccessMode::Unattended,
            fallback_mode: RemoteAccessFallbackMode::Auto,
            display: device.clone(),
            audio: device,
            reboot_required: false,
            checked_at: 123,
        }
    }

    #[test]
    fn response_keeps_security_status_admin_only_and_marks_online() {
        let response = build_status_response(true, true, true, Some(test_status()));
        assert!(response.online);
        assert_eq!(
            response.access_mode,
            Some(RemoteDesktopAccessMode::Unattended)
        );
        assert_eq!(
            response.display.unwrap().source,
            RemoteAccessDeviceSource::Virtual
        );
    }

    #[test]
    fn status_persistence_matches_capability_lifecycle() {
        assert_eq!(status_persistence(true, true), StatusPersistence::Store);
        assert_eq!(status_persistence(true, false), StatusPersistence::Preserve);
        assert_eq!(status_persistence(false, false), StatusPersistence::Clear);
        assert_eq!(status_persistence(false, true), StatusPersistence::Clear);
    }
}
