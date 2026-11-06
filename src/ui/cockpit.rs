//! The cockpit pieces: top bar, mode card with the start buttons, figure cards, API line, progress.

use eframe::egui::{
    self, Color32, CornerRadius, CursorIcon, Margin, RichText, Sense, Shadow, Stroke, StrokeKind, TextStyle, vec2,
};

use super::details::figure_value;
use super::theme::{BIG, palette::*};
use crate::analytics::ApiError;
use crate::cards::{ApiCard, BalanceCard};
use crate::format::{cost_note, progress_text, requests_text, unpriced_text};
use crate::i18n::{tr, tr_args};
use crate::launcher::Mode;
use crate::report::{Bucket, Report};
use crate::view::{DetailKind, ModeCard, Tone};

const CARD_RADIUS: u8 = 10;
/// Available width from which the four figure cards fit in one row.
const WIDE: f32 = 600.0;

fn big(text: impl Into<String>) -> RichText {
    RichText::new(text).text_style(TextStyle::Name(BIG.into())).strong()
}

/// Top bar: logo and name left, gear right. The gear toggles the settings drawer flag.
pub fn top_bar(ui: &mut egui::Ui, logo: &egui::TextureHandle, drawer_open: &mut bool) {
    ui.horizontal(|ui| {
        ui.add(egui::Image::new(logo).fit_to_exact_size(vec2(22.0, 22.0)));
        ui.label(RichText::new("Cerberus Usage").size(14.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let color = if *drawer_open { CYAN } else { MUTED };
            let gear = egui::Button::new(RichText::new("⚙").size(15.0).color(color))
                .min_size(vec2(28.0, 28.0))
                .stroke(Stroke::new(1.0, if *drawer_open { CYAN_DARK } else { LINE }));
            if ui.add(gear).on_hover_text(tr("ui.settings")).on_hover_cursor(CursorIcon::PointingHand).clicked() {
                *drawer_open = !*drawer_open;
            }
        });
    });
}

/// Fill, stroke and title colour of the mode card. Green only for `Tone::Green`.
fn mode_colors(tone: Tone) -> (Color32, Stroke, Color32) {
    match tone {
        Tone::Green => (PANEL.lerp_to_gamma(OK_GREEN, 0.10), Stroke::new(1.0, OK_GREEN.gamma_multiply(0.6)), OK_GREEN),
        Tone::Amber => (PANEL.lerp_to_gamma(AMBER, 0.10), Stroke::new(1.0, AMBER.gamma_multiply(0.7)), AMBER),
        Tone::AmberOutline => (PANEL, Stroke::new(1.5, AMBER), AMBER),
        Tone::Neutral => (PANEL, Stroke::new(1.0, NEUTRAL.gamma_multiply(0.6)), TEXT),
    }
}

/// The big mode card with both start buttons. Returns the mode whose button was clicked.
/// While a launch runs both buttons are off and a spinner shows what happens.
/// `blocked`: why neither button may be used now (an update is running).
pub fn mode_card_ui(
    ui: &mut egui::Ui,
    card: ModeCard,
    launching: Option<(Mode, bool)>,
    blocked: Option<&str>,
) -> Option<Mode> {
    let (fill, stroke, title_color) = mode_colors(card.tone);
    let mut start = None;
    egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(tr("cockpit.mode_heading")).small().color(MUTED));
            ui.label(big(card.title).color(title_color));
            ui.label(RichText::new(card.hint).color(MUTED));
            ui.add_space(4.0);
            let idle = launching.is_none() && blocked.is_none();
            let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
            ui.horizontal(|ui| {
                let max = egui::Button::new(RichText::new(tr("button.start_max")).strong().color(TEXT))
                    .fill(PANEL2)
                    .stroke(Stroke::new(1.0, LINE))
                    .min_size(vec2(width, 36.0));
                let max = ui.add_enabled(idle, max);
                let max = match blocked {
                    Some(text) => max.on_disabled_hover_text(text),
                    None => max,
                };
                if max.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    start = Some(Mode::Max);
                }
                let api = egui::Button::new(RichText::new(tr("button.start_api")).strong().color(AMBER))
                    .fill(PANEL2)
                    .stroke(Stroke::new(1.0, AMBER.gamma_multiply(0.5)))
                    .min_size(vec2(width, 36.0));
                let api = ui.add_enabled(idle, api);
                let api = match blocked {
                    Some(text) => api.on_disabled_hover_text(text),
                    None => api,
                };
                if api.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    start = Some(Mode::ApiKey);
                }
            });
            if let Some((_, confirmed)) = launching {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(tr(if confirmed { "cockpit.launch.closing" } else { "cockpit.launch.starting" }));
                });
            }
        });
    start
}

