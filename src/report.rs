//! Aggregation of scanned usage records into the figures the UI and alerts show.
//!
//! Costs are summed in nano-USD (saturating); formatting is left to the caller.
//! Day and month boundaries follow the time zone of `now` (the app passes `Local`).

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};

use crate::log_scan::{ScanState, UsageRecord};
use crate::pricing::PriceTable;

/// Number of local days (including today) covered by `Report::by_day`.
const DAY_WINDOW: i64 = 31;

/// Summed token counts and estimated cost of a group of requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bucket {
    pub requests: u64,
    pub input: u64,
    pub cache_read: u64,
    /// `cache_5m + cache_1h`.
    pub cache_write: u64,
    pub output: u64,
    /// Estimated list-price cost in nano-USD (unpriced requests contribute 0).
    pub cost_nano: u64,
    /// Requests whose model/speed/geo has no list price; their tokens are still counted.
    pub unpriced_requests: u64,
}

impl Bucket {
    fn add(&mut self, r: &UsageRecord, cost: Option<u64>) {
        self.requests = self.requests.saturating_add(1);
        self.input = self.input.saturating_add(r.input);
        self.cache_read = self.cache_read.saturating_add(r.cache_read);
        self.cache_write = self
            .cache_write
            .saturating_add(r.cache_5m)
            .saturating_add(r.cache_1h);
        self.output = self.output.saturating_add(r.output);
        match cost {
            Some(c) => self.cost_nano = self.cost_nano.saturating_add(c),
            None => self.unpriced_requests = self.unpriced_requests.saturating_add(1),
        }
    }
}

/// An active rate-limit pause (a rejected quota event whose reset lies in the future).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pause {
    pub until: DateTime<Utc>,
    pub limit_type: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub today: Bucket,
    pub month: Bucket,
    /// Requests in the window (now - 5h, now]; exactly 5h old no longer counts.
    pub last_5h: Bucket,
    /// Current local calendar month (same scope as `month`). Sorted by cost descending, ties by name ascending.
    pub by_project: Vec<(String, Bucket)>,
    /// Current local calendar month (same scope as `month`). Sorted by cost descending, ties by name ascending.
    pub by_model: Vec<(String, Bucket)>,
    /// Local today only. Sorted like `by_project`.
    pub today_by_project: Vec<(String, Bucket)>,
    /// Local today only. Sorted like `by_model`.
    pub today_by_model: Vec<(String, Bucket)>,
    /// Last 31 local days including today, only days with usage, newest day first.
    pub by_day: Vec<(NaiveDate, Bucket)>,
    pub pause: Option<Pause>,
}

fn sorted_by_cost(map: HashMap<String, Bucket>) -> Vec<(String, Bucket)> {
    let mut v: Vec<_> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.cost_nano.cmp(&a.1.cost_nano).then_with(|| a.0.cmp(&b.0)));
    v
}

