//! Notification-area icon and the shared window state.
//!
//! Spike result (eframe 0.37, Windows): hiding via `ViewportCommand::Visible(false)` works. While the
//! window is hidden eframe skips `App::ui` but still calls `App::logic` after `Context::request_repaint`,
//! so tray/menu event handlers only need to send viewport commands and request a repaint.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{self, IconData, ViewportCommand};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const ID_OPEN: &str = "open";
const ID_QUIT: &str = "quit";

/// State shared between the tray handlers (main thread, window procedure) and the app.
pub struct WindowControl {
    visible: AtomicBool,
    quit_requested: AtomicBool,
    tray_available: AtomicBool,
}

impl WindowControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            visible: AtomicBool::new(true),
            quit_requested: AtomicBool::new(false),
            tray_available: AtomicBool::new(false),
        })
    }

    pub fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Relaxed)
    }

    /// The window's X button only hides, unless the user chose "Beenden" or there is no tray icon
    /// to bring the window back.
    pub fn close_means_exit(&self) -> bool {
        self.quit_requested.load(Ordering::Relaxed) || !self.tray_available.load(Ordering::Relaxed)
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

    pub fn quit(&self, ctx: &egui::Context) {
        self.quit_requested.store(true, Ordering::Relaxed);
        ctx.send_viewport_cmd(ViewportCommand::Close);
        ctx.request_repaint();
    }
}

/// Keeps the icon alive; dropping it removes the icon from the notification area.
pub struct Tray {
    _icon: TrayIcon,
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
                if ctl.is_visible() {
                    ctl.hide(&c);
                } else {
                    ctl.show(&c);
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
        Ok(Tray { _icon: tray })
    }
}
