//! Estimated credit balance and alerts.
//!
//! The Console balance is not available through the API, so it is estimated:
//! baseline + top-ups since the baseline - API cost since the baseline.
//!
//! The cost is requested with hourly buckets and the API rounds the start down to the full
//! hour, so the deduction may include up to one hour of cost before `baseline.at`. The
//! estimate can therefore be too low by that amount; the UI labels it "geschätzt".

use chrono::{DateTime, Local, Utc};

use crate::analytics::{ApiError, ApiStatus};
use crate::report::Pause;
use crate::settings::{Baseline, Settings, TopUp};

/// Baseline plus top-ups made at or after the baseline, minus the cost since the baseline.
/// May be negative (the user's baseline can be stale or the hour rounding can overshoot).
pub fn estimate(base: &Baseline, topups: &[TopUp], cost_since: u64) -> i128 {
    let added: i128 = topups
        .iter()
        .filter(|t| t.at >= base.at)
        .map(|t| i128::from(t.amount_nano))
        .sum();
    i128::from(base.amount_nano) + added - i128::from(cost_since)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Alert {
    /// API cost this month reached the monthly budget.
    BudgetExceeded,
    /// Estimated balance is at or below the warning level.
    LowBalance,
    /// Max plan is rate-limited until the given time.
    MaxPaused(DateTime<Utc>),
}

/// `month_cost_nano` is the API month cost only (`None` without API data); the local log
/// estimate must not be used, it includes Max usage that is not billed.
pub fn alerts(
    settings: &Settings,
    month_cost_nano: Option<u64>,
    balance: Option<i128>,
    pause: Option<&Pause>,
    now: DateTime<Utc>,
) -> Vec<Alert> {
    let mut out = Vec::new();
    if let (Some(budget), Some(cost)) = (settings.monthly_budget_nano, month_cost_nano)
        && cost >= budget
    {
        out.push(Alert::BudgetExceeded);
    }
    // An undeducted estimate is an upper bound, so "at or below the level" never fires falsely.
    if let (Some(level), Some(balance)) = (settings.low_balance_nano, balance)
        && balance <= i128::from(level)
    {
        out.push(Alert::LowBalance);
    }
    if let Some(p) = pause
        && p.until > now
    {
        out.push(Alert::MaxPaused(p.until));
    }
    out
}

/// The balance as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceStatus {
    pub estimate_nano: i128,
    /// False when no API cost is known (no admin key, not available, or no success yet): the
    /// estimate is then only baseline + top-ups, an upper bound.
    pub cost_deducted: bool,
    pub cost_since_nano: Option<u64>,
    /// Time of the last successful API poll, or of this calculation without API data.
    pub updated: DateTime<Local>,
    pub error: Option<ApiError>,
}

