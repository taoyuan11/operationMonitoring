#[cfg(not(windows))]
use crate::lifecycle::run_agent;
use crate::{
    config::{AgentConfig, RemoteDesktopConsent},
    lifecycle::stop_if_running,
};
use anyhow::{Context, Result, bail};
use std::{
    env, fs,
    io::{self, IsTerminal, Write},
    path::Path,
    process::{Command, ExitStatus},
};
#[cfg(target_os = "macos")]
use std::{
    thread,
    time::{Duration, Instant},
};

#[cfg(any(windows, test))]
const WINDOWS_SERVICE_NAME: &str = "operation-monitoring-agent";
#[cfg(any(windows, test))]
const SHORT_WINDOWS_SERVICE_NAME: &str = "om-agent";
#[cfg(target_os = "macos")]
const MACOS_SERVICE_LABEL: &str = "com.operation-monitoring.agent";
#[cfg(target_os = "macos")]
const MACOS_SERVICE_UNLOAD_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "macos")]
const MACOS_SERVICE_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub fn install(mut config: AgentConfig, non_interactive: bool, yes: bool) -> Result<()> {
    #[cfg(windows)]
    reject_windows_install_path_overrides(&config)?;
    let explicit_server = env::args_os()
        .any(|value| value == "--server" || value.to_string_lossy().starts_with("--server="))
        || env::var_os("OM_SERVER").is_some();
    if non_interactive && !explicit_server {
        bail!("--server is required with --non-interactive");
    }
    validate_unattended_install_confirmation(&config, non_interactive, yes)?;
    if !non_interactive {
        if !io::stdin().is_terminal() {
            bail!(
                "interactive installation requires a terminal; use --non-interactive --server <URL>"
            );
        }
        config.server = prompt_server(&config.server)?;
        if !yes && !confirm("Install system-wide and enable automatic startup? [y/N] ")? {
            bail!("installation cancelled");
        }
    }
    config.normalize_server()?;
    if !is_elevated() {
        return elevate("install", Some(&config));
    }
    install_elevated(&config)
}

#[cfg(any(windows, test))]
fn explicit_windows_install_path_override(config: &AgentConfig) -> Option<&'static str> {
    [
        (
            config.identity_file.is_some(),
            "--identity-file/OM_AGENT_ID_FILE",
        ),
        (config.state_dir.is_some(), "--state-dir/OM_AGENT_STATE_DIR"),
        (config.log_file.is_some(), "--log-file/OM_AGENT_LOG_FILE"),
        (
            config.update_dir.is_some(),
            "--update-dir/OM_AGENT_UPDATE_DIR",
        ),
    ]
    .into_iter()
    .find_map(|(configured, setting)| configured.then_some(setting))
}

#[cfg(windows)]
fn reject_windows_install_path_overrides(config: &AgentConfig) -> Result<()> {
    if let Some(setting) = explicit_windows_install_path_override(config) {
        bail!(
            "{setting} cannot override Windows system-install paths; the service stores identity, state, logs, and updates under the protected ProgramData directory"
        );
    }
    Ok(())
}

fn validate_unattended_install_confirmation(
    config: &AgentConfig,
    non_interactive: bool,
    yes: bool,
) -> Result<()> {
    if non_interactive && config.remote_desktop_consent == RemoteDesktopConsent::Unattended && !yes
    {
        bail!("--yes is required for a non-interactive unattended installation");
    }
    Ok(())
}

pub fn uninstall(config: AgentConfig, yes: bool) -> Result<()> {
    if !yes {
        if !io::stdin().is_terminal() {
            bail!("unattended uninstall requires --yes");
        }
        if !confirm("Remove the agent and all configuration, identity, logs, and updates? [y/N] ")?
        {
            bail!("uninstall cancelled");
        }
    }
    stop_if_running(&config, 10).context("failed to stop background agent before uninstall")?;
    if !is_elevated() {
        return elevate("uninstall", None);
    }
    uninstall_elevated()
}

pub fn run_service(mut config: AgentConfig) -> Result<()> {
    config.normalize_server()?;
    #[cfg(windows)]
    {
        return windows_service_impl::run(config);
    }
    #[cfg(not(windows))]
    {
        tokio::runtime::Runtime::new()?.block_on(run_agent(config))
    }
}

