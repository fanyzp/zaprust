//! Native desktop notifications for incoming messages.
//!
//! The decision half is pure and unit-tested: it reads the
//! existing Settings > Notifications toggles (messages, groups,
//! sound, preview), the per-chat mute flag, and whether the user
//! is already looking at the chat in a focused window. Two gates
//! keep the past quiet: messages stamped before the current
//! connection came up are history and never queue at all (the
//! gate itself lives here, the queueing call site in live.rs),
//! and whatever survives a drain is coalesced to at most one
//! popup per chat plus a hard cap. The firing half hands popups
//! to a single worker thread (notify-rust) over a bounded
//! channel; a missing notification daemon or a headless session
//! makes it a silent no-op, never an error the user sees.

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

/// Whether a message counts as history for notification
/// purposes: it was stamped before the current connection came
/// up (`connected_at`, unix seconds), so it reached this client
/// as first-sync or offline catch-up rather than live. Nothing
/// is live before the first connect completes, so a missing
/// stamp counts as history too. A live message that merely
/// arrives while a backlog drains carries a current stamp and
/// is NOT history: the gate reads the message's own clock, not
/// the queue's depth.
pub(crate) fn is_history(message_ts: i64, connected_at: Option<i64>) -> bool {
    match connected_at {
        Some(connected) => message_ts < connected,
        None => true,
    }
}

/// Most popups one drain may fire. Per-chat coalescing already
/// keeps the count at one per chat; this cap bounds even the
/// pathological case of many chats landing in the same frame,
/// folding the overflow into a single combined summary.
pub(crate) const MAX_POPUPS_PER_DRAIN: usize = 3;

/// Turns a drained request queue into the popups to fire:
/// settings filtering first (a muted or toggled-off chat never
/// contributes, not even to a count), then one popup per chat
/// in first-appearance order, a per-chat count summary when
/// several of its messages landed together, and one combined
/// summary for whatever exceeds [`MAX_POPUPS_PER_DRAIN`].
pub(crate) fn plan_popups(
    settings: NotifySettings,
    requests: &[NotifyRequest],
    looking_at: Option<u64>,
) -> Vec<(String, String)> {
    let mut groups: Vec<(&NotifyRequest, usize)> = Vec::new();
    for req in requests {
        if !should_notify(settings, req, looking_at == Some(req.chat_id)) {
            continue;
        }
        match groups.iter_mut().find(|(rep, _)| rep.chat_id == req.chat_id) {
            Some((_, count)) => *count += 1,
            None => groups.push((req, 1)),
        }
    }
    let (head, tail) = if groups.len() > MAX_POPUPS_PER_DRAIN {
        groups.split_at(MAX_POPUPS_PER_DRAIN - 1)
    } else {
        (&groups[..], &groups[..0])
    };
    let mut popups: Vec<(String, String)> =
        head.iter().map(|(rep, count)| popup_for(settings, rep, *count)).collect();
    if !tail.is_empty() {
        let messages: usize = tail.iter().map(|(_, count)| count).sum();
        popups.push((
            "Zaprust".to_owned(),
            format!("{messages} new messages in {} chats", tail.len()),
        ));
    }
    popups
}

/// The popup for one chat's surviving requests: the composed
/// single-message notification when there is one, a count
/// summary when several landed together. The summary carries
/// no message text, so it is safe with the preview toggle off
/// and on alike.
fn popup_for(settings: NotifySettings, rep: &NotifyRequest, count: usize) -> (String, String) {
    if count == 1 {
        compose(settings, rep)
    } else {
        (rep.title.clone(), format!("{count} new messages"))
    }
}

/// Hands one popup to the single notification worker: a lazily
/// started thread fed by a bounded channel, replacing a fresh
/// thread per popup. Best effort: the drain already coalesces
/// and caps what arrives here, the channel bound is the second
/// line of defence, a full channel drops the popup, and every
/// OS-side failure (no daemon, no session bus, unsupported
/// platform plumbing) is swallowed by design. A worker that
/// died (channel disconnected) is replaced once per call.
pub(crate) fn fire_os_notification(summary: String, body: String, sound: bool) {
    type Popup = (String, String, bool);
    static SENDER: std::sync::OnceLock<
        std::sync::Mutex<Option<std::sync::mpsc::SyncSender<Popup>>>,
    > = std::sync::OnceLock::new();
    let cell = SENDER.get_or_init(|| std::sync::Mutex::new(None));
    let mut slot = cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(spawn_notify_worker());
    }
    let sender = slot.as_ref().expect("the slot was just filled").clone();
    match sender.try_send((summary, body, sound)) {
        Ok(()) => {}
        Err(std::sync::mpsc::TrySendError::Disconnected(popup)) => {
            let replacement = spawn_notify_worker();
            let _ = replacement.try_send(popup);
            *slot = Some(replacement);
        }
        Err(std::sync::mpsc::TrySendError::Full(_)) => {}
    }
}

