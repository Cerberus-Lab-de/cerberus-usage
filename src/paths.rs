//! Where Obsidian and the Claude logs live. Pure functions: environment and file checks are passed in,
//! so tests never touch the real system.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::settings::Settings;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// Nothing found at any of the automatic locations (only the ones actually tried).
    ObsidianNotFound { searched: Vec<PathBuf> },
    /// A path from the settings that does not exist. Never falls back to the automatic search.
    ConfiguredMissing(PathBuf),
    NotAnExe(PathBuf),
    /// Neither `CLAUDE_CONFIG_DIR` nor `USERPROFILE` is usable.
    NoHome,
}

const SETTINGS_HINT: &str = "Bitte den Pfad in den Einstellungen unter „Pfade“ festlegen.";

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathError::ObsidianNotFound { searched } => {
                let list: Vec<String> = searched.iter().map(|p| p.display().to_string()).collect();
                write!(f, "Obsidian wurde nicht gefunden. Gesucht: {}. {SETTINGS_HINT}", list.join("; "))
            }
            PathError::ConfiguredMissing(p) => {
                write!(f, "Die eingestellte Obsidian-Datei gibt es nicht: {}. {SETTINGS_HINT}", p.display())
            }
            PathError::NotAnExe(p) => {
                write!(f, "Der eingestellte Obsidian-Pfad ist keine .exe-Datei: {}. {SETTINGS_HINT}", p.display())
            }
            PathError::NoHome => write!(
                f,
                "Der Protokollordner lässt sich nicht bestimmen (weder CLAUDE_CONFIG_DIR noch USERPROFILE gesetzt). {SETTINGS_HINT}"
            ),
        }
    }
}

pub type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// An environment value that is set, non-empty and absolute; anything else is treated as missing.
fn abs_env(env: Env, name: &str) -> Option<PathBuf> {
    env(name).filter(|v| !v.is_empty()).map(PathBuf::from).filter(|p| p.is_absolute())
}

/// Automatic Obsidian locations, in search order. Candidates whose variable is missing are skipped.
fn obsidian_candidates(env: Env) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(base) = abs_env(env, "LOCALAPPDATA") {
        out.push(base.join(r"Programs\Obsidian\Obsidian.exe"));
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = abs_env(env, var) {
            out.push(base.join(r"Obsidian\Obsidian.exe"));
        }
    }
    out
}

/// The Obsidian executable: the configured path, else the first automatic location that exists.
pub fn resolve_obsidian(
    configured: Option<&Path>,
    env: Env,
    exists: &dyn Fn(&Path) -> bool,
) -> Result<PathBuf, PathError> {
    if let Some(path) = configured {
        if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
            return Err(PathError::NotAnExe(path.to_path_buf()));
        }
        return if exists(path) { Ok(path.to_path_buf()) } else { Err(PathError::ConfiguredMissing(path.to_path_buf())) };
    }
    let searched = obsidian_candidates(env);
    match searched.iter().find(|p| exists(p)) {
        Some(found) => Ok(found.clone()),
        None => Err(PathError::ObsidianNotFound { searched }),
    }
}

/// The Claude projects folder: configured, else `%CLAUDE_CONFIG_DIR%\projects`, else
/// `%USERPROFILE%\.claude\projects`. Never a relative path. Existence is checked by the caller.
pub fn resolve_logs_root(configured: Option<&Path>, env: Env) -> Result<PathBuf, PathError> {
    if let Some(path) = configured {
        return Ok(path.to_path_buf());
    }
    if let Some(dir) = abs_env(env, "CLAUDE_CONFIG_DIR") {
        return Ok(dir.join("projects"));
    }
    abs_env(env, "USERPROFILE").map(|home| home.join(".claude").join("projects")).ok_or(PathError::NoHome)
}

/// Process image name for `tasklist`/`taskkill`: the file name of the executable, so a renamed
/// Obsidian is still recognised.
pub fn image_name(exe: &Path) -> String {
    exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| DEFAULT_IMAGE_NAME.to_string())
}

