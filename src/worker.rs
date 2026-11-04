//! Background service: periodic incremental log scan, report build, snapshots for the UI.

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};

use crate::analytics::{ApiPoller, ApiStatus, Http};
use crate::log_scan::{ScanState, load_state, save_state, scan};
use crate::pricing::PriceTable;
use crate::report::{Report, build};
use crate::secrets::SecretStore;

/// Minimum gap between intermediate progress snapshots.
const PROGRESS_THROTTLE: Duration = Duration::from_millis(200);

/// Default pause between two scans.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// No `Debug` on purpose: it holds the secret store.
#[derive(Clone)]
pub struct WorkerConfig {
    pub logs_root: PathBuf,
    pub state_path: PathBuf,
    pub prices_path: PathBuf,
    /// Pause between scans (see [`DEFAULT_INTERVAL`]).
    pub interval: Duration,
    /// Where the admin key is read from (fresh on every API poll).
    pub secrets: Arc<dyn SecretStore + Send + Sync>,
    pub http: Arc<dyn Http + Send + Sync>,
    /// Pause between API polls (see `analytics::DEFAULT_API_INTERVAL`; raised to at least one minute).
    pub api_interval: Duration,
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
        }
    }
}

/// Starts the worker thread. It exits on its own once the receiver is dropped
/// (detected at the next send).
pub fn spawn_worker(cfg: WorkerConfig, notify: Box<dyn Fn() + Send>) -> Receiver<Snapshot> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || run(cfg, notify, tx));
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

fn run(cfg: WorkerConfig, notify: Box<dyn Fn() + Send>, tx: Sender<Snapshot>) {
    let mut state = load_state(&cfg.state_path);
    let prices = PriceTable::load(&cfg.prices_path);
    let mut last_report: Option<Report> = None;
    let mut api = ApiPoller::new(Arc::clone(&cfg.secrets), Arc::clone(&cfg.http), cfg.api_interval);

    loop {
        if !tick(&cfg, &mut state, &prices, &mut last_report, &mut api, &tx, notify.as_ref()) {
            return;
        }
        std::thread::sleep(cfg.interval);
    }
}

/// One scan + report cycle. Returns false when the receiver is gone.
fn tick(
    cfg: &WorkerConfig,
    state: &mut ScanState,
    prices: &PriceTable,
    last_report: &mut Option<Report>,
    api: &mut ApiPoller,
    tx: &Sender<Snapshot>,
    notify: &dyn Fn(),
) -> bool {
    let first_scan = last_report.is_none();
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
        scan(&cfg.logs_root, state, &progress)
    }));
    if receiver_gone.get() {
        return false;
    }

    let (files, scan_failed, mut error) = match outcome {
        Ok(Ok(stats)) => (stats.files_total, false, None),
        Ok(Err(e)) => (0, true, Some(format!("Scan fehlgeschlagen: {e}"))),
        Err(_) => (
            0,
            true,
            Some("Scan fehlgeschlagen: interner Fehler".to_string()),
        ),
    };

    if !scan_failed
        && let Err(e) = save_state(&cfg.state_path, state)
    {
        error = Some(format!("Zustand konnte nicht gespeichert werden: {e}"));
    }

    // After a failed scan keep the previous report; otherwise build a fresh one.
    let report = match (scan_failed, last_report.clone()) {
        (true, Some(previous)) => previous,
        _ => build(state, prices, Local::now()),
    };
    *last_report = Some(report.clone());

    // The report goes out first with the last known API figures; the network call (up to
    // 30 s per request) must not hold it back. A changed API status follows as a second snapshot.
    let mut snapshot = Snapshot::new(Some(report), files, files, error);
    snapshot.api = api.current().cloned();
    if !deliver(tx, notify, snapshot.clone()) {
        return false;
    }
    if api.due(Instant::now()) && api.refresh(Instant::now(), Local::now()) {
        snapshot.api = api.current().cloned();
        return deliver(tx, notify, snapshot);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::testing::{FakeHttp, FakeStore, ok};
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
            state_path: dir.join("data").join("state.json"),
            prices_path: dir.join("data").join("prices.json"),
            interval,
            secrets: Arc::new(FakeStore::new(None)),
            http: Arc::new(FakeHttp::new(vec![])),
            api_interval: crate::analytics::DEFAULT_API_INTERVAL,
        }
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
    fn missing_logs_root_reports_empty_not_crash() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), Duration::from_secs(60));
        assert!(!cfg.logs_root.exists());
        let rx = spawn_worker(cfg, noop());

        let (_, snap) = until_report(&rx);
        assert!(snap.scan_error.is_none());
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
        let second = rx.recv_timeout(WAIT).expect("no API snapshot in time");
        let api = second.api.expect("api status missing");
        assert_eq!((api.today_nano, api.month_nano, api.error), (0, 0, None));
        assert!(second.report.is_some());
        assert_eq!(http.request_count(), 1);
    }
}
