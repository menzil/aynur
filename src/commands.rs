use crate::app::{AppConfig, RestartPolicy};
use crate::cli::Command;
use crate::daemon;
use crate::env_file;
use crate::ipc::{self, AppStatusView, DAEMON_PROTOCOL_VERSION, DaemonRequest, DaemonResponse};
use crate::paths::AynurPaths;
use crate::process;
use crate::startup;
use anyhow::Context;
use std::collections::BTreeMap;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const LOG_BOUNDARY_BYTES: u64 = 64;

#[derive(Debug, Eq, PartialEq)]
struct LogCursor {
    position: u64,
    boundary: Vec<u8>,
}

pub trait ExecuteCommand {
    fn execute(self, paths: &AynurPaths) -> anyhow::Result<()>;
}

impl ExecuteCommand for Command {
    fn execute(self, paths: &AynurPaths) -> anyhow::Result<()> {
        match self {
            Command::Start {
                binary,
                name,
                cwd,
                env_file,
                args,
            } => {
                ensure_daemon(paths)?;
                let working_directory = match cwd {
                    Some(path) => path,
                    None => {
                        env::current_dir().context("failed to read current working directory")?
                    }
                };
                let app_name = match name {
                    Some(value) => value,
                    None => default_app_name(&binary)?,
                };
                let config = AppConfig {
                    name: app_name,
                    binary_path: absolutize_path(binary)?,
                    args,
                    working_directory: absolutize_path(working_directory)?,
                    env: merged_environment(env_file.as_ref())?,
                    env_file_path: env_file.map(absolutize_path).transpose()?,
                    restart_policy: RestartPolicy {
                        max_restarts: 5,
                        window_seconds: 10,
                    },
                };
                config.validate()?;
                config.save(paths)?;
                let response = ipc::send_request(paths, &DaemonRequest::Start { config })?;
                print_response(response)?;
            }
            Command::Stop { name } => {
                let response = request_running_daemon(paths, DaemonRequest::Stop { name })?;
                print_response(response)?;
            }
            Command::Restart { name } => {
                let response = request_running_daemon(paths, DaemonRequest::Restart { name })?;
                print_response(response)?;
            }
            Command::Reload { name, update_env } => {
                let request = if update_env {
                    DaemonRequest::ReloadUpdateEnv {
                        name,
                        env: BTreeMap::from_iter(env::vars()),
                    }
                } else {
                    DaemonRequest::Reload { name }
                };
                let response = request_running_daemon(paths, request)?;
                print_response(response)?;
            }
            Command::List => {
                let response = request_app_list(paths)?;
                print_response(response)?;
            }
            Command::Save => {
                let response = request_running_daemon(paths, DaemonRequest::Save)?;
                print_response(response)?;
            }
            Command::Logs { name } => follow_logs(paths, &name)?,
            Command::Flush { name } => flush_logs(paths, &name)?,
            Command::Delete { name } => {
                let response = request_running_daemon(paths, DaemonRequest::Delete { name })?;
                print_response(response)?;
            }
            Command::Startup => {
                let message = startup::install(paths)?;
                println!("{message}");
            }
            Command::Unstartup => {
                let message = startup::uninstall(paths)?;
                println!("{message}");
            }
            Command::Version => print_version(),
            Command::Daemon => run_daemon_with_error_log(paths.clone())?,
        }

        Ok(())
    }
}

pub fn print_version() {
    println!("aynur {}", env!("CARGO_PKG_VERSION"));
}

fn run_daemon_with_error_log(paths: AynurPaths) -> anyhow::Result<()> {
    if let Err(error) = daemon::run(paths.clone()) {
        let message = format!("{error:#}\n");
        let log_path = paths.daemon_error_log_path();
        std::fs::write(&log_path, message).with_context(|| {
            format!("failed to write daemon error log at {}", log_path.display())
        })?;
        return Err(error);
    }
    Ok(())
}

fn absolutize_path(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(env::current_dir()
        .context("failed to read current working directory")?
        .join(path))
}

