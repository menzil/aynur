use crate::app::AppConfig;
use crate::paths::AynurPaths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DaemonRequest {
    Ping,
    Start {
        config: AppConfig,
    },
    Stop {
        name: String,
    },
    Restart {
        name: String,
    },
    Reload {
        name: String,
    },
    ReloadUpdateEnv {
        name: String,
        env: BTreeMap<String, String>,
    },
    List,
    Delete {
        name: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DaemonResponse {
    Ok { message: String },
    Error { message: String },
    List { apps: Vec<AppStatusView> },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStatusView {
    pub name: String,
    pub pid: Option<u32>,
    pub status: String,
    pub restarts: u32,
    pub uptime_seconds: Option<u64>,
    pub binary_path: PathBuf,
}

pub fn send_request(paths: &AynurPaths, request: &DaemonRequest) -> anyhow::Result<DaemonResponse> {
    let mut stream = UnixStream::connect(&paths.socket_path)?;
    let request_line = serde_json::to_string(request)?;
    stream.write_all(request_line.as_bytes())?;
    stream.write_all(b"\n")?;

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line)?;
    let response = serde_json::from_str(&response_line)?;
    Ok(response)
}
