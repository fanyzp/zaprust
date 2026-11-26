//! System tray residence (parity row 125, Linux): while the
//! setting is on and a StatusNotifier watcher actually
//! answers on the session bus, closing the window hides
//! Zaprust to the tray instead of quitting. The tray menu is
//! Open and Quit; the tray title carries the unread count.
//!
//! The watcher check is the honesty bound of the feature:
//! the app must never hide into a tray that does not exist.
//! It runs once when the tray starts (a watcher appearing
//! later is picked up at the next start), and the close path
//! consults it every time. The tray itself is ksni, a
//! pure-Rust StatusNotifierItem over D-Bus: no native
//! appindicator library is involved. Menu actions run on
//! the tray's D-Bus thread and act on the window through the
//! egui context directly, which is what wakes a hidden
//! window back up.
//!
//! Linux-only: ksni speaks StatusNotifierItem over D-Bus,
//! which macOS and Windows hosts do not run. On other
//! platforms this module compiles a no-op tray instead, so
//! the app builds and runs with no tray icon there. A native
//! tray for those platforms is a later round, not this one.

use crate::state::App;
#[cfg(target_os = "linux")]
use core::filter::total_unread;
use eframe::egui;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::sync::Arc;

/// The running tray: the ksni handle for title updates and
/// shutdown, plus the two flags shared with the tray thread
/// and the shell.
#[cfg(target_os = "linux")]
pub(crate) struct TrayRuntime {
    /// Title updates and shutdown go through the handle.
    handle: ksni::Handle<TrayState>,
    /// A StatusNotifier watcher answered when the tray
    /// started: the window may hide on close.
    live: Arc<AtomicBool>,
    /// Quit was chosen in the tray menu: closes are real.
    quit: Arc<AtomicBool>,
}

/// Non-Linux builds have no tray host, so the runtime is an
/// empty marker: state.rs still stores `Option<TrayRuntime>`,
/// and the shell's per-frame tray step is a no-op.
#[cfg(not(target_os = "linux"))]
pub(crate) struct TrayRuntime;

/// The tray item's state, owned by the ksni service thread.
#[cfg(target_os = "linux")]
struct TrayState {
    /// The window's egui context: menu actions show, focus
    /// and close the window from the tray thread.
    ctx: egui::Context,
    /// The unread count in the tray title.
    unread: u32,
    /// Shared with the app: Quit was chosen.
    quit: Arc<AtomicBool>,
}

#[cfg(target_os = "linux")]
impl TrayState {
    /// Shows the window again (the Open action, and a click
    /// on the tray icon itself).
    fn open(&self) {
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.ctx.request_repaint();
    }

    /// Quits for real (the Quit action): the shared flag
    /// tells the shell's close path not to intercept this
    /// close into a hide.
    fn quit(&self) {
        self.quit.store(true, Ordering::Relaxed);
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

#[cfg(target_os = "linux")]
impl ksni::Tray for TrayState {
    fn id(&self) -> String {
        "zaprust".to_owned()
    }

    fn title(&self) -> String {
        if self.unread > 0 {
            format!("Zaprust ({})", self.unread)
        } else {
            "Zaprust".to_owned()
        }
    }

    fn icon_name(&self) -> String {
        // The themed icon the desktop integration installs
        // (desktop_install.rs), resolved by the tray host.
        "zaprust".to_owned()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.open();
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{MenuItem, StandardItem};
        vec![
            StandardItem {
                label: "Open Zaprust".to_owned(),
                activate: Box::new(|tray: &mut Self| tray.open()),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".to_owned(),
                activate: Box::new(|tray: &mut Self| tray.quit()),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Whether a StatusNotifier watcher owns its bus name: a
/// tray host is running, so hiding the window has somewhere
/// real to go. Asked on the tray's start thread, never on
/// the UI thread.
#[cfg(target_os = "linux")]
fn watcher_present() -> bool {
    let Ok(conn) = zbus::blocking::Connection::session() else {
        return false;
    };
    let Ok(proxy) = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    ) else {
        return false;
    };
    ["org.kde.StatusNotifierWatcher", "org.freedesktop.StatusNotifierWatcher"]
        .iter()
        .any(|name| proxy.call::<_, _, bool>("NameHasOwner", &(name,)).unwrap_or(false))
}

#[cfg(target_os = "linux")]
impl App {
    /// The shell's per-frame tray step: starts or stops the
    /// service to match the setting, keeps the tray title's
    /// unread count current, and turns a window close into a
    /// hide while the tray is live and no real quit is in
    /// flight.
    pub(crate) fn tray_frame(&mut self, ctx: &egui::Context) {
        if self.tray_residence && self.tray.is_none() {
            let live = Arc::new(AtomicBool::new(false));
            let state = TrayState {
                ctx: ctx.clone(),
                unread: total_unread(&self.chats),
                quit: self.quit_requested.clone(),
            };
            let service = ksni::TrayService::new(state);
            let handle = service.handle();
            service.spawn();
            // The watcher check rides its own thread: a bus
            // round trip must never stall a frame. Until it
            // answers, the tray is not live and closes behave
            // normally.
            let live_probe = live.clone();
            std::thread::spawn(move || {
                live_probe.store(watcher_present(), Ordering::Relaxed);
            });
            self.tray_last_unread = total_unread(&self.chats);
            self.tray = Some(TrayRuntime { handle, live, quit: self.quit_requested.clone() });
        }
        if !self.tray_residence {
            if let Some(runtime) = self.tray.take() {
                runtime.handle.shutdown();
            }
            return;
        }
        let Some(runtime) = &self.tray else {
            return;
        };
        let unread = total_unread(&self.chats);
        if unread != self.tray_last_unread {
            self.tray_last_unread = unread;
            runtime.handle.update(|tray| tray.unread = unread);
        }
        if ctx.input(|i| i.viewport().close_requested())
            && !runtime.quit.load(Ordering::Relaxed)
            && runtime.live.load(Ordering::Relaxed)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl App {
    /// The shell's per-frame tray step on platforms with no
    /// tray host: nothing to start, update, or intercept, so
    /// the tray setting stays inert and closes behave
    /// normally. A native tray for these platforms is a
    /// later round.
    pub(crate) fn tray_frame(&mut self, _ctx: &egui::Context) {}
}
