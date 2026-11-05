//! The areas that open under the figure cards: 30-day chart plus bar lists, balance explanation,
//! Max status. Pure row/tab decisions are separate from the drawing so they can be tested.

use std::borrow::Cow;

use chrono::{Local, NaiveDate};
use eframe::egui::{self, Color32, CornerRadius, FontId, RichText, Sense, Shadow, Stroke, vec2};
use eframe::egui::text::{LayoutJob, TextWrapping};

use super::chart::day_chart;
use super::theme::palette::{AMBER, BG, CYAN, CYAN_DARK, LINE, MUTED, PANEL, PANEL2, TEXT};
use super::cost_cell_text;
use crate::analytics::ApiError;
use crate::cards::{BalanceCard, api_error_text};
use crate::format::{COST_NOTE, format_count, pause_text, unpriced_text};
use crate::pricing::format_usd;
use crate::report::{Bucket, Report};
use crate::view::{DetailKind, bar_fraction, day_bars, truncate_name};

/// Longest name shown in a bar row; the full name is in the tooltip.
const NAME_CHARS: usize = 28;
/// Rows shown before "weitere anzeigen".
const MAX_ROWS: usize = 8;
const DAYS: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailTab {
    Projects,
    Models,
    Days,
}

/// Tabs of a detail area; the balance and Max areas have none.
pub fn tabs_for(kind: DetailKind) -> &'static [DetailTab] {
    match kind {
        DetailKind::Today => &[DetailTab::Projects, DetailTab::Models],
        DetailKind::Month => &[DetailTab::Projects, DetailTab::Models, DetailTab::Days],
        DetailKind::Balance | DetailKind::Max => &[],
    }
}

fn tab_label(tab: DetailTab) -> &'static str {
    match tab {
        DetailTab::Projects => "Projekte",
        DetailTab::Models => "Modelle",
        DetailTab::Days => "Tage",
    }
}

/// The tab to show: the wanted one if this detail has it, else the first.
pub fn effective_tab(kind: DetailKind, wanted: DetailTab) -> DetailTab {
    let tabs = tabs_for(kind);
    if tabs.contains(&wanted) { wanted } else { tabs.first().copied().unwrap_or(DetailTab::Projects) }
}

/// (rows shown, rows hidden) for a list of `len` rows.
pub fn visible_rows(len: usize, expanded: bool) -> (usize, usize) {
    if expanded || len <= MAX_ROWS { (len, 0) } else { (MAX_ROWS, len - MAX_ROWS) }
}

/// Headline number of a bucket. A bucket where no request had a price shows a dash, never "$0.00".
pub fn figure_value(b: &Bucket) -> String {
    if b.cost_nano == 0 && b.requests > 0 && b.unpriced_requests >= b.requests {
        "–".to_string()
    } else {
        format_usd(b.cost_nano)
    }
}

pub fn token_summary(b: &Bucket) -> String {
    format!(
        "{} Ein · {} Cache · {} Aus",
        format_count(b.input),
        format_count(b.cache_read.saturating_add(b.cache_write)),
        format_count(b.output)
    )
}

/// What the list of a Today/Month detail shows, with its title and the text for "nothing yet".
pub fn rows_for<'a>(kind: DetailKind, tab: DetailTab, report: &'a Report) -> (&'static str, &'static str, Vec<(Cow<'a, str>, &'a Bucket)>) {
    let named = |rows: &'a [(String, Bucket)]| rows.iter().map(|(n, b)| (Cow::Borrowed(n.as_str()), b)).collect::<Vec<_>>();
    match (kind, effective_tab(kind, tab)) {
        (DetailKind::Today, DetailTab::Models) => ("Heute nach Modell", "Noch keine Anfragen heute.", named(&report.today_by_model)),
        (DetailKind::Today, _) => ("Heute nach Projekt", "Noch keine Anfragen heute.", named(&report.today_by_project)),
        (_, DetailTab::Models) => ("Diesen Monat nach Modell", "Noch keine Anfragen in diesem Monat.", named(&report.by_model)),
        (_, DetailTab::Days) => (
            "Letzte Tage",
            "Noch keine Anfragen in den letzten Tagen.",
            report.by_day.iter().map(|(d, b)| (Cow::Owned(day_label(*d)), b)).collect(),
        ),
        (_, DetailTab::Projects) => ("Diesen Monat nach Projekt", "Noch keine Anfragen in diesem Monat.", named(&report.by_project)),
    }
}

