//! The single egui window: mode display, launcher buttons, usage cards and tables, key settings.

mod banners;
mod chart;
mod cockpit;
mod details;
mod drawer;
pub mod theme;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use eframe::egui::{self, RichText};

use crate::analytics::{DEFAULT_API_INTERVAL, UreqHttp};
use crate::cards::{BalanceCard, api_card, balance_card};
use crate::format::{COST_NOTE, outcome_message, pause_text, unpriced_text};
use crate::launcher::{LaunchOutcome, Mode, System, WindowsSystem, launch, routing_env_names};
use crate::paths::{Env, real_env, resolve_logs_root};
use crate::paths_ui::PathsPanel;
use crate::pricing::{PriceTable, format_usd};
use crate::report::Bucket;
use crate::settings::Settings;
use crate::secrets::{KeyringStore, SecretError, SecretStore, Slot, validate_key};
use crate::tray::{CloseAction, Tray, WindowControl, icon_state, tooltip};
use crate::view::{
    DetailKind, EscTarget, GrowAction, GrowState, differs_from_request, esc_target, grow_cap, growth_for,
    mode_card_with_routing, shrink_to, toggle_detail,
};
use banners::{BannerInput, banner_items, banner_strip};
use details::{DetailData, DetailView, animated_area, effective_tab};
use drawer::{Notes, Section};
use theme::palette::{BG, MUTED, PANEL};
use crate::worker::{DEFAULT_INTERVAL, Snapshot, WorkerConfig, spawn_worker};

/// How often the background thread asks whether Obsidian runs (each check spawns `tasklist`).
const MODE_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// After a successful start, "not running" polls are ignored for this long (the process may not show up yet).
const START_GRACE: Duration = Duration::from_secs(15);

pub use crate::view::ModeDisplay;

