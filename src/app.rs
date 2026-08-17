use crate::paths::AynurPaths;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub name: String,
    pub binary_path: PathBuf,
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    pub env: BTreeMap<String, String>,
    pub env_file_path: Option<PathBuf>,
    pub restart_policy: RestartPolicy,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestartPolicy {
    pub max_restarts: u32,
    pub window_seconds: u64,
}

impl AppConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.name.trim().is_empty() {
            anyhow::bail!("app name is empty");
        }
        if self.name.contains('/') {
            anyhow::bail!("app name '{}' must not contain '/'", self.name);
        }
        let metadata = std::fs::metadata(&self.binary_path).with_context(|| {
            format!(
                "binary for app '{}' does not exist at {}",
                self.name,
                self.binary_path.display()
            )
        })?;
        if !metadata.is_file() {
            anyhow::bail!(
                "binary for app '{}' is not a file: {}",
                self.name,
                self.binary_path.display()
            );
        }
        if metadata.permissions().mode() & 0o111 == 0 {
            anyhow::bail!(
                "binary for app '{}' is not executable: {}",
                self.name,
                self.binary_path.display()
            );
        }
        let cwd_metadata = std::fs::metadata(&self.working_directory).with_context(|| {
            format!(
                "working directory for app '{}' does not exist at {}",
                self.name,
                self.working_directory.display()
            )
        })?;
        if !cwd_metadata.is_dir() {
            anyhow::bail!(
                "working directory for app '{}' is not a directory: {}",
                self.name,
                self.working_directory.display()
            );
        }
        Ok(())
    }

    pub fn save(&self, paths: &AynurPaths) -> anyhow::Result<()> {
        let content = serde_json::to_string_pretty(self)
            .with_context(|| format!("failed to serialize config for app '{}'", self.name))?;
        let path = paths.app_config_path(&self.name);
        std::fs::write(&path, content)
            .with_context(|| format!("failed to write app config at {}", path.display()))
    }

    pub fn load(paths: &AynurPaths, name: &str) -> anyhow::Result<Self> {
        if name.trim().is_empty() {
            anyhow::bail!("app name is empty");
        }
        if name.contains('/') {
            anyhow::bail!("app name '{name}' must not contain '/'");
        }
        let path = paths.app_config_path(name);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read app config at {}", path.display()))?;
        let config = serde_json::from_str::<Self>(&content)
            .with_context(|| format!("failed to parse app config at {}", path.display()))?;
        if config.name != name {
            anyhow::bail!(
                "app config at {} has name '{}', expected '{}'",
                path.display(),
                config.name,
                name
            );
        }
        Ok(config)
    }

    pub fn delete(paths: &AynurPaths, name: &str) -> anyhow::Result<()> {
        let path = paths.app_config_path(name);
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to delete app config at {}", path.display()))?;
        }
        Ok(())
    }
}
