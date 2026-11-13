//! UI glue: feeds the Slint window from the engine and runs actions on background threads.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};

use crate::hardware::{self, Drive, Link, Package};
use crate::installer;
use crate::net::{self, Decision, Discovery, Host, JobEnd, Peer, firewall};
use crate::steam::{self, Game};
use crate::transfer::{self, Offer, Progress};
use crate::util;
use crate::{AboutDialog, ChipRow, DriveRow, GameRow, LibRow, MainWindow, OfferDialog, PackageRow, PeerRow, Theme, ToastRow};

// ================= state shared with background threads =================

#[derive(Clone, Default, PartialEq)]
struct SysState {
    links: Vec<Link>,
    drives: Vec<Drive>,
    steam_found: bool,
    steam_running: bool,
    usb4: bool,
    firewall: Option<bool>,
}

struct Job {
    progress: Arc<Progress>,
    title: String,
}

pub struct Shared {
    job: Mutex<Option<Job>>,
    discovery: Discovery,
    manual: Mutex<Vec<Peer>>,
    sys: Mutex<SysState>,
    recheck_firewall: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
pub struct UiHost(Arc<Shared>);

/// Run `f` on the UI thread with the window and UI state.
fn on_ui(f: impl FnOnce(&MainWindow, &mut UiState) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || with_ui(f));
}

fn with_ui(f: impl FnOnce(&MainWindow, &mut UiState)) {
    UI.with(|cell| {
        let mut guard = cell.borrow_mut();
        if let Some(st) = guard.as_mut() {
            if let Some(ui) = st.ui.upgrade() {
                f(&ui, st);
            }
        }
    });
}

impl Host for UiHost {
    fn log(&self, msg: String) {
        on_ui(move |ui, st| st.log(ui, msg));
    }

    fn begin_job(&self, title: String) -> Option<Arc<Progress>> {
        let mut job = self.0.job.lock().unwrap();
        if job.is_some() {
            on_ui(|ui, st| st.toast(ui, "Another transfer is still running.", "error"));
            return None;
        }
        let p = Arc::new(Progress::new());
        *job = Some(Job { progress: p.clone(), title: title.clone() });
        drop(job);
        on_ui(move |ui, st| {
            st.log(ui, format!("{title}..."));
            st.speed = 0.0;
            st.last_bytes = 0;
            st.last_secs = 0.0;
            st.last_waiting = None;
            st.cancel_requested = false;
            ui.set_transfer_visible(true);
            st.render_action(ui);
            st.render_packages(ui);
        });
        Some(p)
    }

    fn end_job(&self, end: JobEnd) {
        let job = self.0.job.lock().unwrap().take();
        on_ui(move |ui, st| {
            let (state, mut text) = match end {
                JobEnd::Done(m) => ("ok", m),
                JobEnd::Failed(m) if st.cancel_requested => ("cancelled", cancelled_text(&m)),
                JobEnd::Failed(m) => ("error", m),
                JobEnd::Cancelled => ("cancelled", cancelled_text("")),
                JobEnd::Declined => ("cancelled", "Declined.".to_string()),
            };
            if let (Some(job), "ok") = (&job, state) {
                let t = job.progress.active_time();
                let done = job.progress.done();
                if done > 0 && t.as_secs_f64() > 0.0 {
                    text += &format!("  ({} in {}, avg {}/s)", util::size(done), util::time(t), util::size((done as f64 / t.as_secs_f64()) as u64));
                }
            }
            st.log(ui, if state == "error" { format!("ERROR: {text}") } else { text.clone() });
            if let Some(job) = &job {
                ui.set_t_title(job.title.clone().into());
            }
            ui.set_transfer_visible(true);
            ui.set_t_running(false);
            ui.set_t_paused(false);
            ui.set_t_state(state.into());
            ui.set_t_stats(text.clone().into());
            ui.set_t_file("".into());
            ui.set_t_indeterminate(false);
            if state == "ok" {
                ui.set_t_progress(1.0);
                ui.set_t_percent("".into());
            }
            st.toast(ui, &text, if state == "ok" { "ok" } else if state == "error" { "error" } else { "info" });
            st.cancel_requested = false;
            st.render_action(ui);
            st.render_packages(ui);
        });
    }

    fn ask_accept(&self, offer: &Offer, from: String) -> Option<Decision> {
        let (tx, rx) = mpsc::channel();
        let offer = offer.clone();
        on_ui(move |ui, st| open_offer_dialog(ui, st, offer, from, true, Box::new(move |d| drop(tx.send(d)))));
        rx.recv().ok().flatten()
    }

    fn after_receive(&self, offer: &Offer, steam_was_closed: bool) {
        on_ui(|_, st| st.load_games());
        let log = |m: String| self.log(m);
        if steam_was_closed {
            steam::restart(&log);
            self.log(format!("{} should now show as installed in Steam.", offer.name));
        } else if !steam::is_running() {
            self.log(format!("Open Steam and {} will show as installed.", offer.name));
        } else if installer::ask_yes_no(&format!("{} is copied. Steam needs a restart to see it.\n\nRestart Steam now?", offer.name)) {
            steam::restart(&log);
        }
    }
}

fn cancelled_text(_: &str) -> String {
    "Cancelled. Start it again to resume — files already copied are skipped.".into()
}

