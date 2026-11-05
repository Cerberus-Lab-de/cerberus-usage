//! The single egui window: mode display, launcher buttons, usage cards and tables, key settings.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use eframe::egui::{self, Color32, RichText};

use crate::analytics::{DEFAULT_API_INTERVAL, UreqHttp};
use crate::format::{
    COST_NOTE, format_count, outcome_message, pause_text, progress_text, unpriced_text,
};
use crate::launcher::{LaunchOutcome, Mode, System, WindowsSystem, launch};
use crate::pricing::{PriceTable, format_usd};
use crate::report::Bucket;
use crate::secrets::{KeyringStore, SecretError, SecretStore, Slot, validate_key};
use crate::tray::{Tray, WindowControl};
use crate::worker::{DEFAULT_INTERVAL, Snapshot, WorkerConfig, spawn_worker};

/// How often the background thread asks whether Obsidian runs (each check spawns `tasklist`).
const MODE_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// After a successful start, "not running" polls are ignored for this long (the process may not show up yet).
const START_GRACE: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeDisplay {
    Max,
    ApiKey,
    /// Obsidian runs, but not (or no longer verifiably) started by this app.
    Unknown,
    Closed,
}

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

/// Validate and store an API key. Errors are fixed messages, never the value.
pub fn save_api_key(store: &dyn SecretStore, input: &str) -> Result<(), String> {
    let value = input.trim();
    validate_key(Slot::ApiKey, value).map_err(str::to_string)?;
    store.set(Slot::ApiKey, value).map_err(|e| e.to_string())
}

/// Presence check only; the value is dropped immediately.
fn api_key_stored(store: &dyn SecretStore) -> Option<bool> {
    store.get(Slot::ApiKey).ok().map(|v| v.is_some())
}

/// Fixed id of the key field, so its widget state can be found again.
fn key_field_id() -> egui::Id {
    egui::Id::unique("api-key-field")
}

/// Empties the key field *and* drops its egui widget state. egui keeps the undo history of a text edit
/// (also of password fields) in the context for the process lifetime, so clearing only the string would
/// let Ctrl+Z bring the key back.
fn wipe_key_field(ctx: &egui::Context, input: &mut String) {
    input.clear();
    ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(key_field_id()));
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
                Ok(key) => LaunchMsg::Done(mode, launch(&WindowsSystem, mode, key.as_deref(), confirmed_close)),
                Err(e) => LaunchMsg::StoreError(e),
            }
        };
        let _ = tx.send(msg);
        ctx.request_repaint();
    });
    rx
}

