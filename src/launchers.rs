//! Games from launchers other than Steam: detection on the sending PC, and making the copied game show up
//! on the receiving PC (automatic for Epic, guided "Locate/Import" steps for launchers that support it).
//!
//! Non-Steam games travel as a plain folder: file paths are relative to the game folder, and the receiver
//! places them in a folder of its choice.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY};

use crate::steam::{Game, Library};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum Launcher {
    Steam,
    Epic,
    Gog,
    Ea,
    Ubisoft,
    BattleNet,
    Rockstar,
    Other,
    /// A Windows program installed for the current user (see apps.rs).
    App,
}

impl Launcher {
    pub const ALL: [Launcher; 9] = [Launcher::Steam, Launcher::Epic, Launcher::Gog, Launcher::Ea, Launcher::Ubisoft, Launcher::BattleNet, Launcher::Rockstar, Launcher::Other, Launcher::App];

    /// Wire id (stored in offers and drive packages).
    pub fn id(self) -> &'static str {
        match self {
            Launcher::Steam => "steam",
            Launcher::Epic => "epic",
            Launcher::Gog => "gog",
            Launcher::Ea => "ea",
            Launcher::Ubisoft => "ubisoft",
            Launcher::BattleNet => "battlenet",
            Launcher::Rockstar => "rockstar",
            Launcher::Other => "folder",
            Launcher::App => "app",
        }
    }

    pub fn from_id(id: &str) -> Launcher {
        Launcher::ALL.into_iter().find(|l| l.id() == id).unwrap_or(if id.is_empty() { Launcher::Steam } else { Launcher::Other })
    }

    pub fn name(self) -> &'static str {
        match self {
            Launcher::Steam => "Steam",
            Launcher::Epic => "Epic Games",
            Launcher::Gog => "GOG",
            Launcher::Ea => "EA app",
            Launcher::Ubisoft => "Ubisoft Connect",
            Launcher::BattleNet => "Battle.net",
            Launcher::Rockstar => "Rockstar",
            Launcher::Other => "Folder",
            Launcher::App => "Apps",
        }
    }

    /// Short label for the badge on a game tile.
    pub fn badge(self) -> &'static str {
        match self {
            Launcher::Steam => "STEAM",
            Launcher::Epic => "EPIC",
            Launcher::Gog => "GOG",
            Launcher::Ea => "EA",
            Launcher::Ubisoft => "UBISOFT",
            Launcher::BattleNet => "BATTLE.NET",
            Launcher::Rockstar => "ROCKSTAR",
            Launcher::Other => "FOLDER",
            Launcher::App => "APP",
        }
    }

    /// Brand-ish colour for the badge (RGB).
    pub fn color(self) -> (u8, u8, u8) {
        match self {
            Launcher::Steam => (23, 26, 33),
            Launcher::Epic => (32, 32, 32),
            Launcher::Gog => (134, 50, 155),
            Launcher::Ea => (255, 71, 71),
            Launcher::Ubisoft => (0, 112, 255),
            Launcher::BattleNet => (20, 140, 255),
            Launcher::Rockstar => (252, 175, 23),
            Launcher::Other => (90, 98, 112),
            Launcher::App => (0, 120, 160),
        }
    }
}

// ---------------- detection ----------------

/// Every installed game we can find, Steam first. Games whose folder is already listed are not repeated.
pub fn all_games() -> Vec<Game> {
    let mut games: Vec<Game> = if crate::steam::root().is_some() { crate::steam::games(&crate::steam::libraries()).into_iter().filter(|g| g.app_id != "228980").collect() } else { vec![] };
    let mut others = Vec::new();
    others.extend(epic_games());
    others.extend(gog_games());
    others.extend(ea_games());
    others.extend(ubisoft_games());
    others.extend(uninstall_games(Launcher::BattleNet, &["blizzard entertainment"], &["battle.net"]));
    others.extend(uninstall_games(Launcher::Rockstar, &["rockstar games"], &["rockstar games launcher", "social club"]));
    others.extend(crate::apps::installed_apps());
    let key = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let mut seen: std::collections::HashSet<String> = games.iter().map(|g| key(&g.install_path())).collect();
    for g in others {
        if g.library.path.is_dir() && seen.insert(key(&g.library.path)) {
            games.push(g);
        }
    }
    games.sort_by_key(|g| g.name.to_lowercase());
    games
}