// ================= UI-thread state =================

thread_local! {
    static UI: RefCell<Option<UiState>> = const { RefCell::new(None) };
}

struct UiState {
    ui: slint::Weak<MainWindow>,
    shared: Arc<Shared>,
    host: UiHost,
    games: Vec<Game>,
    games_model: Rc<VecModel<GameRow>>,
    capsules: HashMap<String, Image>,
    heroes: HashMap<String, Image>,
    query: String,
    selected_game: Option<String>,
    selected_peer: Option<Ipv4Addr>,
    selected_drive: Option<PathBuf>,
    user_picked: bool,
    install_hidden: bool,
    peers: Vec<Peer>,
    sys: SysState,
    browsed: Vec<Package>,
    log_text: String,
    speed: f64,
    last_bytes: u64,
    last_secs: f64,
    last_waiting: Option<String>,
    cancel_requested: bool,
    toasts: Vec<(ToastRow, Instant)>,
    dialogs: Vec<OfferDialog>,
    about: Option<AboutDialog>,
    tick: u64,
}

impl UiState {
    fn log(&mut self, ui: &MainWindow, msg: String) {
        let line = format!("[{}] {}\n", clock(), msg);
        self.log_text.push_str(&line);
        if self.log_text.len() > 200_000 {
            let cut = self.log_text.len() - 150_000;
            let cut = self.log_text[cut..].find('\n').map(|i| cut + i + 1).unwrap_or(cut);
            self.log_text.drain(..cut);
        }
        ui.set_log_text(self.log_text.clone().into());
        ui.set_last_log(msg.into());
    }

    fn toast(&mut self, ui: &MainWindow, text: &str, kind: &str) {
        self.toasts.push((ToastRow { text: text.into(), kind: kind.into() }, Instant::now() + Duration::from_secs_f32(if kind == "error" { 7.0 } else { 4.5 })));
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
        self.render_toasts(ui);
    }

    fn render_toasts(&self, ui: &MainWindow) {
        ui.set_toasts(ModelRc::new(VecModel::from(self.toasts.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>())));
    }

    fn busy(&self) -> bool {
        self.shared.job.lock().unwrap().is_some()
    }

    // ---------- games ----------

    fn load_games(&mut self) {
        std::thread::spawn(|| {
            let games: Vec<Game> = if steam::root().is_some() { steam::games(&steam::libraries()).into_iter().filter(|g| g.app_id != "228980").collect() } else { vec![] };
            let ids: Vec<String> = games.iter().map(|g| g.app_id.clone()).collect();
            on_ui(move |ui, st| {
                st.games = games;
                if st.selected_game.as_ref().is_some_and(|s| !st.games.iter().any(|g| &g.app_id == s)) {
                    st.selected_game = None;
                }
                ui.set_games_count(if st.games.is_empty() { SharedString::new() } else { st.games.len().to_string().into() });
                st.render_games(ui);
                st.render_hero(ui);
                st.render_drives(ui);
                st.render_action(ui);
            });
            // cover art in the background
            for id in ids {
                if let Some(buf) = load_art(&id, "capsule", 320) {
                    on_ui(move |ui, st| {
                        let img = Image::from_rgba8(buf);
                        st.capsules.insert(id.clone(), img.clone());
                        st.apply_art(ui, &id, img);
                    });
                }
            }
        });
    }

    fn apply_art(&mut self, _ui: &MainWindow, id: &str, img: Image) {
        for i in 0..self.games_model.row_count() {
            if let Some(mut row) = self.games_model.row_data(i) {
                if row.app_id == id {
                    row.art = img.clone();
                    row.has_art = true;
                    self.games_model.set_row_data(i, row);
                }
            }
        }
    }

    fn render_games(&mut self, ui: &MainWindow) {
        let q = self.query.to_lowercase();
        let rows: Vec<GameRow> = self
            .games
            .iter()
            .filter(|g| q.is_empty() || g.name.to_lowercase().contains(&q))
            .map(|g| {
                let art = self.capsules.get(&g.app_id).cloned();
                GameRow {
                    app_id: g.app_id.clone().into(),
                    name: g.name.clone().into(),
                    meta: format!("{} · {}", if g.size_on_disk > 0 { util::size(g.size_on_disk) } else { "—".into() }, util::drive_of(&g.library.path).to_string_lossy().trim_end_matches('\\')).into(),
                    initials: initials(&g.name).into(),
                    tint: tint(&g.name),
                    has_art: art.is_some(),
                    art: art.unwrap_or_default(),
                    needs_update: !g.fully_installed(),
                }
            })
            .collect();
        let no_steam = steam::root().is_none();
        ui.set_empty_title(if no_steam { "Steam isn't installed on this PC" } else if !self.games.is_empty() { "No matches" } else { "No installed games yet" }.into());
        ui.set_empty_text(if no_steam { "You can still receive games once Steam is installed." } else if !self.games.is_empty() { "Try a different search." } else { "Games you receive will show up here." }.into());
        self.games_model.set_vec(rows);
        ui.set_selected_game(self.selected_game.clone().unwrap_or_default().into());
    }

    fn selected(&self) -> Option<&Game> {
        let id = self.selected_game.as_ref()?;
        self.games.iter().find(|g| &g.app_id == id)
    }

