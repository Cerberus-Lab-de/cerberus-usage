//! Uninstall cleanup: removes the stored keys and the app data folder (best effort).
//!
//! Only reached through `--uninstall-cleanup`, which the uninstaller runs after the user said yes.
//! It deletes the two key slots and exactly one directory, and refuses any directory that does not
//! look like the app's own data folder.

use std::path::Path;

use crate::i18n::{tr, tr_args};
use crate::secrets::{SecretStore, Slot};

/// Deletes both key slots and the data folder. Returns problem lines (they only go to `crash.log`);
/// never panics.
///
/// Refuses a `data_dir` that is relative, a drive root, or not named exactly `CerberusUsage`.
pub fn uninstall_cleanup(store: &dyn SecretStore, data_dir: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    let dir_ok = is_app_data_dir(data_dir);
    if !dir_ok {
        problems.push(tr("cleanup.dir_refused"));
    }
    for (slot, key) in [(Slot::ApiKey, "cleanup.api_key_failed"), (Slot::AdminKey, "cleanup.admin_key_failed")] {
        if let Err(e) = store.delete(slot) {
            problems.push(tr_args(key, &[("error", &e.to_string())]));
        }
    }
    if dir_ok {
        match std::fs::remove_dir_all(data_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => problems.push(tr_args("cleanup.dir_failed", &[("error", &e.to_string())])),
        }
    }
    problems
}

/// Absolute, not a drive root, and named exactly `CerberusUsage`.
fn is_app_data_dir(dir: &Path) -> bool {
    dir.is_absolute()
        && dir.parent().is_some()
        && dir.file_name().is_some_and(|n| n == "CerberusUsage")
}

/// `%LOCALAPPDATA%\CerberusUsage`, `None` without `LOCALAPPDATA`.
pub fn default_data_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|p| std::path::PathBuf::from(p).join("CerberusUsage"))
}

/// The app's data folder; without `LOCALAPPDATA` a folder below the system temp directory.
pub fn data_dir() -> std::path::PathBuf {
    default_data_dir().unwrap_or_else(|| std::env::temp_dir().join("CerberusUsage"))
}

/// Process exit code of `--uninstall-cleanup`: 0 when everything was removed, 1 otherwise. The
/// uninstaller shows a message for any other code than 0.
pub fn exit_code(problems: &[String]) -> i32 {
    if problems.is_empty() { 0 } else { 1 }
}