/// `None` without a baseline. `api` carries the cost since the baseline (see `ApiPoller::set_since`).
pub fn status(settings: &Settings, api: Option<&ApiStatus>, now: DateTime<Local>) -> Option<BalanceStatus> {
    let base = settings.baseline.as_ref()?;
    let cost = api.and_then(|a| a.since_nano);
    Some(BalanceStatus {
        estimate_nano: estimate(base, &settings.topups, cost.unwrap_or(0)),
        cost_deducted: cost.is_some(),
        cost_since_nano: cost,
        updated: api.map_or(now, |a| a.updated),
        // The baseline request's own error comes first; it only concerns the balance.
        error: api.and_then(|a| a.since_error.clone().or_else(|| a.error.clone())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    const NOW: &str = "2026-11-05T12:00:00Z";

    fn base() -> Baseline {
        Baseline { amount_nano: 50_000_000_000, at: utc("2026-11-01T00:00:00Z") }
    }

    fn topup(amount_nano: u64, at: &str) -> TopUp {
        TopUp { amount_nano, at: utc(at) }
    }

    #[test]
    fn estimate_adds_topups_and_subtracts_cost() {
        let t = [topup(10_000_000_000, "2026-11-02T00:00:00Z"), topup(5_000_000_000, "2026-11-03T00:00:00Z")];
        assert_eq!(estimate(&base(), &t, 12_000_000_000), 53_000_000_000);
    }

    #[test]
    fn topup_before_baseline_is_ignored() {
        let t = [topup(10_000_000_000, "2026-10-31T23:59:59Z"), topup(1_000_000_000, "2026-11-01T00:00:00Z")];
        assert_eq!(estimate(&base(), &t, 0), 51_000_000_000);
    }

    #[test]
    fn estimate_can_go_negative() {
        assert_eq!(estimate(&base(), &[], 70_000_000_000), -20_000_000_000);
    }

    #[test]
    fn estimate_survives_extreme_values() {
        let b = Baseline { amount_nano: u64::MAX, at: utc("2026-11-01T00:00:00Z") };
        let t = [topup(u64::MAX, "2026-11-02T00:00:00Z")];
        assert_eq!(estimate(&b, &t, u64::MAX), i128::from(u64::MAX));
    }

    fn settings_with(budget: Option<u64>, low: Option<u64>) -> Settings {
        Settings { monthly_budget_nano: budget, low_balance_nano: low, ..Settings::default() }
    }

    #[test]
    fn budget_alert_at_exact_limit() {
        let s = settings_with(Some(20_000_000_000), None);
        let now = utc(NOW);
        assert_eq!(alerts(&s, Some(20_000_000_000), None, None, now), vec![Alert::BudgetExceeded]);
        assert!(alerts(&s, Some(19_999_999_999), None, None, now).is_empty());
    }

    #[test]
    fn no_budget_alert_without_api_cost() {
        let s = settings_with(Some(1), None);
        assert!(alerts(&s, None, None, None, utc(NOW)).is_empty());
    }

    #[test]
    fn low_balance_alert_at_or_below_level_including_negative() {
        let s = settings_with(None, Some(5_000_000_000));
        let now = utc(NOW);
        assert_eq!(alerts(&s, None, Some(5_000_000_000), None, now), vec![Alert::LowBalance]);
        assert_eq!(alerts(&s, None, Some(-1), None, now), vec![Alert::LowBalance]);
        assert!(alerts(&s, None, Some(5_000_000_001), None, now).is_empty());
        assert!(alerts(&s, None, None, None, now).is_empty());
    }

    #[test]
    fn no_alerts_without_settings() {
        let now = utc(NOW);
        let pause = None;
        assert!(alerts(&Settings::default(), Some(u64::MAX), Some(i128::MIN), pause, now).is_empty());
    }

    #[test]
    fn max_paused_alert_from_report() {
        let now = utc(NOW);
        let until = now + Duration::hours(2);
        let pause = Pause { until, limit_type: "five_hour".to_string() };
        assert_eq!(alerts(&Settings::default(), None, None, Some(&pause), now), vec![Alert::MaxPaused(until)]);
    }

    #[test]
    fn stale_pause_gives_no_alert() {
        let now = utc(NOW);
        let past = Pause { until: now - Duration::minutes(1), limit_type: "five_hour".to_string() };
        assert!(alerts(&Settings::default(), None, None, Some(&past), now).is_empty());
        let exactly = Pause { until: now, limit_type: "five_hour".to_string() };
        assert!(alerts(&Settings::default(), None, None, Some(&exactly), now).is_empty());
    }

    #[test]
    fn alerts_come_in_a_stable_order() {
        let now = utc(NOW);
        let s = settings_with(Some(1), Some(10));
        let pause = Pause { until: now + Duration::hours(1), limit_type: "x".to_string() };
        let got = alerts(&s, Some(1), Some(0), Some(&pause), now);
        assert_eq!(got, vec![Alert::BudgetExceeded, Alert::LowBalance, Alert::MaxPaused(pause.until)]);
    }

    fn api(since: Option<u64>, error: Option<ApiError>) -> ApiStatus {
        ApiStatus {
            today_nano: 0,
            month_nano: 0,
            since_nano: since,
            updated: utc("2026-11-05T11:55:00Z").with_timezone(&Local),
            error,
            since_error: None,
            has_data: true,
        }
    }

    fn with_baseline() -> Settings {
        Settings {
            baseline: Some(base()),
            topups: vec![topup(10_000_000_000, "2026-11-02T00:00:00Z")],
            ..Settings::default()
        }
    }

    #[test]
    fn status_is_none_without_baseline() {
        let now = utc(NOW).with_timezone(&Local);
        assert!(status(&Settings::default(), Some(&api(Some(1), None)), now).is_none());
    }

    #[test]
    fn status_without_api_cost_is_baseline_plus_topups_not_deducted() {
        let now = utc(NOW).with_timezone(&Local);
        for a in [None, Some(api(None, Some(ApiError::NotAvailable)))] {
            let s = status(&with_baseline(), a.as_ref(), now).unwrap();
            assert_eq!(s.estimate_nano, 60_000_000_000);
            assert!(!s.cost_deducted);
            assert_eq!(s.cost_since_nano, None);
            assert_eq!(s.updated, a.as_ref().map_or(now, |x| x.updated));
            assert_eq!(s.error, a.and_then(|x| x.error));
        }
    }

    #[test]
    fn status_with_api_cost_deducts_and_keeps_error_after_a_failure() {
        let now = utc(NOW).with_timezone(&Local);
        let ok = api(Some(12_000_000_000), None);
        let s = status(&with_baseline(), Some(&ok), now).unwrap();
        assert_eq!((s.estimate_nano, s.cost_deducted, s.cost_since_nano), (48_000_000_000, true, Some(12_000_000_000)));
        assert_eq!(s.updated, ok.updated);
        assert_eq!(s.error, None);

        // A later failure keeps the deducted values (ApiPoller keeps them) and reports the error.
        let failed = api(Some(12_000_000_000), Some(ApiError::Server(500)));
        let s = status(&with_baseline(), Some(&failed), now).unwrap();
        assert!(s.cost_deducted);
        assert_eq!(s.estimate_nano, 48_000_000_000);
        assert_eq!(s.error, Some(ApiError::Server(500)));
    }

    #[test]
    fn baseline_request_error_reaches_the_balance_but_totals_error_stays_separate() {
        let now = utc(NOW).with_timezone(&Local);
        let mut a = api(None, None);
        a.since_error = Some(ApiError::BaselineTooOld);
        let s = status(&with_baseline(), Some(&a), now).unwrap();
        assert_eq!(s.error, Some(ApiError::BaselineTooOld));
        assert!(!s.cost_deducted);
    }
}
