//! Pure logic behind the window's new cards and the settings panel: what each card says, how amounts are
//! typed and parsed, and how settings edits are applied. No egui here, so all of it is unit-tested.

use std::path::Path;

use chrono::{DateTime, Local, Utc};

use crate::analytics::{ApiError, ApiStatus};
use crate::balance::{Alert, BalanceStatus};
use crate::pricing::format_usd;
use crate::settings::{self, Baseline, Settings, TopUp};

const NANO_PER_USD: u64 = 1_000_000_000;
const MAX_DECIMALS: usize = 9;

/// German text of an API error. One source: the error's `Display`.
pub fn api_error_text(error: &ApiError) -> String {
    error.to_string()
}

/// `-$1.23` for a negative estimate, otherwise like [`format_usd`].
pub fn format_usd_signed(nano: i128) -> String {
    let magnitude = u64::try_from(nano.unsigned_abs()).unwrap_or(u64::MAX);
    if nano < 0 { format!("-{}", format_usd(magnitude)) } else { format_usd(magnitude) }
}

fn date_text(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%d.%m.%Y").to_string()
}

fn datetime_text(t: DateTime<Local>) -> String {
    t.format("%d.%m.%Y %H:%M").to_string()
}

// ---- amounts -------------------------------------------------------------------------------------

/// Parses "65", "65,03", "65.03", "$65.03" into nano-USD with integer math only.
pub fn parse_usd_nano(input: &str) -> Result<u64, String> {
    const INVALID: &str = "Ungültiger Betrag. Erlaubt sind Zahlen wie 65, 65,03 oder 65.03.";
    let trimmed = input.trim();
    let text = trimmed
        .strip_prefix('$')
        .or_else(|| trimmed.strip_suffix('$'))
        .unwrap_or(trimmed)
        .trim();
    if text.is_empty() {
        return Err("Bitte einen Betrag eingeben.".to_string());
    }
    if text.starts_with('-') {
        return Err("Der Betrag darf nicht negativ sein.".to_string());
    }
    let (whole, frac) = text.split_once([',', '.']).unwrap_or((text, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if (whole.is_empty() && frac.is_empty()) || !digits(whole) || !digits(frac) {
        return Err(INVALID.to_string());
    }
    if frac.len() > MAX_DECIMALS {
        return Err("Zu viele Nachkommastellen (höchstens 9).".to_string());
    }
    let too_big = || "Der Betrag ist zu groß.".to_string();
    let whole: u64 = if whole.is_empty() { 0 } else { whole.parse().map_err(|_| too_big())? };
    let frac: u64 = if frac.is_empty() { 0 } else { format!("{frac:0<9}").parse().map_err(|_| INVALID.to_string())? };
    whole.checked_mul(NANO_PER_USD).and_then(|w| w.checked_add(frac)).ok_or_else(too_big)
}

/// Empty input clears the value.
pub fn parse_optional_usd_nano(input: &str) -> Result<Option<u64>, String> {
    if input.trim().is_empty() { Ok(None) } else { parse_usd_nano(input).map(Some) }
}

// ---- cards ---------------------------------------------------------------------------------------

/// "API-Kosten laut Anthropic".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiCard {
    /// No admin key stored.
    NoKey,
    /// Key stored, first poll not answered yet.
    Pending,
    /// No successful poll yet: only the error text, no figures.
    Error(String),
    Data { today: String, month: String, updated: String, error: Option<String> },
}

pub fn api_card(admin_key_stored: Option<bool>, api: Option<&ApiStatus>) -> ApiCard {
    // A deleted key wins over a stale snapshot; an unreadable store without any status counts as no key.
    if admin_key_stored == Some(false) || (admin_key_stored.is_none() && api.is_none()) {
        return ApiCard::NoKey;
    }
    let Some(api) = api else { return ApiCard::Pending };
    let error = api.error.as_ref().map(api_error_text);
    if api.has_data {
        ApiCard::Data {
            today: format_usd(api.today_nano),
            month: format_usd(api.month_nano),
            updated: datetime_text(api.updated),
            error,
        }
    } else {
        ApiCard::Error(error.unwrap_or_else(|| "Noch keine Daten von der API.".to_string()))
    }
}

/// "Guthaben (geschätzt)".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BalanceCard {
    NoBaseline,
    Estimate {
        value: String,
        /// False: only baseline + top-ups, the API cost could not be subtracted.
        deducted: bool,
        details: Vec<String>,
        note: Option<String>,
    },
}

