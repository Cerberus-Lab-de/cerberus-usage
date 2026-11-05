//! List-price cost estimate in nano-USD (integer math only).
//!
//! Rates in `prices.json` are milli-USD per MTok, so `tokens * rate` is nano-USD directly.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::log_scan::UsageRecord;

const EMBEDDED: &str = include_str!("../assets/prices.json");

#[derive(Debug, Clone, Deserialize)]
struct ModelRate {
    id: String,
    /// Upper bound (inclusive) of the prompt length this tier applies to; `None` = no limit.
    #[serde(default)]
    max_prompt_tokens: Option<u64>,
    input: u64,
    cache_5m: u64,
    cache_1h: u64,
    cache_read: u64,
    output: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PriceTable {
    as_of: String,
    models: Vec<ModelRate>,
}

impl PriceTable {
    /// The table shipped inside the binary.
    pub fn embedded() -> PriceTable {
        Self::parse(EMBEDDED).expect("embedded prices.json must be valid")
    }

    /// Load an override file; falls back to the embedded table if it is missing or broken.
    pub fn load(override_path: &Path) -> PriceTable {
        std::fs::read_to_string(override_path)
            .ok()
            .and_then(|s| Self::parse(&s))
            .unwrap_or_else(Self::embedded)
    }

    /// Default override location: `%LOCALAPPDATA%\CerberusUsage\prices.json`.
    pub fn default_override_path() -> Option<PathBuf> {
        let base = std::env::var_os("LOCALAPPDATA")?;
        Some(PathBuf::from(base).join("CerberusUsage").join("prices.json"))
    }

    fn parse(s: &str) -> Option<PriceTable> {
        let t: PriceTable = serde_json::from_str(s).ok()?;
        if t.models.is_empty() { None } else { Some(t) }
    }

    /// Date of the price documentation this table reflects.
    pub fn as_of(&self) -> &str {
        &self.as_of
    }

    /// Estimated list-price cost in nano-USD, or `None` for unknown models and
    /// fast / US-only requests (priced differently, not estimated).
    pub fn cost_nanousd(&self, r: &UsageRecord) -> Option<u64> {
        if r.speed.as_deref() == Some("fast") || r.geo.as_deref() == Some("us") {
            return None;
        }
        let prompt = r
            .input
            .saturating_add(r.cache_read)
            .saturating_add(r.cache_5m)
            .saturating_add(r.cache_1h);
        let rate = self.models.iter().find(|m| {
            model_matches(&m.id, &r.model) && m.max_prompt_tokens.is_none_or(|max| prompt <= max)
        })?;
        let parts = [
            (r.input, rate.input),
            (r.cache_5m, rate.cache_5m),
            (r.cache_1h, rate.cache_1h),
            (r.cache_read, rate.cache_read),
            (r.output, rate.output),
        ];
        Some(
            parts
                .iter()
                .fold(0u64, |acc, &(tokens, per_mtok)| acc.saturating_add(tokens.saturating_mul(per_mtok))),
        )
    }
}

/// Exact name, or name plus `-` and an 8-digit date suffix (so `claude-opus-5` never matches `claude-opus-5-5`).
fn model_matches(id: &str, model: &str) -> bool {
    if model == id {
        return true;
    }
    model
        .strip_prefix(id)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|date| date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()))
}

