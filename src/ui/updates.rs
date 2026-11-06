//! Update state as the window sees it: the dim hint line in the cockpit, the "About" block in the
//! drawer and the confirm dialog. The checker thread (`update_worker`) does the work; this only keeps
//! the last state it sent and draws it. No IO per frame.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use chrono::{DateTime, Local};
use eframe::egui::{self, RichText};

use super::theme::palette::{AMBER, CYAN, MUTED};
use crate::format::format_time_by;
use crate::i18n::{tr, tr_args};
use crate::update::{CURRENT_VERSION, Release};
use crate::update_worker::{UpdateCmd, UpdateState};

/// The repository, as shown and opened from the drawer.
pub const REPO_URL: &str = "https://github.com/Cerberus-Lab-de/cerberus-usage";

/// Only pages of our own repository are ever opened in the browser.
pub fn url_allowed(url: &str) -> bool {
    url.strip_prefix(REPO_URL).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Why installing is refused right now (`None`: allowed).
pub fn install_block_reason(launch_running: bool) -> Option<String> {
    launch_running.then(|| tr("updates.blocked"))
}

/// "Last checked: 2026-11-05 21:30" or the not-yet text.
pub fn last_checked_text(checked: Option<DateTime<Local>>) -> String {
    match checked {
        Some(t) => {
            let time = format_time_by(&t, "format.datetime_short", "%Y-%m-%d %H:%M");
            tr_args("updates.last_checked", &[("time", &time)])
        }
        None => tr("updates.not_checked"),
    }
}

/// What the cockpit line shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintLine {
    None,
    Available(String),
    Failed(String),
}

/// An available release wins over an old failure; a failure alone is one dim line.
pub fn hint_line(available: Option<&Release>, failed: Option<&str>) -> HintLine {
    match (available, failed) {
        (Some(r), _) => HintLine::Available(r.version.to_string()),
        (None, Some(m)) => HintLine::Failed(m.to_string()),
        (None, None) => HintLine::None,
    }
}

pub struct UpdateUi {
    tx: Sender<UpdateCmd>,
    rx: Receiver<UpdateState>,
    state: UpdateState,
    available: Option<Release>,
    last_checked: Option<DateTime<Local>>,
    /// Latest failure, for the dim line.
    failed: Option<String>,
    /// The user clicked "Check now" or installed: the next failure is shown in full.
    explicit_pending: bool,
    explicit_error: Option<String>,
    /// The explicit action was an install (its error is also shown in the cockpit).
    error_from_install: bool,
    /// An install was confirmed and is not answered yet: one click cannot queue two.
    install_pending: bool,
    confirm_open: bool,
    /// Mirrors "a launch is running" for the checker thread.
    launch_flag: Arc<AtomicBool>,
}

impl UpdateUi {
    pub fn new(tx: Sender<UpdateCmd>, rx: Receiver<UpdateState>, launch_flag: Arc<AtomicBool>) -> Self {
        Self {
            tx,
            rx,
            state: UpdateState::Idle,
            available: None,
            last_checked: None,
            failed: None,
            explicit_pending: false,
            explicit_error: None,
            error_from_install: false,
            install_pending: false,
            confirm_open: false,
            launch_flag,
        }
    }