/// Default image name when no executable could be resolved.
pub const DEFAULT_IMAGE_NAME: &str = "Obsidian.exe";

/// Text field to path: trims, drops the quotes of "Copy as path", empty means automatic (`None`).
fn typed_path(input: &str) -> Option<PathBuf> {
    let t = input.trim().trim_matches('"').trim();
    (!t.is_empty()).then(|| PathBuf::from(t))
}

/// Settings with the Obsidian path from the text field. Empty resets to automatic.
pub fn with_obsidian_path(s: &Settings, input: &str) -> Result<Settings, String> {
    let path = typed_path(input);
    if let Some(p) = &path
        && !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    {
        return Err("Der Obsidian-Pfad muss auf eine .exe-Datei zeigen. Es wurde nichts geändert.".to_string());
    }
    Ok(Settings { obsidian_path: path, ..s.clone() })
}

/// Settings with the log folder from the text field. Empty resets to automatic.
pub fn with_logs_root(s: &Settings, input: &str) -> Result<Settings, String> {
    let path = typed_path(input);
    if path.as_ref().is_some_and(|p| !p.is_absolute()) {
        return Err("Der Protokollordner muss ein vollständiger Pfad sein. Es wurde nichts geändert.".to_string());
    }
    Ok(Settings { logs_root: path, ..s.clone() })
}