    fn render_hero(&mut self, ui: &MainWindow) {
        ui.set_selected_game(self.selected_game.clone().unwrap_or_default().into());
        let Some(g) = self.selected().cloned() else { return };
        ui.set_hero_title(g.name.clone().into());
        ui.set_hero_sub(format!("{}{}", if g.size_on_disk > 0 { format!("{} · ", util::size(g.size_on_disk)) } else { String::new() }, g.library.path.display()).into());
        ui.set_hero_tint(tint(&g.name));
        match self.heroes.get(&g.app_id) {
            Some(img) => {
                ui.set_hero_art(img.clone());
                ui.set_hero_has_art(true);
            }
            None => {
                ui.set_hero_has_art(false);
                let id = g.app_id.clone();
                std::thread::spawn(move || {
                    if let Some(buf) = load_art(&id, "hero", 900) {
                        on_ui(move |ui, st| {
                            let img = Image::from_rgba8(buf);
                            st.heroes.insert(id.clone(), img.clone());
                            if st.selected_game.as_deref() == Some(&id) {
                                ui.set_hero_art(img);
                                ui.set_hero_has_art(true);
                            }
                        });
                    }
                });
            }
        }
    }

    // ---------- targets ----------

    fn all_peers(&self) -> Vec<Peer> {
        let mut found = self.shared.discovery.peers();
        for m in self.shared.manual.lock().unwrap().iter() {
            if !found.iter().any(|f| f.addr == m.addr) {
                found.push(m.clone());
            }
        }
        found
    }

    fn render_peers(&mut self, ui: &MainWindow) {
        let peers = self.all_peers();
        if peers == self.peers && self.tick > 1 {
            return;
        }
        self.peers = peers;
        if self.selected_peer.is_some_and(|ip| !self.peers.iter().any(|p| p.addr == ip)) {
            self.selected_peer = None;
        }
        if !self.user_picked && self.selected_peer.is_none() && self.selected_drive.is_none() {
            self.selected_peer = self.peers.first().map(|p| p.addr); // fastest first
        }
        let n = self.peers.len();
        let rows: Vec<PeerRow> = self
            .peers
            .iter()
            .enumerate()
            .map(|(i, p)| PeerRow {
                key: p.addr.to_string().into(),
                name: p.name.clone().into(),
                subtitle: format!("{}{} · {}", kind_name(p.kind), if p.bps > 0 { format!(" · {}", util::link(p.bps)) } else { String::new() }, p.addr).into(),
                glyph: match p.kind {
                    "usb4" => "\u{E945}",
                    "wifi" => "\u{E701}",
                    _ => "\u{E839}",
                }
                .into(),
                tag: match p.kind {
                    "usb4" => "USB4",
                    "wifi" => "SLOW",
                    _ if i == 0 && n > 1 => "FASTEST",
                    _ => "",
                }
                .into(),
            })
            .collect();
        ui.set_peers(ModelRc::new(VecModel::from(rows)));
        ui.set_selected_peer(self.selected_peer.map(|p| p.to_string()).unwrap_or_default().into());
        self.render_action(ui);
    }

    fn render_drives(&mut self, ui: &MainWindow) {
        let need = self.selected().map(|g| g.size_on_disk).unwrap_or(0);
        if self.selected_drive.as_ref().is_some_and(|d| !self.sys.drives.iter().any(|x| &x.root == d)) {
            self.selected_drive = None;
        }
        let rows: Vec<DriveRow> = self
            .sys
            .drives
            .iter()
            .map(|d| {
                let small = need > 0 && d.free < need;
                DriveRow {
                    root: d.root.to_string_lossy().to_string().into(),
                    title: format!("{} ({})", d.label, d.root.to_string_lossy().trim_end_matches('\\')).into(),
                    subtitle: format!("{} free of {}{}", util::size(d.free), util::size(d.total), if small { " · not enough space" } else { "" }).into(),
                    glyph: if d.removable { "\u{E88E}" } else { "\u{EDA2}" }.into(),
                    used: if d.total > 0 { (d.total - d.free) as f32 / d.total as f32 } else { 0.0 },
                    too_small: small,
                }
            })
            .collect();
        ui.set_drives(ModelRc::new(VecModel::from(rows)));
        ui.set_selected_drive(self.selected_drive.as_ref().map(|d| d.to_string_lossy().to_string()).unwrap_or_default().into());
        self.render_action(ui);
    }

    fn render_packages(&mut self, ui: &MainWindow) {
        let mut seen = std::collections::HashSet::new();
        let rows: Vec<PackageRow> = self
            .sys
            .drives
            .iter()
            .flat_map(|d| d.packages.clone())
            .chain(self.browsed.clone())
            .filter(|p| seen.insert(p.folder.to_string_lossy().to_lowercase()))
            .map(|p| {
                let art = self.capsules.get(&p.app_id).cloned();
                PackageRow {
                    folder: p.folder.to_string_lossy().to_string().into(),
                    name: p.name.clone().into(),
                    subtitle: format!("{} · on {}", util::size(p.size), util::drive_of(&p.folder).to_string_lossy().trim_end_matches('\\')).into(),
                    initials: initials(&p.name).into(),
                    tint: tint(&p.name),
                    has_art: art.is_some(),
                    art: art.unwrap_or_default(),
                }
            })
            .collect();
        ui.set_packages(ModelRc::new(VecModel::from(rows)));
        ui.set_packages_enabled(!self.busy());
    }