/// Polls `obsidian_running()` in the background; the latest answer is in the returned flag.
fn spawn_mode_poller(ctx: egui::Context) -> Arc<AtomicBool> {
    let running = Arc::new(AtomicBool::new(false));
    let shared = running.clone();
    std::thread::spawn(move || {
        let sys = WindowsSystem;
        let mut last: Option<bool> = None;
        loop {
            let now_running = sys.obsidian_running();
            shared.store(now_running, Ordering::Relaxed);
            if last != Some(now_running) {
                last = Some(now_running);
                ctx.request_repaint();
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
    control: Arc<WindowControl>,
    store: KeyringStore,
    prices_as_of: String,
    _tray: Option<Tray>,
}

impl CerberusApp {
    fn new(cc: &eframe::CreationContext<'_>, icon: &egui::IconData) -> Self {
        let ctx = cc.egui_ctx.clone();
        let control = WindowControl::new();
        let tray = match Tray::new(&ctx, &control, icon) {
            Ok(t) => Some(t),
            Err(_) => None, // without a tray icon the X button closes the app
        };

        let data_dir = data_dir();
        let _ = std::fs::create_dir_all(&data_dir);
        let prices_path = PriceTable::default_override_path().unwrap_or_else(|| data_dir.join("prices.json"));
        let prices_as_of = PriceTable::load(&prices_path).as_of().to_string();

        let notify_ctx = ctx.clone();
        let snapshots = spawn_worker(
            WorkerConfig {
                logs_root: logs_root(),
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
        state.key_stored = api_key_stored(&store);

        Self {
            state,
            snapshots,
            obsidian_running,
            launch_rx: None,
            control,
            store,
            prices_as_of,
            _tray: tray,
        }
    }

    /// Pulls in everything that happened off the UI thread. Called from `logic`, which eframe also runs
    /// while the window is hidden.
    fn poll(&mut self, ctx: &egui::Context) {
        // The channel is unbounded: take everything, keep the newest.
        while let Ok(snapshot) = self.snapshots.try_recv() {
            self.state.snapshot = Some(snapshot);
        }
        let now = Instant::now();
        self.state.apply_running(self.obsidian_running.load(Ordering::Relaxed), now);

        if let Some(rx) = &self.launch_rx {
            if let Ok(msg) = rx.try_recv() {
                self.launch_rx = None;
                match msg {
                    LaunchMsg::Done(mode, outcome) => {
                        self.state.apply_outcome(mode, outcome, now);
                        if outcome == LaunchOutcome::Started {
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
        self.launch_rx = Some(spawn_launch(mode, confirmed_close, ctx.clone()));
    }

    fn save_key(&mut self, ctx: &egui::Context) {
        match save_api_key(&self.store, &self.state.key_input) {
            Ok(()) => {
                wipe_key_field(ctx, &mut self.state.key_input);
                self.state.key_stored = Some(true);
                self.state.notice = Some(Notice { kind: NoticeKind::Info, text: "API-Key gespeichert.".into() });
            }
            Err(msg) => {
                self.state.notice = Some(Notice { kind: NoticeKind::Error, text: msg });
            }
        }
    }

    fn delete_key(&mut self, ctx: &egui::Context) {
        wipe_key_field(ctx, &mut self.state.key_input);
        match self.store.delete(Slot::ApiKey) {
            Ok(()) => {
                self.state.key_stored = Some(false);
                self.state.notice = Some(Notice { kind: NoticeKind::Info, text: "API-Key gelöscht.".into() });
            }
            Err(e) => {
                self.state.notice = Some(Notice { kind: NoticeKind::Error, text: e.to_string() });
            }
        }
    }
}

impl eframe::App for CerberusApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested()) && !self.control.close_means_exit() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.control.hide(&ctx);
        }

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.draw(ui, &ctx);
            });
        });
        self.draw_confirm(&ctx);
    }
}

// ---- drawing -------------------------------------------------------------------------------------

fn mode_style(mode: ModeDisplay) -> (&'static str, &'static str, Color32) {
    match mode {
        ModeDisplay::Max => (
            "Max (Abo)",
            "Obsidian läuft über deine Max-Anmeldung. Es entstehen keine API-Kosten.",
            Color32::from_rgb(46, 160, 90),
        ),
        ModeDisplay::ApiKey => (
            "API-Key (kostet Geld)",
            "Obsidian läuft mit deinem API-Key. Jede Claudian-Antwort wird abgerechnet.",
            Color32::from_rgb(230, 150, 30),
        ),
        ModeDisplay::Unknown => (
            "Modus unbekannt",
            "Obsidian läuft, wurde aber nicht über diese App gestartet.",
            Color32::from_rgb(140, 140, 150),
        ),
        ModeDisplay::Closed => (
            "Obsidian ist geschlossen",
            "Starte Obsidian über einen der Buttons.",
            Color32::from_rgb(100, 130, 170),
        ),
    }
}

fn banner(ui: &mut egui::Ui, fill: Color32, text: &str) {
    egui::Frame::new()
        .fill(fill)
        .corner_radius(6)
        .inner_margin(10)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(text).strong().color(Color32::WHITE));
        });
}

impl CerberusApp {
    fn draw(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let (title, hint, color) = mode_style(self.state.mode);
        ui.add_space(4.0);
        ui.label(RichText::new(title).size(30.0).strong().color(color));
        ui.label(RichText::new(hint).weak());
        ui.add_space(8.0);

        let report = self.state.snapshot.as_ref().and_then(|s| s.report.as_ref());
        if let Some(pause) = report.and_then(|r| r.pause.as_ref()) {
            let text = pause_text(pause.until.with_timezone(&Local), &pause.limit_type);
            banner(ui, Color32::from_rgb(170, 60, 50), &text);
            ui.add_space(8.0);
        }
        if let Some(err) = self.state.snapshot.as_ref().and_then(|s| s.scan_error.as_ref()) {
            banner(ui, Color32::from_rgb(170, 60, 50), &format!("Fehler beim Lesen der Protokolle: {err}"));
            ui.add_space(8.0);
        }
        if let Some(notice) = &self.state.notice {
            let fill = match notice.kind {
                NoticeKind::Info => Color32::from_rgb(50, 100, 160),
                NoticeKind::Error => Color32::from_rgb(170, 60, 50),
            };
            banner(ui, fill, &notice.text);
            ui.add_space(8.0);
        }

        self.draw_launcher(ui, ctx);
        ui.add_space(12.0);
        self.draw_usage(ui);
        ui.add_space(12.0);
        self.draw_settings(ui);
    }

