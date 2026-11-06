//! The settings drawer: slides in from the right over the cockpit, one column of sections.
//!
//! Behaviour is the v1 settings code (re-read and broken-file protection through `update_settings`, key
//! handling with the undo-history wipe, explicit key and path buttons); only the presentation changed.
//! Text fields commit on Enter or blur, and only when edited and changed (`should_commit`), so
//! half-typed input never reaches `settings.json`.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use chrono::{Local, Utc};
use eframe::egui::{
    self, Align, Layout, Margin, Order, RichText, Sense, Shadow, Stroke, vec2,
};

use super::theme::palette::{AMBER, BG, CYAN, MUTED, TEXT};
use super::{CerberusApp, Notice, NoticeKind, amount_input, key_field_id, key_label, key_status_line, wipe_key_field};
use crate::cards::{
    inert_limits_hint, should_commit, update_settings, with_baseline, with_chosen_language, with_limits, with_topup,
    without_topup,
};
use crate::paths_ui::{PathsAction, RowNote};
use crate::pricing::format_usd;
use crate::secrets::Slot;
use crate::settings::{Settings, TopUp};
use crate::view::drawer_width;

/// Fill of the drawer panel (a shade lighter than the cockpit behind the backdrop).
const DRAWER_FILL: egui::Color32 = egui::Color32::from_rgb(0x0C, 0x18, 0x29);
/// Border between cockpit and drawer.
const DRAWER_EDGE: egui::Color32 = egui::Color32::from_rgb(0x1F, 0x5A, 0x72);
const SLIDE_SECONDS: f32 = 0.18;
const PAD: i8 = 14;

/// Where the drawer can be opened to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Balance,
}

/// Which field a result message belongs to. Messages are drawn directly under that field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoteKey {
    ApiKey,
    AdminKey,
    Baseline,
    TopUp,
    TopUps,
    Budget,
    LowBalance,
    Obsidian,
    Logs,
    Updates,
    Language,
    Plaintext,
}

pub type Notes = HashMap<NoteKey, Notice>;

fn slot_note(slot: Slot) -> NoteKey {
    match slot {
        Slot::ApiKey => NoteKey::ApiKey,
        Slot::AdminKey => NoteKey::AdminKey,
    }
}

pub(super) enum SettingsAction {
    Baseline,
    TopUp,
    Remove(TopUp),
    Budget,
    LowBalance,
}

impl SettingsAction {
    fn note_key(&self) -> NoteKey {
        match self {
            Self::Baseline => NoteKey::Baseline,
            Self::TopUp => NoteKey::TopUp,
            Self::Remove(_) => NoteKey::TopUps,
            Self::Budget => NoteKey::Budget,
            Self::LowBalance => NoteKey::LowBalance,
        }
    }
}

/// "Einstellungen: <path> · Keys: in dieser Datei, …". Never contains a key; says why when there is no path.
pub fn footer_text(settings_path: Option<&Path>, keys_plaintext: bool) -> String {
    let place = match settings_path {
        Some(p) => p.display().to_string(),
        None => "nicht verfügbar (LOCALAPPDATA ist nicht gesetzt)".to_string(),
    };
    let form = if keys_plaintext { "im Klartext" } else { "verschlüsselt" };
    format!("Einstellungen: {place} · Keys: in dieser Datei, {form}")
}

/// Warning shown before keys are written as plain text. Says what changes and what the default protects against.
pub const PLAINTEXT_WARNING: &str = "Die Keys stehen danach lesbar in der Einstellungsdatei, auch in Backups und Kopien. Wer die Datei bekommt, kann die Keys benutzen. Dafür lässt sich die Datei auf einen anderen PC mitnehmen. Beide gespeicherten Keys werden sofort umgewandelt.";