/// One clickable number card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Figure {
    pub kind: DetailKind,
    pub label: String,
    pub value: String,
    /// The value in amber (a pause).
    pub value_warn: bool,
    /// Lines under the value: text and whether it is a warning.
    pub lines: Vec<(String, bool)>,
}

fn bucket_lines(b: &Bucket) -> Vec<(String, bool)> {
    let mut lines = vec![(requests_text(b.requests), false), (cost_note(), false)];
    if let Some(text) = unpriced_text(b.unpriced_requests) {
        lines.push((text, true));
    }
    lines
}

/// Lines of the 5-hour card: its own first line, plus the unpriced hint when a number is too low.
fn last_5h_lines(b: &Bucket, first: String) -> Vec<(String, bool)> {
    let mut lines = vec![(first, false)];
    if let Some(text) = unpriced_text(b.unpriced_requests) {
        lines.push((text, true));
    }
    lines
}

/// The four cards: Heute, Monat, Guthaben, Max (or the pause). `balance_issue`: the balance has a
/// note or an error that the opened area explains.
pub fn figures(report: &Report, balance: &BalanceCard, balance_issue: bool) -> Vec<Figure> {
    let balance_figure = match balance {
        BalanceCard::NoBaseline => Figure {
            kind: DetailKind::Balance,
            label: tr("cockpit.label.balance"),
            value: "–".to_string(),
            value_warn: false,
            lines: vec![(tr("cockpit.balance.no_baseline"), false)],
        },
        BalanceCard::Estimate { value, deducted, .. } => {
            // One short line; the opened area has the full explanation.
            let line = match (*deducted, balance_issue) {
                (false, _) => ("cockpit.balance.estimated_no_deduction", true),
                (true, true) => ("cockpit.balance.estimated_note", true),
                (true, false) => ("cockpit.balance.estimated", false),
            };
            let lines = vec![(tr(line.0), line.1)];
            Figure {
                kind: DetailKind::Balance,
                label: tr("cockpit.label.balance"),
                value: value.clone(),
                value_warn: false,
                lines,
            }
        }
    };
    let max_figure = match &report.pause {
        Some(_) => Figure {
            kind: DetailKind::Max,
            label: tr("cockpit.label.max"),
            value: tr("cockpit.max.paused"),
            value_warn: true,
            lines: last_5h_lines(
                &report.last_5h,
                tr_args("cockpit.max.last5h", &[("value", &figure_value(&report.last_5h))]),
            ),
        },
        None => Figure {
            kind: DetailKind::Max,
            label: tr("cockpit.label.last5h"),
            value: figure_value(&report.last_5h),
            value_warn: false,
            lines: last_5h_lines(&report.last_5h, requests_text(report.last_5h.requests)),
        },
    };
    vec![
        Figure {
            kind: DetailKind::Today,
            label: tr("cockpit.label.today"),
            value: figure_value(&report.today),
            value_warn: false,
            lines: bucket_lines(&report.today),
        },
        Figure {
            kind: DetailKind::Month,
            label: tr("cockpit.label.month"),
            value: figure_value(&report.month),
            value_warn: false,
            lines: bucket_lines(&report.month),
        },
        balance_figure,
        max_figure,
    ]
}

