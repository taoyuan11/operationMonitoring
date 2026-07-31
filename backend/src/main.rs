mod admin_auth;
mod auth;
mod config;
mod db;
mod docker;
mod error;
mod files;
mod handlers;
mod jobs;
mod models;
mod remote_desktop;
mod request_security;
mod state;
mod update_signature;
mod updates;
mod utils;
mod ws;

use admin_auth::{
    admin_login, admin_logout, admin_me, admin_users, auth_status, bootstrap_confirm,
    bootstrap_start, cancel_admin_enrollment, confirm_admin_enrollment, create_device_enrollment,
    create_user_enrollment, delete_admin_user, reset_admin_auth, revoke_authenticator_device,
    set_admin_user_enabled,
};
use auth::load_auth_cipher;
use axum::{
    Router,
    extract::{ConnectInfo, DefaultBodyLimit, Request, State},
    http::{HeaderMap, Method},
    middleware::{self, Next},
    response::Response,
    routing::{delete, get, patch, post, put},
};
use clap::Parser;
use config::{Cli, validate_bootstrap_password};
use db::{cleanup_loop, connect_db, init_db};
use error::AppResult;
use files::{
    admin_create_directory, admin_delete_file, admin_download_file, admin_file_roots,
    admin_list_files, admin_move_file, admin_upload_file,
};
use handlers::{
    admin_approve_instance, admin_commands, admin_create_command, admin_delete_background_image,
    admin_delete_instance, admin_device_profile, admin_disable_command, admin_disable_instance,
    admin_get_settings, admin_jobs, admin_logs, admin_pending_instances, admin_put_appearance,
    admin_put_settings, admin_reject_instance, admin_run_whitelist_command, admin_terminal_ws,
    admin_update_instance, admin_upload_background_image, agent_register, agent_report, agent_ws,
    health, public_appearance, public_device_profile, public_instances, public_metrics,
};
use jobs::command_timeout_loop;
use remote_desktop::{admin_desktop_ws, agent_desktop_ws};
use request_security::{RequestScheme, ensure_same_origin, request_scheme};
use state::AppState;
use std::{io::ErrorKind, path::Path};
use tower_http::{cors::CorsLayer, services::ServeDir};
use tracing::info;
use update_signature::load_update_signer;
use updates::{
    admin_agent_releases, admin_agent_update_attempts, admin_create_agent_release,
    admin_delete_agent_artifact, admin_delete_agent_release, admin_publish_agent_release,
    admin_retry_agent_update, admin_update_agent_release, admin_upload_agent_artifact,
    agent_download_artifact, agent_download_artifact_checksum, agent_update_manifest,
    update_timeout_loop,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "backend=info,tower_http=info".into()),
        )
        .init();

    let mut cli = Cli::parse();
    let bind = cli.bind;
    let package_body_limit = cli.agent_package_max_bytes.saturating_add(1024 * 1024);
    let file_body_limit = cli.file_transfer_max_bytes;
    prepare_storage_directories(&mut cli).await?;
    let db = connect_db(&cli.database_url, cli.database_password.as_deref()).await?;
    init_db(&db).await?;
    if cli.reset_admin_auth {
        if cli.confirm_reset_admin_auth.as_deref() != Some("RESET-ADMIN-AUTH") {
            anyhow::bail!(
                "--reset-admin-auth requires --confirm-reset-admin-auth RESET-ADMIN-AUTH"
            );
        }
        reset_admin_auth(&db).await?;
        info!(
            "administrator authentication has been reset; restart without reset flags to initialize it again"
        );
        return Ok(());
    }
    let initialized: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admin_users")
        .fetch_one(&db)
        .await?;
    if initialized == 0 {
        let bootstrap_password = cli.admin_password.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "OM_ADMIN_PASSWORD must be set before administrator authentication is initialized"
            )
        })?;
        validate_bootstrap_password(bootstrap_password)?;
    }
    if initialized > 0 && cli.auth_secret_key.is_none() && !cli.auth_key_file.exists() {
        anyhow::bail!(
            "authentication key file {} is missing; restore it from backup or run the explicit administrator authentication reset",
            cli.auth_key_file.display()
        );
    }
    let auth_cipher = load_auth_cipher(cli.auth_secret_key.as_deref(), &cli.auth_key_file)?;
    let update_signer = load_update_signer(
        cli.update_signing_key_file
            .as_deref()
            .filter(|path| !path.as_os_str().is_empty()),
        &cli.update_signing_key_id,
    )?;
    if let Some(signer) = update_signer.as_ref() {
        info!(
            key_id = signer.key_id(),
            public_key = signer.public_key_base64(),
            "agent update signing enabled"
        );
    }

    let upload_dir = cli.upload_dir.clone();
    let state = AppState::new(db, cli, auth_cipher, update_signer);
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/public/appearance", get(public_appearance))
        .route("/api/public/instances", get(public_instances))
        .route("/api/public/instances/{id}/metrics", get(public_metrics))
        .route(
            "/api/public/instances/{id}/device-profile",
            get(public_device_profile),
        )
        .route("/api/admin/auth/status", get(auth_status))
        .route("/api/admin/bootstrap/start", post(bootstrap_start))
        .route(
            "/api/admin/bootstrap/enrollments/{id}/confirm",
            post(bootstrap_confirm),
        )
        .route(
            "/api/admin/login",
            post(admin_login).layer(DefaultBodyLimit::max(4 * 1024)),
        )
        .route("/api/admin/logout", post(admin_logout))
        .route("/api/admin/me", get(admin_me))
        .route("/api/admin/users", get(admin_users))
        .route("/api/admin/users/enrollments", post(create_user_enrollment))
        .route(
            "/api/admin/users/{id}/device-enrollments",
            post(create_device_enrollment),
        )
        .route(
            "/api/admin/auth/enrollments/{id}/confirm",
            post(confirm_admin_enrollment),
        )
        .route(
            "/api/admin/auth/enrollments/{id}",
            delete(cancel_admin_enrollment),
        )
        .route(
            "/api/admin/users/{id}/enabled",
            patch(set_admin_user_enabled),
        )
        .route("/api/admin/users/{id}", delete(delete_admin_user))
        .route(
            "/api/admin/auth/devices/{id}",
            delete(revoke_authenticator_device),
        )
        .route("/api/admin/pending-instances", get(admin_pending_instances))
        .route(
            "/api/admin/pending-instances/{id}/approve",
            post(admin_approve_instance),
        )
        .route(
            "/api/admin/pending-instances/{id}/reject",
            post(admin_reject_instance),
        )
        .route(
            "/api/admin/instances/{id}",
            patch(admin_update_instance).delete(admin_delete_instance),
        )
        .route(
            "/api/admin/instances/{id}/disable",
            post(admin_disable_instance),
        )
        .route(
            "/api/admin/instances/{id}/device-profile",
            get(admin_device_profile),
        )
        .route(
            "/api/admin/settings",
            get(admin_get_settings).put(admin_put_settings),
        )
        .route(
            "/api/admin/settings/background-image",
            post(admin_upload_background_image).delete(admin_delete_background_image),
        )
        .route("/api/admin/settings/appearance", put(admin_put_appearance))
        .route(
            "/api/admin/commands",
            get(admin_commands).post(admin_create_command),
        )
        .route("/api/admin/commands/{id}", delete(admin_disable_command))
        .route(
            "/api/admin/instances/{id}/commands/{command_id}/run",
            post(admin_run_whitelist_command),
        )
        .route(
            "/api/admin/instances/{id}/terminal/ws",
            get(admin_terminal_ws),
        )
        .route(
            "/api/admin/instances/{id}/desktop/ws",
            get(admin_desktop_ws),
        )
        .nest("/api/admin/instances/{id}/docker", docker::router())
        .route(
            "/api/admin/instances/{id}/files/roots",
            get(admin_file_roots),
        )
        .route(
            "/api/admin/instances/{id}/files",
            get(admin_list_files)
                .patch(admin_move_file)
                .delete(admin_delete_file),
        )
        .route(
            "/api/admin/instances/{id}/directories",
            post(admin_create_directory),
        )
        .route(
            "/api/admin/instances/{id}/files/upload",
            put(admin_upload_file).layer(DefaultBodyLimit::max(file_body_limit)),
        )
        .route(
            "/api/admin/instances/{id}/files/download",
            get(admin_download_file),
        )
        .route("/api/admin/jobs", get(admin_jobs))
        .route("/api/admin/logs", get(admin_logs))
        .route(
            "/api/admin/agent-releases",
            get(admin_agent_releases).post(admin_create_agent_release),
        )
        .route(
            "/api/admin/agent-releases/{release_id}",
            patch(admin_update_agent_release).delete(admin_delete_agent_release),
        )
        .route(
            "/api/admin/agent-releases/{release_id}/artifacts",
            post(admin_upload_agent_artifact).layer(DefaultBodyLimit::max(package_body_limit)),
        )
        .route(
            "/api/admin/agent-releases/{release_id}/artifacts/{artifact_id}",
            delete(admin_delete_agent_artifact),
        )
        .route(
            "/api/admin/agent-releases/{release_id}/publish",
            post(admin_publish_agent_release),
        )
        .route(
            "/api/admin/agent-update-attempts",
            get(admin_agent_update_attempts),
        )
        .route(
            "/api/admin/agent-update-attempts/{attempt_id}/retry",
            post(admin_retry_agent_update),
        )
        .route(
            "/api/agent/register",
            post(agent_register).layer(DefaultBodyLimit::max(96 * 1024)),
        )
        .route(
            "/api/agent/report",
            post(agent_report).layer(DefaultBodyLimit::max(96 * 1024)),
        )
        .route("/api/agent/ws", get(agent_ws))
        .route("/api/agent/desktop/ws", get(agent_desktop_ws))
        .route("/api/agent/update/manifest", get(agent_update_manifest))
        .route(
            "/api/agent/update/artifacts/{artifact_id}/download",
            get(agent_download_artifact),
        )
        .route(
            "/api/agent/update/artifacts/{artifact_id}/checksum",
            get(agent_download_artifact_checksum),
        )
        .nest_service("/uploads", ServeDir::new(upload_dir))
        .with_state(state.clone())
        .layer(DefaultBodyLimit::max(6 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_admin_write_origin,
        ))
        .layer(CorsLayer::permissive());

    let cleanup_state = state.clone();
    tokio::spawn(async move {
        cleanup_loop(cleanup_state).await;
    });
    let update_timeout_state = state.clone();
    tokio::spawn(async move {
        update_timeout_loop(update_timeout_state).await;
    });
    let command_timeout_state = state.clone();
    tokio::spawn(async move {
        command_timeout_loop(command_timeout_state).await;
    });

    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!("backend listening on http://{}", bind);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

