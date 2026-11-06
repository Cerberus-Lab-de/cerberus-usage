//! Pure logic behind the window's new cards and the settings panel: what each card says, how amounts are
//! typed and parsed, and how settings edits are applied. No egui here, so all of it is unit-tested.

use std::path::Path;

use chrono::{DateTime, Local, Utc};

use crate::analytics::{ApiError, ApiStatus};
use crate::balance::{Alert, BalanceStatus};
use crate::format::format_time_by;
use crate::i18n::{tr, tr_args};
use crate::pricing::format_usd;
use crate::settings::{self, Baseline, Settings, TopUp};

const NANO_PER_USD: u64 = 1_000_000_000;
const MAX_DECIMALS: usize = 9;

/// Text of an API error. One source: the error's `Display`.
pub fn api_error_text(error: &ApiError) -> String {
    error.to_string()
}

/// `-$1.23` for a negative estimate, otherwise like [`format_usd`].
pub fn format_usd_signed(nano: i128) -> String {
    let magnitude = u64::try_from(nano.unsigned_abs()).unwrap_or(u64::MAX);
    if nano < 0 { format!("-{}", format_usd(magnitude)) } else { format_usd(magnitude) }
}

fn date_text(t: DateTime<Utc>) -> String {
    format_time_by(&t.with_timezone(&Local), "format.date", "%Y-%m-%d")
}

fn datetime_text(t: DateTime<Local>) -> String {
    format_time_by(&t, "format.datetime_short", "%Y-%m-%d %H:%M")
}

// ---- amounts -------------------------------------------------------------------------------------

