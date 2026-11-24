//! Keyboard shortcuts (parity row 127): one static table is
//! the single source of truth for the handler here and for
//! the Settings > Shortcuts reference page, so what the page
//! lists is exactly what the app answers. Two entries are
//! reference-only: their handlers predate this table and live
//! where they always did (Ctrl+B in the shell, Ctrl+F in the
//! conversation); the table still lists them so the reference
//! surface is complete.
//!
//! Chords follow the house pattern of the two pre-existing
//! ones: they are global to the window. None of them types
//! text (every chord carries Ctrl, most carry Alt or Shift
//! as well), and the handler stands down while a modal is
//! open or the app is locked.

use crate::state::App;
use eframe::egui;

/// What a shortcut does when its chord lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShortcutAction {
    /// Open the new-chat dialog.
    NewChat,
    /// Select the next chat in the current list.
    NextChat,
    /// Select the previous chat in the current list.
    PrevChat,
    /// Archive or unarchive the open chat.
    ToggleArchive,
    /// Mute or unmute the open chat.
    ToggleMute,
    /// Mark the open chat as unread.
    MarkUnread,
    /// Lock the app now (needs a lock code).
    LockApp,
    /// Handled elsewhere (the shell / the conversation); the
    /// table lists it for the reference page only.
    ReferenceOnly,
}

/// One shortcut: its settings-page row id, its label key, its
/// chord in display form, and its action.
pub(crate) struct Shortcut {
    /// The settings registry row id.
    pub id: &'static str,
    /// The i18n key of the action's name.
    pub label_key: &'static str,
    /// The chord as the reference page prints it.
    pub chord: &'static str,
    /// What the chord does.
    pub action: ShortcutAction,
}

/// The shortcut table, in the reference page's order.
pub(crate) const SHORTCUTS: &[Shortcut] = &[
    Shortcut {
        id: "shortcut_new_chat",
        label_key: "shortcuts.new_chat",
        chord: "Ctrl+Alt+N",
        action: ShortcutAction::NewChat,
    },
    Shortcut {
        id: "shortcut_next_chat",
        label_key: "shortcuts.next_chat",
        chord: "Ctrl+Shift+]",
        action: ShortcutAction::NextChat,
    },
    Shortcut {
        id: "shortcut_prev_chat",
        label_key: "shortcuts.prev_chat",
        chord: "Ctrl+Shift+[",
        action: ShortcutAction::PrevChat,
    },
    Shortcut {
        id: "shortcut_archive",
        label_key: "shortcuts.archive",
        chord: "Ctrl+E",
        action: ShortcutAction::ToggleArchive,
    },
    Shortcut {
        id: "shortcut_mute",
        label_key: "shortcuts.mute",
        chord: "Ctrl+Shift+M",
        action: ShortcutAction::ToggleMute,
    },
    Shortcut {
        id: "shortcut_mark_unread",
        label_key: "shortcuts.mark_unread",
        chord: "Ctrl+Shift+U",
        action: ShortcutAction::MarkUnread,
    },
    Shortcut {
        id: "shortcut_search",
        label_key: "shortcuts.search_in_chat",
        chord: "Ctrl+F",
        action: ShortcutAction::ReferenceOnly,
    },
    Shortcut {
        id: "shortcut_sidebar",
        label_key: "shortcuts.toggle_sidebar",
        chord: "Ctrl+B",
        action: ShortcutAction::ReferenceOnly,
    },
    Shortcut {
        id: "shortcut_lock_app",
        label_key: "shortcuts.lock_app",
        chord: "Ctrl+Alt+L",
        action: ShortcutAction::LockApp,
    },
];

