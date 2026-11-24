//! Chat actions taken from menus and panels: favorites,
//! clear and delete, mute durations, contact cards, channel
//! paging and the new-chat lookup.

use super::*;

impl crate::state::App {
    /// Applies a favorite toggle for one chat: the mirror flips
    /// at once, and in live mode the flag is written through to
    /// the archive, so it survives restarts. Favorites are an
    /// account property (the phone syncs the set through app
    /// state, and `apply_favorites_sync` adopts it), but the
    /// pinned library exposes no outbound setter for it, so a
    /// toggle made here stays local until the phone's next sync
    /// replaces the set; the archive column the write lands in
    /// is one the chat sync paths never touch, so a later
    /// re-sync of the chat list cannot wipe the flag either. In
    /// demo mode there is no session and no archive: the toggle
    /// is the in-memory fixture flip only, like every other demo
    /// interaction.
    pub(crate) fn apply_favorite(&mut self, chat_id: u64, favorite: bool) {
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.favorite = favorite;
        }
        if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            c.favorite = favorite;
        }
        if let Some(session) = &self.live {
            if let (Some(store), Some(jid)) = (&session.store, session.jids.get(&chat_id)) {
                let _ = store.set_chat_favorite(jid, favorite);
            }
        }
    }

    /// Applies the archive's recorded favorite flags to the
    /// chat mirror. The chat conversion cannot carry them (the
    /// synced chat row has no favorite field), so a live session
    /// overlays them from the archive's own record when it loads
    /// the chat list; every chat the archive does not flag is
    /// set unfavorited, so the mirror never inherits a flag from
    /// anywhere else (least of all from demo fixtures).
    pub(crate) fn apply_stored_favorites(&mut self) {
        let favorites: std::collections::HashSet<u64> = self
            .live
            .as_ref()
            .and_then(|session| session.store.as_ref())
            .and_then(|store| store.chat_favorites().ok())
            .map(|jids| jids.iter().map(|j| jid_id(j)).collect())
            .unwrap_or_default();
        for chat in &mut self.chats {
            chat.favorite = favorites.contains(&chat.id);
        }
        if let Some(c) = self.detached_chat.as_mut() {
            c.favorite = favorites.contains(&c.id);
        }
    }

    /// Adopts the phone's synced favorite set: the list the
    /// backend reports is complete, so every chat not in it is
    /// unfavorited. The mirror and the archive are both updated
    /// (the archive through the same per-chat accessor the local
    /// toggle uses).
    pub(crate) fn apply_favorites_sync(&mut self, chat_jids: &[String]) {
        let favorites: std::collections::HashSet<u64> =
            chat_jids.iter().map(|j| jid_id(j)).collect();
        for chat in &mut self.chats {
            chat.favorite = favorites.contains(&chat.id);
        }
        if let Some(c) = self.detached_chat.as_mut() {
            c.favorite = favorites.contains(&c.id);
        }
        if let Some(session) = &self.live {
            if let Some(store) = &session.store {
                for (id, jid) in &session.jids {
                    let _ = store.set_chat_favorite(jid, favorites.contains(id));
                }
            }
        }
    }

    /// Inserts the placeholder for a message the wire could not
    /// decrypt (its failure mode said to show one), but only
    /// when the message is genuinely absent: a stored or
    /// in-memory copy always wins, and a later successful
    /// decryption upserts the real message over the placeholder
    /// through the normal path.
    pub(crate) fn apply_message_unavailable(
        &mut self,
        chat_jid: &str,
        proto_id: &str,
        sender_jid: Option<String>,
        from_me: bool,
        timestamp: i64,
    ) {
        let chat_id = jid_id(chat_jid);
        let mid = message_id(chat_jid, proto_id);
        if let Some(session) = &self.live {
            if session
                .messages
                .get(&chat_id)
                .is_some_and(|rows| rows.iter().any(|(_, m)| m.id == mid))
            {
                return;
            }
            if let Some(store) = &session.store {
                if store.has_message(chat_jid, proto_id).unwrap_or(false) {
                    return;
                }
            }
        } else {
            return;
        }
        let sender_name = sender_jid
            .as_deref()
            .and_then(|jid| self.live.as_ref().and_then(|s| s.names.get(jid).cloned()));
        let placeholder = backend::LiveMessage::text(
            proto_id.to_owned(),
            chat_jid.to_owned(),
            sender_name,
            "This message could not be decrypted. It may arrive again when your phone reconnects."
                .to_owned(),
            timestamp,
            from_me,
        );
        self.apply_message_upsert(&placeholder);
    }

    /// Applies archive/pin/mute state to one chat: locally at once,
    /// and through the backend in live mode (the archive store is
    /// written by the same path, so the state survives restarts).
    pub(crate) fn apply_chat_action(
        &mut self,
        chat_id: u64,
        archived: bool,
        pinned: bool,
        muted: bool,
    ) {
        let in_chats = self.chats.iter().any(|c| c.id == chat_id);
        let (was_archived, was_pinned, was_muted, was_mute_until) = self
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .or_else(|| self.detached_chat.as_ref().filter(|c| c.id == chat_id))
            .map_or((archived, pinned, muted, 0), |c| {
                (c.archived, c.pinned, c.muted, c.mute_until)
            });
        // Snapshot the pre-action state per field group, so a
        // ChatActionFailed can restore exactly the fields its
        // action owns. First change wins: a second action on the
        // same group while the first is unsettled rolls back to
        // the state before both, the last server-known state.
        if self.live.is_some() {
            let snap =
                (was_archived, was_pinned, was_muted, i64::try_from(was_mute_until).unwrap_or(0));
            if archived != was_archived {
                self.pending_chat_actions.entry((chat_id, "archive")).or_insert(snap);
            }
            if pinned != was_pinned {
                self.pending_chat_actions.entry((chat_id, "pin")).or_insert(snap);
            }
            if muted != was_muted {
                self.pending_chat_actions.entry((chat_id, "mute")).or_insert(snap);
            }
        }
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.archived = archived;
            c.pinned = pinned;
            c.muted = muted;
            // This path only ever sets an indefinite mute or an
            // unmute; a timed mute goes through apply_mute_for.
            // Either way any previous timed end is spent.
            c.mute_until = 0;
        } else if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            // The detached announcement chat lives outside the
            // chat list, so the header menu actions fall back to
            // it instead of silently doing nothing.
            c.archived = archived;
            c.pinned = pinned;
            c.muted = muted;
            c.mute_until = 0;
        }
        // The backend and archive write-through below only run
        // for a chat found in the list: in live mode the detached
        // slot is always None, and the guard makes that explicit.
        if in_chats && self.live.is_some() {
            if let Some(jid) = self.live_jid(chat_id) {
                use backend::{BackendCommand, ChatAction};
                let mut send = |action: ChatAction| {
                    self.send_backend(BackendCommand::ChatAction { chat_jid: jid.clone(), action });
                };
                if archived != was_archived {
                    send(if archived { ChatAction::Archive } else { ChatAction::Unarchive });
                }
                if pinned != was_pinned {
                    send(if pinned { ChatAction::Pin } else { ChatAction::Unpin });
                }
                if muted != was_muted {
                    send(if muted { ChatAction::Mute } else { ChatAction::Unmute });
                }
            }
        }
        // Write the new list state through to the archive. Only
        // a chat found in the list reaches this write-through;
        // the detached fallback above is a local-only mutation.
        let snapshot = if in_chats {
            self.live.as_ref().and_then(|session| {
                let jid = session.jids.get(&chat_id)?.clone();
                let ts = session.times.get(&chat_id).copied().unwrap_or(0);
                let chat = self.chats.iter().find(|c| c.id == chat_id)?.clone();
                Some((chat, jid, ts))
            })
        } else {
            None
        };
        if let (Some((chat, jid, ts)), Some(session)) = (snapshot, &self.live) {
            if let Some(store) = &session.store {
                let _ = store.upsert_chat(&crate::live::chat_to_live(&chat, &jid, ts));
            }
        }
    }

    /// Marks a chat as unread (design standard section 24):
    /// the badge appears at once and persists through the
    /// archive, and the wire state follows through the chat
    /// actions facade (the read-state action with read =
    /// false), never through the receipt path, which would
    /// tell the sender their messages were read. Demo mode
    /// applies the same local change with no command behind it.
    pub(crate) fn mark_chat_unread(&mut self, chat_id: u64) {
        let current = self.chats.iter().find(|c| c.id == chat_id).map_or(0, |c| c.unread);
        if self.live.is_some() {
            self.pending_unread.entry(chat_id).or_insert(current);
        }
        self.apply_chat_read_changed(chat_id, false, current);
        if self.live.is_some() {
            if let Some(jid) = self.live_jid(chat_id) {
                self.send_backend(BackendCommand::MarkChatUnread { chat_jid: jid });
            }
        }
    }

    /// Clears a chat's messages (design standard section 24),
    /// after the confirmation modal. In live mode the command
    /// goes first and NOTHING is deleted locally yet: the local
    /// apply is the confirming ChatCleared report's, through
    /// the same adoption an inbound clear takes, and a
    /// ClearChatFailed leaves every message in place. A clear
    /// whose local half ran before its outcome existed could
    /// destroy the archive copy of messages the server still
    /// holds; that trade is never made. `keep_starred` is the
    /// modal's checkbox choice, honored on both sides.
    pub(crate) fn clear_chat(&mut self, chat_id: u64, keep_starred: bool) {
        if let Some(jid) = self.live_jid(chat_id) {
            self.pending_clears.insert(chat_id, keep_starred);
            self.notice = Some(Notice::busy(crate::i18n::t("notice.clearing_chat")));
            self.send_backend(BackendCommand::ClearChat { chat_jid: jid, keep_starred });
        } else if !self.live_mode {
            // Demo mode: the chat's messages live in the model
            // itself; the same keep-starred rule applies.
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                chat.messages.retain(|m| keep_starred && m.starred);
                chat.preview = String::new();
            }
            if self.selected_chat == chat_id {
                self.conversation.retain(|m| keep_starred && m.starred);
            }
        }
    }

    /// Deletes a chat (design standard section 24), after the
    /// confirmation modal. Live mode follows the same rule as
    /// clear: the command goes first, the local removal is the
    /// confirming ChatDeleted report's, and a DeleteChatFailed
    /// leaves the chat fully in place.
    pub(crate) fn delete_chat(&mut self, chat_id: u64) {
        if let Some(jid) = self.live_jid(chat_id) {
            self.pending_chat_deletes.insert(chat_id);
            self.notice = Some(Notice::busy(crate::i18n::t("notice.deleting_chat")));
            self.send_backend(BackendCommand::DeleteChat { chat_jid: jid });
        } else if self.live_mode {
            // A live session whose address book does not know
            // this chat: nothing to delete against, and no
            // silent pretend-delete.
        } else {
            // Demo mode: the chat exists only locally.
            self.chats.retain(|c| c.id != chat_id);
            if self.selected_chat == chat_id {
                self.selected_chat = 0;
            }
        }
    }

    /// Applies one of the mute duration choices (design
    /// standard section 24): 8 hours, 1 week, or always. The
    /// mirror updates at once with the end the wire will carry;
    /// the per-second tick lifts a timed mute when its end
    /// passes, and the archive row persists both fields.
    pub(crate) fn apply_mute_for(&mut self, chat_id: u64, choice: MuteChoice) {
        let now = unix_now_secs();
        let (until, action) = match choice {
            MuteChoice::Hours8 => {
                let until = now + 8 * 3_600;
                (
                    until,
                    ChatAction::MuteUntil(i64::try_from(until).unwrap_or(0).saturating_mul(1000)),
                )
            }
            MuteChoice::Week => {
                let until = now + 7 * 86_400;
                (
                    until,
                    ChatAction::MuteUntil(i64::try_from(until).unwrap_or(0).saturating_mul(1000)),
                )
            }
            MuteChoice::Always => (0, ChatAction::Mute),
        };
        // The mute snapshot a ChatActionFailed restores,
        // taken before the mirror moves (see apply_chat_action).
        if self.live.is_some() {
            if let Some(c) = self.chats.iter().find(|c| c.id == chat_id) {
                let snap =
                    (c.archived, c.pinned, c.muted, i64::try_from(c.mute_until).unwrap_or(0));
                self.pending_chat_actions.entry((chat_id, "mute")).or_insert(snap);
            }
        }
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.muted = true;
            c.mute_until = until;
        }
        if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            c.muted = true;
            c.mute_until = until;
        }
        if self.live.is_some() {
            if let Some(jid) = self.live_jid(chat_id) {
                self.send_backend(BackendCommand::ChatAction { chat_jid: jid, action });
            }
        }
        self.persist_chat_row(chat_id);
    }

    /// Sends a contact card into a chat (design standard
    /// section 24). Live mode goes through the backend's
    /// generic send; the sent card arrives back as a normal
    /// message upsert and renders through the received-card
    /// path. Demo mode appends the card locally, the way the
    /// demo's other sends land.
    pub(crate) fn send_contact_card(&mut self, chat_id: u64, name: &str, phone: &str) {
        if self.live.is_some() {
            if let Some(jid) = self.live_jid(chat_id) {
                let ephemeral_secs =
                    self.chats.iter().find(|c| c.id == chat_id).map_or(0, |c| c.ephemeral_secs);
                self.send_backend(BackendCommand::SendContact {
                    chat_jid: jid,
                    name: name.to_owned(),
                    phone: phone.to_owned(),
                    ephemeral_secs,
                });
                // Busy while the send is in flight (P2-32).
                self.notice = Some(Notice::busy(crate::i18n::t("notice.sending_contact")));
            }
        } else if !self.live_mode {
            // Demo mode: append the card locally, the shape the
            // demo's other sends take (the location send in
            // attach.rs is the model).
            self.sent_counter += 1;
            let (stamp, expires_at) = self.demo_send_stamp();
            let msg = Message {
                kind: MessageKind::Contact { name: name.to_owned(), phone: phone.to_owned() },
                expires_at,
                ..Message::mine(900 + self.sent_counter, name.to_owned(), stamp)
            };
            if self.selected_chat == chat_id {
                self.conversation.push(msg.clone());
            }
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                chat.messages.push(msg);
                chat.sync_preview_from_last();
            }
        }
    }

    /// The live JID for a chat id, when known.
    pub(crate) fn live_jid(&self, chat_id: u64) -> Option<String> {
        self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned())
    }

    /// Asks the phone for the page of history older than the
    /// oldest message on hand for one chat (design standard
    /// section 22). The anchor is the oldest archived message:
    /// its protocol identity, direction and timestamp are what
    /// the phone pages from. Firing marks the chat Fetching
    /// (the stream's loading row keys off it) and disarms the
    /// scroll trigger; the outcome events move the state from
    /// there. A chat that cannot be anchored (nothing archived
    /// yet) is left idle and disarmed, never marked fetching
    /// for a request that was not sent.
    /// Asks the backend for one page of a channel's history
    /// (the newest page when `before` is None, else the page
    /// older than that server ID), under the channel_history
    /// latch: a page already in flight is never duplicated,
    /// and an exhausted channel is never re-asked.
    pub(crate) fn fetch_channel_page(&mut self, jid: &str, before: Option<u64>) {
        let chat_id = jid_id(jid);
        let Some(session) = &mut self.live else {
            return;
        };
        match session.channel_history.get(&chat_id) {
            Some(HistoryFetch::Fetching) | Some(HistoryFetch::Exhausted) => return,
            None => {}
        }
        session.channel_history.insert(chat_id, HistoryFetch::Fetching);
        self.send_backend(BackendCommand::FetchChannelHistory {
            jid: jid.to_owned(),
            count: 30,
            before,
        });
    }

    /// Opens a channel's backend record: live updates are
    /// armed (the backend dedupes repeat watches) and, while
    /// the channel's history state is idle, its newest page
    /// is fetched, because the roster only carries the latest
    /// post. Called from chat selection for channel chats.
    pub(crate) fn open_channel(&mut self, chat_id: u64) {
        let Some(jid) = self.live_jid(chat_id) else {
            return;
        };
        // The role drives the composer; if this channel's
        // metadata has not landed yet, ask for the list that
        // carries it.
        let meta_missing = self.live.as_ref().is_some_and(|s| !s.channel_meta.contains_key(&jid));
        if meta_missing {
            if let Some(session) = &mut self.live {
                session.channels_list_requested = true;
            }
            self.send_backend(BackendCommand::FetchChannels);
        }
        self.send_backend(BackendCommand::WatchChannel { jid: jid.clone() });
        let idle = self.live.as_ref().is_some_and(|s| !s.channel_history.contains_key(&chat_id));
        if idle {
            self.fetch_channel_page(&jid, None);
        }
    }

    /// The stream's scroll-to-top trigger for a channel: the
    /// page older than the oldest server ID on hand. Channels
    /// page on server IDs, not protocol anchors, so this is
    /// separate from the chat history path.
    pub(crate) fn request_older_channel_page(&mut self, chat_id: u64) {
        let Some(jid) = self.live_jid(chat_id) else {
            return;
        };
        let before = self.live.as_ref().and_then(|s| {
            s.server_ids.keys().filter(|(cid, _)| *cid == chat_id).map(|(_, sid)| *sid).min()
        });
        self.fetch_channel_page(&jid, before);
    }

    /// Applies one channel live-update counter payload: the
    /// update is resolved by (chat, server ID), its reaction
    /// counts are replaced, and its poll tallies are matched
    /// back to option names by their protocol hash. Counters
    /// for an update this session has not loaded change
    /// nothing; the next history fetch carries the same
    /// numbers from the server's record.
    pub(crate) fn apply_channel_counters(&mut self, counters: &backend::LiveChannelCounters) {
        use sha2::Digest as _;
        let chat_id = jid_id(&counters.jid);
        let Some(mid) = self
            .live
            .as_ref()
            .and_then(|s| s.server_ids.get(&(chat_id, counters.server_id)).copied())
        else {
            return;
        };
        let mut changed = false;
        if let Some(session) = &mut self.live {
            if let Some(msgs) = session.messages.get_mut(&chat_id) {
                if let Some((_, msg)) = msgs.iter_mut().find(|(_, m)| m.id == mid) {
                    msg.reactions = counters.reactions.clone();
                    if let core::model::MessageKind::Poll { options, .. } = &mut msg.kind {
                        for (hash, count) in &counters.poll_votes {
                            for (name, votes) in options.iter_mut() {
                                if sha2::Sha256::digest(name.as_bytes()).as_slice()
                                    == hash.as_slice()
                                {
                                    *votes = *count;
                                }
                            }
                        }
                    }
                    changed = true;
                }
            }
        }
        if changed && self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Sets my selection on a channel poll message in the
    /// session store and, when it is the open conversation,
    /// the visible copy: the rollback half of the channel
    /// vote pair. Tallies move by the difference between the
    /// current selection and the one being restored, the
    /// same arithmetic a fresh vote applies.
    pub(crate) fn set_poll_my_votes(&mut self, chat_id: u64, msg_id: u64, votes: &[usize]) {
        fn apply(msg: &mut Message, votes: &[usize]) {
            if let MessageKind::Poll { options, my_votes, .. } = &mut msg.kind {
                for (idx, (_, count)) in options.iter_mut().enumerate() {
                    let was = my_votes.contains(&idx);
                    let now = votes.contains(&idx);
                    if now && !was {
                        *count += 1;
                    } else if was && !now {
                        *count = count.saturating_sub(1);
                    }
                }
                *my_votes = votes.to_vec();
            }
        }
        if let Some(session) = &mut self.live {
            if let Some(msgs) = session.messages.get_mut(&chat_id) {
                if let Some((_, msg)) = msgs.iter_mut().find(|(_, m)| m.id == msg_id) {
                    apply(msg, votes);
                }
            }
        }
        if self.selected_chat == chat_id {
            if let Some(msg) = self.conversation.iter_mut().find(|m| m.id == msg_id) {
                apply(msg, votes);
            }
        }
    }

    pub(crate) fn request_older_page(&mut self, chat_id: u64) {
        if let Some(session) = &mut self.live {
            session.history_armed.remove(&chat_id);
        }
        let anchor = self.live.as_ref().and_then(|session| {
            let jid = session.jids.get(&chat_id)?.clone();
            let store = session.store.as_ref()?;
            let rows = store.messages(&jid).ok()?;
            rows.into_iter()
                .min_by_key(|(m, _)| m.timestamp)
                .map(|(m, _)| (jid, m.id, m.from_me, m.timestamp))
        });
        let Some((chat_jid, oldest_message_id, oldest_from_me, oldest_ts)) = anchor else {
            return;
        };
        if let Some(session) = &mut self.live {
            session.history.insert(chat_id, HistoryFetch::Fetching);
        }
        self.send_backend(BackendCommand::FetchOlderMessages {
            chat_jid,
            oldest_message_id,
            oldest_from_me,
            oldest_timestamp_ms: oldest_ts.saturating_mul(1000),
        });
    }

    /// Starts the new-chat dialog's lookup for the number as
    /// typed (design standard section 23). The dialog tracks
    /// the in-flight input so a late outcome for a replaced
    /// input is ignored by the event handlers.
    pub(crate) fn start_new_chat_lookup(&mut self) {
        let phone = self.new_chat.phone.trim().to_owned();
        if phone.is_empty() || self.new_chat.checking.is_some() {
            return;
        }
        self.new_chat.not_found = None;
        self.new_chat.error = None;
        self.new_chat.checking = Some(phone.clone());
        self.send_backend(BackendCommand::LookupContact { phone });
    }

    /// A number lookup resolved to a person (design standard
    /// section 23): the conversation opens. One person, one
    /// chat: when the roster already holds a chat under either
    /// of the person's address forms, that chat is selected
    /// as-is (its known title stands; the lookup never renames
    /// a chat the session already knows). Otherwise the
    /// resolved chat enters through the normal upsert path and
    /// is selected, and its picture is asked for through the
    /// avatar pipeline.
    pub(crate) fn apply_contact_resolved(
        &mut self,
        chat: backend::LiveChat,
        alt_jid: Option<String>,
    ) {
        self.new_chat.checking = None;
        self.new_chat.not_found = None;
        self.new_chat.error = None;
        let known = |jid: &str| {
            let id = jid_id(jid);
            self.live.as_ref().is_some_and(|s| s.jids.contains_key(&id)).then_some(id)
        };
        let existing = known(&chat.jid).or_else(|| alt_jid.as_deref().and_then(known));
        let target = match existing {
            Some(id) => id,
            None => {
                let id = jid_id(&chat.jid);
                self.apply_chat_upsert(&chat);
                id
            }
        };
        self.new_chat.open = false;
        self.new_chat.phone.clear();
        self.select_chat_now(target);
    }
}

