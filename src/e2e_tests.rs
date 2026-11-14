//! End-to-end tests against a real installed game (skipped if it isn't installed):
//! network send over loopback, resume, pause, USB export with a simulated unplug, and
//! sending from a drive that's unplugged mid-transfer. Uses `subst` to fake a USB drive.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::net::{self, Decision, Host, JobEnd};
use crate::launchers::Launcher;
use crate::steam::{self, Game, Library};
use crate::transfer::{self, Offer, Progress};
use crate::util;

struct TestHost {
    lib: Mutex<Library>,
    ended: Mutex<mpsc::Sender<String>>,
}

impl Host for TestHost {
    fn log(&self, msg: String) {
        println!("  [recv] {msg}");
    }
    fn begin_job(&self, _: String, _: net::Route) -> Option<Arc<Progress>> {
        Some(Arc::new(Progress::new()))
    }
    fn end_job(&self, end: JobEnd) {
        let s = match end {
            JobEnd::Done(m) => format!("ok: {m}"),
            JobEnd::Failed(m) => format!("failed: {m}"),
            JobEnd::Cancelled => "cancelled".into(),
            JobEnd::Declined => "declined".into(),
        };
        let _ = self.ended.lock().unwrap().send(s);
    }
    fn ask_accept(&self, offer: &Offer, _: String) -> Option<Decision> {
        if offer.launcher() == Launcher::App {
            // like the real dialog: apps go into this user's profile through a junction staging folder
            let st = crate::apps::prepare_receive(offer, &|m| println!("  [recv] {m}")).unwrap();
            return Some(Decision { library: Library { path: st.root.clone() }, close_steam: false, staging: Some(Arc::new(st)) });
        }
        Some(Decision { library: self.lib.lock().unwrap().clone(), close_steam: false, staging: None })
    }
    fn after_receive(&self, _: &Offer, _: &Path, _: bool) {}
}

/// The TCP port is one global, so tests that talk over the network take turns.
static NET: Mutex<()> = Mutex::new(());
fn net_turn() -> std::sync::MutexGuard<'static, ()> {
    NET.lock().unwrap_or_else(|e| e.into_inner())
}

fn verify(g: &Game, root: &Path) {
    let offer = transfer::build_offer(g).unwrap();
    let mut bad = 0;
    for f in &offer.files {
        let a = transfer::safe_combine(&g.library.path, &f.path, g.is_steam()).unwrap();
        let b = transfer::safe_combine(root, &f.path, g.is_steam()).unwrap();
        if transfer::is_manifest(&f.path, g.is_steam()) {
            assert!(b.exists(), "missing {}", f.path);
            continue;
        }
        if std::fs::read(&a).unwrap() != std::fs::read(&b).unwrap_or_default() {
            bad += 1;
            println!("MISMATCH {}", f.path);
        }
    }
    assert!(offer.dirs.iter().all(|d| transfer::safe_combine(root, d, g.is_steam()).unwrap().is_dir()), "missing directories");
    println!("verified {} files in {} — {bad} bad", offer.files.len(), root.display());
    assert_eq!(bad, 0);
}

fn subst(on: bool, dir: &Path) {
    let mut c = Command::new("subst");
    if on { c.arg("X:").arg(dir) } else { c.args(["X:", "/d"]) };
    let _ = util::no_window(&mut c).output();
}

/// A fake installed game with lots of small files (+ a few big ones), cached between runs.
fn synthetic_game() -> Game {
    let root = std::env::temp_dir().join("xfer_bench_lib");
    let game_dir = root.join(r"steamapps\common\SmallFiles");
    let manifest = root.join(r"steamapps\appmanifest_999999.acf");
    if !manifest.exists() {
        let mut seed: u64 = 0x9E3779B97F4A7C15;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for i in 0..20_000u32 {
            let dir = game_dir.join(format!("d{:03}", i % 200));
            std::fs::create_dir_all(&dir).unwrap();
            let size = 1024 + (rnd() % (60 * 1024)) as usize; // 1–61 KB
            let data: Vec<u8> = (0..size).map(|_| rnd() as u8).collect();
            std::fs::write(dir.join(format!("f{i}.dat")), data).unwrap();
        }
        for i in 0..3 {
            let data: Vec<u8> = (0..64 * 1024 * 1024).map(|_| rnd() as u8).collect();
            std::fs::write(game_dir.join(format!("big{i}.pak")), data).unwrap();
        }
        std::fs::write(&manifest, "\"AppState\"\n{\n\t\"appid\"\t\t\"999999\"\n\t\"name\"\t\t\"Small Files\"\n\t\"installdir\"\t\t\"SmallFiles\"\n\t\"StateFlags\"\t\t\"4\"\n}\n").unwrap();
    }
    Game { app_id: "999999".into(), name: "Small Files".into(), install_dir: "SmallFiles".into(), size_on_disk: 0, state_flags: 4, library: Library { path: root }, launcher: Launcher::Steam, meta: serde_json::Value::Null }
}

