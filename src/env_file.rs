use anyhow::Context;
use std::collections::BTreeMap;
use std::path::Path;

pub fn read_env_file(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read env file at {}", path.display()))?;
    let mut values = BTreeMap::new();

    for (index, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').with_context(|| {
            format!(
                "invalid env file line {} in {}; expected KEY=VALUE",
                index + 1,
                path.display()
            )
        })?;
        let trimmed_key = key.trim();
        if trimmed_key.is_empty() {
            anyhow::bail!(
                "invalid env file line {} in {}; key is empty",
                index + 1,
                path.display()
            );
        }
        values.insert(trimmed_key.to_string(), value.trim().to_string());
    }

    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::read_env_file;
    use std::io::Write;

    #[test]
    fn reads_key_value_lines() {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        writeln!(file, "A=1\n# comment\nB = two ").expect("write env");

        let values = read_env_file(file.path()).expect("read env");

        assert_eq!(values.get("A"), Some(&"1".to_string()));
        assert_eq!(values.get("B"), Some(&"two".to_string()));
    }
}