#[cfg(test)]
mod channel_tests {
    use super::*;
    use crate::state::{App, Args};

    const CHANNEL_JID: &str = "10000000003@newsletter";
    const OWN_JID: &str = "10000000008@newsletter";

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
            store: Some(store::Store::open_in_memory().unwrap()),
            jids: HashMap::new(),
            messages: HashMap::new(),
            times: HashMap::new(),
            fake: true,
            logout_requested: false,
            flags: HashMap::new(),
            engaged_chats: std::collections::HashSet::new(),
            poll_ballots: HashMap::new(),
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

    pub(super) fn channel_chat(jid: &str, title: &str) -> BackendEvent {
        BackendEvent::ChatUpsert(LiveChat {
            jid: jid.to_owned(),
            title: title.to_owned(),
            kind: LiveChatKind::Channel,
            preview: String::new(),
            preview_sender: None,
            timestamp: 1_700_000_100,
            unread: 0,
            archived: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
        })
    }

    pub(super) fn channel_meta(jid: &str, role: backend::LiveChannelRole) -> backend::LiveChannel {
        backend::LiveChannel {
            jid: jid.to_owned(),
            name: "City News".to_owned(),
            description: "City news and events".to_owned(),
            followers: 12_483,
            verified: true,
            role,
            muted: false,
            invite_code: Some("CityNewsDemo".to_owned()),
            created_at: 1_680_000_000,
        }
    }