/// What to show as the current mode. `started_by_app` is the mode of the last start by this app.
pub fn mode_display(started_by_app: Option<Mode>, running: bool) -> ModeDisplay {
    if !running {
        return ModeDisplay::Closed;
    }
    match started_by_app {
        Some(Mode::Max) => ModeDisplay::Max,
        Some(Mode::ApiKey) => ModeDisplay::ApiKey,
        None => ModeDisplay::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub text: String,
}

/// Everything the window shows. The only key material is the typed text in `key_input` until it is saved
/// (or deleted); a stored key is never kept here.
pub struct AppState {
    pub snapshot: Option<Snapshot>,
    pub mode: ModeDisplay,
    started_by_app: Option<Mode>,
    running: bool,
    ignore_closed_until: Option<Instant>,
    /// A launch is in flight: mode and whether closing Obsidian was confirmed.
    pub launching: Option<(Mode, bool)>,
    /// Waiting for the user to confirm closing Obsidian before starting in this mode.
    pub confirm: Option<Mode>,
    pub notice: Option<Notice>,
    /// Content of the masked key field; cleared after a successful save.
    pub key_input: String,
    /// Whether an API key is stored (`None`: store not readable).
    pub key_stored: Option<bool>,
    /// Content of the masked admin-key field; same rules as `key_input`.
    pub admin_key_input: String,
    pub admin_key_stored: Option<bool>,
    /// Last settings read from the file (for the cards and the lists in the settings panel).
    pub settings: Settings,
    pub settings_path: Option<PathBuf>,
    /// Text fields of the settings panel (amounts as typed).
    pub baseline_input: String,
    pub topup_input: String,
    pub budget_input: String,
    pub low_balance_input: String,
    /// The user typed in the limit field since it was last filled from the file. Only edited fields are
    /// saved, so a stale field can never overwrite the limit in the file.
    pub budget_edited: bool,
    pub low_balance_edited: bool,
    /// Same for the start value field (filled from the file, committed on Enter or blur).
    pub baseline_edited: bool,
    /// The background service has ended (its channel is closed): the numbers are no longer updated.
    pub worker_gone: bool,
    /// The note about an unreadable saved state is shown once.
    state_note_shown: bool,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            snapshot: None,
            mode: ModeDisplay::Closed,
            started_by_app: None,
            running: false,
            ignore_closed_until: None,
            launching: None,
            confirm: None,
            notice: None,
            key_input: String::new(),
            key_stored: None,
            admin_key_input: String::new(),
            admin_key_stored: None,
            settings: Settings::default(),
            settings_path: None,
            baseline_input: String::new(),
            topup_input: String::new(),
            budget_input: String::new(),
            low_balance_input: String::new(),
            budget_edited: false,
            low_balance_edited: false,
            baseline_edited: false,
            worker_gone: false,
            state_note_shown: false,
        }
    }

    /// The launch thread ended without a result (it panicked): the buttons come back, with an error.
    pub fn apply_launch_lost(&mut self) {
        self.launching = None;
        self.notice = Some(Notice {
            kind: NoticeKind::Error,
            text: "Der Startvorgang wurde unerwartet beendet. Ob Obsidian läuft, zeigt die Anzeige oben.".to_string(),
        });
    }

    /// Shows the one-time note from loading the saved state, unless a notice is already up.
    pub fn show_state_note_once(&mut self, note: Option<&str>) {
        if self.state_note_shown {
            return;
        }
        if let Some(text) = note {
            self.state_note_shown = true;
            self.notice = Some(Notice { kind: NoticeKind::Info, text: text.to_string() });
        }
    }

    /// Re-reads `settings.json` for display. A broken or missing file keeps the last good copy; the
    /// problem itself comes from the worker snapshot (`settings_error`).
    pub fn reload_settings(&mut self) {
        if let Some(path) = &self.settings_path {
            let (settings, error) = crate::settings::load(path);
            if error.is_none() {
                self.settings = settings;
                // Fields the user has not touched follow the file (broken at start, edited externally).
                if !self.baseline_edited {
                    self.baseline_input = amount_input(self.settings.baseline.as_ref().map(|b| b.amount_nano));
                }
                if !self.budget_edited {
                    self.budget_input = amount_input(self.settings.monthly_budget_nano);
                }
                if !self.low_balance_edited {
                    self.low_balance_input = amount_input(self.settings.low_balance_nano);
                }
            }
        }
    }

    fn recompute_mode(&mut self) {
        self.mode = mode_display(self.started_by_app, self.running);
    }

    /// Latest answer of the Obsidian process check.
    pub fn apply_running(&mut self, running: bool, now: Instant) {
        if running {
            self.running = true;
        } else {
            if self.ignore_closed_until.is_some_and(|until| now < until) {
                return;
            }
            self.ignore_closed_until = None;
            if self.running {
                // Obsidian was closed: whoever starts it next, we no longer know the mode.
                self.started_by_app = None;
            }
            self.running = false;
        }
        self.recompute_mode();
    }

    fn apply_started(&mut self, mode: Mode, now: Instant) {
        self.started_by_app = Some(mode);
        self.running = true;
        self.ignore_closed_until = Some(now + START_GRACE);
        self.recompute_mode();
    }

    /// Result of a finished launch attempt.
    pub fn apply_outcome(&mut self, mode: Mode, outcome: LaunchOutcome, now: Instant) {
        self.launching = None;
        match outcome {
            LaunchOutcome::NeedsConfirmation => self.confirm = Some(mode),
            LaunchOutcome::Started => {
                self.apply_started(mode, now);
                self.notice = outcome_message(mode, outcome).map(|text| Notice { kind: NoticeKind::Info, text });
            }
            _ => {
                self.notice = outcome_message(mode, outcome).map(|text| Notice { kind: NoticeKind::Error, text });
            }
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate and store a key. Errors are fixed messages, never the value.
pub fn save_key(store: &dyn SecretStore, slot: Slot, input: &str) -> Result<(), String> {
    let value = input.trim();
    validate_key(slot, value).map_err(str::to_string)?;
    store.set(slot, value).map_err(|e| e.to_string())
}

pub fn save_api_key(store: &dyn SecretStore, input: &str) -> Result<(), String> {
    save_key(store, Slot::ApiKey, input)
}

/// Presence check only; the value is dropped immediately.
fn key_stored(store: &dyn SecretStore, slot: Slot) -> Option<bool> {
    store.get(slot).ok().map(|v| v.is_some())
}

/// Fixed id of a key field, so its widget state can be found again.
fn key_field_id(slot: Slot) -> egui::Id {
    match slot {
        Slot::ApiKey => egui::Id::unique("api-key-field"),
        Slot::AdminKey => egui::Id::unique("admin-key-field"),
    }
}

/// Empties a key field *and* drops its egui widget state. egui keeps the undo history of a text edit
/// (also of password fields) in the context for the process lifetime, so clearing only the string would
/// let Ctrl+Z bring the key back.
fn wipe_key_field(ctx: &egui::Context, id: egui::Id, input: &mut String) {
    input.clear();
    ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(id));
}

fn key_label(slot: Slot) -> &'static str {
    match slot {
        Slot::ApiKey => "API-Key",
        Slot::AdminKey => "Admin-Key",
    }
}

/// An optional limit as it is typed into its field (`65.03`, empty for none).
fn amount_input(nano: Option<u64>) -> String {
    nano.map(|n| format_usd(n).trim_start_matches('$').to_string()).unwrap_or_default()
}

/// Status line of a key slot. An unreadable store must not look like "no key".
pub fn key_status_text(stored: Option<bool>) -> &'static str {
    match stored {
        Some(true) => "gespeichert",
        Some(false) => "nicht gespeichert",
        None => "Speicher nicht lesbar",
    }
}

/// Cost cell of a table row. A row where no request had a price shows only the note, not "$0.00".
pub fn cost_cell_text(b: &Bucket) -> String {
    let usd = format_usd(b.cost_nano);
    match unpriced_text(b.unpriced_requests) {
        Some(note) if b.cost_nano == 0 && b.unpriced_requests >= b.requests => note,
        Some(note) => format!("{usd} · {note}"),
        None => usd,
    }
}

/// The log folder for the worker. A settings file that exists but cannot be read is "unknown", never
/// the automatic folder: scanning the wrong folder would wipe the history of a custom one.
pub fn logs_target(settings_path: Option<&Path>, env: Env) -> (PathBuf, Option<String>) {
    let mut configured = None;
    if let Some(path) = settings_path {
        let (settings, error) = crate::settings::load(path);
        if error.is_some() {
            return (
                PathBuf::new(),
                Some(format!(
                    "Die Einstellungsdatei ist nicht lesbar, deshalb werden die Protokolle nicht gelesen. Bitte die Datei prüfen: {}",
                    path.display()
                )),
            );
        }
        configured = settings.logs_root;
    }
    // Resolved once: a changed setting applies after a restart. Never a relative path.
    match resolve_logs_root(configured.as_deref(), env) {
        Ok(p) => (p, None),
        Err(e) => (PathBuf::new(), Some(e.to_string())),
    }
}

