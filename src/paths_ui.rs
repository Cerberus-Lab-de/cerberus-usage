//! The "Pfade" section of the settings panel: Obsidian executable and Claude log folder.
//!
//! The "Verwendet" lines are computed when the settings are loaded or saved, never per frame.
//! File dialogs run on their own thread, one at a time, so the window never blocks.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use eframe::egui::{self, Color32, RichText};

use crate::cards::update_settings;
use crate::paths::{real_env, resolve_logs_root, resolve_obsidian, with_logs_root, with_obsidian_path};
use crate::settings::Settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Obsidian,
    Logs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathsAction {
    Save(Field),
    Reset(Field),
    Browse(Field),
}

/// What a row says about the path in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Used {
    Ok(PathBuf),
    Problem(String),
}

fn obsidian_used(settings: &Settings) -> Used {
    match resolve_obsidian(settings.obsidian_path.as_deref(), &real_env, &|p| p.is_file()) {
        Ok(p) => Used::Ok(p),
        Err(e) => Used::Problem(e.to_string()),
    }
}

fn logs_used(settings: &Settings) -> Used {
    match resolve_logs_root(settings.logs_root.as_deref(), &real_env) {
        Ok(p) if p.is_dir() => Used::Ok(p),
        Ok(p) => Used::Problem(format!("Protokollordner nicht gefunden: {}", p.display())),
        Err(e) => Used::Problem(e.to_string()),
    }
}

pub struct PathsPanel {
    obsidian_input: String,
    logs_input: String,
    /// Typed since the field was last filled from the file; such a field is not overwritten by a reload.
    obsidian_edited: bool,
    logs_edited: bool,
    obsidian: Used,
    logs: Used,
    dialog: Option<(Field, Receiver<Option<PathBuf>>)>,
}

impl PathsPanel {
    pub fn new(settings: &Settings) -> Self {
        let mut panel = Self {
            obsidian_input: String::new(),
            logs_input: String::new(),
            obsidian_edited: false,
            logs_edited: false,
            obsidian: Used::Problem(String::new()),
            logs: Used::Problem(String::new()),
            dialog: None,
        };
        panel.refresh(settings);
        panel
    }

    /// Re-reads the "Verwendet" lines and fills fields the user has not touched.
    pub fn refresh(&mut self, settings: &Settings) {
        if !self.obsidian_edited {
            self.obsidian_input = path_text(settings.obsidian_path.as_deref());
        }
        if !self.logs_edited {
            self.logs_input = path_text(settings.logs_root.as_deref());
        }
        self.obsidian = obsidian_used(settings);
        self.logs = logs_used(settings);
    }

    /// Saves or resets one field through `update_settings` (the file is re-read, a broken file is refused).
    pub fn apply(&mut self, action: PathsAction, settings_path: &Path) -> Result<Settings, String> {
        let (field, reset) = match action {
            PathsAction::Save(f) => (f, false),
            PathsAction::Reset(f) => (f, true),
            PathsAction::Browse(_) => return Err("Kein Speichern".to_string()),
        };
        let input = match (field, reset) {
            (_, true) => "",
            (Field::Obsidian, false) => self.obsidian_input.as_str(),
            (Field::Logs, false) => self.logs_input.as_str(),
        };
        let saved = update_settings(settings_path, |s| match field {
            Field::Obsidian => with_obsidian_path(s, input),
            Field::Logs => with_logs_root(s, input),
        })?;
        match field {
            Field::Obsidian => self.obsidian_edited = false,
            Field::Logs => self.logs_edited = false,
        }
        self.refresh(&saved);
        Ok(saved)
    }

