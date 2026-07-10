mod auth;
mod config;
mod db;
mod error;
mod handlers;
mod jobs;
mod models;
mod state;
mod utils;
mod ws;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{delete, get, patch, post},
};
use clap::Parser;
use config::Cli;
use db::{cleanup_loop, connect_db, init_db};
use handlers::{
    admin_approve_instance, admin_commands, admin_create_command, admin_delete_background_image,
    admin_delete_instance, admin_disable_command, admin_disable_instance, admin_get_settings,
    admin_jobs, admin_login, admin_logout, admin_logs, admin_me, admin_pending_instances,
    admin_put_settings, admin_reject_instance, admin_run_whitelist_command, admin_terminal_ws,
    admin_update_instance, admin_upload_background_image, agent_register, agent_report, agent_ws,
    health, public_appearance, public_instances, public_metrics,
};
use state::AppState;
use tower_http::{cors::CorsLayer, services::ServeDir};
use tracing::{info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "backend=info,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();
    if cli.admin_password == "admin123" {
        warn!("using default admin password; set OM_ADMIN_PASSWORD in production");
    }

    let bind = cli.bind;
    let db = connect_db(&cli.database_url).await?;
    init_db(&db).await?;

    let upload_dir = cli.upload_dir.clone();
    let state = AppState::new(db, cli);
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/public/appearance", get(public_appearance))
        .route("/api/public/instances", get(public_instances))
        .route("/api/public/instances/{id}/metrics", get(public_metrics))
        .route("/api/admin/login", post(admin_login))
        .route("/api/admin/logout", post(admin_logout))
        .route("/api/admin/me", get(admin_me))
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
            "/api/admin/settings",
            get(admin_get_settings).put(admin_put_settings),
        )
        .route(
            "/api/admin/settings/background-image",
            post(admin_upload_background_image).delete(admin_delete_background_image),
        )
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
        .route("/api/admin/jobs", get(admin_jobs))
        .route("/api/admin/logs", get(admin_logs))
        .route("/api/agent/register", post(agent_register))
        .route("/api/agent/report", post(agent_report))
        .route("/api/agent/ws", get(agent_ws))
        .nest_service("/uploads", ServeDir::new(upload_dir))
        .with_state(state.clone())
        .layer(DefaultBodyLimit::max(6 * 1024 * 1024))
        .layer(CorsLayer::permissive());

    let cleanup_state = state.clone();
    tokio::spawn(async move {
        cleanup_loop(cleanup_state).await;
    });

    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!("backend listening on http://{}", bind);
    axum::serve(listener, app).await?;
    Ok(())
}
