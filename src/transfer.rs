//! File plan for a game, the copy pump, pause/cancel, resume (skip identical files),
//! USB-drive packages and recovery from drives that get unplugged mid-copy.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::launchers::Launcher;
use crate::steam::{self, Game, Library};
use crate::util;

pub const BUF_SIZE: usize = 4 * 1024 * 1024;
pub const PACKAGE_FILE: &str = "transfer.json";
const MTIME_TOLERANCE_TICKS: i64 = 2 * 10_000_000; // FAT/exFAT drives round timestamps to 2 s

// ---------------- wire / package types (PascalCase = compatible with the C# version) ----------------

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct FileEntry {
    pub path: String, // relative to library root, forward slashes
    pub size: u64,
    #[serde(rename = "MTime")]
    pub mtime: i64, // .NET UTC ticks
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct Offer {
    pub version: i32,
    pub app_id: String,
    pub name: String,
    pub from: String,
    pub total_bytes: u64,
    pub dirs: Vec<String>,
    pub files: Vec<FileEntry>,
    /// Launcher id ("steam", "epic", "gog", ... — older versions omit it, meaning Steam).
    pub launcher: String,
    /// Launcher install records needed to register the game on the receiving PC (e.g. Epic's manifest).
    pub meta: serde_json::Value,
    /// Folder name for non-Steam games (the receiver creates it under the chosen parent folder).
    pub folder: String,
    /// Drive packages only: files that compress are stored as `<path>.zst` (zstd), the rest as-is.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub packed: bool,
}

impl Default for Offer {
    fn default() -> Self {
        Offer { version: 1, app_id: String::new(), name: String::new(), from: String::new(), total_bytes: 0, dirs: vec![], files: vec![], launcher: String::new(), meta: serde_json::Value::Null, folder: String::new(), packed: false }
    }
}

impl Offer {
    pub fn launcher(&self) -> Launcher {
        Launcher::from_id(&self.launcher)
    }
    /// Steam offers use library-relative `steamapps/...` paths; all others are relative to the game folder.
    pub fn is_steam(&self) -> bool {
        self.launcher() == Launcher::Steam
    }
}

// ---------------- progress, pause, cancel ----------------

#[derive(Default)]
struct PauseState {
    paused: bool,
    active: Duration,
    running_since: Option<Instant>,
}

/// Shared between the transfer threads and the UI.
pub struct Progress {
    done: AtomicU64,
    pub total: AtomicU64,
    pub current: Mutex<String>,
    pub waiting: Mutex<Option<String>>,
    drive_waiters: AtomicI32,
    pause: Mutex<PauseState>,
    resumed: Condvar,
    cancelled: AtomicBool,
    sockets: Mutex<Vec<TcpStream>>,
}

impl Progress {
    pub fn new() -> Self {
        Progress {
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            current: Mutex::new(String::new()),
            waiting: Mutex::new(None),
            drive_waiters: AtomicI32::new(0),
            pause: Mutex::new(PauseState { running_since: Some(Instant::now()), ..Default::default() }),
            resumed: Condvar::new(),
            cancelled: AtomicBool::new(false),
            sockets: Mutex::new(Vec::new()),
        }
    }
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }
    pub fn add(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }
    pub fn sub(&self, n: u64) {
        self.done.fetch_sub(n, Ordering::Relaxed);
    }
    pub fn set_current(&self, s: &str) {
        *self.current.lock().unwrap() = s.to_string();
    }
    pub fn waiting(&self) -> Option<String> {
        self.waiting.lock().unwrap().clone()
    }

    /// Time spent actually transferring (pauses excluded).
    pub fn active_time(&self) -> Duration {
        let st = self.pause.lock().unwrap();
        st.active + st.running_since.map(|s| s.elapsed()).unwrap_or_default()
    }
    pub fn is_paused(&self) -> bool {
        self.pause.lock().unwrap().paused
    }
    pub fn pause(&self) {
        let mut st = self.pause.lock().unwrap();
        if st.paused {
            return;
        }
        st.paused = true;
        if let Some(s) = st.running_since.take() {
            st.active += s.elapsed();
        }
    }
    pub fn resume(&self) {
        let mut st = self.pause.lock().unwrap();
        st.paused = false;
        st.running_since.get_or_insert_with(Instant::now);
        self.resumed.notify_all();
    }

    /// Blocks while paused. Errors if cancelled.
    pub fn checkpoint(&self) -> io::Result<()> {
        let mut st = self.pause.lock().unwrap();
        while st.paused && !self.is_cancelled() {
            st = self.resumed.wait_timeout(st, Duration::from_millis(250)).unwrap().0;
        }
        drop(st);
        if self.is_cancelled() { Err(cancelled()) } else { Ok(()) }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.resume();
        self.abort_io();
    }
    /// Shut down every socket of this transfer, unblocking threads stuck in read/write.
    pub fn abort_io(&self) {
        for s in self.sockets.lock().unwrap().iter() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    /// Sockets to shut down on cancel.
    pub fn register(&self, s: &TcpStream) {
        if let Ok(c) = s.try_clone() {
            if self.is_cancelled() {
                let _ = c.shutdown(Shutdown::Both);
            }
            self.sockets.lock().unwrap().push(c);
        }
    }

    fn sleep(&self, d: Duration) -> io::Result<()> {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if self.is_cancelled() {
                return Err(cancelled());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }
}

/// Not ErrorKind::Interrupted: std retries those automatically.
pub fn cancelled() -> io::Error {
    io::Error::other(CancelledError)
}

pub fn is_cancel_error(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<CancelledError>())
}

#[derive(Debug)]
pub struct CancelledError;
impl std::fmt::Display for CancelledError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cancelled")
    }
}
impl std::error::Error for CancelledError {}

