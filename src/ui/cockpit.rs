//! The cockpit pieces: top bar, mode card with the start buttons, figure cards, API line, progress.

use eframe::egui::{
    self, Color32, CornerRadius, CursorIcon, Margin, RichText, Sense, Shadow, Stroke, StrokeKind, TextStyle, vec2,
};

use super::details::figure_value;
use super::theme::{BIG, palette::*};
use crate::cards::{ApiCard, BalanceCard};
use crate::format::{COST_NOTE, format_count, progress_text, unpriced_text};
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
            if ui.add(gear).on_hover_text("Einstellungen").on_hover_cursor(CursorIcon::PointingHand).clicked() {
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
pub fn mode_card_ui(ui: &mut egui::Ui, card: ModeCard, launching: Option<(Mode, bool)>) -> Option<Mode> {
    let (fill, stroke, title_color) = mode_colors(card.tone);
    let mut start = None;
    egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("AKTUELLER MODUS").small().color(MUTED));
            ui.label(big(card.title).color(title_color));
            ui.label(RichText::new(card.hint).color(MUTED));
            ui.add_space(4.0);
            let idle = launching.is_none();
            let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
            ui.horizontal(|ui| {
                let max = egui::Button::new(RichText::new("Mit Max starten").strong().color(TEXT))
                    .fill(PANEL2)
                    .stroke(Stroke::new(1.0, LINE))
                    .min_size(vec2(width, 36.0));
                if ui.add_enabled(idle, max).on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    start = Some(Mode::Max);
                }
                let api = egui::Button::new(RichText::new("Mit API-Key starten").strong().color(AMBER))
                    .fill(PANEL2)
                    .stroke(Stroke::new(1.0, AMBER.gamma_multiply(0.5)))
                    .min_size(vec2(width, 36.0));
                if ui.add_enabled(idle, api).on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    start = Some(Mode::ApiKey);
                }
            });
            if let Some((_, confirmed)) = launching {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(if confirmed { "Obsidian wird geschlossen …" } else { "Obsidian wird gestartet …" });
                });
            }
        });
    start
}

/// One clickable number card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Figure {
    pub kind: DetailKind,
    pub label: &'static str,
    pub value: String,
    /// The value in amber (a pause).
    pub value_warn: bool,
    /// Lines under the value: text and whether it is a warning.
    pub lines: Vec<(String, bool)>,
}

