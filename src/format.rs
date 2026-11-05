//! Pure label and number formatting for the UI (German texts), kept out of the egui code so it is testable.

use chrono::{DateTime, Datelike, TimeZone, Weekday};

use crate::launcher::{LaunchOutcome, Mode};

/// Text shown next to every cost figure.
pub const COST_NOTE: &str = "geschätzt, Listenpreis";

/// `1234567` -> `1.234.567`.
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push('.');
        }
        out.push(c);
    }
    out
}

fn limit_type_label(limit_type: &str) -> &str {
    match limit_type {
        "five_hour" => "5-Stunden-Limit",
        "seven_day" => "Wochenlimit",
        other => other,
    }
}

/// German weekday abbreviation (no locale dependency).
fn weekday_de(d: Weekday) -> &'static str {
    ["Mo", "Di", "Mi", "Do", "Fr", "Sa", "So"][d.num_days_from_monday() as usize]
}

/// `Max pausiert bis 17:42 (5-Stunden-Limit)`; with the date when the reset is not on the day
/// of `now`: `Max pausiert bis Do, 12.11. 14:30 (Wochenlimit)`. Both times in the same zone (local).
pub fn pause_text<Tz: TimeZone>(until: DateTime<Tz>, now: DateTime<Tz>, limit_type: &str) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let when = if until.date_naive() == now.date_naive() {
        until.format("%H:%M").to_string()
    } else {
        format!("{}, {}", weekday_de(until.weekday()), until.format("%d.%m. %H:%M"))
    };
    format!("Max pausiert bis {when} ({})", limit_type_label(limit_type))
}

/// The short honest line under the mode title. The app cannot see an `apiKeyHelper` or an `env`
/// block in Claude's own settings, so neither mode promises more than the child environment.
pub fn mode_hint(mode: Mode) -> &'static str {
    match mode {
        Mode::Max => "Start ohne API-Key. Es gilt dein Max-Abo, sofern in Claude keine andere Anmeldung eingerichtet ist.",
        Mode::ApiKey => "Start mit deinem API-Key. Abgerechnet wird dieser Key, sofern in Claude keine andere Anmeldung eingerichtet ist.",
    }
}

/// Warning for routing variables that were left untouched. Takes names only, never values.
pub fn routing_warning(names: &[&str]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    Some(format!(
        "Achtung: {} {} gesetzt und kann ändern, wohin Anfragen gehen oder wer sie bezahlt. Die App lässt das unverändert.",
        names.join(", "),
        if names.len() == 1 { "ist" } else { "sind" }
    ))
}

/// `None` when every request had a price.
pub fn unpriced_text(unpriced_requests: u64) -> Option<String> {
    match unpriced_requests {
        0 => None,
        1 => Some("1 Anfrage nicht bewertet".to_string()),
        n => Some(format!("{} Anfragen nicht bewertet", format_count(n))),
    }
}

pub fn progress_text(scanned: usize, total: usize) -> String {
    format!("Lese Protokolle … {scanned} / {total} Dateien")
}

pub fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Max => "Max",
        Mode::ApiKey => "API-Key",
    }
}

/// Message after a finished launch attempt. `None` for `NeedsConfirmation` (handled by a dialog).
/// A spawn io error also arrives as `ObsidianNotFound`, hence the wording.
pub fn outcome_message(mode: Mode, outcome: LaunchOutcome) -> Option<String> {
    match outcome {
        LaunchOutcome::Started => Some(format!("Obsidian wurde im Modus {} gestartet.", mode_label(mode))),
        LaunchOutcome::NeedsConfirmation => None,
        LaunchOutcome::CloseTimedOut => Some(
            "Obsidian hat sich nicht rechtzeitig geschlossen. Es wurde nichts gestartet.".to_string(),
        ),
        LaunchOutcome::ProcessCheckFailed => Some(
            "Ob Obsidian läuft, ließ sich nicht prüfen. Es wurde nichts geschlossen und nichts gestartet. Starte Obsidian bei Bedarf von Hand."
                .to_string(),
        ),
        LaunchOutcome::MissingKey => Some(
            "Kein API-Key gespeichert. Bitte unter Einstellungen einen Key speichern.".to_string(),
        ),
        LaunchOutcome::ObsidianNotFound => Some("Obsidian konnte nicht gestartet werden.".to_string()),
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
    fn mode_hints_do_not_promise_more_than_the_app_can_know() {
        for mode in [Mode::Max, Mode::ApiKey] {
            let hint = mode_hint(mode);
            assert!(!hint.contains("keine API-Kosten"), "{hint}");
            assert!(hint.contains("sofern"), "{hint}");
        }
        assert!(mode_hint(Mode::Max).starts_with("Start ohne API-Key."));
    }

    #[test]
    fn unpriced_text_only_when_needed() {
        assert_eq!(unpriced_text(0), None);
        assert_eq!(unpriced_text(1).unwrap(), "1 Anfrage nicht bewertet");
        assert_eq!(unpriced_text(1234).unwrap(), "1.234 Anfragen nicht bewertet");
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
}