pub fn balance_card(balance: Option<&BalanceStatus>, settings: &Settings) -> BalanceCard {
    let Some(balance) = balance else { return BalanceCard::NoBaseline };
    let base = settings.baseline.as_ref();
    let mut details = Vec::new();
    if let Some(base) = base {
        details.push(format!("Startwert {} vom {}", format_usd(base.amount_nano), date_text(base.at)));
        let added: u64 = settings
            .topups
            .iter()
            .filter(|t| t.at >= base.at)
            .fold(0u64, |acc, t| acc.saturating_add(t.amount_nano));
        if added > 0 {
            details.push(format!("Aufladungen seit Startwert: {}", format_usd(added)));
        }
    }
    if let Some(cost) = balance.cost_since_nano {
        details.push(format!("API-Kosten seit Startwert: {}", format_usd(cost)));
    }
    let note = (!balance.cost_deducted).then(|| {
        let date = base.map_or_else(|| balance.updated.format("%d.%m.%Y").to_string(), |b| date_text(b.at));
        format!(
            "Stand vom {date}, ohne Kostenabzug: Kosten-API nicht verfügbar. Startwert bei Bedarf neu eintragen."
        )
    });
    BalanceCard::Estimate {
        value: format_usd_signed(balance.estimate_nano),
        deducted: balance.cost_deducted,
        details,
        note,
    }
}

/// Banner text for an alert. The Max pause has its own banner and returns `None`.
pub fn alert_banner_text(alert: &Alert) -> Option<&'static str> {
    match alert {
        Alert::BudgetExceeded => Some("Monatsbudget erreicht: Die API-Kosten laut Anthropic liegen über deiner Grenze."),
        Alert::LowBalance => Some("Guthaben (geschätzt) unter deiner Warngrenze."),
        Alert::MaxPaused(_) => None,
    }
}

// ---- settings edits ------------------------------------------------------------------------------

/// New starting balance, dated `now`. Existing top-ups stay (those before `now` are ignored by the estimate).
pub fn with_baseline(s: &Settings, input: &str, now: DateTime<Utc>) -> Result<Settings, String> {
    let amount_nano = parse_usd_nano(input)?;
    Ok(Settings { baseline: Some(Baseline { amount_nano, at: now }), ..s.clone() })
}

pub fn with_topup(s: &Settings, input: &str, now: DateTime<Utc>) -> Result<Settings, String> {
    let amount_nano = parse_usd_nano(input)?;
    if amount_nano == 0 {
        return Err("Eine Aufladung muss größer als 0 sein.".to_string());
    }
    let mut out = s.clone();
    out.topups.push(TopUp { amount_nano, at: now });
    Ok(out)
}

pub fn without_topup(s: &Settings, index: usize) -> Settings {
    let mut out = s.clone();
    if index < out.topups.len() {
        out.topups.remove(index);
    }
    out
}

/// Both limits are optional; empty input clears one.
pub fn with_limits(s: &Settings, budget: &str, low_balance: &str) -> Result<Settings, String> {
    Ok(Settings {
        monthly_budget_nano: parse_optional_usd_nano(budget).map_err(|e| format!("Monatsbudget: {e}"))?,
        low_balance_nano: parse_optional_usd_nano(low_balance).map_err(|e| format!("Guthaben-Grenze: {e}"))?,
        ..s.clone()
    })
}

