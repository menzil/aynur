use crate::app::AppConfig;
use crate::paths::AynurPaths;
use anyhow::Context;
use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

#[derive(Debug)]
pub struct ExitEvent {
    pub name: String,
    pub pid: u32,
}

pub fn spawn_app(
    paths: &AynurPaths,
    config: &AppConfig,
    event_sender: Sender<ExitEvent>,
) -> anyhow::Result<u32> {
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.stdout_log_path(&config.name))
        .with_context(|| format!("failed to open stdout log for app '{}'", config.name))?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.stderr_log_path(&config.name))
        .with_context(|| format!("failed to open stderr log for app '{}'", config.name))?;

    let mut command = Command::new(&config.binary_path);
    command
        .args(&config.args)
        .current_dir(&config.working_directory)
        .env_clear()
        .envs(&config.env)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command
        .spawn()
        .with_context(|| format!("failed to spawn app '{}'", config.name))?;
    let pid = child.id();
    watch_child(config.name.clone(), pid, child, event_sender);
    Ok(pid)
}

pub fn terminate_process_group(pid: u32) -> anyhow::Result<()> {
    send_signal_to_process_group(pid, libc::SIGTERM)
}

pub fn kill_process_group(pid: u32) -> anyhow::Result<()> {
    send_signal_to_process_group(pid, libc::SIGKILL)
}

fn send_signal_to_process_group(pid: u32, signal: libc::c_int) -> anyhow::Result<()> {
    let process_group = -(pid as libc::pid_t);
    let result = unsafe { libc::kill(process_group, signal) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        anyhow::bail!(
            "failed to send signal {} to process group {}: {}",
            signal,
            pid,
            error
        );
    }
    Ok(())
}

fn watch_child(name: String, pid: u32, mut child: Child, event_sender: Sender<ExitEvent>) {
    thread::spawn(move || {
        let _ = child.wait();
        let _ = event_sender.send(ExitEvent { name, pid });
    });
}
