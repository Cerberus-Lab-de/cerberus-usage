//! Background service: periodic incremental log scan, report build, snapshots for the UI.

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};

use crate::analytics::{ApiPoller, ApiStatus, Http};
use crate::balance::{Alert, BalanceStatus, alerts, status as balance_status};
use crate::i18n::{tr, tr_args};
use crate::settings::{Settings, load as load_settings};
use crate::log_scan::{ScanState, ScanStats, load_state_checked, save_state, scan};
use crate::pricing::PriceTable;
use crate::report::{Report, build};
use crate::secrets::SecretStore;

/// Minimum gap between intermediate progress snapshots.
const PROGRESS_THROTTLE: Duration = Duration::from_millis(200);

/// Default pause between two scans.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// The scan state is written at most this often, and only when the scan changed something.
/// Re-reading after a crash is idempotent (dedupe by message id takes the maximum), so the gap costs
/// nothing but a few seconds of re-reading.
pub const SAVE_INTERVAL: Duration = Duration::from_secs(300);

/// No `Debug` on purpose: it holds the secret store.
#[derive(Clone)]
pub struct WorkerConfig {
    pub logs_root: PathBuf,
    /// Set when the log folder could not be determined (`logs_root` is then a placeholder and never
    /// scanned). Shown as the scan error, so the window explains the empty tables.
    pub logs_unavailable: Option<String>,
    pub state_path: PathBuf,
    pub prices_path: PathBuf,
    /// Pause between scans (see [`DEFAULT_INTERVAL`]).
    pub interval: Duration,
    /// Where the admin key is read from (fresh on every API poll).
    pub secrets: Arc<dyn SecretStore + Send + Sync>,
    pub http: Arc<dyn Http + Send + Sync>,
    /// Pause between API polls (see `analytics::DEFAULT_API_INTERVAL`; raised to at least one minute).
    pub api_interval: Duration,
    /// `settings.json` (see `settings::default_path`). `None` when it cannot be determined:
    /// no balance, no alerts, and `Snapshot::settings_error` says why.
    pub settings_path: Option<PathBuf>,
}

/// What the UI gets from the worker. `report` is `None` only while the very
/// first scan is still running (progress snapshots).
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub report: Option<Report>,
    pub scanned_files: usize,
    pub total_files: usize,
    pub scan_error: Option<String>,
    pub updated: DateTime<Local>,
    /// Real API cost. `None` without an admin key or before the first poll.
    pub api: Option<ApiStatus>,
    /// Estimated credit balance. `None` without a baseline in the settings.
    pub balance: Option<BalanceStatus>,
    /// Active warnings (budget, low balance, Max pause).
    pub alerts: Vec<Alert>,
    /// Why `settings.json` could not be used (the last good settings stay in effect).
    pub settings_error: Option<String>,
    /// One-time note from loading the saved state (e.g. an unreadable file was set aside).
    pub state_note: Option<String>,
}

impl Snapshot {
    /// Single construction point, so new fields are added in one place.
    fn new(
        report: Option<Report>,
        scanned_files: usize,
        total_files: usize,
        scan_error: Option<String>,
    ) -> Snapshot {
        Snapshot {
            report,
            scanned_files,
            total_files,
            scan_error,
            updated: Local::now(),
            api: None,
            balance: None,
            alerts: Vec::new(),
            settings_error: None,
            state_note: None,
        }
    }
}

/// Decides when the scan state is written: not more often than [`SAVE_INTERVAL`]. Time is passed in.
struct SaveGate {
    last_save: Option<Instant>,
}

impl SaveGate {
    fn due(&self, now: Instant) -> bool {
        self.last_save.is_none_or(|t| now.saturating_duration_since(t) >= SAVE_INTERVAL)
    }

    fn saved(&mut self, now: Instant) {
        self.last_save = Some(now);
    }
}

/// Everything the worker thread owns between ticks.
struct Core {
    state: ScanState,
    prices: PriceTable,
    last_report: Option<Report>,
    live: Live,
    gate: SaveGate,
    state_note: Option<String>,
}

/// Everything that changes between ticks besides the scan state.
struct Live {
    api: ApiPoller,
    settings: Settings,
    settings_error: Option<String>,
}