    fn render_action(&mut self, ui: &MainWindow) {
        let busy = self.busy();
        let g = self.selected().cloned();
        let peer = self.selected_peer.and_then(|ip| self.peers.iter().find(|p| p.addr == ip).cloned());
        let drive = self.selected_drive.as_ref().and_then(|r| self.sys.drives.iter().find(|d| &d.root == r).cloned());
        let (mut label, mut ok, text) = ("Send".to_string(), false, if busy { "A transfer is in progress…".to_string() } else { String::new() });
        let text = if busy {
            text
        } else if let Some(g) = &g {
            if let Some(p) = &peer {
                label = format!("Send to {}", p.name);
                ok = true;
                let bps = if p.bps > 0 { p.bps } else { 1_000_000_000 } as f64;
                let secs = g.size_on_disk as f64 / (bps / 8.0 * 0.9).min(2.5e9);
                format!("{} → {} via {}{}", g.name, p.name, kind_name(p.kind), if g.size_on_disk > 0 { format!(" · about {}", util::time(Duration::from_secs_f64(secs))) } else { String::new() })
            } else if let Some(d) = &drive {
                label = format!("Copy to {}", d.root.to_string_lossy().trim_end_matches('\\'));
                ok = g.size_on_disk == 0 || d.free >= g.size_on_disk;
                format!("{} → {} ({})", g.name, d.label, d.root.to_string_lossy().trim_end_matches('\\'))
            } else {
                format!("Pick a PC or drive for {}.", g.name)
            }
        } else {
            "Pick a game to send.".into()
        };
        ui.set_go_text(label.into());
        ui.set_go_enabled(ok && !busy);
        ui.set_summary(text.into());
    }

    // ---------- status strip ----------

    fn render_chips(&mut self, ui: &MainWindow) {
        let s = &self.sys;
        let chip = |text: String, glyph: &str, dot: &str, action: &str| ChipRow { text: text.into(), glyph: glyph.into(), dot: dot.into(), action: action.into() };
        let mut chips = vec![if s.steam_found {
            chip(if s.steam_running { "Steam running".into() } else { "Steam".into() }, "", "ok", "")
        } else {
            chip("Steam not found".into(), "", "err", "")
        }];
        match s.firewall {
            Some(false) => chips.push(chip("Firewall: click to fix".into(), "", "warn", "firewall")),
            Some(true) => chips.push(chip("Firewall ready".into(), "", "ok", "")),
            None => {}
        }
        let usb: Vec<&Link> = s.links.iter().filter(|l| l.kind == "usb4").collect();
        if let Some(l) = usb.iter().find(|l| l.up) {
            chips.push(chip(format!("USB4 link {}", util::link(l.bps)), "\u{E945}", "", ""));
        } else if !usb.is_empty() {
            chips.push(chip("USB4 ready · no cable".into(), "\u{E945}", "", ""));
        } else if s.usb4 {
            chips.push(chip("USB4 port".into(), "\u{E945}", "", ""));
        }
        let eth: Vec<&Link> = s.links.iter().filter(|l| l.kind == "ethernet").collect();
        if let Some(l) = eth.iter().filter(|l| l.up).max_by_key(|l| l.bps) {
            chips.push(chip(format!("Ethernet {}", util::link(l.bps)), "", "ok", ""));
        } else if !eth.is_empty() {
            chips.push(chip("Ethernet unplugged".into(), "", "", ""));
        }
        if let Some(w) = s.links.iter().find(|l| l.kind == "wifi" && l.up) {
            chips.push(chip(format!("Wi-Fi {}", util::link(w.bps)), "\u{E701}", "", ""));
        }
        ui.set_chips(ModelRc::new(VecModel::from(chips)));
        ui.set_show_usb4_tip(s.usb4);
        ui.set_show_install(!installer::is_running_installed() && !self.install_hidden);
    }

    // ---------- progress ----------

