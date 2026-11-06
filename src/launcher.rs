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

use crate::paths::{DEFAULT_IMAGE_NAME, PathError, checked_image_name, is_safe_image, real_env, resolve_obsidian};

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
    /// The process check itself failed (tool blocked, error exit). Nothing was closed or started,
    /// because "unknown" must never be treated as "not running".
    ProcessCheckFailed,
    MissingKey,
    /// The process could not be started.
    ObsidianNotFound,
    /// The Obsidian executable could not be resolved from the settings or the automatic search.
    /// Nothing was closed or started.
    ObsidianUnavailable(PathError),
}

/// OS access, abstracted so the launch logic is testable without touching real processes.
pub trait System {
    /// Whether a process with this image name (e.g. `Obsidian.exe`) runs. `Err` means the check
    /// itself failed; callers must fail closed.
    fn obsidian_running(&self, image: &str) -> io::Result<bool>;
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
    let image = match checked_image_name(&exe) {
        Ok(n) => n,
        Err(e) => return LaunchOutcome::ObsidianUnavailable(e),
    };
    // Resolve the key first so a missing key never leads to closing Obsidian.
    let api_key = match mode {
        Mode::Max => None,
        Mode::ApiKey => match key.filter(|k| !k.is_empty()) {
            Some(k) => Some(k),
            None => return LaunchOutcome::MissingKey,
        },
    };

