use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

use ipnet::IpNet;
use sqlx::PgPool;
use tokio::sync::{
    Mutex, OwnedSemaphorePermit, RwLock, Semaphore, broadcast, mpsc, oneshot, watch,
};
use uuid::Uuid;

use crate::{
    auth::AuthCipher,
    config::Cli,
    models::{
        AgentOutbound, DockerError, DockerResponse, DockerStatus, FileResponse, RemoteAccessStatus,
        TerminalServerMessage, TerminalShellInfo,
    },
    update_signature::UpdateSigner,
};

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub admin_password: Option<String>,
    pub auth_cipher: Arc<AuthCipher>,
    pub secure_cookies: bool,
    pub trust_proxy_headers: bool,
    pub trusted_proxy_cidrs: Vec<IpNet>,
    pub allow_legacy_agent_ws_auth: bool,
    pub upload_dir: PathBuf,
    pub update_dir: PathBuf,
    pub update_signer: Option<Arc<UpdateSigner>>,
    pub agent_package_max_bytes: usize,
    pub file_transfer_max_bytes: usize,
    pub sessions: Arc<RwLock<HashMap<String, AdminSession>>>,
    pub auth_source_attempts: Arc<RwLock<HashMap<String, AuthAttempt>>>,
    pub auth_account_attempts: Arc<RwLock<HashMap<String, AuthAttempt>>>,
    pub agents: Arc<RwLock<HashMap<String, AgentHandle>>>,
    pub terminal_sessions: Arc<RwLock<HashMap<String, TerminalSessionHandle>>>,
    pub terminal_shell_requests: Arc<RwLock<HashMap<String, PendingTerminalShellRequest>>>,
    pub file_requests: Arc<RwLock<HashMap<String, PendingFileRequest>>>,
    pub active_file_transfers: Arc<RwLock<HashMap<String, String>>>,
    pub desktop_sessions: Arc<RwLock<HashMap<String, DesktopSessionHandle>>>,
    pub docker_requests: Arc<RwLock<HashMap<String, PendingDockerRequest>>>,
    pub docker_log_streams: Arc<RwLock<HashMap<String, DockerLogStreamHandle>>>,
    pub docker_exec_sessions: Arc<RwLock<HashMap<String, DockerExecSessionHandle>>>,
    pub docker_request_slots: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    pub docker_stream_slots: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
}

