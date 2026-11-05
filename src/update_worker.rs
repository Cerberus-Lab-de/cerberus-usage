//! Background update checker: one thread, commands in, states out.
//!
//! It checks shortly after start and then every 24 h, but only while `check_updates` is on (re-read
//! before each automatic check). "Jetzt prüfen" and "Jetzt aktualisieren" are explicit user actions and
//! always go through. Nothing here ever blocks the UI or touches the Analytics poller.

use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use semver::Version;

use crate::analytics::Http;
use crate::update::{
    CURRENT_VERSION, Release, UpdateError, default_update_dir, download_verified_with_hash,
    latest_release, recheck_installer, run_installer,
};

/// Delay before the first automatic check, so startup stays quick.
pub const START_DELAY: Duration = Duration::from_secs(5);
/// Pause between automatic checks.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// While automatic checks are off, the setting is looked at this often (a tiny file read).
const DISABLED_RECHECK: Duration = Duration::from_secs(60 * 60);

/// Shown when the build carries no update key (until the release process sets one).
pub const NO_KEY_TEXT: &str = "Update-Schlüssel fehlt in dieser Version.";
pub const LAUNCH_BUSY_TEXT: &str = "Obsidian wird gerade gestartet. Update abgebrochen, bitte später erneut versuchen.";
const PANIC_TEXT: &str = "Interner Fehler bei der Update-Prüfung.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate { checked: DateTime<Local> },
    Available(Release),
    Downloading,
    /// The installer was started; the app should quit now.
    Installing,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateCmd {
    /// Check now, even when automatic checks are off.
    CheckNow,
    /// Download, verify and start the installer for the last known release.
    Install,
    /// The settings file changed (the toggle): look at it again now, so switching on does not wait.
    SettingsChanged,
}

type InstallerRunner = Arc<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;

/// Everything the thread needs, injectable for tests.
pub struct UpdateConfig {
    pub http: Arc<dyn Http + Send + Sync>,
    pub settings_path: Option<PathBuf>,
    pub current: Version,
    /// Public release key; `None` refuses every install.
    pub key: Option<[u8; 32]>,
    pub dir: PathBuf,
    pub initial_delay: Duration,
    pub interval: Duration,
    pub run_installer: InstallerRunner,
    /// Set by the UI while Obsidian is being started: the installer is never started then.
    pub launch_busy: Arc<AtomicBool>,
}

pub fn spawn_update_checker(
    http: Arc<dyn Http + Send + Sync>,
    settings_path: Option<PathBuf>,
    launch_busy: Arc<AtomicBool>,
    notify: impl Fn() + Send + 'static,
) -> (Sender<UpdateCmd>, Receiver<UpdateState>) {
    spawn_with(
        UpdateConfig {
            http,
            settings_path,
            current: Version::parse(CURRENT_VERSION).unwrap_or(Version::new(0, 0, 0)),
            key: crate::update_key::RELEASE_PUBLIC_KEY,
            dir: default_update_dir(),
            initial_delay: START_DELAY,
            interval: CHECK_INTERVAL,
            run_installer: Arc::new(|p| run_installer(p)),
            launch_busy,
        },
        notify,
    )
}

pub fn spawn_with(
    cfg: UpdateConfig,
    notify: impl Fn() + Send + 'static,
) -> (Sender<UpdateCmd>, Receiver<UpdateState>) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (state_tx, state_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = catch_unwind(AssertUnwindSafe(|| run(cfg, cmd_rx, state_tx, &notify)));
    });
    (cmd_tx, state_rx)
}

/// Automatic checks need a readable settings file that says yes. A missing file means the default
/// (on); a broken file or no path at all means "do not talk to GitHub on our own".
pub fn automatic_checks_enabled(settings_path: Option<&Path>) -> bool {
    let Some(path) = settings_path else { return false };
    let (settings, error) = crate::settings::load(path);
    error.is_none() && settings.check_updates
}

fn failure_text(e: &UpdateError) -> String {
    match e {
        UpdateError::NoKey => NO_KEY_TEXT.to_string(),
        other => other.to_string(),
    }
}

/// Sends states, dropping a state equal to the last one sent (a release is reported once).
struct Out<'a> {
    tx: &'a Sender<UpdateState>,
    notify: &'a dyn Fn(),
    last: Option<UpdateState>,
    gone: bool,
}