fn bucket_lines(b: &Bucket) -> Vec<(String, bool)> {
    let mut lines = vec![(format!("{} Anfragen", format_count(b.requests)), false), (COST_NOTE.to_string(), false)];
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
            label: "GUTHABEN",
            value: "–".to_string(),
            value_warn: false,
            lines: vec![("Noch kein Startwert".to_string(), false)],
        },
        BalanceCard::Estimate { value, deducted, .. } => {
            let mut lines = vec![(
                (if *deducted { "geschätzt" } else { "geschätzt, ohne Kostenabzug" }).to_string(),
                !*deducted,
            )];
            if balance_issue {
                lines.push(("Hinweis, Klick für Details".to_string(), true));
            }
            Figure { kind: DetailKind::Balance, label: "GUTHABEN", value: value.clone(), value_warn: false, lines }
        }
    };
    let max_figure = match &report.pause {
        Some(_) => Figure {
            kind: DetailKind::Max,
            label: "MAX",
            value: "pausiert".to_string(),
            value_warn: true,
            lines: vec![(format!("letzte 5 Std. {}", figure_value(&report.last_5h)), false)],
        },
        None => Figure {
            kind: DetailKind::Max,
            label: "LETZTE 5 STD.",
            value: figure_value(&report.last_5h),
            value_warn: false,
            lines: vec![(format!("{} Anfragen", format_count(report.last_5h.requests)), false)],
        },
    };
    vec![
        Figure {
            kind: DetailKind::Today,
            label: "HEUTE",
            value: figure_value(&report.today),
            value_warn: false,
            lines: bucket_lines(&report.today),
        },
        Figure {
            kind: DetailKind::Month,
            label: "MONAT",
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

/// A rounded, clickable card. Hover: cyan stroke and pointer; open: dark cyan stroke and a soft glow.
fn figure_card(ui: &mut egui::Ui, width: f32, figure: &Figure, open: bool) -> egui::Response {
    let stroke = Stroke::new(1.0, LINE);
    let mut frame = egui::Frame::new()
        .fill(PANEL)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(Margin::same(12));
    if open {
        frame = frame
            .stroke(Stroke::new(1.0, CYAN_DARK))
            .shadow(Shadow { offset: [0, 0], blur: 12, spread: 0, color: CYAN.gamma_multiply(0.15) });
    }
    let inner = frame.show(ui, |ui| {
        ui.set_width(width - 26.0);
        ui.set_min_height(92.0);
        ui.style_mut().interaction.selectable_labels = false;
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(RichText::new(format!("{}{}", figure.label, if open { " ▴" } else { "" })).small().color(MUTED));
        ui.label(big(&figure.value).color(if figure.value_warn { AMBER } else { TEXT }));
        for (text, warn) in &figure.lines {
            ui.label(RichText::new(text).small().color(if *warn { AMBER } else { MUTED }));
        }
    });
    let id = egui::Id::unique(("figure", figure.label));
    let response = ui.interact(inner.response.rect, id, Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    if response.hovered() {
        ui.painter().rect_stroke(inner.response.rect, CornerRadius::same(CARD_RADIUS), Stroke::new(1.0, CYAN), StrokeKind::Inside);
    }
    response
}

/// The figure cards wrapped in rows. Returns the card that was clicked.
pub fn figure_cards(ui: &mut egui::Ui, figures: &[Figure], open: Option<DetailKind>) -> Option<DetailKind> {
    let spacing = ui.spacing().item_spacing.x;
    let columns = columns_for(ui.available_width());
    let width = (ui.available_width() - spacing * (columns as f32 - 1.0)) / columns as f32;
    let mut clicked = None;
    for row in figures.chunks(columns) {
        ui.horizontal(|ui| {
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

/// The API cost line or card under the figures. No key, nothing yet and errors are one dim line.
pub fn api_line(ui: &mut egui::Ui, view: &ApiCard) {
    match view {
        ApiCard::NoKey => {
            ui.label(RichText::new("Laut Anthropic: Kein Admin-Key gespeichert. Kosten-API nur mit Organisations-Konto.").color(MUTED));
        }
        ApiCard::Pending => {
            ui.label(RichText::new("Laut Anthropic: Warte auf die erste Abfrage …").color(MUTED));
        }
        ApiCard::Error(text) => {
            ui.label(RichText::new(format!("Laut Anthropic: {text}")).color(AMBER));
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
                    ui.label(RichText::new("API-KOSTEN LAUT ANTHROPIC").small().color(MUTED));
                    ui.label(RichText::new(format!("Heute {today}")).size(16.0).strong());
                    ui.label(format!("Dieser Monat {month}"));
                    ui.label(RichText::new(format!("Stand: {updated}")).small().color(MUTED));
                    ui.label(
                        RichText::new("Die Daten können mehrere Stunden hinterherhinken (laut Anthropic etwa 4 Std., bis zu 24 Std.).")
                            .small()
                            .color(MUTED),
                    );
                    if let Some(text) = error {
                        ui.label(RichText::new(text).color(AMBER));
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
        assert!(f[0].lines.iter().any(|(t, _)| t == COST_NOTE));
        assert!(!f[0].lines.iter().any(|(_, warn)| *warn), "today had every request priced");
        assert!(f[1].lines.iter().any(|(t, warn)| *warn && t == "2 Anfragen nicht bewertet"));
    }

    #[test]
    fn balance_states_stay_visible_on_the_card() {
        let none = figures(&report(), &BalanceCard::NoBaseline, false);
        assert_eq!(none[2].value, "–");
        assert!(none[2].lines[0].0.contains("Noch kein Startwert"));
        let no_deduction = figures(&report(), &estimate(false), true);
        assert!(no_deduction[2].lines.iter().any(|(t, w)| *w && t.contains("ohne Kostenabzug")));
        assert!(no_deduction[2].lines.iter().any(|(t, _)| t.contains("Hinweis")));
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
}
