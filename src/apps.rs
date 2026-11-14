//! Windows apps (not just games): programs installed for the current user — the kind that live under
//! %LocalAppData% / %AppData% (VS Code user setup, Discord, Spotify, Slack, Zoom, Obsidian, per-user Chrome, ...).
//!
//! An app is more than its folder, so it travels as a *staging folder* made of NTFS junctions:
//!
//! ```text
//! %LocalAppData%\SteamTransfer\staging\<id>\
//!     install\        → the program folder          (e.g. AppData\Local\Programs\Microsoft VS Code)
//!     roaming\<name>\ → its settings in %AppData%     (e.g. AppData\Roaming\Code)
//!     local\<name>\   → its data in %LocalAppData%
//! ```
//!
//! The transfer engine just sees one folder. The receiver builds the same junctions pointing at *its own* user
//! profile, so files land straight in their final place (and resume works). Its registry settings (HKCU only) and
//! Start-menu / desktop shortcuts travel in the offer's metadata and are re-created with the user-profile path
//! rewritten. Junctions need no admin rights.
//!
//! Machine-wide installs (Program Files, HKLM, services, drivers) need administrator rights and are not listed yet.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

use crate::launchers::Launcher;
use crate::steam::{Game, Library};
use crate::transfer::Offer;
use crate::util::{self, no_window};

const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";
/// Our own uninstall entry.
const SELF_KEY: &str = "SteamGameTransfer";
/// Registry exports bigger than this stay behind (the app still works, it just starts with default settings).
const MAX_REGISTRY: usize = 4 * 1024 * 1024;

/// Folder names that are caches: rebuilt by the app on first start, often huge, never worth copying.
pub const CACHE_DIRS: &[&str] =
    &["cache", "code cache", "gpucache", "dawncache", "dawngraphitecache", "dawnwebgpucache", "grshadercache", "graphitedawncache", "shadercache", "cachedata", "cacheddata", "crashpad", "crashdumps"];

/// Top-level AppData / HKCU\Software names that belong to Windows or to many apps at once — never taken as an app's own.
const SHARED_NAMES: &[&str] = &[
    "microsoft", "programs", "packages", "temp", "steamtransfer", "windows", "classes", "policies", "wow6432node", "clients", "registeredapplications",
    "appdatalow", "connecteddevicesplatform", "comms", "d3dscache", "virtualstore", "publishers", "crashdumps", "low", "applications", "intel", "nvidia",
    "nvidia corporation", "amd", "ati", "apple", "apple computer, inc.", "adobe", "google", "mozilla", "jetbrains", "python", "pip", "npm", "npm-cache",
];

/// The user profile folder. STEAMXFER_PROFILE stands in for it in tests (one process plays two different users).
pub fn profile() -> PathBuf {
    std::env::var("STEAMXFER_PROFILE").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()))
}
fn overridden() -> bool {
    std::env::var_os("STEAMXFER_PROFILE").is_some()
}
fn roaming() -> PathBuf {
    if overridden() { profile().join(r"AppData\Roaming") } else { PathBuf::from(std::env::var("APPDATA").unwrap_or_default()) }
}
fn local() -> PathBuf {
    if overridden() { profile().join(r"AppData\Local") } else { util::local_app_data() }
}

/// Data folders that aren't named after their app: (program folder name, lowercase) → (base, folder).
const KNOWN_DATA: &[(&str, &[(&str, &str)])] = &[
    ("microsoft vs code", &[("roaming", "Code"), ("home", ".vscode")]),
    ("cursor", &[("roaming", "Cursor"), ("home", ".cursor")]),
    ("windsurf", &[("roaming", "Windsurf"), ("home", ".windsurf")]),
    ("vscodium", &[("roaming", "VSCodium"), ("home", ".vscode-oss")]),
];

/// Where a data folder's `base` is in this user's profile: roaming = %AppData%, local = %LocalAppData%,
/// home = the profile folder itself (only for dot-folders such as .vscode).
fn base_dir(base: &str) -> PathBuf {
    match base {
        "roaming" => roaming(),
        "home" => profile(),
        _ => local(),
    }
}

/// Dot-folders in the profile that hold secrets or belong to many tools — never taken along.
const HOME_NEVER: &[&str] = &[".ssh", ".aws", ".azure", ".gnupg", ".kube", ".docker", ".config", ".local", ".cache", ".git", ".gitconfig", ".npmrc", ".m2", ".gradle", ".cargo", ".rustup", ".nuget"];

fn home_rel_ok(rel: &str) -> bool {
    let l = rel.to_lowercase();
    l.starts_with('.') && l.len() > 1 && !l.contains(['\\', '/', ':']) && l != "." && l != ".." && !HOME_NEVER.contains(&l.as_str())
}

fn lower(p: &Path) -> String {
    p.to_string_lossy().trim_end_matches('\\').to_lowercase()
}
/// `p` relative to `base` (case-insensitive), if it is inside it.
fn rel_to(base: &Path, p: &Path) -> Option<String> {
    let (b, s) = (lower(base), lower(p));
    if b.is_empty() || !s.starts_with(&format!("{b}\\")) {
        return None;
    }
    Some(p.to_string_lossy().trim_end_matches('\\')[b.len() + 1..].to_string())
}
fn is_inside(child: &Path, parent: &Path) -> bool {
    lower(child) == lower(parent) || rel_to(parent, child).is_some()
}

