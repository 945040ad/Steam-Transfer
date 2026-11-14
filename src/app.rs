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
use crate::net::{self, Decision, Discovery, Host, JobEnd, Peer, Route, firewall};
use crate::launchers::Launcher;
use crate::steam::{self, Game, Library};
use crate::transfer::{self, Offer, Progress};
use crate::util;
use crate::{AboutDialog, SettingsDialog, ChipRow, DriveRow, FilterRow, GameRow, LibRow, MainWindow, OfferDialog, PackageRow, PeerRow, Theme, ToastRow};

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
    route: Route,
}

pub struct Shared {
    job: Mutex<Option<Job>>,
    discovery: Discovery,
    manual: Mutex<Vec<Peer>>,
    sys: Mutex<SysState>,
    recheck_firewall: std::sync::atomic::AtomicBool,
    settings: Mutex<crate::settings::Settings>,
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

    fn begin_job(&self, title: String, route: Route) -> Option<Arc<Progress>> {
        let mut job = self.0.job.lock().unwrap();
        if job.is_some() {
            on_ui(|ui, st| st.toast(ui, "Another transfer is still running.", "error"));
            return None;
        }
        let p = Arc::new(Progress::new());
        *job = Some(Job { progress: p.clone(), title: title.clone(), route });
        drop(job);
        on_ui(move |ui, st| {
            st.log(ui, format!("{title}..."));
            st.speed = 0.0;
            st.last_bytes = 0;
            st.last_secs = 0.0;
            st.last_waiting = None;
            st.cancel_requested = false;
            st.window.clear();
            // the graph starts as a flat line across the whole minute, so speed visibly rises from zero
            st.samples = std::iter::repeat_n(0.0, GRAPH_POINTS).collect();
            st.shown_speed = 0.0;
            st.peak_speed = 0.0;
            st.graph_max = 1.0e6;
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
            if let Some(job) = &job {
                let (done, t) = (job.progress.done(), job.progress.active_time().as_secs_f64());
                ui.set_t_speed(speed_text(if t > 0.0 { done as f64 / t } else { 0.0 }).into());
                ui.set_t_speed_sub("average speed".into());
                ui.set_t_done(util::size(done).into());
            }
            ui.set_t_eta(match state { "ok" => "Done", "error" => "Failed", _ => "Stopped" }.into());
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

    fn after_receive(&self, offer: &Offer, dest: &Path, steam_was_closed: bool) {
        on_ui(|_, st| st.load_games());
        let log = |m: String| self.log(m);
        if offer.launcher() == Launcher::App {
            let target = crate::apps::install_target(&offer.meta).map(|p| p.display().to_string()).unwrap_or_default();
            self.log(format!("{} is installed in {target}. Find it in the Start menu.", offer.name));
            return;
        }
        if !offer.is_steam() {
            let launcher = offer.launcher();
            match crate::launchers::after_copy_steps(launcher, &offer.name, dest) {
                Some(steps) => {
                    self.log(format!("Last step for {}: {steps}", offer.name));
                    installer::message(&format!("{} is copied to:\n{}\n\nLast step:\n{steps}", offer.name, dest.display()));
                }
                None => self.log(format!("{} is ready in {} — open {} to play.", offer.name, dest.display(), launcher.name())),
            }
            return;
        }
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
    /// One persistent model: adding/removing a toast doesn't re-create (and re-animate) the others.
    toast_model: Rc<VecModel<ToastRow>>,
    /// (active seconds, bytes done) over the last ~1.2 s: the "speed right now".
    window: std::collections::VecDeque<(f64, u64)>,
    /// Speed history for the live graph, one sample every 0.5 s.
    samples: std::collections::VecDeque<f64>,
    last_sample: Instant,
    /// The big speed number glides toward the measured value instead of jumping.
    shown_speed: f64,
    peak_speed: f64,
    graph_max: f64,
    dialogs: Vec<OfferDialog>,
    about: Option<AboutDialog>,
    settings_dialog: Option<SettingsDialog>,
    tick: u64,
    /// Show only one launcher's games (None = all).
    filter: Option<Launcher>,
    /// Game folders added by hand ("Add a game folder…").
    extra: Vec<Game>,
}

/// Unique id of a game across launchers (an Epic id could collide with a Steam app id).
fn game_key(g: &Game) -> String {
    format!("{}:{}", g.launcher.id(), g.app_id)
}

fn dir_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path).into_iter().flatten().filter(|e| e.file_type().is_file()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum()
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

    fn toast(&mut self, _ui: &MainWindow, text: &str, kind: &str) {
        let row = ToastRow { text: text.into(), kind: kind.into() };
        self.toasts.push((row.clone(), Instant::now() + Duration::from_secs_f32(if kind == "error" { 7.0 } else { 4.5 })));
        self.toast_model.push(row);
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
            self.toast_model.remove(0);
        }
    }

    fn expire_toasts(&mut self) {
        let now = Instant::now();
        let mut i = 0;
        while i < self.toasts.len() {
            if self.toasts[i].1 <= now {
                self.toasts.remove(i);
                self.toast_model.remove(i);
            } else {
                i += 1;
            }
        }
    }

    fn busy(&self) -> bool {
        self.shared.job.lock().unwrap().is_some()
    }

    // ---------- games ----------

    fn load_games(&mut self) {
        let extra = self.extra.clone();
        std::thread::spawn(move || {
            let mut games = crate::launchers::all_games();
            let lower = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
            for g in extra {
                if !games.iter().any(|x| lower(&x.install_path()) == lower(&g.install_path())) {
                    games.push(g);
                }
            }
            games.sort_by_key(|g| g.name.to_lowercase());
            let steam_ids: Vec<String> = games.iter().filter(|g| g.is_steam()).map(|g| g.app_id.clone()).collect();
            let to_measure: Vec<(String, PathBuf)> = games.iter().filter(|g| !g.is_steam() && g.size_on_disk == 0).map(|g| (game_key(g), g.library.path.clone())).collect();
            on_ui(move |ui, st| {
                st.games = games;
                if st.selected_game.as_ref().is_some_and(|s| !st.games.iter().any(|g| &game_key(g) == s)) {
                    st.selected_game = None;
                }
                ui.set_games_count(if st.games.is_empty() { SharedString::new() } else { st.games.len().to_string().into() });
                st.render_games(ui);
                st.render_hero(ui);
                st.render_drives(ui);
                st.render_action(ui);
            });
            // cover art (Steam keeps a local cache) and folder sizes, in the background
            for id in steam_ids {
                if let Some(buf) = load_art(&id, "capsule", 320) {
                    on_ui(move |ui, st| {
                        let img = Image::from_rgba8(buf);
                        st.capsules.insert(id.clone(), img.clone());
                        st.apply_art(ui, &id, img);
                    });
                }
            }
            for (key, path) in to_measure {
                let size = dir_size(&path);
                on_ui(move |ui, st| {
                    if let Some(g) = st.games.iter_mut().find(|g| game_key(g) == key) {
                        g.size_on_disk = size;
                    }
                    st.render_games(ui);
                    if st.selected_game.as_deref() == Some(key.as_str()) {
                        st.render_hero(ui);
                        st.render_drives(ui);
                        st.render_action(ui);
                    }
                });
            }
        });
    }

    fn apply_art(&mut self, _ui: &MainWindow, id: &str, img: Image) {
        let key = format!("{}:{id}", Launcher::Steam.id());
        for i in 0..self.games_model.row_count() {
            if let Some(mut row) = self.games_model.row_data(i) {
                if row.app_id == key {
                    row.art = img.clone();
                    row.has_art = true;
                    self.games_model.set_row_data(i, row);
                }
            }
        }
    }

    fn render_games(&mut self, ui: &MainWindow) {
        let q = self.query.to_lowercase();
        // launcher filter chips (only when more than one launcher has games)
        let mut counts: Vec<(Launcher, usize)> = Launcher::ALL.iter().map(|l| (*l, self.games.iter().filter(|g| g.launcher == *l).count())).filter(|(_, n)| *n > 0).collect();
        counts.sort_by_key(|(l, _)| *l);
        if self.filter.is_some_and(|f| !counts.iter().any(|(l, _)| *l == f)) {
            self.filter = None;
        }
        let mixed = counts.len() > 1;
        let mut filters = vec![FilterRow { id: "all".into(), label: format!("All  {}", self.games.len()).into() }];
        filters.extend(counts.iter().map(|(l, n)| FilterRow { id: l.id().into(), label: format!("{}  {n}", l.name()).into() }));
        ui.set_filters(ModelRc::new(VecModel::from(if mixed { filters } else { vec![] })));
        ui.set_selected_filter(self.filter.map(|l| l.id()).unwrap_or("all").into());

        let rows: Vec<GameRow> = self
            .games
            .iter()
            .filter(|g| self.filter.is_none_or(|f| g.launcher == f))
            .filter(|g| q.is_empty() || g.name.to_lowercase().contains(&q))
            .map(|g| {
                let art = if g.is_steam() { self.capsules.get(&g.app_id).cloned() } else { None };
                let (r, gr, b) = g.launcher.color();
                GameRow {
                    app_id: game_key(g).into(),
                    name: g.name.clone().into(),
                    meta: format!("{} · {}", if g.size_on_disk > 0 { util::size(g.size_on_disk) } else { "—".into() }, util::drive_of(&g.library.path).to_string_lossy().trim_end_matches('\\')).into(),
                    initials: initials(&g.name).into(),
                    tint: tint(&g.name),
                    has_art: art.is_some(),
                    art: art.unwrap_or_default(),
                    needs_update: !g.fully_installed(),
                    badge: if mixed { g.launcher.badge().into() } else { SharedString::new() },
                    badge_color: slint::Color::from_rgb_u8(r, gr, b),
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
        self.games.iter().find(|g| &game_key(g) == id)
    }

    fn render_hero(&mut self, ui: &MainWindow) {
        ui.set_selected_game(self.selected_game.clone().unwrap_or_default().into());
        let Some(g) = self.selected().cloned() else { return };
        ui.set_hero_title(g.name.clone().into());
        ui.set_hero_sub(
            format!("{}{} · {}", if g.size_on_disk > 0 { format!("{} · ", util::size(g.size_on_disk)) } else { String::new() }, g.launcher.name(), g.install_path().display()).into(),
        );
        ui.set_hero_tint(tint(&g.name));
        if !g.is_steam() {
            ui.set_hero_has_art(false);
            return;
        }
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
                            if st.selected_game.as_deref() == Some(format!("steam:{id}").as_str()) {
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
        let (p, title, route) = {
            let guard = self.shared.job.lock().unwrap();
            match guard.as_ref() {
                Some(j) => (j.progress.clone(), j.title.clone(), j.route.clone()),
                None => return,
            }
        };
        let done = p.done();
        let total = p.total.load(Ordering::Relaxed);
        let secs = p.active_time().as_secs_f64();
        let waiting = p.waiting();
        let paused = p.is_paused();
        let stalled = waiting.is_some() || paused;

        // speed right now: bytes moved over the last ~1.2 s
        self.window.push_back((secs, done));
        while self.window.len() > 2 && secs - self.window[0].0 > 1.2 {
            self.window.pop_front();
        }
        let now_speed = match (self.window.front(), self.window.back()) {
            _ if stalled => 0.0,
            (Some(a), Some(b)) if b.0 - a.0 > 0.2 => b.1.saturating_sub(a.1) as f64 / (b.0 - a.0),
            _ => self.shown_speed,
        };
        // the big number glides to the new value; ETA uses a calmer average
        self.shown_speed += (now_speed - self.shown_speed) * 0.45;
        if self.shown_speed < 1024.0 {
            self.shown_speed = 0.0;
        }
        self.peak_speed = self.peak_speed.max(now_speed);
        if stalled {
            self.speed = 0.0;
        } else if secs - self.last_secs >= 0.5 {
            self.speed = if self.speed == 0.0 { now_speed } else { self.speed * 0.7 + now_speed * 0.3 };
            self.last_bytes = done;
            self.last_secs = secs;
        }
        // live graph: a sample every half second of wall time (flat while paused)
        if self.last_sample.elapsed() >= Duration::from_millis(240) || self.samples.is_empty() {
            self.last_sample = Instant::now();
            self.samples.push_back(self.shown_speed);
            while self.samples.len() > GRAPH_POINTS {
                self.samples.pop_front();
            }
            // the vertical scale eases toward the peak (quickly up, slowly down) instead of snapping
            let target = self.samples.iter().cloned().fold(1.0e6, f64::max) * 1.18;
            self.graph_max += (target - self.graph_max) * if target > self.graph_max { 0.35 } else { 0.06 };
            let (line, fill) = graph_paths(&self.samples, self.graph_max.max(1.0e6));
            ui.set_t_graph(line.into());
            ui.set_t_graph_fill(fill.into());
        }

        let pct = if total > 0 { (done as f64 / total as f64).min(1.0) } else { 0.0 };
        let current = p.current.lock().unwrap().clone();
        ui.set_transfer_visible(true);
        ui.set_t_running(true);
        ui.set_t_paused(paused);
        ui.set_t_title(title.into());
        ui.set_t_indeterminate(total == 0 && !paused);
        ui.set_t_progress(pct as f32);
        ui.set_t_percent(if total > 0 { format!("{}%", (pct * 100.0) as u32) } else { String::new() }.into());
        ui.set_t_speed(speed_text(self.shown_speed).into());
        let avg = if secs > 0.5 { done as f64 / secs } else { 0.0 };
        ui.set_t_speed_sub(if self.shown_speed > 0.0 { format!("{}  ·  avg {}", bits_text(self.shown_speed), speed_text(avg)) } else { format!("avg {}", speed_text(avg)) }.into());
        ui.set_t_done(util::size(done).into());
        ui.set_t_total(if total > 0 { format!("of {}", util::size(total)) } else { "preparing…".into() }.into());
        ui.set_t_eta(
            if paused {
                "Paused".into()
            } else if waiting.is_some() {
                "Waiting".into()
            } else if total > 0 && self.speed > 1.0 {
                util::time(Duration::from_secs_f64((total - done.min(total)) as f64 / self.speed))
            } else {
                "—".into()
            }
            .into(),
        );
        self.render_route(ui, &route);
        let (state, stats) = if let Some(w) = &waiting {
            ("waiting", w.clone())
        } else if paused {
            ("paused", "Paused — press Resume to continue".to_string())
        } else if total > 0 {
            ("", String::new())
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

    /// Endpoints, connection and cover art of the running transfer.
    fn render_route(&mut self, ui: &MainWindow, r: &Route) {
        let glyph = |kind: &str| if kind == "drive" { "\u{E88E}" } else { "\u{E977}" };
        ui.set_t_from(r.from.clone().into());
        ui.set_t_to(r.to.clone().into());
        ui.set_t_from_glyph(glyph(&r.from_kind).into());
        ui.set_t_to_glyph(glyph(&r.to_kind).into());
        let link = [r.from_kind.as_str(), r.to_kind.as_str()]
            .into_iter()
            .find(|k| *k != "pc")
            .map(|k| match k {
                "drive" => "USB drive",
                "usb4" => "USB4 / Thunderbolt",
                "wifi" => "Wi-Fi",
                _ => "Ethernet",
            })
            .unwrap_or("Network");
        ui.set_t_link(link.into());
        ui.set_t_tint(tint(&r.game));
        ui.set_t_initials(initials(&r.game).into());
        match self.capsules.get(&r.art_id) {
            Some(img) if !r.art_id.is_empty() => {
                ui.set_t_art(img.clone());
                ui.set_t_has_art(true);
            }
            _ => ui.set_t_has_art(false),
        }
    }
}

/// How many speed samples the live graph shows (a quarter second each → one minute): small steps, so it flows.
const GRAPH_POINTS: usize = 240;

/// Size of the speed graph in logical pixels; the path is drawn in exactly this space (Slint keeps a Path's aspect ratio).
const GRAPH_W: f64 = 380.0;
const GRAPH_H: f64 = 74.0;

/// SVG path commands for the speed graph line and the area under it. Newest sample on the right.
fn graph_paths(samples: &std::collections::VecDeque<f64>, max: f64) -> (String, String) {
    if samples.len() < 2 {
        return (format!("M 0 {0} L {GRAPH_W} {0}", GRAPH_H - 1.0), format!("M 0 {GRAPH_H} L {GRAPH_W} {GRAPH_H} Z"));
    }
    let step = GRAPH_W / (GRAPH_POINTS - 1) as f64;
    let n = samples.len();
    let pts: Vec<(f64, f64)> = samples.iter().enumerate().map(|(i, v)| (GRAPH_W - (n - 1 - i) as f64 * step, GRAPH_H - 1.5 - (v / max) * (GRAPH_H - 14.0))).collect();
    let mut line = format!("M {:.2} {:.2}", pts[0].0, pts[0].1);
    for (x, y) in &pts[1..] {
        line.push_str(&format!(" L {x:.2} {y:.2}"));
    }
    let fill = format!("{line} L {GRAPH_W} {GRAPH_H} L {:.2} {GRAPH_H} Z", pts[0].0);
    (line, fill)
}

fn speed_text(bytes_per_sec: f64) -> String {
    if bytes_per_sec <= 0.0 { "0 MB/s".into() } else { format!("{}/s", util::size(bytes_per_sec as u64)) }
}

fn bits_text(bytes_per_sec: f64) -> String {
    let bits = bytes_per_sec * 8.0;
    if bits >= 1e9 { format!("{:.2} Gbps", bits / 1e9) } else { format!("{:.0} Mbps", bits / 1e6) }
}

// ================= actions =================

fn spawn_job(host: &UiHost, title: String, route: Route, work: impl FnOnce(&UiHost, Arc<Progress>) -> std::io::Result<String> + Send + 'static) {
    let host = host.clone();
    std::thread::spawn(move || {
        let Some(p) = host.begin_job(title, route) else { return };
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
    if g.is_steam() && !g.fully_installed() && !installer::ask_yes_no(&format!("{} isn't fully up to date on this PC. Send it anyway?\nSteam will finish the update on the other side.", g.name)) {
        return;
    }
    if let Some(peer) = st.selected_peer.and_then(|ip| st.peers.iter().find(|p| p.addr == ip).cloned()) {
        let title = format!("Sending {} to {}", g.name, peer.name);
        let route = Route { from: "This PC".into(), to: peer.name.clone(), from_kind: "pc".into(), to_kind: peer.kind.into(), game: g.name.clone(), art_id: if g.is_steam() { g.app_id.clone() } else { String::new() } };
        spawn_job(&st.host, title, route, move |host, p| {
            let (g, _staging) = staged(&g, host)?;
            net::send(&g, peer.addr, &p, &|m| host.log(m))?;
            Ok(if g.is_steam() {
                format!("{} is on {}. It shows up in Steam there.", g.name, peer.name)
            } else if g.launcher == Launcher::App {
                format!("{} is installed on {}, with its settings and shortcuts.", g.name, peer.name)
            } else {
                format!("{} is on {}. The other PC shows how to finish adding it to {}.", g.name, peer.name, g.launcher.name())
            })
        });
    } else if let Some(root) = st.selected_drive.clone() {
        export_to(st, g, root);
    }
    st.render_action(ui);
}

fn export_to(st: &mut UiState, g: Game, root: PathBuf) {
    let title = format!("Copying {} to {}", g.name, root.to_string_lossy().trim_end_matches('\\'));
    let route = Route {
        from: "This PC".into(),
        to: format!("Drive {}", util::drive_of(&root).to_string_lossy().trim_end_matches('\\')),
        from_kind: "pc".into(),
        to_kind: "drive".into(),
        game: g.name.clone(),
        art_id: if g.is_steam() { g.app_id.clone() } else { String::new() },
    };
    spawn_job(&st.host, title, route, move |host, p| {
        let app_folder = root.join("SteamTransfer");
        keep_app_copy(host, &app_folder); // first, so it's there even if the copy is cancelled
        let (g, _staging) = staged(&g, host)?;
        transfer::export(&g, &root, &p, &|m| host.log(m))?;
        keep_app_copy(host, &app_folder);
        Ok(format!("{} is on the drive. Plug it into the other PC and open SteamTransfer\\SteamTransfer.exe there.", g.name))
    });
}

/// Apps are sent from a junction staging folder (program + its AppData folders); everything else as-is.
fn staged(g: &Game, host: &UiHost) -> std::io::Result<(Game, Option<crate::apps::Staging>)> {
    if g.launcher != Launcher::App {
        return Ok((g.clone(), None));
    }
    host.log(format!("Collecting {}'s program folder, settings and shortcuts...", g.name));
    let (sg, st) = crate::apps::prepare_send(g, &|m| host.log(m))?;
    Ok((sg, Some(st)))
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
            let route = Route {
                from: format!("Drive {}", util::drive_of(&folder).to_string_lossy().trim_end_matches('\\')),
                to: "This PC".into(),
                from_kind: "drive".into(),
                to_kind: "pc".into(),
                game: offer.name.clone(),
                art_id: if offer.is_steam() { offer.app_id.clone() } else { String::new() },
            };
            spawn_job(&host, format!("Installing {} from drive", offer.name), route, move |host, p| {
                // apps: keep the staging junctions until the copy is done (closures capture fields one by one,
                // so without this line `d.staging` would be dropped — and unlinked — before the job starts)
                let _staging = d.staging.clone();
                let log = |m: String| host.log(m);
                let closed = d.close_steam && steam::is_running();
                if closed {
                    steam::shutdown(&log);
                }
                // the app may have been deleted or moved off the drive: put a working copy back for the next person
                let app_folder = util::drive_of(&folder).join("SteamTransfer");
                keep_app_copy(host, &app_folder);
                let r = transfer::import(&offer, &folder, &d.library, &p, &log);
                keep_app_copy(host, &app_folder);
                if r.is_err() && closed {
                    steam::restart(&log);
                }
                r?;
                let dest = d.library.path.clone();
                if !offer.is_steam() {
                    if let Err(e) = crate::launchers::register(offer.launcher(), &offer.meta, &dest, &log) {
                        log(format!("Couldn't register the game with {}: {e}", offer.launcher().name()));
                    }
                }
                let (h, o) = (host.clone(), offer.clone());
                std::thread::spawn(move || h.after_receive(&o, &dest, closed));
                Ok(format!("{} is installed.", offer.name))
            });
        }),
    );
}

/// Folder name for a received non-Steam game: the sender's folder name, made safe.
fn safe_folder_name(offer: &Offer) -> String {
    let raw = if offer.folder.trim().is_empty() { offer.name.as_str() } else { offer.folder.as_str() };
    let s: String = raw.chars().map(|c| if r#"<>:"/\|?*"#.contains(c) || c.is_control() { '_' } else { c }).collect();
    let s = s.trim().trim_matches('.').trim().to_string();
    if s.is_empty() { "Game".into() } else { s }
}

fn lib_rows(cands: &[Library], existing: Option<usize>) -> ModelRc<LibRow> {
    ModelRc::new(VecModel::from(
        cands
            .iter()
            .enumerate()
            .map(|(i, l)| LibRow {
                path: l.path.to_string_lossy().to_string().into(),
                subtitle: format!("{} free{}", util::size(l.free_bytes()), if Some(i) == existing { " · already has this game" } else { "" }).into(),
            })
            .collect::<Vec<_>>(),
    ))
}

fn open_offer_dialog(_ui: &MainWindow, st: &mut UiState, offer: Offer, from: String, incoming: bool, done: Box<dyn FnOnce(Option<Decision>)>) {
    let launcher = offer.launcher();
    // Where it can go: Steam libraries, or for other launchers a folder named after the game inside a suggested parent.
    let (cands, existing): (Vec<Library>, Option<usize>) = if offer.is_steam() {
        let libs = steam::libraries();
        if libs.is_empty() {
            installer::message("Steam isn't installed on this PC. Install Steam first, then try again.");
            return done(None);
        }
        let existing = steam::find_installed(&offer.app_id, &libs).and_then(|e| libs.iter().position(|l| *l == e));
        (libs, existing)
    } else if launcher == Launcher::App {
        // apps go back where they were, inside this user's profile
        match crate::apps::install_target(&offer.meta) {
            Ok(t) => {
                let existing = std::fs::read_dir(&t).is_ok_and(|mut d| d.next().is_some()).then_some(0);
                (vec![Library { path: t }], existing)
            }
            Err(e) => {
                installer::message(&e.to_string());
                return done(None);
            }
        }
    } else {
        let folder = safe_folder_name(&offer);
        let cands: Vec<Library> = crate::launchers::destination_parents(launcher).into_iter().map(|p| Library { path: p.join(&folder) }).collect();
        let existing = cands.iter().position(|c| std::fs::read_dir(&c.path).is_ok_and(|mut d| d.next().is_some()));
        (cands, existing)
    };
    let Ok(dlg) = OfferDialog::new() else { return done(None) };
    let default = existing.unwrap_or_else(|| if offer.is_steam() { cands.iter().enumerate().max_by_key(|(_, l)| l.free_bytes()).map(|(i, _)| i).unwrap_or(0) } else { 0 });
    dlg.set_dialog_title(if incoming { "Incoming game" } else { "Install from drive" }.into());
    let kicker = if offer.is_steam() { from.to_uppercase() } else { format!("{} · {}", from.to_uppercase(), launcher.name().to_uppercase()) };
    dlg.set_kicker(format!("{} {kicker}", if incoming { "INCOMING FROM" } else { "INSTALL FROM" }).into());
    dlg.set_game_title(offer.name.clone().into());
    dlg.set_sub(format!("{} · {} files", util::size(offer.total_bytes), thousands(offer.files.len())).into());
    dlg.set_show_existing(existing.is_some());
    dlg.set_libs(lib_rows(&cands, existing));
    dlg.set_selected_lib(default as i32);
    dlg.set_show_browse(!offer.is_steam() && launcher != Launcher::App);
    dlg.set_note(
        match launcher {
            Launcher::Steam => String::new(),
            Launcher::Epic => "It will be registered with the Epic Games Launcher automatically.".into(),
            Launcher::Other => "Copied as a plain folder.".into(),
            Launcher::App => "Installed for your Windows account with its settings and Start-menu shortcuts. No administrator rights needed.".into(),
            l => format!("After copying, you'll be shown how to add it to {} without downloading it again.", l.name()),
        }
        .into(),
    );
    let steam_running = offer.is_steam() && steam::is_running();
    dlg.set_show_close_steam(steam_running);
    dlg.set_close_steam(steam_running);
    dlg.set_yes_text(if incoming { "Accept" } else { "Install" }.into());
    dlg.set_no_text(if incoming { "Decline" } else { "Cancel" }.into());
    dlg.set_tint(tint(&offer.name));
    let cands = Rc::new(RefCell::new(cands));
    if !offer.is_steam() && launcher != Launcher::App {
        let (cands, weak, folder) = (cands.clone(), dlg.as_weak(), safe_folder_name(&offer));
        dlg.on_browse(move || {
            let Some(parent) = rfd::FileDialog::new().set_title("Install the game inside which folder?").pick_folder() else { return };
            let lib = Library { path: parent.join(&folder) };
            let mut c = cands.borrow_mut();
            let idx = c.iter().position(|l| l.path == lib.path).unwrap_or_else(|| {
                c.push(lib);
                c.len() - 1
            });
            if let Some(d) = weak.upgrade() {
                d.set_libs(lib_rows(&c, None));
                d.set_selected_lib(idx as i32);
                d.set_error_text("".into());
            }
        });
    }
    if !offer.is_steam() {
        // no Steam cover art for other launchers: the coloured tile stays
    } else if let Some(img) = st.heroes.get(&offer.app_id) {
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
        let cands = cands.clone();
        dlg.on_accept(move |idx, close| {
            let Some(lib) = cands.borrow().get(idx as usize).cloned() else { return };
            let fail = |msg: String| {
                if let Some(d) = weak.upgrade() {
                    d.set_error_text(msg.into());
                }
            };
            let mut staging = None;
            let lib = if offer.launcher() == Launcher::App {
                match crate::apps::prepare_receive(&offer, &|_| {}) {
                    Ok(st) => {
                        let l = Library { path: st.root.clone() };
                        staging = Some(Arc::new(st));
                        l
                    }
                    Err(e) => return fail(e.to_string()),
                }
            } else {
                lib
            };
            if !offer.is_steam() && !crate::launchers::writable(&lib.path) {
                return fail(format!("Can't write to {} (it needs administrator rights). Pick another folder with Browse…", lib.path.display()));
            }
            let have: u64 = transfer::compute_skip(&offer, &lib.path).iter().map(|&i| offer.files[i].size).sum();
            let need = offer.total_bytes - have + 256 * 1024 * 1024;
            let free = lib.free_bytes();
            if free < need {
                return fail(format!("Not enough space on {}: needs {}, only {} free.", lib.path.display(), util::size(need), util::size(free)));
            }
            finish(Some(Decision { library: lib, close_steam: close && steam_running, staging }));
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
    ui.global::<Theme>().set_accent(accent_color(dark_mode()));
    let toast_model = Rc::new(VecModel::<ToastRow>::default());
    ui.set_toasts(toast_model.clone().into());
    ui.set_machine(util::machine_name().into());
    ui.set_version(format!("v{}", installer::VERSION).into());

    let shared = Arc::new(Shared {
        job: Mutex::new(None),
        discovery: Discovery::new(),
        manual: Mutex::new(Vec::new()),
        sys: Mutex::new(SysState::default()),
        recheck_firewall: std::sync::atomic::AtomicBool::new(true),
        settings: Mutex::new(crate::settings::load()),
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
            toast_model: toast_model.clone(),
            window: Default::default(),
            samples: Default::default(),
            last_sample: Instant::now(),
            shown_speed: 0.0,
            peak_speed: 0.0,
            graph_max: 1.0e6,
            dialogs: vec![],
            about: None,
            settings_dialog: None,
            tick: 0,
            filter: None,
            extra: vec![],
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
            let changed = st.selected_game.as_deref() != Some(id.as_str());
            st.selected_game = Some(id.to_string());
            ui.set_selected_game(id.clone());
            if changed {
                // drop and re-create the hero content so its entrance (cover settle, title rise) plays again
                ui.set_hero_shown(false);
                let weak = ui.as_weak();
                slint::Timer::single_shot(Duration::from_millis(16), move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_hero_shown(true);
                    }
                });
            }
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
    ui.on_set_filter(|id| {
        with_ui(|ui, st| {
            st.filter = if id == "all" { None } else { Some(Launcher::from_id(&id)) };
            st.render_games(ui);
        })
    });
    ui.on_add_folder(|| {
        with_ui(|ui, st| {
            let Some(folder) = rfd::FileDialog::new().set_title("Pick the game's folder (any launcher)").pick_folder() else { return };
            if folder.parent().is_none() {
                return st.toast(ui, "Pick the game's own folder, not a whole drive.", "error");
            }
            let g = crate::launchers::folder_game(&folder);
            let key = game_key(&g);
            st.extra.retain(|x| game_key(x) != key);
            st.extra.push(g);
            st.selected_game = Some(key);
            st.filter = None;
            st.load_games();
        })
    });
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
    ui.on_delete_package(|folder| with_ui(|ui, st| delete_package(ui, st, PathBuf::from(folder.as_str()))));
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
    ui.on_settings(|| with_ui(|_, st| open_settings(st)));
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

    // Demo: STEAMXFER_DEMO=1 runs a simulated transfer (no files touched) to preview the transfer view.
    if std::env::var("STEAMXFER_DEMO").is_ok() {
        start_demo(&host);
    }

    // ---------- timer ----------
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(250), || {
        with_ui(|ui, st| {
            st.tick += 1;
            st.update_progress(ui);
            if st.tick % 4 == 0 {
                st.render_peers(ui);
            }
            st.expire_toasts();
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

/// The Windows accent colour (DWM AccentColor is 0xAABBGGRR). Dark mode uses a lighter tint, as Windows does.
fn accent_color(dark: bool) -> slint::Color {
    let raw = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\DWM")
        .and_then(|k| k.get_value::<u32, _>("AccentColor"))
        .ok();
    let Some(v) = raw else { return if dark { slint::Color::from_rgb_u8(0x60, 0xCD, 0xFF) } else { slint::Color::from_rgb_u8(0x00, 0x5F, 0xB8) } };
    let (r, g, b) = ((v & 0xFF) as f32, ((v >> 8) & 0xFF) as f32, ((v >> 16) & 0xFF) as f32);
    let mix = |c: f32| if dark { c + (255.0 - c) * 0.45 } else { c };
    slint::Color::from_rgb_u8(mix(r) as u8, mix(g) as u8, mix(b) as u8)
}

/// A simulated transfer for previewing the transfer view: speed wanders like a real link, pause/cancel work.
fn start_demo(host: &UiHost) {
    let route = Route {
        from: "This PC".into(),
        to: "GAURESHLEGION".into(),
        from_kind: "pc".into(),
        to_kind: "ethernet".into(),
        game: "Hollow Knight: Silksong".into(),
        art_id: "1030300".into(),
    };
    let files = ["Hollow Knight Silksong_Data/data.unity3d", "Hollow Knight Silksong_Data/StreamingAssets/aa/win64/scenes_all.bundle", "Hollow Knight Silksong_Data/sharedassets12.assets", "Hollow Knight Silksong_Data/resources.assets", "UnityPlayer.dll"];
    spawn_job(host, "Sending Hollow Knight: Silksong to GAURESHLEGION".into(), route, move |_, p| {
        let total: u64 = 8_200_000_000;
        p.total.store(total, Ordering::Relaxed);
        let start = Instant::now();
        let mut seed = 0x2545F4914F6CDD1Du64;
        while p.done() < total {
            p.checkpoint()?;
            let t = start.elapsed().as_secs_f64();
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let noise = (seed % 1000) as f64 / 1000.0 - 0.5;
            let rate = 260e6 + 110e6 * (t / 2.7).sin() + 60e6 * noise; // bytes per second
            p.add((rate.max(20e6) * 0.05) as u64);
            p.set_current(files[(t / 3.0) as usize % files.len()]);
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok("Demo transfer finished.".into())
    });
}
/// "Delete from drive": asks first, then removes that game copy in the background and refreshes the drive list.
fn delete_package(ui: &MainWindow, st: &mut UiState, folder: PathBuf) {
    if st.busy() {
        return st.toast(ui, "Wait for the current transfer to finish first.", "error");
    }
    let offer = match transfer::read_package(&folder) {
        Ok(o) => o,
        Err(e) => return st.toast(ui, &format!("Couldn't read that copy: {e}"), "error"),
    };
    let drive = util::drive_of(&folder).to_string_lossy().trim_end_matches('\\').to_string();
    let question = format!(
        "Delete {} from drive {drive}?\n\nThis frees {} on the drive. The game installed on your PCs is not affected.",
        offer.name,
        util::size(offer.total_bytes)
    );
    if !installer::ask_yes_no(&question) {
        return;
    }
    st.toast(ui, &format!("Deleting {} from {drive}…", offer.name), "info");
    let name = offer.name.clone();
    std::thread::spawn(move || {
        let result = transfer::delete_package(&folder);
        let drives = hardware::drives();
        on_ui(move |ui, st| {
            st.browsed.retain(|p| p.folder != folder);
            st.sys.drives = drives;
            st.render_drives(ui);
            st.render_packages(ui);
            match result {
                Ok(freed) => {
                    st.log(ui, format!("Deleted {name} from {drive} ({} freed).", util::size(freed)));
                    st.toast(ui, &format!("Deleted {name} — {} freed on {drive}.", util::size(freed)), "ok");
                }
                Err(e) => st.toast(ui, &format!("Couldn't delete {name}: {e}"), "error"),
            }
        });
    });
}
/// If the setting is on, make sure a working copy of the app is in `folder` (a drive's SteamTransfer folder).
fn keep_app_copy(host: &UiHost, folder: &Path) {
    if !host.0.settings.lock().unwrap().keep_app_on_drives {
        return;
    }
    let where_ = folder.display();
    match installer::ensure_app_copy(folder) {
        Ok(installer::AppCopy::Copied) => host.log(format!("Put a copy of this app in {where_} so the next person can run it from the drive.")),
        Ok(installer::AppCopy::Updated) => host.log(format!("Replaced the app copy in {where_} with a working, up-to-date one.")),
        Ok(installer::AppCopy::Present) | Ok(installer::AppCopy::NewerThere) => {}
        Err(e) => host.log(format!("Couldn't put a copy of this app in {where_}: {e}")),
    }
}

fn open_settings(st: &mut UiState) {
    if st.settings_dialog.is_none() {
        let Ok(d) = SettingsDialog::new() else { return };
        let shared = st.shared.clone();
        let weak = d.as_weak();
        d.on_changed(move || {
            let Some(d) = weak.upgrade() else { return };
            let mut s = shared.settings.lock().unwrap();
            s.keep_app_on_drives = d.get_keep_app();
            let _ = crate::settings::save(&s);
        });
        let weak = d.as_weak();
        d.on_dismiss(move || {
            if let Some(d) = weak.upgrade() {
                let _ = d.hide();
            }
        });
        st.settings_dialog = Some(d);
    }
    if let Some(d) = &st.settings_dialog {
        d.set_keep_app(st.shared.settings.lock().unwrap().keep_app_on_drives);
        let _ = d.show();
    }
}