    fn update_progress(&mut self, ui: &MainWindow) {
        let (p, title) = {
            let guard = self.shared.job.lock().unwrap();
            match guard.as_ref() {
                Some(j) => (j.progress.clone(), j.title.clone()),
                None => return,
            }
        };
        let done = p.done();
        let total = p.total.load(Ordering::Relaxed);
        let secs = p.active_time().as_secs_f64();
        let waiting = p.waiting();
        if waiting.is_some() {
            self.speed = 0.0;
            self.last_bytes = done;
            self.last_secs = secs;
        } else if secs - self.last_secs >= 0.5 {
            let inst = (done.saturating_sub(self.last_bytes)) as f64 / (secs - self.last_secs);
            self.speed = if self.speed == 0.0 { inst } else { self.speed * 0.7 + inst * 0.3 };
            self.last_bytes = done;
            self.last_secs = secs;
        }
        let paused = p.is_paused();
        let pct = if total > 0 { (done as f64 / total as f64).min(1.0) } else { 0.0 };
        let size = if total > 0 { format!("{} of {}", util::size(done), util::size(total)) } else { String::new() };
        let current = p.current.lock().unwrap().clone();

        ui.set_transfer_visible(true);
        ui.set_t_running(true);
        ui.set_t_paused(paused);
        ui.set_t_title(title.into());
        ui.set_t_indeterminate(total == 0 && !paused);
        ui.set_t_progress(pct as f32);
        ui.set_t_percent(if total > 0 { format!("{}%", (pct * 100.0) as u32) } else { String::new() }.into());
        let (state, stats) = if let Some(w) = &waiting {
            ("waiting", format!("{w}   {size}"))
        } else if paused {
            ("paused", format!("Paused   {size}"))
        } else if total > 0 {
            let eta = if self.speed > 1.0 { format!("{} left", util::time(Duration::from_secs_f64((total - done.min(total)) as f64 / self.speed))) } else { "—".into() };
            ("", format!("{size}   ·   {}/s ({:.2} Gbps)   ·   {eta}", util::size(self.speed as u64), self.speed * 8.0 / 1e9))
        } else {
            ("", current.clone())
        };
        ui.set_t_state(state.into());
        ui.set_t_stats(stats.into());
        ui.set_t_file(if total > 0 { current } else { String::new() }.into());
        if waiting.is_some() && waiting != self.last_waiting {
            let w = waiting.clone().unwrap_or_default();
            self.toast(ui, &w, "error");
        }
        self.last_waiting = waiting;
    }

    fn progress(&self) -> Option<Arc<Progress>> {
        self.shared.job.lock().unwrap().as_ref().map(|j| j.progress.clone())
    }
}

// ================= actions =================

fn spawn_job(host: &UiHost, title: String, work: impl FnOnce(&UiHost, Arc<Progress>) -> std::io::Result<String> + Send + 'static) {
    let host = host.clone();
    std::thread::spawn(move || {
        let Some(p) = host.begin_job(title) else { return };
        let end = match work(&host, p.clone()) {
            Ok(msg) => JobEnd::Done(msg),
            Err(_) if p.is_cancelled() => JobEnd::Cancelled,
            Err(e) if transfer::is_cancel_error(&e) => JobEnd::Cancelled,
            Err(e) => JobEnd::Failed(e.to_string()),
        };
        host.end_job(end);
    });
}

fn go(ui: &MainWindow, st: &mut UiState) {
    let Some(g) = st.selected().cloned() else { return };
    if !g.fully_installed() && !installer::ask_yes_no(&format!("{} isn't fully up to date on this PC. Send it anyway?\nSteam will finish the update on the other side.", g.name)) {
        return;
    }
    if let Some(peer) = st.selected_peer.and_then(|ip| st.peers.iter().find(|p| p.addr == ip).cloned()) {
        let title = format!("Sending {} to {}", g.name, peer.name);
        spawn_job(&st.host, title, move |host, p| {
            net::send(&g, peer.addr, &p, &|m| host.log(m))?;
            Ok(format!("{} is on {}. It shows up in Steam there.", g.name, peer.name))
        });
    } else if let Some(root) = st.selected_drive.clone() {
        export_to(st, g, root);
    }
    st.render_action(ui);
}

fn export_to(st: &mut UiState, g: Game, root: PathBuf) {
    let title = format!("Copying {} to {}", g.name, root.to_string_lossy().trim_end_matches('\\'));
    spawn_job(&st.host, title, move |host, p| {
        let pkg = transfer::export(&g, &root, &p, &|m| host.log(m))?;
        installer::copy_app_to(&root.join("SteamTransfer"));
        let _ = pkg;
        Ok(format!("{} is on the drive. Plug it into the other PC and open SteamTransfer\\SteamTransfer.exe there.", g.name))
    });
}

fn import_package(ui: &MainWindow, st: &mut UiState, folder: PathBuf) {
    let offer = match transfer::read_package(&folder) {
        Ok(o) => o,
        Err(e) => return st.toast(ui, &format!("Couldn't read that package: {e}"), "error"),
    };
    let from = format!("drive {}", util::drive_of(&folder).to_string_lossy().trim_end_matches('\\'));
    let host = st.host.clone();
    open_offer_dialog(
        ui,
        st,
        offer.clone(),
        from,
        false,
        Box::new(move |decision| {
            let Some(d) = decision else { return };
            spawn_job(&host, format!("Installing {} from drive", offer.name), move |host, p| {
                let log = |m: String| host.log(m);
                let closed = d.close_steam && steam::is_running();
                if closed {
                    steam::shutdown(&log);
                }
                let r = transfer::import(&offer, &folder, &d.library, &p, &log);
                if r.is_err() && closed {
                    steam::restart(&log);
                }
                r?;
                let (h, o) = (host.clone(), offer.clone());
                std::thread::spawn(move || h.after_receive(&o, closed));
                Ok(format!("{} is installed.", offer.name))
            });
        }),
    );
}

