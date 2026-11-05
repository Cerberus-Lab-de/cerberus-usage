//! Pure, egui-free decisions behind the cockpit: mode card, detail toggling, day bars,
//! drawer width, Esc order and window growth.

use chrono::{Duration, NaiveDate};

use crate::report::Bucket;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeDisplay {
    Max,
    ApiKey,
    /// Obsidian runs, but not (or no longer verifiably) started by this app.
    Unknown,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Only for "Max, started by the app".
    Green,
    Amber,
    AmberOutline,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeCard {
    pub tone: Tone,
    pub title: &'static str,
    pub hint: &'static str,
}

pub fn mode_card(m: ModeDisplay) -> ModeCard {
    match m {
        ModeDisplay::Max => ModeCard {
            tone: Tone::Green,
            title: "Max",
            hint: "Keine API-Kosten, sofern in Claude keine andere Anmeldung eingerichtet ist.",
        },
        ModeDisplay::ApiKey => ModeCard { tone: Tone::Amber, title: "API-Key", hint: "Kosten nach Verbrauch." },
        ModeDisplay::Unknown => ModeCard {
            tone: Tone::AmberOutline,
            title: "Modus unbekannt",
            hint: "Kosten möglich. Über die App neu starten.",
        },
        ModeDisplay::Closed => ModeCard {
            tone: Tone::Neutral,
            title: "Obsidian geschlossen",
            hint: "Starte Obsidian mit Max oder API-Key.",
        },
    }
}

/// The mode card, but never green while a routing variable may redirect or re-bill requests: then
/// the Max claim ("keine API-Kosten") could be wrong and the card says so.
pub fn mode_card_with_routing(m: ModeDisplay, routing_active: bool) -> ModeCard {
    if routing_active && m == ModeDisplay::Max {
        return ModeCard {
            tone: Tone::AmberOutline,
            title: "Max",
            hint: "Umleitung aktiv, Kosten möglich. Siehe Hinweis oben.",
        };
    }
    mode_card(m)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailKind {
    Today,
    Month,
    Balance,
    Max,
}

/// Clicking the open detail closes it; clicking another one switches to it.
pub fn toggle_detail(open: Option<DetailKind>, clicked: DetailKind) -> Option<DetailKind> {
    if open == Some(clicked) { None } else { Some(clicked) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayBar {
    pub date: NaiveDate,
    pub cost_nano: u64,
    /// Requests happened, but none could be priced.
    pub unpriced_only: bool,
    pub is_today: bool,
}

/// Exactly `days` bars, oldest first, ending with `today`. Missing days are zero.
pub fn day_bars(by_day: &[(NaiveDate, Bucket)], today: NaiveDate, days: usize) -> Vec<DayBar> {
    (0..days)
        .map(|i| {
            let date = today - Duration::days((days - 1 - i) as i64);
            let b = by_day.iter().find(|(d, _)| *d == date).map(|(_, b)| *b).unwrap_or_default();
            DayBar {
                date,
                cost_nano: b.cost_nano,
                unpriced_only: b.requests > 0 && b.unpriced_requests == b.requests,
                is_today: date == today,
            }
        })
        .collect()
}

/// Pixel fraction in [0, 1]; the only place money meets `f32`.
pub fn bar_fraction(cost_nano: u64, max_nano: u64) -> f32 {
    if max_nano == 0 {
        return 0.0;
    }
    ((cost_nano as f64 / max_nano as f64) as f32).clamp(0.0, 1.0)
}

/// Shortens to `max_chars` characters (chars, not bytes) with a trailing `…`.
pub fn truncate_name(name: &str, max_chars: usize) -> (String, bool) {
    if name.chars().count() <= max_chars {
        return (name.to_string(), false);
    }
    let mut s: String = name.chars().take(max_chars.saturating_sub(1)).collect();
    s.push('…');
    (s, true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscTarget {
    Confirm,
    Drawer,
    Detail,
    Nothing,
}

/// Esc closes the topmost layer first: confirm, then drawer, then detail.
pub fn esc_target(confirm_open: bool, drawer_open: bool, detail_open: bool) -> EscTarget {
    if confirm_open {
        EscTarget::Confirm
    } else if drawer_open {
        EscTarget::Drawer
    } else if detail_open {
        EscTarget::Detail
    } else {
        EscTarget::Nothing
    }
}

/// Whether an Esc press may close a layer. egui drops the focus of a text field *before* the app runs
/// the frame in which Esc arrives, so `wants_now` alone would let that first Esc through: the field's
/// focus in the frame before counts too. The first Esc only leaves the field, a second one closes.
pub fn esc_may_close(esc_pressed: bool, text_focused_last_frame: bool, wants_keyboard_now: bool) -> bool {
    esc_pressed && !text_focused_last_frame && !wants_keyboard_now
}

/// 360 px, or the full window width when narrower than 560 px.
pub fn drawer_width(window_width: f32) -> f32 {
    if window_width >= 560.0 { 360.0 } else { window_width }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Growth {
    pub target_height: f32,
    pub added: f32,
}

/// Height to grow to when content needs more room; never beyond the work area, never when the
/// user resized the window.
pub fn grow_for_content(current: f32, needed: f32, work_area: f32, user_resized: bool) -> Option<Growth> {
    if user_resized || needed <= current {
        return None;
    }
    let target_height = needed.min(work_area);
    let added = target_height - current;
    if added <= 0.0 {
        return None;
    }
    Some(Growth { target_height, added })
}

/// Returns only the height the app added; nothing when the user resized.
pub fn shrink_after_collapse(current: f32, added: f32, user_resized: bool) -> Option<f32> {
    if user_resized || added <= 0.0 {
        return None;
    }
    Some(current - added)
}

/// Smallest window height the app ever sets (matches `with_min_inner_size`).
pub const MIN_HEIGHT: f32 = 480.0;
/// Height reserved for the taskbar when the work area is derived from the monitor size.
pub const TASKBAR_RESERVE: f32 = 48.0;
/// How far the actual window size may differ from the requested one before it counts as a user resize.
pub const RESIZE_TOLERANCE: f32 = 2.0;

/// Usable height: monitor height minus the taskbar. `None` (monitor unknown): the app does not grow.
pub fn work_area_height(monitor_height: Option<f32>) -> Option<f32> {
    monitor_height.map(|h| (h - TASKBAR_RESERVE).max(0.0))
}

/// The window has a size other than the one the app asked for last: the user resized it.
pub fn differs_from_request(actual: (f32, f32), requested: (f32, f32)) -> bool {
    (actual.0 - requested.0).abs() > RESIZE_TOLERANCE || (actual.1 - requested.1).abs() > RESIZE_TOLERANCE
}

/// Inner height the window may grow to: monitor height minus taskbar, minus the title bar and borders
/// (`chrome`), and, when the window top is known, minus the part of the screen above the window.
/// `None` (monitor unknown): the app does not grow.
pub fn grow_cap(monitor_height: Option<f32>, chrome: f32, window_top: Option<f32>) -> Option<f32> {
    let usable = work_area_height(monitor_height)?;
    let mut cap = usable - chrome.max(0.0);
    if let Some(top) = window_top.filter(|t| *t >= 0.0 && *t < usable) {
        cap = cap.min(usable - top - chrome.max(0.0));
    }
    Some(cap.max(0.0))
}

/// What the window should do this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrowAction {
    None,
    /// Measure the settled content and grow if it does not fit.
    Grow,
    /// Give back the height the app added.
    Shrink,
}

/// Decides when to measure. A content change is requested while the frame is already laid out, so the
/// measurement waits one frame (`armed`) and, for opening, until the animation has finished.
#[derive(Debug, Default, Clone, Copy)]
pub struct GrowState {
    want_grow: bool,
    armed: bool,
    want_shrink: bool,
}

impl GrowState {
    /// Content got taller (opened, switched, rows expanded).
    pub fn content_grew(&mut self) {
        self.want_grow = true;
        self.armed = false;
        self.want_shrink = false;
    }

    /// The detail is closing.
    pub fn content_closing(&mut self) {
        self.want_shrink = true;
        self.want_grow = false;
        self.armed = false;
    }

    /// Call once at the end of every frame with the detail openness (0..=1).
    pub fn on_frame(&mut self, openness: f32) -> GrowAction {
        if self.want_grow {
            if !self.armed {
                self.armed = true;
                return GrowAction::None;
            }
            if openness >= 1.0 {
                self.want_grow = false;
                self.armed = false;
                return GrowAction::Grow;
            }
        }
        if self.want_shrink && openness <= 0.0 {
            self.want_shrink = false;
            return GrowAction::Shrink;
        }
        GrowAction::None
    }

    /// A grow or shrink is waiting for a later frame.
    pub fn pending(&self) -> bool {
        self.want_grow || self.want_shrink
    }
}

/// `grow_for_content` with the work area possibly unknown and a result never below the current height.
pub fn growth_for(current: f32, needed: f32, work_area: Option<f32>, user_resized: bool) -> Option<Growth> {
    let work_area = work_area?;
    let g = grow_for_content(current, needed, work_area, user_resized)?;
    let target_height = g.target_height.max(current);
    Some(Growth { target_height, added: target_height - current })
}

/// `shrink_after_collapse` clamped to the minimum height; `None` when nothing would change.
pub fn shrink_to(current: f32, added: f32, user_resized: bool) -> Option<f32> {
    let h = shrink_after_collapse(current, added, user_resized)?.max(MIN_HEIGHT);
    (h < current).then_some(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn mode_card_green_only_for_max() {
        assert_eq!(mode_card(ModeDisplay::Max).tone, Tone::Green);
        assert_eq!(mode_card(ModeDisplay::ApiKey).tone, Tone::Amber);
        assert_eq!(mode_card(ModeDisplay::Unknown).tone, Tone::AmberOutline);
        assert_eq!(mode_card(ModeDisplay::Closed).tone, Tone::Neutral);
        for m in [ModeDisplay::ApiKey, ModeDisplay::Unknown, ModeDisplay::Closed] {
            assert_ne!(mode_card(m).tone, Tone::Green);
        }
    }

    #[test]
    fn mode_card_texts_match_spec() {
        assert!(mode_card(ModeDisplay::Max).hint.contains("sofern in Claude keine andere Anmeldung"));
        assert_eq!(mode_card(ModeDisplay::Max).title, "Max");
        assert_eq!(mode_card(ModeDisplay::ApiKey).title, "API-Key");
        assert_eq!(mode_card(ModeDisplay::Unknown).title, "Modus unbekannt");
        assert_eq!(mode_card(ModeDisplay::Closed).title, "Obsidian geschlossen");
        assert_eq!(mode_card(ModeDisplay::Closed).hint, "Starte Obsidian mit Max oder API-Key.");
    }

    #[test]
    fn toggle_opens_switches_and_closes() {
        assert_eq!(toggle_detail(None, DetailKind::Month), Some(DetailKind::Month));
        assert_eq!(toggle_detail(Some(DetailKind::Month), DetailKind::Month), None);
        assert_eq!(toggle_detail(Some(DetailKind::Today), DetailKind::Month), Some(DetailKind::Month));
    }

    #[test]
    fn day_bars_fill_missing_days_and_mark_today() {
        let today = d(2026, 11, 5);
        let b = Bucket { requests: 2, cost_nano: 5, ..Default::default() };
        let by_day = vec![(today, b), (d(2026, 11, 1), b)];
        let bars = day_bars(&by_day, today, 30);
        assert_eq!(bars.len(), 30);
        assert_eq!(bars[0].date, d(2026, 10, 7));
        assert_eq!(bars[29].date, today);
        assert!(bars[29].is_today);
        assert!(bars[..29].iter().all(|x| !x.is_today));
        assert_eq!(bars[29].cost_nano, 5);
        assert_eq!(bars[25].date, d(2026, 11, 1));
        assert_eq!(bars[25].cost_nano, 5);
        assert_eq!(bars[28].cost_nano, 0);
        assert!(!bars[28].unpriced_only);
    }

    #[test]
    fn day_bars_mark_unpriced_only_days() {
        let today = d(2026, 11, 5);
        let only = Bucket { requests: 3, unpriced_requests: 3, ..Default::default() };
        let mixed = Bucket { requests: 3, unpriced_requests: 1, cost_nano: 9, ..Default::default() };
        let by_day = vec![(today, only), (d(2026, 11, 4), mixed)];
        let bars = day_bars(&by_day, today, 3);
        assert!(bars[2].unpriced_only);
        assert_eq!(bars[2].cost_nano, 0);
        assert!(!bars[1].unpriced_only);
        assert!(!bars[0].unpriced_only);
    }

    #[test]
    fn bar_fraction_is_clamped_and_zero_safe() {
        assert_eq!(bar_fraction(5, 0), 0.0);
        assert_eq!(bar_fraction(0, 0), 0.0);
        assert_eq!(bar_fraction(20, 10), 1.0);
        assert_eq!(bar_fraction(5, 10), 0.5);
        assert_eq!(bar_fraction(0, 10), 0.0);
    }

    #[test]
    fn truncate_name_keeps_short_and_marks_cut() {
        assert_eq!(truncate_name("abc", 10), ("abc".to_string(), false));
        assert_eq!(truncate_name("abcde", 5), ("abcde".to_string(), false));
        let (s, cut) = truncate_name("Überlängenprojekt", 6);
        assert!(cut);
        assert_eq!(s, "Überl…");
        assert_eq!(s.chars().count(), 6);
    }

    #[test]
    fn esc_closes_topmost_layer_first() {
        assert_eq!(esc_target(true, true, true), EscTarget::Confirm);
        assert_eq!(esc_target(false, true, true), EscTarget::Drawer);
        assert_eq!(esc_target(false, false, true), EscTarget::Detail);
        assert_eq!(esc_target(false, false, false), EscTarget::Nothing);
    }

    #[test]
    fn drawer_is_360_or_full_width_below_560() {
        assert_eq!(drawer_width(800.0), 360.0);
        assert_eq!(drawer_width(560.0), 360.0);
        assert_eq!(drawer_width(559.0), 559.0);
        assert_eq!(drawer_width(420.0), 420.0);
    }

    #[test]
    fn window_growth_never_exceeds_work_area() {
        let g = grow_for_content(640.0, 900.0, 800.0, false).unwrap();
        assert_eq!(g.target_height, 800.0);
        assert_eq!(g.added, 160.0);
        let g = grow_for_content(640.0, 700.0, 800.0, false).unwrap();
        assert_eq!((g.target_height, g.added), (700.0, 60.0));
        assert!(grow_for_content(640.0, 600.0, 800.0, false).is_none());
        assert!(grow_for_content(640.0, 640.0, 800.0, false).is_none());
        // Already at or above the work area: nothing to add.
        assert!(grow_for_content(800.0, 900.0, 800.0, false).is_none());
        assert!(grow_for_content(820.0, 900.0, 800.0, false).is_none());
    }

    #[test]
    fn window_growth_respects_user_resize() {
        assert!(grow_for_content(640.0, 900.0, 800.0, true).is_none());
        assert!(shrink_after_collapse(800.0, 160.0, true).is_none());
    }

    #[test]
    fn window_shrinks_only_by_what_it_added() {
        assert_eq!(shrink_after_collapse(800.0, 160.0, false), Some(640.0));
        assert_eq!(shrink_after_collapse(800.0, 0.0, false), None);
        assert_eq!(shrink_after_collapse(800.0, -5.0, false), None);
    }

    #[test]
    fn work_area_is_monitor_minus_taskbar_or_unknown() {
        assert_eq!(work_area_height(Some(1080.0)), Some(1032.0));
        assert_eq!(work_area_height(None), None);
        assert_eq!(work_area_height(Some(20.0)), Some(0.0));
    }

    #[test]
    fn user_resize_is_detected_with_tolerance() {
        assert!(!differs_from_request((480.0, 640.0), (480.0, 640.0)));
        assert!(!differs_from_request((481.5, 638.5), (480.0, 640.0)));
        assert!(differs_from_request((480.0, 643.0), (480.0, 640.0)));
        assert!(differs_from_request((500.0, 640.0), (480.0, 640.0)));
    }

    #[test]
    fn growth_needs_a_known_work_area_and_never_goes_below_current() {
        assert!(growth_for(640.0, 900.0, None, false).is_none());
        let g = growth_for(640.0, 900.0, Some(800.0), false).unwrap();
        assert_eq!((g.target_height, g.added), (800.0, 160.0));
        assert!(growth_for(640.0, 900.0, Some(500.0), false).is_none());
        assert!(growth_for(640.0, 900.0, Some(800.0), true).is_none());
    }

    #[test]
    fn shrink_is_clamped_to_the_minimum_height() {
        assert_eq!(shrink_to(800.0, 160.0, false), Some(640.0));
        assert_eq!(shrink_to(560.0, 200.0, false), Some(MIN_HEIGHT));
        assert_eq!(shrink_to(480.0, 200.0, false), None);
        assert_eq!(shrink_to(800.0, 160.0, true), None);
        assert_eq!(shrink_to(800.0, 0.0, false), None);
    }

    #[test]
    fn max_is_green_only_without_routing() {
        assert_eq!(mode_card_with_routing(ModeDisplay::Max, false).tone, Tone::Green);
        let routed = mode_card_with_routing(ModeDisplay::Max, true);
        assert_ne!(routed.tone, Tone::Green);
        assert_eq!(routed.tone, Tone::AmberOutline);
        assert_eq!(routed.hint, "Umleitung aktiv, Kosten möglich. Siehe Hinweis oben.");
        for m in [ModeDisplay::ApiKey, ModeDisplay::Unknown, ModeDisplay::Closed] {
            assert_eq!(mode_card_with_routing(m, true), mode_card(m));
            assert_eq!(mode_card_with_routing(m, false), mode_card(m));
        }
    }

    #[test]
    fn cost_hints_carry_an_uncertainty_qualifier() {
        assert!(mode_card(ModeDisplay::Max).hint.contains("sofern"));
        assert!(mode_card(ModeDisplay::ApiKey).hint.contains("nach Verbrauch"));
        assert!(mode_card(ModeDisplay::Unknown).hint.contains("möglich"));
        assert!(mode_card_with_routing(ModeDisplay::Max, true).hint.contains("möglich"));
    }

    #[test]
    fn grow_cap_accounts_for_title_bar_and_window_position() {
        assert_eq!(grow_cap(None, 30.0, Some(0.0)), None);
        assert_eq!(grow_cap(Some(1080.0), 0.0, None), Some(1032.0));
        assert_eq!(grow_cap(Some(1080.0), 31.0, None), Some(1001.0));
        // Window starts 200 px down: only the rest of the screen is left.
        assert_eq!(grow_cap(Some(1080.0), 31.0, Some(200.0)), Some(801.0));
        // Nonsensical top: plain cap.
        assert_eq!(grow_cap(Some(1080.0), 31.0, Some(-5000.0)), Some(1001.0));
        assert_eq!(grow_cap(Some(1080.0), 31.0, Some(5000.0)), Some(1001.0));
    }

    fn frames(g: &mut GrowState, openness: &[f32]) -> Vec<GrowAction> {
        openness.iter().map(|o| g.on_frame(*o)).collect()
    }

    #[test]
    fn grow_waits_a_frame_and_for_the_open_animation() {
        use GrowAction::*;
        let mut g = GrowState::default();
        g.content_grew(); // clicked while closed
        assert_eq!(frames(&mut g, &[0.0, 0.4, 1.0]), [None, None, Grow]);
        assert_eq!(g.on_frame(1.0), None, "only once");
        assert!(!g.pending());
    }

    #[test]
    fn switching_the_detail_measures_the_frame_after_the_change() {
        use GrowAction::*;
        let mut g = GrowState::default();
        g.content_grew(); // switched Guthaben -> Heute, still fully open
        assert_eq!(g.on_frame(1.0), None, "same frame: old content");
        assert_eq!(g.on_frame(1.0), Grow);
    }

    #[test]
    fn expanding_rows_measures_the_frame_after_the_change() {
        use GrowAction::*;
        let mut g = GrowState::default();
        assert_eq!(g.on_frame(1.0), None);
        g.content_grew();
        assert_eq!(frames(&mut g, &[1.0, 1.0, 1.0]), [None, Grow, None]);
    }

    #[test]
    fn closing_shrinks_only_when_fully_closed_and_cancels_a_grow() {
        use GrowAction::*;
        let mut g = GrowState::default();
        g.content_closing();
        assert_eq!(frames(&mut g, &[0.7, 0.2, 0.0, 0.0]), [None, None, Shrink, None]);
        let mut g = GrowState::default();
        g.content_grew();
        g.content_closing();
        assert_eq!(frames(&mut g, &[0.5, 0.0]), [None, Shrink]);
        let mut g = GrowState::default();
        g.content_closing();
        g.content_grew();
        assert_eq!(frames(&mut g, &[0.0, 0.5, 1.0]), [None, None, Grow], "reopen cancels the shrink");
    }

    #[test]
    fn first_esc_only_leaves_a_focused_text_field() {
        // Field focused in the last frame: egui already dropped the focus, the Esc is still the field's.
        assert!(!esc_may_close(true, true, false));
        assert!(!esc_may_close(true, false, true));
        // Second Esc, nothing focused: closes.
        assert!(esc_may_close(true, false, false));
        assert!(!esc_may_close(false, false, false));
    }
}
