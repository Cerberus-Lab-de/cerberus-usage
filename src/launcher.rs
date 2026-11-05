//! Starts Obsidian either with the Max login (no API key in the environment)
//! or with `ANTHROPIC_API_KEY` set for the child process only.
//!
//! Safety rules, carried over from the tested batch launcher:
//! - never start while Obsidian still runs (a new instance would just focus the old one and
//!   the key would be silently ignored),
//! - never force-close (no `/f`),
//! - the key is set on the child `Command` only, never on this process.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::paths::{DEFAULT_IMAGE_NAME, PathError, image_name, real_env, resolve_obsidian};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Max,
    ApiKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchOutcome {
    Started,
    /// Obsidian runs and the user has not agreed to close it yet.
    NeedsConfirmation,
    /// Obsidian did not exit within the wait time; nothing was started.
    CloseTimedOut,
    MissingKey,
    /// The process could not be started.
    ObsidianNotFound,
    /// The Obsidian executable could not be resolved from the settings or the automatic search.
    /// Nothing was closed or started.
    ObsidianUnavailable(PathError),
}

/// OS access, abstracted so the launch logic is testable without touching real processes.
pub trait System {
    /// Whether a process with this image name (e.g. `Obsidian.exe`) runs.
    fn obsidian_running(&self, image: &str) -> bool;
    /// Polite close request (no force).
    fn request_close(&self, image: &str);
    /// Start `exe` without waiting. `Some` sets `ANTHROPIC_API_KEY` for the child, `None` removes it.
    fn spawn(&self, exe: &Path, api_key: Option<&str>) -> io::Result<()>;
    /// Finds the Obsidian executable from the current settings (called at every launch).
    fn resolve_obsidian(&self) -> Result<PathBuf, PathError>;
    fn sleep(&self, d: Duration);
}

const CLOSE_POLLS: u32 = 20;
const CLOSE_POLL_INTERVAL: Duration = Duration::from_secs(1);

pub fn launch(
    sys: &dyn System,
    mode: Mode,
    key: Option<&str>,
    confirmed_close: bool,
) -> LaunchOutcome {
    // Resolve first: an unusable setting must never lead to closing a running Obsidian.
    let exe = match sys.resolve_obsidian() {
        Ok(exe) => exe,
        Err(e) => return LaunchOutcome::ObsidianUnavailable(e),
    };
    let image = image_name(&exe);
    // Resolve the key first so a missing key never leads to closing Obsidian.
    let api_key = match mode {
        Mode::Max => None,
        Mode::ApiKey => match key.filter(|k| !k.is_empty()) {
            Some(k) => Some(k),
            None => return LaunchOutcome::MissingKey,
        },
    };

    if sys.obsidian_running(&image) {
        if !confirmed_close {
            return LaunchOutcome::NeedsConfirmation;
        }
        sys.request_close(&image);
        let mut closed = false;
        for _ in 0..CLOSE_POLLS {
            sys.sleep(CLOSE_POLL_INTERVAL);
            if !sys.obsidian_running(&image) {
                closed = true;
                break;
            }
        }
        if !closed {
            return LaunchOutcome::CloseTimedOut;
        }
    }

    match sys.spawn(&exe, api_key) {
        Ok(()) => LaunchOutcome::Started,
        Err(_) => LaunchOutcome::ObsidianNotFound,
    }
}

/// Real implementation using fixed-path Windows tools (Git Bash on PATH would shadow e.g. `find`).
pub struct WindowsSystem {
    /// `settings.json`, read at every call so a changed Obsidian path applies without a restart.
    pub settings_path: Option<PathBuf>,
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

impl WindowsSystem {
    fn system32_tool(name: &str) -> PathBuf {
        let root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        root.join("System32").join(name)
    }

    fn configured_path(&self) -> Option<PathBuf> {
        let path = self.settings_path.as_ref()?;
        // A broken settings file yields defaults here (automatic search); the window reports the problem.
        crate::settings::load(path).0.obsidian_path
    }