fn day_label(date: NaiveDate) -> String {
    date.format("%d.%m.%Y").to_string()
}

/// Everything a detail area reads. The balance texts come from `cards`, not from here.
pub struct DetailData<'a> {
    pub report: &'a Report,
    pub balance: &'a BalanceCard,
    pub balance_error: Option<&'a ApiError>,
}

/// What the user did inside a detail area this frame.
#[derive(Debug, Default, Clone, Copy)]
pub struct DetailOutput {
    pub open_drawer: bool,
    /// The content got taller (tab switch, more rows): the window may need to grow again.
    pub layout_changed: bool,
}

/// UI state that survives frames and belongs to the area, not to the data.
#[derive(Debug, Clone, Copy)]
pub struct DetailView {
    pub tab: DetailTab,
    pub rows_expanded: bool,
}

impl Default for DetailView {
    fn default() -> Self {
        Self { tab: DetailTab::Projects, rows_expanded: false }
    }
}

/// Runs `body` clipped to `openness` (0..=1) of its measured height. At 1 it is not clipped at all.
/// The measured height is stored under `id` (first frame of an opening: nothing visible yet, the next
/// frame is already animating).
pub fn animated_area<R>(ui: &mut egui::Ui, id: egui::Id, openness: f32, body: impl FnOnce(&mut egui::Ui) -> R) -> Option<R> {
    if openness <= 0.0 {
        return None;
    }
    let known: f32 = ui.data(|d| d.get_temp(id)).unwrap_or(0.0);
    let top = ui.available_rect_before_wrap().min;
    let width = ui.available_width();
    let mut child = ui.new_child(
        egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(top, vec2(width, f32::MAX / 4.0))),
    );
    let visible = (openness < 1.0).then_some(known * openness);
    if let Some(h) = visible {
        child.set_clip_rect(ui.clip_rect().intersect(egui::Rect::from_min_size(top, vec2(width, h))));
    }
    let result = body(&mut child);
    let used = child.min_rect().height();
    ui.data_mut(|d| d.insert_temp(id, used));
    let advance = visible.unwrap_or(used);
    ui.allocate_rect(egui::Rect::from_min_size(top, vec2(width, advance)), Sense::hover());
    Some(result)
}

/// Rounded panel of a detail area.
fn detail_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, CYAN_DARK))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(12))
        .shadow(Shadow { offset: [0, 0], blur: 12, spread: 0, color: CYAN.gamma_multiply(0.12) })
}

/// Segmented switch; `true` when the selection changed.
fn segmented(ui: &mut egui::Ui, current: &mut DetailTab, tabs: &[DetailTab]) -> bool {
    let mut changed = false;
    egui::Frame::new()
        .fill(BG)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(2))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                for &tab in tabs.iter().rev() {
                    let on = *current == tab;
                    let text = RichText::new(tab_label(tab)).color(if on { TEXT } else { MUTED });
                    let button = egui::Button::new(text)
                        .fill(if on { PANEL2 } else { Color32::TRANSPARENT })
                        .stroke(Stroke::NONE)
                        .corner_radius(CornerRadius::same(6));
                    if ui.add(button).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on {
                        *current = tab;
                        changed = true;
                    }
                }
            });
        });
    changed
}