/// Cards per row: two in the default window and at the minimum width, four in a wide one.
fn columns_for(width: f32) -> usize {
    if width >= WIDE { 4 } else { 2 }
}

fn kind_name(kind: DetailKind) -> String {
    tr(match kind {
        DetailKind::Today => "cockpit.card.today",
        DetailKind::Month => "cockpit.card.month",
        DetailKind::Balance => "cockpit.card.balance",
        DetailKind::Max => "cockpit.card.max",
    })
}

/// Screen-reader label of a figure card.
pub fn card_accessible_label(kind: DetailKind, open: bool) -> String {
    tr_args(if open { "cockpit.card.close" } else { "cockpit.card.open" }, &[("name", &kind_name(kind))])
}

/// A rounded, clickable card of exactly `width`. Label on top, big number below, short muted lines
/// that wrap inside the card. Hover or keyboard focus: cyan ring and pointer; open: dark cyan stroke and glow.
fn figure_card(ui: &mut egui::Ui, width: f32, figure: &Figure, open: bool) -> egui::Response {
    let margin = 12.0;
    // A frame adds its margin and a 1 px stroke on every side.
    let inner_width = (width - 2.0 * margin - 2.0).max(40.0);
    let mut frame = egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(Margin::same(margin as i8));
    if open {
        frame = frame
            .stroke(Stroke::new(1.0, CYAN_DARK))
            .shadow(Shadow { offset: [0, 0], blur: 12, spread: 0, color: CYAN.gamma_multiply(0.15) });
    }
    let inner = ui
        .allocate_ui_with_layout(vec2(width, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
            frame.show(ui, |ui| {
                ui.set_width(inner_width);
                ui.set_max_width(inner_width);
                ui.set_min_height(86.0);
                ui.style_mut().interaction.selectable_labels = false;
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.add(egui::Label::new(
                    RichText::new(format!("{}{}", figure.label, if open { " ▴" } else { "" })).small().color(MUTED),
                ));
                ui.add(egui::Label::new(big(&figure.value).color(if figure.value_warn { AMBER } else { TEXT })));
                for (text, warn) in &figure.lines {
                    ui.add(egui::Label::new(RichText::new(text).small().color(if *warn { AMBER } else { MUTED })).wrap());
                }
            })
        })
        .inner;
    let rect = inner.response.rect;
    // The id follows the card, not its label, so a language change keeps the widget state.
    let id = egui::Id::unique(("figure", format!("{:?}", figure.kind)));
    let response = ui.interact(rect, id, Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::Button, true, card_accessible_label(figure.kind, open))
    });
    if response.hovered() || response.has_focus() {
        ui.painter().rect_stroke(rect, CornerRadius::same(CARD_RADIUS), Stroke::new(1.5, CYAN), StrokeKind::Inside);
    }
    response
}

/// The figure cards wrapped in rows. Returns the card that was clicked.
pub fn figure_cards(ui: &mut egui::Ui, figures: &[Figure], open: Option<DetailKind>) -> Option<DetailKind> {
    let spacing = ui.spacing().item_spacing.x;
    let columns = columns_for(ui.available_width());
    let width = ((ui.available_width() - spacing * (columns as f32 - 1.0)) / columns as f32).floor();
    let mut clicked = None;
    for row in figures.chunks(columns) {
        ui.horizontal_top(|ui| {
            for figure in row {
                if figure_card(ui, width, figure, open == Some(figure.kind)).clicked() {
                    clicked = Some(figure.kind);
                }
            }
        });
    }
    clicked
}

/// First scan: where the figures will be. A bar and the file count.
pub fn scan_progress(ui: &mut egui::Ui, scanned: usize, total: usize) {
    ui.label(RichText::new(progress_text(scanned, total)).color(MUTED));
    let fraction = if total == 0 { 0.0 } else { (scanned as f32 / total as f32).clamp(0.0, 1.0) };
    ui.add(egui::ProgressBar::new(fraction).desired_height(8.0).fill(CYAN_DARK).animate(false));
}

