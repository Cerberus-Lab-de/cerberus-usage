//! Notification-area icon and the shared window state.
//!
//! Spike result (eframe 0.37, Windows): hiding via `ViewportCommand::Visible(false)` works. While the
//! window is hidden eframe skips `App::ui` but still calls `App::logic` after `Context::request_repaint`,
//! so tray/menu event handlers only need to send viewport commands and request a repaint.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{self, IconData, ViewportCommand};
use crate::balance::Alert;
use crate::pricing::format_usd;
use crate::ui::ModeDisplay;
use crate::view::mode_card_with_routing;
use crate::worker::Snapshot;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const ID_OPEN: &str = "open";
const ID_QUIT: &str = "quit";

/// What a close request of the window leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// Let the window close (the app ends).
    Exit,
    /// Cancel the close and hide the window to the tray.
    Hide,
    /// Cancel the close; quit as soon as the running launch has delivered its result.
    WaitForLaunch,
}

/// The X button only hides, unless the user chose "Beenden" or there is no tray icon to bring the
/// window back. A launch in flight is never cut off: quitting then waits for its result.
pub fn close_action(quit_requested: bool, tray_available: bool, launch_busy: bool) -> CloseAction {
    if quit_requested {
        CloseAction::Exit
    } else if !tray_available {
        if launch_busy { CloseAction::WaitForLaunch } else { CloseAction::Exit }
    } else {
        CloseAction::Hide
    }
}

/// What a left click on the tray icon does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayClick {
    Show,
    Hide,
}

/// A minimized window counts as visible for Windows, but a click must bring it back, not hide it.
pub fn tray_click(visible: bool, minimized: bool) -> TrayClick {
    if visible && !minimized { TrayClick::Hide } else { TrayClick::Show }
}

/// State shared between the tray handlers (main thread, window procedure) and the app.
pub struct WindowControl {
    visible: AtomicBool,
    minimized: AtomicBool,
    quit_requested: AtomicBool,
    quit_pending: AtomicBool,
    launch_busy: AtomicBool,
    tray_available: AtomicBool,
}

impl WindowControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            visible: AtomicBool::new(true),
            minimized: AtomicBool::new(false),
            quit_requested: AtomicBool::new(false),
            quit_pending: AtomicBool::new(false),
            launch_busy: AtomicBool::new(false),
            tray_available: AtomicBool::new(false),
        })
    }

    pub fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Relaxed)
    }

    pub fn set_minimized(&self, minimized: bool) {
        self.minimized.store(minimized, Ordering::Relaxed);
    }

    pub fn is_minimized(&self) -> bool {
        self.minimized.load(Ordering::Relaxed)
    }

    /// Decision for a close request seen in `logic`.
    pub fn close_action(&self) -> CloseAction {
        close_action(
            self.quit_requested.load(Ordering::Relaxed),
            self.tray_available.load(Ordering::Relaxed),
            self.launch_busy.load(Ordering::Relaxed),
        )
    }

    /// The app tells whether a launch is in flight. When it ends, a waiting quit goes through.
    pub fn set_launch_busy(&self, ctx: &egui::Context, busy: bool) {
        self.launch_busy.store(busy, Ordering::Relaxed);
        if !busy {
            self.finish_quit_if_idle(ctx);
        }
    }

    /// "Beenden" was chosen but a launch is still running.
    pub fn quit_waiting(&self) -> bool {
        self.quit_pending.load(Ordering::Relaxed) && self.launch_busy.load(Ordering::Relaxed)
    }

    /// A close request arrived while a launch runs (no tray): quit once it is done.
    pub fn quit_when_idle(&self, ctx: &egui::Context) {
        self.quit_pending.store(true, Ordering::Relaxed);
        self.finish_quit_if_idle(ctx);
    }

    fn finish_quit_if_idle(&self, ctx: &egui::Context) {
        if self.quit_pending.load(Ordering::Relaxed) && !self.launch_busy.load(Ordering::Relaxed) {
            self.quit_requested.store(true, Ordering::Relaxed);
            ctx.send_viewport_cmd(ViewportCommand::Close);
            ctx.request_repaint();
        } else {
            ctx.request_repaint();
        }
    }

    pub fn show(&self, ctx: &egui::Context) {
        self.visible.store(true, Ordering::Relaxed);
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        ctx.request_repaint();
    }

    pub fn hide(&self, ctx: &egui::Context) {
        self.visible.store(false, Ordering::Relaxed);
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        ctx.request_repaint();
    }

    /// "Beenden": closes at once, or as soon as a running launch has finished.
    pub fn quit(&self, ctx: &egui::Context) {
        self.quit_when_idle(ctx);
    }
}

