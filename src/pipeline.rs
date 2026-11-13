//! The data plane: moving file bytes over parallel connections, fast for huge files and tiny ones.
//!
//! Wire formats on the data connections (the control connection carries the manifests last):
//! * **v1** (what the C# version speaks): `[i32 file index][the whole file's bytes]`
//! * **v2** (negotiated between Rust peers): `[i32 file][u64 offset][u32 raw len][u32 stored len][stored bytes]`,
//!   stored < raw ⇒ zstd-compressed. Frames let one big file be spread over every connection and let
//!   compressible data shrink on slow links.
//! Both end with `i32 -1`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use crate::transfer::{self, FileEntry, Offer, Progress, SMALL_FILE};

pub const END_MARKER: i32 = -1;
/// Socket-side buffering: many small files become few large writes/reads instead of a syscall + packet each.
const SOCKET_BUF: usize = 1024 * 1024;
/// Small files read ahead / waiting to be written (bounded memory).
const READ_AHEAD: usize = 128;
/// Frame payload size for big files.
const CHUNK: usize = transfer::BUF_SIZE;
/// Big files are handed out to connections in ranges of this size (v2), so one huge file uses every connection.
const RANGE: u64 = 64 * 1024 * 1024;
const MAX_FRAME: u32 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct Wire {
    pub v2: bool,
}

// ---------------- adaptive compression ----------------

/// zstd level 1 (≈400–800 MB/s per core) where it pays: a file is sent raw once a chunk of it doesn't shrink
/// by ≥3 %, and compression switches off entirely once the game proves incompressible (most AAA games ship
/// already-compressed archives; e.g. CoD/Persona ≈98 %, while Silksong/Unity-style data shrinks to ≈50 %).
pub struct Compression {
    enabled: bool,
    off: AtomicBool,
    raw: AtomicU64,
    stored: AtomicU64,
    skip: Vec<AtomicBool>,
}

thread_local! {
    static ZCTX: RefCell<Option<zstd::bulk::Compressor<'static>>> = const { RefCell::new(None) };
    static ZDCTX: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) };
}

impl Compression {
    pub fn new(files: usize, enabled: bool) -> Self {
        Compression { enabled, off: AtomicBool::new(false), raw: AtomicU64::new(0), stored: AtomicU64::new(0), skip: (0..files).map(|_| AtomicBool::new(false)).collect() }
    }

    fn active(&self) -> bool {
        self.enabled && !self.off.load(Ordering::Relaxed)
    }

    /// Bytes seen by the compressor and what they became.
    pub fn stats(&self) -> (u64, u64) {
        (self.raw.load(Ordering::Relaxed), self.stored.load(Ordering::Relaxed))
    }

    /// Compresses `data` into `out` if worthwhile; true if `out` now holds the compressed form.
    pub fn encode(&self, idx: usize, data: &[u8], out: &mut Vec<u8>) -> bool {
        if !self.active() || data.len() < 2048 || self.skip[idx].load(Ordering::Relaxed) {
            return false;
        }
        let n = ZCTX.with(|c| {
            let mut c = c.borrow_mut();
            let ctx = match c.as_mut() {
                Some(ctx) => ctx,
                None => c.insert(zstd::bulk::Compressor::new(1).ok()?),
            };
            out.clear();
            out.reserve(zstd::zstd_safe::compress_bound(data.len()));
            ctx.compress_to_buffer(data, out).ok()
        });
        let Some(n) = n else { return false };
        let len = data.len() as u64;
        let raw = self.raw.fetch_add(len, Ordering::Relaxed) + len;
        let stored = self.stored.fetch_add((n as u64).min(len), Ordering::Relaxed) + (n as u64).min(len);
        if raw > 512 * 1024 * 1024 && stored * 100 > raw * 97 {
            self.off.store(true, Ordering::Relaxed); // whole game is incompressible: stop spending CPU
        }
        if (n as u64) * 100 >= len * 97 {
            self.skip[idx].store(true, Ordering::Relaxed);
            return false;
        }
        true
    }
}

fn decode(stored: &[u8], raw_len: usize) -> io::Result<Vec<u8>> {
    ZDCTX.with(|c| {
        let mut c = c.borrow_mut();
        let ctx = match c.as_mut() {
            Some(ctx) => ctx,
            None => c.insert(zstd::bulk::Decompressor::new()?),
        };
        let mut out = Vec::with_capacity(raw_len);
        ctx.decompress_to_buffer(stored, &mut out)?;
        if out.len() != raw_len {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Corrupt compressed data from peer."));
        }
        Ok(out)
    })
}