impl Out<'_> {
    fn send(&mut self, state: UpdateState) {
        if self.last.as_ref() == Some(&state) {
            return;
        }
        self.last = Some(state.clone());
        if self.tx.send(state).is_err() {
            self.gone = true;
            return;
        }
        (self.notify)();
    }
}

struct Core {
    cfg: UpdateConfig,
    /// The newest release found, kept for the install command.
    release: Option<Release>,
    /// The installer was started: further installs are ignored.
    installed: bool,
}

impl Core {
    fn check(&mut self, out: &mut Out<'_>, explicit: bool) {
        if explicit {
            out.send(UpdateState::Checking);
        }
        let state = match latest_release(self.cfg.http.as_ref(), &self.cfg.current) {
            Ok(Some(r)) => {
                self.release = Some(r.clone());
                UpdateState::Available(r)
            }
            // No release (or a 404 while the repo is not public yet) is simply "up to date".
            Ok(None) => {
                self.release = None;
                UpdateState::UpToDate { checked: Local::now() }
            }
            Err(e) => UpdateState::Failed(failure_text(&e)),
        };
        out.send(state);
    }

    fn install(&mut self, out: &mut Out<'_>) {
        if self.installed {
            return;
        }
        let Some(release) = self.release.clone() else {
            out.send(UpdateState::Failed("Kein Update bekannt. Bitte zuerst prüfen.".to_string()));
            return;
        };
        if self.cfg.key.is_none() {
            out.send(UpdateState::Failed(NO_KEY_TEXT.to_string()));
            return;
        }
        out.send(UpdateState::Downloading);
        let fetched = download_verified_with_hash(
            self.cfg.http.as_ref(),
            &release,
            &self.cfg.current,
            self.cfg.key,
            &self.cfg.dir,
        );
        let state = match fetched {
            Err(e) => UpdateState::Failed(failure_text(&e)),
            // Hash the file on disk once more, immediately before it is started.
            Ok((path, sha)) => match recheck_installer(&path, &sha) {
                Err(e) => UpdateState::Failed(failure_text(&e)),
                // The window may have started Obsidian meanwhile: never update under a running launch.
                Ok(()) if self.cfg.launch_busy.load(Ordering::SeqCst) => {
                    UpdateState::Failed(LAUNCH_BUSY_TEXT.to_string())
                }
                Ok(()) => match (self.cfg.run_installer)(&path) {
                    Ok(()) => {
                        self.installed = true;
                        UpdateState::Installing
                    }
                    Err(_) => UpdateState::Failed("Der Installer konnte nicht gestartet werden.".to_string()),
                },
            },
        };
        out.send(state);
    }
}

