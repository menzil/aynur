use crate::paths::AynurPaths;
use anyhow::Context;
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

#[cfg(any(target_os = "macos", test))]
const LAUNCHD_LABEL: &str = "cn.aynur.daemon";
#[cfg(target_os = "linux")]
const SYSTEMD_SERVICE_NAME: &str = "aynur.service";

pub fn install(paths: &AynurPaths) -> anyhow::Result<String> {
    let executable = secure_executable_path(
        &env::current_exe().context("failed to resolve current executable path")?,
    )?;
    platform_install(paths, &executable)
}

pub fn uninstall(paths: &AynurPaths) -> anyhow::Result<String> {
    platform_uninstall(paths)
}

#[cfg(target_os = "linux")]
fn platform_install(paths: &AynurPaths, executable: &Path) -> anyhow::Result<String> {
    let user =
        env::var("USER").context("USER is not set; cannot render loginctl enable-linger hint")?;
    let service_path = systemd_service_path()?;
    write_startup_file(&service_path, &render_systemd_unit(executable, paths)?)?;
    run_command("systemctl", &["--user", "daemon-reload"])?;
    run_command("systemctl", &["--user", "enable", SYSTEMD_SERVICE_NAME])?;
    Ok(format!(
        "installed aynur startup service at {}\nIt will start at the next user session. To start aynur at boot before login, run:\n  loginctl enable-linger {user}",
        service_path.display()
    ))
}

#[cfg(target_os = "linux")]
fn platform_uninstall(_paths: &AynurPaths) -> anyhow::Result<String> {
    let service_path = systemd_service_path()?;
    run_command("systemctl", &["--user", "disable", SYSTEMD_SERVICE_NAME])?;
    if service_path.exists() {
        std::fs::remove_file(&service_path).with_context(|| {
            format!(
                "failed to remove systemd user service at {}",
                service_path.display()
            )
        })?;
    }
    run_command("systemctl", &["--user", "daemon-reload"])?;
    Ok(format!(
        "removed aynur startup service at {}",
        service_path.display()
    ))
}

#[cfg(target_os = "macos")]
fn platform_install(paths: &AynurPaths, executable: &Path) -> anyhow::Result<String> {
    let plist_path = launchd_plist_path()?;
    write_startup_file(&plist_path, &render_launchd_plist(executable, paths)?)?;
    let service_target = launchd_service_target();
    run_command("launchctl", &["enable", &service_target])?;
    Ok(format!(
        "installed aynur LaunchAgent at {}\nIt will start at the next user login",
        plist_path.display()
    ))
}