/// Test aid: STEAMXFER_THROTTLE_MBPS=110 makes the sender behave like a link of that speed
/// (shared by all connections), so link-speed-dependent choices like compression can be measured on one PC.
fn throttle(bytes: usize) {
    static LIMIT: std::sync::OnceLock<Option<(f64, std::time::Instant, AtomicU64)>> = std::sync::OnceLock::new();
    let limit = LIMIT.get_or_init(|| {
        let mbps: f64 = std::env::var("STEAMXFER_THROTTLE_MBPS").ok()?.parse().ok()?;
        Some((mbps * 1e6, std::time::Instant::now(), AtomicU64::new(0)))
    });
    if let Some((rate, start, sent)) = limit {
        let total = sent.fetch_add(bytes as u64, Ordering::Relaxed) + bytes as u64;
        let due = Duration::from_secs_f64(total as f64 / rate);
        let now = start.elapsed();
        if due > now {
            std::thread::sleep(due - now);
        }
    }
}

fn write_header(w: &mut impl Write, wire: Wire, idx: usize, off: u64, raw: usize, stored: usize) -> io::Result<()> {
    if !wire.v2 {
        return w.write_all(&(idx as i32).to_le_bytes());
    }
    let mut h = [0u8; 20];
    h[0..4].copy_from_slice(&(idx as i32).to_le_bytes());
    h[4..12].copy_from_slice(&off.to_le_bytes());
    h[12..16].copy_from_slice(&(raw as u32).to_le_bytes());
    h[16..20].copy_from_slice(&(stored as u32).to_le_bytes());
    w.write_all(&h)
}

// ---------------- sending ----------------

pub struct SendPlan<'a> {
    pub offer: &'a Offer,
    pub root: &'a Path,
    /// Game files (not manifests) the other side still needs.
    pub files: Vec<usize>,
    pub wire: Wire,
    pub comp: &'a Compression,
    pub workers: usize,
    /// Split big files into ranges spread over all connections (v2; not for hard drives).
    pub split: bool,
}

struct Ready {
    idx: usize,
    raw_len: usize,
    payload: Vec<u8>,
}

fn open_waiting(path: &Path, p: &Progress, log: &(dyn Fn(String) + Sync)) -> io::Result<File> {
    loop {
        match transfer::open_read(path) {
            Ok(f) => return Ok(f),
            Err(_) if crate::util::drive_gone(path) => transfer::wait_for_drive(path, p, log)?,
            Err(e) => return Err(e),
        }
    }
}

/// Positional read that survives the source drive being unplugged and plugged back in.
fn read_at(file: &mut File, path: &Path, f: &FileEntry, buf: &mut [u8], pos: u64, p: &Progress, log: &(dyn Fn(String) + Sync)) -> io::Result<()> {
    let mut got = 0;
    while got < buf.len() {
        match file.seek_read(&mut buf[got..], pos + got as u64) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{} got shorter while sending.", f.path))),
            Ok(n) => got += n,
            Err(e) => {
                if transfer::find_disconnected_drive(&[path]).is_none() {
                    return Err(e);
                }
                transfer::wait_for_drive(path, p, log)?;
                *file = transfer::open_read(path)?;
                if file.metadata()?.len() != f.size {
                    return Err(io::Error::other(format!("{} changed while its drive was disconnected.", f.path)));
                }
            }
        }
    }
    Ok(())
}