/// The egui key a chord ends in, by action.
fn key_for(action: ShortcutAction) -> Option<egui::Key> {
    match action {
        ShortcutAction::NewChat => Some(egui::Key::N),
        ShortcutAction::NextChat => Some(egui::Key::CloseBracket),
        ShortcutAction::PrevChat => Some(egui::Key::OpenBracket),
        ShortcutAction::ToggleArchive => Some(egui::Key::E),
        ShortcutAction::ToggleMute => Some(egui::Key::M),
        ShortcutAction::MarkUnread => Some(egui::Key::U),
        ShortcutAction::LockApp => Some(egui::Key::L),
        ShortcutAction::ReferenceOnly => None,
    }
}

/// The modifier shape a chord needs, by action: (alt, shift).
/// The command modifier (Ctrl, or Cmd on macOS) is always
/// required, matching the house chords.
fn mods_for(action: ShortcutAction) -> (bool, bool) {
    match action {
        ShortcutAction::NewChat | ShortcutAction::LockApp => (true, false),
        ShortcutAction::NextChat
        | ShortcutAction::PrevChat
        | ShortcutAction::ToggleMute
        | ShortcutAction::MarkUnread => (false, true),
        ShortcutAction::ToggleArchive | ShortcutAction::ReferenceOnly => (false, false),
    }
}

impl App {
    /// Answers this frame's shortcut chords. Called by the
    /// shell once per frame, after the backend polls and
    /// before the screens draw, and never while the app is
    /// locked (the shell draws the lock screen instead).
    pub(crate) fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.code_prompt.is_some() || self.ui_confirm.is_some() {
            return;
        }
        let fired = ctx.input(|i| {
            SHORTCUTS.iter().find_map(|s| {
                let key = key_for(s.action)?;
                let (alt, shift) = mods_for(s.action);
                (i.modifiers.command
                    && i.modifiers.alt == alt
                    && i.modifiers.shift == shift
                    && i.key_pressed(key))
                .then_some(s.action)
            })
        });
        let Some(action) = fired else { return };
        match action {
            ShortcutAction::NewChat => {
                self.new_chat.open = true;
            }
            ShortcutAction::NextChat => self.step_chat(1),
            ShortcutAction::PrevChat => self.step_chat(-1),
            ShortcutAction::ToggleArchive => {
                let id = self.selected_chat;
                if let Some(c) = self.chats.iter().find(|c| c.id == id) {
                    let (archived, pinned, muted) = (!c.archived, c.pinned, c.muted);
                    self.apply_chat_action(id, archived, pinned, muted);
                }
            }
            ShortcutAction::ToggleMute => {
                let id = self.selected_chat;
                if let Some(c) = self.chats.iter().find(|c| c.id == id) {
                    let (archived, pinned, muted) = (c.archived, c.pinned, !c.muted);
                    self.apply_chat_action(id, archived, pinned, muted);
                }
            }
            ShortcutAction::MarkUnread => {
                let id = self.selected_chat;
                self.mark_chat_unread(id);
            }
            ShortcutAction::LockApp => {
                if self.lock_code.is_some() {
                    self.app_locked_now = true;
                    self.code_entry.clear();
                    self.code_error = None;
                }
            }
            ShortcutAction::ReferenceOnly => {}
        }
    }

    /// Moves the selection one step through the current
    /// list (the visible chats, so the current screen and
    /// filter bound the walk), wrapping at the ends. A
    /// selection outside the list starts from the nearest
    /// end.
    fn step_chat(&mut self, delta: i32) {
        let ids: Vec<u64> = self.visible_chats().iter().map(|c| c.id).collect();
        if ids.is_empty() {
            return;
        }
        let next = match ids.iter().position(|id| *id == self.selected_chat) {
            Some(idx) => {
                let len = i32::try_from(ids.len()).unwrap_or(0);
                let idx = i32::try_from(idx).unwrap_or(0);
                ids[usize::try_from((idx + delta).rem_euclid(len.max(1))).unwrap_or(0)]
            }
            None => {
                if delta >= 0 {
                    ids[0]
                } else {
                    ids[ids.len() - 1]
                }
            }
        };
        self.select_chat_now(next);
    }
}
