//! Finds Steam, its library folders, installed games and cached cover art.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

use crate::util::{self, no_window};
use crate::vdf::{self, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct Library {
    pub path: PathBuf,
}

impl Library {
    pub fn steamapps(&self) -> PathBuf {
        self.path.join("steamapps")
    }
    pub fn free_bytes(&self) -> u64 {
        util::free_space(&self.path)
    }
}

/// An installed game from any launcher. For Steam, `library` is the Steam library and paths in an offer are
/// relative to it (`steamapps/...`); for every other launcher `library.path` is the game's own folder.
#[derive(Clone, Debug)]
pub struct Game {
    pub app_id: String,
    pub name: String,
    pub install_dir: String,
    pub size_on_disk: u64,
    pub state_flags: u32,
    pub library: Library,
    pub launcher: crate::launchers::Launcher,
    /// Launcher-specific install records needed to register the game on the other PC (e.g. Epic's manifest).
    pub meta: serde_json::Value,
}

impl Game {
    pub fn fully_installed(&self) -> bool {
        self.state_flags == 4
    }
    pub fn is_steam(&self) -> bool {
        self.launcher == crate::launchers::Launcher::Steam
    }
    /// The folder holding the game's files.
    pub fn install_path(&self) -> PathBuf {
        if self.is_steam() { self.library.steamapps().join("common").join(&self.install_dir) } else { self.library.path.clone() }
    }
}

pub fn root() -> Option<PathBuf> {
    let read = |hive, key: &str, value: &str| -> Option<String> { RegKey::predef(hive).open_subkey(key).ok()?.get_value(value).ok() };
    let p = read(HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath")
        .or_else(|| read(HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Valve\Steam", "InstallPath"))
        .or_else(|| read(HKEY_LOCAL_MACHINE, r"SOFTWARE\Valve\Steam", "InstallPath"))?;
    let p = PathBuf::from(p.replace('/', "\\"));
    p.is_dir().then(|| std::path::absolute(&p).unwrap_or(p))
}

pub fn exe() -> Option<PathBuf> {
    let e = root()?.join("steam.exe");
    e.is_file().then_some(e)
}

pub fn libraries() -> Vec<Library> {
    let mut out: Vec<Library> = Vec::new();
    let mut add = |p: &str| {
        let p = PathBuf::from(p.replace('/', "\\"));
        let p = std::path::absolute(&p).unwrap_or(p);
        let s = p.to_string_lossy().trim_end_matches('\\').to_string();
        let p = PathBuf::from(s);
        if p.is_dir() && !out.iter().any(|l| l.path.to_string_lossy().eq_ignore_ascii_case(&p.to_string_lossy())) {
            out.push(Library { path: p });
        }
    };
    let Some(root) = root() else { return out };
    add(&root.to_string_lossy());
    if let Ok(text) = std::fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")) {
        if let Some(Value::Obj(libs)) = vdf::parse(&text).get("libraryfolders") {
            for v in libs.values() {
                match v {
                    Value::Obj(lib) => {
                        if let Some(Value::Str(p)) = lib.get("path") {
                            add(p)
                        }
                    }
                    Value::Str(s) if s.contains('\\') || s.contains('/') => add(s), // legacy format
                    _ => {}
                }
            }
        }
    }
    out
}

pub fn games(libs: &[Library]) -> Vec<Game> {
    let mut games = Vec::new();
    for lib in libs {
        let Ok(rd) = std::fs::read_dir(lib.steamapps()) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !(name.starts_with("appmanifest_") && name.ends_with(".acf")) {
                continue;
            }
            if let Some(g) = read_manifest(&e.path(), lib) {
                games.push(g);
            }
        }
    }
    games.sort_by_key(|g| g.name.to_lowercase());
    games
}

fn read_manifest(acf: &Path, lib: &Library) -> Option<Game> {
    let text = std::fs::read_to_string(acf).ok()?;
    let root = vdf::parse(&text);
    let Value::Obj(st) = root.get("AppState")? else { return None };
    let s = |k: &str| match st.get(k) {
        Some(Value::Str(v)) => Some(v.clone()),
        _ => None,
    };
    let app_id = s("appid")?;
    let install_dir = s("installdir")?;
    Some(Game {
        name: s("name").unwrap_or_else(|| install_dir.clone()),
        size_on_disk: s("SizeOnDisk").and_then(|v| v.parse().ok()).unwrap_or(0),
        state_flags: s("StateFlags").and_then(|v| v.parse().ok()).unwrap_or(0),
        app_id,
        install_dir,
        library: lib.clone(),
        launcher: crate::launchers::Launcher::Steam,
        meta: serde_json::Value::Null,
    })
}

pub fn find_installed(app_id: &str, libs: &[Library]) -> Option<Library> {
    libs.iter().find(|l| l.steamapps().join(format!("appmanifest_{app_id}.acf")).is_file()).cloned()
}

/// Steam stores the sender's steam.exe path in the manifest; point it at ours.
pub fn fix_manifest(acf: &Path) {
    let (Some(exe), Ok(text)) = (exe(), std::fs::read_to_string(acf)) else { return };
    let escaped = exe.to_string_lossy().replace('\\', "\\\\");
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    for line in text.split_inclusive('\n') {
        let t = line.trim_start();
        if t.to_ascii_lowercase().starts_with("\"launcherpath\"") {
            let indent = &line[..line.len() - t.len()];
            let nl = if line.ends_with("\r\n") { "\r\n" } else if line.ends_with('\n') { "\n" } else { "" };
            out.push_str(&format!("{indent}\"LauncherPath\"\t\t\"{escaped}\"{nl}"));
            changed = true;
        } else {
            out.push_str(line);
        }
    }
    if changed {
        let _ = std::fs::write(acf, out);
    }
}

// ---------------- running Steam ----------------

pub fn is_running() -> bool {
    let Ok(mut sys) = sysinfo::System::new() else { return false };
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    sys.processes_by_exact_name("steam.exe".as_ref()).next().is_some()
}

pub fn shutdown(log: &dyn Fn(String)) {
    if !is_running() {
        return;
    }
    log("Closing Steam...".into());
    if let Some(exe) = exe() {
        let _ = no_window(Command::new(exe).arg("-shutdown")).spawn();
    }
    let start = Instant::now();
    while is_running() && start.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(500));
    }
    if is_running() {
        log("Steam didn't close in time; ending it.".into());
        if let Ok(mut sys) = sysinfo::System::new() {
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        for p in sys.processes_by_exact_name("steam.exe".as_ref()) {
            p.kill();
        }
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
}

pub fn restart(log: &dyn Fn(String)) {
    shutdown(log);
    match exe() {
        Some(exe) => {
            log("Starting Steam...".into());
            let _ = Command::new(exe).spawn();
        }
        None => log("Steam not found.".into()),
    }
}

// ---------------- cover art ----------------

static ART_CACHE: Mutex<Option<HashMap<String, Option<PathBuf>>>> = Mutex::new(None);

/// Cached library artwork. kind: "capsule" (portrait), "hero" (wide banner).
pub fn find_art(app_id: &str, kind: &str) -> Option<PathBuf> {
    if !app_id.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let key = format!("{app_id}/{kind}");
    if let Some(hit) = ART_CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        return hit.clone();
    }
    let names: &[&str] = match kind {
        "capsule" => &["library_600x900_2x.jpg", "library_600x900.jpg", "library_capsule.jpg"],
        _ => &["library_hero.jpg", "library_header.jpg", "header.jpg"],
    };
    let found = root().and_then(|root| {
        let cache = root.join("appcache").join("librarycache");
        // old flat layout: <appid>_library_600x900.jpg
        for n in names {
            let p = cache.join(format!("{app_id}_{n}"));
            if p.is_file() {
                return Some(p);
            }
        }
        // new layout: <appid>/<hash>/library_capsule.jpg
        let files: Vec<PathBuf> = walkdir::WalkDir::new(cache.join(app_id)).into_iter().flatten().filter(|e| e.file_type().is_file()).map(|e| e.into_path()).collect();
        names.iter().find_map(|n| files.iter().find(|f| f.file_name().is_some_and(|x| x.to_string_lossy().eq_ignore_ascii_case(n))).cloned())
    });
    ART_CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, found.clone());
    found
}