/// A game that lives in an arbitrary folder (chosen by the user).
pub fn folder_game(folder: &Path) -> Game {
    let name = folder.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| folder.display().to_string());
    generic(Launcher::Other, &format!("folder-{}", short_hash(&folder.to_string_lossy())), &name, folder, 0, Value::Null)
}

fn generic(launcher: Launcher, id: &str, name: &str, folder: &Path, size: u64, meta: Value) -> Game {
    Game {
        app_id: id.to_string(),
        name: name.to_string(),
        install_dir: folder.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        size_on_disk: size,
        state_flags: 4,
        library: Library { path: folder.to_path_buf() },
        launcher,
        meta,
    }
}

fn short_hash(s: &str) -> String {
    let h = s.to_lowercase().bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{:08x}", h as u32)
}

/// Where Epic keeps its install records (overridable for tests).
pub fn epic_data_dir() -> PathBuf {
    std::env::var("STEAMXFER_EPIC_DATA").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("ProgramData").unwrap_or(r"C:\ProgramData".into())).join("Epic")
    })
}

fn epic_manifest_dir() -> PathBuf {
    epic_data_dir().join(r"EpicGamesLauncher\Data\Manifests")
}

fn epic_installed_dat() -> PathBuf {
    epic_data_dir().join(r"UnrealEngineLauncher\LauncherInstalled.dat")
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Epic Games Store: one JSON `.item` manifest per install in ProgramData.
pub fn epic_games() -> Vec<Game> {
    let Ok(rd) = std::fs::read_dir(epic_manifest_dir()) else { return vec![] };
    let items: Vec<Value> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("item")))
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .collect();
    let installed: Value = std::fs::read(epic_installed_dat()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    items
        .iter()
        .filter(|it| !it.get("bIsIncompleteInstall").and_then(Value::as_bool).unwrap_or(false))
        .filter(|it| {
            // DLCs have their own .item but live inside the base game's folder; they travel with it
            let main = s(it, "MainGameAppName");
            main.is_empty() || main == s(it, "AppName")
        })
        .filter(|it| !s(it, "InstallLocation").is_empty())
        .map(|it| {
            let app = s(it, "AppName");
            let dlcs: Vec<Value> = items.iter().filter(|d| s(d, "MainGameAppName") == app && s(d, "AppName") != app).cloned().collect();
            let entry = installed
                .get("InstallationList")
                .and_then(Value::as_array)
                .and_then(|l| l.iter().find(|e| s(e, "AppName") == app).cloned())
                .unwrap_or(Value::Null);
            let name = if s(it, "DisplayName").is_empty() { app } else { s(it, "DisplayName") };
            let size = it.get("InstallSize").and_then(Value::as_u64).unwrap_or(0);
            generic(Launcher::Epic, app, name, Path::new(s(it, "InstallLocation")), size, json!({ "item": it, "dlcs": dlcs, "installed": entry }))
        })
        .collect()
}

fn open(hive: winreg::HKEY, path: &str, wow: u32) -> Option<RegKey> {
    RegKey::predef(hive).open_subkey_with_flags(path, KEY_READ | wow).ok()
}

fn reg_str(k: &RegKey, name: &str) -> Option<String> {
    k.get_value::<String, _>(name).ok().filter(|v| !v.trim().is_empty())
}

