//! What this PC has: USB4/Thunderbolt controller, network links, drives you could copy a game to.
//! WMI is used from background threads only (it needs its own COM apartment).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use wmi::{Variant, WMIConnection};

use crate::transfer::{self, PACKAGE_FILE};

#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub name: String,
    pub kind: &'static str, // usb4 | ethernet | wifi
    pub up: bool,
    pub bps: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Package {
    pub folder: PathBuf,
    pub app_id: String,
    pub name: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Drive {
    pub root: PathBuf,
    pub label: String,
    pub free: u64,
    pub total: u64,
    pub removable: bool,
    pub packages: Vec<Package>,
}

type Row = HashMap<String, Variant>;

fn query(namespace: &str, q: &str) -> Option<Vec<Row>> {
    WMIConnection::with_namespace_path(namespace).ok()?.raw_query::<Row>(q).ok()
}

fn s(r: &Row, k: &str) -> String {
    match r.get(k) {
        Some(Variant::String(v)) => v.clone(),
        _ => String::new(),
    }
}

fn n(r: &Row, k: &str) -> u64 {
    match r.get(k) {
        Some(Variant::UI8(v)) => *v,
        Some(Variant::UI4(v)) => *v as u64,
        Some(Variant::UI2(v)) => *v as u64,
        Some(Variant::I4(v)) => *v as u64,
        Some(Variant::I8(v)) => *v as u64,
        Some(Variant::String(v)) => v.parse().unwrap_or(0),
        _ => 0,
    }
}

/// Does this PC have a USB4 or Thunderbolt controller at all?
pub fn has_usb4_controller() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        query(r"ROOT\CIMV2", "SELECT Name FROM Win32_PnPEntity WHERE Name LIKE '%USB4%' OR Name LIKE '%Thunderbolt%'").is_some_and(|r| !r.is_empty())
    })
}

/// Physical network adapters, same view as Get-NetAdapter (no hidden filter drivers / phantom adapters).
pub fn links() -> Vec<Link> {
    let rows = query(
        r"ROOT\StandardCimv2",
        "SELECT Name, InterfaceDescription, MediaConnectState, Speed, HardwareInterface, NdisPhysicalMedium FROM MSFT_NetAdapter WHERE Hidden = FALSE",
    );
    let Some(rows) = rows else { return Vec::new() };
    rows.iter()
        .filter_map(|r| {
            let desc = s(r, "InterfaceDescription").to_lowercase();
            let usb4 = desc.contains("thunderbolt") || desc.contains("usb4");
            let hw = matches!(r.get("HardwareInterface"), Some(Variant::Bool(true)));
            if !(hw || usb4) || desc.contains("bluetooth") {
                return None;
            }
            let up = n(r, "MediaConnectState") == 1;
            let medium = n(r, "NdisPhysicalMedium");
            let kind = if usb4 { "usb4" } else if medium == 1 || medium == 9 { "wifi" } else { "ethernet" };
            Some(Link { name: s(r, "Name"), kind, up, bps: if up { n(r, "Speed") } else { 0 } })
        })
        .collect()
}

/// Drive letters that sit on a real disk partition (None if WMI is unavailable).
fn physical_letters() -> Option<HashSet<String>> {
    let rows = query(r"ROOT\CIMV2", "SELECT Dependent FROM Win32_LogicalDiskToPartition")?;
    Some(
        rows.iter()
            .filter_map(|r| {
                let dep = s(r, "Dependent"); // ...Win32_LogicalDisk.DeviceID="E:"
                let i = dep.find("DeviceID=\"")?;
                dep.get(i + 10..i + 12).map(|l| l.to_ascii_uppercase())
            })
            .collect(),
    )
}

/// Drives you could copy a game to: removable drives and any non-system drive (external SSDs report as fixed).
pub fn drives() -> Vec<Drive> {
    let sys_drive = std::env::var("SystemDrive").unwrap_or("C:".into()).to_ascii_uppercase();
    let physical = physical_letters();
    let Ok(disks) = sysinfo::Disks::new_with_refreshed_list() else { return Vec::new() };
    let mut out: Vec<Drive> = disks
        .iter()
        .filter_map(|d| {
            let root = d.mount_point().to_path_buf();
            let letter = root.to_string_lossy().get(..2)?.to_ascii_uppercase();
            if letter == sys_drive || d.total_space() < 2 * 1024 * 1024 * 1024 {
                return None;
            }
            if physical.as_ref().is_some_and(|p| !p.contains(&letter)) {
                return None; // cloud/virtual drives (Google Drive etc.)
            }
            let name = d.name().to_string_lossy().to_string();
            let label = if name.trim().is_empty() { if d.is_removable() { "USB drive".into() } else { "Local disk".into() } } else { name };
            Some(Drive { packages: find_packages(&root), root, label, free: d.available_space(), total: d.total_space(), removable: d.is_removable() })
        })
        .collect();
    out.sort_by(|a, b| a.root.cmp(&b.root));
    out
}

/// Is this path on a spinning hard drive? HDDs hate parallel small-file I/O (constant seeking),
/// so transfers touching one use fewer workers and don't split big files. Cached per drive letter.
pub fn is_hdd(path: &Path) -> bool {
    static CACHE: std::sync::Mutex<Option<HashMap<char, bool>>> = std::sync::Mutex::new(None);
    let Some(letter) = path.to_string_lossy().chars().next().filter(|c| c.is_ascii_alphabetic()).map(|c| c.to_ascii_uppercase()) else { return false };
    if let Some(v) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&letter) {
        return *v;
    }
    let hdd = (|| {
        let ns = r"ROOT\Microsoft\Windows\Storage";
        let parts = query(ns, "SELECT DiskNumber, DriveLetter FROM MSFT_Partition")?;
        let disk = parts.iter().find(|r| {
            let l = match r.get("DriveLetter") {
                Some(Variant::UI2(v)) => char::from_u32(*v as u32),
                Some(Variant::I2(v)) => char::from_u32(*v as u32),
                Some(Variant::String(s)) => s.chars().next(),
                _ => None,
            };
            l.is_some_and(|l| l.to_ascii_uppercase() == letter)
        })?;
        let disk = n(disk, "DiskNumber");
        let disks = query(ns, "SELECT DeviceId, MediaType FROM MSFT_PhysicalDisk")?;
        let d = disks.iter().find(|r| s(r, "DeviceId") == disk.to_string())?;
        Some(n(d, "MediaType") == 3) // 3 = HDD, 4 = SSD, 5 = SCM, 0 = unknown
    })()
    .unwrap_or(false);
    CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(letter, hdd);
    hdd
}

/// Exported games inside a folder, its subfolders, or its SteamTransfer folder.
pub fn find_packages(folder: &Path) -> Vec<Package> {
    let mut candidates = vec![folder.to_path_buf()];
    let subdirs = |p: &Path| std::fs::read_dir(p).into_iter().flatten().flatten().filter(|e| e.path().is_dir()).map(|e| e.path()).collect::<Vec<_>>();
    candidates.extend(subdirs(folder));
    candidates.extend(subdirs(&folder.join("SteamTransfer")));
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|c| seen.insert(c.to_string_lossy().to_lowercase()))
        .filter(|c| c.join(PACKAGE_FILE).is_file())
        .filter_map(|c| {
            let o = transfer::read_package(&c).ok()?;
            Some(Package { folder: c, app_id: o.app_id, name: o.name, size: o.total_bytes })
        })
        .collect()
}