/// Send `len` bytes of file `idx` starting at `off`.
#[allow(clippy::too_many_arguments)]
fn send_range(w: &mut impl Write, plan: &SendPlan, idx: usize, off: u64, len: u64, p: &Progress, log: &(dyn Fn(String) + Sync)) -> io::Result<()> {
    let f = &plan.offer.files[idx];
    let path = transfer::safe_combine(plan.root, &f.path)?;
    p.set_current(&f.path);
    let mut file = open_waiting(&path, p, log)?;
    if !plan.wire.v2 {
        write_header(w, plan.wire, idx, 0, 0, 0)?; // v1: index once, then the raw bytes
    }
    let mut packed = Vec::new();
    transfer::with_buffer(|buf| {
        let (mut pos, end) = (off, off + len);
        if len == 0 && plan.wire.v2 {
            return write_header(w, plan.wire, idx, 0, 0, 0);
        }
        while pos < end {
            p.checkpoint()?;
            let n = (end - pos).min(CHUNK as u64) as usize;
            read_at(&mut file, &path, f, &mut buf[..n], pos, p, log)?;
            let data = &buf[..n];
            if !plan.wire.v2 {
                w.write_all(data)?;
                throttle(n);
            } else if plan.comp.encode(idx, data, &mut packed) {
                write_header(w, plan.wire, idx, pos, n, packed.len())?;
                w.write_all(&packed)?;
                throttle(packed.len());
            } else {
                write_header(w, plan.wire, idx, pos, n, n)?;
                w.write_all(data)?;
                throttle(n);
            }
            p.add(n as u64);
            pos += n as u64;
        }
        Ok(())
    })
}

/// Send the game files over the data connections. Big files stream from disk (split across connections in v2);
/// small files are read ahead — and compressed — by a pool, so their per-file latency overlaps.
/// Half the connections prefer big files and half small ones, so both kinds flow at the same time.
pub fn send_files(streams: Vec<TcpStream>, plan: &SendPlan, p: &Progress, log: &(dyn Fn(String) + Sync)) -> io::Result<()> {
    let offer = plan.offer;
    let (mut big, small): (Vec<usize>, Vec<usize>) = plan.files.iter().copied().partition(|&i| offer.files[i].size >= SMALL_FILE);
    big.sort_by_key(|&i| std::cmp::Reverse(offer.files[i].size));
    let mut ranges = VecDeque::new();
    for idx in big {
        let size = offer.files[idx].size;
        let step = if plan.split { RANGE } else { size.max(1) };
        let mut off = 0;
        while off < size {
            ranges.push_back((idx, off, step.min(size - off)));
            off += step;
        }
    }
    let ranges = Mutex::new(ranges);
    let small = Mutex::new(VecDeque::from(small));
    let failure: Mutex<Option<io::Error>> = Mutex::new(None);
    let stop = AtomicBool::new(false);
    let (tx, rx) = mpsc::sync_channel::<io::Result<Ready>>(READ_AHEAD);
    let rx = Mutex::new(rx);
    let fail = |e: io::Error| {
        failure.lock().unwrap().get_or_insert(e);
        stop.store(true, Ordering::SeqCst);
        p.abort_io(); // unblock every other connection
    };

    std::thread::scope(|scope| {
        // read-ahead (+ compress) pool for small files
        for _ in 0..plan.workers {
            let (tx, small, stop) = (tx.clone(), &small, &stop);
            scope.spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let next = small.lock().unwrap().pop_front();
                    let Some(idx) = next else { break };
                    let f = &offer.files[idx];
                    let item = transfer::safe_combine(plan.root, &f.path).and_then(|path| transfer::read_small(&path, f, p, log)).map(|data| {
                        let mut packed = Vec::new();
                        if plan.wire.v2 && plan.comp.encode(idx, &data, &mut packed) {
                            Ready { idx, raw_len: data.len(), payload: packed }
                        } else {
                            Ready { idx, raw_len: data.len(), payload: data }
                        }
                    });
                    let is_err = item.is_err();
                    let mut item = Some(item);
                    while let Some(it) = item.take() {
                        match tx.try_send(it) {
                            Ok(()) => {}
                            Err(mpsc::TrySendError::Full(it)) if !stop.load(Ordering::SeqCst) => {
                                item = Some(it);
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            Err(_) => return,
                        }
                    }
                    if is_err {
                        return;
                    }
                }
            });
        }
        drop(tx);

        for (k, s) in streams.into_iter().enumerate() {
            let (ranges, rx, stop, fail) = (&ranges, &rx, &stop, &fail);
            scope.spawn(move || {
                let mut w = BufWriter::with_capacity(SOCKET_BUF, s);
                let prefer_big = k % 2 == 0;
                let next_range = || ranges.lock().unwrap().pop_front();
                let r = (|| -> io::Result<()> {
                    loop {
                        if stop.load(Ordering::SeqCst) {
                            return Ok(());
                        }
                        if prefer_big {
                            if let Some((idx, off, len)) = next_range() {
                                send_range(&mut w, plan, idx, off, len, p, log)?;
                                continue;
                            }
                        }
                        let msg = rx.lock().unwrap().recv_timeout(Duration::from_millis(20));
                        match msg {
                            Ok(Ok(r)) => {
                                p.checkpoint()?;
                                write_header(&mut w, plan.wire, r.idx, 0, r.raw_len, r.payload.len())?;
                                w.write_all(&r.payload)?;
                                throttle(r.payload.len());
                                p.add(r.raw_len as u64);
                            }
                            Ok(Err(e)) => return Err(e),
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                if let Some((idx, off, len)) = next_range() {
                                    send_range(&mut w, plan, idx, off, len, p, log)?;
                                }
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => match next_range() {
                                Some((idx, off, len)) => send_range(&mut w, plan, idx, off, len, p, log)?,
                                None => break,
                            },
                        }
                    }
                    w.write_all(&END_MARKER.to_le_bytes())?;
                    w.flush()
                })();
                if let Err(e) = r {
                    fail(e);
                }
            });
        }
    });
    match failure.into_inner().unwrap() {
        Some(e) => Err(e),
        None => p.checkpoint(),
    }
}

