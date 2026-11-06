//! The banner strip under the top bar: every warning and message of v1, coloured by severity.

use std::path::Path;

use eframe::egui::{self, Color32, CornerRadius, RichText, Stroke};

use super::theme::palette::{AMBER, BG, CYAN_DARK, PANEL, PANEL2, TEXT};
use super::{Notice, NoticeKind};
use crate::balance::Alert;
use crate::cards::alert_banner_text;
use crate::format::routing_warning;
use crate::view::Tone;

/// Everything that can raise a banner. Texts that need the clock (the pause) arrive already formatted.
pub struct BannerInput<'a> {
    pub quit_waiting: bool,
    pub routing_names: &'a [&'static str],
    pub worker_gone: bool,
    pub pause_text: Option<String>,
    pub scan_error: Option<&'a str>,
    pub settings_error: Option<&'a str>,
    pub settings_path: Option<&'a Path>,
    pub alerts: &'a [Alert],
    pub notice: Option<&'a Notice>,
}

/// Banners in v1 order. Severity as a tone: `Neutral` is information, `AmberOutline` a warning,
/// `Amber` (solid) an error or a stop. Green never appears here.
pub fn banner_items(input: &BannerInput) -> Vec<(Tone, String)> {
    let mut items = Vec::new();
    if input.quit_waiting {
        items.push((Tone::Neutral, "Der Start läuft noch. Die App beendet sich danach.".to_string()));
    }
    if let Some(text) = routing_warning(input.routing_names) {
        items.push((Tone::AmberOutline, text));
    }
    if input.worker_gone {
        items.push((
            Tone::Amber,
            "Der Hintergrunddienst ist ausgefallen. Die Zahlen werden nicht mehr aktualisiert. Bitte die App neu starten."
                .to_string(),
        ));
    }
    if let Some(text) = &input.pause_text {
        items.push((Tone::Amber, text.clone()));
    }
    if let Some(err) = input.scan_error {
        items.push((Tone::Amber, scan_error_text(err)));
    }
    if let Some(err) = input.settings_error {
        let place = input.settings_path.map(|p| format!(". Datei: {}", p.display())).unwrap_or_default();
        items.push((Tone::Amber, format!("{}{place}", err.trim_end_matches('.'))));
    }
    for alert in input.alerts {
        if let Some(text) = alert_banner_text(alert) {
            items.push((Tone::AmberOutline, text.to_string()));
        }
    }
    if let Some(notice) = input.notice {
        let tone = match notice.kind {
            NoticeKind::Info => Tone::Neutral,
            NoticeKind::Error => Tone::Amber,
        };
        items.push((tone, notice.text.clone()));
    }
    items
}

/// Banner text for the worker's error. Reading problems get the reading prefix; a failed save
/// already says what failed and must not be called a reading error.
fn scan_error_text(err: &str) -> String {
    if err.starts_with("Zustand konnte nicht gespeichert werden") {
        err.to_string()
    } else {
        format!("Fehler beim Lesen der Protokolle: {err}")
    }
}

/// Fill, stroke and text colour of a banner; the text has at least 4.5:1 against its fill.
fn banner_colors(tone: Tone) -> (Color32, Stroke, Color32) {
    match tone {
        Tone::Amber => (AMBER, Stroke::new(1.0, AMBER), BG),
        Tone::AmberOutline => (PANEL.lerp_to_gamma(AMBER, 0.12), Stroke::new(1.0, AMBER), TEXT),
        Tone::Neutral | Tone::Green => (PANEL2, Stroke::new(1.0, CYAN_DARK), TEXT),
    }
}

/// Index of the banner that gets a close button: the notice (always the last item), unless it
/// removes itself. Warnings and errors that come from the state of the app have no close button.
pub fn closable_index(item_count: usize, has_notice: bool, notice_expires: bool) -> Option<usize> {
    (has_notice && !notice_expires && item_count > 0).then(|| item_count - 1)
}