/// GOG Galaxy / GOG offline installers register every game under GOG.com\Games.
pub fn gog_games() -> Vec<Game> {
    let Some(root) = open(HKEY_LOCAL_MACHINE, r"SOFTWARE\GOG.com\Games", KEY_WOW64_32KEY) else { return vec![] };
    root.enum_keys()
        .flatten()
        .filter_map(|id| {
            let k = root.open_subkey(&id).ok()?;
            let path = reg_str(&k, "path")?;
            let name = reg_str(&k, "gameName").or_else(|| reg_str(&k, "GAMENAME")).unwrap_or_else(|| id.clone());
            let id = reg_str(&k, "gameID").unwrap_or(id);
            Some(generic(Launcher::Gog, &id, &name, Path::new(&path), 0, json!({ "gameID": id })))
        })
        .collect()
}

/// EA app / Origin: games register under "EA Games" or "Electronic Arts" with an "Install Dir".
pub fn ea_games() -> Vec<Game> {
    let mut out = Vec::new();
    for base in [r"SOFTWARE\EA Games", r"SOFTWARE\Electronic Arts"] {
        let Some(root) = open(HKEY_LOCAL_MACHINE, base, KEY_WOW64_32KEY) else { continue };
        for name in root.enum_keys().flatten() {
            let lower = name.to_lowercase();
            if lower.starts_with("ea desktop") || lower.starts_with("ea core") || lower == "origin" {
                continue;
            }
            let Ok(k) = root.open_subkey(&name) else { continue };
            let Some(dir) = reg_str(&k, "Install Dir").or_else(|| reg_str(&k, "InstallDir")) else { continue };
            let display = reg_str(&k, "DisplayName").unwrap_or(name.clone());
            out.push(generic(Launcher::Ea, &format!("ea-{}", short_hash(&name)), &display, Path::new(dir.trim_end_matches('\\')), 0, Value::Null));
        }
    }
    out
}

/// Ubisoft Connect: Launcher\Installs\<id>\InstallDir; the name comes from its uninstall entry.
pub fn ubisoft_games() -> Vec<Game> {
    let Some(root) = open(HKEY_LOCAL_MACHINE, r"SOFTWARE\Ubisoft\Launcher\Installs", KEY_WOW64_32KEY) else { return vec![] };
    root.enum_keys()
        .flatten()
        .filter_map(|id| {
            let dir = reg_str(&root.open_subkey(&id).ok()?, "InstallDir")?.replace('/', "\\");
            let dir = PathBuf::from(dir.trim_end_matches('\\'));
            let name = uninstall_entries()
                .into_iter()
                .find(|e| e.key.eq_ignore_ascii_case(&format!("Uplay Install {id}")))
                .map(|e| e.name)
                .unwrap_or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(id.clone()));
            Some(generic(Launcher::Ubisoft, &id, &name, &dir, 0, json!({ "installId": id })))
        })
        .collect()
}

struct UninstallEntry {
    key: String,
    name: String,
    publisher: String,
    location: String,
    size_kb: u64,
}

fn uninstall_entries() -> Vec<UninstallEntry> {
    let mut out = Vec::new();
    let path = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
    for (hive, wow) in [(HKEY_LOCAL_MACHINE, KEY_WOW64_64KEY), (HKEY_LOCAL_MACHINE, KEY_WOW64_32KEY), (HKEY_CURRENT_USER, 0)] {
        let Some(root) = open(hive, path, wow) else { continue };
        for key in root.enum_keys().flatten() {
            let Ok(k) = root.open_subkey(&key) else { continue };
            out.push(UninstallEntry {
                name: reg_str(&k, "DisplayName").unwrap_or_default(),
                publisher: reg_str(&k, "Publisher").unwrap_or_default(),
                location: reg_str(&k, "InstallLocation").unwrap_or_default(),
                size_kb: k.get_value::<u32, _>("EstimatedSize").unwrap_or(0) as u64,
                key,
            });
        }
    }
    out
}