fn prompt_server(default: &str) -> Result<String> {
    loop {
        print!("Monitoring server URL [{default}]: ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let selected = if input.trim().is_empty() {
            default
        } else {
            input.trim()
        };
        match crate::config::ServerEndpoint::parse(selected) {
            Ok(endpoint) => return Ok(endpoint.normalized_server()),
            Err(error) => eprintln!("{error}"),
        }
    }
}
fn confirm(message: &str) -> Result<bool> {
    print!("{message}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(unix)]
fn is_elevated() -> bool {
    unsafe { libc::geteuid() == 0 }
}
#[cfg(windows)]
fn is_elevated() -> bool {
    unsafe { windows::Win32::UI::Shell::IsUserAnAdmin().as_bool() }
}

#[cfg(windows)]
pub(crate) fn is_elevated_for_service_control() -> bool {
    is_elevated()
}

#[cfg(windows)]
pub(crate) fn elevate_service_control(action: &str) -> Result<()> {
    elevate(action, None)
}
#[cfg(unix)]
fn elevate(action: &str, config: Option<&AgentConfig>) -> Result<()> {
    let mut cmd = Command::new("sudo");
    cmd.arg(env::current_exe()?).arg(action).arg("--yes");
    if let Some(c) = config {
        cmd.arg("--non-interactive");
        c.append_cli_args(&mut cmd);
    }
    success(cmd.status().context("failed to launch sudo")?, "sudo")
}
#[cfg(windows)]
fn elevate(action: &str, config: Option<&AgentConfig>) -> Result<()> {
    windows_runas(action, config)
}

fn install_elevated(config: &AgentConfig) -> Result<()> {
    #[cfg(windows)]
    {
        install_windows(config)
    }
    #[cfg(target_os = "macos")]
    {
        install_macos(config)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if Path::new("/etc/openwrt_release").exists() {
            install_openwrt(config)
        } else {
            install_systemd(config)
        }
    }
}
fn uninstall_elevated() -> Result<()> {
    #[cfg(windows)]
    {
        uninstall_windows()
    }
    #[cfg(target_os = "macos")]
    {
        uninstall_macos()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if Path::new("/etc/openwrt_release").exists()
            || Path::new("/etc/init.d/om-agent").exists()
            || Path::new("/etc/init.d/operation-monitoring-agent").exists()
        {
            uninstall_openwrt()
        } else {
            uninstall_systemd()
        }
    }
}
#[cfg(not(windows))]
fn copy_self(target: &Path) -> Result<()> {
    let source = env::current_exe()?;
    if source == target {
        return Ok(());
    }
    if let Some(p) = target.parent() {
        fs::create_dir_all(p)?
    }
    let temp = target.with_extension("new");
    fs::copy(source, &temp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(temp, target)?;
    Ok(())
}
fn success(status: ExitStatus, name: &str) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        bail!("{name} exited with {status}")
    }
}
fn run(program: &str, args: &[&str]) -> Result<()> {
    success(
        Command::new(program)
            .args(args)
            .status()
            .with_context(|| format!("failed to run {program}"))?,
        program,
    )
}
#[cfg(all(unix, not(target_os = "macos")))]
fn try_run_quiet(program: &str, args: &[&str]) {
    let _ = Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn migrate_path(legacy: &str, current: &str) -> Result<()> {
    fn migrate(legacy: &Path, current: &Path) -> Result<()> {
        if !legacy.exists() {
            return Ok(());
        }
        if fs::symlink_metadata(legacy)?.file_type().is_symlink() {
            fs::remove_file(legacy)?;
            return Ok(());
        }
        if !current.exists() {
            if let Some(parent) = current.parent() {
                fs::create_dir_all(parent)?;
            }
            return fs::rename(legacy, current).with_context(|| {
                format!(
                    "failed to migrate {} to {}",
                    legacy.display(),
                    current.display()
                )
            });
        }
        if legacy.is_dir() && current.is_dir() {
            for entry in fs::read_dir(legacy)? {
                let entry = entry?;
                migrate(&entry.path(), &current.join(entry.file_name()))?;
            }
            let _ = fs::remove_dir(legacy);
        } else if legacy.is_dir() {
            fs::remove_dir_all(legacy)?;
        } else {
            fs::remove_file(legacy)?;
        }
        Ok(())
    }

    migrate(Path::new(legacy), Path::new(current))
}
#[cfg(all(unix, not(target_os = "macos")))]
fn replace_symlink(target: &str, link: &str) -> Result<()> {
    let link = Path::new(link);
    if fs::symlink_metadata(link).is_ok() {
        if link.is_dir() && !fs::symlink_metadata(link)?.file_type().is_symlink() {
            fs::remove_dir_all(link)?;
        } else {
            fs::remove_file(link)?;
        }
    }
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}
#[cfg(target_os = "macos")]
fn bootout_macos_service() -> Result<()> {
    let target = format!("system/{MACOS_SERVICE_LABEL}");
    let output = Command::new("/bin/launchctl")
        .args(["bootout", &target])
        .output()
        .context("failed to run launchctl")?;
    if output.status.success() {
        return wait_for_macos_service_unloaded(
            MACOS_SERVICE_UNLOAD_TIMEOUT,
            MACOS_SERVICE_POLL_INTERVAL,
            query_macos_service_loaded,
        );
    }
    if macos_service_not_loaded(output.status.code()) {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let details = [stdout.trim(), stderr.trim()]
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if details.is_empty() {
        bail!("launchctl bootout exited with {}", output.status);
    }
    bail!("launchctl bootout exited with {}: {details}", output.status)
}
#[cfg(target_os = "macos")]
fn macos_service_not_loaded(exit_code: Option<i32>) -> bool {
    exit_code == Some(3)
}
#[cfg(target_os = "macos")]
fn query_macos_service_loaded() -> Result<bool> {
    let target = format!("system/{MACOS_SERVICE_LABEL}");
    let output = Command::new("/bin/launchctl")
        .args(["print", &target])
        .output()
        .context("failed to query launchd service state")?;
    if output.status.success() {
        return Ok(true);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let details = format!("{stdout}\n{stderr}");
    if macos_service_query_not_found(&details) {
        return Ok(false);
    }
    bail!(
        "launchctl print exited with {} while waiting for service removal: {}",
        output.status,
        details.trim()
    )
}
#[cfg(target_os = "macos")]
fn macos_service_query_not_found(details: &str) -> bool {
    details.contains("Could not find service") || details.contains("service not found")
}
#[cfg(target_os = "macos")]
fn wait_for_macos_service_unloaded(
    timeout: Duration,
    poll_interval: Duration,
    mut service_is_loaded: impl FnMut() -> Result<bool>,
) -> Result<()> {
    let started = Instant::now();
    loop {
        if !service_is_loaded()? {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            bail!(
                "launchd did not finish unloading {MACOS_SERVICE_LABEL} within {} seconds",
                timeout.as_secs()
            );
        }
        thread::sleep(poll_interval.min(timeout.saturating_sub(started.elapsed())));
    }
}
#[cfg(not(windows))]
fn private_file(path: impl AsRef<Path>, contents: &str) -> Result<()> {
    let path = path.as_ref();
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?
    }
    fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
#[cfg(not(windows))]
fn remove(paths: &[&str]) {
    for value in paths {
        let p = Path::new(value);
        if fs::symlink_metadata(p).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            let _ = fs::remove_file(p);
        } else if p.is_dir() {
            let _ = fs::remove_dir_all(p);
        } else {
            let _ = fs::remove_file(p);
        }
    }
}
#[cfg(not(windows))]
fn quoted(value: &str) -> String {
    value.replace('\'', "'\\''")
}
#[cfg(not(windows))]
fn env_file(c: &AgentConfig, mac: bool) -> String {
    let (id, state, log, update) = if mac {
        (
            "/Library/Application Support/OperationMonitoring/identity.json",
            "/Library/Application Support/OperationMonitoring/runtime",
            "/Library/Logs/OperationMonitoring/agent.log",
            "/Library/Application Support/OperationMonitoring/updates",
        )
    } else {
        (
            "/var/lib/om-agent/identity.json",
            "/run/om-agent",
            "/var/log/om-agent/agent.log",
            "/var/lib/om-agent/updates",
        )
    };
    format!(
        "OM_SERVER='{}'\nOM_REPORT_INTERVAL='{}'\nOM_AGENT_ID_FILE='{id}'\nOM_AGENT_STATE_DIR='{state}'\nOM_AGENT_LOG_FILE='{log}'\nOM_AGENT_LOG_MAX_BYTES='{}'\nOM_AGENT_LOG_HISTORY='{}'\nOM_AGENT_UPDATE_DIR='{update}'\nOM_REMOTE_DESKTOP_CONSENT='{}'\nOM_WINDOWS_VIRTUAL_DEVICES='{}'\n",
        quoted(&c.server),
        c.report_interval,
        c.log_max_bytes,
        c.log_history,
        c.remote_desktop_consent,
        c.windows_virtual_devices
    )
}

#[cfg(all(unix, not(target_os = "macos")))]
fn install_systemd(c: &AgentConfig) -> Result<()> {
    if Command::new("systemctl")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .status()
        .is_err()
    {
        bail!("systemd is required")
    };
    try_run_quiet(
        "systemctl",
        &["disable", "--now", "operation-monitoring-agent.service"],
    );
    try_run_quiet("systemctl", &["disable", "--now", "om-agent.service"]);
    migrate_path("/etc/operation-monitoring-agent", "/etc/om-agent")?;
    migrate_path("/var/lib/operation-monitoring-agent", "/var/lib/om-agent")?;
    migrate_path("/var/log/operation-monitoring-agent", "/var/log/om-agent")?;
    migrate_path("/run/operation-monitoring-agent", "/run/om-agent")?;
    copy_self(Path::new("/usr/local/bin/om-agent"))?;
    private_file("/etc/om-agent/agent.env", &env_file(c, false))?;
    fs::write("/etc/om-agent/install-type", "standalone\n")?;
    fs::write("/etc/systemd/system/om-agent.service", SYSTEMD)?;
    remove(&[
        "/usr/local/bin/operation-monitoring-agent",
        "/etc/systemd/system/operation-monitoring-agent.service",
        "/etc/operation-monitoring-agent",
        "/var/lib/operation-monitoring-agent",
        "/var/log/operation-monitoring-agent",
        "/run/operation-monitoring-agent",
    ]);
    // Keep service and marker aliases so older updaters can still roll back.
    replace_symlink(
        "om-agent.service",
        "/etc/systemd/system/operation-monitoring-agent.service",
    )?;
    replace_symlink("om-agent", "/etc/operation-monitoring-agent")?;
    run("systemctl", &["daemon-reload"])?;
    run("systemctl", &["enable", "--now", "om-agent.service"])?;
    println!("agent installed and started");
    Ok(())
}
#[cfg(all(unix, not(target_os = "macos")))]
fn uninstall_systemd() -> Result<()> {
    try_run_quiet("systemctl", &["disable", "--now", "om-agent.service"]);
    try_run_quiet(
        "systemctl",
        &["disable", "--now", "operation-monitoring-agent.service"],
    );
    remove(&[
        "/etc/systemd/system/om-agent.service",
        "/etc/systemd/system/operation-monitoring-agent.service",
        "/usr/local/bin/om-agent",
        "/usr/local/bin/operation-monitoring-agent",
        "/etc/om-agent",
        "/etc/operation-monitoring-agent",
        "/var/lib/om-agent",
        "/var/lib/operation-monitoring-agent",
        "/var/log/om-agent",
        "/var/log/operation-monitoring-agent",
        "/run/om-agent",
        "/run/operation-monitoring-agent",
    ]);
    try_run_quiet("systemctl", &["daemon-reload"]);
    Ok(())
}
#[cfg(all(unix, not(target_os = "macos")))]
fn install_openwrt(c: &AgentConfig) -> Result<()> {
    try_run_quiet("/etc/init.d/operation-monitoring-agent", &["stop"]);
    try_run_quiet("/etc/init.d/operation-monitoring-agent", &["disable"]);
    try_run_quiet("/etc/init.d/om-agent", &["stop"]);
    migrate_path("/etc/operation-monitoring-agent", "/etc/om-agent")?;
    migrate_path("/var/lib/operation-monitoring-agent", "/var/lib/om-agent")?;
    migrate_path("/var/log/operation-monitoring-agent", "/var/log/om-agent")?;
    migrate_path("/var/run/operation-monitoring-agent", "/var/run/om-agent")?;
    copy_self(Path::new("/usr/bin/om-agent"))?;
    fs::create_dir_all("/etc/config")?;
    fs::write(
        "/etc/config/om-agent",
        format!(
            "config agent 'main'\n\toption enabled '1'\n\toption server '{}'\n\toption report_interval '{}'\n\toption log_max_bytes '{}'\n\toption log_history '{}'\n",
            quoted(&c.server),
            c.report_interval,
            c.log_max_bytes,
            c.log_history
        ),
    )?;
    fs::write("/etc/init.d/om-agent", OPENWRT)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions("/etc/init.d/om-agent", fs::Permissions::from_mode(0o755))?;
    fs::create_dir_all("/etc/om-agent")?;
    fs::write("/etc/om-agent/install-type", "standalone\n")?;
    remove(&[
        "/usr/bin/operation-monitoring-agent",
        "/etc/init.d/operation-monitoring-agent",
        "/etc/config/operation-monitoring-agent",
        "/etc/operation-monitoring-agent",
        "/var/lib/operation-monitoring-agent",
        "/var/log/operation-monitoring-agent",
        "/var/run/operation-monitoring-agent",
    ]);
    // Keep service and marker aliases so older updaters can still roll back.
    replace_symlink("om-agent", "/etc/init.d/operation-monitoring-agent")?;
    replace_symlink("om-agent", "/etc/operation-monitoring-agent")?;
    run("/etc/init.d/om-agent", &["enable"])?;
    run("/etc/init.d/om-agent", &["restart"])?;
    println!("agent installed and started");
    Ok(())
}
#[cfg(all(unix, not(target_os = "macos")))]
fn uninstall_openwrt() -> Result<()> {
    try_run_quiet("/etc/init.d/om-agent", &["stop"]);
    try_run_quiet("/etc/init.d/om-agent", &["disable"]);
    try_run_quiet("/etc/init.d/operation-monitoring-agent", &["stop"]);
    try_run_quiet("/etc/init.d/operation-monitoring-agent", &["disable"]);
    remove(&[
        "/usr/bin/om-agent",
        "/usr/bin/operation-monitoring-agent",
        "/etc/init.d/om-agent",
        "/etc/init.d/operation-monitoring-agent",
        "/etc/config/om-agent",
        "/etc/config/operation-monitoring-agent",
        "/etc/om-agent",
        "/etc/operation-monitoring-agent",
        "/var/lib/om-agent",
        "/var/lib/operation-monitoring-agent",
        "/var/log/om-agent",
        "/var/log/operation-monitoring-agent",
        "/var/run/om-agent",
        "/var/run/operation-monitoring-agent",
    ]);
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_macos(c: &AgentConfig) -> Result<()> {
    copy_self(Path::new("/usr/local/bin/om-agent"))?;
    private_file(
        "/Library/Application Support/OperationMonitoring/agent.env",
        &env_file(c, true),
    )?;
    fs::write(
        "/Library/Application Support/OperationMonitoring/install-type",
        "standalone\n",
    )?;
    fs::create_dir_all("/Library/Logs/OperationMonitoring")?;
    fs::write(
        "/Library/LaunchDaemons/com.operation-monitoring.agent.plist",
        MACOS,
    )?;
    bootout_macos_service()?;
    let _ = fs::remove_file("/usr/local/bin/operation-monitoring-agent");
    run(
        "launchctl",
        &[
            "bootstrap",
            "system",
            "/Library/LaunchDaemons/com.operation-monitoring.agent.plist",
        ],
    )?;
    run(
        "launchctl",
        &["enable", "system/com.operation-monitoring.agent"],
    )?;
    run(
        "launchctl",
        &["kickstart", "-k", "system/com.operation-monitoring.agent"],
    )?;
    println!("agent installed and started");
    Ok(())
}
#[cfg(target_os = "macos")]
fn uninstall_macos() -> Result<()> {
    bootout_macos_service()?;
    remove(&[
        "/Library/LaunchDaemons/com.operation-monitoring.agent.plist",
        "/usr/local/bin/om-agent",
        "/usr/local/bin/operation-monitoring-agent",
        "/Library/Application Support/OperationMonitoring",
        "/Library/Logs/OperationMonitoring",
    ]);
    Ok(())
}

#[cfg(windows)]
fn install_windows(c: &AgentConfig) -> Result<()> {
    let program_files = crate::windows_security::program_files_directory()?;
    let program_data = crate::windows_security::program_data_directory()?;
    let install = crate::privileged_path::prepare_configured_directory(
        &program_files.join("OM Agent"),
        true,
        "Windows agent installation directory",
    )?;
    let legacy_install = program_files.join("Operation Monitoring Agent");
    validate_optional_windows_directory(
        &legacy_install,
        "legacy Windows agent installation directory",
    )?;
    let data = crate::privileged_path::prepare_configured_directory(
        &program_data.join("OperationMonitoring"),
        true,
        "Windows agent data directory",
    )?;
    let runtime = crate::privileged_path::prepare_configured_directory(
        &data.join("runtime"),
        true,
        "Windows agent runtime directory",
    )?;
    crate::privileged_path::prepare_configured_directory(
        &data.join("logs"),
        true,
        "Windows agent log directory",
    )?;
    let updates = crate::privileged_path::prepare_configured_directory(
        &data.join("updates"),
        true,
        "Windows agent update directory",
    )?;
    let _update_lock = acquire_windows_install_update_lock(&updates)?;
    let binary = install.join("om-agent.exe");
    let staged_binary = stage_windows_install_binary(&binary)?;
    let driver_reboot_required = crate::remote_access::stage_bundled_windows_drivers(&data)?;
    let identity_path = data.join("identity.json");
    if identity_path.try_exists()? {
        crate::privileged_path::validate_regular_file(&identity_path, "Windows agent identity")?;
    }
    let install_metadata_path = data.join("install.json");
    let install_type_path = data.join("install-type");
    let metadata_snapshots = [
        WindowsPrivateFileSnapshot::capture(
            &install_metadata_path,
            "Windows installation metadata",
        )?,
        WindowsPrivateFileSnapshot::capture(
            &install_type_path,
            "Windows installation type marker",
        )?,
    ];
    let canonical = snapshot_windows_service(WINDOWS_SERVICE_NAME)?;
    let legacy = snapshot_windows_service(SHORT_WINDOWS_SERVICE_NAME)?;
    let (install_service_name, redundant_service_name) =
        windows_install_service_names(canonical.is_some(), legacy.is_some());
    let image = windows_service_image(&binary, c, &data);
    let install_metadata = serde_json::to_string_pretty(&serde_json::json!({
        "server": c.server,
        "report_interval": c.report_interval,
        "remote_desktop_consent": c.remote_desktop_consent.to_string(),
        "windows_virtual_devices": c.windows_virtual_devices.to_string(),
        "driver_reboot_required": driver_reboot_required
    }))?;

    let mut service_switch_started = false;
    let switch_result = (|| -> Result<()> {
        write_windows_private_file(&install_metadata_path, &install_metadata)?;
        write_windows_private_file(&install_type_path, "standalone\n")?;
        service_switch_started = true;
        stop_windows_service(WINDOWS_SERVICE_NAME)?;
        stop_windows_service(SHORT_WINDOWS_SERVICE_NAME)?;
        staged_binary.activate(&binary)?;
        configure_windows_service(
            install_service_name,
            &image,
            canonical.is_some() || legacy.is_some(),
        )?;
        remove_windows_runtime_marker(&runtime.join("agent.ready"), "agent ready marker")?;
        repair_windows_global_command(&binary)?;
        start_windows_service_and_wait_ready(install_service_name, &data)?;
        Ok(())
    })();
    if let Err(error) = switch_result {
        let rollback_errors = if service_switch_started {
            rollback_windows_install(
                &binary,
                &staged_binary,
                canonical.as_ref(),
                legacy.as_ref(),
                &metadata_snapshots,
            )
        } else {
            restore_windows_private_files(&metadata_snapshots)
        };
        if rollback_errors.is_empty() {
            return Err(error).context("Windows agent installation was rolled back");
        }
        bail!(
            "Windows agent installation failed: {error:#}; rollback also encountered: {}",
            rollback_errors.join("; ")
        );
    }

    if canonical.is_some()
        && legacy.is_some()
        && let Err(error) = stop_and_delete_windows_service(redundant_service_name)
    {
        eprintln!(
            "warning: installed agent is healthy, but redundant Windows service {redundant_service_name} could not be removed: {error:#}"
        );
    }
    if let Err(error) = windows_path(&legacy_install, false) {
        eprintln!(
            "warning: installed agent is healthy, but the legacy installation directory could not be removed from PATH: {error:#}"
        );
    }
    println!(
        "agent installed and started; open a new terminal to use om-agent globally ({})",
        binary.display()
    );
    match env::current_exe() {
        Ok(current) if current.starts_with(&legacy_install) => {
            let command = format!(
                "ping 127.0.0.1 -n 3 >nul & rmdir /S /Q \"{}\"",
                legacy_install.display()
            );
            if let Err(error) = Command::new("cmd.exe").args(["/C", &command]).spawn() {
                eprintln!(
                    "warning: installed agent is healthy, but legacy installation cleanup could not be scheduled: {error}"
                );
            }
        }
        Ok(_) => match fs::remove_dir_all(&legacy_install) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => eprintln!(
                "warning: installed agent is healthy, but legacy installation directory {} could not be removed: {error}",
                legacy_install.display()
            ),
        },
        Err(error) => eprintln!(
            "warning: installed agent is healthy, but the current executable could not be resolved for legacy cleanup: {error}"
        ),
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn windows_install_service_names(
    canonical_exists: bool,
    legacy_exists: bool,
) -> (&'static str, &'static str) {
    if canonical_exists || !legacy_exists {
        (WINDOWS_SERVICE_NAME, SHORT_WINDOWS_SERVICE_NAME)
    } else {
        (SHORT_WINDOWS_SERVICE_NAME, WINDOWS_SERVICE_NAME)
    }
}

#[cfg(windows)]
struct StagedWindowsBinary {
    temporary: Option<std::path::PathBuf>,
    backup: Option<std::path::PathBuf>,
    target_existed: bool,
}

#[cfg(windows)]
impl StagedWindowsBinary {
    fn activate(&self, target: &Path) -> Result<()> {
        let Some(temporary) = &self.temporary else {
            return Ok(());
        };
        crate::windows_security::replace_file(temporary, target)
            .with_context(|| format!("failed to activate staged agent {}", target.display()))
    }

    fn restore(&self, target: &Path) -> Result<()> {
        if self.temporary.is_none() {
            return Ok(());
        }
        if let Some(backup) = &self.backup {
            crate::windows_security::replace_file(backup, target).with_context(|| {
                format!(
                    "failed to restore previous agent binary {}",
                    target.display()
                )
            })?;
        } else if self.target_existed {
            bail!("previous agent binary backup is missing");
        } else if let Err(error) = fs::remove_file(target)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(error).with_context(|| {
                format!(
                    "failed to remove newly installed agent {}",
                    target.display()
                )
            });
        }
        Ok(())
    }

    fn cleanup(&self) {
        for path in [&self.temporary, &self.backup].into_iter().flatten() {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(windows)]
impl Drop for StagedWindowsBinary {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(windows)]
fn stage_windows_install_binary(target: &Path) -> Result<StagedWindowsBinary> {
    let source = env::current_exe()?;
    let target_existed = target.try_exists()?;
    if target_existed {
        crate::privileged_path::validate_regular_file(target, "installed Windows agent")?;
        if files_equal(&source, target)? {
            return Ok(StagedWindowsBinary {
                temporary: None,
                backup: None,
                target_existed: true,
            });
        }
    }

    let unique = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4());
    let temporary = target.with_file_name(format!("om-agent-install-{unique}.new.exe"));
    let backup = target_existed
        .then(|| target.with_file_name(format!("om-agent-install-{unique}.backup.exe")));
    copy_windows_private_file(&source, &temporary, "staged Windows agent")?;
    if let Some(backup) = &backup
        && let Err(error) = copy_windows_private_file(target, backup, "Windows agent backup")
    {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(StagedWindowsBinary {
        temporary: Some(temporary),
        backup,
        target_existed,
    })
}

#[cfg(windows)]
fn copy_windows_private_file(source: &Path, target: &Path, description: &str) -> Result<()> {
    use std::io::Write as _;

    let result = (|| -> Result<()> {
        let mut source_file = fs::File::open(source)
            .with_context(|| format!("failed to open {description} source {}", source.display()))?;
        if !source_file.metadata()?.is_file() {
            bail!(
                "{description} source {} is not a regular file",
                source.display()
            );
        }
        let mut target_file = crate::windows_security::create_private_file(target)
            .with_context(|| format!("failed to create {description} {}", target.display()))?;
        io::copy(&mut source_file, &mut target_file)?;
        target_file.flush()?;
        target_file.sync_all()?;
        drop(target_file);
        crate::privileged_path::validate_regular_file(target, description)
    })();
    if result.is_err() {
        let _ = fs::remove_file(target);
    }
    result
}

#[cfg(windows)]
fn validate_optional_windows_directory(path: &Path, description: &str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                bail!("{description} {} is not a directory", path.display());
            }
            crate::privileged_path::validate_configured_directory(path, true, description)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect {description} {}", path.display())),
    }
}

#[cfg(windows)]
fn write_windows_private_file(path: &Path, contents: &str) -> Result<()> {
    write_windows_private_bytes(path, contents.as_bytes())
}

#[cfg(windows)]
fn write_windows_private_bytes(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write as _;

    if path.try_exists()? {
        crate::privileged_path::validate_regular_file(path, "Windows installation metadata")?;
    }
    let temporary = path.with_file_name(format!(
        ".om-agent-install-{}-{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut file = crate::windows_security::create_private_file(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        crate::windows_security::replace_file(&temporary, path)?;
        crate::privileged_path::validate_regular_file(path, "Windows installation metadata")
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(windows)]
struct WindowsPrivateFileSnapshot {
    path: std::path::PathBuf,
    description: &'static str,
    contents: Option<Vec<u8>>,
}

#[cfg(windows)]
impl WindowsPrivateFileSnapshot {
    fn capture(path: &Path, description: &'static str) -> Result<Self> {
        let contents = match fs::symlink_metadata(path) {
            Ok(_) => {
                crate::privileged_path::validate_regular_file(path, description)?;
                Some(fs::read(path).with_context(|| {
                    format!("failed to snapshot {description} {}", path.display())
                })?)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect {description} {}", path.display())
                });
            }
        };
        Ok(Self {
            path: path.to_owned(),
            description,
            contents,
        })
    }

    fn restore(&self) -> Result<()> {
        if let Some(contents) = &self.contents {
            return write_windows_private_bytes(&self.path, contents).with_context(|| {
                format!(
                    "failed to restore {} {}",
                    self.description,
                    self.path.display()
                )
            });
        }
        match fs::symlink_metadata(&self.path) {
            Ok(_) => {
                crate::privileged_path::validate_regular_file(&self.path, self.description)?;
                fs::remove_file(&self.path).with_context(|| {
                    format!(
                        "failed to remove newly created {} {}",
                        self.description,
                        self.path.display()
                    )
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to inspect {} {} during rollback",
                    self.description,
                    self.path.display()
                )
            }),
        }
    }
}

#[cfg(windows)]
fn restore_windows_private_files(snapshots: &[WindowsPrivateFileSnapshot]) -> Vec<String> {
    snapshots
        .iter()
        .filter_map(|snapshot| {
            snapshot
                .restore()
                .err()
                .map(|error| format!("could not restore {}: {error:#}", snapshot.description))
        })
        .collect()
}

#[cfg(windows)]
fn acquire_windows_install_update_lock(update_dir: &Path) -> Result<fs::File> {
    use fs2::FileExt as _;
    use std::fs::OpenOptions;

    let path = update_dir.join("updater.lock");
    let file = match crate::windows_security::create_private_file(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            crate::privileged_path::validate_regular_file(&path, "agent updater lock")?;
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("failed to open updater lock {}", path.display()))?
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to create updater lock {}", path.display()));
        }
    };
    file.try_lock_exclusive().map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock
            || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
        {
            anyhow::anyhow!(
                "an agent update is currently running; wait for it to finish before installing"
            )
        } else {
            anyhow::Error::new(error).context("failed to acquire the agent update lock")
        }
    })?;
    crate::update::ensure_no_windows_update_handoff_while_locked(update_dir)?;
    Ok(file)
}

