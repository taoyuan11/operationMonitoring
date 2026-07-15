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
    windows_path(&install, true)?;
    run("sc.exe", &["start", WINDOWS_SERVICE_NAME])?;
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
    let current_executable = env::current_exe()?;
    let running_install = if current_executable.starts_with(&install) {
        Some(&install)
    } else if current_executable.starts_with(&legacy_install) {
        Some(&legacy_install)
    } else {
        None
    };
    if let Some(running_install) = running_install {
        let other_install = if running_install == &install {
            &legacy_install
        } else {
            &install
        };
        let _ = fs::remove_dir_all(other_install);
        let command = format!(
            "ping 127.0.0.1 -n 3 >nul & rmdir /S /Q \"{}\"",
            running_install.display()
        );
        Command::new("cmd.exe").args(["/C", &command]).spawn()?;
    } else {
        let _ = fs::remove_dir_all(&install);
        let _ = fs::remove_dir_all(&legacy_install);
    }
    Ok(())
}
#[cfg(windows)]
fn windows_path(path: &Path, add: bool) -> Result<()> {
    let key = r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
    let out = Command::new("reg.exe")
        .args(["query", key, "/v", "Path"])
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let current = text
        .lines()
        .find_map(|l| {
            l.split_once("REG_EXPAND_SZ")
                .or_else(|| l.split_once("REG_SZ"))
                .map(|x| x.1.trim())
        })
        .unwrap_or("");
    let target = path.to_string_lossy();
    let mut parts: Vec<&str> = current
        .split(';')
        .filter(|x| !x.is_empty() && !x.eq_ignore_ascii_case(&target))
        .collect();
    if add {
        parts.push(&target)
    }
    let value = parts.join(";");
    success(
        Command::new("reg.exe")
            .args([
                "add",
                key,
                "/v",
                "Path",
                "/t",
                "REG_EXPAND_SZ",
                "/d",
                &value,
                "/f",
            ])
            .status()?,
        "reg add",
    )
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
    use super::migrate_path;
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