// ---------------- the file plan ----------------

/// Everything that makes up an installed game, manifests last so Steam never sees a half-copied game.
pub fn build_offer(g: &Game) -> io::Result<Offer> {
    if !g.is_steam() {
        return build_folder_offer(g);
    }
    let mut offer = Offer { app_id: g.app_id.clone(), name: g.name.clone(), from: util::machine_name(), launcher: Launcher::Steam.id().into(), ..Default::default() };
    let root = &g.library.path;

    let add_tree = |rel: String, offer: &mut Offer| -> io::Result<()> {
        let full = root.join(rel.replace('/', "\\"));
        if !full.is_dir() {
            return Ok(());
        }
        for e in walkdir::WalkDir::new(&full) {
            let e = e.map_err(io::Error::other)?;
            let r = rel_path(root, e.path());
            if e.file_type().is_dir() {
                offer.dirs.push(r);
            } else if e.file_type().is_file() {
                let md = e.metadata().map_err(io::Error::other)?;
                offer.files.push(FileEntry { path: r, size: md.len(), mtime: util::to_ticks(md.modified()?) });
            }
        }
        Ok(())
    };
    add_tree(format!("steamapps/common/{}", g.install_dir), &mut offer)?;
    add_tree(format!("steamapps/workshop/content/{}", g.app_id), &mut offer)?;
    for f in [format!("steamapps/workshop/appworkshop_{}.acf", g.app_id), format!("steamapps/appmanifest_{}.acf", g.app_id)] {
        let full = root.join(f.replace('/', "\\"));
        if let Ok(md) = fs::metadata(&full) {
            offer.files.push(FileEntry { path: f, size: md.len(), mtime: util::to_ticks(md.modified()?) });
        }
    }
    if !offer.files.iter().any(|f| f.path.to_lowercase().ends_with(&format!("appmanifest_{}.acf", g.app_id))) {
        return Err(io::Error::other("Game manifest not found."));
    }
    offer.total_bytes = offer.files.iter().map(|f| f.size).sum();
    Ok(offer)
}

/// A non-Steam game: every file in its folder, paths relative to that folder.
fn build_folder_offer(g: &Game) -> io::Result<Offer> {
    let root = &g.library.path;
    if !root.is_dir() {
        return Err(io::Error::other(format!("Game folder not found: {}", root.display())));
    }
    let mut offer = Offer {
        app_id: g.app_id.clone(),
        name: g.name.clone(),
        from: util::machine_name(),
        launcher: g.launcher.id().into(),
        meta: g.meta.clone(),
        folder: g.install_dir.clone(),
        ..Default::default()
    };
    // apps: the staging folder is made of junctions (follow them) and caches stay behind
    let app = g.launcher == Launcher::App;
    let walk = walkdir::WalkDir::new(root).min_depth(1).follow_links(app).into_iter().filter_entry(|e| {
        !(app && e.file_type().is_dir() && crate::apps::CACHE_DIRS.contains(&e.file_name().to_string_lossy().to_lowercase().as_str()))
    });
    for e in walk {
        let e = e.map_err(io::Error::other)?;
        let r = rel_path(root, e.path());
        if e.file_type().is_dir() {
            offer.dirs.push(r);
        } else if e.file_type().is_file() {
            let md = e.metadata().map_err(io::Error::other)?;
            offer.files.push(FileEntry { path: r, size: md.len(), mtime: util::to_ticks(md.modified()?) });
        }
    }
    if offer.files.is_empty() {
        return Err(io::Error::other(format!("No files found in {}", root.display())));
    }
    offer.total_bytes = offer.files.iter().map(|f| f.size).sum();
    Ok(offer)
}

fn rel_path(root: &Path, full: &Path) -> String {
    full.strip_prefix(root).unwrap_or(full).to_string_lossy().replace('\\', "/")
}

/// Steam manifests are written last (so Steam never sees a half-copied game). Other launchers have none here.
pub fn is_manifest(rel: &str, steam: bool) -> bool {
    steam && rel.to_ascii_lowercase().ends_with(".acf")
}

