use crate::app::AppConfig;
use crate::env_file;
use crate::ipc::{AppStatusView, DAEMON_PROTOCOL_VERSION, DaemonRequest, DaemonResponse};
use crate::paths::AynurPaths;
use crate::process::{self, ExitEvent};
use crate::saved::{SavedApps, unix_timestamp};
use anyhow::Context;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, SystemTime};

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
    let _pid_lock = acquire_pid_lock(&paths)?;
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
    restore_saved_apps(&mut state)?;

    loop {
        accept_pending_connections(&listener, &mut state)?;
        handle_exit_events(&mut state, &event_receiver)?;
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn acquire_pid_lock(paths: &AynurPaths) -> anyhow::Result<File> {
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&paths.pid_path)
        .with_context(|| format!("failed to open daemon pid at {}", paths.pid_path.display()))?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            anyhow::bail!(
                "aynur daemon is already running for {}",
                paths.root_dir.display()
            );
        }
        return Err(error)
            .with_context(|| format!("failed to lock daemon pid at {}", paths.pid_path.display()));
    }
    file.set_len(0).with_context(|| {
        format!(
            "failed to truncate daemon pid at {}",
            paths.pid_path.display()
        )
    })?;
    file.write_all(std::process::id().to_string().as_bytes())
        .with_context(|| format!("failed to write daemon pid at {}", paths.pid_path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to flush daemon pid at {}", paths.pid_path.display()))?;
    Ok(file)
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
            Ok((stream, _address)) => {
                set_connection_blocking(&stream)?;
                if let Err(error) = handle_connection(stream, state) {
                    append_daemon_error(
                        &state.paths,
                        &format!("failed to handle daemon request: {error:#}"),
                    )?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error).context("failed to accept daemon connection"),
        }
    }
}