    /// Image name for process checks: from the resolved exe, else the default name.
    pub fn current_image_name(&self) -> String {
        self.resolve_obsidian().map(|p| image_name(&p)).unwrap_or_else(|_| DEFAULT_IMAGE_NAME.to_string())
    }
}

/// True if a `tasklist /nh` line starts with exactly this image name (case-insensitive), so
/// `Obsidian.exe` does not match `NotObsidian.exe`.
fn lists_image(tasklist_stdout: &str, image: &str) -> bool {
    let image = image.to_lowercase();
    tasklist_stdout.lines().any(|line| {
        let line = line.trim_start().to_lowercase();
        line.strip_prefix(&image).is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    })
}

#[cfg(windows)]
impl System for WindowsSystem {
    fn obsidian_running(&self, image: &str) -> bool {
        use std::os::windows::process::CommandExt;
        Command::new(Self::system32_tool("tasklist.exe"))
            .args(["/fi", &format!("imagename eq {image}"), "/nh"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|o| lists_image(&String::from_utf8_lossy(&o.stdout), image))
            .unwrap_or(false)
    }

    fn request_close(&self, image: &str) {
        use std::os::windows::process::CommandExt;
        // No /f: Obsidian gets the chance to save and exit on its own.
        let _ = Command::new(Self::system32_tool("taskkill.exe"))
            .args(["/im", image])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    fn spawn(&self, exe: &Path, api_key: Option<&str>) -> io::Result<()> {
        use std::os::windows::process::CommandExt;
        let mut cmd = Command::new(exe);
        // Set/removed on the child only; this process's environment stays untouched.
        match api_key {
            Some(k) => {
                cmd.env("ANTHROPIC_API_KEY", k);
            }
            None => {
                cmd.env_remove("ANTHROPIC_API_KEY");
            }
        }
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Dropping the child handle does not kill it; we do not wait for Obsidian.
        cmd.spawn().map(drop)
    }

    fn resolve_obsidian(&self) -> Result<PathBuf, PathError> {
        resolve_obsidian(self.configured_path().as_deref(), &real_env, &|p| p.is_file())
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// Records calls. `running_polls` = how many `obsidian_running` calls still report true
    /// (u32::MAX = never exits).
    struct FakeSystem {
        resolve: Result<PathBuf, PathError>,
        /// Image names passed to `obsidian_running` / `request_close`.
        images: RefCell<Vec<String>>,
        spawned: RefCell<Vec<PathBuf>>,
        running_polls: Cell<u32>,
        calls: RefCell<Vec<String>>,
        slept: Cell<u32>,
    }

    impl FakeSystem {
        fn new(exists: bool, running_polls: u32) -> Self {
            Self {
                resolve: if exists {
                    Ok(PathBuf::from(r"C:\Obsidian\Obsidian.exe"))
                } else {
                    Err(PathError::ObsidianNotFound { searched: vec![] })
                },
                images: RefCell::new(Vec::new()),
                spawned: RefCell::new(Vec::new()),
                running_polls: Cell::new(running_polls),
                calls: RefCell::new(Vec::new()),
                slept: Cell::new(0),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
        fn count(&self, prefix: &str) -> usize {
            self.calls.borrow().iter().filter(|c| c.starts_with(prefix)).count()
        }
    }

    impl System for FakeSystem {
        fn obsidian_running(&self, image: &str) -> bool {
            self.images.borrow_mut().push(image.to_string());
            let n = self.running_polls.get();
            if n == 0 {
                return false;
            }
            if n != u32::MAX {
                self.running_polls.set(n - 1);
            }
            true
        }
        fn request_close(&self, image: &str) {
            self.images.borrow_mut().push(image.to_string());
            self.calls.borrow_mut().push("close".into());
        }
        fn spawn(&self, exe: &Path, api_key: Option<&str>) -> io::Result<()> {
            self.spawned.borrow_mut().push(exe.to_path_buf());
            self.calls.borrow_mut().push(format!("spawn:{api_key:?}"));
            Ok(())
        }
        fn resolve_obsidian(&self) -> Result<PathBuf, PathError> {
            self.resolve.clone()
        }
        fn sleep(&self, _d: Duration) {
            self.slept.set(self.slept.get() + 1);
        }
    }

    const FAKE_KEY: &str = "sk-ant-test-NICHTECHT-1234";

    #[test]
    fn not_running_spawns_directly() {
        let sys = FakeSystem::new(true, 0);
        assert_eq!(launch(&sys, Mode::Max, None, false), LaunchOutcome::Started);
        assert_eq!(sys.calls(), vec!["spawn:None"]);
    }

    #[test]
    fn running_without_confirmation_does_not_close_or_spawn() {
        let sys = FakeSystem::new(true, u32::MAX);
        let out = launch(&sys, Mode::Max, None, false);
        assert_eq!(out, LaunchOutcome::NeedsConfirmation);
        assert_eq!(sys.count("close"), 0);
        assert_eq!(sys.count("spawn"), 0);
    }

    #[test]
    fn confirmed_closes_then_spawns() {
        // Running at the first check, still running after the close request once, then gone.
        let sys = FakeSystem::new(true, 3);
        let out = launch(&sys, Mode::Max, None, true);
        assert_eq!(out, LaunchOutcome::Started);
        assert_eq!(sys.calls(), vec!["close", "spawn:None"]);
        assert!(sys.slept.get() >= 1);
    }

    #[test]
    fn close_timeout_never_spawns() {
        let sys = FakeSystem::new(true, u32::MAX);
        let out = launch(&sys, Mode::Max, None, true);
        assert_eq!(out, LaunchOutcome::CloseTimedOut);
        assert_eq!(sys.count("close"), 1);
        assert_eq!(sys.count("spawn"), 0);
        assert_eq!(sys.slept.get(), 20, "polls once per second, 20 times");
    }

    #[test]
    fn apikey_mode_without_key_is_missing_key() {
        let sys = FakeSystem::new(true, 0);
        assert_eq!(launch(&sys, Mode::ApiKey, None, false), LaunchOutcome::MissingKey);
        assert_eq!(launch(&sys, Mode::ApiKey, Some(""), false), LaunchOutcome::MissingKey);
        assert!(sys.calls().is_empty());
    }

    #[test]
    fn missing_key_never_closes_running_obsidian() {
        let sys = FakeSystem::new(true, u32::MAX);
        assert_eq!(launch(&sys, Mode::ApiKey, None, true), LaunchOutcome::MissingKey);
        assert!(sys.calls().is_empty());
    }

    #[test]
    fn max_mode_spawns_without_key_even_if_key_given() {
        let sys = FakeSystem::new(true, 0);
        let out = launch(&sys, Mode::Max, Some(FAKE_KEY), false);
        assert_eq!(out, LaunchOutcome::Started);
        assert_eq!(sys.calls(), vec!["spawn:None"]);
    }

    #[test]
    fn apikey_mode_passes_key_to_spawn() {
        let sys = FakeSystem::new(true, 0);
        let out = launch(&sys, Mode::ApiKey, Some(FAKE_KEY), false);
        assert_eq!(out, LaunchOutcome::Started);
        assert_eq!(sys.calls(), vec![format!("spawn:Some({FAKE_KEY:?})")]);
    }

    #[test]
    fn missing_exe_reports_not_found() {
        let sys = FakeSystem::new(false, 0);
        assert!(matches!(
            launch(&sys, Mode::Max, None, true),
            LaunchOutcome::ObsidianUnavailable(PathError::ObsidianNotFound { .. })
        ));
        assert!(sys.calls().is_empty());
    }

    #[test]
    fn unresolved_obsidian_is_reported_before_anything_is_closed() {
        let mut sys = FakeSystem::new(true, u32::MAX);
        sys.resolve = Err(PathError::ConfiguredMissing(PathBuf::from(r"D:\gone\Obsidian.exe")));
        let out = launch(&sys, Mode::Max, None, true);
        assert_eq!(out, LaunchOutcome::ObsidianUnavailable(PathError::ConfiguredMissing(PathBuf::from(r"D:\gone\Obsidian.exe"))));
        assert!(sys.calls().is_empty(), "nothing may be closed or started: {:?}", sys.calls());
    }

    #[test]
    fn renamed_exe_is_used_for_close_and_detection() {
        let mut sys = FakeSystem::new(true, 2);
        sys.resolve = Ok(PathBuf::from(r"C:\Tools\MyObs.exe"));
        assert_eq!(launch(&sys, Mode::Max, None, true), LaunchOutcome::Started);
        assert_eq!(sys.calls(), vec!["close", "spawn:None"]);
        assert!(sys.images.borrow().iter().all(|i| i == "MyObs.exe"), "{:?}", sys.images.borrow());
        assert_eq!(*sys.spawned.borrow(), vec![PathBuf::from(r"C:\Tools\MyObs.exe")]);
    }

    #[test]
    fn tasklist_output_matching_is_case_insensitive() {
        let o = |s: &str| lists_image(s, "Obsidian.exe");
        assert!(o("Obsidian.exe                  1234 Console   1   150.000 K"));
        assert!(o("OBSIDIAN.EXE 1"));
        assert!(o("\r\nobsidian.exe 1\r\n"));
        assert!(!o("INFO: No tasks are running which match the specified criteria."));
        assert!(!o("INFO: Es werden keine Aufgaben ausgeführt, die den angegebenen Kriterien entsprechen."));
        assert!(!o(""));
        // Only the exact image name counts, in either direction.
        assert!(!o("NotObsidian.exe 12 Console 1 10 K"));
        assert!(!o("Obsidian.exe.bak 12 Console 1 10 K"));
        assert!(lists_image("MyObs.exe 5 Console 1 10 K", "myobs.exe"));
    }

    #[test]
    fn obsidian_exists_is_harmless_on_real_system() {
        // Only checks a file path; never starts or stops anything.
        let _ = WindowsSystem { settings_path: None }.resolve_obsidian();
    }
}