fn reg_str(k: &RegKey, name: &str) -> Option<String> {
    k.get_value::<String, _>(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// The executable named in an uninstall string or icon path: `"C:\x\y.exe" --uninstall`, `C:\x\y.exe,0`.
fn exe_in(cmd: &str) -> Option<PathBuf> {
    let c = cmd.trim();
    let path = if let Some(rest) = c.strip_prefix('"') {
        rest.split('"').next()?.to_string()
    } else {
        let l = c.to_lowercase();
        let end = l.find(".exe").map(|i| i + 4).unwrap_or_else(|| l.find(',').unwrap_or(l.len()));
        c[..end].to_string()
    };
    let p = PathBuf::from(path.trim());
    p.is_absolute().then_some(p)
}

/// "Foo 1.2.3" / "Foo (x64)" / "Foo version 2" → "Foo".
fn base_name(name: &str) -> String {
    // "Roblox Player for alice" → "Roblox Player"
    let user = std::env::var("USERNAME").unwrap_or_default();
    let name = match name.rsplit_once(" for ") {
        Some((head, who)) if !user.is_empty() && who.trim().eq_ignore_ascii_case(&user) => head,
        _ => name,
    };
    let mut words: Vec<&str> = name.split_whitespace().collect();
    while let Some(last) = words.last() {
        let w = last.trim_matches(|c| c == '(' || c == ')').to_lowercase();
        let version = w.trim_start_matches('v').chars().all(|c| c.is_ascii_digit() || c == '.') && w.chars().any(|c| c.is_ascii_digit());
        if words.len() > 1 && (version || ["x64", "x86", "64-bit", "32-bit", "version", "(user)", "user", "-"].contains(&w.as_str())) {
            words.pop();
        } else {
            break;
        }
    }
    words.join(" ")
}

/// "Google LLC" → "Google", "Spotify AB" → "Spotify", "Discord Inc." → "Discord".
fn publisher_name(p: &str) -> String {
    let mut s = p.trim().trim_end_matches('.').to_string();
    for suffix in [", inc", " inc", " llc", " ltd", " limited", " gmbh", " corporation", " corp", " ab", " co", " s.a", " b.v", " pty"] {
        if s.to_lowercase().ends_with(suffix) {
            s.truncate(s.len() - suffix.len());
            s = s.trim().trim_end_matches(',').trim_end_matches('.').trim().to_string();
        }
    }
    s
}

fn is_shared(rel: &str) -> bool {
    let first = rel.split(['\\', '/']).next().unwrap_or("").to_lowercase();
    first.is_empty() || SHARED_NAMES.contains(&first.as_str())
}

/// Where the app lives inside the profile must be AppData\Local\… or AppData\Roaming\…, at least one folder below
/// the shared top level, and not a Windows-owned folder. (Also what a receiver accepts from a peer.)
fn install_rel_ok(rel: &str) -> bool {
    let parts: Vec<String> = rel.split(['\\', '/']).map(|s| s.to_lowercase()).collect();
    if parts.iter().any(|s| s.is_empty() || s == "." || s == ".." || s.contains(':')) || parts.len() < 3 {
        return false;
    }
    if parts[0] != "appdata" || !(parts[1] == "local" || parts[1] == "roaming") {
        return false;
    }
    // Windows' own folders: never
    const NEVER: &[&str] = &["microsoft", "temp", "packages", "steamtransfer", "virtualstore", "crashdumps", "d3dscache", "connecteddevicesplatform", "comms", "low"];
    if NEVER.contains(&parts[2].as_str()) {
        return false;
    }
    if parts[2] == "programs" {
        // AppData\Local\Programs\<App>: the standard per-user location
        return parts.len() >= 4 && parts[3] != "common";
    }
    // a vendor folder shared by several apps (Google, Adobe, ...) only one level down: Local\Google\Chrome
    !SHARED_NAMES.contains(&parts[2].as_str()) || parts.len() >= 4
}

/// A data folder name under %AppData% / %LocalAppData%: one or two plain segments, not a shared folder.
fn data_rel_ok(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split(['\\', '/']).collect();
    !parts.is_empty() && parts.len() <= 2 && parts.iter().all(|s| !s.is_empty() && *s != "." && *s != ".." && !s.contains(':')) && !is_shared(rel)
}

// ---------------- what's installed ----------------

/// Programs installed for this user (HKCU uninstall entries whose folder is inside the user profile).
pub fn installed_apps() -> Vec<Game> {
    let Ok(root) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(UNINSTALL, KEY_READ) else { return vec![] };
    let prof = profile();
    let mut out: Vec<Game> = Vec::new();
    for key in root.enum_keys().flatten() {
        if key.eq_ignore_ascii_case(SELF_KEY) {
            continue;
        }
        let Ok(k) = root.open_subkey(&key) else { continue };
        if k.get_value::<u32, _>("SystemComponent").unwrap_or(0) == 1 || reg_str(&k, "ParentKeyName").is_some() {
            continue;
        }
        if reg_str(&k, "ReleaseType").is_some_and(|t| t.to_lowercase().contains("update") || t.to_lowercase().contains("hotfix")) {
            continue;
        }
        let Some(name) = reg_str(&k, "DisplayName") else { continue };
        let candidates = [reg_str(&k, "InstallLocation").map(PathBuf::from), reg_str(&k, "DisplayIcon").and_then(|s| exe_in(&s)).and_then(|p| p.parent().map(Path::to_path_buf)), reg_str(&k, "UninstallString").and_then(|s| exe_in(&s)).and_then(|p| p.parent().map(Path::to_path_buf))];
        let Some(mut install) = candidates.into_iter().flatten().map(|p| PathBuf::from(p.to_string_lossy().trim_end_matches('\\'))).find(|p| p.is_dir()) else { continue };
        let file_name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        // Chromium browsers: ...\Brave-Browser\Application → the browser folder (its profile is in "User Data" next to it)
        if file_name(&install) == "application" {
            if let Some(p) = install.parent() {
                install = p.to_path_buf();
            }
        }
        // one version folder of an app that keeps several (Roblox: ...\Roblox\Versions\version-abc) → the app itself
        if install.parent().is_some_and(|p| file_name(p) == "versions") {
            if let Some(p) = install.parent().and_then(Path::parent) {
                install = p.to_path_buf();
            }
        }
        // installer caches, not the program
        let l = lower(&install);
        if l.contains(r"\installer") || l.contains('{') {
            continue;
        }
        let publisher = reg_str(&k, "Publisher").unwrap_or_default();

        // Its settings/data folders: %AppData%\<name> or %AppData%\<publisher>\<name> (and the same in Local).
        let folder = install.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let base = base_name(&name);
        let pubn = publisher_name(&publisher);
        let mut names: Vec<String> = vec![folder.clone(), name.clone(), base.clone(), base.replace(' ', "")];
        if !pubn.is_empty() {
            names.extend([format!("{pubn}\\{base}"), format!("{pubn}\\{folder}"), format!("{pubn}\\{}", base.replace(' ', ""))]);
        }
        let mut data: Vec<(&'static str, String)> = Vec::new();
        // apps whose data folders aren't named after them
        for (folder_l, extra) in KNOWN_DATA {
            if file_name(&install) == *folder_l {
                for (kind, rel) in *extra {
                    if base_dir(kind).join(rel).is_dir() {
                        data.push((*kind, rel.to_string()));
                    }
                }
            }
        }
        for (kind, dir) in [("roaming", roaming()), ("local", local())] {
            for n in &names {
                if n.trim().is_empty() || !data_rel_ok(n) {
                    continue;
                }
                let p = dir.join(n);
                if !p.is_dir() || is_inside(&p, &install) {
                    continue; // missing, or already part of the program folder
                }
                if rel_to(&p, &install).is_some() {
                    // the program sits inside its own data folder (e.g. Local\Google\Chrome\Application): take the whole thing
                    install = p.clone();
                    continue;
                }
                let rel = rel_to(&dir, &p).unwrap_or_default();
                if !data.iter().any(|(k2, r)| *k2 == kind && (r.eq_ignore_ascii_case(&rel) || is_inside(&dir.join(&rel), &dir.join(r)))) {
                    data.retain(|(k2, r)| !(*k2 == kind && is_inside(&dir.join(r), &dir.join(&rel))));
                    data.push((kind, rel));
                }
            }
        }
        let Some(install_rel) = rel_to(&prof, &install) else { continue };
        if !install_rel_ok(&install_rel) {
            continue;
        }
        data.retain(|(kind, rel)| !is_inside(&base_dir(kind).join(rel), &install));
        let id = format!("app-{}", short_hash(&key));
        let meta = json!({
            "app": {
                "key": key,
                "install": install_rel,
                "data": data.iter().map(|(k, r)| json!({ "base": k, "path": r })).collect::<Vec<_>>(),
                "profile": prof.to_string_lossy(),
                "publisher": publisher,
            }
        });
        let size = k.get_value::<u32, _>("EstimatedSize").unwrap_or(0) as u64 * 1024;
        if out.iter().any(|g| lower(&g.library.path) == lower(&install)) {
            continue; // two uninstall entries for one folder
        }
        out.push(Game {
            app_id: id,
            name: base_name(&name),
            install_dir: folder,
            size_on_disk: size,
            state_flags: 4,
            library: Library { path: install },
            launcher: Launcher::App,
            meta,
        });
    }
    out
}

fn short_hash(s: &str) -> String {
    let h = s.to_lowercase().bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{:08x}", h as u32)
}

// ---------------- the plan carried in an offer ----------------

/// An app's pieces, validated (a peer's metadata is never trusted as-is).
#[derive(Debug, Clone)]
pub struct Plan {
    pub key: String,
    /// Profile-relative program folder.
    pub install: String,
    /// ("roaming" | "local", folder name).
    pub data: Vec<(String, String)>,
    /// The sending user's profile folder (to rewrite paths in registry values and shortcuts).
    pub from_profile: String,
    pub registry: String,
    pub shortcuts: Vec<Shortcut>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct Shortcut {
    /// Profile-relative .lnk path.
    pub lnk: String,
    /// Profile-relative target (always inside the program folder).
    pub target: String,
    pub args: String,
    /// Profile-relative, or empty.
    pub workdir: String,
    /// Profile-relative icon file (or absolute system path), and the icon index.
    pub icon: String,
    pub icon_index: i32,
    pub desc: String,
}

pub fn plan(meta: &Value) -> io::Result<Plan> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, format!("This app can't be installed: {m}"));
    let a = meta.get("app").ok_or_else(|| bad("missing details"))?;
    let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let install = s("install").replace('/', "\\");
    if !install_rel_ok(&install) {
        return Err(bad(&format!("unexpected program folder \"{install}\"")));
    }
    let key = s("key");
    if key.is_empty() || key.contains(['\\', '/']) || key.eq_ignore_ascii_case(SELF_KEY) {
        return Err(bad("bad uninstall entry name"));
    }
    let mut data = Vec::new();
    for d in a.get("data").and_then(Value::as_array).into_iter().flatten() {
        let base = d.get("base").and_then(Value::as_str).unwrap_or("");
        let path = d.get("path").and_then(Value::as_str).unwrap_or("").replace('/', "\\");
        if ((base == "roaming" || base == "local") && data_rel_ok(&path)) || (base == "home" && home_rel_ok(&path)) {
            data.push((base.to_string(), path));
        }
    }
    let shortcuts = a.get("shortcuts").and_then(|v| serde_json::from_value::<Vec<Shortcut>>(v.clone()).ok()).unwrap_or_default();
    let shortcuts = shortcuts.into_iter().filter(|sc| shortcut_ok(sc, &install)).collect();
    Ok(Plan { key, install, data, from_profile: s("profile"), registry: s("registry"), shortcuts })
}

fn rel_safe(rel: &str) -> bool {
    !rel.is_empty() && !rel.split(['\\', '/']).any(|s| s.is_empty() || s == "." || s == ".." || s.contains(':'))
}

/// Start menu (not Startup) or desktop, pointing into the program folder.
fn shortcut_ok(sc: &Shortcut, install: &str) -> bool {
    let l = sc.lnk.to_lowercase().replace('/', "\\");
    let in_start = l.starts_with(r"appdata\roaming\microsoft\windows\start menu\programs\") && !l.contains(r"\startup\");
    let parent = Path::new(&l).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let on_desktop = parent == "desktop" || parent == r"onedrive\desktop";
    let t = sc.target.to_lowercase().replace('/', "\\");
    rel_safe(&sc.lnk) && l.ends_with(".lnk") && (in_start || on_desktop) && rel_safe(&sc.target) && t.starts_with(&format!("{}\\", install.to_lowercase()))
}

/// Where the app will be installed on this PC.
pub fn install_target(meta: &Value) -> io::Result<PathBuf> {
    Ok(profile().join(plan(meta)?.install))
}

// ---------------- junctions ----------------

/// Creates an NTFS junction `link` → `target` (an empty folder is created for `link`).
pub fn junction(link: &Path, target: &Path) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, OPEN_EXISTING};
    use windows_sys::Win32::System::IO::DeviceIoControl;
    const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;

    let target = std::path::absolute(target)?;
    let print: Vec<u16> = target.to_string_lossy().trim_end_matches('\\').encode_utf16().collect();
    let subst: Vec<u16> = format!(r"\??\{}", target.to_string_lossy().trim_end_matches('\\')).encode_utf16().collect();
    let mut path_buf: Vec<u16> = Vec::new();
    path_buf.extend(&subst);
    path_buf.push(0);
    path_buf.extend(&print);
    path_buf.push(0);
    let data_len = 8 + path_buf.len() * 2;
    let mut buf: Vec<u8> = Vec::with_capacity(8 + data_len);
    buf.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buf.extend((data_len as u16).to_le_bytes());
    buf.extend(0u16.to_le_bytes());
    buf.extend(0u16.to_le_bytes()); // substitute name offset
    buf.extend(((subst.len() * 2) as u16).to_le_bytes());
    buf.extend((((subst.len() + 1) * 2) as u16).to_le_bytes()); // print name offset
    buf.extend(((print.len() * 2) as u16).to_le_bytes());
    for c in path_buf {
        buf.extend(c.to_le_bytes());
    }

    std::fs::create_dir(link)?;
    let w: Vec<u16> = link.as_os_str().encode_wide_nul();
    let ok = unsafe {
        let h = CreateFileW(w.as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, std::ptr::null_mut());
        if h == INVALID_HANDLE_VALUE {
            false
        } else {
            let mut ret = 0u32;
            let r = DeviceIoControl(h, FSCTL_SET_REPARSE_POINT, buf.as_ptr().cast(), buf.len() as u32, std::ptr::null_mut(), 0, &mut ret, std::ptr::null_mut());
            CloseHandle(h);
            r != 0
        }
    };
    if !ok {
        let e = io::Error::last_os_error();
        let _ = std::fs::remove_dir(link);
        return Err(e);
    }
    Ok(())
}

trait WideNul {
    fn encode_wide_nul(&self) -> Vec<u16>;
}
impl WideNul for std::ffi::OsStr {
    fn encode_wide_nul(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain(Some(0)).collect()
    }
}

fn is_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink() || is_reparse(&m))
}
fn is_reparse(m: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
}