fn default_app_name(binary_path: &Path) -> anyhow::Result<String> {
    let file_name = binary_path.file_name().and_then(|name| name.to_str());
    match file_name {
        Some(name) if !name.trim().is_empty() => Ok(name.to_string()),
        _ => anyhow::bail!(
            "failed to infer app name from binary path {}; pass --name <name>",
            binary_path.display()
        ),
    }
}

fn merged_environment(env_file_path: Option<&PathBuf>) -> anyhow::Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::from_iter(env::vars());
    if let Some(path) = env_file_path {
        let file_values = env_file::read_env_file(path)?;
        values.extend(file_values);
    }
    Ok(values)
}

fn ensure_daemon(paths: &AynurPaths) -> anyhow::Result<()> {
    match ipc::send_request(paths, &DaemonRequest::Ping) {
        Ok(response) => return validate_daemon_ping(response),
        Err(error) if is_inactive_daemon_socket_error(&error) => {}
        Err(error) => return daemon_contact_error(paths, error),
    }

    let current_exe = env::current_exe().context("failed to resolve current executable path")?;
    let mut command = ProcessCommand::new(current_exe);
    command
        .arg("__daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().context("failed to spawn aynur daemon")?;

    wait_for_daemon(paths)
}

fn wait_for_daemon(paths: &AynurPaths) -> anyhow::Result<()> {
    let started_at = Instant::now();
    let timeout = Duration::from_secs(3);
    while started_at.elapsed() < timeout {
        match ipc::send_request(paths, &DaemonRequest::Ping) {
            Ok(response) => return validate_daemon_ping(response),
            Err(error) if is_inactive_daemon_socket_error(&error) => {}
            Err(error) => return daemon_contact_error(paths, error),
        }
        thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!(
        "daemon did not become ready within {} seconds at {}",
        timeout.as_secs(),
        paths.socket_path.display()
    );
}

fn request_running_daemon(
    paths: &AynurPaths,
    request: DaemonRequest,
) -> anyhow::Result<DaemonResponse> {
    require_compatible_daemon(paths)?;
    ipc::send_request(paths, &request).or_else(|error| daemon_contact_error(paths, error))
}

fn request_app_list(paths: &AynurPaths) -> anyhow::Result<DaemonResponse> {
    match ipc::send_request(paths, &DaemonRequest::Ping) {
        Ok(response) => {
            validate_daemon_ping(response)?;
            ipc::send_request(paths, &DaemonRequest::List)
                .or_else(|error| daemon_contact_error(paths, error))
        }
        Err(error) => {
            if is_inactive_daemon_socket_error(&error) {
                return Ok(DaemonResponse::List {
                    apps: configured_app_statuses(paths)?,
                });
            }
            daemon_contact_error(paths, error)
        }
    }
}

fn require_compatible_daemon(paths: &AynurPaths) -> anyhow::Result<()> {
    let response = ipc::send_request(paths, &DaemonRequest::Ping)
        .or_else(|error| daemon_contact_error(paths, error))?;
    validate_daemon_ping(response)
}

fn validate_daemon_ping(response: DaemonResponse) -> anyhow::Result<()> {
    match response {
        DaemonResponse::Pong {
            protocol_version,
            daemon_version,
        } if protocol_version == DAEMON_PROTOCOL_VERSION && !daemon_version.trim().is_empty() => {
            Ok(())
        }
        DaemonResponse::Pong {
            protocol_version,
            daemon_version,
        } if protocol_version == DAEMON_PROTOCOL_VERSION => anyhow::bail!(
            "aynur daemon returned an invalid ping response: daemonVersion is empty for protocol {}; received daemonVersion '{}'",
            protocol_version,
            daemon_version
        ),
        DaemonResponse::Pong {
            protocol_version,
            daemon_version,
        } => anyhow::bail!(
            "aynur daemon protocol is incompatible: CLI {} requires protocol {}, but daemon {} uses protocol {}; restart the daemon after upgrading aynur",
            env!("CARGO_PKG_VERSION"),
            DAEMON_PROTOCOL_VERSION,
            daemon_version,
            protocol_version
        ),
        DaemonResponse::Ok { message } if message == "pong" => anyhow::bail!(
            "aynur daemon protocol is incompatible: CLI {} requires protocol {}, but the running daemon uses the legacy protocol; restart the daemon after upgrading aynur",
            env!("CARGO_PKG_VERSION"),
            DAEMON_PROTOCOL_VERSION
        ),
        response => anyhow::bail!("aynur daemon returned an invalid ping response: {response:?}"),
    }
}

fn daemon_contact_error<T>(paths: &AynurPaths, error: anyhow::Error) -> anyhow::Result<T> {
    Err(error).with_context(|| {
        format!(
            "failed to contact aynur daemon at {}; run `aynur start <binary> --name <name>` first",
            paths.socket_path.display()
        )
    })
}

fn is_inactive_daemon_socket_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|io_error| {
            matches!(
                io_error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
        })
}

fn configured_app_statuses(paths: &AynurPaths) -> anyhow::Result<Vec<AppStatusView>> {
    let entries = std::fs::read_dir(&paths.apps_dir).with_context(|| {
        format!(
            "failed to read configured apps directory at {}",
            paths.apps_dir.display()
        )
    })?;
    let mut apps = Vec::new();
    for entry_result in entries {
        let entry = entry_result.with_context(|| {
            format!(
                "failed to read configured app entry in {}",
                paths.apps_dir.display()
            )
        })?;
        let file_type = entry.file_type().with_context(|| {
            format!(
                "failed to read configured app file type at {}",
                entry.path().display()
            )
        })?;
        if !file_type.is_file() || !is_app_config_file(&entry.path()) {
            continue;
        }

        let path = entry.path();
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read app config at {}", path.display()))?;
        let config = serde_json::from_str::<AppConfig>(&content)
            .with_context(|| format!("failed to parse app config at {}", path.display()))?;
        apps.push(AppStatusView {
            name: config.name,
            pid: None,
            status: "stopped".to_string(),
            restarts: 0,
            uptime_seconds: None,
            binary_path: config.binary_path,
        });
    }
    apps.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(apps)
}

fn is_app_config_file(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("json")
}

fn print_response(response: DaemonResponse) -> anyhow::Result<()> {
    match response {
        DaemonResponse::Pong {
            protocol_version,
            daemon_version,
        } => anyhow::bail!(
            "received an unexpected ping response from daemon {} using protocol {}",
            daemon_version,
            protocol_version
        ),
        DaemonResponse::Ok { message } => println!("{message}"),
        DaemonResponse::Error { message } => anyhow::bail!("{message}"),
        DaemonResponse::List { apps } => {
            println!(
                "{:<20} {:<8} {:<10} {:<8} {:<10} {:<12} binary",
                "name", "pid", "status", "restarts", "uptime", "memory"
            );
            for app in apps {
                let memory = app.pid.map_or_else(
                    || "-".to_string(),
                    |pid| {
                        process::read_process_rss_bytes(pid)
                            .map_or_else(|_| "unavailable".to_string(), format_memory)
                    },
                );
                println!(
                    "{:<20} {:<8} {:<10} {:<8} {:<10} {:<12} {}",
                    app.name,
                    app.pid
                        .map_or_else(|| "-".to_string(), |pid| pid.to_string()),
                    app.status,
                    app.restarts,
                    app.uptime_seconds
                        .map_or_else(|| "-".to_string(), |uptime| format!("{uptime}s")),
                    memory,
                    app.binary_path.display()
                );
            }
        }
    }
    Ok(())
}

fn format_memory(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;

    if bytes >= GIB {
        format_memory_value(bytes, GIB, "GiB")
    } else if bytes >= MIB {
        format_memory_value(bytes, MIB, "MiB")
    } else {
        format_memory_value(bytes, KIB, "KiB")
    }
}

fn format_memory_value(bytes: u64, unit: u64, suffix: &str) -> String {
    let value = bytes as f64 / unit as f64;
    if value >= 10.0 || value.fract() == 0.0 {
        format!("{value:.0} {suffix}")
    } else {
        format!("{value:.1} {suffix}")
    }
}

fn follow_logs(paths: &AynurPaths, name: &str) -> anyhow::Result<()> {
    ensure_configured_app(paths, name)?;
    let out_path = paths.stdout_log_path(name);
    let err_path = paths.stderr_log_path(name);
    let mut out_file = open_log(&out_path, "stdout", name)?;
    let mut err_file = open_log(&err_path, "stderr", name)?;
    let mut out_cursor = LogCursor {
        position: 0,
        boundary: Vec::new(),
    };
    let mut err_cursor = LogCursor {
        position: 0,
        boundary: Vec::new(),
    };
    let stdout = std::io::stdout();
    let mut output = stdout.lock();

    writeln!(output, "==> {} <==", out_path.display())
        .with_context(|| format!("failed to print stdout log header for app '{name}'"))?;
    out_cursor = write_new_log_content(&mut out_file, &out_path, out_cursor, &mut output)?;
    writeln!(output, "==> {} <==", err_path.display())
        .with_context(|| format!("failed to print stderr log header for app '{name}'"))?;
    err_cursor = write_new_log_content(&mut err_file, &err_path, err_cursor, &mut output)?;

    loop {
        out_cursor = write_new_log_content(&mut out_file, &out_path, out_cursor, &mut output)?;
        err_cursor = write_new_log_content(&mut err_file, &err_path, err_cursor, &mut output)?;
        thread::sleep(Duration::from_millis(100));
    }
}

fn flush_logs(paths: &AynurPaths, name: &str) -> anyhow::Result<()> {
    ensure_configured_app(paths, name)?;

    let out_path = paths.stdout_log_path(name);
    let err_path = paths.stderr_log_path(name);
    let out_file = open_log_for_truncate(&out_path, "stdout", name)?;
    let err_file = open_log_for_truncate(&err_path, "stderr", name)?;
    truncate_log(out_file, &out_path, "stdout", name)?;
    truncate_log(err_file, &err_path, "stderr", name)?;
    println!("flushed logs for {name}");
    Ok(())
}

fn ensure_configured_app(paths: &AynurPaths, name: &str) -> anyhow::Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("app name is empty");
    }
    if name.contains('/') {
        anyhow::bail!("app name '{name}' must not contain '/'");
    }
    let config_path = paths.app_config_path(name);
    if !config_path.is_file() {
        anyhow::bail!(
            "app '{name}' is not configured at {}",
            config_path.display()
        );
    }
    Ok(())
}