/// cargo test --release bench_small_files -- --ignored --nocapture
#[test]
#[ignore]
fn bench_small_files() {
    let _turn = net_turn();
    let game = synthetic_game();
    let offer = transfer::build_offer(&game).unwrap();
    println!("synthetic game: {} files, {}", offer.files.len(), util::size(offer.total_bytes));
    net::TCP_PORT.store(47823, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("xfer_bench_{}", std::process::id()));
    let recv = tmp.join("recv");
    std::fs::create_dir_all(&recv).unwrap();
    let (tx, rx) = mpsc::channel();
    let host = Arc::new(TestHost { lib: Mutex::new(Library { path: recv.clone() }), ended: Mutex::new(tx) });
    let _server = net::Server::start(host).unwrap();

    let p = Arc::new(Progress::new());
    let t = Instant::now();
    net::send(&game, Ipv4Addr::LOCALHOST, &p, &|m| println!("  {:>6.2}s [send] {m}", t.elapsed().as_secs_f64())).unwrap();
    println!("  {:>6.2}s sender done", t.elapsed().as_secs_f64());
    rx.recv_timeout(Duration::from_secs(600)).unwrap();
    let s = t.elapsed().as_secs_f64();
    println!("BENCH network: {:.1}s  {}/s  {:.0} files/s", s, util::size((p.done() as f64 / s) as u64), offer.files.len() as f64 / s);
    verify(&game, &recv);

    let p = Arc::new(Progress::new());
    let t = Instant::now();
    transfer::export(&game, &tmp.join("drive"), &p, &|_| {}).unwrap();
    let s = t.elapsed().as_secs_f64();
    println!("BENCH drive copy: {:.1}s  {}/s  {:.0} files/s", s, util::size((p.done() as f64 / s) as u64), offer.files.len() as f64 / s);
    verify(&game, &tmp.join(r"drive\SteamTransfer\999999 - Small Files"));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// How compressible are real installed games? Samples 4 MB chunks (weighted by file size) from every game.
/// cargo test --release measure_compressibility -- --ignored --nocapture
#[test]
#[ignore]
fn measure_compressibility() {
    use std::io::{Read, Seek, SeekFrom};
    const CHUNK: usize = 4 * 1024 * 1024;
    for game in steam::games(&steam::libraries()) {
        let Ok(offer) = transfer::build_offer(&game) else { continue };
        let files: Vec<_> = offer.files.iter().filter(|f| f.size > 0).collect();
        if files.is_empty() {
            continue;
        }
        let total: u64 = files.iter().map(|f| f.size).sum();
        let mut seed = 12345u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let (mut raw, mut z1, mut z3, mut lz) = (0usize, 0usize, 0usize, 0usize);
        let (mut t1, mut t3, mut tl) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
        let mut buf = vec![0u8; CHUNK];
        for _ in 0..48 {
            // pick a file weighted by size, then a random 4 MB window in it
            let mut target = rnd() % total;
            let f = files.iter().find(|f| if target < f.size { true } else { target -= f.size; false }).unwrap();
            let Ok(path) = transfer::safe_combine(&game.library.path, &f.path, game.is_steam()) else { continue };
            let Ok(mut file) = std::fs::File::open(path) else { continue };
            let off = if f.size > CHUNK as u64 { rnd() % (f.size - CHUNK as u64) } else { 0 };
            let _ = file.seek(SeekFrom::Start(off));
            let n = file.read(&mut buf).unwrap_or(0);
            if n == 0 {
                continue;
            }
            let data = &buf[..n];
            raw += n;
            let t = Instant::now();
            z1 += zstd::bulk::compress(data, 1).unwrap().len();
            t1 += t.elapsed();
            let t = Instant::now();
            z3 += zstd::bulk::compress(data, 3).unwrap().len();
            t3 += t.elapsed();
            let t = Instant::now();
            lz += lz4_flex::block::compress(data).len();
            tl += t.elapsed();
        }
        let mbps = |d: Duration| raw as f64 / 1e6 / d.as_secs_f64().max(1e-9);
        println!(
            "{:<34} {:>9}  zstd-1 {:>5.1}% ({:>4.0} MB/s/core)  zstd-3 {:>5.1}% ({:>4.0} MB/s)  lz4 {:>5.1}% ({:>4.0} MB/s)",
            game.name,
            util::size(total),
            100.0 * z1 as f64 / raw as f64,
            mbps(t1),
            100.0 * z3 as f64 / raw as f64,
            mbps(t3),
            100.0 * lz as f64 / raw as f64,
            mbps(tl)
        );
    }
}

/// How small can one game get, and at what CPU cost? Samples 4 MB chunks across the game (weighted by size) and
/// compresses them at several zstd levels (single core), plus 1 MB-chunk dedup like Steam's depots.
/// GAME=silksong cargo test --release compress_levels -- --ignored --nocapture
#[test]
#[ignore]
fn compress_levels() {
    use std::io::{Read, Seek, SeekFrom};
    const CHUNK: usize = 4 * 1024 * 1024;
    let want = std::env::var("GAME").unwrap_or_default().to_lowercase();
    let game = steam::games(&steam::libraries()).into_iter().find(|g| g.name.to_lowercase().contains(&want)).expect("game not installed");
    let offer = transfer::build_offer(&game).unwrap();
    let files: Vec<_> = offer.files.iter().filter(|f| f.size > 0).collect();
    let total: u64 = files.iter().map(|f| f.size).sum();
    println!("{}: {} files, {}", game.name, files.len(), util::size(total));
    // biggest files and their share
    let mut by_ext: std::collections::HashMap<String, u64> = Default::default();
    for f in &files {
        let ext = Path::new(&f.path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        *by_ext.entry(ext).or_default() += f.size;
    }
    let mut exts: Vec<_> = by_ext.into_iter().collect();
    exts.sort_by_key(|(_, s)| std::cmp::Reverse(*s));
    println!("by type: {}", exts.iter().take(8).map(|(e, s)| format!(".{e} {}", util::size(*s))).collect::<Vec<_>>().join(", "));

    let mut seed = 4242u64;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let samples: usize = std::env::var("SAMPLES").ok().and_then(|s| s.parse().ok()).unwrap_or(48);
    let mut chunks = Vec::new();
    for _ in 0..samples {
        let mut target = rnd() % total;
        let f = files.iter().find(|f| if target < f.size { true } else { target -= f.size; false }).unwrap();
        let Ok(mut file) = std::fs::File::open(transfer::safe_combine(&game.library.path, &f.path, true).unwrap()) else { continue };
        let off = if f.size > CHUNK as u64 { rnd() % (f.size - CHUNK as u64) } else { 0 };
        let _ = file.seek(SeekFrom::Start(off));
        let mut buf = vec![0u8; CHUNK];
        let n = file.read(&mut buf).unwrap_or(0);
        buf.truncate(n);
        if n > 0 {
            chunks.push(buf);
        }
    }
    let raw: usize = chunks.iter().map(|c| c.len()).sum();
    for level in [1, 3, 6, 9, 12, 15, 19] {
        let t = Instant::now();
        let out: usize = chunks.iter().map(|c| zstd::bulk::compress(c, level).unwrap().len()).sum();
        let secs = t.elapsed().as_secs_f64();
        let t = Instant::now();
        for c in &chunks {
            let z = zstd::bulk::compress(c, level).unwrap();
            let _ = zstd::bulk::decompress(&z, CHUNK).unwrap();
        }
        let dsecs = (t.elapsed().as_secs_f64() - secs).max(1e-6);
        println!(
            "zstd {level:>2}: {:>5.1}%  → whole game ≈ {:>8}   compress {:>6.0} MB/s/core   decompress {:>5.0} MB/s/core",
            100.0 * out as f64 / raw as f64,
            util::size((total as f64 * out as f64 / raw as f64) as u64),
            raw as f64 / 1e6 / secs,
            raw as f64 / 1e6 / dsecs
        );
    }
    // whole-game 1 MB chunk dedup (exact duplicates), like Steam depots
    let mut seen = std::collections::HashSet::new();
    let (mut all, mut dup) = (0u64, 0u64);
    for f in &files {
        let Ok(data) = std::fs::read(transfer::safe_combine(&game.library.path, &f.path, true).unwrap()) else { continue };
        for c in data.chunks(1024 * 1024) {
            all += c.len() as u64;
            let h = c.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3)) ^ (c.len() as u64) << 48;
            if !seen.insert(h) {
                dup += c.len() as u64;
            }
        }
    }
    println!("dedup (1 MB chunks): {} of {} are repeats ({:.1}%)", util::size(dup), util::size(all), 100.0 * dup as f64 / all.max(1) as f64);
}