/// Re-reads the file, applies `edit` and saves. A broken file is never overwritten; the error names the file.
pub fn update_settings(
    path: &Path,
    edit: impl FnOnce(&Settings) -> Result<Settings, String>,
) -> Result<Settings, String> {
    let (current, error) = settings::load(path);
    if let Some(error) = error {
        return Err(format!("{error}. Datei: {}", path.display()));
    }
    let updated = edit(&current)?;
    settings::save(path, &updated)
        .map_err(|_| format!("Einstellungen konnten nicht gespeichert werden. Datei: {}", path.display()))?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::{ApiError, ApiStatus};
    use crate::balance::{Alert, BalanceStatus};
    use crate::settings::{Baseline, Settings, TopUp};
    use chrono::{DateTime, Local, Utc};

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn amount_parser_table() {
        let ok: &[(&str, u64)] = &[
            ("65", 65_000_000_000),
            ("65,03", 65_030_000_000),
            ("65.03", 65_030_000_000),
            ("  $65,03 ", 65_030_000_000),
            ("65,03 $", 65_030_000_000),
            ("$ 0,5", 500_000_000),
            ("0", 0),
            (".5", 500_000_000),
            ("65.", 65_000_000_000),
            ("0,000000001", 1),
            ("18446744073,709551615", u64::MAX),
        ];
        for (input, want) in ok {
            assert_eq!(parse_usd_nano(input), Ok(*want), "input {input:?}");
        }
        let bad = [
            "",
            "   ",
            "$",
            "abc",
            "65,03,1",
            "1.234,50",
            "6 5",
            "-5",
            "-0,5",
            "+5",
            "1e3",
            "NaN",
            ",",
            "0,0000000001",          // 10 decimals
            "18446744073,709551616", // one nano over u64
            "99999999999999999999",  // overflow
            "65,0x",
        ];
        for input in bad {
            let err = parse_usd_nano(input).expect_err(input);
            assert!(!err.is_empty(), "German message expected for {input:?}");
        }
        assert!(parse_usd_nano("-5").unwrap_err().contains("negativ"));
    }

    #[test]
    fn optional_amount_empty_clears() {
        assert_eq!(parse_optional_usd_nano("  "), Ok(None));
        assert_eq!(parse_optional_usd_nano("20"), Ok(Some(20_000_000_000)));
        assert!(parse_optional_usd_nano("x").is_err());
    }

    #[test]
    fn api_error_text_for_unauthorized() {
        let text = api_error_text(&ApiError::Unauthorized);
        assert!(text.contains("Admin-Key") && text.contains("prüfen"), "{text}");
        assert_eq!(api_error_text(&ApiError::NotAvailable), ApiError::NotAvailable.to_string());
    }

    fn status(has_data: bool, error: Option<ApiError>) -> ApiStatus {
        ApiStatus {
            today_nano: 1_230_000_000,
            month_nano: 45_000_000_000,
            since_nano: None,
            updated: utc("2026-11-05T11:55:00Z").with_timezone(&Local),
            error,
            has_data,
        }
    }

    #[test]
    fn api_card_states() {
        assert_eq!(api_card(Some(false), None), ApiCard::NoKey);
        assert_eq!(api_card(None, None), ApiCard::NoKey);
        assert_eq!(api_card(Some(true), None), ApiCard::Pending);

        // Never succeeded: only the error text, never figures.
        for e in [ApiError::NotAvailable, ApiError::Server(500), ApiError::Unauthorized] {
            assert_eq!(api_card(Some(true), Some(&status(false, Some(e.clone())))), ApiCard::Error(e.to_string()));
        }

        match api_card(Some(true), Some(&status(true, None))) {
            ApiCard::Data { today, month, error, .. } => {
                assert_eq!((today.as_str(), month.as_str()), ("$1.23", "$45.00"));
                assert!(error.is_none());
            }
            other => panic!("{other:?}"),
        }
        match api_card(Some(true), Some(&status(true, Some(ApiError::RateLimited)))) {
            ApiCard::Data { error, .. } => assert_eq!(error, Some(ApiError::RateLimited.to_string())),
            other => panic!("{other:?}"),
        }
    }

    fn bal(nano: i128, deducted: bool) -> BalanceStatus {
        BalanceStatus {
            estimate_nano: nano,
            cost_deducted: deducted,
            cost_since_nano: deducted.then_some(12_000_000_000),
            updated: utc("2026-11-05T11:55:00Z").with_timezone(&Local),
            error: None,
        }
    }

    fn with_base() -> Settings {
        Settings {
            baseline: Some(Baseline { amount_nano: 65_000_000_000, at: utc("2026-11-01T10:00:00Z") }),
            topups: vec![TopUp { amount_nano: 10_000_000_000, at: utc("2026-11-02T10:00:00Z") }],
            ..Settings::default()
        }
    }

    #[test]
    fn balance_card_states() {
        assert_eq!(balance_card(None, &Settings::default()), BalanceCard::NoBaseline);

        let BalanceCard::Estimate { value, deducted, details, note } =
            balance_card(Some(&bal(63_000_000_000, true)), &with_base())
        else {
            panic!("estimate expected")
        };
        assert_eq!(value, "$63.00");
        assert!(deducted && note.is_none());
        let joined = details.join(" | ");
        assert!(joined.contains("$65.00") && joined.contains("01.11.2026"), "{joined}");
        assert!(joined.contains("$10.00") && joined.contains("$12.00"), "{joined}");

        let BalanceCard::Estimate { deducted, note, .. } =
            balance_card(Some(&bal(75_000_000_000, false)), &with_base())
        else {
            panic!("estimate expected")
        };
        assert!(!deducted);
        let note = note.expect("note without cost deduction");
        assert!(note.contains("Stand vom 01.11.2026") && note.contains("ohne Kostenabzug"), "{note}");
        assert!(note.contains("Startwert bei Bedarf neu eintragen"));
    }

    #[test]
    fn negative_estimate_is_shown_with_sign() {
        assert_eq!(format_usd_signed(-1_230_000_000), "-$1.23");
        assert_eq!(format_usd_signed(0), "$0.00");
        assert_eq!(format_usd_signed(i128::MIN), format!("-{}", crate::pricing::format_usd(u64::MAX)));
    }

    #[test]
    fn alert_banner_texts() {
        assert!(alert_banner_text(&Alert::BudgetExceeded).unwrap().contains("Monatsbudget"));
        assert!(alert_banner_text(&Alert::LowBalance).unwrap().contains("Guthaben"));
        // The pause already has its own banner.
        assert!(alert_banner_text(&Alert::MaxPaused(utc("2026-11-05T12:00:00Z"))).is_none());
    }

    #[test]
    fn settings_edits() {
        let now = utc("2026-11-05T12:00:00Z");
        let s = with_baseline(&Settings::default(), "65,03", now).unwrap();
        assert_eq!(s.baseline, Some(Baseline { amount_nano: 65_030_000_000, at: now }));
        assert!(with_baseline(&s, "abc", now).is_err());

        let s = with_topup(&s, "$10", now).unwrap();
        assert_eq!(s.topups, vec![TopUp { amount_nano: 10_000_000_000, at: now }]);
        assert!(with_topup(&s, "0", now).is_err());
        assert!(with_topup(&s, "", now).is_err());
        let s = without_topup(&s, 0);
        assert!(s.topups.is_empty());
        assert_eq!(without_topup(&s, 7), s);

        let s = with_limits(&s, "20", "").unwrap();
        assert_eq!((s.monthly_budget_nano, s.low_balance_nano), (Some(20_000_000_000), None));
        assert!(with_limits(&s, "20", "x").is_err());
    }

    #[test]
    fn update_settings_saves_and_refuses_a_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let now = utc("2026-11-05T12:00:00Z");

        let saved = update_settings(&path, |s| with_baseline(s, "65", now)).unwrap();
        assert_eq!(crate::settings::load(&path), (saved, None));

        // An invalid input saves nothing.
        assert!(update_settings(&path, |s| with_topup(s, "zzz", now)).is_err());
        assert!(crate::settings::load(&path).0.topups.is_empty());

        // A broken file is reported with its location and left untouched.
        std::fs::write(&path, "{ broken").unwrap();
        let err = update_settings(&path, |s| with_baseline(s, "1", now)).unwrap_err();
        assert!(err.contains("settings.json"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ broken");
    }
}