fn open_offer_dialog(_ui: &MainWindow, st: &mut UiState, offer: Offer, from: String, incoming: bool, done: Box<dyn FnOnce(Option<Decision>)>) {
    let libs = steam::libraries();
    if libs.is_empty() {
        installer::message("Steam isn't installed on this PC. Install Steam first, then try again.");
        return done(None);
    }
    let Ok(dlg) = OfferDialog::new() else { return done(None) };
    let existing = steam::find_installed(&offer.app_id, &libs);
    let default = existing.as_ref().and_then(|e| libs.iter().position(|l| l == e)).unwrap_or_else(|| {
        libs.iter().enumerate().max_by_key(|(_, l)| l.free_bytes()).map(|(i, _)| i).unwrap_or(0)
    });
    dlg.set_dialog_title(if incoming { "Incoming game" } else { "Install from drive" }.into());
    dlg.set_kicker(format!("{} {}", if incoming { "INCOMING FROM" } else { "INSTALL FROM" }, from.to_uppercase()).into());
    dlg.set_game_title(offer.name.clone().into());
    dlg.set_sub(format!("{} · {} files", util::size(offer.total_bytes), thousands(offer.files.len())).into());
    dlg.set_show_existing(existing.is_some());
    dlg.set_libs(ModelRc::new(VecModel::from(
        libs.iter()
            .map(|l| LibRow {
                path: l.path.to_string_lossy().to_string().into(),
                subtitle: format!("{} free{}", util::size(l.free_bytes()), if Some(l) == existing.as_ref() { " · already has this game" } else { "" }).into(),
            })
            .collect::<Vec<_>>(),
    )));
    dlg.set_selected_lib(default as i32);
    let steam_running = steam::is_running();
    dlg.set_show_close_steam(steam_running);
    dlg.set_close_steam(steam_running);
    dlg.set_yes_text(if incoming { "Accept" } else { "Install" }.into());
    dlg.set_no_text(if incoming { "Decline" } else { "Cancel" }.into());
    dlg.set_tint(tint(&offer.name));
    if let Some(img) = st.heroes.get(&offer.app_id) {
        dlg.set_hero(img.clone());
        dlg.set_has_hero(true);
    } else {
        let (weak, id) = (dlg.as_weak(), offer.app_id.clone());
        std::thread::spawn(move || {
            if let Some(buf) = load_art(&id, "hero", 1000) {
                let _ = weak.upgrade_in_event_loop(move |d| {
                    d.set_hero(Image::from_rgba8(buf));
                    d.set_has_hero(true);
                });
            }
        });
    }

    let done = Rc::new(RefCell::new(Some(done)));
    let finish = {
        let (done, weak) = (done.clone(), dlg.as_weak());
        move |d: Option<Decision>| {
            if let Some(f) = done.borrow_mut().take() {
                f(d);
            }
            if let Some(w) = weak.upgrade() {
                let _ = w.hide();
            }
            UI.with(|c| {
                if let Some(st) = c.borrow_mut().as_mut() {
                    st.dialogs.retain(|x| x.window().is_visible());
                }
            });
        }
    };
    let finish = Rc::new(finish);
    {
        let (finish, weak) = (finish.clone(), dlg.as_weak());
        let offer = offer.clone();
        dlg.on_accept(move |idx, close| {
            let Some(lib) = libs.get(idx as usize).cloned() else { return };
            let have: u64 = transfer::compute_skip(&offer, &lib.path).iter().map(|&i| offer.files[i].size).sum();
            let need = offer.total_bytes - have + 256 * 1024 * 1024;
            let free = lib.free_bytes();
            if free < need {
                if let Some(d) = weak.upgrade() {
                    d.set_error_text(format!("Not enough space on {}: needs {}, only {} free.", lib.path.display(), util::size(need), util::size(free)).into());
                }
                return;
            }
            finish(Some(Decision { library: lib, close_steam: close && steam_running }));
        });
    }
    {
        let finish = finish.clone();
        dlg.on_decline(move || finish(None));
    }
    {
        let finish = finish.clone();
        dlg.window().on_close_requested(move || {
            finish(None);
            slint::CloseRequestResponse::HideWindow
        });
    }
    let _ = dlg.show();
    st.dialogs.push(dlg);
}

// ================= startup =================

