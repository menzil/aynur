use crate::app::AppConfig;
use crate::env_file;
use crate::ipc::{AppStatusView, DaemonRequest, DaemonResponse};
use crate::paths::AynurPaths;
use crate::process::{self, ExitEvent};
use crate::saved::{SavedApps, unix_timestamp};
use anyhow::Context;
use std::collections::{BTreeMap, HashMap};
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
        DaemonRequest::Save => response_from_result(save_online_apps(state)),
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
    if let Some(existing) = state.apps.get(&config.name)
        && existing.pid.is_some()
    {
        anyhow::bail!("app '{}' is already running", config.name);
    }

    let name = config.name.clone();
    let mut runtime = new_runtime_app(config, AppStatus::Starting);
    spawn_runtime_app(state, &mut runtime)?;
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

#[cfg(test)]
mod tests {
    use super::{
        AppStatus, DaemonState, RuntimeApp, acquire_pid_lock, restore_saved_apps, save_online_apps,
        set_connection_blocking, stop_app,
    };
    use crate::app::{AppConfig, RestartPolicy};
    use crate::paths::AynurPaths;
    use crate::saved::SavedApps;
    use std::collections::{BTreeMap, HashMap};
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
    }
}