impl AppState {
    pub fn new(
        db: PgPool,
        cli: Cli,
        auth_cipher: AuthCipher,
        update_signer: Option<UpdateSigner>,
    ) -> Self {
        Self {
            db,
            admin_password: cli.admin_password,
            auth_cipher: Arc::new(auth_cipher),
            secure_cookies: cli.secure_cookies,
            trust_proxy_headers: cli.trust_proxy_headers,
            trusted_proxy_cidrs: cli.trusted_proxy_cidrs,
            allow_legacy_agent_ws_auth: cli.allow_legacy_agent_ws_auth,
            upload_dir: cli.upload_dir,
            update_dir: cli.update_dir,
            update_signer: update_signer.map(Arc::new),
            agent_package_max_bytes: cli.agent_package_max_bytes,
            file_transfer_max_bytes: cli.file_transfer_max_bytes,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            auth_source_attempts: Arc::new(RwLock::new(HashMap::new())),
            auth_account_attempts: Arc::new(RwLock::new(HashMap::new())),
            agents: Arc::new(RwLock::new(HashMap::new())),
            terminal_sessions: Arc::new(RwLock::new(HashMap::new())),
            terminal_shell_requests: Arc::new(RwLock::new(HashMap::new())),
            file_requests: Arc::new(RwLock::new(HashMap::new())),
            active_file_transfers: Arc::new(RwLock::new(HashMap::new())),
            desktop_sessions: Arc::new(RwLock::new(HashMap::new())),
            docker_requests: Arc::new(RwLock::new(HashMap::new())),
            docker_log_streams: Arc::new(RwLock::new(HashMap::new())),
            docker_exec_sessions: Arc::new(RwLock::new(HashMap::new())),
            docker_request_slots: Arc::new(Mutex::new(HashMap::new())),
            docker_stream_slots: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[derive(Clone)]
pub struct AdminSession {
    pub user_id: String,
    pub username: String,
    pub device_id: String,
    pub login_totp_counter: Option<i64>,
    pub expires_at: i64,
    pub revoked_tx: watch::Sender<bool>,
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
    pub tx: AgentOutboundSender,
    pub binary_tx: mpsc::Sender<Vec<u8>>,
    pub shutdown_tx: watch::Sender<bool>,
    pub capabilities: Vec<String>,
    pub docker_status: Arc<RwLock<Option<DockerStatus>>>,
    pub remote_access_status: Arc<RwLock<Option<RemoteAccessStatus>>>,
}

#[derive(Clone)]
pub struct AgentOutboundSender {
    tx: mpsc::Sender<AgentOutbound>,
    shutdown_tx: watch::Sender<bool>,
}

impl AgentOutboundSender {
    pub fn channel(
        capacity: usize,
        shutdown_tx: watch::Sender<bool>,
    ) -> (Self, mpsc::Receiver<AgentOutbound>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self { tx, shutdown_tx }, rx)
    }

    pub fn send(&self, message: AgentOutbound) -> Result<(), AgentOutboundSendError> {
        self.tx.try_send(message).map_err(|error| {
            self.shutdown_tx.send_replace(true);
            match error {
                mpsc::error::TrySendError::Full(_) => AgentOutboundSendError::Full,
                mpsc::error::TrySendError::Closed(_) => AgentOutboundSendError::Closed,
            }
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentOutboundSendError {
    Full,
    Closed,
}

#[derive(Clone)]
pub struct TerminalSessionHandle {
    pub instance_id: String,
    pub agent_connection_id: Uuid,
    pub tx: mpsc::Sender<TerminalServerMessage>,
}

#[derive(Debug)]
pub enum TerminalShellRequestFailure {
    Disconnected,
}

pub struct PendingTerminalShellRequest {
    pub instance_id: String,
    pub agent_connection_id: Uuid,
    pub tx: oneshot::Sender<Result<Vec<TerminalShellInfo>, TerminalShellRequestFailure>>,
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

#[derive(Clone)]
pub struct DesktopSessionHandle {
    pub instance_id: String,
    pub agent_connection_id: Uuid,
    pub token_hash: [u8; 32],
    pub token_expires_at: i64,
    pub token_claimed: bool,
    pub browser_tx: mpsc::Sender<String>,
    pub frame_tx: watch::Sender<Option<Arc<Vec<u8>>>>,
    pub audio_tx: broadcast::Sender<DesktopAudioPacket>,
    pub audio_codec: Option<String>,
    pub unattended_capable: bool,
    pub agent_input_rx: Arc<Mutex<Option<mpsc::Receiver<String>>>>,
    pub close_tx: watch::Sender<Option<String>>,
}

#[derive(Clone)]
pub struct DesktopAudioPacket {
    pub generation: u64,
    pub frame: Arc<Vec<u8>>,
}

#[derive(Debug)]
pub enum DockerRequestFailure {
    Disconnected,
}

pub struct PendingDockerRequest {
    pub instance_id: String,
    pub agent_connection_id: Uuid,
    pub tx: oneshot::Sender<Result<DockerResponse, DockerRequestFailure>>,
    pub audit_id: Option<String>,
    pub _permit: OwnedSemaphorePermit,
}

#[derive(Clone, Debug)]
pub enum DockerLogEvent {
    Chunk {
        sequence: u64,
        data: String,
        cursor: Option<String>,
    },
    Closed {
        error: Option<DockerError>,
    },
    Disconnected,
    Backpressure,
}

#[derive(Clone)]
pub struct DockerLogStreamHandle {
    pub instance_id: String,
    pub agent_connection_id: Uuid,
    pub tx: mpsc::Sender<DockerLogEvent>,
    pub close_tx: watch::Sender<Option<DockerLogEvent>>,
}

#[derive(Clone)]
pub struct DockerExecSessionHandle {
    pub instance_id: String,
    pub agent_connection_id: Uuid,
    pub tx: mpsc::Sender<TerminalServerMessage>,
    pub close_tx: watch::Sender<Option<TerminalServerMessage>>,
    pub opened: Arc<AtomicBool>,
    pub audit_id: String,
}