enum LaunchMsg {
    Done(Mode, LaunchOutcome),
    StoreError(SecretError),
}

/// Runs `launch` on its own thread (it may wait ~20 s for Obsidian to close). The API key is read here,
/// handed to `launch` and dropped right after.
fn spawn_launch(mode: Mode, confirmed_close: bool, ctx: egui::Context) -> Receiver<LaunchMsg> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let msg = {
            let key = match mode {
                Mode::Max => Ok(None),
                Mode::ApiKey => KeyringStore::new().get(Slot::ApiKey),
            };
            match key {
                Ok(key) => {
                    let sys = WindowsSystem { settings_path: crate::settings::default_path().ok() };
                    LaunchMsg::Done(mode, launch(&sys, mode, key.as_deref(), confirmed_close))
                }
                Err(e) => LaunchMsg::StoreError(e),
            }
        };
        let _ = tx.send(msg);
        ctx.request_repaint();
    });
    rx
}

/// The poller's answer after one check: a failed check keeps the previous answer.
fn poll_answer(check: std::io::Result<bool>, previous: Option<bool>) -> Option<bool> {
    match check {
        Ok(running) => Some(running),
        Err(_) => previous,
    }
}

/// Polls `obsidian_running()` in the background; the latest answer is in the returned flag.
fn spawn_mode_poller(ctx: egui::Context) -> Arc<AtomicBool> {
    let running = Arc::new(AtomicBool::new(false));
    let shared = running.clone();
    std::thread::spawn(move || {
        let sys = WindowsSystem { settings_path: crate::settings::default_path().ok() };
        let mut last: Option<bool> = None;
        loop {
            // The image name follows the Obsidian path from the settings.
            // Unknown image name (unreadable settings): keep the last answer.
            if let Some(image) = sys.current_image_name() {
                // A failed check keeps the last answer instead of showing "geschlossen".
                let answer = poll_answer(sys.obsidian_running(&image), last);
                if let Some(now_running) = answer {
                    shared.store(now_running, Ordering::Relaxed);
                }
                if last != answer {
                    last = answer;
                    ctx.request_repaint();
                }
            }
            std::thread::sleep(MODE_POLL_INTERVAL);
        }
    });
    running
}

pub struct CerberusApp {
    state: AppState,
    snapshots: Receiver<Snapshot>,
    obsidian_running: Arc<AtomicBool>,
    launch_rx: Option<Receiver<LaunchMsg>>,
    paths: PathsPanel,
    control: Arc<WindowControl>,
    store: KeyringStore,
    prices_as_of: String,
    tray: Option<Tray>,
    /// Names (never values) of routing variables that are set in this process's environment.
    routing_names: Vec<&'static str>,
    /// Top-bar logo, decoded once at startup.
    logo: egui::TextureHandle,
    /// The open figure-card detail; only one at a time.
    detail: Option<DetailKind>,
    /// The detail that is drawn: `detail`, or the one that is still folding up.
    detail_shown: Option<DetailKind>,
    detail_view: DetailView,
    /// Openness (0..=1) of the detail area this frame.
    detail_openness: f32,
    /// The settings drawer is open (the slide itself is animated in `draw_drawer`).
    drawer_open: bool,
    /// Section the drawer should scroll to once drawn.
    drawer_scroll: Option<Section>,
    /// Result messages shown directly under the settings field they belong to.
    notes: Notes,
    /// Height the app added to the window for an open detail.
    grown_by: f32,
    /// The window size differs from what the app last asked for: the user resized it.
    user_resized: bool,
    /// Inner size the app last asked for (or saw first).
    last_requested: Option<egui::Vec2>,
    /// The window size before the last request, to tell "not arrived yet" from "arrived differently".
    size_before_request: Option<egui::Vec2>,
    requested_at: Option<Instant>,
    /// When to grow or give back height.
    grow: GrowState,
}

/// A request younger than this may not have reached the window yet.
const RESIZE_SETTLE: Duration = Duration::from_millis(500);

impl CerberusApp {
    fn new(cc: &eframe::CreationContext<'_>, icon: &egui::IconData) -> Self {
        let ctx = cc.egui_ctx.clone();
        theme::apply(&ctx);
        let control = WindowControl::new();
        let tray = match Tray::new(&ctx, &control, icon) {
            Ok(t) => Some(t),
            Err(_) => None, // without a tray icon the X button closes the app
        };

        let data_dir = data_dir();
        let _ = std::fs::create_dir_all(&data_dir);
        let prices_path = PriceTable::default_override_path().unwrap_or_else(|| data_dir.join("prices.json"));
        let prices_as_of = PriceTable::load(&prices_path).as_of().to_string();

        let settings_path = crate::settings::default_path().ok();
        let (logs_root, logs_unavailable) = logs_target(settings_path.as_deref(), &real_env);

        let notify_ctx = ctx.clone();
        let snapshots = spawn_worker(
            WorkerConfig {
                logs_root,
                logs_unavailable,
                state_path: data_dir.join("scan_state.json"),
                prices_path,
                interval: DEFAULT_INTERVAL,
                secrets: Arc::new(KeyringStore::new()),
                http: Arc::new(UreqHttp::new()),
                api_interval: DEFAULT_API_INTERVAL,
                settings_path: crate::settings::default_path().ok(),
            },
            Box::new(move || notify_ctx.request_repaint()),
        );
        let obsidian_running = spawn_mode_poller(ctx);

        let store = KeyringStore::new();
        let mut state = AppState::new();
        state.key_stored = key_stored(&store, Slot::ApiKey);
        state.admin_key_stored = key_stored(&store, Slot::AdminKey);
        state.settings_path = crate::settings::default_path().ok();
        state.reload_settings();
        let paths = PathsPanel::new(&state.settings);
        let logo = cc.egui_ctx.load_texture(
            "logo",
            egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba),
            egui::TextureOptions::LINEAR.with_mipmap_mode(Some(egui::TextureFilter::Linear)),
        );