/// Banners one under the other, each in its severity colour. A tall stack scrolls inside the strip.
/// The banner at `closable` has a close button; returns true when it was clicked.
pub fn banner_strip(ui: &mut egui::Ui, items: &[(Tone, String)], closable: Option<usize>) -> bool {
    if items.is_empty() {
        return false;
    }
    let mut closed = false;
    egui::ScrollArea::vertical().id_salt("banner-strip").max_height(150.0).auto_shrink([false, true]).show(ui, |ui| {
        for (i, (tone, text)) in items.iter().enumerate() {
            let (fill, stroke, text_color) = banner_colors(*tone);
            egui::Frame::new()
                .fill(fill)
                .stroke(stroke)
                .corner_radius(CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(10, 7))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if closable == Some(i) {
                        egui::Sides::new().shrink_left().show(
                            ui,
                            |ui| {
                                ui.label(RichText::new(text).color(text_color));
                            },
                            |ui| {
                                let close = egui::Button::new(RichText::new("✕").size(13.0).color(text_color)).frame(false);
                                if ui.add(close).on_hover_text("Schließen").clicked() {
                                    closed = true;
                                }
                            },
                        );
                    } else {
                        ui.label(RichText::new(text).color(text_color));
                    }
                });
        }
    });
    closed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::contrast_ratio;
    use chrono::Utc;

    fn empty() -> BannerInput<'static> {
        BannerInput {
            quit_waiting: false,
            routing_names: &[],
            worker_gone: false,
            pause_text: None,
            scan_error: None,
            settings_error: None,
            settings_path: None,
            alerts: &[],
            notice: None,
        }
    }

    #[test]
    fn nothing_to_say_means_no_banner() {
        assert!(banner_items(&empty()).is_empty());
    }

    #[test]
    fn every_v1_banner_is_reachable_in_v1_order() {
        let notice = Notice { kind: NoticeKind::Info, text: "gestartet".into() };
        let alerts = [Alert::BudgetExceeded, Alert::LowBalance, Alert::MaxPaused(Utc::now())];
        let path = Path::new(r"C:\x\settings.json");
        let input = BannerInput {
            quit_waiting: true,
            routing_names: &["ANTHROPIC_BASE_URL"],
            worker_gone: true,
            pause_text: Some("Max pausiert bis 17:42 (5-Stunden-Limit)".into()),
            scan_error: Some("Ordner nicht gefunden"),
            settings_error: Some("Einstellungsdatei kaputt."),
            settings_path: Some(path),
            alerts: &alerts,
            notice: Some(&notice),
        };
        let texts: Vec<String> = banner_items(&input).into_iter().map(|(_, t)| t).collect();
        assert_eq!(texts.len(), 9, "{texts:#?}");
        assert!(texts[0].starts_with("Der Start läuft noch"));
        assert!(texts[1].contains("ANTHROPIC_BASE_URL"));
        assert!(texts[2].contains("Hintergrunddienst"));
        assert!(texts[3].starts_with("Max pausiert bis"));
        assert_eq!(texts[4], "Fehler beim Lesen der Protokolle: Ordner nicht gefunden");
        assert_eq!(texts[5], r"Einstellungsdatei kaputt. Datei: C:\x\settings.json");
        assert!(texts[6].starts_with("Monatsbudget erreicht"));
        assert!(texts[7].starts_with("Guthaben (geschätzt) unter"));
        assert_eq!(texts[8], "gestartet");
    }

    #[test]
    fn a_failed_save_is_not_called_a_reading_error() {
        let save = "Zustand konnte nicht gespeichert werden: Zugriff verweigert (os error 5)";
        let items = banner_items(&BannerInput { scan_error: Some(save), ..empty() });
        assert_eq!(items[0].1, save);
        let read = banner_items(&BannerInput { scan_error: Some("Ordner fehlt"), ..empty() });
        assert_eq!(read[0].1, "Fehler beim Lesen der Protokolle: Ordner fehlt");
    }

    #[test]
    fn only_a_lasting_notice_gets_a_close_button() {
        // The notice is the last item; a self-removing "gestartet" notice has no button.
        assert_eq!(closable_index(3, true, false), Some(2));
        assert_eq!(closable_index(3, true, true), None);
        assert_eq!(closable_index(3, false, false), None);
        assert_eq!(closable_index(0, true, false), None);
    }

    #[test]
    fn severity_maps_to_tone_and_green_never_appears() {
        let info = Notice { kind: NoticeKind::Info, text: "i".into() };
        let err = Notice { kind: NoticeKind::Error, text: "e".into() };
        let tone_of = |n: &Notice| banner_items(&BannerInput { notice: Some(n), ..empty() })[0].0;
        assert_eq!(tone_of(&info), Tone::Neutral);
        assert_eq!(tone_of(&err), Tone::Amber);
        let alerts = [Alert::LowBalance];
        let warn = banner_items(&BannerInput { alerts: &alerts, routing_names: &["X"], ..empty() });
        assert!(warn.iter().all(|(t, _)| *t == Tone::AmberOutline));
        let all = banner_items(&BannerInput { worker_gone: true, quit_waiting: true, ..empty() });
        assert!(all.iter().all(|(t, _)| *t != Tone::Green));
    }

    #[test]
    fn a_max_pause_alert_has_no_second_banner() {
        let alerts = [Alert::MaxPaused(Utc::now())];
        assert!(banner_items(&BannerInput { alerts: &alerts, ..empty() }).is_empty());
    }

    #[test]
    fn banner_text_has_aa_contrast_on_every_fill() {
        for tone in [Tone::Amber, Tone::AmberOutline, Tone::Neutral] {
            let (fill, _, text) = banner_colors(tone);
            let r = contrast_ratio(text, fill);
            assert!(r >= 4.5, "{tone:?}: {r}");
        }
    }
}
