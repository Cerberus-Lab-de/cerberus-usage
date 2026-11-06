//! User settings (`%LOCALAPPDATA%\CerberusUsage\settings.json`): credit baseline, top-ups, alert limits.
//!
//! All amounts are nano-USD (`u64`), timestamps are RFC 3339. API and admin keys live here too,
//! encrypted with DPAPI by default (see `config_store`).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Credit balance at a known point in time (read from the Console by the user).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    pub amount_nano: u64,
    pub at: DateTime<Utc>,
}

/// Credit added after the baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopUp {
    pub amount_nano: u64,
    pub at: DateTime<Utc>,
}

/// How a stored key value is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyForm {
    /// Base64 of a DPAPI blob (current Windows user).
    Dpapi,
    /// The key itself, readable in the file.
    Plain,
}

/// A key as stored in `settings.json`. `Debug` never prints the value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredKey {
    pub form: KeyForm,
    pub value: String,
}

impl std::fmt::Debug for StoredKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredKey").field("form", &self.form).field("value", &"<redacted>").finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub baseline: Option<Baseline>,
    pub topups: Vec<TopUp>,
    pub monthly_budget_nano: Option<u64>,
    pub low_balance_nano: Option<u64>,
    /// Obsidian executable; `None` searches the usual locations.
    pub obsidian_path: Option<PathBuf>,
    /// Claude projects folder; `None` uses `CLAUDE_CONFIG_DIR` or `%USERPROFILE%\.claude\projects`.
    pub logs_root: Option<PathBuf>,
    /// Look for new releases on GitHub (at start and every 24 h). Off: no request to GitHub unless the
    /// user clicks "Jetzt prüfen" or installs.
    #[serde(default = "default_true")]
    pub check_updates: bool,
    /// Interface language code; `None` follows the system.
    pub language: Option<String>,
    /// Keys are written readable instead of DPAPI-encrypted.
    pub keys_plaintext: bool,
    pub api_key: Option<StoredKey>,
    pub admin_key: Option<StoredKey>,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            baseline: None,
            topups: Vec::new(),
            monthly_budget_nano: None,
            low_balance_nano: None,
            obsidian_path: None,
            logs_root: None,
            check_updates: true,
            language: None,
            keys_plaintext: false,
            api_key: None,
            admin_key: None,
        }
    }
}

/// `%LOCALAPPDATA%\CerberusUsage\settings.json`. Errors when `LOCALAPPDATA` is missing:
/// there is deliberately no fallback to a relative or temporary path.
pub fn default_path() -> io::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA not set"))?;
    Ok(PathBuf::from(base).join("CerberusUsage").join("settings.json"))
}

/// Loads the settings. A missing file is normal (defaults, no error). An unreadable or broken
/// file yields defaults plus a fixed user-facing message; the file itself is never touched,
/// so the user can repair it.
pub fn load(path: &Path) -> (Settings, Option<String>) {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (Settings::default(), None),
        Err(_) => {
            return (
                Settings::default(),
                Some("Einstellungen konnten nicht gelesen werden".to_string()),
            );
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(s) => (s, None),
        Err(e) => (
            Settings::default(),
            Some(format!(
                "Einstellungen sind ungültig (Zeile {}, Spalte {}); die Datei wurde nicht verändert",
                e.line(),
                e.column()
            )),
        ),
    }
}

