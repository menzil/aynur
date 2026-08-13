use crate::app::AppConfig;
use crate::paths::AynurPaths;
use anyhow::Context;
use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

pub fn read_process_rss_bytes(pid: u32) -> anyhow::Result<u64> {
    platform::read_process_rss_bytes(pid)
}

#[cfg(target_os = "linux")]
mod platform {
    use anyhow::Context;

    pub fn read_process_rss_bytes(pid: u32) -> anyhow::Result<u64> {
        let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).with_context(|| {
            format!("failed to read RSS for process {pid} from /proc/{pid}/statm")
        })?;
        let resident_pages = statm
            .split_whitespace()
            .nth(1)
            .with_context(|| {
                format!("RSS data for process {pid} is missing the resident page count")
            })?
            .parse::<u64>()
            .with_context(|| {
                format!("RSS data for process {pid} has an invalid resident page count")
            })?;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 {
            anyhow::bail!("failed to read system page size while sampling process {pid} RSS");
        }
        Ok(resident_pages * page_size as u64)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    pub fn read_process_rss_bytes(pid: u32) -> anyhow::Result<u64> {
        let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        let result = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTASKINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if result != size {
            anyhow::bail!(
                "failed to read RSS for process {pid} with proc_pidinfo (returned {result}, expected {size})"
            );
        }
        let info = unsafe { info.assume_init() };
        Ok(info.pti_resident_size)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    pub fn read_process_rss_bytes(pid: u32) -> anyhow::Result<u64> {
        anyhow::bail!("reading RSS is unsupported on this platform for process {pid}");
    }
}

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

#[cfg(test)]
mod tests {
    use super::read_process_rss_bytes;

    #[test]
    fn reads_rss_for_the_current_process() {
        assert!(read_process_rss_bytes(std::process::id()).expect("current process RSS") > 0);
    }

    #[test]
    fn reports_missing_process_rss_as_an_error() {
        let error = read_process_rss_bytes(u32::MAX).expect_err("missing process must fail");
        assert!(!error.to_string().is_empty());
    }
}
