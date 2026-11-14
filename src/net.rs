//! Finding other PCs (UDP broadcast on every adapter), the firewall rule, and the
//! parallel-stream transfer protocol. Wire-compatible with the C# version of the app.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use netdev::prelude::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::pipeline::{self, Compression, SendPlan, Wire};
use crate::steam::{self, Game, Library};
use crate::transfer::{self, Offer, Progress};
use crate::util::{self, no_window};

pub const UDP_PORT: u16 = 47800;
pub static TCP_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(47801);
const MAGIC: &str = "STEAMXFER2";
const MAX_STREAMS: i32 = 8;
/// Negotiated protocol extension: framed chunks (big files split across connections) + zstd compression.
const FEATURE_V2: &str = "v2-frames-zstd";
/// Negotiated: the receiver understands non-Steam games (Epic, GOG, EA, Ubisoft, ... and plain folders).
const FEATURE_LAUNCHERS: &str = "launchers";
/// Negotiated: the receiver can install Windows apps (junction staging, registry, shortcuts).
const FEATURE_APPS: &str = "apps";

pub fn tcp_port() -> u16 {
    TCP_PORT.load(Ordering::Relaxed)
}

// ---------------- adapters ----------------

#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    pub name: String,
    pub addr: Ipv4Addr,
    pub via: String,
    pub bps: u64,
    pub kind: &'static str, // "usb4" | "ethernet" | "wifi"
    pub manual: bool,
}

pub fn is_thunderbolt(i: &Interface) -> bool {
    let has = |s: &Option<String>| s.as_deref().is_some_and(|s| {
        let s = s.to_ascii_lowercase();
        s.contains("thunderbolt") || s.contains("usb4")
    });
    has(&i.description) || has(&i.friendly_name)
}

pub fn kind_of(i: &Interface) -> &'static str {
    if is_thunderbolt(i) {
        "usb4"
    } else if i.if_type == InterfaceType::Wireless80211 {
        "wifi"
    } else {
        "ethernet"
    }
}

fn active_ipv4() -> Vec<(Interface, Ipv4Net)> {
    netdev::get_interfaces()
        .into_iter()
        .filter(|i| i.oper_state == OperState::Up && i.if_type != InterfaceType::Loopback)
        .flat_map(|i| i.ipv4.clone().into_iter().map(move |n| (i.clone(), n)))
        .collect()
}

/// Which local adapter reaches this address: (adapter name, link speed, kind).
pub fn route_to(remote: Ipv4Addr) -> (String, u64, &'static str) {
    if remote.is_loopback() {
        return ("this PC".into(), 100_000_000_000, "ethernet");
    }
    for (i, net) in active_ipv4() {
        if net.contains(&remote) {
            return (i.friendly_name.clone().unwrap_or(i.name.clone()), i.transmit_speed.unwrap_or(0), kind_of(&i));
        }
    }
    ("routed".into(), 0, "ethernet")
}

// ---------------- discovery ----------------

pub struct Discovery {
    id: String,
    peers: Arc<Mutex<HashMap<Ipv4Addr, (Peer, Instant)>>>,
    stop: Arc<AtomicBool>,
}

impl Discovery {
    pub fn new() -> Self {
        let id = format!("{:x}{:x}", std::process::id(), Instant::now().elapsed().as_nanos() ^ util::to_ticks(std::time::SystemTime::now()) as u128);
        Discovery { id, peers: Arc::default(), stop: Arc::default() }
    }