#[cfg(windows)]
struct WindowsServiceSnapshot {
    config: windows_service::service::ServiceConfig,
    was_active: bool,
}

#[cfg(windows)]
fn snapshot_windows_service(service_name: &str) -> Result<Option<WindowsServiceSnapshot>> {
    use windows_service::{
        service::ServiceAccess,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let Some(status) = windows_service_status(service_name)? else {
        return Ok(None);
    };
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("failed to open the Windows service manager")?;
    let service = manager
        .open_service(service_name, ServiceAccess::QUERY_CONFIG)
        .with_context(|| format!("failed to open Windows service {service_name}"))?;
    let config = service
        .query_config()
        .with_context(|| format!("failed to query Windows service {service_name}"))?;
    Ok(Some(WindowsServiceSnapshot {
        config,
        was_active: status.state != windows_service::service::ServiceState::Stopped,
    }))
}

#[cfg(windows)]
fn windows_service_image(binary: &Path, c: &AgentConfig, data: &Path) -> std::ffi::OsString {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};

    let arguments = vec![
        OsString::from("service-run"),
        OsString::from("--server"),
        OsString::from(&c.server),
        OsString::from("--report-interval"),
        OsString::from(c.report_interval.to_string()),
        OsString::from("--identity-file"),
        data.join("identity.json").into_os_string(),
        OsString::from("--state-dir"),
        data.join("runtime").into_os_string(),
        OsString::from("--log-file"),
        data.join("logs/agent.log").into_os_string(),
        OsString::from("--log-max-bytes"),
        OsString::from(c.log_max_bytes.to_string()),
        OsString::from("--log-history"),
        OsString::from(c.log_history.to_string()),
        OsString::from("--update-dir"),
        data.join("updates").into_os_string(),
        OsString::from("--remote-desktop-consent"),
        OsString::from(c.remote_desktop_consent.to_string()),
        OsString::from("--windows-virtual-devices"),
        OsString::from(c.windows_virtual_devices.to_string()),
    ];
    let mut encoded = Vec::new();
    append_quoted_windows_argument(&mut encoded, binary.as_os_str());
    for argument in &arguments {
        encoded.push(b' ' as u16);
        append_quoted_windows_argument(&mut encoded, argument);
    }
    OsString::from_wide(&encoded)
}