impl Live {
    /// Re-reads `settings.json` (a small file, so every tick). A broken file keeps the last good
    /// settings; the problem is reported instead.
    fn reload_settings(&mut self, path: Option<&Path>) {
        let Some(path) = path else {
            self.settings_error = Some(tr("worker.settings_no_localappdata"));
            return;
        };
        let (loaded, error) = load_settings(path);
        if error.is_none() {
            self.settings = loaded;
        }
        self.settings_error = error;
        self.api.set_since(self.settings.baseline.as_ref().map(|b| b.at));
    }

    /// Fills the API, balance and alert fields from the current state.
    fn fill(&self, snapshot: &mut Snapshot) {
        snapshot.api = self.api.current().cloned();
        snapshot.balance = balance_status(&self.settings, snapshot.api.as_ref(), Local::now());
        let pause = snapshot.report.as_ref().and_then(|r| r.pause.as_ref());
        snapshot.alerts = alerts(
            &self.settings,
            // Only a real figure: after a first failed poll the 0 is a placeholder.
            snapshot.api.as_ref().filter(|a| a.has_data).map(|a| a.month_nano),
            snapshot.balance.as_ref().map(|b| b.estimate_nano),
            pause,
            Utc::now(),
        );
        snapshot.settings_error = self.settings_error.clone();
    }
}

/// Starts the worker thread. It exits on its own once the receiver is dropped
/// (detected at the next send). If it dies anyway, the sender is dropped with it and the UI sees a
/// disconnected channel.
pub fn spawn_worker(cfg: WorkerConfig, notify: Box<dyn Fn() + Send>) -> Receiver<Snapshot> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = catch_unwind(AssertUnwindSafe(|| run(cfg, notify, tx)));
    });
    rx
}

/// Sends a snapshot and pokes the UI. Returns false when the receiver is gone.
fn deliver(tx: &Sender<Snapshot>, notify: &dyn Fn(), snapshot: Snapshot) -> bool {
    if tx.send(snapshot).is_err() {
        return false;
    }
    notify();
    true
}

fn new_core(cfg: &WorkerConfig) -> Core {
    let loaded = load_state_checked(&cfg.state_path);
    Core {
        state: loaded.state,
        prices: PriceTable::load(&cfg.prices_path),
        last_report: None,
        live: Live {
            api: ApiPoller::new(Arc::clone(&cfg.secrets), Arc::clone(&cfg.http), cfg.api_interval),
            settings: Settings::default(),
            settings_error: None,
        },
        gate: SaveGate { last_save: None },
        state_note: loaded.note,
    }
}

fn run(cfg: WorkerConfig, notify: Box<dyn Fn() + Send>, tx: Sender<Snapshot>) {
    let mut core = new_core(&cfg);
    loop {
        if !guarded_tick(&cfg, &mut core, &tx, notify.as_ref(), Instant::now()) {
            break;
        }
        std::thread::sleep(cfg.interval);
    }
    // Clean end (the window is gone): do not lose what the last ticks read.
    if core.state.dirty {
        let _ = save_state(&cfg.state_path, &core.state);
    }
}

/// One tick that cannot end the worker: a panic anywhere in it becomes an error snapshot and the
/// next tick tries again. Returns false when the receiver is gone.
fn guarded_tick(cfg: &WorkerConfig, core: &mut Core, tx: &Sender<Snapshot>, notify: &dyn Fn(), now: Instant) -> bool {
    match catch_unwind(AssertUnwindSafe(|| tick(cfg, core, tx, notify, now))) {
        Ok(alive) => alive,
        Err(_) => {
            let mut snapshot = Snapshot::new(
                core.last_report.clone(),
                0,
                0,
                Some(tr("worker.internal_error")),
            );
            snapshot.state_note = core.state_note.clone();
            deliver(tx, notify, snapshot)
        }
    }
}