fn run(cfg: UpdateConfig, cmds: Receiver<UpdateCmd>, tx: Sender<UpdateState>, notify: &dyn Fn()) {
    let (initial, interval) = (cfg.initial_delay, cfg.interval);
    let settings_path = cfg.settings_path.clone();
    let mut core = Core { cfg, release: None, installed: false };
    let mut out = Out { tx: &tx, notify, last: None, gone: false };
    let mut due = Instant::now() + initial;
    loop {
        let wait = due.saturating_duration_since(Instant::now());
        // Sleeps until a command arrives, the check is due, or the UI has gone (sender dropped).
        let cmd = match cmds.recv_timeout(wait) {
            Ok(cmd) => Some(cmd),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // An explicit action always gets an answer, even one equal to the last (a repeated failure).
        if matches!(cmd, Some(UpdateCmd::CheckNow | UpdateCmd::Install)) {
            out.last = None;
        }
        let step = catch_unwind(AssertUnwindSafe(|| match cmd {
            Some(UpdateCmd::CheckNow) => {
                core.check(&mut out, true);
                due = Instant::now() + interval;
            }
            Some(UpdateCmd::Install) => core.install(&mut out),
            Some(UpdateCmd::SettingsChanged) => {
                if automatic_checks_enabled(settings_path.as_deref()) {
                    core.check(&mut out, false);
                    due = Instant::now() + interval;
                }
            }
            None if automatic_checks_enabled(settings_path.as_deref()) => {
                core.check(&mut out, false);
                due = Instant::now() + interval;
            }
            None => due = Instant::now() + DISABLED_RECHECK.min(interval),
        }));
        if step.is_err() {
            out.send(UpdateState::Failed(PANIC_TEXT.to_string()));
            due = Instant::now() + interval;
        }
        if out.gone {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::NetError;
    use crate::release_sign::{keypair_from_seed, sign_manifest};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const REPO: &str = "https://github.com/Cerberus-Lab-de/cerberus-usage/";
    const INSTALLER: &[u8] = b"fake installer bytes";

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[derive(Default)]
    struct Fake {
        calls: AtomicUsize,
        fail: AtomicBool,
        explode: AtomicBool,
        responses: Mutex<HashMap<String, (u16, Vec<u8>)>>,
    }

    impl Fake {
        fn put(&self, url: &str, status: u16, body: &[u8]) {
            self.responses.lock().unwrap().insert(url.to_string(), (status, body.to_vec()));
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Http for Fake {
        fn get(&self, _u: &str, _h: &[(&str, &str)]) -> Result<(u16, String), NetError> {
            unreachable!("updates use get_bytes")
        }
        fn get_bytes(&self, url: &str, _h: &[(&str, &str)], _max: usize) -> Result<(u16, Vec<u8>), NetError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.explode.load(Ordering::SeqCst) {
                panic!("fake transport exploded");
            }
            if self.fail.load(Ordering::SeqCst) {
                return Err(NetError("Server nicht erreichbar".into()));
            }
            match self.responses.lock().unwrap().get(url) {
                Some((s, b)) => Ok((*s, b.clone())),
                None => Err(NetError("Server nicht erreichbar".into())),
            }
        }
    }

    fn name(ver: &str) -> String {
        format!("CerberusUsage-Setup-{ver}.exe")
    }

    /// Serves release `ver` (latest + manifest + installer), signed with `seed`'s key.
    fn serve_release(http: &Fake, ver: &str, seed: u8) {
        let dl = format!("{REPO}releases/download/v{ver}/");
        let (exe, man) = (name(ver), format!("{}.manifest.json", name(ver)));
        let body = serde_json::json!({
            "tag_name": format!("v{ver}"), "html_url": format!("{REPO}releases/tag/v{ver}"),
            "draft": false, "prerelease": false,
            "assets": [
                {"name": exe, "browser_download_url": format!("{dl}{exe}")},
                {"name": man, "browser_download_url": format!("{dl}{man}")},
            ],
        });
        http.put(crate::update::LATEST_URL, 200, &serde_json::to_vec(&body).unwrap());
        let manifest = sign_manifest(&keypair_from_seed([seed; 32]), &v(ver), INSTALLER, &exe);
        http.put(&format!("{dl}{man}"), 200, &serde_json::to_vec(&manifest).unwrap());
        http.put(&format!("{dl}{exe}"), 200, INSTALLER);
    }

    struct Rig {
        http: Arc<Fake>,
        cmd: Sender<UpdateCmd>,
        rx: Receiver<UpdateState>,
        started: Arc<Mutex<Vec<PathBuf>>>,
        busy: Arc<AtomicBool>,
        dir: tempfile::TempDir,
    }

    fn rig(settings_json: Option<&str>, delay_ms: u64, interval_ms: u64, key_seed: Option<u8>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        if let Some(json) = settings_json {
            std::fs::write(&settings, json).unwrap();
        }
        let http = Arc::new(Fake::default());
        http.put(crate::update::LATEST_URL, 404, b"{}");
        let started: Arc<Mutex<Vec<PathBuf>>> = Default::default();
        let log = Arc::clone(&started);
        let busy = Arc::new(AtomicBool::new(false));
        let cfg = UpdateConfig {
            http: http.clone(),
            settings_path: Some(settings),
            current: v("1.0.0"),
            key: key_seed.map(|s| keypair_from_seed([s; 32]).verifying_key().to_bytes()),
            dir: dir.path().join("dl"),
            initial_delay: Duration::from_millis(delay_ms),
            interval: Duration::from_millis(interval_ms),
            run_installer: Arc::new(move |p| {
                log.lock().unwrap().push(p.to_path_buf());
                Ok(())
            }),
            launch_busy: Arc::clone(&busy),
        };
        let (cmd, rx) = spawn_with(cfg, || {});
        Rig { http, cmd, rx, started, busy, dir }
    }

    fn next(rx: &Receiver<UpdateState>) -> UpdateState {
        rx.recv_timeout(Duration::from_secs(5)).expect("a state within 5 s")
    }

    fn drain(rx: &Receiver<UpdateState>) -> Vec<UpdateState> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn disabled_setting_makes_no_request() {
        let r = rig(Some(r#"{"check_updates": false}"#), 5, 10, None);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(r.http.calls(), 0);
        assert!(drain(&r.rx).is_empty());
        // An explicit click still goes out (and a 404 is "up to date").
        r.cmd.send(UpdateCmd::CheckNow).unwrap();
        assert_eq!(next(&r.rx), UpdateState::Checking);
        assert!(matches!(next(&r.rx), UpdateState::UpToDate { .. }));
        assert_eq!(r.http.calls(), 1);
    }

    #[test]
    fn broken_or_unknown_settings_make_no_automatic_request() {
        let r = rig(Some("{ broken"), 5, 10, None);
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(r.http.calls(), 0);
        assert!(!automatic_checks_enabled(None));
    }

    #[test]
    fn missing_settings_file_checks_by_default() {
        let r = rig(None, 5, 100_000, None);
        assert!(matches!(next(&r.rx), UpdateState::UpToDate { .. }));
        assert_eq!(r.http.calls(), 1);
    }

    #[test]
    fn network_failure_reports_failed_quietly_and_check_now_retries() {
        let r = rig(None, 100, 100_000, None);
        r.http.fail.store(true, Ordering::SeqCst);
        // The automatic check fails without a Checking state first.
        match next(&r.rx) {
            UpdateState::Failed(m) => assert!(m.contains("Netzwerkfehler"), "{m}"),
            other => panic!("{other:?}"),
        }
        r.http.fail.store(false, Ordering::SeqCst);
        serve_release(&r.http, "1.0.1", 1);
        r.cmd.send(UpdateCmd::CheckNow).unwrap();
        assert_eq!(next(&r.rx), UpdateState::Checking);
        assert!(matches!(next(&r.rx), UpdateState::Available(rel) if rel.version == v("1.0.1")));
    }

    #[test]
    fn rate_limit_is_failed_and_404_is_up_to_date() {
        let r = rig(None, 5, 100_000, None);
        assert!(matches!(next(&r.rx), UpdateState::UpToDate { .. }));
        r.http.put(crate::update::LATEST_URL, 403, b"{}");
        r.cmd.send(UpdateCmd::CheckNow).unwrap();
        assert_eq!(next(&r.rx), UpdateState::Checking);
        assert!(matches!(next(&r.rx), UpdateState::Failed(m) if m.contains("GitHub-Limit")));
    }

    #[test]
    fn available_release_is_reported_once() {
        let r = rig(None, 150, 20, None);
        serve_release(&r.http, "1.0.1", 1);
        std::thread::sleep(Duration::from_millis(500));
        assert!(r.http.calls() >= 3, "several automatic checks ran: {}", r.http.calls());
        let available = drain(&r.rx).into_iter().filter(|s| matches!(s, UpdateState::Available(_))).count();
        assert_eq!(available, 1);
    }

    #[test]
    fn a_panic_ends_in_failed_and_the_thread_keeps_serving() {
        let r = rig(Some(r#"{"check_updates": false}"#), 100_000, 100_000, None);
        r.http.explode.store(true, Ordering::SeqCst);
        r.cmd.send(UpdateCmd::CheckNow).unwrap();
        assert_eq!(next(&r.rx), UpdateState::Checking);
        assert!(matches!(next(&r.rx), UpdateState::Failed(_)));
        r.http.explode.store(false, Ordering::SeqCst);
        r.cmd.send(UpdateCmd::CheckNow).unwrap();
        assert_eq!(next(&r.rx), UpdateState::Checking);
        assert!(matches!(next(&r.rx), UpdateState::UpToDate { .. }));
    }

    #[test]
    fn dropping_the_command_sender_stops_the_thread() {
        let r = rig(None, 100_000, 100_000, None);
        drop(r.cmd);
        assert!(matches!(r.rx.recv_timeout(Duration::from_secs(5)), Err(RecvTimeoutError::Disconnected)));
    }

    fn available_rig(key_seed: Option<u8>, sign_seed: u8) -> Rig {
        let r = rig(Some(r#"{"check_updates": false}"#), 100_000, 100_000, key_seed);
        serve_release(&r.http, "1.0.1", sign_seed);
        r.cmd.send(UpdateCmd::CheckNow).unwrap();
        assert_eq!(next(&r.rx), UpdateState::Checking);
        assert!(matches!(next(&r.rx), UpdateState::Available(_)));
        r
    }

    #[test]
    fn install_downloads_verifies_and_starts_the_installer() {
        let r = available_rig(Some(1), 1);
        r.cmd.send(UpdateCmd::Install).unwrap();
        assert_eq!(next(&r.rx), UpdateState::Downloading);
        assert_eq!(next(&r.rx), UpdateState::Installing);
        let started = r.started.lock().unwrap().clone();
        assert_eq!(started, vec![r.dir.path().join("dl").join(name("1.0.1"))]);
        assert_eq!(std::fs::read(&started[0]).unwrap(), INSTALLER);
    }

    #[test]
    fn install_without_a_compiled_key_says_so_and_downloads_nothing() {
        let r = available_rig(None, 1);
        let before = r.http.calls();
        r.cmd.send(UpdateCmd::Install).unwrap();
        assert_eq!(next(&r.rx), UpdateState::Failed(NO_KEY_TEXT.to_string()));
        assert_eq!(r.http.calls(), before);
        assert!(r.started.lock().unwrap().is_empty());
        assert!(!r.dir.path().join("dl").exists());
    }

    #[test]
    fn a_repeated_failure_is_reported_again() {
        let r = available_rig(None, 1);
        for _ in 0..2 {
            r.cmd.send(UpdateCmd::Install).unwrap();
            assert_eq!(next(&r.rx), UpdateState::Failed(NO_KEY_TEXT.to_string()));
        }
        // Same for "no release known" and a repeated failed check.
        let r = rig(Some(r#"{"check_updates": false}"#), 100_000, 100_000, Some(1));
        for _ in 0..2 {
            r.cmd.send(UpdateCmd::Install).unwrap();
            assert!(matches!(next(&r.rx), UpdateState::Failed(_)));
        }
        r.http.fail.store(true, Ordering::SeqCst);
        for _ in 0..2 {
            r.cmd.send(UpdateCmd::CheckNow).unwrap();
            assert_eq!(next(&r.rx), UpdateState::Checking);
            assert!(matches!(next(&r.rx), UpdateState::Failed(_)));
        }
    }

    #[test]
    fn only_one_install_runs() {
        let r = available_rig(Some(1), 1);
        r.cmd.send(UpdateCmd::Install).unwrap();
        r.cmd.send(UpdateCmd::Install).unwrap();
        assert_eq!(next(&r.rx), UpdateState::Downloading);
        assert_eq!(next(&r.rx), UpdateState::Installing);
        std::thread::sleep(Duration::from_millis(200));
        assert!(drain(&r.rx).is_empty());
        assert_eq!(r.started.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_launch_that_started_meanwhile_stops_the_installer() {
        let r = available_rig(Some(1), 1);
        r.busy.store(true, Ordering::SeqCst);
        r.cmd.send(UpdateCmd::Install).unwrap();
        assert_eq!(next(&r.rx), UpdateState::Downloading);
        assert_eq!(next(&r.rx), UpdateState::Failed(LAUNCH_BUSY_TEXT.to_string()));
        assert!(r.started.lock().unwrap().is_empty());
        // Once the launch is over a retry goes through.
        r.busy.store(false, Ordering::SeqCst);
        r.cmd.send(UpdateCmd::Install).unwrap();
        assert_eq!(next(&r.rx), UpdateState::Downloading);
        assert_eq!(next(&r.rx), UpdateState::Installing);
    }

    #[test]
    fn switching_the_toggle_on_checks_soon_and_off_stays_quiet() {
        let r = rig(Some(r#"{"check_updates": false}"#), 100_000, 100_000, None);
        r.cmd.send(UpdateCmd::SettingsChanged).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(r.http.calls(), 0);
        std::fs::write(r.dir.path().join("settings.json"), r#"{"check_updates": true}"#).unwrap();
        r.cmd.send(UpdateCmd::SettingsChanged).unwrap();
        assert!(matches!(next(&r.rx), UpdateState::UpToDate { .. }));
        assert_eq!(r.http.calls(), 1);
    }

    #[test]
    fn install_with_a_bad_signature_runs_nothing() {
        let r = available_rig(Some(1), 2);
        r.cmd.send(UpdateCmd::Install).unwrap();
        assert_eq!(next(&r.rx), UpdateState::Downloading);
        assert!(matches!(next(&r.rx), UpdateState::Failed(_)));
        assert!(r.started.lock().unwrap().is_empty());
        assert!(!r.dir.path().join("dl").exists());
    }

    #[test]
    fn install_without_a_known_release_is_refused() {
        let r = rig(Some(r#"{"check_updates": false}"#), 100_000, 100_000, Some(1));
        r.cmd.send(UpdateCmd::Install).unwrap();
        assert!(matches!(next(&r.rx), UpdateState::Failed(_)));
        assert_eq!(r.http.calls(), 0);
    }
}
