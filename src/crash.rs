//! Last-resort reporting: a message box when the window cannot start, and a small append-only
//! `crash.log` for panics. Everything here is best effort and must never panic itself.
//!
//! The panic text is cut short and any `sk-ant-` token is blanked, so even an unexpected key in a
//! panic message would not reach the file.

use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

/// When the log grows beyond this it is started afresh (the file stays small).
const MAX_LOG_BYTES: u64 = 64 * 1024;
/// Longest message kept per entry.
const MAX_MESSAGE_CHARS: usize = 400;

/// `%LOCALAPPDATA%\CerberusUsage\crash.log`; `None` without `LOCALAPPDATA` (no guessing a folder).
pub fn log_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("CerberusUsage").join("crash.log"))
}

/// Cuts the text short and blanks anything that looks like an Anthropic key.
pub fn scrub(message: &str) -> String {
    let mut out = String::new();
    let mut rest = message;
    while let Some(i) = rest.find("sk-ant-") {
        out.push_str(&rest[..i]);
        out.push_str("sk-ant-<entfernt>");
        let after = &rest[i + "sk-ant-".len()..];
        let end = after.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ')')).unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out.chars().take(MAX_MESSAGE_CHARS).collect()
}

/// One log entry: time, location, message.
pub fn format_entry(time: DateTime<Local>, location: Option<(&str, u32)>, message: &str) -> String {
    let place = match location {
        Some((file, line)) => format!("{file}:{line}"),
        None => "unbekannt".to_string(),
    };
    format!("{} {} {}\n", time.format("%Y-%m-%d %H:%M:%S"), place, scrub(message).replace('\n', " "))
}

/// Appends an entry; the file is restarted when it is too big. Errors are returned, never raised.
pub fn append_entry(path: &Path, entry: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let too_big = std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES);
    let mut file = std::fs::OpenOptions::new().create(true).append(!too_big).write(true).truncate(too_big).open(path)?;
    file.write_all(entry.as_bytes())
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Modal error box (works without a window; the app has no console).
pub fn show_error_box(text: &str) {
    let text = wide(text);
    let title = wide("Cerberus Usage");
    // SAFETY: both strings are valid NUL-terminated UTF-16; no owner window.
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
    }
}

/// Logs the failure to `crash.log` (best effort) and tells the user.
pub fn report_startup_failure(error: &dyn std::fmt::Display) {
    let message = format!("Fenster konnte nicht gestartet werden: {error}");
    if let Some(path) = log_path() {
        let _ = append_entry(&path, &format_entry(Local::now(), None, &message));
    }
    show_error_box(&format!(
        "Cerberus Usage konnte das Fenster nicht öffnen.\n\n{}\n\nEin Eintrag steht in %LOCALAPPDATA%\\CerberusUsage\\crash.log.",
        scrub(&error.to_string())
    ));
}

/// Writes every panic to `crash.log`. A panic on the main (UI) thread also shows a message box,
/// because the window is gone right after. The previous hook (stderr output in debug builds) still runs.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Nothing in here may panic, or the process aborts without a trace.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let message = match info.payload().downcast_ref::<&str>() {
                Some(s) => (*s).to_string(),
                None => info.payload().downcast_ref::<String>().cloned().unwrap_or_else(|| "(kein Text)".to_string()),
            };
            let location = info.location().map(|l| (l.file(), l.line()));
            if let Some(path) = log_path() {
                let _ = append_entry(&path, &format_entry(Local::now(), location, &message));
            }
            if std::thread::current().name() == Some("main") {
                show_error_box("Cerberus Usage ist auf einen Fehler gestoßen und wird beendet.\n\nDetails stehen in %LOCALAPPDATA%\\CerberusUsage\\crash.log.");
            }
        }));
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn entry_has_time_location_and_message_on_one_line() {
        let t = Local.with_ymd_and_hms(2026, 11, 5, 14, 30, 5).unwrap();
        let e = format_entry(t, Some(("src/ui.rs", 42)), "boom\nsecond line");
        assert_eq!(e, "2026-11-05 14:30:05 src/ui.rs:42 boom second line\n");
        assert!(format_entry(t, None, "x").contains("unbekannt"));
    }

    #[test]
    fn keys_are_blanked_and_long_messages_cut() {
        let fake = "sk-ant-api03-NICHTECHT-1234";
        let out = scrub(&format!("failed with {fake} in header, and sk-ant-admin01-ALSONOT end"));
        assert!(!out.contains("NICHTECHT") && !out.contains("ALSONOT") && !out.contains("1234"), "{out}");
        assert!(out.contains("sk-ant-<entfernt>") && out.contains("end"), "{out}");
        assert_eq!(scrub(&"a".repeat(5000)).chars().count(), MAX_MESSAGE_CHARS);
    }

    #[test]
    fn log_is_append_only_and_restarted_when_too_big() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("crash.log");
        append_entry(&path, "one\n").unwrap();
        append_entry(&path, "two\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");

        std::fs::write(&path, vec![b'x'; (MAX_LOG_BYTES + 1) as usize]).unwrap();
        append_entry(&path, "fresh\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh\n");
    }
}