/// Under the switch: the default and its honest limit.
pub const PLAINTEXT_HINT: &str = "Standard ist verschlüsselt (Windows, nur dieser Benutzer auf diesem PC). Das schützt davor, dass die Datei mitgenommen oder versehentlich geteilt wird. Programme, die unter deinem Windows-Benutzer laufen, können die Keys trotzdem lesen. Für einen Umzug auf einen anderen PC: Klartext einschalten, Datei kopieren, dort wieder ausschalten.";

/// Applies a language choice: the language changes at once for this session; saving it into the
/// settings file is a second step that may fail (broken file, no path). On failure the text says that
/// the choice only holds until the app is closed.
pub fn choose_language(settings_path: Option<&Path>, code: &str) -> Result<Settings, String> {
    crate::i18n::set_language(Some(code));
    let Some(path) = settings_path else {
        return Err("Die Sprache gilt nur bis zum Beenden der App. Einstellungen sind nicht verfügbar.".to_string());
    };
    update_settings(path, |s| Ok(with_chosen_language(s, code)))
        .map_err(|e| format!("Die Sprache gilt nur bis zum Beenden der App, gespeichert wurde sie nicht. {e}"))
}

/// The command that opens a folder in Explorer. The folder is one plain argument, never a shell string.
pub fn explorer_command(dir: &Path) -> Command {
    let mut cmd = Command::new("explorer.exe");
    cmd.arg(dir);
    cmd
}

/// Creates the folder if it is missing and returns the command that opens it.
pub fn open_folder_command(dir: &Path) -> Result<Command, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Der Sprachordner lässt sich nicht anlegen ({}).", e.kind()))?;
    Ok(explorer_command(dir))
}

fn open_lang_folder() -> Result<(), String> {
    let dir = crate::i18n::app_lang_dir().ok_or_else(|| "Der Sprachordner ist nicht bekannt.".to_string())?;
    open_folder_command(&dir)?
        .spawn()
        .map(drop)
        .map_err(|e| format!("Der Explorer lässt sich nicht starten ({}).", e.kind()))
}

/// Cubic ease-out of the slide.
fn ease(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

fn section_title(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add_space(8.0);
    ui.add(egui::Label::new(RichText::new(text.to_uppercase()).size(11.0).strong().color(CYAN)))
}

fn hint(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(12.0).color(MUTED)));
}

fn warn(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).color(AMBER)));
}

/// The result message of one field, directly under it. Errors amber, confirmations cyan (green is
/// reserved for "Max, started by the app").
fn field_note(ui: &mut egui::Ui, note: Option<&Notice>) {
    if let Some(note) = note {
        let color = if note.kind == NoticeKind::Error { AMBER } else { CYAN };
        ui.add(egui::Label::new(RichText::new(&note.text).color(color)));
    }
}

/// A one-line amount field that commits on Enter or blur, only when edited and changed. Esc reverts it.
/// Returns true when the caller should save now.
fn amount_field(ui: &mut egui::Ui, input: &mut String, edited: &mut bool, current: &str, hint_text: &str) -> bool {
    let r = ui.add(egui::TextEdit::singleline(input).hint_text(hint_text).desired_width(120.0));
    if r.changed() {
        *edited = true;
    }
    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        *input = current.to_string();
        *edited = false;
        return false;
    }
    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    should_commit(*edited, r.lost_focus(), enter, input, current)
}

impl CerberusApp {
    /// Opens the drawer, optionally scrolled to a section, and makes sure a frame is drawn for it.
    pub(super) fn open_drawer(&mut self, ctx: &egui::Context, at: Option<Section>) {
        self.drawer_open = true;
        if at.is_some() {
            self.drawer_scroll = at;
        }
        ctx.request_repaint();
    }

    fn note(&self, key: NoteKey) -> Option<&Notice> {
        self.notes.get(&key)
    }

    fn set_note(&mut self, key: NoteKey, kind: NoticeKind, text: impl Into<String>) {
        self.notes.insert(key, Notice { kind, text: text.into() });
    }

    // ---- state changes (unchanged behaviour) -----------------------------------------------------