#[cfg(target_os = "macos")]
fn platform_uninstall(_paths: &AynurPaths) -> anyhow::Result<String> {
    let plist_path = launchd_plist_path()?;
    let service_target = launchd_service_target();
    run_command("launchctl", &["disable", &service_target])?;
    if plist_path.exists() {
        std::fs::remove_file(&plist_path)
            .with_context(|| format!("failed to remove LaunchAgent at {}", plist_path.display()))?;
    }
    Ok(format!(
        "removed aynur LaunchAgent at {}",
        plist_path.display()
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_install(_paths: &AynurPaths, _executable: &Path) -> anyhow::Result<String> {
    anyhow::bail!(
        "aynur startup is only supported on Linux systemd user services and macOS launchd"
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_uninstall(_paths: &AynurPaths) -> anyhow::Result<String> {
    anyhow::bail!(
        "aynur unstartup is only supported on Linux systemd user services and macOS launchd"
    )
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn render_systemd_unit(executable: &Path, paths: &AynurPaths) -> anyhow::Result<String> {
    let executable = path_to_string(executable, "aynur executable path")?;
    let aynur_home = path_to_string(&paths.root_dir, "AYNUR_HOME path")?;
    Ok(format!(
        "[Unit]\nDescription=aynur daemon\nAfter=default.target\n\n[Service]\nType=simple\nEnvironment={}\nExecStart={} __daemon\nRestart=always\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n",
        systemd_quote(&format!("AYNUR_HOME={aynur_home}")),
        systemd_quote(&executable)
    ))
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn render_launchd_plist(
    executable: &Path,
    paths: &AynurPaths,
) -> anyhow::Result<String> {
    let executable = path_to_string(executable, "aynur executable path")?;
    let aynur_home = path_to_string(&paths.root_dir, "AYNUR_HOME path")?;
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{}</string>\n    <string>__daemon</string>\n  </array>\n  <key>EnvironmentVariables</key>\n  <dict>\n    <key>AYNUR_HOME</key>\n    <string>{}</string>\n  </dict>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n</dict>\n</plist>\n",
        xml_escape(LAUNCHD_LABEL),
        xml_escape(&executable),
        xml_escape(&aynur_home)
    ))
}

fn write_startup_file(path: &Path, content: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("startup file path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create startup service directory at {}",
            parent.display()
        )
    })?;
    let file_name = path
        .file_name()
        .with_context(|| format!("startup service path has no file name: {}", path.display()))?;
    let temporary_path = path.with_file_name(format!(".{}.tmp", file_name.to_string_lossy()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary_path)
        .with_context(|| {
            format!(
                "failed to create temporary startup service at {}",
                temporary_path.display()
            )
        })?;
    file.write_all(content.as_bytes()).with_context(|| {
        format!(
            "failed to write temporary startup service at {}",
            temporary_path.display()
        )
    })?;
    file.sync_all().with_context(|| {
        format!(
            "failed to flush temporary startup service at {}",
            temporary_path.display()
        )
    })?;
    drop(file);
    std::fs::rename(&temporary_path, path).with_context(|| {
        format!(
            "failed to replace startup service at {} with {}",
            path.display(),
            temporary_path.display()
        )
    })
}

#[cfg(target_os = "linux")]
fn systemd_service_path() -> anyhow::Result<PathBuf> {
    Ok(xdg_config_home()?
        .join("systemd/user")
        .join(SYSTEMD_SERVICE_NAME))
}

#[cfg(target_os = "linux")]
fn xdg_config_home() -> anyhow::Result<PathBuf> {
    match env::var_os("XDG_CONFIG_HOME") {
        Some(path) => absolute_path(PathBuf::from(path)),
        None => Ok(user_home()?.join(".config")),
    }
}

#[cfg(target_os = "macos")]
fn launchd_plist_path() -> anyhow::Result<PathBuf> {
    Ok(user_home()?
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn user_home() -> anyhow::Result<PathBuf> {
    let home = env::var_os("HOME")
        .context("HOME is not set; cannot resolve user startup service directory")?;
    absolute_path(PathBuf::from(home))
}

#[cfg(target_os = "macos")]
fn launchd_domain() -> String {
    let uid = unsafe { libc::getuid() };
    format!("gui/{uid}")
}

#[cfg(target_os = "macos")]
fn launchd_service_target() -> String {
    format!("{}/{}", launchd_domain(), LAUNCHD_LABEL)
}

fn run_command(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = ProcessCommand::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run command `{}`", format_command(program, args)))?;
    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!(
        "command `{}` failed with status {}\nstdout:\n{}\nstderr:\n{}",
        format_command(program, args),
        output.status,
        stdout,
        stderr
    );
}

fn format_command(program: &str, args: &[&str]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(program.to_string());
    parts.extend(args.iter().map(|arg| arg.to_string()));
    parts.join(" ")
}

fn path_to_string(path: &Path, label: &str) -> anyhow::Result<String> {
    let value = path
        .to_str()
        .with_context(|| format!("{label} is not valid UTF-8: {}", path.display()))?;
    reject_line_breaks(value, label)?;
    Ok(value.to_string())
}

fn absolute_path(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(env::current_dir()
        .context("failed to read current working directory")?
        .join(path))
}

fn secure_executable_path(path: &Path) -> anyhow::Result<PathBuf> {
    let executable = path
        .canonicalize()
        .with_context(|| format!("failed to resolve aynur executable at {}", path.display()))?;
    let metadata = std::fs::symlink_metadata(&executable).with_context(|| {
        format!(
            "failed to inspect aynur executable at {}",
            executable.display()
        )
    })?;
    if !metadata.is_file() {
        anyhow::bail!(
            "aynur executable is not a regular file: {}",
            executable.display()
        );
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        anyhow::bail!(
            "aynur executable is not executable: {}",
            executable.display()
        );
    }

    let uid = unsafe { libc::geteuid() };
    let mut current = executable.as_path();
    loop {
        let current_metadata = std::fs::symlink_metadata(current).with_context(|| {
            format!(
                "failed to inspect aynur executable path component {}",
                current.display()
            )
        })?;
        if current_metadata.file_type().is_symlink() {
            anyhow::bail!(
                "aynur executable path contains a symbolic link: {}",
                current.display()
            );
        }
        if current_metadata.permissions().mode() & 0o022 != 0 {
            anyhow::bail!(
                "aynur executable path component is writable by another user: {}",
                current.display()
            );
        }
        if current_metadata.uid() != uid && current_metadata.uid() != 0 {
            anyhow::bail!(
                "aynur executable path component is owned by uid {}: {}",
                current_metadata.uid(),
                current.display()
            );
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent;
    }
    Ok(executable)
}

fn reject_line_breaks(value: &str, label: &str) -> anyhow::Result<()> {
    if value.contains('\n') || value.contains('\r') {
        anyhow::bail!("{label} contains a line break");
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn systemd_quote(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '%' => escaped.push_str("%%"),
            value => escaped.push(value),
        }
    }
    format!("\"{escaped}\"")
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            value => escaped.push(value),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::{render_launchd_plist, render_systemd_unit};
    use crate::paths::AynurPaths;
    use std::path::{Path, PathBuf};

    fn test_paths(root_dir: PathBuf) -> AynurPaths {
        AynurPaths {
            apps_dir: root_dir.join("apps"),
            logs_dir: root_dir.join("logs"),
            socket_path: root_dir.join("daemon.sock"),
            pid_path: root_dir.join("daemon.pid"),
            root_dir,
        }
    }

    #[test]
    fn renders_linux_systemd_unit_with_executable_and_aynur_home() {
        let paths = test_paths(PathBuf::from("/home/aynur/.aynur"));
        let unit =
            render_systemd_unit(Path::new("/usr/local/bin/aynur"), &paths).expect("systemd unit");

        assert!(unit.contains("ExecStart=\"/usr/local/bin/aynur\" __daemon"));
        assert!(unit.contains("Environment=\"AYNUR_HOME=/home/aynur/.aynur\""));
    }

    #[test]
    fn renders_macos_launchd_plist_with_program_arguments_and_environment() {
        let paths = test_paths(PathBuf::from("/Users/aynur/.aynur"));
        let plist = render_launchd_plist(Path::new("/usr/local/bin/aynur"), &paths).expect("plist");

        assert!(plist.contains("<key>ProgramArguments</key>"));
        assert!(plist.contains("<string>/usr/local/bin/aynur</string>"));
        assert!(plist.contains("<string>__daemon</string>"));
        assert!(plist.contains("<key>EnvironmentVariables</key>"));
        assert!(plist.contains("<key>AYNUR_HOME</key>"));
        assert!(plist.contains("<string>/Users/aynur/.aynur</string>"));
    }
}
