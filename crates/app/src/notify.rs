//! Native desktop notifications for incoming messages.
//!
//! The decision half is pure and unit-tested: it reads the
//! existing Settings > Notifications toggles (messages, groups,
//! sound, preview), the per-chat mute flag, and whether the user
//! is already looking at the chat in a focused window. The firing
//! half hands the notification to the OS on a worker thread
//! (notify-rust); a missing notification daemon or a headless
//! session makes it a silent no-op, never an error the user sees.

use core::model::ChatKind;
use eframe::egui;

use crate::state::App;

/// One pending notification request, queued by the message-upsert
/// path for genuinely new incoming messages only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotifyRequest {
    /// App chat id the message landed in.
    pub chat_id: u64,
    /// Chat title for the notification heading.
    pub title: String,
    /// Chat kind, for the messages/groups toggle split.
    pub kind: ChatKind,
    /// Whether the chat is muted (per-chat silent mode).
    pub muted: bool,
    /// Sender display name for group messages, when known.
    pub sender: Option<String>,
    /// Message body (used only when the preview toggle is on).
    pub body: String,
}

/// The user's notification settings, copied out of App state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NotifySettings {
    pub messages: bool,
    pub groups: bool,
    pub sound: bool,
    pub preview: bool,
}

/// Decides whether a queued request becomes an OS notification.
/// `looking_at_it` is true when the chat is open in a focused
/// window, in which case a desktop popup would be noise.
pub(crate) fn should_notify(
    settings: NotifySettings,
    req: &NotifyRequest,
    looking_at_it: bool,
) -> bool {
    if looking_at_it || req.muted {
        return false;
    }
    match req.kind {
        ChatKind::Dm => settings.messages,
        ChatKind::Group | ChatKind::Channel => settings.groups,
    }
}

/// Composes the (summary, body) pair for the OS notification,
/// honoring the preview toggle: off means the message text never
/// leaves the app.
pub(crate) fn compose(settings: NotifySettings, req: &NotifyRequest) -> (String, String) {
    let summary = match (&req.sender, req.kind) {
        (Some(sender), ChatKind::Group) => format!("{sender} in {}", req.title),
        _ => req.title.clone(),
    };
    let body = if settings.preview && !req.body.is_empty() {
        req.body.clone()
    } else {
        "New message".to_owned()
    };
    (summary, body)
}

/// Fires one OS notification on a detached thread. Best effort:
/// every failure (no daemon, no session bus, unsupported platform
/// plumbing) is swallowed by design.
pub(crate) fn fire_os_notification(summary: String, body: String, sound: bool) {
    std::thread::Builder::new()
        .name("zaprust-notify".to_owned())
        .spawn(move || {
            let mut n = notify_rust::Notification::new();
            n.appname("Zaprust").summary(&summary).body(&body);
            if sound {
                n.sound_name("message-new-instant");
            }
            let _ = n.show();
        })
        .ok();
}

impl App {
    /// Drains queued notification requests (filled by the upsert
    /// path for new incoming messages) and fires the ones the
    /// settings allow. Called per frame with the egui context so
    /// focus can suppress popups for the chat on screen.
    pub(crate) fn drain_os_notifications(&mut self, ctx: &egui::Context) {
        if self.pending_notifications.is_empty() {
            return;
        }
        let settings = NotifySettings {
            messages: self.notify_messages,
            groups: self.notify_groups,
            sound: self.notify_sound,
            preview: self.notify_preview,
        };
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(false));
        let queued: Vec<NotifyRequest> = std::mem::take(&mut self.pending_notifications);
        for req in queued {
            let looking_at_it = focused && req.chat_id == self.selected_chat;
            if should_notify(settings, &req, looking_at_it) {
                let (summary, body) = compose(settings, &req);
                fire_os_notification(summary, body, settings.sound);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(kind: ChatKind) -> NotifyRequest {
        NotifyRequest {
            chat_id: 7,
            title: "Ada".to_owned(),
            kind,
            muted: false,
            sender: None,
            body: "hello there".to_owned(),
        }
    }

    fn settings() -> NotifySettings {
        NotifySettings { messages: true, groups: true, sound: true, preview: true }
    }

    #[test]
    fn toggles_gate_by_chat_kind() {
        let mut s = settings();
        assert!(should_notify(s, &req(ChatKind::Dm), false));
        s.messages = false;
        assert!(!should_notify(s, &req(ChatKind::Dm), false));
        assert!(should_notify(s, &req(ChatKind::Group), false));
        s.groups = false;
        assert!(!should_notify(s, &req(ChatKind::Group), false));
        assert!(!should_notify(s, &req(ChatKind::Channel), false));
    }

    #[test]
    fn muted_and_focus_suppress() {
        let mut r = req(ChatKind::Dm);
        r.muted = true;
        assert!(!should_notify(settings(), &r, false), "muted chats stay silent");
        assert!(
            !should_notify(settings(), &req(ChatKind::Dm), true),
            "the chat on screen in a focused window needs no popup"
        );
    }

    #[test]
    fn preview_toggle_controls_the_body() {
        let mut s = settings();
        let (_, body) = compose(s, &req(ChatKind::Dm));
        assert_eq!(body, "hello there");
        s.preview = false;
        let (_, body) = compose(s, &req(ChatKind::Dm));
        assert_eq!(body, "New message", "no message text leaves the app when preview is off");
    }

    #[test]
    fn group_summary_names_the_sender() {
        let mut r = req(ChatKind::Group);
        r.title = "Weekend Plans".to_owned();
        r.sender = Some("Mike".to_owned());
        let (summary, _) = compose(settings(), &r);
        assert_eq!(summary, "Mike in Weekend Plans");
    }
}