/// Spawns the notification worker thread and returns its
/// channel. If the OS refuses the thread, the receiver drops
/// with the failed closure and sends against the channel fail
/// disconnected, which the caller treats as best-effort loss.
fn spawn_notify_worker() -> std::sync::mpsc::SyncSender<(String, String, bool)> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<(String, String, bool)>(16);
    std::thread::Builder::new()
        .name("zaprust-notify".to_owned())
        .spawn(move || {
            while let Ok((summary, body, sound)) = rx.recv() {
                let mut n = notify_rust::Notification::new();
                n.appname("Zaprust").summary(&summary).body(&body);
                if sound {
                    n.sound_name("message-new-instant");
                }
                let _ = n.show();
            }
        })
        .ok();
    tx
}

impl App {
    /// Drains queued notification requests (filled by the upsert
    /// path for new incoming messages) and fires the popups the
    /// settings allow, coalesced by [`plan_popups`]: one popup
    /// per chat at most, never a storm. Called per frame with
    /// the egui context so focus can suppress popups for the
    /// chat on screen.
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
        let looking_at = focused.then_some(self.selected_chat);
        let queued: Vec<NotifyRequest> = std::mem::take(&mut self.pending_notifications);
        for (summary, body) in plan_popups(settings, &queued, looking_at) {
            fire_os_notification(summary, body, settings.sound);
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

    #[test]
    fn history_gate_compares_against_connect_time() {
        assert!(is_history(1_000, Some(2_000)), "stamped before connect: catch-up");
        assert!(!is_history(2_000, Some(2_000)), "stamped at connect: live");
        assert!(!is_history(3_000, Some(2_000)), "stamped after connect: live");
        assert!(is_history(3_000, None), "no connect yet: nothing is live");
    }

    #[test]
    fn single_message_popup_is_the_composed_one() {
        let r = req(ChatKind::Dm);
        let popups = plan_popups(settings(), std::slice::from_ref(&r), None);
        assert_eq!(popups, vec![compose(settings(), &r)], "one live message, one popup, same text");
    }

    #[test]
    fn burst_in_one_chat_coalesces_to_a_count() {
        let reqs: Vec<NotifyRequest> = (0..5)
            .map(|i| {
                let mut r = req(ChatKind::Dm);
                r.body = format!("message {i}");
                r
            })
            .collect();
        let popups = plan_popups(settings(), &reqs, None);
        assert_eq!(popups, vec![("Ada".to_owned(), "5 new messages".to_owned())]);
    }

    #[test]
    fn muted_and_open_chats_never_contribute() {
        let mut muted = req(ChatKind::Dm);
        muted.chat_id = 9;
        muted.muted = true;
        let mut open = req(ChatKind::Dm);
        open.chat_id = 11;
        let popups = plan_popups(settings(), &[muted, open], Some(11));
        assert!(popups.is_empty(), "muted and on-screen chats stay silent, counts included");
    }

    #[test]
    fn many_chats_fold_into_the_cap() {
        let reqs: Vec<NotifyRequest> = (0..6)
            .map(|i| {
                let mut r = req(ChatKind::Dm);
                r.chat_id = 100 + i;
                r.title = format!("Chat {i}");
                r
            })
            .collect();
        let popups = plan_popups(settings(), &reqs, None);
        assert_eq!(popups.len(), MAX_POPUPS_PER_DRAIN, "the cap holds however wide the burst");
        assert_eq!(popups[0].0, "Chat 0", "first chats keep their own popups, in order");
        assert_eq!(popups[1].0, "Chat 1");
        assert_eq!(popups[2].1, "4 new messages in 4 chats", "the overflow folds into one summary");
    }
}