/// One 2 GB file (incompressible), cached between runs.
fn synthetic_big_game() -> Game {
    let root = std::env::temp_dir().join("xfer_bench_big");
    let manifest = root.join(r"steamapps\appmanifest_999998.acf");
    if !manifest.exists() {
        let dir = root.join(r"steamapps\common\BigFile");
        std::fs::create_dir_all(&dir).unwrap();
        let mut seed: u64 = 0x2545F4914F6CDD1D;
        let mut out = std::io::BufWriter::new(std::fs::File::create(dir.join("data.pak")).unwrap());
        for _ in 0..(2u64 * 1024 * 1024 * 1024 / 8) {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            std::io::Write::write_all(&mut out, &seed.to_le_bytes()).unwrap();
        }
        drop(out);
        std::fs::write(&manifest, "\"AppState\"\n{\n\t\"appid\"\t\t\"999998\"\n\t\"name\"\t\t\"Big File\"\n\t\"installdir\"\t\t\"BigFile\"\n\t\"StateFlags\"\t\t\"4\"\n}\n").unwrap();
    }
    Game { app_id: "999998".into(), name: "Big File".into(), install_dir: "BigFile".into(), size_on_disk: 0, state_flags: 4, library: Library { path: root }, launcher: Launcher::Steam, meta: serde_json::Value::Null }
}

/// Network-send benchmark. BENCH_GAME = an installed app id, "small" (20k small files) or "big" (one 2 GB file).
/// Combine with STEAMXFER_COMPRESS, STEAMXFER_SPLIT, STEAMXFER_THROTTLE_MBPS, STEAMXFER_WORKERS.
/// cargo test --release bench_send -- --ignored --nocapture
#[test]
#[ignore]
fn bench_send() {
    let _turn = net_turn();
    let which = std::env::var("BENCH_GAME").unwrap_or("big".into());
    let game = match which.as_str() {
        "big" => synthetic_big_game(),
        "small" => synthetic_game(),
        id => steam::games(&steam::libraries()).into_iter().find(|g| g.app_id == id).expect("game not installed"),
    };
    net::TCP_PORT.store(47824, Ordering::Relaxed);
    let recv = std::env::temp_dir().join(format!("xfer_bsend_{}", std::process::id()));
    std::fs::create_dir_all(&recv).unwrap();
    let (tx, rx) = mpsc::channel();
    let host = Arc::new(TestHost { lib: Mutex::new(Library { path: recv.clone() }), ended: Mutex::new(tx) });
    let _server = net::Server::start(host).unwrap();
    let p = Arc::new(Progress::new());
    let t = Instant::now();
    net::send(&game, Ipv4Addr::LOCALHOST, &p, &|m| if m.contains("Compression saved") || m.contains("Sending over") { println!("  {m}") }).unwrap();
    rx.recv_timeout(Duration::from_secs(900)).unwrap();
    let s = t.elapsed().as_secs_f64();
    println!("BENCH {}: {} in {s:.1}s = {}/s effective", game.name, util::size(p.done()), util::size((p.done() as f64 / s) as u64));
    verify(&game, &recv);
    let _ = std::fs::remove_dir_all(&recv);
}