pub fn run() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;
    ui.global::<Theme>().set_icon_font(icon_font().into());
    ui.global::<Theme>().set_dark(dark_mode());
    ui.set_machine(util::machine_name().into());
    ui.set_version(format!("v{}", installer::VERSION).into());

    let shared = Arc::new(Shared {
        job: Mutex::new(None),
        discovery: Discovery::new(),
        manual: Mutex::new(Vec::new()),
        sys: Mutex::new(SysState::default()),
        recheck_firewall: std::sync::atomic::AtomicBool::new(true),
    });
    let host = UiHost(shared.clone());
    let games_model = Rc::new(VecModel::default());
    ui.set_games(games_model.clone().into());

    UI.with(|c| {
        *c.borrow_mut() = Some(UiState {
            ui: ui.as_weak(),
            shared: shared.clone(),
            host: host.clone(),
            games: vec![],
            games_model,
            capsules: HashMap::new(),
            heroes: HashMap::new(),
            query: String::new(),
            selected_game: None,
            selected_peer: None,
            selected_drive: None,
            user_picked: false,
            install_hidden: false,
            peers: vec![],
            sys: SysState::default(),
            browsed: vec![],
            log_text: String::new(),
            speed: 0.0,
            last_bytes: 0,
            last_secs: 0.0,
            last_waiting: None,
            cancel_requested: false,
            toasts: vec![],
            dialogs: vec![],
            about: None,
            tick: 0,
        })
    });

    // network
    let mut net_error = None;
    if let Err(e) = shared.discovery.start() {
        net_error = Some(e.to_string());
    }
    let server = net::Server::start(Arc::new(host.clone()));
    if let Err(e) = &server {
        net_error = Some(e.to_string());
    }
    with_ui(|ui, st| {
        st.load_games();
        st.render_chips(ui);
        st.render_peers(ui);
        st.render_drives(ui);
        st.render_packages(ui);
        if let Some(e) = net_error {
            st.log(ui, format!("Network error: {e}. Is another copy of this app already running?"));
        }
    });

    // system monitor (WMI lives on this thread)
    {
        let shared = shared.clone();
        let host = host.clone();
        std::thread::spawn(move || {
            let mut firewall: Option<bool> = None;
            loop {
                if shared.recheck_firewall.swap(false, Ordering::SeqCst) {
                    firewall = Some(firewall::rule_exists());
                    if firewall == Some(false) && installer::is_running_installed() {
                        host.log("Setting up Windows Firewall so other PCs can connect (approve the prompt once)...".into());
                        firewall = Some(firewall::add_rules());
                    }
                }
                let state = SysState {
                    links: hardware::links(),
                    drives: hardware::drives(),
                    steam_found: steam::root().is_some(),
                    steam_running: steam::is_running(),
                    usb4: hardware::has_usb4_controller(),
                    firewall,
                };
                let changed = {
                    let mut s = shared.sys.lock().unwrap();
                    let c = *s != state;
                    *s = state.clone();
                    c
                };
                if changed {
                    on_ui(move |ui, st| {
                        st.sys = state;
                        st.render_chips(ui);
                        st.render_drives(ui);
                        st.render_packages(ui);
                    });
                }
                for _ in 0..30 {
                    if shared.recheck_firewall.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
    }

    // ---------- callbacks ----------
    ui.on_select_game(|id| {
        with_ui(|ui, st| {
            st.selected_game = Some(id.to_string());
            ui.set_selected_game(id.clone());
            st.render_hero(ui);
            st.render_drives(ui);
        })
    });
    ui.on_select_peer(|key| {
        with_ui(|ui, st| {
            st.selected_peer = key.parse().ok();
            st.selected_drive = None;
            st.user_picked = true;
            ui.set_selected_peer(key);
            ui.set_selected_drive("".into());
            st.render_action(ui);
        })
    });
    ui.on_select_drive(|root| {
        with_ui(|ui, st| {
            st.selected_drive = Some(PathBuf::from(root.as_str()));
            st.selected_peer = None;
            st.user_picked = true;
            ui.set_selected_drive(root);
            ui.set_selected_peer("".into());
            st.render_action(ui);
        })
    });
    ui.on_search(|q| {
        with_ui(|ui, st| {
            st.query = q.to_string();
            st.render_games(ui);
        })
    });
    ui.on_refresh(|| with_ui(|_, st| st.load_games()));
    ui.on_add_peer(|text| {
        with_ui(|ui, st| {
            let Ok(ip) = text.trim().parse::<Ipv4Addr>() else { return st.toast(ui, "That's not a valid IPv4 address.", "error") };
            let (via, bps, kind) = net::route_to(ip);
            let mut manual = st.shared.manual.lock().unwrap();
            manual.retain(|p| p.addr != ip);
            manual.push(Peer { name: ip.to_string(), addr: ip, via, bps, kind, manual: true });
            drop(manual);
            st.selected_peer = Some(ip);
            st.selected_drive = None;
            st.user_picked = true;
            st.render_peers(ui);
        })
    });
    ui.on_go(|| with_ui(go));
    ui.on_browse_export(|| {
        with_ui(|ui, st| {
            let Some(g) = st.selected().cloned() else { return st.toast(ui, "Pick a game first.", "info") };
            if let Some(folder) = rfd::FileDialog::new().set_title("Where should the game be copied to?").pick_folder() {
                export_to(st, g, folder);
            }
        })
    });
    ui.on_browse_import(|| {
        with_ui(|ui, st| {
            let Some(folder) = rfd::FileDialog::new().set_title("Pick the drive or folder that has the copied game").pick_folder() else { return };
            let found = hardware::find_packages(&folder);
            if found.is_empty() {
                return st.toast(ui, "No copied games found in that folder.", "error");
            }
            let n = found.len();
            st.browsed.extend(found);
            st.render_packages(ui);
            st.toast(ui, &format!("Found {n} game{} — click Install.", if n > 1 { "s" } else { "" }), "ok");
        })
    });
    ui.on_import_package(|folder| with_ui(|ui, st| import_package(ui, st, PathBuf::from(folder.as_str()))));
    ui.on_pause(|| {
        with_ui(|ui, st| {
            if let Some(p) = st.progress() {
                p.pause();
                st.log(ui, "Paused.".into());
                st.update_progress(ui);
            }
        })
    });
    ui.on_resume(|| {
        with_ui(|ui, st| {
            if let Some(p) = st.progress() {
                p.resume();
                st.speed = 0.0;
                st.last_bytes = p.done();
                st.last_secs = p.active_time().as_secs_f64();
                st.log(ui, "Resumed.".into());
                st.update_progress(ui);
            }
        })
    });
    ui.on_cancel(|| {
        with_ui(|ui, st| {
            if let Some(p) = st.progress() {
                st.cancel_requested = true;
                p.cancel();
                st.log(ui, "Cancelling...".into());
            }
        })
    });
    ui.on_close_transfer(|| with_ui(|ui, _| ui.set_transfer_visible(false)));
    ui.on_hide_install(|| {
        with_ui(|ui, st| {
            st.install_hidden = true;
            st.render_chips(ui);
        })
    });
    ui.on_install(|| {
        with_ui(|ui, st| {
            st.toast(ui, "Installing… approve the Windows prompt so other PCs can connect.", "info");
            let shared = st.shared.clone();
            std::thread::spawn(move || {
                let r = installer::install();
                on_ui(move |ui, st| match r {
                    Ok(()) if !installer::is_running_installed() => {
                        if st.busy() {
                            return st.toast(ui, "Installed. Reopen the app from the Start menu after the transfer.", "ok");
                        }
                        installer::launch_installed();
                        let _ = ui.hide();
                        let _ = slint::quit_event_loop();
                    }
                    Ok(()) => {
                        shared.recheck_firewall.store(true, Ordering::SeqCst);
                        st.toast(ui, "Installed.", "ok");
                    }
                    Err(e) => st.toast(ui, &format!("Install failed: {e}"), "error"),
                });
            });
        })
    });
    ui.on_about(|| {
        with_ui(|_, st| {
            if st.about.is_none() {
                let Ok(d) = AboutDialog::new() else { return };
                d.set_version(installer::VERSION.into());
                let weak = d.as_weak();
                d.on_dismiss(move || {
                    if let Some(d) = weak.upgrade() {
                        let _ = d.hide();
                    }
                });
                st.about = Some(d);
            }
            if let Some(d) = &st.about {
                let _ = d.show();
            }
        })
    });
    ui.on_chip_action(|action| {
        if action == "firewall" {
            with_ui(|_, st| {
                let shared = st.shared.clone();
                std::thread::spawn(move || {
                    firewall::add_rules();
                    shared.recheck_firewall.store(true, Ordering::SeqCst);
                });
            })
        }
    });

    ui.window().on_close_requested(|| {
        let mut close = true;
        with_ui(|_, st| {
            if let Some(p) = st.progress() {
                close = installer::ask_yes_no("A transfer is running. Quit anyway?\nYou can resume it later — finished files are kept.");
                if close {
                    p.cancel();
                }
            }
        });
        if close {
            let _ = slint::quit_event_loop();
            slint::CloseRequestResponse::HideWindow
        } else {
            slint::CloseRequestResponse::KeepWindowShown
        }
    });

    // ---------- timer ----------
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(250), || {
        with_ui(|ui, st| {
            st.tick += 1;
            st.update_progress(ui);
            if st.tick % 4 == 0 {
                st.render_peers(ui);
            }
            let before = st.toasts.len();
            st.toasts.retain(|(_, until)| *until > Instant::now());
            if st.toasts.len() != before {
                st.render_toasts(ui);
            }
        })
    });

    ui.run()?;
    drop(server);
    Ok(())
}

// ================= helpers =================

/// Local wall-clock time for log lines (std has no time-zone support; ask Windows).
fn clock() -> String {
    let t = unsafe {
        let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t);
        t
    };
    format!("{:02}:{:02}:{:02}", t.wHour, t.wMinute, t.wSecond)
}

fn kind_name(kind: &str) -> &'static str {
    match kind {
        "usb4" => "USB4 / Thunderbolt",
        "wifi" => "Wi-Fi",
        _ => "Ethernet",
    }
}

