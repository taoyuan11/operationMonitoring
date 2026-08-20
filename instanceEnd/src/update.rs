use std::{
    collections::hash_map::DefaultHasher,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    hash::{Hash, Hasher},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    str::FromStr,
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use directories::ProjectDirs;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use fs2::{FileExt, available_space};
use futures_util::StreamExt;
use reqwest::Client;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::{
    activity::ActivityTracker,
    config::AgentConfig,
    http::{AGENT_CONTROL_HTTP_TIMEOUT, MAX_AGENT_JSON_RESPONSE_BYTES, bounded_json_response},
    models::{AgentInbound, Identity, RollbackOffer, RollbackPackage, UpdateOffer, UpdateStatus},
    outbound::AgentEventSender,
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
const PACKAGE_INSPECTION_TIMEOUT: Duration = Duration::from_secs(10);
const PARENT_HANDOFF_GRACE: Duration = Duration::from_secs(1);
#[cfg(windows)]
const WINDOWS_FILE_REPLACE_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const DISK_RESERVE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CHECKSUM_FILE_BYTES: usize = 4096;
const MAX_AGENT_UPDATE_RETRY_COUNT: i64 = 100;
const FIRST_BASELINE_PRESERVING_UPDATER_VERSION: &str = "0.1.23";
const LEGACY_UPDATE_SIGNATURE_DOMAIN: &str = "operation-monitoring-agent-update-v1";
const UPDATE_SIGNATURE_V2_DOMAIN: &str = "operation-monitoring-agent-update-v2";
const ROLLBACK_SIGNATURE_DOMAIN: &str = "operation-monitoring-agent-rollback-v1";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareResult {
    ReadyToApply { offer: UpdateOffer },
    Finished,
}

#[derive(Debug, Deserialize)]
struct UpdateManifest {
    update: Option<UpdateOffer>,
    #[serde(default)]
    rollback: Option<RollbackOffer>,
}

#[derive(Debug, Clone)]
struct UpdatePaths {
    root: PathBuf,
    configured_root: bool,
    packages: PathBuf,
    state_file: PathBuf,
    health_file: PathBuf,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AttemptOperation {
    #[default]
    Upgrade,
    Rollback {
        attempt_id: String,
        release_id: String,
        from_version: String,
        target_version: String,
        retry_count: i64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedAttempt {
    offer: UpdateOffer,
    #[serde(default)]
    operation: AttemptOperation,
    #[serde(default)]
    manual: bool,
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
    #[serde(default)]
    rollback_package: Option<CachedPackage>,
    attempt: Option<PersistedAttempt>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            schema_version: UPDATE_SCHEMA_VERSION,
            current_package: None,
            rollback_package: None,
            attempt: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApplyPlan {
    offer: UpdateOffer,
    #[serde(default)]
    operation: AttemptOperation,
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

pub fn rollback_baseline_version(config: &AgentConfig) -> Option<String> {
    let paths = UpdatePaths::from_config(config).ok()?;
    let state = read_update_state(&paths.state_file).ok()?;
    let package = state.rollback_package?;
    if package.package_type != PackageType::Standalone
        || package.native_arch != standalone_native_arch()
        || verify_package_at_rest(
            &package.path,
            package.package_type,
            package.size_bytes,
            &package.sha256,
        )
        .is_err()
    {
        return None;
    }
    Some(package.version)
}

pub(crate) fn update_handoff_pending(config: &AgentConfig) -> Result<bool> {
    let paths = UpdatePaths::from_config(config)?;
    let state = read_update_state(&paths.state_file)?;
    Ok(state.attempt.is_some_and(|attempt| {
        matches!(attempt.phase, AttemptPhase::Target | AttemptPhase::Rollback)
            && matches!(
                attempt.status,
                UpdateStatus::Installing | UpdateStatus::AwaitingRestart
            )
    }))
}

pub fn force_update(config: &AgentConfig, package: &Path) -> Result<()> {
    let capability = update_capability();
    if capability.package_type.as_deref() != Some(PackageType::Standalone.as_str()) {
        bail!("forced updates require a managed standalone installation");
    }
    if !capability.update_privileged {
        bail!("forced updates require root or administrator privileges");
    }

    let source = fs::canonicalize(package)
        .with_context(|| format!("failed to resolve update package {}", package.display()))?;
    if !source.is_file() {
        bail!("update package {} is not a regular file", source.display());
    }
    validate_package_magic(PackageType::Standalone, &source)?;

    let installed_executable = installed_standalone_executable()?;
    let mut force_config = config.clone();
    if force_config.update_dir.is_none() && force_config.state_dir.is_none() {
        force_config.update_dir = Some(installed_standalone_update_dir()?);
    }
    let paths = UpdatePaths::from_config(&force_config)?;
    paths.prepare()?;
    if update_lock_is_held(&paths.lock_file)? {
        bail!("another updater is already running; wait for it to finish before forcing an update");
    }

    let operation_id = uuid::Uuid::new_v4().to_string();
    let staged_path = paths.packages.join(format!(
        "manual-{}{}",
        safe_component(&operation_id),
        std::env::consts::EXE_SUFFIX
    ));
    fs::copy(&source, &staged_path).with_context(|| {
        format!(
            "failed to stage forced update package {}",
            staged_path.display()
        )
    })?;
    let prepared = (|| {
        set_owner_only_executable(&staged_path)?;
        let version = inspect_agent_package_version(&staged_path)?;
        let (size_bytes, sha256) = file_integrity(&staged_path)?;
        verify_package_at_rest(&staged_path, PackageType::Standalone, size_bytes, &sha256)?;
        Result::<_>::Ok((version, size_bytes, sha256))
    })();
    let (version, size_bytes, sha256) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = fs::remove_file(&staged_path);
            return Err(error);
        }
    };
    let offer = UpdateOffer {
        attempt_id: None,
        instance_id: None,
        release_id: format!("manual-{operation_id}"),
        version: version.clone(),
        artifact_id: format!("manual-{operation_id}"),
        download_url: format!("local://{}", source.display()),
        sha256,
        size_bytes: i64::try_from(size_bytes).context("forced update package is too large")?,
        package_type: PackageType::Standalone.as_str().to_string(),
        native_arch: standalone_native_arch(),
        target_os: Some(standalone_target_os().to_string()),
        signature_key_id: None,
        signature: None,
        signature_v2: None,
        retry_count: 0,
    };

    let mut state = read_update_state(&paths.state_file)?;
    state.attempt = Some(PersistedAttempt {
        offer: offer.clone(),
        operation: AttemptOperation::Upgrade,
        manual: true,
        status: UpdateStatus::AwaitingRestart,
        message: Some(format!("forced update from {}", source.display())),
        package_path: Some(staged_path.clone()),
        previous_package: None,
        phase: AttemptPhase::Target,
        updated_at: now_ts(),
    });
    write_update_state(&paths.state_file, &state)?;

    let manager = UpdateManager {
        config: force_config,
        identity: Identity {
            instance_id: "manual-update".to_string(),
            secret: String::new(),
            credential_version: 1,
            previous_secret: None,
        },
        client: crate::tls::http_client(),
        activity: ActivityTracker::default(),
        capability,
        paths,
    };
    // This is the recovery path for a broken automatic handoff. The state is armed before
    // launch and the worker observes a grace period, so success depends only on starting the
    // detached process, not on the parent being able to inspect the Windows ownership lock.
    if let Err(error) = manager.spawn_updater_for_executable(
        &offer,
        staged_path,
        installed_executable.clone(),
        false,
    ) {
        let message = format!("failed to launch forced updater: {error:#}");
        let mut state = read_update_state(&manager.paths.state_file)?;
        if let Some(attempt) = &mut state.attempt
            && attempt.offer.artifact_id == offer.artifact_id
        {
            attempt.status = UpdateStatus::Failed;
            attempt.message = Some(message.clone());
            attempt.phase = AttemptPhase::Completed;
            attempt.updated_at = now_ts();
            write_update_state(&manager.paths.state_file, &state)?;
        }
        bail!(message);
    }

    println!("forced update to {version} has been handed off");
    println!("target: {}", installed_executable.display());
    println!("the agent service will restart automatically");
    Ok(())
}

fn inspect_agent_package_version(path: &Path) -> Result<String> {
    let spec = CommandSpec {
        program: path.as_os_str().to_owned(),
        args: vec!["--version".into()],
    };
    let output = run_command_output_with_timeout(
        &spec,
        PACKAGE_INSPECTION_TIMEOUT,
        "forced update package inspection",
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        bail!(
            "update package --version exited with {}: {}{}",
            output.status,
            stdout.trim(),
            stderr.trim()
        );
    }
    parse_agent_package_version(&stdout)
}

fn parse_agent_package_version(output: &str) -> Result<String> {
    let version = output
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("om-agent "))
        .context("update package did not identify itself as om-agent")?;
    Ok(Version::parse(version.trim())?.to_string())
}

fn installed_standalone_update_dir() -> Result<PathBuf> {
    let marker = standalone_install_marker().context("standalone install marker is missing")?;
    #[cfg(any(windows, target_os = "macos"))]
    {
        return Ok(marker
            .parent()
            .context("standalone install marker has no parent directory")?
            .join("updates"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if marker.starts_with("/etc/om-agent") {
            Ok(PathBuf::from("/var/lib/om-agent/updates"))
        } else {
            Ok(PathBuf::from("/var/lib/operation-monitoring-agent/updates"))
        }
    }
}

fn installed_standalone_executable() -> Result<PathBuf> {
    let preferred = if cfg!(windows) {
        PathBuf::from(std::env::var_os("ProgramFiles").context("ProgramFiles is missing")?)
            .join("OM Agent/om-agent.exe")
    } else if cfg!(target_os = "macos") {
        PathBuf::from("/usr/local/bin/om-agent")
    } else if Path::new("/etc/openwrt_release").exists() {
        PathBuf::from("/usr/bin/om-agent")
    } else {
        PathBuf::from("/usr/local/bin/om-agent")
    };
    Ok(preferred)
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

    pub async fn fetch_manifest(&self) -> Result<(Option<UpdateOffer>, Option<RollbackOffer>)> {
        let url = self
            .config
            .server_endpoint()?
            .http_url("api/agent/update/manifest")?;
        let response = self
            .client
            .get(url)
            .timeout(AGENT_CONTROL_HTTP_TIMEOUT)
            .header(AGENT_ID_HEADER, &self.identity.instance_id)
            .header(AGENT_SECRET_HEADER, &self.identity.secret)
            .send()
            .await?
            .error_for_status()?;
        let manifest = bounded_json_response::<UpdateManifest>(
            response,
            MAX_AGENT_JSON_RESPONSE_BYTES,
            "agent update manifest",
        )
        .await?;
        Ok((manifest.update, manifest.rollback))
    }

    pub fn rollback_version(&self) -> Option<String> {
        rollback_baseline_version(&self.config)
    }

    pub fn connected_status(&self) -> Result<Option<AgentInbound>> {
        let mut state = read_update_state(&self.paths.state_file)?;
        let Some(attempt) = state.attempt.clone() else {
            return Ok(None);
        };

        let current_version = env!("CARGO_PKG_VERSION");
        let (status, health, message, finalize) = match attempt.phase {
            AttemptPhase::Target if current_version == attempt.offer.version => {
                let status = target_success_status(&attempt.operation);
                let message = matches!(attempt.operation, AttemptOperation::Rollback { .. })
                    .then(|| attempt.message.clone())
                    .flatten();
                (
                    status,
                    Some((
                        attempt.offer.artifact_id.clone(),
                        attempt.offer.version.clone(),
                        attempt.offer.retry_count,
                    )),
                    message,
                    true,
                )
            }
            AttemptPhase::Rollback => match &attempt.previous_package {
                Some(previous) if current_version == previous.version => {
                    let status = restored_previous_status(&attempt.operation);
                    (
                        status,
                        Some((
                            previous.artifact_id.clone(),
                            previous.version.clone(),
                            previous.retry_count,
                        )),
                        attempt.message.clone(),
                        true,
                    )
                }
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
            AttemptPhase::Completed
                if attempt.status == UpdateStatus::RollbackSucceeded
                    && matches!(attempt.operation, AttemptOperation::Rollback { .. })
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
            AttemptPhase::Completed
                if attempt.status == UpdateStatus::RollbackSucceeded
                    && matches!(attempt.operation, AttemptOperation::Upgrade) =>
            {
                match &attempt.previous_package {
                    Some(previous) if current_version == previous.version => (
                        UpdateStatus::RollbackSucceeded,
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

        if let Some((artifact_id, version, retry_count)) = &health {
            write_json_atomic(
                &self.paths.health_file,
                &HealthMarker {
                    artifact_id: artifact_id.clone(),
                    version: version.clone(),
                    retry_count: *retry_count,
                    connected_at: now_ts(),
                },
            )?;
        }

        let mut stale_package = None;
        let mut finalized_log = None;
        if finalize {
            if status == target_success_status(&attempt.operation) {
                stale_package = rotate_successful_package_state(&mut state, &attempt)?;
            }
            if let Some(current_attempt) = &mut state.attempt
                && offer_generation_matches(&current_attempt.offer, &attempt.offer)
            {
                current_attempt.status = status;
                current_attempt.message = message.clone();
                current_attempt.phase = AttemptPhase::Completed;
                current_attempt.updated_at = now_ts();
                finalized_log = Some(attempt_status_log_line(
                    current_attempt,
                    status,
                    current_attempt.message.as_deref(),
                ));
            }
            write_update_state(&self.paths.state_file, &state)?;
            if let Some(log_line) = finalized_log.as_deref() {
                log_update_status(status, log_line);
            }
        }
        if let Some(path) = stale_package {
            let _ = fs::remove_file(path);
        }

        if attempt.manual {
            return Ok(None);
        }

        Ok(Some(attempt_status_message(&attempt, status, message)))
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
            if same_offer_attempt(&attempt.offer, offer) {
                return Ok(offer.retry_count > attempt.offer.retry_count);
            }
            let persisted_version = Version::parse(&attempt.offer.version)
                .with_context(|| format!("invalid persisted version {}", attempt.offer.version))?;
            let incoming_version = Version::parse(&offer.version)
                .with_context(|| format!("invalid offered version {}", offer.version))?;
            return Ok(incoming_version > persisted_version);
        }
        if !same_offer_attempt(&attempt.offer, offer) {
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

    pub fn can_start_rollback(&self, offer: &RollbackOffer) -> Result<bool> {
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
            return Ok(false);
        }
        let terminal = matches!(
            attempt.status,
            UpdateStatus::Succeeded | UpdateStatus::RollbackSucceeded | UpdateStatus::Failed
        );
        match attempt.operation {
            AttemptOperation::Rollback {
                attempt_id,
                retry_count,
                ..
            } if attempt_id == offer.attempt_id => {
                if terminal {
                    Ok(offer.retry_count > retry_count)
                } else {
                    Ok(offer.retry_count >= retry_count)
                }
            }
            _ => Ok(terminal),
        }
    }

    pub fn cancel_preparation(&self) {
        self.activity.stop_draining();
    }

    pub async fn prepare(&self, offer: UpdateOffer, outbound: AgentEventSender) -> PrepareResult {
        let package_type = match self.validate_offer(&offer) {
            Ok(package_type) => package_type,
            Err(error) => {
                self.activity.stop_draining();
                let message = format!("{error:#}");
                crate::logging::error(format_args!(
                    "rejected agent update offer: {} error={message:?}",
                    update_offer_log_fields(&offer)
                ));
                let _ = outbound.send(update_status_message(
                    &offer,
                    UpdateStatus::Failed,
                    Some(message),
                ));
                return PrepareResult::Finished;
            }
        };
        crate::logging::info(format_args!(
            "accepted agent update offer: current_version={:?} {}",
            env!("CARGO_PKG_VERSION"),
            update_offer_log_fields(&offer)
        ));
        match self.prepare_inner(&offer, package_type, &outbound).await {
            Ok(result) => result,
            Err(error) => {
                self.activity.stop_draining();
                let message = format!("{error:#}");
                crate::logging::error(format_args!(
                    "agent update preparation failed: {} error={message:?}",
                    update_offer_log_fields(&offer)
                ));
                if let Err(persist_error) =
                    self.send_status(&offer, UpdateStatus::Failed, Some(message), &outbound)
                {
                    crate::logging::error(format_args!(
                        "failed to persist update failure: {persist_error:#}"
                    ));
                }
                PrepareResult::Finished
            }
        }
    }

    async fn prepare_inner(
        &self,
        offer: &UpdateOffer,
        package_type: PackageType,
        outbound: &AgentEventSender,
    ) -> Result<PrepareResult> {
        self.begin_attempt(offer, AttemptOperation::Upgrade)?;

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
        let downloaded = self
            .download_to_temporary(offer, package_type, &offer_storage_component(offer))
            .await?;

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

        Ok(PrepareResult::ReadyToApply {
            offer: offer.clone(),
        })
    }

    pub async fn prepare_rollback(
        &self,
        rollback: RollbackOffer,
        outbound: AgentEventSender,
    ) -> PrepareResult {
        match self.prepare_rollback_inner(&rollback, &outbound).await {
            Ok(result) => result,
            Err(error) => {
                self.activity.stop_draining();
                let message = format!("{error:#}");
                crate::logging::error(format_args!(
                    "agent rollback preparation failed: {} error={message:?}",
                    rollback_offer_log_fields(&rollback)
                ));
                if let Ok(state) = read_update_state(&self.paths.state_file)
                    && state.attempt.as_ref().is_some_and(|attempt| {
                        matches!(
                            &attempt.operation,
                            AttemptOperation::Rollback { attempt_id, retry_count, .. }
                                if attempt_id == &rollback.attempt_id
                                    && *retry_count == rollback.retry_count
                        )
                    })
                {
                    if let Err(persist_error) = self.send_status_for_current_attempt(
                        UpdateStatus::Failed,
                        Some(message),
                        &outbound,
                    ) {
                        crate::logging::error(format_args!(
                            "failed to persist rollback failure: {persist_error:#}"
                        ));
                    }
                } else {
                    let _ = outbound.send(rollback_status_message(
                        &rollback,
                        UpdateStatus::Failed,
                        Some(message),
                    ));
                }
                PrepareResult::Finished
            }
        }
    }

    async fn prepare_rollback_inner(
        &self,
        rollback: &RollbackOffer,
        outbound: &AgentEventSender,
    ) -> Result<PrepareResult> {
        self.validate_rollback_offer(rollback)?;
        crate::logging::info(format_args!(
            "accepted agent rollback offer: current_version={:?} {}",
            env!("CARGO_PKG_VERSION"),
            rollback_offer_log_fields(rollback)
        ));
        let current_version = Version::parse(env!("CARGO_PKG_VERSION"))?;
        let from_version = Version::parse(&rollback.from_version).with_context(|| {
            format!("invalid rollback source version {}", rollback.from_version)
        })?;
        let target_version = Version::parse(&rollback.target_version).with_context(|| {
            format!(
                "invalid rollback target version {}",
                rollback.target_version
            )
        })?;
        if current_version == target_version {
            let _ = outbound.send(rollback_status_message(
                rollback,
                UpdateStatus::RollbackSucceeded,
                Some("rollback target version is already running".to_string()),
            ));
            return Ok(PrepareResult::Finished);
        }
        if current_version != from_version {
            bail!(
                "rollback source mismatch: running {current_version}, instruction expects {from_version}"
            );
        }
        if target_version >= from_version {
            bail!("rollback target {target_version} must be lower than source {from_version}");
        }

        let local_baseline = self.local_rollback_package(&rollback.target_version);
        if rollback.package.is_none() && local_baseline.is_none() {
            bail!("no matching server package or local rollback baseline is available");
        }
        let mut effective =
            rollback_effective_offer(rollback, rollback.package.as_ref(), local_baseline.as_ref())?;
        let operation = AttemptOperation::Rollback {
            attempt_id: rollback.attempt_id.clone(),
            release_id: rollback.release_id.clone(),
            from_version: rollback.from_version.clone(),
            target_version: rollback.target_version.clone(),
            retry_count: rollback.retry_count,
        };
        self.begin_attempt(&effective, operation)?;

        let delay = update_delay_seconds(&self.identity.instance_id, &rollback.attempt_id);
        self.send_status(
            &effective,
            UpdateStatus::Waiting,
            Some(format!(
                "rollback will start after a {delay} second spread delay"
            )),
            outbound,
        )?;
        tokio::time::sleep(Duration::from_secs(delay)).await;

        let package_path = if rollback.package.is_some() {
            self.send_status(&effective, UpdateStatus::Downloading, None, outbound)?;
            let server_download = async {
                let checksum_sha256 = self.download_checksum(&effective).await?;
                let downloaded = self
                    .download_to_temporary(
                        &effective,
                        PackageType::Standalone,
                        &format!("rollback-{}", rollback.attempt_id),
                    )
                    .await?;
                self.send_status(&effective, UpdateStatus::Verifying, None, outbound)?;
                verify_download(
                    &effective,
                    PackageType::Standalone,
                    &downloaded,
                    &checksum_sha256,
                )?;
                replace_file(&downloaded.temporary_path, &downloaded.final_path)?;
                Result::<PathBuf>::Ok(downloaded.final_path)
            }
            .await;
            match server_download {
                Ok(path) => path,
                Err(server_error) => {
                    let Some(local) = local_baseline else {
                        return Err(server_error.context("server rollback package failed"));
                    };
                    verify_package_at_rest(
                        &local.path,
                        local.package_type,
                        local.size_bytes,
                        &local.sha256,
                    )
                    .context("local rollback baseline verification failed")?;
                    let local_effective = rollback_effective_offer(rollback, None, Some(&local))?;
                    self.replace_current_attempt_offer(&effective, &local_effective)?;
                    effective = local_effective;
                    self.send_status(
                        &effective,
                        UpdateStatus::Verifying,
                        Some(format!(
                            "server rollback package failed; using local baseline: {server_error:#}"
                        )),
                        outbound,
                    )?;
                    local.path
                }
            }
        } else {
            let local = local_baseline.context("local rollback baseline disappeared")?;
            self.send_status(
                &effective,
                UpdateStatus::Verifying,
                Some("using locally cached rollback baseline".to_string()),
                outbound,
            )?;
            verify_package_at_rest(
                &local.path,
                local.package_type,
                local.size_bytes,
                &local.sha256,
            )?;
            local.path
        };
        self.set_package_path(&effective, package_path)?;

        self.activity.start_draining();
        let active = self.activity.active_count();
        if active > 0 {
            self.send_status(
                &effective,
                UpdateStatus::WaitingIdle,
                Some(format!(
                    "waiting for {active} active command or terminal session(s)"
                )),
                outbound,
            )?;
        }
        self.activity.wait_until_idle().await;
        Ok(PrepareResult::ReadyToApply { offer: effective })
    }

    pub fn launch_prepared_update(&self, offer: &UpdateOffer, outbound: &AgentEventSender) -> bool {
        crate::logging::info(format_args!(
            "launching detached updater: {}",
            update_offer_log_fields(offer)
        ));
        let result = (|| {
            let state = read_update_state(&self.paths.state_file)?;
            let package_path = state
                .attempt
                .as_ref()
                .filter(|attempt| offer_generation_matches(&attempt.offer, offer))
                .and_then(|attempt| attempt.package_path.clone())
                .ok_or_else(|| anyhow!("prepared update package is missing from update state"))?;
            self.spawn_updater(offer, package_path)
        })();

        if let Err(error) = result {
            self.activity.stop_draining();
            let message = format!("failed to launch detached updater: {error:#}");
            crate::logging::error(format_args!(
                "agent update handoff failed: {} error={message:?}",
                update_offer_log_fields(offer)
            ));
            if let Err(persist_error) =
                self.send_status(offer, UpdateStatus::Failed, Some(message), outbound)
            {
                crate::logging::error(format_args!(
                    "failed to persist updater launch failure: {persist_error:#}"
                ));
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
            crate::logging::error(format_args!(
                "failed to persist updater handoff status: {error:#}"
            ));
        }
        true
    }

    fn validate_offer(&self, offer: &UpdateOffer) -> Result<PackageType> {
        if !self.capability.update_privileged {
            bail!("agent lacks root or administrator privileges required for updates");
        }
        validate_update_instance_binding(offer, &self.identity.instance_id)?;
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
        let expected_os = standalone_target_os();
        if let Some(target_os) = offer.target_os.as_deref()
            && target_os != expected_os
        {
            bail!(
                "target operating system mismatch: agent requires {expected_os}, offer is {target_os}"
            );
        }
        if offer.size_bytes <= 0 {
            bail!("update package size must be positive");
        }
        if !(0..=MAX_AGENT_UPDATE_RETRY_COUNT).contains(&offer.retry_count) {
            bail!("update retry count must be between 0 and {MAX_AGENT_UPDATE_RETRY_COUNT}");
        }
        if offer.sha256.len() != 64 || !offer.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("update package SHA-256 is invalid");
        }
        Version::parse(&offer.version)
            .with_context(|| format!("invalid target version {}", offer.version))?;
        if offer.artifact_id.is_empty()
            || offer.artifact_id.len() > 128
            || !offer
                .artifact_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            bail!("update artifact ID is invalid");
        }
        let expected_download_url =
            format!("/api/agent/update/artifacts/{}/download", offer.artifact_id);
        if offer.download_url != expected_download_url {
            bail!("update download URL must be an agent update API path");
        }
        verify_update_signature_policy(offer, self.config.server_endpoint()?.is_https())?;
        offer.package_type.parse()
    }

    fn validate_rollback_offer(&self, offer: &RollbackOffer) -> Result<()> {
        if !self.capability.update_privileged {
            bail!("agent lacks root or administrator privileges required for rollback");
        }
        if self.capability.package_type.as_deref() != Some(PackageType::Standalone.as_str()) {
            bail!("this process is not a managed standalone installation");
        }
        if offer.instance_id != self.identity.instance_id {
            bail!("rollback instruction targets a different agent instance");
        }
        for (name, value) in [
            ("attempt_id", offer.attempt_id.as_str()),
            ("release_id", offer.release_id.as_str()),
        ] {
            if value.is_empty()
                || value.len() > 128
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            {
                bail!("rollback {name} is invalid");
            }
        }
        if !(0..=MAX_AGENT_UPDATE_RETRY_COUNT).contains(&offer.retry_count) {
            bail!("rollback retry count must be between 0 and {MAX_AGENT_UPDATE_RETRY_COUNT}");
        }
        Version::parse(&offer.from_version)
            .with_context(|| format!("invalid rollback source version {}", offer.from_version))?;
        Version::parse(&offer.target_version)
            .with_context(|| format!("invalid rollback target version {}", offer.target_version))?;
        if let Some(package) = &offer.package {
            self.validate_rollback_package(package)?;
        }
        verify_rollback_signature_policy(offer, self.config.server_endpoint()?.is_https())
    }

    fn validate_rollback_package(&self, package: &RollbackPackage) -> Result<()> {
        let local_package_type = self
            .capability
            .package_type
            .as_deref()
            .context("local update package type is unavailable")?;
        if package.package_type != local_package_type {
            bail!(
                "rollback package type mismatch: agent requires {local_package_type}, offer is {}",
                package.package_type
            );
        }
        let local_arch = self
            .capability
            .native_arch
            .as_deref()
            .context("local native architecture is unavailable")?;
        if package.native_arch != local_arch {
            bail!(
                "rollback native architecture mismatch: agent requires {local_arch}, offer is {}",
                package.native_arch
            );
        }
        if package.target_os != standalone_target_os() {
            bail!(
                "rollback target operating system mismatch: agent requires {}, offer is {}",
                standalone_target_os(),
                package.target_os
            );
        }
        if package.size_bytes <= 0
            || package.sha256.len() != 64
            || !package.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("rollback package integrity metadata is invalid");
        }
        let expected_download_url = format!(
            "/api/agent/update/artifacts/{}/download",
            package.artifact_id
        );
        if package.download_url != expected_download_url {
            bail!("rollback download URL must be an agent update API path");
        }
        Ok(())
    }

    fn local_rollback_package(&self, version: &str) -> Option<CachedPackage> {
        let state = read_update_state(&self.paths.state_file).ok()?;
        state.rollback_package.filter(|package| {
            package.version == version
                && package.package_type == PackageType::Standalone
                && self
                    .capability
                    .native_arch
                    .as_deref()
                    .is_some_and(|arch| arch == package.native_arch)
                && package.path.is_file()
        })
    }

    async fn download_to_temporary(
        &self,
        offer: &UpdateOffer,
        package_type: PackageType,
        component: &str,
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

        let basename = safe_component(component);
        let final_path = self
            .paths
            .packages
            .join(format!("{basename}.{}", package_type.extension()));
        let temporary_path = self.paths.packages.join(format!("{basename}.part"));
        let _ = fs::remove_file(&temporary_path);

        let url = self
            .config
            .server_endpoint()?
            .http_url(offer.download_url.trim_start_matches('/'))?;
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
            sha256: crate::hex::encode_lower(hasher.finalize()),
        })
    }

    async fn download_checksum(&self, offer: &UpdateOffer) -> Result<String> {
        let checksum_path = offer
            .download_url
            .strip_suffix("/download")
            .context("update download URL has no download suffix")?;
        let checksum_url = self.config.server_endpoint()?.http_url(&format!(
            "{}/checksum",
            checksum_path.trim_start_matches('/')
        ))?;
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

    fn begin_attempt(&self, offer: &UpdateOffer, operation: AttemptOperation) -> Result<()> {
        let mut state = read_update_state(&self.paths.state_file)?;
        state.attempt = Some(PersistedAttempt {
            offer: offer.clone(),
            operation,
            manual: false,
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
        outbound: &AgentEventSender,
    ) -> Result<()> {
        self.send_status_for_attempt(Some(offer), status, message, outbound)
    }

    fn send_status_for_current_attempt(
        &self,
        status: UpdateStatus,
        message: Option<String>,
        outbound: &AgentEventSender,
    ) -> Result<()> {
        self.send_status_for_attempt(None, status, message, outbound)
    }

    fn send_status_for_attempt(
        &self,
        expected: Option<&UpdateOffer>,
        status: UpdateStatus,
        message: Option<String>,
        outbound: &AgentEventSender,
    ) -> Result<()> {
        let mut state = read_update_state(&self.paths.state_file)?;
        let attempt = state
            .attempt
            .as_mut()
            .filter(|attempt| {
                expected.is_none_or(|expected| offer_generation_matches(&attempt.offer, expected))
            })
            .ok_or_else(|| {
                let expected = expected.map(offer_generation_label);
                anyhow!(
                    "update attempt {} is no longer current",
                    expected.as_deref().unwrap_or("current")
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
        let outbound_message = attempt_status_message(attempt, status, message);
        let log_line = attempt_status_log_line(attempt, status, attempt.message.as_deref());
        write_update_state(&self.paths.state_file, &state)?;
        let _ = outbound.send(outbound_message);
        log_update_status(status, &log_line);
        Ok(())
    }

    fn set_package_path(&self, offer: &UpdateOffer, path: PathBuf) -> Result<()> {
        mutate_attempt(&self.paths.state_file, offer, |attempt| {
            attempt.package_path = Some(path);
        })
    }

    fn replace_current_attempt_offer(
        &self,
        previous: &UpdateOffer,
        replacement: &UpdateOffer,
    ) -> Result<()> {
        let mut state = read_update_state(&self.paths.state_file)?;
        let attempt = state
            .attempt
            .as_mut()
            .filter(|attempt| offer_generation_matches(&attempt.offer, previous))
            .ok_or_else(|| anyhow!("rollback attempt is no longer current"))?;
        attempt.offer = replacement.clone();
        attempt.updated_at = now_ts();
        write_update_state(&self.paths.state_file, &state)
    }

    fn mark_handoff_started(
        &self,
        offer: &UpdateOffer,
        message: Option<String>,
        outbound: &AgentEventSender,
    ) -> Result<()> {
        let mut state = read_update_state(&self.paths.state_file)?;
        let attempt = state
            .attempt
            .as_mut()
            .filter(|attempt| offer_generation_matches(&attempt.offer, offer))
            .ok_or_else(|| anyhow!("prepared update attempt is missing from update state"))?;
        attempt.status = UpdateStatus::AwaitingRestart;
        attempt.message = message.clone();
        attempt.phase = AttemptPhase::Target;
        attempt.updated_at = now_ts();
        let outbound_message =
            attempt_status_message(attempt, UpdateStatus::AwaitingRestart, message.clone());
        let log_line = attempt_status_log_line(
            attempt,
            UpdateStatus::AwaitingRestart,
            attempt.message.as_deref(),
        );
        write_update_state(&self.paths.state_file, &state)?;
        let _ = outbound.send(outbound_message);
        log_update_status(UpdateStatus::AwaitingRestart, &log_line);
        Ok(())
    }

    fn spawn_updater(&self, offer: &UpdateOffer, package_path: PathBuf) -> Result<()> {
        self.spawn_updater_for_executable(offer, package_path, std::env::current_exe()?, true)
    }

    fn spawn_updater_for_executable(
        &self,
        offer: &UpdateOffer,
        package_path: PathBuf,
        installed_executable: PathBuf,
        verify_ownership: bool,
    ) -> Result<()> {
        crate::privileged_path::validate_configured_directory(
            &self.paths.root,
            self.paths.configured_root,
            "agent update directory",
        )?;
        let mut state = read_update_state(&self.paths.state_file)?;
        let mut previous_package = state
            .attempt
            .as_ref()
            .filter(|attempt| offer_generation_matches(&attempt.offer, offer))
            .and_then(|attempt| attempt.previous_package.clone());
        let operation = state
            .attempt
            .as_ref()
            .filter(|attempt| offer_generation_matches(&attempt.offer, offer))
            .map(|attempt| attempt.operation.clone())
            .ok_or_else(|| anyhow!("prepared update attempt is missing from update state"))?;
        if previous_package.is_none()
            && offer.package_type == PackageType::Standalone.as_str()
            && installed_executable.is_file()
        {
            let rollback_path = self.paths.packages.join(format!(
                "standalone-rollback-{}{}",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::EXE_SUFFIX
            ));
            fs::copy(&installed_executable, &rollback_path)?;
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
            && offer_generation_matches(&attempt.offer, offer)
            && attempt.previous_package.is_none()
        {
            attempt.previous_package = previous_package.clone();
            write_update_state(&self.paths.state_file, &state)?;
        }
        let component = safe_component(&offer_storage_component(offer));
        let plan_path = self.paths.root.join(format!("apply-{component}.json"));
        let executable_suffix = std::env::consts::EXE_SUFFIX;
        let updater_path = self
            .paths
            .root
            .join(format!("updater-{component}{executable_suffix}"));
        let plan = ApplyPlan {
            offer: offer.clone(),
            operation,
            package_path,
            previous_package,
            state_file: self.paths.state_file.clone(),
            health_file: self.paths.health_file.clone(),
            lock_file: self.paths.lock_file.clone(),
            lock_owner_file: self.paths.lock_owner_file.clone(),
            lock_owner: uuid::Uuid::new_v4().to_string(),
            old_pid: std::process::id(),
            installed_executable: Some(installed_executable),
        };
        write_json_atomic(&plan_path, &plan)?;

        if updater_path.try_exists()? {
            crate::privileged_path::validate_regular_file(
                &updater_path,
                "detached updater executable",
            )?;
            fs::remove_file(&updater_path)?;
        }
        copy_private_executable(&std::env::current_exe()?, &updater_path).with_context(|| {
            format!(
                "failed to create detached updater {}",
                updater_path.display()
            )
        })?;
        crate::privileged_path::validate_regular_file(
            &updater_path,
            "detached updater executable",
        )?;

        let spawned_with_systemd = try_spawn_systemd_updater(
            &updater_path,
            &plan_path,
            &component,
            self.config.log_max_bytes,
            self.config.log_history,
        )?;
        if !spawned_with_systemd {
            let mut command = Command::new(&updater_path);
            command
                .arg("apply-update")
                .arg("--plan-file")
                .arg(&plan_path)
                .arg("--log-max-bytes")
                .arg(self.config.log_max_bytes.to_string())
                .arg("--log-history")
                .arg(self.config.log_history.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            detach(&mut command);
            command
                .spawn()
                .with_context(|| format!("failed to start updater {}", updater_path.display()))?;
        }
        if verify_ownership {
            wait_for_worker_ownership(&plan, WORKER_LOCK_TIMEOUT)?;
        }
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
    #[cfg(windows)]
    crate::windows_security::replace_file(source, target).with_context(|| {
        format!(
            "failed to atomically replace {} with {}",
            target.display(),
            source.display()
        )
    })?;
    #[cfg(not(windows))]
    fs::rename(source, target).with_context(|| {
        format!(
            "failed to atomically replace {} with {}",
            target.display(),
            source.display()
        )
    })?;
    #[cfg(unix)]
    {
        let parent = target
            .parent()
            .ok_or_else(|| anyhow!("{} has no parent directory", target.display()))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("failed to sync directory {}", parent.display()))?;
    }
    Ok(())
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

fn same_offer_attempt(left: &UpdateOffer, right: &UpdateOffer) -> bool {
    match (&left.attempt_id, &right.attempt_id) {
        (Some(left), Some(right)) => left == right,
        (None, None) => left.artifact_id == right.artifact_id,
        _ => false,
    }
}

fn offer_generation_matches(left: &UpdateOffer, right: &UpdateOffer) -> bool {
    same_offer_attempt(left, right) && left.retry_count == right.retry_count
}

fn offer_generation_label(offer: &UpdateOffer) -> String {
    format!(
        "{} generation {}",
        offer.attempt_id.as_deref().unwrap_or(&offer.artifact_id),
        offer.retry_count
    )
}

pub(crate) fn update_offer_log_fields(offer: &UpdateOffer) -> String {
    format!(
        "version={:?} attempt_id={:?} release_id={:?} artifact_id={:?} retry_count={} size_bytes={}",
        offer.version,
        offer.attempt_id.as_deref(),
        offer.release_id,
        offer.artifact_id,
        offer.retry_count,
        offer.size_bytes
    )
}

pub(crate) fn rollback_offer_log_fields(offer: &RollbackOffer) -> String {
    let package = offer.package.as_ref().map_or_else(
        || "package_source=local".to_string(),
        |package| {
            format!(
                "package_source=server_or_local artifact_id={:?} size_bytes={}",
                package.artifact_id, package.size_bytes
            )
        },
    );
    format!(
        "from_version={:?} target_version={:?} attempt_id={:?} release_id={:?} retry_count={} {package}",
        offer.from_version,
        offer.target_version,
        offer.attempt_id,
        offer.release_id,
        offer.retry_count
    )
}

fn update_status_label(status: UpdateStatus) -> &'static str {
    match status {
        UpdateStatus::Waiting => "waiting",
        UpdateStatus::Downloading => "downloading",
        UpdateStatus::Verifying => "verifying",
        UpdateStatus::WaitingIdle => "waiting_idle",
        UpdateStatus::Installing => "installing",
        UpdateStatus::AwaitingRestart => "awaiting_restart",
        UpdateStatus::Succeeded => "succeeded",
        UpdateStatus::RollbackSucceeded => "rollback_succeeded",
        UpdateStatus::Failed => "failed",
    }
}

fn attempt_phase_label(phase: AttemptPhase) -> &'static str {
    match phase {
        AttemptPhase::Staging => "staging",
        AttemptPhase::Target => "target",
        AttemptPhase::Rollback => "rollback",
        AttemptPhase::Completed => "completed",
    }
}

fn attempt_status_log_line(
    attempt: &PersistedAttempt,
    status: UpdateStatus,
    message: Option<&str>,
) -> String {
    let message = message
        .map(|message| format!(" message={message:?}"))
        .unwrap_or_default();
    match &attempt.operation {
        AttemptOperation::Upgrade => format!(
            "agent update status: status={} phase={} {}{message}",
            update_status_label(status),
            attempt_phase_label(attempt.phase),
            update_offer_log_fields(&attempt.offer)
        ),
        AttemptOperation::Rollback {
            attempt_id,
            release_id,
            from_version,
            target_version,
            retry_count,
        } => format!(
            "agent rollback status: status={} phase={} from_version={from_version:?} target_version={target_version:?} attempt_id={attempt_id:?} release_id={release_id:?} artifact_id={:?} retry_count={retry_count}{message}",
            update_status_label(status),
            attempt_phase_label(attempt.phase),
            attempt.offer.artifact_id
        ),
    }
}

fn log_update_status(status: UpdateStatus, line: &str) {
    if status == UpdateStatus::Failed {
        crate::logging::error(format_args!("{line}"));
    } else {
        crate::logging::info(format_args!("{line}"));
    }
}

fn offer_storage_component(offer: &UpdateOffer) -> String {
    format!(
        "{}-retry-{}",
        offer.attempt_id.as_deref().unwrap_or(&offer.artifact_id),
        offer.retry_count
    )
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
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(UpdateState::default()),
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
    let result = (|| -> Result<()> {
        let mut file = options.open(&temporary)?;
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn update_status_message(
    offer: &UpdateOffer,
    status: UpdateStatus,
    message: Option<String>,
) -> AgentInbound {
    AgentInbound::UpdateStatus {
        attempt_id: offer.attempt_id.clone(),
        release_id: offer.release_id.clone(),
        artifact_id: offer.artifact_id.clone(),
        version: offer.version.clone(),
        retry_count: offer.retry_count,
        status,
        message,
    }
}

fn rollback_status_message(
    offer: &RollbackOffer,
    status: UpdateStatus,
    message: Option<String>,
) -> AgentInbound {
    AgentInbound::RollbackStatus {
        attempt_id: offer.attempt_id.clone(),
        retry_count: offer.retry_count,
        status,
        message,
    }
}

fn attempt_status_message(
    attempt: &PersistedAttempt,
    status: UpdateStatus,
    message: Option<String>,
) -> AgentInbound {
    match &attempt.operation {
        AttemptOperation::Upgrade => update_status_message(&attempt.offer, status, message),
        AttemptOperation::Rollback {
            attempt_id,
            retry_count,
            ..
        } => AgentInbound::RollbackStatus {
            attempt_id: attempt_id.clone(),
            retry_count: *retry_count,
            status,
            message,
        },
    }
}

fn target_success_status(operation: &AttemptOperation) -> UpdateStatus {
    match operation {
        AttemptOperation::Upgrade => UpdateStatus::Succeeded,
        AttemptOperation::Rollback { .. } => UpdateStatus::RollbackSucceeded,
    }
}

fn restored_previous_status(operation: &AttemptOperation) -> UpdateStatus {
    match operation {
        AttemptOperation::Upgrade => UpdateStatus::RollbackSucceeded,
        AttemptOperation::Rollback { .. } => UpdateStatus::Failed,
    }
}

fn cached_package_from_attempt(attempt: &PersistedAttempt) -> Result<CachedPackage> {
    let package_type: PackageType = attempt.offer.package_type.parse()?;
    let package_path = attempt
        .package_path
        .clone()
        .ok_or_else(|| anyhow!("installed update package is missing from state"))?;
    Ok(CachedPackage {
        artifact_id: attempt.offer.artifact_id.clone(),
        version: attempt.offer.version.clone(),
        package_type,
        native_arch: attempt.offer.native_arch.clone(),
        path: package_path,
        retry_count: attempt.offer.retry_count,
        size_bytes: u64::try_from(attempt.offer.size_bytes)
            .context("invalid installed package size")?,
        sha256: attempt.offer.sha256.clone(),
    })
}

fn rotate_successful_package_state(
    state: &mut UpdateState,
    attempt: &PersistedAttempt,
) -> Result<Option<PathBuf>> {
    let target = cached_package_from_attempt(attempt)?;
    let next_baseline = preserve_baseline_for_legacy_updater(attempt)?;
    let stale = state
        .rollback_package
        .as_ref()
        .map(|package| package.path.clone())
        .filter(|path| path != &target.path)
        .filter(|path| {
            next_baseline
                .as_ref()
                .is_none_or(|package| path != &package.path)
        });
    state.current_package = Some(target);
    state.rollback_package = next_baseline;
    Ok(stale)
}

fn preserve_baseline_for_legacy_updater(
    attempt: &PersistedAttempt,
) -> Result<Option<CachedPackage>> {
    let Some(mut previous) = attempt.previous_package.clone() else {
        return Ok(None);
    };
    if !matches!(attempt.operation, AttemptOperation::Upgrade)
        || !legacy_updater_removes_previous_package(&previous.version)
    {
        return Ok(Some(previous));
    }

    let parent = previous
        .path
        .parent()
        .context("cached rollback package has no parent directory")?;
    let component = safe_component(&format!(
        "{}-retry-{}-{}",
        previous.artifact_id, previous.retry_count, previous.sha256
    ));
    let preserved_path = parent.join(format!(
        "standalone-baseline-{component}{}",
        std::env::consts::EXE_SUFFIX
    ));
    if preserved_path == previous.path {
        return Ok(Some(previous));
    }

    if verify_package_at_rest(
        &preserved_path,
        previous.package_type,
        previous.size_bytes,
        &previous.sha256,
    )
    .is_err()
    {
        let temporary = parent.join(format!(".standalone-baseline-{}.tmp", uuid::Uuid::new_v4()));
        let copied = (|| {
            if fs::hard_link(&previous.path, &temporary).is_err() {
                fs::copy(&previous.path, &temporary).with_context(|| {
                    format!(
                        "failed to preserve rollback package {}",
                        previous.path.display()
                    )
                })?;
            }
            set_owner_only_executable(&temporary)?;
            verify_package_at_rest(
                &temporary,
                previous.package_type,
                previous.size_bytes,
                &previous.sha256,
            )?;
            replace_file(&temporary, &preserved_path)
        })();
        if copied.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        copied?;
    }
    previous.path = preserved_path;
    Ok(Some(previous))
}

fn legacy_updater_removes_previous_package(version: &str) -> bool {
    let first_safe = Version::parse(FIRST_BASELINE_PRESERVING_UPDATER_VERSION)
        .expect("baseline-preserving updater version must be valid SemVer");
    Version::parse(version).map_or(true, |version| version < first_safe)
}

fn rollback_effective_offer(
    rollback: &RollbackOffer,
    server_package: Option<&RollbackPackage>,
    local_package: Option<&CachedPackage>,
) -> Result<UpdateOffer> {
    if let Some(package) = server_package {
        return Ok(UpdateOffer {
            attempt_id: Some(rollback.attempt_id.clone()),
            instance_id: None,
            release_id: rollback.release_id.clone(),
            version: rollback.target_version.clone(),
            artifact_id: package.artifact_id.clone(),
            download_url: package.download_url.clone(),
            sha256: package.sha256.clone(),
            size_bytes: package.size_bytes,
            package_type: package.package_type.clone(),
            native_arch: package.native_arch.clone(),
            target_os: Some(package.target_os.clone()),
            signature_key_id: None,
            signature: None,
            signature_v2: None,
            retry_count: rollback.retry_count,
        });
    }
    let package = local_package.context("local rollback package is unavailable")?;
    Ok(UpdateOffer {
        attempt_id: Some(rollback.attempt_id.clone()),
        instance_id: None,
        release_id: rollback.release_id.clone(),
        version: rollback.target_version.clone(),
        artifact_id: package.artifact_id.clone(),
        download_url: format!("local://{}", package.path.display()),
        sha256: package.sha256.clone(),
        size_bytes: i64::try_from(package.size_bytes)
            .context("local rollback package is too large")?,
        package_type: package.package_type.as_str().to_string(),
        native_arch: package.native_arch.clone(),
        target_os: Some(standalone_target_os().to_string()),
        signature_key_id: None,
        signature: None,
        signature_v2: None,
        retry_count: rollback.retry_count,
    })
}

fn mutate_attempt(
    state_file: &Path,
    expected: &UpdateOffer,
    mutate: impl FnOnce(&mut PersistedAttempt),
) -> Result<()> {
    let mut state = read_update_state(state_file)?;
    let attempt = state
        .attempt
        .as_mut()
        .filter(|attempt| offer_generation_matches(&attempt.offer, expected))
        .ok_or_else(|| anyhow!("update attempt is no longer current"))?;
    mutate(attempt);
    attempt.updated_at = now_ts();
    write_update_state(state_file, &state)
}

fn copy_private_executable(source: &Path, target: &Path) -> Result<()> {
    let mut source_file = File::open(source)?;
    if !source_file.metadata()?.is_file() {
        bail!("updater source {} is not a regular file", source.display());
    }
    let mut target_file = create_new_executable(target)?;
    io::copy(&mut source_file, &mut target_file)?;
    target_file.sync_all()?;
    drop(target_file);
    set_owner_only_executable(target)
}

fn create_new_executable(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    if crate::privileged_path::is_process_privileged() {
        return crate::windows_security::create_private_file(path);
    }
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o700).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

#[cfg(unix)]
fn set_owner_only_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
#[cfg(windows)]
fn set_owner_only_directory(path: &Path) -> Result<()> {
    if crate::privileged_path::is_process_privileged() {
        crate::windows_security::restrict_to_system_and_administrators(path)?;
    }
    Ok(())
}
#[cfg(not(any(unix, windows)))]
fn set_owner_only_directory(_path: &Path) -> Result<()> {
    Ok(())
}
#[cfg(unix)]
fn set_owner_only_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
#[cfg(windows)]
fn set_owner_only_executable(path: &Path) -> Result<()> {
    if crate::privileged_path::is_process_privileged() {
        crate::windows_security::restrict_to_system_and_administrators(path)?;
    }
    Ok(())
}
#[cfg(not(any(unix, windows)))]
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
                Err(io::Error::last_os_error())
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
    log_max_bytes: u64,
    log_history: usize,
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
            "--log-max-bytes".into(),
            log_max_bytes.to_string().into(),
            "--log-history".into(),
            log_history.to_string().into(),
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
            Err(error) if update_lock_is_contended(&error) => {
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
        Err(error) if update_lock_is_contended(&error) => Ok(true),
        Err(error) => Err(error).context("failed to inspect updater ownership lock"),
    }
}

fn update_lock_is_contended(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        || error
            .raw_os_error()
            .is_some_and(|code| fs2::lock_contended_error().raw_os_error() == Some(code))
}

impl UpdatePaths {
    fn from_config(config: &AgentConfig) -> Result<Self> {
        let configured_root = config.update_dir.is_some() || config.state_dir.is_some();
        let root = if let Some(path) = &config.update_dir {
            path.clone()
        } else if let Some(path) = &config.state_dir {
            path.join("updates")
        } else if let Some(dirs) = ProjectDirs::from("com", "operation-monitoring", "agent") {
            dirs.data_local_dir().join("updates")
        } else {
            std::env::current_dir()?.join(".operation-monitoring-updates")
        };
        Ok(Self {
            packages: root.join("packages"),
            state_file: root.join("state.json"),
            health_file: root.join("health.json"),
            lock_file: root.join("updater.lock"),
            lock_owner_file: root.join("updater-owner.json"),
            configured_root,
            root,
        })
    }

    fn prepare(&self) -> Result<()> {
        crate::privileged_path::prepare_configured_directory(
            &self.root,
            self.configured_root,
            "agent update directory",
        )?;
        crate::privileged_path::prepare_configured_directory(
            &self.packages,
            self.configured_root,
            "agent update package directory",
        )?;
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

pub(crate) fn docker_protected_update_root(config: &AgentConfig) -> Result<PathBuf> {
    Ok(UpdatePaths::from_config(config)?.root)
}

pub fn apply_update(plan_file: &Path) -> Result<()> {
    if crate::privileged_path::is_process_privileged() {
        let parent = plan_file
            .parent()
            .context("update plan path has no parent directory")?;
        crate::privileged_path::validate_configured_directory(
            parent,
            true,
            "updater plan directory",
        )?;
        crate::privileged_path::validate_regular_file(plan_file, "updater plan")?;
    }
    let content = fs::read_to_string(plan_file)
        .with_context(|| format!("failed to read update plan {}", plan_file.display()))?;
    let plan: ApplyPlan = serde_json::from_str(&content)?;
    let _ownership = acquire_worker_ownership(&plan)?;
    crate::logging::info(format_args!(
        "detached updater started: operation={} old_pid={} {} package_path={}",
        match &plan.operation {
            AttemptOperation::Upgrade => "upgrade",
            AttemptOperation::Rollback { .. } => "rollback",
        },
        plan.old_pid,
        update_offer_log_fields(&plan.offer),
        plan.package_path.display()
    ));
    thread::sleep(PARENT_HANDOFF_GRACE);

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
    crate::logging::info(format_args!(
        "updater stopping agent service: old_pid={} {}",
        plan.old_pid,
        update_offer_log_fields(&plan.offer)
    ));
    stop_standalone_service()?;
    wait_for_process_exit(plan.old_pid, OLD_PROCESS_TIMEOUT)?;
    crate::logging::info(format_args!(
        "previous agent process stopped; beginning installation: {}",
        update_offer_log_fields(&plan.offer)
    ));
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

    crate::logging::info(format_args!(
        "agent package installed; requesting service restart: {}",
        update_offer_log_fields(&plan.offer)
    ));
    persist_apply_status(
        plan,
        UpdateStatus::AwaitingRestart,
        AttemptPhase::Target,
        None,
    )?;
    if let Err(error) = restart_agent_service(package_type) {
        crate::logging::error(format_args!(
            "failed to request agent service restart: {error:#}"
        ));
    }

    crate::logging::info(format_args!(
        "waiting for updated agent health confirmation: timeout_seconds={} {}",
        HEALTH_TIMEOUT.as_secs(),
        update_offer_log_fields(&plan.offer)
    ));
    if wait_for_health(
        &plan.health_file,
        &plan.offer.artifact_id,
        &plan.offer.version,
        plan.offer.retry_count,
        HEALTH_TIMEOUT,
    ) {
        complete_target_update(plan, package_type)?;
        crate::logging::info(format_args!(
            "agent update completed and is healthy: {}",
            update_offer_log_fields(&plan.offer)
        ));
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
    let current = state
        .attempt
        .as_ref()
        .is_some_and(|attempt| offer_generation_matches(&attempt.offer, &plan.offer));
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
        offer_generation_matches(&attempt.offer, &plan.offer)
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

    crate::logging::error(format_args!(
        "{reason}; rolling back to agent {} from {}",
        previous.version,
        previous.path.display()
    ));
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
        crate::logging::error(format_args!(
            "failed to request rolled-back service restart: {error:#}"
        ));
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
    let final_status = restored_previous_status(&plan.operation);
    let final_message = match &plan.operation {
        AttemptOperation::Upgrade => reason,
        AttemptOperation::Rollback { .. } => format!(
            "rollback target failed; restored agent {}: {reason}",
            previous.version
        ),
    };
    persist_apply_status(
        plan,
        final_status,
        AttemptPhase::Completed,
        Some(final_message),
    )?;
    if matches!(&plan.operation, AttemptOperation::Upgrade) {
        crate::logging::info(format_args!(
            "agent rollback to {} succeeded",
            previous.version
        ));
        Ok(())
    } else {
        bail!(
            "rollback target failed and agent {} was restored",
            previous.version
        )
    }
}

fn complete_target_update(plan: &ApplyPlan, package_type: PackageType) -> Result<()> {
    let mut state = read_update_state(&plan.state_file)?;
    let target_already_cached = state.current_package.as_ref().is_some_and(|package| {
        package.artifact_id == plan.offer.artifact_id
            && package.retry_count == plan.offer.retry_count
    });
    if target_already_cached {
        if let (Some(previous), Some(preserved)) =
            (&plan.previous_package, state.rollback_package.as_ref())
            && previous.path != plan.package_path
            && previous.path != preserved.path
            && state
                .current_package
                .as_ref()
                .is_none_or(|current| previous.path != current.path)
        {
            let _ = fs::remove_file(&previous.path);
        }
        return Ok(());
    }
    let plan_is_current = state
        .attempt
        .as_ref()
        .is_some_and(|attempt| offer_generation_matches(&attempt.offer, &plan.offer));
    if !plan_is_current {
        return Ok(());
    }
    let attempt = state
        .attempt
        .as_ref()
        .cloned()
        .context("current update attempt disappeared")?;
    let mut target = cached_package_from_attempt(&attempt)?;
    target.package_type = package_type;
    let stale = state
        .rollback_package
        .as_ref()
        .map(|package| package.path.clone())
        .filter(|path| path != &target.path)
        .filter(|path| {
            plan.previous_package
                .as_ref()
                .is_none_or(|package| path != &package.path)
        });
    state.current_package = Some(target);
    state.rollback_package = plan.previous_package.clone();
    if let Some(current_attempt) = &mut state.attempt {
        current_attempt.status = target_success_status(&plan.operation);
        if matches!(&plan.operation, AttemptOperation::Upgrade) {
            current_attempt.message = None;
        }
        current_attempt.phase = AttemptPhase::Completed;
        current_attempt.updated_at = now_ts();
    }
    write_update_state(&plan.state_file, &state)?;
    if let Some(path) = stale {
        let _ = fs::remove_file(path);
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
    let Some(attempt) = state
        .attempt
        .as_mut()
        .filter(|attempt| offer_generation_matches(&attempt.offer, &plan.offer))
    else {
        return Ok(());
    };
    let finalized_for_same_phase = attempt.phase == AttemptPhase::Completed
        && matches!(
            (attempt.status, phase),
            (UpdateStatus::Succeeded, AttemptPhase::Target)
                | (UpdateStatus::RollbackSucceeded, AttemptPhase::Target)
                | (UpdateStatus::RollbackSucceeded, AttemptPhase::Rollback)
                | (UpdateStatus::Failed, AttemptPhase::Rollback)
        );
    if attempt.phase == AttemptPhase::Completed
        && matches!(
            attempt.status,
            UpdateStatus::Succeeded | UpdateStatus::RollbackSucceeded | UpdateStatus::Failed
        )
        && attempt.status == status
    {
        return Ok(());
    }
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
    let log_line = attempt_status_log_line(attempt, status, attempt.message.as_deref());
    write_update_state(&plan.state_file, &state)?;
    log_update_status(status, &log_line);
    Ok(())
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
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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

fn verify_update_signature_policy(offer: &UpdateOffer, server_is_https: bool) -> Result<()> {
    let embedded_key = embedded_update_verifying_key()?;
    verify_update_signature_policy_with_key(
        offer,
        server_is_https,
        embedded_key
            .as_ref()
            .map(|(key_id, key)| (key_id.as_str(), key)),
    )
}

fn verify_rollback_signature_policy(offer: &RollbackOffer, server_is_https: bool) -> Result<()> {
    let embedded_key = embedded_update_verifying_key()?;
    verify_rollback_signature_policy_with_key(
        offer,
        server_is_https,
        embedded_key
            .as_ref()
            .map(|(key_id, key)| (key_id.as_str(), key)),
    )
}

fn verify_rollback_signature_policy_with_key(
    offer: &RollbackOffer,
    server_is_https: bool,
    embedded_key: Option<(&str, &VerifyingKey)>,
) -> Result<()> {
    match embedded_key {
        Some((key_id, verifying_key)) => verify_rollback_signature(offer, key_id, verifying_key),
        None if server_is_https => Ok(()),
        None => bail!(
            "automatic rollback over HTTP requires an Ed25519 public key embedded with OM_UPDATE_PUBLIC_KEY"
        ),
    }
}

fn verify_update_signature_policy_with_key(
    offer: &UpdateOffer,
    server_is_https: bool,
    embedded_key: Option<(&str, &VerifyingKey)>,
) -> Result<()> {
    match embedded_key {
        Some((key_id, verifying_key)) if offer.signature_v2.is_some() => {
            verify_update_signature_v2(offer, key_id, verifying_key)
        }
        Some((key_id, verifying_key)) if server_is_https => {
            verify_legacy_update_signature(offer, key_id, verifying_key)
        }
        Some(_) => bail!("automatic updates over HTTP require an update metadata v2 signature"),
        None if server_is_https => Ok(()),
        None => bail!(
            "automatic updates over HTTP require an Ed25519 public key embedded with OM_UPDATE_PUBLIC_KEY"
        ),
    }
}

fn validate_update_instance_binding(offer: &UpdateOffer, instance_id: &str) -> Result<()> {
    if offer.signature_v2.is_none() {
        return Ok(());
    }
    let target_instance = offer
        .instance_id
        .as_deref()
        .context("update metadata v2 offer is missing instance_id")?;
    if target_instance != instance_id {
        bail!("update instruction targets a different agent instance");
    }
    Ok(())
}

fn embedded_update_verifying_key() -> Result<Option<(String, VerifyingKey)>> {
    let Some(encoded_key) = option_env!("OM_UPDATE_PUBLIC_KEY") else {
        return Ok(None);
    };
    let key_bytes = BASE64_STANDARD
        .decode(encoded_key.trim())
        .context("embedded OM_UPDATE_PUBLIC_KEY is not valid Base64")?;
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| anyhow!("embedded OM_UPDATE_PUBLIC_KEY must decode to 32 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&key_bytes)
        .context("embedded OM_UPDATE_PUBLIC_KEY is not a valid Ed25519 public key")?;
    let key_id = option_env!("OM_UPDATE_PUBLIC_KEY_ID")
        .context("embedded OM_UPDATE_PUBLIC_KEY_ID is missing")?
        .trim()
        .to_string();
    if !valid_signature_key_id(&key_id) {
        bail!("embedded OM_UPDATE_PUBLIC_KEY_ID is invalid");
    }
    Ok(Some((key_id, verifying_key)))
}

fn verify_legacy_update_signature(
    offer: &UpdateOffer,
    expected_key_id: &str,
    verifying_key: &VerifyingKey,
) -> Result<()> {
    let key_id = offer
        .signature_key_id
        .as_deref()
        .context("signed update offer is missing signature_key_id")?;
    if !valid_signature_key_id(key_id) || key_id != expected_key_id {
        bail!("update signature key ID is not trusted");
    }
    let encoded_signature = offer
        .signature
        .as_deref()
        .context("signed update offer is missing signature")?;
    let signature_bytes = BASE64_STANDARD
        .decode(encoded_signature)
        .context("update signature is not valid Base64")?;
    let signature = Signature::from_slice(&signature_bytes)
        .context("update signature must decode to 64 bytes")?;
    let payload = legacy_update_signature_payload(offer)?;
    verifying_key
        .verify(payload.as_bytes(), &signature)
        .context("legacy update signature verification failed")
}

fn verify_update_signature_v2(
    offer: &UpdateOffer,
    expected_key_id: &str,
    verifying_key: &VerifyingKey,
) -> Result<()> {
    let key_id = offer
        .signature_key_id
        .as_deref()
        .context("signed update offer is missing signature_key_id")?;
    if !valid_signature_key_id(key_id) || key_id != expected_key_id {
        bail!("update signature key ID is not trusted");
    }
    let encoded_signature = offer
        .signature_v2
        .as_deref()
        .context("signed update offer is missing signature_v2")?;
    let signature_bytes = BASE64_STANDARD
        .decode(encoded_signature)
        .context("update signature_v2 is not valid Base64")?;
    let signature = Signature::from_slice(&signature_bytes)
        .context("update signature_v2 must decode to 64 bytes")?;
    let payload = update_signature_payload_v2(offer)?;
    verifying_key
        .verify(payload.as_bytes(), &signature)
        .context("update metadata v2 signature verification failed")
}

fn verify_rollback_signature(
    offer: &RollbackOffer,
    expected_key_id: &str,
    verifying_key: &VerifyingKey,
) -> Result<()> {
    let key_id = offer
        .signature_key_id
        .as_deref()
        .context("signed rollback offer is missing signature_key_id")?;
    if !valid_signature_key_id(key_id) || key_id != expected_key_id {
        bail!("rollback signature key ID is not trusted");
    }
    let encoded_signature = offer
        .signature
        .as_deref()
        .context("signed rollback offer is missing signature")?;
    let signature_bytes = BASE64_STANDARD
        .decode(encoded_signature)
        .context("rollback signature is not valid Base64")?;
    let signature = Signature::from_slice(&signature_bytes)
        .context("rollback signature must decode to 64 bytes")?;
    let payload = rollback_signature_payload(offer)?;
    verifying_key
        .verify(payload.as_bytes(), &signature)
        .context("rollback signature verification failed")
}

fn valid_signature_key_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn legacy_update_signature_payload(offer: &UpdateOffer) -> Result<String> {
    let target_os = offer
        .target_os
        .as_deref()
        .context("signed update offer is missing target_os")?;
    for (name, value) in [
        ("version", offer.version.as_str()),
        ("target_os", target_os),
        ("package_type", offer.package_type.as_str()),
        ("native_arch", offer.native_arch.as_str()),
        ("sha256", offer.sha256.as_str()),
    ] {
        if value.is_empty()
            || value
                .bytes()
                .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
        {
            bail!("update signature field {name} is invalid");
        }
    }
    Ok(format!(
        "{LEGACY_UPDATE_SIGNATURE_DOMAIN}\nversion={}\ntarget_os={target_os}\npackage_type={}\nnative_arch={}\nsize_bytes={}\nsha256={}\n",
        offer.version,
        offer.package_type,
        offer.native_arch,
        offer.size_bytes,
        offer.sha256.to_ascii_lowercase(),
    ))
}

fn update_signature_payload_v2(offer: &UpdateOffer) -> Result<String> {
    let attempt_id = offer
        .attempt_id
        .as_deref()
        .context("signed update offer is missing attempt_id")?;
    let instance_id = offer
        .instance_id
        .as_deref()
        .context("signed update offer is missing instance_id")?;
    let target_os = offer
        .target_os
        .as_deref()
        .context("signed update offer is missing target_os")?;
    for (name, value) in [
        ("attempt_id", attempt_id),
        ("instance_id", instance_id),
        ("release_id", offer.release_id.as_str()),
        ("version", offer.version.as_str()),
        ("artifact_id", offer.artifact_id.as_str()),
        ("download_url", offer.download_url.as_str()),
        ("target_os", target_os),
        ("package_type", offer.package_type.as_str()),
        ("native_arch", offer.native_arch.as_str()),
        ("sha256", offer.sha256.as_str()),
    ] {
        if value.is_empty()
            || value
                .bytes()
                .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
        {
            bail!("update v2 signature field {name} is invalid");
        }
    }
    if offer.size_bytes <= 0 || !(0..=MAX_AGENT_UPDATE_RETRY_COUNT).contains(&offer.retry_count) {
        bail!("update v2 signature numeric fields are invalid");
    }
    Ok(format!(
        "{UPDATE_SIGNATURE_V2_DOMAIN}\nattempt_id={attempt_id}\ninstance_id={instance_id}\nrelease_id={}\nversion={}\nretry_count={}\nartifact_id={}\ndownload_url={}\ntarget_os={target_os}\npackage_type={}\nnative_arch={}\nsize_bytes={}\nsha256={}\n",
        offer.release_id,
        offer.version,
        offer.retry_count,
        offer.artifact_id,
        offer.download_url,
        offer.package_type,
        offer.native_arch,
        offer.size_bytes,
        offer.sha256.to_ascii_lowercase(),
    ))
}

fn rollback_signature_payload(offer: &RollbackOffer) -> Result<String> {
    let (artifact_id, target_os, package_type, native_arch, size_bytes, sha256) = offer
        .package
        .as_ref()
        .map(|package| {
            (
                package.artifact_id.as_str(),
                package.target_os.as_str(),
                package.package_type.as_str(),
                package.native_arch.as_str(),
                package.size_bytes,
                package.sha256.as_str(),
            )
        })
        .unwrap_or(("", "", "", "", 0, ""));
    for (name, value) in [
        ("attempt_id", offer.attempt_id.as_str()),
        ("release_id", offer.release_id.as_str()),
        ("instance_id", offer.instance_id.as_str()),
        ("from_version", offer.from_version.as_str()),
        ("target_version", offer.target_version.as_str()),
        ("artifact_id", artifact_id),
        ("target_os", target_os),
        ("package_type", package_type),
        ("native_arch", native_arch),
        ("sha256", sha256),
    ] {
        if value
            .bytes()
            .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
        {
            bail!("rollback signature field {name} is invalid");
        }
    }
    if offer.attempt_id.is_empty()
        || offer.release_id.is_empty()
        || offer.instance_id.is_empty()
        || offer.from_version.is_empty()
        || offer.target_version.is_empty()
        || offer.retry_count < 0
    {
        bail!("rollback signature fields are incomplete");
    }
    Ok(format!(
        "{ROLLBACK_SIGNATURE_DOMAIN}\nattempt_id={}\nrelease_id={}\ninstance_id={}\nfrom_version={}\ntarget_version={}\nretry_count={}\nartifact_id={artifact_id}\ntarget_os={target_os}\npackage_type={package_type}\nnative_arch={native_arch}\nsize_bytes={size_bytes}\nsha256={}\n",
        offer.attempt_id,
        offer.release_id,
        offer.instance_id,
        offer.from_version,
        offer.target_version,
        offer.retry_count,
        sha256.to_ascii_lowercase(),
    ))
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
    let actual_sha256 = crate::hex::encode_lower(hasher.finalize());
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
    Ok((size, crate::hex::encode_lower(hasher.finalize())))
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

fn standalone_target_os() -> &'static str {
    #[cfg(windows)]
    {
        return "windows";
    }
    #[cfg(target_os = "macos")]
    {
        return "macos";
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        "linux"
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
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
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
        if !target.exists() {
            retry_windows_file_operation(WINDOWS_FILE_REPLACE_TIMEOUT, || {
                fs::rename(&temporary, target)
            })
            .with_context(|| {
                format!(
                    "failed to restore missing installed executable {}",
                    target.display()
                )
            })?;
            repair_windows_global_command_best_effort(target);
            return Ok(());
        }
        let backup = target.with_extension("update-old.exe");
        remove_windows_file_if_exists(&backup, WINDOWS_FILE_REPLACE_TIMEOUT).with_context(
            || {
                format!(
                    "failed to remove previous update backup {}",
                    backup.display()
                )
            },
        )?;
        retry_windows_file_operation(WINDOWS_FILE_REPLACE_TIMEOUT, || fs::rename(target, &backup))
            .with_context(|| {
                format!(
                    "failed to release installed executable {} for replacement",
                    target.display()
                )
            })?;
        if let Err(error) = retry_windows_file_operation(WINDOWS_FILE_REPLACE_TIMEOUT, || {
            fs::rename(&temporary, target)
        }) {
            let restore = retry_windows_file_operation(WINDOWS_FILE_REPLACE_TIMEOUT, || {
                fs::rename(&backup, target)
            });
            return match restore {
                Ok(()) => Err(error).with_context(|| {
                    format!("failed to install new executable {}", target.display())
                }),
                Err(restore_error) => bail!(
                    "failed to install new executable {}: {error}; also failed to restore {}: {restore_error}",
                    target.display(),
                    backup.display()
                ),
            };
        }
        let _ = remove_windows_file_if_exists(&backup, WINDOWS_FILE_REPLACE_TIMEOUT);
        repair_windows_global_command_best_effort(target);
    }
    #[cfg(not(windows))]
    {
        fs::rename(&temporary, target)?;
    }
    Ok(())
}

#[cfg(windows)]
fn repair_windows_global_command_best_effort(target: &Path) {
    if let Err(error) = crate::install::repair_windows_global_command(target) {
        // The installed executable is the update contract. A machine-wide convenience entry can
        // be protected by System32 ACLs or a third-party file lock, so leave the update successful
        // and let the service startup retry the repair with its service token.
        crate::logging::error(format_args!(
            "failed to repair the global Windows command after update; continuing with the installed executable: {error:#}"
        ));
    }
}

#[cfg(any(windows, test))]
fn retry_windows_file_operation<T>(
    timeout: Duration,
    mut operation: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let started = Instant::now();
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if started.elapsed() < timeout => {
                thread::sleep(POLL_INTERVAL.min(timeout.saturating_sub(started.elapsed())));
                if started.elapsed() >= timeout {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(windows)]
fn remove_windows_file_if_exists(path: &Path, timeout: Duration) -> io::Result<()> {
    retry_windows_file_operation(timeout, || match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    })
}

fn stop_standalone_service() -> Result<()> {
    #[cfg(windows)]
    {
        let mut found = false;
        let mut errors = Vec::new();
        for service_name in [LEGACY_SERVICE_NAME, SERVICE_NAME] {
            if query_windows_agent_service(service_name).is_ok() {
                found = true;
                if let Err(error) = stop_windows_agent_service(service_name) {
                    errors.push(format!("{service_name}: {error:#}"));
                }
            }
        }
        if !errors.is_empty() {
            bail!("{}", errors.join("; "));
        }
        if !found {
            bail!("OM Agent Windows service is not installed");
        }
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let spec = CommandSpec {
            program: "/bin/launchctl".into(),
            args: vec![
                "bootout".into(),
                "--wait".into(),
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
    let privileged = crate::privileged_path::is_process_privileged();
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

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};

    use super::*;

    fn signed_test_offer() -> (UpdateOffer, SigningKey) {
        let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
        let mut offer = UpdateOffer {
            attempt_id: Some("attempt-signed".to_string()),
            instance_id: Some("instance-signed".to_string()),
            release_id: "release-signed".to_string(),
            version: "1.2.3".to_string(),
            artifact_id: "artifact-signed".to_string(),
            download_url: "/api/agent/update/artifacts/artifact-signed/download".to_string(),
            sha256: "a".repeat(64),
            size_bytes: 42,
            package_type: "standalone".to_string(),
            native_arch: standalone_native_arch(),
            target_os: Some(standalone_target_os().to_string()),
            signature_key_id: Some("release-v1".to_string()),
            signature: None,
            signature_v2: None,
            retry_count: 0,
        };
        let payload = legacy_update_signature_payload(&offer).unwrap();
        offer.signature =
            Some(BASE64_STANDARD.encode(signing_key.sign(payload.as_bytes()).to_bytes()));
        let payload_v2 = update_signature_payload_v2(&offer).unwrap();
        offer.signature_v2 =
            Some(BASE64_STANDARD.encode(signing_key.sign(payload_v2.as_bytes()).to_bytes()));
        (offer, signing_key)
    }

    fn signed_test_rollback_offer() -> (RollbackOffer, SigningKey) {
        let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
        let mut offer = RollbackOffer {
            attempt_id: "rollback-attempt-1".to_string(),
            release_id: "release-rollback".to_string(),
            instance_id: "instance-rollback".to_string(),
            from_version: "2.0.0".to_string(),
            target_version: "1.2.3".to_string(),
            retry_count: 3,
            package: Some(RollbackPackage {
                artifact_id: "artifact-baseline".to_string(),
                download_url: "/api/agent/update/artifacts/artifact-baseline/download".to_string(),
                sha256: "A".repeat(64),
                size_bytes: 42,
                package_type: "standalone".to_string(),
                native_arch: standalone_native_arch(),
                target_os: standalone_target_os().to_string(),
            }),
            signature_key_id: Some("release-v1".to_string()),
            signature: None,
        };
        let payload = rollback_signature_payload(&offer).unwrap();
        offer.signature =
            Some(BASE64_STANDARD.encode(signing_key.sign(payload.as_bytes()).to_bytes()));
        (offer, signing_key)
    }

    #[test]
    fn update_offer_logs_include_operational_ids_without_sensitive_metadata() {
        let (offer, _) = signed_test_offer();

        let fields = update_offer_log_fields(&offer);

        assert!(fields.contains("version=\"1.2.3\""));
        assert!(fields.contains("attempt_id=Some(\"attempt-signed\")"));
        assert!(fields.contains("release_id=\"release-signed\""));
        assert!(fields.contains("artifact_id=\"artifact-signed\""));
        assert!(fields.contains("size_bytes=42"));
        assert!(!fields.contains(&offer.download_url));
        assert!(!fields.contains(&offer.sha256));
        assert!(!fields.contains(offer.signature.as_deref().unwrap()));
    }

    #[test]
    fn rollback_offer_logs_include_versions_and_package_source() {
        let (offer, _) = signed_test_rollback_offer();

        assert_eq!(
            rollback_offer_log_fields(&offer),
            "from_version=\"2.0.0\" target_version=\"1.2.3\" attempt_id=\"rollback-attempt-1\" release_id=\"release-rollback\" retry_count=3 package_source=server_or_local artifact_id=\"artifact-baseline\" size_bytes=42"
        );
    }

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
    fn verifies_legacy_and_metadata_bound_update_signatures() {
        let (offer, signing_key) = signed_test_offer();
        let payload = legacy_update_signature_payload(&offer).unwrap();
        assert_eq!(
            payload,
            format!(
                "operation-monitoring-agent-update-v1\nversion=1.2.3\ntarget_os={}\npackage_type=standalone\nnative_arch={}\nsize_bytes=42\nsha256={}\n",
                standalone_target_os(),
                standalone_native_arch(),
                "a".repeat(64)
            )
        );
        verify_legacy_update_signature(&offer, "release-v1", &signing_key.verifying_key()).unwrap();

        assert_eq!(
            update_signature_payload_v2(&offer).unwrap(),
            format!(
                "operation-monitoring-agent-update-v2\nattempt_id=attempt-signed\ninstance_id=instance-signed\nrelease_id=release-signed\nversion=1.2.3\nretry_count=0\nartifact_id=artifact-signed\ndownload_url=/api/agent/update/artifacts/artifact-signed/download\ntarget_os={}\npackage_type=standalone\nnative_arch={}\nsize_bytes=42\nsha256={}\n",
                standalone_target_os(),
                standalone_native_arch(),
                "a".repeat(64)
            )
        );
        verify_update_signature_v2(&offer, "release-v1", &signing_key.verifying_key()).unwrap();

        let mut tampered = offer.clone();
        tampered.retry_count = 1;
        assert!(
            verify_update_signature_v2(&tampered, "release-v1", &signing_key.verifying_key())
                .is_err()
        );

        let mut out_of_range = offer.clone();
        out_of_range.retry_count = MAX_AGENT_UPDATE_RETRY_COUNT + 1;
        assert!(update_signature_payload_v2(&out_of_range).is_err());

        let mut tampered = offer.clone();
        tampered.attempt_id = Some("attempt-other".to_string());
        assert!(
            verify_update_signature_v2(&tampered, "release-v1", &signing_key.verifying_key())
                .is_err()
        );

        let mut tampered = offer.clone();
        tampered.instance_id = Some("instance-other".to_string());
        assert!(
            verify_update_signature_v2(&tampered, "release-v1", &signing_key.verifying_key())
                .is_err()
        );

        let mut tampered = offer.clone();
        tampered.artifact_id = "artifact-other".to_string();
        tampered.download_url = "/api/agent/update/artifacts/artifact-other/download".to_string();
        assert!(
            verify_update_signature_v2(&tampered, "release-v1", &signing_key.verifying_key())
                .is_err()
        );
        assert!(
            verify_update_signature_v2(&offer, "other-key", &signing_key.verifying_key()).is_err()
        );
    }

    #[test]
    fn v2_updates_are_bound_to_the_current_instance_but_legacy_https_offers_are_not() {
        let (offer, _) = signed_test_offer();
        assert!(validate_update_instance_binding(&offer, "instance-signed").is_ok());

        let mut other_instance = offer.clone();
        other_instance.instance_id = Some("instance-other".to_string());
        assert!(validate_update_instance_binding(&other_instance, "instance-signed").is_err());

        let mut missing_instance = offer.clone();
        missing_instance.instance_id = None;
        assert!(validate_update_instance_binding(&missing_instance, "instance-signed").is_err());

        let mut legacy = offer;
        legacy.signature_v2 = None;
        legacy.instance_id = None;
        assert!(validate_update_instance_binding(&legacy, "instance-signed").is_ok());
    }

    #[test]
    fn requires_a_pinned_key_for_http_automatic_updates() {
        let (offer, signing_key) = signed_test_offer();
        assert!(verify_update_signature_policy_with_key(&offer, true, None).is_ok());
        assert!(verify_update_signature_policy_with_key(&offer, false, None).is_err());
        assert!(
            verify_update_signature_policy_with_key(
                &offer,
                false,
                Some(("release-v1", &signing_key.verifying_key()))
            )
            .is_ok()
        );

        let mut legacy = offer.clone();
        legacy.signature_v2 = None;
        assert!(
            verify_update_signature_policy_with_key(
                &legacy,
                false,
                Some(("release-v1", &signing_key.verifying_key()))
            )
            .is_err()
        );
        assert!(
            verify_update_signature_policy_with_key(
                &legacy,
                true,
                Some(("release-v1", &signing_key.verifying_key()))
            )
            .is_ok()
        );

        let mut unsigned = offer;
        unsigned.signature = None;
        unsigned.signature_v2 = None;
        assert!(
            verify_update_signature_policy_with_key(
                &unsigned,
                true,
                Some(("release-v1", &signing_key.verifying_key()))
            )
            .is_err()
        );
    }

    #[test]
    fn verifies_domain_separated_rollback_signatures() {
        let (offer, signing_key) = signed_test_rollback_offer();
        assert_eq!(
            rollback_signature_payload(&offer).unwrap(),
            format!(
                "operation-monitoring-agent-rollback-v1\nattempt_id=rollback-attempt-1\nrelease_id=release-rollback\ninstance_id=instance-rollback\nfrom_version=2.0.0\ntarget_version=1.2.3\nretry_count=3\nartifact_id=artifact-baseline\ntarget_os={}\npackage_type=standalone\nnative_arch={}\nsize_bytes=42\nsha256={}\n",
                standalone_target_os(),
                standalone_native_arch(),
                "a".repeat(64),
            )
        );
        verify_rollback_signature(&offer, "release-v1", &signing_key.verifying_key()).unwrap();
        assert!(
            verify_rollback_signature_policy_with_key(
                &offer,
                false,
                Some(("release-v1", &signing_key.verifying_key())),
            )
            .is_ok()
        );

        let mut tampered = offer.clone();
        tampered.instance_id = "other-instance".to_string();
        assert!(
            verify_rollback_signature(&tampered, "release-v1", &signing_key.verifying_key())
                .is_err()
        );

        let (update_offer, _) = signed_test_offer();
        let mut wrong_domain = offer.clone();
        wrong_domain.signature = update_offer.signature;
        assert!(
            verify_rollback_signature(&wrong_domain, "release-v1", &signing_key.verifying_key())
                .is_err()
        );

        let mut unsigned = offer;
        unsigned.signature = None;
        assert!(
            verify_rollback_signature_policy_with_key(
                &unsigned,
                true,
                Some(("release-v1", &signing_key.verifying_key())),
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_invalid_offers_before_persisting_update_state() {
        let directory =
            std::env::temp_dir().join(format!("om-update-invalid-offer-{}", uuid::Uuid::new_v4()));
        let config = AgentConfig {
            server: "https://monitor.example".to_string(),
            identity_file: None,
            report_interval: 5,
            state_dir: None,
            log_file: None,
            log_max_bytes: 10 * 1024 * 1024,
            log_history: 3,
            update_dir: Some(directory.clone()),
            remote_desktop_consent: crate::config::RemoteDesktopConsent::Required,
            windows_virtual_devices: crate::config::WindowsVirtualDevices::Disabled,
        };
        let paths = UpdatePaths::from_config(&config).unwrap();
        paths.prepare().unwrap();
        let manager = UpdateManager {
            config,
            identity: Identity {
                instance_id: "instance-signed".to_string(),
                secret: "secret-test".to_string(),
                credential_version: 1,
                previous_secret: None,
            },
            client: crate::tls::http_client(),
            activity: ActivityTracker::default(),
            capability: UpdateCapability {
                package_type: Some("standalone".to_string()),
                native_arch: Some(standalone_native_arch()),
                update_privileged: true,
            },
            paths: paths.clone(),
        };
        let (mut offer, _) = signed_test_offer();
        offer.artifact_id = "../invalid".to_string();
        offer.download_url = "/api/agent/update/artifacts/../invalid/download".to_string();
        let (outbound, mut events, _failed) = AgentEventSender::channel(4);

        assert!(matches!(
            manager.prepare(offer, outbound).await,
            PrepareResult::Finished
        ));
        assert!(
            read_update_state(&paths.state_file)
                .unwrap()
                .attempt
                .is_none()
        );
        assert!(matches!(
            events.try_recv(),
            Ok(AgentInbound::UpdateStatus {
                status: UpdateStatus::Failed,
                ..
            })
        ));

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn rejects_downgrade_through_a_normal_update_offer() {
        let directory =
            std::env::temp_dir().join(format!("om-update-downgrade-{}", uuid::Uuid::new_v4()));
        let config = AgentConfig {
            server: "https://monitor.example".to_string(),
            identity_file: None,
            report_interval: 5,
            state_dir: None,
            log_file: None,
            log_max_bytes: 10 * 1024 * 1024,
            log_history: 3,
            update_dir: Some(directory.clone()),
            remote_desktop_consent: crate::config::RemoteDesktopConsent::Required,
            windows_virtual_devices: crate::config::WindowsVirtualDevices::Disabled,
        };
        let paths = UpdatePaths::from_config(&config).unwrap();
        paths.prepare().unwrap();
        let manager = UpdateManager {
            config,
            identity: Identity {
                instance_id: "downgrade-test".to_string(),
                secret: "secret-test".to_string(),
                credential_version: 1,
                previous_secret: None,
            },
            client: crate::tls::http_client(),
            activity: ActivityTracker::default(),
            capability: UpdateCapability {
                package_type: Some("standalone".to_string()),
                native_arch: Some(standalone_native_arch()),
                update_privileged: true,
            },
            paths,
        };
        let offer = UpdateOffer {
            attempt_id: Some("downgrade-attempt".to_string()),
            instance_id: None,
            release_id: "release-downgrade".to_string(),
            version: "0.0.0".to_string(),
            artifact_id: "artifact-downgrade".to_string(),
            download_url: "/api/agent/update/artifacts/artifact-downgrade/download".to_string(),
            sha256: "a".repeat(64),
            size_bytes: 42,
            package_type: "standalone".to_string(),
            native_arch: standalone_native_arch(),
            target_os: Some(standalone_target_os().to_string()),
            signature_key_id: None,
            signature: None,
            signature_v2: None,
            retry_count: 0,
        };
        let (outbound, mut events, _failed) = AgentEventSender::channel(4);

        assert!(matches!(
            manager.prepare(offer, outbound).await,
            PrepareResult::Finished
        ));
        assert!(matches!(
            events.try_recv(),
            Ok(AgentInbound::UpdateStatus {
                status: UpdateStatus::Failed,
                message: Some(message),
                ..
            }) if message.contains("refusing automatic downgrade")
        ));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn parses_forced_update_package_versions() {
        assert_eq!(
            parse_agent_package_version("om-agent 1.2.3\n").unwrap(),
            "1.2.3"
        );
        assert!(parse_agent_package_version("different-agent 1.2.3\n").is_err());
        assert!(parse_agent_package_version("om-agent invalid\n").is_err());
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
        let digest = crate::hex::encode_lower(Sha256::digest(package));
        let offer = UpdateOffer {
            attempt_id: None,
            instance_id: None,
            release_id: "release-checksum".to_string(),
            version: "9.9.9".to_string(),
            artifact_id: "artifact-checksum".to_string(),
            download_url: "/api/agent/update/artifacts/artifact-checksum/download".to_string(),
            sha256: digest.clone(),
            size_bytes: package.len() as i64,
            package_type: "standalone".to_string(),
            native_arch: standalone_native_arch(),
            target_os: Some(standalone_target_os().to_string()),
            signature_key_id: None,
            signature: None,
            signature_v2: None,
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
            log_max_bytes: 10 * 1024 * 1024,
            log_history: 3,
            update_dir: Some(directory.clone()),
            remote_desktop_consent: crate::config::RemoteDesktopConsent::Required,
            windows_virtual_devices: crate::config::WindowsVirtualDevices::Disabled,
        };
        let paths = UpdatePaths::from_config(&config).unwrap();
        paths.prepare().unwrap();
        let manager = UpdateManager {
            config,
            identity: Identity {
                instance_id: "instance-gate".to_string(),
                secret: "secret-gate".to_string(),
                credential_version: 1,
                previous_secret: None,
            },
            client: crate::tls::http_client(),
            activity: ActivityTracker::default(),
            capability: UpdateCapability {
                package_type: Some("standalone".to_string()),
                native_arch: Some("arm64".to_string()),
                update_privileged: true,
            },
            paths: paths.clone(),
        };
        let offer = UpdateOffer {
            attempt_id: None,
            instance_id: None,
            release_id: "release-handoff".to_string(),
            version: "9.9.9".to_string(),
            artifact_id: "artifact-handoff".to_string(),
            download_url: "/api/agent/update/artifacts/artifact-handoff/download".to_string(),
            sha256: "a".repeat(64),
            size_bytes: 6,
            package_type: "standalone".to_string(),
            native_arch: "arm64".to_string(),
            target_os: Some(standalone_target_os().to_string()),
            signature_key_id: None,
            signature: None,
            signature_v2: None,
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
                rollback_package: None,
                attempt: Some(PersistedAttempt {
                    offer: offer.clone(),
                    operation: AttemptOperation::Upgrade,
                    manual: false,
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

        let (outbound, mut inbound, _failed) = AgentEventSender::channel(32);
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
        let sha256 = crate::hex::encode_lower(Sha256::digest(original));

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
    fn forced_install_can_restore_a_missing_executable() {
        let directory = std::env::temp_dir().join(format!("om-update-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("source-agent");
        let target = directory.join(format!("installed-agent{}", std::env::consts::EXE_SUFFIX));
        fs::write(&source, b"replacement-agent").unwrap();
        let plan = ApplyPlan {
            offer: UpdateOffer {
                attempt_id: None,
                instance_id: None,
                release_id: "manual-release".to_string(),
                version: "9.9.9".to_string(),
                artifact_id: "manual-artifact".to_string(),
                download_url: "local://replacement".to_string(),
                sha256: "a".repeat(64),
                size_bytes: 17,
                package_type: "standalone".to_string(),
                native_arch: standalone_native_arch(),
                target_os: Some(standalone_target_os().to_string()),
                signature_key_id: None,
                signature: None,
                signature_v2: None,
                retry_count: 0,
            },
            package_path: source.clone(),
            operation: AttemptOperation::Upgrade,
            previous_package: None,
            state_file: directory.join("state.json"),
            health_file: directory.join("health.json"),
            lock_file: directory.join("updater.lock"),
            lock_owner_file: directory.join("updater-owner.json"),
            lock_owner: "manual-owner".to_string(),
            old_pid: 1,
            installed_executable: Some(target.clone()),
        };

        install_standalone(&plan, &source).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"replacement-agent");
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
            attempt_id: Some("attempt-old".to_string()),
            instance_id: None,
            release_id: "release-target".to_string(),
            version: "1.0.0".to_string(),
            artifact_id: "artifact-target".to_string(),
            download_url: "/api/agent/update/artifacts/artifact-target/download".to_string(),
            sha256: "a".repeat(64),
            size_bytes: 6,
            package_type: "standalone".to_string(),
            native_arch: "arm64".to_string(),
            target_os: Some(standalone_target_os().to_string()),
            signature_key_id: None,
            signature: None,
            signature_v2: None,
            retry_count: 0,
        };
        let mut new_offer = old_offer.clone();
        new_offer.attempt_id = Some("attempt-new".to_string());
        write_update_state(
            &state_file,
            &UpdateState {
                schema_version: UPDATE_SCHEMA_VERSION,
                current_package: Some(previous.clone()),
                rollback_package: None,
                attempt: Some(PersistedAttempt {
                    offer: new_offer,
                    operation: AttemptOperation::Upgrade,
                    manual: false,
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
            operation: AttemptOperation::Upgrade,
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
        assert_eq!(
            state.attempt.unwrap().offer.attempt_id.as_deref(),
            Some("attempt-new")
        );
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
    fn recognizes_the_platform_file_lock_contention_error() {
        let error = fs2::lock_contended_error();

        assert!(update_lock_is_contended(&error));
        assert!(!update_lock_is_contended(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unrelated file access failure",
        )));
        #[cfg(windows)]
        assert_eq!(error.raw_os_error(), Some(33));
    }

    #[test]
    fn windows_file_replacement_retries_transient_lock_failures() {
        let mut attempts = 0;
        retry_windows_file_operation(Duration::from_secs(2), || {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "executable is still locked",
                ))
            } else {
                Ok(())
            }
        })
        .unwrap();

        assert_eq!(attempts, 3);
    }

    #[test]
    fn pending_handoff_detection_distinguishes_active_and_terminal_state() {
        let directory = std::env::temp_dir().join(format!(
            "om-update-handoff-detection-{}",
            uuid::Uuid::new_v4()
        ));
        let config = AgentConfig {
            server: "http://127.0.0.1:13500".to_string(),
            identity_file: None,
            report_interval: 5,
            state_dir: None,
            log_file: None,
            log_max_bytes: 10 * 1024 * 1024,
            log_history: 3,
            update_dir: Some(directory.clone()),
            remote_desktop_consent: crate::config::RemoteDesktopConsent::Required,
            windows_virtual_devices: crate::config::WindowsVirtualDevices::Disabled,
        };
        let paths = UpdatePaths::from_config(&config).unwrap();
        assert!(!update_handoff_pending(&config).unwrap());

        let (offer, _) = signed_test_offer();
        let mut state = UpdateState {
            schema_version: UPDATE_SCHEMA_VERSION,
            current_package: None,
            rollback_package: None,
            attempt: Some(PersistedAttempt {
                offer,
                operation: AttemptOperation::Upgrade,
                manual: false,
                status: UpdateStatus::AwaitingRestart,
                message: None,
                package_path: None,
                previous_package: None,
                phase: AttemptPhase::Target,
                updated_at: now_ts(),
            }),
        };
        write_update_state(&paths.state_file, &state).unwrap();
        assert!(update_handoff_pending(&config).unwrap());

        let attempt = state.attempt.as_mut().unwrap();
        attempt.phase = AttemptPhase::Rollback;
        attempt.status = UpdateStatus::Installing;
        write_update_state(&paths.state_file, &state).unwrap();
        assert!(update_handoff_pending(&config).unwrap());

        let attempt = state.attempt.as_mut().unwrap();
        attempt.phase = AttemptPhase::Completed;
        attempt.status = UpdateStatus::Succeeded;
        write_update_state(&paths.state_file, &state).unwrap();
        assert!(!update_handoff_pending(&config).unwrap());

        fs::write(&paths.state_file, b"not-json").unwrap();
        assert!(update_handoff_pending(&config).is_err());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn connected_target_signals_health_and_finalizes_cache() {
        let directory = std::env::temp_dir().join(format!("om-update-{}", uuid::Uuid::new_v4()));
        let config = AgentConfig {
            server: "http://127.0.0.1:13500".to_string(),
            identity_file: None,
            report_interval: 5,
            state_dir: None,
            log_file: None,
            log_max_bytes: 10 * 1024 * 1024,
            log_history: 3,
            update_dir: Some(directory.clone()),
            remote_desktop_consent: crate::config::RemoteDesktopConsent::Required,
            windows_virtual_devices: crate::config::WindowsVirtualDevices::Disabled,
        };
        let paths = UpdatePaths::from_config(&config).unwrap();
        paths.prepare().unwrap();
        let manager = UpdateManager {
            config,
            identity: Identity {
                instance_id: "instance-test".to_string(),
                secret: "secret-test".to_string(),
                credential_version: 1,
                previous_secret: None,
            },
            client: crate::tls::http_client(),
            activity: ActivityTracker::default(),
            capability: UpdateCapability {
                package_type: Some("standalone".to_string()),
                native_arch: Some(standalone_native_arch()),
                update_privileged: true,
            },
            paths: paths.clone(),
        };
        let previous_path = paths.packages.join("previous.standalone");
        let target_path = paths.packages.join("target.standalone");
        fs::copy(std::env::current_exe().unwrap(), &previous_path).unwrap();
        set_owner_only_executable(&previous_path).unwrap();
        let (previous_size, previous_sha256) = file_integrity(&previous_path).unwrap();
        fs::write(&target_path, b"target").unwrap();
        let previous = CachedPackage {
            artifact_id: "artifact-previous".to_string(),
            version: "0.1.22".to_string(),
            package_type: PackageType::Standalone,
            native_arch: standalone_native_arch(),
            path: previous_path.clone(),
            retry_count: 0,
            size_bytes: previous_size,
            sha256: previous_sha256,
        };
        let offer = UpdateOffer {
            attempt_id: None,
            instance_id: None,
            release_id: "release-target".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            artifact_id: "artifact-target".to_string(),
            download_url: "/api/agent/update/artifacts/artifact-target/download".to_string(),
            sha256: "a".repeat(64),
            size_bytes: 6,
            package_type: "standalone".to_string(),
            native_arch: standalone_native_arch(),
            target_os: Some(standalone_target_os().to_string()),
            signature_key_id: None,
            signature: None,
            signature_v2: None,
            retry_count: 0,
        };
        write_update_state(
            &paths.state_file,
            &UpdateState {
                schema_version: UPDATE_SCHEMA_VERSION,
                current_package: Some(previous.clone()),
                rollback_package: None,
                attempt: Some(PersistedAttempt {
                    offer: offer.clone(),
                    operation: AttemptOperation::Upgrade,
                    manual: false,
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
        let baseline_path = state.rollback_package.as_ref().unwrap().path.clone();
        assert_ne!(baseline_path, previous_path);
        assert!(baseline_path.exists());
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
        fs::remove_file(&previous_path).unwrap();
        assert_eq!(manager.rollback_version().as_deref(), Some("0.1.22"));

        let mut manual_state = read_update_state(&paths.state_file).unwrap();
        let manual_attempt = manual_state.attempt.as_mut().unwrap();
        manual_attempt.manual = true;
        manual_attempt.status = UpdateStatus::AwaitingRestart;
        manual_attempt.phase = AttemptPhase::Target;
        write_update_state(&paths.state_file, &manual_state).unwrap();
        fs::remove_file(&paths.health_file).unwrap();

        assert!(manager.connected_status().unwrap().is_none());
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
            operation: AttemptOperation::Upgrade,
            manual: false,
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
            operation: AttemptOperation::Upgrade,
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
        assert!(baseline_path.exists());
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
    fn rollback_restart_recovers_status_and_rotates_one_baseline_generation() {
        let directory =
            std::env::temp_dir().join(format!("om-rollback-restart-{}", uuid::Uuid::new_v4()));
        let config = AgentConfig {
            server: "http://127.0.0.1:13500".to_string(),
            identity_file: None,
            report_interval: 5,
            state_dir: None,
            log_file: None,
            log_max_bytes: 10 * 1024 * 1024,
            log_history: 3,
            update_dir: Some(directory.clone()),
            remote_desktop_consent: crate::config::RemoteDesktopConsent::Required,
            windows_virtual_devices: crate::config::WindowsVirtualDevices::Disabled,
        };
        let paths = UpdatePaths::from_config(&config).unwrap();
        paths.prepare().unwrap();
        let manager = UpdateManager {
            config,
            identity: Identity {
                instance_id: "instance-rollback".to_string(),
                secret: "secret-rollback".to_string(),
                credential_version: 1,
                previous_secret: None,
            },
            client: crate::tls::http_client(),
            activity: ActivityTracker::default(),
            capability: UpdateCapability {
                package_type: Some("standalone".to_string()),
                native_arch: Some(standalone_native_arch()),
                update_privileged: true,
            },
            paths: paths.clone(),
        };
        let source_path = paths.packages.join("source.standalone");
        let target_path = paths.packages.join("target.standalone");
        let stale_path = paths.packages.join("stale.standalone");
        fs::write(&source_path, b"source").unwrap();
        fs::write(&target_path, b"target").unwrap();
        fs::write(&stale_path, b"stale").unwrap();
        let source = CachedPackage {
            artifact_id: "artifact-source".to_string(),
            version: "9.9.9".to_string(),
            package_type: PackageType::Standalone,
            native_arch: standalone_native_arch(),
            path: source_path.clone(),
            retry_count: 0,
            size_bytes: 6,
            sha256: "b".repeat(64),
        };
        let stale = CachedPackage {
            artifact_id: "artifact-stale".to_string(),
            version: "0.0.1".to_string(),
            package_type: PackageType::Standalone,
            native_arch: standalone_native_arch(),
            path: stale_path.clone(),
            retry_count: 0,
            size_bytes: 5,
            sha256: "c".repeat(64),
        };
        let offer = UpdateOffer {
            attempt_id: Some("rollback-attempt-1".to_string()),
            instance_id: None,
            release_id: "release-rollback".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            artifact_id: "artifact-target".to_string(),
            download_url: "local://target.standalone".to_string(),
            sha256: "a".repeat(64),
            size_bytes: 6,
            package_type: "standalone".to_string(),
            native_arch: standalone_native_arch(),
            target_os: Some(standalone_target_os().to_string()),
            signature_key_id: None,
            signature: None,
            signature_v2: None,
            retry_count: 2,
        };
        write_update_state(
            &paths.state_file,
            &UpdateState {
                schema_version: UPDATE_SCHEMA_VERSION,
                current_package: Some(source.clone()),
                rollback_package: Some(stale),
                attempt: Some(PersistedAttempt {
                    offer: offer.clone(),
                    operation: AttemptOperation::Rollback {
                        attempt_id: "rollback-attempt-1".to_string(),
                        release_id: "release-rollback".to_string(),
                        from_version: "9.9.9".to_string(),
                        target_version: env!("CARGO_PKG_VERSION").to_string(),
                        retry_count: 2,
                    },
                    manual: false,
                    status: UpdateStatus::AwaitingRestart,
                    message: Some("rollback target installed".to_string()),
                    package_path: Some(target_path.clone()),
                    previous_package: Some(source.clone()),
                    phase: AttemptPhase::Target,
                    updated_at: now_ts(),
                }),
            },
        )
        .unwrap();

        let status = manager.connected_status().unwrap().unwrap();
        assert!(matches!(
            status,
            AgentInbound::RollbackStatus {
                attempt_id,
                retry_count: 2,
                status: UpdateStatus::RollbackSucceeded,
                message: Some(message),
            } if attempt_id == "rollback-attempt-1" && message == "rollback target installed"
        ));
        let state = read_update_state(&paths.state_file).unwrap();
        assert_eq!(
            state.current_package.as_ref().map(|package| &package.path),
            Some(&target_path)
        );
        assert_eq!(
            state.rollback_package.as_ref().map(|package| &package.path),
            Some(&source_path)
        );
        assert!(matches!(
            state.attempt,
            Some(PersistedAttempt {
                operation: AttemptOperation::Rollback { .. },
                status: UpdateStatus::RollbackSucceeded,
                phase: AttemptPhase::Completed,
                ..
            })
        ));
        assert!(target_path.exists());
        assert!(source_path.exists());
        assert!(!stale_path.exists());

        assert!(matches!(
            manager.connected_status().unwrap(),
            Some(AgentInbound::RollbackStatus {
                status: UpdateStatus::RollbackSucceeded,
                ..
            })
        ));
        assert!(!stale_path.exists());
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

    #[test]
    fn failed_atomic_replacement_preserves_the_previous_file() {
        let directory = std::env::temp_dir().join(format!("om-update-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let target = directory.join("state.json");
        fs::write(&target, b"previous-state").unwrap();

        assert!(replace_file(&directory.join("missing.tmp"), &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"previous-state");

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn atomic_replacement_commits_the_complete_new_file() {
        let directory = std::env::temp_dir().join(format!("om-update-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("state.tmp");
        let target = directory.join("state.json");
        fs::write(&source, b"complete-new-state").unwrap();
        fs::write(&target, b"previous-state").unwrap();

        replace_file(&source, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"complete-new-state");
        assert!(!source.exists());

        let _ = fs::remove_dir_all(directory);
    }
}