/// Launchers without their own install registry (Battle.net, Rockstar): their games' uninstall entries.
fn uninstall_games(launcher: Launcher, publishers: &[&str], exclude_names: &[&str]) -> Vec<Game> {
    uninstall_entries()
        .into_iter()
        .filter(|e| {
            let p = e.publisher.to_lowercase();
            let n = e.name.to_lowercase();
            publishers.iter().any(|x| p.contains(x)) && !e.location.is_empty() && !exclude_names.iter().any(|x| n.contains(x))
        })
        .map(|e| generic(launcher, &format!("{}-{}", launcher.id(), short_hash(&e.key)), &e.name, Path::new(e.location.trim_end_matches('\\')), e.size_kb * 1024, Value::Null))
        .collect()
}

// ---------------- receiving side ----------------

/// Suggested parent folders for a received game: where that launcher's games already live, then C:\Games\<launcher>.
/// Only folders this user can write to (Program Files needs admin, so it's usually skipped).
pub fn destination_parents(launcher: Launcher) -> Vec<PathBuf> {
    let mut parents: Vec<PathBuf> = all_games()
        .into_iter()
        .filter(|g| g.launcher == launcher && launcher != Launcher::Steam)
        .filter_map(|g| g.library.path.parent().map(Path::to_path_buf))
        .collect();
    let system = std::env::var("SystemDrive").unwrap_or("C:".into());
    let folder = match launcher {
        Launcher::Epic => "Epic Games",
        Launcher::Gog => "GOG Games",
        Launcher::Ea => "EA Games",
        Launcher::Ubisoft => "Ubisoft Games",
        Launcher::BattleNet => "Battle.net Games",
        Launcher::Rockstar => "Rockstar Games",
        _ => "Games",
    };
    // other drives first if they already have a Games folder, then the system drive
    for d in crate::hardware::drives() {
        let p = d.root.join("Games");
        if p.is_dir() {
            parents.push(p.join(folder));
        }
    }
    parents.push(PathBuf::from(format!("{system}\\Games")).join(folder));
    let mut seen = std::collections::HashSet::new();
    parents.retain(|p| seen.insert(p.to_string_lossy().to_lowercase()) && writable(p));
    parents
}