/// Name and cost on one line, a thin bar below. Returns `true` when "weitere anzeigen" or
/// "Weniger anzeigen" was clicked.
fn bar_rows(ui: &mut egui::Ui, rows: &[(Cow<str>, &Bucket)], empty_text: &str, expanded: bool) -> bool {
    if rows.is_empty() {
        ui.label(RichText::new(empty_text).color(MUTED));
        return false;
    }
    let max = rows.iter().map(|(_, b)| b.cost_nano).max().unwrap_or(0);
    let (shown, hidden) = visible_rows(rows.len(), expanded);
    let font = FontId::proportional(13.0);
    ui.spacing_mut().item_spacing.y = 2.0;
    for (name, b) in &rows[..shown] {
        let width = ui.available_width();
        let (rect, response) = ui.allocate_exact_size(vec2(width, 32.0), Sense::hover());
        let painter = ui.painter_at(rect);
        let value = painter.layout_no_wrap(cost_cell_text(b), font.clone(), TEXT);
        let (short, _) = truncate_name(name, NAME_CHARS);
        let mut job = LayoutJob::simple_singleline(short, font.clone(), TEXT);
        job.wrap = TextWrapping::truncate_at_width((width - value.size().x - 10.0).max(40.0));
        let label = painter.layout_job(job);
        painter.galley(rect.left_top() + vec2(0.0, 2.0), label, TEXT);
        painter.galley(egui::pos2(rect.right() - value.size().x, rect.top() + 2.0), value, TEXT);
        let track = egui::Rect::from_min_size(egui::pos2(rect.left(), rect.bottom() - 8.0), vec2(width, 5.0));
        painter.rect_filled(track, CornerRadius::same(3), PANEL2);
        let fill_w = (width * bar_fraction(b.cost_nano, max)).max(if b.cost_nano > 0 { 2.0 } else { 0.0 });
        painter.rect_filled(egui::Rect::from_min_size(track.min, vec2(fill_w, 5.0)), CornerRadius::same(3), CYAN_DARK);
        response.on_hover_text(format!("{name}\n{} Anfragen · {}", format_count(b.requests), token_summary(b)));
    }
    if hidden > 0 {
        return ui.link(format!("+ {hidden} weitere anzeigen")).clicked();
    }
    expanded && rows.len() > MAX_ROWS && ui.link("Weniger anzeigen").clicked()
}

fn explanation(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).small().color(MUTED));
}

fn warn_line(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).color(AMBER));
}

fn figure_lines(ui: &mut egui::Ui, b: &Bucket) {
    ui.label(format!("{} Anfragen", format_count(b.requests)));
    ui.label(RichText::new(token_summary(b)).small().color(MUTED));
    if let Some(text) = unpriced_text(b.unpriced_requests) {
        warn_line(ui, &text);
    }
}

fn history(ui: &mut egui::Ui, kind: DetailKind, data: &DetailData, view: &mut DetailView, out: &mut DetailOutput) {
    let tabs = tabs_for(kind);
    view.tab = effective_tab(kind, view.tab);
    ui.horizontal(|ui| {
        ui.label(RichText::new("LETZTE 30 TAGE · GESCHÄTZT").small().color(MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if segmented(ui, &mut view.tab, tabs) {
                view.rows_expanded = false;
                out.layout_changed = true;
            }
        });
    });
    let today = Local::now().date_naive();
    day_chart(ui, &day_bars(&data.report.by_day, today, DAYS));
    explanation(ui, "Aus den Claude-Protokollen auf diesem Rechner, nach Listenpreis berechnet. Das ist nicht die Rechnung.");
    ui.add_space(2.0);
    let (title, empty, rows) = rows_for(kind, view.tab, data.report);
    ui.label(RichText::new(title).strong());
    if bar_rows(ui, &rows, empty, view.rows_expanded) {
        view.rows_expanded = !view.rows_expanded;
        // Only taller content may need the window to grow.
        out.layout_changed = view.rows_expanded;
    }
    ui.label(RichText::new(format!("Kosten sind {COST_NOTE}")).small().color(MUTED));
}

fn balance(ui: &mut egui::Ui, data: &DetailData, out: &mut DetailOutput) {
    match data.balance {
        BalanceCard::NoBaseline => {
            ui.label("Noch kein Startwert. Trage unter Einstellungen den aktuellen Betrag aus der Console ein.");
        }
        BalanceCard::Estimate { deducted, details, note, .. } => {
            ui.label(if *deducted {
                "Geschätzt aus dem Startwert, den Aufladungen und den API-Kosten seit dem Startwert."
            } else {
                "Geschätzt aus dem Startwert und den Aufladungen, ohne Kostenabzug."
            });
            for line in details {
                ui.label(RichText::new(line).color(MUTED));
            }
            if let Some(note) = note {
                warn_line(ui, note);
            }
            if let Some(e) = data.balance_error {
                warn_line(ui, &api_error_text(e));
            }
        }
    }
    if ui.link("In den Einstellungen ändern").clicked() {
        out.open_drawer = true;
    }
}