    /// Takes in everything the checker sent. Returns true once the installer has been started: the
    /// app must quit now.
    pub fn poll(&mut self) -> bool {
        let mut installing = false;
        loop {
            let state = match self.rx.try_recv() {
                Ok(s) => s,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !matches!(self.state, UpdateState::Failed(_)) {
                        self.apply(UpdateState::Failed(tr("updates.service_gone")));
                    }
                    break;
                }
            };
            installing |= state == UpdateState::Installing;
            self.apply(state);
        }
        installing
    }

    fn apply(&mut self, state: UpdateState) {
        match &state {
            UpdateState::UpToDate { checked } => {
                self.available = None;
                self.last_checked = Some(*checked);
                self.settle(None);
            }
            UpdateState::Available(r) => {
                self.available = Some(r.clone());
                self.last_checked = Some(Local::now());
                self.settle(None);
            }
            UpdateState::Failed(m) => {
                let explicit = self.explicit_pending.then(|| m.clone());
                self.settle(Some(m.clone()));
                self.explicit_error = explicit;
            }
            UpdateState::Idle | UpdateState::Checking | UpdateState::Downloading | UpdateState::Installing => {}
        }
        self.state = state;
    }

    /// A check or install ended: remember the failure (if any) and drop the pending flag.
    fn settle(&mut self, failed: Option<String>) {
        self.failed = failed;
        self.explicit_pending = false;
        self.install_pending = false;
        self.explicit_error = None;
    }

    /// Checking or downloading: the only times anything may animate.
    pub fn busy(&self) -> bool {
        matches!(self.state, UpdateState::Checking | UpdateState::Downloading)
    }

    fn downloading(&self) -> bool {
        self.state == UpdateState::Downloading
    }

    fn send(&mut self, cmd: UpdateCmd) {
        self.explicit_pending = true;
        self.explicit_error = None;
        self.error_from_install = cmd == UpdateCmd::Install;
        self.install_pending = cmd == UpdateCmd::Install;
        let _ = self.tx.send(cmd);
    }

    /// Tells the checker a launch is (not) running, so it never starts the installer under one.
    pub fn set_launch_running(&self, running: bool) {
        self.launch_flag.store(running, Ordering::SeqCst);
    }

    /// The toggle was saved: the checker looks at the setting now (switching on checks at once).
    pub fn settings_changed(&self) {
        let _ = self.tx.send(UpdateCmd::SettingsChanged);
    }

    /// Why Obsidian cannot be started right now (a download or the installer is running).
    pub fn start_block_reason(&self) -> Option<String> {
        (self.downloading() || self.install_pending || self.state == UpdateState::Installing)
            .then(|| tr("updates.start_blocked"))
    }

    pub fn check_now(&mut self) {
        if !self.busy() {
            self.send(UpdateCmd::CheckNow);
        }
    }

    fn open(ctx: &egui::Context, url: &str) {
        if url_allowed(url) {
            ctx.open_url(egui::OpenUrl::new_tab(url));
        }
    }

    /// The "Version x.y.z available · Changes · Update now" part, shared by the cockpit
    /// and the drawer.
    fn available_row(&mut self, ui: &mut egui::Ui, launch_running: bool) {
        let Some(release) = self.available.clone() else { return };
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let version = release.version.to_string();
            ui.label(RichText::new(tr_args("updates.available", &[("version", &version)])).size(13.0).color(MUTED));
            ui.label(RichText::new("·").color(MUTED));
            if link(ui, &tr("updates.changes")).clicked() {
                Self::open(ui.ctx(), &release.notes_url);
            }
            ui.label(RichText::new("·").color(MUTED));
            self.install_button(ui, launch_running);
        });
    }

    fn install_button(&mut self, ui: &mut egui::Ui, launch_running: bool) {
        if self.downloading() || self.install_pending {
            ui.spinner();
            ui.label(RichText::new(tr("updates.downloading")).size(13.0).color(MUTED));
            return;
        }
        let reason = install_block_reason(launch_running)
            .or_else(|| (self.state == UpdateState::Installing).then(|| tr("updates.running")));
        let response = ui.add_enabled(
            reason.is_none(),
            egui::Button::new(RichText::new(tr("button.update_now")).size(13.0).color(CYAN)).frame(false),
        );
        let response = match reason {
            Some(text) => response.on_disabled_hover_text(text),
            None => response,
        };
        if response.clicked() {
            self.confirm_open = true;
        }
    }

    /// The dim line under the cockpit.
    pub fn draw_hint(&mut self, ui: &mut egui::Ui, launch_running: bool) {
        match hint_line(self.available.as_ref(), self.failed.as_deref()) {
            HintLine::None => {}
            HintLine::Available(_) => {
                self.available_row(ui, launch_running);
                // A failed "Check now" is shown in the drawer only; a failed install here too.
                if let (Some(error), true) = (&self.explicit_error, self.error_from_install) {
                    ui.add(egui::Label::new(RichText::new(error).size(13.0).color(AMBER)).wrap());
                }
            }
            HintLine::Failed(message) => {
                ui.label(RichText::new(tr("updates.check_failed")).size(12.0).color(MUTED))
                    .on_hover_text(message);
            }
        }
    }

    /// The update part of the drawer's "About" section. Returns the new value of "Check for
    /// updates" when it was clicked; the caller saves it.
    pub fn draw_about(&mut self, ui: &mut egui::Ui, check_updates: bool, can_save: bool, settings_ok: bool, launch_running: bool) -> Option<bool> {
        ui.label(format!("Cerberus Usage {CURRENT_VERSION}"));
        ui.add(egui::Label::new(RichText::new(tr("updates.about.by")).size(12.0).color(MUTED)));
        ui.add_space(4.0);

        let mut toggled = None;
        if settings_ok {
            ui.add_enabled_ui(can_save, |ui| {
                let mut on = check_updates;
                if ui.checkbox(&mut on, tr("toggle.check_updates")).changed() {
                    toggled = Some(on);
                }
            });
        } else {
            ui.add(
                egui::Label::new(
                    RichText::new(tr("updates.settings_unreadable"))
                        .size(12.0)
                        .color(MUTED),
                )
                .wrap(),
            );
        }
        if settings_ok && !check_updates {
            ui.add(
                egui::Label::new(
                    RichText::new(tr_args("updates.auto_off", &[("button", &tr("button.check_now"))]))
                    .size(12.0)
                    .color(MUTED),
                )
                .wrap(),
            );
        }

        ui.horizontal(|ui| {
            if ui.add_enabled(!self.busy(), egui::Button::new(tr("button.check_now"))).clicked() {
                self.check_now();
            }
            if self.state == UpdateState::Checking {
                ui.spinner();
                ui.label(RichText::new(tr("updates.checking")).size(12.0).color(MUTED));
            }
        });
        ui.add(egui::Label::new(RichText::new(last_checked_text(self.last_checked)).size(12.0).color(MUTED)));

        if self.available.is_some() {
            self.available_row(ui, launch_running);
        } else if matches!(self.state, UpdateState::UpToDate { .. }) {
            ui.add(egui::Label::new(RichText::new(tr("updates.up_to_date")).size(12.0).color(MUTED)));
        }
        if let Some(error) = &self.explicit_error {
            ui.add(egui::Label::new(RichText::new(error).color(AMBER)).wrap());
        } else if let (Some(message), None) = (&self.failed, &self.available) {
            ui.add(egui::Label::new(RichText::new(message).size(12.0).color(MUTED)).wrap());
        }

        ui.add_space(4.0);
        // Shown without the scheme so it fits the 360 px drawer; the click opens the full address.
        if link(ui, REPO_URL.trim_start_matches("https://")).clicked() {
            Self::open(ui.ctx(), REPO_URL);
        }
        ui.add(egui::Label::new(RichText::new(tr("updates.license")).size(12.0).color(MUTED)));
        toggled
    }

    /// The update confirmation is open (Esc closes it before anything behind it).
    pub fn confirm_is_open(&self) -> bool {
        self.confirm_open
    }

    /// Closes the confirmation without installing.
    pub fn cancel_confirm(&mut self) {
        self.confirm_open = false;
    }

    /// The confirmation before anything is downloaded.
    pub fn draw_confirm(&mut self, ctx: &egui::Context, launch_running: bool) {
        if !self.confirm_open {
            return;
        }
        let mut answer = None;
        egui::Modal::new(egui::Id::unique("confirm-update")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading(tr("updates.confirm.title"));
            ui.label(tr("updates.confirm.text"));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let reason = install_block_reason(launch_running);
                let button = ui.add_enabled(reason.is_none(), egui::Button::new(tr("button.update_now")));
                let button = match reason {
                    Some(text) => button.on_disabled_hover_text(text),
                    None => button,
                };
                if button.clicked() {
                    answer = Some(true);
                }
                if ui.button(tr("button.cancel")).clicked() {
                    answer = Some(false);
                }
            });
        });
        match answer {
            Some(true) => {
                self.confirm_open = false;
                self.send(UpdateCmd::Install);
            }
            Some(false) => self.confirm_open = false,
            None => {}
        }
    }
}