        Self {
            state,
            snapshots,
            obsidian_running,
            launch_rx: None,
            paths,
            control,
            store,
            prices_as_of,
            tray,
            routing_names: routing_env_names(&|n| real_env(n).map(|v| v.to_string_lossy().into_owned())),
            logo,
            detail: None,
            detail_shown: None,
            detail_view: DetailView::default(),
            detail_openness: 0.0,
            drawer_open: false,
            drawer_scroll: None,
            notes: Notes::new(),
            grown_by: 0.0,
            user_resized: false,
            last_requested: None,
            size_before_request: None,
            requested_at: None,
            grow: GrowState::default(),
        }
    }

    /// Pulls in everything that happened off the UI thread. Called from `logic`, which eframe also runs
    /// while the window is hidden.
    fn poll(&mut self, ctx: &egui::Context) {
        // The channel is unbounded: take everything, keep the newest.
        let mut fresh = false;
        loop {
            match self.snapshots.try_recv() {
                Ok(snapshot) => {
                    self.state.snapshot = Some(snapshot);
                    fresh = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.state.worker_gone = true;
                    break;
                }
            }
        }
        if fresh {
            let note = self.state.snapshot.as_ref().and_then(|s| s.state_note.clone());
            self.state.show_state_note_once(note.as_deref());
            self.state.reload_settings();
            self.paths.refresh(&self.state.settings);
        }
        self.paths.poll_dialog();
        let now = Instant::now();
        self.state.apply_running(self.obsidian_running.load(Ordering::Relaxed), now);
        if let (Some(tray), Some(snapshot)) = (self.tray.as_mut(), &self.state.snapshot) {
            tray.update(tooltip(snapshot, self.state.mode, !self.routing_names.is_empty()), icon_state(&snapshot.alerts));
        }

        if let Some(rx) = &self.launch_rx {
            let received = match rx.try_recv() {
                Ok(msg) => Some(Some(msg)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(None),
            };
            if let Some(msg) = received {
                self.launch_rx = None;
                // A waiting "Beenden" goes through now.
                self.control.set_launch_busy(ctx, false);
                let Some(msg) = msg else {
                    self.state.apply_launch_lost();
                    return;
                };
                match msg {
                    LaunchMsg::Done(mode, outcome) => {
                        let started = outcome == LaunchOutcome::Started;
                        self.state.apply_outcome(mode, outcome, now);
                        if started {
                            // Re-check once the grace period is over (the poller only reports changes).
                            ctx.request_repaint_after(START_GRACE + Duration::from_secs(1));
                        }
                    }
                    LaunchMsg::StoreError(e) => {
                        self.state.launching = None;
                        self.state.notice = Some(Notice { kind: NoticeKind::Error, text: e.to_string() });
                    }
                }
            }
        }
    }

    fn start_launch(&mut self, ctx: &egui::Context, mode: Mode, confirmed_close: bool) {
        if self.launch_rx.is_some() {
            return;
        }
        self.state.notice = None;
        self.state.launching = Some((mode, confirmed_close));
        self.control.set_launch_busy(ctx, true);
        self.launch_rx = Some(spawn_launch(mode, confirmed_close, ctx.clone()));
    }

    fn input_mut(&mut self, slot: Slot) -> &mut String {
        match slot {
            Slot::ApiKey => &mut self.state.key_input,
            Slot::AdminKey => &mut self.state.admin_key_input,
        }
    }

    fn set_stored(&mut self, slot: Slot, stored: bool) {
        match slot {
            Slot::ApiKey => self.state.key_stored = Some(stored),
            Slot::AdminKey => self.state.admin_key_stored = Some(stored),
        }
    }
}


impl eframe::App for CerberusApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        // Close handling lives here, not in `ui`: eframe skips `ui` while the window is minimized or
        // hidden, and a close request then must still hide the window instead of ending the app.
        self.control.set_minimized(ctx.input(|i| i.viewport().minimized.unwrap_or(false)));
        if ctx.input(|i| i.viewport().close_requested()) {
            match self.control.close_action() {
                CloseAction::Exit => {}
                CloseAction::Hide => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    self.control.hide(ctx);
                }
                CloseAction::WaitForLaunch => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    self.control.quit_when_idle(ctx);
                }
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.track_window_size(&ctx);
        self.handle_escape(&ctx);

        let items = self.banners();
        egui::Panel::top("top-bar")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(egui::Margin::symmetric(12, 8)))
            .show(ui, |ui| cockpit::top_bar(ui, &self.logo, &mut self.drawer_open));
        if !items.is_empty() {
            egui::Panel::top("banner-strip")
                .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::symmetric(12, 8)))
                .show_separator_line(false)
                .show(ui, |ui| banner_strip(ui, &items));
        }
        // Below the top bar and the banner strip: where the backdrop and the drawer go.
        let body = ui.available_rect_before_wrap();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::same(12))).show(
            ui,
            |ui| {
                let out = egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.draw(ui, &ctx);
                });
                self.fit_window(&ctx, out.content_size.y, out.inner_rect.height());
            },
        );
        self.draw_drawer(&ctx, body);
        self.draw_confirm(&ctx);
    }
}

