//! The 30-day cost bars, drawn with the egui painter (no chart library).

use eframe::egui::{self, CornerRadius, Rect, Sense, Shape, Stroke, pos2, vec2};

use super::theme::palette::{AMBER, CYAN, LINE, MUTED};
use crate::format::format_date_by;
use crate::i18n::{tr, tr_args};
use crate::pricing::format_usd;
use crate::view::{DayBar, bar_fraction};

const HEIGHT: f32 = 80.0;
const GAP: f32 = 3.0;
/// Height of the dashed outline for a day with only unpriced requests, as a fraction of the plot.
const UNPRICED_FRACTION: f32 = 0.3;

/// Hover text of one bar: the date and the estimated cost, or the reason there is no cost.
pub fn bar_tooltip(bar: &DayBar) -> String {
    let date = format_date_by(bar.date, "format.date", "%Y-%m-%d");
    if bar.unpriced_only {
        tr_args("chart.unpriced_day", &[("date", &date)])
    } else {
        format!("{date} · {}", format_usd(bar.cost_nano))
    }
}

/// One column per day, oldest left. Today is amber, a day with only unpriced requests a dashed outline.
pub fn day_chart(ui: &mut egui::Ui, bars: &[DayBar]) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, HEIGHT), Sense::hover());
    let count = bars.len().max(1) as f32;
    let bar_w = ((width - GAP * (count - 1.0)) / count).max(1.0);
    let max = bars.iter().map(|b| b.cost_nano).max().unwrap_or(0);
    let painter = ui.painter_at(rect);
    painter.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, LINE));
    let plot = HEIGHT - 2.0;
    let top_round = CornerRadius { nw: 3, ne: 3, sw: 0, se: 0 };
    let base = egui::Id::unique("day-chart");

    for (i, bar) in bars.iter().enumerate() {
        let x = rect.left() + i as f32 * (bar_w + GAP);
        let column = Rect::from_min_size(pos2(x, rect.top()), vec2(bar_w, HEIGHT));
        let response = ui.interact(column, base.with(i), Sense::hover());
        let tint = if bar.is_today { AMBER } else { CYAN };
        let color = if response.hovered() { tint } else { tint.gamma_multiply(0.8) };
        if bar.unpriced_only {
            let h = plot * UNPRICED_FRACTION;
            let outline = Rect::from_min_max(pos2(x, rect.bottom() - 1.0 - h), pos2(x + bar_w, rect.bottom() - 1.0));
            let corners = [outline.left_top(), outline.right_top(), outline.right_bottom(), outline.left_bottom(), outline.left_top()];
            painter.extend(Shape::dashed_line(&corners, Stroke::new(1.0, if bar.is_today { AMBER } else { MUTED }), 3.0, 2.0));
        } else if bar.cost_nano > 0 {
            let h = (plot * bar_fraction(bar.cost_nano, max)).max(2.0);
            let r = Rect::from_min_max(pos2(x, rect.bottom() - 1.0 - h), pos2(x + bar_w, rect.bottom() - 1.0));
            painter.rect_filled(r, top_round, color);
        } else {
            let r = Rect::from_min_max(pos2(x, rect.bottom() - 3.0), pos2(x + bar_w, rect.bottom() - 1.0));
            painter.rect_filled(r, 0, LINE);
        }
        response.on_hover_text(bar_tooltip(bar));
    }

    ui.horizontal(|ui| {
        if let Some(first) = bars.first() {
            let short = format_date_by(first.date, "format.date_short", "%m-%d");
            ui.label(egui::RichText::new(short).small().color(MUTED));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(tr("chart.today")).small().color(MUTED));
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn bar(cost_nano: u64, unpriced_only: bool) -> DayBar {
        DayBar { date: NaiveDate::from_ymd_opt(2026, 11, 5).unwrap(), cost_nano, unpriced_only, is_today: false }
    }

    #[test]
    fn tooltip_has_date_and_cost() {
        assert_eq!(bar_tooltip(&bar(25_970_000_000, false)), "05.11.2026 · $25.97");
        assert_eq!(bar_tooltip(&bar(0, false)), "05.11.2026 · $0.00");
    }

    #[test]
    fn tooltip_of_an_unpriced_day_has_no_dollar_zero() {
        let t = bar_tooltip(&bar(0, true));
        assert_eq!(t, "05.11.2026 · nicht bewertet");
        assert!(!t.contains('$'));
    }
}
