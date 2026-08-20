use std::{
    ffi::OsString,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::SystemTime,
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, SecondsFormat, Utc};

static LOG_FILE: OnceLock<Mutex<RollingLog>> = OnceLock::new();

pub fn init(path: &Path, max_bytes: u64, history: usize) -> Result<()> {
    let log = RollingLog::open(path, max_bytes, history)?;
    LOG_FILE
        .set(Mutex::new(log))
        .map_err(|_| anyhow!("log file is already initialized"))
}

pub fn info(arguments: fmt::Arguments<'_>) {
    write(false, arguments);
}

pub fn error(arguments: fmt::Arguments<'_>) {
    write(true, arguments);
}

fn write(is_error: bool, arguments: fmt::Arguments<'_>) {
    let timestamp = utc_now();
    if let Some(log) = LOG_FILE.get() {
        match log.lock() {
            Ok(mut log) => {
                if let Err(error) = log.write_line(is_error, arguments) {
                    write_stderr_line(
                        timestamp,
                        format_args!("failed to write agent log: {error:#}"),
                    );
                }
            }
            Err(_) => {
                write_stderr_line(timestamp, format_args!("agent log lock is poisoned"));
            }
        }
        return;
    }

    if is_error {
        write_stderr_line(timestamp, arguments);
    } else {
        let _ = writeln!(
            io::stdout().lock(),
            "{}",
            format_line(timestamp, false, arguments)
        );
    }
}

fn write_stderr_line(timestamp: DateTime<Utc>, arguments: fmt::Arguments<'_>) {
    let _ = writeln!(
        io::stderr().lock(),
        "{}",
        format_line(timestamp, true, arguments)
    );
}

fn format_line(timestamp: DateTime<Utc>, is_error: bool, arguments: fmt::Arguments<'_>) -> String {
    format!(
        "[{}] [{}] {arguments}",
        timestamp.to_rfc3339_opts(SecondsFormat::Millis, true),
        if is_error { "ERROR" } else { "INFO" }
    )
}

fn utc_now() -> DateTime<Utc> {
    SystemTime::now().into()
}

struct RollingLog {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    max_bytes: u64,
    history: usize,
}

impl RollingLog {
    fn open(path: &Path, max_bytes: u64, history: usize) -> Result<Self> {
        if max_bytes == 0 {
            return Err(anyhow!("log max bytes must be greater than zero"));
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create log directory {}", parent.display()))?;
        }
        remove_excess_history(path, history)?;
        let file = open_log_file(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path: path.to_owned(),
            file: Some(file),
            size,
            max_bytes,
            history,
        })
    }

    fn write_line(&mut self, is_error: bool, arguments: fmt::Arguments<'_>) -> Result<()> {
        self.write_line_at(utc_now(), is_error, arguments)
    }

    fn write_line_at(
        &mut self,
        timestamp: DateTime<Utc>,
        is_error: bool,
        arguments: fmt::Arguments<'_>,
    ) -> Result<()> {
        let mut line = format_line(timestamp, is_error, arguments);
        line.push('\n');
        let line_size = u64::try_from(line.len()).unwrap_or(u64::MAX);
        if self.size > 0 && self.size.saturating_add(line_size) > self.max_bytes {
            self.rotate()?;
        }
        let file = self
            .file
            .as_mut()
            .context("log file is unavailable after rotation")?;
        file.write_all(line.as_bytes())?;
        file.flush()?;
        self.size = self.size.saturating_add(line_size);
        Ok(())
    }

    fn rotate(&mut self) -> Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
        }

        let result = self.rotate_files();
        let reopened = open_log_file(&self.path);
        match reopened {
            Ok(file) => {
                self.size = file.metadata()?.len();
                self.file = Some(file);
            }
            Err(error) => return Err(error),
        }
        result
    }

    fn rotate_files(&self) -> Result<()> {
        if self.history == 0 {
            remove_if_exists(&self.path)?;
            return Ok(());
        }

        remove_if_exists(&history_path(&self.path, self.history))?;
        for index in (1..self.history).rev() {
            let source = history_path(&self.path, index);
            if source.exists() {
                let target = history_path(&self.path, index + 1);
                remove_if_exists(&target)?;
                fs::rename(&source, &target).with_context(|| {
                    format!(
                        "failed to rotate log {} to {}",
                        source.display(),
                        target.display()
                    )
                })?;
            }
        }
        if self.path.exists() {
            let target = history_path(&self.path, 1);
            remove_if_exists(&target)?;
            fs::rename(&self.path, &target).with_context(|| {
                format!(
                    "failed to rotate log {} to {}",
                    self.path.display(),
                    target.display()
                )
            })?;
        }
        Ok(())
    }
}

