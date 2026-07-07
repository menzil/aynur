use crate::app::AppConfig;
use crate::env_file;
use crate::ipc::{AppStatusView, DaemonRequest, DaemonResponse};
use crate::paths::AynurPaths;
use crate::process::{self, ExitEvent};
use anyhow::Context;
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
struct RuntimeApp {
    config: AppConfig,
    pid: Option<u32>,
    status: AppStatus,
    restarts: u32,
    started_at: Option<SystemTime>,
    restart_times: Vec<SystemTime>,
}

#[derive(Clone, Debug)]
enum AppStatus {
    Stopped,
    Starting,
    Online,
    Stopping,
    Reloading,
    Errored,
}

impl AppStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Online => "online",
            Self::Stopping => "stopping",
            Self::Reloading => "reloading",
            Self::Errored => "errored",
        }
    }
}

struct DaemonState {
    paths: AynurPaths,
    apps: HashMap<String, RuntimeApp>,
    event_sender: Sender<ExitEvent>,
}

pub fn run(paths: AynurPaths) -> anyhow::Result<()> {
    paths.ensure()?;
    write_pid(&paths)?;
    remove_stale_socket(&paths)?;

    let listener = UnixListener::bind(&paths.socket_path).with_context(|| {
        format!(
            "failed to bind daemon socket at {}",
            paths.socket_path.display()
        )
    })?;
    listener
        .set_nonblocking(true)
        .context("failed to set daemon socket to non-blocking")?;

    let (event_sender, event_receiver) = mpsc::channel();
    let mut state = DaemonState {
        paths,
        apps: HashMap::new(),
        event_sender,
    };

    loop {
        accept_pending_connections(&listener, &mut state)?;
        handle_exit_events(&mut state, &event_receiver)?;
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn write_pid(paths: &AynurPaths) -> anyhow::Result<()> {
    std::fs::write(&paths.pid_path, std::process::id().to_string())
        .with_context(|| format!("failed to write daemon pid at {}", paths.pid_path.display()))
}

fn remove_stale_socket(paths: &AynurPaths) -> anyhow::Result<()> {
    if paths.socket_path.exists() {
        std::fs::remove_file(&paths.socket_path).with_context(|| {
            format!(
                "failed to remove stale daemon socket at {}",
                paths.socket_path.display()
            )
        })?;
    }
    Ok(())
}

fn accept_pending_connections(
    listener: &UnixListener,
    state: &mut DaemonState,
) -> anyhow::Result<()> {
    loop {
        match listener.accept() {
            Ok((stream, _address)) => handle_connection(stream, state)?,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error).context("failed to accept daemon connection"),
        }
    }
}

fn handle_connection(mut stream: UnixStream, state: &mut DaemonState) -> anyhow::Result<()> {
    let mut request_line = String::new();
    {
        let mut reader = BufReader::new(&stream);
        reader
            .read_line(&mut request_line)
            .context("failed to read daemon request")?;
    }
    let request = serde_json::from_str::<DaemonRequest>(&request_line)
        .context("failed to parse daemon request JSON")?;
    let response = handle_request(request, state);
    let response_line =
        serde_json::to_string(&response).context("failed to serialize daemon response")?;
    stream
        .write_all(response_line.as_bytes())
        .context("failed to write daemon response")?;
    stream
        .write_all(b"\n")
        .context("failed to write daemon response newline")?;
    Ok(())
}

fn handle_request(request: DaemonRequest, state: &mut DaemonState) -> DaemonResponse {
    match request {
        DaemonRequest::Ping => ok("pong"),
        DaemonRequest::Start { config } => response_from_result(start_app(state, config)),
        DaemonRequest::Stop { name } => response_from_result(stop_app(state, &name)),
        DaemonRequest::Restart { name } => response_from_result(restart_app(state, &name)),
        DaemonRequest::Reload { name } => response_from_result(reload_app(state, &name)),
        DaemonRequest::ReloadUpdateEnv { name, env } => {
            response_from_result(reload_update_env(state, &name, env))
        }
        DaemonRequest::List => DaemonResponse::List {
            apps: list_apps(state),
        },
        DaemonRequest::Delete { name } => response_from_result(delete_app(state, &name)),
    }
}

fn response_from_result(result: anyhow::Result<String>) -> DaemonResponse {
    match result {
        Ok(message) => ok(&message),
        Err(error) => DaemonResponse::Error {
            message: format!("{error:#}"),
        },
    }
}

fn ok(message: &str) -> DaemonResponse {
    DaemonResponse::Ok {
        message: message.to_string(),
    }
}

fn start_app(state: &mut DaemonState, config: AppConfig) -> anyhow::Result<String> {
    config.validate()?;
    if let Some(existing) = state.apps.get(&config.name) {
        if existing.pid.is_some() {
            anyhow::bail!("app '{}' is already running", config.name);
        }
    }

    let name = config.name.clone();
    let mut runtime = RuntimeApp {
        config,
        pid: None,
        status: AppStatus::Starting,
        restarts: 0,
        started_at: None,
        restart_times: Vec::new(),
    };
    spawn_runtime_app(state, &mut runtime)?;
    let pid = runtime.pid.context("spawned app did not record pid")?;
    state.apps.insert(name.clone(), runtime);
    Ok(format!("started {name} with pid {pid}"))
}

fn stop_app(state: &mut DaemonState, name: &str) -> anyhow::Result<String> {
    let app = state
        .apps
        .get_mut(name)
        .with_context(|| format!("app '{name}' is not managed"))?;
    app.status = AppStatus::Stopping;
    if let Some(pid) = app.pid.take() {
        process::terminate_process_group(pid)?;
        std::thread::sleep(Duration::from_millis(800));
        process::kill_process_group(pid)?;
    }
    app.status = AppStatus::Stopped;
    app.started_at = None;
    Ok(format!("stopped {name}"))
}

fn restart_app(state: &mut DaemonState, name: &str) -> anyhow::Result<String> {
    stop_app(state, name)?;
    let mut app = state
        .apps
        .remove(name)
        .with_context(|| format!("app '{name}' is not managed"))?;
    app.status = AppStatus::Starting;
    spawn_runtime_app(state, &mut app)?;
    let pid = app.pid.context("restarted app did not record pid")?;
    state.apps.insert(name.to_string(), app);
    Ok(format!("restarted {name} with pid {pid}"))
}

fn reload_app(state: &mut DaemonState, name: &str) -> anyhow::Result<String> {
    {
        let app = state
            .apps
            .get_mut(name)
            .with_context(|| format!("app '{name}' is not managed"))?;
        app.status = AppStatus::Reloading;
    }
    restart_app(state, name).map(|message| message.replacen("restarted", "reloaded", 1))
}

fn reload_update_env(
    state: &mut DaemonState,
    name: &str,
    mut env: BTreeMap<String, String>,
) -> anyhow::Result<String> {
    let env_file_path = state
        .apps
        .get(name)
        .with_context(|| format!("app '{name}' is not managed"))?
        .config
        .env_file_path
        .clone();
    if let Some(path) = env_file_path {
        env.extend(env_file::read_env_file(&path)?);
    }
    {
        let app = state
            .apps
            .get_mut(name)
            .with_context(|| format!("app '{name}' is not managed"))?;
        app.status = AppStatus::Reloading;
        app.config.env = env;
        app.config.save(&state.paths)?;
    }
    restart_app(state, name)
}

fn delete_app(state: &mut DaemonState, name: &str) -> anyhow::Result<String> {
    if state.apps.contains_key(name) {
        stop_app(state, name)?;
        state.apps.remove(name);
    }
    AppConfig::delete(&state.paths, name)?;
    Ok(format!("deleted {name}"))
}

fn spawn_runtime_app(state: &DaemonState, app: &mut RuntimeApp) -> anyhow::Result<()> {
    let pid = process::spawn_app(&state.paths, &app.config, state.event_sender.clone())?;
    app.pid = Some(pid);
    app.status = AppStatus::Online;
    app.started_at = Some(SystemTime::now());
    Ok(())
}

fn handle_exit_events(
    state: &mut DaemonState,
    event_receiver: &Receiver<ExitEvent>,
) -> anyhow::Result<()> {
    loop {
        match event_receiver.try_recv() {
            Ok(event) => handle_exit_event(state, event)?,
            Err(mpsc::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::TryRecvError::Disconnected) => anyhow::bail!("exit event channel closed"),
        }
    }
}

fn handle_exit_event(state: &mut DaemonState, event: ExitEvent) -> anyhow::Result<()> {
    let mut app = match state.apps.remove(&event.name) {
        Some(app) => app,
        None => return Ok(()),
    };
    if app.pid != Some(event.pid) {
        state.apps.insert(event.name, app);
        return Ok(());
    }
    app.pid = None;
    app.started_at = None;

    if matches!(app.status, AppStatus::Stopped | AppStatus::Stopping) {
        app.status = AppStatus::Stopped;
        state.apps.insert(app.config.name.clone(), app);
        return Ok(());
    }

    if should_mark_errored(&mut app) {
        app.status = AppStatus::Errored;
        state.apps.insert(app.config.name.clone(), app);
        return Ok(());
    }

    let name = app.config.name.clone();
    app.restarts += 1;
    app.status = AppStatus::Starting;
    spawn_runtime_app(state, &mut app)
        .with_context(|| format!("failed to restart app '{name}' after child exit"))?;
    state.apps.insert(name, app);
    Ok(())
}

fn should_mark_errored(app: &mut RuntimeApp) -> bool {
    let now = SystemTime::now();
    let window = Duration::from_secs(app.config.restart_policy.window_seconds);
    app.restart_times
        .retain(|time| now.duration_since(*time).unwrap_or(window) <= window);
    app.restart_times.push(now);
    app.restart_times.len() > app.config.restart_policy.max_restarts as usize
}

fn list_apps(state: &DaemonState) -> Vec<AppStatusView> {
    let mut apps = state
        .apps
        .values()
        .map(|app| AppStatusView {
            name: app.config.name.clone(),
            pid: app.pid,
            status: app.status.as_str().to_string(),
            restarts: app.restarts,
            uptime_seconds: app.started_at.and_then(|started_at| {
                SystemTime::now()
                    .duration_since(started_at)
                    .ok()
                    .map(|duration| duration.as_secs())
            }),
            binary_path: app.config.binary_path.clone(),
        })
        .collect::<Vec<_>>();
    apps.sort_by(|left, right| left.name.cmp(&right.name));
    apps
}

#[allow(dead_code)]
fn unix_timestamp(time: SystemTime) -> anyhow::Result<u64> {
    Ok(time
        .duration_since(UNIX_EPOCH)
        .context("system time is before UNIX_EPOCH")?
        .as_secs())
}
