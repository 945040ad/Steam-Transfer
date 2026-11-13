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
    fn begin_job(&self, _: String) -> Option<Arc<Progress>> {
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
    fn ask_accept(&self, _: &Offer, _: String) -> Option<Decision> {
        Some(Decision { library: self.lib.lock().unwrap().clone(), close_steam: false })
    }
    fn after_receive(&self, _: &Offer, _: bool) {}
}

fn verify(g: &Game, root: &Path) {
    let offer = transfer::build_offer(g).unwrap();
    let mut bad = 0;
    for f in &offer.files {
        let a = transfer::safe_combine(&g.library.path, &f.path).unwrap();
        let b = transfer::safe_combine(root, &f.path).unwrap();
        if transfer::is_manifest(&f.path) {
            assert!(b.exists(), "missing {}", f.path);
            continue;
        }
        if std::fs::read(&a).unwrap() != std::fs::read(&b).unwrap_or_default() {
            bad += 1;
            println!("MISMATCH {}", f.path);
        }
    }
    assert!(offer.dirs.iter().all(|d| transfer::safe_combine(root, d).unwrap().is_dir()), "missing directories");
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
    Game { app_id: "999999".into(), name: "Small Files".into(), install_dir: "SmallFiles".into(), size_on_disk: 0, state_flags: 4, library: Library { path: root } }
}

/// cargo test --release bench_small_files -- --ignored --nocapture
#[test]
#[ignore]
fn bench_small_files() {
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
            let Ok(path) = transfer::safe_combine(&game.library.path, &f.path) else { continue };
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
    Game { app_id: "999998".into(), name: "Big File".into(), install_dir: "BigFile".into(), size_on_disk: 0, state_flags: 4, library: Library { path: root } }
}

/// Network-send benchmark. BENCH_GAME = an installed app id, "small" (20k small files) or "big" (one 2 GB file).
/// Combine with STEAMXFER_COMPRESS, STEAMXFER_SPLIT, STEAMXFER_THROTTLE_MBPS, STEAMXFER_WORKERS.
/// cargo test --release bench_send -- --ignored --nocapture
#[test]
#[ignore]
fn bench_send() {
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

fn wallpaper_engine() -> Option<Game> {
    steam::games(&steam::libraries()).into_iter().find(|g| g.app_id == "431960")
}

/// Interop: send to a C# receiver listening on INTEROP_PORT (run manually: cargo test interop_send -- --ignored).
#[test]
#[ignore]
fn interop_send() {
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