// ---- drawing -------------------------------------------------------------------------------------

impl CerberusApp {
    /// Every v1 banner, in severity colours.
    fn banners(&self) -> Vec<(crate::view::Tone, String)> {
        let snapshot = self.state.snapshot.as_ref();
        let pause_text = snapshot
            .and_then(|s| s.report.as_ref())
            .and_then(|r| r.pause.as_ref())
            .map(|pause| pause_text(pause.until.with_timezone(&Local), Local::now(), &pause.limit_type));
        let alerts: &[crate::balance::Alert] = snapshot.map_or(&[], |s| s.alerts.as_slice());
        banner_items(&BannerInput {
            quit_waiting: self.control.quit_waiting(),
            routing_names: &self.routing_names,
            worker_gone: self.state.worker_gone,
            pause_text,
            scan_error: snapshot.and_then(|s| s.scan_error.as_deref()),
            settings_error: snapshot.and_then(|s| s.settings_error.as_deref()),
            settings_path: self.state.settings_path.as_deref(),
            alerts,
            notice: self.state.notice.as_ref(),
        })
    }

    /// Opens, switches or closes the detail area and queues the matching window change.
    fn set_detail(&mut self, new: Option<DetailKind>) {
        if new == self.detail {
            return;
        }
        self.detail = new;
        self.detail_view.rows_expanded = false;
        match new {
            Some(kind) => {
                self.detail_view.tab = effective_tab(kind, self.detail_view.tab);
                self.grow.content_grew();
            }
            None => self.grow.content_closing(),
        }
    }

