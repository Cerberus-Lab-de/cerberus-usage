//! Real API cost from the Usage and Cost Analytics API (beta, needs an admin key).
//!
//! Amounts are decimal strings in cents with six decimals; they are parsed with integer math
//! into nano-USD. The key only travels in the `x-api-key` header and never appears in a URL,
//! an error text or a `Debug` output.
//!
//! Granularity: the API buckets are UTC days (`1d`) and each bucket is assigned to a day by its
//! UTC start date. "Today" is the bucket whose UTC date equals the local date, "month" the
//! buckets whose UTC date lies in the local month. The figure is therefore per UTC-day label,
//! not a local-time cut: around midnight, usage of the previous local evening can still sit in
//! the other label (off by up to the local UTC offset). The data also lags behind real usage
//! (the docs speak of up to 4 hours, in rare cases 24 hours), so this is the billed figure, not
//! a live counter.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone, Utc};
use serde::Deserialize;

use crate::secrets::{SecretStore, Slot};

const BASE_URL: &str = "https://api.anthropic.com/v1/organizations/analytics/cost";
const API_VERSION: &str = "2026-10-21";
const BETA: &str = "analytics-usage-cost-2026-08-24";
const USER_AGENT: &str = concat!("cerberus-usage/", env!("CARGO_PKG_VERSION"));

/// Rows per page (the API maximum).
const PAGE_LIMIT: u32 = 1000;
/// Safety net against a server that never stops sending `has_more`.
const MAX_PAGES: usize = 50;
/// Longest range one request may cover.
const MAX_SPAN_DAYS: i64 = 31;
/// Timeout of a single request.
const TIMEOUT: StdDuration = StdDuration::from_secs(30);
/// Upper bound for one whole `fetch_cost` call (all chunks and pages). Checked between requests.
const TOTAL_DEADLINE: StdDuration = StdDuration::from_secs(90);

/// How often the worker asks the API.
pub const DEFAULT_API_INTERVAL: StdDuration = StdDuration::from_secs(300);
/// Lower bound for the API interval, whatever the configuration says.
pub const MIN_API_INTERVAL: StdDuration = StdDuration::from_secs(60);
/// Pause after `ApiError::NotAvailable` (individual account or missing permission): retrying
/// every 5 minutes is pointless, but a later upgrade or key change should still be noticed.
pub const NOT_AVAILABLE_INTERVAL: StdDuration = StdDuration::from_secs(3600);

/// 1 cent = 10^7 nano-USD; the API amount has 6 decimals, so one unit of it is 10 nano-USD.
const NANO_PER_AMOUNT_UNIT: u64 = 10;

/// The API rejects `starting_at` before this instant.
fn earliest_start() -> DateTime<Utc> {
    NaiveDate::from_ymd_opt(2026, 1, 1)
        .map(|d| d.and_time(NaiveTime::MIN).and_utc())
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

// ---------------------------------------------------------------------------------------------
// HTTP seam
// ---------------------------------------------------------------------------------------------

/// Transport failure. The text is a fixed, user-facing category; it never echoes request data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetError(pub String);

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NetError {}

/// Minimal HTTP GET so the fetch logic is testable without a network.
/// Non-2xx statuses are returned as `Ok((status, body))`, not as errors.
pub trait Http {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<(u16, String), NetError>;

    /// GET returning raw bytes, at most `max_bytes` (an oversized body is an error). The default
    /// goes through [`Http::get`]; transports that can stream override it.
    fn get_bytes(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        max_bytes: usize,
    ) -> Result<(u16, Vec<u8>), NetError> {
        let (status, body) = self.get(url, headers)?;
        if body.len() > max_bytes {
            return Err(NetError("Antwort zu groß".to_string()));
        }
        Ok((status, body.into_bytes()))
    }
}

/// `ureq` with rustls: HTTPS only, no redirects (the key must not follow a redirect), 30 s timeout.
pub struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .https_only(true)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .timeout_global(Some(TIMEOUT))
            .build();
        Self { agent: ureq::Agent::new_with_config(config) }
    }
}

impl Default for UreqHttp {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn net_error(e: ureq::Error) -> NetError {
    NetError(
        match e {
            ureq::Error::Timeout(_) => "Zeitüberschreitung",
            ureq::Error::HostNotFound | ureq::Error::ConnectionFailed | ureq::Error::Io(_) => {
                "Server nicht erreichbar"
            }
            _ => "Netzwerkfehler",
        }
        .to_string(),
    )
}

impl Http for UreqHttp {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<(u16, String), NetError> {
        let mut request = self.agent.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let mut response = request.call().map_err(net_error)?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().map_err(net_error)?;
        Ok((status, body))
    }
}

// ---------------------------------------------------------------------------------------------
// Errors and rows
// ---------------------------------------------------------------------------------------------

/// Why the API call failed. The texts are shown in the UI and never contain the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// HTTP 401: key unknown or invalid.
    Unauthorized,
    /// HTTP 403 or 404: the endpoint is not offered to this account (individual account, no admin
    /// key, or missing permission). A state, not a failure worth retrying every few minutes.
    NotAvailable,
    /// HTTP 429.
    RateLimited,
    /// Transport failure (fixed category text).
    Network(String),
    /// Any other non-2xx status.
    Server(u16),
    /// The answer could not be understood.
    Parse(String),
    /// The balance baseline is older than the API allows (365 days): no request is made.
    BaselineTooOld,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Unauthorized => {
                f.write_str("Admin-Key wurde abgelehnt. Bitte den Key in den Einstellungen prüfen.")
            }
            ApiError::NotAvailable => f.write_str(
                "Kosten-API nicht verfügbar: nur für Organisations-Konten mit Admin-Key (Einzelkonto oder fehlende Berechtigung)",
            ),
            ApiError::RateLimited => {
                f.write_str("Anfragelimit der API erreicht. Der nächste Versuch folgt später.")
            }
            ApiError::Network(m) => write!(f, "Netzwerkfehler: {m}"),
            ApiError::Server(code) => write!(f, "Serverfehler der API (HTTP {code})"),
            ApiError::Parse(m) => write!(f, "Antwort der API nicht lesbar: {m}"),
            ApiError::BaselineTooOld => f.write_str("Startwert zu alt, bitte neu eintragen"),
        }
    }
}

impl std::error::Error for ApiError {}

/// One row of the cost report: one UTC bucket, one model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostRow {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub model: Option<String>,
    /// Cost after discounts, in nano-USD.
    pub amount_nano: u64,
}

#[derive(Deserialize)]
struct PageDto {
    data: Vec<RowDto>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    next_page: Option<String>,
}

#[derive(Deserialize)]
struct RowDto {
    starting_at: DateTime<Utc>,
    ending_at: DateTime<Utc>,
    #[serde(default)]
    group: GroupDto,
    cost: CostDto,
}

