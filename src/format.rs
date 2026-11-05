//! Pure label and number formatting for the UI (German texts), kept out of the egui code so it is testable.

use chrono::{DateTime, Local};

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

/// `Max pausiert bis 17:42 (5-Stunden-Limit)` (local time).
pub fn pause_text(until: DateTime<Local>, limit_type: &str) -> String {
    format!(
        "Max pausiert bis {} ({})",
        until.format("%H:%M"),
        limit_type_label(limit_type)
    )
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
    use chrono::{TimeZone, Utc};

    #[test]
    fn counts_get_german_thousand_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1.000");
        assert_eq!(format_count(1_234_567), "1.234.567");
    }

    #[test]
    fn pause_text_uses_local_time_and_type() {
        let utc = Utc.with_ymd_and_hms(2026, 11, 4, 16, 42, 0).unwrap();
        let local = utc.with_timezone(&Local);
        let expected = format!("Max pausiert bis {} (5-Stunden-Limit)", local.format("%H:%M"));
        assert_eq!(pause_text(local, "five_hour"), expected);
        assert!(pause_text(local, "unbekannt").ends_with("(unbekannt)"));
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
        let missing = crate::paths::PathError::ConfiguredMissing(r"D:pps\Obsidian.exe".into());
        let text = outcome_message(Mode::Max, LaunchOutcome::ObsidianUnavailable(missing)).unwrap();
        assert!(text.contains(r"D:pps\Obsidian.exe") && text.contains("Einstellungen"), "{text}");
    }
}