    /// Esc closes the topmost layer: confirm dialog, drawer, detail. A focused text field keeps its Esc.
    fn handle_escape(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.key_pressed(egui::Key::Escape)) || ctx.egui_wants_keyboard_input() {
            return;
        }
        match esc_target(self.state.confirm.is_some(), self.drawer_open, self.detail.is_some()) {
            EscTarget::Confirm => self.state.confirm = None,
            EscTarget::Drawer => self.drawer_open = false,
            EscTarget::Detail => self.set_detail(None),
            EscTarget::Nothing => {}
        }
    }

    /// Compares the window with what the app asked for. A size that differs long after a request is a
    /// user resize: the app re-baselines on it (nothing to give back, grows are relative to the user's
    /// size from then on). A size that arrives differently right after a request is an OS clamp or DPI
    /// rounding: adopted without counting as a user resize.
    fn track_window_size(&mut self, ctx: &egui::Context) {
        self.user_resized = false;
        let Some(rect) = ctx.input(|i| i.viewport().inner_rect) else { return };
        let actual = rect.size();
        let Some(requested) = self.last_requested else {
            self.last_requested = Some(actual);
            return;
        };
        if !differs_from_request((actual.x, actual.y), (requested.x, requested.y)) {
            return;
        }
        let young = self.requested_at.is_some_and(|at| at.elapsed() < RESIZE_SETTLE);
        if young {
            let waiting = self
                .size_before_request
                .is_some_and(|before| !differs_from_request((actual.x, actual.y), (before.x, before.y)));
            if !waiting {
                // Arrived, but smaller or larger than asked: the OS clamped it.
                let lost = (requested.y - actual.y).max(0.0);
                self.grown_by = (self.grown_by - lost).max(0.0);
                self.last_requested = Some(actual);
            }
            return;
        }
        self.user_resized = true;
        self.last_requested = Some(actual);
        self.grown_by = 0.0;
    }

    fn request_height(&mut self, ctx: &egui::Context, current: egui::Vec2, height: f32) {
        let size = egui::vec2(current.x, height);
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        self.size_before_request = Some(current);
        self.last_requested = Some(size);
        self.requested_at = Some(Instant::now());
    }

    /// After the content settled (a frame after a change, the animation finished) the window grows to fit
    /// it, never past the work area and never when the user is resizing; after the detail has fully
    /// closed the window gives back what it added.
    fn fit_window(&mut self, ctx: &egui::Context, content_height: f32, scroll_height: f32) {
        let action = self.grow.on_frame(self.detail_openness);
        let still = self.detail_openness >= 1.0 || self.detail_openness <= 0.0;
        if self.grow.pending() && still {
            // Not animating, so egui would not draw the frame that does the measuring.
            ctx.request_repaint();
        }
        let Some(rect) = ctx.input(|i| i.viewport().inner_rect) else { return };
        let (size, current) = (rect.size(), rect.height());
        match action {
            GrowAction::None => {}
            GrowAction::Grow => {
                let needed = current - scroll_height + content_height;
                let (monitor, top, chrome) = ctx.input(|i| {
                    let v = i.viewport();
                    let chrome = v.outer_rect.map(|o| (o.height() - current).max(0.0));
                    (v.monitor_size.map(|m| m.y), v.outer_rect.map(|o| o.min.y), chrome)
                });
                let cap = grow_cap(monitor, chrome.unwrap_or(0.0), top);
                if let Some(growth) = growth_for(current, needed, cap, self.user_resized) {
                    self.grown_by += growth.added;
                    self.request_height(ctx, size, growth.target_height);
                }
            }
            GrowAction::Shrink => {
                if let Some(height) = shrink_to(current, self.grown_by, self.user_resized) {
                    self.request_height(ctx, size, height);
                }
                self.grown_by = 0.0;
            }
        }
    }

    fn draw(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.spacing_mut().item_spacing.y = 10.0;
        if let Some(mode) = cockpit::mode_card_ui(
            ui,
            mode_card_with_routing(self.state.mode, !self.routing_names.is_empty()),
            self.state.launching,
        ) {
            self.start_launch(ctx, mode, false);
        }

        let open = self.detail.is_some();
        let t = ctx.animate_bool_with_time(egui::Id::unique("detail-open"), open, 0.18);
        self.detail_openness = t;
        if open {
            self.detail_shown = self.detail;
        } else if t <= 0.0 {
            self.detail_shown = None;
        }

        let mut clicked = None;
        let mut area_out = None;
        match self.state.snapshot.as_ref() {
            None => {
                ui.label("Lese Protokolle …");
            }
            Some(snapshot) => match &snapshot.report {
                None => cockpit::scan_progress(ui, snapshot.scanned_files, snapshot.total_files),
                Some(report) => {
                    let balance = snapshot.balance.as_ref();
                    let balance_view = balance_card(balance, &self.state.settings);
                    let balance_error = balance.filter(|b| b.cost_deducted).and_then(|b| b.error.as_ref());
                    let issue = balance_error.is_some()
                        || matches!(&balance_view, BalanceCard::Estimate { note: Some(_), .. });
                    let figures = cockpit::figures(report, &balance_view, issue);
                    clicked = cockpit::figure_cards(ui, &figures, self.detail);
                    if let Some(kind) = self.detail_shown {
                        let data = DetailData { report, balance: &balance_view, balance_error };
                        let view = &mut self.detail_view;
                        area_out = animated_area(ui, egui::Id::unique("detail-area"), t, |ui| {
                            details::show(ui, kind, &data, view)
                        });
                    }
                }
            },
        }
        if let Some(kind) = clicked {
            self.set_detail(toggle_detail(self.detail, kind));
        }
        if let Some(out) = area_out {
            if out.open_drawer {
                self.open_drawer(ctx, Some(Section::Balance));
            }
            if out.layout_changed && self.detail.is_some() {
                self.grow.content_grew();
            }
        }

        let api = self.state.snapshot.as_ref().and_then(|s| s.api.as_ref());
        cockpit::api_line(ui, &api_card(self.state.admin_key_stored, api));
        if let Some(snapshot) = self.state.snapshot.as_ref().filter(|s| s.report.is_some()) {
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "Stand: {} · Preisliste vom {} · Kosten sind {COST_NOTE}",
                        format_time(snapshot.updated),
                        self.prices_as_of
                    ))
                    .small()
                    .color(MUTED),
                )
                .wrap(),
            );
        }
        // The update hint (task 8) goes here.
    }

    fn draw_confirm(&mut self, ctx: &egui::Context) {
        let Some(mode) = self.state.confirm else { return };
        let mut answer: Option<bool> = None;
        egui::Modal::new(egui::Id::unique("confirm-close")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("Obsidian schließen?");
            ui.label("Obsidian läuft und muss geschlossen werden. Laufende Claudian-Antworten brechen ab. Jetzt schließen?");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Ja, schließen").clicked() {
                    answer = Some(true);
                }
                if ui.button("Abbrechen").clicked() {
                    answer = Some(false);
                }
            });
        });
        match answer {
            Some(true) => {
                self.state.confirm = None;
                self.start_launch(ctx, mode, true);
            }
            Some(false) => self.state.confirm = None,
            None => {}
        }
    }
}

fn format_time(t: DateTime<Local>) -> String {
    t.format("%d.%m.%Y %H:%M:%S").to_string()
}

// ---- startup -------------------------------------------------------------------------------------

fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("CerberusUsage")
}