/// A flat, cyan text button (a link look without a browser round trip in egui itself).
fn link(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).size(13.0).color(CYAN)).frame(false))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;
    use std::sync::mpsc;

    fn release(v: &str) -> Release {
        Release {
            version: Version::parse(v).unwrap(),
            notes_url: format!("{REPO_URL}/releases/tag/v{v}"),
            installer_url: String::new(),
            manifest_url: String::new(),
        }
    }

    fn ui_with() -> (UpdateUi, Sender<UpdateState>, Receiver<UpdateCmd>) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (state_tx, state_rx) = mpsc::channel();
        (UpdateUi::new(cmd_tx, state_rx, Default::default()), state_tx, cmd_rx)
    }

    #[test]
    fn only_our_own_repository_urls_are_opened() {
        assert!(url_allowed(REPO_URL));
        assert!(url_allowed(&format!("{REPO_URL}/releases/tag/v1.0.1")));
        for bad in [
            "http://github.com/Cerberus-Lab-de/cerberus-usage/",
            "https://github.com/Cerberus-Lab-de/cerberus-usage-evil",
            "https://github.com/Cerberus-Lab-de/cerberus-usage.evil.com/",
            "https://github.com/Cerberus-Lab-de/other/releases",
            "https://evil.example/https://github.com/Cerberus-Lab-de/cerberus-usage/",
            "file:///C:/Windows/System32/calc.exe",
            "",
        ] {
            assert!(!url_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn a_running_launch_blocks_the_install_with_a_reason() {
        assert!(install_block_reason(true).is_some());
        assert!(install_block_reason(false).is_none());
    }

    #[test]
    fn hint_line_prefers_an_available_release_and_is_empty_otherwise() {
        let r = release("1.0.1");
        assert_eq!(hint_line(Some(&r), Some("x")), HintLine::Available("1.0.1".into()));
        assert_eq!(hint_line(None, Some("x")), HintLine::Failed("x".into()));
        assert_eq!(hint_line(None, None), HintLine::None);
    }

    #[test]
    fn automatic_failure_stays_a_dim_hint_and_explicit_failure_is_kept_for_the_drawer() {
        let (mut ui, tx, _cmds) = ui_with();
        tx.send(UpdateState::Failed("offline".into())).unwrap();
        assert!(!ui.poll());
        assert_eq!(ui.failed.as_deref(), Some("offline"));
        assert_eq!(ui.explicit_error, None);

        ui.check_now();
        tx.send(UpdateState::Checking).unwrap();
        tx.send(UpdateState::Failed("GitHub-Limit".into())).unwrap();
        ui.poll();
        assert_eq!(ui.explicit_error.as_deref(), Some("GitHub-Limit"));

        tx.send(UpdateState::UpToDate { checked: Local::now() }).unwrap();
        ui.poll();
        assert_eq!((ui.failed.as_deref(), ui.explicit_error.as_deref()), (None, None));
    }

    #[test]
    fn a_failed_install_keeps_the_available_release_for_a_retry() {
        let (mut ui, tx, cmds) = ui_with();
        tx.send(UpdateState::Available(release("1.0.1"))).unwrap();
        ui.poll();
        ui.send(UpdateCmd::Install);
        assert_eq!(cmds.try_recv(), Ok(UpdateCmd::Install));
        tx.send(UpdateState::Downloading).unwrap();
        ui.poll();
        assert!(ui.busy());
        tx.send(UpdateState::Failed("Update-Schlüssel fehlt in dieser Version.".into())).unwrap();
        ui.poll();
        assert!(!ui.busy());
        assert!(ui.available.is_some());
        assert_eq!(ui.explicit_error.as_deref(), Some("Update-Schlüssel fehlt in dieser Version."));
    }

    #[test]
    fn every_answer_clears_the_pending_flags_and_repeats_are_shown() {
        let (mut ui, tx, _cmds) = ui_with();
        for _ in 0..2 {
            ui.send(UpdateCmd::Install);
            assert!(ui.explicit_pending && ui.install_pending && ui.start_block_reason().is_some());
            tx.send(UpdateState::Failed("Update-Schlüssel fehlt in dieser Version.".into())).unwrap();
            ui.poll();
            assert!(!ui.explicit_pending && !ui.install_pending && ui.start_block_reason().is_none());
            assert_eq!(ui.explicit_error.as_deref(), Some("Update-Schlüssel fehlt in dieser Version."));
        }
        // A later automatic failure is not shown as an explicit one.
        tx.send(UpdateState::Failed("offline".into())).unwrap();
        ui.poll();
        assert_eq!(ui.explicit_error, None);
        // Each explicit check answers and clears the flag, whatever the answer is.
        for answer in [UpdateState::UpToDate { checked: Local::now() }, UpdateState::Available(release("1.0.1"))] {
            ui.check_now();
            assert!(ui.explicit_pending);
            tx.send(UpdateState::Checking).unwrap();
            tx.send(answer).unwrap();
            ui.poll();
            assert!(!ui.explicit_pending);
        }
    }

    #[test]
    fn a_failed_check_with_a_release_available_is_not_repeated_in_the_cockpit() {
        let (mut ui, tx, _cmds) = ui_with();
        tx.send(UpdateState::Available(release("1.0.1"))).unwrap();
        ui.poll();
        ui.check_now();
        tx.send(UpdateState::Failed("offline".into())).unwrap();
        ui.poll();
        assert_eq!(ui.explicit_error.as_deref(), Some("offline"));
        assert!(!ui.error_from_install, "the cockpit shows explicit errors of installs only");
    }

    #[test]
    fn installing_asks_the_app_to_quit_and_a_dead_checker_is_a_quiet_failure() {
        let (mut ui, tx, _cmds) = ui_with();
        tx.send(UpdateState::Installing).unwrap();
        assert!(ui.poll());
        drop(tx);
        assert!(!ui.poll());
        assert!(matches!(ui.state, UpdateState::Failed(_)));
        assert_eq!(ui.explicit_error, None);
    }

    #[test]
    fn check_now_is_ignored_while_a_check_runs_and_idle_is_not_busy() {
        let (mut ui, tx, cmds) = ui_with();
        assert!(!ui.busy());
        tx.send(UpdateState::Checking).unwrap();
        ui.poll();
        assert!(ui.busy());
        ui.check_now();
        assert!(cmds.try_recv().is_err());
    }

    #[test]
    fn last_check_text() {
        assert_eq!(last_checked_text(None), "Noch nicht geprüft.");
        assert!(last_checked_text(Some(Local::now())).starts_with("Zuletzt geprüft: "));
    }
}
