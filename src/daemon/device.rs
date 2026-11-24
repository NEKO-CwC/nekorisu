//! This device's identity: a random id made on first run, and a readable name
//! (used to answer "the session from my desktop"). No secrets here.

use std::fs;

use serde::{Deserialize, Serialize};

use crate::paths::Paths;
use crate::timefmt::now_ms;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub created_at_ms: i64,
}

pub fn load_or_create(paths: &Paths) -> anyhow::Result<Device> {
    let file = paths.device_file();
    if let Ok(bytes) = fs::read(&file)
        && let Ok(device) = serde_json::from_slice(&bytes)
    {
        return Ok(device);
    }
    let device = Device {
        id: uuid::Uuid::new_v4().to_string(),
        name: hostname(),
        created_at_ms: now_ms(),
    };
    fs::create_dir_all(&paths.home)?;
    let tmp = file.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&device)?)?;
    fs::rename(&tmp, &file)?;
    Ok(device)
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
