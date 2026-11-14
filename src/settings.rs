//! User settings, saved as JSON next to the app's other per-user data (%LocalAppData%\SteamTransfer).

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::util;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Whenever a game is copied to or installed from a drive, make sure a working copy of the app is on that drive.
    pub keep_app_on_drives: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { keep_app_on_drives: true }
    }
}

fn path() -> PathBuf {
    util::local_app_data().join("SteamTransfer").join("settings.json")
}

pub fn load() -> Settings {
    std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save(s: &Settings) -> io::Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    std::fs::rename(tmp, p)
}
