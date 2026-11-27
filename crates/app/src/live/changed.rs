//! Server-reported chat and message state adoption: the
//! apply_*_changed family and its shared write-through, chat
//! deletion and clearing adoption, and the matching restores.

use super::*;

impl crate::state::App {
    /// Applies a disappearing-timer change for one chat, from the
    /// local dock action or from a backend DisappearingChanged
    /// event (a change made on another device, or by the other
    /// participant). State and archive update either way; the
    /// in-conversation system notice is added only when the value
    /// actually changes, which is what keeps the echo of our own
    /// change from announcing itself twice.
    /// Applies `f` to a chat's row in the list mirror and to
    /// the detached slot when it holds the same chat: the two
    /// places a chat's display state lives, which every
    /// server-reported chat change below must land on together
    /// or the open conversation and the list would disagree.
    pub(super) fn mutate_mirror_chat(&mut self, chat_id: u64, f: impl Fn(&mut Chat)) {
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            f(c);
        }
        if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            f(c);
        }
    }

    pub(crate) fn apply_disappearing_changed(&mut self, chat_id: u64, duration_secs: u32) {
        let current =
            self.chats.iter().find(|c| c.id == chat_id).map(|c| c.ephemeral_secs).unwrap_or(0);
        self.set_ephemeral_state(chat_id, duration_secs);
        if current != duration_secs {
            self.push_system_notice(
                chat_id,
                &crate::extras::disappearing_notice_text(duration_secs),
            );
        }
    }

    /// Sets a chat's disappearing-timer state (mirror and
    /// archive) without any in-stream announcement. The state
    /// half of [`App::apply_disappearing_changed`], shared with
    /// the DisappearingTimerFailed rollback, which must restore
    /// the previous timer quietly: the announcement the
    /// optimistic apply posted belongs to the failed change,
    /// and a second one would claim a change that never
    /// happened.
    pub(crate) fn set_ephemeral_state(&mut self, chat_id: u64, duration_secs: u32) {
        self.mutate_mirror_chat(chat_id, |c| c.ephemeral_secs = duration_secs);
        self.persist_chat_row(chat_id);
    }

    /// Applies a mute change the server reported (a mute or
    /// unmute done on another device, or the account state from
    /// a full sync). The reported value wins over the local
    /// mirror unconditionally, including over an optimistic
    /// local toggle whose backend command failed: this event is
    /// the reconciliation path for mute, the counterpart of
    /// DisappearingChanged for the timer. State and archive
    /// update either way. A mute set from this client does not
    /// wait for this event; it applies through apply_chat_action
    /// at once.
    pub(crate) fn apply_mute_changed(&mut self, chat_id: u64, muted: bool, mute_until: i64) {
        // The end counts only while the chat is muted; a past
        // end with muted = false is how a lapsed timed mute
        // reconciles, and the field clears with the flag.
        let until =
            if muted && mute_until > 0 { u64::try_from(mute_until).unwrap_or(0) } else { 0 };
        self.mutate_mirror_chat(chat_id, |c| {
            c.muted = muted;
            c.mute_until = until;
        });
        self.persist_chat_row(chat_id);
    }

    /// Applies a pin change the server reported (a pin or unpin
    /// done on another device, or the account state from a full
    /// sync). The reported value wins over the local mirror
    /// unconditionally, including over an optimistic local
    /// toggle whose backend command failed: this event is the
    /// reconciliation path for pin, the counterpart of
    /// MuteChanged for mute. State and archive update either
    /// way; the pinned-first list order reads the field at
    /// render time, so no re-sort is needed here. A pin set
    /// from this client does not wait for this event; it
    /// applies through apply_chat_action at once.
    pub(crate) fn apply_pin_changed(&mut self, chat_id: u64, pinned: bool) {
        // The pin drives the pinned-first partition of every
        // filtered view, so a flip is a roster change (qc1644);
        // a report restating the current pin is not.
        let flipped =
            self.chats.iter().find(|c| c.id == chat_id).is_some_and(|c| c.pinned != pinned);
        self.mutate_mirror_chat(chat_id, |c| c.pinned = pinned);
        if flipped {
            if let Some(session) = &mut self.live {
                session.note_roster_change();
            }
        }
        self.persist_chat_row(chat_id);
    }

    /// Applies an archive change the server reported (an
    /// archive or unarchive done on another device, or the
    /// account state from a full sync). The reported value wins
    /// over the local mirror unconditionally, including over an
    /// optimistic local toggle whose backend command failed:
    /// this event is the reconciliation path for archive, the
    /// counterpart of MuteChanged for mute. State and archive
    /// update either way; the list filters read the field at
    /// render time, so an archived chat leaves the main list
    /// views (and an unarchived one returns) exactly as with a
    /// local archive action. An archive set from this client
    /// does not wait for this event; it applies through
    /// apply_chat_action at once.
    pub(crate) fn apply_archive_changed(&mut self, chat_id: u64, archived: bool) {
        // The archived flag decides which views the chat
        // belongs to, so a flip is a roster change (qc1644);
        // a report restating the current state is not.
        let flipped =
            self.chats.iter().find(|c| c.id == chat_id).is_some_and(|c| c.archived != archived);
        self.mutate_mirror_chat(chat_id, |c| c.archived = archived);
        if flipped {
            if let Some(session) = &mut self.live {
                session.note_roster_change();
            }
        }
        self.persist_chat_row(chat_id);
    }

    /// Applies a confirmed lock-state change (own echo,
    /// another device, or the link-time import) to the mirror
    /// and the archive: the one writer for the flag outside
    /// the optimistic apply and its rollback.
    pub(crate) fn apply_lock_changed(&mut self, chat_id: u64, locked: bool) {
        // The locked flag decides which views the chat belongs
        // to, so a flip is a roster change (qc1644); a report
        // restating the current state is not.
        let flipped =
            self.chats.iter().find(|c| c.id == chat_id).is_some_and(|c| c.locked != locked);
        self.mutate_mirror_chat(chat_id, |c| c.locked = locked);
        if flipped {
            if let Some(session) = &mut self.live {
                session.note_roster_change();
            }
        }
        self.persist_chat_row(chat_id);
    }

    /// Puts the lock flag back after a lock action failed:
    /// the change never reached the server, so the snapshot
    /// value is the truth again, in the mirror and the
    /// archive alike (section 27).
    pub(crate) fn restore_lock_action(&mut self, chat_id: u64, was_locked: bool) {
        // A rollback that flips the flag back is a roster
        // change like the change it undoes (qc1644).
        let mut flipped = false;
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            flipped = c.locked != was_locked;
            c.locked = was_locked;
        }
        if flipped {
            if let Some(session) = &mut self.live {
                session.note_roster_change();
            }
        }
        if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            c.locked = was_locked;
        }
        self.persist_chat_row(chat_id);
    }

    /// Applies a star change the server reported (a star or
    /// unstar done on another device, or the account state from
    /// a full sync). The reported value wins over the local
    /// mirror unconditionally, exactly as a local toggle would
    /// land it, minus the backend command: the session mirror
    /// flips, and the flag write-through (`set_live_flag`)
    /// updates the session flag map and the archive row, which
    /// is also what protects the value from uninformed arrival
    /// upserts (they resolve their flags from that map). A star
    /// set from this client does not wait for this event; it
    /// applies through the composer's toggle at once.
    pub(crate) fn apply_star_changed(
        &mut self,
        chat_id: u64,
        msg_id: u64,
        protocol_id: &str,
        starred: bool,
    ) {
        // The protocol-id bookkeeping an upsert would have done,
        // so the flag write-through below finds the archive row
        // even for a message this session has not re-upserted.
        self.protocol_ids.insert(msg_id, protocol_id.to_owned());
        // A server report settles any star change of mine still
        // pending for this message: the reported state is the
        // state now, whether it confirms my change or supersedes
        // it, so nothing remains to roll back.
        self.pending_stars.remove(&msg_id);
        if let Some(entry) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            for (_, m) in entry.iter_mut() {
                if m.id == msg_id {
                    m.starred = starred;
                }
            }
        }
        self.set_live_flag(chat_id, msg_id, store::FLAG_STARRED, starred);
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Restores the pin state a refused pin change started
    /// from (see [`BackendEvent::PinFailed`]): the mirror flips
    /// back and the flag write-through updates the session flag
    /// map and the archive row, exactly how `apply_star_changed`
    /// lands a star rollback, minus any backend command (the
    /// change being undone never reached the server).
    pub(crate) fn apply_pin_restored(
        &mut self,
        chat_id: u64,
        msg_id: u64,
        protocol_id: &str,
        pinned: bool,
    ) {
        // The protocol-id bookkeeping an upsert would have done,
        // so the flag write-through below finds the archive row.
        self.protocol_ids.insert(msg_id, protocol_id.to_owned());
        if let Some(entry) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            for (_, m) in entry.iter_mut() {
                if m.id == msg_id {
                    m.pinned = pinned;
                }
            }
        }
        self.set_live_flag(chat_id, msg_id, store::FLAG_PINNED_MESSAGE, pinned);
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Applies a pin state the server reported (a pin made on
    /// another device, or by another member): the mirror flips
    /// and the flag write-through persists it, the same landing
    /// a rollback takes. Nothing of mine is in flight for
    /// someone else's pin, so no pending entry is involved.
    pub(super) fn apply_server_pin_changed(
        &mut self,
        chat_jid: &str,
        protocol_id: &str,
        pinned: bool,
    ) {
        let chat_id = jid_id(chat_jid);
        // The message is in this chat's rows: walk them
        // (bounded by the retention window) and check each
        // row's binding, instead of scanning the whole binding
        // map, which holds an entry per filed message.
        let msg_id = self.live.as_ref().and_then(|s| {
            s.messages.get(&chat_id).and_then(|rows| {
                rows.iter()
                    .find(|(_, m)| self.protocol_ids.get(&m.id).is_some_and(|p| p == protocol_id))
                    .map(|(_, m)| m.id)
            })
        });
        let Some(msg_id) = msg_id else {
            return;
        };
        if let Some(entry) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            for (_, m) in entry.iter_mut() {
                if m.id == msg_id {
                    m.pinned = pinned;
                }
            }
        }
        self.set_live_flag(chat_id, msg_id, store::FLAG_PINNED_MESSAGE, pinned);
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Applies a keep change for a disappearing message, from
    /// any source: my own toggle, a MessageKept report (a keep
    /// made on another device), or a KeepFailed rollback. One
    /// path for all three, like apply_star_changed: the mirror
    /// flips and the flag write-through persists it, which is
    /// also what protects the value from uninformed arrival
    /// upserts. A server report settles any pending change of
    /// mine for the message.
    pub(crate) fn apply_keep_changed(
        &mut self,
        chat_id: u64,
        msg_id: u64,
        protocol_id: &str,
        kept: bool,
    ) {
        self.protocol_ids.insert(msg_id, protocol_id.to_owned());
        self.pending_keeps.remove(&msg_id);
        let mut expires_at = 0;
        if let Some(entry) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            for (_, m) in entry.iter_mut() {
                if m.id == msg_id {
                    m.kept = kept;
                    expires_at = m.expires_at;
                }
            }
        }
        // The ephemeral index follows the keep (qc1644): a
        // kept message leaves it, an un-kept one rejoins it.
        // The sweep also verifies against the mirror, so this
        // is promptness, not the only line of defense.
        if let Some(session) = &mut self.live {
            if kept {
                session.drop_ephemeral_entry(chat_id, msg_id, expires_at);
            } else {
                session.seed_ephemeral(chat_id, msg_id, expires_at);
            }
        }
        self.set_live_flag(chat_id, msg_id, store::FLAG_KEPT, kept);
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Writes one chat's archive row from the mirror, so the
    /// stored row carries the state the list shows. For mirror
    /// changes that arrive without a chat upsert of their own
    /// (the unread badge an incoming message raised), whose own
    /// write-through would otherwise never happen. Inside an
    /// event batch the write defers to the flush, which writes
    /// the row from the mirror's final state in the batch's one
    /// transaction instead of one commit per event (qc1634 H2).
    pub(crate) fn persist_chat_row(&mut self, chat_id: u64) {
        if self.event_batch.active {
            self.event_batch.chat_rows_dirty.insert(chat_id);
            return;
        }
        if let Some(session) = &self.live {
            if let (Some(store), Some(jid)) = (&session.store, session.jids.get(&chat_id)) {
                if let Some(chat) = self.chats.iter().find(|c| c.id == chat_id) {
                    let ts = session.times.get(&chat_id).copied().unwrap_or(0);
                    let _ = store.upsert_chat(&chat_to_live(chat, jid, ts));
                }
            }
        }
    }

    /// Applies a read-state change the server reported (a chat
    /// marked read or unread on another device, or the account
    /// state from a full sync). State adoption only, in both
    /// directions: the mirror and the archive take the reported
    /// state, and nothing is ever sent from here. A remote read
    /// in particular must not produce read receipts from this
    /// client (the other device already sent them), so the
    /// chat's pending receipt queue is dropped, not flushed:
    /// those messages are read, and a later local engagement
    /// finds nothing left to mark. A remote mark-as-unread
    /// adopts the reported count, floored at one: the chat IS
    /// unread, so a zero badge would hide the state the server
    /// just asserted.
    /// App-state arrivals are sync activity too (QC1614-A0-5
    /// W3): while a catch-up is running they hold the sync row,
    /// exactly as a history chunk does.
    pub(crate) fn stamp_sync_activity(&mut self) {
        if let Some(session) = &mut self.live {
            if session.syncing {
                session.sync_last_activity = Some(std::time::Instant::now());
            }
        }
    }

    pub(crate) fn apply_chat_read_changed(&mut self, chat_id: u64, read: bool, unread_count: u32) {
        let unread = if read { 0 } else { unread_count.max(1) };
        // The unread count is the Unread filter's input, so a
        // count that actually moves is a roster change
        // (qc1644): a chat read elsewhere must leave the
        // Unread view, and one marked unread must enter it.
        let moved = self.chats.iter().find(|c| c.id == chat_id).is_some_and(|c| c.unread != unread);
        self.mutate_mirror_chat(chat_id, |c| c.unread = unread);
        if moved {
            if let Some(session) = &mut self.live {
                session.note_roster_change();
            }
        }
        if read {
            self.unread_live.remove(&chat_id);
        }
        self.persist_chat_row(chat_id);
    }

    /// Applies a chat deletion the server reported: the chat
    /// was deleted on the phone, or my own DeleteChat command
    /// was confirmed back (section 24: a whole-chat removal is
    /// destructive, so its local apply waits for the outcome;
    /// a DeleteChatFailed leaves the chat fully in place). The
    /// adoption is the removal itself, everywhere the chat
    /// exists locally: the chat list, the detached slot, the
    /// session mirror and its per-message helper entries, the
    /// pending receipt queue (receipts for a deleted chat must
    /// never go out), the parked composer state (a chat that
    /// later reappears under the same JID starts clean), and
    /// the archive rows. The event's own JID keys the archive
    /// deletion, so a chat that exists only in the archive is
    /// removed too. If the deleted chat was open, the selection
    /// moves to the first remaining chat, as a live start does.
    pub(crate) fn apply_chat_deleted(&mut self, chat_id: u64, chat_jid: &str) {
        // Per-message helper entries go first, while the mirror
        // still lists the messages they belong to.
        let helpers: Vec<(u64, Option<String>)> = self
            .live
            .as_ref()
            .and_then(|s| s.messages.get(&chat_id))
            .map(|msgs| {
                msgs.iter().map(|(_, m)| (m.id, self.protocol_ids.get(&m.id).cloned())).collect()
            })
            .unwrap_or_default();
        for (msg_id, protocol_id) in helpers {
            self.forget_live_message(chat_id, msg_id, protocol_id.as_deref());
        }
        if let Some(session) = &mut self.live {
            session.messages.remove(&chat_id);
            session.invalidate_msg_index(chat_id);
            session.jids.remove(&chat_id);
            session.times.remove(&chat_id);
            session.engaged_chats.remove(&chat_id);
            if let Some(store) = &session.store {
                let _ = store.delete_chat(chat_jid);
            }
        }
        // Media the user saved from this chat sits in the
        // downloads folder as plaintext files of their own: the
        // chat's deletion takes them too, or the messages would
        // survive their own deletion indefinitely (privacy
        // audit A3). Chat exports are not touched: an export is
        // a document the user deliberately created, and the
        // Storage page already discloses that exports are not
        // encrypted.
        backend::remove_downloads_for_chat(&backend::data_dir(), chat_jid);
        self.unread_live.remove(&chat_id);
        self.live_typing.remove(&chat_id);
        self.pending_notifications.retain(|n| n.chat_id != chat_id);
        self.chats.retain(|c| c.id != chat_id);
        if self.detached_chat.as_ref().is_some_and(|c| c.id == chat_id) {
            self.detached_chat = None;
        }
        self.composer.chat_drafts.remove(&chat_id);
        self.composer.chat_parked.remove(&chat_id);
        if self.selected_chat == chat_id {
            let next = self.chats.first().map_or(0, |c| c.id);
            self.switch_composer_chat(chat_id, next);
            // The switch parked the deleted chat's composer
            // state under its id; a deleted chat keeps nothing.
            self.composer.chat_drafts.remove(&chat_id);
            self.composer.chat_parked.remove(&chat_id);
            self.selected_chat = next;
            self.sync_conversation();
        }
        // The deleted chat's draft leaves the persisted set
        // with the chat, or a restart would resurrect it.
        self.persist_settings();
    }

    /// Applies a chat clearing the server reported (the chat
    /// was cleared on the phone). This client has no
    /// clear-chat action of its own, so there is no local
    /// behavior to mirror and nothing is sent back; the
    /// adoption is the removal of the cleared messages,
    /// everywhere they live locally: the session mirror, the
    /// per-message helper entries, and the archive rows. The
    /// chat itself stays: its row, its recorded activity stamp
    /// (its list position), and its settings (pin, mute,
    /// archive, disappearing timer) are exactly as they were,
    /// and its unread state is untouched (clearing asserts
    /// nothing about read state; a read change arrives as its
    /// own sync event). The clear's own terms are honored:
    /// messages stamped after the bound are not the clear's to
    /// remove, and starred messages survive unless the
    /// clearing device deleted them too. The event's own JID
    /// keys the archive deletion, so rows the mirror does not
    /// hold are cleared too.
    pub(crate) fn apply_chat_cleared(
        &mut self,
        chat_id: u64,
        chat_jid: &str,
        delete_starred: bool,
        last_message_timestamp: Option<i64>,
    ) {
        // Collect the doomed messages first (app id + protocol
        // id), while the mirror still lists them: the helper
        // cleanup below needs both.
        let doomed: Vec<(u64, Option<String>)> = self
            .live
            .as_ref()
            .and_then(|s| s.messages.get(&chat_id))
            .map(|msgs| {
                msgs.iter()
                    .filter(|(ts, m)| {
                        last_message_timestamp.is_none_or(|bound| *ts <= bound)
                            && (delete_starred || !m.starred)
                    })
                    .map(|(_, m)| (m.id, self.protocol_ids.get(&m.id).cloned()))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(session) = &mut self.live {
            if let Some(msgs) = session.messages.get_mut(&chat_id) {
                let doomed_ids: std::collections::HashSet<u64> =
                    doomed.iter().map(|(id, _)| *id).collect();
                msgs.retain(|(_, m)| !doomed_ids.contains(&m.id));
            }
            // Positions shifted: the chat's index rebuilds on
            // its next lookup.
            session.invalidate_msg_index(chat_id);
        }
        if let Some(session) = &self.live {
            if let Some(store) = &session.store {
                if !delete_starred {
                    // One source of truth for "starred" at
                    // clear time (P2-43): the store clear spares
                    // rows by their ARCHIVE flags, so a star
                    // known only from a live upsert (held in the
                    // session flag map, not yet persisted) is
                    // flushed into the archive first. Without
                    // this, the mirror kept a message the
                    // archive deleted.
                    let starred: Vec<(String, i64)> = session
                        .messages
                        .get(&chat_id)
                        .map(|msgs| {
                            msgs.iter()
                                .filter(|(_, m)| m.starred)
                                .filter_map(|(_, m)| {
                                    let pid = self.protocol_ids.get(&m.id)?;
                                    let flags = session.flags.get(&m.id).copied().unwrap_or(0)
                                        | store::FLAG_STARRED;
                                    Some((pid.clone(), flags))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    for (pid, flags) in starred {
                        let _ = store.set_message_flags(chat_jid, &pid, flags);
                    }
                }
                let _ =
                    store.delete_chat_messages(chat_jid, !delete_starred, last_message_timestamp);
            }
        }
        for (msg_id, protocol_id) in &doomed {
            self.forget_live_message(chat_id, *msg_id, protocol_id.as_deref());
            // A cleared message's saved download goes with it
            // (privacy audit A3); kept (starred) messages are
            // not in `doomed`, so their downloads stay, exactly
            // like their archive rows.
            if let Some(protocol_id) = protocol_id {
                backend::remove_download_for_message(&backend::data_dir(), chat_jid, protocol_id);
            }
        }
        if !doomed.is_empty() {
            self.refresh_preview_after_removal(chat_id);
            if self.selected_chat == chat_id {
                self.sync_conversation();
            }
        }
    }

    /// Applies a delete-for-me: the server reported it (one
    /// message deleted for the account on the phone), or my own
    /// delete-for-me action applied it locally ahead of its
    /// command (section 24, the revoke precedent). Nothing is
    /// sent back from here; the adoption is the removal of
    /// exactly that
    /// message: from the session mirror, from its per-message
    /// helper entries, and from the archive. The store
    /// deletion is keyed by the event's own JID and protocol
    /// ID, so a row the mirror does not hold is removed too.
    /// Neighbouring messages, the chat, and its settings are
    /// untouched.
    pub(crate) fn apply_message_deleted_for_me(
        &mut self,
        chat_id: u64,
        chat_jid: &str,
        msg_id: u64,
        protocol_id: &str,
    ) {
        let mut removed_newest = false;
        if let Some(msgs) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            if let Some(pos) = msgs.iter().position(|(_, m)| m.id == msg_id) {
                removed_newest = pos + 1 == msgs.len();
                msgs.remove(pos);
            }
        }
        if let Some(session) = &self.live {
            if let Some(store) = &session.store {
                let _ = store.delete_message(chat_jid, protocol_id);
            }
        }
        // The message's saved download, if any, goes with it
        // (privacy audit A3).
        backend::remove_download_for_message(&backend::data_dir(), chat_jid, protocol_id);
        self.forget_live_message(chat_id, msg_id, Some(protocol_id));
        if removed_newest {
            self.refresh_preview_after_removal(chat_id);
        }
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Rolls a chat action back after a ChatActionFailed: the
    /// fields the failed action owns (archive, pin, or the mute
    /// pair) return to the snapshot taken when the action was
    /// sent, in the mirror and the archive alike. Fields other
    /// actions own are untouched, so a settled sibling action
    /// keeps its state.
    pub(crate) fn restore_chat_action(
        &mut self,
        chat_id: u64,
        group: &str,
        snapshot: (bool, bool, bool, i64),
    ) {
        let (archived, pinned, muted, mute_until) = snapshot;
        let until = u64::try_from(mute_until).unwrap_or(0);
        // The archive and pin groups own filter inputs, so a
        // rollback that flips one back is a roster change like
        // the change it undoes (qc1644); the mute group owns
        // none.
        let mut flipped = false;
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            match group {
                "archive" => {
                    flipped = c.archived != archived;
                    c.archived = archived;
                }
                "pin" => {
                    flipped = c.pinned != pinned;
                    c.pinned = pinned;
                }
                _ => {
                    c.muted = muted;
                    c.mute_until = until;
                }
            }
        }
        if flipped {
            if let Some(session) = &mut self.live {
                session.note_roster_change();
            }
        }
        if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            match group {
                "archive" => c.archived = archived,
                "pin" => c.pinned = pinned,
                _ => {
                    c.muted = muted;
                    c.mute_until = until;
                }
            }
        }
        self.persist_chat_row(chat_id);
    }

    /// Puts a message back after a DeleteForMeFailed: the
    /// delete never reached the server, so the message is still
    /// the account's. Its archived row is written back first;
    /// then the session mirror re-adopts it with the helper
    /// entries its upsert would have built (media refs,
    /// thumbnails, mentions, flags, protocol bookkeeping), the
    /// row preview is recomputed, and the open conversation
    /// re-syncs. The message's saved download, removed by the
    /// optimistic apply, is not restored: it is a local copy of
    /// media the message still names, fetchable again.
    pub(crate) fn restore_deleted_for_me(
        &mut self,
        chat_id: u64,
        row: &backend::LiveMessage,
        flags: i64,
    ) {
        if let Some(session) = &self.live {
            if let Some(store) = &session.store {
                let _ = store.upsert_message(row, flags);
            }
        }
        let msg_id = message_id(&row.chat_jid, &row.id);
        if let Some(session) = &mut self.live {
            if let Some(reference) = &row.photo_ref {
                session.photo_refs.insert(msg_id, reference.clone());
            }
            if let Some(reference) = &row.voice_ref {
                session.voice_refs.insert(msg_id, reference.clone());
            }
            if let Some(reference) = &row.video_ref {
                session.video_refs.insert(msg_id, reference.clone());
            }
            if let Some(thumb) = &row.video_thumbnail {
                session.put_video_thumb(msg_id, thumb.clone());
            }
            if let Some(reference) = &row.poster_ref {
                session.poster_refs.insert(msg_id, reference.clone());
            }
            if let (Some(w), Some(h)) = (row.video_width, row.video_height) {
                session.video_dims.insert(msg_id, (w, h));
            }
            if let Some(reference) = &row.sticker_ref {
                session.sticker_refs.insert(msg_id, reference.clone());
            }
            if let Some(thumb) = &row.photo_thumbnail {
                session.put_photo_thumb(msg_id, thumb.clone());
            }
            if let Some(reference) = &row.document_ref {
                session.document_refs.insert(msg_id, reference.clone());
            }
            if let Some(thumb) = &row.document_thumbnail {
                session.document_thumbs.insert(msg_id, thumb.clone());
            }
            if row.gif_playback {
                session.gif_videos.insert(msg_id);
            }
            if row.sticker_animated || row.sticker_lottie {
                session.sticker_anim.insert(msg_id, (row.sticker_animated, row.sticker_lottie));
            }
            if let Some(thumb) = &row.sticker_thumbnail {
                session.sticker_thumbs.insert(msg_id, thumb.clone());
            }
            if let Some(thumb) = &row.location_thumbnail {
                session.location_thumbs.insert(msg_id, thumb.clone());
            }
            if let Some(thumb) = row.link_preview.as_ref().and_then(|p| p.thumbnail.as_ref()) {
                session.preview_thumbs.insert(msg_id, thumb.clone());
            }
            if row.view_once {
                session.view_once_ids.insert(msg_id);
            }
            if let (Some(jid), Some(name)) = (&row.sender_jid, &row.sender_verified_name) {
                session.verified_names.insert(jid.clone(), name.clone());
                session.verified_checked.insert(jid.clone());
            }
            if !row.mentions.is_empty() {
                session.message_mentions.insert(msg_id, row.mentions.clone());
            }
            if flags != 0 {
                session.flags.insert(msg_id, flags);
            }
            let entry = session.messages.entry(chat_id).or_default();
            let mut restored_expires = None;
            if !entry.iter().any(|(_, m)| m.id == msg_id) {
                let restored = to_message_with_flags(row, flags);
                restored_expires = Some(restored.expires_at);
                entry.push((row.timestamp, restored));
                entry.sort_by_key(|(ts, _)| *ts);
                // The vec changed shape and order: rebuild the
                // position index from the final vec (qc1634 H5).
                session.rebuild_msg_index(chat_id);
            }
            // The restored row's expiry rejoins the ephemeral
            // index with it (qc1644).
            if let Some(expires_at) = restored_expires {
                session.seed_ephemeral(chat_id, msg_id, expires_at);
            }
        }
        self.protocol_ids.insert(msg_id, row.id.clone());
        if let Some(jid) = &row.sender_jid {
            self.note_sender_jid(msg_id, jid);
        }
        self.refresh_preview_after_removal(chat_id);
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }
}

#[cfg(test)]
mod star_pin_tests {
    use super::*;
    use crate::state::{App, Args};

    const GROUP_JID: &str = "120363001234567890@g.us";
    const DM_JID: &str = "6281234567890@s.whatsapp.net";
    const ALICE_JID: &str = "6281111111111@s.whatsapp.net";

    /// An app in live mode on a hand-fed backend channel, the
    /// same harness shape as the other live test modules.
    pub(super) fn live_app() -> (
        App,
        std::sync::mpsc::Sender<BackendEvent>,
        egui::Context,
        std::sync::mpsc::Receiver<BackendCommand>,
    ) {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = App::new(Args::parse(), &ctx);
        app.live_start_error = None;
        app.logged_out = false;
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<BackendCommand>();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel::<BackendEvent>();
        app.live = Some(LiveSession {
            saved_contacts: std::collections::HashMap::new(),
            saved_contact_forms: std::collections::HashMap::new(),
            retired_contact_forms: std::collections::HashMap::new(),
            saved_contacts_report_step: 0,
            orphaned_contact_forms: std::collections::HashSet::new(),
            sync_finishing: false,
            chat_positions: std::collections::HashMap::new(),
            roster_generation: 0,
            pending_archive: None,
            channels: BackendChannels { commands: cmd_tx, events: evt_rx },
            status: LinkStatus::Linked,
            qr: None,
            qr_expires_at: None,
            pair_code: None,
            pair_code_at: None,
            account: "Test account".to_owned(),
            connected_at: None,
            syncing: false,
            sync_last_activity: None,
            synced_once: false,
            sync_percent: None,
            sync_completed_at: None,
            seeded_unread: std::collections::HashSet::new(),
            msg_index: HashMap::new(),
            msg_index_complete: std::collections::HashSet::new(),
            ephemeral_index: std::collections::BTreeMap::new(),
            store: Some(store::Store::open_in_memory().unwrap()),
            jids: HashMap::new(),
            messages: HashMap::new(),
            times: HashMap::new(),
            fake: true,
            logout_requested: false,
            flags: HashMap::new(),
            engaged_chats: std::collections::HashSet::new(),
            poll_ballots: HashMap::new(),
            event_ballots: HashMap::new(),
            photo_refs: HashMap::new(),
            voice_refs: HashMap::new(),
            video_refs: HashMap::new(),
            video_thumbs: HashMap::new(),
            poster_refs: HashMap::new(),
            video_dims: HashMap::new(),
            document_refs: HashMap::new(),
            gif_videos: std::collections::HashSet::new(),
            sticker_anim: HashMap::new(),
            sticker_thumbs: HashMap::new(),
            document_thumbs: HashMap::new(),
            location_thumbs: HashMap::new(),
            preview_thumbs: HashMap::new(),
            view_once_ids: std::collections::HashSet::new(),
            verified_names: HashMap::new(),
            verified_checked: std::collections::HashSet::new(),
            sticker_refs: HashMap::new(),
            photo_thumbs: HashMap::new(),
            message_mentions: HashMap::new(),
            names: HashMap::new(),
            avatar_bytes: HashMap::new(),
            media_thumb_budget: crate::live::ByteBudget::default(),
            avatar_byte_budget: crate::live::ByteBudget::default(),
            avatar_missing: HashMap::new(),
            avatar_requested: std::collections::HashSet::new(),
            history: HashMap::new(),
            channel_meta: HashMap::new(),
            channel_infos: HashMap::new(),
            server_ids: HashMap::new(),
            channel_history: HashMap::new(),
            pending_channel_votes: HashMap::new(),
            channels_list_requested: false,
            history_armed: std::collections::HashSet::new(),
            paged_chats: std::collections::HashSet::new(),
            paged_to_start: std::collections::HashSet::new(),
        });
        app.live_mode = true;
        (app, evt_tx, ctx, cmd_rx)
    }

    pub(super) fn drain_all(app: &mut App, ctx: &egui::Context) {
        for _ in 0..64 {
            app.poll_backend(ctx);
            if app.event_queue.is_empty() {
                return;
            }
        }
        panic!("the event queue did not drain");
    }

    pub(super) fn sent_commands(
        cmd_rx: &std::sync::mpsc::Receiver<BackendCommand>,
    ) -> Vec<BackendCommand> {
        let mut out = Vec::new();
        while let Ok(cmd) = cmd_rx.try_recv() {
            out.push(cmd);
        }
        out
    }

    /// Feeds one message into the live mirror and opens its
    /// chat, returning the app message ID.
    pub(super) fn open_message(
        app: &mut App,
        tx: &std::sync::mpsc::Sender<BackendEvent>,
        ctx: &egui::Context,
        msg: LiveMessage,
    ) -> u64 {
        let chat_jid = msg.chat_jid.clone();
        let proto = msg.id.clone();
        tx.send(BackendEvent::MessageUpsert(msg)).unwrap();
        drain_all(app, ctx);
        app.selected_chat = jid_id(&chat_jid);
        app.sync_conversation();
        message_id(&chat_jid, &proto)
    }

    pub(super) fn alice_group_message() -> LiveMessage {
        let mut msg = LiveMessage::text(
            "g1",
            GROUP_JID,
            Some("Alice".to_owned()),
            "hello group",
            1_700_000_000,
            false,
        );
        msg.sender_jid = Some(ALICE_JID.to_owned());
        msg
    }

    pub(super) fn alice_dm_message() -> LiveMessage {
        let mut msg =
            LiveMessage::text("d1", DM_JID, Some("Partner".to_owned()), "hi", 1_700_000_000, false);
        msg.sender_jid = Some(DM_JID.to_owned());
        msg
    }

    pub(super) fn archived_has(app: &App, chat_jid: &str, protocol_id: &str) -> bool {
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        store.messages(chat_jid).unwrap().iter().any(|(m, _)| m.id == protocol_id)
    }

    pub(super) fn notice_kind(app: &App) -> Option<crate::state::NoticeKind> {
        app.notice.as_ref().map(|n| n.kind)
    }

    pub(super) fn mirror_flag(app: &App, chat_id: u64, mid: u64, pin: bool) -> bool {
        let mirror = app.live.as_ref().unwrap().messages.get(&chat_id).unwrap();
        let msg = &mirror.iter().find(|(_, m)| m.id == mid).unwrap().1;
        if pin {
            msg.pinned
        } else {
            msg.starred
        }
    }

    pub(super) fn view_flag(app: &App, mid: u64, pin: bool) -> bool {
        let msg = app.conversation.iter().find(|m| m.id == mid).unwrap();
        if pin {
            msg.pinned
        } else {
            msg.starred
        }
    }

    pub(super) fn archived_flags(app: &App, chat_jid: &str, protocol_id: &str) -> i64 {
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        store
            .messages(chat_jid)
            .unwrap()
            .into_iter()
            .find(|(m, _)| m.id == protocol_id)
            .expect("the message reached the archive")
            .1
    }

    pub(super) fn star_command(
        cmd_rx: &std::sync::mpsc::Receiver<BackendCommand>,
    ) -> (String, String, Option<String>, bool, bool) {
        let sent = sent_commands(cmd_rx);
        let found = sent.iter().find_map(|c| match c {
            BackendCommand::StarMessage { chat_jid, message_id, sender_jid, from_me, star } => {
                Some((chat_jid.clone(), message_id.clone(), sender_jid.clone(), *from_me, *star))
            }
            _ => None,
        });
        found.expect("a star command is sent")
    }

    pub(super) fn pin_command(
        cmd_rx: &std::sync::mpsc::Receiver<BackendCommand>,
    ) -> (String, String, Option<String>, bool, bool) {
        let sent = sent_commands(cmd_rx);
        let found = sent.iter().find_map(|c| match c {
            BackendCommand::PinMessage {
                chat_jid, message_id, sender_jid, from_me, pin, ..
            } => Some((chat_jid.clone(), message_id.clone(), sender_jid.clone(), *from_me, *pin)),
            _ => None,
        });
        found.expect("a pin command is sent")
    }

    /// A group star names the participant who authored the
    /// target message: the JID recorded for the message when it
    /// arrived is what the command carries, not None.
    #[test]
    pub(super) fn group_star_names_the_message_author() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.toggle_star_message(mid);
        let (chat_jid, message_id, sender_jid, from_me, star) = star_command(&cmd_rx);
        assert_eq!(chat_jid, GROUP_JID);
        assert_eq!(message_id, "g1");
        assert_eq!(sender_jid.as_deref(), Some(ALICE_JID), "the author is named");
        assert!(!from_me);
        assert!(star);
        assert_eq!(app.pending_stars.get(&mid), Some(&false), "the change awaits settlement");
    }

    /// A group pin follows the same author rule as the star.
    #[test]
    pub(super) fn group_pin_names_the_message_author() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.toggle_pin_message(mid, backend::PinDuration::Days7);
        let (chat_jid, message_id, sender_jid, from_me, pin) = pin_command(&cmd_rx);
        assert_eq!(chat_jid, GROUP_JID);
        assert_eq!(message_id, "g1");
        assert_eq!(sender_jid.as_deref(), Some(ALICE_JID), "the author is named");
        assert!(!from_me);
        assert!(pin);
        assert_eq!(app.pending_pins.get(&mid), Some(&false), "the change awaits settlement");
    }

    /// Starring my own group message needs no sender: the
    /// library resolves my group identity itself, and a sender
    /// in my name could even be the wrong one (PN vs LID
    /// addressing is the library's call).
    #[test]
    pub(super) fn own_group_star_omits_the_sender() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let msg = LiveMessage::text("g2", GROUP_JID, None, "my own", 1_700_000_000, true);
        let mid = open_message(&mut app, &tx, &ctx, msg);
        app.toggle_star_message(mid);
        let (_, _, sender_jid, from_me, _) = star_command(&cmd_rx);
        assert_eq!(sender_jid, None);
        assert!(from_me);
    }

    /// Direct-chat stars are unchanged: no sender is attached
    /// even though the partner's JID is on record, because a DM
    /// key carries no participant.
    #[test]
    pub(super) fn dm_star_carries_no_sender() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mut msg =
            LiveMessage::text("d1", DM_JID, Some("Partner".to_owned()), "hi", 1_700_000_000, false);
        msg.sender_jid = Some(DM_JID.to_owned());
        let mid = open_message(&mut app, &tx, &ctx, msg);
        app.toggle_star_message(mid);
        let (chat_jid, _, sender_jid, from_me, _) = star_command(&cmd_rx);
        assert_eq!(chat_jid, DM_JID);
        assert_eq!(sender_jid, None);
        assert!(!from_me);
    }

    /// A star the backend reports as failed rolls the
    /// optimistic state back everywhere it landed: the open
    /// view, the session mirror, and the persisted archive
    /// flag. The failure reaches the notice banner.
    #[test]
    pub(super) fn failed_group_star_rolls_back() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.toggle_star_message(mid);
        assert!(view_flag(&app, mid, false), "the optimistic star shows");
        assert!(mirror_flag(&app, chat_id, mid, false));
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), store::FLAG_STARRED);

        tx.send(BackendEvent::StarFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Starring failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!view_flag(&app, mid, false), "the open view rolls back");
        assert!(!mirror_flag(&app, chat_id, mid, false), "the mirror rolls back");
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), 0, "the archive flag clears");
        assert!(!app.pending_stars.contains_key(&mid), "nothing stays pending");
        let notice = app.notice_str().unwrap_or_default().to_owned();
        assert!(notice.contains("Starring failed"), "the failure is surfaced: {notice}");
    }

    /// The pin counterpart: a refused pin restores the
    /// unpinned state in the view, the mirror, and the archive.
    #[test]
    pub(super) fn failed_group_pin_rolls_back() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.toggle_pin_message(mid, backend::PinDuration::Days7);
        assert!(view_flag(&app, mid, true), "the optimistic pin shows");
        assert!(mirror_flag(&app, chat_id, mid, true));
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), store::FLAG_PINNED_MESSAGE);

        tx.send(BackendEvent::PinFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Pinning the message failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!view_flag(&app, mid, true), "the open view rolls back");
        assert!(!mirror_flag(&app, chat_id, mid, true), "the mirror rolls back");
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), 0, "the archive flag clears");
        assert!(!app.pending_pins.contains_key(&mid), "nothing stays pending");
        let notice = app.notice_str().unwrap_or_default().to_owned();
        assert!(notice.contains("Pinning the message failed"), "the failure is surfaced: {notice}");
    }

    /// Section 24: revoking another member's group message
    /// (the admin path) names the message's author in the
    /// command, exactly like star and pin do; before this round
    /// the revoke command carried no author at all, so the
    /// library could only ever take the sender path.
    #[test]
    pub(super) fn admin_revoke_names_the_group_author() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.delete_for_everyone(mid);
        let sent = sent_commands(&cmd_rx);
        let revoke = sent.iter().find_map(|c| match c {
            BackendCommand::RevokeMessage { chat_jid, message_id, sender_jid, from_me } => {
                Some((chat_jid.clone(), message_id.clone(), sender_jid.clone(), *from_me))
            }
            _ => None,
        });
        assert_eq!(
            revoke.expect("a revoke command is sent"),
            (GROUP_JID.to_owned(), "g1".to_owned(), Some(ALICE_JID.to_owned()), false),
            "the revoke target names the author and the admin path"
        );
    }

    /// Section 24: delete for me removes the message from the
    /// open view and the mirror at once, and the command names
    /// the author and carries the archive timestamp.
    #[test]
    pub(super) fn delete_for_me_removes_and_sends() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.delete_for_me(mid);
        assert!(app.conversation.iter().all(|m| m.id != mid), "the message leaves the open view");
        let mirror = app.live.as_ref().unwrap().messages.get(&chat_id).unwrap();
        assert!(mirror.iter().all(|(_, m)| m.id != mid), "and the session mirror");
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(|c| matches!(
                c,
                BackendCommand::DeleteMessageForMe { chat_jid, message_id, sender_jid, message_timestamp, .. }
                    if chat_jid == GROUP_JID
                        && message_id == "g1"
                        && sender_jid.as_deref() == Some(ALICE_JID)
                        && *message_timestamp == Some(1_700_000_000)
            )),
            "the command names the author and the archived timestamp: {sent:?}"
        );
    }

    /// Section 24: keeping a disappearing message applies at
    /// once (view, archive flag, command), and a KeepFailed
    /// rolls all three back, the star discipline exactly.
    #[test]
    pub(super) fn keep_toggle_applies_and_failed_keep_rolls_back() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mut msg = alice_group_message();
        msg.expires_at = 1_700_003_600;
        let mid = open_message(&mut app, &tx, &ctx, msg);
        app.toggle_keep_message(mid);
        assert!(
            app.conversation.iter().find(|m| m.id == mid).is_some_and(|m| m.kept),
            "the optimistic keep shows"
        );
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), store::FLAG_KEPT);
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(|c| matches!(
                c,
                BackendCommand::KeepMessage { message_id, keep, .. } if message_id == "g1" && *keep
            )),
            "the keep command is sent: {sent:?}"
        );

        tx.send(BackendEvent::KeepFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Keeping the message failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            !app.conversation.iter().find(|m| m.id == mid).is_some_and(|m| m.kept),
            "the view rolls back"
        );
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), 0, "the archive flag clears");
        assert!(!app.pending_keeps.contains_key(&mid), "nothing stays pending");
    }

    /// Section 24: a keep made on another device arrives as a
    /// MessageKept report and lands in the view and the
    /// archive without any command of mine.
    #[test]
    pub(super) fn kept_report_from_the_server_lands() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mut msg = alice_group_message();
        msg.expires_at = 1_700_003_600;
        let mid = open_message(&mut app, &tx, &ctx, msg);
        tx.send(BackendEvent::MessageKept {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            kept: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            app.conversation.iter().find(|m| m.id == mid).is_some_and(|m| m.kept),
            "the reported keep shows"
        );
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), store::FLAG_KEPT);
    }

    /// Section 24: mark as unread raises the badge at once,
    /// persists it in the archive, and sends the read-state
    /// command (never a receipt).
    #[test]
    pub(super) fn mark_unread_raises_and_persists_the_badge() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let _ = mid;
        let chat_id = jid_id(GROUP_JID);
        // Start from a known read state, whatever the arrival
        // itself did to the badge.
        app.apply_chat_read_changed(chat_id, true, 0);
        assert_eq!(app.chats.iter().find(|c| c.id == chat_id).map_or(9, |c| c.unread), 0);
        app.mark_chat_unread(chat_id);
        assert_eq!(
            app.chats.iter().find(|c| c.id == chat_id).map_or(0, |c| c.unread),
            1,
            "the badge floors at one"
        );
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        let archived = store.chats().unwrap();
        assert_eq!(
            archived.iter().find(|c| c.jid == GROUP_JID).map_or(0, |c| c.unread),
            1,
            "the archive row carries the badge"
        );
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(|c| matches!(c, BackendCommand::MarkChatUnread { chat_jid } if chat_jid == GROUP_JID)),
            "the mark-unread command is sent: {sent:?}"
        );
    }

    /// Section 24: a timed mute carries its end in the mirror
    /// and the archive, sends MuteUntil in milliseconds, and
    /// lifts itself when the end passes.
    #[test]
    pub(super) fn timed_mute_sets_end_and_lifts_when_it_passes() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.apply_mute_for(chat_id, MuteChoice::Hours8);
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(chat.muted);
        let until = chat.mute_until;
        let now = unix_now_secs();
        assert!(
            until > now && until <= now + 8 * 3_600,
            "the end is 8 hours out: {until} vs {now}"
        );
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(|c| matches!(
                c,
                BackendCommand::ChatAction { action: ChatAction::MuteUntil(ms), .. }
                    if *ms == i64::try_from(until).unwrap_or(0) * 1000
            )),
            "the wire end is the mirror end in milliseconds: {sent:?}"
        );
        app.lift_expired_mutes(until);
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(!chat.muted && chat.mute_until == 0, "the mute lifts itself at its end");
    }

    /// Section 24: clearing is ack-gated. The command goes out
    /// with the keep-starred choice and a Busy notice shows,
    /// but nothing is removed until the server confirms; the
    /// confirmation then keeps the starred message and drops
    /// the rest.
    #[test]
    pub(super) fn clear_chat_keeps_starred_when_asked() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mut starred = alice_group_message();
        starred.starred = true;
        let mid = open_message(&mut app, &tx, &ctx, starred);
        let mut other = alice_group_message();
        other.id = "g2".to_owned();
        let mid2 = open_message(&mut app, &tx, &ctx, other);
        let chat_id = jid_id(GROUP_JID);
        app.clear_chat(chat_id, true);
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(|c| matches!(
                c,
                BackendCommand::ClearChat { chat_jid, keep_starred } if chat_jid == GROUP_JID && *keep_starred
            )),
            "the clear command carries the keep-starred choice: {sent:?}"
        );
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Busy), "the wait is announced");
        assert!(
            app.conversation.iter().any(|m| m.id == mid2),
            "nothing is cleared before the server answers"
        );
        tx.send(BackendEvent::ChatCleared {
            chat_jid: GROUP_JID.to_owned(),
            delete_starred: false,
            last_message_timestamp: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.conversation.iter().any(|m| m.id == mid), "the starred message stays");
        assert!(app.conversation.iter().all(|m| m.id != mid2), "the rest is cleared");
        assert!(app.pending_clears.is_empty(), "the wait is settled");
    }

    /// Section 24: a failed clear destroys nothing: every
    /// message stays, in the view and in the archive, and the
    /// failure is the notice.
    #[test]
    pub(super) fn clear_chat_failure_keeps_everything() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.clear_chat(chat_id, false);
        tx.send(BackendEvent::ClearChatFailed {
            chat_jid: GROUP_JID.to_owned(),
            message: "Clearing the chat failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.conversation.iter().any(|m| m.id == mid), "the message survives");
        assert!(archived_has(&app, GROUP_JID, "g1"), "the archive row survives");
        assert!(
            notice_kind(&app) == Some(crate::state::NoticeKind::Error),
            "the failure is announced"
        );
        assert!(app.pending_clears.is_empty(), "the wait is settled");
    }

    /// Section 24: deleting a chat is ack-gated too: the chat
    /// stays in the list until the server confirms the delete.
    #[test]
    pub(super) fn delete_chat_removes_and_sends() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        assert!(app.chats.iter().any(|c| c.id == chat_id));
        app.delete_chat(chat_id);
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(
                |c| matches!(c, BackendCommand::DeleteChat { chat_jid } if chat_jid == GROUP_JID)
            ),
            "the delete command is sent: {sent:?}"
        );
        assert!(
            app.chats.iter().any(|c| c.id == chat_id),
            "the chat stays until the server confirms"
        );
        tx.send(BackendEvent::ChatDeleted { chat_jid: GROUP_JID.to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.chats.iter().all(|c| c.id != chat_id), "the chat leaves the list");
        assert!(app.pending_chat_deletes.is_empty(), "the wait is settled");
    }

    /// Section 24: a failed delete keeps the chat, its
    /// messages, and its archive rows.
    #[test]
    pub(super) fn delete_chat_failure_keeps_the_chat() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.delete_chat(chat_id);
        tx.send(BackendEvent::DeleteChatFailed {
            chat_jid: GROUP_JID.to_owned(),
            message: "Deleting the chat failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.chats.iter().any(|c| c.id == chat_id), "the chat stays");
        assert!(archived_has(&app, GROUP_JID, "g1"), "its messages stay");
        assert!(
            notice_kind(&app) == Some(crate::state::NoticeKind::Error),
            "the failure is announced"
        );
    }

    /// Section 24: a failed timed mute restores the exact
    /// previous state (unmuted, no end), and a failed unmute of
    /// a timed mute restores the end with it.
    #[test]
    pub(super) fn failed_chat_action_restores_the_mute_snapshot() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.apply_mute_for(chat_id, MuteChoice::Hours8);
        let until = app.chats.iter().find(|c| c.id == chat_id).unwrap().mute_until;
        assert!(until > 0);
        tx.send(BackendEvent::ChatActionFailed {
            chat_jid: GROUP_JID.to_owned(),
            action: backend::ChatAction::MuteUntil(1),
            message: "Changing the chat failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(!chat.muted && chat.mute_until == 0, "the failed mute rolls back fully");
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
        // And the other direction: a settled timed mute whose
        // unmute fails keeps its end.
        app.apply_mute_for(chat_id, MuteChoice::Hours8);
        let until = app.chats.iter().find(|c| c.id == chat_id).unwrap().mute_until;
        tx.send(BackendEvent::MuteChanged {
            chat_jid: GROUP_JID.to_owned(),
            muted: true,
            mute_until: i64::try_from(until).unwrap_or(0),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.pending_chat_actions.is_empty(), "the report settles the mute");
        app.apply_chat_action(chat_id, false, false, false);
        tx.send(BackendEvent::ChatActionFailed {
            chat_jid: GROUP_JID.to_owned(),
            action: backend::ChatAction::Unmute,
            message: "Changing the chat failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(
            chat.muted && chat.mute_until == until,
            "the failed unmute restores the timed mute"
        );
    }

    /// Section 24: mark-as-unread rolls the badge back when
    /// the server refuses it.
    #[test]
    pub(super) fn failed_mark_unread_restores_the_badge() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.apply_chat_read_changed(chat_id, true, 0);
        app.mark_chat_unread(chat_id);
        assert_eq!(app.chats.iter().find(|c| c.id == chat_id).map_or(0, |c| c.unread), 1);
        tx.send(BackendEvent::MarkUnreadFailed {
            chat_jid: GROUP_JID.to_owned(),
            message: "Marking the chat unread failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.chats.iter().find(|c| c.id == chat_id).map_or(9, |c| c.unread),
            0,
            "the badge rolls back"
        );
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
    }

    /// Section 24: a block is announced only when the
    /// server's blocklist confirms it, and a failed block
    /// restores the set.
    #[test]
    pub(super) fn block_waits_for_the_blocklist_and_rolls_back_on_failure() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_dm_message());
        let chat_id = jid_id(DM_JID);
        app.notice = None;
        app.toggle_block_contact(chat_id);
        assert!(app.blocked_chats.contains(&chat_id), "the set flips at once");
        assert!(app.notice.is_none(), "no success is claimed before the server answers");
        tx.send(BackendEvent::Blocklist(vec![DM_JID.to_owned()])).unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            app.notice_str() == Some("Contact blocked."),
            "the confirmation speaks the success: {:?}",
            app.notice
        );
        // Unblock, then fail it: the contact stays blocked.
        app.toggle_block_contact(chat_id);
        assert!(!app.blocked_chats.contains(&chat_id));
        tx.send(BackendEvent::BlockFailed {
            chat_jid: DM_JID.to_owned(),
            message: "Changing the block failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.blocked_chats.contains(&chat_id), "the failed unblock restores the block");
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
    }

    /// Section 24: a group info save applies its title only
    /// when the backend confirms it; a failure keeps the old
    /// title and says so.
    #[test]
    pub(super) fn group_info_save_waits_for_confirmation() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        let old_title = app.chats.iter().find(|c| c.id == chat_id).unwrap().title.clone();
        app.group_subject_edit = "Renamed Group".to_owned();
        app.group_desc_edit = String::new();
        app.notice = None;
        app.save_group_info(chat_id);
        assert_eq!(
            app.chats.iter().find(|c| c.id == chat_id).unwrap().title,
            old_title,
            "the title does not move at click time"
        );
        assert!(app.notice.is_none(), "nothing is claimed at click time");
        tx.send(BackendEvent::GroupInfoFailed {
            chat_jid: GROUP_JID.to_owned(),
            message: "Saving the group info failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.chats.iter().find(|c| c.id == chat_id).unwrap().title, old_title);
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
        app.save_group_info(chat_id);
        tx.send(BackendEvent::GroupInfoSaved { chat_jid: GROUP_JID.to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.chats.iter().find(|c| c.id == chat_id).unwrap().title,
            "Renamed Group",
            "the confirmed save applies the title"
        );
        assert!(
            app.notice_str() == Some("Group info saved."),
            "the confirmation speaks the success"
        );
    }

    /// Section 24: a failed leave restores the row the
    /// optimistic archive took out of the archived view.
    #[test]
    pub(super) fn failed_leave_restores_the_row() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.leave_group(chat_id);
        assert!(app.chats.iter().find(|c| c.id == chat_id).unwrap().archived);
        tx.send(BackendEvent::LeaveGroupFailed {
            chat_jid: GROUP_JID.to_owned(),
            message: "Leaving the group failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            !app.chats.iter().find(|c| c.id == chat_id).unwrap().archived,
            "the failed leave un-archives the row"
        );
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
    }

    /// qc1645 regression: the optimistic archive in
    /// leave_group flips a filter input, so it must bump the
    /// roster generation: the left group leaves the All view
    /// and appears under Archived without waiting for an
    /// unrelated roster change.
    #[test]
    pub(super) fn leave_group_bumps_the_roster_generation_and_moves_the_chat() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        let visible_ids = |app: &App| -> Vec<u64> {
            let indices: Vec<usize> = app.visible_chats().iter().copied().collect();
            indices.iter().map(|&index| app.chats[index].id).collect()
        };
        app.filter = core::filter::ChatFilter::All;
        assert!(visible_ids(&app).contains(&chat_id), "the group starts in All");
        let before = app.live.as_ref().unwrap().roster_generation;
        app.leave_group(chat_id);
        assert_eq!(
            app.live.as_ref().unwrap().roster_generation,
            before + 1,
            "the optimistic archive bumps the roster generation once"
        );
        assert!(!visible_ids(&app).contains(&chat_id), "the left group leaves All");
        app.filter = core::filter::ChatFilter::Archived;
        assert!(visible_ids(&app).contains(&chat_id), "the left group appears under Archived");
        // Leaving again does not flip the flag, so it does
        // not bump again.
        app.leave_group(chat_id);
        assert_eq!(
            app.live.as_ref().unwrap().roster_generation,
            before + 1,
            "a leave that flips nothing bumps nothing"
        );
    }

    /// Section 24: a failed timer change restores the
    /// previous duration quietly.
    #[test]
    pub(super) fn failed_timer_restores_the_previous_duration() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.set_disappearing_timer(chat_id, 86_400);
        assert_eq!(app.ephemeral_secs_of(chat_id), 86_400);
        tx.send(BackendEvent::DisappearingTimerFailed {
            chat_jid: GROUP_JID.to_owned(),
            message: "Changing the timer failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.ephemeral_secs_of(chat_id), 0, "the timer rolls back");
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
    }

    /// Section 24: a failed edit restores the original body,
    /// and a failed revoke restores the message itself.
    #[test]
    pub(super) fn failed_edit_and_revoke_restore_the_message() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mut mine = alice_group_message();
        mine.sender = None;
        mine.from_me = true;
        let mid = open_message(&mut app, &tx, &ctx, mine);
        let original = app.conversation.iter().find(|m| m.id == mid).unwrap().body.clone();
        app.apply_edit(mid, "a corrected body");
        assert_eq!(app.conversation.iter().find(|m| m.id == mid).unwrap().body, "a corrected body");
        tx.send(BackendEvent::EditMessageFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Editing the message failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let msg = app.conversation.iter().find(|m| m.id == mid).unwrap();
        assert_eq!(msg.body, original, "the failed edit restores the body");
        assert!(!msg.edited, "and the edited mark");
        app.delete_for_everyone(mid);
        assert!(app.conversation.iter().find(|m| m.id == mid).unwrap().deleted);
        tx.send(BackendEvent::RevokeMessageFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Deleting the message failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            !app.conversation.iter().find(|m| m.id == mid).unwrap().deleted,
            "the failed revoke restores the message"
        );
    }

    /// Section 24: a failed delete-for-me restores the
    /// message everywhere the optimistic removal took it
    /// from: the view, the mirror, and the archive.
    #[test]
    pub(super) fn failed_delete_for_me_restores_the_message() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.delete_for_me(mid);
        assert!(app.conversation.iter().all(|m| m.id != mid), "the removal applies at once");
        tx.send(BackendEvent::DeleteForMeFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Deleting the message failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.conversation.iter().any(|m| m.id == mid), "the view gets it back");
        assert!(archived_has(&app, GROUP_JID, "g1"), "the archive gets it back");
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
        assert!(app.pending_delete_for_me.is_empty(), "the wait is settled");
    }

    /// Section 24: a failed poll vote restores my previous
    /// selection in the view, and the notice names the
    /// failure.
    #[test]
    pub(super) fn failed_poll_vote_restores_my_selection() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mut poll = alice_group_message();
        poll.kind = backend::LiveMessageKind::Poll {
            question: "Lunch?".to_owned(),
            options: vec![("Deli".to_owned(), 1), ("Sushi".to_owned(), 0)],
            multiple: false,
            my_votes: vec![0],
            correct: None,
        };
        let mid = open_message(&mut app, &tx, &ctx, poll);
        let my_votes =
            |app: &App| match &app.conversation.iter().find(|m| m.id == mid).unwrap().kind {
                MessageKind::Poll { ref my_votes, .. } => my_votes.clone(),
                _ => panic!("not a poll"),
            };
        app.vote_poll(mid, 1);
        assert_eq!(my_votes(&app), vec![1], "the vote applies at once");
        tx.send(BackendEvent::PollVoteFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Voting failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(my_votes(&app), vec![0], "the failed vote restores my selection");
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
        assert!(app.pending_poll_votes.is_empty(), "the wait is settled");
    }

    fn alice_event_message() -> LiveMessage {
        let mut msg = alice_group_message();
        msg.kind = backend::LiveMessageKind::Event {
            name: "Sunday hike".to_owned(),
            description: String::new(),
            start_time: 1_700_086_400,
            end_time: 0,
            location: "Trailhead".to_owned(),
            join_link: String::new(),
            scheduled_call: false,
            extra_guests: false,
            canceled: false,
            my_rsvp: None,
            going: 0,
            maybe: 0,
            not_going: 0,
        };
        msg.event_secret = Some(vec![5u8; 32]);
        msg.event_creator = Some(ALICE_JID.to_owned());
        msg
    }

    fn view_event(app: &App, mid: u64) -> (Option<core::model::EventRsvp>, u32, String) {
        match &app.conversation.iter().find(|m| m.id == mid).unwrap().kind {
            MessageKind::Event { my_rsvp, going, name, .. } => (*my_rsvp, *going, name.clone()),
            _ => panic!("not an event"),
        }
    }

    /// P7 section 27: a failed RSVP restores my previous
    /// answer (here: none) and the tally that came with it.
    #[test]
    pub(super) fn failed_event_rsvp_restores_my_answer() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_event_message());
        app.respond_event(mid, core::model::EventRsvp::Going, None);
        assert_eq!(
            view_event(&app, mid).0,
            Some(core::model::EventRsvp::Going),
            "the answer applies at once"
        );
        assert_eq!(view_event(&app, mid).1, 1, "and counts at once");
        tx.send(BackendEvent::EventRespondFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Answering failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(view_event(&app, mid).0, None, "the failed RSVP restores my answer");
        assert_eq!(view_event(&app, mid).1, 0, "and the tally");
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
        assert!(app.pending_event_rsvps.is_empty(), "the wait is settled");
    }

    /// P7: another participant's answer lands on the tallies.
    #[test]
    pub(super) fn incoming_event_response_counts_the_ballot() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_event_message());
        tx.send(BackendEvent::EventResponse {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            responder_jid: "10000000007@s.whatsapp.net".to_owned(),
            from_me: false,
            response: backend::EventRsvp::Going,
            extra_guest_count: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(view_event(&app, mid).1, 1, "the ballot counts");
        assert_eq!(view_event(&app, mid).0, None, "it is not my answer");
    }

    /// P7 section 27: a failed edit restores the event's
    /// previous fields in the view.
    #[test]
    pub(super) fn failed_event_edit_restores_the_fields() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mut mine = alice_event_message();
        mine.from_me = true;
        let mid = open_message(&mut app, &tx, &ctx, mine);
        app.open_event_edit(mid);
        app.event_edit_draft.as_mut().unwrap().2.name = "Sunday hike (moved)".to_owned();
        app.submit_event_edit();
        assert_eq!(view_event(&app, mid).2, "Sunday hike (moved)", "the edit applies at once");
        tx.send(BackendEvent::EventEditFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Editing failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(view_event(&app, mid).2, "Sunday hike", "the failed edit restores the name");
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
        assert!(app.pending_event_edits.is_empty(), "the wait is settled");
    }

    /// A pin made on another device arrives as a
    /// MessagePinChanged report and lands on the message in
    /// the view, the mirror, and the archive.
    #[test]
    pub(super) fn server_pin_change_lands_on_the_message() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        tx.send(BackendEvent::MessagePinChanged {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            pinned: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(view_flag(&app, mid, true), "the view shows the pin");
        assert!(mirror_flag(&app, chat_id, mid, true), "the mirror shows the pin");
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), store::FLAG_PINNED_MESSAGE);
        tx.send(BackendEvent::MessagePinChanged {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            pinned: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!view_flag(&app, mid, true), "the unpin lands too");
    }

    /// The status broadcast address never becomes a chat:
    /// neither a chat upsert nor a message filed under it
    /// creates the fake "status" DM an earlier build showed.
    #[test]
    pub(super) fn status_broadcast_never_becomes_a_chat() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::ChatUpsert(LiveChat {
            jid: "status@broadcast".to_owned(),
            title: "status".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "a status".to_owned(),
            preview_sender: None,
            timestamp: 1_700_000_000,
            unread: 1,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
            from_history: false,
        }))
        .unwrap();
        let mut status_msg = alice_dm_message();
        status_msg.chat_jid = "status@broadcast".to_owned();
        tx.send(BackendEvent::MessageUpsert(status_msg)).unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            app.chats.iter().all(|c| c.title != "status"),
            "no status chat exists: {:?}",
            app.chats.iter().map(|c| &c.title).collect::<Vec<_>>()
        );
        assert!(
            !app.live.as_ref().unwrap().messages.contains_key(&jid_id("status@broadcast")),
            "no mirror is kept under the broadcast address"
        );
    }

    /// A timed mute survives message-arrival upserts, which
    /// report mute_until 0 without knowing better; the merged
    /// row keeps the end in the view and the archive.
    #[test]
    pub(super) fn timed_mute_survives_arrival_upserts() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.apply_mute_for(chat_id, MuteChoice::Hours8);
        let until = app.chats.iter().find(|c| c.id == chat_id).unwrap().mute_until;
        tx.send(BackendEvent::ChatUpsert(LiveChat {
            jid: GROUP_JID.to_owned(),
            title: "Group".to_owned(),
            kind: LiveChatKind::Group,
            preview: "new message".to_owned(),
            preview_sender: None,
            timestamp: 1_700_000_060,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
            from_history: false,
        }))
        .unwrap();
        drain_all(&mut app, &ctx);
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(chat.muted && chat.mute_until == until, "the timed mute keeps its end");
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        let archived = store.chats().unwrap();
        let row = archived.iter().find(|c| c.jid == GROUP_JID).unwrap();
        assert_eq!(row.mute_until, i64::try_from(until).unwrap_or(0), "the archive keeps it too");
    }

    /// The dock's Report row opens the spam-report
    /// confirmation for the chat's latest incoming message,
    /// and refuses honestly when there is nothing to report.
    #[test]
    pub(super) fn report_chat_opens_the_spam_confirmation() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_dm_message());
        let chat_id = jid_id(DM_JID);
        app.report_chat(chat_id);
        assert!(
            matches!(
                app.ui_confirm,
                Some(crate::state::UiConfirm::ReportSpam { chat_id: c, msg_id: m, .. })
                    if c == chat_id && m == mid
            ),
            "the confirmation names the latest incoming message: {:?}",
            app.ui_confirm.as_ref().map(|_| "other")
        );
        // A chat with no incoming message: an honest refusal,
        // no modal that could only fail.
        let (mut app2, tx2, ctx2, _rx2) = live_app();
        let mut mine = alice_dm_message();
        mine.sender = None;
        mine.from_me = true;
        let _ = open_message(&mut app2, &tx2, &ctx2, mine);
        app2.report_chat(chat_id);
        assert!(app2.ui_confirm.is_none(), "no confirmation opens");
        assert!(
            notice_kind(&app2) == Some(crate::state::NoticeKind::Error),
            "the refusal is spoken"
        );
    }

    /// A received voice note renders the waveform its payload
    /// carried; a payload without one renders the neutral
    /// flat form, never a shape derived from the caption.
    #[test]
    pub(super) fn voice_waveform_comes_from_the_wire_or_is_neutral() {
        let bars = voice_waveform_bars(Some(&[0, 128, 255]));
        assert_eq!(bars.len(), 3);
        assert!(bars[0] < bars[1] && bars[1] < bars[2], "the bars follow the bytes: {bars:?}");
        let flat = voice_waveform_bars(None);
        assert!(flat.iter().all(|b| *b == flat[0]), "no payload, one flat height");
        let mut msg = alice_dm_message();
        msg.kind = backend::LiveMessageKind::Media {
            media: backend::MediaKind::Voice,
            caption: String::new(),
            seconds: 3,
            size_bytes: 10,
        };
        msg.voice_waveform = Some(vec![255, 0]);
        let converted = to_message_with_flags(&msg, 0);
        match converted.kind {
            MessageKind::Voice { waveform, .. } => {
                assert_eq!(waveform.len(), 2);
                assert!(waveform[0] > waveform[1], "the wire shape survives conversion");
            }
            other => panic!("expected a voice message, got {other:?}"),
        }
    }

    /// Section 24: the pin command carries the duration the
    /// menu picked, not a fixed one.
    #[test]
    pub(super) fn pin_command_carries_the_chosen_duration() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.toggle_pin_message(mid, backend::PinDuration::Hours24);
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(|c| matches!(
                c,
                BackendCommand::PinMessage { duration: backend::PinDuration::Hours24, .. }
            )),
            "the 24-hour duration reaches the command: {sent:?}"
        );
    }

    /// Rolling back an *unstar* restores the star the server
    /// had confirmed before, not an empty state.
    #[test]
    pub(super) fn failed_unstar_restores_the_star() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mut msg = alice_group_message();
        msg.starred = true;
        let mid = open_message(&mut app, &tx, &ctx, msg);
        assert!(view_flag(&app, mid, false), "the message starts starred");

        app.toggle_star_message(mid);
        assert!(!view_flag(&app, mid, false), "the optimistic unstar shows");
        assert_eq!(app.pending_stars.get(&mid), Some(&true));

        tx.send(BackendEvent::StarFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Starring failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(view_flag(&app, mid, false), "the confirmed star is restored");
        assert!(mirror_flag(&app, chat_id, mid, false));
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), store::FLAG_STARRED);
        assert!(!app.pending_stars.contains_key(&mid), "nothing stays pending");
    }

    /// A server report of the star settles the pending change:
    /// the value stays, and the snapshot is gone, so a later
    /// failure report for a different change starts from the
    /// confirmed state.
    #[test]
    pub(super) fn server_star_report_confirms_the_pending_change() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.toggle_star_message(mid);
        assert!(app.pending_stars.contains_key(&mid));

        tx.send(BackendEvent::StarChanged {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            starred: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(mirror_flag(&app, chat_id, mid, false), "the star stands");
        assert!(!app.pending_stars.contains_key(&mid), "the report confirms the change");
    }

    /// An upsert that already carries the pinned state settles
    /// a pending pin (the pin has no report event of its own):
    /// the value stays and the snapshot is gone.
    #[test]
    pub(super) fn pin_upsert_confirms_the_pending_change() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.toggle_pin_message(mid, backend::PinDuration::Days7);
        assert!(app.pending_pins.contains_key(&mid));

        let mut confirmed = alice_group_message();
        confirmed.pinned = true;
        tx.send(BackendEvent::MessageUpsert(confirmed)).unwrap();
        drain_all(&mut app, &ctx);
        assert!(mirror_flag(&app, chat_id, mid, true), "the pin stands");
        assert!(!app.pending_pins.contains_key(&mid), "the upsert confirms the change");
    }
}