/// Opens the window and the tray icon and runs until "Beenden".
pub fn run() -> eframe::Result {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon.png"))
        .expect("embedded icon is a valid PNG");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Cerberus Usage")
            .with_inner_size([480.0, 640.0])
            .with_min_inner_size([420.0, 480.0])
            .with_icon(icon.clone()),
        ..Default::default()
    };
    eframe::run_native(
        "Cerberus Usage",
        options,
        Box::new(move |cc| Ok(Box::new(CerberusApp::new(cc, &icon)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cards::{update_settings, with_limits};
    use std::cell::RefCell;

    #[test]
    fn mode_unknown_if_running_but_not_started_by_app() {
        assert_eq!(mode_display(None, true), ModeDisplay::Unknown);
    }

    #[test]
    fn mode_closed_when_not_running() {
        assert_eq!(mode_display(None, false), ModeDisplay::Closed);
        assert_eq!(mode_display(Some(Mode::Max), false), ModeDisplay::Closed);
    }

    #[test]
    fn mode_matches_when_started_by_app() {
        assert_eq!(mode_display(Some(Mode::Max), true), ModeDisplay::Max);
        assert_eq!(mode_display(Some(Mode::ApiKey), true), ModeDisplay::ApiKey);
    }

    #[test]
    fn started_mode_is_shown_and_survives_a_missed_process_check() {
        let t0 = Instant::now();
        let mut s = AppState::new();
        s.apply_outcome(Mode::ApiKey, LaunchOutcome::Started, t0);
        assert_eq!(s.mode, ModeDisplay::ApiKey);
        // Obsidian not yet in the process list right after the start: ignored.
        s.apply_running(false, t0 + Duration::from_secs(2));
        assert_eq!(s.mode, ModeDisplay::ApiKey);
        // Later a real close is honoured and the mode is forgotten.
        s.apply_running(false, t0 + START_GRACE + Duration::from_secs(1));
        assert_eq!(s.mode, ModeDisplay::Closed);
        s.apply_running(true, t0 + START_GRACE + Duration::from_secs(10));
        assert_eq!(s.mode, ModeDisplay::Unknown);
    }

    #[test]
    fn running_obsidian_without_app_start_is_unknown() {
        let mut s = AppState::new();
        s.apply_running(true, Instant::now());
        assert_eq!(s.mode, ModeDisplay::Unknown);
    }

    #[test]
    fn needs_confirmation_opens_dialog_and_failures_set_error_notice() {
        let now = Instant::now();
        let mut s = AppState::new();
        s.launching = Some((Mode::Max, false));
        s.apply_outcome(Mode::Max, LaunchOutcome::NeedsConfirmation, now);
        assert_eq!(s.confirm, Some(Mode::Max));
        assert!(s.launching.is_none() && s.notice.is_none());

        s.apply_outcome(Mode::ApiKey, LaunchOutcome::MissingKey, now);
        assert_eq!(s.notice.as_ref().unwrap().kind, NoticeKind::Error);
        assert_eq!(s.mode, ModeDisplay::Closed);
    }

    #[test]
    fn failed_process_check_keeps_the_last_answer() {
        assert_eq!(poll_answer(Ok(true), None), Some(true));
        assert_eq!(poll_answer(Ok(false), Some(true)), Some(false));
        let err = || Err(std::io::Error::other("blocked"));
        assert_eq!(poll_answer(err(), Some(true)), Some(true), "no forgetting the mode on one failed check");
        assert_eq!(poll_answer(err(), Some(false)), Some(false));
        assert_eq!(poll_answer(err(), None), None);
    }

    #[test]
    fn lost_launch_thread_releases_the_buttons_with_an_error() {
        let mut s = AppState::new();
        s.launching = Some((Mode::Max, true));
        s.apply_launch_lost();
        assert!(s.launching.is_none());
        let n = s.notice.unwrap();
        assert_eq!(n.kind, NoticeKind::Error);
        assert!(n.text.contains("unerwartet"));
    }

    #[test]
    fn state_note_is_shown_exactly_once() {
        let mut s = AppState::new();
        s.show_state_note_once(None);
        assert!(s.notice.is_none());
        s.show_state_note_once(Some("alte Datei zur Seite gelegt"));
        assert_eq!(s.notice.as_ref().unwrap().text, "alte Datei zur Seite gelegt");
        s.notice = None;
        s.show_state_note_once(Some("alte Datei zur Seite gelegt"));
        assert!(s.notice.is_none(), "dismissed notes do not come back with every snapshot");
    }

    #[test]
    fn key_status_distinguishes_unreadable_from_missing() {
        assert_eq!(key_status_text(Some(true)), "gespeichert");
        assert_eq!(key_status_text(Some(false)), "nicht gespeichert");
        assert_eq!(key_status_text(None), "Speicher nicht lesbar");
    }

    #[test]
    fn fully_unpriced_rows_show_no_dollar_zero() {
        let row = |cost_nano, requests, unpriced_requests| Bucket { cost_nano, requests, unpriced_requests, ..Bucket::default() };
        assert_eq!(cost_cell_text(&row(0, 3, 3)), "3 Anfragen nicht bewertet");
        assert_eq!(cost_cell_text(&row(0, 1, 1)), "1 Anfrage nicht bewertet");
        assert_eq!(cost_cell_text(&row(1_230_000_000, 5, 2)), "$1.23 · 2 Anfragen nicht bewertet");
        assert_eq!(cost_cell_text(&row(1_230_000_000, 5, 0)), "$1.23");
        assert_eq!(cost_cell_text(&row(0, 5, 0)), "$0.00", "all priced but free stays a number");
    }

    #[test]
    fn unreadable_settings_never_fall_back_to_the_auto_log_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let env = |n: &str| (n == "USERPROFILE").then(|| std::ffi::OsString::from(r"C:\Users\someone"));

        // A broken file: nothing is scanned, the reason is shown (this protects a custom folder's history).
        std::fs::write(&path, "{ broken").unwrap();
        let (root, reason) = logs_target(Some(&path), &env);
        assert!(root.as_os_str().is_empty());
        assert!(reason.unwrap().contains("settings.json"));

        // A missing file is the normal first run: automatic folder.
        std::fs::remove_file(&path).unwrap();
        let (root, reason) = logs_target(Some(&path), &env);
        assert!(reason.is_none());
        assert!(root.ends_with(Path::new(".claude").join("projects")), "{root:?}");

        // A good file with a custom folder uses it.
        std::fs::write(&path, r#"{"logs_root": "D:\\Claude\\projects"}"#).unwrap();
        let (root, reason) = logs_target(Some(&path), &env);
        assert_eq!((root, reason), (PathBuf::from(r"D:\Claude\projects"), None));
    }

    struct FakeStore(RefCell<Option<String>>);

    impl SecretStore for FakeStore {
        fn get(&self, _s: Slot) -> Result<Option<String>, SecretError> {
            Ok(self.0.borrow().clone())
        }
        fn set(&self, _s: Slot, v: &str) -> Result<(), SecretError> {
            *self.0.borrow_mut() = Some(v.to_string());
            Ok(())
        }
        fn delete(&self, _s: Slot) -> Result<(), SecretError> {
            *self.0.borrow_mut() = None;
            Ok(())
        }
    }

    #[test]
    fn invalid_key_is_rejected_with_fixed_message_and_not_stored() {
        let store = FakeStore(RefCell::new(None));
        let err = save_api_key(&store, "not-a-key-xyz").unwrap_err();
        assert_eq!(err, "Ein API-Key beginnt mit sk-ant-.");
        assert!(!err.contains("not-a-key-xyz"));
        assert_eq!(key_stored(&store, Slot::ApiKey), Some(false));
    }

    #[test]
    fn valid_key_is_trimmed_and_stored() {
        let store = FakeStore(RefCell::new(None));
        save_api_key(&store, "  sk-ant-api03-fake-test-value\n").unwrap();
        assert_eq!(key_stored(&store, Slot::ApiKey), Some(true));
        assert_eq!(store.0.borrow().as_deref(), Some("sk-ant-api03-fake-test-value"));
    }

    /// Runs one frame with the key field; the text lives in `text`.
    fn key_field_frame(ctx: &egui::Context, id: egui::Id, text: &mut String, events: Vec<egui::Event>, focus: bool) {
        let input = egui::RawInput { events, ..Default::default() };
        let mut output = ctx.run_ui(input, |ui| {
            let r = ui.add(egui::TextEdit::singleline(text).password(true).id(id));
            if focus {
                r.request_focus();
            }
        });
        output.textures_delta.clear();
    }

    #[test]
    fn wiping_the_key_field_removes_its_undo_history() {
        for slot in [Slot::ApiKey, Slot::AdminKey] {
            let id = key_field_id(slot);
            let ctx = egui::Context::default();
            let mut text = String::new();
            key_field_frame(&ctx, id, &mut text, vec![], true);
            key_field_frame(&ctx, id, &mut text, vec![egui::Event::Text("sk-ant-fake-test-value".into())], true);
            key_field_frame(&ctx, id, &mut text, vec![], true);
            assert_eq!(text, "sk-ant-fake-test-value");
            assert!(
                egui::TextEdit::load_state(&ctx, id).is_some(),
                "egui should hold widget state (undo history) for the typed text"
            );

            wipe_key_field(&ctx, id, &mut text);

            assert!(text.is_empty());
            assert!(
                egui::TextEdit::load_state(&ctx, id).is_none(),
                "widget state must be gone so Ctrl+Z cannot restore the key"
            );
            // A following frame starts from scratch: still empty.
            key_field_frame(&ctx, id, &mut text, vec![], true);
            assert!(text.is_empty());
        }
    }

    #[test]
    fn key_fields_have_distinct_ids_and_admin_keys_are_validated_as_admin_keys() {
        assert_ne!(key_field_id(Slot::ApiKey), key_field_id(Slot::AdminKey));
        let store = FakeStore(RefCell::new(None));
        assert!(save_key(&store, Slot::AdminKey, "sk-ant-api03-fake").is_err());
        assert_eq!(key_stored(&store, Slot::AdminKey), Some(false));
        save_key(&store, Slot::AdminKey, "sk-ant-admin01-fake-test-value").unwrap();
        assert_eq!(key_stored(&store, Slot::AdminKey), Some(true));
    }

    #[test]
    fn limits_from_a_fixed_file_survive_a_save_that_did_not_touch_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ broken").unwrap();
        let mut st = AppState::new();
        st.settings_path = Some(path.clone());
        st.reload_settings();
        assert!(st.budget_input.is_empty() && !st.budget_edited);

        // The file gets repaired outside; untouched fields follow it.
        std::fs::write(&path, r#"{"monthly_budget_nano": 20000000000, "low_balance_nano": 5000000000}"#).unwrap();
        st.reload_settings();
        assert_eq!((st.budget_input.as_str(), st.low_balance_input.as_str()), ("20.00", "5.00"));

        // Saving only the budget field leaves the other limit as in the file.
        st.budget_input = "30".into();
        st.budget_edited = true;
        let saved = update_settings(&path, |s| {
            with_limits(s, st.budget_edited.then_some(st.budget_input.as_str()), st.low_balance_edited.then_some(st.low_balance_input.as_str()))
        })
        .unwrap();
        assert_eq!((saved.monthly_budget_nano, saved.low_balance_nano), (Some(30_000_000_000), Some(5_000_000_000)));

        // An edited field keeps the user text over a reload.
        std::fs::write(&path, r#"{"monthly_budget_nano": 1000000000}"#).unwrap();
        st.reload_settings();
        assert_eq!(st.budget_input, "30");
        assert_eq!(st.low_balance_input, "");
    }

    #[test]
    fn amount_field_text_round_trips_through_the_parser() {
        assert_eq!(amount_input(None), "");
        let typed = amount_input(Some(20_500_000_000));
        assert_eq!(typed, "20.50");
        assert_eq!(crate::cards::parse_usd_nano(&typed), Ok(20_500_000_000));
    }
}
