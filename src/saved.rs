use crate::paths::AynurPaths;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const SAVED_APPS_VERSION: u32 = 1;
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedApps {
    pub version: u32,
    pub saved_at_unix_seconds: u64,
    pub app_names: Vec<String>,
}

impl SavedApps {
    pub fn from_app_names(mut app_names: Vec<String>, saved_at_unix_seconds: u64) -> Self {
        app_names.sort();
        Self {
            version: SAVED_APPS_VERSION,
            saved_at_unix_seconds,
            app_names,
        }
    }

    pub fn load_optional(paths: &AynurPaths) -> anyhow::Result<Option<Self>> {
        let path = paths.saved_apps_path();
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read saved app list at {}", path.display())
                });
            }
        };
        let saved_apps = serde_json::from_str::<Self>(&content)
            .with_context(|| format!("failed to parse saved app list at {}", path.display()))?;
        saved_apps.validate()?;
        Ok(Some(saved_apps))
    }

    pub fn save(&self, paths: &AynurPaths) -> anyhow::Result<()> {
        self.validate()?;
        let path = paths.saved_apps_path();
        let content =
            serde_json::to_string_pretty(self).context("failed to serialize saved app list")?;
        write_atomically(&path, &content)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.version != SAVED_APPS_VERSION {
            anyhow::bail!(
                "saved app list version {} is unsupported; expected {}",
                self.version,
                SAVED_APPS_VERSION
            );
        }
        let mut names = BTreeSet::new();
        for name in &self.app_names {
            if name.trim().is_empty() {
                anyhow::bail!("saved app list contains an empty app name");
            }
            if name.contains('/') {
                anyhow::bail!("saved app name '{name}' must not contain '/'");
            }
            if !names.insert(name) {
                anyhow::bail!("saved app list contains duplicate app name '{name}'");
            }
        }
        Ok(())
    }
}

fn write_atomically(path: &Path, content: &str) -> anyhow::Result<()> {
    let temporary_path = path.with_file_name(".saved.json.tmp");
    match std::fs::remove_file(&temporary_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to remove stale temporary saved app list at {}",
                    temporary_path.display()
                )
            });
        }
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary_path)
        .with_context(|| {
            format!(
                "failed to create temporary saved app list at {}",
                temporary_path.display()
            )
        })?;
    file.write_all(content.as_bytes()).with_context(|| {
        format!(
            "failed to write temporary saved app list at {}",
            temporary_path.display()
        )
    })?;
    file.sync_all().with_context(|| {
        format!(
            "failed to flush temporary saved app list at {}",
            temporary_path.display()
        )
    })?;
    drop(file);
    std::fs::rename(&temporary_path, path).with_context(|| {
        format!(
            "failed to replace saved app list at {} with {}",
            path.display(),
            temporary_path.display()
        )
    })
}

pub fn unix_timestamp(time: SystemTime) -> anyhow::Result<u64> {
    Ok(time
        .duration_since(UNIX_EPOCH)
        .context("system time is before UNIX_EPOCH")?
        .as_secs())
}

#[cfg(test)]
mod tests {
    use super::{SavedApps, unix_timestamp};
    use crate::paths::AynurPaths;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

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
    fn sorts_app_names_before_saving() {
        let saved_apps = SavedApps::from_app_names(vec!["web".to_string(), "api".to_string()], 42);

        assert_eq!(
            saved_apps,
            SavedApps {
                version: 1,
                saved_at_unix_seconds: 42,
                app_names: vec!["api".to_string(), "web".to_string()],
            }
        );
    }

    #[test]
    fn writes_and_reads_strict_saved_app_fields() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        let saved_apps = SavedApps {
            version: 1,
            saved_at_unix_seconds: 7,
            app_names: vec!["api".to_string()],
        };

        saved_apps.save(&paths).expect("save app list");
        let content = std::fs::read_to_string(paths.saved_apps_path()).expect("saved json");
        let value = serde_json::from_str::<Value>(&content).expect("parse saved json");
        let keys = value
            .as_object()
            .expect("saved json object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();

        assert_eq!(keys, vec!["appNames", "savedAtUnixSeconds", "version"]);
        assert_eq!(
            SavedApps::load_optional(&paths).expect("load saved apps"),
            Some(saved_apps)
        );
        assert!(!paths.root_dir.join(".saved.json.tmp").exists());
    }

    #[test]
    fn replaces_a_stale_temporary_file_without_losing_the_previous_snapshot() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        let previous = SavedApps {
            version: 1,
            saved_at_unix_seconds: 7,
            app_names: vec!["api".to_string()],
        };
        previous.save(&paths).expect("previous snapshot");
        std::fs::write(paths.root_dir.join(".saved.json.tmp"), "partial")
            .expect("temporary snapshot");

        let replacement = SavedApps {
            version: 1,
            saved_at_unix_seconds: 8,
            app_names: vec!["web".to_string()],
        };
        replacement
            .save(&paths)
            .expect("replace stale temporary file");
        assert_eq!(
            SavedApps::load_optional(&paths).expect("load previous"),
            Some(replacement)
        );
        assert!(!paths.root_dir.join(".saved.json.tmp").exists());
    }

    #[test]
    fn returns_none_when_saved_app_list_is_missing() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");

        assert_eq!(
            SavedApps::load_optional(&paths).expect("missing saved apps"),
            None
        );
    }

    #[test]
    fn rejects_duplicate_saved_app_names() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let paths = test_paths(temp_dir.path().to_path_buf());
        paths.ensure().expect("state directories");
        std::fs::write(
            paths.saved_apps_path(),
            r#"{"version":1,"savedAtUnixSeconds":1,"appNames":["api","api"]}"#,
        )
        .expect("saved json");

        let error = SavedApps::load_optional(&paths).expect_err("duplicate app must fail");

        assert!(
            error
                .to_string()
                .contains("saved app list contains duplicate app name 'api'")
        );
    }

    #[test]
    fn converts_system_time_to_unix_seconds() {
        let time = UNIX_EPOCH + Duration::from_secs(123);

        assert_eq!(unix_timestamp(time).expect("unix timestamp"), 123);
    }
}