    fn save_key(&mut self, ctx: &egui::Context, slot: Slot) {
        // Take the text out of the field while saving; on failure it goes back so the user can fix it.
        let input = std::mem::take(self.input_mut(slot));
        match super::save_key(&*self.store, slot, &input) {
            Ok(()) => {
                wipe_key_field(ctx, key_field_id(slot), self.input_mut(slot));
                self.set_stored(slot, true);
                self.set_note(slot_note(slot), NoticeKind::Info, format!("{} gespeichert.", key_label(slot)));
            }
            Err(msg) => {
                *self.input_mut(slot) = input;
                self.set_note(slot_note(slot), NoticeKind::Error, msg);
            }
        }
    }

    fn delete_key(&mut self, ctx: &egui::Context, slot: Slot) {
        wipe_key_field(ctx, key_field_id(slot), self.input_mut(slot));
        match self.store.delete(slot) {
            Ok(()) => {
                self.set_stored(slot, false);
                self.set_note(slot_note(slot), NoticeKind::Info, format!("{} gelöscht.", key_label(slot)));
            }
            Err(e) => self.set_note(slot_note(slot), NoticeKind::Error, e.to_string()),
        }
    }

    pub(super) fn apply_paths_action(&mut self, ctx: &egui::Context, action: PathsAction) {
        let key = match action {
            PathsAction::Browse(f) | PathsAction::Save(f) | PathsAction::Reset(f) => match f {
                crate::paths_ui::Field::Obsidian => NoteKey::Obsidian,
                crate::paths_ui::Field::Logs => NoteKey::Logs,
            },
        };
        if let PathsAction::Browse(field) = action {
            self.paths.browse(field, ctx);
            return;
        }
        let Some(path) = self.state.settings_path.clone() else {
            self.set_note(key, NoticeKind::Error, "Einstellungen sind nicht verfügbar.");
            return;
        };
        match self.paths.apply(action, &path) {
            Ok(settings) => {
                self.state.settings = settings;
                let text = match key {
                    NoteKey::Obsidian => "Gespeichert. Der Obsidian-Pfad gilt ab dem nächsten Start.",
                    _ => "Gespeichert. Der Protokollordner gilt nach einem Neustart der App.",
                };
                self.set_note(key, NoticeKind::Info, text);
            }
            Err(text) => self.set_note(key, NoticeKind::Error, text),
        }
    }

    /// Reads the language folder again. Only when the drawer opens, never per frame.
    fn rescan_languages(&mut self) {
        self.lang_problems = crate::i18n::app_lang_dir().as_deref().map(crate::i18n::scan_folder).unwrap_or_default();
        self.languages = crate::i18n::available();
    }

    fn apply_language(&mut self, code: &str) {
        match choose_language(self.state.settings_path.as_deref(), code) {
            Ok(settings) => {
                self.state.settings = settings;
                self.notes.remove(&NoteKey::Language);
            }
            Err(text) => self.set_note(NoteKey::Language, NoticeKind::Error, text),
        }
    }

    /// Switches how keys are stored, at once, in both directions. On error nothing changed.
    fn apply_plaintext(&mut self, on: bool) {
        let Some(path) = self.state.settings_path.clone() else {
            self.set_note(NoteKey::Plaintext, NoticeKind::Error, "Einstellungen sind nicht verfügbar.");
            return;
        };
        match crate::config_store::set_plaintext(&path, on) {
            Ok(settings) => {
                self.state.settings = settings;
                let text = if on {
                    "Die Keys stehen jetzt im Klartext in der Einstellungsdatei."
                } else {
                    "Die Keys sind jetzt verschlüsselt gespeichert."
                };
                self.set_note(NoteKey::Plaintext, NoticeKind::Info, text);
            }
            Err(text) => self.set_note(NoteKey::Plaintext, NoticeKind::Error, text),
        }
    }