#[cfg(windows)]
fn append_quoted_windows_argument(output: &mut Vec<u16>, value: &std::ffi::OsStr) {
    use std::os::windows::ffi::OsStrExt as _;

    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    output.push(QUOTE);
    let mut backslashes = 0;
    for unit in value.encode_wide() {
        if unit == BACKSLASH {
            backslashes += 1;
            continue;
        }
        if unit == QUOTE {
            output.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2 + 1));
            output.push(QUOTE);
        } else {
            output.extend(std::iter::repeat_n(BACKSLASH, backslashes));
            output.push(unit);
        }
        backslashes = 0;
    }
    output.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2));
    output.push(QUOTE);
}

#[cfg(windows)]
fn configure_windows_service(
    service_name: &str,
    image: &std::ffi::OsStr,
    exists: bool,
) -> Result<()> {
    let action = if exists { "config" } else { "create" };
    let output = Command::new("sc.exe")
        .arg(action)
        .arg(service_name)
        .args(["start=", "auto", "DisplayName=", "OM Agent", "binPath="])
        .arg(image)
        .output()
        .with_context(|| format!("failed to {action} Windows service {service_name}"))?;
    if output.status.success() {
        return Ok(());
    }
    let details = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    bail!(
        "sc {action} {service_name} exited with {}: {}",
        output.status,
        details.trim()
    )
}

#[cfg(windows)]
fn restore_windows_service_config(
    service_name: &str,
    snapshot: &WindowsServiceSnapshot,
) -> Result<()> {
    use windows_service::service::ServiceStartType;

    let start_type = match snapshot.config.start_type {
        ServiceStartType::AutoStart => "auto",
        ServiceStartType::OnDemand => "demand",
        ServiceStartType::Disabled => "disabled",
        ServiceStartType::SystemStart => "system",
        ServiceStartType::BootStart => "boot",
    };
    let output = Command::new("sc.exe")
        .args(["config", service_name, "start=", start_type, "DisplayName="])
        .arg(&snapshot.config.display_name)
        .arg("binPath=")
        .arg(snapshot.config.executable_path.as_os_str())
        .output()
        .with_context(|| format!("failed to restore Windows service {service_name}"))?;
    if output.status.success() {
        return Ok(());
    }
    let details = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    bail!(
        "sc config {service_name} exited with {} while restoring it: {}",
        output.status,
        details.trim()
    )
}

