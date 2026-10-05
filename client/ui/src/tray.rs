//! The notification-area icon. It shows while Optimize is on (closing the
//! window then hides it here instead of quitting) and while the window is
//! hidden, with a menu to open the window, turn Optimize off, or quit.
//! Quitting closes only the window: an optimized session belongs to the
//! service and keeps running (docs/decisions/007-client-architecture.md).

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Wry};

use crate::strings::Strings;

pub const OPEN: &str = "open";
pub const DISABLE: &str = "disable";
pub const QUIT: &str = "quit";

/// The icon and its menu items. The handles are thread-safe, but every
/// change runs on the main thread, so call them from there (see
/// `crate::on_main_thread`) and never while holding a lock.
#[derive(Clone)]
pub struct Tray {
    icon: TrayIcon<Wry>,
    open: MenuItem<Wry>,
    disable: MenuItem<Wry>,
    quit: MenuItem<Wry>,
}

impl Tray {
    /// Builds the icon, hidden until a session starts.
    pub fn build(app: &AppHandle, strings: &Strings) -> tauri::Result<Self> {
        let open = MenuItem::with_id(app, OPEN, strings.tr("tray.open"), true, None::<&str>)?;
        let disable =
            MenuItem::with_id(app, DISABLE, strings.tr("tray.disable"), false, None::<&str>)?;
        let quit = MenuItem::with_id(app, QUIT, strings.tr("tray.quit"), true, None::<&str>)?;
        let separator = PredefinedMenuItem::separator(app)?;
        let menu = Menu::with_items(app, &[&open, &disable, &separator, &quit])?;
        let mut builder = TrayIconBuilder::with_id("main")
            .menu(&menu)
            .tooltip(strings.tr("app.name"))
            .show_menu_on_left_click(false)
            .on_menu_event(|app, event| crate::on_tray_menu(app, event.id().as_ref()))
            .on_tray_icon_event(|tray, event| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    crate::show_window(tray.app_handle());
                }
            });
        if let Some(icon) = app.default_window_icon() {
            builder = builder.icon(icon.clone());
        }
        let icon = builder.build(app)?;
        icon.set_visible(false)?;
        Ok(Self {
            icon,
            open,
            disable,
            quit,
        })
    }

    pub fn set_strings(&self, strings: &Strings) {
        for (item, key) in [
            (&self.open, "tray.open"),
            (&self.disable, "tray.disable"),
            (&self.quit, "tray.quit"),
        ] {
            if let Err(error) = item.set_text(strings.tr(key)) {
                tracing::warn!(%error, "cannot change the tray menu's language");
            }
        }
    }

    /// Shows or hides the icon; "Turn Optimize off" works only for the
    /// player's own session.
    pub fn update(&self, visible: bool, can_disable: bool, tooltip: &str) {
        let results = [
            self.icon.set_visible(visible),
            self.disable.set_enabled(can_disable),
            self.icon.set_tooltip(Some(tooltip)),
        ];
        for error in results.into_iter().filter_map(Result::err) {
            tracing::warn!(%error, "cannot update the tray icon");
        }
    }
}
