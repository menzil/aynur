use anyhow::Context;
use std::env;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct AynurPaths {
    pub root_dir: PathBuf,
    pub apps_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
}

impl AynurPaths {
    pub fn from_env() -> anyhow::Result<Self> {
        let root_dir = match env::var_os("AYNUR_HOME") {
            Some(path) => PathBuf::from(path),
            None => {
                let home = env::var_os("HOME")
                    .context("HOME is not set; set AYNUR_HOME to choose aynur state directory")?;
                PathBuf::from(home).join(".aynur")
            }
        };
        let root_dir = absolutize_path(root_dir)?;
        Ok(Self {
            apps_dir: root_dir.join("apps"),
            logs_dir: root_dir.join("logs"),
            socket_path: root_dir.join("daemon.sock"),
            pid_path: root_dir.join("daemon.pid"),
            root_dir,
        })
    }

    pub fn ensure(&self) -> anyhow::Result<()> {
        ensure_private_directory(&self.root_dir, "AYNUR_HOME")?;
        ensure_private_directory(&self.apps_dir, "aynur apps directory")?;
        ensure_private_directory(&self.logs_dir, "aynur logs directory")?;
        Ok(())
    }

    pub fn app_config_path(&self, name: &str) -> PathBuf {
        self.apps_dir.join(format!("{name}.json"))
    }

    pub fn saved_apps_path(&self) -> PathBuf {
        self.root_dir.join("saved.json")
    }

    pub fn daemon_error_log_path(&self) -> PathBuf {
        self.root_dir.join("daemon.err.log")
    }

    pub fn stdout_log_path(&self, name: &str) -> PathBuf {
        self.logs_dir.join(format!("{name}.out.log"))
    }

    pub fn stderr_log_path(&self, name: &str) -> PathBuf {
        self.logs_dir.join(format!("{name}.err.log"))
    }
}

fn absolutize_path(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(env::current_dir()
        .context("failed to read current working directory")?
        .join(path))
}

fn ensure_private_directory(path: &PathBuf, label: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create {label} at {}", path.display()))?;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {label} at {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!("{label} must not be a symbolic link: {}", path.display());
    }
    if !metadata.is_dir() {
        anyhow::bail!("{label} is not a directory: {}", path.display());
    }
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid {
        anyhow::bail!(
            "{label} at {} is owned by uid {}, expected uid {}",
            path.display(),
            metadata.uid(),
            uid
        );
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions).with_context(|| {
        format!(
            "failed to restrict permissions for {label} at {}",
            path.display()
        )
    })
}