/// Manifests go last, on the control connection.
pub fn send_manifests(control: &mut TcpStream, plan: &SendPlan, manifests: &[usize], p: &Progress, log: &(dyn Fn(String) + Sync)) -> io::Result<()> {
    let mut w = BufWriter::new(control);
    for &idx in manifests {
        send_range(&mut w, plan, idx, 0, plan.offer.files[idx].size, p, log)?;
    }
    w.write_all(&END_MARKER.to_le_bytes())?;
    w.flush()
}

// ---------------- receiving ----------------

/// A big file arriving in ranges, possibly over several connections at once.
struct Partial {
    file: Mutex<Option<File>>,
    dest: PathBuf,
    tmp: PathBuf,
    remaining: Mutex<u64>,
}

pub struct Receiver<'a> {
    offer: &'a Offer,
    dest: &'a Path,
    expected: &'a HashSet<usize>,
    wire: Wire,
    workers: usize,
    received: Mutex<HashSet<usize>>,
    partials: Mutex<HashMap<usize, Arc<Partial>>>,
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

impl<'a> Receiver<'a> {
    pub fn new(offer: &'a Offer, dest: &'a Path, expected: &'a HashSet<usize>, wire: Wire) -> Self {
        Receiver { offer, dest, expected, wire, workers: transfer::workers_for(&[dest]), received: Mutex::default(), partials: Mutex::default() }
    }

    fn mark_received(&self, idx: usize) -> io::Result<()> {
        if self.received.lock().unwrap().insert(idx) { Ok(()) } else { Err(bad("Peer sent a file twice.")) }
    }

    fn write_range(&self, idx: usize, f: &FileEntry, off: u64, data: &[u8]) -> io::Result<()> {
        let part = {
            let mut map = self.partials.lock().unwrap();
            match map.get(&idx) {
                Some(p) => p.clone(),
                None => {
                    if self.received.lock().unwrap().contains(&idx) {
                        return Err(bad("Peer sent a file twice."));
                    }
                    let (file, dest, tmp) = transfer::create_dest(f, self.dest, false)?;
                    let p = Arc::new(Partial { file: Mutex::new(Some(file)), dest, tmp, remaining: Mutex::new(f.size) });
                    map.insert(idx, p.clone());
                    p
                }
            }
        };
        {
            let guard = part.file.lock().unwrap();
            let file = guard.as_ref().ok_or_else(|| bad("Peer sent more data than the file holds."))?;
            let mut done = 0;
            while done < data.len() {
                done += file.seek_write(&data[done..], off + done as u64)?;
            }
        }
        let complete = {
            let mut rem = part.remaining.lock().unwrap();
            if data.len() as u64 > *rem {
                return Err(bad("Peer sent more data than the file holds."));
            }
            *rem -= data.len() as u64;
            *rem == 0
        };
        if complete {
            self.partials.lock().unwrap().remove(&idx);
            let file = part.file.lock().unwrap().take().ok_or_else(|| bad("file finished twice"))?;
            transfer::finish_dest(file, f, &part.dest, &part.tmp, false)?;
            self.mark_received(idx)?;
        }
        Ok(())
    }