    pub fn start(&self) -> io::Result<()> {
        let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
        sock.set_reuse_address(true)?;
        sock.set_broadcast(true)?;
        sock.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, UDP_PORT).into())?;
        let sock: UdpSocket = sock.into();
        sock.set_read_timeout(Some(Duration::from_secs(1)))?;

        let (peers, stop, id) = (self.peers.clone(), self.stop.clone(), self.id.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            while !stop.load(Ordering::Relaxed) {
                let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
                let msg = String::from_utf8_lossy(&buf[..n]);
                let parts: Vec<&str> = msg.split('|').collect();
                if parts.len() < 3 || parts[0] != MAGIC || parts[2] == id {
                    continue;
                }
                let IpAddr::V4(ip) = from.ip() else { continue };
                let (via, bps, kind) = route_to(ip);
                peers.lock().unwrap().insert(ip, (Peer { name: parts[1].to_string(), addr: ip, via, bps, kind, manual: false }, Instant::now()));
            }
        });

        let (stop, id) = (self.stop.clone(), self.id.clone());
        std::thread::spawn(move || {
            let msg = format!("{MAGIC}|{}|{id}", util::machine_name());
            while !stop.load(Ordering::Relaxed) {
                for (_, net) in active_ipv4() {
                    if let Ok(u) = UdpSocket::bind((net.addr(), 0)) {
                        let _ = u.set_broadcast(true);
                        let _ = u.send_to(msg.as_bytes(), (net.broadcast(), UDP_PORT));
                        let _ = u.send_to(msg.as_bytes(), (Ipv4Addr::BROADCAST, UDP_PORT));
                    }
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
        Ok(())
    }

    /// Thunderbolt/USB4 links first, then fastest link.
    pub fn peers(&self) -> Vec<Peer> {
        let mut map = self.peers.lock().unwrap();
        map.retain(|_, (_, seen)| seen.elapsed() < Duration::from_secs(8));
        let mut v: Vec<Peer> = map.values().map(|(p, _)| p.clone()).collect();
        v.sort_by(|a, b| (b.kind == "usb4").cmp(&(a.kind == "usb4")).then(b.bps.cmp(&a.bps)).then(a.name.cmp(&b.name)));
        v
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ---------------- firewall ----------------

/// Inbound rules for all network profiles: a direct cable / Thunderbolt link is an "Unidentified" (Public)
/// network, which Windows blocks by default.
pub mod firewall {
    use super::*;
    const RULE: &str = "Steam Game Transfer";

    pub fn rule_exists() -> bool {
        no_window(Command::new("netsh").args(["advfirewall", "firewall", "show", "rule", &format!("name={RULE}")]))
            .output()
            .is_ok_and(|o| o.status.success())
    }

    pub fn add_rules() -> bool {
        let args = format!(
            "/c netsh advfirewall firewall add rule name=\"{RULE}\" dir=in action=allow protocol=TCP localport={} profile=any \
             & netsh advfirewall firewall add rule name=\"{RULE}\" dir=in action=allow protocol=UDP localport={UDP_PORT} profile=any",
            47801
        );
        crate::installer::run_elevated("cmd.exe", &args) && rule_exists()
    }

    pub fn remove_rules() {
        crate::installer::run_elevated("netsh", &format!("advfirewall firewall delete rule name=\"{RULE}\""));
    }
}

// ---------------- protocol ----------------

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct Hello {
    pub offer: Option<Offer>,
    pub streams: i32,
    pub session: Option<String>,
    /// Protocol extensions this side speaks (the C# version sends none and ignores the field).
    pub features: Vec<String>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct Reply {
    pub accept: bool,
    pub reason: Option<String>,
    pub skip: Vec<usize>,
    pub session: Option<String>,
    pub streams: i32,
    pub features: Vec<String>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct Outcome {
    pub ok: bool,
    pub error: Option<String>,
}

pub fn send_msg<T: Serialize>(s: &mut TcpStream, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    s.write_all(&(body.len() as i32).to_le_bytes())?;
    s.write_all(&body)?;
    s.flush()
}

pub fn read_msg<T: DeserializeOwned>(s: &mut TcpStream) -> io::Result<T> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let n = i32::from_le_bytes(len);
    if n <= 0 || n > 512 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Bad message from peer."));
    }
    let mut body = vec![0u8; n as usize];
    s.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

fn tune(s: &TcpStream) {
    let _ = s.set_nodelay(true);
    let r = socket2::SockRef::from(s);
    let _ = r.set_send_buffer_size(8 * 1024 * 1024);
    let _ = r.set_recv_buffer_size(8 * 1024 * 1024);
}

/// More parallel streams on fast links (Thunderbolt/USB4, 2.5G+) to keep the pipe and the disks busy.
pub fn streams_for(bps: u64, usb4: bool) -> i32 {
    if usb4 || bps >= 5_000_000_000 { 4 } else if bps >= 2_000_000_000 { 3 } else { 2 }
}

// ---------------- sending ----------------

fn connect(to: Ipv4Addr, p: &Progress) -> io::Result<TcpStream> {
    let s = TcpStream::connect_timeout(&SocketAddr::from((to, tcp_port())), Duration::from_secs(8))
        .map_err(|_| io::Error::other(format!("Could not reach {to}. Is the app open on the other PC?")))?;
    tune(&s);
    p.register(&s);
    Ok(s)
}

pub fn send(game: &Game, to: Ipv4Addr, p: &Arc<Progress>, log: &(dyn Fn(String) + Sync)) -> io::Result<()> {
    log(format!("Scanning files of {}...", game.name));
    p.set_current("scanning files");
    let offer = transfer::build_offer(game)?;
    let (via, bps, kind) = route_to(to);
    let want = streams_for(bps, kind == "usb4");
    log(format!("{} files, {}. Connecting to {to} via {via}...", offer.files.len(), util::size(offer.total_bytes)));

    let mut control = connect(to, p)?;
    send_msg(&mut control, &Hello { offer: Some(offer.clone()), streams: want, session: None, features: vec![FEATURE_V2.into(), FEATURE_LAUNCHERS.into(), FEATURE_APPS.into()] })?;
    p.set_current("waiting for the other PC to accept");
    log("Waiting for the other PC to accept...".into());
    let reply: Reply = read_msg(&mut control)?;
    if !reply.accept {
        return Err(io::Error::other(format!("Transfer declined: {}", reply.reason.unwrap_or("rejected by other PC".into()))));
    }

    if !offer.is_steam() && !reply.features.iter().any(|f| f == FEATURE_LAUNCHERS) {
        return Err(io::Error::other(format!(
            "The other PC runs an older version that can only receive Steam games. Update it to send {} games.",
            offer.launcher().name()
        )));
    }
    if offer.launcher() == crate::launchers::Launcher::App && !reply.features.iter().any(|f| f == FEATURE_APPS) {
        return Err(io::Error::other("The other PC runs an older version that can't install apps. Update it first."));
    }
    let skip: HashSet<usize> = reply.skip.iter().copied().collect();
    let todo: Vec<usize> = (0..offer.files.len()).filter(|i| !skip.contains(i)).collect();
    p.total.store(todo.iter().map(|&i| offer.files[i].size).sum(), Ordering::Relaxed);
    if !skip.is_empty() {
        log(format!("Other PC already has {} files, skipping them.", skip.len()));
    }
    let (manifests, files): (Vec<usize>, Vec<usize>) = todo.into_iter().partition(|&i| transfer::is_manifest(&offer.files[i].path, offer.is_steam()));

    let wire = Wire { v2: reply.features.iter().any(|f| f == FEATURE_V2) };
    let source_hdd = crate::hardware::is_hdd(&game.library.path);
    let compress = wire.v2 && compression_wanted(to, bps);
    let comp = Compression::new(offer.files.len(), compress);
    let plan = SendPlan {
        offer: &offer,
        root: &game.library.path,
        files,
        wire,
        comp: &comp,
        workers: transfer::workers_for(&[&game.library.path]),
        split: wire.v2 && !source_hdd && std::env::var("STEAMXFER_SPLIT").as_deref() != Ok("0"),
    };
    let streams = reply.streams.clamp(1, MAX_STREAMS);
    log(format!(
        "Sending over {streams} parallel stream{}{}{}.",
        if streams > 1 { "s" } else { "" },
        if compress { ", compressing on the fly" } else { "" },
        if source_hdd { " (hard drive: sequential reads)" } else { "" }
    ));

    let session = reply.session.unwrap_or_default();
    let mut data = Vec::new();
    for _ in 0..streams {
        let mut s = connect(to, p)?;
        send_msg(&mut s, &Hello { offer: None, streams: 0, session: Some(session.clone()), features: vec![] })?;
        data.push(s);
    }
    pipeline::send_files(data, &plan, p, log)?;
    pipeline::send_manifests(&mut control, &plan, &manifests, p, log)?;

    let (raw, stored) = comp.stats();
    if raw > 0 && stored < raw {
        log(format!("Compression saved {} of network traffic.", util::size(raw - stored)));
    }
    p.set_current("finishing up on other PC");
    let outcome: Outcome = read_msg(&mut control)?;
    if !outcome.ok {
        return Err(io::Error::other(format!("Other PC reported an error: {}", outcome.error.unwrap_or_default())));
    }
    Ok(())
}

/// Compress only where the network, not the CPU, is the bottleneck (Wi-Fi, 1–2.5 GbE).
/// STEAMXFER_COMPRESS=1/0 forces it for testing.
fn compression_wanted(to: Ipv4Addr, bps: u64) -> bool {
    if let Ok(v) = std::env::var("STEAMXFER_COMPRESS") {
        return v == "1";
    }
    !to.is_loopback() && bps < 5_000_000_000
}

// ---------------- receiving ----------------

pub struct Decision {
    pub library: Library,
    pub close_steam: bool,
    /// Apps: the receiving staging folder (junctions into this user's profile); unlinked when dropped.
    /// Only held, never read: it must live until the files are written.
    #[allow(dead_code)]
    pub staging: Option<Arc<crate::apps::Staging>>,
}

pub enum JobEnd {
    Done(String),
    Failed(String),
    Cancelled,
    Declined,
}

/// Where a transfer goes, for the live transfer view: endpoints and how they're connected.
#[derive(Clone, Debug, Default)]
pub struct Route {
    pub from: String,
    pub to: String,
    /// "pc", "usb4", "ethernet", "wifi" or "drive"
    pub from_kind: String,
    pub to_kind: String,
    /// Game name and cover-art key (Steam app id, empty for other launchers).
    pub game: String,
    pub art_id: String,
}

/// What the transfer engine needs from the UI.
pub trait Host: Send + Sync {
    fn log(&self, msg: String);
    /// None if another transfer is running.
    fn begin_job(&self, title: String, route: Route) -> Option<Arc<Progress>>;
    fn end_job(&self, end: JobEnd);
    /// Blocks until the user decides.
    fn ask_accept(&self, offer: &Offer, from: String) -> Option<Decision>;
    /// `dest`: the Steam library, or for other launchers the game folder.
    fn after_receive(&self, offer: &Offer, dest: &std::path::Path, steam_was_closed: bool);
}

pub struct Server {
    host: Arc<dyn Host>,
    sessions: Arc<Mutex<HashMap<String, mpsc::Sender<TcpStream>>>>,
}

impl Server {
    pub fn start(host: Arc<dyn Host>) -> io::Result<Arc<Server>> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, tcp_port()))?;
        let server = Arc::new(Server { host, sessions: Arc::default() });
        let srv = server.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let srv = srv.clone();
                std::thread::spawn(move || srv.handle(conn));
            }
        });
        Ok(server)
    }

    fn handle(&self, mut conn: TcpStream) {
        tune(&conn);
        let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
        let Ok(hello) = read_msg::<Hello>(&mut conn) else { return };
        let _ = conn.set_read_timeout(None);
        if let Some(session) = hello.session {
            // Data connection for an accepted transfer: hand it to that session.
            if let Some(tx) = self.sessions.lock().unwrap().get(&session) {
                let _ = tx.send(conn);
            }
            return;
        }
        if let Some(offer) = hello.offer {
            let wire = Wire { v2: hello.features.iter().any(|f| f == FEATURE_V2) };
            self.handle_offer(conn, offer, hello.streams, wire);
        }
    }

    fn handle_offer(&self, mut control: TcpStream, offer: Offer, want: i32, wire: Wire) {
        let host = &self.host;
        let from = control.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
        let route = Route {
            from: offer.from.clone(),
            to: "This PC".into(),
            from_kind: control.peer_addr().ok().and_then(|a| match a.ip() { IpAddr::V4(ip) => Some(route_to(ip).2.to_string()), _ => None }).unwrap_or("ethernet".into()),
            to_kind: "pc".into(),
            game: offer.name.clone(),
            art_id: if offer.is_steam() { offer.app_id.clone() } else { String::new() },
        };
        let Some(p) = host.begin_job(format!("Receiving {}", offer.name), route) else {
            let _ = send_msg(&mut control, &Reply { accept: false, reason: Some("the other PC is busy with another transfer".into()), ..Default::default() });
            return;
        };
        p.register(&control);
        let session = format!("{:032x}", util::to_ticks(std::time::SystemTime::now()) as u128 ^ (std::process::id() as u128) << 64);
        let mut closed_steam = false;
        let mut root: Option<std::path::PathBuf> = None;
        let log = |m: String| host.log(m);

        let result = (|| -> io::Result<bool> {
            host.log(format!("{} ({from}) wants to send {} ({}).", offer.from, offer.name, util::size(offer.total_bytes)));
            let Some(decision) = host.ask_accept(&offer, format!("{} ({from})", offer.from)) else {
                send_msg(&mut control, &Reply { accept: false, reason: Some("declined by user".into()), ..Default::default() })?;
                return Ok(false);
            };
            let dest = decision.library.path.clone();
            root = Some(dest.clone());
            let skip = transfer::compute_skip(&offer, &dest);
            if decision.close_steam && steam::is_running() {
                steam::shutdown(&log);
                closed_steam = true;
            }
            transfer::create_dirs(&offer, &dest)?;

            let skip_set: HashSet<usize> = skip.iter().copied().collect();
            let expected: HashSet<usize> = (0..offer.files.len()).filter(|i| !skip_set.contains(i)).collect();
            p.total.store(expected.iter().map(|&i| offer.files[i].size).sum(), Ordering::Relaxed);
            let streams = want.clamp(1, MAX_STREAMS);
            let (tx, rx) = mpsc::channel();
            self.sessions.lock().unwrap().insert(session.clone(), tx);
            let mut features = vec![FEATURE_LAUNCHERS.to_string(), FEATURE_APPS.to_string()];
            if wire.v2 {
                features.push(FEATURE_V2.to_string());
            }
            send_msg(&mut control, &Reply { accept: true, skip: skip.clone(), session: Some(session.clone()), streams, reason: None, features })?;
            host.log(format!(
                "Receiving into {} over {streams} stream{}{}",
                dest.display(),
                if streams > 1 { "s" } else { "" },
                if skip.is_empty() { String::new() } else { format!(" ({} files already here)", skip.len()) }
            ));

            let mut data = Vec::new();
            for _ in 0..streams {
                let s = rx.recv_timeout(Duration::from_secs(30)).map_err(|_| io::Error::other("The other PC didn't open its data connections."))?;
                p.register(&s);
                data.push(s);
            }
            let receiver = pipeline::Receiver::new(&offer, &dest, &expected, wire);
            receiver.receive_files(data, &p)?;
            receiver.receive_manifests(&mut control, &p)?;
            send_msg(&mut control, &Outcome { ok: true, error: None })?;
            Ok(true)
        })();
        self.sessions.lock().unwrap().remove(&session);

        match result {
            Ok(false) => host.end_job(JobEnd::Declined),
            Ok(true) => {
                let dest = root.clone().unwrap_or_default();
                if !offer.is_steam() {
                    if let Err(e) = crate::launchers::register(offer.launcher(), &offer.meta, &dest, &log) {
                        host.log(format!("Couldn't register the game with {}: {e}", offer.launcher().name()));
                    }
                }
                host.end_job(JobEnd::Done(format!("Received {}.", offer.name)));
                host.after_receive(&offer, &dest, closed_steam);
            }
            Err(e) => {
                let mut msg = e.to_string();
                if let Some(drive) = root.as_deref().and_then(|r| transfer::find_disconnected_drive(&[r])) {
                    msg = format!(
                        "Drive {} was disconnected. Plug it back in and send again — files already copied are kept and skipped.",
                        drive.to_string_lossy().trim_end_matches('\\')
                    );
                }
                let _ = send_msg(&mut control, &Outcome { ok: false, error: Some(msg.clone()) });
                host.end_job(if p.is_cancelled() { JobEnd::Cancelled } else { JobEnd::Failed(msg) });
                if closed_steam {
                    steam::restart(&log);
                }
            }
        }
    }
}