#[cfg(windows)]
fn rollback_windows_install(
    binary: &Path,
    staged_binary: &StagedWindowsBinary,
    canonical: Option<&WindowsServiceSnapshot>,
    legacy: Option<&WindowsServiceSnapshot>,
    metadata_snapshots: &[WindowsPrivateFileSnapshot],
) -> Vec<String> {
    let mut errors = Vec::new();
    for service_name in [WINDOWS_SERVICE_NAME, SHORT_WINDOWS_SERVICE_NAME] {
        if let Err(error) = stop_windows_service(service_name) {
            errors.push(format!("could not stop {service_name}: {error:#}"));
        }
    }
    if let Err(error) = staged_binary.restore(binary) {
        errors.push(format!("could not restore previous binary: {error:#}"));
    }
    for (service_name, snapshot) in [
        (WINDOWS_SERVICE_NAME, canonical),
        (SHORT_WINDOWS_SERVICE_NAME, legacy),
    ] {
        if let Some(snapshot) = snapshot {
            if let Err(error) = restore_windows_service_config(service_name, snapshot) {
                errors.push(format!(
                    "could not restore {service_name} configuration: {error:#}"
                ));
            }
        } else if service_name == WINDOWS_SERVICE_NAME
            && let Err(error) = stop_and_delete_windows_service(service_name)
        {
            errors.push(format!("could not remove replacement service: {error:#}"));
        }
    }
    if binary.try_exists().unwrap_or(false) {
        if let Err(error) = repair_windows_global_command(binary) {
            errors.push(format!("could not restore the global command: {error:#}"));
        }
    } else {
        for command in windows_command_paths().unwrap_or_default() {
            if let Err(error) = fs::remove_file(&command)
                && error.kind() != io::ErrorKind::NotFound
            {
                errors.push(format!(
                    "could not remove global command {}: {error}",
                    command.display()
                ));
            }
        }
    }
    errors.extend(restore_windows_private_files(metadata_snapshots));
    for (service_name, snapshot) in [
        (WINDOWS_SERVICE_NAME, canonical),
        (SHORT_WINDOWS_SERVICE_NAME, legacy),
    ] {
        if snapshot.is_some_and(|snapshot| snapshot.was_active)
            && let Err(error) = start_windows_service_and_wait_running(service_name)
        {
            errors.push(format!("could not restart {service_name}: {error:#}"));
        }
    }
    errors
}

#[cfg(windows)]
fn remove_windows_runtime_marker(path: &Path, description: &str) -> Result<()> {
    match path.symlink_metadata() {
        Ok(_) => {
            crate::privileged_path::validate_regular_file(path, description)?;
            fs::remove_file(path)
                .with_context(|| format!("failed to remove {description} {}", path.display()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect {description} {}", path.display())),
    }
}

#[cfg(windows)]
fn start_windows_service_and_wait_running(service_name: &str) -> Result<()> {
    use std::{thread, time::Duration};

    let output = Command::new("sc.exe")
        .args(["start", service_name])
        .output()
        .with_context(|| format!("failed to start Windows service {service_name}"))?;
    if !output.status.success()
        && !windows_service_status(service_name)?
            .is_some_and(|status| status.state == windows_service::service::ServiceState::Running)
    {
        let details = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        bail!(
            "sc start {service_name} exited with {}: {}",
            output.status,
            details.trim()
        );
    }
    let started = std::time::Instant::now();
    loop {
        match windows_service_status(service_name)? {
            Some(status) if status.state == windows_service::service::ServiceState::Running => {
                return Ok(());
            }
            Some(status) if status.state == windows_service::service::ServiceState::Stopped => {
                bail!("Windows service {service_name} stopped during startup")
            }
            None => bail!("Windows service {service_name} disappeared during startup"),
            Some(_) if started.elapsed() < Duration::from_secs(30) => {
                thread::sleep(Duration::from_millis(250));
            }
            Some(_) => bail!("Windows service {service_name} did not start within 30 seconds"),
        }
    }
}

#[cfg(windows)]
fn start_windows_service_and_wait_ready(service_name: &str, data: &Path) -> Result<()> {
    use std::{thread, time::Duration};

    start_windows_service_and_wait_running(service_name)?;
    let ready_file = data.join("runtime/agent.ready");
    let log_file = data.join("logs/agent.log");
    let started = std::time::Instant::now();
    let mut stable_since = None;
    loop {
        let status = windows_service_status(service_name)?.with_context(|| {
            format!("Windows service {service_name} disappeared during startup")
        })?;
        if status.state == windows_service::service::ServiceState::Stopped {
            bail!(
                "Windows service {service_name} stopped during startup; inspect {}",
                log_file.display()
            );
        }
        let ready_pid = fs::read_to_string(&ready_file)
            .ok()
            .and_then(|value| parse_windows_ready_pid(&value));
        if status.state == windows_service::service::ServiceState::Running
            && status.pid.is_some()
            && ready_pid == status.pid
        {
            let stable = stable_since.get_or_insert_with(std::time::Instant::now);
            if stable.elapsed() >= Duration::from_secs(2) {
                return Ok(());
            }
        } else {
            stable_since = None;
        }
        if started.elapsed() >= Duration::from_secs(30) {
            bail!(
                "Windows service {service_name} did not remain RUNNING and ready within 30 seconds; inspect {}",
                log_file.display()
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(windows)]
pub(crate) fn repair_windows_global_command(installed_executable: &Path) -> Result<()> {
    let install_dir = installed_executable
        .parent()
        .context("installed Windows executable has no parent directory")?;
    windows_path(install_dir, true)?;
    for command in windows_command_paths()? {
        install_windows_command_entry(installed_executable, &command)?;
    }
    Ok(())
}

#[cfg(windows)]
fn install_windows_command_entry(installed_executable: &Path, command: &Path) -> Result<()> {
    if files_equal(installed_executable, command)? {
        return Ok(());
    }

    // The command entry is normally a hard link created during installation. Updating its
    // contents in place avoids requiring DELETE access on the protected System32 directory and
    // keeps that hard link valid after the installed executable is replaced.
    let command_is_regular_file =
        fs::symlink_metadata(command).is_ok_and(|metadata| metadata.file_type().is_file());
    if command_is_regular_file {
        let overwrite = (|| -> Result<()> {
            make_windows_file_writable(command)?;
            fs::copy(installed_executable, command).with_context(|| {
                format!(
                    "failed to update global command contents {}",
                    command.display()
                )
            })?;
            if !files_equal(installed_executable, command)? {
                bail!(
                    "global command {} does not match the installed executable after in-place update",
                    command.display()
                );
            }
            Ok(())
        })();
        if overwrite.is_ok() {
            return Ok(());
        }
    }

    let temporary =
        command.with_file_name(format!("om-agent-command-{}.new.exe", std::process::id()));
    let _ = make_windows_file_writable(&temporary);
    let _ = fs::remove_file(&temporary);

    if let Err(link_error) = fs::hard_link(installed_executable, &temporary) {
        fs::copy(installed_executable, &temporary).with_context(|| {
            format!(
                "failed to create global command {} after hard-link creation failed: {link_error}",
                temporary.display()
            )
        })?;
    }

    let _ = make_windows_file_writable(&command);
    if let Err(error) = fs::remove_file(&command)
        && error.kind() != io::ErrorKind::NotFound
    {
        let _ = fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("failed to replace global command {}", command.display()));
    }
    if let Err(error) = fs::rename(&temporary, &command) {
        let _ = fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("failed to activate global command {}", command.display()));
    }

    if !files_equal(installed_executable, &command)? {
        bail!(
            "global command {} does not match the installed executable after replacement",
            command.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn make_windows_file_writable(path: &Path) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    if permissions.readonly() {
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(windows)]
fn files_equal(left: &Path, right: &Path) -> Result<bool> {
    use std::io::Read;

    let left_metadata = fs::metadata(left)?;
    let right_metadata = match fs::metadata(right) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if left_metadata.len() != right_metadata.len() {
        return Ok(false);
    }

    let mut left = io::BufReader::new(fs::File::open(left)?);
    let mut right = io::BufReader::new(fs::File::open(right)?);
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];
    loop {
        let left_read = left.read(&mut left_buffer)?;
        let right_read = right.read(&mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

#[cfg(windows)]
fn windows_command_paths() -> Result<Vec<std::path::PathBuf>> {
    let system_root = crate::windows_security::windows_directory()?;
    let is_wow64 = cfg!(target_arch = "x86") && env::var_os("PROCESSOR_ARCHITEW6432").is_some();
    let has_syswow64 = system_root.join("SysWOW64").is_dir();
    Ok(windows_command_paths_from_root(
        &system_root,
        is_wow64,
        has_syswow64,
    ))
}

#[cfg(any(windows, test))]
fn windows_command_paths_from_root(
    system_root: &Path,
    is_wow64: bool,
    has_syswow64: bool,
) -> Vec<std::path::PathBuf> {
    let mut directories = vec![if is_wow64 { "Sysnative" } else { "System32" }];
    if has_syswow64 {
        directories.push(if is_wow64 { "System32" } else { "SysWOW64" });
    }
    directories
        .into_iter()
        .map(|directory| system_root.join(directory).join("om-agent.exe"))
        .collect()
}

#[cfg(windows)]
fn uninstall_windows() -> Result<()> {
    let program_files = crate::windows_security::program_files_directory()?;
    let program_data = crate::windows_security::program_data_directory()?;
    let install = program_files.join("OM Agent");
    let legacy_install = program_files.join("Operation Monitoring Agent");
    let data = program_data.join("OperationMonitoring");
    validate_optional_windows_directory(&install, "Windows agent installation directory")?;
    validate_optional_windows_directory(
        &legacy_install,
        "legacy Windows agent installation directory",
    )?;
    let data_exists = validate_optional_windows_directory(&data, "Windows agent data directory")?;
    let update_lock = if data_exists {
        let updates = crate::privileged_path::prepare_configured_directory(
            &data.join("updates"),
            true,
            "Windows agent update directory",
        )?;
        Some(acquire_windows_install_update_lock(&updates)?)
    } else {
        None
    };
    stop_and_delete_windows_service(WINDOWS_SERVICE_NAME)?;
    stop_and_delete_windows_service(SHORT_WINDOWS_SERVICE_NAME)?;
    crate::remote_access::uninstall_product_windows_drivers(&data)?;
    windows_path(&install, false)?;
    windows_path(&legacy_install, false)?;
    drop(update_lock);
    match fs::remove_dir_all(&data) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to remove Windows agent data {}; identity keys and update state may remain",
                    data.display()
                )
            });
        }
    }
    if data.try_exists()? {
        bail!(
            "Windows agent data {} still exists after removal; identity keys and update state may remain",
            data.display()
        );
    }
    let mut cleanup = String::from("ping 127.0.0.1 -n 3 >nul");
    for command in windows_command_paths()? {
        cleanup.push_str(&format!(" & del /F /Q \"{}\" >nul 2>&1", command.display()));
    }
    cleanup.push_str(&format!(
        " & rmdir /S /Q \"{}\" >nul 2>&1 & rmdir /S /Q \"{}\" >nul 2>&1",
        install.display(),
        legacy_install.display()
    ));
    Command::new("cmd.exe")
        .args(["/D", "/S", "/C", &cleanup])
        .spawn()
        .context("failed to schedule Windows uninstall cleanup")?;
    Ok(())
}
#[cfg(windows)]
fn windows_path(path: &Path, add: bool) -> Result<()> {
    use std::{mem::size_of, os::windows::ffi::OsStrExt};
    use windows::{
        Win32::{
            Foundation::{LPARAM, WPARAM},
            System::Registry::{
                HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ, REG_SZ,
                REG_VALUE_TYPE, RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
            },
            UI::WindowsAndMessaging::{
                HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
            },
        },
        core::PCWSTR,
    };

    struct RegistryKey(HKEY);
    impl Drop for RegistryKey {
        fn drop(&mut self) {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    let subkey = wide(r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment");
    let value_name = wide("Path");
    let mut raw_key = HKEY::default();
    unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            None,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            &mut raw_key,
        )
        .ok()
        .context("failed to open the machine environment registry key")?;
    }
    let key = RegistryKey(raw_key);

    let mut value_type = REG_VALUE_TYPE::default();
    let mut byte_len = 0_u32;
    unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(value_name.as_ptr()),
            None,
            Some(&mut value_type),
            None,
            Some(&mut byte_len),
        )
        .ok()
        .context("failed to read the machine PATH size")?;
    }
    if value_type != REG_SZ && value_type != REG_EXPAND_SZ {
        bail!("machine PATH has an unsupported registry value type");
    }
    if byte_len as usize % size_of::<u16>() != 0 {
        bail!("machine PATH contains malformed UTF-16 data");
    }

    let mut buffer = vec![0_u16; byte_len as usize / size_of::<u16>()];
    unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(value_name.as_ptr()),
            None,
            Some(&mut value_type),
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut byte_len),
        )
        .ok()
        .context("failed to read the machine PATH")?;
    }
    let current = String::from_utf16(buffer.strip_suffix(&[0]).unwrap_or(buffer.as_slice()))
        .context("machine PATH contains invalid UTF-16")?;
    let target = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let target = String::from_utf16(&target).context("install path contains invalid UTF-16")?;
    let value = update_windows_path(&current, &target, add);
    let encoded = value.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let bytes = unsafe {
        std::slice::from_raw_parts(
            encoded.as_ptr().cast::<u8>(),
            encoded.len() * size_of::<u16>(),
        )
    };
    unsafe {
        RegSetValueExW(
            key.0,
            PCWSTR(value_name.as_ptr()),
            None,
            value_type,
            Some(bytes),
        )
        .ok()
        .context("failed to update the machine PATH")?;
    }

    // Notify Explorer and other long-running applications so terminals opened after installation
    // inherit the new machine PATH. An already-open cmd.exe still needs to be reopened.
    let environment = wide("Environment");
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(environment.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            5_000,
            None,
        );
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn update_windows_path(current: &str, target: &str, add: bool) -> String {
    let matches_target = |entry: &str| {
        entry
            .trim()
            .trim_matches('"')
            .trim_end_matches(['\\', '/'])
            .eq_ignore_ascii_case(target.trim_end_matches(['\\', '/']))
    };
    let mut entries = current
        .split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty() && !matches_target(entry))
        .collect::<Vec<_>>();
    if add {
        entries.push(target);
    }
    entries.join(";")
}