/// Resolve a relative path from a peer or package. Steam: only the game folders inside a Steam library are allowed
/// (the library root also holds steam.exe). Other launchers: anything inside the game's own folder.
pub fn safe_combine(root: &Path, rel: &str, steam: bool) -> io::Result<PathBuf> {
    let r = rel.replace('\\', "/");
    let lower = r.to_ascii_lowercase();
    let bad = || io::Error::new(io::ErrorKind::InvalidData, format!("Refusing unexpected path: {rel}"));
    if r.is_empty() || r.starts_with('/') || r.split('/').any(|seg| seg.is_empty() || seg == ".." || seg == "." || seg.contains(':')) {
        return Err(bad());
    }
    let allowed = !steam
        || lower.starts_with("steamapps/common/")
        || lower.starts_with("steamapps/workshop/")
        || (lower.starts_with("steamapps/appmanifest_") && !lower[10..].contains('/'));
    if !allowed {
        return Err(bad());
    }
    let full = root.join(r.replace('/', "\\"));
    if full.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(bad());
    }
    Ok(full)
}

/// Indices of files already at the destination with the same size and timestamp (resume / update).
pub fn compute_skip(offer: &Offer, dest_root: &Path) -> Vec<usize> {
    offer
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| !is_manifest(&f.path, offer.is_steam()))
        .filter(|(_, f)| {
            safe_combine(dest_root, &f.path, offer.is_steam())
                .ok()
                .and_then(|p| fs::metadata(p).ok())
                .is_some_and(|md| md.len() == f.size && md.modified().is_ok_and(|m| (util::to_ticks(m) - f.mtime).abs() <= MTIME_TOLERANCE_TICKS))
        })
        .map(|(i, _)| i)
        .collect()
}

pub fn create_dirs(offer: &Offer, dest_root: &Path) -> io::Result<()> {
    for d in &offer.dirs {
        fs::create_dir_all(safe_combine(dest_root, d, offer.is_steam())?)?;
    }
    Ok(())
}

thread_local! {
    static BUFFER: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A reusable 4 MB buffer per thread (games have thousands of small files; don't allocate per file).
pub fn with_buffer<R>(f: impl FnOnce(&mut [u8]) -> R) -> R {
    BUFFER.with(|b| {
        let mut b = b.borrow_mut();
        if b.len() < BUF_SIZE {
            b.resize(BUF_SIZE, 0);
        }
        f(&mut b[..])
    })
}

/// Copy exactly `count` bytes, honoring pause/cancel.
pub fn pump(src: &mut (impl Read + ?Sized), dst: &mut impl Write, count: u64, p: &Progress, added: &mut u64) -> io::Result<()> {
    with_buffer(|buf| {
        let mut left = count;
        while left > 0 {
            p.checkpoint()?;
            let want = left.min(buf.len() as u64) as usize;
            let n = src.read(&mut buf[..want])?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Source ended early (connection lost or file changed)."));
            }
            dst.write_all(&buf[..n])?;
            p.add(n as u64);
            *added += n as u64;
            left -= n as u64;
        }
        Ok(())
    })
}

/// Files at or below this size are read whole into memory and handled by thread pools,
/// so per-file overhead (create, close, antivirus scan) overlaps instead of adding up.
pub const SMALL_FILE: u64 = 1024 * 1024;
/// Parallel file workers for small files: 32 on SSDs, 2 if a hard drive is involved (seeks).
/// STEAMXFER_WORKERS overrides it for benchmarking.
pub fn workers_for(paths: &[&Path]) -> usize {
    if let Some(n) = std::env::var("STEAMXFER_WORKERS").ok().and_then(|v| v.parse().ok()) {
        return n;
    }
    // Small-file speed is latency-bound (per-file round trips, antivirus scan), so more files in flight help:
    // on a USB SSD 8 → 32 workers took 34 → 47 MB/s; beyond 32 it flattens. HDDs need few (seeks).
    if paths.iter().any(|p| crate::hardware::is_hdd(p)) { 2 } else { 32 }
}

/// Open the destination for writing, cheaply: directories normally exist already (created up front),
/// read-only leftovers are made writable, preallocation only for big files.
/// How files written to a drive that may be unplugged are made durable before they count as done.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Durability {
    /// FlushFileBuffers per file: a device cache-flush command each time (slow over USB with many files).
    Flush,
    /// FILE_FLAG_WRITE_THROUGH: every write goes straight to the device, no separate flush.
    WriteThrough,
    /// Benchmark baseline only: no guarantee.
    None,
}

pub fn durability() -> Durability {
    match std::env::var("STEAMXFER_DURABLE").as_deref() {
        Ok("flush") => Durability::Flush,
        Ok("none") => Durability::None,
        Ok("wt") => Durability::WriteThrough,
        // measured on a USB 3.1 SSD (exFAT): write-through is as fast as no guarantee at all, 1.9× faster than flushing per file
        _ => Durability::WriteThrough,
    }
}

/// Workers copying big files (≥1 MB) at once: SSDs handle several in parallel, a hard drive wants one.
/// STEAMXFER_BIG_WORKERS overrides it for benchmarking.
pub fn big_workers_for(paths: &[&Path]) -> usize {
    if let Some(n) = std::env::var("STEAMXFER_BIG_WORKERS").ok().and_then(|v| v.parse().ok()) {
        return n;
    }
    if paths.iter().any(|p| crate::hardware::is_hdd(p)) { 1 } else { 4 }
}