/// One scan + report cycle. Returns false when the receiver is gone.
fn tick(cfg: &WorkerConfig, core: &mut Core, tx: &Sender<Snapshot>, notify: &dyn Fn(), now: Instant) -> bool {
    let first_scan = core.last_report.is_none();
    let receiver_gone = Cell::new(false);
    let last_sent: Cell<Option<Instant>> = Cell::new(None);

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let progress = |done: usize, total: usize| {
            // Progress only matters while there is no report yet; the final
            // snapshot of each tick follows right after the scan.
            if !first_scan || receiver_gone.get() || (done == total && total > 0) {
                return;
            }
            let due = last_sent
                .get()
                .is_none_or(|t| t.elapsed() >= PROGRESS_THROTTLE);
            if due {
                last_sent.set(Some(Instant::now()));
                if !deliver(tx, notify, Snapshot::new(None, done, total, None)) {
                    receiver_gone.set(true);
                }
            }
        };
        if cfg.logs_unavailable.is_some() {
            return Ok(ScanStats::default());
        }
        scan(&cfg.logs_root, &mut core.state, &progress)
    }));
    if receiver_gone.get() {
        return false;
    }

    let (files, scan_failed, mut error) = match outcome {
        Ok(Ok(stats)) => (stats.files_total, false, None),
        Ok(Err(e)) => (0, true, Some(tr_args("worker.scan_failed", &[("error", &e.to_string())]))),
        Err(_) => (0, true, Some(tr("worker.scan_failed_internal"))),
    };

    // Written only when the scan changed something and not more often than every SAVE_INTERVAL.
    if !scan_failed && core.state.dirty && core.gate.due(now) {
        match save_state(&cfg.state_path, &core.state) {
            Ok(()) => {
                core.state.dirty = false;
                core.gate.saved(now);
            }
            Err(e) => error = Some(tr_args("worker.save_failed", &[("error", &e.to_string())])),
        }
    }
    if error.is_none() {
        error = match &cfg.logs_unavailable {
            Some(reason) => Some(reason.clone()),
            None if !cfg.logs_root.is_dir() => Some(tr_args(
                "worker.logs_not_found",
                &[
                    ("path", &cfg.logs_root.display().to_string()),
                    ("settings", &tr("ui.settings")),
                    ("paths", &tr("ui.paths")),
                ],
            )),
            None => None,
        };
    }

    // After a failed scan keep the previous report; otherwise build a fresh one.
    let report = match (scan_failed, core.last_report.clone()) {
        (true, Some(previous)) => previous,
        _ => build(&core.state, &core.prices, Local::now()),
    };
    core.last_report = Some(report.clone());

    // The report goes out first with the last known API figures; the network call (up to
    // 30 s per request) must not hold it back. A changed API status follows as a second snapshot.
    let mut snapshot = Snapshot::new(Some(report), files, files, error);
    snapshot.state_note = core.state_note.clone();
    core.live.reload_settings(cfg.settings_path.as_deref());
    core.live.fill(&mut snapshot);
    if !deliver(tx, notify, snapshot.clone()) {
        return false;
    }
    if core.live.api.due(Instant::now()) {
        // Same protection as the scan: a panic in the poll path must not end the worker.
        let changed = catch_unwind(AssertUnwindSafe(|| core.live.api.refresh(Instant::now(), Local::now())))
            .unwrap_or(false);
        if changed {
            core.live.fill(&mut snapshot);
            return deliver(tx, notify, snapshot);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_scan::load_state;
    use crate::analytics::testing::{FakeHttp, FakeStore, ok};
    use crate::analytics::{Http, NetError};
    use chrono::Utc;
    use serde_json::json;
    use std::fs;
    use std::io::Write;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    const WAIT: Duration = Duration::from_secs(10);

    fn usage_line(id: &str) -> String {
        json!({
            "timestamp": Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "cwd": "C:\\Work\\demo-project",
            "sessionId": "sess-1",
            "isSidechain": false,
            "message": {
                "id": id,
                "model": "claude-sonnet-5",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 5,
                    "cache_read_input_tokens": 20,
                    "cache_creation_input_tokens": 30,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": 30,
                        "ephemeral_1h_input_tokens": 0
                    }
                }
            }
        })
        .to_string()
    }

    fn write_log(root: &Path, rel: &str, ids: &[&str]) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lines: Vec<String> = ids.iter().map(|id| usage_line(id)).collect();
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    fn config(dir: &Path, interval: Duration) -> WorkerConfig {
        WorkerConfig {
            logs_root: dir.join("logs"),
            logs_unavailable: None,
            state_path: dir.join("data").join("state.json"),
            prices_path: dir.join("data").join("prices.json"),
            interval,
            secrets: Arc::new(FakeStore::new(None)),
            http: Arc::new(FakeHttp::new(vec![])),
            api_interval: crate::analytics::DEFAULT_API_INTERVAL,
            settings_path: Some(dir.join("data").join("settings.json")),
        }
    }

    fn write_settings(cfg: &WorkerConfig, json: &str) {
        let path = cfg.settings_path.as_ref().unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, json).unwrap();
    }

    /// Settings with a baseline relative to now (3 days ago, whole hours), so the tests do not age:
    /// the poll uses the real clock and a fixed date would eventually need more than one chunk.
    fn baseline_json() -> String {
        let at = |days: i64| {
            let t = Utc::now() - chrono::Duration::days(days);
            t.format("%Y-%m-%dT%H:00:00Z").to_string()
        };
        format!(
            r#"{{"baseline":{{"amount_nano":50000000000,"at":"{}"}},
            "topups":[{{"amount_nano":10000000000,"at":"{}"}}],
            "monthly_budget_nano":1,"low_balance_nano":100000000000}}"#,
            at(3),
            at(2)
        )
    }

    /// Config with an admin key and a scripted API; also returns the fake to inspect its requests.
    fn config_with_api(
        dir: &Path,
        admin: Option<&str>,
        responses: Vec<Result<(u16, String), crate::analytics::NetError>>,
    ) -> (WorkerConfig, Arc<FakeHttp>) {
        let http = Arc::new(FakeHttp::new(responses));
        let mut cfg = config(dir, Duration::from_secs(60));
        cfg.secrets = Arc::new(FakeStore::new(admin));
        cfg.http = http.clone();
        (cfg, http)
    }

    fn noop() -> Box<dyn Fn() + Send> {
        Box::new(|| {})
    }

    /// Receives until a snapshot with a report arrives; returns it plus all earlier ones.
    fn until_report(rx: &Receiver<Snapshot>) -> (Vec<Snapshot>, Snapshot) {
        let mut earlier = Vec::new();
        loop {
            let snap = rx.recv_timeout(WAIT).expect("no snapshot in time");
            if snap.report.is_some() {
                return (earlier, snap);
            }
            earlier.push(snap);
        }
    }

    /// Receives until a snapshot carries an API status (or the deadline passes). Snapshots may arrive in
    /// between, e.g. from a scan tick; the test must not depend on their exact order.
    fn next_with_api(rx: &Receiver<Snapshot>) -> Snapshot {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let snap = rx.recv_timeout(left).expect("no API snapshot in time");
            if snap.api.is_some() {
                return snap;
            }
        }
    }

    #[test]
    fn sends_progress_then_report() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), Duration::from_secs(60));
        write_log(&cfg.logs_root, "p1/a.jsonl", &["m1"]);
        write_log(&cfg.logs_root, "p1/b.jsonl", &["m2"]);
        write_log(&cfg.logs_root, "p2/c.jsonl", &["m3"]);

        let notified = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&notified);
        let rx = spawn_worker(
            cfg,
            Box::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        );

        let (progress, last) = until_report(&rx);
        assert!(!progress.is_empty(), "expected at least one progress snapshot");
        assert!(progress.iter().all(|s| s.report.is_none() && s.total_files == 3));
        assert!(progress.iter().all(|s| s.scanned_files <= s.total_files));
        assert_eq!((last.scanned_files, last.total_files), (3, 3));
        assert!(last.scan_error.is_none());
        assert_eq!(last.report.unwrap().last_5h.requests, 3);
        // notify() runs after send(), so poll instead of reading right after recv.
        let deadline = Instant::now() + Duration::from_secs(2);
        while notified.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(notified.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn picks_up_appended_lines_on_next_tick() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), Duration::from_millis(50));
        write_log(&cfg.logs_root, "p/a.jsonl", &["m1"]);
        let log = cfg.logs_root.join("p/a.jsonl");
        let rx = spawn_worker(cfg, noop());

        let (_, first) = until_report(&rx);
        assert_eq!(first.report.unwrap().last_5h.requests, 1);

        let mut f = fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all((usage_line("m2") + "\n").as_bytes()).unwrap();
        drop(f);

        let deadline = Instant::now() + WAIT;
        loop {
            let snap = rx.recv_timeout(WAIT).expect("no snapshot in time");
            if snap.report.is_some_and(|r| r.last_5h.requests == 2) {
                break;
            }
            assert!(Instant::now() < deadline, "appended line never showed up");
        }
    }

    #[test]
    fn persists_state_for_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), Duration::from_secs(60));
        write_log(&cfg.logs_root, "p/a.jsonl", &["m1", "m2"]);

        let rx = spawn_worker(cfg.clone(), noop());
        let (_, snap) = until_report(&rx);
        assert!(snap.scan_error.is_none());
        assert_eq!(load_state(&cfg.state_path).records.len(), 2);
        drop(rx);

        // Second start with the logs gone: the saved state alone feeds the report.
        fs::remove_dir_all(&cfg.logs_root).unwrap();
        let rx = spawn_worker(cfg, noop());
        let (_, snap) = until_report(&rx);
        assert_eq!(snap.report.unwrap().month.requests, 2);
    }

    #[test]
    fn unresolved_logs_root_is_never_scanned_and_explained() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path(), Duration::from_secs(60));
        // A log below the (relative) placeholder must not be picked up.
        cfg.logs_root = PathBuf::new();
        cfg.logs_unavailable = Some("Der Protokollordner lässt sich nicht bestimmen.".into());
        let rx = spawn_worker(cfg, noop());
        let (_, snap) = until_report(&rx);
        assert_eq!(snap.scan_error.as_deref(), Some("Der Protokollordner lässt sich nicht bestimmen."));
        assert_eq!(snap.total_files, 0);
        assert!(snap.report.is_some(), "the rest of the window stays usable");
    }

    #[test]
    fn missing_logs_root_reports_empty_not_crash() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), Duration::from_secs(60));
        assert!(!cfg.logs_root.exists());
        let rx = spawn_worker(cfg, noop());

        let (_, snap) = until_report(&rx);
        let msg = snap.scan_error.clone().expect("a missing folder must be explained");
        assert!(msg.starts_with("Protokollordner nicht gefunden:") && msg.contains("logs"), "{msg}");
        assert_eq!(snap.total_files, 0);
        let report = snap.report.unwrap();
        assert_eq!(report.today.requests, 0);
        assert_eq!(report.month.requests, 0);
        assert_eq!(report.last_5h.requests, 0);
        assert!(report.by_model.is_empty() && report.by_project.is_empty());
    }

    #[test]
    fn exits_when_receiver_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), Duration::from_millis(50));
        // The notify closure owns a sender; it is dropped when the thread exits.
        let (ntx, nrx) = mpsc::channel::<()>();
        let rx = spawn_worker(
            cfg,
            Box::new(move || {
                let _ = ntx.send(());
            }),
        );
        let _ = until_report(&rx);
        drop(rx);
        loop {
            match nrx.recv_timeout(WAIT) {
                Ok(()) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => panic!("worker did not exit"),
            }
        }
    }

    #[test]
    fn snapshot_has_no_api_without_admin_key() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, http) = config_with_api(dir.path(), None, vec![]);
        let rx = spawn_worker(cfg, noop());
        let (_, snap) = until_report(&rx);
        assert!(snap.api.is_none());
        assert_eq!(http.request_count(), 0, "no admin key must mean no HTTP call");
    }

    #[test]
    fn delivers_api_status_after_report_when_admin_key_exists() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"{"data":[],"has_more":false,"next_page":null}"#;
        let (cfg, http) = config_with_api(dir.path(), Some("sk-ant-admin01-NICHTECHT-1234"), vec![ok(body)]);
        let rx = spawn_worker(cfg, noop());

        // The report is not held back by the API call: first full snapshot has no status yet.
        let (_, first) = until_report(&rx);
        assert!(first.api.is_none());
        let second = next_with_api(&rx);
        let api = second.api.expect("api status missing");
        assert_eq!((api.today_nano, api.month_nano, api.error), (0, 0, None));
        assert!(second.report.is_some());
        assert_eq!(http.request_count(), 1);
    }

    #[test]
    fn snapshot_debug_never_contains_admin_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = "sk-ant-admin01-NICHTECHT-1234";
        // First poll fails (401), so the snapshot carries an error status; the key is set throughout.
        let (cfg, _http) = config_with_api(dir.path(), Some(key), vec![Ok((401, "{}".to_string()))]);
        let rx = spawn_worker(cfg, noop());
        let (_, first) = until_report(&rx);
        let second = next_with_api(&rx);
        assert_eq!(second.api.as_ref().and_then(|a| a.error.clone()), Some(crate::analytics::ApiError::Unauthorized));
        for snap in [&first, &second] {
            let shown = format!("{snap:?}");
            assert!(!shown.contains("NICHTECHT") && !shown.contains("1234"), "snapshot leaked key");
        }
    }

    #[test]
    fn no_baseline_means_no_balance_and_no_extra_request() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"{"data":[],"has_more":false,"next_page":null}"#;
        let (cfg, http) = config_with_api(dir.path(), Some("sk-ant-admin01-NICHTECHT-1234"), vec![ok(body)]);
        let rx = spawn_worker(cfg, noop());
        let (_, first) = until_report(&rx);
        let second = next_with_api(&rx);
        assert!(first.balance.is_none() && second.balance.is_none());
        assert!(second.alerts.is_empty() && second.settings_error.is_none());
        assert_eq!(http.request_count(), 1);
    }

    #[test]
    fn baseline_without_admin_key_shows_undeducted_estimate() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, http) = config_with_api(dir.path(), None, vec![]);
        write_settings(&cfg, &baseline_json());
        let rx = spawn_worker(cfg, noop());
        let (_, snap) = until_report(&rx);
        let balance = snap.balance.expect("balance missing");
        assert_eq!((balance.estimate_nano, balance.cost_deducted), (60_000_000_000, false));
        // 60 USD is below the 100 USD warning level; the budget alert needs API cost and stays off.
        assert_eq!(snap.alerts, vec![Alert::LowBalance]);
        assert_eq!(http.request_count(), 0);
    }

    #[test]
    fn no_budget_alert_after_a_first_failed_poll() {
        let dir = tempfile::tempdir().unwrap();
        // The budget in BASELINE_JSON is 1 nano: a placeholder "0 cost" must not be compared with it
        // and a real cost of 0 must not be invented either.
        let (cfg, _http) = config_with_api(
            dir.path(),
            Some("sk-ant-admin01-NICHTECHT-1234"),
            vec![Ok((429, "{}".to_string()))],
        );
        write_settings(&cfg, &baseline_json());
        let rx = spawn_worker(cfg, noop());
        let (_, _first) = until_report(&rx);
        let second = next_with_api(&rx);
        let api = second.api.as_ref().expect("api status missing");
        assert!(!api.has_data && api.error.is_some());
        assert!(!second.alerts.contains(&Alert::BudgetExceeded), "alerts: {:?}", second.alerts);
    }

    #[test]
    fn baseline_with_admin_key_deducts_cost_since_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let totals = r#"{"data":[],"has_more":false,"next_page":null}"#;
        let since = r#"{"data":[{"starting_at":"2026-11-01T00:00:00Z","ending_at":"2026-11-01T01:00:00Z","group":{"model":"m"},"cost":{"amount":"20000.000000"}}],"has_more":false,"next_page":null}"#;
        let (cfg, http) = config_with_api(dir.path(), Some("sk-ant-admin01-NICHTECHT-1234"), vec![ok(totals), ok(since)]);
        write_settings(&cfg, &baseline_json());
        let rx = spawn_worker(cfg, noop());
        let (_, first) = until_report(&rx);
        assert!(!first.balance.unwrap().cost_deducted);
        let second = next_with_api(&rx);
        // 20000 cents = 200 USD.
        let balance = second.balance.expect("balance missing");
        assert_eq!(balance.cost_since_nano, Some(200_000_000_000));
        assert_eq!(balance.estimate_nano, 60_000_000_000 - 200_000_000_000);
        assert!(balance.cost_deducted);
        assert_eq!(http.request_count(), 2);
    }

    #[test]
    fn broken_settings_are_reported_and_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, _http) = config_with_api(dir.path(), None, vec![]);
        write_settings(&cfg, "{ nope");
        let path = cfg.settings_path.clone().unwrap();
        let rx = spawn_worker(cfg, noop());
        let (_, snap) = until_report(&rx);
        assert!(snap.settings_error.is_some() && snap.balance.is_none());
        assert_eq!(fs::read_to_string(path).unwrap(), "{ nope");
    }

    #[test]
    fn missing_settings_path_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path(), Duration::from_secs(60));
        cfg.settings_path = None;
        let rx = spawn_worker(cfg, noop());
        let (_, snap) = until_report(&rx);
        assert!(snap.settings_error.is_some() && snap.balance.is_none());
    }

    #[test]
    fn settings_edits_are_picked_up_on_the_next_tick() {
        let dir = tempfile::tempdir().unwrap();
        let (mut cfg, _http) = config_with_api(dir.path(), None, vec![]);
        cfg.interval = Duration::from_millis(50);
        let rx = spawn_worker(cfg.clone(), noop());
        let (_, first) = until_report(&rx);
        assert!(first.balance.is_none());
        write_settings(&cfg, &baseline_json());
        let deadline = Instant::now() + WAIT;
        loop {
            let snap = rx.recv_timeout(WAIT).expect("no snapshot in time");
            if snap.balance.is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "settings change never showed up");
        }
    }

    #[test]
    fn snapshot_debug_never_contains_key_with_balance() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, _http) = config_with_api(
            dir.path(),
            Some("sk-ant-admin01-NICHTECHT-1234"),
            vec![Ok((401, "{}".to_string()))],
        );
        write_settings(&cfg, &baseline_json());
        let rx = spawn_worker(cfg, noop());
        let (_, first) = until_report(&rx);
        let second = next_with_api(&rx);
        assert!(second.balance.is_some());
        for snap in [&first, &second] {
            let shown = format!("{snap:?}");
            assert!(!shown.contains("NICHTECHT") && !shown.contains("1234"), "snapshot leaked key");
        }
    }

    /// A core plus config for driving `tick` directly with an injected clock.
    fn core_for(cfg: &WorkerConfig) -> Core {
        new_core(cfg)
    }

    fn drain(rx: &Receiver<Snapshot>) -> Vec<Snapshot> {
        rx.try_iter().collect()
    }

    #[test]
    fn state_is_saved_only_when_something_changed_and_at_most_every_five_minutes() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), Duration::from_secs(60));
        write_log(&cfg.logs_root, "p/a.jsonl", &["m1"]);
        let (tx, rx) = mpsc::channel();
        let mut core = core_for(&cfg);
        let t0 = Instant::now();

        // First scan changed things: saved at once.
        assert!(tick(&cfg, &mut core, &tx, &|| {}, t0));
        assert_eq!(load_state(&cfg.state_path).records.len(), 1);
        assert!(!core.state.dirty);

        // Nothing changed: the file is not written again (prove it by removing it).
        fs::remove_file(&cfg.state_path).unwrap();
        assert!(tick(&cfg, &mut core, &tx, &|| {}, t0 + Duration::from_secs(60)));
        assert!(!cfg.state_path.exists(), "an unchanged scan must not write the state");
        assert!(tick(&cfg, &mut core, &tx, &|| {}, t0 + Duration::from_secs(120)));
        assert!(!cfg.state_path.exists());

        // A change inside the 5-minute window waits...
        write_log(&cfg.logs_root, "p/b.jsonl", &["m2"]);
        assert!(tick(&cfg, &mut core, &tx, &|| {}, t0 + Duration::from_secs(180)));
        assert!(core.state.dirty, "changed but saved too recently");
        assert!(!cfg.state_path.exists());
        // ...but not forever: the report already shows the new record.
        let last = drain(&rx).into_iter().rev().find(|s| s.report.is_some()).unwrap();
        assert_eq!(last.report.unwrap().last_5h.requests, 2);

        // Once the gap is over, it is written (all records, not just the new one).
        assert!(tick(&cfg, &mut core, &tx, &|| {}, t0 + SAVE_INTERVAL + Duration::from_secs(1)));
        let last = drain(&rx).into_iter().last().unwrap();
        assert_eq!(last.scan_error, None, "saving the state must not come back with an error");
        assert!(!core.state.dirty);
        assert_eq!(load_state(&cfg.state_path).records.len(), 2);
    }

    #[test]
    fn dirty_state_is_written_when_the_worker_ends_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), Duration::from_millis(20));
        write_log(&cfg.logs_root, "p/a.jsonl", &["m1"]);
        let rx = spawn_worker(cfg.clone(), noop());
        let (_, _) = until_report(&rx);
        // A change that the 5-minute gate holds back from the periodic save.
        write_log(&cfg.logs_root, "p/b.jsonl", &["m2"]);
        let deadline = Instant::now() + WAIT;
        loop {
            let snap = rx.recv_timeout(WAIT).expect("no snapshot");
            if snap.report.is_some_and(|r| r.last_5h.requests == 2) {
                break;
            }
            assert!(Instant::now() < deadline);
        }
        assert_eq!(load_state(&cfg.state_path).records.len(), 1, "gate holds the periodic save back");
        drop(rx);
        let deadline = Instant::now() + WAIT;
        while load_state(&cfg.state_path).records.len() < 2 {
            assert!(Instant::now() < deadline, "worker did not save on its clean end");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn unreadable_state_file_is_set_aside_and_the_ui_gets_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), Duration::from_secs(60));
        fs::create_dir_all(cfg.state_path.parent().unwrap()).unwrap();
        fs::write(&cfg.state_path, "{ broken").unwrap();
        write_log(&cfg.logs_root, "p/a.jsonl", &["m1"]);
        let rx = spawn_worker(cfg.clone(), noop());
        let (_, snap) = until_report(&rx);
        assert!(snap.state_note.as_deref().is_some_and(|n| n.contains("nicht lesbar")), "{:?}", snap.state_note);
        assert_eq!(snap.report.unwrap().last_5h.requests, 1, "rebuilt from the logs");
        let mut bad = cfg.state_path.as_os_str().to_owned();
        bad.push(".bad");
        assert_eq!(fs::read_to_string(PathBuf::from(bad)).unwrap(), "{ broken");
    }

    #[test]
    fn panic_anywhere_in_a_tick_becomes_an_error_snapshot_and_the_worker_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), Duration::from_millis(30));
        write_log(&cfg.logs_root, "p/a.jsonl", &["m1"]);
        // notify() panics once, on the delivery of the very first report (outside the scan's own guard).
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let rx = spawn_worker(
            cfg,
            Box::new(move || {
                // Call 1 is the progress poke inside the scan guard, call 2 the report delivery.
                if counter.fetch_add(1, Ordering::SeqCst) == 1 {
                    panic!("simulated crash outside the scan guard");
                }
            }),
        );
        let deadline = Instant::now() + WAIT;
        let mut saw_error = false;
        let mut later_report = false;
        while Instant::now() < deadline && !(saw_error && later_report) {
            let Ok(snap) = rx.recv_timeout(Duration::from_secs(2)) else { break };
            if snap.scan_error.as_deref().is_some_and(|e| e.contains("Hintergrunddienst")) {
                saw_error = true;
            } else if saw_error && snap.report.is_some() && snap.scan_error.is_none() {
                later_report = true;
            }
        }
        assert!(saw_error, "the panic must be reported");
        assert!(later_report, "the worker must keep delivering afterwards");
    }

    /// An `Http` that always panics, to prove the worker survives a crash in the poll path.
    struct PanicHttp;

    impl Http for PanicHttp {
        fn get(&self, _url: &str, _headers: &[(&str, &str)]) -> Result<(u16, String), NetError> {
            panic!("simulated crash in the API path");
        }
    }

    #[test]
    fn panic_in_api_poll_does_not_kill_the_worker() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path(), Duration::from_millis(50));
        cfg.secrets = Arc::new(FakeStore::new(Some("sk-ant-admin01-NICHTECHT-1234")));
        cfg.http = Arc::new(PanicHttp);
        write_log(&cfg.logs_root, "p/a.jsonl", &["m1"]);
        let log = cfg.logs_root.join("p/a.jsonl");
        let rx = spawn_worker(cfg, noop());

        let (_, first) = until_report(&rx);
        assert_eq!(first.report.unwrap().last_5h.requests, 1);

        // The poll panicked after the first report; later ticks must still scan and deliver.
        let mut f = fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all((usage_line("m2") + "\n").as_bytes()).unwrap();
        drop(f);
        let deadline = Instant::now() + WAIT;
        loop {
            let snap = rx.recv_timeout(WAIT).expect("worker stopped delivering after the panic");
            if snap.report.is_some_and(|r| r.last_5h.requests == 2) {
                break;
            }
            assert!(Instant::now() < deadline, "appended line never showed up");
        }
    }
}