/// Real environment lookup.
pub fn real_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs.iter().map(|(k, v)| (k.to_string(), OsString::from(v))).collect();
        move |k| map.get(k).cloned()
    }

    fn exists_in(list: &[&str]) -> impl Fn(&Path) -> bool {
        let set: Vec<PathBuf> = list.iter().map(PathBuf::from).collect();
        move |p| set.iter().any(|s| s == p)
    }

    const LOCAL: &str = r"C:\Users\x\AppData\Local\Programs\Obsidian\Obsidian.exe";
    const PF: &str = r"C:\Program Files\Obsidian\Obsidian.exe";
    const PF86: &str = r"C:\Program Files (x86)\Obsidian\Obsidian.exe";

    fn full_env() -> impl Fn(&str) -> Option<OsString> {
        env_of(&[
            ("LOCALAPPDATA", r"C:\Users\x\AppData\Local"),
            ("ProgramFiles", r"C:\Program Files"),
            ("ProgramFiles(x86)", r"C:\Program Files (x86)"),
        ])
    }

    #[test]
    fn obsidian_search_order_prefers_configured() {
        let cfg = PathBuf::from(r"D:\Apps\Obsidian\Obsidian.exe");
        let got = resolve_obsidian(Some(&cfg), &full_env(), &exists_in(&[LOCAL, PF, cfg.to_str().unwrap()])).unwrap();
        assert_eq!(got, cfg);
        let got = resolve_obsidian(None, &full_env(), &exists_in(&[PF, LOCAL])).unwrap();
        assert_eq!(got, PathBuf::from(LOCAL));
    }

    #[test]
    fn obsidian_falls_back_to_program_files() {
        assert_eq!(resolve_obsidian(None, &full_env(), &exists_in(&[PF, PF86])).unwrap(), PathBuf::from(PF));
        assert_eq!(resolve_obsidian(None, &full_env(), &exists_in(&[PF86])).unwrap(), PathBuf::from(PF86));
    }

    #[test]
    fn configured_missing_is_an_error_not_a_fallback() {
        let cfg = PathBuf::from(r"D:\gone\Obsidian.exe");
        // The automatic location exists, but must not be used.
        let err = resolve_obsidian(Some(&cfg), &full_env(), &exists_in(&[LOCAL])).unwrap_err();
        assert_eq!(err, PathError::ConfiguredMissing(cfg));
    }

    #[test]
    fn configured_path_must_be_an_exe() {
        let cfg = PathBuf::from(r"D:\Apps\Obsidian\readme.txt");
        let err = resolve_obsidian(Some(&cfg), &full_env(), &exists_in(&[r"D:\Apps\Obsidian\readme.txt"])).unwrap_err();
        assert_eq!(err, PathError::NotAnExe(cfg));
        let upper = PathBuf::from(r"D:\Apps\OBSIDIAN.EXE");
        assert!(resolve_obsidian(Some(&upper), &full_env(), &exists_in(&[r"D:\Apps\OBSIDIAN.EXE"])).is_ok());
    }

    #[test]
    fn not_found_lists_searched_paths() {
        let err = resolve_obsidian(None, &full_env(), &exists_in(&[])).unwrap_err();
        let PathError::ObsidianNotFound { searched } = &err else { panic!("{err:?}") };
        assert_eq!(searched, &[PathBuf::from(LOCAL), PathBuf::from(PF), PathBuf::from(PF86)]);
        let text = err.to_string();
        assert!(text.contains(LOCAL) && text.contains("Einstellungen"), "{text}");

        // Missing variables are skipped and not listed.
        let env = env_of(&[("ProgramFiles", r"C:\Program Files")]);
        let PathError::ObsidianNotFound { searched } = resolve_obsidian(None, &env, &exists_in(&[])).unwrap_err() else {
            panic!()
        };
        assert_eq!(searched, vec![PathBuf::from(PF)]);
    }

    #[test]
    fn logs_root_prefers_configured_then_claude_config_dir() {
        let env = env_of(&[("CLAUDE_CONFIG_DIR", r"D:\claude"), ("USERPROFILE", r"C:\Users\x")]);
        let cfg = PathBuf::from(r"E:\logs");
        assert_eq!(resolve_logs_root(Some(&cfg), &env).unwrap(), cfg);
        assert_eq!(resolve_logs_root(None, &env).unwrap(), PathBuf::from(r"D:\claude\projects"));
        let env = env_of(&[("USERPROFILE", r"C:\Users\x")]);
        assert_eq!(resolve_logs_root(None, &env).unwrap(), PathBuf::from(r"C:\Users\x\.claude\projects"));
    }

    #[test]
    fn logs_root_without_home_is_an_error() {
        assert_eq!(resolve_logs_root(None, &env_of(&[])).unwrap_err(), PathError::NoHome);
        // Empty or relative values never produce a relative path.
        let env = env_of(&[("USERPROFILE", ""), ("CLAUDE_CONFIG_DIR", "rel")]);
        assert_eq!(resolve_logs_root(None, &env).unwrap_err(), PathError::NoHome);
        assert!(PathError::NoHome.to_string().contains("USERPROFILE"));
    }

    #[test]
    fn typed_paths_are_validated_and_empty_resets() {
        let base = Settings { monthly_budget_nano: Some(5), ..Settings::default() };
        let s = with_obsidian_path(&base, r#"  "D:\Apps\Obsidian.exe"  "#).unwrap();
        assert_eq!(s.obsidian_path, Some(PathBuf::from(r"D:\Apps\Obsidian.exe")));
        assert_eq!(s.monthly_budget_nano, Some(5), "other settings stay");
        assert!(with_obsidian_path(&s, r"D:\Apps
otes.txt").is_err());
        assert_eq!(with_obsidian_path(&s, "  ").unwrap().obsidian_path, None);

        let s = with_logs_root(&base, r"E:\logs").unwrap();
        assert_eq!(s.logs_root, Some(PathBuf::from(r"E:\logs")));
        assert!(with_logs_root(&s, r"logs\relative").is_err());
        assert_eq!(with_logs_root(&s, "").unwrap().logs_root, None);
    }

    #[test]
    fn image_name_comes_from_the_exe_file_name() {
        assert_eq!(image_name(Path::new(r"C:\Tools\MyObsidian.exe")), "MyObsidian.exe");
        assert_eq!(image_name(Path::new(LOCAL)), "Obsidian.exe");
    }
}