    let running = match sys.obsidian_running(&image) {
        Ok(r) => r,
        Err(_) => return LaunchOutcome::ProcessCheckFailed,
    };
    if running {
        if !confirmed_close {
            return LaunchOutcome::NeedsConfirmation;
        }
        sys.request_close(&image);
        let mut closed = false;
        for _ in 0..CLOSE_POLLS {
            sys.sleep(CLOSE_POLL_INTERVAL);
            // An unknown answer counts as "still running", so it ends in the timeout, not in a start.
            if matches!(sys.obsidian_running(&image), Ok(false)) {
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

/// One change to the child's environment.
#[derive(Clone, PartialEq, Eq)]
pub enum EnvOp {
    Set(&'static str, String),
    Remove(&'static str),
}

// Manual Debug: the value of `Set` is the API key and must never show up in any output.
impl std::fmt::Debug for EnvOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvOp::Set(name, _) => write!(f, "Set({name}, <redacted>)"),
            EnvOp::Remove(name) => write!(f, "Remove({name})"),
        }
    }
}

/// The environment changes for the Obsidian child. Both modes remove `ANTHROPIC_AUTH_TOKEN`,
/// which would otherwise beat every other credential; Max also removes `ANTHROPIC_API_KEY`.
pub fn child_env_ops(mode: Mode, key: Option<&str>) -> Vec<EnvOp> {
    let mut ops = Vec::new();
    match (mode, key) {
        (Mode::ApiKey, Some(k)) => ops.push(EnvOp::Set("ANTHROPIC_API_KEY", k.to_string())),
        _ => ops.push(EnvOp::Remove("ANTHROPIC_API_KEY")),
    }
    ops.push(EnvOp::Remove("ANTHROPIC_AUTH_TOKEN"));
    ops
}

/// Variables that reroute traffic or billing. They are left untouched (a deliberate setup),
/// but the window names them.
pub const ROUTING_ENV_VARS: [&str; 4] = [
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// Names (never values) of the routing variables that are set to a non-empty value.
pub fn routing_env_names(env: &dyn Fn(&str) -> Option<String>) -> Vec<&'static str> {
    ROUTING_ENV_VARS
        .iter()
        .copied()
        .filter(|n| env(n).is_some_and(|v| !v.trim().is_empty()))
        .collect()
}

/// Maps a finished `tasklist` run to an answer. Exit 0 with the "no tasks" INFORMATION text
/// is "not running"; any non-zero status is a failed check.
fn tasklist_answer(success: bool, stdout: &str, image: &str) -> io::Result<bool> {
    if !success {
        return Err(io::Error::other("tasklist failed"));
    }
    Ok(lists_image(stdout, image))
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

    /// Image name for process checks: from the resolved exe, else the default name. `None` when the
    /// answer is unknown (unreadable settings file, unusable file name): the caller keeps its last state.
    pub fn current_image_name(&self) -> Option<String> {
        match self.resolve_obsidian() {
            Ok(p) => checked_image_name(&p).ok(),
            Err(PathError::SettingsUnreadable(_)) => None,
            Err(_) => Some(DEFAULT_IMAGE_NAME.to_string()),
        }
    }
}

/// The command that starts Obsidian: environment changes on the child only (this process's
/// environment stays untouched), and the working directory is Obsidian's own folder, so the child
/// does not hold our install folder (`{app}` after a self-update) open.
fn obsidian_command(exe: &Path, api_key: Option<&str>) -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new(exe);
    let mode = if api_key.is_some() { Mode::ApiKey } else { Mode::Max };
    for op in child_env_ops(mode, api_key) {
        match op {
            EnvOp::Set(name, value) => {
                cmd.env(name, value);
            }
            EnvOp::Remove(name) => {
                cmd.env_remove(name);
            }
        }
    }
    // A path without a parent keeps the old behaviour (inherit the working directory).
    if let Some(dir) = exe.parent().filter(|d| !d.as_os_str().is_empty()) {
        cmd.current_dir(dir);
    }
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// True if a `tasklist /fo csv /nh` line has exactly this image name as its first quoted field
/// (case-insensitive, whole name). `Obsidian.exe` does not match `NotObsidian.exe`, and names
/// longer than 25 characters are compared in full.
fn lists_image(tasklist_stdout: &str, image: &str) -> bool {
    let image = image.to_lowercase();
    tasklist_stdout.lines().any(|line| {
        line.trim()
            .strip_prefix('"')
            .and_then(|rest| rest.split('"').next())
            .is_some_and(|first| first.to_lowercase() == image)
    })
}

#[cfg(windows)]
impl System for WindowsSystem {
    fn obsidian_running(&self, image: &str) -> io::Result<bool> {
        use std::os::windows::process::CommandExt;
        if !is_safe_image(image) {
            return Err(io::Error::other("unsafe image name"));
        }
        let out = Command::new(Self::system32_tool("tasklist.exe"))
            .args(["/fi", &format!("imagename eq {image}"), "/fo", "csv", "/nh"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?;
        tasklist_answer(out.status.success(), &String::from_utf8_lossy(&out.stdout), image)
    }

    fn request_close(&self, image: &str) {
        use std::os::windows::process::CommandExt;
        if !is_safe_image(image) {
            return;
        }
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
        // Dropping the child handle does not kill it; we do not wait for Obsidian.
        obsidian_command(exe, api_key).spawn().map(drop)
    }

    fn resolve_obsidian(&self) -> Result<PathBuf, PathError> {
        let mut configured = None;
        if let Some(path) = &self.settings_path {
            // A missing file is normal (defaults); a broken one must not silently mean "search".
            let (settings, error) = crate::settings::load(path);
            if error.is_some() {
                return Err(PathError::SettingsUnreadable(path.clone()));
            }
            configured = settings.obsidian_path;
        }
        resolve_obsidian(configured.as_deref(), &real_env, &|p| p.is_file())
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
        /// When set, `obsidian_running` fails once `fail_after` successful calls are used up.
        fail_check: Cell<bool>,
        fail_after: Cell<u32>,
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
                fail_check: Cell::new(false),
                fail_after: Cell::new(0),
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
        fn obsidian_running(&self, image: &str) -> io::Result<bool> {
            self.images.borrow_mut().push(image.to_string());
            if self.fail_check.get() {
                let left = self.fail_after.get();
                if left == 0 {
                    return Err(io::Error::other("blocked"));
                }
                self.fail_after.set(left - 1);
            }
            let n = self.running_polls.get();
            if n == 0 {
                return Ok(false);
            }
            if n != u32::MAX {
                self.running_polls.set(n - 1);
            }
            Ok(true)
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
    fn unreadable_settings_and_unsafe_names_never_close_or_spawn() {
        let mut sys = FakeSystem::new(true, u32::MAX);
        sys.resolve = Err(PathError::SettingsUnreadable(PathBuf::from(r"C:\d\settings.json")));
        let out = launch(&sys, Mode::Max, None, true);
        assert!(matches!(out, LaunchOutcome::ObsidianUnavailable(PathError::SettingsUnreadable(_))));
        assert!(sys.calls().is_empty() && sys.images.borrow().is_empty());

        // A resolved path whose file name is a wildcard never reaches tasklist/taskkill.
        let mut sys = FakeSystem::new(true, u32::MAX);
        sys.resolve = Ok(PathBuf::from(r"C:\Tools\*.exe"));
        let out = launch(&sys, Mode::Max, None, true);
        assert!(matches!(out, LaunchOutcome::ObsidianUnavailable(PathError::UnsafeImageName(_))));
        assert!(sys.calls().is_empty() && sys.images.borrow().is_empty() && sys.spawned.borrow().is_empty());
    }

    #[test]
    fn real_system_refuses_a_broken_settings_file_but_accepts_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let sys = WindowsSystem { settings_path: Some(path.clone()) };
        // Missing file: normal automatic search (the result depends on this machine, but is no settings error).
        assert!(!matches!(sys.resolve_obsidian(), Err(PathError::SettingsUnreadable(_))));
        assert!(sys.current_image_name().is_some());

        std::fs::write(&path, "{ broken").unwrap();
        assert_eq!(sys.resolve_obsidian(), Err(PathError::SettingsUnreadable(path.clone())));
        assert_eq!(sys.current_image_name(), None, "poller keeps its last state");

        // A relative path from a hand-edited file is refused as well.
        std::fs::write(&path, r#"{"obsidian_path": "Obsidian.exe"}"#).unwrap();
        assert!(matches!(sys.resolve_obsidian(), Err(PathError::NotAbsolute(_))));
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
        let row = |n: &str| format!("\"{n}\",\"1234\",\"Console\",\"1\",\"150.000 K\"");
        assert!(o(&row("Obsidian.exe")));
        assert!(o(&row("OBSIDIAN.EXE")));
        assert!(o(&format!("\r\n{}\r\n", row("obsidian.exe"))));
        assert!(!o("INFO: No tasks are running which match the specified criteria."));
        assert!(!o("INFO: Es werden keine Aufgaben ausgeführt, die den angegebenen Kriterien entsprechen."));
        assert!(!o(""));
        // Only the exact image name counts, in either direction.
        assert!(!o(&row("NotObsidian.exe")));
        assert!(!o(&row("Obsidian.exe.bak")));
        // Names longer than 25 characters are compared in full.
        let long = "MyVeryLongRenamedObsidian123.exe";
        assert!(long.len() > 25);
        assert!(lists_image(&row(long), long));
        assert!(lists_image(&row(long), &long.to_uppercase()));
        assert!(!lists_image(&row(&long[..25]), long));
    }

    #[test]
    fn obsidian_starts_in_its_own_folder_and_falls_back_without_a_parent() {
        let cmd = obsidian_command(Path::new(r"C:\Apps\Obsidian\Obsidian.exe"), None);
        assert_eq!(cmd.get_current_dir(), Some(Path::new(r"C:\Apps\Obsidian")));
        // No parent folder in the path: the working directory stays inherited (as before).
        assert_eq!(obsidian_command(Path::new("Obsidian.exe"), None).get_current_dir(), None);
        assert_eq!(obsidian_command(Path::new(""), None).get_current_dir(), None);
        // The environment edits still apply to the child, and the key is only set in API-key mode.
        let key = obsidian_command(Path::new(r"C:\Apps\Obsidian.exe"), Some("sk-ant-test"));
        assert!(key.get_envs().any(|(k, v)| k == "ANTHROPIC_API_KEY" && v.is_some()));
        let max = obsidian_command(Path::new(r"C:\Apps\Obsidian.exe"), None);
        assert!(max.get_envs().any(|(k, v)| k == "ANTHROPIC_API_KEY" && v.is_none()));
    }

    #[test]
    fn failed_process_check_closes_and_starts_nothing() {
        let sys = FakeSystem::new(true, 0);
        sys.fail_check.set(true);
        for confirmed in [false, true] {
            assert_eq!(launch(&sys, Mode::Max, None, confirmed), LaunchOutcome::ProcessCheckFailed);
            assert_eq!(launch(&sys, Mode::ApiKey, Some(FAKE_KEY), confirmed), LaunchOutcome::ProcessCheckFailed);
        }
        assert!(sys.calls().is_empty(), "{:?}", sys.calls());
    }

    #[test]
    fn check_failing_while_waiting_for_exit_ends_in_timeout_not_start() {
        // Running at the first check, then every check fails: must never spawn.
        let sys = FakeSystem::new(true, u32::MAX);
        sys.fail_check.set(true);
        sys.fail_after.set(1);
        assert_eq!(launch(&sys, Mode::Max, None, true), LaunchOutcome::CloseTimedOut);
        assert_eq!(sys.count("close"), 1);
        assert_eq!(sys.count("spawn"), 0);
        assert_eq!(sys.slept.get(), 20);
    }

    #[test]
    fn tasklist_status_decides_between_not_running_and_failed() {
        let info = "INFO: No tasks are running which match the specified criteria.";
        assert!(!tasklist_answer(true, info, "Obsidian.exe").unwrap());
        assert!(tasklist_answer(true, "\"Obsidian.exe\",\"1\"", "Obsidian.exe").unwrap());
        assert!(tasklist_answer(false, "", "Obsidian.exe").is_err());
        assert!(tasklist_answer(false, "\"Obsidian.exe\",\"1\"", "Obsidian.exe").is_err());
    }

    #[test]
    fn max_mode_removes_key_and_token_even_if_key_given() {
        for key in [None, Some(FAKE_KEY)] {
            assert_eq!(
                child_env_ops(Mode::Max, key),
                vec![EnvOp::Remove("ANTHROPIC_API_KEY"), EnvOp::Remove("ANTHROPIC_AUTH_TOKEN")]
            );
        }
    }

    #[test]
    fn api_mode_sets_key_and_removes_token() {
        assert_eq!(
            child_env_ops(Mode::ApiKey, Some(FAKE_KEY)),
            vec![
                EnvOp::Set("ANTHROPIC_API_KEY", FAKE_KEY.to_string()),
                EnvOp::Remove("ANTHROPIC_AUTH_TOKEN")
            ]
        );
    }

    #[test]
    fn env_op_debug_never_shows_the_key() {
        let text = format!("{:?}", child_env_ops(Mode::ApiKey, Some(FAKE_KEY)));
        assert!(!text.contains(FAKE_KEY) && !text.contains("NICHTECHT"), "{text}");
    }

    #[test]
    fn routing_vars_are_reported_by_name_only() {
        let env = |n: &str| match n {
            "ANTHROPIC_BASE_URL" => Some("https://proxy.example/SECRET-VALUE".to_string()),
            "CLAUDE_CODE_USE_VERTEX" => Some("1".to_string()),
            "CLAUDE_CODE_USE_BEDROCK" => Some("  ".to_string()),
            _ => None,
        };
        let names = routing_env_names(&env);
        assert_eq!(names, vec!["ANTHROPIC_BASE_URL", "CLAUDE_CODE_USE_VERTEX"]);
        assert!(!format!("{names:?}").contains("SECRET-VALUE"));
        assert!(routing_env_names(&|_| None).is_empty());
    }
}