fn open_log_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to open log file {}", path.display()))?;
    #[cfg(unix)]
    set_private_permissions(&file, path)?;
    Ok(file)
}

#[cfg(unix)]
fn set_private_permissions(file: &File, path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure log file {}", path.display()))
}

fn history_path(path: &Path, index: usize) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(format!(".{index}"));
    PathBuf::from(value)
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove old log file {}", path.display()))
        }
    }
}

fn remove_excess_history(path: &Path, history: usize) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let Some(file_name) = path.file_name() else {
        return Ok(());
    };
    let prefix = format!("{}.", file_name.to_string_lossy());
    for entry in fs::read_dir(parent)
        .with_context(|| format!("failed to inspect log directory {}", parent.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let Some(index) = name
            .to_string_lossy()
            .strip_prefix(&prefix)
            .and_then(|value| value.parse::<usize>().ok())
        else {
            continue;
        };
        if index > history {
            remove_if_exists(&entry.path())?;
        } else {
            #[cfg(unix)]
            secure_existing_history(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn secure_existing_history(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Ok(());
    }
    let file = File::open(path)
        .with_context(|| format!("failed to open log history {}", path.display()))?;
    set_private_permissions(&file, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_log_message(path: &Path, expected: &str) {
        let content = fs::read_to_string(path).unwrap();
        assert!(content.starts_with('['), "missing timestamp: {content:?}");
        assert!(
            content.ends_with(&format!("] [INFO] {expected}\n")),
            "{content:?}"
        );
    }

    fn temp_log() -> PathBuf {
        std::env::temp_dir()
            .join(format!("om-agent-log-{}", uuid::Uuid::new_v4()))
            .join("agent.log")
    }

    #[test]
    fn rotates_by_size_and_deletes_excess_history() {
        let path = temp_log();
        let mut log = RollingLog::open(&path, 10, 2).unwrap();

        for value in ["11111", "22222", "33333", "44444"] {
            log.write_line(false, format_args!("{value}")).unwrap();
        }

        assert_log_message(&path, "44444");
        assert_log_message(&history_path(&path, 1), "33333");
        assert_log_message(&history_path(&path, 2), "22222");
        assert!(!history_path(&path, 3).exists());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn zero_history_discards_rotated_content() {
        let path = temp_log();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(history_path(&path, 1), "old history\n").unwrap();
        let mut log = RollingLog::open(&path, 10, 0).unwrap();

        assert!(!history_path(&path, 1).exists());

        log.write_line(false, format_args!("11111")).unwrap();
        log.write_line(false, format_args!("22222")).unwrap();

        assert_log_message(&path, "22222");
        assert!(!history_path(&path, 1).exists());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn formats_log_timestamp_as_utc_with_milliseconds() {
        let timestamp = "2026-08-19T08:30:12.345Z".parse::<DateTime<Utc>>().unwrap();

        assert_eq!(
            format_line(timestamp, false, format_args!("websocket connected")),
            "[2026-08-19T08:30:12.345Z] [INFO] websocket connected"
        );
    }

    #[test]
    fn distinguishes_error_log_entries() {
        let timestamp = "2026-08-19T08:30:12.345Z".parse::<DateTime<Utc>>().unwrap();

        assert_eq!(
            format_line(timestamp, true, format_args!("update failed")),
            "[2026-08-19T08:30:12.345Z] [ERROR] update failed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restricts_existing_log_and_history_to_the_owner() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_log();
        let history = history_path(&path, 1);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "current\n").unwrap();
        fs::write(&history, "history\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&history, fs::Permissions::from_mode(0o644)).unwrap();

        drop(RollingLog::open(&path, 1024, 1).unwrap());

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&history).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