/// A staging folder of junctions; removed (links only — never what they point at) when dropped.
pub struct Staging {
    pub root: PathBuf,
}

impl Staging {
    fn new(name: &str) -> io::Result<Staging> {
        let root = local().join("SteamTransfer").join("staging").join(name);
        let s = Staging { root };
        s.clear();
        std::fs::create_dir_all(&s.root)?;
        Ok(s)
    }

    fn link(&self, rel: &str, target: &Path) -> io::Result<()> {
        let link = self.root.join(rel);
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(target)?;
        junction(&link, target)
    }

    /// Unlinks every junction first, then removes the (now link-free) staging folders.
    pub fn clear(&self) {
        fn unlink(dir: &Path) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if is_link(&p) {
                    let _ = std::fs::remove_dir(&p); // removes the junction itself, not its target
                } else if p.is_dir() {
                    unlink(&p);
                }
            }
        }
        if !self.root.exists() {
            return;
        }
        unlink(&self.root);
        // only plain folders/files are left now (anything a peer sent outside the known links)
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        self.clear();
    }
}

fn staging_name(id: &str, dir: &str) -> String {
    let id: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(40).collect();
    format!("{id}-{dir}")
}

// ---------------- sending ----------------

/// Gets an app ready to send or copy to a drive: closes it if it's running (asks first), builds the junction
/// staging folder and collects its registry settings and shortcuts. Returns a `Game` whose folder is the staging
/// folder; keep the `Staging` alive until the copy is done.
pub fn prepare_send(g: &Game, log: &dyn Fn(String)) -> io::Result<(Game, Staging)> {
    let p = plan(&g.meta)?;
    let prof = profile();
    let install = prof.join(&p.install);
    let data_dirs: Vec<PathBuf> = p.data.iter().map(|(b, r)| base_dir(b).join(r)).collect();
    let mut dirs = vec![install.clone()];
    dirs.extend(data_dirs.iter().cloned());
    ensure_closed(&g.name, &dirs, log)?;

    let st = Staging::new(&staging_name(&g.app_id, "out"))?;
    st.link("install", &install)?;
    for ((base, rel), dir) in p.data.iter().zip(&data_dirs) {
        st.link(&format!("{base}\\{rel}"), dir)?;
    }

    let registry = export_registry(&p.key, g, log);
    let shortcuts = find_shortcuts(&install, &prof);
    if !shortcuts.is_empty() {
        log(format!("{} shortcut{} will be re-created.", shortcuts.len(), if shortcuts.len() == 1 { "" } else { "s" }));
    }
    let mut meta = g.meta.clone();
    meta["app"]["registry"] = json!(registry);
    meta["app"]["shortcuts"] = json!(shortcuts);
    meta["app"]["profile"] = json!(prof.to_string_lossy());
    let mut staged = g.clone();
    staged.library = Library { path: st.root.clone() };
    staged.meta = meta;
    Ok((staged, st))
}

