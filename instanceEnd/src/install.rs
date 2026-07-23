use crate::{
    config::AgentConfig,
    lifecycle::{run_agent, stop_if_running},
};
use anyhow::{Context, Result, bail};
use reqwest::Url;
use std::{
    env, fs,
    io::{self, IsTerminal, Write},
    path::Path,
    process::{Command, ExitStatus},
};

#[cfg(windows)]
const WINDOWS_SERVICE_NAME: &str = "operation-monitoring-agent";
#[cfg(windows)]
const SHORT_WINDOWS_SERVICE_NAME: &str = "om-agent";
#[cfg(target_os = "macos")]
const MACOS_SERVICE_LABEL: &str = "com.operation-monitoring.agent";

pub fn install(mut config: AgentConfig, non_interactive: bool, yes: bool) -> Result<()> {
    let explicit_server = env::args_os()
        .any(|value| value == "--server" || value.to_string_lossy().starts_with("--server="))
        || env::var_os("OM_SERVER").is_some();
    if non_interactive && !explicit_server {
        bail!("--server is required with --non-interactive");
    }
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
    validate_server(&config.server)?;
    if !is_elevated() {
        return elevate("install", Some(&config));
    }
    install_elevated(&config)
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

pub fn run_service(config: AgentConfig) -> Result<()> {
    #[cfg(windows)]
    {
        return windows_service_impl::run(config);
    }
    #[cfg(not(windows))]
    {
        tokio::runtime::Runtime::new()?.block_on(run_agent(config))
    }
}

fn validate_server(value: &str) -> Result<()> {
    let url = Url::parse(value).context("invalid server URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("server URL must be an absolute HTTP or HTTPS URL");
    }
    Ok(())
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
        match validate_server(selected) {
            Ok(()) => return Ok(selected.to_owned()),
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
        return install_windows(config);
    }
    #[cfg(target_os = "macos")]
    {
        return install_macos(config);
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
        return uninstall_windows();
    }
    #[cfg(target_os = "macos")]
    {
        return uninstall_macos();
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
        return Ok(());
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
    let output = Command::new("launchctl")
        .args(["bootout", &target])
        .output()
        .context("failed to run launchctl")?;
    if output.status.success() || macos_service_not_loaded(output.status.code()) {
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
fn quoted(value: &str) -> String {
    value.replace('\'', "'\\''")
}
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
        "OM_SERVER='{}'\nOM_REPORT_INTERVAL='{}'\nOM_AGENT_ID_FILE='{id}'\nOM_AGENT_STATE_DIR='{state}'\nOM_AGENT_LOG_FILE='{log}'\nOM_AGENT_LOG_MAX_BYTES='{}'\nOM_AGENT_LOG_HISTORY='{}'\nOM_AGENT_UPDATE_DIR='{update}'\n",
        quoted(&c.server),
        c.report_interval,
        c.log_max_bytes,
        c.log_history
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
    let install =
        std::path::PathBuf::from(env::var_os("ProgramFiles").context("ProgramFiles missing")?)
            .join("OM Agent");
    let legacy_install =
        std::path::PathBuf::from(env::var_os("ProgramFiles").context("ProgramFiles missing")?)
            .join("Operation Monitoring Agent");
    let data = std::path::PathBuf::from(env::var_os("ProgramData").context("ProgramData missing")?)
        .join("OperationMonitoring");
    let binary = install.join("om-agent.exe");
    stop_and_delete_windows_service(WINDOWS_SERVICE_NAME)?;
    stop_and_delete_windows_service(SHORT_WINDOWS_SERVICE_NAME)?;
    copy_self(&binary)?;
    fs::create_dir_all(&data)?;
    private_file(
        data.join("install.json"),
        &serde_json::to_string_pretty(
            &serde_json::json!({"server":c.server,"report_interval":c.report_interval}),
        )?,
    )?;
    fs::write(data.join("install-type"), "standalone\n")?;
    let image = format!(
        "\"{}\" service-run --server \"{}\" --report-interval {} --identity-file \"{}\" --state-dir \"{}\" --log-file \"{}\" --log-max-bytes {} --log-history {} --update-dir \"{}\"",
        binary.display(),
        c.server,
        c.report_interval,
        data.join("identity.json").display(),
        data.join("runtime").display(),
        data.join("logs/agent.log").display(),
        c.log_max_bytes,
        c.log_history,
        data.join("updates").display()
    );
    success(
        Command::new("sc.exe")
            .args([
                "create",
                WINDOWS_SERVICE_NAME,
                "start=",
                "auto",
                "DisplayName=",
                "OM Agent",
                "binPath=",
                &image,
            ])
            .status()?,
        "sc create",
    )?;
    windows_path(&legacy_install, false)?;
    repair_windows_global_command(&binary)?;
    run("sc.exe", &["start", WINDOWS_SERVICE_NAME])?;
    println!(
        "agent installed and started; open a new terminal to use om-agent globally ({})",
        binary.display()
    );
    if env::current_exe()?.starts_with(&legacy_install) {
        let command = format!(
            "ping 127.0.0.1 -n 3 >nul & rmdir /S /Q \"{}\"",
            legacy_install.display()
        );
        Command::new("cmd.exe").args(["/C", &command]).spawn()?;
    } else {
        let _ = fs::remove_dir_all(legacy_install);
    }
    Ok(())
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
    let system_root =
        std::path::PathBuf::from(env::var_os("SystemRoot").context("SystemRoot missing")?);
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
    let install =
        std::path::PathBuf::from(env::var_os("ProgramFiles").context("ProgramFiles missing")?)
            .join("OM Agent");
    let legacy_install =
        std::path::PathBuf::from(env::var_os("ProgramFiles").context("ProgramFiles missing")?)
            .join("Operation Monitoring Agent");
    let data = std::path::PathBuf::from(env::var_os("ProgramData").context("ProgramData missing")?)
        .join("OperationMonitoring");
    stop_and_delete_windows_service(WINDOWS_SERVICE_NAME)?;
    stop_and_delete_windows_service(SHORT_WINDOWS_SERVICE_NAME)?;
    windows_path(&install, false)?;
    windows_path(&legacy_install, false)?;
    let _ = fs::remove_dir_all(data);
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
            0,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            &mut raw_key,
        )
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
            0,
            value_type,
            Some(bytes),
        )
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
#[cfg(windows)]
fn windows_service_state(service_name: &str) -> Result<Option<u32>> {
    let output = Command::new("sc.exe")
        .args(["query", service_name])
        .output()
        .context("failed to query Windows service")?;
    if !output.status.success() {
        let message = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if message.contains("1060") {
            return Ok(None);
        }
        bail!("sc query exited with {}: {}", output.status, message.trim());
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((_, fields)) = line.split_once(':') else {
            continue;
        };
        if let Some(state) = fields
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| (1..=7).contains(value))
        {
            return Ok(Some(state));
        }
    }
    bail!("sc query did not report a service state")
}

#[cfg(windows)]
fn stop_and_delete_windows_service(service_name: &str) -> Result<()> {
    use std::{
        thread,
        time::{Duration, Instant},
    };

    let Some(state) = windows_service_state(service_name)? else {
        return Ok(());
    };
    if state != 1 {
        let _ = run("sc.exe", &["stop", service_name]);
        let started = Instant::now();
        loop {
            match windows_service_state(service_name)? {
                None | Some(1) => break,
                Some(_) if started.elapsed() < Duration::from_secs(30) => {
                    thread::sleep(Duration::from_millis(250));
                }
                Some(_) => bail!("Windows service did not stop within 30 seconds"),
            }
        }
    }
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
    let args = if let Some(c) = c {
        format!(
            "{action} --yes --non-interactive --server \"{}\" --report-interval {}",
            c.server, c.report_interval
        )
    } else {
        format!("{action} --yes")
    };
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
const MACOS: &str = r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict><key>Label</key><string>com.operation-monitoring.agent</string><key>ProgramArguments</key><array><string>/bin/sh</string><string>-c</string><string>set -a; . '/Library/Application Support/OperationMonitoring/agent.env'; exec /usr/local/bin/om-agent service-run</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>StandardOutPath</key><string>/Library/Logs/OperationMonitoring/agent.log</string><key>StandardErrorPath</key><string>/Library/Logs/OperationMonitoring/agent.log</string></dict></plist>"#;

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::install_windows_command_entry;
    use super::{migrate_path, update_windows_path, windows_command_paths_from_root};
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
    use super::{MACOS, macos_service_not_loaded};

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
}

#[cfg(windows)]
mod windows_service_impl {
    use super::WINDOWS_SERVICE_NAME;
    use crate::{config::AgentConfig, lifecycle::run_agent};
    use anyhow::{Context, Result};
    use std::{ffi::OsString, fs, sync::OnceLock, time::Duration};
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
        let service_name = WINDOWS_SERVICE_NAME;
        ACTIVE_SERVICE_NAME
            .set(service_name)
            .map_err(|_| anyhow::anyhow!("service name already initialized"))?;
        service_dispatcher::start(service_name, ffi_main)?;
        Ok(())
    }
    fn service_main(_: Vec<OsString>) {
        if let Err(e) = inner() {
            crate::logging::error(format_args!("service failed: {e:#}"))
        }
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
                let _ = fs::create_dir_all(&state);
                let _ = fs::write(state.join("agent.stop"), "stop");
                ServiceControlHandlerResult::NoError
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
        let result = tokio::runtime::Runtime::new()?.block_on(run_agent(c));
        h.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(if result.is_ok() { 0 } else { 1 }),
            checkpoint: 0,
            wait_hint: Duration::ZERO,
            process_id: None,
        })?;
        result
    }
}