fn open_log(path: &Path, stream: &str, name: &str) -> anyhow::Result<File> {
    File::open(path).with_context(|| {
        format!(
            "failed to open {stream} log for app '{name}' at {}",
            path.display()
        )
    })
}

fn write_new_log_content(
    file: &mut File,
    path: &Path,
    cursor: LogCursor,
    output: &mut impl Write,
) -> anyhow::Result<LogCursor> {
    let length = file
        .metadata()
        .with_context(|| format!("failed to read log metadata at {}", path.display()))?
        .len();
    let boundary_matches = log_boundary_matches(file, path, &cursor, length)?;
    let read_position = if length < cursor.position || !boundary_matches {
        0
    } else {
        cursor.position
    };
    file.seek(SeekFrom::Start(read_position))
        .with_context(|| format!("failed to seek log at {}", path.display()))?;
    let bytes_written = std::io::copy(file, output)
        .with_context(|| format!("failed to print log at {}", path.display()))?;
    output
        .flush()
        .with_context(|| format!("failed to flush log output for {}", path.display()))?;
    let position = read_position + bytes_written;
    let boundary = read_log_boundary(file, path, position)?;
    Ok(LogCursor { position, boundary })
}

fn log_boundary_matches(
    file: &mut File,
    path: &Path,
    cursor: &LogCursor,
    length: u64,
) -> anyhow::Result<bool> {
    if cursor.boundary.is_empty() || length < cursor.position {
        return Ok(true);
    }
    let boundary_start = cursor.position - cursor.boundary.len() as u64;
    file.seek(SeekFrom::Start(boundary_start))
        .with_context(|| format!("failed to seek log boundary at {}", path.display()))?;
    let mut current_boundary = vec![0; cursor.boundary.len()];
    file.read_exact(&mut current_boundary)
        .with_context(|| format!("failed to read log boundary at {}", path.display()))?;
    Ok(current_boundary == cursor.boundary)
}