    /// The warning before plain text is switched on, in the style of the update confirmation.
    pub(super) fn draw_plaintext_confirm(&mut self, ctx: &egui::Context) {
        if !self.plaintext_confirm_open {
            return;
        }
        let mut answer = None;
        egui::Modal::new(egui::Id::unique("confirm-plaintext")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("Keys im Klartext speichern?");
            ui.label(PLAINTEXT_WARNING);
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("Im Klartext speichern").clicked() {
                    answer = Some(true);
                }
                if ui.button("Abbrechen").clicked() {
                    answer = Some(false);
                }
            });
        });
        match answer {
            Some(true) => {
                self.plaintext_confirm_open = false;
                self.apply_plaintext(true);
            }
            Some(false) => self.plaintext_confirm_open = false,
            None => {}
        }
    }

    /// "Nach Updates suchen" applies at once, through the same safe save as every other setting.
    fn apply_check_updates(&mut self, on: bool) {
        let Some(path) = self.state.settings_path.clone() else {
            self.set_note(NoteKey::Updates, NoticeKind::Error, "Einstellungen sind nicht verfügbar.");
            return;
        };
        match update_settings(&path, |s| Ok(crate::settings::Settings { check_updates: on, ..s.clone() })) {
            Ok(settings) => {
                self.state.settings = settings;
                self.notes.remove(&NoteKey::Updates);
                self.update.settings_changed();
            }
            Err(text) => self.set_note(NoteKey::Updates, NoticeKind::Error, text),
        }
    }

    pub(super) fn apply_settings_action(&mut self, action: SettingsAction) {
        let key = action.note_key();
        let Some(path) = self.state.settings_path.clone() else {
            self.set_note(key, NoticeKind::Error, "Einstellungen sind nicht verfügbar.");
            return;
        };
        let now = Utc::now();
        let st = &self.state;
        let result = update_settings(&path, |s| match &action {
            SettingsAction::Baseline => with_baseline(s, &st.baseline_input, now),
            SettingsAction::TopUp => with_topup(s, &st.topup_input, now),
            SettingsAction::Remove(entry) => without_topup(s, entry).ok_or_else(|| {
                "Diese Aufladung steht nicht mehr in der Einstellungsdatei. Es wurde nichts geändert.".to_string()
            }),
            SettingsAction::Budget => with_limits(s, Some(st.budget_input.as_str()), None),
            SettingsAction::LowBalance => with_limits(s, None, Some(st.low_balance_input.as_str())),
        });
        match result {
            Ok(settings) => {
                self.state.settings = settings;
                let s = &self.state.settings;
                match action {
                    SettingsAction::Baseline => {
                        self.state.baseline_input = amount_input(s.baseline.as_ref().map(|b| b.amount_nano));
                        self.state.baseline_edited = false;
                    }
                    SettingsAction::TopUp => self.state.topup_input.clear(),
                    SettingsAction::Budget => {
                        self.state.budget_input = amount_input(s.monthly_budget_nano);
                        self.state.budget_edited = false;
                    }
                    SettingsAction::LowBalance => {
                        self.state.low_balance_input = amount_input(s.low_balance_nano);
                        self.state.low_balance_edited = false;
                    }
                    SettingsAction::Remove(_) => {}
                }
                self.set_note(
                    key,
                    NoticeKind::Info,
                    "Gespeichert. Die Karten übernehmen die Änderung beim nächsten Durchlauf (höchstens eine Minute).",
                );
            }
            Err(text) => self.set_note(key, NoticeKind::Error, text),
        }
    }

    // ---- drawing ---------------------------------------------------------------------------------

    /// Backdrop and panel. `body` is the area below the top bar and the banner strip.
    pub(super) fn draw_drawer(&mut self, ctx: &egui::Context, body: egui::Rect) {
        if self.drawer_open && !self.drawer_was_open {
            self.rescan_languages();
        }
        self.drawer_was_open = self.drawer_open;
        let t = ctx.animate_bool_with_time(egui::Id::unique("settings-drawer"), self.drawer_open, SLIDE_SECONDS);
        if t <= 0.0 {
            return;
        }
        let width = drawer_width(body.width());
        let x = body.right() - width * ease(t);

        // Backdrop: dims the cockpit and swallows every click on it.
        egui::Area::new(egui::Id::unique("drawer-backdrop"))
            .order(Order::Middle)
            .fixed_pos(body.min)
            .constrain(false)
            .show(ctx, |ui| {
                let (rect, response) = ui.allocate_exact_size(body.size(), Sense::click());
                ui.painter().rect_filled(rect, 0.0, BG.gamma_multiply(0.55 * t));
                if response.clicked() {
                    self.drawer_open = false;
                }
            });

        egui::Area::new(egui::Id::unique("drawer-panel"))
            .order(Order::Foreground)
            .fixed_pos(egui::pos2(x, body.top()))
            .constrain(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(DRAWER_FILL)
                    .inner_margin(Margin::same(PAD))
                    .shadow(Shadow { offset: [-6, 0], blur: 20, spread: 0, color: egui::Color32::from_black_alpha(110) })
                    .show(ui, |ui| {
                        let inner = vec2(width - 2.0 * f32::from(PAD), body.height() - 2.0 * f32::from(PAD));
                        ui.set_min_size(inner);
                        ui.set_max_size(inner);
                        let edge = ui.max_rect().expand(f32::from(PAD));
                        ui.painter().vline(edge.left(), edge.y_range(), Stroke::new(1.0, DRAWER_EDGE));
                        self.drawer_contents(ui);
                    });
            });
    }

    fn drawer_contents(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
        // Footer first (bottom-up), so the scroll area takes the rest.
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add(egui::Label::new(RichText::new(footer_text(self.state.settings_path.as_deref(), self.state.settings.keys_plaintext)).size(11.0).color(MUTED)));
            ui.add(egui::Separator::default().spacing(4.0));
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Einstellungen").size(14.0).strong().color(TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let close = egui::Button::new(RichText::new("✕").size(13.0).color(MUTED)).frame(false);
                        if ui.add(close).on_hover_text("Schließen").clicked() {
                            self.drawer_open = false;
                        }
                    });
                });
                ui.add_space(2.0);
                egui::ScrollArea::vertical().id_salt("drawer-scroll").auto_shrink([false, false]).show(ui, |ui| {
                    // Leave room for the scrollbar so no row runs under it.
                    ui.set_width(ui.available_width() - 10.0);
                    self.drawer_sections(ui);
                });
            });
        });
    }

    fn drawer_sections(&mut self, ui: &mut egui::Ui) {
        let broken = self.state.snapshot.as_ref().is_some_and(|s| s.settings_error.is_some());
        let can_save = !broken && self.state.settings_path.is_some();
        if broken {
            let place = self.state.settings_path.as_ref().map(|p| format!(" {}", p.display())).unwrap_or_default();
            warn(
                ui,
                &format!(
                    "Die Einstellungsdatei ist nicht lesbar. Speichern ist gesperrt, solange die Datei nicht geprüft oder gelöscht ist:{place}"
                ),
            );
        } else if self.state.settings_path.is_none() {
            warn(ui, "Speichern nicht möglich: Die Umgebungsvariable LOCALAPPDATA ist nicht gesetzt, es gibt keinen Ort für die Einstellungen.");
        }

        // -- Sprache
        section_title(ui, "Sprache");
        self.draw_language_section(ui);

        // -- Schlüssel
        section_title(ui, "Schlüssel");
        ui.label("API-Key");
        self.draw_key_row(ui, Slot::ApiKey, "sk-ant-…");
        hint(ui, "Nur für „Mit API-Key starten“. Der Key muss einem Workspace zugeordnet sein (Geltungsbereich Default), sonst lehnt die API Anfragen ab.");
        ui.add_space(6.0);
        ui.label("Admin-Key");
        self.draw_key_row(ui, Slot::AdminKey, "sk-ant-admin01-…");
        hint(ui, "Nur für die Kosten-API, braucht ein Organisations-Konto. Liefert die echten Kosten von Anthropic.");
        ui.add_space(6.0);
        ui.add_enabled_ui(can_save, |ui| {
            let mut on = self.state.settings.keys_plaintext;
            if ui.checkbox(&mut on, "Keys im Klartext speichern").changed() {
                if on {
                    self.plaintext_confirm_open = true;
                } else {
                    self.apply_plaintext(false);
                }
            }
        });
        hint(ui, PLAINTEXT_HINT);
        field_note(ui, self.note(NoteKey::Plaintext));

        // -- Guthaben
        let title = section_title(ui, "Guthaben");
        if self.drawer_scroll == Some(Section::Balance) {
            self.drawer_scroll = None;
            title.scroll_to_me(Some(Align::TOP));
        }
        ui.add_enabled_ui(can_save, |ui| self.draw_balance_section(ui));

        // -- Grenzen
        section_title(ui, "Grenzen");
        ui.add_enabled_ui(can_save, |ui| self.draw_limits_section(ui));
        let api = self.state.snapshot.as_ref().and_then(|s| s.api.as_ref());
        if let Some(text) = inert_limits_hint(api) {
            warn(ui, text);
        }

        // -- Pfade
        section_title(ui, "Pfade");
        let notes = [self.notes.get(&NoteKey::Obsidian), self.notes.get(&NoteKey::Logs)]
            .map(|n| n.map(|n| RowNote { error: n.kind == NoticeKind::Error, text: n.text.as_str() }));
        // `notes` borrows `self.notes`; the action is applied after the borrow ends.
        let action = self.paths.draw(ui, can_save, notes);
        if let Some(action) = action {
            self.apply_paths_action(ui.ctx(), action);
        }

        // -- Über
        section_title(ui, "Über");
        let launch_running = self.launch_running();
        let check_updates = self.state.settings.check_updates;
        if let Some(on) = self.update.draw_about(ui, check_updates, can_save, !broken, launch_running) {
            self.apply_check_updates(on);
        }
        field_note(ui, self.note(NoteKey::Updates));
        ui.add_space(8.0);
    }

    fn draw_language_section(&mut self, ui: &mut egui::Ui) {
        let current = crate::i18n::current_code();
        let shown = self
            .languages
            .iter()
            .find(|l| l.code.eq_ignore_ascii_case(&current))
            .map_or_else(|| current.clone(), |l| l.name.clone());
        let saved = self.state.settings.language.clone();
        let mut chosen = None;
        let mut open_folder = false;
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("language-choice").selected_text(shown).width(170.0).show_ui(ui, |ui| {
                for lang in &self.languages {
                    let is_current = lang.code.eq_ignore_ascii_case(&current);
                    if ui.selectable_label(is_current, &lang.name).clicked() {
                        // A pick of the language already saved is not a change; a pick of the one in use
                        // that is not saved yet (system default) pins it.
                        if !saved.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&lang.code)) {
                            chosen = Some(lang.code.clone());
                        }
                    }
                }
            });
            open_folder = ui.button("Sprachordner öffnen").clicked();
        });
        if let Some(code) = chosen {
            self.apply_language(&code);
        }
        if open_folder {
            match open_lang_folder() {
                Ok(()) => {
                    self.notes.remove(&NoteKey::Language);
                }
                Err(text) => self.set_note(NoteKey::Language, NoticeKind::Error, text),
            }
        }
        field_note(ui, self.note(NoteKey::Language));
        for problem in &self.lang_problems {
            ui.add(egui::Label::new(RichText::new(format!("{}: {}", problem.file, problem.reason)).size(12.0).color(AMBER)));
        }
        hint(ui, "Eigene Sprachdateien (.json) kommen in den Sprachordner und erscheinen, sobald die Einstellungen neu geöffnet werden.");
    }

    fn draw_key_row(&mut self, ui: &mut egui::Ui, slot: Slot, hint_text: &str) {
        let (stored, undecryptable) = match slot {
            Slot::ApiKey => (self.state.key_stored, self.state.key_undecryptable),
            Slot::AdminKey => (self.state.admin_key_stored, self.state.admin_key_undecryptable),
        };
        let id = key_field_id(slot);
        let status_color = if undecryptable {
            AMBER
        } else if stored == Some(true) {
            CYAN
        } else {
            MUTED
        };
        ui.add(egui::Label::new(RichText::new(key_status_line(stored, undecryptable)).color(status_color)));
        let mut save = false;
        let mut delete = false;
        let input = self.input_mut(slot);
        ui.add(
            egui::TextEdit::singleline(input)
                .id(id)
                .password(true)
                .hint_text(hint_text)
                .desired_width(ui.available_width()),
        );
        let empty = input.is_empty();
        ui.horizontal_wrapped(|ui| {
            save = ui.add_enabled(!empty, egui::Button::new("Speichern")).clicked();
            delete = ui.add_enabled(stored == Some(true), egui::Button::new("Löschen")).clicked();
        });
        field_note(ui, self.note(slot_note(slot)));
        if save {
            self.save_key(ui.ctx(), slot);
        }
        if delete {
            self.delete_key(ui.ctx(), slot);
        }
    }

    fn draw_balance_section(&mut self, ui: &mut egui::Ui) {
        let mut action: Option<SettingsAction> = None;

        ui.label("Startwert aus der Console (USD)");
        let baseline = self.state.settings.baseline.clone();
        let current = amount_input(baseline.as_ref().map(|b| b.amount_nano));
        ui.horizontal_wrapped(|ui| {
            let st = &mut self.state;
            if amount_field(ui, &mut st.baseline_input, &mut st.baseline_edited, &current, "65,03") {
                action = Some(SettingsAction::Baseline);
            }
            match &baseline {
                Some(b) => hint(ui, &format!("gilt seit {}", b.at.with_timezone(&Local).format("%d.%m.%Y"))),
                None => hint(ui, "noch nicht gesetzt"),
            }
        });
        hint(ui, "Der Startwert gilt ab jetzt; Aufladungen und Kosten davor zählen nicht. Speichert mit Enter.");
        field_note(ui, self.note(NoteKey::Baseline));

        ui.add_space(6.0);
        ui.label("Aufladungen");
        let topups = self.state.settings.topups.clone();
        if topups.is_empty() {
            hint(ui, "Keine Aufladungen eingetragen.");
        }
        for topup in &topups {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("{} · {}", topup.at.with_timezone(&Local).format("%d.%m.%Y"), format_usd(topup.amount_nano)));
                if ui.small_button("Entfernen").clicked() {
                    action = Some(SettingsAction::Remove(topup.clone()));
                }
            });
        }
        field_note(ui, self.note(NoteKey::TopUps));
        ui.horizontal_wrapped(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut self.state.topup_input).hint_text("10").desired_width(120.0));
            // A top-up is added on Enter or the button, never on blur: half-typed text must not become money.
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let add = ui.button("+ Hinzufügen").clicked();
            if (enter || add) && !self.state.topup_input.trim().is_empty() {
                action = Some(SettingsAction::TopUp);
            }
        });
        field_note(ui, self.note(NoteKey::TopUp));

        if let Some(action) = action {
            self.apply_settings_action(action);
        }
    }

    fn draw_limits_section(&mut self, ui: &mut egui::Ui) {
        let mut action: Option<SettingsAction> = None;
        let budget_now = amount_input(self.state.settings.monthly_budget_nano);
        let low_now = amount_input(self.state.settings.low_balance_nano);

        ui.label("Monatsbudget (USD, optional)");
        let st = &mut self.state;
        if amount_field(ui, &mut st.budget_input, &mut st.budget_edited, &budget_now, "20") {
            action = Some(SettingsAction::Budget);
        }
        field_note(ui, self.notes.get(&NoteKey::Budget));

        ui.add_space(4.0);
        ui.label("Warngrenze Guthaben (USD, optional)");
        let st = &mut self.state;
        if amount_field(ui, &mut st.low_balance_input, &mut st.low_balance_edited, &low_now, "10") {
            action = Some(SettingsAction::LowBalance);
        }
        field_note(ui, self.notes.get(&NoteKey::LowBalance));
        hint(ui, "Leer lassen schaltet die Grenze aus. Speichert mit Enter.");

        if let Some(action) = action {
            self.apply_settings_action(action);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn footer_names_the_config_not_the_credential_store() {
        let p = PathBuf::from(r"C:\Users\x\AppData\Local\CerberusUsage\settings.json");
        let text = footer_text(Some(&p), false);
        assert!(text.starts_with("Einstellungen: C:\\Users\\x"), "{text}");
        assert!(text.contains("settings.json") && text.ends_with("Keys: in dieser Datei, verschlüsselt"), "{text}");
        assert!(!text.contains("sk-ant") && !text.contains("Anmeldeinformationsspeicher"));
        assert!(footer_text(Some(&p), true).ends_with("Keys: in dieser Datei, im Klartext"));
        let none = footer_text(None, false);
        assert!(none.contains("LOCALAPPDATA") && none.contains("Keys: in dieser Datei"), "{none}");
        assert!(!none.contains("Anmeldeinformationsspeicher"), "{none}");
    }

    #[test]
    fn plaintext_texts_are_honest_about_what_encryption_protects_against() {
        // The warning names what becomes readable and where the copies end up.
        assert!(PLAINTEXT_WARNING.contains("lesbar") && PLAINTEXT_WARNING.contains("Backups"));
        // The default does not claim protection from other programs of the same Windows user.
        assert!(PLAINTEXT_HINT.contains("mitgenommen") && PLAINTEXT_HINT.contains("trotzdem lesen"));
        assert!(!PLAINTEXT_HINT.contains('\u{2014}') && !PLAINTEXT_WARNING.contains('\u{2014}'));
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cu-drawer-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_language_choice_applies_at_once_and_is_saved() {
        crate::i18n::isolate();
        let dir = temp_dir("choice-ok");
        let path = dir.join("settings.json");
        let saved = choose_language(Some(&path), "en").unwrap();
        assert_eq!(saved.language.as_deref(), Some("en"));
        assert_eq!(crate::i18n::current_code(), "en");
        assert_eq!(crate::settings::load(&path).0.language.as_deref(), Some("en"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_save_still_changes_the_language_for_the_session_and_says_so() {
        crate::i18n::isolate();
        let dir = temp_dir("choice-broken");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = choose_language(Some(&path), "en").unwrap_err();
        assert!(err.contains("nur bis zum Beenden"), "{err}");
        assert_eq!(crate::i18n::current_code(), "en");
        assert_eq!(std::fs::read(&path).unwrap(), b"{ not json", "a broken file is never overwritten");
        let none = choose_language(None, "en").unwrap_err();
        assert!(none.contains("nur bis zum Beenden"), "{none}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explorer_gets_the_folder_as_one_plain_argument() {
        let weird = PathBuf::from(r"C:\Users\a b\x & calc.exe\lang");
        let cmd = explorer_command(&weird);
        assert_eq!(cmd.get_program(), "explorer.exe");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, [weird.as_os_str()]);
    }

    #[test]
    fn opening_the_language_folder_creates_it_when_missing() {
        let dir = temp_dir("open").join("lang");
        assert!(!dir.exists());
        let cmd = open_folder_command(&dir).unwrap();
        assert!(dir.is_dir());
        assert_eq!(cmd.get_args().count(), 1);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn ease_runs_from_zero_to_one() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert!(ease(0.5) > 0.5);
        assert_eq!(ease(2.0), 1.0);
    }
}