    fn draw_launcher(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let idle = self.state.launching.is_none();
        let mut start: Option<Mode> = None;
        ui.horizontal(|ui| {
            let size = egui::vec2(190.0, 36.0);
            if ui.add_enabled(idle, egui::Button::new("Mit Max starten").min_size(size)).clicked() {
                start = Some(Mode::Max);
            }
            if ui.add_enabled(idle, egui::Button::new("Mit API-Key starten").min_size(size)).clicked() {
                start = Some(Mode::ApiKey);
            }
            if let Some((_, confirmed)) = self.state.launching {
                ui.spinner();
                ui.label(if confirmed { "Obsidian wird geschlossen …" } else { "Obsidian wird gestartet …" });
            }
        });
        if let Some(mode) = start {
            self.start_launch(ctx, mode, false);
        }
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

    fn draw_usage(&self, ui: &mut egui::Ui) {
        let Some(snapshot) = &self.state.snapshot else {
            ui.label("Lese Protokolle …");
            return;
        };
        let Some(report) = &snapshot.report else {
            ui.label(progress_text(snapshot.scanned_files, snapshot.total_files));
            return;
        };

        ui.horizontal_top(|ui| {
            let width = (ui.available_width() - 2.0 * ui.spacing().item_spacing.x) / 3.0;
            card(ui, width, "Heute", &report.today);
            card(ui, width, "Dieser Monat", &report.month);
            card(ui, width, "Letzte 5 Std.", &report.last_5h);
        });
        ui.add_space(12.0);

        ui.heading("Diesen Monat nach Projekt");
        bucket_table(ui, "by_project", "Projekt", &report.by_project);
        ui.add_space(12.0);
        ui.heading("Diesen Monat nach Modell");
        bucket_table(ui, "by_model", "Modell", &report.by_model);
        ui.add_space(8.0);

        ui.label(
            RichText::new(format!(
                "Stand: {} · Preisliste vom {} · Kosten sind {COST_NOTE}",
                format_time(snapshot.updated),
                self.prices_as_of
            ))
            .weak()
            .small(),
        );
    }

    fn draw_settings(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Einstellungen").default_open(false).show(ui, |ui| {
            ui.label("API-Key (nur für „Mit API-Key starten“, liegt im Windows-Anmeldeinformationsspeicher)");
            ui.label(match self.state.key_stored {
                Some(true) => "gespeichert",
                _ => "nicht gespeichert",
            });
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.state.key_input)
                        .id(key_field_id())
                        .password(true)
                        .hint_text("sk-ant-…")
                        .desired_width(320.0),
                );
                if ui.add_enabled(!self.state.key_input.is_empty(), egui::Button::new("Speichern")).clicked() {
                    self.save_key(ui.ctx());
                }
                if ui.add_enabled(self.state.key_stored == Some(true), egui::Button::new("Löschen")).clicked() {
                    self.delete_key(ui.ctx());
                }
            });
        });
    }
}

fn format_time(t: DateTime<Local>) -> String {
    t.format("%d.%m.%Y %H:%M:%S").to_string()
}

fn token_summary(b: &Bucket) -> String {
    format!(
        "{} Ein · {} Cache · {} Aus",
        format_count(b.input),
        format_count(b.cache_read.saturating_add(b.cache_write)),
        format_count(b.output)
    )
}

fn card(ui: &mut egui::Ui, width: f32, title: &str, b: &Bucket) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(width - 20.0);
        // The surrounding row is horizontal; each card stacks its lines vertically.
        ui.vertical(|ui| {
            ui.label(RichText::new(title).strong());
            ui.label(RichText::new(format_usd(b.cost_nano)).size(26.0).strong());
            ui.label(RichText::new(COST_NOTE).weak().small());
            if let Some(text) = unpriced_text(b.unpriced_requests) {
                ui.label(RichText::new(text).small().color(Color32::from_rgb(230, 150, 30)));
            }
            ui.add_space(4.0);
            ui.label(format!("{} Anfragen", format_count(b.requests)));
            ui.label(RichText::new(token_summary(b)).small());
        });
    });
}