fn read_log_boundary(file: &mut File, path: &Path, position: u64) -> anyhow::Result<Vec<u8>> {
    let boundary_length = position.min(LOG_BOUNDARY_BYTES);
    let boundary_start = position - boundary_length;
    file.seek(SeekFrom::Start(boundary_start))
        .with_context(|| format!("failed to seek log boundary at {}", path.display()))?;
    let mut boundary = vec![0; boundary_length as usize];
    file.read_exact(&mut boundary)
        .with_context(|| format!("failed to read log boundary at {}", path.display()))?;
    Ok(boundary)
}

fn open_log_for_truncate(path: &Path, stream: &str, name: &str) -> anyhow::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| {
            format!(
                "failed to open {stream} log for app '{name}' for truncation at {}",
                path.display()
            )
        })?;
    let metadata = file.metadata().with_context(|| {
        format!(
            "failed to read {stream} log metadata for app '{name}' at {}",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        anyhow::bail!(
            "{stream} log for app '{name}' is not a regular file: {}",
            path.display()
        );
    }
    Ok(file)
}

fn truncate_log(file: File, path: &Path, stream: &str, name: &str) -> anyhow::Result<()> {
    file.set_len(0).with_context(|| {
        format!(
            "failed to truncate {stream} log for app '{name}' at {}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        LogCursor, flush_logs, format_memory, request_app_list, request_running_daemon,
        validate_daemon_ping, write_new_log_content,
    };
    use crate::app::{AppConfig, RestartPolicy};
    use crate::ipc::{AppStatusView, DAEMON_PROTOCOL_VERSION, DaemonRequest, DaemonResponse};
    use crate::paths::AynurPaths;
    use std::collections::BTreeMap;
    use std::fs::{File, OpenOptions};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn test_paths(root_dir: std::path::PathBuf) -> AynurPaths {
        AynurPaths {
            apps_dir: root_dir.join("apps"),
            logs_dir: root_dir.join("logs"),
            socket_path: root_dir.join("daemon.sock"),
            pid_path: root_dir.join("daemon.pid"),
            root_dir,
        }
    }

    fn write_app_config(paths: &AynurPaths, name: &str) {
        let config = AppConfig {
            name: name.to_string(),
            binary_path: paths.root_dir.join(format!("{name}-bin")),
            args: Vec::new(),
            working_directory: paths.root_dir.clone(),
            env: BTreeMap::new(),
            env_file_path: None,
            restart_policy: RestartPolicy {
                max_restarts: 5,
                window_seconds: 10,
            },
        };
        let content = serde_json::to_string(&config).expect("serialize app config");
        std::fs::write(paths.app_config_path(name), content).expect("app config");
    }

    fn assert_stopped_app(app: AppStatusView, name: &str, binary_path: std::path::PathBuf) {
        assert_eq!(app.name, name);
        assert_eq!(app.pid, None);
        assert_eq!(app.status, "stopped");
        assert_eq!(app.restarts, 0);
        assert_eq!(app.uptime_seconds, None);
        assert_eq!(app.binary_path, binary_path);
    }

    #[test]
    fn accepts_matching_daemon_protocol() {
        validate_daemon_ping(DaemonResponse::Pong {
            protocol_version: DAEMON_PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        })
        .expect("matching daemon protocol");
    }

    #[test]
    fn rejects_legacy_daemon_before_sending_business_requests() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        let listener = UnixListener::bind(&paths.socket_path).expect("legacy daemon socket");
        let server = thread::spawn(move || {
            let (mut stream, _address) = listener.accept().expect("accept ping");
            let mut request_line = String::new();
            BufReader::new(&stream)
                .read_line(&mut request_line)
                .expect("read ping request");
            let request =
                serde_json::from_str::<DaemonRequest>(&request_line).expect("parse ping request");
            assert!(matches!(request, DaemonRequest::Ping));
            let response = serde_json::to_string(&DaemonResponse::Ok {
                message: "pong".to_string(),
            })
            .expect("serialize legacy ping response");
            writeln!(stream, "{response}").expect("write legacy ping response");
        });

        let error = request_running_daemon(&paths, DaemonRequest::Save)
            .expect_err("legacy daemon must be rejected");
        server.join().expect("legacy daemon thread");

        assert!(error.to_string().contains("legacy protocol"));
        assert!(error.to_string().contains("restart the daemon"));
    }

    #[test]
    fn rejects_mismatched_daemon_protocol() {
        let error = validate_daemon_ping(DaemonResponse::Pong {
            protocol_version: DAEMON_PROTOCOL_VERSION + 1,
            daemon_version: "future".to_string(),
        })
        .expect_err("mismatched daemon protocol must be rejected");

        assert!(error.to_string().contains("daemon future"));
        assert!(error.to_string().contains("protocol 2"));
    }

    #[test]
    fn flushes_only_the_named_apps_logs_and_keeps_open_writers_valid() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        std::fs::write(paths.app_config_path("api"), "{}").expect("app config");
        std::fs::write(paths.stdout_log_path("api"), "old stdout\n").expect("stdout log");
        std::fs::write(paths.stderr_log_path("api"), "old stderr\n").expect("stderr log");
        std::fs::write(paths.stdout_log_path("worker"), "keep\n").expect("other log");
        let mut open_writer = OpenOptions::new()
            .append(true)
            .open(paths.stdout_log_path("api"))
            .expect("open stdout writer");

        flush_logs(&paths, "api").expect("flush logs");
        writeln!(open_writer, "new stdout").expect("write after flush");

        assert_eq!(
            std::fs::read_to_string(paths.stdout_log_path("api")).expect("read stdout"),
            "new stdout\n"
        );
        assert_eq!(
            std::fs::read_to_string(paths.stderr_log_path("api")).expect("read stderr"),
            ""
        );
        assert_eq!(
            std::fs::read_to_string(paths.stdout_log_path("worker")).expect("read other log"),
            "keep\n"
        );
    }

    #[test]
    fn rejects_an_app_that_is_not_configured() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");

        let error = flush_logs(&paths, "missing").expect_err("missing app must fail");

        assert!(
            error
                .to_string()
                .contains("app 'missing' is not configured")
        );
    }

    #[test]
    fn rejects_a_configured_app_with_missing_logs() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        std::fs::write(paths.app_config_path("api"), "{}").expect("app config");

        let error = flush_logs(&paths, "api").expect_err("missing logs must fail");

        assert!(error.to_string().contains("failed to open stdout log"));
    }

    #[test]
    fn lists_no_apps_when_daemon_socket_is_missing_and_no_apps_are_configured() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");

        let response = request_app_list(&paths).expect("list without configured apps");

        match response {
            DaemonResponse::List { apps } => assert!(apps.is_empty()),
            response => panic!("expected list response, got {response:?}"),
        }
    }

    #[test]
    fn lists_no_apps_when_daemon_socket_is_stale_and_no_apps_are_configured() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        let listener = UnixListener::bind(&paths.socket_path).expect("stale daemon socket");
        drop(listener);

        let response = request_app_list(&paths).expect("list with stale socket");

        match response {
            DaemonResponse::List { apps } => assert!(apps.is_empty()),
            response => panic!("expected list response, got {response:?}"),
        }
    }

    #[test]
    fn lists_configured_apps_as_stopped_when_daemon_socket_is_missing() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        write_app_config(&paths, "api");

        let response = request_app_list(&paths).expect("list configured app without daemon");

        match response {
            DaemonResponse::List { apps } => {
                let [app] = apps.try_into().expect("one configured app");
                assert_stopped_app(app, "api", paths.root_dir.join("api-bin"));
            }
            response => panic!("expected list response, got {response:?}"),
        }
    }

    #[test]
    fn lists_configured_apps_as_stopped_when_daemon_socket_is_stale() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        write_app_config(&paths, "api");
        let listener = UnixListener::bind(&paths.socket_path).expect("stale daemon socket");
        drop(listener);

        let response = request_app_list(&paths).expect("list configured app with stale socket");

        match response {
            DaemonResponse::List { apps } => {
                let [app] = apps.try_into().expect("one configured app");
                assert_stopped_app(app, "api", paths.root_dir.join("api-bin"));
            }
            response => panic!("expected list response, got {response:?}"),
        }
    }

    #[test]
    fn follows_new_content_after_a_log_is_truncated() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let log_path = temp_dir.path().join("api.out.log");
        std::fs::write(&log_path, "old stdout\n").expect("initial log");
        let mut log_file = File::open(&log_path).expect("open log");
        let mut output = Vec::new();
        let cursor = write_new_log_content(
            &mut log_file,
            &log_path,
            LogCursor {
                position: 0,
                boundary: Vec::new(),
            },
            &mut output,
        )
        .expect("read initial log");
        std::fs::write(&log_path, "new\n").expect("truncate log");

        let new_cursor = write_new_log_content(&mut log_file, &log_path, cursor, &mut output)
            .expect("read truncated log");

        assert_eq!(output, b"old stdout\nnew\n");
        assert_eq!(new_cursor.position, 4);
    }

    #[test]
    fn follows_a_truncated_log_that_regrows_past_the_previous_position() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let log_path = temp_dir.path().join("api.out.log");
        std::fs::write(&log_path, "old\n").expect("initial log");
        let mut log_file = File::open(&log_path).expect("open log");
        let mut output = Vec::new();
        let cursor = write_new_log_content(
            &mut log_file,
            &log_path,
            LogCursor {
                position: 0,
                boundary: Vec::new(),
            },
            &mut output,
        )
        .expect("read initial log");
        std::fs::write(&log_path, "new content longer than old\n").expect("replace log");

        let new_cursor = write_new_log_content(&mut log_file, &log_path, cursor, &mut output)
            .expect("read regrown log");

        assert_eq!(output, b"old\nnew content longer than old\n");
        assert_eq!(new_cursor.position, 28);
    }

    #[test]
    fn rejects_a_symbolic_link_log_target() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        std::fs::write(paths.app_config_path("api"), "{}").expect("app config");
        let external_path = temp_dir.path().join("external.log");
        std::fs::write(&external_path, "keep\n").expect("external log");
        std::os::unix::fs::symlink(&external_path, paths.stdout_log_path("api"))
            .expect("stdout symlink");
        std::fs::write(paths.stderr_log_path("api"), "stderr\n").expect("stderr log");

        let error = flush_logs(&paths, "api").expect_err("symlink must fail");

        assert!(error.to_string().contains("failed to open stdout log"));
        assert_eq!(
            std::fs::read_to_string(external_path).expect("external log content"),
            "keep\n"
        );
    }

    #[test]
    fn formats_memory_using_binary_units() {
        assert_eq!(format_memory(1024), "1 KiB");
        assert_eq!(format_memory(1536), "1.5 KiB");
        assert_eq!(format_memory(10 * 1024 + 512), "10 KiB");
        assert_eq!(format_memory(1024 * 1024), "1 MiB");
        assert_eq!(format_memory(1024 * 1024 * 1024), "1 GiB");
    }
}