#[derive(Deserialize, Default)]
struct GroupDto {
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct CostDto {
    amount: String,
}

/// Parses an API amount (cents, up to 6 decimals, e.g. `"41280.000000"`) into nano-USD.
/// Digits only: no sign, exponent or whitespace. Overflow is a parse error.
pub fn parse_amount_nano(s: &str) -> Result<u64, ApiError> {
    let bad = || ApiError::Parse("ungültiger Betrag".to_string());
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    let digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if int_part.is_empty() || !digits(int_part) || !digits(frac_part) || frac_part.len() > 6 {
        return Err(bad());
    }
    if s.contains('.') && frac_part.is_empty() {
        return Err(bad());
    }
    let int: u64 = int_part.parse().map_err(|_| bad())?;
    let frac: u64 = if frac_part.is_empty() {
        0
    } else {
        // Right-pad to 6 digits: "5" -> 500000.
        let padded = format!("{frac_part:0<6}");
        padded.parse().map_err(|_| bad())?
    };
    int.checked_mul(1_000_000)
        .and_then(|v| v.checked_add(frac))
        .and_then(|v| v.checked_mul(NANO_PER_AMOUNT_UNIT))
        .ok_or_else(bad)
}

// ---------------------------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------------------------

/// Percent-encodes everything except RFC 3986 unreserved characters.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn request_url(start: DateTime<Utc>, end: DateTime<Utc>, width: &str, page: Option<&str>) -> String {
    let mut url = format!(
        "{BASE_URL}?starting_at={}&ending_at={}&time_bucket_width={}&group_by%5B%5D=model&limit={PAGE_LIMIT}",
        encode(&rfc3339(start)),
        encode(&rfc3339(end)),
        encode(width),
    );
    if let Some(token) = page {
        url.push_str("&page=");
        url.push_str(&encode(token));
    }
    url
}

/// Fetches all rows of `[start, end)`. `start` is clamped to 2026-01-01, ranges over 31 days
/// are split into several requests, pages are followed via `next_page`.
/// The whole call is bounded by 90 s; the deadline is checked between requests.
pub fn fetch_cost(
    http: &dyn Http,
    key: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    width: &str,
) -> Result<Vec<CostRow>, ApiError> {
    fetch_cost_until(http, key, start, end, width, Instant::now() + TOTAL_DEADLINE)
}

fn fetch_cost_until(
    http: &dyn Http,
    key: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    width: &str,
    deadline: Instant,
) -> Result<Vec<CostRow>, ApiError> {
    let start = start.max(earliest_start());
    let mut rows = Vec::new();
    let mut from = start;
    while from < end {
        let to = end.min(from + Duration::days(MAX_SPAN_DAYS));
        fetch_chunk(http, key, from, to, width, deadline, &mut rows)?;
        from = to;
    }
    Ok(rows)
}

fn fetch_chunk(
    http: &dyn Http,
    key: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    width: &str,
    deadline: Instant,
    rows: &mut Vec<CostRow>,
) -> Result<(), ApiError> {
    let headers = [
        ("x-api-key", key),
        ("anthropic-version", API_VERSION),
        ("anthropic-beta", BETA),
        ("user-agent", USER_AGENT),
    ];
    let mut page: Option<String> = None;
    for _ in 0..MAX_PAGES {
        if Instant::now() >= deadline {
            return Err(ApiError::Network("Zeitüberschreitung".to_string()));
        }
        let url = request_url(start, end, width, page.as_deref());
        let (status, body) = http
            .get(&url, &headers)
            .map_err(|e| ApiError::Network(e.0))?;
        match status {
            200..=299 => {}
            401 => return Err(ApiError::Unauthorized),
            403 | 404 => return Err(ApiError::NotAvailable),
            429 => return Err(ApiError::RateLimited),
            other => return Err(ApiError::Server(other)),
        }
        // Fixed text with position only: serde's own message can echo parts of the body.
        let parsed: PageDto = serde_json::from_str(&body).map_err(|e| {
            ApiError::Parse(format!("unerwartetes Format (Zeile {}, Spalte {})", e.line(), e.column()))
        })?;
        for row in parsed.data {
            rows.push(CostRow {
                start: row.starting_at,
                end: row.ending_at,
                model: row.group.model,
                amount_nano: parse_amount_nano(&row.cost.amount)?,
            });
        }
        if !parsed.has_more {
            return Ok(());
        }
        match parsed.next_page {
            Some(token) if !token.is_empty() => page = Some(token),
            _ => return Err(ApiError::Parse("Folgeseite fehlt".to_string())),
        }
    }
    Err(ApiError::Parse("zu viele Seiten".to_string()))
}

// ---------------------------------------------------------------------------------------------
// Today / month totals and the polling state
// ---------------------------------------------------------------------------------------------

/// Real API cost as the UI shows it. After a failed poll the previous figures stay and
/// `error` is set. `updated` is the time of the last successful poll (or of the first
/// attempt when none has succeeded yet, in which case the figures are 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiStatus {
    pub today_nano: u64,
    pub month_nano: u64,
    /// Cost since the balance baseline (hourly buckets, see `ApiPoller::set_since`). `None`
    /// when no baseline is set or no poll including it has succeeded yet.
    pub since_nano: Option<u64>,
    pub updated: DateTime<Local>,
    /// Error of the today/month request. The figures above stay as they were.
    pub error: Option<ApiError>,
    /// Error of the "since baseline" request only; it never freezes today and month.
    pub since_error: Option<ApiError>,
    /// True once a poll has succeeded. False means the figures are placeholders (0) after a first
    /// failed poll and must not be shown as costs.
    pub has_data: bool,
}

/// Which UTC-day buckets count as "today" and "month", and which UTC range to request.
struct Windows {
    /// Local date of `now`; the bucket with this UTC date is "today".
    today: NaiveDate,
    /// Buckets whose UTC date lies in this local year and month are "month".
    month: (i32, u32),
    /// UTC-day aligned request range `[start, end)`.
    fetch: (DateTime<Utc>, DateTime<Utc>),
}

