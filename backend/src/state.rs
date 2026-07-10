use std::{collections::HashMap, path::PathBuf, sync::Arc};

use sqlx::SqlitePool;
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

use crate::{
    config::Cli,
    models::{AgentOutbound, TerminalServerMessage},
};

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub admin_user: String,
    pub admin_password: String,
    pub upload_dir: PathBuf,
    pub sessions: Arc<RwLock<HashMap<String, i64>>>,
    pub agents: Arc<RwLock<HashMap<String, AgentHandle>>>,
    pub terminal_sessions: Arc<RwLock<HashMap<String, TerminalSessionHandle>>>,
}

impl AppState {
    pub fn new(db: SqlitePool, cli: Cli) -> Self {
        Self {
            db,
            admin_user: cli.admin_user,
            admin_password: cli.admin_password,
            upload_dir: cli.upload_dir,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            agents: Arc::new(RwLock::new(HashMap::new())),
            terminal_sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[derive(Clone)]
pub struct AgentHandle {
    pub connection_id: Uuid,
    pub tx: mpsc::UnboundedSender<AgentOutbound>,
}

#[derive(Clone)]
pub struct TerminalSessionHandle {
    pub instance_id: String,
    pub agent_connection_id: Uuid,
    pub tx: mpsc::UnboundedSender<TerminalServerMessage>,
}