/// HKCU registry keys that belong to the app, exported as one .reg text.
fn export_registry(key: &str, g: &Game, log: &dyn Fn(String)) -> String {
    let pubn = g.meta["app"]["publisher"].as_str().map(publisher_name).unwrap_or_default();
    let base = base_name(&g.name);
    let mut keys = vec![format!(r"HKCU\{UNINSTALL}\{key}")];
    let mut names = vec![g.name.clone(), base.clone(), g.install_dir.clone(), base.replace(' ', "")];
    if !pubn.is_empty() {
        names.extend([format!("{pubn}\\{base}"), format!("{pubn}\\{}", g.install_dir)]);
    }
    let sw = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags("Software", KEY_READ).ok();
    for n in names {
        if n.trim().is_empty() || is_shared(&n) || !data_rel_ok(&n) {
            continue;
        }
        if sw.as_ref().is_some_and(|sw| sw.open_subkey_with_flags(&n, KEY_READ).is_ok()) {
            let k = format!(r"HKCU\Software\{n}");
            if !keys.iter().any(|x| x.eq_ignore_ascii_case(&k) || x.to_lowercase().starts_with(&format!("{}\\", k.to_lowercase()))) {
                keys.push(k);
            }
        }
    }
    let tmp = std::env::temp_dir().join(format!("steamtransfer-{}.reg", std::process::id()));
    let mut out = String::from("Windows Registry Editor Version 5.00\r\n");
    for k in &keys {
        let _ = std::fs::remove_file(&tmp);
        let ok = no_window(std::process::Command::new("reg").args(["export", k, &tmp.to_string_lossy(), "/y"])).output().is_ok_and(|o| o.status.success());
        if !ok {
            continue;
        }
        let Some(text) = std::fs::read(&tmp).ok().map(|b| decode_reg(&b)) else { continue };
        for line in text.lines().skip_while(|l| !l.starts_with('[')) {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    let _ = std::fs::remove_file(&tmp);
    if out.len() > MAX_REGISTRY {
        log(format!("{}'s registry settings are too big to bring along ({}); it will start with default settings.", g.name, util::size(out.len() as u64)));
        return String::new();
    }
    out
}

/// .reg files from `reg export` are UTF-16LE with a BOM.
fn decode_reg(b: &[u8]) -> String {
    if b.len() >= 2 && b[0] == 0xFF && b[1] == 0xFE {
        let w: Vec<u16> = b[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&w)
    } else {
        String::from_utf8_lossy(b).into_owned()
    }
}

/// Start-menu and desktop shortcuts of this user that point into `install`.
fn find_shortcuts(install: &Path, prof: &Path) -> Vec<Shortcut> {
    let ps = r#"$ErrorActionPreference='SilentlyContinue';$sh=New-Object -ComObject WScript.Shell;
$d=@([Environment]::GetFolderPath('Programs'),[Environment]::GetFolderPath('Desktop'));
@(Get-ChildItem -LiteralPath $d -Recurse -Filter *.lnk | ForEach-Object { $s=$sh.CreateShortcut($_.FullName);
[pscustomobject]@{lnk=$_.FullName;target=$s.TargetPath;args=$s.Arguments;workdir=$s.WorkingDirectory;icon=$s.IconLocation;desc=$s.Description} }) | ConvertTo-Json -Compress"#;
    let Ok(out) = no_window(std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", ps])).output() else { return vec![] };
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    let list = match v {
        Value::Array(a) => a,
        o @ Value::Object(_) => vec![o],
        _ => vec![],
    };
    let rel = |p: &str| rel_to(prof, Path::new(p));
    list.iter()
        .filter_map(|o| {
            let g = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let target = g("target");
            if !is_inside(Path::new(&target), install) || lower(Path::new(&target)) == lower(install) {
                return None;
            }
            let (icon, idx) = match g("icon").rsplit_once(',') {
                Some((f, i)) => (f.to_string(), i.trim().parse().unwrap_or(0)),
                None => (g("icon"), 0),
            };
            let icon = if icon.is_empty() { String::new() } else { rel(&icon).unwrap_or(icon) };
            Some(Shortcut { lnk: rel(&g("lnk"))?, target: rel(&target)?, args: g("args"), workdir: rel(&g("workdir")).unwrap_or_default(), icon, icon_index: idx, desc: g("desc") })
        })
        .collect()
}

/// Apps lock their files while running: offer to close them (sender: files would be half-written; receiver: in use).
pub fn ensure_closed(name: &str, dirs: &[PathBuf], log: &dyn Fn(String)) -> io::Result<()> {
    let Ok(mut sys) = sysinfo::System::new() else { return Ok(()) };
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let me = std::process::id();
    let running: Vec<_> = sys
        .processes()
        .values()
        .filter(|p| p.pid().as_u32() != me)
        .filter(|p| p.exe().is_some_and(|e| dirs.iter().any(|d| is_inside(e, d))))
        .collect();
    if running.is_empty() {
        return Ok(());
    }
    let mut names: Vec<String> = running.iter().map(|p| p.name().to_string_lossy().to_string()).collect();
    names.sort();
    names.dedup();
    if !crate::installer::ask_yes_no(&format!("{name} is running ({}).\nIt has to be closed so its files can be copied.\n\nClose it now?", names.join(", "))) {
        return Err(io::Error::other(format!("{name} is running. Close it and try again.")));
    }
    log(format!("Closing {name}..."));
    for p in &running {
        p.kill();
    }
    std::thread::sleep(std::time::Duration::from_millis(1500));
    Ok(())
}

// ---------------- receiving ----------------

/// Builds the receiving staging folder: junctions to where the app's pieces go in this user's profile.
pub fn prepare_receive(offer: &Offer, log: &dyn Fn(String)) -> io::Result<Staging> {
    let p = plan(&offer.meta)?;
    let prof = profile();
    let install = prof.join(&p.install);
    let data_dirs: Vec<PathBuf> = p.data.iter().map(|(b, r)| base_dir(b).join(r)).collect();
    let mut dirs = vec![install.clone()];
    dirs.extend(data_dirs.iter().cloned());
    ensure_closed(&offer.name, &dirs, log)?;
    let st = Staging::new(&staging_name(&offer.app_id, "in"))?;
    st.link("install", &install)?;
    for ((base, rel), dir) in p.data.iter().zip(&data_dirs) {
        st.link(&format!("{base}\\{rel}"), dir)?;
    }
    Ok(st)
}

/// After the files arrived: registry settings (paths rewritten for this user) and shortcuts.
pub fn finish_receive(meta: &Value, log: &dyn Fn(String)) -> io::Result<()> {
    let p = plan(meta)?;
    let prof = profile();
    let prof_s = prof.to_string_lossy().to_string();

    if !p.registry.trim().is_empty() {
        let text = filter_registry(&p.registry, &p.key);
        let text = rewrite_profile(&text, &p.from_profile, &prof_s);
        let tmp = std::env::temp_dir().join(format!("steamtransfer-import-{}.reg", std::process::id()));
        let mut bytes = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            bytes.extend(u.to_le_bytes());
        }
        std::fs::write(&tmp, bytes)?;
        let ok = no_window(std::process::Command::new("reg").args(["import", &tmp.to_string_lossy()])).output().is_ok_and(|o| o.status.success());
        let _ = std::fs::remove_file(&tmp);
        log(if ok { "Imported the app's settings from the registry.".into() } else { "Couldn't import some of the app's registry settings.".into() });
    }

    if !p.shortcuts.is_empty() {
        let q = |s: &str| s.replace('\'', "''");
        let abs = |s: &str| if s.is_empty() { String::new() } else if Path::new(s).is_absolute() { rewrite_profile(s, &p.from_profile, &prof_s) } else { prof.join(s).to_string_lossy().to_string() };
        let mut ps = String::from("$ErrorActionPreference='Continue';$sh=New-Object -ComObject WScript.Shell;");
        for sc in &p.shortcuts {
            let lnk = prof.join(&sc.lnk);
            let icon = abs(&sc.icon);
            ps.push_str(&format!(
                "New-Item -ItemType Directory -Force -Path '{}' | Out-Null;$s=$sh.CreateShortcut('{}');$s.TargetPath='{}';$s.Arguments='{}';$s.WorkingDirectory='{}';{}$s.Description='{}';$s.Save();",
                q(&lnk.parent().unwrap_or(&prof).to_string_lossy()),
                q(&lnk.to_string_lossy()),
                q(&abs(&sc.target)),
                q(&rewrite_profile(&sc.args, &p.from_profile, &prof_s)),
                q(&abs(&sc.workdir)),
                if icon.is_empty() { String::new() } else { format!("$s.IconLocation='{},{}';", q(&icon), sc.icon_index) },
                q(&sc.desc),
            ));
        }
        let _ = no_window(std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &ps])).status();
        log(format!("Added {} shortcut{}.", p.shortcuts.len(), if p.shortcuts.len() == 1 { "" } else { "s" }));
    }
    Ok(())
}