    /// Read frames from one connection until its end marker.
    fn receive_stream(&self, s: &mut dyn Read, manifests: bool, pool: Option<&mpsc::SyncSender<(usize, Vec<u8>)>>, p: &Progress) -> io::Result<()> {
        let mut b4 = [0u8; 4];
        loop {
            s.read_exact(&mut b4)?;
            let idx = i32::from_le_bytes(b4);
            if idx == END_MARKER {
                return Ok(());
            }
            let idx = idx as usize;
            if !self.expected.contains(&idx) || transfer::is_manifest(&self.offer.files[idx].path) != manifests {
                return Err(bad("Peer sent an unexpected file."));
            }
            let f = &self.offer.files[idx];

            if !self.wire.v2 {
                self.mark_received(idx)?;
                match pool {
                    Some(tx) if f.size < SMALL_FILE => {
                        let mut bytes = vec![0u8; f.size as usize];
                        s.read_exact(&mut bytes)?;
                        p.add(f.size);
                        p.checkpoint()?;
                        tx.send((idx, bytes)).map_err(|_| io::Error::other("file writers stopped"))?;
                    }
                    _ => transfer::write_file(s, f, self.dest, p, false, &mut 0)?,
                }
                continue;
            }

            let mut h = [0u8; 16];
            s.read_exact(&mut h)?;
            let off = u64::from_le_bytes(h[0..8].try_into().unwrap());
            let raw = u32::from_le_bytes(h[8..12].try_into().unwrap());
            let stored = u32::from_le_bytes(h[12..16].try_into().unwrap());
            if raw > MAX_FRAME || stored > raw || off.checked_add(raw as u64).is_none_or(|e| e > f.size) || (raw == 0 && f.size != 0) {
                return Err(bad("Malformed data from peer."));
            }
            let mut payload = vec![0u8; stored as usize];
            s.read_exact(&mut payload)?;
            let data = if stored < raw { decode(&payload, raw as usize)? } else { payload };
            p.add(raw as u64);
            p.checkpoint()?;
            if off == 0 && raw as u64 == f.size {
                self.mark_received(idx)?;
                match pool {
                    Some(tx) if f.size < SMALL_FILE => tx.send((idx, data)).map_err(|_| io::Error::other("file writers stopped"))?,
                    _ => transfer::write_bytes(&data, f, self.dest, false)?,
                }
            } else {
                p.set_current(&f.path);
                self.write_range(idx, f, off, &data)?;
            }
        }
    }

    /// Receive the game files from every data connection. Small files go to a pool of writer threads so
    /// creating/closing thousands of files (and the antivirus scan on each) never stalls the network.
    pub fn receive_files(&self, streams: Vec<TcpStream>, p: &Progress) -> io::Result<()> {
        let failure: Mutex<Option<io::Error>> = Mutex::new(None);
        let fail = |e: io::Error| {
            failure.lock().unwrap().get_or_insert(e);
            p.abort_io(); // wake the other streams
        };
        let (wtx, wrx) = mpsc::sync_channel::<(usize, Vec<u8>)>(READ_AHEAD);
        let wrx = Mutex::new(wrx);
        std::thread::scope(|scope| {
            for _ in 0..self.workers {
                let (wrx, fail, failure) = (&wrx, &fail, &failure);
                scope.spawn(move || {
                    loop {
                        let job = wrx.lock().unwrap().recv();
                        let Ok((idx, bytes)) = job else { break };
                        // after a failure keep draining so the network threads never block
                        if failure.lock().unwrap().is_none() {
                            if let Err(e) = transfer::write_bytes(&bytes, &self.offer.files[idx], self.dest, false) {
                                fail(e);
                            }
                        }
                    }
                });
            }
            for s in streams {
                let (fail, wtx) = (&fail, wtx.clone());
                scope.spawn(move || {
                    let mut r = BufReader::with_capacity(SOCKET_BUF, s);
                    if let Err(e) = self.receive_stream(&mut r, false, Some(&wtx), p) {
                        fail(e);
                    }
                });
            }
            drop(wtx); // writers finish once every connection is done
        });
        match failure.into_inner().unwrap() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Manifests last (after every game file is on disk), then check nothing is missing.
    pub fn receive_manifests(&self, control: &mut TcpStream, p: &Progress) -> io::Result<()> {
        self.receive_stream(control, true, None, p)?;
        if self.received.lock().unwrap().len() != self.expected.len() || !self.partials.lock().unwrap().is_empty() {
            return Err(io::Error::other("Transfer incomplete: some files are missing."));
        }
        Ok(())
    }
}