/// Drive benchmark: raw sequential speed vs the app's export/import. BENCH_DRIVE=E:\ (a scratch folder is
/// created there and removed afterwards). BENCH_GAMES=big,small,1030300 picks what to copy.
/// cargo test --release bench_drive -- --ignored --nocapture
#[test]
#[ignore]
fn bench_drive() {
    use std::io::{Read, Write};
    let drive = PathBuf::from(std::env::var("BENCH_DRIVE").expect("set BENCH_DRIVE"));
    let scratch = drive.join("SteamTransfer-bench");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let rate = |bytes: u64, t: Instant| format!("{:>7.1} MB/s", bytes as f64 / 1e6 / t.elapsed().as_secs_f64());

    if std::env::var("BENCH_RAW").as_deref() != Ok("0") {
        // raw sequential write / read of 4 GB (incompressible), 8 MB writes, flushed
        let path = scratch.join("raw.bin");
        let mut buf = vec![0u8; 8 * 1024 * 1024];
        let mut seed = 0x9E3779B97F4A7C15u64;
        for c in buf.chunks_mut(8) {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            c.copy_from_slice(&seed.to_le_bytes());
        }
        let total = 4u64 * 1024 * 1024 * 1024;
        let t = Instant::now();
        let mut f = std::fs::File::create(&path).unwrap();
        for _ in 0..total / buf.len() as u64 {
            f.write_all(&buf).unwrap();
        }
        f.sync_all().unwrap();
        drop(f);
        println!("RAW sequential write 4 GB: {}", rate(total, t));
        let t = Instant::now();
        let mut f = transfer::open_read(&path).unwrap();
        let mut n = 0u64;
        loop {
            let r = f.read(&mut buf).unwrap();
            if r == 0 {
                break;
            }
            n += r as u64;
        }
        println!("RAW sequential read  4 GB: {}  (may be partly cached)", rate(n, t));
        // true device read speed: unbuffered (bypasses the RAM cache), sector-aligned buffer, 4 reads in flight
        {
            use std::os::windows::fs::{FileExt, OpenOptionsExt};
            const NO_BUFFERING: u32 = 0x2000_0000;
            let layout = std::alloc::Layout::from_size_align(8 * 1024 * 1024, 4096).unwrap();
            let t = Instant::now();
            let total = std::thread::scope(|s| {
                (0..4u64)
                    .map(|k| {
                        let path = path.clone();
                        s.spawn(move || {
                            let f = std::fs::OpenOptions::new().read(true).custom_flags(NO_BUFFERING).open(&path).unwrap();
                            let ptr = unsafe { std::alloc::alloc(layout) };
                            let buf = unsafe { std::slice::from_raw_parts_mut(ptr, layout.size()) };
                            let mut got = 0u64;
                            let mut off = k * layout.size() as u64;
                            while off < total {
                                got += f.seek_read(buf, off).unwrap() as u64;
                                off += 4 * layout.size() as u64;
                            }
                            unsafe { std::alloc::dealloc(ptr, layout) };
                            got
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|h| h.join().unwrap())
                    .sum::<u64>()
            });
            println!("RAW unbuffered read  4 GB: {}  (real device speed)", rate(total, t));
        }
        std::fs::remove_file(&path).unwrap();
    }

    let games = std::env::var("BENCH_GAMES").unwrap_or("big,small".into());
    for which in games.split(',').filter(|w| *w != "none") {
        let game = match which {
            "big" => synthetic_big_game(),
            "small" => synthetic_game(),
            id => steam::games(&steam::libraries()).into_iter().find(|g| g.app_id == id).expect("game not installed"),
        };
        let p = Progress::new();
        let t = Instant::now();
        let pkg = transfer::export(&game, &scratch, &p, &|_| {}).unwrap();
        println!("EXPORT {:<26} {:>9}  {}  {:.1}s", game.name, util::size(p.done()), rate(p.done(), t), t.elapsed().as_secs_f64());
        let offer = transfer::read_package(&pkg).unwrap();
        let stored: u64 = walkdir::WalkDir::new(&pkg).into_iter().flatten().filter(|e| e.file_type().is_file()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum();
        println!("ON DRIVE {:<24} {:>9} of {} ({})", game.name, util::size(stored), util::size(offer.total_bytes), if offer.packed { "compressed" } else { "plain" });
        let back = std::env::temp_dir().join(format!("xfer_import_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&back);
        let p = Progress::new();
        let t = Instant::now();
        transfer::import(&offer, &pkg, &Library { path: back.clone() }, &p, &|_| {}).unwrap();
        println!("IMPORT {:<26} {:>9}  {}  {:.1}s", game.name, util::size(p.done()), rate(p.done(), t), t.elapsed().as_secs_f64());
        if std::env::var("BENCH_VERIFY").as_deref() == Ok("1") {
            verify(&game, &back);
        }
        let _ = std::fs::remove_dir_all(&back);
        let _ = std::fs::remove_dir_all(&pkg);
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

/// Non-Steam games end to end, without any real launcher installed:
/// a fake Epic install (manifest, DLC manifest, LauncherInstalled.dat, .egstore) is detected, sent over the network,
/// and must arrive byte-identical *and* be registered in the receiving PC's Epic records at the new location.
/// Then a plain folder game goes through a USB-drive package and back.
#[test]
fn other_launchers_end_to_end() {
    let _turn = net_turn();
    use serde_json::json;
    let tmp = std::env::temp_dir().join(format!("xfer_launchers_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let sender_epic = tmp.join("sender_epic");
    let receiver_epic = tmp.join("receiver_epic");
    let game_dir = tmp.join(r"source\Epic Games\FakeGame");

    // ---- a fake Epic game on the "sending PC"
    let mut seed = 7u64;
    for (i, rel) in ["FakeGame.exe", r"Content\Paks\data.pak", r"Content\Movies\intro.bk2", r".egstore\ABC123.manifest", r"Engine\Binaries\a.dll"].iter().enumerate() {
        let p = game_dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let size = [50_000usize, 6 * 1024 * 1024, 200_000, 4096, 1500][i];
        let data: Vec<u8> = (0..size)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        std::fs::write(p, data).unwrap();
    }
    std::fs::create_dir_all(game_dir.join("EmptyFolder")).unwrap();
    let manifests = sender_epic.join(r"EpicGamesLauncher\Data\Manifests");
    std::fs::create_dir_all(&manifests).unwrap();
    std::fs::create_dir_all(sender_epic.join("UnrealEngineLauncher")).unwrap();
    let loc = game_dir.to_string_lossy().to_string();
    let item = |app: &str, guid: &str, name: &str, main: &str| {
        json!({ "FormatVersion": 0, "bIsIncompleteInstall": false, "AppName": app, "MainGameAppName": main, "DisplayName": name,
                "InstallationGuid": guid, "InstallLocation": loc, "ManifestLocation": format!("{loc}\\.egstore"),
                "CatalogNamespace": "ns1", "CatalogItemId": "item1", "AppVersionString": "1.2.3", "InstallSize": 6_500_000 })
    };
    std::fs::write(manifests.join("GUID-BASE.item"), serde_json::to_vec(&item("Fake", "GUID-BASE", "Fake Epic Game", "Fake")).unwrap()).unwrap();
    std::fs::write(manifests.join("GUID-DLC.item"), serde_json::to_vec(&item("FakeDlc", "GUID-DLC", "Fake DLC", "Fake")).unwrap()).unwrap();
    std::fs::write(
        sender_epic.join(r"UnrealEngineLauncher\LauncherInstalled.dat"),
        serde_json::to_vec(&json!({ "InstallationList": [ { "InstallLocation": loc, "NamespaceId": "ns1", "ItemId": "item1", "ArtifactId": "Fake", "AppVersion": "1.2.3", "AppName": "Fake" } ] })).unwrap(),
    )
    .unwrap();

    // detection (the DLC is folded into its base game)
    unsafe { std::env::set_var("STEAMXFER_EPIC_DATA", &sender_epic) };
    let games = crate::launchers::epic_games();
    assert_eq!(games.len(), 1, "one Epic game expected (DLC folded in)");
    let game = games.into_iter().next().unwrap();
    assert_eq!(game.launcher, Launcher::Epic);
    assert_eq!(game.name, "Fake Epic Game");
    assert_eq!(game.meta["dlcs"].as_array().unwrap().len(), 1);

    // ---- send it to the "receiving PC" (its Epic records live elsewhere)
    unsafe { std::env::set_var("STEAMXFER_EPIC_DATA", &receiver_epic) };
    std::fs::create_dir_all(receiver_epic.join(r"UnrealEngineLauncher")).unwrap();
    std::fs::write(receiver_epic.join(r"UnrealEngineLauncher\LauncherInstalled.dat"), br#"{"InstallationList":[{"AppName":"SomethingElse","InstallLocation":"D:\\X"}]}"#).unwrap();
    let dest = tmp.join(r"dest\Games\Epic Games\FakeGame");
    net::TCP_PORT.store(47825, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    let host = Arc::new(TestHost { lib: Mutex::new(Library { path: dest.clone() }), ended: Mutex::new(tx) });
    let _server = net::Server::start(host).unwrap();
    let p = Arc::new(Progress::new());
    net::send(&game, Ipv4Addr::LOCALHOST, &p, &|m| println!("  [send] {m}")).unwrap();
    assert!(rx.recv_timeout(Duration::from_secs(60)).unwrap().starts_with("ok"));
    verify(&game, &dest);
    assert!(dest.join("EmptyFolder").is_dir(), "empty folders travel too");

    // registered with the receiving Epic at the new location
    let dest_s = dest.to_string_lossy().to_string();
    let base: serde_json::Value = serde_json::from_slice(&std::fs::read(receiver_epic.join(r"EpicGamesLauncher\Data\Manifests\GUID-BASE.item")).unwrap()).unwrap();
    assert_eq!(base["InstallLocation"], json!(dest_s));
    assert_eq!(base["ManifestLocation"], json!(format!("{dest_s}\\.egstore")));
    let dlc: serde_json::Value = serde_json::from_slice(&std::fs::read(receiver_epic.join(r"EpicGamesLauncher\Data\Manifests\GUID-DLC.item")).unwrap()).unwrap();
    assert_eq!(dlc["InstallLocation"], json!(dest_s));
    let dat: serde_json::Value = serde_json::from_slice(&std::fs::read(receiver_epic.join(r"UnrealEngineLauncher\LauncherInstalled.dat")).unwrap()).unwrap();
    let list = dat["InstallationList"].as_array().unwrap();
    assert!(list.iter().any(|e| e["AppName"] == "SomethingElse"), "other games' entries are kept");
    assert!(list.iter().any(|e| e["AppName"] == "Fake" && e["InstallLocation"] == json!(dest_s)), "game registered at the new folder");
    println!("EPIC: detected, sent, verified and registered at {dest_s}");

    // ---- a plain folder game through a USB-drive package and back
    let folder = crate::launchers::folder_game(&game_dir);
    assert_eq!(folder.launcher, Launcher::Other);
    let pp = Progress::new();
    let pkg = transfer::export(&folder, &tmp.join("usb"), &pp, &|_| {}).unwrap();
    assert!(pkg.join("game").join("FakeGame.exe").is_file(), "folder games are stored under game\\ in a package");
    let offer = transfer::read_package(&pkg).unwrap();
    assert_eq!(offer.launcher(), Launcher::Other);
    let dest2 = tmp.join(r"dest2\MyGame");
    transfer::import(&offer, &pkg, &Library { path: dest2.clone() }, &Progress::new(), &|_| {}).unwrap();
    verify(&folder, &dest2);
    println!("FOLDER: exported to a drive package and installed back, verified");

    // ---- deleting a copy from the drive removes exactly the package, nothing else
    let usb = tmp.join("usb");
    std::fs::write(pkg.join("my notes.txt"), b"keep me").unwrap(); // something the user put there
    let app_copy = usb.join(r"SteamTransfer\SteamTransfer.exe");
    std::fs::write(&app_copy, b"app").unwrap();
    let freed = transfer::delete_package(&pkg).unwrap();
    assert_eq!(freed, offer.total_bytes, "every game byte freed");
    assert!(!pkg.join("game").exists(), "game files and folders gone");
    assert!(!pkg.join(transfer::PACKAGE_FILE).exists(), "package marker gone");
    assert!(pkg.join("my notes.txt").is_file(), "unrelated files are kept");
    assert!(app_copy.is_file(), "the app copy on the drive is kept");
    assert!(transfer::delete_package(Path::new(r"C:\")).is_err(), "never a whole drive");
    // a Steam-layout package also leaves nothing behind
    let steam_pkg = transfer::export(&synthetic_tiny_steam(&tmp), &usb, &Progress::new(), &|_| {}).unwrap();
    transfer::delete_package(&steam_pkg).unwrap();
    assert!(!steam_pkg.exists(), "Steam package folder removed completely");
    println!("DELETE: package removed, unrelated files and the app copy kept");

    unsafe { std::env::remove_var("STEAMXFER_EPIC_DATA") };
    let _ = std::fs::remove_dir_all(&tmp);
}

/// A Windows app (per-user install) end to end between two fake user profiles: program folder + its AppData folders
/// (caches left behind) + registry settings, over the network and through a drive package. The paths in its
/// registry values must be rewritten for the receiving user, and the sender's files must be untouched afterwards
/// (staging junctions are unlinked, never followed when cleaning up).
#[test]
fn apps_end_to_end() {
    let _turn = net_turn();
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let tmp = std::env::temp_dir().join(format!("xfer_apps_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let (prof_a, prof_b, prof_c) = (tmp.join("UserA"), tmp.join("UserB"), tmp.join("UserC"));
    let install_a = prof_a.join(r"AppData\Local\Programs\XferTestApp");
    let files: [(&str, usize); 4] = [(r"AppData\Local\Programs\XferTestApp\XferTestApp.exe", 300_000), (r"AppData\Local\Programs\XferTestApp\resources\app.asar", 5 * 1024 * 1024), (r"AppData\Roaming\XferTestApp\settings.json", 900), (r"AppData\Local\XferTestApp\state\db.bin", 70_000)];
    let mut seed = 99u64;
    for (rel, size) in files {
        let p = prof_a.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let data: Vec<u8> = (0..size)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        std::fs::write(p, data).unwrap();
    }
    let caches = [r"AppData\Roaming\XferTestApp\Cache\big.tmp", r"AppData\Roaming\XferTestApp\GPUCache\x"];
    for c in caches {
        std::fs::create_dir_all(prof_a.join(c).parent().unwrap()).unwrap();
        std::fs::write(prof_a.join(c), vec![1u8; 100_000]).unwrap();
    }

    // registered like a per-user installer would (real HKCU, unique test key names, removed at the end)
    let key = format!("XferTestApp_{}", std::process::id());
    let sw_name = "XferTestApp";
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (u, _) = hkcu.create_subkey(format!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{key}")).unwrap();
    u.set_value("DisplayName", &"Xfer Test App").unwrap();
    u.set_value("Publisher", &"Xfer Labs Inc.").unwrap();
    u.set_value("InstallLocation", &install_a.to_string_lossy().to_string()).unwrap();
    u.set_value("DisplayIcon", &format!("{}\\XferTestApp.exe,0", install_a.display())).unwrap();
    let (s, _) = hkcu.create_subkey(format!(r"Software\{sw_name}\Prefs")).unwrap();
    s.set_value("LastFile", &prof_a.join(r"Documents\notes.txt").to_string_lossy().to_string()).unwrap();
    s.set_value("Volume", &7u32).unwrap();
    let cleanup_registry = || {
        let _ = hkcu.delete_subkey_all(format!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{key}"));
        let _ = hkcu.delete_subkey_all(format!(r"Software\{sw_name}"));
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // ---- detection on "user A"
        unsafe { std::env::set_var("STEAMXFER_PROFILE", &prof_a) };
        let app = crate::apps::installed_apps().into_iter().find(|g| g.name == "Xfer Test App").expect("test app not detected");
        assert_eq!(app.launcher, Launcher::App);
        let data = app.meta["app"]["data"].as_array().unwrap();
        assert_eq!(data.len(), 2, "roaming + local data folders: {data:?}");
        let (staged, staging) = crate::apps::prepare_send(&app, &|m| println!("  [send] {m}")).unwrap();
        let offer = transfer::build_offer(&staged).unwrap();
        assert_eq!(offer.files.len(), 4, "program + data, no caches: {:?}", offer.files.iter().map(|f| &f.path).collect::<Vec<_>>());
        assert!(offer.meta["app"]["registry"].as_str().unwrap().contains("LastFile"));

        // ---- over the network to "user B"
        unsafe { std::env::set_var("STEAMXFER_PROFILE", &prof_b) };
        net::TCP_PORT.store(47826, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        let host = Arc::new(TestHost { lib: Mutex::new(Library { path: tmp.join("unused") }), ended: Mutex::new(tx) });
        let _server = net::Server::start(host).unwrap();
        net::send(&staged, Ipv4Addr::LOCALHOST, &Arc::new(Progress::new()), &|m| println!("  [send] {m}")).unwrap();
        let end = rx.recv_timeout(Duration::from_secs(60)).unwrap();
        assert!(end.starts_with("ok"), "{end}");
        for (rel, _) in files {
            assert_eq!(std::fs::read(prof_a.join(rel)).unwrap(), std::fs::read(prof_b.join(rel)).unwrap_or_default(), "{rel}");
        }
        for c in caches {
            assert!(!prof_b.join(c).exists(), "cache {c} must stay behind");
        }
        // registry values now point into user B's profile
        let s = hkcu.open_subkey(format!(r"Software\{sw_name}\Prefs")).unwrap();
        assert_eq!(s.get_value::<String, _>("LastFile").unwrap(), prof_b.join(r"Documents\notes.txt").to_string_lossy());
        assert_eq!(s.get_value::<u32, _>("Volume").unwrap(), 7);
        let u = hkcu.open_subkey(format!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{key}")).unwrap();
        assert_eq!(u.get_value::<String, _>("InstallLocation").unwrap(), prof_b.join(r"AppData\Local\Programs\XferTestApp").to_string_lossy());
        assert!(!prof_b.join(r"AppData\Local\SteamTransfer\staging").read_dir().is_ok_and(|mut d| d.next().is_some()), "receiving staging cleaned up");
        println!("APP: sent user A → user B, files + settings + registry (paths rewritten), caches skipped");

        // ---- through a drive package to "user C"
        let pkg = transfer::export(&staged, &tmp.join("usb"), &Progress::new(), &|_| {}).unwrap();
        let offer = transfer::read_package(&pkg).unwrap();
        assert_eq!(offer.launcher(), Launcher::App);
        unsafe { std::env::set_var("STEAMXFER_PROFILE", &prof_c) };
        let st = crate::apps::prepare_receive(&offer, &|_| {}).unwrap();
        transfer::import(&offer, &pkg, &Library { path: st.root.clone() }, &Progress::new(), &|_| {}).unwrap();
        crate::launchers::register(offer.launcher(), &offer.meta, &st.root, &|m| println!("  [import] {m}")).unwrap();
        drop(st);
        for (rel, _) in files {
            assert_eq!(std::fs::read(prof_a.join(rel)).unwrap(), std::fs::read(prof_c.join(rel)).unwrap_or_default(), "{rel}");
        }
        assert_eq!(hkcu.open_subkey(format!(r"Software\{sw_name}\Prefs")).unwrap().get_value::<String, _>("LastFile").unwrap(), prof_c.join(r"Documents\notes.txt").to_string_lossy());
        println!("APP: drive package → user C, verified");

        // ---- the sender's own files are untouched after its staging folder is removed
        drop(staging);
        for (rel, _) in files {
            assert!(prof_a.join(rel).is_file(), "sender lost {rel}");
        }
        assert!(!prof_a.join(r"AppData\Local\SteamTransfer\staging").read_dir().is_ok_and(|mut d| d.next().is_some()), "sending staging cleaned up");
    }));
    unsafe { std::env::remove_var("STEAMXFER_PROFILE") };
    cleanup_registry();
    let _ = std::fs::remove_dir_all(&tmp);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

/// What this PC's apps look like to the app list. cargo test --release list_apps -- --ignored --nocapture
#[test]
#[ignore]
fn list_apps() {
    for g in crate::apps::installed_apps() {
        let data: Vec<String> = g.meta["app"]["data"].as_array().unwrap().iter().map(|d| format!("{}\\{}", d["base"].as_str().unwrap(), d["path"].as_str().unwrap())).collect();
        println!("{:<40} {:>9}  {}  + {:?}", g.name, util::size(g.size_on_disk), g.library.path.display(), data);
    }
}

/// Compressed drive packages: compressible files are stored as .zst (much smaller), incompressible ones as-is;
/// re-exporting skips everything already there; installing restores every byte; deleting removes both forms.
#[test]
fn packed_drive_package() {
    let _turn = net_turn(); // STEAMXFER_PACK is process-wide; end_to_end turns it off while it runs
    let tmp = std::env::temp_dir().join(format!("xfer_packed_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let src = tmp.join(r"games\PackMe");
    let words = ["sprite", "level", "enemy", "boss", "silk", "needle", "bench", "map", "0.25", "true", "{", "}", "\"name\":"];
    let mut seed = 1234u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut text = |len: usize| {
        let mut s = String::with_capacity(len + 16);
        while s.len() < len {
            s.push_str(words[(rnd() % words.len() as u64) as usize]);
            s.push(' ');
        }
        s.truncate(len);
        s.into_bytes()
    };
    for i in 0..300 {
        let p = src.join(format!(r"Data\level{:02}\obj{i}.json", i % 12));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text(2_000 + i * 150)).unwrap();
    }
    std::fs::write(src.join("big.bundle"), text(12 * 1024 * 1024)).unwrap();
    let noise: Vec<u8> = {
        let mut s = 99u64;
        (0..3 * 1024 * 1024)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect()
    };
    std::fs::write(src.join("movie.bk2"), &noise).unwrap();
    std::fs::write(src.join("empty.txt"), b"").unwrap();

    let game = crate::launchers::folder_game(&src);
    let usb = tmp.join("usb");
    let pkg = transfer::export(&game, &usb, &Progress::new(), &|m| println!("  [export] {m}")).unwrap();
    let offer = transfer::read_package(&pkg).unwrap();
    assert!(offer.packed, "text-like game should be packed");
    let data = pkg.join("game");
    assert!(data.join("big.bundle.zst").is_file() && !data.join("big.bundle").exists(), "compressible file stored as .zst only");
    assert!(data.join("movie.bk2").is_file() && !data.join("movie.bk2.zst").exists(), "incompressible file stored as-is");
    assert!(data.join("empty.txt").is_file());
    let stored: u64 = walkdir::WalkDir::new(&data).into_iter().flatten().filter(|e| e.file_type().is_file()).map(|e| e.metadata().unwrap().len()).sum();
    println!("PACKED: {} → {} on the drive", util::size(offer.total_bytes), util::size(stored));
    assert!(stored < offer.total_bytes * 6 / 10, "should be much smaller: {stored} of {}", offer.total_bytes);

    // re-export: nothing left to do
    let again = Progress::new();
    transfer::export(&game, &usb, &again, &|_| {}).unwrap();
    assert_eq!(again.total.load(Ordering::Relaxed), 0, "everything already on the drive");

    // install from it: identical files
    let dest = tmp.join(r"dest\PackMe");
    let p = Progress::new();
    transfer::import(&offer, &pkg, &Library { path: dest.clone() }, &p, &|_| {}).unwrap();
    assert_eq!(p.done(), offer.total_bytes, "progress counts game bytes, not stored bytes");
    verify(&game, &dest);

    // delete: both forms gone, bytes freed = what was stored
    let freed = transfer::delete_package(&pkg).unwrap();
    assert_eq!(freed, stored);
    assert!(!pkg.exists(), "package removed completely");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The drive always ends up with a working app copy: missing, deleted, cut/moved away, half-copied or older →
/// (re)copied; identical → left alone; newer → left alone; in use → reported, nothing broken left behind.
#[test]
fn app_copy_on_drive() {
    use crate::installer::{AppCopy, ensure_app_copy, exe_version};
    let drive = std::env::temp_dir().join(format!("xfer_appcopy_{}", std::process::id()));
    let folder = drive.join("SteamTransfer");
    let _ = std::fs::remove_dir_all(&drive);
    let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    let dest = folder.join("SteamTransfer.exe");

    assert_eq!(ensure_app_copy(&folder).unwrap(), AppCopy::Copied, "first time: copied");
    assert_eq!(std::fs::read(&dest).unwrap(), me, "a byte-exact working copy");
    assert_eq!(ensure_app_copy(&folder).unwrap(), AppCopy::Present, "already there: left alone");

    std::fs::write(&dest, &me[..me.len() / 3]).unwrap(); // half-copied / damaged
    assert_eq!(ensure_app_copy(&folder).unwrap(), AppCopy::Updated, "damaged copy: replaced");
    assert_eq!(std::fs::read(&dest).unwrap(), me);

    std::fs::remove_file(&dest).unwrap(); // someone deleted it
    assert_eq!(ensure_app_copy(&folder).unwrap(), AppCopy::Copied, "deleted: put back");

    let moved = drive.join("moved-away.exe"); // someone cut & pasted it off the drive
    std::fs::rename(&dest, &moved).unwrap();
    assert_eq!(ensure_app_copy(&folder).unwrap(), AppCopy::Copied, "moved away: put back");
    assert!(moved.is_file(), "the moved copy is not touched");

    // a different exe with a version number: replaced if older than us, kept if newer
    let notepad = PathBuf::from(std::env::var("WINDIR").unwrap_or(r"C:\Windows".into())).join(r"System32\notepad.exe");
    if notepad.is_file() {
        std::fs::copy(&notepad, &dest).unwrap();
        let expected = match (exe_version(&notepad), exe_version(&std::env::current_exe().unwrap())) {
            (Some(theirs), Some(mine)) if theirs > mine => AppCopy::NewerThere,
            _ => AppCopy::Updated,
        };
        assert_eq!(ensure_app_copy(&folder).unwrap(), expected);
        println!("version check: notepad {:?} vs this build {:?} → {expected:?}", exe_version(&notepad), exe_version(&std::env::current_exe().unwrap()));
    }

    // the copy on the drive is running/locked: an error, and no half-written file left over
    std::fs::write(&dest, b"old").unwrap();
    {
        use std::os::windows::fs::OpenOptionsExt;
        let _lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&dest).unwrap();
        assert!(ensure_app_copy(&folder).is_err(), "locked copy: reported, not silently broken");
        assert!(!folder.join("SteamTransfer.exe.partial").exists(), "no temp file left behind");
    }
    let _ = std::fs::remove_dir_all(&drive);
    println!("APP COPY: copied, kept, repaired, restored after delete and after cut/paste, locked case handled");
}

/// A tiny fake Steam game (library layout with a manifest) inside `dir`.
fn synthetic_tiny_steam(dir: &Path) -> Game {
    let lib = dir.join("tiny_steam_lib");
    let game = lib.join(r"steamapps\common\Tiny\bin");
    std::fs::create_dir_all(&game).unwrap();
    std::fs::write(game.join("tiny.exe"), vec![7u8; 30_000]).unwrap();
    std::fs::write(lib.join(r"steamapps\appmanifest_424242.acf"), "\"AppState\"\n{\n\t\"appid\"\t\t\"424242\"\n\t\"name\"\t\t\"Tiny\"\n\t\"installdir\"\t\t\"Tiny\"\n}\n").unwrap();
    Game { app_id: "424242".into(), name: "Tiny".into(), install_dir: "Tiny".into(), size_on_disk: 0, state_flags: 4, library: Library { path: lib }, launcher: Launcher::Steam, meta: serde_json::Value::Null }
}

fn wallpaper_engine() -> Option<Game> {
    steam::games(&steam::libraries()).into_iter().find(|g| g.app_id == "431960")
}

/// Interop: send to a C# receiver listening on INTEROP_PORT (run manually: cargo test interop_send -- --ignored).
#[test]
#[ignore]
fn interop_send() {
    let _turn = net_turn();
    let game = wallpaper_engine().expect("needs Wallpaper Engine");
    let port: u16 = std::env::var("INTEROP_PORT").unwrap().parse().unwrap();
    net::TCP_PORT.store(port, Ordering::Relaxed);
    let p = Arc::new(Progress::new());
    net::send(&game, Ipv4Addr::LOCALHOST, &p, &|m| println!("  [rust send] {m}")).unwrap();
    verify(&game, Path::new(&std::env::var("INTEROP_DEST").unwrap()));
    println!("INTEROP SEND OK");
}

/// Interop: receive one game from the C# sender on INTEROP_PORT.
#[test]
#[ignore]
fn interop_receive() {
    let _turn = net_turn();
    let game = wallpaper_engine().expect("needs Wallpaper Engine");
    let port: u16 = std::env::var("INTEROP_PORT").unwrap().parse().unwrap();
    net::TCP_PORT.store(port, Ordering::Relaxed);
    let dest = PathBuf::from(std::env::var("INTEROP_DEST").unwrap());
    std::fs::create_dir_all(&dest).unwrap();
    let (tx, rx) = mpsc::channel();
    let host = Arc::new(TestHost { lib: Mutex::new(Library { path: dest.clone() }), ended: Mutex::new(tx) });
    let _server = net::Server::start(host).unwrap();
    println!("READY");
    let end = rx.recv_timeout(Duration::from_secs(180)).unwrap();
    println!("rust receiver: {end}");
    assert!(end.starts_with("ok"));
    verify(&game, &dest);
    println!("INTEROP RECEIVE OK");
}

#[test]
fn end_to_end() {
    let _turn = net_turn();
    // step 4 sends straight from the drive package, so it must hold plain files
    unsafe { std::env::set_var("STEAMXFER_PACK", "0") };
    struct Unset;
    impl Drop for Unset {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("STEAMXFER_PACK") };
        }
    }
    let _unset = Unset;
    let libs = steam::libraries();
    let Some(game) = steam::games(&libs).into_iter().find(|g| g.app_id == "431960") else {
        println!("Wallpaper Engine not installed; skipping end-to-end test");
        return;
    };
    net::TCP_PORT.store(47821, Ordering::Relaxed); // don't clash with a running copy of the app
    let tmp = std::env::temp_dir().join(format!("xfer_rs_{}", std::process::id()));
    let recv = tmp.join("recv");
    std::fs::create_dir_all(&recv).unwrap();
    let (tx, rx) = mpsc::channel();
    let host = Arc::new(TestHost { lib: Mutex::new(Library { path: recv.clone() }), ended: Mutex::new(tx) });
    let _server = net::Server::start(host.clone()).unwrap();
    let log = |m: String| println!("  [send] {m}");
    let lo = Ipv4Addr::LOCALHOST;

    // 1) network send
    let p = Arc::new(Progress::new());
    let t = Instant::now();
    net::send(&game, lo, &p, &log).unwrap();
    assert!(rx.recv_timeout(Duration::from_secs(30)).unwrap().starts_with("ok"));
    let secs = t.elapsed().as_secs_f64();
    println!("NETWORK: {} in {secs:.1}s = {}/s", util::size(p.done()), util::size((p.done() as f64 / secs) as u64));
    verify(&game, &recv);

    // 2) resume: everything already there
    let p = Arc::new(Progress::new());
    net::send(&game, lo, &p, &log).unwrap();
    rx.recv_timeout(Duration::from_secs(30)).unwrap();
    println!("RESEND moved {} bytes (manifests only)", p.done());
    assert!(p.done() < 64 * 1024);

    // 3) export to a "USB drive" (X: via subst), unplug at ~30%, plug back in
    let usb = tmp.join("usbdisk");
    std::fs::create_dir_all(&usb).unwrap();
    subst(false, &usb);
    subst(true, &usb);
    assert!(Path::new(r"X:\").is_dir(), "subst X: failed");
    let p = Arc::new(Progress::new());
    let pg = game.clone();
    let pp = p.clone();
    let export = std::thread::spawn(move || transfer::export(&pg, Path::new(r"X:\"), &pp, &|m| println!("  [export] {m}")));
    while p.total.load(Ordering::Relaxed) == 0 || p.done() < p.total.load(Ordering::Relaxed) * 3 / 10 {
        std::thread::sleep(Duration::from_millis(10));
    }
    subst(false, &usb);
    println!("UNPLUG X: at {}", util::size(p.done()));
    std::thread::sleep(Duration::from_secs(4));
    println!("while unplugged: waiting = {:?}", p.waiting());
    assert!(p.waiting().is_some(), "export should be waiting for the drive");
    subst(true, &usb);
    println!("REPLUG X:");
    let pkg = export.join().unwrap().unwrap();
    let pkg_real: PathBuf = usb.join(pkg.strip_prefix(r"X:\").unwrap());
    verify(&game, &pkg_real);
    let offer = transfer::read_package(&pkg_real).unwrap();
    assert_eq!(offer.app_id, game.app_id);

    // 4) network send FROM the drive: pause, resume, then unplug the source mid-send
    let recv2 = tmp.join("recv2");
    std::fs::create_dir_all(&recv2).unwrap();
    *host.lib.lock().unwrap() = Library { path: recv2.clone() };
    let from_drive = Game { library: Library { path: pkg.clone() }, ..game.clone() };
    let p = Arc::new(Progress::new());
    let pp = p.clone();
    let sender = std::thread::spawn(move || net::send(&from_drive, lo, &pp, &|m| println!("  [send] {m}")));
    while p.total.load(Ordering::Relaxed) == 0 || p.done() < p.total.load(Ordering::Relaxed) / 5 {
        std::thread::sleep(Duration::from_millis(5));
    }
    p.pause();
    // writes already in flight (stuck in a full socket buffer) may still finish; nothing new may start
    std::thread::sleep(Duration::from_millis(1500));
    let d1 = p.done();
    std::thread::sleep(Duration::from_millis(1500));
    println!("PAUSED: moved {} bytes in 1.5 s while paused", p.done() - d1);
    assert_eq!(p.done(), d1);
    p.resume();
    while p.done() < p.total.load(Ordering::Relaxed) / 2 {
        std::thread::sleep(Duration::from_millis(5));
    }
    subst(false, &usb);
    println!("UNPLUG source X: at {}", util::size(p.done()));
    std::thread::sleep(Duration::from_secs(4));
    println!("while unplugged: waiting = {:?}", p.waiting());
    subst(true, &usb);
    println!("REPLUG X:");
    sender.join().unwrap().unwrap();
    assert!(rx.recv_timeout(Duration::from_secs(60)).unwrap().starts_with("ok"));
    verify(&game, &recv2);

    subst(false, &usb);
    let _ = std::fs::remove_dir_all(&tmp);
    println!("END TO END OK");
}