/// Entry point for `--uninstall-cleanup`: runs the cleanup and logs leftovers to `crash.log`
/// only when the data folder survived (otherwise there would be nowhere to put the log).
/// Returns the exit code for the uninstaller (see [`exit_code`]).
pub fn run_uninstall_cleanup() -> i32 {
    let Some(dir) = default_data_dir() else { return 1 };
    let problems = uninstall_cleanup(&crate::secrets::KeyringStore::new(), &dir);
    let code = exit_code(&problems);
    if problems.is_empty() || !dir.exists() {
        return code;
    }
    if let Some(log) = crate::crash::log_path() {
        let now = chrono::Local::now();
        for line in problems {
            let _ = crate::crash::append_entry(&log, &crate::crash::format_entry(now, None, &line));
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeStore {
        deleted: RefCell<Vec<Slot>>,
        fail: bool,
    }

    impl SecretStore for FakeStore {
        fn get(&self, _s: Slot) -> Result<Option<String>, crate::secrets::SecretError> {
            Ok(None)
        }
        fn set(&self, _s: Slot, _v: &str) -> Result<(), crate::secrets::SecretError> {
            Ok(())
        }
        fn delete(&self, s: Slot) -> Result<(), crate::secrets::SecretError> {
            self.deleted.borrow_mut().push(s);
            if self.fail { Err(crate::secrets::SecretError::StoreUnavailable) } else { Ok(()) }
        }
    }

    #[test]
    fn cleanup_removes_keys_and_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("CerberusUsage");
        std::fs::create_dir_all(data.join("sub")).unwrap();
        std::fs::write(data.join("settings.json"), b"{}").unwrap();
        std::fs::write(data.join("sub").join("x"), b"x").unwrap();
        let sibling = tmp.path().join("Other");
        std::fs::create_dir_all(&sibling).unwrap();

        let store = FakeStore::default();
        let problems = uninstall_cleanup(&store, &data);

        assert!(problems.is_empty(), "{problems:?}");
        assert!(!data.exists());
        assert!(sibling.exists(), "sibling directory must stay");
        assert_eq!(*store.deleted.borrow(), vec![Slot::ApiKey, Slot::AdminKey]);
    }

    #[test]
    fn cleanup_removes_a_config_that_holds_keys_and_still_clears_the_legacy_slots() {
        use crate::secrets::SecretStore as _;
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("CerberusUsage");
        let path = data.join("settings.json");
        let config = crate::config_store::ConfigStore::new(path.clone());
        config.set(Slot::ApiKey, "sk-ant-test-NICHTECHT-1234").unwrap();
        config.set(Slot::AdminKey, "sk-ant-admin01-NICHTECHT-5678").unwrap();
        assert!(path.exists());

        let legacy = FakeStore::default();
        let problems = uninstall_cleanup(&legacy, &data);

        assert!(problems.is_empty(), "{problems:?}");
        assert!(!data.exists(), "the config with the keys goes with the data folder");
        assert_eq!(*legacy.deleted.borrow(), vec![Slot::ApiKey, Slot::AdminKey]);
    }

    #[test]
    fn exit_code_is_zero_only_without_problems() {
        assert_eq!(exit_code(&[]), 0);
        assert_eq!(exit_code(&["Datenordner konnte nicht gelöscht werden".to_string()]), 1);
        // A failed cleanup run reports a non-zero code through the same path.
        let tmp = tempfile::tempdir().unwrap();
        let store = FakeStore { fail: true, ..Default::default() };
        let problems = uninstall_cleanup(&store, &tmp.path().join("CerberusUsage"));
        assert_eq!(exit_code(&problems), 1);
    }

    #[test]
    fn cleanup_reports_but_survives_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("CerberusUsage");
        std::fs::create_dir_all(&data).unwrap();
        let store = FakeStore { fail: true, ..Default::default() };

        let problems = uninstall_cleanup(&store, &data);

        // Both slots are tried even though the first failed; the folder is removed regardless.
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert_eq!(store.deleted.borrow().len(), 2);
        assert!(!data.exists());
        assert!(problems.iter().all(|p| !p.contains("sk-ant")));
    }

    #[test]
    fn cleanup_missing_data_dir_is_not_a_problem() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("CerberusUsage");
        let problems = uninstall_cleanup(&FakeStore::default(), &data);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn cleanup_refuses_unsafe_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let wrong_name = tmp.path().join("Documents");
        std::fs::create_dir_all(&wrong_name).unwrap();
        std::fs::write(wrong_name.join("keep.txt"), b"keep").unwrap();
        let lower = tmp.path().join("cerberususage");
        std::fs::create_dir_all(&lower).unwrap();

        for bad in [
            wrong_name.clone(),
            lower.clone(),
            tmp.path().to_path_buf(),
            std::path::PathBuf::from(r"C:\"),
            std::path::PathBuf::from(r"CerberusUsage"),
            std::path::PathBuf::from(r"..\CerberusUsage"),
            std::path::PathBuf::new(),
        ] {
            let store = FakeStore::default();
            let problems = uninstall_cleanup(&store, &bad);
            assert_eq!(problems.len(), 1, "{bad:?}: {problems:?}");
            assert!(problems[0].contains("Datenordner"), "{problems:?}");
        }
        assert!(wrong_name.join("keep.txt").exists());
        assert!(lower.exists());
        assert!(tmp.path().exists());
    }
}