/// Parses "65", "65,03", "65.03", "$65.03" into nano-USD with integer math only.
pub fn parse_usd_nano(input: &str) -> Result<u64, String> {
    let invalid = || tr("cards.amount.invalid");
    let trimmed = input.trim();
    let text = trimmed
        .strip_prefix('$')
        .or_else(|| trimmed.strip_suffix('$'))
        .unwrap_or(trimmed)
        .trim();
    if text.is_empty() {
        return Err(tr("cards.amount.empty"));
    }
    if text.starts_with('-') {
        return Err(tr("cards.amount.negative"));
    }
    let (whole, frac) = text.split_once([',', '.']).unwrap_or((text, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if (whole.is_empty() && frac.is_empty()) || !digits(whole) || !digits(frac) {
        return Err(invalid());
    }
    // "1.000" could be one thousand (German) or exactly one (English): never guess with money.
    if frac.len() == 3 {
        return Err(tr("cards.amount.ambiguous"));
    }
    if frac.len() > MAX_DECIMALS {
        return Err(tr("cards.amount.decimals"));
    }
    let too_big = || tr("cards.amount.too_big");
    let whole: u64 = if whole.is_empty() { 0 } else { whole.parse().map_err(|_| too_big())? };
    let frac: u64 = if frac.is_empty() { 0 } else { format!("{frac:0<9}").parse().map_err(|_| invalid())? };
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
        ApiCard::Error(error.unwrap_or_else(|| tr("cards.api.no_data")))
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
        details.push(tr_args(
            "cards.balance.baseline",
            &[("amount", &format_usd(base.amount_nano)), ("date", &date_text(base.at))],
        ));
        let added: u64 = settings
            .topups
            .iter()
            .filter(|t| t.at >= base.at)
            .fold(0u64, |acc, t| acc.saturating_add(t.amount_nano));
        if added > 0 {
            details.push(tr_args("cards.balance.topups", &[("amount", &format_usd(added))]));
        }
    }
    if let Some(cost) = balance.cost_since_nano {
        details.push(tr_args("cards.balance.cost", &[("amount", &format_usd(cost))]));
    }
    let note = (!balance.cost_deducted).then(|| {
        let date = base.map_or_else(
            || format_time_by(&balance.updated, "format.date", "%Y-%m-%d"),
            |b| date_text(b.at),
        );
        let key = match &balance.error {
            None | Some(ApiError::NotAvailable) => "cards.balance.note.not_available",
            Some(ApiError::BaselineTooOld) => "cards.balance.note.baseline_too_old",
            Some(_) => "cards.balance.note.failed",
        };
        tr_args(key, &[("date", &date)])
    });
    BalanceCard::Estimate {
        value: format_usd_signed(balance.estimate_nano),
        deducted: balance.cost_deducted,
        details,
        note,
    }
}

/// Shown next to the limit fields while no Cost API answers: the budget warning compares the API
/// month cost with the limit, so without it nothing can ever fire.
pub fn inert_limits_text() -> String {
    tr("cards.inert_limits_hint")
}

/// `Some` when the Cost API is absent or not available, i.e. the budget warning is inert.
pub fn inert_limits_hint(api: Option<&ApiStatus>) -> Option<String> {
    match api {
        None => Some(inert_limits_text()),
        Some(a) if a.error == Some(ApiError::NotAvailable) => Some(inert_limits_text()),
        Some(_) => None,
    }
}

/// Banner text for an alert. The Max pause has its own banner and returns `None`.
pub fn alert_banner_text(alert: &Alert) -> Option<String> {
    match alert {
        Alert::BudgetExceeded => Some(tr("cards.alert.budget")),
        Alert::LowBalance => Some(tr("cards.alert.low_balance")),
        Alert::MaxPaused(_) => None,
    }
}

/// Whether a text field commits now: only a user-edited, changed value, and only on Enter or blur.
/// Half-typed input never reaches the file because nothing commits while the field has focus.
/// "Changed" compares the parsed amounts, so `65,03` against a stored `65.03` is no change.
pub fn should_commit(edited: bool, lost_focus: bool, enter: bool, input: &str, current: &str) -> bool {
    edited && !same_amount(input, current) && (lost_focus || enter)
}

/// True when both texts are valid amounts with the same nano value, or are the same text.
fn same_amount(input: &str, current: &str) -> bool {
    match (parse_committed_nano(input), parse_committed_nano(current)) {
        (Ok(a), Ok(b)) => a == b,
        _ => input == current,
    }
}

/// Amount for a *commit* (baseline, top-up, limits). Stricter than [`parse_usd_nano`]: a dangling
/// decimal separator ("12,") looks half-typed, so it is refused instead of saved as 12.
fn parse_committed_nano(input: &str) -> Result<u64, String> {
    let text = input.trim().trim_end_matches('$').trim_end();
    if text.ends_with([',', '.']) {
        return Err(tr("cards.amount.incomplete"));
    }
    parse_usd_nano(input)
}

fn parse_committed_optional(input: &str) -> Result<Option<u64>, String> {
    if input.trim().is_empty() { Ok(None) } else { parse_committed_nano(input).map(Some) }
}

// ---- settings edits ------------------------------------------------------------------------------

/// New starting balance, dated `now`. Existing top-ups stay (those before `now` are ignored by the estimate).
pub fn with_baseline(s: &Settings, input: &str, now: DateTime<Utc>) -> Result<Settings, String> {
    let amount_nano = parse_committed_nano(input)?;
    Ok(Settings { baseline: Some(Baseline { amount_nano, at: now }), ..s.clone() })
}

pub fn with_topup(s: &Settings, input: &str, now: DateTime<Utc>) -> Result<Settings, String> {
    let amount_nano = parse_committed_nano(input)?;
    if amount_nano == 0 {
        return Err(tr("cards.topup_zero"));
    }
    let mut out = s.clone();
    out.topups.push(TopUp { amount_nano, at: now });
    Ok(out)
}

/// The settings with the language the user picked in the drawer. Nothing else changes.
pub fn with_chosen_language(s: &Settings, code: &str) -> Settings {
    Settings { language: Some(code.to_string()), ..s.clone() }
}

/// Removes the first top-up equal to `entry`. `None` when the file no longer has it (changed meanwhile).
pub fn without_topup(s: &Settings, entry: &TopUp) -> Option<Settings> {
    let index = s.topups.iter().position(|t| t == entry)?;
    let mut out = s.clone();
    out.topups.remove(index);
    Some(out)
}

/// Both limits are optional; empty input clears one. `None` leaves that limit as it is in `s`
/// (the field was not edited), so a stale field can never overwrite the file.
pub fn with_limits(s: &Settings, budget: Option<&str>, low_balance: Option<&str>) -> Result<Settings, String> {
    let mut out = s.clone();
    if let Some(text) = budget {
        out.monthly_budget_nano = parse_committed_optional(text).map_err(|e| tr_args("cards.limits.budget_error", &[("error", &e)]))?;
    }
    if let Some(text) = low_balance {
        out.low_balance_nano = parse_committed_optional(text).map_err(|e| tr_args("cards.limits.balance_error", &[("error", &e)]))?;
    }
    Ok(out)
}

/// Serialises every load-edit-save cycle in this process, so two writers cannot lose each other's change.
static SETTINGS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Re-reads the file, applies `edit` and saves. A broken file is never overwritten; the error names the file.
pub fn update_settings(
    path: &Path,
    edit: impl FnOnce(&Settings) -> Result<Settings, String>,
) -> Result<Settings, String> {
    // Poison is harmless here: the guarded data is `()`, and the file is only replaced atomically.
    let _guard = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (current, error) = settings::load(path);
    if let Some(error) = error {
        return Err(tr_args("cards.file_error", &[("error", &error), ("path", &path.display().to_string())]));
    }
    let updated = edit(&current)?;
    settings::save(path, &updated)
        .map_err(|_| tr_args("cards.save_failed", &[("path", &path.display().to_string())]))?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choosing_a_language_stores_its_code() {
        let before = Settings { keys_plaintext: true, monthly_budget_nano: Some(5), ..Settings::default() };
        let after = with_chosen_language(&before, "en");
        assert_eq!(after.language.as_deref(), Some("en"));
        // Everything else is carried over untouched.
        assert!(after.keys_plaintext);
        assert_eq!(after.monthly_budget_nano, Some(5));
        assert_eq!(with_chosen_language(&after, "de").language.as_deref(), Some("de"));
        assert_eq!(before.language, None, "the input is not changed");
    }
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
            "1.000",
            "1,000",
            "65,123", // three decimals: ambiguous with a thousands separator
        ];
        for input in bad {
            let err = parse_usd_nano(input).expect_err(input);
            assert!(!err.is_empty(), "German message expected for {input:?}");
        }
        assert!(parse_usd_nano("-5").unwrap_err().contains("negativ"));
        assert!(parse_usd_nano("1.000").unwrap_err().contains("65,03"));
        assert_eq!(parse_usd_nano("1.00"), Ok(1_000_000_000));
        assert_eq!(parse_usd_nano("1,0000"), Ok(1_000_000_000));
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
            since_error: None,
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
    fn balance_note_names_the_real_reason() {
        let note_for = |error: Option<ApiError>| {
            let mut b = bal(75_000_000_000, false);
            b.error = error;
            match balance_card(Some(&b), &with_base()) {
                BalanceCard::Estimate { note, .. } => note.unwrap(),
                other => panic!("{other:?}"),
            }
        };
        assert!(note_for(Some(ApiError::NotAvailable)).contains("Kosten-API nicht verfügbar"));
        let old = note_for(Some(ApiError::BaselineTooOld));
        assert!(old.contains("Startwert zu alt, bitte neu eintragen"), "{old}");
        assert!(!old.contains("nicht verfügbar"), "{old}");
        let other = note_for(Some(ApiError::RateLimited));
        assert!(other.contains("nicht abrufbar") && !other.contains("Kosten-API nicht verfügbar"), "{other}");
    }

    #[test]
    fn limit_hint_only_while_the_cost_api_is_absent_or_unavailable() {
        assert_eq!(inert_limits_hint(None), Some(inert_limits_text()));
        assert_eq!(inert_limits_hint(Some(&status(false, Some(ApiError::NotAvailable)))), Some(inert_limits_text()));
        assert_eq!(inert_limits_hint(Some(&status(true, None))), None);
        assert_eq!(inert_limits_hint(Some(&status(true, Some(ApiError::RateLimited)))), None);
        assert!(inert_limits_text().contains("Budget-Warnung") && inert_limits_text().contains("Guthaben-Warnung"));
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
        let entry = s.topups[0].clone();
        let s = without_topup(&s, &entry).unwrap();
        assert!(s.topups.is_empty());
        assert_eq!(without_topup(&s, &entry), None);

        let s = with_limits(&s, Some("20"), Some("")).unwrap();
        assert_eq!((s.monthly_budget_nano, s.low_balance_nano), (Some(20_000_000_000), None));
        assert!(with_limits(&s, Some("20"), Some("x")).is_err());
    }

    #[test]
    fn untouched_limit_fields_keep_the_value_from_the_file() {
        let s = Settings { monthly_budget_nano: Some(5), low_balance_nano: Some(7), ..Settings::default() };
        let same = with_limits(&s, None, None).unwrap();
        assert_eq!(same, s);
        let only_budget = with_limits(&s, Some("1"), None).unwrap();
        assert_eq!((only_budget.monthly_budget_nano, only_budget.low_balance_nano), (Some(1_000_000_000), Some(7)));
    }

    #[test]
    fn removing_a_topup_targets_the_entry_not_the_position() {
        let a = TopUp { amount_nano: 1_000_000_000, at: utc("2026-11-02T10:00:00Z") };
        let b = TopUp { amount_nano: 2_000_000_000, at: utc("2026-11-03T10:00:00Z") };
        let inserted = TopUp { amount_nano: 9_000_000_000, at: utc("2026-11-01T10:00:00Z") };
        // The UI showed [a, b] and wants to remove b (index 1); the file changed to [inserted, a, b].
        let file = Settings { topups: vec![inserted.clone(), a.clone(), b.clone()], ..Settings::default() };
        let after = without_topup(&file, &b).unwrap();
        assert_eq!(after.topups, vec![inserted, a]);
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

    #[test]
    fn commit_only_on_enter_or_blur_with_change() {
        // Unchanged text that loses focus: nothing to save.
        assert!(!should_commit(true, true, false, "65.03", "65.03"));
        // Same value in the other decimal notation: no commit, so the baseline date stays.
        assert!(!should_commit(true, true, false, "65,03", "65.03"));
        assert!(!should_commit(true, false, true, "65.03", "65,03"));
        assert!(!should_commit(true, true, false, "65,30", "65.3"));
        assert!(should_commit(true, true, false, "65,04", "65.03"));
        // Not a valid amount is never "the same": it commits so the error is shown.
        assert!(should_commit(true, true, false, "65.", "65"));
        // Changed but still typing: no commit.
        assert!(!should_commit(true, false, false, "65.0", "65.03"));
        // Changed and Enter / blur: commit.
        assert!(should_commit(true, false, true, "70", "65.03"));
        assert!(should_commit(true, true, false, "70", "65.03"));
        // Never edited by the user (a reload filled it): never commit.
        assert!(!should_commit(false, true, true, "70", "65.03"));
    }

    #[test]
    fn half_typed_amount_keeps_old_value_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        update_settings(&path, |s| with_limits(s, Some("20"), Some("5"))).unwrap();
        let before = std::fs::read(&path).unwrap();
        for typed in ["12,", "12.", "1.000", "1,000", "", "abc"] {
            let budget = update_settings(&path, |s| with_limits(s, Some(typed), None));
            // An empty optional limit is a valid "clear"; everything else here must be refused.
            if typed.is_empty() {
                assert!(budget.is_ok());
                std::fs::write(&path, &before).unwrap();
                continue;
            }
            assert!(budget.is_err(), "{typed:?}");
            assert_eq!(std::fs::read(&path).unwrap(), before, "file changed for {typed:?}");
            assert!(update_settings(&path, |s| with_limits(s, None, Some(typed))).is_err(), "{typed:?}");
            assert!(update_settings(&path, |s| with_baseline(s, typed, Utc::now())).is_err(), "{typed:?}");
            assert!(update_settings(&path, |s| with_topup(s, typed, Utc::now())).is_err(), "{typed:?}");
            assert_eq!(std::fs::read(&path).unwrap(), before, "file changed for {typed:?}");
        }
        assert!(with_limits(&Settings::default(), Some("12,"), None).unwrap_err().contains("Unvollständig"));
    }

    #[test]
    fn empty_optional_limit_clears_only_that_limit() {
        let s = Settings { monthly_budget_nano: Some(5), low_balance_nano: Some(7), ..Settings::default() };
        let no_budget = with_limits(&s, Some(" "), None).unwrap();
        assert_eq!((no_budget.monthly_budget_nano, no_budget.low_balance_nano), (None, Some(7)));
        let no_low = with_limits(&s, None, Some("")).unwrap();
        assert_eq!((no_low.monthly_budget_nano, no_low.low_balance_nano), (Some(5), None));
    }
}
