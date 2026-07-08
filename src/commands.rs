use crate::app::{AppConfig, RestartPolicy};
use crate::cli::Command;
use crate::daemon;
use crate::env_file;
use crate::ipc::{self, DaemonRequest, DaemonResponse};
use crate::paths::AynurPaths;
use anyhow::Context;
use std::collections::BTreeMap;
use std::env;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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
            Command::List | Command::Status => {
                let response = request_running_daemon(paths, DaemonRequest::List)?;
                print_response(response)?;
            }
            Command::Logs { name } => print_logs(paths, &name)?,
            Command::Delete { name } => {
                let response = request_running_daemon(paths, DaemonRequest::Delete { name })?;
                print_response(response)?;
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
        let log_path = paths.root_dir.join("daemon.err.log");
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

fn default_app_name(binary_path: &PathBuf) -> anyhow::Result<String> {
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
    if ipc::send_request(paths, &DaemonRequest::Ping).is_ok() {
        return Ok(());
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
        if ipc::send_request(paths, &DaemonRequest::Ping).is_ok() {
            return Ok(());
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
    ipc::send_request(paths, &request).with_context(|| {
        format!(
            "failed to contact aynur daemon at {}; run `aynur start <binary> --name <name>` first",
            paths.socket_path.display()
        )
    })
}

fn print_response(response: DaemonResponse) -> anyhow::Result<()> {
    match response {
        DaemonResponse::Ok { message } => println!("{message}"),
        DaemonResponse::Error { message } => anyhow::bail!("{message}"),
        DaemonResponse::List { apps } => {
            println!(
                "{:<20} {:<8} {:<10} {:<8} {:<10} binary",
                "name", "pid", "status", "restarts", "uptime"
            );
            for app in apps {
                println!(
                    "{:<20} {:<8} {:<10} {:<8} {:<10} {}",
                    app.name,
                    app.pid
                        .map_or_else(|| "-".to_string(), |pid| pid.to_string()),
                    app.status,
                    app.restarts,
                    app.uptime_seconds
                        .map_or_else(|| "-".to_string(), |uptime| format!("{uptime}s")),
                    app.binary_path.display()
                );
            }
        }
    }
    Ok(())
}

fn print_logs(paths: &AynurPaths, name: &str) -> anyhow::Result<()> {
    let out_path = paths.stdout_log_path(name);
    let err_path = paths.stderr_log_path(name);
    println!("==> {} <==", out_path.display());
    if out_path.exists() {
        print!(
            "{}",
            std::fs::read_to_string(&out_path).with_context(|| {
                format!("failed to read stdout log at {}", out_path.display())
            })?
        );
    }
    println!("==> {} <==", err_path.display());
    if err_path.exists() {
        print!(
            "{}",
            std::fs::read_to_string(&err_path).with_context(|| {
                format!("failed to read stderr log at {}", err_path.display())
            })?
        );
    }
    Ok(())
}