fn windows<Tz: TimeZone>(now: &DateTime<Tz>) -> Windows {
    let today = now.date_naive();
    let utc_today = now.with_timezone(&Utc).date_naive();
    let month_first = today.with_day(1).unwrap_or(today);
    let next_month_first = if today.month() == 12 {
        NaiveDate::from_ymd_opt(today.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(today.year(), today.month() + 1, 1)
    }
    .unwrap_or(today);
    // Last bucket that can matter: the later of the local and the UTC date, but nothing past the
    // local month (those buckets count for neither figure). This also keeps the span <= 31 days.
    let last_day = today.max(utc_today);
    let end_day = last_day.succ_opt().unwrap_or(last_day).min(next_month_first);
    Windows {
        today,
        month: (today.year(), today.month()),
        fetch: (
            month_first.and_time(NaiveTime::MIN).and_utc(),
            end_day.and_time(NaiveTime::MIN).and_utc(),
        ),
    }
}

/// Sum of the rows for which `keep(UTC start date)` holds.
fn sum_where(rows: &[CostRow], keep: impl Fn(NaiveDate) -> bool) -> u64 {
    rows.iter()
        .filter(|r| keep(r.start.date_naive()))
        .fold(0u64, |acc, r| acc.saturating_add(r.amount_nano))
}

fn fetch_totals<Tz: TimeZone>(
    http: &dyn Http,
    key: &str,
    now: &DateTime<Tz>,
) -> Result<(u64, u64), ApiError> {
    let w = windows(now);
    let rows = fetch_cost(http, key, w.fetch.0, w.fetch.1, "1d")?;
    let today = sum_where(&rows, |d| d == w.today);
    let month = sum_where(&rows, |d| (d.year(), d.month()) == w.month);
    Ok((today, month))
}

/// The API only serves `starting_at` within this many days.
const MAX_LOOKBACK_DAYS: i64 = 365;

/// Rounds down to the full hour, like the API does with both ends of a range.
fn floor_hour(t: DateTime<Utc>) -> DateTime<Utc> {
    let ts = t.timestamp();
    DateTime::from_timestamp(ts - ts.rem_euclid(3600), 0).unwrap_or(t)
}

/// Total cost in `[from, to)` with hourly buckets. Both ends are rounded down to the full hour
/// (as the API does), so the sum may include up to one hour of cost before `from`. When both
/// ends fall into the same hour there is nothing to ask: no request, cost 0. A start older than
/// 365 days is refused without a request.
fn fetch_since(http: &dyn Http, key: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<u64, ApiError> {
    if to.signed_duration_since(from) > Duration::days(MAX_LOOKBACK_DAYS) {
        return Err(ApiError::BaselineTooOld);
    }
    let (from, to) = (floor_hour(from), floor_hour(to));
    if from >= to {
        return Ok(0);
    }
    let rows = fetch_cost(http, key, from, to, "1h")?;
    Ok(sum_where(&rows, |_| true))
}

/// Decides when to ask the API and keeps the last known figures.
///
/// Time is passed in, so tests need no sleeping. The admin key is read fresh from the store
/// on every poll (a key saved later is picked up) and dropped right after the call. Without
/// a key (or when the store is unreachable) there is no status and no HTTP call.
pub struct ApiPoller {
    store: Arc<dyn SecretStore + Send + Sync>,
    http: Arc<dyn Http + Send + Sync>,
    interval: StdDuration,
    last_attempt: Option<Instant>,
    status: Option<ApiStatus>,
    /// Start of the extra "cost since" request (the balance baseline), if any.
    since: Option<DateTime<Utc>>,
}

impl ApiPoller {
    /// `interval` is raised to [`MIN_API_INTERVAL`] if smaller.
    pub fn new(
        store: Arc<dyn SecretStore + Send + Sync>,
        http: Arc<dyn Http + Send + Sync>,
        interval: StdDuration,
    ) -> Self {
        Self {
            store,
            http,
            interval: interval.max(MIN_API_INTERVAL),
            last_attempt: None,
            status: None,
            since: None,
        }
    }

    /// Sets the start of the "cost since" figure (`ApiStatus::since_nano`), requested with hourly
    /// buckets in the same poll. A changed start drops the old figure and makes the poller due
    /// right away; an unchanged one does nothing.
    pub fn set_since(&mut self, since: Option<DateTime<Utc>>) {
        if since == self.since {
            return;
        }
        self.since = since;
        self.last_attempt = None;
        if let Some(status) = self.status.as_mut() {
            status.since_nano = None;
        }
    }

    /// Last known figures (`None` until the first successful or failed poll with a key).
    pub fn current(&self) -> Option<&ApiStatus> {
        self.status.as_ref()
    }

    /// True when no HTTP call was made yet or the interval has passed since the last one.
    /// After `ApiError::NotAvailable` the wait is [`NOT_AVAILABLE_INTERVAL`] instead.
    pub fn due(&self, now: Instant) -> bool {
        let wait = match self.status.as_ref().map(|s| &s.error) {
            Some(Some(ApiError::NotAvailable)) => self.interval.max(NOT_AVAILABLE_INTERVAL),
            _ => self.interval,
        };
        self.last_attempt
            .is_none_or(|t| now.saturating_duration_since(t) >= wait)
    }

    /// Polls the API if a key exists. Returns true when `current()` changed.
    pub fn refresh<Tz: TimeZone>(&mut self, now: Instant, wall: DateTime<Tz>) -> bool {
        let before = self.status.clone();
        let key = match self.store.get(Slot::AdminKey) {
            Ok(Some(k)) if !k.trim().is_empty() => k,
            _ => {
                self.status = None;
                self.last_attempt = None;
                return before.is_some();
            }
        };
        self.last_attempt = Some(now);
        let totals = fetch_totals(self.http.as_ref(), &key, &wall);
        // Today and month are settled first. The baseline request comes after and can only set its own
        // error; it never freezes the totals. Without fresh totals there is no point in asking.
        let since_result = match (&totals, self.since) {
            (Ok(_), Some(from)) => Some(fetch_since(self.http.as_ref(), &key, from, wall.with_timezone(&Utc))),
            _ => None,
        };
        drop(key);

        let updated = wall.with_timezone(&Local);
        self.status = Some(match (totals, before.clone()) {
            (Ok((today_nano, month_nano)), previous) => {
                let (since_nano, since_error) = match since_result {
                    None => (None, None),
                    Some(Ok(v)) => (Some(v), None),
                    Some(Err(ApiError::BaselineTooOld)) => (None, Some(ApiError::BaselineTooOld)),
                    // Any other failure keeps the last known figure (if any) and reports itself.
                    Some(Err(e)) => (previous.and_then(|p| p.since_nano), Some(e)),
                };
                ApiStatus {
                    today_nano,
                    month_nano,
                    since_nano,
                    updated,
                    error: None,
                    since_error,
                    has_data: true,
                }
            }
            (Err(e), Some(previous)) => ApiStatus { error: Some(e), ..previous },
            (Err(e), None) => ApiStatus {
                today_nano: 0,
                month_nano: 0,
                since_nano: None,
                updated,
                error: Some(e),
                since_error: None,
                has_data: false,
            },
        });
        self.status != before
    }
}

/// Test doubles shared with the worker tests.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::secrets::SecretError;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    pub struct FakeStore {
        pub admin: Mutex<Option<String>>,
    }

    impl FakeStore {
        pub fn new(admin: Option<&str>) -> Self {
            Self { admin: Mutex::new(admin.map(str::to_string)) }
        }
    }

    impl SecretStore for FakeStore {
        fn get(&self, s: Slot) -> Result<Option<String>, SecretError> {
            Ok(match s {
                Slot::AdminKey => self.admin.lock().unwrap().clone(),
                Slot::ApiKey => None,
            })
        }
        fn set(&self, s: Slot, v: &str) -> Result<(), SecretError> {
            if s == Slot::AdminKey {
                *self.admin.lock().unwrap() = Some(v.to_string());
            }
            Ok(())
        }
        fn delete(&self, s: Slot) -> Result<(), SecretError> {
            if s == Slot::AdminKey {
                *self.admin.lock().unwrap() = None;
            }
            Ok(())
        }
    }

    pub type Recorded = (String, Vec<(String, String)>);

    /// Replays scripted responses and records every request.
    pub struct FakeHttp {
        responses: Mutex<VecDeque<Result<(u16, String), NetError>>>,
        pub requests: Mutex<Vec<Recorded>>,
    }

    impl FakeHttp {
        pub fn new(responses: Vec<Result<(u16, String), NetError>>) -> Self {
            Self { responses: Mutex::new(responses.into()), requests: Mutex::new(Vec::new()) }
        }
        pub fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    impl Http for FakeHttp {
        fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<(u16, String), NetError> {
            self.requests.lock().unwrap().push((
                url.to_string(),
                headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(NetError("keine Antwort vorbereitet".to_string())))
        }
    }

    pub fn ok(body: &str) -> Result<(u16, String), NetError> {
        Ok((200, body.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use chrono::FixedOffset;

    const KEY: &str = "sk-ant-admin01-NICHTECHT-1234";
    const EMPTY: &str = r#"{"data":[],"has_more":false,"next_page":null}"#;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn row_json(start: &str, end: &str, model: &str, amount: &str) -> String {
        format!(
            r#"{{"starting_at":"{start}","ending_at":"{end}","group":{{"model":"{model}","workspace_id":null}},"cost":{{"currency":"USD","amount":"{amount}","list_amount":"1.000000","requests":null}}}}"#
        )
    }

    fn page(rows: &[String], next: Option<&str>) -> String {
        match next {
            Some(t) => format!(r#"{{"data":[{}],"has_more":true,"next_page":"{t}"}}"#, rows.join(",")),
            None => format!(r#"{{"data":[{}],"has_more":false,"next_page":null}}"#, rows.join(",")),
        }
    }

    #[test]
    fn parses_amount_in_cents() {
        // 41280.000000 cents = 412.80 USD = 412_800_000_000 nano-USD.
        assert_eq!(parse_amount_nano("41280.000000").unwrap(), 412_800_000_000);
        assert_eq!(parse_amount_nano("0.000001").unwrap(), 10);
        assert_eq!(parse_amount_nano("123.45").unwrap(), 1_234_500_000);
        assert_eq!(parse_amount_nano("7").unwrap(), 70_000_000);
        for bad in ["", "-1.000000", "1e3", "abc", "1.0000001", "1.", ".5", " 1", "1 ", "+1", "1,5"] {
            assert!(matches!(parse_amount_nano(bad), Err(ApiError::Parse(_))), "accepted {bad:?}");
        }
        // Overflow of u64 nano-USD is an error, not a wrap.
        assert!(matches!(parse_amount_nano("18446744073709551615"), Err(ApiError::Parse(_))));
        assert!(matches!(parse_amount_nano("99999999999999999999999"), Err(ApiError::Parse(_))));
    }

    #[test]
    fn fetch_returns_rows_with_model_and_amount() {
        let body = page(
            &[
                row_json("2026-10-03T00:00:00Z", "2026-10-04T00:00:00Z", "claude-sonnet-5", "41280.000000"),
                r#"{"starting_at":"2026-10-03T00:00:00Z","ending_at":"2026-10-04T00:00:00Z","group":{"model":null},"cost":{"amount":"5.500000"}}"#.to_string(),
            ],
            None,
        );
        let http = FakeHttp::new(vec![ok(&body)]);
        let rows =
            fetch_cost(&http, KEY, utc("2026-10-03T00:00:00Z"), utc("2026-10-04T00:00:00Z"), "1d").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(rows[0].amount_nano, 412_800_000_000);
        assert_eq!(rows[0].start, utc("2026-10-03T00:00:00Z"));
        assert_eq!(rows[0].end, utc("2026-10-04T00:00:00Z"));
        assert_eq!(rows[1].model, None);
        assert_eq!(rows[1].amount_nano, 55_000_000);
    }

    #[test]
    fn follows_pagination() {
        let first = page(
            &[row_json("2026-10-03T00:00:00Z", "2026-10-04T00:00:00Z", "m1", "100.000000")],
            Some("page_V2mQ4dkt9HcbX3pLr=="),
        );
        let second = page(&[row_json("2026-10-03T00:00:00Z", "2026-10-04T00:00:00Z", "m2", "200.000000")], None);
        let http = FakeHttp::new(vec![ok(&first), ok(&second)]);
        let rows =
            fetch_cost(&http, KEY, utc("2026-10-03T00:00:00Z"), utc("2026-10-04T00:00:00Z"), "1d").unwrap();
        assert_eq!(rows.iter().map(|r| r.amount_nano).sum::<u64>(), 3_000_000_000);

        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(!requests[0].0.contains("page="));
        assert!(requests[1].0.ends_with("&page=page_V2mQ4dkt9HcbX3pLr%3D%3D"), "{}", requests[1].0);
    }

    #[test]
    fn has_more_without_next_page_is_an_error_not_a_loop() {
        let body = r#"{"data":[],"has_more":true,"next_page":null}"#;
        let http = FakeHttp::new(vec![ok(body)]);
        let err = fetch_cost(&http, KEY, utc("2026-10-03T00:00:00Z"), utc("2026-10-04T00:00:00Z"), "1d")
            .unwrap_err();
        assert!(matches!(err, ApiError::Parse(_)));
        assert_eq!(http.request_count(), 1);
    }

    #[test]
    fn sends_required_headers() {
        let http = FakeHttp::new(vec![ok(EMPTY)]);
        fetch_cost(&http, KEY, utc("2026-10-03T00:00:00Z"), utc("2026-10-05T00:00:00Z"), "1d").unwrap();

        let requests = http.requests.lock().unwrap();
        let (url, headers) = &requests[0];
        let header = |name: &str| {
            headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
        };
        assert_eq!(header("x-api-key"), Some(KEY));
        assert_eq!(header("anthropic-version"), Some("2026-10-21"));
        assert_eq!(header("anthropic-beta"), Some("analytics-usage-cost-2026-08-24"));
        assert_eq!(
            header("user-agent"),
            Some(concat!("cerberus-usage/", env!("CARGO_PKG_VERSION")))
        );
        assert!(url.starts_with("https://api.anthropic.com/v1/organizations/analytics/cost?"));
        assert!(url.contains("starting_at=2026-10-03T00%3A00%3A00Z"), "{url}");
        assert!(url.contains("ending_at=2026-10-05T00%3A00%3A00Z"), "{url}");
        assert!(url.contains("time_bucket_width=1d"));
        assert!(url.contains("group_by%5B%5D=model"));
        assert!(!url.contains("NICHTECHT"), "key leaked into URL");
        assert!(!url.contains('+'));
    }

    #[test]
    fn splits_range_over_31_days() {
        // 40 days -> 31 + 9.
        let http = FakeHttp::new(vec![ok(EMPTY), ok(EMPTY)]);
        fetch_cost(&http, KEY, utc("2026-10-01T00:00:00Z"), utc("2026-11-10T00:00:00Z"), "1d").unwrap();

        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].0.contains("starting_at=2026-10-01T00%3A00%3A00Z"));
        assert!(requests[0].0.contains("ending_at=2026-11-01T00%3A00%3A00Z"));
        assert!(requests[1].0.contains("starting_at=2026-11-01T00%3A00%3A00Z"));
        assert!(requests[1].0.contains("ending_at=2026-11-10T00%3A00%3A00Z"));
    }

    #[test]
    fn exactly_31_days_is_one_request() {
        let http = FakeHttp::new(vec![ok(EMPTY)]);
        fetch_cost(&http, KEY, utc("2026-10-01T00:00:00Z"), utc("2026-11-01T00:00:00Z"), "1d").unwrap();
        assert_eq!(http.request_count(), 1);
    }

    #[test]
    fn clamps_start_to_2026_01_01() {
        let http = FakeHttp::new(vec![ok(EMPTY)]);
        fetch_cost(&http, KEY, utc("2025-12-01T00:00:00Z"), utc("2026-01-03T00:00:00Z"), "1d").unwrap();
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].0.contains("starting_at=2026-01-01T00%3A00%3A00Z"), "{}", requests[0].0);
    }

    #[test]
    fn empty_range_makes_no_request() {
        let http = FakeHttp::new(vec![]);
        let t = utc("2026-10-03T00:00:00Z");
        assert!(fetch_cost(&http, KEY, t, t, "1d").unwrap().is_empty());
        assert_eq!(http.request_count(), 0);
    }

    fn fetch_status(status: u16) -> Result<Vec<CostRow>, ApiError> {
        let http = FakeHttp::new(vec![Ok((status, "{}".to_string()))]);
        fetch_cost(&http, KEY, utc("2026-10-03T00:00:00Z"), utc("2026-10-04T00:00:00Z"), "1d")
    }

    #[test]
    fn maps_401_to_unauthorized() {
        assert_eq!(fetch_status(401).unwrap_err(), ApiError::Unauthorized);
    }

    #[test]
    fn maps_403_and_404_to_not_available() {
        for status in [403, 404] {
            let err = fetch_status(status).unwrap_err();
            assert_eq!(err, ApiError::NotAvailable);
            let text = err.to_string();
            assert_eq!(
                text,
                "Kosten-API nicht verfügbar: nur für Organisations-Konten mit Admin-Key (Einzelkonto oder fehlende Berechtigung)"
            );
            assert!(!text.contains("Serverfehler") && !text.contains("HTTP 404"));
        }
    }

    #[test]
    fn maps_429_to_rate_limited() {
        assert_eq!(fetch_status(429).unwrap_err(), ApiError::RateLimited);
    }

    #[test]
    fn maps_other_statuses_to_server_error() {
        assert_eq!(fetch_status(500).unwrap_err(), ApiError::Server(500));
        assert_eq!(fetch_status(302).unwrap_err(), ApiError::Server(302));
    }

    #[test]
    fn maps_transport_failure_to_network_error() {
        let http = FakeHttp::new(vec![Err(NetError("Zeitüberschreitung".to_string()))]);
        let err = fetch_cost(&http, KEY, utc("2026-10-03T00:00:00Z"), utc("2026-10-04T00:00:00Z"), "1d")
            .unwrap_err();
        assert_eq!(err, ApiError::Network("Zeitüberschreitung".to_string()));
    }

    #[test]
    fn errors_never_contain_key() {
        // A server that echoes the key back in a malformed body must not leak it via Parse.
        let echo = format!(r#"{{"data":"{KEY}"}}"#);
        let echo_row = format!(
            r#"{{"data":[{{"starting_at":"{KEY}","ending_at":"x","cost":{{"amount":"1"}}}}]}}"#
        );
        let responses: Vec<Result<(u16, String), NetError>> = vec![
            Ok((401, "{}".into())),
            Ok((403, "{}".into())),
            Ok((404, "{}".into())),
            Ok((429, "{}".into())),
            Ok((500, "{}".into())),
            Ok((200, "not json".into())),
            Ok((200, echo)),
            Ok((200, echo_row)),
            Ok((200, r#"{"data":[{"starting_at":"2026-10-03T00:00:00Z","ending_at":"2026-10-04T00:00:00Z","cost":{"amount":"-1"}}]}"#.into())),
            Err(NetError("Server nicht erreichbar".into())),
        ];
        let count = responses.len();
        let http = FakeHttp::new(responses);
        for _ in 0..count {
            let err = fetch_cost(&http, KEY, utc("2026-10-03T00:00:00Z"), utc("2026-10-04T00:00:00Z"), "1d")
                .unwrap_err();
            let shown = format!("{err} | {err:?}");
            assert!(!shown.contains("NICHTECHT") && !shown.contains("1234"), "leaked: {shown}");
        }
        for (url, _) in http.requests.lock().unwrap().iter() {
            assert!(!url.contains("NICHTECHT"));
        }
    }

    #[test]
    fn status_debug_never_contains_key_after_success_and_errors() {
        let echo = format!(r#"{{"data":"{KEY}"}}"#);
        let (mut p, _http) = poller(
            Some(KEY),
            vec![
                ok(&totals_body()),
                Ok((401, "{}".into())),
                Ok((404, "{}".into())),
                Ok((200, echo)),
                Err(NetError("Server nicht erreichbar".into())),
            ],
        );
        let t0 = Instant::now();
        for i in 0..5u32 {
            p.refresh(t0 + DEFAULT_API_INTERVAL * i, cet("2026-10-15T12:00:00Z"));
            let status = p.current().expect("status after a poll with key");
            let shown = format!("{status:?} | {p_error}", p_error = status.error.as_ref().map(|e| e.to_string()).unwrap_or_default());
            assert!(!shown.contains("NICHTECHT") && !shown.contains("1234"), "poll {i} leaked: {shown}");
        }
    }

    // -- totals and polling ------------------------------------------------------------------

    fn cet(s: &str) -> DateTime<FixedOffset> {
        utc(s).with_timezone(&FixedOffset::east_opt(3600).unwrap())
    }

    /// Fixture for 2026-10-15 12:00Z in UTC+1 (local date Oct 15). Buckets count by UTC start date.
    fn totals_body() -> String {
        page(
            &[
                // Today: UTC date == local date.
                row_json("2026-10-15T00:00:00Z", "2026-10-16T00:00:00Z", "m", "100.000000"),
                // Overlaps the local day by one hour, but its UTC date is the 14th: month only.
                row_json("2026-10-14T00:00:00Z", "2026-10-15T00:00:00Z", "m", "50.000000"),
                row_json("2026-10-10T00:00:00Z", "2026-10-11T00:00:00Z", "m", "200.000000"),
                // Overlaps the local month by one hour, but its UTC date is in September: not counted.
                row_json("2026-09-30T00:00:00Z", "2026-10-01T00:00:00Z", "m", "77.000000"),
                row_json("2026-09-29T00:00:00Z", "2026-09-30T00:00:00Z", "m", "999.000000"),
            ],
            None,
        )
    }

    fn poller(admin: Option<&str>, responses: Vec<Result<(u16, String), NetError>>) -> (ApiPoller, Arc<FakeHttp>) {
        let http = Arc::new(FakeHttp::new(responses));
        let shared: Arc<dyn Http + Send + Sync> = http.clone();
        let store: Arc<dyn SecretStore + Send + Sync> = Arc::new(FakeStore::new(admin));
        (ApiPoller::new(store, shared, DEFAULT_API_INTERVAL), http)
    }

    #[test]
    fn totals_assign_each_utc_day_bucket_by_its_date() {
        let (mut p, http) = poller(Some(KEY), vec![ok(&totals_body())]);
        assert!(p.refresh(Instant::now(), cet("2026-10-15T12:00:00Z")));

        let s = p.current().unwrap();
        assert_eq!(s.today_nano, 1_000_000_000); // one UTC day = $1.00, not $1.50
        assert_eq!(s.month_nano, 3_500_000_000); // 100 + 50 + 200 cents
        assert_eq!(s.error, None);

        // One request covers the local month so far, aligned to UTC days.
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].0.contains("starting_at=2026-10-01T00%3A00%3A00Z"), "{}", requests[0].0);
        assert!(requests[0].0.contains("ending_at=2026-10-16T00%3A00%3A00Z"), "{}", requests[0].0);
    }

    #[test]
    fn worker_keeps_last_values_on_error() {
        let (mut p, _http) = poller(
            Some(KEY),
            vec![ok(&totals_body()), Ok((500, "{}".into())), Ok((401, "{}".into()))],
        );
        let t0 = Instant::now();
        let first_wall = cet("2026-10-15T12:00:00Z");
        p.refresh(t0, first_wall);
        let good = p.current().unwrap().clone();

        let changed = p.refresh(t0 + DEFAULT_API_INTERVAL, cet("2026-10-15T12:05:00Z"));
        assert!(changed);
        let after_error = p.current().unwrap();
        assert_eq!(after_error.today_nano, good.today_nano);
        assert_eq!(after_error.month_nano, good.month_nano);
        assert_eq!(after_error.updated, good.updated, "updated must stay at the last success");
        assert_eq!(after_error.error, Some(ApiError::Server(500)));

        // A different error replaces the old one, still with the old figures.
        p.refresh(t0 + DEFAULT_API_INTERVAL * 2, cet("2026-10-15T12:10:00Z"));
        let again = p.current().unwrap();
        assert_eq!(again.error, Some(ApiError::Unauthorized));
        assert_eq!((again.today_nano, again.month_nano), (good.today_nano, good.month_nano));
    }

    #[test]
    fn first_poll_failing_yields_zero_figures_with_error() {
        let (mut p, _http) = poller(Some(KEY), vec![Ok((429, "{}".into()))]);
        p.refresh(Instant::now(), cet("2026-10-15T12:00:00Z"));
        let s = p.current().unwrap();
        assert_eq!((s.today_nano, s.month_nano), (0, 0));
        assert_eq!(s.error, Some(ApiError::RateLimited));
    }

    #[test]
    fn worker_skips_api_without_admin_key() {
        let (mut p, http) = poller(None, vec![ok(&totals_body())]);
        assert!(p.due(Instant::now()));
        assert!(!p.refresh(Instant::now(), cet("2026-10-15T12:00:00Z")));
        assert!(p.current().is_none());
        assert_eq!(http.request_count(), 0, "no key means no HTTP call");
        // Still due: a key saved later is picked up on the next tick.
        assert!(p.due(Instant::now()));
    }

    #[test]
    fn deleting_the_key_clears_the_status() {
        let http = Arc::new(FakeHttp::new(vec![ok(&totals_body())]));
        let shared: Arc<dyn Http + Send + Sync> = http.clone();
        let store = Arc::new(FakeStore::new(Some(KEY)));
        let dyn_store: Arc<dyn SecretStore + Send + Sync> = store.clone();
        let mut p = ApiPoller::new(dyn_store, shared, DEFAULT_API_INTERVAL);
        p.refresh(Instant::now(), cet("2026-10-15T12:00:00Z"));
        assert!(p.current().is_some());

        store.delete(Slot::AdminKey).unwrap();
        assert!(p.refresh(Instant::now(), cet("2026-10-15T12:01:00Z")));
        assert!(p.current().is_none());
    }

    #[test]
    fn polls_every_5_minutes_and_never_more_than_once_a_minute() {
        let (mut p, _http) = poller(Some(KEY), vec![ok(EMPTY)]);
        let t0 = Instant::now();
        assert!(p.due(t0));
        p.refresh(t0, cet("2026-10-15T12:00:00Z"));
        assert!(!p.due(t0 + StdDuration::from_secs(60)));
        assert!(!p.due(t0 + StdDuration::from_secs(299)));
        assert!(p.due(t0 + StdDuration::from_secs(300)));

        // A configured interval below the floor is raised to one minute.
        let store: Arc<dyn SecretStore + Send + Sync> = Arc::new(FakeStore::new(Some(KEY)));
        let http: Arc<dyn Http + Send + Sync> = Arc::new(FakeHttp::new(vec![ok(EMPTY)]));
        let mut fast = ApiPoller::new(store, http, StdDuration::from_millis(1));
        fast.refresh(t0, cet("2026-10-15T12:00:00Z"));
        assert!(!fast.due(t0 + StdDuration::from_secs(59)));
        assert!(fast.due(t0 + StdDuration::from_secs(60)));
    }

    fn since_body() -> String {
        page(
            &[
                row_json("2026-10-12T08:00:00Z", "2026-10-12T09:00:00Z", "m", "30.000000"),
                row_json("2026-10-12T09:00:00Z", "2026-10-12T10:00:00Z", "m", "12.500000"),
            ],
            None,
        )
    }

    #[test]
    fn since_figure_uses_hourly_buckets_from_the_given_start() {
        let (mut p, http) = poller(Some(KEY), vec![ok(&totals_body()), ok(&since_body())]);
        p.set_since(Some(utc("2026-10-12T08:30:00Z")));
        assert!(p.refresh(Instant::now(), cet("2026-10-15T12:00:00Z")));

        // 42.5 cents = 425_000_000 nano-USD.
        assert_eq!(p.current().unwrap().since_nano, Some(425_000_000));
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].0.contains("time_bucket_width=1h"), "{}", requests[1].0);
        assert!(requests[1].0.contains("starting_at=2026-10-12T08%3A00%3A00Z"), "{}", requests[1].0);
        assert!(requests[1].0.contains("ending_at=2026-10-15T12%3A00%3A00Z"), "{}", requests[1].0);
    }

    #[test]
    fn without_since_there_is_one_request_and_no_figure() {
        let (mut p, http) = poller(Some(KEY), vec![ok(&totals_body())]);
        p.refresh(Instant::now(), cet("2026-10-15T12:00:00Z"));
        assert_eq!(p.current().unwrap().since_nano, None);
        assert_eq!(http.request_count(), 1);
    }

    #[test]
    fn since_figure_survives_a_later_error_and_is_dropped_when_the_start_changes() {
        let (mut p, _http) = poller(
            Some(KEY),
            vec![ok(&totals_body()), ok(&since_body()), Ok((500, "{}".into()))],
        );
        let start = utc("2026-10-12T08:30:00Z");
        p.set_since(Some(start));
        let t0 = Instant::now();
        p.refresh(t0, cet("2026-10-15T12:00:00Z"));
        assert!(!p.due(t0));

        p.refresh(t0 + DEFAULT_API_INTERVAL, cet("2026-10-15T12:05:00Z"));
        let s = p.current().unwrap();
        assert_eq!((s.since_nano, s.error.clone()), (Some(425_000_000), Some(ApiError::Server(500))));

        // Same start: nothing happens. New start: figure dropped, poll due immediately.
        p.set_since(Some(start));
        assert_eq!(p.current().unwrap().since_nano, Some(425_000_000));
        p.set_since(Some(utc("2026-10-13T00:00:00Z")));
        assert_eq!(p.current().unwrap().since_nano, None);
        assert!(p.due(t0 + DEFAULT_API_INTERVAL));
    }

    #[test]
    fn since_request_failing_keeps_fresh_totals_and_sets_only_its_own_error() {
        let (mut p, _http) = poller(Some(KEY), vec![ok(&totals_body()), Ok((429, "{}".into()))]);
        p.set_since(Some(utc("2026-10-12T08:30:00Z")));
        assert!(p.refresh(Instant::now(), cet("2026-10-15T12:00:00Z")));
        let s = p.current().unwrap();
        // Totals are fresh and error-free; only the balance side carries the failure.
        assert_eq!((s.today_nano, s.month_nano), (1_000_000_000, 3_500_000_000));
        assert_eq!((s.since_nano, s.error.clone(), s.since_error.clone()), (None, None, Some(ApiError::RateLimited)));
        assert!(s.has_data);
    }

    #[test]
    fn a_failing_since_request_keeps_the_last_figure_while_totals_move_on() {
        let (mut p, _http) = poller(
            Some(KEY),
            vec![ok(&totals_body()), ok(&since_body()), ok(&totals_body()), Ok((500, "{}".into()))],
        );
        p.set_since(Some(utc("2026-10-12T08:30:00Z")));
        let t0 = Instant::now();
        p.refresh(t0, cet("2026-10-15T12:00:00Z"));
        p.refresh(t0 + DEFAULT_API_INTERVAL, cet("2026-10-15T12:05:00Z"));
        let s = p.current().unwrap();
        assert_eq!(s.since_nano, Some(425_000_000));
        assert_eq!((s.error.clone(), s.since_error.clone()), (None, Some(ApiError::Server(500))));
        assert_eq!(s.updated.with_timezone(&Utc), utc("2026-10-15T12:05:00Z"));
    }

    #[test]
    fn baseline_in_the_same_utc_hour_makes_no_request_and_counts_zero() {
        let (mut p, http) = poller(Some(KEY), vec![ok(&totals_body())]);
        p.set_since(Some(utc("2026-10-15T12:10:00Z")));
        p.refresh(Instant::now(), cet("2026-10-15T12:50:00Z"));
        let s = p.current().unwrap();
        assert_eq!((s.since_nano, s.since_error.clone()), (Some(0), None), "counted as deducted, cost 0");
        assert_eq!(http.request_count(), 1, "only the totals request goes out");
    }

    #[test]
    fn since_range_is_floored_to_the_hour_on_both_ends() {
        let (mut p, http) = poller(Some(KEY), vec![ok(&totals_body()), ok(&since_body())]);
        p.set_since(Some(utc("2026-10-12T08:30:00Z")));
        p.refresh(Instant::now(), cet("2026-10-15T12:45:10Z"));
        let requests = http.requests.lock().unwrap();
        assert!(requests[1].0.contains("starting_at=2026-10-12T08%3A00%3A00Z"), "{}", requests[1].0);
        assert!(requests[1].0.contains("ending_at=2026-10-15T12%3A00%3A00Z"), "{}", requests[1].0);
    }

    #[test]
    fn baseline_older_than_365_days_makes_no_request_and_says_so() {
        let (mut p, http) = poller(Some(KEY), vec![ok(&totals_body())]);
        p.set_since(Some(utc("2025-10-14T00:00:00Z")));
        p.refresh(Instant::now(), cet("2026-10-15T12:00:00Z"));
        let s = p.current().unwrap();
        assert_eq!((s.since_nano, s.since_error.clone()), (None, Some(ApiError::BaselineTooOld)));
        assert_eq!(s.error, None);
        assert_eq!(s.today_nano, 1_000_000_000, "totals stay fresh");
        assert_eq!(http.request_count(), 1);
        assert_eq!(ApiError::BaselineTooOld.to_string(), "Startwert zu alt, bitte neu eintragen");

    }

    #[test]
    fn the_365_day_limit_is_exact() {
        let end = utc("2027-06-01T12:00:00Z");
        // No answers are prepared: a request that goes out fails with a network error, a refused one with TooOld.
        let http = FakeHttp::new(vec![]);
        let at_limit = fetch_since(&http, KEY, end - Duration::days(365), end);
        assert!(matches!(at_limit, Err(ApiError::Network(_))), "{at_limit:?}");
        assert_eq!(http.request_count(), 1);
        let http = FakeHttp::new(vec![]);
        let over = fetch_since(&http, KEY, end - Duration::days(365) - Duration::hours(1), end);
        assert_eq!(over, Err(ApiError::BaselineTooOld));
        assert_eq!(http.request_count(), 0);
    }

    fn at(s: &str, hours: i32) -> DateTime<FixedOffset> {
        utc(s).with_timezone(&FixedOffset::east_opt(hours * 3600).unwrap())
    }

    #[test]
    fn windows_handle_year_end_and_local_offsets() {
        // UTC+2, already Jan 1 locally while UTC is still Dec 31.
        let w = windows(&at("2026-12-31T23:30:00Z", 2));
        assert_eq!(w.today, NaiveDate::from_ymd_opt(2027, 1, 1).unwrap());
        assert_eq!(w.month, (2027, 1));
        assert_eq!(w.fetch, (utc("2027-01-01T00:00:00Z"), utc("2027-01-02T00:00:00Z")));

        // UTC-5: local Oct 14 21:00 while UTC is already Oct 15; both buckets must be fetched.
        let w = windows(&at("2026-10-15T02:00:00Z", -5));
        assert_eq!(w.today, NaiveDate::from_ymd_opt(2026, 10, 14).unwrap());
        assert_eq!(w.month, (2026, 10));
        assert_eq!(w.fetch, (utc("2026-10-01T00:00:00Z"), utc("2026-10-16T00:00:00Z")));

        // Last local day of the month with UTC already in the next month: range stops at the
        // month end (31 days), the next month's bucket counts for neither figure.
        let w = windows(&at("2026-11-01T02:00:00Z", -5));
        assert_eq!(w.today, NaiveDate::from_ymd_opt(2026, 10, 31).unwrap());
        assert_eq!(w.fetch, (utc("2026-10-01T00:00:00Z"), utc("2026-11-01T00:00:00Z")));
    }

    #[test]
    fn local_date_decides_today_even_across_utc_midnight() {
        // Local Jan 1 (UTC+2), UTC still Dec 31: the Jan 1 bucket is today and month,
        // the Dec 31 bucket belongs to the old month and is not counted.
        let body = page(
            &[
                row_json("2027-01-01T00:00:00Z", "2027-01-02T00:00:00Z", "m", "100.000000"),
                row_json("2026-12-31T00:00:00Z", "2027-01-01T00:00:00Z", "m", "300.000000"),
            ],
            None,
        );
        let (mut p, _http) = poller(Some(KEY), vec![ok(&body)]);
        p.refresh(Instant::now(), at("2026-12-31T23:30:00Z", 2));
        let s = p.current().unwrap();
        assert_eq!((s.today_nano, s.month_nano), (1_000_000_000, 1_000_000_000));
    }

    #[test]
    fn not_available_backs_off_for_60_minutes_then_recovers() {
        let (mut p, http) = poller(Some(KEY), vec![Ok((404, "{}".into())), ok(EMPTY), ok(EMPTY)]);
        let t0 = Instant::now();
        let wall = cet("2026-10-15T12:00:00Z");
        p.refresh(t0, wall);
        assert_eq!(p.current().unwrap().error, Some(ApiError::NotAvailable));

        assert!(!p.due(t0 + StdDuration::from_secs(300)), "normal interval must not apply");
        assert!(!p.due(t0 + NOT_AVAILABLE_INTERVAL - StdDuration::from_secs(1)));
        assert!(p.due(t0 + NOT_AVAILABLE_INTERVAL));

        // Once the endpoint works again, the 5-minute cadence is back.
        let t1 = t0 + NOT_AVAILABLE_INTERVAL;
        p.refresh(t1, wall);
        assert_eq!(p.current().unwrap().error, None);
        assert!(!p.due(t1 + StdDuration::from_secs(299)));
        assert!(p.due(t1 + DEFAULT_API_INTERVAL));
        assert_eq!(http.request_count(), 2);
    }

    /// Delegates to `FakeHttp` after sleeping, to exercise the overall deadline.
    struct SlowHttp {
        inner: FakeHttp,
        delay: StdDuration,
    }

    impl Http for SlowHttp {
        fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<(u16, String), NetError> {
            std::thread::sleep(self.delay);
            self.inner.get(url, headers)
        }
    }

    #[test]
    fn overall_deadline_stops_paging() {
        let more = page(&[], Some("page_next"));
        let http = SlowHttp {
            inner: FakeHttp::new(vec![ok(&more), ok(&more), ok(&more)]),
            delay: StdDuration::from_millis(60),
        };
        let deadline = Instant::now() + StdDuration::from_millis(100);
        let err = fetch_cost_until(
            &http,
            KEY,
            utc("2026-10-03T00:00:00Z"),
            utc("2026-10-04T00:00:00Z"),
            "1d",
            deadline,
        )
        .unwrap_err();
        assert_eq!(err, ApiError::Network("Zeitüberschreitung".to_string()));
        let made = http.inner.request_count();
        assert!((1..3).contains(&made), "expected paging to stop early, made {made} requests");
    }

    #[test]
    fn deadline_also_stops_before_the_next_chunk() {
        // Deadline already over: nothing is requested at all.
        let http = FakeHttp::new(vec![ok(EMPTY)]);
        let err = fetch_cost_until(
            &http,
            KEY,
            utc("2026-10-01T00:00:00Z"),
            utc("2026-11-10T00:00:00Z"),
            "1d",
            Instant::now(),
        )
        .unwrap_err();
        assert_eq!(err, ApiError::Network("Zeitüberschreitung".to_string()));
        assert_eq!(http.request_count(), 0);
    }

    /// Needs a stored admin key and network. Run manually (save the key via the app first):
    /// `cargo test live_cost_today -- --ignored --nocapture`. Prints amounts only.
    #[test]
    #[ignore]
    fn live_cost_today() {
        use crate::secrets::KeyringStore;
        let key = KeyringStore::new()
            .get(Slot::AdminKey)
            .expect("credential store")
            .expect("no admin key stored: save one in the app first");
        let now = Utc::now();
        let rows = fetch_cost(&UreqHttp::new(), &key, now - Duration::days(2), now + Duration::days(1), "1d")
            .expect("API call failed");
        let total: u64 = rows.iter().map(|r| r.amount_nano).sum();
        for r in &rows {
            println!("{} {:?}: {} nano-USD", r.start, r.model, r.amount_nano);
        }
        println!(
            "rows: {}, sum: {} nano-USD (${}.{:02})",
            rows.len(),
            total,
            total / 1_000_000_000,
            total / 10_000_000 % 100
        );
    }
}
