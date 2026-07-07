use anyhow::Context;
use std::env;
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
        Ok(Self {
            apps_dir: root_dir.join("apps"),
            logs_dir: root_dir.join("logs"),
            socket_path: root_dir.join("daemon.sock"),
            pid_path: root_dir.join("daemon.pid"),
            root_dir,
        })
    }

    pub fn ensure(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.root_dir)
            .with_context(|| format!("failed to create {}", self.root_dir.display()))?;
        std::fs::create_dir_all(&self.apps_dir)
            .with_context(|| format!("failed to create {}", self.apps_dir.display()))?;
        std::fs::create_dir_all(&self.logs_dir)
            .with_context(|| format!("failed to create {}", self.logs_dir.display()))?;
        Ok(())
    }

    pub fn app_config_path(&self, name: &str) -> PathBuf {
        self.apps_dir.join(format!("{name}.json"))
    }

    pub fn stdout_log_path(&self, name: &str) -> PathBuf {
        self.logs_dir.join(format!("{name}.out.log"))
    }

    pub fn stderr_log_path(&self, name: &str) -> PathBuf {
        self.logs_dir.join(format!("{name}.err.log"))
    }
}