/// Can this user create files in `dir` (or in the nearest existing parent)?
pub fn writable(dir: &Path) -> bool {
    let mut probe_dir = dir.to_path_buf();
    while !probe_dir.exists() {
        match probe_dir.parent() {
            Some(p) => probe_dir = p.to_path_buf(),
            None => return false,
        }
    }
    let probe = probe_dir.join(format!(".steamtransfer-{}.tmp", std::process::id()));
    let ok = std::fs::write(&probe, b"x").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// What the user has to do after the files arrive (None = nothing, it's automatic).
pub fn after_copy_steps(launcher: Launcher, name: &str, dest: &Path) -> Option<String> {
    let d = dest.display();
    let parent = dest.parent().map(|p| p.display().to_string()).unwrap_or_default();
    Some(match launcher {
        Launcher::Steam | Launcher::Epic | Launcher::App => return None,
        Launcher::Gog => format!("Open GOG Galaxy → Add games (+) → Scan and import folders → choose \"{parent}\". {name} also runs straight from \"{d}\" (GOG games are DRM-free)."),
        Launcher::Ea => format!("Open the EA app → Library → {name} → Install → \"Locate\" and choose \"{d}\". The EA app checks the files instead of downloading them."),
        Launcher::Ubisoft => format!("Open Ubisoft Connect → {name} → \"Locate installed game\" (under the Download button) → choose \"{d}\"."),
        Launcher::BattleNet => format!("Open Battle.net → {name} → \"Locate the game\" (below the Install button) → choose \"{d}\"."),
        Launcher::Rockstar => format!("Open the Rockstar Games Launcher → Settings → My installed games → {name} → Locate → choose \"{d}\"."),
        Launcher::Other => format!("The files are in \"{d}\". If it belongs to a launcher, use that launcher's \"locate\" or \"import\" option and point it there."),
    })
}

/// Make the received game known to its launcher where that can be done automatically.
/// Epic: rewrite its install manifest(s) and LauncherInstalled.dat for the new folder (Epic must be closed).
pub fn register(launcher: Launcher, meta: &Value, dest: &Path, log: &dyn Fn(String)) -> io::Result<()> {
    if launcher == Launcher::App {
        return crate::apps::finish_receive(meta, log);
    }
    if launcher != Launcher::Epic {
        return Ok(());
    }
    let Some(item) = meta.get("item") else { return Ok(()) };
    if !epic_data_dir().exists() {
        log("Epic Games Launcher isn't installed here; the game was copied but can't be registered with Epic.".into());
        return Ok(());
    }
    let was_running = close_process("EpicGamesLauncher.exe", log);
    let dest_s = dest.to_string_lossy().to_string();
    let relocate = |it: &Value| -> Value {
        let mut it = it.clone();
        if let Some(o) = it.as_object_mut() {
            o.insert("InstallLocation".into(), json!(dest_s));
            o.insert("ManifestLocation".into(), json!(format!("{dest_s}\\.egstore")));
            o.insert("StagingLocation".into(), json!(format!("{dest_s}\\.egstore\\bps")));
            o.insert("bIsIncompleteInstall".into(), json!(false));
        }
        it
    };
    let dir = epic_manifest_dir();
    std::fs::create_dir_all(&dir)?;
    let mut all = vec![item.clone()];
    all.extend(meta.get("dlcs").and_then(Value::as_array).cloned().unwrap_or_default());
    for it in &all {
        let guid = s(it, "InstallationGuid");
        let file = if guid.is_empty() { format!("{}.item", s(it, "AppName")) } else { format!("{guid}.item") };
        if file.contains(['\\', '/', ':']) || file.starts_with('.') {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad Epic manifest name"));
        }
        std::fs::write(dir.join(file), serde_json::to_vec_pretty(&relocate(it))?)?;
    }

    // LauncherInstalled.dat: replace this game's entry
    let dat = epic_installed_dat();
    let mut doc: Value = std::fs::read(&dat).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| json!({ "InstallationList": [] }));
    let app = s(item, "AppName").to_string();
    let mut entry = meta.get("installed").cloned().filter(|e| e.is_object()).unwrap_or_else(|| {
        json!({
            "InstallLocation": "",
            "NamespaceId": s(item, "CatalogNamespace"),
            "ItemId": s(item, "CatalogItemId"),
            "ArtifactId": app,
            "AppVersion": s(item, "AppVersionString"),
            "AppName": app,
        })
    });
    entry["InstallLocation"] = json!(dest_s);
    if !doc.get("InstallationList").is_some_and(Value::is_array) {
        doc["InstallationList"] = json!([]);
    }
    let list = doc["InstallationList"].as_array_mut().unwrap();
    list.retain(|e| s(e, "AppName") != app);
    list.push(entry);
    if let Some(parent) = dat.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dat, serde_json::to_vec_pretty(&doc)?)?;
    log("Registered the game with the Epic Games Launcher.".into());
    if was_running {
        let _ = std::process::Command::new("cmd").args(["/c", "start", "", "com.epicgames.launcher://"]).spawn();
    }
    Ok(())
}

/// Ends a launcher process so it doesn't overwrite what we write. Returns whether it was running.
fn close_process(exe: &str, log: &dyn Fn(String)) -> bool {
    let Ok(mut sys) = sysinfo::System::new() else { return false };
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let procs: Vec<_> = sys.processes_by_exact_name(exe.as_ref()).collect();
    if procs.is_empty() {
        return false;
    }
    log(format!("Closing {} to register the game...", exe.trim_end_matches(".exe")));
    for p in procs {
        p.kill();
    }
    std::thread::sleep(std::time::Duration::from_millis(1500));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launcher_ids_roundtrip() {
        for l in Launcher::ALL {
            assert_eq!(Launcher::from_id(l.id()), l);
        }
        assert_eq!(Launcher::from_id(""), Launcher::Steam); // offers from older versions
        assert_eq!(Launcher::from_id("something-new"), Launcher::Other);
    }
}