fn set_connection_blocking(stream: &UnixStream) -> anyhow::Result<()> {
    stream
        .set_nonblocking(false)
        .context("failed to set daemon connection to blocking")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .context("failed to set daemon connection read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .context("failed to set daemon connection write timeout")
}

fn handle_connection(mut stream: UnixStream, state: &mut DaemonState) -> anyhow::Result<()> {
    let mut request_line = String::new();
    {
        let mut reader = BufReader::new(&stream);
        reader
            .read_line(&mut request_line)
            .context("failed to read daemon request")?;
    }
    let response = response_from_request_line(&request_line, state);
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

fn response_from_request_line(request_line: &str, state: &mut DaemonState) -> DaemonResponse {
    match serde_json::from_str::<DaemonRequest>(request_line) {
        Ok(request) => handle_request(request, state),
        Err(error) => DaemonResponse::Error {
            message: format!("unsupported or malformed daemon request: {error}"),
        },
    }
}

fn handle_request(request: DaemonRequest, state: &mut DaemonState) -> DaemonResponse {
    match request {
        DaemonRequest::Ping => DaemonResponse::Pong {
            protocol_version: DAEMON_PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        },
        DaemonRequest::Start { config } => {
            let result = start_app(state, config);
            response_from_mutation_result(state, result, None)
        }
        DaemonRequest::Stop { name } => {
            let result = stop_app(state, &name);
            response_from_mutation_result(state, result, Some(&name))
        }
        DaemonRequest::Restart { name } => {
            let result = restart_app(state, &name);
            response_from_mutation_result(state, result, None)
        }
        DaemonRequest::Reload { name } => {
            let result = reload_app(state, &name);
            response_from_mutation_result(state, result, None)
        }
        DaemonRequest::ReloadUpdateEnv { name, env } => {
            let result = reload_update_env(state, &name, env);
            response_from_mutation_result(state, result, None)
        }
        DaemonRequest::List => DaemonResponse::List {
            apps: list_apps(state),
        },
        DaemonRequest::Save => response_from_result(save_online_apps(state)),
        DaemonRequest::Delete { name } => {
            let result = delete_app(state, &name);
            response_from_mutation_result(state, result, Some(&name))
        }
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

fn response_from_mutation_result(
    state: &DaemonState,
    result: anyhow::Result<String>,
    excluded_name: Option<&str>,
) -> DaemonResponse {
    let result = result.and_then(|message| {
        synchronize_restart_snapshot(state, excluded_name)
            .context("failed to synchronize restart snapshot after successful operation")?;
        Ok(message)
    });
    response_from_result(result)
}

fn ok(message: &str) -> DaemonResponse {
    DaemonResponse::Ok {
        message: message.to_string(),
    }
}

fn start_app(state: &mut DaemonState, config: AppConfig) -> anyhow::Result<String> {
    config.validate()?;
    if let Some(existing) = state.apps.get(&config.name)
        && existing.pid.is_some()
    {
        anyhow::bail!("app '{}' is already running", config.name);
    }

    let name = config.name.clone();
    let mut runtime = new_runtime_app(config, AppStatus::Starting);
    if let Err(error) = spawn_runtime_app(state, &mut runtime) {
        runtime.status = AppStatus::Errored;
        state.apps.insert(name, runtime);
        return Err(error);
    }
    let pid = runtime.pid.context("spawned app did not record pid")?;
    state.apps.insert(name.clone(), runtime);
    Ok(format!("started {name} with pid {pid}"))
}

fn new_runtime_app(config: AppConfig, status: AppStatus) -> RuntimeApp {
    RuntimeApp {
        config,
        pid: None,
        status,
        restarts: 0,
        started_at: None,
        restart_times: Vec::new(),
    }
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
    if let Err(error) = spawn_runtime_app(state, &mut app) {
        app.status = AppStatus::Errored;
        state.apps.insert(name.to_string(), app);
        return Err(error);
    }
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

fn save_online_apps(state: &DaemonState) -> anyhow::Result<String> {
    let app_names = state
        .apps
        .values()
        .filter(|app| matches!(app.status, AppStatus::Online))
        .map(|app| app.config.name.clone())
        .collect::<Vec<_>>();
    let saved_apps = SavedApps::from_app_names(app_names, unix_timestamp(SystemTime::now())?);
    saved_apps.save(&state.paths)?;
    Ok(format!(
        "saved {} online app(s) to {}",
        saved_apps.app_names.len(),
        state.paths.saved_apps_path().display()
    ))
}

fn synchronize_restart_snapshot(
    state: &DaemonState,
    excluded_name: Option<&str>,
) -> anyhow::Result<String> {
    let previous_names = match SavedApps::load_optional(&state.paths) {
        Ok(saved_apps) => saved_apps
            .map(|saved_apps| saved_apps.app_names)
            .unwrap_or_default(),
        Err(error) => {
            append_daemon_error(
                &state.paths,
                &format!(
                    "failed to read the existing restart snapshot during automatic sync; rebuilding it from daemon state: {error:#}"
                ),
            )?;
            Vec::new()
        }
    };
    let mut app_names = state
        .apps
        .values()
        .filter(|app| matches!(app.status, AppStatus::Online | AppStatus::Errored))
        .map(|app| app.config.name.clone())
        .collect::<BTreeSet<_>>();

    for name in previous_names {
        if excluded_name == Some(name.as_str()) || state.apps.contains_key(&name) {
            continue;
        }
        app_names.insert(name);
    }

    let saved_apps = SavedApps::from_app_names(
        app_names.into_iter().collect(),
        unix_timestamp(SystemTime::now())?,
    );
    saved_apps.save(&state.paths)?;
    Ok(format!(
        "saved {} restart app(s) to {}",
        saved_apps.app_names.len(),
        state.paths.saved_apps_path().display()
    ))
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

fn restore_saved_apps(state: &mut DaemonState) -> anyhow::Result<()> {
    let Some(saved_apps) = SavedApps::load_optional(&state.paths)? else {
        return Ok(());
    };

    for app_name in saved_apps.app_names {
        restore_saved_app(state, &app_name)?;
    }
    Ok(())
}

fn restore_saved_app(state: &mut DaemonState, name: &str) -> anyhow::Result<()> {
    let config_path = state.paths.app_config_path(name);
    if !config_path.is_file() {
        append_daemon_error(
            &state.paths,
            &format!(
                "failed to restore app '{name}': saved app config is missing at {}",
                config_path.display()
            ),
        )?;
        return Ok(());
    }

    let config = match AppConfig::load(&state.paths, name) {
        Ok(config) => config,
        Err(error) => {
            append_daemon_error(
                &state.paths,
                &format!("failed to restore app '{name}': {error:#}"),
            )?;
            return Ok(());
        }
    };
    let app_name = config.name.clone();
    let mut runtime = new_runtime_app(config, AppStatus::Starting);
    let restore_result = runtime
        .config
        .validate()
        .and_then(|()| spawn_runtime_app(state, &mut runtime));
    match restore_result {
        Ok(()) => {
            state.apps.insert(app_name, runtime);
        }
        Err(error) => {
            runtime.pid = None;
            runtime.started_at = None;
            runtime.status = AppStatus::Errored;
            append_daemon_error(
                &state.paths,
                &format!("failed to restore app '{app_name}': {error:#}"),
            )?;
            state.apps.insert(app_name, runtime);
        }
    }
    Ok(())
}

fn append_daemon_error(paths: &AynurPaths, message: &str) -> anyhow::Result<()> {
    let log_path = paths.daemon_error_log_path();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open daemon error log at {}", log_path.display()))?;
    writeln!(file, "{message}").with_context(|| {
        format!(
            "failed to append daemon error log at {}",
            log_path.display()
        )
    })
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
    if let Err(error) = spawn_runtime_app(state, &mut app) {
        app.status = AppStatus::Errored;
        append_daemon_error(
            &state.paths,
            &format!("failed to restart app '{name}' after child exit: {error:#}"),
        )?;
        state.apps.insert(name, app);
        return Ok(());
    }
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

#[cfg(test)]
mod tests {
    use super::{
        AppStatus, DaemonState, RuntimeApp, acquire_pid_lock, handle_connection, handle_exit_event,
        handle_request, restore_saved_apps, save_online_apps, set_connection_blocking, stop_app,
    };
    use crate::app::{AppConfig, RestartPolicy};
    use crate::ipc::{DaemonRequest, DaemonResponse};
    use crate::paths::AynurPaths;
    use crate::process::ExitEvent;
    use crate::saved::SavedApps;
    use std::collections::{BTreeMap, HashMap};
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::mpsc;

    fn test_paths(root_dir: PathBuf) -> AynurPaths {
        AynurPaths {
            apps_dir: root_dir.join("apps"),
            logs_dir: root_dir.join("logs"),
            socket_path: root_dir.join("daemon.sock"),
            pid_path: root_dir.join("daemon.pid"),
            root_dir,
        }
    }

    fn test_state(paths: AynurPaths) -> DaemonState {
        let (event_sender, _event_receiver) = mpsc::channel();
        DaemonState {
            paths,
            apps: HashMap::new(),
            event_sender,
        }
    }

    fn runtime_app(config: AppConfig, status: AppStatus) -> RuntimeApp {
        RuntimeApp {
            config,
            pid: None,
            status,
            restarts: 0,
            started_at: None,
            restart_times: Vec::new(),
        }
    }

    #[test]
    fn daemon_pid_lock_rejects_a_second_daemon_for_the_same_state() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");

        let _first_lock = acquire_pid_lock(&paths).expect("first daemon lock");
        let error = acquire_pid_lock(&paths).expect_err("second daemon lock");

        assert!(error.to_string().contains("daemon is already running"));
    }

    #[test]
    fn accepted_connections_are_blocking() {
        let (stream, _peer) = UnixStream::pair().expect("unix stream pair");
        stream
            .set_nonblocking(true)
            .expect("set stream nonblocking");

        set_connection_blocking(&stream).expect("set stream blocking");

        let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0, "failed to read stream flags");
        assert_eq!(flags & libc::O_NONBLOCK, 0);
    }

    fn sleep_config(paths: &AynurPaths, name: &str) -> AppConfig {
        AppConfig {
            name: name.to_string(),
            binary_path: PathBuf::from("/bin/sleep"),
            args: vec!["30".to_string()],
            working_directory: paths.root_dir.clone(),
            env: BTreeMap::new(),
            env_file_path: None,
            restart_policy: RestartPolicy {
                max_restarts: 5,
                window_seconds: 10,
            },
        }
    }

    fn missing_binary_config(paths: &AynurPaths, name: &str) -> AppConfig {
        AppConfig {
            name: name.to_string(),
            binary_path: paths.root_dir.join("missing-binary"),
            args: Vec::new(),
            working_directory: paths.root_dir.clone(),
            env: BTreeMap::new(),
            env_file_path: None,
            restart_policy: RestartPolicy {
                max_restarts: 5,
                window_seconds: 10,
            },
        }
    }

    fn immediate_exit_config(paths: &AynurPaths, name: &str) -> AppConfig {
        AppConfig {
            name: name.to_string(),
            binary_path: PathBuf::from("/usr/bin/false"),
            args: Vec::new(),
            working_directory: paths.root_dir.clone(),
            env: BTreeMap::new(),
            env_file_path: None,
            restart_policy: RestartPolicy {
                max_restarts: 5,
                window_seconds: 10,
            },
        }
    }

    fn save_snapshot(paths: &AynurPaths, app_names: Vec<String>) {
        SavedApps {
            version: 1,
            saved_at_unix_seconds: 1,
            app_names,
        }
        .save(paths)
        .expect("save app snapshot");
    }

    #[test]
    fn restores_saved_app_config_as_runtime_app() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        sleep_config(&paths, "api")
            .save(&paths)
            .expect("app config");
        save_snapshot(&paths, vec!["api".to_string()]);
        let mut state = test_state(paths);

        restore_saved_apps(&mut state).expect("restore saved apps");

        let app = state.apps.get("api").expect("restored app");
        assert!(app.pid.is_some());
        assert_eq!(app.status.as_str(), "online");
        stop_app(&mut state, "api").expect("stop restored app");
    }

    #[test]
    fn daemon_saves_only_online_runtime_apps() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        let mut state = test_state(paths);
        state.apps.insert(
            "online".to_string(),
            runtime_app(sleep_config(&state.paths, "online"), AppStatus::Online),
        );
        state.apps.insert(
            "stopped".to_string(),
            runtime_app(sleep_config(&state.paths, "stopped"), AppStatus::Stopped),
        );

        save_online_apps(&state).expect("save online apps");

        let saved_apps = SavedApps::load_optional(&state.paths)
            .expect("load saved apps")
            .expect("saved apps");
        assert_eq!(saved_apps.app_names, vec!["online"]);
    }

    #[test]
    fn ping_reports_daemon_protocol_and_version() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        let mut state = test_state(paths);

        let response = handle_request(DaemonRequest::Ping, &mut state);

        assert!(matches!(
            response,
            DaemonResponse::Pong {
                protocol_version: crate::ipc::DAEMON_PROTOCOL_VERSION,
                daemon_version,
            } if daemon_version == env!("CARGO_PKG_VERSION")
        ));
    }

    #[test]
    fn malformed_request_returns_a_structured_error_response() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        let mut state = test_state(paths);
        let (mut client, server) = UnixStream::pair().expect("unix stream pair");
        client
            .write_all(b"{\"type\":\"futureCommand\"}\n")
            .expect("write unsupported request");

        handle_connection(server, &mut state).expect("handle unsupported request");

        let mut response_line = String::new();
        BufReader::new(client)
            .read_line(&mut response_line)
            .expect("read daemon response");
        let response =
            serde_json::from_str::<DaemonResponse>(&response_line).expect("parse daemon response");
        match response {
            DaemonResponse::Error { message } => {
                assert!(message.contains("unsupported or malformed daemon request"));
                assert!(message.contains("futureCommand"));
            }
            response => panic!("expected error response, got {response:?}"),
        }
    }

    #[test]
    fn reports_an_app_that_exits_during_startup() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        let config = immediate_exit_config(&paths, "api");
        config.save(&paths).expect("app config");
        let mut state = test_state(paths);

        let response = handle_request(DaemonRequest::Start { config }, &mut state);

        match response {
            DaemonResponse::Error { message } => {
                assert!(message.contains("app 'api' exited during startup"));
                assert!(message.contains("/usr/bin/false"));
                assert!(message.contains("api.err.log"));
            }
            response => panic!("expected startup error, got {response:?}"),
        }
        let app = state.apps.get("api").expect("errored app state");
        assert_eq!(app.pid, None);
        assert!(matches!(app.status, AppStatus::Errored));
    }

    #[test]
    fn marks_an_immediate_automatic_restart_failure_as_errored() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        let mut state = test_state(paths);
        let mut app = runtime_app(
            immediate_exit_config(&state.paths, "api"),
            AppStatus::Online,
        );
        app.pid = Some(42);
        state.apps.insert("api".to_string(), app);

        handle_exit_event(
            &mut state,
            ExitEvent {
                name: "api".to_string(),
                pid: 42,
            },
        )
        .expect("automatic restart failure must not stop daemon");

        let app = state.apps.get("api").expect("errored app state");
        assert_eq!(app.pid, None);
        assert!(matches!(app.status, AppStatus::Errored));
        let log_content =
            std::fs::read_to_string(state.paths.daemon_error_log_path()).expect("daemon error log");
        assert!(log_content.contains("failed to restart app 'api' after child exit"));
        assert!(log_content.contains("exited during startup"));
    }

    #[test]
    fn lifecycle_mutations_refresh_the_restart_snapshot() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        let config = sleep_config(&paths, "api");
        config.save(&paths).expect("app config");
        let mut state = test_state(paths);

        let response = handle_request(
            DaemonRequest::Start {
                config: config.clone(),
            },
            &mut state,
        );
        assert!(matches!(response, DaemonResponse::Ok { .. }));
        let saved_apps = SavedApps::load_optional(&state.paths)
            .expect("load saved apps after start")
            .expect("saved apps after start");
        assert_eq!(saved_apps.app_names, vec!["api"]);

        let response = handle_request(
            DaemonRequest::Restart {
                name: "api".to_string(),
            },
            &mut state,
        );
        assert!(matches!(response, DaemonResponse::Ok { .. }));
        let saved_apps = SavedApps::load_optional(&state.paths)
            .expect("load saved apps after restart")
            .expect("saved apps after restart");
        assert_eq!(saved_apps.app_names, vec!["api"]);

        let response = handle_request(
            DaemonRequest::Reload {
                name: "api".to_string(),
            },
            &mut state,
        );
        assert!(matches!(response, DaemonResponse::Ok { .. }));
        let saved_apps = SavedApps::load_optional(&state.paths)
            .expect("load saved apps after reload")
            .expect("saved apps after reload");
        assert_eq!(saved_apps.app_names, vec!["api"]);

        let response = handle_request(
            DaemonRequest::Stop {
                name: "api".to_string(),
            },
            &mut state,
        );
        assert!(matches!(response, DaemonResponse::Ok { .. }));
        let saved_apps = SavedApps::load_optional(&state.paths)
            .expect("load saved apps after stop")
            .expect("saved apps after stop");
        assert!(saved_apps.app_names.is_empty());
    }

    #[test]
    fn delete_refreshes_the_restart_snapshot() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        let config = sleep_config(&paths, "api");
        config.save(&paths).expect("app config");
        let mut state = test_state(paths);

        let response = handle_request(DaemonRequest::Start { config }, &mut state);
        assert!(matches!(response, DaemonResponse::Ok { .. }));

        let response = handle_request(
            DaemonRequest::Delete {
                name: "api".to_string(),
            },
            &mut state,
        );
        assert!(matches!(response, DaemonResponse::Ok { .. }));
        let saved_apps = SavedApps::load_optional(&state.paths)
            .expect("load saved apps after delete")
            .expect("saved apps after delete");
        assert!(saved_apps.app_names.is_empty());
        assert!(!state.paths.app_config_path("api").exists());
    }

    #[test]
    fn failed_lifecycle_mutation_preserves_the_restart_snapshot() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        save_snapshot(&paths, vec!["api".to_string()]);
        let mut state = test_state(paths);

        let response = handle_request(
            DaemonRequest::Stop {
                name: "api".to_string(),
            },
            &mut state,
        );
        assert!(matches!(response, DaemonResponse::Error { .. }));
        let saved_apps = SavedApps::load_optional(&state.paths)
            .expect("load saved apps after failed stop")
            .expect("saved apps after failed stop");
        assert_eq!(saved_apps.app_names, vec!["api"]);
    }

    #[test]
    fn automatic_sync_preserves_failed_restore_entries_until_removed() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        save_snapshot(&paths, vec!["bad".to_string(), "good".to_string()]);
        let mut state = test_state(paths);
        state.apps.insert(
            "bad".to_string(),
            runtime_app(
                missing_binary_config(&state.paths, "bad"),
                AppStatus::Errored,
            ),
        );
        state.apps.insert(
            "good".to_string(),
            runtime_app(sleep_config(&state.paths, "good"), AppStatus::Online),
        );

        let response = handle_request(
            DaemonRequest::Stop {
                name: "good".to_string(),
            },
            &mut state,
        );
        assert!(matches!(response, DaemonResponse::Ok { .. }));
        let saved_apps = SavedApps::load_optional(&state.paths)
            .expect("load saved apps after stopping good")
            .expect("saved apps after stopping good");
        assert_eq!(saved_apps.app_names, vec!["bad"]);

        let response = handle_request(
            DaemonRequest::Delete {
                name: "bad".to_string(),
            },
            &mut state,
        );
        assert!(matches!(response, DaemonResponse::Ok { .. }));
        let saved_apps = SavedApps::load_optional(&state.paths)
            .expect("load saved apps after deleting bad")
            .expect("saved apps after deleting bad");
        assert!(saved_apps.app_names.is_empty());
    }

    #[test]
    fn automatic_sync_rebuilds_a_corrupt_restart_snapshot() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        std::fs::write(paths.saved_apps_path(), "not json").expect("corrupt snapshot");
        let mut state = test_state(paths);
        state.apps.insert(
            "api".to_string(),
            runtime_app(sleep_config(&state.paths, "api"), AppStatus::Online),
        );

        let response = handle_request(
            DaemonRequest::Delete {
                name: "missing".to_string(),
            },
            &mut state,
        );
        assert!(matches!(response, DaemonResponse::Ok { .. }));
        let saved_apps = SavedApps::load_optional(&state.paths)
            .expect("load rebuilt saved apps")
            .expect("rebuilt saved apps");
        assert_eq!(saved_apps.app_names, vec!["api"]);
        let log_content =
            std::fs::read_to_string(state.paths.daemon_error_log_path()).expect("daemon log");
        assert!(log_content.contains("rebuilding it from daemon state"));
    }

    #[test]
    fn marks_failed_restore_as_errored_and_continues_restoring_other_apps() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        missing_binary_config(&paths, "bad")
            .save(&paths)
            .expect("bad app config");
        sleep_config(&paths, "good")
            .save(&paths)
            .expect("good app config");
        save_snapshot(&paths, vec!["bad".to_string(), "good".to_string()]);
        let mut state = test_state(paths);

        restore_saved_apps(&mut state).expect("restore saved apps");

        let bad_app = state.apps.get("bad").expect("bad app");
        assert_eq!(bad_app.pid, None);
        assert!(matches!(bad_app.status, AppStatus::Errored));
        let good_app = state.apps.get("good").expect("good app");
        assert!(good_app.pid.is_some());
        assert_eq!(good_app.status.as_str(), "online");
        let log_content =
            std::fs::read_to_string(state.paths.daemon_error_log_path()).expect("error log");
        assert!(log_content.contains("failed to restore app 'bad'"));
        let saved_apps = SavedApps::load_optional(&state.paths)
            .expect("load saved apps after restore")
            .expect("saved apps after restore");
        assert_eq!(saved_apps.app_names, vec!["bad", "good"]);
        stop_app(&mut state, "good").expect("stop restored app");
    }

    #[test]
    fn skips_missing_saved_app_config_and_logs_the_error() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        save_snapshot(&paths, vec!["missing".to_string()]);
        let mut state = test_state(paths);

        restore_saved_apps(&mut state).expect("restore saved apps");

        assert!(!state.apps.contains_key("missing"));
        let log_content =
            std::fs::read_to_string(state.paths.daemon_error_log_path()).expect("error log");
        assert!(log_content.contains("saved app config is missing"));
        let saved_apps = SavedApps::load_optional(&state.paths)
            .expect("load saved apps after missing restore")
            .expect("saved apps after missing restore");
        assert_eq!(saved_apps.app_names, vec!["missing"]);
    }
}