pub(crate) fn create_dest(f: &FileEntry, dest_root: &Path, steam: bool, durable: bool) -> io::Result<(File, PathBuf, PathBuf)> {
    let dest = safe_combine(dest_root, &f.path, steam)?;
    let tmp = if is_manifest(&f.path, steam) { PathBuf::from(format!("{}.steamtransfer.tmp", dest.display())) } else { dest.clone() };
    let write_through = durable && durability() == Durability::WriteThrough;
    let open = || {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
        OpenOptions::new().write(true).create(true).truncate(true).custom_flags(if write_through { FILE_FLAG_WRITE_THROUGH } else { 0 }).open(&tmp)
    };
    let out = match open() {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(tmp.parent().unwrap_or(dest_root))?;
            open()?
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied && tmp.exists() => {
            let mut perms = fs::metadata(&tmp)?.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            fs::set_permissions(&tmp, perms)?;
            open()?
        }
        Err(e) => return Err(e),
    };
    if f.size >= SMALL_FILE {
        out.set_len(f.size)?; // preallocate: less fragmentation
    }
    Ok((out, dest, tmp))
}

/// Timestamp via the open handle (no second open), optional flush, then manifest fix-ups.
pub(crate) fn finish_dest(out: File, f: &FileEntry, dest: &Path, tmp: &Path, flush: bool) -> io::Result<()> {
    out.set_modified(util::from_ticks(f.mtime))?;
    if flush && durability() == Durability::Flush {
        out.sync_all()?;
    }
    drop(out);
    if tmp != dest {
        fs::rename(tmp, dest)?;
    }
    if dest.file_name().is_some_and(|n| n.to_string_lossy().to_ascii_lowercase().starts_with("appmanifest_")) {
        steam::fix_manifest(dest);
    }
    Ok(())
}

/// Write one file streamed from `src` (used for big files and manifests).
/// `flush`: force data onto the device before the file counts as done (for drives that may be yanked).
/// `added` receives the bytes counted into progress, so a failed attempt can be taken back.
pub fn write_file(src: &mut (impl Read + ?Sized), f: &FileEntry, dest_root: &Path, steam: bool, p: &Progress, flush: bool, added: &mut u64) -> io::Result<()> {
    p.set_current(&f.path);
    let (mut out, dest, tmp) = create_dest(f, dest_root, steam, flush)?;
    pump(src, &mut out, f.size, p, added)?;
    finish_dest(out, f, &dest, &tmp, flush)
}

/// Write one small file whose contents are already in memory. Progress is counted by the caller.
pub fn write_bytes(data: &[u8], f: &FileEntry, dest_root: &Path, steam: bool, flush: bool) -> io::Result<()> {
    let (mut out, dest, tmp) = create_dest(f, dest_root, steam, flush)?;
    out.write_all(data)?;
    finish_dest(out, f, &dest, &tmp, flush)
}

/// Read a whole (small) file, waiting for its drive if it was unplugged.
pub fn read_small(path: &Path, f: &FileEntry, p: &Progress, log: &(dyn Fn(String) + Sync)) -> io::Result<Vec<u8>> {
    loop {
        p.checkpoint()?;
        let r = (|| {
            let mut file = open_read(path)?;
            let mut data = Vec::with_capacity(f.size as usize);
            file.read_to_end(&mut data)?;
            if data.len() as u64 != f.size {
                return Err(io::Error::other(format!("{} changed size during the transfer.", f.path)));
            }
            Ok(data)
        })();
        match r {
            Ok(d) => return Ok(d),
            Err(e) => match find_disconnected_drive(&[path]) {
                Some(_) => wait_for_drive(path, p, log)?,
                None => return Err(e),
            },
        }
    }
}

pub fn open_read(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
    OpenOptions::new().read(true).share_mode(FILE_SHARE_ALL).custom_flags(FILE_FLAG_SEQUENTIAL_SCAN).open(path)
}

// ---------------- disconnected drives ----------------