/// Keeps only the app's own keys: its uninstall entry and HKCU\Software\<its name> — never Windows' keys (Run,
/// Policies, file associations, ...), and never deletions.
pub fn filter_registry(text: &str, key: &str) -> String {
    let uninstall = format!(r"hkey_current_user\{}\{}", UNINSTALL.to_lowercase(), key.to_lowercase());
    let allowed = |section: &str| {
        let s = section.to_lowercase();
        if s == uninstall || s.starts_with(&format!("{uninstall}\\")) {
            return true;
        }
        let Some(rest) = s.strip_prefix(r"hkey_current_user\software\") else { return false };
        !rest.is_empty() && !rest.split('\\').any(|seg| seg.is_empty() || seg == "..") && !is_shared(rest)
    };
    let mut out = String::from("Windows Registry Editor Version 5.00\r\n");
    let mut keep = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('[') {
            let section = rest.trim_end().trim_end_matches(']');
            keep = !section.starts_with('-') && allowed(section);
            if keep {
                out.push_str("\r\n");
                out.push_str(line);
                out.push_str("\r\n");
            }
        } else if keep && !line.trim().is_empty() {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    out
}

/// Replaces the sender's profile path with ours (plain and .reg-escaped forms), case-insensitively.
pub fn rewrite_profile(text: &str, from: &str, to: &str) -> String {
    let from = from.trim_end_matches('\\');
    let to = to.trim_end_matches('\\');
    if from.is_empty() || from.eq_ignore_ascii_case(to) {
        return text.to_string();
    }
    let mut s = text.to_string();
    for (f, t) in [(from.replace('\\', r"\\"), to.replace('\\', r"\\")), (from.to_string(), to.to_string()), (from.replace('\\', "/"), to.replace('\\', "/"))] {
        s = replace_ci(&s, &f, &t);
    }
    s
}

fn replace_ci(s: &str, from: &str, to: &str) -> String {
    let (ls, lf) = (s.to_lowercase(), from.to_lowercase());
    if lf.is_empty() || ls.len() != s.len() {
        return if ls.len() != s.len() { s.replace(from, to) } else { s.to_string() };
    }
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(pos) = ls[i..].find(&lf) {
        out.push_str(&s[i..i + pos]);
        out.push_str(to);
        i += pos + lf.len();
    }
    out.push_str(&s[i..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(base_name("Microsoft Visual Studio Code (User)"), "Microsoft Visual Studio Code");
        assert_eq!(base_name("Zoom 6.2.5 (x64)"), "Zoom");
        assert_eq!(base_name("7"), "7");
        assert_eq!(base_name("Notepad++ v8.6.2"), "Notepad++");
        assert_eq!(base_name("Player 404notfound"), "Player 404notfound");
        assert_eq!(base_name("Mozilla Firefox 3D"), "Mozilla Firefox 3D");
        assert_eq!(publisher_name("Discord Inc."), "Discord");
        assert_eq!(publisher_name("Google LLC"), "Google");
        assert_eq!(exe_in(r#""C:\Users\a\AppData\Local\Discord\Update.exe" --uninstall"#), Some(PathBuf::from(r"C:\Users\a\AppData\Local\Discord\Update.exe")));
        assert_eq!(exe_in(r"C:\x y\app.exe,0"), Some(PathBuf::from(r"C:\x y\app.exe")));
    }

    #[test]
    fn install_paths_from_peers_are_confined() {
        assert!(install_rel_ok(r"AppData\Local\Programs\Microsoft VS Code"));
        assert!(install_rel_ok(r"AppData\Local\Discord"));
        assert!(install_rel_ok(r"AppData\Local\Google\Chrome"));
        assert!(!install_rel_ok(r"AppData\Local\Programs"));
        assert!(!install_rel_ok(r"AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup"));
        assert!(!install_rel_ok(r"AppData\Local\Microsoft\WindowsApps"));
        assert!(!install_rel_ok(r"Desktop\evil"));
        assert!(!install_rel_ok(r"AppData\Local\..\..\Windows"));
        assert!(!install_rel_ok(r"AppData\Local\C:\x"));
        assert!(!install_rel_ok(r"AppData\Local\Temp\x"));
        assert!(data_rel_ok("discord") && data_rel_ok(r"Spotify\Users"));
        assert!(!data_rel_ok(r"Microsoft\Windows") && !data_rel_ok("..") && !data_rel_ok(r"a\b\c"));
        assert!(home_rel_ok(".vscode") && !home_rel_ok(".ssh") && !home_rel_ok("Documents") && !home_rel_ok(r".vscode\..\x") && !home_rel_ok(".."));
    }

    #[test]
    fn registry_is_filtered_and_rewritten() {
        let reg = "Windows Registry Editor Version 5.00\r\n\r\n[HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run]\r\n\"evil\"=\"x.exe\"\r\n\r\n[HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Foo]\r\n\"InstallLocation\"=\"C:\\\\Users\\\\Alice\\\\AppData\\\\Local\\\\Foo\"\r\n\r\n[-HKEY_CURRENT_USER\\Software\\Foo]\r\n\r\n[HKEY_CURRENT_USER\\Software\\Foo\\Settings]\r\n\"Theme\"=\"dark\"\r\n[HKEY_CURRENT_USER\\Software\\Classes\\foo]\r\n@=\"x\"\r\n[HKEY_LOCAL_MACHINE\\Software\\Foo]\r\n\"a\"=\"b\"\r\n";
        let f = filter_registry(reg, "Foo");
        assert!(!f.contains("Run") && !f.contains("evil") && !f.contains("[-") && !f.contains("Classes") && !f.contains("LOCAL_MACHINE"), "{f}");
        assert!(f.contains(r"Uninstall\Foo]") && f.contains(r"Software\Foo\Settings]") && f.contains("\"Theme\"=\"dark\""), "{f}");
        let r = rewrite_profile(&f, r"C:\Users\Alice", r"C:\Users\Bob");
        assert!(r.contains(r#""C:\\Users\\Bob\\AppData\\Local\\Foo""#), "{r}");
    }

    #[test]
    fn shortcuts_from_peers_are_confined() {
        let ok = Shortcut { lnk: r"AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Foo.lnk".into(), target: r"AppData\Local\Foo\foo.exe".into(), ..Default::default() };
        assert!(shortcut_ok(&ok, r"AppData\Local\Foo"));
        let startup = Shortcut { lnk: r"AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\Foo.lnk".into(), ..ok.clone() };
        assert!(!shortcut_ok(&startup, r"AppData\Local\Foo"));
        let outside = Shortcut { target: r"AppData\Local\Other\x.exe".into(), ..ok.clone() };
        assert!(!shortcut_ok(&outside, r"AppData\Local\Foo"));
        let desk = Shortcut { lnk: r"Desktop\Foo.lnk".into(), ..ok.clone() };
        assert!(shortcut_ok(&desk, r"AppData\Local\Foo"));
        let not_lnk = Shortcut { lnk: r"Desktop\Foo.exe".into(), ..ok };
        assert!(!shortcut_ok(&not_lnk, r"AppData\Local\Foo"));
    }

    #[test]
    fn junctions_link_and_unlink_without_touching_targets() {
        let base = std::env::temp_dir().join(format!("steamxfer-junction-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let target = base.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.txt"), b"hi").unwrap();
        let link = base.join("link");
        junction(&link, &target).unwrap();
        assert_eq!(std::fs::read(link.join("keep.txt")).unwrap(), b"hi");
        std::fs::write(link.join("new.txt"), b"x").unwrap();
        assert!(target.join("new.txt").is_file());
        assert!(is_link(&link));
        std::fs::remove_dir(&link).unwrap();
        assert!(target.join("keep.txt").is_file() && target.join("new.txt").is_file());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
