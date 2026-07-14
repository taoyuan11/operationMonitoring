use std::{
    collections::hash_map::DefaultHasher,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    hash::{Hash, Hasher},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    str::FromStr,
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use fs2::{FileExt, available_space};
use futures_util::StreamExt;
use reqwest::Client;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, sync::mpsc};

use crate::{
    activity::ActivityTracker,
    config::AgentConfig,
    models::{AgentInbound, Identity, UpdateOffer, UpdateStatus},
    time::now_ts,
};

const AGENT_ID_HEADER: &str = "X-Agent-ID";
const AGENT_SECRET_HEADER: &str = "X-Agent-Secret";
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
const SERVICE_NAME: &str = "om-agent";
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
const LEGACY_SERVICE_NAME: &str = "operation-monitoring-agent";
#[cfg(target_os = "macos")]
const MACOS_SERVICE_LABEL: &str = "com.operation-monitoring.agent";
const UPDATE_SCHEMA_VERSION: u32 = 1;
const HEALTH_TIMEOUT: Duration = Duration::from_secs(120);
const OLD_PROCESS_TIMEOUT: Duration = Duration::from_secs(30);
const WORKER_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const SERVICE_STOP_TIMEOUT: Duration = Duration::from_secs(30);
const SERVICE_RESTART_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const DISK_RESERVE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CHECKSUM_FILE_BYTES: usize = 4096;

static UPDATE_CAPABILITY: OnceLock<UpdateCapability> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCapability {
    pub package_type: Option<String>,
    pub native_arch: Option<String>,
    pub update_privileged: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PackageType {
    Standalone,
}

impl PackageType {
    fn as_str(self) -> &'static str {
        "standalone"
    }

    fn extension(self) -> &'static str {
        "bin"
    }
}

impl FromStr for PackageType {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        if value == "standalone" {
            Ok(Self::Standalone)
        } else {
            bail!("unsupported package type {value}; only standalone updates are supported")
        }
    }
}