    /// Opens the file dialog on its own thread. Ignored while another dialog is open.
    pub fn browse(&mut self, field: Field, ctx: &egui::Context) {
        if self.dialog.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let picked = match field {
                Field::Obsidian => rfd::FileDialog::new()
                    .set_title("Obsidian.exe auswählen")
                    .add_filter("Programm (*.exe)", &["exe"])
                    .pick_file(),
                Field::Logs => rfd::FileDialog::new().set_title("Protokollordner auswählen").pick_folder(),
            };
            let _ = tx.send(picked);
            ctx.request_repaint();
        });
        self.dialog = Some((field, rx));
    }

    /// Takes a finished dialog result into the text field (saving stays an explicit click).
    pub fn poll_dialog(&mut self) {
        let Some((field, rx)) = &self.dialog else { return };
        let field = *field;
        match rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.dialog = None,
            Ok(picked) => {
                self.dialog = None;
                if let Some(path) = picked {
                    let text = path.display().to_string();
                    match field {
                        Field::Obsidian => {
                            self.obsidian_input = text;
                            self.obsidian_edited = true;
                        }
                        Field::Logs => {
                            self.logs_input = text;
                            self.logs_edited = true;
                        }
                    }
                }
            }
        }
    }

    pub fn draw(&mut self, ui: &mut egui::Ui, can_save: bool) -> Option<PathsAction> {
        let mut action = None;
        ui.label(RichText::new("Pfade").strong());
        let busy = self.dialog.is_some();
        ui.add_enabled_ui(can_save, |ui| {
            ui.label("Obsidian (leer: automatisch suchen)");
            let (input, edited) = (&mut self.obsidian_input, &mut self.obsidian_edited);
            row(ui, Field::Obsidian, input, edited, busy, r"C:\Users\…\AppData\Local\Programs\Obsidian\Obsidian.exe", &mut action);
            used_line(ui, &self.obsidian, None);

            ui.add_space(4.0);
            ui.label("Claude-Protokolle (leer: automatisch suchen)");
            let (input, edited) = (&mut self.logs_input, &mut self.logs_edited);
            row(ui, Field::Logs, input, edited, busy, r"C:\Users\…\.claude\projects", &mut action);
            used_line(ui, &self.logs, Some("Wird nach einem Neustart der App wirksam."));
        });
        action
    }
}

fn path_text(p: Option<&Path>) -> String {
    p.map(|p| p.display().to_string()).unwrap_or_default()
}

fn row(
    ui: &mut egui::Ui,
    field: Field,
    input: &mut String,
    edited: &mut bool,
    busy: bool,
    hint: &str,
    action: &mut Option<PathsAction>,
) {
    ui.horizontal(|ui| {
        let r = ui.add(egui::TextEdit::singleline(input).hint_text(hint).desired_width(380.0));
        *edited |= r.changed();
        if ui.add_enabled(!busy, egui::Button::new("Durchsuchen")).clicked() {
            *action = Some(PathsAction::Browse(field));
        }
        if ui.button("Speichern").clicked() {
            *action = Some(PathsAction::Save(field));
        }
        if ui.button("Zurücksetzen").clicked() {
            *action = Some(PathsAction::Reset(field));
        }
    });
}

fn used_line(ui: &mut egui::Ui, used: &Used, note: Option<&str>) {
    match used {
        Used::Ok(p) => {
            ui.label(RichText::new(format!("Verwendet: {}", p.display())).weak().small());
        }
        Used::Problem(text) => {
            ui.colored_label(Color32::from_rgb(230, 150, 30), text);
        }
    }
    if let Some(note) = note {
        ui.label(RichText::new(note).weak().small());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_reset_go_through_the_safe_update_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut panel = PathsPanel::new(&Settings::default());

        panel.obsidian_input = r"D:\Apps\Obsidian.exe".into();
        panel.obsidian_edited = true;
        let saved = panel.apply(PathsAction::Save(Field::Obsidian), &path).unwrap();
        assert_eq!(saved.obsidian_path, Some(PathBuf::from(r"D:\Apps\Obsidian.exe")));
        assert!(!panel.obsidian_edited);
        // The configured file does not exist here: the line says so instead of falling back.
        assert!(matches!(panel.obsidian, Used::Problem(_)));

        panel.logs_input = "relative".into();
        assert!(panel.apply(PathsAction::Save(Field::Logs), &path).is_err());

        let reset = panel.apply(PathsAction::Reset(Field::Obsidian), &path).unwrap();
        assert_eq!(reset.obsidian_path, None);
        assert!(panel.obsidian_input.is_empty());

        // A broken file is refused and left alone.
        std::fs::write(&path, "{ broken").unwrap();
        panel.logs_input = r"E:\logs".into();
        assert!(panel.apply(PathsAction::Save(Field::Logs), &path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ broken");
    }

    #[test]
    fn edited_fields_survive_a_refresh() {
        let mut panel = PathsPanel::new(&Settings::default());
        panel.logs_input = "typed".into();
        panel.logs_edited = true;
        panel.refresh(&Settings { logs_root: Some(PathBuf::from(r"E:\other")), ..Settings::default() });
        assert_eq!(panel.logs_input, "typed");
        assert!(panel.obsidian_input.is_empty());
    }
}
