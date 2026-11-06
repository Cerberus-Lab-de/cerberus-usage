//! Pure label and number formatting for the UI, kept out of the egui code so it is testable.
//! All texts come from the language files (`crate::i18n`).

use std::fmt::Write as _;

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Weekday};

use crate::i18n::{thousands_separator, tr, tr_args, tr_n};
use crate::launcher::{LaunchOutcome, Mode};

/// Text shown next to every cost figure.
pub fn cost_note() -> String {
    tr("format.cost_note")
}

/// `1234567` -> `1.234.567` (German) or `1,234,567` (English): the separator comes from the language.
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let separator = thousands_separator();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 * separator.len());
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push_str(&separator);
        }
        out.push(c);
    }
    out
}

/// Formats `t` with the pattern stored under `key`. A pattern from a language file can be invalid
/// (chrono then fails while formatting) or missing, so `fallback` is used instead of panicking.
pub fn format_time_by<Tz: TimeZone>(t: &DateTime<Tz>, key: &str, fallback: &str) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let pattern = tr(key);
    let mut out = String::new();
    if pattern != key && !pattern.is_empty() && write!(out, "{}", t.format(&pattern)).is_ok() {
        return out;
    }
    out.clear();
    // The fallback is a fixed, valid pattern.
    let _ = write!(out, "{}", t.format(fallback));
    out
}

/// Like [`format_time_by`] for a plain date.
pub fn format_date_by(d: NaiveDate, key: &str, fallback: &str) -> String {
    let pattern = tr(key);
    let mut out = String::new();
    if pattern != key && !pattern.is_empty() && write!(out, "{}", d.format(&pattern)).is_ok() {
        return out;
    }
    out.clear();
    // The fallback is a fixed, valid pattern.
    let _ = write!(out, "{}", d.format(fallback));
    out
}

/// `352 requests` with the thousands separator and the singular where it applies.
pub fn requests_text(n: u64) -> String {
    tr_n("format.requests", n, &[("count", &format_count(n))])
}

fn limit_type_label(limit_type: &str) -> String {
    match limit_type {
        "five_hour" => tr("format.limit.five_hour"),
        "seven_day" => tr("format.limit.seven_day"),
        other => other.to_string(),
    }
}

fn weekday_label(d: Weekday) -> String {
    let key = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"][d.num_days_from_monday() as usize];
    tr(&format!("format.weekday.{key}"))
}

/// `Max pausiert bis 17:42 (5-Stunden-Limit)`; with the date when the reset is not on the day
/// of `now`: `Max pausiert bis Do, 12.11. 14:30 (Wochenlimit)`. Both times in the same zone (local).
pub fn pause_text<Tz: TimeZone>(until: DateTime<Tz>, now: DateTime<Tz>, limit_type: &str) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let limit = limit_type_label(limit_type);
    let time = format_time_by(&until, "format.time_short", "%H:%M");
    if until.date_naive() == now.date_naive() {
        return tr_args("format.pause.today", &[("time", &time), ("limit", &limit)]);
    }
    let weekday = weekday_label(until.weekday());
    let date = format_time_by(&until, "format.date_short", "%m-%d");
    tr_args(
        "format.pause.later",
        &[("weekday", &weekday), ("date", &date), ("time", &time), ("limit", &limit)],
    )
}

/// Warning for routing variables that were left untouched. Takes names only, never values.
pub fn routing_warning(names: &[&str]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    Some(tr_n("format.routing_warning", names.len() as u64, &[("names", &names.join(", "))]))
}

/// `None` when every request had a price.
pub fn unpriced_text(unpriced_requests: u64) -> Option<String> {
    if unpriced_requests == 0 {
        return None;
    }
    Some(tr_n("format.unpriced", unpriced_requests, &[("count", &format_count(unpriced_requests))]))
}

pub fn progress_text(scanned: usize, total: usize) -> String {
    tr_args("format.progress", &[("scanned", &scanned.to_string()), ("total", &total.to_string())])
}

pub fn mode_label(mode: Mode) -> String {
    match mode {
        Mode::Max => tr("format.mode.max"),
        Mode::ApiKey => tr("format.mode.api_key"),
    }
}