/// Right-aligned cell with a fixed width, so every column (and the row striping) has a stable size.
fn right(ui: &mut egui::Ui, width: f32, text: impl Into<String>) {
    let size = egui::vec2(width, ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(size, egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.label(text.into());
    });
}

fn bucket_table(ui: &mut egui::Ui, id: &str, first_col: &str, rows: &[(String, Bucket)]) {
    if rows.is_empty() {
        ui.label(RichText::new("Noch keine Anfragen in diesem Monat.").weak());
        return;
    }
    egui::Grid::new(id).striped(true).num_columns(5).spacing([16.0, 4.0]).show(ui, |ui| {
        ui.label(RichText::new(first_col).strong());
        right(ui, 70.0, "Anfragen");
        right(ui, 120.0, "Ein (inkl. Cache)");
        right(ui, 80.0, "Aus");
        right(ui, 250.0, format!("Kosten ({COST_NOTE})"));
        ui.end_row();
        for (name, b) in rows {
            ui.scope(|ui| {
                ui.set_max_width(280.0);
                ui.add(egui::Label::new(name.as_str()).truncate()).on_hover_text(name.as_str());
            });
            right(ui, 70.0, format_count(b.requests));
            right(ui, 120.0, format_count(b.input.saturating_add(b.cache_read).saturating_add(b.cache_write)));
            right(ui, 80.0, format_count(b.output));
            let cost = match unpriced_text(b.unpriced_requests) {
                Some(note) => format!("{} · {note}", format_usd(b.cost_nano)),
                None => format_usd(b.cost_nano),
            };
            right(ui, 250.0, cost);
            ui.end_row();
        }
    });
}

// ---- startup -------------------------------------------------------------------------------------

fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("CerberusUsage")
}

fn logs_root() -> PathBuf {
    PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default()).join(".claude").join("projects")
}

/// Opens the window and the tray icon and runs until "Beenden".
pub fn run() -> eframe::Result {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
        .expect("embedded icon is a valid PNG");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Cerberus Usage")
            .with_inner_size([820.0, 760.0])
            .with_min_inner_size([620.0, 420.0])
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
        assert_eq!(api_key_stored(&store), Some(false));
    }

    #[test]
    fn valid_key_is_trimmed_and_stored() {
        let store = FakeStore(RefCell::new(None));
        save_api_key(&store, "  sk-ant-api03-fake-test-value\n").unwrap();
        assert_eq!(api_key_stored(&store), Some(true));
        assert_eq!(store.0.borrow().as_deref(), Some("sk-ant-api03-fake-test-value"));
    }

    /// Runs one frame with the key field; the text lives in `text`.
    fn key_field_frame(ctx: &egui::Context, text: &mut String, events: Vec<egui::Event>, focus: bool) {
        let input = egui::RawInput { events, ..Default::default() };
        let mut output = ctx.run_ui(input, |ui| {
            let r = ui.add(egui::TextEdit::singleline(text).password(true).id(key_field_id()));
            if focus {
                r.request_focus();
            }
        });
        output.textures_delta.clear();
    }

    #[test]
    fn wiping_the_key_field_removes_its_undo_history() {
        let ctx = egui::Context::default();
        let mut text = String::new();
        key_field_frame(&ctx, &mut text, vec![], true);
        key_field_frame(&ctx, &mut text, vec![egui::Event::Text("sk-ant-fake-test-value".into())], true);
        key_field_frame(&ctx, &mut text, vec![], true);
        assert_eq!(text, "sk-ant-fake-test-value");
        assert!(
            egui::TextEdit::load_state(&ctx, key_field_id()).is_some(),
            "egui should hold widget state (undo history) for the typed text"
        );

        wipe_key_field(&ctx, &mut text);

        assert!(text.is_empty());
        assert!(
            egui::TextEdit::load_state(&ctx, key_field_id()).is_none(),
            "widget state must be gone so Ctrl+Z cannot restore the key"
        );
        // A following frame starts from scratch: still empty.
        key_field_frame(&ctx, &mut text, vec![], true);
        assert!(text.is_empty());
    }
}