/// After an I/O error: give Windows a moment to finish removing the device, then report which drive vanished.
pub fn find_disconnected_drive(paths: &[&Path]) -> Option<PathBuf> {
    for _ in 0..6 {
        if let Some(p) = paths.iter().find(|p| util::drive_gone(p)) {
            return Some(util::drive_of(p));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
}

/// Waits (shown as "drive disconnected" in the UI) until the drive is back.
pub fn wait_for_drive(path: &Path, p: &Progress, log: &dyn Fn(String)) -> io::Result<()> {
    let drive = util::drive_of(path).to_string_lossy().trim_end_matches('\\').to_string();
    let msg = format!("Drive {drive} disconnected — plug it back in to continue.");
    *p.waiting.lock().unwrap() = Some(msg.clone());
    if p.drive_waiters.fetch_add(1, Ordering::SeqCst) == 0 {
        log(msg);
    }
    let result = (|| {
        while util::drive_gone(path) {
            p.sleep(Duration::from_secs(1))?;
        }
        p.sleep(Duration::from_secs(2)) // let Windows finish mounting it
    })();
    if p.drive_waiters.fetch_sub(1, Ordering::SeqCst) == 1 {
        *p.waiting.lock().unwrap() = None;
        if result.is_ok() {
            log(format!("Drive {drive} is back — continuing."));
        }
    }
    result
}

/// Retry one step of a local copy across drive unplugs.
fn retry_local(p: &Progress, roots: &[&Path], log: &(dyn Fn(String) + Sync), step: &mut dyn FnMut() -> io::Result<()>) -> io::Result<()> {
    loop {
        p.checkpoint()?;
        match step() {
            Ok(()) => return Ok(()),
            Err(e) if p.is_cancelled() || is_cancel_error(&e) => return Err(e),
            Err(e) => match find_disconnected_drive(roots) {
                Some(gone) => wait_for_drive(&gone, p, log)?,
                None => return Err(e),
            },
        }
    }
}

// ---------------- compressed drive packages ----------------

/// How a local copy treats drive-package files.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Packing {
    /// Byte-for-byte.
    Plain,
    /// Writing a package: files that compress are stored as `<path>.zst`.
    Compress,
    /// Reading a package: `<path>.zst` files are decompressed.
    Decompress,
}

/// zstd level for packages: 3 compresses ~600 MB/s per core (faster than any USB drive writes with a few cores
/// busy) and decompresses at ~1.6 GB/s. Higher levels save only 1–4 % more (measured on Silksong: 7.64 GB →
/// 2.23 GB at level 1, 2.13 GB at 3, 1.98 GB at 19, which is ~100× slower).
pub fn pack_level() -> i32 {
    std::env::var("STEAMXFER_PACK_LEVEL").ok().and_then(|v| v.parse().ok()).unwrap_or(3)
}
const PACK_EXT: &str = ".zst";

pub fn packed_name(rel: &str) -> String {
    format!("{rel}{PACK_EXT}")
}

/// Worth storing compressed: saves at least 3 %.
fn packs_well(raw: usize, packed: usize) -> bool {
    (packed as f64) < raw as f64 * 0.97
}

/// Expected packed size / raw size, from 24 random 1 MB samples across the game (weighted by file size).
pub fn estimate_ratio(offer: &Offer, root: &Path) -> f64 {
    let steam = offer.is_steam();
    let files: Vec<&FileEntry> = offer.files.iter().filter(|f| f.size > 0 && !is_manifest(&f.path, steam)).collect();
    let total: u64 = files.iter().map(|f| f.size).sum();
    if total == 0 {
        return 1.0;
    }
    let mut seed = 0x2545F4914F6CDD1Du64;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let (mut raw, mut packed) = (0usize, 0usize);
    let mut buf = vec![0u8; 1024 * 1024];
    for _ in 0..24 {
        let mut target = rnd() % total;
        let Some(f) = files.iter().find(|f| if target < f.size { true } else { target -= f.size; false }) else { continue };
        let Ok(mut file) = safe_combine(root, &f.path, steam).and_then(|p| open_read(&p)) else { continue };
        let off = if f.size > buf.len() as u64 { rnd() % (f.size - buf.len() as u64) } else { 0 };
        use std::io::Seek;
        if file.seek(io::SeekFrom::Start(off)).is_err() {
            continue;
        }
        let Ok(n) = file.read(&mut buf) else { continue };
        if n == 0 {
            continue;
        }
        let z = zstd::bulk::compress(&buf[..n], pack_level()).map(|z| z.len()).unwrap_or(n);
        raw += n;
        packed += if packs_well(n, z) { z } else { n };
    }
    if raw == 0 { 1.0 } else { packed as f64 / raw as f64 }
}

/// Like compute_skip, for a package being written: a finished `<path>.zst` counts when its timestamp matches
/// (its size differs from the original by design; unfinished ones are still named `.zst.partial`).
pub fn compute_skip_packed(offer: &Offer, data_root: &Path) -> Vec<usize> {
    let steam = offer.is_steam();
    let raw: std::collections::HashSet<usize> = compute_skip(offer, data_root).into_iter().collect();
    let same_time = |md: fs::Metadata, f: &FileEntry| md.modified().is_ok_and(|m| (util::to_ticks(m) - f.mtime).abs() <= MTIME_TOLERANCE_TICKS);
    (0..offer.files.len())
        .filter(|i| {
            let f = &offer.files[*i];
            raw.contains(i) || (!is_manifest(&f.path, steam) && safe_combine(data_root, &packed_name(&f.path), steam).ok().and_then(|p| fs::metadata(p).ok()).is_some_and(|md| same_time(md, f)))
        })
        .collect()
}

/// Store one file in a package: compressed (`<path>.zst`) if its first 4 MB shrink, otherwise as-is.
/// The first chunk is written as its own zstd frame and the rest streamed as a second one (a valid .zst).
fn pack_one(f: &FileEntry, src_root: &Path, dest_root: &Path, steam: bool, p: &Progress, flush: bool) -> io::Result<()> {
    let mut src = open_read(&safe_combine(src_root, &f.path, steam)?)?;
    if src.metadata()?.len() != f.size {
        return Err(io::Error::other(format!("File changed size since the copy started: {}", f.path)));
    }
    let raw_dest = safe_combine(dest_root, &f.path, steam)?;
    let zst_dest = safe_combine(dest_root, &packed_name(&f.path), steam)?;
    let mut first = vec![0u8; f.size.min(BUF_SIZE as u64) as usize];
    src.read_exact(&mut first)?;
    let z = zstd::bulk::compress(&first, pack_level())?;
    let mut added = 0u64;
    let r = (|| {
        if !first.is_empty() && packs_well(first.len(), z.len()) {
            let _ = fs::remove_file(&raw_dest); // never both forms
            let part = FileEntry { path: format!("{}.partial", packed_name(&f.path)), size: 0, mtime: f.mtime };
            let (out, part_path, _) = create_dest(&part, dest_root, steam, flush)?;
            let mut out = io::BufWriter::with_capacity(1024 * 1024, out);
            out.write_all(&z)?;
            p.add(first.len() as u64);
            added += first.len() as u64;
            let rest = f.size - first.len() as u64;
            let out = if rest > 0 {
                let mut enc = zstd::stream::Encoder::new(out, pack_level())?;
                pump(&mut src, &mut enc, rest, p, &mut added)?;
                enc.finish()?
            } else {
                out
            };
            let out = out.into_inner().map_err(|e| e.into_error())?;
            finish_dest(out, f, &zst_dest, &part_path, flush)
        } else {
            let _ = fs::remove_file(&zst_dest);
            let mut chained = io::Cursor::new(first).chain(src);
            write_file(&mut chained, f, dest_root, steam, p, flush, &mut added)
        }
    })();
    if r.is_err() {
        p.sub(added);
    }
    r
}

/// Restore one file from its `<path>.zst` in a package.
fn unpack_one(f: &FileEntry, zst: &Path, dest_root: &Path, steam: bool, p: &Progress, flush: bool) -> io::Result<()> {
    let mut added = 0u64;
    let r = (|| {
        if f.size < SMALL_FILE {
            let mut comp = Vec::new();
            open_read(zst)?.read_to_end(&mut comp)?;
            let data = zstd::stream::decode_all(&comp[..])?;
            if data.len() as u64 != f.size {
                return Err(io::Error::other(format!("{} is damaged on the drive (wrong size after unpacking).", f.path)));
            }
            write_bytes(&data, f, dest_root, steam, flush)?;
            p.add(f.size);
            added = f.size;
            Ok(())
        } else {
            let mut dec = zstd::stream::Decoder::new(open_read(zst)?)?;
            write_file(&mut dec, f, dest_root, steam, p, flush, &mut added)
        }
    })();
    if r.is_err() {
        p.sub(added);
    }
    r
}

/// Copy one file between drives; progress of a failed attempt is taken back.
fn copy_one(f: &FileEntry, src_root: &Path, dest_root: &Path, steam: bool, p: &Progress, flush: bool, mode: Packing) -> io::Result<()> {
    if !is_manifest(&f.path, steam) {
        match mode {
            Packing::Compress => return pack_one(f, src_root, dest_root, steam, p, flush),
            Packing::Decompress => {
                let zst = safe_combine(src_root, &packed_name(&f.path), steam)?;
                if zst.is_file() {
                    return unpack_one(f, &zst, dest_root, steam, p, flush);
                }
            }
            Packing::Plain => {}
        }
    }
    let src_path = safe_combine(src_root, &f.path, steam)?;
    if f.size < SMALL_FILE && !is_manifest(&f.path, steam) {
        let mut data = Vec::with_capacity(f.size as usize);
        open_read(&src_path)?.read_to_end(&mut data)?;
        if data.len() as u64 != f.size {
            return Err(io::Error::other(format!("File changed size since the copy started: {}", f.path)));
        }
        write_bytes(&data, f, dest_root, steam, flush)?;
        p.add(f.size);
        return Ok(());
    }
    let mut src = open_read(&src_path)?;
    if src.metadata()?.len() != f.size {
        return Err(io::Error::other(format!("File changed size since the copy started: {}", f.path)));
    }
    let mut added = 0;
    let r = write_file(&mut src, f, dest_root, steam, p, flush, &mut added);
    if r.is_err() {
        p.sub(added); // take back a failed attempt's progress before the retry
    }
    r
}

/// Parallel copy between drives: one worker streams the big files in order (kind to HDDs and USB sticks)
/// while a pool handles small files concurrently, so per-file overhead overlaps. If either drive is unplugged,
/// the affected files wait for it and are redone from the start (half-written files on a yanked drive can't be trusted).
fn copy_local_parallel(offer: &Offer, src_root: &Path, dest_root: &Path, p: &Progress, log: &(dyn Fn(String) + Sync), flush: bool, mode: Packing) -> io::Result<()> {
    let skip: std::collections::HashSet<usize> = if mode == Packing::Compress { compute_skip_packed(offer, dest_root) } else { compute_skip(offer, dest_root) }.into_iter().collect();
    p.total.store(offer.files.iter().enumerate().filter(|(i, _)| !skip.contains(i)).map(|(_, f)| f.size).sum(), Ordering::Relaxed);
    if !skip.is_empty() {
        log(format!("{} files already present, skipping them.", skip.len()));
    }
    let roots = [src_root, dest_root];
    retry_local(p, &roots, log, &mut || create_dirs(offer, dest_root))?;

    let steam = offer.is_steam();
    let todo = (0..offer.files.len()).filter(|i| !skip.contains(i) && !is_manifest(&offer.files[*i].path, steam));
    let (big, small): (Vec<usize>, Vec<usize>) = todo.partition(|&i| offer.files[i].size >= SMALL_FILE);
    let big = Mutex::new(std::collections::VecDeque::from(big));
    let small = Mutex::new(std::collections::VecDeque::from(small));
    let failure: Mutex<Option<io::Error>> = Mutex::new(None);
    let stop = AtomicBool::new(false);
    let workers = workers_for(&roots);
    let mut big_workers = big_workers_for(&roots);
    if mode == Packing::Compress && big_workers > 1 && std::env::var_os("STEAMXFER_BIG_WORKERS").is_none() {
        // compressing is CPU work: more files at once (Silksong → USB SSD: 4 workers 17.6 s, 12 workers 11.6 s,
        // flat beyond). Hard drives keep one.
        big_workers = std::thread::available_parallelism().map(|n| n.get() / 2).unwrap_or(4).clamp(4, 12);
    }

    std::thread::scope(|scope| {
        for worker in 0..workers + big_workers {
            let (big, small, failure, stop, roots) = (&big, &small, &failure, &stop, &roots);
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    // the first big_workers own the big files, then help with small ones
                    let next = if worker < big_workers { big.lock().unwrap().pop_front().or_else(|| small.lock().unwrap().pop_front()) } else { small.lock().unwrap().pop_front() };
                    let Some(idx) = next else { break };
                    let f = &offer.files[idx];
                    if f.size >= SMALL_FILE {
                        p.set_current(&f.path);
                    }
                    if let Err(e) = retry_local(p, roots, log, &mut || copy_one(f, src_root, dest_root, steam, p, flush, mode)) {
                        failure.lock().unwrap().get_or_insert(e);
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    if let Some(e) = failure.into_inner().unwrap() {
        return Err(e);
    }
    // Manifests last, so Steam never sees a half-copied game.
    for f in offer.files.iter().enumerate().filter(|(i, f)| !skip.contains(i) && is_manifest(&f.path, steam)).map(|(_, f)| f) {
        retry_local(p, &roots, log, &mut || copy_one(f, src_root, dest_root, steam, p, flush, mode))?;
    }
    Ok(())
}

/// Copy a game to a folder on an external drive (resumable). Returns the package folder.
pub fn export(g: &Game, target: &Path, p: &Progress, log: &(dyn Fn(String) + Sync)) -> io::Result<PathBuf> {
    let offer = build_offer(g)?;
    let safe: String = g.name.chars().map(|c| if r#"<>:"/\|?*"#.contains(c) || c.is_control() { '_' } else { c }).collect();
    let id = if g.is_steam() { g.app_id.clone() } else { format!("{}-{}", g.launcher.id(), g.app_id) };
    let pkg = target.join("SteamTransfer").join(format!("{} - {}", sanitize(&id), safe.trim()));
    fs::create_dir_all(&pkg)?;
    let marker = pkg.join(PACKAGE_FILE);
    let _ = fs::remove_file(&marker); // incomplete until rewritten at the end

    let mut offer = offer;
    let data = package_data(&offer, &pkg);
    // Compress when it pays (Unity/indie games often shrink 3×; most AAA games ship already compressed).
    let ratio = if std::env::var("STEAMXFER_PACK").as_deref() == Ok("0") { 1.0 } else { estimate_ratio(&offer, &g.library.path) };
    offer.packed = ratio < 0.9;
    let have: u64 = if offer.packed { compute_skip_packed(&offer, &data) } else { compute_skip(&offer, &data) }.iter().map(|&i| offer.files[i].size).sum();
    let need = if offer.packed { ((offer.total_bytes - have) as f64 * (ratio + 0.08).min(1.0)) as u64 } else { offer.total_bytes - have };
    if offer.packed {
        log(format!("{} compresses well: about {} on the drive instead of {}.", g.name, util::size((offer.total_bytes as f64 * ratio) as u64), util::size(offer.total_bytes)));
    }
    let free = util::free_space(&pkg);
    if free < need {
        return Err(io::Error::other(format!("Not enough space on the drive: need {}, have {}.", util::size(need), util::size(free))));
    }
    copy_local_parallel(&offer, &g.library.path, &data, p, log, true, if offer.packed { Packing::Compress } else { Packing::Plain })?;
    fs::write(&marker, serde_json::to_vec(&offer)?)?;
    Ok(pkg)
}

/// Install a package folder (from a drive) into a local library.
/// Install a package folder (from a drive). `lib.path`: the Steam library, or for other launchers the game folder.
pub fn import(offer: &Offer, package: &Path, lib: &Library, p: &Progress, log: &(dyn Fn(String) + Sync)) -> io::Result<()> {
    copy_local_parallel(offer, &package_data(offer, package), &lib.path, p, log, false, if offer.packed { Packing::Decompress } else { Packing::Plain })
}

/// Where a package keeps its files: Steam packages mirror a library (`steamapps/...`); other games live in `game\`
/// (so a game file can never collide with the package's own transfer.json).
pub fn package_data(offer: &Offer, package: &Path) -> PathBuf {
    if offer.is_steam() { package.to_path_buf() } else { package.join("game") }
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if r#"<>:"/\|?*"#.contains(c) || c.is_control() { '_' } else { c }).collect()
}

/// Delete a game copy from a drive. Only what the package itself lists is removed (its files, then its now-empty
/// folders, then transfer.json — compressed .zst forms included); anything else on the drive — including the app copy next to it — is left alone.
/// Returns the bytes freed.
pub fn delete_package(folder: &Path) -> io::Result<u64> {
    let is_root = folder.parent().is_none() || util::drive_of(folder).as_path() == folder.components().collect::<PathBuf>().as_path();
    if is_root {
        return Err(io::Error::other("Refusing to delete a whole drive."));
    }
    let offer = read_package(folder)?;
    let data = package_data(&offer, folder);
    let steam = offer.is_steam();
    let remove = |p: &Path| -> io::Result<()> {
        match fs::remove_file(p) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                let mut perms = fs::metadata(p)?.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                fs::set_permissions(p, perms)?;
                fs::remove_file(p)
            }
            r => r,
        }
    };
    let mut freed = 0;
    for f in &offer.files {
        for rel in [f.path.clone(), packed_name(&f.path), format!("{}.partial", packed_name(&f.path))] {
            let Ok(p) = safe_combine(&data, &rel, steam) else { continue };
            if let Ok(md) = fs::symlink_metadata(&p) {
                if md.is_file() {
                    remove(&p)?;
                    freed += md.len();
                }
            }
        }
    }
    // folders, deepest first; remove_dir only succeeds on empty ones, so unknown content keeps its folder
    let mut dirs: Vec<PathBuf> = offer.dirs.iter().filter_map(|d| safe_combine(&data, d, steam).ok()).collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        let _ = fs::remove_dir(d);
    }
    let layout: &[&str] = if steam { &[r"steamapps\workshop\content", r"steamapps\workshop", r"steamapps\common", "steamapps"] } else { &["game"] };
    for d in layout {
        let _ = fs::remove_dir(folder.join(d));
    }
    remove(&folder.join(PACKAGE_FILE))?;
    let _ = fs::remove_dir(folder);
    Ok(freed)
}

pub fn read_package(folder: &Path) -> io::Result<Offer> {
    Ok(serde_json::from_slice(&fs::read(folder.join(PACKAGE_FILE))?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_safety() {
        let root = Path::new(r"C:\lib");
        assert!(safe_combine(root, "steamapps/common/Game/a.pak", true).is_ok());
        assert!(safe_combine(root, "steamapps/appmanifest_10.acf", true).is_ok());
        assert!(safe_combine(root, "steamapps/common/../../steam.exe", true).is_err());
        assert!(safe_combine(root, "Windows/evil.dll", true).is_err());
        assert!(safe_combine(root, "steam.exe", true).is_err());
        assert!(safe_combine(root, "steamapps/common/C:/x", true).is_err());
        assert!(safe_combine(root, "steamapps/appmanifest_x/../../a", true).is_err());
        // other launchers: anything inside the game folder, nothing outside it
        let game = Path::new(r"D:\Games\Epic Games\Fortnite");
        assert!(safe_combine(game, "FortniteGame/Binaries/x.exe", false).is_ok());
        assert!(safe_combine(game, ".egstore/1.manifest", false).is_ok());
        assert!(safe_combine(game, "../Other/x", false).is_err());
        assert!(safe_combine(game, "a/../../x", false).is_err());
        assert!(safe_combine(game, "/etc/x", false).is_err());
        assert!(safe_combine(game, "C:/Windows/x", false).is_err());
        assert!(safe_combine(game, "a//b", false).is_err());
    }
    #[test]
    fn wire_format_is_pascal_case() {
        let o = Offer { app_id: "10".into(), files: vec![FileEntry { path: "a".into(), size: 1, mtime: 2 }], ..Default::default() };
        let j = serde_json::to_string(&o).unwrap();
        assert!(j.contains("\"AppId\":\"10\"") && j.contains("\"MTime\":2") && j.contains("\"TotalBytes\""), "{j}");
    }
}