/// Message after a finished launch attempt. `None` for `NeedsConfirmation` (handled by a dialog).
/// A spawn io error also arrives as `ObsidianNotFound`, hence the wording.
pub fn outcome_message(mode: Mode, outcome: LaunchOutcome) -> Option<String> {
    match outcome {
        LaunchOutcome::Started => Some(tr_args("format.outcome.started", &[("mode", &mode_label(mode))])),
        LaunchOutcome::NeedsConfirmation => None,
        LaunchOutcome::CloseTimedOut => Some(tr("format.outcome.close_timed_out")),
        LaunchOutcome::ProcessCheckFailed => Some(tr("format.outcome.process_check_failed")),
        LaunchOutcome::MissingKey => {
            Some(tr_args("format.outcome.missing_key", &[("settings", &tr("ui.settings"))]))
        }
        LaunchOutcome::ObsidianNotFound => Some(tr("format.outcome.not_found")),
        LaunchOutcome::ObsidianUnavailable(e) => Some(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, Local, TimeZone, Utc};

    #[test]
    fn counts_get_german_thousand_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1.000");
        assert_eq!(format_count(1_234_567), "1.234.567");
    }

    #[test]
    fn pause_text_uses_local_time_and_type() {
        // Same day: time only. Local zone as the app uses it; "now" is the same day.
        let utc = Utc.with_ymd_and_hms(2026, 11, 4, 16, 42, 0).unwrap();
        let local = utc.with_timezone(&Local);
        let expected = format!("Max pausiert bis {} (5-Stunden-Limit)", local.format("%H:%M"));
        assert_eq!(pause_text(local, local, "five_hour"), expected);
        assert!(pause_text(local, local, "unbekannt").ends_with("(unbekannt)"));
    }

    #[test]
    fn pause_text_names_the_date_when_the_reset_is_not_today() {
        let tz = FixedOffset::east_opt(3600).unwrap();
        let at = |d: u32, h: u32, m: u32| tz.with_ymd_and_hms(2026, 11, d, h, m, 0).unwrap();
        // 2026-11-12 is a Thursday.
        assert_eq!(
            pause_text(at(12, 14, 30), at(8, 9, 0), "seven_day"),
            "Max pausiert bis Do, 12.11. 14:30 (Wochenlimit)"
        );
        // Same day: no date.
        assert_eq!(pause_text(at(8, 17, 5), at(8, 9, 0), "five_hour"), "Max pausiert bis 17:05 (5-Stunden-Limit)");
        // Across midnight: 23:50 now, reset 01:10 the next day (Mo 9.11.2026 is a Monday).
        assert_eq!(
            pause_text(at(9, 1, 10), at(8, 23, 50), "five_hour"),
            "Max pausiert bis Mo, 09.11. 01:10 (5-Stunden-Limit)"
        );
        // All weekday abbreviations are German and need no locale.
        let days: Vec<String> = (9..=15)
            .map(|d| pause_text(at(d, 8, 0), at(1, 8, 0), "x")[17..19].to_string())
            .collect();
        assert_eq!(days, ["Mo", "Di", "Mi", "Do", "Fr", "Sa", "So"]);
    }

    #[test]
    fn routing_warning_names_variables_but_never_values() {
        assert!(routing_warning(&[]).is_none());
        let text = routing_warning(&["ANTHROPIC_BASE_URL", "CLAUDE_CODE_USE_VERTEX"]).unwrap();
        assert!(text.contains("ANTHROPIC_BASE_URL") && text.contains("CLAUDE_CODE_USE_VERTEX"), "{text}");
        // The caller only ever passes names; a value read through the injected env cannot reach the text.
        let env = |n: &str| (n == "ANTHROPIC_BASE_URL").then(|| "https://secret-gateway.example/KEY123".to_string());
        let names = crate::launcher::routing_env_names(&env);
        let text = routing_warning(&names).unwrap();
        assert!(!text.contains("secret-gateway") && !text.contains("KEY123"), "{text}");
    }

    #[test]
    fn unpriced_text_only_when_needed() {
        assert_eq!(unpriced_text(0), None);
        assert_eq!(unpriced_text(1).unwrap(), "1 Anfrage nicht bewertet");
        assert_eq!(unpriced_text(1234).unwrap(), "1.234 Anfragen nicht bewertet");
    }

    #[test]
    fn request_counts_agree_with_the_number() {
        use crate::i18n::with_language;
        assert_eq!(requests_text(1), "1 Anfrage");
        assert_eq!(requests_text(1234), "1.234 Anfragen");
        with_language("en", || {
            assert_eq!(requests_text(1), "1 request");
            assert_eq!(requests_text(1234), "1,234 requests");
        });
    }

    #[test]
    fn dates_follow_the_language_pattern() {
        use crate::i18n::with_language;
        let d = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        assert_eq!(format_date_by(d, "format.date", "%Y-%m-%d"), "05.11.2026");
        with_language("en", || assert_eq!(format_date_by(d, "format.date", "%Y-%m-%d"), "2026-11-05"));
        assert_eq!(format_date_by(d, "format.no_such_pattern", "%Y-%m-%d"), "2026-11-05");
    }

    #[test]
    fn progress_text_shows_counts() {
        assert_eq!(progress_text(3, 40), "Lese Protokolle … 3 / 40 Dateien");
    }

    #[test]
    fn outcome_messages_cover_all_but_confirmation() {
        assert!(outcome_message(Mode::Max, LaunchOutcome::NeedsConfirmation).is_none());
        for o in [
            LaunchOutcome::Started,
            LaunchOutcome::CloseTimedOut,
            LaunchOutcome::ProcessCheckFailed,
            LaunchOutcome::MissingKey,
            LaunchOutcome::ObsidianNotFound,
            LaunchOutcome::ObsidianUnavailable(crate::paths::PathError::NoHome),
        ] {
            assert!(outcome_message(Mode::ApiKey, o).is_some());
        }
        assert!(
            outcome_message(Mode::Max, LaunchOutcome::ObsidianNotFound)
                .unwrap()
                .contains("konnte nicht gestartet werden")
        );
        let missing = crate::paths::PathError::ConfiguredMissing(r"D:\Apps\Obsidian.exe".into());
        let text = outcome_message(Mode::Max, LaunchOutcome::ObsidianUnavailable(missing)).unwrap();
        assert!(text.contains(r"D:\Apps\Obsidian.exe") && text.contains("Einstellungen"), "{text}");
    }

    #[test]
    fn english_counts_use_a_comma() {
        use crate::i18n::with_language;
        assert_eq!(with_language("en", || format_count(1234)), "1,234");
        assert_eq!(with_language("en", || format_count(1_234_567)), "1,234,567");
        assert_eq!(with_language("en", || format_count(999)), "999");
        // German stays on the dot.
        assert_eq!(with_language("de", || format_count(1234)), "1.234");
        assert_eq!(format_count(1234), "1.234");
    }

    #[test]
    fn unpriced_text_is_singular_and_plural_in_english() {
        use crate::i18n::with_language;
        with_language("en", || {
            assert_eq!(unpriced_text(0), None);
            assert_eq!(unpriced_text(1).unwrap(), "1 request not priced");
            assert_eq!(unpriced_text(2).unwrap(), "2 requests not priced");
            assert_eq!(unpriced_text(1234).unwrap(), "1,234 requests not priced");
        });
    }

    #[test]
    fn routing_warning_agrees_with_the_number_of_names() {
        use crate::i18n::with_language;
        with_language("en", || {
            assert!(routing_warning(&["A"]).unwrap().starts_with("Warning: A is set"));
            assert!(routing_warning(&["A", "B"]).unwrap().starts_with("Warning: A, B are set"));
        });
        assert!(routing_warning(&["A"]).unwrap().starts_with("Achtung: A ist gesetzt"));
        assert!(routing_warning(&["A", "B"]).unwrap().starts_with("Achtung: A, B sind gesetzt und können"));
    }

    #[test]
    fn pause_text_follows_the_language() {
        use crate::i18n::with_language;
        let tz = FixedOffset::east_opt(3600).unwrap();
        let at = |d: u32, h: u32, m: u32| tz.with_ymd_and_hms(2026, 11, d, h, m, 0).unwrap();
        with_language("en", || {
            assert_eq!(pause_text(at(12, 14, 30), at(8, 9, 0), "seven_day"), "Max paused until Thu, Nov 12 14:30 (weekly limit)");
            assert_eq!(pause_text(at(8, 17, 5), at(8, 9, 0), "five_hour"), "Max paused until 17:05 (5-hour limit)");
        });
    }

    #[test]
    fn a_broken_date_pattern_in_a_language_file_falls_back() {
        // A pattern from a user file can be invalid; formatting must not panic.
        crate::i18n::isolate();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("xx.json"),
            r#"{"_meta":{"code":"xx","name":"Test","format":1},"format.date_short":"%Q %","format.thousands":"'"}"#,
        )
        .unwrap();
        assert!(crate::i18n::scan_folder(dir.path()).is_empty());
        crate::i18n::with_language("xx", || {
            let tz = FixedOffset::east_opt(3600).unwrap();
            let t = tz.with_ymd_and_hms(2026, 11, 12, 14, 30, 0).unwrap();
            assert_eq!(format_time_by(&t, "format.date_short", "%m-%d"), "11-12");
            // A key nobody defines uses the fallback too.
            assert_eq!(format_time_by(&t, "format.no_such_pattern", "%m-%d"), "11-12");
            assert_eq!(format_count(1234), "1'234");
        });
    }
}