#[derive(Clone)]
pub struct UpdateManager {
    config: AgentConfig,
    identity: Identity,
    client: Client,
    activity: ActivityTracker,
    capability: UpdateCapability,
    paths: UpdatePaths,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareResult {
    ReadyToApply,
    Finished,
}

#[derive(Debug, Deserialize)]
struct UpdateManifest {
    update: Option<UpdateOffer>,
}

#[derive(Debug, Clone)]
struct UpdatePaths {
    root: PathBuf,
    packages: PathBuf,
    state_file: PathBuf,
    health_file: PathBuf,
    update_log: PathBuf,
    lock_file: PathBuf,
    lock_owner_file: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedPackage {
    artifact_id: String,
    version: String,
    package_type: PackageType,
    native_arch: String,
    path: PathBuf,
    #[serde(default)]
    retry_count: i64,
    #[serde(default)]
    size_bytes: u64,
    #[serde(default)]
    sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AttemptPhase {
    Staging,
    Target,
    Rollback,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedAttempt {
    offer: UpdateOffer,
    status: UpdateStatus,
    message: Option<String>,
    package_path: Option<PathBuf>,
    previous_package: Option<CachedPackage>,
    phase: AttemptPhase,
    updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateState {
    schema_version: u32,
    current_package: Option<CachedPackage>,
    attempt: Option<PersistedAttempt>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            schema_version: UPDATE_SCHEMA_VERSION,
            current_package: None,
            attempt: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApplyPlan {
    offer: UpdateOffer,
    package_path: PathBuf,
    previous_package: Option<CachedPackage>,
    state_file: PathBuf,
    health_file: PathBuf,
    lock_file: PathBuf,
    lock_owner_file: PathBuf,
    lock_owner: String,
    old_pid: u32,
    #[serde(default)]
    installed_executable: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
struct HealthMarker {
    artifact_id: String,
    version: String,
    #[serde(default)]
    retry_count: i64,
    connected_at: i64,
}

#[derive(Debug)]
struct DownloadedPackage {
    temporary_path: PathBuf,
    final_path: PathBuf,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug, PartialEq, Eq)]
struct CommandSpec {
    program: OsString,
    args: Vec<OsString>,
}

pub fn update_capability() -> UpdateCapability {
    UPDATE_CAPABILITY
        .get_or_init(detect_update_capability)
        .clone()
}

impl UpdateManager {
    pub fn new(
        config: AgentConfig,
        identity: Identity,
        client: Client,
        activity: ActivityTracker,
    ) -> Result<Self> {
        let paths = UpdatePaths::from_config(&config)?;
        paths.prepare()?;
        Ok(Self {
            config,
            identity,
            client,
            activity,
            capability: update_capability(),
            paths,
        })
    }

    pub async fn fetch_manifest(&self) -> Result<Option<UpdateOffer>> {
        let url = format!(
            "{}/api/agent/update/manifest",
            self.config.server.trim_end_matches('/')
        );
        let manifest = self
            .client
            .get(url)
            .header(AGENT_ID_HEADER, &self.identity.instance_id)
            .header(AGENT_SECRET_HEADER, &self.identity.secret)
            .send()
            .await?
            .error_for_status()?
            .json::<UpdateManifest>()
            .await?;
        Ok(manifest.update)
    }

    pub fn connected_status(&self) -> Result<Option<AgentInbound>> {
        let mut state = read_update_state(&self.paths.state_file)?;
        let Some(attempt) = state.attempt.clone() else {
            return Ok(None);
        };

        let current_version = env!("CARGO_PKG_VERSION");
        let (status, health, message, finalize) = match attempt.phase {
            AttemptPhase::Target if current_version == attempt.offer.version => (
                UpdateStatus::Succeeded,
                Some((
                    attempt.offer.artifact_id.clone(),
                    attempt.offer.version.clone(),
                    attempt.offer.retry_count,
                )),
                None,
                true,
            ),
            AttemptPhase::Rollback => match &attempt.previous_package {
                Some(previous) if current_version == previous.version => (
                    UpdateStatus::RollbackSucceeded,
                    Some((
                        previous.artifact_id.clone(),
                        previous.version.clone(),
                        previous.retry_count,
                    )),
                    attempt.message.clone(),
                    true,
                ),
                _ => (attempt.status, None, attempt.message.clone(), false),
            },
            AttemptPhase::Completed
                if attempt.status == UpdateStatus::Succeeded
                    && current_version == attempt.offer.version =>
            {
                (
                    attempt.status,
                    Some((
                        attempt.offer.artifact_id.clone(),
                        attempt.offer.version.clone(),
                        attempt.offer.retry_count,
                    )),
                    attempt.message.clone(),
                    false,
                )
            }
            AttemptPhase::Completed if attempt.status == UpdateStatus::RollbackSucceeded => {
                match &attempt.previous_package {
                    Some(previous) if current_version == previous.version => (
                        attempt.status,
                        Some((
                            previous.artifact_id.clone(),
                            previous.version.clone(),
                            previous.retry_count,
                        )),
                        attempt.message.clone(),
                        false,
                    ),
                    _ => (attempt.status, None, attempt.message.clone(), false),
                }
            }
            _ => (attempt.status, None, attempt.message.clone(), false),
        };

        if finalize {
            if status == UpdateStatus::Succeeded {
                let package_type: PackageType = attempt.offer.package_type.parse()?;
                let package_path = attempt
                    .package_path
                    .clone()
                    .ok_or_else(|| anyhow!("installed update package is missing from state"))?;
                state.current_package = Some(CachedPackage {
                    artifact_id: attempt.offer.artifact_id.clone(),
                    version: attempt.offer.version.clone(),
                    package_type,
                    native_arch: attempt.offer.native_arch.clone(),
                    path: package_path,
                    retry_count: attempt.offer.retry_count,
                    size_bytes: u64::try_from(attempt.offer.size_bytes)
                        .context("invalid installed package size")?,
                    sha256: attempt.offer.sha256.clone(),
                });
            }
            if let Some(current_attempt) = &mut state.attempt
                && current_attempt.offer.artifact_id == attempt.offer.artifact_id
                && current_attempt.offer.retry_count == attempt.offer.retry_count
            {
                current_attempt.status = status;
                current_attempt.message = message.clone();
                current_attempt.phase = AttemptPhase::Completed;
                current_attempt.updated_at = now_ts();
            }
            write_update_state(&self.paths.state_file, &state)?;
        }

        if let Some((artifact_id, version, retry_count)) = health {
            write_json_atomic(
                &self.paths.health_file,
                &HealthMarker {
                    artifact_id,
                    version,
                    retry_count,
                    connected_at: now_ts(),
                },
            )?;
        }

        Ok(Some(update_status_message(&attempt.offer, status, message)))
    }

    pub fn can_start_offer(&self, offer: &UpdateOffer) -> Result<bool> {
        if update_lock_is_held(&self.paths.lock_file)? {
            return Ok(false);
        }
        let state = read_update_state(&self.paths.state_file)?;
        let Some(attempt) = state.attempt else {
            return Ok(true);
        };
        let active_handoff = matches!(attempt.phase, AttemptPhase::Target | AttemptPhase::Rollback)
            && matches!(
                attempt.status,
                UpdateStatus::Installing | UpdateStatus::AwaitingRestart
            );
        if active_handoff {
            if attempt.offer.artifact_id == offer.artifact_id {
                return Ok(offer.retry_count > attempt.offer.retry_count);
            }
            let persisted_version = Version::parse(&attempt.offer.version)
                .with_context(|| format!("invalid persisted version {}", attempt.offer.version))?;
            let incoming_version = Version::parse(&offer.version)
                .with_context(|| format!("invalid offered version {}", offer.version))?;
            return Ok(incoming_version > persisted_version);
        }
        if attempt.offer.artifact_id != offer.artifact_id {
            return Ok(true);
        }
        let terminal = matches!(
            attempt.status,
            UpdateStatus::Succeeded | UpdateStatus::RollbackSucceeded | UpdateStatus::Failed
        );
        if terminal {
            return Ok(offer.retry_count > attempt.offer.retry_count);
        }
        Ok(offer.retry_count >= attempt.offer.retry_count)
    }

    pub fn cancel_preparation(&self) {
        self.activity.stop_draining();
    }

    pub async fn prepare(
        &self,
        offer: UpdateOffer,
        outbound: mpsc::UnboundedSender<AgentInbound>,
    ) -> PrepareResult {
        match self.prepare_inner(&offer, &outbound).await {
            Ok(result) => result,
            Err(error) => {
                self.activity.stop_draining();
                let message = format!("{error:#}");
                if let Err(persist_error) =
                    self.send_status(&offer, UpdateStatus::Failed, Some(message), &outbound)
                {
                    eprintln!("failed to persist update failure: {persist_error:#}");
                }
                PrepareResult::Finished
            }
        }
    }

    async fn prepare_inner(
        &self,
        offer: &UpdateOffer,
        outbound: &mpsc::UnboundedSender<AgentInbound>,
    ) -> Result<PrepareResult> {
        self.begin_attempt(offer)?;
        let package_type = self.validate_offer(offer)?;

        let current_version = Version::parse(env!("CARGO_PKG_VERSION"))?;
        let target_version = Version::parse(&offer.version)
            .with_context(|| format!("invalid target version {}", offer.version))?;
        if target_version == current_version {
            self.send_status(
                offer,
                UpdateStatus::Succeeded,
                Some("target version is already running".to_string()),
                outbound,
            )?;
            return Ok(PrepareResult::Finished);
        }
        if target_version < current_version {
            bail!("refusing automatic downgrade from {current_version} to {target_version}");
        }

        let delay = update_delay_seconds(&self.identity.instance_id, &offer.artifact_id);
        self.send_status(
            offer,
            UpdateStatus::Waiting,
            Some(format!(
                "update will start after a {delay} second spread delay"
            )),
            outbound,
        )?;
        tokio::time::sleep(Duration::from_secs(delay)).await;

        self.send_status(offer, UpdateStatus::Downloading, None, outbound)?;
        let checksum_sha256 = self.download_checksum(offer).await?;
        let downloaded = self.download_to_temporary(offer, package_type).await?;

        self.send_status(offer, UpdateStatus::Verifying, None, outbound)?;
        verify_download(offer, package_type, &downloaded, &checksum_sha256)?;
        replace_file(&downloaded.temporary_path, &downloaded.final_path)?;
        self.set_package_path(offer, downloaded.final_path.clone())?;

        self.activity.start_draining();
        let active = self.activity.active_count();
        if active > 0 {
            self.send_status(
                offer,
                UpdateStatus::WaitingIdle,
                Some(format!(
                    "waiting for {active} active command or terminal session(s)"
                )),
                outbound,
            )?;
        }
        self.activity.wait_until_idle().await;

        Ok(PrepareResult::ReadyToApply)
    }

    pub fn launch_prepared_update(
        &self,
        offer: &UpdateOffer,
        outbound: &mpsc::UnboundedSender<AgentInbound>,
    ) -> bool {
        let result = (|| {
            let state = read_update_state(&self.paths.state_file)?;
            let package_path = state
                .attempt
                .as_ref()
                .filter(|attempt| {
                    attempt.offer.artifact_id == offer.artifact_id
                        && attempt.offer.retry_count == offer.retry_count
                })
                .and_then(|attempt| attempt.package_path.clone())
                .ok_or_else(|| anyhow!("prepared update package is missing from update state"))?;
            self.spawn_updater(offer, package_path)
        })();

        if let Err(error) = result {
            self.activity.stop_draining();
            let message = format!("failed to launch detached updater: {error:#}");
            if let Err(persist_error) =
                self.send_status(offer, UpdateStatus::Failed, Some(message), outbound)
            {
                eprintln!("failed to persist updater launch failure: {persist_error:#}");
            }
            return false;
        }

        if let Err(error) = self.mark_handoff_started(
            offer,
            Some("detached updater started; agent is exiting".to_string()),
            outbound,
        ) {
            // The updater is already independent of this process. The parent must still
            // exit so that the standalone updater can replace the executable.
            eprintln!("failed to persist updater handoff status: {error:#}");
        }
        true
    }

    fn validate_offer(&self, offer: &UpdateOffer) -> Result<PackageType> {
        if !self.capability.update_privileged {
            bail!("agent lacks root or administrator privileges required for updates");
        }
        let Some(local_package_type) = self.capability.package_type.as_deref() else {
            bail!("this process is not a managed standalone installation");
        };
        if offer.package_type != local_package_type {
            bail!(
                "package type mismatch: agent requires {local_package_type}, offer is {}",
                offer.package_type
            );
        }
        let Some(local_arch) = self.capability.native_arch.as_deref() else {
            bail!("unable to determine standalone executable architecture");
        };
        if offer.native_arch != local_arch {
            bail!(
                "native architecture mismatch: agent requires {local_arch}, offer is {}",
                offer.native_arch
            );
        }
        if offer.size_bytes <= 0 {
            bail!("update package size must be positive");
        }
        if offer.retry_count < 0 {
            bail!("update retry count must not be negative");
        }
        if offer.sha256.len() != 64 || !offer.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("update package SHA-256 is invalid");
        }
        let expected_download_url =
            format!("/api/agent/update/artifacts/{}/download", offer.artifact_id);
        if offer.download_url != expected_download_url {
            bail!("update download URL must be an agent update API path");
        }
        offer.package_type.parse()
    }

    async fn download_to_temporary(
        &self,
        offer: &UpdateOffer,
        package_type: PackageType,
    ) -> Result<DownloadedPackage> {
        let expected_size = u64::try_from(offer.size_bytes).context("invalid package size")?;
        let available = available_space(&self.paths.packages).with_context(|| {
            format!(
                "failed to inspect free space in {}",
                self.paths.packages.display()
            )
        })?;
        if available < expected_size.saturating_add(DISK_RESERVE_BYTES) {
            bail!(
                "insufficient disk space: {available} bytes available, {} bytes required",
                expected_size.saturating_add(DISK_RESERVE_BYTES)
            );
        }

        let basename = safe_component(&offer.artifact_id);
        let final_path = self
            .paths
            .packages
            .join(format!("{basename}.{}", package_type.extension()));
        let temporary_path = self.paths.packages.join(format!("{basename}.part"));
        let _ = fs::remove_file(&temporary_path);

        let url = format!(
            "{}{}",
            self.config.server.trim_end_matches('/'),
            offer.download_url
        );
        let response = self
            .client
            .get(url)
            .header(AGENT_ID_HEADER, &self.identity.instance_id)
            .header(AGENT_SECRET_HEADER, &self.identity.secret)
            .send()
            .await?
            .error_for_status()?;
        if let Some(content_length) = response.content_length()
            && content_length != expected_size
        {
            bail!(
                "download Content-Length mismatch: expected {expected_size}, got {content_length}"
            );
        }

        let mut file = secure_new_file(&temporary_path).await?;
        let mut stream = response.bytes_stream();
        let mut hasher = Sha256::new();
        let mut received = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            received = received
                .checked_add(chunk.len() as u64)
                .context("download size overflow")?;
            if received > expected_size {
                let _ = tokio::fs::remove_file(&temporary_path).await;
                bail!("download exceeded declared package size");
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);

        Ok(DownloadedPackage {
            temporary_path,
            final_path,
            size_bytes: received,
            sha256: format!("{:x}", hasher.finalize()),
        })
    }

    async fn download_checksum(&self, offer: &UpdateOffer) -> Result<String> {
        let checksum_path = offer
            .download_url
            .strip_suffix("/download")
            .context("update download URL has no download suffix")?;
        let checksum_url = format!(
            "{}{checksum_path}/checksum",
            self.config.server.trim_end_matches('/'),
        );
        let response = self
            .client
            .get(checksum_url)
            .header(AGENT_ID_HEADER, &self.identity.instance_id)
            .header(AGENT_SECRET_HEADER, &self.identity.secret)
            .send()
            .await?
            .error_for_status()?;
        if response
            .content_length()
            .is_some_and(|length| length == 0 || length > MAX_CHECKSUM_FILE_BYTES as u64)
        {
            bail!("update SHA-256 sidecar size is invalid");
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len() + chunk.len() > MAX_CHECKSUM_FILE_BYTES {
                bail!("update SHA-256 sidecar size is invalid");
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            bail!("update SHA-256 sidecar size is invalid");
        }
        let contents = std::str::from_utf8(&bytes)
            .context("update SHA-256 sidecar is not valid UTF-8 text")?;
        parse_checksum_sidecar(contents)
    }

    fn begin_attempt(&self, offer: &UpdateOffer) -> Result<()> {
        let mut state = read_update_state(&self.paths.state_file)?;
        state.attempt = Some(PersistedAttempt {
            offer: offer.clone(),
            status: UpdateStatus::Waiting,
            message: None,
            package_path: None,
            previous_package: state.current_package.clone(),
            phase: AttemptPhase::Staging,
            updated_at: now_ts(),
        });
        write_update_state(&self.paths.state_file, &state)
    }

    fn send_status(
        &self,
        offer: &UpdateOffer,
        status: UpdateStatus,
        message: Option<String>,
        outbound: &mpsc::UnboundedSender<AgentInbound>,
    ) -> Result<()> {
        let mut state = read_update_state(&self.paths.state_file)?;
        let attempt = state
            .attempt
            .as_mut()
            .filter(|attempt| {
                attempt.offer.artifact_id == offer.artifact_id
                    && attempt.offer.retry_count == offer.retry_count
            })
            .ok_or_else(|| {
                anyhow!(
                    "update attempt {} generation {} is no longer current",
                    offer.artifact_id,
                    offer.retry_count
                )
            })?;
        attempt.status = status;
        attempt.message = message.clone();
        attempt.updated_at = now_ts();
        if matches!(
            status,
            UpdateStatus::Succeeded | UpdateStatus::RollbackSucceeded | UpdateStatus::Failed
        ) {
            attempt.phase = AttemptPhase::Completed;
        }
        write_update_state(&self.paths.state_file, &state)?;
        let _ = outbound.send(update_status_message(offer, status, message));
        Ok(())
    }

    fn set_package_path(&self, offer: &UpdateOffer, path: PathBuf) -> Result<()> {
        mutate_attempt(
            &self.paths.state_file,
            &offer.artifact_id,
            offer.retry_count,
            |attempt| {
                attempt.package_path = Some(path);
            },
        )
    }

    fn mark_handoff_started(
        &self,
        offer: &UpdateOffer,
        message: Option<String>,
        outbound: &mpsc::UnboundedSender<AgentInbound>,
    ) -> Result<()> {
        let mut state = read_update_state(&self.paths.state_file)?;
        let attempt = state
            .attempt
            .as_mut()
            .filter(|attempt| {
                attempt.offer.artifact_id == offer.artifact_id
                    && attempt.offer.retry_count == offer.retry_count
            })
            .ok_or_else(|| anyhow!("prepared update attempt is missing from update state"))?;
        attempt.status = UpdateStatus::AwaitingRestart;
        attempt.message = message.clone();
        attempt.phase = AttemptPhase::Target;
        attempt.updated_at = now_ts();
        write_update_state(&self.paths.state_file, &state)?;
        let _ = outbound.send(update_status_message(
            offer,
            UpdateStatus::AwaitingRestart,
            message,
        ));
        Ok(())
    }

    fn spawn_updater(&self, offer: &UpdateOffer, package_path: PathBuf) -> Result<()> {
        let mut state = read_update_state(&self.paths.state_file)?;
        let mut previous_package = state
            .attempt
            .as_ref()
            .filter(|attempt| {
                attempt.offer.artifact_id == offer.artifact_id
                    && attempt.offer.retry_count == offer.retry_count
            })
            .and_then(|attempt| attempt.previous_package.clone());
        if previous_package.is_none() && offer.package_type == PackageType::Standalone.as_str() {
            let current = std::env::current_exe()?;
            let rollback_path = self.paths.packages.join(format!(
                "standalone-rollback-{}{}",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::EXE_SUFFIX
            ));
            fs::copy(&current, &rollback_path)?;
            set_owner_only_executable(&rollback_path)?;
            let (size_bytes, sha256) = file_integrity(&rollback_path)?;
            previous_package = Some(CachedPackage {
                artifact_id: format!("standalone-installed-{}", env!("CARGO_PKG_VERSION")),
                version: env!("CARGO_PKG_VERSION").to_string(),
                package_type: PackageType::Standalone,
                native_arch: standalone_native_arch(),
                path: rollback_path,
                retry_count: 0,
                size_bytes,
                sha256,
            });
        }
        if let Some(attempt) = &mut state.attempt
            && attempt.offer.artifact_id == offer.artifact_id
            && attempt.offer.retry_count == offer.retry_count
            && attempt.previous_package.is_none()
        {
            attempt.previous_package = previous_package.clone();
            write_update_state(&self.paths.state_file, &state)?;
        }
        let component = safe_component(&format!(
            "{}-retry-{}",
            offer.artifact_id, offer.retry_count
        ));
        let plan_path = self.paths.root.join(format!("apply-{component}.json"));
        let executable_suffix = std::env::consts::EXE_SUFFIX;
        let updater_path = self
            .paths
            .root
            .join(format!("updater-{component}{executable_suffix}"));
        let plan = ApplyPlan {
            offer: offer.clone(),
            package_path,
            previous_package,
            state_file: self.paths.state_file.clone(),
            health_file: self.paths.health_file.clone(),
            lock_file: self.paths.lock_file.clone(),
            lock_owner_file: self.paths.lock_owner_file.clone(),
            lock_owner: uuid::Uuid::new_v4().to_string(),
            old_pid: std::process::id(),
            installed_executable: Some(std::env::current_exe()?),
        };
        write_json_atomic(&plan_path, &plan)?;

        fs::copy(std::env::current_exe()?, &updater_path).with_context(|| {
            format!(
                "failed to create detached updater {}",
                updater_path.display()
            )
        })?;
        set_owner_only_executable(&updater_path)?;

        let spawned_with_systemd =
            try_spawn_systemd_updater(&updater_path, &plan_path, &component)?;
        if !spawned_with_systemd {
            let stdout = open_update_log(&self.paths.update_log)?;
            let stderr = stdout.try_clone()?;
            let mut command = Command::new(&updater_path);
            command
                .arg("apply-update")
                .arg("--plan-file")
                .arg(&plan_path)
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr));
            detach(&mut command);
            command
                .spawn()
                .with_context(|| format!("failed to start updater {}", updater_path.display()))?;
        }
        wait_for_worker_ownership(&plan, WORKER_LOCK_TIMEOUT)?;
        Ok(())
    }
}

fn update_delay_seconds(instance_id: &str, artifact_id: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    instance_id.hash(&mut hasher);
    artifact_id.hash(&mut hasher);
    hasher.finish() % 61
}

fn replace_file(source: &Path, target: &Path) -> Result<()> {
    if target.exists() {
        fs::remove_file(target)?;
    }
    fs::rename(source, target).with_context(|| {
        format!(
            "failed to move {} to {}",
            source.display(),
            target.display()
        )
    })
}

fn safe_component(value: &str) -> String {
    let value: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(96)
        .collect();
    if value.is_empty() {
        "artifact".into()
    } else {
        value
    }
}

async fn secure_new_file(path: &Path) -> Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    options
        .open(path)
        .await
        .with_context(|| format!("failed to create {}", path.display()))
}

fn read_update_state(path: &Path) -> Result<UpdateState> {
    match fs::read(path) {
        Ok(content) => {
            let state: UpdateState = serde_json::from_slice(&content)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            if state.schema_version != UPDATE_SCHEMA_VERSION {
                bail!("unsupported update state schema {}", state.schema_version);
            }
            Ok(state)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(UpdateState::default()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn write_update_state(path: &Path, state: &UpdateState) -> Result<()> {
    write_json_atomic(path, state)
}

fn write_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|v| v.to_str()).unwrap_or("state"),
        uuid::Uuid::new_v4()
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    replace_file(&temporary, path)
}

fn update_status_message(
    offer: &UpdateOffer,
    status: UpdateStatus,
    message: Option<String>,
) -> AgentInbound {
    AgentInbound::UpdateStatus {
        release_id: offer.release_id.clone(),
        artifact_id: offer.artifact_id.clone(),
        version: offer.version.clone(),
        retry_count: offer.retry_count,
        status,
        message,
    }
}

fn mutate_attempt(
    state_file: &Path,
    artifact_id: &str,
    retry_count: i64,
    mutate: impl FnOnce(&mut PersistedAttempt),
) -> Result<()> {
    let mut state = read_update_state(state_file)?;
    let attempt = state
        .attempt
        .as_mut()
        .filter(|attempt| {
            attempt.offer.artifact_id == artifact_id && attempt.offer.retry_count == retry_count
        })
        .ok_or_else(|| anyhow!("update attempt is no longer current"))?;
    mutate(attempt);
    attempt.updated_at = now_ts();
    write_update_state(state_file, &state)
}

fn open_update_log(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))
}

#[cfg(unix)]
fn set_owner_only_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_owner_only_directory(_path: &Path) -> Result<()> {
    Ok(())
}
#[cfg(unix)]
fn set_owner_only_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_owner_only_executable(_path: &Path) -> Result<()> {
    Ok(())
}
#[cfg(unix)]
fn set_installed_executable_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_installed_executable_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn normalize_macos_arch(arch: &str) -> String {
    match arch.to_ascii_lowercase().as_str() {
        "aarch64" | "arm64" => "arm64".into(),
        "x86_64" | "amd64" => "x86_64".into(),
        value => value.into(),
    }
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}
#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x00000008 | 0x00000200);
}

fn try_spawn_systemd_updater(
    updater_path: &Path,
    plan_path: &Path,
    component: &str,
) -> Result<bool> {
    if !Path::new("/run/systemd/system").is_dir() {
        return Ok(false);
    }
    let unit = format!(
        "om-agent-update-{}",
        component.chars().take(40).collect::<String>()
    );
    let spec = CommandSpec {
        program: "systemd-run".into(),
        args: vec![
            "--quiet".into(),
            "--collect".into(),
            "--unit".into(),
            unit.into(),
            updater_path.as_os_str().to_owned(),
            "apply-update".into(),
            "--plan-file".into(),
            plan_path.as_os_str().to_owned(),
        ],
    };
    match run_command_with_timeout(&spec, SERVICE_RESTART_TIMEOUT, "systemd updater launch") {
        Ok(status) if status.success() => Ok(true),
        Ok(status) => bail!("systemd-run exited with {status}"),
        Err(error) => Err(error).context("failed to start updater through systemd-run"),
    }
}

fn open_update_lock(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("failed to open updater ownership lock {}", path.display()))
}

fn acquire_worker_ownership(plan: &ApplyPlan) -> Result<File> {
    let lock = open_update_lock(&plan.lock_file)?;
    lock.try_lock_exclusive()
        .with_context(|| format!("another updater already owns {}", plan.lock_file.display()))?;
    write_json_atomic(&plan.lock_owner_file, &plan.lock_owner)?;
    Ok(lock)
}

fn wait_for_worker_ownership(plan: &ApplyPlan, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() <= timeout {
        let lock = open_update_lock(&plan.lock_file)?;
        match lock.try_lock_exclusive() {
            Ok(()) => {
                FileExt::unlock(&lock)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let owner = fs::read_to_string(&plan.lock_owner_file)
                    .ok()
                    .and_then(|value| serde_json::from_str::<String>(&value).ok());
                if owner.as_deref() == Some(&plan.lock_owner) {
                    return Ok(());
                }
            }
            Err(error) => return Err(error).context("failed to inspect updater ownership lock"),
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!(
        "updater did not acquire its ownership lock within {} seconds",
        timeout.as_secs_f64()
    )
}

fn update_lock_is_held(path: &Path) -> Result<bool> {
    let lock = open_update_lock(path)?;
    match lock.try_lock_exclusive() {
        Ok(()) => {
            FileExt::unlock(&lock)?;
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error).context("failed to inspect updater ownership lock"),
    }
}

impl UpdatePaths {
    fn from_config(config: &AgentConfig) -> Result<Self> {
        let root = if let Some(path) = &config.update_dir {
            path.clone()
        } else if let Some(path) = &config.state_dir {
            path.join("updates")
        } else {
            ProjectDirs::from("com", "operation-monitoring", "agent")
                .map(|dirs| dirs.data_local_dir().join("updates"))
                .unwrap_or(std::env::current_dir()?.join(".operation-monitoring-updates"))
        };
        Ok(Self {
            packages: root.join("packages"),
            state_file: root.join("state.json"),
            health_file: root.join("health.json"),
            update_log: root.join("updater.log"),
            lock_file: root.join("updater.lock"),
            lock_owner_file: root.join("updater-owner.json"),
            root,
        })
    }

    fn prepare(&self) -> Result<()> {
        fs::create_dir_all(&self.packages).with_context(|| {
            format!(
                "failed to create update directory {}",
                self.packages.display()
            )
        })?;
        set_owner_only_directory(&self.root)?;
        set_owner_only_directory(&self.packages)?;
        for entry in fs::read_dir(&self.packages)? {
            let path = entry?.path();
            if path.extension().is_some_and(|value| value == "part") {
                let _ = fs::remove_file(path);
            }
        }
        Ok(())
    }
}

pub fn apply_update(plan_file: &Path) -> Result<()> {
    let content = fs::read_to_string(plan_file)
        .with_context(|| format!("failed to read update plan {}", plan_file.display()))?;
    let plan: ApplyPlan = serde_json::from_str(&content)?;
    let _ownership = acquire_worker_ownership(&plan)?;
    println!(
        "applying agent update {} from {}",
        plan.offer.version,
        plan.package_path.display()
    );

    let result = apply_update_inner(&plan);
    if let Err(error) = &result {
        let message = format!("updater failed: {error:#}");
        let _ = persist_apply_status(
            &plan,
            UpdateStatus::Failed,
            AttemptPhase::Completed,
            Some(message),
        );
    }
    result
}

fn apply_update_inner(plan: &ApplyPlan) -> Result<()> {
    ensure_plan_generation_is_current(plan)?;
    let package_type: PackageType = plan.offer.package_type.parse()?;
    stop_standalone_service()?;
    wait_for_process_exit(plan.old_pid, OLD_PROCESS_TIMEOUT)?;
    ensure_plan_handoff_is_active(plan)?;
    let _ = fs::remove_file(&plan.health_file);
    persist_apply_status(plan, UpdateStatus::Installing, AttemptPhase::Target, None)?;

    let target_result = u64::try_from(plan.offer.size_bytes)
        .context("invalid target package size")
        .and_then(|size_bytes| {
            verify_package_at_rest(
                &plan.package_path,
                package_type,
                size_bytes,
                &plan.offer.sha256,
            )
            .context("staged target package verification failed")
        })
        .and_then(|()| install_standalone(plan, &plan.package_path));
    if let Err(error) = target_result {
        return attempt_rollback(
            plan,
            format!("target package installation failed: {error:#}"),
        );
    }

    persist_apply_status(
        plan,
        UpdateStatus::AwaitingRestart,
        AttemptPhase::Target,
        None,
    )?;
    if let Err(error) = restart_agent_service(package_type) {
        eprintln!("failed to request agent service restart: {error:#}");
    }

    if wait_for_health(
        &plan.health_file,
        &plan.offer.artifact_id,
        &plan.offer.version,
        plan.offer.retry_count,
        HEALTH_TIMEOUT,
    ) {
        complete_target_update(plan, package_type)?;
        println!("agent update {} is healthy", plan.offer.version);
        return Ok(());
    }

    attempt_rollback(
        plan,
        format!(
            "agent version {} did not reconnect within {} seconds",
            plan.offer.version,
            HEALTH_TIMEOUT.as_secs()
        ),
    )
}

fn ensure_plan_generation_is_current(plan: &ApplyPlan) -> Result<()> {
    let state = read_update_state(&plan.state_file)?;
    let current = state.attempt.as_ref().is_some_and(|attempt| {
        attempt.offer.artifact_id == plan.offer.artifact_id
            && attempt.offer.retry_count == plan.offer.retry_count
    });
    if !current {
        bail!(
            "update plan {} generation {} is stale",
            plan.offer.artifact_id,
            plan.offer.retry_count
        );
    }
    Ok(())
}

fn ensure_plan_handoff_is_active(plan: &ApplyPlan) -> Result<()> {
    let state = read_update_state(&plan.state_file)?;
    let active = state.attempt.as_ref().is_some_and(|attempt| {
        attempt.offer.artifact_id == plan.offer.artifact_id
            && attempt.offer.retry_count == plan.offer.retry_count
            && attempt.phase == AttemptPhase::Target
            && matches!(
                attempt.status,
                UpdateStatus::Installing | UpdateStatus::AwaitingRestart
            )
    });
    if !active {
        bail!(
            "update plan {} generation {} has no active parent handoff",
            plan.offer.artifact_id,
            plan.offer.retry_count
        );
    }
    Ok(())
}

fn attempt_rollback(plan: &ApplyPlan, reason: String) -> Result<()> {
    let Some(previous) = &plan.previous_package else {
        let package_type: PackageType = plan.offer.package_type.parse()?;
        let _ = restart_agent_service(package_type);
        persist_apply_status(
            plan,
            UpdateStatus::Failed,
            AttemptPhase::Completed,
            Some(format!("{reason}; no cached rollback package is available")),
        )?;
        bail!("{reason}; no cached rollback package is available");
    };

    eprintln!(
        "{reason}; rolling back to agent {} from {}",
        previous.version,
        previous.path.display()
    );
    let _ = fs::remove_file(&plan.health_file);
    persist_apply_status(
        plan,
        UpdateStatus::Installing,
        AttemptPhase::Rollback,
        Some(reason.clone()),
    )?;
    verify_package_at_rest(
        &previous.path,
        previous.package_type,
        previous.size_bytes,
        &previous.sha256,
    )
    .context("cached rollback package verification failed")?;
    stop_standalone_service().context("failed to stop service before standalone rollback")?;
    install_standalone(plan, &previous.path).context("standalone rollback failed")?;
    persist_apply_status(
        plan,
        UpdateStatus::AwaitingRestart,
        AttemptPhase::Rollback,
        Some(reason.clone()),
    )?;
    if let Err(error) = restart_agent_service(previous.package_type) {
        eprintln!("failed to request rolled-back service restart: {error:#}");
    }

    if !wait_for_health(
        &plan.health_file,
        &previous.artifact_id,
        &previous.version,
        previous.retry_count,
        HEALTH_TIMEOUT,
    ) {
        persist_apply_status(
            plan,
            UpdateStatus::Failed,
            AttemptPhase::Completed,
            Some(format!(
                "{reason}; rollback version {} did not reconnect",
                previous.version
            )),
        )?;
        bail!("rollback version {} did not reconnect", previous.version);
    }

    let _ = fs::remove_file(&plan.package_path);
    persist_apply_status(
        plan,
        UpdateStatus::RollbackSucceeded,
        AttemptPhase::Completed,
        Some(reason),
    )?;
    println!("agent rollback to {} succeeded", previous.version);
    Ok(())
}

fn complete_target_update(plan: &ApplyPlan, package_type: PackageType) -> Result<()> {
    let mut state = read_update_state(&plan.state_file)?;
    let previous_path = plan
        .previous_package
        .as_ref()
        .map(|value| value.path.clone());
    let target_already_cached = state.current_package.as_ref().is_some_and(|package| {
        package.artifact_id == plan.offer.artifact_id
            && package.retry_count == plan.offer.retry_count
    });
    let plan_is_current = state.attempt.as_ref().is_some_and(|attempt| {
        attempt.offer.artifact_id == plan.offer.artifact_id
            && attempt.offer.retry_count == plan.offer.retry_count
    });
    if !target_already_cached && !plan_is_current {
        return Ok(());
    }
    if !target_already_cached {
        state.current_package = Some(CachedPackage {
            artifact_id: plan.offer.artifact_id.clone(),
            version: plan.offer.version.clone(),
            package_type,
            native_arch: plan.offer.native_arch.clone(),
            path: plan.package_path.clone(),
            retry_count: plan.offer.retry_count,
            size_bytes: u64::try_from(plan.offer.size_bytes)
                .context("invalid installed package size")?,
            sha256: plan.offer.sha256.clone(),
        });
        if let Some(attempt) = &mut state.attempt
            && attempt.offer.artifact_id == plan.offer.artifact_id
            && attempt.offer.retry_count == plan.offer.retry_count
        {
            attempt.status = UpdateStatus::Succeeded;
            attempt.message = None;
            attempt.phase = AttemptPhase::Completed;
            attempt.updated_at = now_ts();
        }
        write_update_state(&plan.state_file, &state)?;
    }
    if let Some(previous_path) = previous_path
        && previous_path != plan.package_path
    {
        let _ = fs::remove_file(previous_path);
    }
    Ok(())
}

fn persist_apply_status(
    plan: &ApplyPlan,
    status: UpdateStatus,
    phase: AttemptPhase,
    message: Option<String>,
) -> Result<()> {
    let mut state = read_update_state(&plan.state_file)?;
    let Some(attempt) = state.attempt.as_mut().filter(|attempt| {
        attempt.offer.artifact_id == plan.offer.artifact_id
            && attempt.offer.retry_count == plan.offer.retry_count
    }) else {
        return Ok(());
    };
    let finalized_for_same_phase = attempt.phase == AttemptPhase::Completed
        && matches!(
            (attempt.status, phase),
            (UpdateStatus::Succeeded, AttemptPhase::Target)
                | (UpdateStatus::RollbackSucceeded, AttemptPhase::Rollback)
        );
    if finalized_for_same_phase
        && !matches!(
            status,
            UpdateStatus::Succeeded | UpdateStatus::RollbackSucceeded | UpdateStatus::Failed
        )
    {
        return Ok(());
    }
    if attempt.status == status && attempt.phase == phase && attempt.message == message {
        return Ok(());
    }
    attempt.status = status;
    attempt.phase = phase;
    attempt.message = message;
    attempt.updated_at = now_ts();
    write_update_state(&plan.state_file, &state)
}

fn run_command_with_timeout(
    spec: &CommandSpec,
    timeout: Duration,
    description: &str,
) -> Result<ExitStatus> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args).stdin(Stdio::null());
    configure_command_process(&mut command);
    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to start {description} {}",
            spec.program.to_string_lossy()
        )
    })?;
    wait_for_command(&mut child, spec, timeout, description)
}