/// Format nano-USD as USD with two decimals, rounding half up.
pub fn format_usd(nano: u64) -> String {
    const CENT: u64 = 10_000_000;
    let cents = nano / CENT + u64::from(nano % CENT >= CENT / 2);
    format!("${}.{:02}", cents / 100, cents % 100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn rec(model: &str) -> UsageRecord {
        UsageRecord {
            message_id: "msg_test".into(),
            ts: Utc::now(),
            model: model.into(),
            project: "p".into(),
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

    #[test]
    fn opus55_doc_example() {
        let t = PriceTable::embedded();
        let mut r = rec("claude-opus-5-5");
        r.input = 10_000;
        r.cache_read = 40_000;
        let c = t.cost_nanousd(&r).unwrap();
        assert_eq!(c, 48_000_000);
        assert_eq!(format_usd(c), "$0.05");
    }

    #[test]
    fn output_rate() {
        let mut r = rec("claude-opus-5-5");
        r.output = 15_000;
        assert_eq!(PriceTable::embedded().cost_nanousd(&r), Some(300_000_000));
    }

    #[test]
    fn splits_cache_ttls() {
        let t = PriceTable::embedded();
        let mut r = rec("claude-opus-5-5");
        r.cache_5m = 1_000_000;
        assert_eq!(format_usd(t.cost_nanousd(&r).unwrap()), "$5.00");
        let mut r = rec("claude-opus-5-5");
        r.cache_1h = 1_000_000;
        assert_eq!(format_usd(t.cost_nanousd(&r).unwrap()), "$8.00");
    }

    #[test]
    fn opus5_does_not_match_opus55() {
        let t = PriceTable::embedded();
        let mut a = rec("claude-opus-5-5");
        a.input = 1_000_000;
        let mut b = rec("claude-opus-5");
        b.input = 1_000_000;
        assert_eq!(t.cost_nanousd(&a), Some(4_000_000_000));
        assert_eq!(t.cost_nanousd(&b), Some(5_000_000_000));
    }

    #[test]
    fn dated_model_matches() {
        let t = PriceTable::embedded();
        let mut r = rec("claude-opus-5-20261001");
        r.input = 1_000_000;
        assert_eq!(t.cost_nanousd(&r), Some(5_000_000_000));
        let mut bad = rec("claude-opus-5-2026");
        bad.input = 1;
        assert_eq!(t.cost_nanousd(&bad), None);
    }

    #[test]
    fn unknown_model_is_none() {
        let mut r = rec("claude-unknown-9");
        r.input = 1;
        assert_eq!(PriceTable::embedded().cost_nanousd(&r), None);
    }

    #[test]
    fn fast_and_us_are_none() {
        let t = PriceTable::embedded();
        let mut r = rec("claude-opus-5-5");
        r.input = 1000;
        r.speed = Some("fast".into());
        assert_eq!(t.cost_nanousd(&r), None);
        let mut r = rec("claude-opus-5-5");
        r.input = 1000;
        r.geo = Some("us".into());
        assert_eq!(t.cost_nanousd(&r), None);
        let mut r = rec("claude-opus-5-5");
        r.input = 1000;
        r.speed = Some("standard".into());
        r.geo = Some("global".into());
        assert!(t.cost_nanousd(&r).is_some());
    }

    #[test]
    fn haiku_switches_above_100k_prompt_tokens() {
        let t = PriceTable::embedded();
        let mut lo = rec("claude-haiku-5-5");
        lo.input = 100_000;
        assert_eq!(t.cost_nanousd(&lo), Some(100_000 * 100));
        let mut hi = rec("claude-haiku-5-5");
        hi.input = 100_000;
        hi.cache_read = 1;
        assert_eq!(t.cost_nanousd(&hi), Some(100_000 * 500 + 50));
    }

    #[test]
    fn broken_override_falls_back_to_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("prices.json");
        std::fs::write(&p, "{ not json").unwrap();
        let t = PriceTable::load(&p);
        assert_eq!(t.as_of(), PriceTable::embedded().as_of());
        let missing = PriceTable::load(&dir.path().join("nope.json"));
        assert_eq!(missing.as_of(), "2026-11-05");
        let mut r = rec("claude-opus-5-5");
        r.output = 15_000;
        assert_eq!(t.cost_nanousd(&r), Some(300_000_000));
    }

    #[test]
    fn valid_override_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("prices.json");
        std::fs::write(
            &p,
            r#"{"as_of":"2030-01-01","models":[{"id":"claude-x","input":1,"cache_5m":1,"cache_1h":1,"cache_read":1,"output":1}]}"#,
        )
        .unwrap();
        let t = PriceTable::load(&p);
        assert_eq!(t.as_of(), "2030-01-01");
    }

    #[test]
    fn format_usd_rounds_half_up() {
        assert_eq!(format_usd(0), "$0.00");
        assert_eq!(format_usd(4_999_999), "$0.00");
        assert_eq!(format_usd(5_000_000), "$0.01");
        assert_eq!(format_usd(1_234_000_000_000), "$1234.00");
    }

    #[test]
    fn overflow_saturates() {
        let mut r = rec("claude-fable-5");
        r.output = u64::MAX;
        assert_eq!(PriceTable::embedded().cost_nanousd(&r), Some(u64::MAX));
    }

    #[test]
    fn haiku_45_with_date_suffix_and_older_models_are_priced() {
        let t = PriceTable::embedded();
        // 1M input + 1M output of Haiku 4.5 = $1 + $5.
        let mut r = rec("claude-haiku-4-5-20251001");
        r.input = 1_000_000;
        r.output = 1_000_000;
        assert_eq!(t.cost_nanousd(&r), Some(6_000_000_000));
        for id in ["claude-opus-4-7", "claude-opus-4-1-20250805", "claude-opus-4-20250514", "claude-sonnet-4-5-20250929", "claude-haiku-3-5-20241022"] {
            let mut r = rec(id);
            r.input = 1;
            assert!(t.cost_nanousd(&r).is_some(), "{id} unpriced");
        }
        // Opus 4 ($15) must not be confused with Opus 4.1 or 4.5.
        let mut r = rec("claude-opus-4-5");
        r.input = 1_000_000;
        assert_eq!(t.cost_nanousd(&r), Some(5_000_000_000));
    }
}