#[cfg(any(windows, test))]
fn parse_windows_ready_pid(value: &str) -> Option<u32> {
    value.trim().parse::<u32>().ok().filter(|pid| *pid != 0)
}

#[cfg(windows)]
struct WindowsServiceStatus {
    state: windows_service::service::ServiceState,
    pid: Option<u32>,
}

#[cfg(windows)]
fn windows_service_error_is_missing(error: &windows_service::Error) -> bool {
    matches!(
        error,
        windows_service::Error::Winapi(error) if error.raw_os_error() == Some(1060)
    )
}

#[cfg(windows)]
fn windows_service_status(service_name: &str) -> Result<Option<WindowsServiceStatus>> {
    use windows_service::{
        service::ServiceAccess,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("failed to open the Windows service manager")?;
    let service = match manager.open_service(service_name, ServiceAccess::QUERY_STATUS) {
        Ok(service) => service,
        Err(error) if windows_service_error_is_missing(&error) => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to open Windows service {service_name}"));
        }
    };
    let status = service
        .query_status()
        .with_context(|| format!("failed to query Windows service {service_name}"))?;
    Ok(Some(WindowsServiceStatus {
        state: status.current_state,
        pid: status.process_id.filter(|pid| *pid != 0),
    }))
}

#[cfg(windows)]
fn stop_windows_service(service_name: &str) -> Result<()> {
    use std::{thread, time::Duration};
    use windows_service::service::ServiceState;

    let started = std::time::Instant::now();
    let mut stop_requested = false;
    loop {
        let Some(status) = windows_service_status(service_name)? else {
            return Ok(());
        };
        match status.state {
            ServiceState::Stopped => return Ok(()),
            ServiceState::Running | ServiceState::Paused if !stop_requested => {
                let output = Command::new("sc.exe")
                    .args(["stop", service_name])
                    .output()
                    .with_context(|| format!("failed to stop Windows service {service_name}"))?;
                stop_requested = true;
                if !output.status.success() {
                    let current = windows_service_status(service_name)?;
                    let progressing = current.as_ref().is_none_or(|status| {
                        matches!(
                            status.state,
                            ServiceState::Stopped | ServiceState::StopPending
                        )
                    });
                    if !progressing {
                        let details = format!(
                            "{}{}",
                            String::from_utf8_lossy(&output.stdout),
                            String::from_utf8_lossy(&output.stderr)
                        );
                        bail!(
                            "sc stop {service_name} exited with {}: {}",
                            output.status,
                            details.trim()
                        );
                    }
                }
            }
            _ => {}
        }
        if started.elapsed() >= Duration::from_secs(30) {
            bail!("Windows service {service_name} did not stop within 30 seconds");
        }
        thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(windows)]
fn windows_service_state(service_name: &str) -> Result<Option<u32>> {
    Ok(windows_service_status(service_name)?.map(|status| status.state as u32))
}

#[cfg(windows)]
pub(crate) fn stop_and_delete_windows_service(service_name: &str) -> Result<()> {
    use std::{
        thread,
        time::{Duration, Instant},
    };

    let Some(_) = windows_service_state(service_name)? else {
        return Ok(());
    };
    stop_windows_service(service_name)?;
    if windows_service_state(service_name)?.is_some() {
        run("sc.exe", &["delete", service_name])?;
        let started = Instant::now();
        loop {
            match windows_service_state(service_name)? {
                None => break,
                Some(_) if started.elapsed() < Duration::from_secs(10) => {
                    thread::sleep(Duration::from_millis(250));
                }
                Some(_) => bail!("Windows service deletion did not finish within 10 seconds"),
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn windows_runas(action: &str, c: Option<&AgentConfig>) -> Result<()> {
    use std::{ffi::OsStr, mem::size_of, os::windows::ffi::OsStrExt};
    use windows::{
        Win32::{
            Foundation::{CloseHandle, WAIT_OBJECT_0},
            System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject},
            UI::{
                Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
                WindowsAndMessaging::SW_SHOWNORMAL,
            },
        },
        core::PCWSTR,
    };
    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }
    let exe = wide(env::current_exe()?.as_os_str());
    let args = windows_elevation_arguments(action, c);
    let verb = wide(OsStr::new("runas"));
    let args = wide(OsStr::new(&args));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(args.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).context("UAC elevation failed")?;
        let wait = WaitForSingleObject(info.hProcess, INFINITE);
        if wait != WAIT_OBJECT_0 {
            let _ = CloseHandle(info.hProcess);
            bail!("failed while waiting for elevated installer")
        }
        let mut exit_code = 1_u32;
        let result = GetExitCodeProcess(info.hProcess, &mut exit_code);
        let _ = CloseHandle(info.hProcess);
        result.context("failed to read elevated installer exit code")?;
        if exit_code != 0 {
            bail!("elevated installer exited with code {exit_code}")
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn windows_elevation_arguments(action: &str, c: Option<&AgentConfig>) -> String {
    if let Some(c) = c {
        format!(
            "{action} --yes --non-interactive --server \"{}\" --report-interval {} --log-max-bytes {} --log-history {} --remote-desktop-consent {} --windows-virtual-devices {}",
            c.server,
            c.report_interval,
            c.log_max_bytes,
            c.log_history,
            c.remote_desktop_consent,
            c.windows_virtual_devices
        )
    } else {
        match action {
            "uninstall" => format!("{action} --yes"),
            _ => action.to_owned(),
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
const SYSTEMD: &str = "[Unit]\nDescription=OM Agent\nAfter=network-online.target\nWants=network-online.target\n[Service]\nType=simple\nEnvironmentFile=-/etc/om-agent/agent.env\nExecStart=/usr/local/bin/om-agent service-run\nRestart=always\nRestartSec=5\nRuntimeDirectory=om-agent\nStateDirectory=om-agent\nUMask=0077\n[Install]\nWantedBy=multi-user.target\n";
#[cfg(all(unix, not(target_os = "macos")))]
const OPENWRT: &str = r#"#!/bin/sh /etc/rc.common
USE_PROCD=1
START=95
start_service() {
 config_load om-agent
 config_get_bool enabled main enabled 1
 [ "$enabled" -eq 1 ] || return 0
 config_get server main server 'http://127.0.0.1:13500'
 config_get interval main report_interval '5'
 config_get log_max_bytes main log_max_bytes '10485760'
 config_get log_history main log_history '3'
 procd_open_instance
 procd_set_param command /usr/bin/om-agent service-run --server "$server" --report-interval "$interval" --identity-file /etc/om-agent/identity.json --state-dir /var/run/om-agent --log-file /var/log/om-agent/agent.log --log-max-bytes "$log_max_bytes" --log-history "$log_history" --update-dir /var/lib/om-agent/updates
 procd_set_param respawn 3600 5 5
 procd_set_param stdout 1
 procd_set_param stderr 1
 procd_close_instance
}
"#;
#[cfg(target_os = "macos")]
const MACOS: &str = r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict><key>Label</key><string>com.operation-monitoring.agent</string><key>ProgramArguments</key><array><string>/bin/sh</string><string>-c</string><string>set -a; . '/Library/Application Support/OperationMonitoring/agent.env'; exec /usr/local/bin/om-agent service-run</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><true/></dict></plist>"#;

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::install_windows_command_entry;
    use super::{
        explicit_windows_install_path_override, migrate_path, parse_windows_ready_pid,
        update_windows_path, validate_unattended_install_confirmation,
        windows_command_paths_from_root, windows_elevation_arguments,
        windows_install_service_names,
    };
    use crate::config::AgentConfig;
    use std::fs;

    #[test]
    fn migration_merges_legacy_data_without_overwriting_current_files() {
        let root =
            std::env::temp_dir().join(format!("om-agent-migration-{}", uuid::Uuid::new_v4()));
        let legacy = root.join("legacy");
        let current = root.join("current");
        fs::create_dir_all(legacy.join("updates")).unwrap();
        fs::create_dir_all(&current).unwrap();
        fs::write(legacy.join("identity.json"), "legacy identity").unwrap();
        fs::write(legacy.join("updates/state.json"), "legacy update").unwrap();
        fs::write(current.join("identity.json"), "current identity").unwrap();

        migrate_path(legacy.to_str().unwrap(), current.to_str().unwrap()).unwrap();

        assert_eq!(
            fs::read_to_string(current.join("identity.json")).unwrap(),
            "current identity"
        );
        assert_eq!(
            fs::read_to_string(current.join("updates/state.json")).unwrap(),
            "legacy update"
        );
        assert!(!legacy.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn windows_elevation_preserves_log_rotation_options() {
        let config = AgentConfig {
            server: "https://monitor.example".to_string(),
            identity_file: None,
            report_interval: 9,
            state_dir: None,
            log_file: None,
            log_max_bytes: 2_048,
            log_history: 7,
            update_dir: None,
            remote_desktop_consent: crate::config::RemoteDesktopConsent::Unattended,
            windows_virtual_devices: crate::config::WindowsVirtualDevices::Auto,
        };

        assert_eq!(
            windows_elevation_arguments("install", Some(&config)),
            "install --yes --non-interactive --server \"https://monitor.example\" --report-interval 9 --log-max-bytes 2048 --log-history 7 --remote-desktop-consent unattended --windows-virtual-devices auto"
        );
    }

    #[test]
    fn windows_service_control_elevation_uses_valid_cli_arguments() {
        assert_eq!(windows_elevation_arguments("start", None), "start");
        assert_eq!(windows_elevation_arguments("stop", None), "stop");
        assert_eq!(
            windows_elevation_arguments("uninstall", None),
            "uninstall --yes"
        );
    }

    #[test]
    fn windows_system_install_rejects_custom_runtime_paths() {
        let mut config = AgentConfig {
            server: "https://monitor.example".to_string(),
            identity_file: None,
            report_interval: 5,
            state_dir: None,
            log_file: None,
            log_max_bytes: 1024,
            log_history: 1,
            update_dir: None,
            remote_desktop_consent: crate::config::RemoteDesktopConsent::Required,
            windows_virtual_devices: crate::config::WindowsVirtualDevices::Disabled,
        };
        assert_eq!(explicit_windows_install_path_override(&config), None);
        config.state_dir = Some(std::path::PathBuf::from(r"C:\custom-state"));
        assert_eq!(
            explicit_windows_install_path_override(&config),
            Some("--state-dir/OM_AGENT_STATE_DIR")
        );
    }

    #[test]
    fn windows_ready_pid_rejects_zero_and_malformed_values() {
        assert_eq!(parse_windows_ready_pid(" 42\r\n"), Some(42));
        assert_eq!(parse_windows_ready_pid("0"), None);
        assert_eq!(parse_windows_ready_pid("not-a-pid"), None);
    }

    #[test]
    fn reinstall_keeps_the_existing_windows_service_name() {
        assert_eq!(
            windows_install_service_names(true, false),
            ("operation-monitoring-agent", "om-agent")
        );
        assert_eq!(
            windows_install_service_names(false, true),
            ("om-agent", "operation-monitoring-agent")
        );
        assert_eq!(
            windows_install_service_names(false, false),
            ("operation-monitoring-agent", "om-agent")
        );
    }

    #[test]
    fn non_interactive_unattended_install_requires_explicit_confirmation() {
        let config = AgentConfig {
            server: "https://monitor.example".to_string(),
            identity_file: None,
            report_interval: 5,
            state_dir: None,
            log_file: None,
            log_max_bytes: 1024,
            log_history: 1,
            update_dir: None,
            remote_desktop_consent: crate::config::RemoteDesktopConsent::Unattended,
            windows_virtual_devices: crate::config::WindowsVirtualDevices::Auto,
        };
        assert!(validate_unattended_install_confirmation(&config, true, false).is_err());
        assert!(validate_unattended_install_confirmation(&config, true, true).is_ok());
        assert!(validate_unattended_install_confirmation(&config, false, false).is_ok());
    }

    #[test]
    fn windows_path_adds_the_install_directory_without_losing_existing_entries() {
        let path = update_windows_path(
            r"%SystemRoot%\system32;C:\Tools",
            r"C:\Program Files\OM Agent",
            true,
        );

        assert_eq!(
            path,
            r"%SystemRoot%\system32;C:\Tools;C:\Program Files\OM Agent"
        );
    }

    #[test]
    fn windows_path_repairs_equivalent_install_directory_entries() {
        let path = update_windows_path(
            r#"C:\Tools;"C:\Program Files\OM Agent\";C:\Other"#,
            r"C:\Program Files\OM Agent",
            true,
        );

        assert_eq!(path, r"C:\Tools;C:\Other;C:\Program Files\OM Agent");
    }

    #[test]
    fn windows_path_removes_all_install_directory_entries() {
        let path = update_windows_path(
            r"C:\Program Files\OM Agent;C:\Tools;c:\program files\om agent\",
            r"C:\Program Files\OM Agent",
            false,
        );

        assert_eq!(path, r"C:\Tools");
    }

    #[test]
    fn windows_command_uses_system32_for_native_agents() {
        assert_eq!(
            windows_command_paths_from_root(std::path::Path::new(r"C:\Windows"), false, false),
            vec![
                std::path::PathBuf::from(r"C:\Windows")
                    .join("System32")
                    .join("om-agent.exe")
            ]
        );
    }

    #[test]
    fn windows_x86_command_covers_native_and_wow64_system_directories() {
        assert_eq!(
            windows_command_paths_from_root(std::path::Path::new(r"C:\Windows"), true, true),
            vec![
                std::path::PathBuf::from(r"C:\Windows")
                    .join("Sysnative")
                    .join("om-agent.exe"),
                std::path::PathBuf::from(r"C:\Windows")
                    .join("System32")
                    .join("om-agent.exe")
            ]
        );
    }

    #[test]
    fn windows_x64_command_covers_system32_and_syswow64() {
        assert_eq!(
            windows_command_paths_from_root(std::path::Path::new(r"C:\Windows"), false, true),
            vec![
                std::path::PathBuf::from(r"C:\Windows")
                    .join("System32")
                    .join("om-agent.exe"),
                std::path::PathBuf::from(r"C:\Windows")
                    .join("SysWOW64")
                    .join("om-agent.exe")
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_command_update_clears_readonly_before_overwriting() {
        let root = std::env::temp_dir().join(format!("om-agent-command-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let installed = root.join("installed.exe");
        let command = root.join("om-agent.exe");
        fs::write(&installed, b"new-agent").unwrap();
        fs::write(&command, b"old-agent").unwrap();
        let mut permissions = fs::metadata(&command).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&command, permissions).unwrap();

        install_windows_command_entry(&installed, &command).unwrap();

        assert_eq!(fs::read(&command).unwrap(), b"new-agent");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn migration_removes_a_legacy_alias_without_touching_current_data() {
        let root = std::env::temp_dir().join(format!("om-agent-alias-{}", uuid::Uuid::new_v4()));
        let legacy = root.join("legacy");
        let current = root.join("current");
        fs::create_dir_all(&current).unwrap();
        fs::write(current.join("identity.json"), "current identity").unwrap();
        std::os::unix::fs::symlink(&current, &legacy).unwrap();

        migrate_path(legacy.to_str().unwrap(), current.to_str().unwrap()).unwrap();

        assert!(!legacy.exists());
        assert_eq!(
            fs::read_to_string(current.join("identity.json")).unwrap(),
            "current identity"
        );
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use std::{cell::Cell, time::Duration};

    use super::{
        MACOS, macos_service_not_loaded, macos_service_query_not_found,
        wait_for_macos_service_unloaded,
    };

    #[test]
    fn accepts_only_launchctl_no_such_process_as_not_loaded() {
        assert!(macos_service_not_loaded(Some(3)));
        assert!(!macos_service_not_loaded(Some(1)));
        assert!(!macos_service_not_loaded(None));
    }

    #[test]
    fn launch_daemon_uses_the_short_executable_name() {
        assert!(MACOS.contains("exec /usr/local/bin/om-agent service-run"));
        assert!(!MACOS.contains("exec /usr/local/bin/operation-monitoring-agent service-run"));
    }

    #[test]
    fn launch_daemon_does_not_write_the_agent_managed_log() {
        assert!(!MACOS.contains("StandardOutPath"));
        assert!(!MACOS.contains("StandardErrorPath"));
        assert!(!MACOS.contains("/Library/Logs/OperationMonitoring/agent.log"));
    }

    #[test]
    fn recognizes_launchctl_print_missing_service_message() {
        assert!(macos_service_query_not_found(
            "Could not find service \"com.operation-monitoring.agent\" in domain for system"
        ));
        assert!(!macos_service_query_not_found("Operation not permitted"));
    }

    #[test]
    fn waits_until_launchd_has_removed_the_service() {
        let queries = Cell::new(0);

        wait_for_macos_service_unloaded(Duration::from_secs(1), Duration::ZERO, || {
            let current = queries.get() + 1;
            queries.set(current);
            Ok(current < 3)
        })
        .unwrap();

        assert_eq!(queries.get(), 3);
    }

    #[test]
    fn stops_waiting_when_launchd_removal_times_out() {
        let error = wait_for_macos_service_unloaded(Duration::ZERO, Duration::ZERO, || Ok(true))
            .unwrap_err();

        assert!(format!("{error:#}").contains("did not finish unloading"));
    }
}

#[cfg(windows)]
mod windows_service_impl {
    use super::{SHORT_WINDOWS_SERVICE_NAME, WINDOWS_SERVICE_NAME};
    use crate::{config::AgentConfig, lifecycle::run_agent};
    use anyhow::{Context, Result};
    use std::{
        ffi::OsString,
        fs::{self, OpenOptions},
        io::{self, Write},
        sync::OnceLock,
        time::Duration,
    };
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };
    static CONFIG: OnceLock<AgentConfig> = OnceLock::new();
    static ACTIVE_SERVICE_NAME: OnceLock<&'static str> = OnceLock::new();
    define_windows_service!(ffi_main, service_main);
    pub fn run(c: AgentConfig) -> Result<()> {
        CONFIG
            .set(c)
            .map_err(|_| anyhow::anyhow!("service config already initialized"))?;
        match service_dispatcher::start(WINDOWS_SERVICE_NAME, ffi_main) {
            Ok(()) => Ok(()),
            Err(legacy_error) => service_dispatcher::start(SHORT_WINDOWS_SERVICE_NAME, ffi_main)
                .map_err(|short_error| {
                    anyhow::anyhow!(
                        "failed to connect to either Windows service name: {WINDOWS_SERVICE_NAME}: {legacy_error}; {SHORT_WINDOWS_SERVICE_NAME}: {short_error}"
                    )
                }),
        }
    }
    fn service_main(arguments: Vec<OsString>) {
        let service_name = service_name_from_arguments(&arguments);
        let _ = ACTIVE_SERVICE_NAME.set(service_name);
        if let Err(error) = inner() {
            // Logging is initialized after the legacy ACL migration. Keep a small bootstrap
            // record so a service that dies during that phase is diagnosable from the machine.
            if let Some(config) = CONFIG.get() {
                write_bootstrap_error(config, &error);
            }
            crate::logging::error(format_args!("service failed: {error:#}"));
        }
    }

    fn service_name_from_arguments(arguments: &[OsString]) -> &'static str {
        arguments
            .first()
            .map(|name| name.to_string_lossy())
            .filter(|name| name.eq_ignore_ascii_case(SHORT_WINDOWS_SERVICE_NAME))
            .map(|_| SHORT_WINDOWS_SERVICE_NAME)
            .unwrap_or(WINDOWS_SERVICE_NAME)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn accepts_both_supported_service_names() {
            assert_eq!(
                service_name_from_arguments(&[OsString::from("operation-monitoring-agent")]),
                WINDOWS_SERVICE_NAME
            );
            assert_eq!(
                service_name_from_arguments(&[OsString::from("OM-AGENT")]),
                SHORT_WINDOWS_SERVICE_NAME
            );
            assert_eq!(service_name_from_arguments(&[]), WINDOWS_SERVICE_NAME);
        }
    }

    fn write_bootstrap_error(config: &AgentConfig, error: &anyhow::Error) {
        let path = config.log_file.clone().or_else(|| {
            crate::windows_security::program_data_directory()
                .ok()
                .map(|program_data| {
                    program_data
                        .join("OperationMonitoring")
                        .join("logs")
                        .join("agent.log")
                })
        });
        let Some(path) = path else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        let Ok(parent) = crate::privileged_path::prepare_configured_directory(
            parent,
            true,
            "agent bootstrap log directory",
        ) else {
            return;
        };
        let Some(file_name) = path.file_name() else {
            return;
        };
        let path = parent.join(file_name);
        if path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
            || (path.try_exists().is_ok_and(|exists| exists)
                && crate::privileged_path::validate_regular_file(&path, "agent bootstrap log")
                    .is_err())
        {
            return;
        }
        let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
            return;
        };
        let _ = writeln!(
            file,
            "[{}] [ERROR] Windows service bootstrap failed: {error:#}",
            crate::time::now_ts()
        );
    }

    fn set_stopped_status(
        handle: &windows_service::service_control_handler::ServiceStatusHandle,
        result: &Result<()>,
    ) {
        let _ = handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(if result.is_ok() { 0 } else { 1 }),
            checkpoint: 0,
            wait_hint: Duration::ZERO,
            process_id: None,
        });
    }

    fn service_control_error_code(error: &io::Error) -> u32 {
        error
            .raw_os_error()
            .and_then(|code| u32::try_from(code).ok())
            .filter(|code| *code != 0)
            .unwrap_or(windows_sys::Win32::Foundation::ERROR_WRITE_FAULT)
    }

    fn inner() -> Result<()> {
        let c = CONFIG.get().cloned().context("missing service config")?;
        let service_name = ACTIVE_SERVICE_NAME
            .get()
            .copied()
            .context("missing service name")?;
        let state = c.state_dir.clone().context("missing state dir")?;
        let h = service_control_handler::register(service_name, move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let result = fs::create_dir_all(&state)
                    .and_then(|()| fs::write(state.join("agent.stop"), "stop"));
                match result {
                    Ok(()) => ServiceControlHandlerResult::NoError,
                    Err(error) => {
                        crate::logging::error(format_args!(
                            "failed to write Windows service stop request {}: {error}",
                            state.join("agent.stop").display()
                        ));
                        ServiceControlHandlerResult::Other(service_control_error_code(&error))
                    }
                }
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;
        h.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::StartPending,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 1,
            wait_hint: Duration::from_secs(30),
            process_id: None,
        })?;
        let result = (|| -> Result<()> {
            crate::windows_security::repair_legacy_product_tree()?;
            crate::logging::init(
                &crate::lifecycle::log_file(&c)?,
                c.log_max_bytes,
                c.log_history,
            )?;
            crate::remote_access::initialize_service_devices(&c);
            if let Err(error) = super::repair_windows_global_command(&std::env::current_exe()?) {
                crate::logging::error(format_args!(
                    "failed to repair the global Windows command: {error:#}"
                ));
            }
            h.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: ServiceState::Running,
                controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::ZERO,
                process_id: None,
            })?;
            tokio::runtime::Runtime::new()?.block_on(run_agent(c))
        })();
        if let Err(error) = &result {
            if let Some(config) = CONFIG.get() {
                write_bootstrap_error(
                    config,
                    &anyhow::anyhow!("agent lifecycle failed: {error:#}"),
                );
            }
            crate::logging::error(format_args!("agent lifecycle failed: {error:#}"));
        }
        set_stopped_status(&h, &result);
        result
    }
}
