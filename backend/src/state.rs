use std::{collections::HashMap, path::PathBuf, sync::Arc};

use sqlx::PgPool;
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

use crate::{
    auth::AuthCipher,
    config::Cli,
    models::{AgentOutbound, FileResponse, TerminalServerMessage},
};

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub admin_password: String,
    pub auth_cipher: Arc<AuthCipher>,
    pub secure_cookies: bool,
    pub upload_dir: PathBuf,
    pub update_dir: PathBuf,
    pub agent_package_max_bytes: usize,
    pub file_transfer_max_bytes: usize,
    pub sessions: Arc<RwLock<HashMap<String, AdminSession>>>,
    pub auth_attempts: Arc<RwLock<HashMap<String, AuthAttempt>>>,
    pub agents: Arc<RwLock<HashMap<String, AgentHandle>>>,
    pub terminal_sessions: Arc<RwLock<HashMap<String, TerminalSessionHandle>>>,
    pub file_requests: Arc<RwLock<HashMap<String, PendingFileRequest>>>,
    pub active_file_transfers: Arc<RwLock<HashMap<String, String>>>,
}

impl AppState {
    pub fn new(db: PgPool, cli: Cli, auth_cipher: AuthCipher) -> Self {
        Self {
            db,
            admin_password: cli.admin_password,
            auth_cipher: Arc::new(auth_cipher),
            secure_cookies: cli.secure_cookies,
            upload_dir: cli.upload_dir,
            update_dir: cli.update_dir,
            agent_package_max_bytes: cli.agent_package_max_bytes,
            file_transfer_max_bytes: cli.file_transfer_max_bytes,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            auth_attempts: Arc::new(RwLock::new(HashMap::new())),
            agents: Arc::new(RwLock::new(HashMap::new())),
            terminal_sessions: Arc::new(RwLock::new(HashMap::new())),
            file_requests: Arc::new(RwLock::new(HashMap::new())),
            active_file_transfers: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[derive(Clone)]
pub struct AdminSession {
    pub user_id: String,
    pub username: String,
    pub device_id: String,
    pub expires_at: i64,
}

#[derive(Clone, Default)]
pub struct AuthAttempt {
    pub failures: u32,
    pub window_started_at: i64,
    pub blocked_until: i64,
}

#[derive(Clone)]
pub struct AgentHandle {
    pub connection_id: Uuid,
    pub tx: mpsc::UnboundedSender<AgentOutbound>,
    pub binary_tx: mpsc::Sender<Vec<u8>>,
    pub capabilities: Vec<String>,
}

#[derive(Clone)]
pub struct TerminalSessionHandle {
    pub instance_id: String,
    pub agent_connection_id: Uuid,
    pub tx: mpsc::UnboundedSender<TerminalServerMessage>,
}

#[derive(Debug)]
pub enum FileRequestEvent {
    Response(FileResponse),
    Chunk { sequence: u64, data: Vec<u8> },
    Disconnected,
}

#[derive(Clone)]
pub struct PendingFileRequest {
    pub instance_id: String,
    pub agent_connection_id: Uuid,
    pub tx: mpsc::Sender<FileRequestEvent>,
}
