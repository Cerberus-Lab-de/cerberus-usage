//! Starts Obsidian either with the Max login (no API key in the environment)
//! or with `ANTHROPIC_API_KEY` set for the child process only.
//!
//! Safety rules, carried over from the tested batch launcher:
//! - never start while Obsidian still runs (a new instance would just focus the old one and
//!   the key would be silently ignored),
//! - never force-close (no `/f`),
//! - the key is set on the child `Command` only, never on this process.

use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Max,
    ApiKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchOutcome {
    Started,
    /// Obsidian runs and the user has not agreed to close it yet.
    NeedsConfirmation,
    /// Obsidian did not exit within the wait time; nothing was started.
    CloseTimedOut,
    MissingKey,
    /// Executable missing (or the process could not be started).
    ObsidianNotFound,
}

/// OS access, abstracted so the launch logic is testable without touching real processes.
pub trait System {
    fn obsidian_running(&self) -> bool;
    /// Polite close request (no force).
    fn request_close(&self);
    /// Start Obsidian without waiting. `Some` sets `ANTHROPIC_API_KEY` for the child, `None` removes it.
    fn spawn(&self, api_key: Option<&str>) -> io::Result<()>;
    fn obsidian_exists(&self) -> bool;
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
    if !sys.obsidian_exists() {
        return LaunchOutcome::ObsidianNotFound;
    }
    // Resolve the key first so a missing key never leads to closing Obsidian.
    let api_key = match mode {
        Mode::Max => None,
        Mode::ApiKey => match key.filter(|k| !k.is_empty()) {
            Some(k) => Some(k),
            None => return LaunchOutcome::MissingKey,
        },
    };

    if sys.obsidian_running() {
        if !confirmed_close {
            return LaunchOutcome::NeedsConfirmation;
        }
        sys.request_close();
        let mut closed = false;
        for _ in 0..CLOSE_POLLS {
            sys.sleep(CLOSE_POLL_INTERVAL);
            if !sys.obsidian_running() {
                closed = true;
                break;
            }
        }
        if !closed {
            return LaunchOutcome::CloseTimedOut;
        }
    }

    match sys.spawn(api_key) {
        Ok(()) => LaunchOutcome::Started,
        Err(_) => LaunchOutcome::ObsidianNotFound,
    }
}

/// Real implementation using fixed-path Windows tools (Git Bash on PATH would shadow e.g. `find`).
pub struct WindowsSystem;

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

    fn obsidian_path() -> Option<PathBuf> {
        let base = std::env::var_os("LOCALAPPDATA")?;
        Some(PathBuf::from(base).join(r"Programs\Obsidian\Obsidian.exe"))
    }
}

/// True if `tasklist` output lists the Obsidian process (case-insensitive).
fn lists_obsidian(tasklist_stdout: &str) -> bool {
    tasklist_stdout.to_ascii_lowercase().contains("obsidian.exe")
}

#[cfg(windows)]
impl System for WindowsSystem {
    fn obsidian_running(&self) -> bool {
        use std::os::windows::process::CommandExt;
        Command::new(Self::system32_tool("tasklist.exe"))
            .args(["/fi", "imagename eq Obsidian.exe", "/nh"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|o| lists_obsidian(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or(false)
    }

    fn request_close(&self) {
        use std::os::windows::process::CommandExt;
        // No /f: Obsidian gets the chance to save and exit on its own.
        let _ = Command::new(Self::system32_tool("taskkill.exe"))
            .args(["/im", "Obsidian.exe"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    fn spawn(&self, api_key: Option<&str>) -> io::Result<()> {
        use std::os::windows::process::CommandExt;
        let path = Self::obsidian_path()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA not set"))?;
        let mut cmd = Command::new(path);
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

    fn obsidian_exists(&self) -> bool {
        Self::obsidian_path().is_some_and(|p| p.is_file())
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
        exists: bool,
        running_polls: Cell<u32>,
        calls: RefCell<Vec<String>>,
        slept: Cell<u32>,
    }

    impl FakeSystem {
        fn new(exists: bool, running_polls: u32) -> Self {
            Self {
                exists,
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
        fn obsidian_running(&self) -> bool {
            let n = self.running_polls.get();
            if n == 0 {
                return false;
            }
            if n != u32::MAX {
                self.running_polls.set(n - 1);
            }
            true
        }
        fn request_close(&self) {
            self.calls.borrow_mut().push("close".into());
        }
        fn spawn(&self, api_key: Option<&str>) -> io::Result<()> {
            self.calls.borrow_mut().push(format!("spawn:{api_key:?}"));
            Ok(())
        }
        fn obsidian_exists(&self) -> bool {
            self.exists
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
        assert_eq!(launch(&sys, Mode::Max, None, true), LaunchOutcome::ObsidianNotFound);
        assert!(sys.calls().is_empty());
    }

    #[test]
    fn tasklist_output_matching_is_case_insensitive() {
        assert!(lists_obsidian("Obsidian.exe                  1234 Console   1   150.000 K"));
        assert!(lists_obsidian("OBSIDIAN.EXE 1"));
        assert!(!lists_obsidian("INFO: No tasks are running which match the specified criteria."));
        assert!(!lists_obsidian(
            "INFO: Es werden keine Aufgaben ausgeführt, die den angegebenen Kriterien entsprechen."
        ));
        assert!(!lists_obsidian(""));
    }

    #[test]
    fn obsidian_exists_is_harmless_on_real_system() {
        // Only checks a file path; never starts or stops anything.
        let _ = WindowsSystem.obsidian_exists();
    }
}