/// How the notification-area icon looks: plain, or with a coloured badge for a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    Normal,
    /// Max is paused (orange badge).
    Warning,
    /// Budget exceeded or balance low (red badge).
    Alert,
}

/// The most severe state among the alerts.
pub fn icon_state(alerts: &[Alert]) -> IconState {
    alerts.iter().fold(IconState::Normal, |state, alert| {
        let this = match alert {
            Alert::BudgetExceeded | Alert::LowBalance => IconState::Alert,
            Alert::MaxPaused(_) => IconState::Warning,
        };
        if state == IconState::Alert || this == IconState::Alert { IconState::Alert } else { this }
    })
}

/// Copy of the RGBA image with a filled circle in the lower right corner; `Normal` returns it unchanged.
pub fn icon_with_badge(rgba: &[u8], width: u32, height: u32, state: IconState) -> Vec<u8> {
    let color = match state {
        IconState::Normal => return rgba.to_vec(),
        IconState::Warning => [230u8, 150, 30, 255],
        IconState::Alert => [200u8, 40, 40, 255],
    };
    let mut out = rgba.to_vec();
    let (w, h) = (width as f32, height as f32);
    let (cx, cy, r) = (w * 0.75, h * 0.75, w.min(h) * 0.2);
    for y in 0..height {
        for x in 0..width {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            let i = ((y * width + x) * 4) as usize;
            if dx * dx + dy * dy <= r * r && i + 4 <= out.len() {
                out[i..i + 4].copy_from_slice(&color);
            }
        }
    }
    out
}

/// Tooltip text: mode, today's estimated cost, a hint when warnings are active. Windows cuts tooltips
/// at 127 characters. Built from the snapshot only, which never holds a key.
pub fn tooltip(snapshot: &Snapshot, mode: ModeDisplay, routing_active: bool) -> String {
    let mut text = format!("Cerberus Usage · {}", mode_card_with_routing(mode, routing_active).title);
    if routing_active && mode == ModeDisplay::Max {
        text.push_str(" (Umleitung aktiv)");
    }
    if let Some(report) = &snapshot.report {
        text.push_str(&format!(" · heute {} (geschätzt)", format_usd(report.today.cost_nano)));
    }
    if !snapshot.alerts.is_empty() {
        text.push_str(" · Warnung");
    }
    text
}

/// Keeps the icon alive; dropping it removes the icon from the notification area.
pub struct Tray {
    icon: TrayIcon,
    base_rgba: Vec<u8>,
    size: (u32, u32),
    shown: Option<(String, IconState)>,
}