fn initials(name: &str) -> String {
    let s: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(|c| c.to_uppercase())
        .collect();
    if s.is_empty() { "?".into() } else { s }
}

fn tint(name: &str) -> slint::Color {
    let h = name.chars().fold(0u32, |h, c| (h.wrapping_mul(31).wrapping_add(c as u32)) % 360) as f32;
    hsl(h, 0.55, 0.36)
}

fn hsl(h: f32, s: f32, l: f32) -> slint::Color {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h as u32 {
        0..=59 => (c, x, 0.0),
        60..=119 => (x, c, 0.0),
        120..=179 => (0.0, c, x),
        180..=239 => (0.0, x, c),
        240..=299 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    slint::Color::from_rgb_u8(((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8)
}

/// Decode a cover image off the UI thread (scaled down to save memory).
fn load_art(app_id: &str, kind: &str, max_w: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let path = steam::find_art(app_id, kind)?;
    let img = image::open(&path).ok()?;
    let img = if img.width() > max_w { img.resize(max_w, max_w * 3, image::imageops::FilterType::Triangle) } else { img };
    let rgba = img.to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height()))
}

fn icon_font() -> &'static str {
    let windir = std::env::var("WINDIR").unwrap_or(r"C:\Windows".into());
    if Path::new(&windir).join(r"Fonts\SegoeIcons.ttf").exists() { "Segoe Fluent Icons" } else { "Segoe MDL2 Assets" }
}

fn dark_mode() -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
        .map(|v| v == 0)
        .unwrap_or(true)
}

fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}