/// "Not available" (a personal account has no Cost API) is an expected state, not a fault: dim, not amber.
pub fn api_error_is_dim(text: &str) -> bool {
    text == ApiError::NotAvailable.to_string()
}

fn wrapped(ui: &mut egui::Ui, text: RichText) {
    ui.add(egui::Label::new(text).wrap());
}

/// The API cost line or card under the figures. No key, nothing yet and errors are one dim line.
pub fn api_line(ui: &mut egui::Ui, view: &ApiCard) {
    match view {
        ApiCard::NoKey => wrapped(ui, RichText::new(tr("api.no_key")).color(MUTED)),
        ApiCard::Pending => wrapped(ui, RichText::new(tr("api.pending")).color(MUTED)),
        ApiCard::Error(text) => {
            let color = if api_error_is_dim(text) { MUTED } else { AMBER };
            wrapped(ui, RichText::new(tr_args("api.error", &[("error", text.as_str())])).color(color));
        }
        ApiCard::Data { today, month, updated, error } => {
            egui::Frame::new()
                .fill(PANEL)
                .stroke(Stroke::new(1.0, LINE))
                .corner_radius(CornerRadius::same(CARD_RADIUS))
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 2.0;
                    wrapped(ui, RichText::new(tr("api.title")).small().color(MUTED));
                    wrapped(ui, RichText::new(tr_args("api.today", &[("amount", today.as_str())])).size(16.0).strong());
                    wrapped(ui, RichText::new(tr_args("api.month", &[("amount", month.as_str())])));
                    wrapped(ui, RichText::new(tr_args("api.updated", &[("time", updated.as_str())])).small().color(MUTED));
                    wrapped(ui, RichText::new(tr("api.lag")).small().color(MUTED));
                    if let Some(text) = error {
                        let color = if api_error_is_dim(text) { MUTED } else { AMBER };
                        wrapped(ui, RichText::new(text).color(color));
                    }
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Pause;
    use crate::ui::theme::contrast_ratio;
    use chrono::Utc;

    fn report() -> Report {
        Report {
            today: Bucket { requests: 352, cost_nano: 25_970_000_000, ..Bucket::default() },
            month: Bucket { requests: 4000, cost_nano: 358_390_000_000, unpriced_requests: 2, ..Bucket::default() },
            last_5h: Bucket { requests: 10, cost_nano: 1_000_000_000, ..Bucket::default() },
            ..Report::default()
        }
    }

    fn estimate(deducted: bool) -> BalanceCard {
        BalanceCard::Estimate { value: "$65.03".into(), deducted, details: vec![], note: None }
    }

    #[test]
    fn four_cards_in_spec_order_open_their_details() {
        let f = figures(&report(), &estimate(true), false);
        let kinds: Vec<_> = f.iter().map(|x| x.kind).collect();
        assert_eq!(kinds, [DetailKind::Today, DetailKind::Month, DetailKind::Balance, DetailKind::Max]);
        assert_eq!(f[0].value, "$25.97");
        assert_eq!(f[1].value, "$358.39");
        assert_eq!(f[2].value, "$65.03");
    }

    #[test]
    fn local_figures_keep_the_estimate_note_and_unpriced_hint() {
        let f = figures(&report(), &estimate(true), false);
        assert!(f[0].lines.iter().any(|(t, _)| *t == cost_note()));
        assert!(!f[0].lines.iter().any(|(_, warn)| *warn), "today had every request priced");
        assert!(f[1].lines.iter().any(|(t, warn)| *warn && t == "2 Anfragen nicht bewertet"));
    }

    #[test]
    fn the_five_hour_card_carries_the_unpriced_hint_too() {
        let mut r = report();
        let clean = figures(&r, &estimate(true), false);
        assert!(!clean[3].lines.iter().any(|(_, warn)| *warn));
        r.last_5h.unpriced_requests = 3;
        let f = figures(&r, &estimate(true), false);
        assert_eq!(f[3].label, "LETZTE 5 STD.");
        assert!(f[3].lines.iter().any(|(t, warn)| *warn && t == "3 Anfragen nicht bewertet"));
        // The paused Max card shows the same number, so it carries the hint as well.
        r.pause = Some(Pause { until: Utc::now(), limit_type: "five_hour".into() });
        let paused = figures(&r, &estimate(true), false);
        assert!(paused[3].lines.iter().any(|(t, warn)| *warn && t == "3 Anfragen nicht bewertet"));
    }

    #[test]
    fn balance_states_stay_visible_on_the_card() {
        let none = figures(&report(), &BalanceCard::NoBaseline, false);
        assert_eq!(none[2].value, "–");
        assert!(none[2].lines[0].0.contains("Noch kein Startwert"));
        let no_deduction = figures(&report(), &estimate(false), true);
        assert_eq!(no_deduction[2].lines, [("geschätzt · ohne Kostenabzug".to_string(), true)]);
        let hint = figures(&report(), &estimate(true), true);
        assert_eq!(hint[2].lines, [("geschätzt · siehe Details".to_string(), true)]);
        assert_eq!(figures(&report(), &estimate(true), false)[2].lines, [("geschätzt".to_string(), false)]);
    }

    #[test]
    fn a_pause_replaces_the_max_card_without_any_green() {
        let mut r = report();
        r.pause = Some(Pause { until: Utc::now(), limit_type: "five_hour".into() });
        let paused = figures(&r, &estimate(true), false);
        assert_eq!(paused[3].value, "pausiert");
        assert!(paused[3].value_warn);
        let running = figures(&report(), &estimate(true), false);
        assert_eq!(running[3].label, "LETZTE 5 STD.");
        assert!(!running[3].value_warn);
    }

    #[test]
    fn cards_wrap_to_two_per_row_unless_the_window_is_wide() {
        assert_eq!(columns_for(396.0), 2, "420 px window");
        assert_eq!(columns_for(456.0), 2, "480 px window");
        assert_eq!(columns_for(599.0), 2);
        assert_eq!(columns_for(600.0), 4);
    }

    #[test]
    fn green_is_only_the_max_card_and_its_text_stays_readable() {
        for tone in [Tone::Amber, Tone::AmberOutline, Tone::Neutral] {
            let (fill, stroke, title) = mode_colors(tone);
            assert_ne!(title, OK_GREEN, "{tone:?}");
            assert_ne!(stroke.color, OK_GREEN, "{tone:?}");
            assert_ne!(fill, OK_GREEN);
        }
        for tone in [Tone::Green, Tone::Amber, Tone::AmberOutline, Tone::Neutral] {
            let (fill, _, title) = mode_colors(tone);
            assert!(contrast_ratio(title, fill) >= 4.5, "{tone:?} title");
            assert!(contrast_ratio(MUTED, fill) >= 4.5, "{tone:?} hint");
        }
        assert!(contrast_ratio(AMBER, PANEL2) >= 4.5, "API-Key button text");
    }

    #[test]
    fn not_available_is_a_dim_line_other_errors_stay_amber() {
        assert!(api_error_is_dim(&ApiError::NotAvailable.to_string()));
        for e in [ApiError::Unauthorized, ApiError::RateLimited, ApiError::Server(500)] {
            assert!(!api_error_is_dim(&e.to_string()), "{e:?}");
        }
        // The card carries exactly that text for a Cost API that was never available.
        let status = crate::analytics::ApiStatus {
            today_nano: 0,
            month_nano: 0,
            has_data: false,
            updated: chrono::Local::now(),
            since_nano: None,
            error: Some(ApiError::NotAvailable),
            since_error: None,
        };
        match crate::cards::api_card(Some(true), false, Some(&status)) {
            ApiCard::Error(text) => assert!(api_error_is_dim(&text)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn card_labels_are_german_and_follow_the_state() {
        assert_eq!(card_accessible_label(DetailKind::Today, false), "Heute, Details öffnen");
        assert_eq!(card_accessible_label(DetailKind::Balance, true), "Guthaben, Details schließen");
    }
}
