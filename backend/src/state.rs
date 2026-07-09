use std::{collections::HashMap, path::PathBuf, sync::Arc};

use sqlx::SqlitePool;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};

use crate::{
    config::Cli,
    models::{AgentOutbound, CommandOutcome},
};

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub admin_user: String,
    pub admin_password: String,
    pub upload_dir: PathBuf,
    pub sessions: Arc<RwLock<HashMap<String, i64>>>,
    pub agents: Arc<RwLock<HashMap<String, AgentHandle>>>,
    pub command_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<CommandOutcome>>>>,
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
            command_waiters: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[derive(Clone)]
pub struct AgentHandle {
    pub tx: mpsc::UnboundedSender<AgentOutbound>,
}