/// Build the report as seen at `now`. Generic over the time zone so tests can use a fixed offset.
pub fn build<Tz: TimeZone>(state: &ScanState, prices: &PriceTable, now: DateTime<Tz>) -> Report {
    let tz = now.timezone();
    let today = now.date_naive();
    let first_day = today - Duration::days(DAY_WINDOW - 1);
    let now_utc = now.with_timezone(&Utc);
    let window_start = now_utc - Duration::hours(5);

    let mut report = Report::default();
    let mut by_project: HashMap<String, Bucket> = HashMap::new();
    let mut by_model: HashMap<String, Bucket> = HashMap::new();
    let mut by_day: HashMap<NaiveDate, Bucket> = HashMap::new();
    let mut today_by_project: HashMap<String, Bucket> = HashMap::new();
    let mut today_by_model: HashMap<String, Bucket> = HashMap::new();

    for r in state.records.values() {
        let cost = prices.cost_nanousd(r);
        let day = r.ts.with_timezone(&tz).date_naive();

        if day == today {
            report.today.add(r, cost);
            today_by_project.entry(r.project.clone()).or_default().add(r, cost);
            today_by_model.entry(r.model.clone()).or_default().add(r, cost);
        }
        if day.year() == today.year() && day.month() == today.month() {
            report.month.add(r, cost);
            by_project.entry(r.project.clone()).or_default().add(r, cost);
            by_model.entry(r.model.clone()).or_default().add(r, cost);
        }
        if r.ts > window_start && r.ts <= now_utc {
            report.last_5h.add(r, cost);
        }
        if day >= first_day && day <= today {
            by_day.entry(day).or_default().add(r, cost);
        }
    }

    report.by_project = sorted_by_cost(by_project);
    report.by_model = sorted_by_cost(by_model);
    report.today_by_project = sorted_by_cost(today_by_project);
    report.today_by_model = sorted_by_cost(today_by_model);
    let mut days: Vec<_> = by_day.into_iter().collect();
    days.sort_by(|a, b| b.0.cmp(&a.0));
    report.by_day = days;

    report.pause = state
        .quota
        .iter()
        .filter(|e| e.status == "rejected")
        .filter_map(|e| e.resets_at.filter(|&t| t > now_utc).map(|t| (e, t)))
        .max_by_key(|(e, t)| (e.ts, *t))
        .map(|(e, t)| Pause {
            until: t,
            limit_type: e.limit_type.clone().unwrap_or_else(|| "unbekannt".to_string()),
        });

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_scan::QuotaEvent;
    use chrono::FixedOffset;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn cet(s: &str) -> DateTime<FixedOffset> {
        utc(s).with_timezone(&FixedOffset::east_opt(3600).unwrap())
    }

    fn rec(id: &str, ts: &str, model: &str, project: &str) -> UsageRecord {
        UsageRecord {
            message_id: id.into(),
            ts: utc(ts),
            model: model.into(),
            project: project.into(),
            session: "s".into(),
            is_subagent: false,
            input: 0,
            cache_read: 0,
            cache_5m: 0,
            cache_1h: 0,
            output: 0,
            speed: None,
            geo: None,
        }
    }

    fn state(records: Vec<UsageRecord>, quota: Vec<QuotaEvent>) -> ScanState {
        let mut s = ScanState::default();
        for r in records {
            s.records.insert(r.message_id.clone(), r);
        }
        s.quota = quota;
        s
    }

    fn ev(ts: &str, status: &str, resets: Option<&str>, lt: Option<&str>) -> QuotaEvent {
        QuotaEvent {
            ts: utc(ts),
            status: status.into(),
            resets_at: resets.map(utc),
            limit_type: lt.map(String::from),
        }
    }

    #[test]
    fn day_boundary_uses_local_time() {
        // 23:30Z on 04.11. is 00:30 on 05.11. at +01:00.
        let mut r = rec("a", "2026-11-04T23:30:00Z", "claude-opus-5-5", "p");
        r.output = 7;
        let st = state(vec![r], vec![]);
        // 05.11. 09:00 local: the record belongs to today, not to 04.11.
        let rep = build(&st, &PriceTable::embedded(), cet("2026-11-05T08:00:00Z"));
        assert_eq!(rep.today.requests, 1);
        assert_eq!(rep.by_day.len(), 1);
        assert_eq!(rep.by_day[0].0, NaiveDate::from_ymd_opt(2026, 11, 5).unwrap());
        // 04.11. 21:00 local: the record is on a later local day, so it is not "today".
        let rep = build(&st, &PriceTable::embedded(), cet("2026-11-04T20:00:00Z"));
        assert_eq!(rep.today.requests, 0);
        assert_eq!(rep.by_day.len(), 0);
    }

    #[test]
    fn month_excludes_previous_month() {
        let st = state(
            vec![
                rec("a", "2026-10-31T22:30:00Z", "m", "p"), // 31.10. 23:30 local
                rec("b", "2026-10-31T23:30:00Z", "m", "p"), // 01.11. 00:30 local
                rec("c", "2026-11-03T12:00:00Z", "m", "p"),
            ],
            vec![],
        );
        let rep = build(&st, &PriceTable::embedded(), cet("2026-11-04T12:00:00Z"));
        assert_eq!(rep.month.requests, 2);
        assert_eq!(rep.today.requests, 0);
        assert_eq!(rep.by_day.len(), 3);
    }

    #[test]
    fn last_5h_window_is_exact() {
        let st = state(
            vec![
                rec("old", "2026-11-04T07:00:00Z", "m", "p"), // exactly 5h old
                rec("in", "2026-11-04T07:00:01Z", "m", "p"),  // 4h59m59s old
                rec("now", "2026-11-04T12:00:00Z", "m", "p"), // exactly now
                rec("fut", "2026-11-04T12:00:01Z", "m", "p"), // after now
            ],
            vec![],
        );
        let rep = build(&st, &PriceTable::embedded(), cet("2026-11-04T12:00:00Z"));
        assert_eq!(rep.last_5h.requests, 2);
    }

    #[test]
    fn unpriced_counts_tokens_but_not_cost() {
        let mut a = rec("a", "2026-11-04T10:00:00Z", "claude-opus-5-5", "p");
        a.input = 10_000;
        a.cache_read = 40_000;
        let mut b = rec("b", "2026-11-04T10:01:00Z", "unknown-model", "p");
        b.input = 5;
        b.cache_5m = 3;
        b.cache_1h = 4;
        b.cache_read = 2;
        b.output = 1;
        let st = state(vec![a, b], vec![]);
        let rep = build(&st, &PriceTable::embedded(), cet("2026-11-04T12:00:00Z"));
        let t = rep.today;
        assert_eq!(t.requests, 2);
        assert_eq!(t.unpriced_requests, 1);
        assert_eq!(t.cost_nano, 48_000_000);
        assert_eq!(t.input, 10_005);
        assert_eq!(t.cache_read, 40_002);
        assert_eq!(t.cache_write, 7);
        assert_eq!(t.output, 1);
        let unknown = rep.by_model.iter().find(|(m, _)| m == "unknown-model").unwrap();
        assert_eq!(unknown.1.unpriced_requests, 1);
        assert_eq!(unknown.1.cost_nano, 0);
    }

    #[test]
    fn pause_active_only_until_resets_at() {
        let st = state(
            vec![],
            vec![ev(
                "2026-11-04T11:00:00Z",
                "rejected",
                Some("2026-11-04T15:00:00Z"),
                Some("five_hour"),
            )],
        );
        let rep = build(&st, &PriceTable::embedded(), cet("2026-11-04T12:00:00Z"));
        let p = rep.pause.expect("pause active");
        assert_eq!(p.until, utc("2026-11-04T15:00:00Z"));
        assert_eq!(p.limit_type, "five_hour");
        // At exactly resets_at the pause is over.
        let rep = build(&st, &PriceTable::embedded(), cet("2026-11-04T15:00:00Z"));
        assert!(rep.pause.is_none());
        let rep = build(&st, &PriceTable::embedded(), cet("2026-11-04T16:00:00Z"));
        assert!(rep.pause.is_none());
    }

    #[test]
    fn pause_picks_most_recent_and_defaults_limit_type() {
        let st = state(
            vec![],
            vec![
                ev("2026-11-04T10:00:00Z", "rejected", Some("2026-11-04T17:00:00Z"), Some("seven_day")),
                ev("2026-11-04T11:00:00Z", "rejected", Some("2026-11-04T15:00:00Z"), None),
                ev("2026-11-04T11:30:00Z", "rejected", Some("2026-11-04T11:45:00Z"), Some("x")), // already over
            ],
        );
        let rep = build(&st, &PriceTable::embedded(), cet("2026-11-04T12:00:00Z"));
        let p = rep.pause.unwrap();
        assert_eq!(p.until, utc("2026-11-04T15:00:00Z"));
        assert_eq!(p.limit_type, "unbekannt");
    }

    #[test]
    fn pause_ignores_non_rejected_status() {
        let st = state(
            vec![],
            vec![
                ev("2026-11-04T11:00:00Z", "allowed", Some("2026-11-04T15:00:00Z"), Some("five_hour")),
                ev("2026-11-04T11:10:00Z", "allowed_warning", Some("2026-11-04T15:00:00Z"), None),
                ev("2026-11-04T11:20:00Z", "rejected", None, None),
            ],
        );
        let rep = build(&st, &PriceTable::embedded(), cet("2026-11-04T12:00:00Z"));
        assert!(rep.pause.is_none());
    }

    #[test]
    fn lists_sorted_by_cost_desc() {
        let mut cheap = rec("a", "2026-11-04T10:00:00Z", "claude-opus-5-5", "cheap");
        cheap.output = 1_000;
        let mut dear = rec("b", "2026-11-04T10:01:00Z", "claude-sonnet-5-5", "dear");
        dear.output = 100_000;
        let tie1 = rec("c", "2026-11-04T10:02:00Z", "zzz", "beta");
        let tie2 = rec("d", "2026-11-04T10:03:00Z", "aaa", "alpha");
        let st = state(vec![cheap, dear, tie1, tie2], vec![]);
        let rep = build(&st, &PriceTable::embedded(), cet("2026-11-04T12:00:00Z"));
        let projects: Vec<_> = rep.by_project.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(projects, ["dear", "cheap", "alpha", "beta"]);
        let models: Vec<_> = rep.by_model.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(models, ["claude-sonnet-5-5", "claude-opus-5-5", "aaa", "zzz"]);
    }

    #[test]
    fn by_project_and_model_cover_current_month_only() {
        let st = state(
            vec![
                rec("old", "2026-10-20T10:00:00Z", "old-model", "old-project"),
                rec("new", "2026-11-03T10:00:00Z", "new-model", "new-project"),
            ],
            vec![],
        );
        let rep = build(&st, &PriceTable::embedded(), cet("2026-11-04T12:00:00Z"));
        let projects: Vec<_> = rep.by_project.iter().map(|(n, _)| n.as_str()).collect();
        let models: Vec<_> = rep.by_model.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(projects, ["new-project"]);
        assert_eq!(models, ["new-model"]);
        // by_day still reaches back 31 days.
        assert_eq!(rep.by_day.len(), 2);
    }

    #[test]
    fn by_day_covers_last_31_days_newest_first() {
        let st = state(
            vec![
                rec("a", "2026-11-04T10:00:00Z", "m", "p"),
                rec("b", "2026-10-05T10:00:00Z", "m", "p"), // 30 days back: included
                rec("c", "2026-10-04T10:00:00Z", "m", "p"), // 31 days back: excluded
            ],
            vec![],
        );
        let rep = build(&st, &PriceTable::embedded(), cet("2026-11-04T12:00:00Z"));
        let days: Vec<_> = rep.by_day.iter().map(|(d, _)| d.to_string()).collect();
        assert_eq!(days, ["2026-11-04", "2026-10-05"]);
    }

    #[test]
    fn today_breakdown_counts_only_today_by_project_and_model() {
        let mut a = rec("a", "2026-11-04T10:00:00Z", "claude-opus-5-5", "alpha");
        a.output = 1_000;
        let mut b = rec("b", "2026-11-04T10:01:00Z", "claude-sonnet-5-5", "beta");
        b.output = 100_000;
        let c = rec("c", "2026-11-04T10:02:00Z", "claude-opus-5-5", "alpha");
        let earlier = rec("d", "2026-11-03T10:00:00Z", "old-model", "old-project");
        let st = state(vec![a, b, c, earlier], vec![]);
        let rep = build(&st, &PriceTable::embedded(), cet("2026-11-04T12:00:00Z"));
        let projects: Vec<_> = rep.today_by_project.iter().map(|(n, b)| (n.as_str(), b.requests)).collect();
        assert_eq!(projects, [("beta", 1), ("alpha", 2)]);
        let models: Vec<_> = rep.today_by_model.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(models, ["claude-sonnet-5-5", "claude-opus-5-5"]);
        // The month lists still include the earlier day.
        assert_eq!(rep.by_project.len(), 3);
    }
}