impl Tray {
    /// Must be created on the thread that runs the window event loop (the main thread).
    pub fn new(ctx: &egui::Context, control: &Arc<WindowControl>, icon: &IconData) -> Result<Tray, String> {
        let menu = Menu::new();
        let open = MenuItem::with_id(ID_OPEN, "Öffnen", true, None);
        let quit = MenuItem::with_id(ID_QUIT, "Beenden", true, None);
        menu.append(&open).map_err(|e| e.to_string())?;
        menu.append(&quit).map_err(|e| e.to_string())?;

        let tray_icon = Icon::from_rgba(icon.rgba.clone(), icon.width, icon.height).map_err(|e| e.to_string())?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("Cerberus Usage")
            .with_icon(tray_icon)
            .build()
            .map_err(|e| e.to_string())?;

        let (c, ctl) = (ctx.clone(), control.clone());
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                match tray_click(ctl.is_visible(), ctl.is_minimized()) {
                    TrayClick::Hide => ctl.hide(&c),
                    TrayClick::Show => ctl.show(&c),
                }
            }
        }));

        let (c, ctl) = (ctx.clone(), control.clone());
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| match event.id.0.as_str() {
            ID_OPEN => ctl.show(&c),
            ID_QUIT => ctl.quit(&c),
            _ => {}
        }));

        control.tray_available.store(true, Ordering::Relaxed);
        Ok(Tray {
            icon: tray,
            base_rgba: icon.rgba.clone(),
            size: (icon.width, icon.height),
            shown: None,
        })
    }

    /// Updates tooltip and icon badge; does nothing when neither changed.
    pub fn update(&mut self, tooltip: String, state: IconState) {
        if self.shown.as_ref().is_some_and(|(t, s)| *t == tooltip && *s == state) {
            return;
        }
        // `shown` is only set after both calls worked, so a failed update is retried at the next poll.
        let tip_ok = self.icon.set_tooltip(Some(tooltip.as_str())).is_ok();
        let rgba = icon_with_badge(&self.base_rgba, self.size.0, self.size.1, state);
        let icon_ok = Icon::from_rgba(rgba, self.size.0, self.size.1)
            .ok()
            .is_some_and(|icon| self.icon.set_icon(Some(icon)).is_ok());
        if !(tip_ok && icon_ok) {
            return;
        }
        self.shown = Some((tooltip, state));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::balance::Alert;
    use crate::report::{Bucket, Report};
    use crate::ui::ModeDisplay;
    use crate::worker::Snapshot;
    use chrono::{Local, Utc};

    fn snapshot(today_nano: u64) -> Snapshot {
        let report = Report { today: Bucket { cost_nano: today_nano, ..Bucket::default() }, ..Report::default() };
        Snapshot {
            report: Some(report),
            scanned_files: 1,
            total_files: 1,
            scan_error: None,
            updated: Local::now(),
            api: None,
            balance: None,
            alerts: Vec::new(),
            settings_error: None,
            state_note: None,
        }
    }

    #[test]
    fn tooltip_flags_a_routed_max() {
        let tip = tooltip(&snapshot(0), ModeDisplay::Max, true);
        assert!(tip.contains("Umleitung aktiv"), "{tip}");
        assert!(!tooltip(&snapshot(0), ModeDisplay::ApiKey, true).contains("Umleitung"));
    }

    #[test]
    fn tray_tooltip_shows_mode_and_today_cost() {
        let tip = tooltip(&snapshot(1_230_000_000), ModeDisplay::Max, false);
        assert!(tip.contains("Max") && tip.contains("$1.23"), "{tip}");
        assert!(tip.len() < 128, "Windows truncates tooltips at 127 characters: {tip}");
        // Without a report yet there is still a valid tooltip.
        let mut s = snapshot(0);
        s.report = None;
        assert!(tooltip(&s, ModeDisplay::Closed, false).contains("Cerberus Usage"));
        // Alerts are mentioned; no key material can be in a snapshot tooltip.
        let mut s = snapshot(0);
        s.alerts = vec![Alert::LowBalance];
        assert!(tooltip(&s, ModeDisplay::ApiKey, false).contains("Warnung"));
    }

    #[test]
    fn close_request_decisions() {
        // With a tray: X hides, "Beenden" exits.
        assert_eq!(close_action(false, true, false), CloseAction::Hide);
        assert_eq!(close_action(false, true, true), CloseAction::Hide);
        assert_eq!(close_action(true, true, false), CloseAction::Exit);
        // Without a tray the X button ends the app, but not in the middle of a launch.
        assert_eq!(close_action(false, false, false), CloseAction::Exit);
        assert_eq!(close_action(false, false, true), CloseAction::WaitForLaunch);
    }

    #[test]
    fn tray_click_restores_a_minimized_window() {
        assert_eq!(tray_click(true, false), TrayClick::Hide);
        assert_eq!(tray_click(true, true), TrayClick::Show);
        assert_eq!(tray_click(false, false), TrayClick::Show);
        assert_eq!(tray_click(false, true), TrayClick::Show);
    }

    #[test]
    fn quit_during_a_launch_waits_for_its_result() {
        let ctx = egui::Context::default();
        let control = WindowControl::new();
        control.set_launch_busy(&ctx, true);
        control.quit(&ctx);
        assert!(control.quit_waiting());
        assert_ne!(control.close_action(), CloseAction::Exit, "no close while the launch runs");
        control.set_launch_busy(&ctx, false);
        assert!(!control.quit_waiting());
        assert_eq!(control.close_action(), CloseAction::Exit, "quit goes through once the launch is done");

        // Without a launch "Beenden" is immediate.
        let control = WindowControl::new();
        control.quit(&ctx);
        assert_eq!(control.close_action(), CloseAction::Exit);
    }

    #[test]
    fn tray_icon_state_prefers_alert() {
        assert_eq!(icon_state(&[]), IconState::Normal);
        let paused = Alert::MaxPaused(Utc::now());
        assert_eq!(icon_state(std::slice::from_ref(&paused)), IconState::Warning);
        assert_eq!(icon_state(&[paused, Alert::BudgetExceeded]), IconState::Alert);
        assert_eq!(icon_state(&[Alert::LowBalance]), IconState::Alert);
    }

    #[test]
    fn badge_recolours_a_corner_and_keeps_the_rest() {
        let (w, h) = (32usize, 32usize);
        let base = vec![10u8; w * h * 4];
        assert_eq!(icon_with_badge(&base, w as u32, h as u32, IconState::Normal), base);
        let alert = icon_with_badge(&base, w as u32, h as u32, IconState::Alert);
        let warn = icon_with_badge(&base, w as u32, h as u32, IconState::Warning);
        let px = |v: &[u8], x: usize, y: usize| v[(y * w + x) * 4..(y * w + x) * 4 + 4].to_vec();
        assert_eq!(px(&alert, 26, 26)[3], 255);
        assert_ne!(px(&alert, 26, 26), px(&warn, 26, 26));
        assert_eq!(px(&alert, 2, 2), px(&base, 2, 2));
    }
}