/// Writes the settings atomically: temporary file next to the target, then rename.
pub fn save(path: &Path, settings: &Settings) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let result = (|| {
        let bytes = serde_json::to_vec_pretty(settings).map_err(io::Error::other)?;
        let mut file = crate::io_retry::retry_transient(|| std::fs::File::create(&tmp))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        crate::io_retry::retry_transient(|| std::fs::rename(&tmp, path))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn missing_file_gives_defaults_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let (s, err) = load(&dir.path().join("settings.json"));
        assert_eq!(s, Settings::default());
        assert!(err.is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("settings.json");
        let s = Settings {
            baseline: Some(Baseline { amount_nano: 50_000_000_000, at: utc("2026-11-01T08:30:00Z") }),
            topups: vec![TopUp { amount_nano: 10_000_000_000, at: utc("2026-11-03T12:00:00Z") }],
            monthly_budget_nano: Some(20_000_000_000),
            low_balance_nano: None,
            obsidian_path: None,
            logs_root: None,
            check_updates: false,
            language: Some("de".to_string()),
            keys_plaintext: true,
            api_key: Some(StoredKey { form: KeyForm::Plain, value: "x".to_string() }),
            admin_key: None,
        };
        save(&path, &s).unwrap();
        assert_eq!(load(&path), (s, None));
        assert!(!dir.path().join("sub").join("settings.json.tmp").exists());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("2026-11-01T08:30:00Z"), "RFC 3339 expected: {text}");
    }

    #[test]
    fn old_settings_file_defaults_to_checking() {
        // A file written before the setting existed, and an empty object, both mean "check".
        let old: Settings = serde_json::from_str(r#"{"monthly_budget_nano": 5}"#).unwrap();
        assert!(old.check_updates);
        let empty: Settings = serde_json::from_str("{}").unwrap();
        assert!(empty.check_updates && Settings::default().check_updates);
        let off: Settings = serde_json::from_str(r#"{"check_updates": false}"#).unwrap();
        assert!(!off.check_updates);
    }

    #[test]
    fn old_config_loads_with_defaults() {
        // The v1.0.1 file shape: no language, no keys.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"baseline": null, "topups": [], "monthly_budget_nano": 7, "low_balance_nano": null,
                "obsidian_path": null, "logs_root": null, "check_updates": true}"#,
        )
        .unwrap();
        let (s, err) = load(&path);
        assert!(err.is_none());
        assert!(!s.keys_plaintext && s.api_key.is_none() && s.admin_key.is_none() && s.language.is_none());
        assert_eq!(s.monthly_budget_nano, Some(7));
    }

    #[test]
    fn debug_never_shows_a_key() {
        let s = Settings {
            api_key: Some(StoredKey { form: KeyForm::Plain, value: "sk-ant-test-SECRET".to_string() }),
            admin_key: Some(StoredKey { form: KeyForm::Dpapi, value: "sk-ant-test-SECRET".to_string() }),
            ..Settings::default()
        };
        let shown = format!("{s:?} {:#?}", s.api_key);
        assert!(!shown.contains("SECRET"), "leaked: {shown}");
        assert!(shown.contains("<redacted>"));
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""form":"plain""#) && json.contains(r#""form":"dpapi""#));
    }

    #[test]
    fn partial_file_fills_missing_fields_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"monthly_budget_nano": 5}"#).unwrap();
        let (s, err) = load(&path);
        assert!(err.is_none());
        assert_eq!(s.monthly_budget_nano, Some(5));
        assert!(s.baseline.is_none() && s.topups.is_empty());
    }

    #[test]
    fn broken_settings_file_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let broken = r#"{"baseline": {"amount_nano": "oops", SECRET-ish"#;
        std::fs::write(&path, broken).unwrap();

        let (s, err) = load(&path);
        assert_eq!(s, Settings::default());
        let msg = err.expect("a broken file must be reported");
        assert!(!msg.contains("SECRET"), "message must not echo file content");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
    }

    #[test]
    fn old_settings_file_without_paths_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"topups": [], "monthly_budget_nano": 7}"#).unwrap();
        let (s, err) = load(&path);
        assert!(err.is_none());
        assert_eq!(s.monthly_budget_nano, Some(7));
        assert!(s.obsidian_path.is_none() && s.logs_root.is_none());

        let with = Settings { obsidian_path: Some(PathBuf::from(r"D:\O\Obsidian.exe")), logs_root: Some(PathBuf::from(r"E:\logs")), ..Settings::default() };
        save(&path, &with).unwrap();
        assert_eq!(load(&path), (with, None));
    }

    #[test]
    fn default_path_requires_localappdata() {
        // Only the failure shape is tested; the environment is not mutated.
        if std::env::var_os("LOCALAPPDATA").is_some() {
            let p = default_path().unwrap();
            assert!(p.is_absolute() && p.ends_with("CerberusUsage/settings.json"));
        } else {
            assert!(default_path().is_err());
        }
    }
}