async fn enforce_admin_write_origin(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<std::net::SocketAddr>,
    request: Request,
    next: Next,
) -> AppResult<Response> {
    if admin_write_requires_origin(request.method(), request.uri().path()) {
        let scheme = request_scheme(&state, request.headers(), peer_addr.ip())?;
        ensure_admin_write_origin(
            request.method(),
            request.uri().path(),
            request.headers(),
            scheme,
        )?;
    }
    Ok(next.run(request).await)
}

fn admin_write_requires_origin(method: &Method, path: &str) -> bool {
    let safe_method = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    path.starts_with("/api/admin/") && !safe_method
}

fn ensure_admin_write_origin(
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    request_scheme: RequestScheme,
) -> AppResult<()> {
    if admin_write_requires_origin(method, path) {
        ensure_same_origin(headers, request_scheme)?;
    }
    Ok(())
}

async fn prepare_storage_directories(cli: &mut Cli) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(&cli.upload_dir).await?;
    tokio::fs::create_dir_all(&cli.update_dir).await?;

    let upload_dir = tokio::fs::canonicalize(&cli.upload_dir).await?;
    let update_dir = tokio::fs::canonicalize(&cli.update_dir).await?;
    if paths_overlap(&upload_dir, &update_dir) {
        anyhow::bail!(
            "OM_UPDATE_DIR must not equal, contain, or be contained by the public OM_UPLOAD_DIR"
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&update_dir, std::fs::Permissions::from_mode(0o700)).await?;
    }

    match tokio::fs::remove_dir_all(update_dir.join(".tmp")).await {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    cli.upload_dir = upload_dir;
    cli.update_dir = update_dir;
    Ok(())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

#[cfg(test)]
mod tests {
    use super::{ensure_admin_write_origin, paths_overlap};
    use axum::http::{HeaderMap, Method, header};
    use std::path::Path;

    use crate::request_security::RequestScheme;

    fn session_headers(host: &str, origin: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.parse().expect("valid Host header"));
        headers.insert(
            header::COOKIE,
            "om_session=test-session"
                .parse()
                .expect("valid Cookie header"),
        );
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, origin.parse().expect("valid Origin header"));
        }
        headers
    }

    #[test]
    fn private_update_storage_must_not_overlap_public_uploads() {
        assert!(paths_overlap(
            Path::new("/srv/uploads"),
            Path::new("/srv/uploads")
        ));
        assert!(paths_overlap(
            Path::new("/srv/uploads"),
            Path::new("/srv/uploads/updates")
        ));
        assert!(paths_overlap(
            Path::new("/srv/private"),
            Path::new("/srv/private/uploads")
        ));
        assert!(!paths_overlap(
            Path::new("/srv/uploads"),
            Path::new("/srv/updates")
        ));
    }

    #[test]
    fn authenticated_admin_writes_require_a_matching_origin() {
        let missing = session_headers("console.example.com", None);
        assert!(
            ensure_admin_write_origin(
                &Method::POST,
                "/api/admin/instances/agent/docker/containers/web/actions/start",
                &missing,
                RequestScheme::Https,
            )
            .is_err()
        );

        let cross_site =
            session_headers("console.example.com", Some("https://attacker.example.com"));
        assert!(
            ensure_admin_write_origin(
                &Method::POST,
                "/api/admin/instances/agent/docker/compose/projects/app/actions/down",
                &cross_site,
                RequestScheme::Https,
            )
            .is_err()
        );

        let same_origin = session_headers(
            "console.example.com:13500",
            Some("https://console.example.com:13500"),
        );
        assert!(
            ensure_admin_write_origin(
                &Method::DELETE,
                "/api/admin/instances/agent/docker/images/image",
                &same_origin,
                RequestScheme::Https,
            )
            .is_ok()
        );
    }

    #[test]
    fn origin_guard_protects_unauthenticated_admin_writes_too() {
        let session = session_headers("console.example.com", None);
        assert!(
            ensure_admin_write_origin(
                &Method::GET,
                "/api/admin/instances/agent/docker/containers",
                &session,
                RequestScheme::Https,
            )
            .is_ok()
        );

        assert!(
            ensure_admin_write_origin(
                &Method::POST,
                "/api/admin/login",
                &HeaderMap::new(),
                RequestScheme::Https,
            )
            .is_err()
        );
        let login = session_headers("console.example.com", Some("https://console.example.com"));
        assert!(
            ensure_admin_write_origin(
                &Method::POST,
                "/api/admin/login",
                &login,
                RequestScheme::Https,
            )
            .is_ok()
        );
        assert!(
            ensure_admin_write_origin(
                &Method::POST,
                "/api/agent/report",
                &session,
                RequestScheme::Https,
            )
            .is_ok()
        );
    }
}