    /// Files the channel's chat row and opens it; returns the
    /// chat id. Opening sends the watch and first history
    /// page commands, which the command receiver holds.
    pub(super) fn open_channel(
        app: &mut App,
        tx: &std::sync::mpsc::Sender<BackendEvent>,
        ctx: &egui::Context,
        jid: &str,
    ) -> u64 {
        tx.send(channel_chat(jid, "City News")).unwrap();
        drain_all(app, ctx);
        let chat_id = jid_id(jid);
        app.select_chat_now(chat_id);
        chat_id
    }

    pub(super) fn poll_message() -> LiveMessage {
        let mut msg = LiveMessage::text(
            "fake-ch-4",
            CHANNEL_JID,
            Some("City News".to_owned()),
            "",
            1_699_995_000,
            false,
        );
        msg.server_id = Some(104);
        msg.kind = backend::LiveMessageKind::Poll {
            question: "Should the night market run every weekend?".to_owned(),
            options: vec![("Every weekend".to_owned(), 412), ("Once a month".to_owned(), 187)],
            multiple: false,
            my_votes: Vec::new(),
            correct: None,
        };
        msg
    }

    pub(super) fn mirror_message(app: &App, chat_id: u64, mid: u64) -> Message {
        app.live
            .as_ref()
            .unwrap()
            .messages
            .get(&chat_id)
            .unwrap()
            .iter()
            .find(|(_, m)| m.id == mid)
            .unwrap()
            .1
            .clone()
    }