#[cfg(any(windows, test))]
fn run_command_output_with_timeout(
    spec: &CommandSpec,
    timeout: Duration,
    description: &str,
) -> Result<std::process::Output> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_command_process(&mut command);
    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to start {description} {}",
            spec.program.to_string_lossy()
        )
    })?;
    wait_for_command(&mut child, spec, timeout, description)?;
    child.wait_with_output().with_context(|| {
        format!(
            "failed to collect {description} output from {}",
            spec.program.to_string_lossy()
        )
    })
}

fn wait_for_command(
    child: &mut Child,
    spec: &CommandSpec,
    timeout: Duration,
    description: &str,
) -> Result<ExitStatus> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if started.elapsed() >= timeout {
            terminate_process_tree(child);
            bail!(
                "{description} {} timed out after {} seconds",
                spec.program.to_string_lossy(),
                timeout.as_secs_f64()
            );
        }
        thread::sleep(POLL_INTERVAL.min(timeout.saturating_sub(started.elapsed())));
    }
}

#[cfg(unix)]
fn configure_command_process(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn configure_command_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn configure_command_process(_command: &mut Command) {}

fn terminate_process_tree(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        let _ = libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let pid = child.id().to_string();
        if let Ok(mut killer) = Command::new("taskkill.exe")
            .args(["/PID", pid.as_str(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let started = Instant::now();
            loop {
                if killer.try_wait().ok().flatten().is_some() {
                    break;
                }
                if started.elapsed() >= Duration::from_secs(10) {
                    let _ = killer.kill();
                    let _ = killer.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn restart_agent_service(_package_type: PackageType) -> Result<()> {
    #[cfg(windows)]
    {
        let mut errors = Vec::new();
        for service_name in [LEGACY_SERVICE_NAME, SERVICE_NAME] {
            match restart_windows_agent_service(service_name) {
                Ok(()) => return Ok(()),
                Err(error) => errors.push(format!("{service_name}: {error:#}")),
            }
        }
        bail!("{}", errors.join("; "));
    }

    #[cfg(not(windows))]
    let candidates = standalone_restart_candidates();
    #[cfg(not(windows))]
    let mut errors = Vec::new();
    #[cfg(not(windows))]
    for candidate in candidates {
        match run_command_with_timeout(
            &candidate,
            SERVICE_RESTART_TIMEOUT,
            "service restart command",
        ) {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => errors.push(format!(
                "{} exited with {status}",
                candidate.program.to_string_lossy()
            )),
            Err(error) => errors.push(format!("{}: {error}", candidate.program.to_string_lossy())),
        }
    }
    #[cfg(not(windows))]
    bail!("{}", errors.join("; "))
}

#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScServiceState {
    Stopped,
    Running,
    Other,
}

#[cfg(any(windows, test))]
fn parse_sc_query_state(output: &str) -> Option<ScServiceState> {
    for line in output.lines() {
        let Some((_, fields)) = line.split_once(':') else {
            continue;
        };
        let Some(value) = fields
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        if (1..=7).contains(&value) {
            return Some(match value {
                1 => ScServiceState::Stopped,
                4 => ScServiceState::Running,
                _ => ScServiceState::Other,
            });
        }
    }
    None
}

#[cfg(windows)]
fn query_windows_agent_service(service_name: &str) -> Result<ScServiceState> {
    let spec = CommandSpec {
        program: "sc.exe".into(),
        args: vec!["query".into(), service_name.into()],
    };
    let output =
        run_command_output_with_timeout(&spec, SERVICE_RESTART_TIMEOUT, "Windows service query")?;
    if !output.status.success() {
        bail!(
            "sc query exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_sc_query_state(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| anyhow!("sc query did not report a service state"))
}

#[cfg(windows)]
fn stop_windows_agent_service(service_name: &str) -> Result<()> {
    if query_windows_agent_service(service_name)? == ScServiceState::Stopped {
        return Ok(());
    }
    let stop = CommandSpec {
        program: "sc.exe".into(),
        args: vec!["stop".into(), service_name.into()],
    };
    let _ = run_command_with_timeout(&stop, SERVICE_RESTART_TIMEOUT, "Windows service stop")?;

    let started = Instant::now();
    loop {
        if query_windows_agent_service(service_name)? == ScServiceState::Stopped {
            return Ok(());
        }
        if started.elapsed() >= SERVICE_STOP_TIMEOUT {
            bail!(
                "Windows agent service did not stop within {} seconds",
                SERVICE_STOP_TIMEOUT.as_secs()
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(windows)]
fn restart_windows_agent_service(service_name: &str) -> Result<()> {
    stop_windows_agent_service(service_name)?;

    let start = CommandSpec {
        program: "sc.exe".into(),
        args: vec!["start".into(), service_name.into()],
    };
    let output =
        run_command_output_with_timeout(&start, SERVICE_RESTART_TIMEOUT, "Windows service start")?;
    if output.status.success()
        || query_windows_agent_service(service_name)? == ScServiceState::Running
    {
        return Ok(());
    }
    bail!(
        "sc start exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn wait_for_health(
    path: &Path,
    artifact_id: &str,
    version: &str,
    retry_count: i64,
    timeout: Duration,
) -> bool {
    let started = Instant::now();
    while started.elapsed() <= timeout {
        if fs::read_to_string(path)
            .ok()
            .and_then(|content| serde_json::from_str::<HealthMarker>(&content).ok())
            .is_some_and(|marker| {
                marker.artifact_id == artifact_id
                    && marker.version == version
                    && marker.retry_count == retry_count
            })
        {
            return true;
        }
        thread::sleep(POLL_INTERVAL);
    }
    false
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() <= timeout {
        if !process_is_running(pid) {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }
    bail!("agent process {pid} did not exit before update timeout")
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };

    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return GetLastError() != ERROR_INVALID_PARAMETER;
        }
        let result = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        match result {
            WAIT_OBJECT_0 => false,
            WAIT_TIMEOUT => true,
            _ => true,
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn process_is_running(_pid: u32) -> bool {
    false
}

fn verify_download(
    offer: &UpdateOffer,
    package_type: PackageType,
    downloaded: &DownloadedPackage,
    checksum_sha256: &str,
) -> Result<()> {
    let expected_size = u64::try_from(offer.size_bytes).context("invalid package size")?;
    if downloaded.size_bytes != expected_size {
        bail!(
            "download size mismatch: expected {expected_size}, got {}",
            downloaded.size_bytes
        );
    }
    if !checksum_sha256.eq_ignore_ascii_case(&offer.sha256) {
        bail!(
            "SHA-256 sidecar mismatch: offer expected {}, sidecar declared {}",
            offer.sha256,
            checksum_sha256
        );
    }
    if !downloaded.sha256.eq_ignore_ascii_case(checksum_sha256) {
        bail!(
            "download SHA-256 mismatch: expected {}, got {}",
            checksum_sha256,
            downloaded.sha256
        );
    }
    validate_package_magic(package_type, &downloaded.temporary_path)
}

fn parse_checksum_sidecar(contents: &str) -> Result<String> {
    let mut fields = contents.split_whitespace();
    let sha256 = fields.next().unwrap_or_default();
    if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("update SHA-256 sidecar contains an invalid digest");
    }
    let _file_name = fields.next();
    if fields.next().is_some() {
        bail!("update SHA-256 sidecar contains unexpected fields");
    }
    Ok(sha256.to_ascii_lowercase())
}

fn verify_package_at_rest(
    path: &Path,
    package_type: PackageType,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<()> {
    if expected_size == 0
        || expected_sha256.len() != 64
        || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("cached package integrity metadata is invalid");
    }
    let mut file = File::open(path)
        .with_context(|| format!("failed to open staged package {}", path.display()))?;
    let actual_size = file.metadata()?.len();
    if actual_size != expected_size {
        bail!("staged package size mismatch: expected {expected_size}, got {actual_size}");
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let actual_sha256 = format!("{:x}", hasher.finalize());
    if !actual_sha256.eq_ignore_ascii_case(expected_sha256) {
        bail!("staged package SHA-256 mismatch: expected {expected_sha256}, got {actual_sha256}");
    }
    validate_package_magic(package_type, path)
}

fn validate_package_magic(_package_type: PackageType, path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let mut magic = [0_u8; 8];
    let count = file.read(&mut magic)?;
    let valid = {
        #[cfg(windows)]
        {
            count >= 2 && magic[..2] == *b"MZ"
        }
        #[cfg(target_os = "macos")]
        {
            count >= 4
                && matches!(
                    &magic[..4],
                    [0xcf, 0xfa, 0xed, 0xfe] | [0xca, 0xfe, 0xba, 0xbe] | [0xca, 0xfe, 0xba, 0xbf]
                )
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            count >= 4 && magic[..4] == [0x7f, b'E', b'L', b'F']
        }
    };
    if valid {
        Ok(())
    } else {
        bail!("standalone executable signature does not match this operating system")
    }
}

fn file_integrity(path: &Path) -> Result<(u64, String)> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok((size, format!("{:x}", hasher.finalize())))
}

fn standalone_install_marker() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = if cfg!(windows) {
        std::env::var_os("ProgramData")
            .map(|v| PathBuf::from(v).join("OperationMonitoring/install-type"))
            .into_iter()
            .collect()
    } else if cfg!(target_os = "macos") {
        vec![PathBuf::from(
            "/Library/Application Support/OperationMonitoring/install-type",
        )]
    } else {
        vec![
            PathBuf::from("/etc/om-agent/install-type"),
            PathBuf::from("/etc/operation-monitoring-agent/install-type"),
        ]
    };
    candidates
        .into_iter()
        .find(|path| fs::read_to_string(path).is_ok_and(|value| value.trim() == "standalone"))
}

fn standalone_native_arch() -> String {
    #[cfg(windows)]
    {
        return windows_native_arch();
    }
    #[cfg(target_os = "macos")]
    {
        return normalize_macos_arch(std::env::consts::ARCH);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        linux_standalone_native_arch(std::env::consts::ARCH, Path::new("/etc/openwrt_release"))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn linux_standalone_native_arch(arch: &str, openwrt_release: &Path) -> String {
    if arch == "x86_64" && openwrt_release.exists() {
        "x86_64-musl".to_string()
    } else {
        arch.to_string()
    }
}

fn install_standalone(plan: &ApplyPlan, source: &Path) -> Result<()> {
    let target = plan
        .installed_executable
        .as_ref()
        .ok_or_else(|| anyhow!("standalone update plan has no installed executable"))?;
    let temporary = target.with_extension("update-new");
    fs::copy(source, &temporary).with_context(|| {
        format!(
            "failed to stage standalone executable {}",
            temporary.display()
        )
    })?;
    set_installed_executable_permissions(&temporary)?;
    #[cfg(windows)]
    {
        let backup = target.with_extension("update-old.exe");
        let _ = fs::remove_file(&backup);
        fs::rename(target, &backup)?;
        fs::rename(&temporary, target).inspect_err(|_| {
            let _ = fs::rename(&backup, target);
        })?;
        let _ = fs::remove_file(backup);
    }
    #[cfg(not(windows))]
    {
        fs::rename(&temporary, target)?;
    }
    Ok(())
}

fn stop_standalone_service() -> Result<()> {
    #[cfg(windows)]
    {
        for service_name in [LEGACY_SERVICE_NAME, SERVICE_NAME] {
            if query_windows_agent_service(service_name).is_ok() {
                return stop_windows_agent_service(service_name);
            }
        }
        bail!("OM Agent Windows service is not installed");
    }
    #[cfg(target_os = "macos")]
    {
        let spec = CommandSpec {
            program: "/bin/launchctl".into(),
            args: vec![
                "bootout".into(),
                format!("system/{MACOS_SERVICE_LABEL}").into(),
            ],
        };
        let _ = run_command_with_timeout(&spec, SERVICE_STOP_TIMEOUT, "standalone service stop")?;
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let service_name = if Path::new(&format!("/etc/init.d/{SERVICE_NAME}")).exists()
            || Path::new(&format!("/etc/systemd/system/{SERVICE_NAME}.service")).exists()
        {
            SERVICE_NAME
        } else {
            LEGACY_SERVICE_NAME
        };
        let spec = if Path::new(&format!("/etc/init.d/{service_name}")).exists() {
            CommandSpec {
                program: format!("/etc/init.d/{service_name}").into(),
                args: vec!["stop".into()],
            }
        } else {
            CommandSpec {
                program: "systemctl".into(),
                args: vec!["stop".into(), format!("{service_name}.service").into()],
            }
        };
        let _ = run_command_with_timeout(&spec, SERVICE_STOP_TIMEOUT, "standalone service stop")?;
        Ok(())
    }
}

#[cfg(not(windows))]
fn standalone_restart_candidates() -> Vec<CommandSpec> {
    #[cfg(target_os = "macos")]
    {
        return vec![CommandSpec {
            program: "/bin/launchctl".into(),
            args: vec![
                "bootstrap".into(),
                "system".into(),
                format!("/Library/LaunchDaemons/{MACOS_SERVICE_LABEL}.plist").into(),
            ],
        }];
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return [SERVICE_NAME, LEGACY_SERVICE_NAME]
            .into_iter()
            .map(|service_name| {
                if Path::new(&format!("/etc/init.d/{service_name}")).exists() {
                    CommandSpec {
                        program: format!("/etc/init.d/{service_name}").into(),
                        args: vec!["restart".into()],
                    }
                } else {
                    CommandSpec {
                        program: "systemctl".into(),
                        args: vec!["restart".into(), format!("{service_name}.service").into()],
                    }
                }
            })
            .collect();
    }
}

fn detect_update_capability() -> UpdateCapability {
    let privileged = is_update_privileged();
    if standalone_install_marker().is_some() {
        UpdateCapability {
            package_type: Some(PackageType::Standalone.as_str().to_string()),
            native_arch: Some(standalone_native_arch()),
            update_privileged: privileged,
        }
    } else {
        UpdateCapability {
            package_type: None,
            native_arch: None,
            update_privileged: privileged,
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_native_arch() -> String {
    let arch = std::env::var("PROCESSOR_ARCHITEW6432")
        .or_else(|_| std::env::var("PROCESSOR_ARCHITECTURE"))
        .unwrap_or_else(|_| std::env::consts::ARCH.to_string());
    match arch.to_ascii_lowercase().as_str() {
        "amd64" | "x86_64" => "x64".to_string(),
        "arm64" | "aarch64" => "arm64".to_string(),
        "x86" | "i386" | "i586" | "i686" => "x86".to_string(),
        value => value.to_string(),
    }
}

#[cfg(unix)]
fn is_update_privileged() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
fn is_update_privileged() -> bool {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation: TOKEN_ELEVATION = zeroed();
        let mut returned = 0_u32;
        let elevated = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        ) != 0
            && elevation.TokenIsElevated != 0;
        CloseHandle(token);
        elevated
    }
}

#[cfg(not(any(unix, windows)))]
fn is_update_privileged() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn distinguishes_openwrt_x86_64_musl_updates() {
        let directory = std::env::temp_dir().join(format!("om-openwrt-{}", uuid::Uuid::new_v4()));
        let marker = directory.join("openwrt_release");

        assert_eq!(linux_standalone_native_arch("x86_64", &marker), "x86_64");
        fs::create_dir_all(&directory).unwrap();
        fs::write(&marker, "DISTRIB_ID='OpenWrt'\n").unwrap();
        assert_eq!(
            linux_standalone_native_arch("x86_64", &marker),
            "x86_64-musl"
        );
        assert_eq!(linux_standalone_native_arch("aarch64", &marker), "aarch64");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn parses_sha256_sidecar_and_rejects_invalid_content() {
        let digest = "a".repeat(64);
        assert_eq!(
            parse_checksum_sidecar(&format!("{digest}  om-agent.bin\n")).unwrap(),
            digest
        );
        assert_eq!(
            parse_checksum_sidecar(&format!("{}\n", "B".repeat(64))).unwrap(),
            "b".repeat(64)
        );
        assert!(parse_checksum_sidecar("not-a-digest om-agent.bin").is_err());
        assert!(
            parse_checksum_sidecar(&format!("{} om-agent.bin extra\n", "a".repeat(64))).is_err()
        );
    }

    #[test]
    fn downloaded_package_must_match_offer_and_sidecar() {
        #[cfg(windows)]
        let package: &[u8] = b"MZtrusted-executable";
        #[cfg(target_os = "macos")]
        let package: &[u8] = &[0xcf, 0xfa, 0xed, 0xfe, b't', b'r', b'u', b's', b't'];
        #[cfg(all(unix, not(target_os = "macos")))]
        let package: &[u8] = b"\x7fELFtrusted-executable";
        let directory = std::env::temp_dir().join(format!("om-update-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("download.part");
        fs::write(&path, package).unwrap();
        let digest = format!("{:x}", Sha256::digest(package));
        let offer = UpdateOffer {
            release_id: "release-checksum".to_string(),
            version: "9.9.9".to_string(),
            artifact_id: "artifact-checksum".to_string(),
            download_url: "/api/agent/update/artifacts/artifact-checksum/download".to_string(),
            sha256: digest.clone(),
            size_bytes: package.len() as i64,
            package_type: "standalone".to_string(),
            native_arch: standalone_native_arch(),
            retry_count: 0,
        };
        let downloaded = DownloadedPackage {
            temporary_path: path,
            final_path: directory.join("final.standalone"),
            size_bytes: package.len() as u64,
            sha256: digest.clone(),
        };

        assert!(verify_download(&offer, PackageType::Standalone, &downloaded, &digest).is_ok());
        assert!(
            verify_download(
                &offer,
                PackageType::Standalone,
                &downloaded,
                &"b".repeat(64),
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn update_spread_is_stable_and_bounded() {
        let first = update_delay_seconds("instance-a", "artifact-a");
        let second = update_delay_seconds("instance-a", "artifact-a");
        assert_eq!(first, second);
        assert!(first <= 60);
    }

    #[test]
    fn offer_gate_distinguishes_prepared_handoff_and_terminal_retry() {
        let directory = std::env::temp_dir().join(format!("om-update-{}", uuid::Uuid::new_v4()));
        let config = AgentConfig {
            server: "http://127.0.0.1:13500".to_string(),
            identity_file: None,
            report_interval: 5,
            state_dir: None,
            log_file: None,
            update_dir: Some(directory.clone()),
        };
        let paths = UpdatePaths::from_config(&config).unwrap();
        paths.prepare().unwrap();
        let manager = UpdateManager {
            config,
            identity: Identity {
                instance_id: "instance-gate".to_string(),
                secret: "secret-gate".to_string(),
            },
            client: Client::new(),
            activity: ActivityTracker::default(),
            capability: UpdateCapability {
                package_type: Some("standalone".to_string()),
                native_arch: Some("arm64".to_string()),
                update_privileged: true,
            },
            paths: paths.clone(),
        };
        let offer = UpdateOffer {
            release_id: "release-handoff".to_string(),
            version: "9.9.9".to_string(),
            artifact_id: "artifact-handoff".to_string(),
            download_url: "/api/agent/update/artifacts/artifact-handoff/download".to_string(),
            sha256: "a".repeat(64),
            size_bytes: 6,
            package_type: "standalone".to_string(),
            native_arch: "arm64".to_string(),
            retry_count: 0,
        };
        let previous = CachedPackage {
            artifact_id: "artifact-current".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            package_type: PackageType::Standalone,
            native_arch: "arm64".to_string(),
            path: paths.packages.join("current.standalone"),
            retry_count: 0,
            size_bytes: 8,
            sha256: "b".repeat(64),
        };
        write_update_state(
            &paths.state_file,
            &UpdateState {
                schema_version: UPDATE_SCHEMA_VERSION,
                current_package: Some(previous.clone()),
                attempt: Some(PersistedAttempt {
                    offer: offer.clone(),
                    status: UpdateStatus::Verifying,
                    message: None,
                    package_path: Some(paths.packages.join("target.standalone")),
                    previous_package: Some(previous),
                    phase: AttemptPhase::Staging,
                    updated_at: now_ts(),
                }),
            },
        )
        .unwrap();

        assert!(
            manager.can_start_offer(&offer).unwrap(),
            "a socket drop before updater launch must remain retryable"
        );

        let (outbound, mut inbound) = mpsc::unbounded_channel();
        manager
            .mark_handoff_started(&offer, Some("handoff started".to_string()), &outbound)
            .unwrap();
        assert!(matches!(
            inbound.try_recv().unwrap(),
            AgentInbound::UpdateStatus {
                status: UpdateStatus::AwaitingRestart,
                ..
            }
        ));
        let ownership = open_update_lock(&paths.lock_file).unwrap();
        ownership.try_lock_exclusive().unwrap();
        assert!(
            !manager.can_start_offer(&offer).unwrap(),
            "a live updater owner must reject duplicate offers"
        );
        let mut other_offer = offer.clone();
        other_offer.artifact_id = "artifact-other".to_string();
        other_offer.version = "10.0.0".to_string();
        assert!(
            !manager.can_start_offer(&other_offer).unwrap(),
            "a live updater owner must reject other releases too"
        );
        let mut retry_offer = offer.clone();
        retry_offer.retry_count = 1;
        assert!(!manager.can_start_offer(&retry_offer).unwrap());

        let reconnect_status = manager.connected_status().unwrap().unwrap();
        assert!(matches!(
            reconnect_status,
            AgentInbound::UpdateStatus {
                status: UpdateStatus::AwaitingRestart,
                ..
            }
        ));
        let reconnect_state = read_update_state(&paths.state_file).unwrap();
        assert!(matches!(
            reconnect_state.attempt,
            Some(PersistedAttempt {
                status: UpdateStatus::AwaitingRestart,
                phase: AttemptPhase::Target,
                ..
            })
        ));
        assert!(!paths.health_file.exists());
        assert!(
            !manager.can_start_offer(&offer).unwrap(),
            "an old-version service reconnect must not launch a second updater"
        );
        FileExt::unlock(&ownership).unwrap();
        assert!(
            !manager.can_start_offer(&offer).unwrap(),
            "a stale handoff must reject the same retry generation"
        );
        assert!(
            manager.can_start_offer(&retry_offer).unwrap(),
            "a stale handoff must allow a newer retry generation"
        );
        assert!(
            manager.can_start_offer(&other_offer).unwrap(),
            "a stale handoff may recover through a newer release"
        );
        let mut older_offer = other_offer.clone();
        older_offer.version = "9.8.0".to_string();
        assert!(!manager.can_start_offer(&older_offer).unwrap());

        let mut failed_state = read_update_state(&paths.state_file).unwrap();
        let attempt = failed_state.attempt.as_mut().unwrap();
        attempt.status = UpdateStatus::Failed;
        attempt.phase = AttemptPhase::Completed;
        write_update_state(&paths.state_file, &failed_state).unwrap();
        assert!(
            !manager.can_start_offer(&offer).unwrap(),
            "an automatic offer from the same retry generation stays suppressed"
        );
        assert!(
            manager.can_start_offer(&retry_offer).unwrap(),
            "a newer retry generation is an explicit administrator retry"
        );
        assert!(
            manager.can_start_offer(&other_offer).unwrap(),
            "a different artifact may start after the old handoff is terminal"
        );
        drop(ownership);
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[test]
    fn service_command_timeout_terminates_the_process_group() {
        let spec = CommandSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
        };
        let started = Instant::now();
        let error =
            run_command_with_timeout(&spec, Duration::from_millis(50), "test command").unwrap_err();

        assert!(format!("{error:#}").contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_collects_small_control_output() {
        let spec = CommandSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "printf 'STATE : 1 STOPPED'".into()],
        };
        let output =
            run_command_output_with_timeout(&spec, Duration::from_secs(1), "test query").unwrap();

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "STATE : 1 STOPPED"
        );
    }

    #[test]
    fn staged_executable_tampering_is_detected_before_install() {
        let directory = std::env::temp_dir().join(format!("om-update-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let executable = directory.join("agent.bin");
        #[cfg(windows)]
        let original: &[u8] = b"MZtrusted-executable";
        #[cfg(target_os = "macos")]
        let original: &[u8] = &[0xcf, 0xfa, 0xed, 0xfe, b't', b'r', b'u', b's', b't'];
        #[cfg(all(unix, not(target_os = "macos")))]
        let original: &[u8] = b"\x7fELFtrusted-executable";
        fs::write(&executable, original).unwrap();
        let sha256 = format!("{:x}", Sha256::digest(original));

        verify_package_at_rest(
            &executable,
            PackageType::Standalone,
            original.len() as u64,
            &sha256,
        )
        .unwrap();
        fs::write(&executable, b"tampered-executable").unwrap();
        assert!(
            verify_package_at_rest(
                &executable,
                PackageType::Standalone,
                original.len() as u64,
                &sha256
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn delayed_old_plan_cannot_rewrite_a_newer_generation() {
        let directory = std::env::temp_dir().join(format!("om-update-{}", uuid::Uuid::new_v4()));
        let state_file = directory.join("state.json");
        let health_file = directory.join("health.json");
        let lock_file = directory.join("updater.lock");
        fs::create_dir_all(&directory).unwrap();
        let previous_path = directory.join("previous.standalone");
        let target_path = directory.join("target.standalone");
        fs::write(&previous_path, b"previous").unwrap();
        fs::write(&target_path, b"target").unwrap();
        let previous = CachedPackage {
            artifact_id: "artifact-previous".to_string(),
            version: "0.0.1".to_string(),
            package_type: PackageType::Standalone,
            native_arch: "arm64".to_string(),
            path: previous_path.clone(),
            retry_count: 0,
            size_bytes: 8,
            sha256: "b".repeat(64),
        };
        let old_offer = UpdateOffer {
            release_id: "release-target".to_string(),
            version: "1.0.0".to_string(),
            artifact_id: "artifact-target".to_string(),
            download_url: "/api/agent/update/artifacts/artifact-target/download".to_string(),
            sha256: "a".repeat(64),
            size_bytes: 6,
            package_type: "standalone".to_string(),
            native_arch: "arm64".to_string(),
            retry_count: 0,
        };
        let mut new_offer = old_offer.clone();
        new_offer.retry_count = 1;
        write_update_state(
            &state_file,
            &UpdateState {
                schema_version: UPDATE_SCHEMA_VERSION,
                current_package: Some(previous.clone()),
                attempt: Some(PersistedAttempt {
                    offer: new_offer,
                    status: UpdateStatus::Waiting,
                    message: None,
                    package_path: None,
                    previous_package: Some(previous.clone()),
                    phase: AttemptPhase::Staging,
                    updated_at: now_ts(),
                }),
            },
        )
        .unwrap();
        let old_plan = ApplyPlan {
            offer: old_offer,
            package_path: target_path,
            previous_package: Some(previous),
            state_file: state_file.clone(),
            health_file,
            lock_file,
            lock_owner_file: directory.join("updater-owner.json"),
            lock_owner: "old-owner".to_string(),
            old_pid: 1,
            installed_executable: None,
        };

        let ownership = acquire_worker_ownership(&old_plan).unwrap();
        wait_for_worker_ownership(&old_plan, Duration::from_millis(10)).unwrap();
        let mut wrong_owner = old_plan.clone();
        wrong_owner.lock_owner = "different-owner".to_string();
        assert!(
            wait_for_worker_ownership(&wrong_owner, Duration::from_millis(10)).is_err(),
            "the parent handshake must verify the worker owner token"
        );
        drop(ownership);
        assert!(ensure_plan_generation_is_current(&old_plan).is_err());

        persist_apply_status(
            &old_plan,
            UpdateStatus::Failed,
            AttemptPhase::Completed,
            Some("late failure".to_string()),
        )
        .unwrap();
        complete_target_update(&old_plan, PackageType::Standalone).unwrap();

        let state = read_update_state(&state_file).unwrap();
        assert_eq!(state.attempt.unwrap().offer.retry_count, 1);
        assert_eq!(
            state.current_package.unwrap().artifact_id,
            "artifact-previous"
        );
        assert!(previous_path.exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn parses_windows_service_query_states() {
        assert_eq!(
            parse_sc_query_state("        STATE              : 1  STOPPED\r\n"),
            Some(ScServiceState::Stopped)
        );
        assert_eq!(
            parse_sc_query_state("        STATE              : 4  RUNNING\r\n"),
            Some(ScServiceState::Running)
        );
        assert_eq!(
            parse_sc_query_state("        STATE              : 2  START_PENDING\r\n"),
            Some(ScServiceState::Other)
        );
    }

    #[test]
    fn connected_target_finalizes_cache_before_signaling_health() {
        let directory = std::env::temp_dir().join(format!("om-update-{}", uuid::Uuid::new_v4()));
        let config = AgentConfig {
            server: "http://127.0.0.1:13500".to_string(),
            identity_file: None,
            report_interval: 5,
            state_dir: None,
            log_file: None,
            update_dir: Some(directory.clone()),
        };
        let paths = UpdatePaths::from_config(&config).unwrap();
        paths.prepare().unwrap();
        let manager = UpdateManager {
            config,
            identity: Identity {
                instance_id: "instance-test".to_string(),
                secret: "secret-test".to_string(),
            },
            client: Client::new(),
            activity: ActivityTracker::default(),
            capability: UpdateCapability {
                package_type: Some("standalone".to_string()),
                native_arch: Some("arm64".to_string()),
                update_privileged: true,
            },
            paths: paths.clone(),
        };
        let previous_path = paths.packages.join("previous.standalone");
        let target_path = paths.packages.join("target.standalone");
        fs::write(&previous_path, b"previous").unwrap();
        fs::write(&target_path, b"target").unwrap();
        let previous = CachedPackage {
            artifact_id: "artifact-previous".to_string(),
            version: "0.0.1".to_string(),
            package_type: PackageType::Standalone,
            native_arch: "arm64".to_string(),
            path: previous_path.clone(),
            retry_count: 0,
            size_bytes: 8,
            sha256: "b".repeat(64),
        };
        let offer = UpdateOffer {
            release_id: "release-target".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            artifact_id: "artifact-target".to_string(),
            download_url: "/api/agent/update/artifacts/artifact-target/download".to_string(),
            sha256: "a".repeat(64),
            size_bytes: 6,
            package_type: "standalone".to_string(),
            native_arch: "arm64".to_string(),
            retry_count: 0,
        };
        write_update_state(
            &paths.state_file,
            &UpdateState {
                schema_version: UPDATE_SCHEMA_VERSION,
                current_package: Some(previous.clone()),
                attempt: Some(PersistedAttempt {
                    offer: offer.clone(),
                    status: UpdateStatus::AwaitingRestart,
                    message: Some("waiting for restart".to_string()),
                    package_path: Some(target_path.clone()),
                    previous_package: Some(previous.clone()),
                    phase: AttemptPhase::Target,
                    updated_at: now_ts(),
                }),
            },
        )
        .unwrap();

        let status = manager.connected_status().unwrap().unwrap();
        assert!(matches!(
            status,
            AgentInbound::UpdateStatus {
                status: UpdateStatus::Succeeded,
                message: None,
                ..
            }
        ));
        let state = read_update_state(&paths.state_file).unwrap();
        assert_eq!(
            state.current_package.as_ref().map(|value| &value.path),
            Some(&target_path)
        );
        assert!(matches!(
            state.attempt.as_ref(),
            Some(PersistedAttempt {
                status: UpdateStatus::Succeeded,
                phase: AttemptPhase::Completed,
                ..
            })
        ));
        assert!(!manager.can_start_offer(&offer).unwrap());
        assert!(wait_for_health(
            &paths.health_file,
            "artifact-target",
            env!("CARGO_PKG_VERSION"),
            0,
            Duration::from_millis(10),
        ));

        let mut next_state = read_update_state(&paths.state_file).unwrap();
        let mut next_offer = offer.clone();
        next_offer.release_id = "release-next".to_string();
        next_offer.artifact_id = "artifact-next".to_string();
        next_offer.version = "9.9.9".to_string();
        next_state.attempt = Some(PersistedAttempt {
            offer: next_offer,
            status: UpdateStatus::Waiting,
            message: None,
            package_path: None,
            previous_package: next_state.current_package.clone(),
            phase: AttemptPhase::Staging,
            updated_at: now_ts(),
        });
        write_update_state(&paths.state_file, &next_state).unwrap();

        let plan = ApplyPlan {
            offer,
            package_path: target_path,
            previous_package: Some(previous),
            state_file: paths.state_file.clone(),
            health_file: paths.health_file.clone(),
            lock_file: paths.lock_file.clone(),
            lock_owner_file: paths.lock_owner_file.clone(),
            lock_owner: "test-owner".to_string(),
            old_pid: 1,
            installed_executable: None,
        };
        complete_target_update(&plan, PackageType::Standalone).unwrap();
        assert!(!previous_path.exists());
        assert_eq!(
            read_update_state(&paths.state_file)
                .unwrap()
                .attempt
                .as_ref()
                .map(|attempt| attempt.offer.artifact_id.as_str()),
            Some("artifact-next")
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn state_file_is_owner_only_on_unix() {
        let directory = std::env::temp_dir().join(format!("om-update-{}", uuid::Uuid::new_v4()));
        let path = directory.join("state.json");
        write_update_state(&path, &UpdateState::default()).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = fs::remove_dir_all(directory);
    }
}