fn max_status(ui: &mut egui::Ui, data: &DetailData) {
    let report = data.report;
    ui.label(RichText::new("LETZTE 5 STUNDEN · GESCHÄTZT").small().color(MUTED));
    ui.label(RichText::new(figure_value(&report.last_5h)).text_style(egui::TextStyle::Name(super::theme::BIG.into())).strong());
    figure_lines(ui, &report.last_5h);
    ui.add_space(2.0);
    match &report.pause {
        Some(pause) => {
            warn_line(ui, &pause_text(pause.until.with_timezone(&Local), Local::now(), &pause.limit_type));
        }
        None => {
            ui.label(RichText::new("Keine Pause in den Protokollen.").color(MUTED));
        }
    }
}

/// Draws the area for `kind`. Call inside `animated_area`.
pub fn show(ui: &mut egui::Ui, kind: DetailKind, data: &DetailData, view: &mut DetailView) -> DetailOutput {
    let mut out = DetailOutput::default();
    detail_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.style_mut().interaction.selectable_labels = false;
        match kind {
            DetailKind::Today | DetailKind::Month => history(ui, kind, data, view, &mut out),
            DetailKind::Balance => balance(ui, data, &mut out),
            DetailKind::Max => max_status(ui, data),
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(cost_nano: u64, requests: u64, unpriced_requests: u64) -> Bucket {
        Bucket { cost_nano, requests, unpriced_requests, ..Bucket::default() }
    }

    #[test]
    fn today_has_two_tabs_month_three_others_none() {
        assert_eq!(tabs_for(DetailKind::Today), &[DetailTab::Projects, DetailTab::Models]);
        assert_eq!(tabs_for(DetailKind::Month), &[DetailTab::Projects, DetailTab::Models, DetailTab::Days]);
        assert!(tabs_for(DetailKind::Balance).is_empty() && tabs_for(DetailKind::Max).is_empty());
    }

    #[test]
    fn a_tab_a_detail_does_not_have_falls_back_to_the_first() {
        assert_eq!(effective_tab(DetailKind::Today, DetailTab::Days), DetailTab::Projects);
        assert_eq!(effective_tab(DetailKind::Month, DetailTab::Days), DetailTab::Days);
        assert_eq!(effective_tab(DetailKind::Today, DetailTab::Models), DetailTab::Models);
    }

    #[test]
    fn long_lists_are_cut_until_expanded() {
        assert_eq!(visible_rows(3, false), (3, 0));
        assert_eq!(visible_rows(8, false), (8, 0));
        assert_eq!(visible_rows(17, false), (8, 9));
        assert_eq!(visible_rows(17, true), (17, 0));
        assert_eq!(visible_rows(0, false), (0, 0));
    }

    #[test]
    fn fully_unpriced_figures_show_a_dash_not_dollar_zero() {
        assert_eq!(figure_value(&bucket(0, 3, 3)), "–");
        assert_eq!(figure_value(&bucket(0, 0, 0)), "$0.00");
        assert_eq!(figure_value(&bucket(1_230_000_000, 5, 2)), "$1.23");
        assert_eq!(figure_value(&bucket(0, 5, 0)), "$0.00");
    }

    #[test]
    fn lists_follow_card_and_tab() {
        let b = |n| bucket(n, 1, 0);
        let d = |day| NaiveDate::from_ymd_opt(2026, 11, day).unwrap();
        let report = Report {
            by_project: vec![("p-month".into(), b(5))],
            by_model: vec![("m-month".into(), b(4))],
            today_by_project: vec![("p-today".into(), b(3))],
            today_by_model: vec![("m-today".into(), b(2))],
            by_day: vec![(d(5), b(9)), (d(4), b(8))],
            ..Report::default()
        };
        let first = |kind, tab| rows_for(kind, tab, &report).2[0].0.to_string();
        assert_eq!(first(DetailKind::Today, DetailTab::Projects), "p-today");
        assert_eq!(first(DetailKind::Today, DetailTab::Models), "m-today");
        assert_eq!(first(DetailKind::Today, DetailTab::Days), "p-today", "no Tage tab for today");
        assert_eq!(first(DetailKind::Month, DetailTab::Projects), "p-month");
        assert_eq!(first(DetailKind::Month, DetailTab::Models), "m-month");
        assert_eq!(first(DetailKind::Month, DetailTab::Days), "05.11.2026");
        assert_eq!(rows_for(DetailKind::Month, DetailTab::Days, &report).2.len(), 2);
    }

    #[test]
    fn token_summary_counts_cache_together() {
        let b = Bucket { input: 1000, cache_read: 2000, cache_write: 500, output: 7, ..Bucket::default() };
        assert_eq!(token_summary(&b), "1.000 Ein · 2.500 Cache · 7 Aus");
    }

    /// Draws every detail, the figure cards and the banners through a few frames, so a layout panic shows
    /// up in the tests (no assertions on pixels, and egui does not panic on an id clash).
    #[test]
    fn every_detail_and_the_cockpit_pieces_draw_without_panicking() {
        use crate::report::Pause;
        let b = |n: u64| Bucket { requests: 3, cost_nano: n, input: 10, ..Bucket::default() };
        let today = Local::now().date_naive();
        let report = Report {
            today: b(5),
            month: b(50),
            last_5h: b(1),
            by_project: (0..12).map(|i| (format!("project-{i}-with-a-rather-long-name-over-28-chars"), b(i + 1))).collect(),
            by_model: vec![("claude-opus".into(), b(9))],
            today_by_project: vec![("p".into(), b(2))],
            today_by_model: vec![],
            by_day: vec![(today, b(5)), (today - chrono::Duration::days(1), Bucket { requests: 2, unpriced_requests: 2, ..Bucket::default() })],
            pause: Some(Pause { until: chrono::Utc::now(), limit_type: "seven_day".into() }),
        };
        let balance = BalanceCard::Estimate { value: "$1.00".into(), deducted: false, details: vec!["Startwert".into()], note: Some("n".into()) };
        let data = DetailData { report: &report, balance: &balance, balance_error: Some(&ApiError::NotAvailable) };
        let ctx = egui::Context::default();
        crate::ui::theme::apply(&ctx);
        let logo = ctx.load_texture("t", egui::ColorImage::filled([4, 4], Color32::WHITE), Default::default());
        let mut view = DetailView::default();
        let mut drawer = false;
        for width in [420.0, 700.0] {
            for kind in [DetailKind::Today, DetailKind::Month, DetailKind::Balance, DetailKind::Max] {
                for tab in [DetailTab::Projects, DetailTab::Models, DetailTab::Days] {
                    view.tab = tab;
                    for openness in [0.5, 1.0] {
                        let input = egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(width, 800.0))),
                            ..Default::default()
                        };
                        let mut output = ctx.run_ui(input, |ui| {
                            crate::ui::cockpit::top_bar(ui, &logo, &mut drawer);
                            crate::ui::banners::banner_strip(ui, &[(crate::view::Tone::Amber, "x".into())]);
                            let card = crate::view::mode_card(crate::view::ModeDisplay::Max);
                            crate::ui::cockpit::mode_card_ui(ui, card, Some((crate::launcher::Mode::Max, false)), None);
                            let figs = crate::ui::cockpit::figures(&report, &balance, true);
                            crate::ui::cockpit::figure_cards(ui, &figs, Some(kind));
                            animated_area(ui, egui::Id::unique("t-area"), openness, |ui| show(ui, kind, &data, &mut view));
                            crate::ui::cockpit::scan_progress(ui, 1, 3);
                            crate::ui::cockpit::api_line(ui, &crate::cards::ApiCard::NoKey);
                        });
                        output.textures_delta.clear();
                    }
                }
            }
        }
    }
}