    /// The subscribed list caches metadata by JID, and the
    /// history latch settles: an unfinished page returns the
    /// chat to idle, an exhausted one latches.
    #[test]
    pub(super) fn channels_listed_caches_meta_and_history_latch_settles() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = open_channel(&mut app, &tx, &ctx, CHANNEL_JID);
        assert_eq!(
            app.live.as_ref().unwrap().channel_history.get(&chat_id),
            Some(&HistoryFetch::Fetching),
            "opening a channel starts its first history page"
        );
        tx.send(BackendEvent::ChannelsListed {
            channels: vec![channel_meta(CHANNEL_JID, backend::LiveChannelRole::Follower)],
        })
        .unwrap();
        tx.send(BackendEvent::ChannelHistoryFetched {
            jid: CHANNEL_JID.to_owned(),
            added: 5,
            end_of_history: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert_eq!(
            session.channel_meta.get(CHANNEL_JID).map(|m| m.followers),
            Some(12_483),
            "the metadata is cached by JID"
        );
        assert!(
            !session.channel_history.contains_key(&chat_id),
            "an unfinished page returns the latch to idle"
        );
        app.fetch_channel_page(CHANNEL_JID, Some(102));
        tx.send(BackendEvent::ChannelHistoryFetched {
            jid: CHANNEL_JID.to_owned(),
            added: 1,
            end_of_history: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.live.as_ref().unwrap().channel_history.get(&chat_id),
            Some(&HistoryFetch::Exhausted),
            "the last page latches the history as exhausted"
        );
    }

    /// A live counter payload resolves its update by server
    /// ID and replaces the reaction counts and the poll
    /// tallies (matched back to options by protocol hash).
    #[test]
    pub(super) fn channel_counters_apply_by_server_id() {
        use sha2::Digest as _;
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = open_channel(&mut app, &tx, &ctx, CHANNEL_JID);
        tx.send(BackendEvent::MessageUpsert(poll_message())).unwrap();
        drain_all(&mut app, &ctx);
        let mid = message_id(CHANNEL_JID, "fake-ch-4");
        let hash = |name: &str| -> [u8; 32] { sha2::Sha256::digest(name.as_bytes()).into() };
        tx.send(BackendEvent::ChannelCountersUpdated {
            counters: backend::LiveChannelCounters {
                jid: CHANNEL_JID.to_owned(),
                server_id: 104,
                reactions: vec![("👍".to_owned(), 3)],
                poll_votes: vec![(hash("Every weekend"), 413), (hash("Once a month"), 187)],
            },
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let msg = mirror_message(&app, chat_id, mid);
        assert_eq!(msg.reactions, vec![("👍".to_owned(), 3)], "the counts are replaced");
        match &msg.kind {
            MessageKind::Poll { options, .. } => {
                assert_eq!(options[0].1, 413, "the hash-matched tally moved");
                assert_eq!(options[1].1, 187, "the other tally is untouched");
            }
            other => panic!("expected the poll, got {other:?}"),
        }
        let visible = app.conversation.iter().find(|m| m.id == mid).expect("the poll is open");
        assert_eq!(visible.reactions, vec![("👍".to_owned(), 3)], "the open view follows");
    }

    /// A channel poll vote is optimistic and keyed by server
    /// ID; its failure restores the previous selection and
    /// tallies, and speaks through the banner.
    #[test]
    pub(super) fn channel_poll_vote_failure_rolls_back() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let chat_id = open_channel(&mut app, &tx, &ctx, CHANNEL_JID);
        tx.send(BackendEvent::MessageUpsert(poll_message())).unwrap();
        drain_all(&mut app, &ctx);
        let mid = message_id(CHANNEL_JID, "fake-ch-4");
        let _ = sent_commands(&cmd_rx);
        app.vote_poll(mid, 0);
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(|c| matches!(c, BackendCommand::VoteChannelPoll { server_id: 104, options, .. } if options == &vec!["Every weekend".to_owned()])),
            "the vote goes out as option names on the server ID: {sent:?}"
        );
        let msg = app.conversation.iter().find(|m| m.id == mid).unwrap();
        match &msg.kind {
            MessageKind::Poll { options, my_votes, .. } => {
                assert_eq!(my_votes, &vec![0], "the optimistic ballot shows");
                assert_eq!(options[0].1, 413, "the optimistic tally moved");
            }
            other => panic!("expected the poll, got {other:?}"),
        }
        tx.send(BackendEvent::ChannelPollVoteFailed {
            jid: CHANNEL_JID.to_owned(),
            server_id: 104,
            message: "That vote could not be sent: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let msg = mirror_message(&app, chat_id, mid);
        match &msg.kind {
            MessageKind::Poll { options, my_votes, .. } => {
                assert!(my_votes.is_empty(), "the ballot is restored");
                assert_eq!(options[0].1, 412, "the tally is restored");
            }
            other => panic!("expected the poll, got {other:?}"),
        }
        assert_eq!(
            app.notice.as_ref().map(|n| n.kind),
            Some(crate::state::NoticeKind::Error),
            "the failure reaches the banner"
        );
    }

    /// A channel reaction sends the channel command with the
    /// update's server ID; its failure restores the previous
    /// reaction state.
    #[test]
    pub(super) fn channel_reaction_failure_rolls_back() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let chat_id = open_channel(&mut app, &tx, &ctx, CHANNEL_JID);
        let mut msg = LiveMessage::text(
            "fake-ch-1",
            CHANNEL_JID,
            Some("City News".to_owned()),
            "Morning briefing",
            1_699_900_000,
            false,
        );
        msg.server_id = Some(101);
        msg.reactions = vec![("👍".to_owned(), 14)];
        tx.send(BackendEvent::MessageUpsert(msg)).unwrap();
        drain_all(&mut app, &ctx);
        let mid = message_id(CHANNEL_JID, "fake-ch-1");
        let _ = sent_commands(&cmd_rx);
        app.apply_reaction(mid, "❤️");
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(|c| matches!(c, BackendCommand::ReactToChannelUpdate { server_id: 101, emoji, .. } if emoji == "❤️")),
            "the reaction goes out on the server ID: {sent:?}"
        );
        assert_eq!(app.my_reactions.get(&mid).map(String::as_str), Some("❤️"));
        tx.send(BackendEvent::ChannelReactionFailed {
            jid: CHANNEL_JID.to_owned(),
            server_id: 101,
            message: "That reaction could not be sent: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!app.my_reactions.contains_key(&mid), "my reaction is rolled back");
        let msg = mirror_message(&app, chat_id, mid);
        assert_eq!(msg.reactions, vec![("👍".to_owned(), 14)], "the counts are restored");
    }

    /// A created channel opens: the create form closes and
    /// the new channel's chat becomes the selection.
    #[test]
    pub(super) fn channel_created_opens_the_new_channel() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.channel_create_open = true;
        tx.send(channel_chat(OWN_JID, "Weekend Builds")).unwrap();
        tx.send(BackendEvent::ChannelCreated {
            channel: channel_meta(OWN_JID, backend::LiveChannelRole::Owner),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!app.channel_create_open, "the form closes");
        assert_eq!(app.selected_chat, jid_id(OWN_JID), "the new channel opens");
        assert_eq!(
            app.live.as_ref().unwrap().channel_meta.get(OWN_JID).map(|m| m.role),
            Some(backend::LiveChannelRole::Owner),
            "the created metadata is cached"
        );
    }

    /// Following from the panel's preview card caches the
    /// followed metadata and clears the panel notice; a
    /// lookup failure lands in the panel, in place.
    #[test]
    pub(super) fn channel_follow_and_lookup_failure_lands_in_the_panel() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::ChannelLookupFailed {
            message: "That channel could not be found. Check the link and try again.".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.channel_found.is_none(), "no card for a failed lookup");
        assert!(
            app.channel_panel_notice.as_deref().is_some_and(|n| n.contains("could not be found")),
            "the failure is stated in the panel: {:?}",
            app.channel_panel_notice
        );
        tx.send(BackendEvent::ChannelFound {
            channel: channel_meta(CHANNEL_JID, backend::LiveChannelRole::Guest),
        })
        .unwrap();
        tx.send(BackendEvent::ChannelFollowed {
            channel: channel_meta(CHANNEL_JID, backend::LiveChannelRole::Follower),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.channel_panel_notice.is_none(), "the follow clears the panel notice");
        assert_eq!(
            app.channel_found.as_ref().map(|c| c.role),
            Some(backend::LiveChannelRole::Follower),
            "the card flips to following"
        );
    }
}
