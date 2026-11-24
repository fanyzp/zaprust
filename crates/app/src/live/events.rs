//! The inbound path: draining backend events each frame and
//! applying them to the mirror (chat upserts, renames, call
//! recording), plus conversation re-sync.

use super::*;

impl crate::state::App {
    /// Loads the archive's persisted call history into the Calls
    /// panel's live list. Called when a live session starts, the
    /// same point the archived chats load; the loaded call IDs
    /// seed the recorded set, so a call the archive already holds
    /// (a missed call replayed again on a later reconnect) is
    /// never entered twice.
    pub(super) fn load_call_history(&mut self) {
        let records = self
            .live
            .as_ref()
            .and_then(|session| session.store.as_ref())
            .and_then(|store| store.call_history().ok());
        if let Some(records) = records {
            for record in &records {
                self.calls.recorded_ids.insert(record.call_id.clone());
            }
            self.calls.live_entries = records.iter().map(to_call_entry).collect();
        }
    }

    /// Records the tracked call that just ended: the record is
    /// built from the call's own tracked state (direction,
    /// whether it connected, whether the user declined it) and
    /// the Ended event's final duration.
    pub(super) fn record_ended_call(&mut self, call_id: &str, chat_jid: &str, seconds: u32) {
        let record = {
            let Some(call) = &self.calls.active else {
                return;
            };
            if call.call_id != call_id {
                return;
            }
            call.history_record(chat_jid, seconds)
        };
        self.record_call(record);
    }

    /// Records a call that ended on another of the account's
    /// devices (see [`BackendEvent::CallEndedElsewhere`]). When
    /// the app is showing that call ringing, the overlay ends
    /// with the elsewhere outcome and the entry is built from
    /// the call's tracked state; a call this device never
    /// showed (another call held the line, or it rang and
    /// ended while this device was away) is recorded directly.
    /// Either way the entry is an incoming call that is not
    /// missed: it was answered or declined on the other
    /// device, and no talk time accrued here.
    pub(super) fn record_elsewhere_call(
        &mut self,
        call_id: &str,
        chat_jid: &str,
        answered: bool,
        at: i64,
    ) {
        if self.on_call_ended_elsewhere(call_id, answered) {
            self.record_ended_call(call_id, chat_jid, 0);
        } else if !self.calls.active.as_ref().is_some_and(|c| c.call_id == call_id) {
            // Not the call on screen (or none is): a call that
            // never rang here still belongs in the history. A
            // call that IS on screen but already left the
            // ringing phase is left alone instead: its own end
            // event records it, with its real duration.
            self.record_call(backend::CallRecord {
                id: 0,
                call_id: call_id.to_owned(),
                chat_jid: chat_jid.to_owned(),
                video: false,
                outgoing: false,
                missed: false,
                duration_secs: 0,
                started_at: at,
            });
        }
    }

    /// Records a missed call the app never showed ringing (see
    /// [`BackendEvent::CallMissed`]): an incoming entry with no
    /// duration, stamped when it rang. The signal carries no
    /// media kind, so the entry is a voice call.
    pub(super) fn record_missed_call(&mut self, call_id: &str, chat_jid: &str, at: i64) {
        self.record_call(backend::CallRecord {
            id: 0,
            call_id: call_id.to_owned(),
            chat_jid: chat_jid.to_owned(),
            video: false,
            outgoing: false,
            missed: true,
            duration_secs: 0,
            started_at: at,
        });
    }

    /// Writes one call record into the history: the archive row
    /// (which survives restarts) and the in-memory live list the
    /// Calls panel renders, kept newest first in the archive's
    /// read-back order. Each call ID is recorded once per run; a
    /// repeat report of the same call changes nothing.
    pub(super) fn record_call(&mut self, mut record: backend::CallRecord) {
        if !self.calls.recorded_ids.insert(record.call_id.clone()) {
            return;
        }
        if let Some(session) = &self.live {
            if let Some(store) = &session.store {
                if let Ok(Some(row)) = store.record_call(&record) {
                    record.id = row;
                }
            }
        }
        if record.id == 0 {
            // No archive row backs this entry (the archive is
            // unavailable, or it already held this call from an
            // earlier run): the in-memory entry still needs a
            // stable ID of its own.
            record.id = jid_id(&record.call_id);
        }
        let entry = to_call_entry(&record);
        let pos = self
            .calls
            .live_entries
            .iter()
            .position(|e| e.timestamp <= entry.timestamp)
            .unwrap_or(self.calls.live_entries.len());
        self.calls.live_entries.insert(pos, entry);
    }

    /// Rebuilds the rendered conversation from the live mirror.
    /// Inside an event batch the rebuild is deferred to the end of
    /// the batch (see [`EventBatch`]): a burst touching the open
    /// chat would otherwise pay a full clone of the conversation
    /// per event. Nothing renders mid-batch, so the deferred
    /// rebuild is indistinguishable from the immediate one.
    pub(crate) fn sync_conversation(&mut self) {
        if self.event_batch.active {
            self.event_batch.conversation_dirty = true;
            return;
        }
        self.sync_conversation_now();
    }

    /// The immediate conversation rebuild behind
    /// [`App::sync_conversation`].
    pub(super) fn sync_conversation_now(&mut self) {
        if !self.live_mode {
            return;
        }
        let msgs = self
            .live
            .as_ref()
            .and_then(|s| s.messages.get(&self.selected_chat))
            .map(|v| v.iter().map(|(_, m)| m.clone()).collect())
            .unwrap_or_default();
        self.conversation = msgs;
    }

    /// Drains backend events and applies them, in budgeted
    /// batches. Called every frame. Events are applied strictly
    /// in arrival order and none are dropped: whatever does not
    /// fit this frame's budget stays in `event_queue`, and a
    /// repaint is requested so the queue keeps draining on the
    /// following frames without waiting for user input.
    pub(crate) fn poll_backend(&mut self, ctx: &egui::Context) {
        // The sync row's quiet-window fallback: the library's
        // completion event covers the offline drain, and a
        // catch-up can end without one (a reconnect with an
        // empty backlog produces no end marker), which used to
        // latch the row forever. After a long quiet with an
        // empty event queue the row retires, but the session
        // does not count as synced: only a real completion
        // latches that.
        if let Some(session) = &mut self.live {
            if session.syncing {
                // Sync honesty (QC1614-A0-5): the quiet fallback
                // may only retire the visual row after a long
                // quiet (real inter-chunk gaps run minutes on a
                // populated roster) and only while no backend
                // events are still queued unapplied. It never
                // latches synced_once: only a real completion
                // event may do that.
                let quiet = session
                    .sync_last_activity
                    .is_some_and(|last| last.elapsed() > std::time::Duration::from_secs(180));
                if quiet && self.event_queue.is_empty() {
                    session.syncing = false;
                    session.sync_last_activity = None;
                }
            }
        }
        // Top the queue up from the channel. Moving an event is
        // cheap; applying it is what costs, and that is budgeted
        // below. A closed channel is remembered for this frame:
        // the backend's run has ended, and once everything it
        // queued has been applied a logged-out session is torn
        // down (see `end_live_session`). std's channel returns
        // every queued event before reporting the closure, so
        // nothing the backend said is lost by tearing down after
        // the drain.
        let mut backend_gone = false;
        if let Some(session) = &self.live {
            loop {
                match session.channels.events.try_recv() {
                    Ok(event) => self.event_queue.push_back(event),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        backend_gone = true;
                        break;
                    }
                }
            }
        }
        if !self.event_queue.is_empty() {
            let started = std::time::Instant::now();
            let mut applied = 0usize;
            self.event_batch.active = true;
            while let Some(event) = self.event_queue.pop_front() {
                self.trace_event(&event);
                self.apply_event(ctx, event);
                applied += 1;
                // At least one event always applies, so a single
                // slow event can never starve the queue.
                if applied >= MAX_EVENTS_PER_FRAME || started.elapsed() >= EVENT_FRAME_BUDGET {
                    break;
                }
            }
            self.finish_event_batch();
            if !self.event_queue.is_empty() {
                ctx.request_repaint();
            }
        }
        // A closed channel after a logout means the session is
        // over: drop it so the app returns to a start-fresh
        // connect state instead of holding a dead session it can
        // never re-link from. A closure with no logout (the
        // supervisor gave up after repeated crashes) keeps the
        // session and its failure state on screen.
        if backend_gone && self.event_queue.is_empty() {
            let ended_by_logout = self.live.as_ref().is_some_and(|session| {
                session.logout_requested || matches!(session.status, LinkStatus::LoggedOut)
            });
            if ended_by_logout {
                self.end_live_session();
            }
        }
        // QR payloads die on the validity window the library
        // reported with them; drop an expired one so a dead code
        // is never presented as scannable (the same discipline as
        // the pairing-code sweep below).
        self.sweep_qr_expiry();
        // The presence watch follows the open conversation (and
        // the group member fetch that feeds its header count);
        // reconciling it here, once per frame, covers every
        // selection path without each one having to know.
        self.sync_presence_watch();
        // Pairing codes die after about three minutes. Clear an
        // expired code and return the panel to the request form so a
        // dead code is never presented as live (logic review F13).
        if let Some(session) = &mut self.live {
            let expired = session.pair_code.is_some()
                && session.pair_code_at.is_some_and(|at| at.elapsed() > PAIR_CODE_TTL);
            if expired {
                session.pair_code = None;
                session.pair_code_at = None;
                if !session.status.is_linked() {
                    self.pair_code_shown = false;
                    self.notice = Some(Notice::info(
                        "That pairing code expired. Request a new one.".to_owned(),
                    ));
                }
            }
        }
        if let Some(id) = self.pending_select_chat {
            // The deferred selection resolves only to the exact
            // chat id it was given. There is deliberately no
            // fallback: an earlier version treated a small id as
            // a 1-based index into the chat list, which silently
            // opened an unrelated chat when the requested one
            // never arrived (audit finding 15). An id that never
            // resolves simply stays pending.
            if self.chats.iter().any(|c| c.id == id) {
                self.pending_select_chat = None;
                self.select_chat_now(id);
            }
        }
    }

    /// Runs the side effects one event batch deferred (see
    /// [`EventBatch`]): sort each touched chat's messages once,
    /// flush the batch's archive writes in a single transaction,
    /// sort the chat list once, and rebuild the open conversation
    /// once. Runs before anything renders, so the state the user
    /// sees after a burst is exactly the state the old per-event
    /// application produced.
    pub(super) fn finish_event_batch(&mut self) {
        let batch = std::mem::take(&mut self.event_batch);
        if !batch.active {
            return;
        }
        if let Some(session) = &mut self.live {
            for chat_id in &batch.unsorted_chats {
                if let Some(entry) = session.messages.get_mut(chat_id) {
                    entry.sort_by_key(|(ts, _)| *ts);
                }
            }
            if !batch.store_writes.is_empty() {
                if let Some(store) = &session.store {
                    // Repeated upserts of one message inside the
                    // batch collapse to the newest copy. Flags are
                    // read from the flag map as it stands now, so
                    // a star or revoke applied later in the batch
                    // is part of the stored row.
                    // Delivery advances the batch applied after
                    // an upsert (a receipt in the same burst)
                    // live in the mirror, not in the upsert copy
                    // the batch recorded; overlay them so the
                    // flush cannot write a stale tick state over
                    // the receipt's own store write.
                    let mirror_delivery: HashMap<u64, backend::DeliveryState> = session
                        .messages
                        .values()
                        .flat_map(|entry| entry.iter())
                        .filter_map(|(_, m)| m.delivery.map(|d| (m.id, backend_delivery(d))))
                        .collect();
                    let mut latest: HashMap<u64, (LiveMessage, i64)> = HashMap::new();
                    let mut order: Vec<u64> = Vec::new();
                    for mut msg in batch.store_writes {
                        let mid = message_id(&msg.chat_jid, &msg.id);
                        if !latest.contains_key(&mid) {
                            order.push(mid);
                        }
                        if let Some(d) = mirror_delivery.get(&mid) {
                            msg.delivery = backend::DeliveryState::max_opt(msg.delivery, Some(*d));
                        }
                        let flags = session.flags.get(&mid).copied().unwrap_or(0);
                        latest.insert(mid, (msg, flags));
                    }
                    let rows: Vec<(LiveMessage, i64)> =
                        order.into_iter().filter_map(|mid| latest.remove(&mid)).collect();
                    let _ = store.upsert_messages(&rows);
                }
            }
            if !batch.poll_tally_writes.is_empty() {
                if let Some(store) = &session.store {
                    // After the upserts, per the batch contract:
                    // a tally for a poll created in this same
                    // batch lands on the row the flush just wrote.
                    for tally in &batch.poll_tally_writes {
                        let _ = store.set_poll_tally(
                            &tally.chat_jid,
                            &tally.message_id,
                            &tally.counts,
                            &tally.my_votes,
                            &tally.ballots,
                        );
                    }
                }
            }
            if !batch.event_tally_writes.is_empty() {
                if let Some(store) = &session.store {
                    // Same contract as the poll tallies above:
                    // after the upserts, last snapshot wins.
                    for tally in &batch.event_tally_writes {
                        let _ = store.set_event_tally(
                            &tally.chat_jid,
                            &tally.message_id,
                            tally.going,
                            tally.maybe,
                            tally.not_going,
                            tally.my_rsvp,
                            &tally.ballots,
                        );
                    }
                }
            }
            if !batch.chat_rows_dirty.is_empty() {
                if let Some(store) = &session.store {
                    // Written from the mirror as it stands now,
                    // so the stored count is the badge the list
                    // shows after the whole batch, whatever order
                    // the increments and read changes arrived in.
                    // A chat the batch went on to delete is no
                    // longer in the list and is skipped: its rows
                    // are already gone.
                    for chat_id in &batch.chat_rows_dirty {
                        let Some(jid) = session.jids.get(chat_id) else { continue };
                        let Some(chat) = self.chats.iter().find(|c| c.id == *chat_id) else {
                            continue;
                        };
                        let ts = session.times.get(chat_id).copied().unwrap_or(0);
                        let _ = store.upsert_chat(&chat_to_live(chat, jid, ts));
                    }
                }
            }
        }
        if batch.chats_dirty {
            self.sort_live_chats_now();
        }
        if batch.conversation_dirty {
            self.sync_conversation_now();
        }
    }

    pub(super) fn apply_event(&mut self, ctx: &egui::Context, event: BackendEvent) {
        match event {
            BackendEvent::QrCode { payload, valid_for } => {
                if let Some(session) = &mut self.live {
                    if let Ok(matrix) = backend::qr_matrix(&payload) {
                        session.qr = Some(matrix);
                        session.qr_expires_at = Some(std::time::Instant::now() + valid_for);
                    }
                    if !session.status.is_linked() {
                        session.status = LinkStatus::WaitingQr;
                    }
                }
            }
            BackendEvent::QrExpired => {
                if let Some(session) = &mut self.live {
                    session.clear_qr();
                    // The library issues no further codes for this
                    // session. Unless a pairing code is still on
                    // screen (that flow outlives the QR rotation
                    // budget), the link attempt is over: say so as
                    // a failure instead of waiting forever.
                    if !session.status.is_linked()
                        && !matches!(session.status, LinkStatus::CodeReady)
                    {
                        session.status = LinkStatus::Failed(
                            "The QR code expired before it was scanned. \
                             Restart Zaprust to link."
                                .to_owned(),
                        );
                    }
                }
            }
            BackendEvent::PairCode { code } => {
                if let Some(session) = &mut self.live {
                    session.pair_code = Some(code);
                    session.pair_code_at = Some(std::time::Instant::now());
                    if !session.status.is_linked() {
                        session.status = LinkStatus::CodeReady;
                    }
                }
            }
            BackendEvent::Connected { account } => {
                // Chats whose titles are still raw identifiers,
                // collected before the mutable session borrow:
                // they are reported for resolution below.
                let raw_titles: Vec<String> = match &self.live {
                    Some(session) => self
                        .chats
                        .iter()
                        .filter(|chat| is_raw_identifier(&chat.title))
                        .filter_map(|chat| session.jids.get(&chat.id).cloned())
                        .collect(),
                    None => Vec::new(),
                };
                if let Some(session) = &mut self.live {
                    session.status = LinkStatus::Linked;
                    session.account = account;
                    // The notification history gate compares
                    // message stamps against this moment (see
                    // `notify::is_history`): everything stamped
                    // earlier is catch-up, on the first connect
                    // and on every reconnect alike.
                    session.connected_at =
                        Some(i64::try_from(crate::state::now_unix()).unwrap_or(0));
                    // Every (re)connect starts a catch-up: the
                    // backend reports progress and completion as
                    // sync events, and the status row shows the
                    // sync until it completes (audit S12).
                    session.syncing = true;
                    session.sync_last_activity = Some(std::time::Instant::now());
                    session.clear_qr();
                    // A used code must never resurface on the
                    // connect screen (logic review F13).
                    session.pair_code = None;
                    session.pair_code_at = None;
                    // The raw-titled chats collected above are
                    // reported for resolution: a later launch
                    // gets no fresh history sync to heal from,
                    // so the connect itself is the trigger (the
                    // backend retries each fetch a bounded
                    // number of times).
                    if !raw_titles.is_empty() {
                        let _ = session
                            .channels
                            .commands
                            .send(BackendCommand::ResolveTitles { jids: raw_titles });
                    }
                }
                if self.screen == crate::state::Screen::Connect {
                    self.screen = crate::state::Screen::Chats;
                }
            }
            BackendEvent::LoggedOut => {
                let fake = self.live.as_ref().is_some_and(|session| session.fake);
                if let Some(session) = &mut self.live {
                    session.status = LinkStatus::LoggedOut;
                    session.clear_qr();
                }
                self.notice = Some(Notice::info("The device was logged out.".to_owned()));
                // A logged-out client belongs on the link screen,
                // not on an empty chat list.
                if self.live_mode {
                    self.screen = crate::state::Screen::Connect;
                }
                if fake {
                    // The fake backend answers a disconnect with
                    // this event and then keeps running, so the
                    // session can end right here: shut the fake
                    // down (its supervisor exits with the run) and
                    // drop the session. The real backend ends its
                    // own run on logout; its session is dropped by
                    // `poll_backend` when the event channel closes,
                    // after the deregistration failure it reports
                    // behind this event has been applied.
                    self.send_backend(BackendCommand::Shutdown);
                    self.end_live_session();
                }
            }
            BackendEvent::TemporaryBan { code } => {
                if let Some(session) = &mut self.live {
                    session.status = LinkStatus::Banned(code);
                    session.clear_qr();
                }
            }
            BackendEvent::ChatUpsert(chat) => self.apply_chat_upsert(&chat),
            BackendEvent::CommunityCreated { jid, name } => {
                self.register_created_community(&jid, &name);
            }
            BackendEvent::CommunityGroupsLinked { failed, .. } => {
                // The structure itself lands through the
                // ChatCommunity events the backend emits per
                // linked group; this arm only speaks for the
                // groups the server refused.
                if !failed.is_empty() {
                    self.notice = Some(Notice::info(crate::i18n::tf(
                        "community.link_failed",
                        "count",
                        &failed.len().to_string(),
                    )));
                }
            }
            BackendEvent::CommunityGroupsUnlinked { failed, .. } => {
                if !failed.is_empty() {
                    self.notice = Some(Notice::info(crate::i18n::tf(
                        "community.unlink_failed",
                        "count",
                        &failed.len().to_string(),
                    )));
                }
            }
            BackendEvent::CommunitySubgroupCreated { group, .. } => {
                // The roster entry carries its community parent,
                // so the plain upsert path files it under the
                // community like any healed subgroup.
                self.apply_chat_upsert(&group);
            }
            BackendEvent::CommunityParticipantsRemoved { .. } => {
                // The parent group's refreshed member list
                // follows as a GroupMembers event and does the
                // visible work; nothing else to apply.
            }
            BackendEvent::CommunityDeactivated { community_jid } => {
                self.apply_community_deactivated(&community_jid);
            }
            BackendEvent::MessageUpsert(msg) => {
                // Applying an upsert never triggers a network
                // fetch: a link preview renders only when the
                // sender's client attached one to the message.
                self.apply_message_upsert(&msg);
            }
            BackendEvent::MessageSent { chat_jid, message_id } => {
                self.apply_message_sent(&chat_jid, &message_id);
            }
            BackendEvent::Receipt { chat_jid, message_ids, kind, at, actor } => {
                let chat_id = jid_id(&chat_jid);
                for pid in &message_ids {
                    let mid = message_id(&chat_jid, pid);
                    let mut delivery = match kind {
                        ReceiptKind::Sent => Delivery::Sent,
                        ReceiptKind::Delivered => Delivery::Delivered,
                        ReceiptKind::Read | ReceiptKind::Played => Delivery::Read,
                    };
                    // Group correctness: one member's receipt is
                    // not everyone's. A named actor's Read in a
                    // group advances the ticks to Delivered at
                    // most; Read in a group is the aggregate the
                    // library reports without a single actor.
                    let is_group = self
                        .chats
                        .iter()
                        .find(|c| c.id == chat_id)
                        .is_some_and(|c| c.kind == core::model::ChatKind::Group);
                    if is_group && actor.is_some() && delivery == Delivery::Read {
                        delivery = Delivery::Delivered;
                    }
                    // First sight of this message's history:
                    // record the insertion order the size cap
                    // prunes by.
                    if !self.receipt_log.contains_key(&mid) {
                        self.receipt_log_order.push_back(mid);
                    }
                    let log = self.receipt_log.entry(mid).or_default();
                    log.push((kind, fmt_time(at)));
                    // The receipt log only feeds the message-info
                    // popover; keep it bounded (logic review F21).
                    if log.len() > 32 {
                        log.drain(..log.len() - 32);
                    }
                    self.prune_receipt_log();
                    let mut applied = false;
                    if let Some(entry) =
                        self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id))
                    {
                        for (_, m) in entry.iter_mut() {
                            if m.id == mid && m.delivery.is_some() {
                                // Receipts only move forward.
                                let rank = |d: Delivery| match d {
                                    Delivery::Failed | Delivery::Sending => 0,
                                    Delivery::Sent => 1,
                                    Delivery::Delivered => 2,
                                    Delivery::Read => 3,
                                };
                                if m.delivery.is_some_and(|cur| rank(delivery) > rank(cur)) {
                                    m.delivery = Some(delivery);
                                }
                                applied = true;
                            }
                        }
                        if !applied {
                            // The library does not always upsert our
                            // own sends back, so the local echo (a
                            // Sending clock with no protocol id) would
                            // stay pending forever. Retire the oldest
                            // pending echo of this chat and bind it to
                            // the protocol id the receipt names.
                            let rank = |d: Delivery| match d {
                                Delivery::Failed | Delivery::Sending => 0,
                                Delivery::Sent => 1,
                                Delivery::Delivered => 2,
                                Delivery::Read => 3,
                            };
                            if let Some((_, echo)) = entry
                                .iter_mut()
                                .filter(|(_, m)| {
                                    m.delivery == Some(Delivery::Sending) && m.is_mine()
                                })
                                .min_by_key(|(ts, _)| *ts)
                            {
                                if rank(delivery) > rank(Delivery::Sending) {
                                    echo.delivery = Some(delivery);
                                }
                                if echo.time == "…" {
                                    echo.time = fmt_time(at);
                                }
                                echo.id = mid;
                                self.protocol_ids.insert(mid, pid.clone());
                            }
                        }
                    }
                }
                // Persist the advanced state: ticks are synced
                // state, and a restart must not lose a Read the
                // receipts already proved.
                if let Some(session) = &self.live {
                    if let Some(store) = &session.store {
                        let state = match kind {
                            ReceiptKind::Sent => backend::DeliveryState::Sent,
                            ReceiptKind::Delivered | ReceiptKind::Played => {
                                backend::DeliveryState::Delivered
                            }
                            ReceiptKind::Read => backend::DeliveryState::Read,
                        };
                        let group_named_actor = actor.is_some()
                            && self
                                .chats
                                .iter()
                                .find(|c| c.id == chat_id)
                                .is_some_and(|c| c.kind == core::model::ChatKind::Group);
                        let state = if group_named_actor && state == backend::DeliveryState::Read {
                            backend::DeliveryState::Delivered
                        } else {
                            state
                        };
                        for pid in &message_ids {
                            let _ = store.set_message_delivery(&chat_jid, pid, state);
                        }
                    }
                }
                if self.selected_chat == chat_id {
                    self.sync_conversation();
                }
            }
            BackendEvent::ReactionUpdate { chat_jid, message_id: pid, actor, emoji } => {
                // A reaction addressed to the status
                // broadcast belongs to a status in the
                // Updates feed, not to a chat message.
                if chat_jid == STATUS_BROADCAST_JID {
                    self.apply_status_reaction(&pid, &actor, &emoji);
                } else {
                    let chat_id = jid_id(&chat_jid);
                    let mid = message_id(&chat_jid, &pid);
                    let key = format!("{chat_id}:{mid}");
                    if actor == "Me" {
                        // The server has spoken for my reaction: any
                        // optimistic change awaiting settlement is
                        // confirmed by this report, whatever it says.
                        self.pending_reactions.remove(&mid);
                        if emoji.is_empty() {
                            self.my_reactions.remove(&mid);
                        } else {
                            self.my_reactions.insert(mid, emoji.clone());
                        }
                    }
                    {
                        let actors = self.reaction_actors.entry(key.clone()).or_default();
                        if emoji.is_empty() {
                            actors.remove(&actor);
                        } else {
                            actors.insert(actor.clone(), emoji.clone());
                        }
                        let counts = count_reactions(actors);
                        if let Some(entry) =
                            self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id))
                        {
                            for (_, m) in entry.iter_mut() {
                                if m.id == mid {
                                    m.reactions = counts.clone();
                                }
                            }
                        }
                        if counts.is_empty() {
                            // The last actor removed their reaction:
                            // the entry itself goes too, so the map
                            // holds only messages with live reactions
                            // instead of an empty set per message ever
                            // reacted to.
                            self.reaction_actors.remove(&key);
                        }
                    }
                    if self.selected_chat == chat_id {
                        self.sync_conversation();
                    }
                }
            }
            BackendEvent::ReactionFailed { chat_jid, message_id: pid, message } => {
                // A failed reaction to a status rolls the
                // status's optimistic reaction back, in the
                // viewer, not a chat message's.
                if chat_jid == STATUS_BROADCAST_JID {
                    self.apply_status_reaction_failed(&pid, &message);
                } else {
                    let chat_id = jid_id(&chat_jid);
                    let mid = message_id(&chat_jid, &pid);
                    // Same display treatment as the generic error
                    // path: backend text can embed server-supplied
                    // strings, so it is scrubbed before it can reach
                    // the notice banner.
                    let message = core::crash::scrub_for_display(&message);
                    tracing::warn!("reaction failed: {message}");
                    if let Some(previous) = self.pending_reactions.remove(&mid) {
                        // The change never reached the server: put my
                        // reaction back to what it was before it, in
                        // the picker state and in the actor-derived
                        // counts alike.
                        match &previous {
                            Some(emoji) => {
                                self.my_reactions.insert(mid, emoji.clone());
                            }
                            None => {
                                self.my_reactions.remove(&mid);
                            }
                        }
                        self.apply_my_reaction_to_actors(chat_id, mid, previous.as_deref());
                    }
                    self.notice = Some(Notice::error(human_error(&message)));
                }
            }
            BackendEvent::StarFailed { chat_jid, message_id: pid, message } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                // Same display treatment as the generic error
                // path: backend text can embed server-supplied
                // strings, so it is scrubbed before it can reach
                // the notice banner.
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("star failed: {message}");
                if let Some(previous) = self.pending_stars.remove(&mid) {
                    // The change never reached the server: put
                    // the star back to what it was before it,
                    // through the same adoption path a server
                    // report takes.
                    self.apply_star_changed(chat_id, mid, &pid, previous);
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::PinFailed { chat_jid, message_id: pid, message } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                // Scrubbed before display, like the star path
                // above and the generic error path.
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("pin failed: {message}");
                if let Some(previous) = self.pending_pins.remove(&mid) {
                    // The change never reached the server: put
                    // the pin back to what it was before it.
                    self.apply_pin_restored(chat_id, mid, &pid, previous);
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::MessageKept { chat_jid, message_id: pid, kept } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                self.apply_keep_changed(chat_id, mid, &pid, kept);
            }
            BackendEvent::KeepFailed { chat_jid, message_id: pid, message } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                // Scrubbed before display, like the star and pin
                // failure paths above.
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("keep failed: {message}");
                if let Some(previous) = self.pending_keeps.remove(&mid) {
                    // The change never reached the server: put
                    // the kept state back to what it was.
                    self.apply_keep_changed(chat_id, mid, &pid, previous);
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::PollVoteFailed { chat_jid, message_id: pid, message } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                // Scrubbed before display, like the star and pin
                // failure paths above.
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("poll vote failed: {message}");
                if let Some(previous) = self.pending_poll_votes.remove(&mid) {
                    // The vote never reached the server: re-apply
                    // the selection it replaced as my ballot,
                    // which recomputes the tally back in the
                    // mirror and the archive alike, then bring
                    // the open conversation's copy with it.
                    self.apply_poll_vote(&chat_jid, &pid, "", true, &previous);
                    self.sync_poll_from_mirror(chat_id, mid);
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::EditMessageFailed { chat_jid, message_id: pid, message } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("edit failed: {message}");
                if let Some((body, edited)) = self.pending_edits.remove(&mid) {
                    // The edit never reached the server: the
                    // message keeps its previous body, in the
                    // session mirror and the open view alike.
                    if let Some(entry) =
                        self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id))
                    {
                        for (_, m) in entry.iter_mut() {
                            if m.id == mid {
                                m.body = body.clone();
                                m.edited = edited;
                            }
                        }
                    }
                    if self.selected_chat == chat_id {
                        self.sync_conversation();
                    }
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::RevokeMessageFailed { chat_jid, message_id: pid, message } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("revoke failed: {message}");
                if let Some(previous) = self.pending_revokes.remove(&mid) {
                    // The revoke never reached the server: the
                    // message is not deleted for anyone, so its
                    // deleted flag goes back to what it was.
                    if let Some(entry) =
                        self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id))
                    {
                        for (_, m) in entry.iter_mut() {
                            if m.id == mid {
                                m.deleted = previous;
                            }
                        }
                    }
                    if self.selected_chat == chat_id {
                        self.sync_conversation();
                    }
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::DeleteForMeFailed { chat_jid, message_id: pid, message } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("delete for me failed: {message}");
                if let Some((snap_chat, row, flags)) = self.pending_delete_for_me.remove(&mid) {
                    // The delete never reached the server: the
                    // message is still the account's, so its
                    // archived row goes back and the mirror
                    // re-adopts it.
                    if snap_chat == chat_id {
                        self.restore_deleted_for_me(chat_id, &row, flags);
                    }
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::SpamReported { chat_jid } => {
                let chat_id = jid_id(&chat_jid);
                let name = self
                    .chats
                    .iter()
                    .find(|c| c.id == chat_id)
                    .map(|c| c.title.clone())
                    .unwrap_or_default();
                self.notice = Some(Notice::info(format!(
                    "{} {}",
                    crate::i18n::t("notice.report_sent"),
                    name
                )));
            }
            BackendEvent::SpamReportFailed { message } => {
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("spam report failed: {message}");
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::StatusPosted { text, message_id } => {
                self.apply_status_posted(text, message_id);
            }
            BackendEvent::StatusMediaPosted { status } => {
                self.apply_status_media_posted(*status);
            }
            BackendEvent::StatusPostFailed { message } => {
                self.apply_status_post_failed(message);
            }
            BackendEvent::StatusReceived { status } => {
                self.apply_status_received(*status);
            }
            BackendEvent::StatusDeleted { message_id } => {
                self.apply_status_deleted(&message_id);
            }
            BackendEvent::StatusDeleteFailed { message_id, message } => {
                self.apply_status_delete_failed(&message_id, message);
            }
            BackendEvent::PollVote { chat_jid, message_id: pid, voter_jid, from_me, selected } => {
                if from_me {
                    // My own vote reported back settles the
                    // pending change it confirms.
                    let mid = message_id(&chat_jid, &pid);
                    self.pending_poll_votes.remove(&mid);
                }
                self.apply_poll_vote(&chat_jid, &pid, &voter_jid, from_me, &selected);
            }
            BackendEvent::EventResponse {
                chat_jid,
                message_id: pid,
                responder_jid,
                from_me,
                response,
                ..
            } => {
                if from_me {
                    // My own answer reported back settles the
                    // pending change it confirms.
                    let mid = message_id(&chat_jid, &pid);
                    self.pending_event_rsvps.remove(&mid);
                }
                self.apply_event_response(&chat_jid, &pid, &responder_jid, from_me, response);
            }
            BackendEvent::EventRespondFailed { chat_jid, message_id: pid, message } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                // Scrubbed before display, like the poll vote
                // failure path above.
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("event response failed: {message}");
                if let Some(previous) = self.pending_event_rsvps.remove(&mid) {
                    // The answer never reached the server: put
                    // my previous answer back (or clear mine
                    // when there was none), which recomputes
                    // the tally in the mirror and the archive
                    // alike. With no previous answer there is
                    // nothing to re-apply: the optimistic count
                    // came from my answer alone, so restore the
                    // event from the archive copy instead.
                    match previous {
                        Some(prev) => {
                            self.apply_event_response(&chat_jid, &pid, "", true, prev);
                        }
                        None => {
                            self.apply_event_retraction(&chat_jid, &pid);
                        }
                    }
                    self.sync_event_from_mirror(chat_id, mid);
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::EventEdited {
                chat_jid,
                message_id: pid,
                name,
                description,
                start_time,
                end_time,
                location,
                join_link,
                scheduled_call,
                extra_guests,
                canceled,
            } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                // An edit echo for my own event settles the
                // pending change it confirms.
                self.pending_event_edits.remove(&mid);
                self.apply_event_edit(
                    &chat_jid,
                    &pid,
                    chat_id,
                    mid,
                    core::model::MessageKind::Event {
                        name,
                        description,
                        start_time,
                        end_time,
                        location,
                        join_link,
                        scheduled_call,
                        extra_guests,
                        canceled,
                        my_rsvp: None,
                        going: 0,
                        maybe: 0,
                        not_going: 0,
                    },
                );
            }
            BackendEvent::EventEditFailed { chat_jid, message_id: pid, message } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("event edit failed: {message}");
                if let Some(previous) = self.pending_event_edits.remove(&mid) {
                    // The edit never reached the server: the
                    // event keeps its previous fields, in the
                    // session mirror and the open view alike.
                    self.apply_event_edit(&chat_jid, &pid, chat_id, mid, previous);
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::MessageEdited { chat_jid, message_id: pid, new_body } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                self.pending_edits.remove(&mid);
                if let Some(entry) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
                    for (_, m) in entry.iter_mut() {
                        if m.id == mid {
                            m.body = new_body.clone();
                            m.edited = true;
                        }
                    }
                }
                if self.selected_chat == chat_id {
                    self.sync_conversation();
                }
            }
            BackendEvent::MessageRevoked { chat_jid, message_id: pid } => {
                if chat_jid == STATUS_BROADCAST_JID {
                    // A status revoke (my own delete landing, or
                    // an author deleting their post): the status
                    // leaves the feed or My Status.
                    self.apply_status_revoked(&pid);
                } else {
                    let chat_id = jid_id(&chat_jid);
                    let mid = message_id(&chat_jid, &pid);
                    self.pending_revokes.remove(&mid);
                    self.apply_revoke(chat_id, mid);
                }
            }
            // Channels (parity P5): metadata, discovery,
            // membership, history pagination state, live
            // counters, and the typed settle/rollback pairs of
            // design standard section 27. Successes land as
            // state changes the surface already shows; failures
            // surface where the action was attempted (the
            // panel notice, the create form, or the banner).
            BackendEvent::ChannelsListed { channels } => {
                if let Some(session) = &mut self.live {
                    for channel in channels {
                        session.channel_meta.insert(channel.jid.clone(), channel);
                    }
                }
            }
            BackendEvent::ChannelsListFailed { message } => {
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("channels list failed: {message}");
                self.channel_panel_notice = Some(human_error(&message));
            }
            BackendEvent::ChannelFound { channel } => {
                self.channel_panel_notice = None;
                self.channel_found = Some(channel);
            }
            BackendEvent::ChannelLookupFailed { message } => {
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("channel lookup failed: {message}");
                self.channel_found = None;
                self.channel_panel_notice = Some(human_error(&message));
            }
            BackendEvent::ChannelFollowed { channel } => {
                if let Some(session) = &mut self.live {
                    session.channel_meta.insert(channel.jid.clone(), channel.clone());
                }
                if self.channel_found.as_ref().is_some_and(|c| c.jid == channel.jid) {
                    self.channel_found = Some(channel);
                }
                self.channel_panel_notice = None;
            }
            BackendEvent::ChannelFollowFailed { message, .. } => {
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("channel follow failed: {message}");
                self.channel_panel_notice = Some(human_error(&message));
            }
            BackendEvent::ChannelUnfollowed { jid } => {
                if let Some(session) = &mut self.live {
                    if let Some(meta) = session.channel_meta.get_mut(&jid) {
                        meta.role = backend::LiveChannelRole::Guest;
                    }
                }
                if self.channel_found.as_ref().is_some_and(|c| c.jid == jid) {
                    if let Some(found) = &mut self.channel_found {
                        found.role = backend::LiveChannelRole::Guest;
                    }
                }
            }
            BackendEvent::ChannelUnfollowFailed { message, .. } => {
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("channel unfollow failed: {message}");
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::ChannelCreated { channel } => {
                if let Some(session) = &mut self.live {
                    session.channel_meta.insert(channel.jid.clone(), channel.clone());
                }
                self.channel_create_open = false;
                self.channel_create_name.clear();
                self.channel_create_desc.clear();
                self.channel_create_error = None;
                // The backend filed the channel's chat row
                // ahead of this event; open it.
                let chat_id = jid_id(&channel.jid);
                if self.chats.iter().any(|c| c.id == chat_id) {
                    self.select_chat_now(chat_id);
                }
            }
            BackendEvent::ChannelCreateFailed { message } => {
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("channel create failed: {message}");
                self.channel_create_error = Some(human_error(&message));
            }
            BackendEvent::ChannelHistoryFetched { jid, end_of_history, .. } => {
                let chat_id = jid_id(&jid);
                if let Some(session) = &mut self.live {
                    if end_of_history {
                        session.channel_history.insert(chat_id, HistoryFetch::Exhausted);
                    } else {
                        session.channel_history.remove(&chat_id);
                    }
                }
            }
            BackendEvent::ChannelHistoryFailed { jid, message } => {
                let chat_id = jid_id(&jid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("channel history failed: {message}");
                // Back to idle: the next open or scroll to
                // the top asks again, like a failed chat
                // history page.
                if let Some(session) = &mut self.live {
                    session.channel_history.remove(&chat_id);
                }
            }
            BackendEvent::ChannelCountersUpdated { counters } => {
                self.apply_channel_counters(&counters);
            }
            BackendEvent::ChannelReactionSet { jid, server_id, .. } => {
                let chat_id = jid_id(&jid);
                if let Some(mid) = self
                    .live
                    .as_ref()
                    .and_then(|s| s.server_ids.get(&(chat_id, server_id)).copied())
                {
                    self.pending_reactions.remove(&mid);
                    self.pending_channel_reactions.remove(&mid);
                }
            }
            BackendEvent::ChannelReactionFailed { jid, server_id, message } => {
                let chat_id = jid_id(&jid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("channel reaction failed: {message}");
                let mid = self
                    .live
                    .as_ref()
                    .and_then(|s| s.server_ids.get(&(chat_id, server_id)).copied());
                if let Some(mid) = mid {
                    if let Some(previous) = self.pending_reactions.remove(&mid) {
                        match &previous {
                            Some(emoji) => {
                                self.my_reactions.insert(mid, emoji.clone());
                            }
                            None => {
                                self.my_reactions.remove(&mid);
                            }
                        }
                    }
                    if let Some(counts) = self.pending_channel_reactions.remove(&mid) {
                        self.set_message_reactions(chat_id, mid, counts);
                    }
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::ChannelPollCreated { jid, .. } => {
                // No local echo exists for a channel poll:
                // pull the newest page so the poll lands in
                // the stream from the backend's own record.
                let chat_id = jid_id(&jid);
                if let Some(session) = &mut self.live {
                    session.channel_history.remove(&chat_id);
                }
                self.fetch_channel_page(&jid, None);
            }
            BackendEvent::ChannelPollFailed { message, .. } => {
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("channel poll create failed: {message}");
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::ChannelPollVoted { jid, server_id } => {
                let chat_id = jid_id(&jid);
                let mid = self
                    .live
                    .as_ref()
                    .and_then(|s| s.server_ids.get(&(chat_id, server_id)).copied());
                if let Some(mid) = mid {
                    if let Some(session) = &mut self.live {
                        session.pending_channel_votes.remove(&mid);
                    }
                }
            }
            BackendEvent::ChannelPollVoteFailed { jid, server_id, message } => {
                let chat_id = jid_id(&jid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("channel poll vote failed: {message}");
                let mid = self
                    .live
                    .as_ref()
                    .and_then(|s| s.server_ids.get(&(chat_id, server_id)).copied());
                if let Some(mid) = mid {
                    let previous =
                        self.live.as_mut().and_then(|s| s.pending_channel_votes.remove(&mid));
                    if let Some(previous) = previous {
                        self.set_poll_my_votes(chat_id, mid, &previous);
                    }
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::ChannelUpdateEdited { jid, .. } => {
                // Converge on the backend's record of the
                // edit, like a created poll.
                let chat_id = jid_id(&jid);
                if let Some(session) = &mut self.live {
                    session.channel_history.remove(&chat_id);
                }
                self.fetch_channel_page(&jid, None);
            }
            BackendEvent::ChannelUpdateEditFailed { jid, message_id: pid, message } => {
                let chat_id = jid_id(&jid);
                let mid = message_id(&jid, &pid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("channel update edit failed: {message}");
                if let Some((body, edited)) = self.pending_edits.remove(&mid) {
                    // The edit never reached the server: the
                    // update keeps its previous body, in the
                    // session mirror and the open view alike
                    // (the failed chat edit's discipline).
                    if let Some(entry) =
                        self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id))
                    {
                        for (_, m) in entry.iter_mut() {
                            if m.id == mid {
                                m.body = body.clone();
                                m.edited = edited;
                            }
                        }
                    }
                    if self.selected_chat == chat_id {
                        self.sync_conversation();
                    }
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::ChannelUpdateRevoked { jid, message_id: pid } => {
                let chat_id = jid_id(&jid);
                let mid = message_id(&jid, &pid);
                self.pending_revokes.remove(&mid);
                self.apply_revoke(chat_id, mid);
            }
            BackendEvent::ChannelUpdateRevokeFailed { jid, message_id: pid, message } => {
                let chat_id = jid_id(&jid);
                let mid = message_id(&jid, &pid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("channel update revoke failed: {message}");
                if let Some(previous) = self.pending_revokes.remove(&mid) {
                    if let Some(entry) =
                        self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id))
                    {
                        for (_, m) in entry.iter_mut() {
                            if m.id == mid {
                                m.deleted = previous;
                            }
                        }
                    }
                    if self.selected_chat == chat_id {
                        self.sync_conversation();
                    }
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::ChannelInfoFetched { info } => {
                if let Some(session) = &mut self.live {
                    session.channel_meta.insert(info.channel.jid.clone(), info.channel.clone());
                    session.channel_infos.insert(info.channel.jid.clone(), info);
                }
            }
            BackendEvent::ChannelInfoFailed { message, .. } => {
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("channel info failed: {message}");
                self.channel_info_for = None;
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::ChannelMuteSet { jid, muted } => {
                let chat_id = jid_id(&jid);
                if let Some(session) = &mut self.live {
                    if let Some(meta) = session.channel_meta.get_mut(&jid) {
                        meta.muted = muted;
                    }
                }
                if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                    chat.muted = muted;
                }
            }
            BackendEvent::ChannelMuteFailed { message, .. } => {
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("channel mute failed: {message}");
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::DisappearingChanged { chat_jid, duration_secs } => {
                let chat_id = jid_id(&chat_jid);
                self.pending_timers.remove(&chat_id);
                self.apply_disappearing_changed(chat_id, duration_secs);
            }
            BackendEvent::MessagePinChanged { chat_jid, message_id: pid, pinned } => {
                self.apply_server_pin_changed(&chat_jid, &pid, pinned);
            }
            BackendEvent::DisappearingTimerFailed { chat_jid, message } => {
                let chat_id = jid_id(&chat_jid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("disappearing timer failed: {message}");
                if let Some(previous) = self.pending_timers.remove(&chat_id) {
                    // The change never reached the server: the
                    // previous timer goes back quietly, with no
                    // new in-stream announcement (the one the
                    // optimistic apply posted was the failure's
                    // own; the error notice below is its
                    // correction).
                    self.set_ephemeral_state(chat_id, previous);
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::DefaultDisappearingTimerChanged { duration_secs } => {
                self.apply_default_disappearing_timer(duration_secs);
            }
            BackendEvent::MuteChanged { chat_jid, muted, mute_until } => {
                let chat_id = jid_id(&chat_jid);
                self.pending_chat_actions.remove(&(chat_id, "mute"));
                self.apply_mute_changed(chat_id, muted, mute_until);
            }
            BackendEvent::PinChanged { chat_jid, pinned } => {
                let chat_id = jid_id(&chat_jid);
                self.pending_chat_actions.remove(&(chat_id, "pin"));
                self.apply_pin_changed(chat_id, pinned);
            }
            BackendEvent::ArchiveChanged { chat_jid, archived } => {
                let chat_id = jid_id(&chat_jid);
                self.pending_chat_actions.remove(&(chat_id, "archive"));
                self.apply_archive_changed(chat_id, archived);
            }
            BackendEvent::LockChanged { chat_jid, locked } => {
                // The account's lock state for this chat, from
                // the lock action's own echo or from another
                // device: authoritative over the upsert mirror,
                // and the confirming report for a pending lock
                // action of our own (section 27).
                let chat_id = jid_id(&chat_jid);
                self.pending_lock_actions.remove(&chat_id);
                self.apply_lock_changed(chat_id, locked);
            }
            BackendEvent::StarChanged { chat_jid, message_id: pid, starred } => {
                self.stamp_sync_activity();
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                self.apply_star_changed(chat_id, mid, &pid, starred);
            }
            BackendEvent::ChatReadChanged { chat_jid, read, unread_count } => {
                self.stamp_sync_activity();
                let chat_id = jid_id(&chat_jid);
                self.pending_unread.remove(&chat_id);
                self.apply_chat_read_changed(chat_id, read, unread_count);
            }
            BackendEvent::ChatDeleted { chat_jid } => {
                let chat_id = jid_id(&chat_jid);
                if self.pending_chat_deletes.remove(&chat_id) {
                    // My own delete, confirmed: its Busy notice
                    // has done its work.
                    self.clear_busy_notice();
                }
                self.apply_chat_deleted(chat_id, &chat_jid);
            }
            BackendEvent::ChatCleared { chat_jid, delete_starred, last_message_timestamp } => {
                let chat_id = jid_id(&chat_jid);
                if self.pending_clears.remove(&chat_id).is_some() {
                    self.clear_busy_notice();
                }
                self.apply_chat_cleared(chat_id, &chat_jid, delete_starred, last_message_timestamp);
            }
            BackendEvent::MessageDeletedForMe { chat_jid, message_id: pid } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                self.pending_delete_for_me.remove(&mid);
                self.apply_message_deleted_for_me(chat_id, &chat_jid, mid, &pid);
            }
            BackendEvent::ChatActionFailed { chat_jid, action, message } => {
                let chat_id = jid_id(&chat_jid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("chat action failed: {message}");
                let group = match action {
                    backend::ChatAction::Archive | backend::ChatAction::Unarchive => "archive",
                    backend::ChatAction::Pin | backend::ChatAction::Unpin => "pin",
                    backend::ChatAction::Mute
                    | backend::ChatAction::MuteUntil(_)
                    | backend::ChatAction::Unmute => "mute",
                    backend::ChatAction::Lock | backend::ChatAction::Unlock => "lock",
                };
                if group == "lock" {
                    if let Some(was_locked) = self.pending_lock_actions.remove(&chat_id) {
                        // The lock change never reached the
                        // server: the flag goes back to its
                        // snapshot, in the mirror and the
                        // archive alike.
                        self.restore_lock_action(chat_id, was_locked);
                    }
                } else if let Some(snapshot) = self.pending_chat_actions.remove(&(chat_id, group)) {
                    // The action never reached the server: the
                    // fields it owns go back to the snapshot, in
                    // the mirror and the archive alike.
                    self.restore_chat_action(chat_id, group, snapshot);
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::ClearChatFailed { chat_jid, message } => {
                let chat_id = jid_id(&chat_jid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("clear chat failed: {message}");
                // Nothing was staged locally for this command,
                // so there is nothing to restore: the pending
                // entry and its Busy notice are all that go.
                self.pending_clears.remove(&chat_id);
                self.clear_busy_notice();
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::DeleteChatFailed { chat_jid, message } => {
                let chat_id = jid_id(&chat_jid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("delete chat failed: {message}");
                self.pending_chat_deletes.remove(&chat_id);
                self.clear_busy_notice();
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::MarkUnreadFailed { chat_jid, message } => {
                let chat_id = jid_id(&chat_jid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("mark unread failed: {message}");
                if let Some(previous) = self.pending_unread.remove(&chat_id) {
                    // The badge was mine alone: the count goes
                    // back to what it was before the mark.
                    if previous == 0 {
                        self.apply_chat_read_changed(chat_id, true, 0);
                    } else {
                        self.apply_chat_read_changed(chat_id, false, previous);
                    }
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::BlockFailed { chat_jid, message } => {
                let chat_id = jid_id(&chat_jid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("block failed: {message}");
                if let Some(previous) = self.pending_blocks.remove(&chat_id) {
                    // The toggle never reached the server: the
                    // set (and its persisted copy) goes back.
                    if previous {
                        self.blocked_chats.insert(chat_id);
                    } else {
                        self.blocked_chats.remove(&chat_id);
                    }
                    self.persist_blocked_chats();
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::GroupInfoSaved { chat_jid } => {
                let chat_id = jid_id(&chat_jid);
                if let Some((_previous, subject)) = self.pending_group_info.remove(&chat_id) {
                    // The save landed: only now does the new
                    // title apply, and only now is it announced.
                    // (Not apply_chat_renamed: that is the heal
                    // path, which never overwrites a real name; a
                    // confirmed rename replaces it outright.)
                    if let Some(subject) = subject {
                        if let Some(session) = &self.live {
                            if let Some(store) = &session.store {
                                let _ = store.rename_chat_authoritative(&chat_jid, &subject);
                            }
                        }
                        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                            chat.title = subject.clone();
                        }
                    }
                    self.notice = Some(Notice::info("Group info saved.".to_owned()));
                }
            }
            BackendEvent::GroupInfoFailed { chat_jid, message } => {
                let chat_id = jid_id(&chat_jid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("group info failed: {message}");
                // The title was never applied at click time, so
                // a failure has nothing to restore; a field that
                // did land on the server reconciles through the
                // next group metadata fetch.
                self.pending_group_info.remove(&chat_id);
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::LeaveGroupFailed { chat_jid, message } => {
                let chat_id = jid_id(&chat_jid);
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("leave group failed: {message}");
                if let Some(previous) = self.pending_leave.remove(&chat_id) {
                    // Still a member: the chat row's archived
                    // flag goes back, mirror and archive alike.
                    if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                        c.archived = previous;
                    }
                    self.persist_chat_row(chat_id);
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::PrivacyState(map) => {
                self.apply_privacy_state(map);
            }
            BackendEvent::OwnProfile(profile) => {
                self.apply_own_profile(profile);
            }
            BackendEvent::OwnProfileSaveFailed { message } => {
                self.apply_own_profile_save_failed(message);
            }
            BackendEvent::LabelEdited { label_id, name, color, predefined_id, deleted } => {
                self.apply_label_edited(label_id, name, color, predefined_id, deleted);
            }
            BackendEvent::ChatLabelChanged { label_id, chat_jid, labeled } => {
                self.apply_chat_label_changed(label_id, chat_jid, labeled);
            }
            BackendEvent::MessageLabelChanged { label_id, chat_jid, message_id, labeled } => {
                self.apply_message_label_changed(label_id, chat_jid, message_id, labeled);
            }
            BackendEvent::LabelActionFailed { action, message } => {
                self.apply_label_action_failed(action, message);
            }
            BackendEvent::LabelDeleted { label_id } => {
                self.apply_label_deleted(label_id);
            }
            BackendEvent::LabelDeleteFailed { message, .. } => {
                self.apply_label_delete_failed(message);
            }
            BackendEvent::QuickReplyEdited { id, shortcut, message, keywords, count, deleted } => {
                self.apply_quick_reply_edited(
                    backend::LiveQuickReply { id, shortcut, message, keywords, count },
                    deleted,
                );
            }
            BackendEvent::QuickReplyFailed { id, message } => {
                self.apply_quick_reply_failed(id, message);
            }
            BackendEvent::QuickReplyDeleted { id } => {
                self.apply_quick_reply_deleted(id);
            }
            BackendEvent::QuickReplyDeleteFailed { message, .. } => {
                self.apply_quick_reply_delete_failed(message);
            }
            BackendEvent::BusinessProfileFetched { chat_jid, profile } => {
                self.apply_business_profile_fetched(chat_jid, profile);
            }
            BackendEvent::BusinessProfileFetchFailed { chat_jid, message } => {
                self.apply_business_profile_fetch_failed(chat_jid, message);
            }
            BackendEvent::CatalogFetched { chat_jid, products, after_cursor } => {
                self.apply_catalog_fetched(chat_jid, products, after_cursor);
            }
            BackendEvent::CatalogFetchFailed { chat_jid, message } => {
                self.apply_catalog_fetch_failed(chat_jid, message);
            }
            BackendEvent::CollectionsFetched { chat_jid, collections } => {
                self.apply_collections_fetched(chat_jid, collections);
            }
            BackendEvent::CollectionsFetchFailed { chat_jid, message } => {
                self.apply_collections_fetch_failed(chat_jid, message);
            }
            BackendEvent::ProductImageBytes { url, bytes } => {
                self.apply_product_image_bytes(ctx, url, bytes);
            }
            BackendEvent::ProductImageFetchFailed { url } => {
                self.apply_product_image_fetch_failed(url);
            }
            BackendEvent::OrderDetailsFetched { chat_jid, order_id, order } => {
                self.apply_order_details_fetched(chat_jid, order_id, order);
            }
            BackendEvent::OrderDetailsFetchFailed { chat_jid, order_id, message } => {
                self.apply_order_details_fetch_failed(chat_jid, order_id, message);
            }
            BackendEvent::OwnBusinessProfileFetched { profile } => {
                self.apply_own_business_fetched(profile);
            }
            BackendEvent::OwnBusinessProfileFetchFailed { message } => {
                self.apply_own_business_fetch_failed(message);
            }
            BackendEvent::OwnBusinessProfileSaved => {
                self.apply_own_business_saved();
            }
            BackendEvent::OwnBusinessProfileSaveFailed { message } => {
                self.apply_own_business_save_failed(message);
            }
            BackendEvent::Blocklist(jids) => {
                // The server's list is the source of truth for
                // blocks: adopt it wholesale, replacing the
                // persisted copy that covered the time before
                // this fetch landed. Chat ids are JID hashes, so
                // the adoption needs no chat list. The adopted
                // set is written through to the persisted copy
                // (the encrypted archive store, privacy audit
                // A6), so the stored truth is the server's, not
                // merely the last local toggle's.
                self.blocked_chats = jids.iter().map(|j| jid_id(j)).collect();
                self.persist_blocked_chats();
                // A pending toggle this list confirms is settled
                // here, and only here does its success get
                // announced: at click time nothing was claimed.
                let confirmed: Vec<u64> = self
                    .pending_blocks
                    .keys()
                    .filter(|id| {
                        // The toggle's intent was the opposite of
                        // the state it replaced: it is confirmed
                        // when the adopted set now holds the chat
                        // exactly when the toggle blocked it.
                        let was_blocked = self.pending_blocks[*id];
                        self.blocked_chats.contains(id) != was_blocked
                    })
                    .copied()
                    .collect();
                for chat_id in confirmed {
                    self.pending_blocks.remove(&chat_id);
                    self.notice = Some(Notice::info(if self.blocked_chats.contains(&chat_id) {
                        "Contact blocked.".to_owned()
                    } else {
                        "Contact unblocked.".to_owned()
                    }));
                }
            }
            BackendEvent::MediaSaved { chat_jid, message_id: pid, path } => {
                // The download is done: its chip stops spinning.
                let mid = message_id(&chat_jid, &pid);
                self.media.downloads.remove(&mid);
                // A ZIP export gathering this document settles
                // its item with the saved file; its notice is
                // the export's own, not a per-file Saved line.
                let for_export = self.zip_gather.as_ref().is_some_and(|g| g.waiting == Some(mid));
                if for_export {
                    self.zip_collect_file(mid, ZipKind::Document, std::path::PathBuf::from(&path));
                } else {
                    self.notice = Some(Notice::info(format!("Saved to {path}")));
                }
            }
            BackendEvent::PhotoBytes { chat_jid, message_id: pid, bytes } => {
                // The fetched bytes become the message's texture:
                // from here on its bubble, album cell and viewer
                // paint the photo itself. Bytes that do not decode
                // are a failed fetch, never a reason to substitute
                // another image.
                let mid = message_id(&chat_jid, &pid);
                // A ZIP export waiting on this photo keeps its
                // own copy of the bytes (section 25).
                self.zip_collect_bytes(mid, ZipKind::Photo, bytes.clone());
                match crate::media::decode_photo(ctx, &bytes) {
                    Some(tex) => {
                        self.media.textures.insert(mid, tex);
                        self.media.photo_fetch.remove(&mid);
                    }
                    None => {
                        self.media.photo_fetch.insert(mid, crate::state::PhotoFetchState::Failed);
                    }
                }
            }
            BackendEvent::PhotoFetchFailed { chat_jid, message_id: pid, message } => {
                let mid = message_id(&chat_jid, &pid);
                self.zip_collect_missing(mid, Some(ZipKind::Photo), message);
                // Only a fetch still in flight can fail; a late
                // failure must not clobber a photo that has since
                // landed.
                if self.media.photo_fetch.get(&mid) == Some(&crate::state::PhotoFetchState::Loading)
                {
                    self.media.photo_fetch.insert(mid, crate::state::PhotoFetchState::Failed);
                }
            }
            BackendEvent::StickerBytes { chat_jid, message_id: pid, bytes } => {
                // A sticker's fetched bytes become its texture,
                // the same landing as a photo's.
                let mid = message_id(&chat_jid, &pid);
                self.zip_collect_bytes(mid, ZipKind::Sticker, bytes.clone());
                match crate::media::decode_photo(ctx, &bytes) {
                    Some(tex) => {
                        self.media.textures.insert(mid, tex);
                        self.media.sticker_fetch.remove(&mid);
                        // A view-once sticker's view is spent
                        // by its bytes actually landing as a
                        // paintable picture: the opened state is
                        // recorded and the archived reference
                        // dropped, so the sticker cannot be
                        // fetched again after a restart. The
                        // decoded texture stays for this
                        // session's viewing, like a view-once
                        // photo's pixels stay until the viewer
                        // closes.
                        if self.live.as_ref().is_some_and(|s| s.view_once_ids.contains(&mid)) {
                            self.mark_view_once_opened(mid);
                            self.drop_sticker_ref(mid);
                        }
                    }
                    None => {
                        self.media.sticker_fetch.insert(mid, crate::state::PhotoFetchState::Failed);
                    }
                }
            }
            BackendEvent::StickerFetchFailed { chat_jid, message_id: pid } => {
                let mid = message_id(&chat_jid, &pid);
                self.zip_collect_missing(
                    mid,
                    Some(ZipKind::Sticker),
                    "the sticker could not be fetched".to_owned(),
                );
                if self.media.sticker_fetch.get(&mid)
                    == Some(&crate::state::PhotoFetchState::Loading)
                {
                    self.media.sticker_fetch.insert(mid, crate::state::PhotoFetchState::Failed);
                }
            }
            BackendEvent::VerifiedNameResult { jid, name } => {
                if let Some(session) = &mut self.live {
                    session.verified_checked.insert(jid.clone());
                    if let Some(name) = name {
                        session.verified_names.insert(jid, name);
                    }
                }
            }
            BackendEvent::AvatarBytes { jid, picture_id, bytes } => {
                // The request is answered: the in-flight mark
                // clears whatever the bytes turn out to be, or
                // every retry rule below it stays unreachable
                // for the whole session.
                if let Some(session) = &mut self.live {
                    session.avatar_requested.remove(&jid);
                }
                // Persist first (the store is the launch seed),
                // then decode into the live texture cache. Bytes
                // that do not decode are dropped, never painted
                // and never stored as a picture.
                let now = i64::try_from(crate::state::now_unix()).unwrap_or(0);
                if let Some(tex) = crate::media::decode_photo(ctx, &bytes) {
                    if let Some(session) = &mut self.live {
                        if let Some(store) = &session.store {
                            let _ =
                                store.put_avatar(&jid, picture_id.as_deref(), Some(&bytes), now);
                        }
                        session.avatar_missing.remove(&jid);
                        session.avatar_bytes.insert(jid.clone(), bytes);
                    }
                    self.media.live_avatar_textures.insert(jid, tex);
                }
            }
            BackendEvent::AvatarMissing { jid } => {
                let now = i64::try_from(crate::state::now_unix()).unwrap_or(0);
                if let Some(session) = &mut self.live {
                    if let Some(store) = &session.store {
                        let _ = store.put_avatar(&jid, None, None, now);
                    }
                    // Answered: the in-flight mark clears, so
                    // the negative's own 24-hour re-ask rule in
                    // ensure_avatar can actually fire later.
                    session.avatar_requested.remove(&jid);
                    session.avatar_missing.insert(jid, now);
                }
            }
            BackendEvent::AvatarUnavailable { jid } => {
                // No answer arrived, so nothing is recorded as
                // a miss (and nothing is persisted): the
                // in-flight mark clears and an in-memory
                // timestamp backdated to five minutes before
                // the negative window ends rate-limits the
                // retry to once per five minutes of display
                // instead of once per paint.
                let now = i64::try_from(crate::state::now_unix()).unwrap_or(0);
                if let Some(session) = &mut self.live {
                    session.avatar_requested.remove(&jid);
                    session.avatar_missing.insert(jid, now - 86_400 + 300);
                }
            }
            BackendEvent::AvatarChanged { jid } => {
                // The wire says this identity's picture changed:
                // every cached form is stale. The next display
                // refetches through `ensure_avatar`.
                self.drop_avatar(&jid);
            }
            BackendEvent::HistoryPageFetched { chat_jid, end_of_history, .. } => {
                // The page's messages already landed through
                // MessageUpsert: the backend reports completion
                // only after translation filed them. Only the
                // fetch state moves here (design standard
                // section 22): exhaustion latches, otherwise the
                // chat returns to idle, and the trigger stays
                // disarmed until the stream scrolls away from
                // the top again.
                let id = jid_id(&chat_jid);
                if let Some(session) = &mut self.live {
                    if end_of_history {
                        session.history.insert(id, HistoryFetch::Exhausted);
                    } else {
                        session.history.remove(&id);
                    }
                    session.history_armed.remove(&id);
                }
                if self.selected_chat == id && self.live_mode {
                    self.sync_conversation();
                }
            }
            BackendEvent::HistoryFetchFailed { chat_jid, message } => {
                // Back to idle, never latched (section 22): the
                // user can scroll away and back to retry. The
                // failure is surfaced only when the chat is the
                // one on screen; another chat's failed page is
                // not worth a global notice. ACCEPTED as the
                // standing behavior (audit P2-35, signed off in
                // the v1.6.15 round): no per-chat failure marker
                // is kept for unopened chats, by decision, not
                // by omission.
                let id = jid_id(&chat_jid);
                if let Some(session) = &mut self.live {
                    session.history.remove(&id);
                }
                if self.selected_chat == id && self.live_mode {
                    self.notice = Some(Notice::error(message));
                }
            }
            BackendEvent::GroupPictureChanged { chat_jid } => {
                // Our own picture write landed (section 23):
                // reconcile through the avatar pipeline, never
                // by painting the picked bytes as server state.
                self.drop_avatar(&chat_jid);
                self.notice = Some(Notice::info(crate::i18n::t("profile.group_picture_updated")));
            }
            BackendEvent::GroupPictureFailed { message, .. } => {
                self.notice = Some(Notice::error(message));
            }
            BackendEvent::ContactResolved { chat, alt_jid } => {
                // Only a dialog still open acts on the answer:
                // a lookup the user cancelled while it was in
                // flight does not yank them into a conversation.
                if self.new_chat.open {
                    self.apply_contact_resolved(chat, alt_jid);
                }
            }
            BackendEvent::ContactNotFound { phone } => {
                // Only the dialog's own outstanding check moves
                // (section 23): a stale outcome for an input the
                // user has since replaced is ignored.
                if self.new_chat.checking.as_deref() == Some(phone.as_str()) {
                    self.new_chat.checking = None;
                    self.new_chat.not_found = Some(phone);
                    self.new_chat.error = None;
                }
            }
            BackendEvent::ContactLookupFailed { phone, message } => {
                if self.new_chat.checking.as_deref() == Some(phone.as_str()) {
                    self.new_chat.checking = None;
                    self.new_chat.error = Some(message);
                }
            }
            BackendEvent::MediaRefreshed { chat_jid, message_id: pid, direct_path } => {
                // The media-retry flow re-uploaded this message's
                // media under a fresh CDN path: update the
                // session's references and the archive, so later
                // fetches and later sessions go straight to it.
                let mid = message_id(&chat_jid, &pid);
                if let Some(session) = &mut self.live {
                    if let Some(reference) = session.photo_refs.get_mut(&mid) {
                        reference.direct_path = direct_path.clone();
                    }
                    if let Some(reference) = session.voice_refs.get_mut(&mid) {
                        reference.direct_path = direct_path.clone();
                    }
                    if let Some(reference) = session.video_refs.get_mut(&mid) {
                        reference.direct_path = direct_path.clone();
                    }
                    if let Some(reference) = session.sticker_refs.get_mut(&mid) {
                        reference.direct_path = direct_path.clone();
                    }
                    if let Some(store) = &session.store {
                        let _ = store.update_media_direct_path(&chat_jid, &pid, &direct_path);
                    }
                }
            }
            BackendEvent::FavoritesSync { chat_jids } => {
                self.apply_favorites_sync(&chat_jids);
            }
            BackendEvent::StickerFavoriteChanged { filehash, favorite } => {
                if favorite {
                    self.synced_sticker_favorites.insert(filehash);
                } else {
                    self.synced_sticker_favorites.remove(&filehash);
                }
            }
            BackendEvent::RecentStickerRemoved { filehash } => {
                self.synced_recent_sticker_removals.push(filehash);
            }
            BackendEvent::CallHistoryRecord { record } => {
                // The phone's own call history: the store keeps
                // the first record per call ID, so replays of a
                // history chunk are harmless. The panel list is
                // then reloaded from the archive, the same load
                // a session start performs.
                if let Some(session) = &self.live {
                    if let Some(store) = &session.store {
                        let _ = store.record_call(&record);
                    }
                }
                self.load_call_history();
            }
            BackendEvent::GroupChanged { chat_jid } => {
                // The cached member list for this group is
                // stale: forget that it was fetched. The frame
                // loop's member sync refetches it while the
                // group is open.
                self.groups.members_requested.remove(&jid_id(&chat_jid));
            }
            BackendEvent::MessageUnavailable {
                chat_jid,
                message_id: pid,
                sender_jid,
                from_me,
                timestamp,
            } => {
                self.apply_message_unavailable(&chat_jid, &pid, sender_jid, from_me, timestamp);
            }
            BackendEvent::VoiceBytes { chat_jid, message_id: pid, bytes } => {
                let mid = message_id(&chat_jid, &pid);
                // A ZIP export waiting on this note keeps its
                // own copy: the playback path below drops bytes
                // for any note but the wanted one, and the
                // export's claim on them is independent.
                self.zip_collect_bytes(mid, ZipKind::Voice, bytes.clone());
                // Bytes that land for a note the user has already
                // moved past are dropped, never played over the
                // current one; only the wanted note decodes.
                if self.media.voice_wanted != Some(mid) {
                    if self.media.voice_fetch.get(&mid)
                        == Some(&crate::state::VoiceFetchState::Loading)
                    {
                        self.media.voice_fetch.remove(&mid);
                    }
                } else {
                    match crate::player::decode_voice(&bytes) {
                        Ok(pcm) => {
                            let pcm: std::sync::Arc<[i16]> = std::sync::Arc::from(pcm);
                            self.media.voice_pcm = Some((mid, std::sync::Arc::clone(&pcm)));
                            self.media.voice_fetch.remove(&mid);
                            let seek = self.media.voice_pending_seek.remove(&mid);
                            self.start_voice_player(mid, pcm, seek);
                        }
                        Err(message) => {
                            // Bytes that do not decode are a failed
                            // fetch, never silent playback.
                            self.media
                                .voice_fetch
                                .insert(mid, crate::state::VoiceFetchState::Failed);
                            self.media.voice_pending_seek.remove(&mid);
                            self.notice = Some(Notice::error(message));
                        }
                    }
                }
            }
            BackendEvent::VoiceFetchFailed { chat_jid, message_id: pid, message } => {
                let mid = message_id(&chat_jid, &pid);
                self.zip_collect_missing(mid, Some(ZipKind::Voice), message.clone());
                // Only a fetch still in flight can fail (see the
                // photo arm); the failure is surfaced only for the
                // note the user is actually trying to hear.
                if self.media.voice_fetch.get(&mid) == Some(&crate::state::VoiceFetchState::Loading)
                {
                    self.media.voice_fetch.remove(&mid);
                    if self.media.voice_wanted == Some(mid) {
                        self.media.voice_fetch.insert(mid, crate::state::VoiceFetchState::Failed);
                        self.media.voice_pending_seek.remove(&mid);
                        self.notice = Some(Notice::error(message));
                    }
                }
            }
            BackendEvent::VideoBytes { chat_jid, message_id: pid, bytes } => {
                // View-once bytes, memory only. They are kept
                // for the viewer (which starts the player from
                // them); the fetch state clears either way.
                let mid = message_id(&chat_jid, &pid);
                if self.media.video_fetch.get(&mid) == Some(&crate::state::PhotoFetchState::Loading)
                {
                    self.media.video_fetch.remove(&mid);
                    self.media.video_bytes.insert(mid, bytes);
                }
            }
            BackendEvent::VideoReady { chat_jid, message_id: pid, path } => {
                // A regular video's bytes are on disk in the
                // video cache; the path is what the player opens.
                let mid = message_id(&chat_jid, &pid);
                // A ZIP export waiting on this video settles
                // with the cache file, whatever the display
                // fetch state says.
                self.zip_collect_file(mid, ZipKind::Video, std::path::PathBuf::from(&path));
                if self.media.video_fetch.get(&mid) == Some(&crate::state::PhotoFetchState::Loading)
                {
                    self.media.video_fetch.remove(&mid);
                    self.media.video_files.insert(mid, path);
                }
            }
            BackendEvent::VideoFetchFailed { chat_jid, message_id: pid, message } => {
                let mid = message_id(&chat_jid, &pid);
                self.zip_collect_missing(mid, Some(ZipKind::Video), message.clone());
                // Only a fetch still in flight can fail (see the
                // photo arm). The viewer paints the failed state
                // and offers a retry.
                if self.media.video_fetch.get(&mid) == Some(&crate::state::PhotoFetchState::Loading)
                {
                    self.media.video_fetch.insert(mid, crate::state::PhotoFetchState::Failed);
                    self.notice = Some(Notice::error(message));
                }
            }
            BackendEvent::VideoPosterBytes { chat_jid, message_id: pid, bytes } => {
                // The fetched poster becomes the video's
                // thumbnail everywhere a thumbnail lives: the
                // session map the tiles decode from, and the
                // archive row, so later sessions present the
                // frame without fetching again. Bytes that do
                // not decode are a failed fetch, never a reason
                // to substitute another image.
                let mid = message_id(&chat_jid, &pid);
                if self.media.poster_fetch.get(&mid)
                    != Some(&crate::state::PhotoFetchState::Loading)
                {
                    // A poster nobody is waiting for (its fetch
                    // was superseded) is dropped, not landed.
                } else if crate::media::decode_photo(ctx, &bytes).is_some() {
                    self.media.poster_fetch.remove(&mid);
                    if let Some(session) = &mut self.live {
                        session.video_thumbs.insert(mid, bytes.clone());
                        if let Some(store) = &session.store {
                            let _ = store.set_video_thumbnail(&chat_jid, &pid, &bytes);
                        }
                    }
                } else {
                    self.media.poster_fetch.insert(mid, crate::state::PhotoFetchState::Failed);
                }
            }
            BackendEvent::VideoPosterFetchFailed { chat_jid, message_id: pid, .. } => {
                let mid = message_id(&chat_jid, &pid);
                // Only a fetch still in flight can fail (see
                // the photo arm). A failed poster is quiet: the
                // tile stays neutral and the video itself still
                // fetches on play.
                if self.media.poster_fetch.get(&mid)
                    == Some(&crate::state::PhotoFetchState::Loading)
                {
                    self.media.poster_fetch.insert(mid, crate::state::PhotoFetchState::Failed);
                }
            }
            BackendEvent::TypingReceived { chat_jid, typing } => {
                let chat_id = jid_id(&chat_jid);
                if typing {
                    self.live_typing.insert(chat_id, std::time::Instant::now());
                } else {
                    self.live_typing.remove(&chat_id);
                }
            }
            BackendEvent::PresenceReceived { chat_jid, online, last_seen } => {
                // Only the watched chat's reports are stored;
                // the watch (see `sync_presence_watch`) owns the
                // entry's lifetime, so a report for a chat the
                // user already left cannot resurrect stale
                // presence there.
                let chat_id = jid_id(&chat_jid);
                if self.presence_watch.as_ref().is_some_and(|(id, _)| *id == chat_id) {
                    self.live_presence
                        .insert(chat_id, crate::state::ContactPresence { online, last_seen });
                }
            }
            BackendEvent::GroupMembers { chat_jid, members, own_admin } => {
                // A participant add's outcome refresh (P2-37):
                // only now does the input clear, and only if it
                // still holds the address that was sent.
                if let Some((awaiting_chat, awaiting_addr)) =
                    self.groups.participant_add_awaiting.take()
                {
                    if awaiting_chat == jid_id(&chat_jid)
                        && self.add_participant_input.trim() == awaiting_addr
                    {
                        self.add_participant_input.clear();
                    }
                    self.clear_busy_notice();
                }
                self.groups.own_admin.insert(jid_id(&chat_jid), own_admin);
                // Member names join the name directory before
                // row resolution masks any of them: mentions of
                // this group's members resolve from here.
                if let Some(session) = &mut self.live {
                    for (jid, name, _admin) in &members {
                        if !name.is_empty() && !is_raw_identifier(name) {
                            session.names.insert(jid.clone(), name.clone());
                        }
                    }
                }
                let rows = self.resolve_member_rows(members);
                self.groups.live_members.insert(jid_id(&chat_jid), rows);
            }
            BackendEvent::GroupInviteLink { chat_jid, link } => {
                self.groups.invite_link = Some((jid_id(&chat_jid), link));
            }
            BackendEvent::GroupApprovalState { chat_jid, enabled } => {
                self.groups.approval.insert(jid_id(&chat_jid), enabled);
            }
            BackendEvent::GroupModesState { chat_jid, announce, locked } => {
                self.groups.modes.insert(jid_id(&chat_jid), (announce, locked));
            }
            BackendEvent::JoinRequests { chat_jid, requests } => {
                let rows = self.resolve_request_rows(requests);
                self.groups.join_requests = Some((jid_id(&chat_jid), rows));
            }
            BackendEvent::SyncProgress { .. } => {
                if let Some(session) = &mut self.live {
                    session.syncing = true;
                    session.sync_last_activity = Some(std::time::Instant::now());
                }
            }
            BackendEvent::SyncInterrupted => {
                // The drain did not finish: the client is not
                // caught up. The sync row stays up and
                // synced_once is never latched by an interruption
                // (QC1614-A0-5 W1).
                if let Some(session) = &mut self.live {
                    session.syncing = true;
                    session.sync_last_activity = Some(std::time::Instant::now());
                }
            }
            BackendEvent::ChatCommunity { chat_jid, parent_jid, is_parent } => {
                let chat_id = jid_id(&chat_jid);
                if is_parent {
                    self.chat_parents.insert(chat_id, chat_id);
                }
                if let Some(parent) = &parent_jid {
                    self.chat_parents.insert(chat_id, jid_id(parent));
                }
                // The child's community tag is the parent's
                // title; the parent's tag is its own title.
                let tag = if is_parent {
                    self.chats.iter().find(|c| c.id == chat_id).map(|c| c.title.clone())
                } else if let Some(parent) = &parent_jid {
                    let pid = jid_id(parent);
                    self.chats.iter().find(|c| c.id == pid).map(|c| c.title.clone())
                } else {
                    None
                };
                if let Some(tag) = tag.filter(|t| !t.is_empty() && !is_raw_identifier(t)) {
                    if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                        chat.community = Some(tag);
                    }
                }
                if !is_parent && parent_jid.is_none() {
                    // A group reporting no community stands
                    // alone: an unlink (or a deactivation the
                    // server already applied) clears the mapping
                    // and the tag an earlier link had set.
                    self.chat_parents.remove(&chat_id);
                    if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                        chat.community = None;
                    }
                }
            }
            BackendEvent::SyncCompleted => {
                // On a fresh pairing, Connected fired before
                // the history sync delivered the roster, so the
                // connect-time title resolution collected
                // nothing. The first completed sync re-reports
                // the raw titles now that the roster exists;
                // later syncs do not repeat it.
                let raw_titles: Vec<String> = match &self.live {
                    Some(session) if !session.synced_once => self
                        .chats
                        .iter()
                        .filter(|chat| {
                            is_raw_identifier(&chat.title)
                                || chat.kind == core::model::ChatKind::Group
                        })
                        .filter_map(|chat| session.jids.get(&chat.id).cloned())
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .collect(),
                    _ => Vec::new(),
                };
                if let Some(session) = &mut self.live {
                    session.syncing = false;
                    session.sync_last_activity = None;
                    session.synced_once = true;
                    if !raw_titles.is_empty() {
                        let _ = session
                            .channels
                            .commands
                            .send(BackendCommand::ResolveTitles { jids: raw_titles });
                    }
                }
            }
            BackendEvent::ChatRenamed { chat_jid, title, authoritative } => {
                self.apply_chat_renamed(&chat_jid, &title, authoritative);
            }
            BackendEvent::SenderNamed { sender_jid, name } => {
                self.apply_sender_named(&sender_jid, &name);
            }
            BackendEvent::ConnectionLost { reason } => {
                if let Some(session) = &mut self.live {
                    if session.status.is_linked() || session.status == LinkStatus::WaitingQr {
                        session.status = LinkStatus::Lost(reason);
                    }
                    // The pairing refs behind a held payload belong
                    // to the connection that just dropped; a fresh
                    // payload arrives with the reconnect.
                    session.clear_qr();
                }
                // Presence is only as good as the connection
                // feeding it: while the link is down an "online"
                // kept on screen would be a guess. The reports
                // resume when the watch re-subscribes after the
                // reconnect.
                self.live_presence.clear();
            }
            BackendEvent::Crashed { summary, restarted } => {
                if let Some(session) = &mut self.live {
                    session.status = LinkStatus::Crashed(summary);
                    session.clear_qr();
                }
                // Sends caught by the crash can never complete; do
                // not leave their echoes spinning (logic review F5).
                self.fail_pending_echoes(true);
                if restarted {
                    // Documented supervisor contract: re-issue Connect.
                    self.send_backend(BackendCommand::ConnectQr);
                }
            }
            BackendEvent::Error { message } => {
                // A failure ends any in-flight create (P2-31):
                // the error replaces the Busy notice below, and
                // no stale pending flag may let a later unrelated
                // group upsert claim the outcome.
                self.groups.create_pending = false;
                // Backend error text is largely library error
                // Display output and can embed server-supplied
                // strings and identifiers. Everything below
                // displays it (the notice banner via human_error,
                // the pre-link status line), so it passes through
                // the crash display scrubber first, the same
                // treatment the Crashed summary gets (privacy
                // audit C6). App-authored strings contain no
                // secret or phone shapes and pass unchanged.
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("backend error: {message}");
                // A ZIP export waiting on a document download
                // claims this error as its item's answer: the
                // manifest carries the reason, and the export's
                // own Busy-to-result arc keeps the notice line.
                let zip_doc_wait = self.zip_gather.as_ref().and_then(|g| {
                    let waiting = g.waiting?;
                    g.items
                        .iter()
                        .find(|i| i.msg_id == waiting && i.kind == ZipKind::Document)
                        .map(|_| waiting)
                });
                if let Some(waiting) = zip_doc_wait {
                    self.zip_collect_missing(waiting, Some(ZipKind::Document), message.clone());
                }
                // An error while a call action is unconfirmed almost
                // always belongs to it; release the buttons (F18).
                if let Some(call) = &mut self.calls.active {
                    call.action_pending = false;
                }
                if zip_doc_wait.is_none() {
                    self.notice = Some(Notice::error(human_error(&message)));
                }
                // A download that failed reports through this
                // arm; its chip stops spinning either way.
                self.media.downloads.clear();
                // A failed send must surface on its own bubble, not
                // only as a transient notice (logic review F6).
                // Only a send failure may mark an echo, though:
                // this arm carries every backend error, and an
                // unrelated failure (a rename, a lookup) says
                // nothing about an in-flight send, so marking
                // the oldest one failed from it is a false report.
                if is_send_failure(&message) {
                    self.fail_pending_echoes(false);
                }
                if let Some(session) = &mut self.live {
                    if !session.status.is_linked() {
                        session.status = LinkStatus::Failed(message);
                        session.clear_qr();
                    }
                }
            }
            BackendEvent::JoinRequestPending => {
                self.notice = Some(Notice::info(
                    "Join request sent. The group appears here once an admin approves it."
                        .to_owned(),
                ));
            }
            BackendEvent::IncomingCall { call_id, chat_jid, video } => {
                let chat_id = jid_id(&chat_jid);
                self.on_incoming_call(&call_id, chat_id, video);
                // When the window is not in front of the user,
                // the ring needs an OS popup like a message gets:
                // the overlay alone rings to an empty room. The
                // popup follows the message discipline (the
                // per-kind toggles, the mute and block gates, the
                // preview privacy) and fires only when the
                // overlay actually took this call.
                let adopted = self.calls.active.as_ref().is_some_and(|call| {
                    call.call_id == call_id && call.phase == crate::call_ui::CallUiPhase::Incoming
                });
                if adopted {
                    let title = self
                        .calls
                        .active
                        .as_ref()
                        .map(|call| call.title.clone())
                        .unwrap_or_default();
                    let chat = self.chats.iter().find(|c| c.id == chat_id);
                    let settings = crate::notify::NotifySettings {
                        messages: self.notify_messages,
                        groups: self.notify_groups,
                        sound: self.notify_sound,
                        preview: self.notify_preview,
                    };
                    let req = crate::notify::NotifyRequest {
                        chat_id,
                        title: title.clone(),
                        kind: chat.map(|c| c.kind).unwrap_or(core::model::ChatKind::Dm),
                        muted: chat.map(|c| c.muted).unwrap_or(false),
                        blocked: self.blocked_chats.contains(&chat_id),
                        sender: None,
                        body: String::new(),
                        locked: chat.map(|c| c.locked).unwrap_or(false),
                    };
                    // For a call, a focused window IS looking at
                    // it: the overlay covers the window while it
                    // rings, so focus alone is the suppression.
                    let focused = ctx.input(|i| i.viewport().focused.unwrap_or(false));
                    if crate::notify::should_notify(settings, &req, focused) {
                        let (summary, body) = if settings.preview {
                            (
                                title,
                                if video {
                                    "Incoming video call".to_owned()
                                } else {
                                    "Incoming voice call".to_owned()
                                },
                            )
                        } else {
                            ("Zaprust".to_owned(), "Incoming call".to_owned())
                        };
                        crate::notify::fire_os_notification(summary, body, settings.sound);
                    }
                }
            }
            BackendEvent::CallAudioState { muted, speaker } => {
                // Confirmation for an optimistic in-call toggle
                // (P2-33): the confirmed dimension is applied
                // and the backup is released.
                if let Some(call) = &mut self.calls.active {
                    if let Some(muted) = muted {
                        call.muted = muted;
                    }
                    if let Some(speaker) = speaker {
                        call.speaker = speaker;
                    }
                    call.audio_backup = None;
                }
            }
            BackendEvent::CallAudioFailed { message } => {
                // A failed in-call audio toggle (P2-33): the
                // optimistic flip goes back to its backup pair.
                // The revert hangs on this typed event, never on
                // the generic error: an unrelated failure used
                // to trip it and flap a toggle that had actually
                // landed (VRF-04). Same display treatment as the
                // generic error path: scrubbed, then user words.
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("call audio toggle failed: {message}");
                if let Some(call) = &mut self.calls.active {
                    if let Some((muted, speaker)) = call.audio_backup.take() {
                        call.muted = muted;
                        call.speaker = speaker;
                    }
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::CallChanged { call_id, chat_jid, phase, seconds, reason } => {
                let chat_id = jid_id(&chat_jid);
                let ended = phase == backend::CallPhase::Ended;
                self.on_call_changed(&call_id, chat_id, phase, seconds, reason);
                if ended {
                    self.record_ended_call(&call_id, &chat_jid, seconds);
                }
            }
            BackendEvent::CallMissed { call_id, chat_jid, at } => {
                self.record_missed_call(&call_id, &chat_jid, at);
                // This event only fires for calls that never rang
                // on this device (the offer could not be shown, or
                // it rang while the device was away and replayed
                // on reconnect), so the Calls list was the only
                // place it existed. Name it in a notice too: the
                // caller resolves through the roster, like every
                // other surface.
                let title = self
                    .chats
                    .iter()
                    .find(|c| c.id == jid_id(&chat_jid))
                    .map(|c| c.title.as_str())
                    .filter(|title| !title.is_empty());
                self.notice = Some(Notice::info(match title {
                    Some(title) => format!("Missed call from {title}"),
                    None => "Missed call".to_owned(),
                }));
            }
            BackendEvent::CallEndedElsewhere { call_id, chat_jid, answered, at } => {
                self.record_elsewhere_call(&call_id, &chat_jid, answered, at);
            }
        }
    }

    /// Applies a resolved display name for a chat (identity
    /// healing from the backend: a contact update, a group
    /// subject, a LID-to-phone mapping). A heal rename applies
    /// only where the title is still a raw identifier or the
    /// chat's own formatted phone number, so memory and disk
    /// only ever move upward from an identifier. An authoritative
    /// rename (the server stated the chat's current name: a
    /// saved contact renamed on a device, a group subject
    /// change) replaces a resolved title outright; without it
    /// the first name a chat ever resolved would be frozen
    /// forever, and real renames would never land.
    pub(super) fn apply_chat_renamed(&mut self, chat_jid: &str, title: &str, authoritative: bool) {
        if title.is_empty() || is_raw_identifier(title) {
            return;
        }
        if let Some(session) = &mut self.live {
            if let Some(store) = &session.store {
                if authoritative {
                    let _ = store.rename_chat_authoritative(chat_jid, title);
                } else {
                    let _ = store.rename_chat(chat_jid, title);
                }
            }
            // A direct chat's title is the contact's name: it
            // joins the name directory under the chat's JID.
            // Only direct chats feed the person directory: a
            // channel's healed name is not a person, and letting
            // it in would let a channel title answer for a
            // mention or a sender later.
            let is_dm = chat_jid.ends_with("@s.whatsapp.net") || chat_jid.ends_with("@lid");
            if is_dm {
                session.names.insert(chat_jid.to_owned(), title.to_owned());
            }
        }
        let id = jid_id(chat_jid);
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == id) {
            let healable = chat.title.is_empty()
                || is_raw_identifier(&chat.title)
                || is_formatted_phone(&chat.title);
            if authoritative || healable {
                chat.title = title.to_owned();
            }
        }
    }

    /// Applies a resolved display name for one sender: stored
    /// message labels and previews that still show the sender's
    /// raw identifier are rewritten in the archive and in memory.
    /// Labels that already hold a name are never touched.
    pub(super) fn apply_sender_named(&mut self, sender_jid: &str, name: &str) {
        // A phone-formatted name learned for a LID identity is
        // the session learning that identity's LID-to-phone
        // mapping. A picture negative recorded while the mapping
        // was unknown may be a LID-form miss rather than a true
        // absence, so it is dropped: the next paint retries the
        // fetch, which now resolves under the phone form.
        if sender_jid.ends_with("@lid") && is_formatted_phone(name) {
            let has_negative = self
                .live
                .as_ref()
                .is_some_and(|session| session.avatar_missing.contains_key(sender_jid));
            if has_negative {
                self.drop_avatar(sender_jid);
            }
        }
        // Every learned name joins the name directory first
        // (mention resolution reads it), whether or not any
        // stored label still needs the rewrite below.
        if let Some(session) = &mut self.live {
            if !name.is_empty() && !is_raw_identifier(name) {
                session.names.insert(sender_jid.to_owned(), name.to_owned());
            }
        }
        let raw = sender_jid.split('@').next().unwrap_or(sender_jid);
        if !is_raw_identifier(raw) {
            return;
        }
        if let Some(session) = &mut self.live {
            if let Some(store) = &session.store {
                let _ = store.rename_sender(sender_jid, name);
            }
            for list in session.messages.values_mut() {
                for (_, msg) in list.iter_mut() {
                    if msg.sender.as_deref() == Some(raw) {
                        msg.sender = Some(name.to_owned());
                    }
                }
            }
        }
        for chat in self.chats.iter_mut() {
            if chat.preview_sender.as_deref() == Some(raw) {
                chat.preview_sender = Some(name.to_owned());
            }
        }
    }

    pub(super) fn apply_chat_upsert(&mut self, live: &LiveChat) {
        if self.groups.create_pending && live.kind == LiveChatKind::Group {
            // The answering upsert for the in-flight CreateGroup
            // (P2-31): the group exists now, so the Busy banner
            // becomes the Info outcome.
            self.groups.create_pending = false;
            self.notice = Some(Notice::info(crate::i18n::t("notice.group_created")));
        }
        if live.jid == "status@broadcast" {
            // The status broadcast address is not a chat (see
            // apply_message_upsert's guard): no source may
            // materialize it as one.
            return;
        }
        let id = jid_id(&live.jid);
        // The disappearing timer is the one field a message-arrival
        // upsert cannot know (the wire does not carry it there), so
        // an upsert reporting "off" must not erase a timer this
        // client already knows about. Timer changes have their own
        // authoritative path (DisappearingChanged events and the
        // archive); this merge only protects the stored value, and
        // the merged row is what reaches the archive.
        let mut merged = live.clone();
        if merged.ephemeral_secs == 0 {
            if let Some(known) = self.chats.iter().find(|c| c.id == id) {
                merged.ephemeral_secs = known.ephemeral_secs;
            }
        }
        // Same protection for mute: an arrival upsert reports
        // `false` without knowing better, so it must not unmute
        // a chat this client knows is muted. Server-side mute
        // changes have their own authoritative path
        // (MuteChanged events); this merge only protects the
        // stored value, exactly as for the timer above.
        if !merged.muted {
            if let Some(known) = self.chats.iter().find(|c| c.id == id) {
                merged.muted = known.muted;
            }
        }
        // The mute's end time gets the same protection, and
        // without it the protection above was a trap: an
        // arrival upsert reports mute_until 0 ("no end"),
        // the merge kept `muted` but zeroed the end, and a
        // timed mute silently became a permanent one. Only a
        // live timed mute is preserved (a known end already in
        // the past is spent either way), and real unmutes and
        // re-mutes arrive through MuteChanged, never through
        // this merge.
        if merged.mute_until == 0 {
            if let Some(known) = self.chats.iter().find(|c| c.id == id) {
                let now = i64::try_from(unix_now_secs()).unwrap_or(0);
                let known_until = i64::try_from(known.mute_until).unwrap_or(0);
                if known.muted && known_until > now {
                    merged.mute_until = known_until;
                }
            }
        }
        // Same protection for pin and archive: an arrival upsert
        // reports `false` for both without knowing better, so it
        // must not unpin or unarchive a chat this client knows
        // is pinned or archived. Server-side changes have their
        // own authoritative path (PinChanged / ArchiveChanged
        // events); this merge only protects the stored value. An
        // upsert that does know the state (the fake's chat-action
        // and LeaveGroup echoes carry its full state) still
        // lands, because the protection only ever preserves a
        // known `true` against a reported `false`.
        if !merged.pinned {
            if let Some(known) = self.chats.iter().find(|c| c.id == id) {
                merged.pinned = known.pinned;
            }
        }
        // Same protection for lock: arrival upserts never carry
        // it, so an uninformed `false` must not unlock a chat
        // this client knows is locked. LockChanged is the
        // authoritative path; the fake's lock echoes carry the
        // full state and still land.
        if !merged.locked {
            if let Some(known) = self.chats.iter().find(|c| c.id == id) {
                merged.locked = known.locked;
            }
        }
        if !merged.archived {
            if let Some(known) = self.chats.iter().find(|c| c.id == id) {
                merged.archived = known.archived;
            }
        }
        // Same protection for the unread count: a message-arrival
        // upsert reports 0 without knowing the count (the wire
        // does not carry it there), so it must not reset a count
        // this client already knows. Message upserts own the
        // increments, and read state has its own authoritative
        // path (ChatReadChanged events, which set the count
        // outright, to 0 included); this merge only protects the
        // known value. An upsert that does report a count (the
        // fake's initial chat list carries the real counts)
        // still lands, because the protection only ever
        // preserves a known count against a reported 0.
        if merged.unread == 0 {
            if let Some(known) = self.chats.iter().find(|c| c.id == id) {
                merged.unread = known.unread;
            }
        }
        // Same protection for the title, in the identifier
        // direction: a message-arrival upsert titles its chat
        // with the raw identifier (the wire carries no subject
        // or contact name on that path), so a raw incoming title
        // must never stamp over a title this client has already
        // resolved, in the mirror or in the archive. A resolved
        // incoming title always lands: it is how renames and
        // healing arrive. Without this, the most active group
        // reverted to its raw JID on every new message, because
        // live traffic re-stamped it faster than healing could
        // land (the v1.6.6 live retest defect).
        if is_raw_identifier(&merged.title) {
            if let Some(known) = self.chats.iter().find(|c| c.id == id) {
                if !known.title.is_empty() && !is_raw_identifier(&known.title) {
                    merged.title = known.title.clone();
                }
            }
        }
        // Same protection in the saved-name direction: a direct
        // chat whose contact name the session directory already
        // knows (learned from a history payload, a contact
        // update, or a rename) keeps that name over an arrival
        // upsert's lesser title. An arrival titles its chat from
        // the sender's current push name, and a push name must
        // never downgrade a saved name; the backend resolves its
        // own titles through its identity book, and this is the
        // same rule on the host side for upserts from any source.
        if live.kind == LiveChatKind::Dm {
            if let Some(session) = &self.live {
                if let Some(name) = session.names.get(&live.jid) {
                    if !name.is_empty() && !is_raw_identifier(name) {
                        merged.title = name.clone();
                    }
                }
            }
        }
        // Community structure rides the same upsert: a parent
        // maps to itself, a linked subgroup to its parent, and
        // the community tag is the parent's title once known.
        if merged.is_parent {
            self.chat_parents.insert(id, id);
        }
        if let Some(parent) = &merged.parent_jid {
            self.chat_parents.insert(id, jid_id(parent));
        }
        if let Some(session) = &mut self.live {
            if let Some(store) = &session.store {
                let _ = store.upsert_chat(&merged);
            }
            session.jids.insert(id, merged.jid.clone());
            session.times.insert(id, merged.timestamp);
        }
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == id) {
            let fresh = to_chat(&merged);
            chat.title = fresh.title;
            chat.preview = fresh.preview;
            chat.preview_sender = fresh.preview_sender;
            chat.time = fresh.time;
            chat.unread = fresh.unread;
            // These take the merged values, so the protection
            // above (known state survives an uninformed upsert)
            // reaches the mirror too, while an informed upsert
            // still lands. Dropping them here once made
            // phone-side archive/pin (and the fake's LeaveGroup)
            // invisible until restart (logic review F14).
            chat.archived = fresh.archived;
            chat.pinned = fresh.pinned;
            chat.locked = fresh.locked;
            chat.ephemeral_secs = fresh.ephemeral_secs;
            chat.muted = fresh.muted;
        } else {
            self.chats.push(to_chat(&merged));
        }
        // The community tag is the parent community's title:
        // the parent's own tag is its title, a linked subgroup
        // takes its parent's title once the parent is known,
        // and a parent landing now retags its known children.
        if merged.is_parent && !is_raw_identifier(&merged.title) {
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == id) {
                chat.community = Some(merged.title.clone());
            }
            let children: Vec<u64> = self
                .chat_parents
                .iter()
                .filter(|(_, parent)| **parent == id)
                .map(|(child, _)| *child)
                .collect();
            for child in children {
                if let Some(chat) = self.chats.iter_mut().find(|c| c.id == child) {
                    chat.community = Some(merged.title.clone());
                }
            }
        }
        if let Some(parent) = &merged.parent_jid {
            let pid = jid_id(parent);
            let tag = self
                .chats
                .iter()
                .find(|c| c.id == pid)
                .map(|c| c.title.clone())
                .filter(|t| !t.is_empty() && !is_raw_identifier(t));
            if let Some(tag) = tag {
                if let Some(chat) = self.chats.iter_mut().find(|c| c.id == id) {
                    chat.community = Some(tag);
                }
            }
        }
        self.sort_live_chats();
    }
}

#[cfg(test)]
mod burst_drain_tests {
    use super::*;
    use crate::state::{App, Args};

    const CHAT_JID: &str = "6281234567890@s.whatsapp.net";
    const BASE_TS: i64 = 1_700_000_000;

    /// An app in live mode whose backend channel the test feeds
    /// by hand, with an in-memory archive attached so the batch
    /// flush is exercised too. The command receiver is returned
    /// with the app: dropping it would close the channel the
    /// session sends commands on.
    pub(super) fn live_app() -> (
        App,
        std::sync::mpsc::Sender<BackendEvent>,
        egui::Context,
        std::sync::mpsc::Receiver<BackendCommand>,
    ) {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = App::new(Args::parse(), &ctx);
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
            poll_ballots: HashMap::new(),
            event_ballots: HashMap::new(),
        });
        app.live_mode = true;
        (app, evt_tx, ctx, cmd_rx)
    }

    pub(super) fn incoming(id: &str, ts: i64) -> BackendEvent {
        BackendEvent::MessageUpsert(LiveMessage::text(
            id,
            CHAT_JID,
            Some("Sender".to_owned()),
            format!("message {id}"),
            ts,
            false,
        ))
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

    pub(super) fn stored_messages(app: &App) -> &Vec<(i64, Message)> {
        app.live.as_ref().unwrap().messages.get(&jid_id(CHAT_JID)).expect("the chat has messages")
    }

    pub(super) fn call_changed(
        call_id: &str,
        phase: backend::CallPhase,
        seconds: u32,
        reason: Option<&str>,
    ) -> BackendEvent {
        BackendEvent::CallChanged {
            call_id: call_id.to_owned(),
            chat_jid: CHAT_JID.to_owned(),
            phase,
            seconds,
            reason: reason.map(str::to_owned),
        }
    }

    pub(super) fn archived_calls(app: &App) -> Vec<backend::CallRecord> {
        app.live.as_ref().unwrap().store.as_ref().unwrap().call_history().unwrap()
    }

    /// v1.6.7: a call record synced from the phone's history
    /// lands in the archive and in the Calls panel data, filed
    /// under its chat, without any live call taking place.
    #[test]
    pub(super) fn synced_call_history_lands_in_the_archive_and_panel() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::CallHistoryRecord {
            record: backend::CallRecord {
                id: 0,
                call_id: "synced-1".to_owned(),
                chat_jid: CHAT_JID.to_owned(),
                video: true,
                outgoing: false,
                missed: true,
                duration_secs: 0,
                started_at: BASE_TS - 86_400,
            },
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let history = archived_calls(&app);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].call_id, "synced-1");
        assert!(history[0].missed && history[0].video);
        assert_eq!(app.calls.live_entries.len(), 1);
        assert_eq!(app.calls.live_entries[0].chat_id, jid_id(CHAT_JID));
        assert!(app.calls.live_entries[0].missed);
    }

    /// v1.6.7: the phone's favorites list is adopted as the
    /// complete set: listed chats become favorites in the mirror
    /// and the archive, and a later sync that omits a chat
    /// un-favorites it.
    #[test]
    pub(super) fn favorites_sync_adopts_the_complete_phone_set() {
        const OTHER: &str = "6289999999999@s.whatsapp.net";
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        tx.send(dm_upsert(OTHER)).unwrap();
        tx.send(BackendEvent::FavoritesSync { chat_jids: vec![CHAT_JID.to_owned()] }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_of(&app, jid_id(CHAT_JID)).favorite);
        assert!(!chat_of(&app, jid_id(OTHER)).favorite);
        let stored = app.live.as_ref().unwrap().store.as_ref().unwrap().chat_favorites().unwrap();
        assert_eq!(stored, vec![CHAT_JID.to_owned()]);
        tx.send(BackendEvent::FavoritesSync { chat_jids: vec![OTHER.to_owned()] }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(!chat_of(&app, jid_id(CHAT_JID)).favorite, "omission un-favorites");
        assert!(chat_of(&app, jid_id(OTHER)).favorite);
    }

    /// v1.6.7: avatar events drive the cache lifecycle: bytes
    /// are staged and archived, a change event drops both, and a
    /// missing answer records the negative in both places.
    #[test]
    pub(super) fn avatar_events_cache_invalidate_and_negative() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::AvatarBytes {
            jid: CHAT_JID.to_owned(),
            picture_id: Some("pic-1".to_owned()),
            bytes: png_bytes(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        {
            let session = app.live.as_ref().unwrap();
            assert!(session.avatar_bytes.contains_key(CHAT_JID), "bytes staged for painting");
            let store = session.store.as_ref().unwrap();
            let (_, bytes, _) = store.avatar(CHAT_JID).unwrap().unwrap();
            assert!(bytes.is_some(), "bytes archived for the next launch");
        }
        tx.send(BackendEvent::AvatarChanged { jid: CHAT_JID.to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        {
            let session = app.live.as_ref().unwrap();
            assert!(!session.avatar_bytes.contains_key(CHAT_JID), "a change drops the photo");
            let store = session.store.as_ref().unwrap();
            assert!(store.avatar(CHAT_JID).unwrap().is_none(), "and its archive row");
        }
        tx.send(BackendEvent::AvatarMissing { jid: CHAT_JID.to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert!(session.avatar_missing.contains_key(CHAT_JID), "the negative is remembered");
        let store = session.store.as_ref().unwrap();
        let (_, bytes, _) = store.avatar(CHAT_JID).unwrap().unwrap();
        assert!(bytes.is_none(), "the negative is archived without bytes");
    }

    /// An unanswered picture lookup (transport failure, or a
    /// LID whose mapping is still unknown) must not latch the
    /// in-flight mark for the whole session: the outcome event
    /// clears it, records no persisted miss, and the answered
    /// outcomes clear it too, so the negative's 24-hour re-ask
    /// can fire. Before the fix the mark was only ever removed
    /// by a picture change, so one failure froze the avatar
    /// until restart.
    #[test]
    pub(super) fn avatar_outcomes_clear_the_in_flight_mark() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        {
            let session = app.live.as_mut().unwrap();
            session.avatar_requested.insert(CHAT_JID.to_owned());
        }
        tx.send(BackendEvent::AvatarUnavailable { jid: CHAT_JID.to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        {
            let session = app.live.as_ref().unwrap();
            assert!(!session.avatar_requested.contains(CHAT_JID), "unavailable clears the mark");
            let store = session.store.as_ref().unwrap();
            assert!(store.avatar(CHAT_JID).unwrap().is_none(), "no miss is persisted");
        }
        {
            let session = app.live.as_mut().unwrap();
            session.avatar_requested.insert(CHAT_JID.to_owned());
        }
        tx.send(BackendEvent::AvatarMissing { jid: CHAT_JID.to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert!(!session.avatar_requested.contains(CHAT_JID), "an answer clears the mark");
        assert!(session.avatar_missing.contains_key(CHAT_JID), "the miss is remembered");
    }

    /// A backend error that is not a send failure must not
    /// mark an in-flight send failed: the error arm carries
    /// every failure, and an unrelated one (a rename, a lookup)
    /// says nothing about the send. A send failure still marks
    /// the oldest pending echo.
    #[test]
    pub(super) fn unrelated_backend_errors_leave_pending_sends_alone() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        drain_all(&mut app, &ctx);
        let chat_id = jid_id(CHAT_JID);
        {
            let session = app.live.as_mut().unwrap();
            let echo = Message {
                delivery: Some(Delivery::Sending),
                ..Message::mine(9_000_001, "on its way".to_owned(), "…")
            };
            session.messages.entry(chat_id).or_default().push((BASE_TS, echo));
        }
        let delivery_of = |app: &App| app.live.as_ref().unwrap().messages[&chat_id][0].1.delivery;
        tx.send(BackendEvent::Error { message: "Renaming the group failed: boom".to_owned() })
            .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            delivery_of(&app),
            Some(Delivery::Sending),
            "an unrelated error is not a send outcome"
        );
        tx.send(BackendEvent::Error { message: "Sending failed: boom".to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(delivery_of(&app), Some(Delivery::Failed), "a send failure marks the echo");
    }

    /// A picture negative recorded for a LID identity while its
    /// LID-to-phone mapping was unknown may be a LID-form miss
    /// rather than a true absence: when the mapping is learned
    /// (the healer broadcasts the phone-formatted name for the
    /// LID), the negative is dropped so the next paint retries
    /// the fetch, which now resolves under the phone form.
    #[test]
    pub(super) fn learned_lid_mapping_drops_the_avatar_negative() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let lid = "123456789012345@lid";
        tx.send(BackendEvent::AvatarMissing { jid: lid.to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            app.live.as_ref().unwrap().avatar_missing.contains_key(lid),
            "the negative is remembered"
        );
        tx.send(BackendEvent::SenderNamed {
            sender_jid: lid.to_owned(),
            name: "+628123456789".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert!(!session.avatar_missing.contains_key(lid), "the mapping drops the negative");
        let store = session.store.as_ref().unwrap();
        assert!(store.avatar(lid).unwrap().is_none(), "and its archive row");
    }

    /// v1.6.7: the defect behind the owner's largest group
    /// showing its raw JID: a live upsert titled with the raw
    /// identifier must not re-stamp a healed group title, in the
    /// mirror or the archive, while a resolved rename still
    /// lands.
    #[test]
    pub(super) fn healed_group_title_survives_a_raw_live_upsert() {
        const GROUP: &str = "120363412811529147@g.us";
        let group_upsert = |title: &str| {
            BackendEvent::ChatUpsert(LiveChat {
                jid: GROUP.to_owned(),
                title: title.to_owned(),
                kind: LiveChatKind::Group,
                preview: String::new(),
                preview_sender: None,
                timestamp: BASE_TS,
                unread: 0,
                archived: false,
                locked: false,
                pinned: false,
                ephemeral_secs: 0,
                muted: false,
                mute_until: 0,
                parent_jid: None,
                is_parent: false,
            })
        };
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(group_upsert("Grup Kebesaran")).unwrap();
        drain_all(&mut app, &ctx);
        tx.send(group_upsert("120363412811529147")).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_of(&app, jid_id(GROUP)).title, "Grup Kebesaran");
        let stored = app.live.as_ref().unwrap().store.as_ref().unwrap().chats().unwrap();
        assert_eq!(stored.iter().find(|c| c.jid == GROUP).unwrap().title, "Grup Kebesaran");
        tx.send(group_upsert("Grup Kebesaran Baru")).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_of(&app, jid_id(GROUP)).title, "Grup Kebesaran Baru");
    }

    /// v1.6.12: the identity discipline on titles. A direct
    /// chat's arrival upsert titles from the sender's push name;
    /// once the session directory knows the saved name, arrivals
    /// must not downgrade the title back to the push name. A
    /// heal rename never replaces a resolved title, an
    /// authoritative rename does (mirror and archive), a title
    /// healed to the formatted phone number still upgrades to a
    /// name, and a channel's healed name never enters the person
    /// directory.
    #[test]
    pub(super) fn dm_titles_follow_the_identity_discipline() {
        let dm_upsert_title = |title: &str| {
            BackendEvent::ChatUpsert(LiveChat {
                jid: CHAT_JID.to_owned(),
                title: title.to_owned(),
                kind: LiveChatKind::Dm,
                preview: String::new(),
                preview_sender: None,
                timestamp: BASE_TS,
                unread: 0,
                archived: false,
                locked: false,
                pinned: false,
                ephemeral_secs: 0,
                muted: false,
                mute_until: 0,
                parent_jid: None,
                is_parent: false,
            })
        };
        let stored_title = |app: &App| {
            app.live
                .as_ref()
                .unwrap()
                .store
                .as_ref()
                .unwrap()
                .chats()
                .unwrap()
                .iter()
                .find(|c| c.jid == CHAT_JID)
                .unwrap()
                .title
                .clone()
        };
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        // The chat arrives titled by the push name; the saved
        // name is learned (a history payload or contact update
        // broadcasts it), and the next push-name arrival must
        // not downgrade the title.
        tx.send(dm_upsert_title("Push Sarah")).unwrap();
        drain_all(&mut app, &ctx);
        tx.send(BackendEvent::SenderNamed {
            sender_jid: CHAT_JID.to_owned(),
            name: "Saved Sarah".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        tx.send(dm_upsert_title("Push Sarah")).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_of(&app, jid_id(CHAT_JID)).title, "Saved Sarah");
        assert_eq!(stored_title(&app), "Saved Sarah");
        // A heal rename cannot replace a resolved title; an
        // authoritative one can, in the mirror and the archive.
        tx.send(BackendEvent::ChatRenamed {
            chat_jid: CHAT_JID.to_owned(),
            title: "Heal Name".to_owned(),
            authoritative: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_of(&app, jid_id(CHAT_JID)).title, "Saved Sarah");
        tx.send(BackendEvent::ChatRenamed {
            chat_jid: CHAT_JID.to_owned(),
            title: "Sarah Wijaya".to_owned(),
            authoritative: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_of(&app, jid_id(CHAT_JID)).title, "Sarah Wijaya");
        assert_eq!(stored_title(&app), "Sarah Wijaya");
        // A channel's healed name stays out of the person
        // directory.
        tx.send(BackendEvent::ChatRenamed {
            chat_jid: "120363999888777@newsletter".to_owned(),
            title: "Some Channel".to_owned(),
            authoritative: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert!(!session.names.contains_key("120363999888777@newsletter"));
    }

    /// v1.6.12: a direct chat healed to its formatted phone
    /// number upgrades when the name arrives through the heal
    /// path (the number is a healable state, not a resolution).
    #[test]
    pub(super) fn phone_healed_dm_title_upgrades_to_the_name() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::ChatUpsert(LiveChat {
            jid: CHAT_JID.to_owned(),
            title: "+6281234567890".to_owned(),
            kind: LiveChatKind::Dm,
            preview: String::new(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
        }))
        .unwrap();
        drain_all(&mut app, &ctx);
        tx.send(BackendEvent::ChatRenamed {
            chat_jid: CHAT_JID.to_owned(),
            title: "Saved Sarah".to_owned(),
            authoritative: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_of(&app, jid_id(CHAT_JID)).title, "Saved Sarah");
    }

    /// v1.6.7: an undecryptable message the wire says to show
    /// becomes a placeholder in the conversation, exactly once:
    /// a copy the archive already holds is never duplicated.
    #[test]
    pub(super) fn undecryptable_placeholder_is_inserted_once() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        let unavailable = BackendEvent::MessageUnavailable {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "lost-1".to_owned(),
            sender_jid: Some(CHAT_JID.to_owned()),
            from_me: false,
            timestamp: BASE_TS,
        };
        tx.send(unavailable.clone()).unwrap();
        tx.send(unavailable).unwrap();
        drain_all(&mut app, &ctx);
        let messages = stored_messages(&app);
        assert_eq!(messages.len(), 1, "one placeholder, not two");
        assert!(messages[0].1.body.contains("could not be decrypted"));
    }

    /// Item 113: an outgoing call that connects and ends is
    /// recorded in the live history (and in the archive behind
    /// it) with the duration the backend reported.
    #[test]
    pub(super) fn connected_outgoing_call_is_recorded_with_its_duration() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        for event in [
            call_changed("call-1", backend::CallPhase::Ringing, 0, None),
            call_changed("call-1", backend::CallPhase::Active, 0, None),
            call_changed("call-1", backend::CallPhase::Active, 65, None),
            call_changed("call-1", backend::CallPhase::Ended, 65, Some("Call ended")),
        ] {
            tx.send(event).unwrap();
        }
        drain_all(&mut app, &ctx);
        assert_eq!(app.calls.live_entries.len(), 1);
        let entry = &app.calls.live_entries[0];
        assert_eq!(entry.direction, CallDirection::Outgoing);
        assert!(!entry.missed, "a connected call is not missed");
        assert_eq!(entry.duration_secs, 65);
        assert_eq!(entry.chat_id, jid_id(CHAT_JID));
        let history = archived_calls(&app);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].call_id, "call-1");
        assert!(history[0].outgoing && !history[0].missed);
        assert_eq!(history[0].duration_secs, 65);
    }

    /// Item 113: an outgoing call the peer never answers is
    /// recorded as an outgoing entry with no duration, not as a
    /// missed call (missed is an incoming outcome).
    #[test]
    pub(super) fn unanswered_outgoing_call_is_recorded_not_missed() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(call_changed("call-2", backend::CallPhase::Ringing, 0, None)).unwrap();
        tx.send(call_changed("call-2", backend::CallPhase::Ended, 0, Some("No answer"))).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.calls.live_entries.len(), 1);
        let entry = &app.calls.live_entries[0];
        assert_eq!(entry.direction, CallDirection::Outgoing);
        assert!(!entry.missed);
        assert_eq!(entry.duration_secs, 0);
    }

    /// Item 113: an incoming call that rings out unanswered is
    /// recorded as missed, in the live list and the archive.
    #[test]
    pub(super) fn unanswered_incoming_call_is_recorded_as_missed() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::IncomingCall {
            call_id: "call-3".to_owned(),
            chat_jid: CHAT_JID.to_owned(),
            video: false,
        })
        .unwrap();
        tx.send(call_changed("call-3", backend::CallPhase::Ended, 0, Some("Missed call"))).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.calls.live_entries.len(), 1);
        let entry = &app.calls.live_entries[0];
        assert_eq!(entry.direction, CallDirection::Incoming);
        assert!(entry.missed, "an unanswered incoming call is missed");
        assert_eq!(entry.duration_secs, 0);
        let history = archived_calls(&app);
        assert_eq!(history.len(), 1);
        assert!(history[0].missed);
    }

    /// Item 113: a call the user declines is recorded as a
    /// normal incoming entry, not a missed one: declining is an
    /// answer. The decline goes through the same action the
    /// overlay's button uses, and a missed-call report for the
    /// same call afterwards adds no second entry.
    #[test]
    pub(super) fn declined_incoming_call_is_recorded_but_not_missed() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::IncomingCall {
            call_id: "call-4".to_owned(),
            chat_jid: CHAT_JID.to_owned(),
            video: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.reject_active_call();
        assert!(
            matches!(cmd_rx.try_recv(), Ok(BackendCommand::RejectCall)),
            "the decline sends the reject command"
        );
        tx.send(call_changed("call-4", backend::CallPhase::Ended, 0, Some("Declined"))).unwrap();
        tx.send(BackendEvent::CallMissed {
            call_id: "call-4".to_owned(),
            chat_jid: CHAT_JID.to_owned(),
            at: BASE_TS,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.calls.live_entries.len(), 1, "one call, one entry");
        let entry = &app.calls.live_entries[0];
        assert_eq!(entry.direction, CallDirection::Incoming);
        assert!(!entry.missed, "a declined call is not a missed call");
        assert_eq!(archived_calls(&app).len(), 1);
    }

    /// Item 113: a missed call the app never showed ringing (the
    /// CallMissed path: another call was in progress, or the call
    /// rang while this device was offline) is still recorded,
    /// stamped when it rang rather than when it was reported,
    /// and a repeat report of it changes nothing.
    #[test]
    pub(super) fn missed_call_event_records_an_entry_stamped_when_it_rang() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let rang_at = BASE_TS - 7_200;
        for _ in 0..2 {
            tx.send(BackendEvent::CallMissed {
                call_id: "call-5".to_owned(),
                chat_jid: CHAT_JID.to_owned(),
                at: rang_at,
            })
            .unwrap();
        }
        drain_all(&mut app, &ctx);
        assert_eq!(app.calls.live_entries.len(), 1);
        let entry = &app.calls.live_entries[0];
        assert_eq!(entry.direction, CallDirection::Incoming);
        assert!(entry.missed);
        assert_eq!(entry.timestamp, rang_at);
        assert_eq!(entry.chat_id, jid_id(CHAT_JID));
        assert_eq!(archived_calls(&app).len(), 1);
    }

    /// Perfection round: a call that never rang here surfaces
    /// by name, not only as a Calls list entry: the CallMissed
    /// event also leaves a notice naming the caller through the
    /// roster.
    #[test]
    pub(super) fn missed_call_event_names_the_caller_in_a_notice() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(incoming("m-1", BASE_TS)).unwrap();
        drain_all(&mut app, &ctx);
        let title = app
            .chats
            .iter()
            .find(|c| c.id == jid_id(CHAT_JID))
            .map(|c| c.title.clone())
            .expect("the upsert created the chat");
        assert!(!title.is_empty());
        app.notice = None;
        tx.send(BackendEvent::CallMissed {
            call_id: "call-missed-1".to_owned(),
            chat_jid: CHAT_JID.to_owned(),
            at: BASE_TS,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.notice_str(),
            Some(format!("Missed call from {title}").as_str()),
            "the notice names the caller"
        );
        assert_eq!(
            app.notice.as_ref().map(|n| n.kind),
            Some(crate::state::NoticeKind::Info),
            "a missed call is a fact, not a failure"
        );
    }

    /// Perfection round: a media download rides the in-flight
    /// set from its command to the backend's answer: MediaSaved
    /// clears the message's entry (its chip stops spinning) and
    /// leaves the saved-path notice.
    #[test]
    pub(super) fn media_saved_clears_the_download_in_flight() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mid = message_id(CHAT_JID, "doc-1");
        app.media.downloads.insert(mid);
        tx.send(BackendEvent::MediaSaved {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "doc-1".to_owned(),
            path: "/tmp/doc.pdf".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!app.media.downloads.contains(&mid), "the chip stops spinning");
        assert_eq!(app.notice_str(), Some("Saved to /tmp/doc.pdf"));
    }

    /// Item 130: the call ringing here is answered on the
    /// phone. The elsewhere event ends the overlay with that
    /// reason, and the history entry is an incoming call that
    /// is not missed: the call was answered, just not here.
    #[test]
    pub(super) fn incoming_call_answered_elsewhere_ends_the_overlay_and_is_not_missed() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::IncomingCall {
            call_id: "call-7".to_owned(),
            chat_jid: CHAT_JID.to_owned(),
            video: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let active = app.calls.active.as_ref().expect("the call is on screen");
        assert_eq!(active.phase, crate::call_ui::CallUiPhase::Incoming);
        tx.send(BackendEvent::CallEndedElsewhere {
            call_id: "call-7".to_owned(),
            chat_jid: CHAT_JID.to_owned(),
            answered: true,
            at: BASE_TS,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let active = app.calls.active.as_ref().expect("the ended card is still shown");
        assert_eq!(active.phase, crate::call_ui::CallUiPhase::Ended);
        assert_eq!(active.reason.as_deref(), Some("Answered on another device"));
        assert_eq!(app.calls.live_entries.len(), 1);
        let entry = &app.calls.live_entries[0];
        assert_eq!(entry.direction, CallDirection::Incoming);
        assert!(!entry.missed, "a call answered on another device is not missed");
        assert_eq!(entry.duration_secs, 0);
        let history = archived_calls(&app);
        assert_eq!(history.len(), 1);
        assert!(!history[0].missed && !history[0].outgoing);
    }

    /// Item 130: the call ringing here is declined on the
    /// phone. Same shape as the answered case: the overlay
    /// ends with the elsewhere reason and the entry is a
    /// normal incoming call, not a missed one.
    #[test]
    pub(super) fn incoming_call_declined_elsewhere_is_recorded_not_missed() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::IncomingCall {
            call_id: "call-8".to_owned(),
            chat_jid: CHAT_JID.to_owned(),
            video: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        tx.send(BackendEvent::CallEndedElsewhere {
            call_id: "call-8".to_owned(),
            chat_jid: CHAT_JID.to_owned(),
            answered: false,
            at: BASE_TS,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let active = app.calls.active.as_ref().expect("the ended card is still shown");
        assert_eq!(active.phase, crate::call_ui::CallUiPhase::Ended);
        assert_eq!(active.reason.as_deref(), Some("Declined on another device"));
        assert_eq!(app.calls.live_entries.len(), 1);
        let entry = &app.calls.live_entries[0];
        assert_eq!(entry.direction, CallDirection::Incoming);
        assert!(!entry.missed, "a call declined on another device is not missed");
        assert_eq!(archived_calls(&app).len(), 1);
    }

    /// Item 130: a call that ended elsewhere without ever
    /// ringing here (another call held the line, or it rang
    /// while this device was away) is still recorded, as an
    /// incoming call that is not missed, stamped with the
    /// event's own time; a repeat report changes nothing.
    #[test]
    pub(super) fn unshown_call_ended_elsewhere_is_recorded_not_missed() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let ended_at = BASE_TS - 3_600;
        for _ in 0..2 {
            tx.send(BackendEvent::CallEndedElsewhere {
                call_id: "call-9".to_owned(),
                chat_jid: CHAT_JID.to_owned(),
                answered: true,
                at: ended_at,
            })
            .unwrap();
        }
        drain_all(&mut app, &ctx);
        assert!(app.calls.active.is_none(), "no overlay for a call that never rang here");
        assert_eq!(app.calls.live_entries.len(), 1);
        let entry = &app.calls.live_entries[0];
        assert_eq!(entry.direction, CallDirection::Incoming);
        assert!(!entry.missed);
        assert_eq!(entry.timestamp, ended_at);
        assert_eq!(archived_calls(&app).len(), 1);
    }

    /// Item 130: an elsewhere report for a second call while
    /// one is ringing here records the second call and leaves
    /// the ringing one untouched: it still ends through its
    /// own end event afterwards, as a missed call.
    #[test]
    pub(super) fn elsewhere_report_for_another_call_leaves_the_ringing_one_alone() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::IncomingCall {
            call_id: "call-10".to_owned(),
            chat_jid: CHAT_JID.to_owned(),
            video: false,
        })
        .unwrap();
        tx.send(BackendEvent::CallEndedElsewhere {
            call_id: "call-11".to_owned(),
            chat_jid: CHAT_JID.to_owned(),
            answered: true,
            at: BASE_TS,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let active = app.calls.active.as_ref().expect("the first call is still on screen");
        assert_eq!(active.call_id, "call-10");
        assert_eq!(active.phase, crate::call_ui::CallUiPhase::Incoming);
        assert_eq!(app.calls.live_entries.len(), 1);
        assert!(!app.calls.live_entries[0].missed);
        // The ringing call ends on its own path, unaffected.
        tx.send(call_changed("call-10", backend::CallPhase::Ended, 0, Some("Missed call")))
            .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.calls.live_entries.len(), 2);
        let missed = app.calls.live_entries.iter().filter(|e| e.missed).collect::<Vec<_>>();
        assert_eq!(missed.len(), 1, "only the call that rang out here is missed");
        assert_eq!(archived_calls(&app).len(), 2);
    }

    /// Item 113: the recorded history is durable across app
    /// sessions: a new session loading the same archive (the
    /// load start_live performs) renders the same entries, and
    /// the loaded call IDs keep a replayed report of an already
    /// archived call from entering twice.
    #[test]
    pub(super) fn call_history_is_reloaded_from_the_archive_by_a_new_session() {
        let dir =
            std::env::temp_dir().join(format!("zaprust-app-calls-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("archive.db");
        let _ = std::fs::remove_file(&path);
        let key = [3u8; 32];
        {
            let (mut app, tx, ctx, _cmd_rx) = live_app();
            app.live.as_mut().unwrap().store = Some(store::Store::open(&path, &key).unwrap());
            for event in [
                call_changed("call-6", backend::CallPhase::Ringing, 0, None),
                call_changed("call-6", backend::CallPhase::Active, 42, None),
                call_changed("call-6", backend::CallPhase::Ended, 42, Some("Call ended")),
            ] {
                tx.send(event).unwrap();
            }
            drain_all(&mut app, &ctx);
            assert_eq!(app.calls.live_entries.len(), 1);
        }
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.live.as_mut().unwrap().store = Some(store::Store::open(&path, &key).unwrap());
        app.calls.live_entries = Vec::new();
        app.load_call_history();
        assert_eq!(app.calls.live_entries.len(), 1);
        let entry = &app.calls.live_entries[0];
        assert_eq!(entry.direction, CallDirection::Outgoing);
        assert_eq!(entry.duration_secs, 42);
        // A missed-call replay of the archived call adds nothing.
        tx.send(BackendEvent::CallMissed {
            call_id: "call-6".to_owned(),
            chat_jid: CHAT_JID.to_owned(),
            at: BASE_TS,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.calls.live_entries.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    /// Privacy finding B1: a received message containing a URL
    /// must never make the client fetch a preview from the
    /// sender-chosen host (that leaks the recipient's address and
    /// online timing). Applying the upsert leaves the message
    /// without a preview; previews exist only when the sender's
    /// client attached one.
    #[test]
    pub(super) fn received_url_message_gets_no_fetched_preview() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "url-1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "look at https://example.com/page".to_owned(),
            BASE_TS,
            false,
        )))
        .unwrap();
        drain_all(&mut app, &ctx);
        let (_, msg) =
            stored_messages(&app).iter().find(|(_, m)| m.body.contains("example.com")).unwrap();
        assert!(msg.link_preview.is_none(), "a received link must not be fetched");
    }

    /// The other half of B1: a preview the sender's client
    /// attached to the message is carried through to the mirror
    /// untouched, so it can render with no network call.
    #[test]
    pub(super) fn sender_attached_preview_reaches_the_mirror() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mut live = LiveMessage::text(
            "prev-1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "look at https://example.com/page".to_owned(),
            BASE_TS,
            false,
        );
        live.link_preview = Some(backend::LivePreview {
            title: "Example Page".to_owned(),
            description: "A page about examples".to_owned(),
            url: "https://example.com/page".to_owned(),
            thumbnail: None,
        });
        tx.send(BackendEvent::MessageUpsert(live)).unwrap();
        drain_all(&mut app, &ctx);
        let (_, msg) =
            stored_messages(&app).iter().find(|(_, m)| m.body.contains("example.com")).unwrap();
        let preview = msg.link_preview.as_ref().expect("the attached preview survives");
        assert_eq!(preview.title, "Example Page");
        assert_eq!(preview.description, "A page about examples");
        assert_eq!(preview.url, "https://example.com/page");
    }

    /// Builds a view-once photo upsert whose body is its
    /// caption, the way the backend delivered it before the D2
    /// fix taught it not to promote the caption. The app must
    /// stay safe whatever body a view-once message carries.
    pub(super) fn view_once_photo(id: &str, caption: &str, ts: i64) -> LiveMessage {
        let mut live =
            LiveMessage::text(id, CHAT_JID, Some("Sender".to_owned()), caption, ts, false);
        live.kind = backend::LiveMessageKind::Media {
            media: backend::MediaKind::Photo,
            caption: caption.to_owned(),
            seconds: 0,
            size_bytes: 42_000,
        };
        live.view_once = true;
        live
    }

    /// Privacy finding D2: a view-once message's caption is part
    /// of its sealed payload and must not reach the chat-list
    /// preview, a notification popup (the lock screen), or the
    /// archive. The preview and the popup body carry only the
    /// generic placeholder; the archive row keeps the kind and
    /// the view-once flag but no caption.
    #[test]
    pub(super) fn view_once_caption_stays_out_of_preview_popups_and_archive() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(view_once_photo("vo-1", "a secret caption", BASE_TS)))
            .unwrap();
        drain_all(&mut app, &ctx);
        let chat_id = jid_id(CHAT_JID);
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert_eq!(chat.preview, core::model::VIEW_ONCE_PREVIEW, "the preview is the placeholder");
        assert!(!chat.preview.contains("secret"), "the caption must not be the preview");

        // The popup plan with the preview toggle on and off: the
        // caption appears in neither. On shows the placeholder,
        // off the generic notice.
        let on = crate::notify::NotifySettings {
            messages: true,
            groups: true,
            sound: false,
            preview: true,
        };
        let popups = crate::notify::plan_popups(on, &app.pending_notifications, None);
        assert_eq!(popups.len(), 1, "one popup for the one new message");
        assert_eq!(popups[0].1, core::model::VIEW_ONCE_PREVIEW);
        assert!(
            !popups[0].0.contains("secret") && !popups[0].1.contains("secret"),
            "the caption must not reach the popup: {popups:?}"
        );
        let off = crate::notify::NotifySettings { preview: false, ..on };
        let popups = crate::notify::plan_popups(off, &app.pending_notifications, None);
        assert_eq!(popups.len(), 1);
        assert_eq!(popups[0].1, "New message");

        // The archive row: kind + flag survive, the caption does not.
        let session = app.live.as_ref().unwrap();
        let stored = session.store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let row = &stored.iter().find(|(m, _)| m.id == "vo-1").unwrap().0;
        assert_eq!(row.body, "", "the archived body must not be the caption");
        assert!(row.view_once, "the view-once flag survives archiving");
        match &row.kind {
            backend::LiveMessageKind::Media { caption, .. } => {
                assert_eq!(caption, "", "the archived kind must not carry the caption");
            }
            other => panic!("expected the media kind to survive, got {other:?}"),
        }
    }

    /// Privacy finding D13: opening a view-once message
    /// persists the opened state with the message's stored
    /// flags, so the archive load after a restart seeds the
    /// opened set from it and the message never presents as
    /// unopened again.
    #[test]
    pub(super) fn view_once_opened_state_persists_with_the_message() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(view_once_photo("vo-1", "a caption", BASE_TS)))
            .unwrap();
        drain_all(&mut app, &ctx);
        let msg_id = message_id(CHAT_JID, "vo-1");

        app.mark_view_once_opened(msg_id);
        assert!(app.media.opened_view_once.contains(&msg_id), "the session set knows");

        let session = app.live.as_ref().unwrap();
        let stored = session.store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let (_, flags) = stored.iter().find(|(m, _)| m.id == "vo-1").unwrap();
        assert_ne!(
            flags & store::FLAG_VIEW_ONCE_OPENED,
            0,
            "the opened state is persisted with the message's flags"
        );
    }

    /// A photo upsert carrying its re-download reference, the
    /// shape the backend now delivers for every photo whose wire
    /// payload was complete.
    pub(super) fn photo_message(id: &str, ts: i64, view_once: bool) -> LiveMessage {
        let mut live =
            LiveMessage::text(id, CHAT_JID, Some("Sender".to_owned()), String::new(), ts, false);
        live.kind = backend::LiveMessageKind::Media {
            media: backend::MediaKind::Photo,
            caption: String::new(),
            seconds: 0,
            size_bytes: 123_456,
        };
        live.view_once = view_once;
        live.photo_ref = Some(backend::PhotoRef {
            direct_path: "/v/t62.7118-24/photo.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![8u8; 32],
            file_enc_sha256: vec![9u8; 32],
            file_length: 123_456,
        });
        live
    }

    /// A voice-note upsert carrying its re-download reference,
    /// the shape the backend delivers for every voice note whose
    /// wire payload was complete.
    pub(super) fn voice_message(id: &str, ts: i64) -> LiveMessage {
        let mut live =
            LiveMessage::text(id, CHAT_JID, Some("Sender".to_owned()), String::new(), ts, false);
        live.kind = backend::LiveMessageKind::Media {
            media: backend::MediaKind::Voice,
            caption: String::new(),
            seconds: 1,
            size_bytes: 4_096,
        };
        live.voice_ref = Some(backend::VoiceRef {
            direct_path: "/v/t62.7119-24/voice.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![8u8; 32],
            file_enc_sha256: vec![9u8; 32],
            file_length: 4_096,
        });
        live
    }

    /// A small real voice note, encoded in memory with the app's
    /// own recorder encoder: a one-second tone in Ogg/Opus.
    pub(super) fn voice_bytes() -> Vec<u8> {
        let pcm: Vec<i16> = (0..48_000)
            .map(|i| {
                let t = i as f64 / 48_000.0;
                ((t * 440.0 * std::f64::consts::TAU).sin() * 12_000.0) as i16
            })
            .collect();
        crate::recorder::encode_ogg_opus(&pcm).unwrap()
    }

    /// A small real image, encoded in memory, standing in for
    /// fetched photo bytes.
    pub(super) fn png_bytes() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 40, 40, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    /// Turns the harness session into a real one: the harness
    /// runs the fake backend (test mode, where the fixture photo
    /// is the disclosed content), while the photo-fetch behaviour
    /// under test is the real live session's.
    pub(super) fn make_real(app: &mut App) {
        app.live.as_mut().unwrap().fake = false;
    }

    /// Item 109: a real live session never paints the demo
    /// fixture for a received photo. Before any fetch the tile is
    /// Idle, the fetch command carries the archived reference,
    /// and a second request while one is running sends nothing.
    #[test]
    pub(super) fn photo_fetch_sends_the_command_with_the_archived_reference() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        make_real(&mut app);
        tx.send(BackendEvent::MessageUpsert(photo_message("ph-1", BASE_TS, false))).unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "ph-1");

        assert_eq!(app.photo_tile(mid), crate::media::PhotoTile::Idle);
        app.ensure_photo_fetch(mid);
        assert_eq!(app.photo_tile(mid), crate::media::PhotoTile::Loading);
        match cmd_rx.try_recv() {
            Ok(BackendCommand::FetchPhoto { chat_jid, message_id, photo_ref }) => {
                assert_eq!(chat_jid, CHAT_JID);
                assert_eq!(message_id, "ph-1");
                let reference = photo_ref.expect("the archived reference rides along");
                assert_eq!(reference.direct_path, "/v/t62.7118-24/photo.enc");
                assert_eq!(reference.file_length, 123_456);
            }
            other => panic!("expected a FetchPhoto command, got {other:?}"),
        }
        app.ensure_photo_fetch(mid);
        assert!(cmd_rx.try_recv().is_err(), "a running fetch is not duplicated");
    }

    /// Item 109: fetched bytes become the message's own texture,
    /// and the tile flips to Real. In the fake session the same
    /// message would have painted the fixture instead (the mode
    /// split the honesty rule draws).
    #[test]
    pub(super) fn photo_bytes_install_the_real_texture() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(photo_message("ph-1", BASE_TS, false))).unwrap();
        drain_all(&mut app, &ctx);
        let mid = message_id(CHAT_JID, "ph-1");
        assert_eq!(app.photo_tile(mid), crate::media::PhotoTile::Demo, "test mode fixture");

        make_real(&mut app);
        app.selected_chat = jid_id(CHAT_JID);
        app.ensure_photo_fetch(mid);
        tx.send(BackendEvent::PhotoBytes {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "ph-1".to_owned(),
            bytes: png_bytes(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.media.textures.contains_key(&mid), "the real bytes are the texture now");
        assert!(!app.media.photo_fetch.contains_key(&mid), "the fetch state clears");
        assert_eq!(app.photo_tile(mid), crate::media::PhotoTile::Real);
    }

    /// Item 109: a failed fetch marks the tile Failed (the
    /// placeholder state), and bytes that do not decode fail the
    /// same way. Neither path ever falls back to the demo photo
    /// in a real session.
    #[test]
    pub(super) fn photo_fetch_failure_marks_the_tile_failed() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        make_real(&mut app);
        tx.send(BackendEvent::MessageUpsert(photo_message("ph-1", BASE_TS, false))).unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "ph-1");

        app.ensure_photo_fetch(mid);
        tx.send(BackendEvent::PhotoFetchFailed {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "ph-1".to_owned(),
            message: "Download failed.".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.photo_tile(mid), crate::media::PhotoTile::Failed);

        app.retry_photo_fetch(mid);
        assert_eq!(app.photo_tile(mid), crate::media::PhotoTile::Loading);
        tx.send(BackendEvent::PhotoBytes {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "ph-1".to_owned(),
            bytes: vec![1, 2, 3],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.photo_tile(mid),
            crate::media::PhotoTile::Failed,
            "undecodable bytes are a failed fetch, not a photo"
        );
        assert!(!app.media.textures.contains_key(&mid));
    }

    /// Item 110: pressing play on a live voice note fetches its
    /// bytes with the archived reference riding along, and a
    /// second press while the fetch runs sends nothing new.
    #[test]
    pub(super) fn voice_play_fetches_with_the_archived_reference() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        make_real(&mut app);
        tx.send(BackendEvent::MessageUpsert(voice_message("vn-1", BASE_TS))).unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "vn-1");

        app.toggle_voice(mid);
        assert_eq!(app.media.voice_fetch.get(&mid), Some(&crate::state::VoiceFetchState::Loading));
        match cmd_rx.try_recv() {
            Ok(BackendCommand::FetchVoice { chat_jid, message_id, voice_ref }) => {
                assert_eq!(chat_jid, CHAT_JID);
                assert_eq!(message_id, "vn-1");
                let reference = voice_ref.expect("the archived reference rides along");
                assert_eq!(reference.direct_path, "/v/t62.7119-24/voice.enc");
                assert_eq!(reference.file_length, 4_096);
            }
            other => panic!("expected a FetchVoice command, got {other:?}"),
        }
        app.toggle_voice(mid);
        assert!(cmd_rx.try_recv().is_err(), "a running fetch is not duplicated");
        assert!(app.media.voice_player.is_none(), "nothing plays before the bytes land");
    }

    /// Item 110: fetched voice bytes decode and start the real
    /// playback engine (against the null sink on this machine);
    /// the play gesture then pauses the engine itself.
    #[test]
    pub(super) fn voice_bytes_start_the_playback_engine() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        make_real(&mut app);
        app.media.voice_sink = crate::player::VoiceSink::Null;
        tx.send(BackendEvent::MessageUpsert(voice_message("vn-1", BASE_TS))).unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "vn-1");

        app.toggle_voice(mid);
        tx.send(BackendEvent::VoiceBytes {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "vn-1".to_owned(),
            bytes: voice_bytes(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let player = app.media.voice_player.as_ref().expect("the engine session exists");
        assert_eq!(player.id, mid);
        assert!(player.is_playing(), "the engine is consuming the note");
        assert!(!app.media.voice_fetch.contains_key(&mid), "the fetch state clears");

        app.toggle_voice(mid);
        let player = app.media.voice_player.as_ref().expect("the session survives a pause");
        assert!(!player.is_playing(), "the pause reaches the engine");
    }

    /// Item 110: a failed fetch and bytes that do not decode both
    /// fail the note honestly: a Failed state and a spoken reason,
    /// never a player animating over silence.
    #[test]
    pub(super) fn voice_failures_mark_the_note_failed() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        make_real(&mut app);
        app.media.voice_sink = crate::player::VoiceSink::Null;
        tx.send(BackendEvent::MessageUpsert(voice_message("vn-1", BASE_TS))).unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "vn-1");

        app.toggle_voice(mid);
        tx.send(BackendEvent::VoiceFetchFailed {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "vn-1".to_owned(),
            message: "Download failed.".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.media.voice_fetch.get(&mid), Some(&crate::state::VoiceFetchState::Failed));
        assert!(app.media.voice_player.is_none());
        assert_eq!(app.notice_str(), Some("Download failed."));

        // Retrying fetches again; undecodable bytes fail the same
        // way instead of playing.
        app.toggle_voice(mid);
        assert_eq!(app.media.voice_fetch.get(&mid), Some(&crate::state::VoiceFetchState::Loading));
        tx.send(BackendEvent::VoiceBytes {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "vn-1".to_owned(),
            bytes: vec![1, 2, 3],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.media.voice_fetch.get(&mid),
            Some(&crate::state::VoiceFetchState::Failed),
            "undecodable bytes are a failed fetch, not playback"
        );
        assert!(app.media.voice_player.is_none());
        assert!(app.notice.is_some(), "the decode failure is said out loud");
    }

    /// Item 110: in demo (test) mode a voice note has no audio
    /// behind it, so pressing play states that plainly and starts
    /// nothing: no fetch, no player, no simulated playhead.
    #[test]
    pub(super) fn demo_voice_press_states_unavailable() {
        let (mut app, _tx, _ctx, cmd_rx) = live_app();
        app.live = None;
        app.live_mode = false;
        app.toggle_voice(42);
        assert_eq!(app.media.voice_fetch.get(&42), Some(&crate::state::VoiceFetchState::Failed));
        assert!(app.media.voice_player.is_none());
        assert_eq!(app.notice_str(), Some(crate::i18n::t("voice.demo_unavailable").as_str()));
        assert!(cmd_rx.try_recv().is_err(), "no fetch leaves a sessionless app");
    }

    /// Item 109, the view-once half: closing the viewer on a
    /// view-once photo whose real pixels were never shown does
    /// NOT spend the view. The opened set stays empty, the user
    /// is told, and the archived re-download reference survives
    /// so the photo can still be opened later.
    #[test]
    pub(super) fn view_once_close_without_shown_pixels_keeps_the_view() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        make_real(&mut app);
        tx.send(BackendEvent::MessageUpsert(photo_message("vo-ph", BASE_TS, true))).unwrap();
        drain_all(&mut app, &ctx);
        let mid = message_id(CHAT_JID, "vo-ph");
        app.media.viewing_photo = Some(mid);

        app.close_photo_viewer(mid, true);
        assert!(!app.media.opened_view_once.contains(&mid), "an unshown photo has not been viewed");
        assert!(app.notice.is_some(), "the kept view is said out loud");
        assert!(app.media.viewing_photo.is_none(), "the viewer still closes");
        let session = app.live.as_ref().unwrap();
        assert!(session.photo_refs.contains_key(&mid), "the reference survives for a retry");
        let stored = session.store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let row = &stored.iter().find(|(m, _)| m.id == "vo-ph").unwrap().0;
        assert!(row.photo_ref.is_some(), "the archived reference survives too");
    }

    /// Item 109, the other half: once the real pixels were
    /// actually painted, closing spends the view, and the spend
    /// is complete: the opened state persists, the decoded pixels
    /// are dropped, and the re-download reference is removed from
    /// the session and the archive, so the photo cannot be
    /// brought back.
    #[test]
    pub(super) fn view_once_close_after_shown_pixels_spends_the_view() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        make_real(&mut app);
        tx.send(BackendEvent::MessageUpsert(photo_message("vo-ph", BASE_TS, true))).unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "vo-ph");

        app.ensure_photo_fetch(mid);
        tx.send(BackendEvent::PhotoBytes {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "vo-ph".to_owned(),
            bytes: png_bytes(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.media.textures.contains_key(&mid));
        // The viewer painted the real pixels during this viewing.
        app.media.view_once_shown = Some(mid);
        app.media.viewing_photo = Some(mid);

        app.close_photo_viewer(mid, true);
        assert!(app.media.opened_view_once.contains(&mid), "the view is spent");
        assert!(!app.media.textures.contains_key(&mid), "the pixels are dropped");
        let session = app.live.as_ref().unwrap();
        assert!(!session.photo_refs.contains_key(&mid), "the session reference is dropped");
        let stored = session.store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let (row, flags) = stored.iter().find(|(m, _)| m.id == "vo-ph").unwrap();
        assert_ne!(flags & store::FLAG_VIEW_ONCE_OPENED, 0, "the spend persists");
        assert!(row.photo_ref.is_none(), "the archived reference is dropped");
    }

    /// The mode split, pinned from the other side: in test mode
    /// the fixture photo is the content, so closing the viewer on
    /// a view-once photo spends the view exactly as before.
    #[test]
    pub(super) fn view_once_close_in_test_mode_still_spends_the_view() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(photo_message("vo-ph", BASE_TS, true))).unwrap();
        drain_all(&mut app, &ctx);
        let mid = message_id(CHAT_JID, "vo-ph");
        app.media.viewing_photo = Some(mid);
        app.close_photo_viewer(mid, true);
        assert!(app.media.opened_view_once.contains(&mid));
    }

    /// A small real JPEG, encoded in memory, standing in for a
    /// video's wire thumbnail bytes.
    pub(super) fn jpeg_bytes() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(4, 3, image::Rgba([40, 120, 200, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut buf, image::ImageFormat::Jpeg).unwrap();
        buf.into_inner()
    }

    /// A video upsert carrying its re-download reference and,
    /// optionally, its embedded thumbnail: the shape the backend
    /// now delivers for every video whose wire payload was
    /// complete.
    pub(super) fn video_message(
        id: &str,
        ts: i64,
        view_once: bool,
        with_thumb: bool,
    ) -> LiveMessage {
        let mut live =
            LiveMessage::text(id, CHAT_JID, Some("Sender".to_owned()), String::new(), ts, false);
        live.kind = backend::LiveMessageKind::Media {
            media: backend::MediaKind::Video,
            caption: String::new(),
            seconds: 1,
            size_bytes: 3_992,
        };
        live.view_once = view_once;
        live.video_ref = Some(backend::VideoRef {
            direct_path: "/v/t62.7118-24/video.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![8u8; 32],
            file_enc_sha256: vec![9u8; 32],
            file_length: 3_992,
        });
        if with_thumb {
            live.video_thumbnail = Some(jpeg_bytes());
        }
        live
    }

    /// Item 111: a video's tile is its own wire thumbnail when
    /// those bytes arrived with the message, and the neutral
    /// tile when they did not. There is no third option: no mode
    /// ever paints a generated picture for a video.
    #[test]
    pub(super) fn video_tile_is_the_wire_thumbnail_or_neutral() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        make_real(&mut app);
        tx.send(BackendEvent::MessageUpsert(video_message("vid-1", BASE_TS, false, true))).unwrap();
        tx.send(BackendEvent::MessageUpsert(video_message("vid-2", BASE_TS + 1, false, false)))
            .unwrap();
        drain_all(&mut app, &ctx);
        let with = message_id(CHAT_JID, "vid-1");
        let without = message_id(CHAT_JID, "vid-2");
        assert_eq!(app.video_tile(with), crate::media::VideoTile::Thumb);
        assert_eq!(app.video_tile(without), crate::media::VideoTile::Neutral);
        // The thumbnail decodes into a real texture, and the
        // tile learns the frame's aspect from the poster (the
        // fixture JPEG is 4:3).
        let tex = App::video_thumb_texture(&mut app.media, app.live.as_ref(), &ctx, with);
        let (_tex_id, aspect) = tex.expect("the wire thumbnail decodes");
        assert!((aspect - 4.0 / 3.0).abs() < 0.01, "poster aspect was {aspect}");
        assert!(app.media.video_textures.contains_key(&with));
        // The archived copy round-trips too (later sessions).
        let stored = app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let row = &stored.iter().find(|(m, _)| m.id == "vid-1").unwrap().0;
        assert_eq!(row.video_thumbnail.as_deref(), Some(jpeg_bytes().as_slice()));
        assert!(row.video_ref.is_some());
    }

    /// Item 111: pressing play on a live video fetches its bytes
    /// with the archived reference riding along (and the
    /// view-once discipline for a view-once video); the route
    /// tracks the fetch state, and a second request while one
    /// runs sends nothing.
    #[test]
    pub(super) fn video_fetch_sends_the_command_with_the_archived_reference() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        make_real(&mut app);
        tx.send(BackendEvent::MessageUpsert(video_message("vid-1", BASE_TS, false, true))).unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "vid-1");

        assert_eq!(app.video_play_route(mid), crate::media::VideoPlayRoute::Idle);
        app.ensure_video_fetch(mid, false);
        assert_eq!(app.video_play_route(mid), crate::media::VideoPlayRoute::Loading);
        match cmd_rx.try_recv() {
            Ok(BackendCommand::FetchVideo { chat_jid, message_id, video_ref, view_once }) => {
                assert_eq!(chat_jid, CHAT_JID);
                assert_eq!(message_id, "vid-1");
                assert!(!view_once, "a regular video fetches to a cache file");
                let reference = video_ref.expect("the archived reference rides along");
                assert_eq!(reference.direct_path, "/v/t62.7118-24/video.enc");
                assert_eq!(reference.file_length, 3_992);
            }
            other => panic!("expected a FetchVideo command, got {other:?}"),
        }
        app.ensure_video_fetch(mid, false);
        assert!(cmd_rx.try_recv().is_err(), "a running fetch is not duplicated");

        // The landed file flips the route to Ready.
        tx.send(BackendEvent::VideoReady {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "vid-1".to_owned(),
            path: "/tmp/zaprust-test-video.mp4".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.video_play_route(mid), crate::media::VideoPlayRoute::Ready);
        assert_eq!(
            app.media.video_files.get(&mid).map(String::as_str),
            Some("/tmp/zaprust-test-video.mp4")
        );
    }

    /// Item 111: a failed video fetch marks the route Failed and
    /// a retry runs the fetch again.
    #[test]
    pub(super) fn video_fetch_failure_marks_the_route_failed() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        make_real(&mut app);
        tx.send(BackendEvent::MessageUpsert(video_message("vid-1", BASE_TS, false, false)))
            .unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "vid-1");

        app.ensure_video_fetch(mid, false);
        tx.send(BackendEvent::VideoFetchFailed {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "vid-1".to_owned(),
            message: "Download failed.".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.video_play_route(mid), crate::media::VideoPlayRoute::Failed);
        app.retry_video_fetch(mid, false);
        assert_eq!(app.video_play_route(mid), crate::media::VideoPlayRoute::Loading);
    }

    /// Row 22: a video whose payload carried no embedded
    /// thumbnail fetches its hosted poster with the archived
    /// reference riding along; the landed poster becomes the
    /// message's thumbnail in the session and in the archive,
    /// and the wire dimensions seed alongside.
    #[test]
    pub(super) fn poster_fetch_lands_as_the_videos_thumbnail() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        make_real(&mut app);
        let mut live = video_message("vid-p", BASE_TS, false, false);
        live.poster_ref = Some(backend::PosterRef {
            direct_path: "/v/t62.7118-24/poster.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![11u8; 32],
            file_enc_sha256: vec![12u8; 32],
        });
        live.video_width = Some(1280);
        live.video_height = Some(720);
        tx.send(BackendEvent::MessageUpsert(live)).unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "vid-p");
        {
            let session = app.live.as_ref().unwrap();
            assert!(session.poster_refs.contains_key(&mid), "the poster reference is seeded");
            assert_eq!(session.video_dims.get(&mid), Some(&(1280, 720)));
        }
        assert_eq!(app.video_tile(mid), crate::media::VideoTile::Neutral);

        app.ensure_poster_fetch(mid);
        match cmd_rx.try_recv() {
            Ok(BackendCommand::FetchVideoPoster { chat_jid, message_id, poster_ref }) => {
                assert_eq!(chat_jid, CHAT_JID);
                assert_eq!(message_id, "vid-p");
                let reference = poster_ref.expect("the archived reference rides along");
                assert_eq!(reference.direct_path, "/v/t62.7118-24/poster.enc");
            }
            other => panic!("expected a FetchVideoPoster command, got {other:?}"),
        }
        app.ensure_poster_fetch(mid);
        assert!(cmd_rx.try_recv().is_err(), "a running poster fetch is not duplicated");

        tx.send(BackendEvent::VideoPosterBytes {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "vid-p".to_owned(),
            bytes: jpeg_bytes(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.video_tile(mid), crate::media::VideoTile::Thumb);
        assert!(!app.media.poster_fetch.contains_key(&mid), "the fetch state clears on landing");
        let stored = app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let row = &stored.iter().find(|(m, _)| m.id == "vid-p").unwrap().0;
        assert_eq!(
            row.video_thumbnail.as_deref(),
            Some(jpeg_bytes().as_slice()),
            "the poster is archived as the video's thumbnail"
        );
    }

    /// Row 22: a failed poster fetch is recorded and quiet: the
    /// tile stays neutral and no notice fires.
    #[test]
    pub(super) fn poster_fetch_failure_stays_neutral() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        make_real(&mut app);
        let mut live = video_message("vid-p", BASE_TS, false, false);
        live.poster_ref = Some(backend::PosterRef {
            direct_path: "/v/t62.7118-24/poster.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![11u8; 32],
            file_enc_sha256: vec![12u8; 32],
        });
        tx.send(BackendEvent::MessageUpsert(live)).unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "vid-p");
        app.ensure_poster_fetch(mid);
        // Whatever the harness raised before is not this test's
        // concern; the failure event must add nothing.
        app.notice = None;
        tx.send(BackendEvent::VideoPosterFetchFailed {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "vid-p".to_owned(),
            message: "Download failed.".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.media.poster_fetch.get(&mid), Some(&crate::state::PhotoFetchState::Failed));
        assert_eq!(app.video_tile(mid), crate::media::VideoTile::Neutral);
        assert!(app.notice.is_none(), "a poster failure never raises a notice");
    }

    pub(super) fn sticker_message(id: &str, view_once: bool) -> LiveMessage {
        let mut live = LiveMessage::text(
            id,
            CHAT_JID,
            Some("Sender".to_owned()),
            String::new(),
            BASE_TS,
            false,
        );
        live.kind = backend::LiveMessageKind::Media {
            media: backend::MediaKind::Sticker,
            caption: String::new(),
            seconds: 0,
            size_bytes: 12_345,
        };
        live.view_once = view_once;
        live.sticker_ref = Some(backend::StickerRef {
            direct_path: "/v/t62.7118-24/sticker.enc".to_owned(),
            media_key: vec![7u8; 32],
            file_sha256: vec![8u8; 32],
            file_enc_sha256: vec![9u8; 32],
            file_length: 12_345,
        });
        live
    }

    /// An audio file (no ptt flag) converts to the audio card
    /// kind with its format label, duration and size, never to
    /// the voice-note kind.
    #[test]
    pub(super) fn audio_file_converts_to_an_audio_card() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        make_real(&mut app);
        let mut live = LiveMessage::text(
            "aud-1",
            CHAT_JID,
            Some("Sender".to_owned()),
            String::new(),
            BASE_TS,
            false,
        );
        live.kind = backend::LiveMessageKind::Media {
            media: backend::MediaKind::Audio,
            caption: "MP3 audio file".to_owned(),
            seconds: 83,
            size_bytes: 1_234_567,
        };
        tx.send(BackendEvent::MessageUpsert(live)).unwrap();
        drain_all(&mut app, &ctx);
        let want = message_id(CHAT_JID, "aud-1");
        let (_, msg) = stored_messages(&app).iter().find(|(_, m)| m.id == want).unwrap();
        match &msg.kind {
            core::model::MessageKind::Audio { secs, name, size_bytes } => {
                assert_eq!(*secs, 83);
                assert_eq!(name, "MP3 audio file");
                assert_eq!(*size_bytes, 1_234_567);
            }
            other => panic!("an audio file must convert to the audio card, got {other:?}"),
        }
    }

    /// A document converts with its own name and page count
    /// from the message's document fields, its real size in
    /// bytes, and its caption as the body.
    #[test]
    pub(super) fn document_message_carries_its_own_metadata() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        make_real(&mut app);
        let mut live = LiveMessage::text(
            "doc-1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "the quarterly report".to_owned(),
            BASE_TS,
            false,
        );
        live.kind = backend::LiveMessageKind::Media {
            media: backend::MediaKind::Document,
            caption: "the quarterly report".to_owned(),
            seconds: 0,
            size_bytes: 54_321,
        };
        live.document_name = Some("report.pdf".to_owned());
        live.document_page_count = Some(12);
        tx.send(BackendEvent::MessageUpsert(live)).unwrap();
        drain_all(&mut app, &ctx);
        let want = message_id(CHAT_JID, "doc-1");
        let (_, msg) = stored_messages(&app).iter().find(|(_, m)| m.id == want).unwrap();
        match &msg.kind {
            core::model::MessageKind::Document { name, size_bytes, page_count } => {
                assert_eq!(name, "report.pdf");
                assert_eq!(*size_bytes, 54_321);
                assert_eq!(*page_count, Some(12));
            }
            other => panic!("expected the document kind, got {other:?}"),
        }
        assert_eq!(msg.body, "the quarterly report");
    }

    /// A failed sticker fetch is retried on demand, like a
    /// failed photo's: the retry sends the fetch command again
    /// with the archived reference.
    #[test]
    pub(super) fn failed_sticker_retries_on_demand() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        make_real(&mut app);
        tx.send(BackendEvent::MessageUpsert(sticker_message("stk-1", false))).unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "stk-1");
        app.ensure_sticker_fetch(mid);
        // Other commands may already be queued; scan for the
        // fetch instead of assuming it is next.
        let mut sent_fetch = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(cmd, BackendCommand::FetchSticker { ref message_id, .. } if message_id.as_str() == "stk-1")
            {
                sent_fetch = true;
                break;
            }
        }
        assert!(sent_fetch, "the first fetch command is sent");
        tx.send(BackendEvent::StickerFetchFailed {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "stk-1".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.sticker_tile(mid), crate::media::PhotoTile::Failed);
        app.retry_sticker_fetch(mid);
        assert_eq!(app.sticker_tile(mid), crate::media::PhotoTile::Loading);
        // A presence subscription for the open chat may be
        // queued ahead of the retry; scan for it.
        let mut retry_ref = None;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if let BackendCommand::FetchSticker { sticker_ref, .. } = cmd {
                retry_ref = Some(sticker_ref);
                break;
            }
        }
        let reference = retry_ref
            .flatten()
            .expect("the retry sends a FetchSticker with the archived reference");
        assert_eq!(reference.direct_path, "/v/t62.7118-24/sticker.enc");
    }

    /// A view-once sticker's view is spent when its bytes land
    /// as a paintable picture: the opened state is recorded and
    /// the archived reference is dropped.
    #[test]
    pub(super) fn view_once_sticker_spends_when_its_bytes_land() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        make_real(&mut app);
        tx.send(BackendEvent::MessageUpsert(sticker_message("stk-vo", true))).unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "stk-vo");
        tx.send(BackendEvent::StickerBytes {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "stk-vo".to_owned(),
            bytes: jpeg_bytes(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.media.opened_view_once.contains(&mid), "the spend is recorded");
        let session = app.live.as_ref().unwrap();
        assert!(!session.sticker_refs.contains_key(&mid), "the session reference is dropped");
        let stored = session.store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let row = &stored.iter().find(|(m, _)| m.id == "stk-vo").unwrap().0;
        assert_eq!(row.sticker_ref, None, "the archived reference is dropped");
    }

    /// A verified name arriving (a usync answer, or a history
    /// payload that stated one) badges the identity's chat:
    /// the flag the list and the header paint from.
    #[test]
    pub(super) fn verified_name_badges_the_chat() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        make_real(&mut app);
        let chat_id = jid_id(CHAT_JID);
        app.chats.push(to_chat(&backend::LiveChat {
            jid: CHAT_JID.to_owned(),
            kind: backend::LiveChatKind::Dm,
            title: "Sender".to_owned(),
            preview: String::new(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread: 0,
            pinned: false,
            archived: false,
            locked: false,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
            ephemeral_secs: 0,
        }));
        {
            let session = app.live.as_mut().unwrap();
            session.jids.insert(chat_id, CHAT_JID.to_owned());
        }
        // The ask goes out once per identity.
        app.ensure_verified_name(CHAT_JID);
        app.ensure_verified_name(CHAT_JID);
        let asks = cmd_rx
            .try_iter()
            .filter(|c| matches!(c, BackendCommand::FetchVerifiedName { .. }))
            .count();
        assert_eq!(asks, 1, "one verified-name ask per identity per session");
        tx.send(BackendEvent::VerifiedNameResult {
            jid: CHAT_JID.to_owned(),
            name: Some("Toko Berkah".to_owned()),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.apply_verified_badges();
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(chat.verified, "the chat wears the badge once the name is proven");
        // A history payload stating the name seeds the book too.
        let mut live = LiveMessage::text(
            "biz-1",
            CHAT_JID,
            Some("Toko Berkah".to_owned()),
            "promo today",
            BASE_TS,
            false,
        );
        live.sender_jid = Some(CHAT_JID.to_owned());
        live.sender_verified_name = Some("Toko Berkah".to_owned());
        tx.send(BackendEvent::MessageUpsert(live)).unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert_eq!(session.verified_names.get(CHAT_JID).map(String::as_str), Some("Toko Berkah"));
    }

    /// Item 111: view-once video bytes land in memory (never a
    /// file), the player starts from them and presents a real
    /// decoded frame of the actual video.
    #[test]
    pub(super) fn view_once_video_bytes_start_a_real_player() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        make_real(&mut app);
        tx.send(BackendEvent::MessageUpsert(video_message("vid-vo", BASE_TS, true, true))).unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "vid-vo");

        app.ensure_video_fetch(mid, true);
        match cmd_rx.try_recv() {
            Ok(BackendCommand::FetchVideo { view_once, .. }) => {
                assert!(view_once, "a view-once video fetches into memory only");
            }
            other => panic!("expected a FetchVideo command, got {other:?}"),
        }
        tx.send(BackendEvent::VideoBytes {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "vid-vo".to_owned(),
            bytes: include_bytes!("../testdata/tiny.mp4").to_vec(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.media.video_bytes.contains_key(&mid), "the bytes are held in memory");
        assert!(!app.media.video_files.contains_key(&mid), "never as a file");
        assert_eq!(app.video_play_route(mid), crate::media::VideoPlayRoute::Ready);

        app.start_video_player(mid, 1);
        let player = app.media.video_player.as_ref().expect("the player started");
        assert_eq!(player.key, mid);
        // Pump the app-side handle until the worker presents the
        // first real frame.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let presented = {
                let player = app.media.video_player.as_mut().unwrap();
                player.pump();
                player.current.is_some()
            };
            if presented {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the first frame never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let player = app.media.video_player.as_ref().unwrap();
        let frame = player.current.as_ref().unwrap();
        assert_eq!((frame.width, frame.height), (128, 96));
        assert_eq!(frame.pts, 0.0);
    }

    /// Item 111, the view-once gate for video: closing the
    /// viewer on a view-once video whose real content was never
    /// presented does NOT spend the view. The reference and the
    /// thumbnail survive, in the session and in the archive.
    #[test]
    pub(super) fn view_once_video_close_without_presentation_keeps_the_view() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        make_real(&mut app);
        tx.send(BackendEvent::MessageUpsert(video_message("vid-vo", BASE_TS, true, true))).unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = jid_id(CHAT_JID);
        app.sync_conversation();
        let mid = message_id(CHAT_JID, "vid-vo");
        app.media.viewing_photo = Some(mid);

        app.close_photo_viewer(mid, true);
        assert!(
            !app.media.opened_view_once.contains(&mid),
            "an unpresented video has not been viewed"
        );
        assert!(app.notice.is_some(), "the kept view is said out loud");
        assert_eq!(app.notice_str(), Some(crate::i18n::t("video.view_once_not_loaded").as_str()));
        let session = app.live.as_ref().unwrap();
        assert!(session.video_refs.contains_key(&mid), "the reference survives for a retry");
        assert!(session.video_thumbs.contains_key(&mid), "the thumbnail survives too");
        let stored = session.store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let row = &stored.iter().find(|(m, _)| m.id == "vid-vo").unwrap().0;
        assert!(row.video_ref.is_some(), "the archived reference survives");
        assert!(row.video_thumbnail.is_some(), "the archived thumbnail survives");
    }

    /// Item 111, the other half: once the video's real content
    /// was presented in the viewer, closing spends the view
    /// completely: the opened state persists, the in-memory
    /// bytes are dropped, and the reference and thumbnail are
    /// removed from the session and the archive.
    #[test]
    pub(super) fn view_once_video_close_after_presentation_spends_the_view() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        make_real(&mut app);
        tx.send(BackendEvent::MessageUpsert(video_message("vid-vo", BASE_TS, true, true))).unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = jid_id(CHAT_JID);
        app.sync_conversation();
        let mid = message_id(CHAT_JID, "vid-vo");
        app.media.video_bytes.insert(mid, vec![1, 2, 3]);
        // The viewer presented the video's real thumbnail during
        // this viewing.
        app.media.video_presented = Some(mid);
        app.media.viewing_photo = Some(mid);

        app.close_photo_viewer(mid, true);
        assert!(app.media.opened_view_once.contains(&mid), "the view is spent");
        assert!(!app.media.video_bytes.contains_key(&mid), "the in-memory bytes are dropped");
        assert!(app.media.video_presented.is_none(), "the presentation record clears");
        let session = app.live.as_ref().unwrap();
        assert!(!session.video_refs.contains_key(&mid), "the session reference is dropped");
        assert!(!session.video_thumbs.contains_key(&mid), "the session thumbnail is dropped");
        let stored = session.store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let (row, flags) = stored.iter().find(|(m, _)| m.id == "vid-vo").unwrap();
        assert_ne!(flags & store::FLAG_VIEW_ONCE_OPENED, 0, "the spend persists");
        assert!(row.video_ref.is_none(), "the archived reference is dropped");
        assert!(row.video_thumbnail.is_none(), "the archived thumbnail is dropped");
    }

    /// Item 111: without a live session there is nothing to
    /// play: the route is the honest test-mode answer and no
    /// fetch is ever sent.
    #[test]
    pub(super) fn video_route_without_a_session_is_demo_unavailable() {
        let (mut app, _tx, _ctx, cmd_rx) = live_app();
        app.live = None;
        app.live_mode = false;
        assert_eq!(app.video_play_route(42), crate::media::VideoPlayRoute::DemoUnavailable);
        app.ensure_video_fetch(42, false);
        assert!(cmd_rx.try_recv().is_err(), "no fetch leaves a sessionless app");
        assert!(!app.media.video_fetch.contains_key(&42));
    }

    /// Privacy finding C6: backend error text is largely library
    /// Display output and can embed server-supplied strings,
    /// phone numbers, and secret shapes. The surfaces that
    /// display it (the notice banner, the pre-link status line)
    /// must receive the scrubbed form.
    #[test]
    pub(super) fn backend_error_text_is_scrubbed_before_display() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        // Pre-link, so the status line takes the message too.
        app.live.as_mut().unwrap().status = LinkStatus::Starting;
        tx.send(BackendEvent::Error {
            message: "server refused the call from +62 812-3456-7890, token=zzz999888".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let notice = app.notice_str().expect("the error surfaces as a notice");
        assert!(!notice.contains("812-3456"), "phone leaked into the notice: {notice}");
        assert!(!notice.contains("zzz999888"), "secret leaked into the notice: {notice}");
        let status = app.live.as_ref().unwrap().status.label();
        assert!(!status.contains("812-3456"), "phone leaked into the status line: {status}");
        assert!(!status.contains("zzz999888"), "secret leaked into the status line: {status}");
    }

    /// The other half of C6: a plain error string with no secret
    /// or phone shapes (the shape of the app's own authored
    /// errors) reaches the notice banner byte-identical.
    #[test]
    pub(super) fn plain_backend_error_text_is_displayed_unchanged() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::Error {
            message: "The group settings could not be changed.".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.notice_str(), Some("The group settings could not be changed."));
    }

    /// The removal-refresh path (delete-for-me, clear, expiry)
    /// recomputes the preview from the mirror: falling back to
    /// a view-once message must yield its placeholder, not the
    /// caption its mirror body still carries for the viewer,
    /// and the refreshed preview is written through to the
    /// archive row.
    #[test]
    pub(super) fn preview_falls_back_to_the_view_once_placeholder() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        tx.send(BackendEvent::MessageUpsert(view_once_photo("vo-1", "a secret caption", BASE_TS)))
            .unwrap();
        tx.send(incoming("m2", BASE_TS + 10)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.chats.iter().find(|c| c.id == chat_id).unwrap().preview,
            "message m2",
            "the newer text message previews as itself"
        );
        tx.send(BackendEvent::MessageDeletedForMe {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "m2".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert_eq!(chat.preview, core::model::VIEW_ONCE_PREVIEW, "the fallback is the placeholder");
        let session = app.live.as_ref().unwrap();
        let stored = session.store.as_ref().unwrap().chats().unwrap();
        assert_eq!(
            stored.iter().find(|c| c.jid == CHAT_JID).unwrap().preview,
            core::model::VIEW_ONCE_PREVIEW,
            "and the archive row follows"
        );
    }

    /// The audit's freeze scenario, shrunk: a 600-message history
    /// burst must spread over several frames (never all in the
    /// first), lose nothing, keep arrival order, and leave the
    /// archive and the rendered conversation complete.
    #[test]
    pub(super) fn burst_drains_over_multiple_frames_in_order() {
        const TOTAL: i64 = 600;
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.selected_chat = jid_id(CHAT_JID);
        for i in 0..TOTAL {
            tx.send(incoming(&format!("m{i}"), BASE_TS + i)).unwrap();
        }

        app.poll_backend(&ctx);
        let after_first = stored_messages(&app).len();
        assert!(
            after_first <= MAX_EVENTS_PER_FRAME,
            "one frame must stay within its budget, applied {after_first}"
        );
        assert!(after_first < TOTAL as usize, "the burst must not land in a single frame");
        assert!(!app.event_queue.is_empty(), "the remainder waits in the queue");

        drain_all(&mut app, &ctx);
        let stored = stored_messages(&app);
        assert_eq!(stored.len(), TOTAL as usize, "no event was dropped");
        assert!(stored.windows(2).all(|w| w[0].0 < w[1].0), "messages end up sorted by timestamp");
        assert_eq!(stored.first().unwrap().1.body, "message m0");
        assert_eq!(stored.last().unwrap().1.body, format!("message m{}", TOTAL - 1));
        let chat = app.chats.iter().find(|c| c.id == jid_id(CHAT_JID)).unwrap();
        assert_eq!(chat.unread, u32::try_from(TOTAL).unwrap(), "every upsert counted once");
        assert_eq!(app.conversation.len(), TOTAL as usize, "the open conversation is rebuilt");
        let archived =
            app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        assert_eq!(archived.len(), TOTAL as usize, "the batch flush archived every message");
    }

    /// A receipt and an edit that arrive in the same burst as
    /// their message must still find it: batching defers sorts
    /// and writes, never the application order.
    #[test]
    pub(super) fn receipt_and_edit_in_the_same_burst_still_apply() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.selected_chat = jid_id(CHAT_JID);
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "mine-1",
            CHAT_JID,
            None,
            "sent from here",
            BASE_TS,
            true,
        )))
        .unwrap();
        tx.send(BackendEvent::Receipt {
            chat_jid: CHAT_JID.to_owned(),
            message_ids: vec!["mine-1".to_owned()],
            kind: ReceiptKind::Read,
            at: BASE_TS + 60,
            actor: None,
        })
        .unwrap();
        tx.send(incoming("theirs-1", BASE_TS + 120)).unwrap();
        tx.send(BackendEvent::MessageEdited {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "theirs-1".to_owned(),
            new_body: "edited in the same burst".to_owned(),
        })
        .unwrap();

        drain_all(&mut app, &ctx);
        let stored = stored_messages(&app);
        assert_eq!(stored.len(), 2);
        let mine = stored.iter().find(|(_, m)| m.body == "sent from here").unwrap();
        assert_eq!(mine.1.delivery, Some(Delivery::Read), "the receipt advanced the message");
        let theirs = stored.iter().find(|(_, m)| m.id == message_id(CHAT_JID, "theirs-1")).unwrap();
        assert_eq!(theirs.1.body, "edited in the same burst");
        assert!(theirs.1.edited);
        let archived =
            app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        assert_eq!(archived.len(), 2, "both burst messages reached the archive");
    }

    /// QC1614-A0-1: a Read receipt survives a re-upsert of the
    /// same message (a reaction, an edit, a history re-delivery)
    /// in the mirror and in the archive.
    #[test]
    pub(super) fn receipt_then_reupsert_keeps_read_ticks() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.selected_chat = jid_id(CHAT_JID);
        let upsert = || {
            BackendEvent::MessageUpsert(LiveMessage::text(
                "mine-1",
                CHAT_JID,
                None,
                "sent from here",
                BASE_TS,
                true,
            ))
        };
        tx.send(upsert()).unwrap();
        tx.send(BackendEvent::Receipt {
            chat_jid: CHAT_JID.to_owned(),
            message_ids: vec!["mine-1".to_owned()],
            kind: ReceiptKind::Read,
            at: BASE_TS + 60,
            actor: None,
        })
        .unwrap();
        tx.send(upsert()).unwrap();
        drain_all(&mut app, &ctx);
        let stored = stored_messages(&app);
        let mine = stored.iter().find(|(_, m)| m.body == "sent from here").unwrap();
        assert_eq!(mine.1.delivery, Some(Delivery::Read), "re-upsert must not downgrade Read");
        let archived =
            app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let archived_mine = archived.iter().find(|(m, _)| m.id == "mine-1").unwrap();
        assert_eq!(
            archived_mine.0.delivery,
            Some(backend::DeliveryState::Read),
            "the receipt persisted to the archive"
        );
    }

    /// QC1614-A0-1: one group member's Read receipt advances a
    /// group message to Delivered at most, never to Read.
    #[test]
    pub(super) fn group_receipt_from_one_member_caps_at_delivered() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        const GROUP: &str = "120363000000000001@g.us";
        tx.send(BackendEvent::ChatUpsert(LiveChat {
            jid: GROUP.to_owned(),
            title: "Group".to_owned(),
            kind: LiveChatKind::Group,
            preview: String::new(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
        }))
        .unwrap();
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "g-1",
            GROUP,
            None,
            "group hello",
            BASE_TS,
            true,
        )))
        .unwrap();
        tx.send(BackendEvent::Receipt {
            chat_jid: GROUP.to_owned(),
            message_ids: vec!["g-1".to_owned()],
            kind: ReceiptKind::Read,
            at: BASE_TS + 60,
            actor: Some("6281111111111@s.whatsapp.net".to_owned()),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let entry = app.live.as_ref().unwrap().messages.get(&jid_id(GROUP)).unwrap();
        let mine = entry.iter().find(|(_, m)| m.body == "group hello").unwrap();
        assert_eq!(mine.1.delivery, Some(Delivery::Delivered));
    }

    /// QC1614-A0-5 W1: an interrupted drain is not a completion:
    /// the sync row stays up and synced_once stays unlatched
    /// until a real completion arrives.
    #[test]
    pub(super) fn interrupted_sync_never_latches_synced_once() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::SyncProgress { percent: None }).unwrap();
        tx.send(BackendEvent::SyncInterrupted).unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert!(session.syncing, "the row stays up after an interruption");
        assert!(!session.synced_once, "an interruption never latches synced_once");
        tx.send(BackendEvent::SyncCompleted).unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert!(!session.syncing);
        assert!(session.synced_once);
    }

    /// QC1614-A0-3: a fake roster with a parent group and a
    /// linked child yields one community containing the child.
    #[test]
    pub(super) fn community_structure_derives_from_parent_links() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        for chat in backend::fake::scripted_chats() {
            tx.send(BackendEvent::ChatUpsert(chat)).unwrap();
        }
        drain_all(&mut app, &ctx);
        let communities = app.communities();
        assert_eq!(communities.len(), 1, "one community from the parent link");
        let community = &communities[0];
        assert_eq!(community.name, "Weekend Community (live demo)");
        assert_eq!(
            community.announcement_chat_id,
            jid_id(backend::fake::FAKE_COMMUNITY_JID),
            "the parent is the announcement chat"
        );
        assert_eq!(
            community.group_chat_ids,
            vec![jid_id(backend::fake::FAKE_GROUP_JID)],
            "the linked group is its member"
        );
    }

    /// P6: a ChatCommunity report with no parent stands a
    /// linked group alone again (the unlink answer's shape),
    /// and a CommunityDeactivated answer removes the parent
    /// chat while its subgroups keep their chats, unlinked.
    #[test]
    pub(super) fn community_unlink_and_deactivation_apply() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        for chat in backend::fake::scripted_chats() {
            tx.send(BackendEvent::ChatUpsert(chat)).unwrap();
        }
        drain_all(&mut app, &ctx);
        assert_eq!(app.communities().len(), 1, "the scripted community derives");
        let group_id = jid_id(backend::fake::FAKE_GROUP_JID);
        tx.send(BackendEvent::ChatCommunity {
            chat_jid: backend::fake::FAKE_GROUP_JID.to_owned(),
            parent_jid: None,
            is_parent: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            app.chats.iter().find(|c| c.id == group_id).is_some_and(|c| c.community.is_none()),
            "the unlinked group's tag clears"
        );
        assert_eq!(
            app.communities()[0].group_chat_ids,
            Vec::<u64>::new(),
            "the community remains, with no linked groups"
        );
        tx.send(BackendEvent::ChatCommunity {
            chat_jid: backend::fake::FAKE_GROUP_JID.to_owned(),
            parent_jid: Some(backend::fake::FAKE_COMMUNITY_JID.to_owned()),
            is_parent: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.communities().len(), 1, "the re-linked community derives again");
        tx.send(BackendEvent::CommunityDeactivated {
            community_jid: backend::fake::FAKE_COMMUNITY_JID.to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let parent_id = jid_id(backend::fake::FAKE_COMMUNITY_JID);
        assert!(!app.chats.iter().any(|c| c.id == parent_id), "the parent chat is removed");
        assert!(
            app.chats.iter().find(|c| c.id == group_id).is_some_and(|c| c.community.is_none()),
            "the subgroup survives, unlinked"
        );
        assert_eq!(app.communities().len(), 0, "no community remains");
    }

    /// The message-info popover renders exactly what the
    /// receipt log holds for a message (draw_message_info
    /// reads `receipt_log` for its rows), so the log's
    /// population from Receipt events is the feature: kinds
    /// in arrival order, each stamped with its own event
    /// time, kept per message.
    #[test]
    pub(super) fn receipt_events_populate_the_message_info_log() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.selected_chat = jid_id(CHAT_JID);
        for (id, body, ts) in [("mine-1", "first", BASE_TS), ("mine-2", "second", BASE_TS + 1)] {
            tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
                id, CHAT_JID, None, body, ts, true,
            )))
            .unwrap();
        }
        tx.send(BackendEvent::Receipt {
            chat_jid: CHAT_JID.to_owned(),
            message_ids: vec!["mine-1".to_owned()],
            kind: ReceiptKind::Sent,
            at: BASE_TS + 600,
            actor: None,
        })
        .unwrap();
        tx.send(BackendEvent::Receipt {
            chat_jid: CHAT_JID.to_owned(),
            message_ids: vec!["mine-1".to_owned()],
            kind: ReceiptKind::Delivered,
            at: BASE_TS + 1_200,
            actor: None,
        })
        .unwrap();
        // One receipt event can cover several messages at once.
        tx.send(BackendEvent::Receipt {
            chat_jid: CHAT_JID.to_owned(),
            message_ids: vec!["mine-1".to_owned(), "mine-2".to_owned()],
            kind: ReceiptKind::Read,
            at: BASE_TS + 1_800,
            actor: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);

        let mid1 = message_id(CHAT_JID, "mine-1");
        assert_eq!(
            app.receipt_log.get(&mid1),
            Some(&vec![
                (ReceiptKind::Sent, fmt_time(BASE_TS + 600)),
                (ReceiptKind::Delivered, fmt_time(BASE_TS + 1_200)),
                (ReceiptKind::Read, fmt_time(BASE_TS + 1_800)),
            ]),
            "every receipt lands in the log, in order, with its own time"
        );
        let mid2 = message_id(CHAT_JID, "mine-2");
        assert_eq!(
            app.receipt_log.get(&mid2),
            Some(&vec![(ReceiptKind::Read, fmt_time(BASE_TS + 1_800))]),
            "the shared receipt logs once for the other message too"
        );
        let mine = stored_messages(&app).iter().find(|(_, m)| m.id == mid1).unwrap();
        assert_eq!(mine.1.delivery, Some(Delivery::Read), "the log and the ticks agree");
    }

    /// The log is bounded per message (logic review F21): a
    /// long receipt history keeps only the newest entries.
    #[test]
    pub(super) fn receipt_log_keeps_only_the_newest_entries() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.selected_chat = jid_id(CHAT_JID);
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "mine-1", CHAT_JID, None, "tracked", BASE_TS, true,
        )))
        .unwrap();
        for i in 0..40i64 {
            tx.send(BackendEvent::Receipt {
                chat_jid: CHAT_JID.to_owned(),
                message_ids: vec!["mine-1".to_owned()],
                kind: ReceiptKind::Delivered,
                at: BASE_TS + i * 60,
                actor: None,
            })
            .unwrap();
        }
        drain_all(&mut app, &ctx);
        let log = app.receipt_log.get(&message_id(CHAT_JID, "mine-1")).unwrap();
        assert_eq!(log.len(), 32, "the log is capped");
        assert_eq!(
            log.first(),
            Some(&(ReceiptKind::Delivered, fmt_time(BASE_TS + 8 * 60))),
            "the oldest entries drain away"
        );
        assert_eq!(log.last(), Some(&(ReceiptKind::Delivered, fmt_time(BASE_TS + 39 * 60))));
    }

    /// VRF-05: the log's size cap drops the oldest histories
    /// and keeps the recent ones. It used to clear the whole
    /// map past 4,096 entries, silently emptying the Message
    /// info history of every message at once.
    #[test]
    pub(super) fn receipt_log_cap_drops_oldest_and_keeps_recent() {
        let (mut app, _tx, _ctx, _cmd_rx) = live_app();
        for id in 0..4_100u64 {
            app.receipt_log.insert(id, vec![(ReceiptKind::Read, "09:05".to_owned())]);
            app.receipt_log_order.push_back(id);
        }
        app.prune_receipt_log();
        assert_eq!(app.receipt_log.len(), 4_096);
        for dropped in 0..4u64 {
            assert!(!app.receipt_log.contains_key(&dropped), "oldest {dropped} dropped");
        }
        assert!(app.receipt_log.contains_key(&4), "the fifth history survives");
        assert!(app.receipt_log.contains_key(&4_099), "the newest history survives");
        // A stale slot in the order queue (a forgotten
        // message) does not stall or corrupt the prune.
        app.receipt_log_order.push_front(999_999);
        app.receipt_log.insert(500_000, vec![(ReceiptKind::Read, "09:06".to_owned())]);
        app.receipt_log_order.push_back(500_000);
        app.prune_receipt_log();
        assert_eq!(app.receipt_log.len(), 4_096);
        assert!(app.receipt_log.contains_key(&500_000));
    }

    /// Incoming typing: a TypingReceived event marks the chat
    /// (the conversation header and the chat list read the
    /// mark's freshness, under six seconds), and the matching
    /// stop event clears it, per chat.
    #[test]
    pub(super) fn typing_received_marks_and_clears_per_chat() {
        const OTHER_JID: &str = "6289999999999@s.whatsapp.net";
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        let other_id = jid_id(OTHER_JID);
        tx.send(BackendEvent::TypingReceived { chat_jid: CHAT_JID.to_owned(), typing: true })
            .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            app.live_typing.get(&chat_id).is_some_and(|at| at.elapsed().as_secs() < 6),
            "a fresh mark is what the header's typing label reads"
        );
        tx.send(BackendEvent::TypingReceived { chat_jid: OTHER_JID.to_owned(), typing: true })
            .unwrap();
        tx.send(BackendEvent::TypingReceived { chat_jid: CHAT_JID.to_owned(), typing: false })
            .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!app.live_typing.contains_key(&chat_id), "the stop event clears the mark");
        assert!(app.live_typing.contains_key(&other_id), "the other chat keeps its own mark");
    }

    /// v1.6.8: the older-history state machine (design
    /// standard section 22): firing anchors on the oldest
    /// archived message and marks the chat fetching; a landed
    /// page returns it to idle, the end of history latches it
    /// exhausted, and a failure returns it to idle without
    /// latching, so a later scroll can retry.
    #[test]
    pub(super) fn history_fetch_state_moves_with_the_outcome_events() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        tx.send(incoming("m2", BASE_TS + 200)).unwrap();
        tx.send(incoming("m1", BASE_TS + 100)).unwrap();
        drain_all(&mut app, &ctx);
        let id = jid_id(CHAT_JID);
        app.select_chat_now(id);
        while cmd_rx.try_recv().is_ok() {}
        app.request_older_page(id);
        {
            let session = app.live.as_ref().unwrap();
            assert_eq!(session.history.get(&id), Some(&HistoryFetch::Fetching));
            assert!(!session.history_armed.contains(&id), "firing disarms the trigger");
        }
        let mut fetch_cmd = None;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(cmd, BackendCommand::FetchOlderMessages { .. }) {
                fetch_cmd = Some(cmd);
            }
        }
        match fetch_cmd {
            Some(BackendCommand::FetchOlderMessages {
                chat_jid,
                oldest_message_id,
                oldest_from_me,
                oldest_timestamp_ms,
            }) => {
                assert_eq!(chat_jid, CHAT_JID);
                assert_eq!(oldest_message_id, "m1", "the anchor is the oldest message on hand");
                assert!(!oldest_from_me);
                assert_eq!(oldest_timestamp_ms, (BASE_TS + 100) * 1000);
            }
            other => panic!("expected a fetch command, got {other:?}"),
        }
        // A page with more behind it: back to idle.
        tx.send(BackendEvent::HistoryPageFetched {
            chat_jid: CHAT_JID.to_owned(),
            added: 50,
            end_of_history: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            !app.live.as_ref().unwrap().history.contains_key(&id),
            "a landed page idles the chat"
        );
        // A failure: idle too, never latched, and surfaced
        // because the chat is the one on screen.
        app.request_older_page(id);
        tx.send(BackendEvent::HistoryFetchFailed {
            chat_jid: CHAT_JID.to_owned(),
            message: "no answer".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!app.live.as_ref().unwrap().history.contains_key(&id), "a failure idles the chat");
        assert_eq!(
            app.notice.as_ref().map(|n| n.kind),
            Some(crate::state::NoticeKind::Error),
            "the failure is surfaced for the open chat"
        );
        // The end of history latches for the session.
        app.request_older_page(id);
        tx.send(BackendEvent::HistoryPageFetched {
            chat_jid: CHAT_JID.to_owned(),
            added: 0,
            end_of_history: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.live.as_ref().unwrap().history.get(&id),
            Some(&HistoryFetch::Exhausted),
            "the end of history latches"
        );
    }

    /// v1.6.8: a resolved number opens the person's chat
    /// (design standard section 23): a new person enters
    /// through the upsert path and is selected; a person the
    /// roster already holds, under either address form, is
    /// selected as-is and never duplicated or renamed.
    #[test]
    pub(super) fn contact_resolved_opens_one_chat_per_person() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        drain_all(&mut app, &ctx);
        let dm_chat = |jid: &str, title: &str| LiveChat {
            jid: jid.to_owned(),
            title: title.to_owned(),
            kind: LiveChatKind::Dm,
            preview: String::new(),
            preview_sender: None,
            timestamp: 0,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
        };
        app.new_chat.open = true;
        tx.send(BackendEvent::ContactResolved {
            chat: dm_chat("10000000007@s.whatsapp.net", "Emma Wilson"),
            alt_jid: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let emma_id = jid_id("10000000007@s.whatsapp.net");
        assert_eq!(app.selected_chat, emma_id, "the resolved chat opens");
        assert!(!app.new_chat.open, "the dialog closes on resolution");
        assert_eq!(chat_of(&app, emma_id).title, "Emma Wilson");
        // The roster person, resolved under their other
        // address form: the existing chat is selected, no
        // second chat appears, its known title stands.
        app.new_chat.open = true;
        tx.send(BackendEvent::ContactResolved {
            chat: dm_chat("10000000001@s.whatsapp.net", "+10000000001"),
            alt_jid: Some(CHAT_JID.to_owned()),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.selected_chat, jid_id(CHAT_JID), "the roster chat opens");
        assert!(
            app.chats.iter().all(|c| c.id != jid_id("10000000001@s.whatsapp.net")),
            "no duplicate chat for the same person"
        );
        assert_eq!(chat_of(&app, jid_id(CHAT_JID)).title, "Sender", "the known title stands");
    }

    /// v1.6.8: lookup outcomes move only the dialog state that
    /// asked (section 23): not-found is recorded as its own
    /// outcome, and a stale failure for a replaced input is
    /// ignored.
    #[test]
    pub(super) fn lookup_outcomes_move_only_the_dialog_that_asked() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.new_chat.open = true;
        app.new_chat.phone = "999999999".to_owned();
        app.start_new_chat_lookup();
        assert_eq!(app.new_chat.checking.as_deref(), Some("999999999"));
        tx.send(BackendEvent::ContactNotFound { phone: "999999999".to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.new_chat.not_found.as_deref(), Some("999999999"));
        assert!(app.new_chat.checking.is_none());
        assert!(app.new_chat.error.is_none(), "not found is not an error");
        app.new_chat.phone = "10000000007".to_owned();
        app.start_new_chat_lookup();
        tx.send(BackendEvent::ContactLookupFailed {
            phone: "999999999".to_owned(),
            message: "stale".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.new_chat.error.is_none(), "a stale outcome changes nothing");
        assert_eq!(app.new_chat.checking.as_deref(), Some("10000000007"));
    }

    /// v1.6.8: our own group picture write reconciles through
    /// the avatar pipeline (section 23): the change event
    /// drops every cached form of the picture, and a failure
    /// surfaces as an error notice in the backend's words.
    #[test]
    pub(super) fn group_picture_events_invalidate_and_report() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::AvatarBytes {
            jid: CHAT_JID.to_owned(),
            picture_id: Some("pic-1".to_owned()),
            bytes: png_bytes(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.live.as_ref().unwrap().avatar_bytes.contains_key(CHAT_JID));
        tx.send(BackendEvent::GroupPictureChanged { chat_jid: CHAT_JID.to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        {
            let session = app.live.as_ref().unwrap();
            assert!(!session.avatar_bytes.contains_key(CHAT_JID), "the cached picture drops");
            assert!(
                session.store.as_ref().unwrap().avatar(CHAT_JID).unwrap().is_none(),
                "and its archive row"
            );
        }
        assert_eq!(
            app.notice.as_ref().map(|n| n.kind),
            Some(crate::state::NoticeKind::Info),
            "the landed write confirms"
        );
        tx.send(BackendEvent::GroupPictureFailed {
            chat_jid: CHAT_JID.to_owned(),
            message: "refused".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let notice = app.notice.as_ref().expect("the failure surfaces");
        assert_eq!(notice.kind, crate::state::NoticeKind::Error);
        assert_eq!(notice.text, "refused");
    }

    pub(super) fn dm_upsert(jid: &str) -> BackendEvent {
        BackendEvent::ChatUpsert(LiveChat {
            jid: jid.to_owned(),
            title: "Sender".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "hello".to_owned(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
        })
    }

    pub(super) fn chat_of(app: &App, id: u64) -> Chat {
        app.chats.iter().find(|c| c.id == id).expect("the chat exists").clone()
    }

    /// Item 111: the live conversion must not invent a header
    /// line. Every live kind converts with an empty subtitle;
    /// the header derives its line from live state instead.
    #[test]
    pub(super) fn live_conversion_carries_no_subtitle() {
        for kind in [LiveChatKind::Dm, LiveChatKind::Group, LiveChatKind::Channel] {
            let chat = to_chat(&LiveChat {
                jid: CHAT_JID.to_owned(),
                title: "Sender".to_owned(),
                kind,
                preview: String::new(),
                preview_sender: None,
                timestamp: BASE_TS,
                unread: 0,
                archived: false,
                locked: false,
                pinned: false,
                ephemeral_secs: 0,
                muted: false,
                mute_until: 0,
                parent_jid: None,
                is_parent: false,
            });
            assert!(chat.subtitle.is_empty(), "{kind:?} carries no invented subtitle");
        }
    }

    /// Item 111: opening a DM subscribes to the contact's
    /// presence, and the backend's reports drive the header
    /// line: online, then a last seen carrying the real
    /// instant, then nothing when last seen is hidden.
    #[test]
    pub(super) fn presence_watch_subscribes_and_reports_drive_the_header_line() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        drain_all(&mut app, &ctx);
        let chat_id = jid_id(CHAT_JID);
        app.selected_chat = chat_id;
        app.poll_backend(&ctx);
        assert_eq!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::SetPresenceSubscription {
                chat_jid: CHAT_JID.to_owned(),
                subscribed: true,
            }),
            "opening the DM subscribes to the contact's presence"
        );
        let chat = chat_of(&app, chat_id);
        assert_eq!(app.header_subtitle(&chat), "", "nothing shows before the first report");

        tx.send(BackendEvent::PresenceReceived {
            chat_jid: CHAT_JID.to_owned(),
            online: true,
            last_seen: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.header_subtitle(&chat), "online");

        // Offline, last seen at 01:00 today (UTC, the grid the
        // app's clock labels use): the line names that instant.
        let now = i64::try_from(crate::state::now_unix()).unwrap();
        let seen = now.div_euclid(86_400) * 86_400 + 3_600;
        tx.send(BackendEvent::PresenceReceived {
            chat_jid: CHAT_JID.to_owned(),
            online: false,
            last_seen: Some(seen),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.header_subtitle(&chat),
            format!("last seen today at {}", fmt_time(seen)),
            "the last-seen line carries the reported instant"
        );

        // Offline with the last seen hidden by the contact's
        // privacy settings: there is nothing truthful to show.
        tx.send(BackendEvent::PresenceReceived {
            chat_jid: CHAT_JID.to_owned(),
            online: false,
            last_seen: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.header_subtitle(&chat), "");
    }

    /// Item 111: the watch follows the open chat. Leaving a
    /// chat unsubscribes it and drops its reported presence, so
    /// an "online" nobody is receiving updates for can never
    /// linger.
    #[test]
    pub(super) fn presence_watch_follows_the_open_chat_and_drops_stale_state() {
        const OTHER_JID: &str = "6289999999999@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        tx.send(dm_upsert(OTHER_JID)).unwrap();
        drain_all(&mut app, &ctx);
        let first = jid_id(CHAT_JID);
        let second = jid_id(OTHER_JID);
        app.selected_chat = first;
        app.poll_backend(&ctx);
        assert_eq!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::SetPresenceSubscription {
                chat_jid: CHAT_JID.to_owned(),
                subscribed: true,
            })
        );
        tx.send(BackendEvent::PresenceReceived {
            chat_jid: CHAT_JID.to_owned(),
            online: true,
            last_seen: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let first_chat = chat_of(&app, first);
        assert_eq!(app.header_subtitle(&first_chat), "online");

        app.selected_chat = second;
        app.poll_backend(&ctx);
        assert_eq!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::SetPresenceSubscription {
                chat_jid: CHAT_JID.to_owned(),
                subscribed: false,
            }),
            "leaving the chat unsubscribes it"
        );
        assert_eq!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::SetPresenceSubscription {
                chat_jid: OTHER_JID.to_owned(),
                subscribed: true,
            }),
            "the newly open chat is subscribed instead"
        );
        assert_eq!(app.header_subtitle(&first_chat), "", "the left chat's presence is dropped");
        assert!(!app.live_presence.contains_key(&first));

        // A late report for the chat nobody watches anymore is
        // not stored either.
        tx.send(BackendEvent::PresenceReceived {
            chat_jid: CHAT_JID.to_owned(),
            online: true,
            last_seen: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!app.live_presence.contains_key(&first), "unwatched reports are not stored");
    }

    /// Item 111: losing the connection drops reported presence.
    /// An "online" the feed can no longer back must not survive
    /// the outage on screen.
    #[test]
    pub(super) fn connection_loss_drops_reported_presence() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        drain_all(&mut app, &ctx);
        let chat_id = jid_id(CHAT_JID);
        app.selected_chat = chat_id;
        app.poll_backend(&ctx);
        tx.send(BackendEvent::PresenceReceived {
            chat_jid: CHAT_JID.to_owned(),
            online: true,
            last_seen: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let chat = chat_of(&app, chat_id);
        assert_eq!(app.header_subtitle(&chat), "online");
        tx.send(BackendEvent::ConnectionLost { reason: "test outage".to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.header_subtitle(&chat), "", "presence does not outlive its connection");
        assert!(app.live_presence.is_empty());
    }

    /// Item 111: a live group's header line is the real member
    /// count from the fetched member list. Opening the group
    /// asks for the list once; before it arrives the line is
    /// empty.
    #[test]
    pub(super) fn group_header_line_counts_the_fetched_members() {
        const GROUP_JID: &str = "120207777777777777@g.us";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::ChatUpsert(LiveChat {
            jid: GROUP_JID.to_owned(),
            title: "Group".to_owned(),
            kind: LiveChatKind::Group,
            preview: String::new(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
        }))
        .unwrap();
        drain_all(&mut app, &ctx);
        let group_id = jid_id(GROUP_JID);
        app.selected_chat = group_id;
        app.poll_backend(&ctx);
        assert_eq!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::FetchGroupMembers { chat_jid: GROUP_JID.to_owned() }),
            "opening the group asks for its member list"
        );
        let chat = chat_of(&app, group_id);
        assert_eq!(app.header_subtitle(&chat), "", "no count before the list arrives");
        tx.send(BackendEvent::GroupMembers {
            chat_jid: GROUP_JID.to_owned(),
            members: vec![
                ("6281111111111@s.whatsapp.net".to_owned(), "Alice Johnson".to_owned(), true),
                ("6282222222222@s.whatsapp.net".to_owned(), "Bob Carter".to_owned(), false),
                ("6283333333333@s.whatsapp.net".to_owned(), "Carol Danvers".to_owned(), false),
            ],
            own_admin: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.header_subtitle(&chat), "3 members");
    }

    /// Item 111: the last-seen label names today and yesterday
    /// with their word and older reports with the full date,
    /// always with the report's own clock time.
    #[test]
    pub(super) fn last_seen_label_names_today_yesterday_and_older_days() {
        let now = i64::try_from(crate::state::now_unix()).unwrap();
        let day_start = now.div_euclid(86_400) * 86_400;
        let today = day_start + 3_600;
        assert_eq!(last_seen_label(today), format!("last seen today at {}", fmt_time(today)));
        let yesterday = day_start - 3_600;
        assert_eq!(
            last_seen_label(yesterday),
            format!("last seen yesterday at {}", fmt_time(yesterday))
        );
        let older = day_start - 3 * 86_400 + 3_600;
        assert_eq!(
            last_seen_label(older),
            format!("last seen {} at {}", fmt_day(older), fmt_time(older))
        );
    }

    /// The notification flood scenario: on a real (non-fake)
    /// session, a message stamped before the connection came up
    /// is offline catch-up and must not queue an OS notification,
    /// while a message stamped after it is live and must. Both
    /// still land in the chat and raise the unread badge: only
    /// the popup is gated.
    #[test]
    pub(super) fn history_catch_up_queues_no_notification_but_live_does() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        // The harness session runs the fake backend, whose
        // scripted clock exempts it from the history gate; this
        // test exercises the real-backend path.
        app.live.as_mut().unwrap().fake = false;
        app.selected_chat = jid_id(CHAT_JID);
        tx.send(BackendEvent::Connected { account: "Test account".to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            app.live.as_ref().unwrap().connected_at.is_some(),
            "the connect stamps the session clock"
        );

        let now = i64::try_from(crate::state::now_unix()).unwrap();
        tx.send(incoming("weekend-1", now - 7_200)).unwrap();
        tx.send(incoming("fresh-1", now + 120)).unwrap();
        drain_all(&mut app, &ctx);

        assert_eq!(stored_messages(&app).len(), 2, "history still lands in the chat");
        let chat = app.chats.iter().find(|c| c.id == jid_id(CHAT_JID)).unwrap();
        assert_eq!(chat.unread, 2, "the badge counts history too");
        assert_eq!(app.pending_notifications.len(), 1, "only the live message queues a popup");
        assert_eq!(app.pending_notifications[0].body, "message fresh-1");
    }

    /// Privacy finding D5: member and join-request rows lead
    /// with a name, never the raw phone number or the full JID.
    /// The backend leaves the name empty when the server
    /// rendered none; resolution falls back to names the session
    /// already knows (a 1:1 chat's title, a message sender's
    /// push name) and finally to a masked identifier.
    const GROUP_JID: &str = "120207777777777777@g.us";
    const ALICE_JID: &str = "6281111111111@s.whatsapp.net";
    const BOB_JID: &str = "6282222222222@s.whatsapp.net";
    const CAROL_JID: &str = "6283333333333@s.whatsapp.net";
    const STRANGER_JID: &str = "6284444444444@s.whatsapp.net";
    const LID_JID: &str = "555555555555555@lid";

    /// Alice has a 1:1 chat (its title is her push name) and Bob
    /// has spoken in the group (his push name rides his message).
    pub(super) fn seed_named_people(
        app: &mut App,
        tx: &std::sync::mpsc::Sender<BackendEvent>,
        ctx: &egui::Context,
    ) {
        tx.send(BackendEvent::ChatUpsert(LiveChat {
            jid: ALICE_JID.to_owned(),
            title: "Alice Johnson".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "hi".to_owned(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
        }))
        .unwrap();
        let mut bob_msg = LiveMessage::text(
            "bob-1",
            GROUP_JID,
            Some("Bob Carter".to_owned()),
            "hello group".to_owned(),
            BASE_TS,
            false,
        );
        bob_msg.sender_jid = Some(BOB_JID.to_owned());
        tx.send(BackendEvent::MessageUpsert(bob_msg)).unwrap();
        drain_all(app, ctx);
    }

    #[test]
    pub(super) fn member_rows_lead_with_names_and_mask_the_unknown() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_named_people(&mut app, &tx, &ctx);
        tx.send(BackendEvent::GroupMembers {
            chat_jid: GROUP_JID.to_owned(),
            members: vec![
                (ALICE_JID.to_owned(), String::new(), false),
                (BOB_JID.to_owned(), String::new(), true),
                (CAROL_JID.to_owned(), "Carol Danvers".to_owned(), false),
                (STRANGER_JID.to_owned(), String::new(), false),
                (LID_JID.to_owned(), String::new(), false),
            ],
            own_admin: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let rows =
            app.groups.live_members.get(&jid_id(GROUP_JID)).expect("the member rows are stored");
        assert_eq!(rows.len(), 5);
        // Resolved from the 1:1 chat title; the number is only
        // the secondary line under a real name.
        assert_eq!(rows[0].label, "Alice Johnson");
        assert_eq!(rows[0].detail.as_deref(), Some("+6281111111111"));
        assert!(!rows[0].admin);
        // Resolved from the push name on his group message; the
        // admin flag survives resolution.
        assert_eq!(rows[1].label, "Bob Carter");
        assert_eq!(rows[1].detail.as_deref(), Some("+6282222222222"));
        assert!(rows[1].admin);
        // The name the server rendered is carried through.
        assert_eq!(rows[2].label, "Carol Danvers");
        assert_eq!(rows[2].detail.as_deref(), Some("+6283333333333"));
        // Unknown: a masked identifier, and no number line
        // beneath it (that would undo the masking).
        assert_eq!(rows[3].label, "+628 ••• 4444");
        assert_eq!(rows[3].detail, None);
        // LID-addressed and unknown: masked without a plus, and
        // no number line at all.
        assert_eq!(rows[4].label, "555 ••• 5555");
        assert_eq!(rows[4].detail, None);
        for row in rows {
            assert!(!row.label.contains('@'), "no label is a JID: {}", row.label);
            assert_ne!(row.address, row.label, "the address is never the label");
        }
    }

    #[test]
    pub(super) fn join_request_rows_use_the_same_label_rule() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_named_people(&mut app, &tx, &ctx);
        tx.send(BackendEvent::JoinRequests {
            chat_jid: GROUP_JID.to_owned(),
            requests: vec![
                (BOB_JID.to_owned(), String::new()),
                (STRANGER_JID.to_owned(), String::new()),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let (_, rows) = app.groups.join_requests.as_ref().expect("the requests are stored");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, "Bob Carter", "a requester who has spoken resolves to his name");
        assert_eq!(rows[1].1, "+628 ••• 4444", "an unknown requester is masked");
        for (_, label) in rows {
            assert!(!label.contains('@'), "no request label is a JID: {label}");
            assert!(!label.contains("6284444444444"), "no request label is a full number");
        }
        // The JID survives as the review command's target.
        assert_eq!(rows[1].0, STRANGER_JID);
    }

    #[test]
    pub(super) fn masked_identifier_never_shows_a_full_number_or_jid() {
        assert_eq!(masked_identifier("6281234567890@s.whatsapp.net"), "+628 ••• 7890");
        assert_eq!(masked_identifier("123456789012345@lid"), "123 ••• 2345");
        // Not phone-like: returned as the bare user part.
        assert_eq!(masked_identifier("12345@s.whatsapp.net"), "12345");
        let masked = masked_identifier(STRANGER_JID);
        assert!(!masked.contains("6284444444444"));
        assert!(!masked.contains('@'));
    }

    #[test]
    pub(super) fn person_label_prefers_backend_then_known_then_masked() {
        let known = HashMap::from([(BOB_JID.to_owned(), "Bob Carter".to_owned())]);
        assert_eq!(
            resolve_person_label("Server Name", BOB_JID, &known),
            ("Server Name".to_owned(), true),
            "the carried name wins"
        );
        assert_eq!(
            resolve_person_label("", BOB_JID, &known),
            ("Bob Carter".to_owned(), true),
            "a known name fills an empty one"
        );
        assert_eq!(
            resolve_person_label("  ", STRANGER_JID, &known),
            ("+628 ••• 4444".to_owned(), false),
            "nothing known means the masked identifier"
        );
        // The detail line follows the label kind and the
        // address form.
        assert_eq!(member_detail(BOB_JID, true).as_deref(), Some("+6282222222222"));
        assert_eq!(member_detail(BOB_JID, false), None, "no number under a masked label");
        assert_eq!(member_detail(LID_JID, true), None, "no number for a LID row");
    }
}

#[cfg(test)]
mod chat_state_routing_tests {
    use super::*;
    use crate::state::{App, Args};
    use backend::ChatAction;

    const CHAT_JID: &str = "6281234567890@s.whatsapp.net";
    const BASE_TS: i64 = 1_700_000_000;

    /// The same live-mode harness as the burst tests: an app
    /// whose backend channels the test feeds and reads by hand,
    /// with an in-memory archive attached.
    pub(super) fn live_app() -> (
        App,
        std::sync::mpsc::Sender<BackendEvent>,
        egui::Context,
        std::sync::mpsc::Receiver<BackendCommand>,
    ) {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = App::new(Args::parse(), &ctx);
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
            poll_ballots: HashMap::new(),
            event_ballots: HashMap::new(),
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

    /// Section 25: the ZIP gather issues one fetch at a time
    /// on the item's own rail, settles the item when the
    /// answering event lands, and hands the completed
    /// collection to the writer, whose ZIP holds the
    /// transcript, the gathered media, and a manifest that
    /// accounts for the item that failed.
    #[test]
    pub(super) fn zip_gather_fetches_settles_and_writes() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        let photo_mid = message_id(CHAT_JID, "ph-1");
        let doc_mid = message_id(CHAT_JID, "dc-1");
        app.live.as_mut().unwrap().jids.insert(chat_id, CHAT_JID.to_owned());
        app.protocol_ids.insert(photo_mid, "ph-1".to_owned());
        app.protocol_ids.insert(doc_mid, "dc-1".to_owned());
        let item = |msg_id: u64, kind: ZipKind, file_name: &str| ZipItem {
            msg_id,
            kind,
            file_name: file_name.to_owned(),
            stamp: "14 November 2026 · 10:00".to_owned(),
            sender: "Ana".to_owned(),
            caption: String::new(),
            state: ZipItemState::Pending,
        };
        app.zip_gather = Some(ZipGather {
            chat_id,
            chat_title: "Ana".to_owned(),
            file_base: "zaprust-chat-ana.md".to_owned(),
            transcript: "# Ana\n".to_owned(),
            message_count: 2,
            items: vec![
                item(photo_mid, ZipKind::Photo, "001-photo.jpg"),
                item(doc_mid, ZipKind::Document, "002-document.pdf"),
            ],
            waiting: None,
            wait_started: 0.0,
        });
        // One fetch at a time: the photo goes first.
        app.zip_gather_advance(0.0);
        assert!(matches!(cmd_rx.try_recv(), Ok(BackendCommand::FetchPhoto { .. })));
        assert_eq!(app.zip_gather.as_ref().unwrap().waiting, Some(photo_mid));
        // Its bytes settle it; the document's download follows.
        tx.send(BackendEvent::PhotoBytes {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "ph-1".to_owned(),
            bytes: vec![1, 2, 3],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.zip_gather_advance(1.0);
        assert!(matches!(cmd_rx.try_recv(), Ok(BackendCommand::DownloadMedia { .. })));
        // The document's download fails as a generic error: the
        // item settles as missing, the export still completes,
        // and no error notice hijacks the export's own arc.
        tx.send(BackendEvent::Error { message: "media not found".to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        app.zip_gather_advance(2.0);
        assert!(app.zip_gather.is_none(), "every item settled, the gather finished");
        let rx = app.zip_export_rx.take().expect("the writer was armed");
        let path = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the writer answered")
            .expect("the write succeeded");
        let file = std::fs::File::open(&path).unwrap();
        let mut archive = ::zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
        assert!(names.contains(&"chat.md".to_owned()), "{names:?}");
        assert!(names.contains(&"media/001-photo.jpg".to_owned()), "{names:?}");
        assert!(names.contains(&"manifest.txt".to_owned()), "{names:?}");
        let mut manifest = String::new();
        {
            use std::io::Read as _;
            archive.by_name("manifest.txt").unwrap().read_to_string(&mut manifest).unwrap();
        }
        assert!(manifest.contains("media/001-photo.jpg"), "{manifest}");
        assert!(manifest.contains("Not included:"), "{manifest}");
        assert!(manifest.contains("media not found"), "{manifest}");
        let _ = std::fs::remove_file(&path);
    }

    /// Section 25: a fetch that never answers is settled by
    /// the watchdog instead of stalling the export forever.
    #[test]
    pub(super) fn zip_gather_watchdog_settles_a_silent_fetch() {
        let (mut app, _tx, _ctx, cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "ph-9");
        app.live.as_mut().unwrap().jids.insert(chat_id, CHAT_JID.to_owned());
        app.protocol_ids.insert(mid, "ph-9".to_owned());
        app.zip_gather = Some(ZipGather {
            chat_id,
            chat_title: "Ana".to_owned(),
            file_base: "zaprust-chat-ana.md".to_owned(),
            transcript: "# Ana\n".to_owned(),
            message_count: 1,
            items: vec![ZipItem {
                msg_id: mid,
                kind: ZipKind::Photo,
                file_name: "001-photo.jpg".to_owned(),
                stamp: "14 November 2026 · 10:00".to_owned(),
                sender: "Ana".to_owned(),
                caption: String::new(),
                state: ZipItemState::Pending,
            }],
            waiting: None,
            wait_started: 0.0,
        });
        app.zip_gather_advance(0.0);
        assert!(matches!(cmd_rx.try_recv(), Ok(BackendCommand::FetchPhoto { .. })));
        // Twenty-one seconds of silence: the item settles as
        // missing and the export completes without the photo.
        app.zip_gather_advance(21.0);
        assert!(app.zip_gather.is_none());
        let rx = app.zip_export_rx.take().expect("the writer was armed");
        let path = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the writer answered")
            .expect("the write succeeded");
        let _ = std::fs::remove_file(&path);
    }

    /// An arrival-style chat upsert: it carries no chat-action
    /// state (mute least of all), exactly like the real
    /// backend's message-arrival upserts.
    pub(super) fn arrival_upsert() -> BackendEvent {
        BackendEvent::ChatUpsert(LiveChat {
            jid: CHAT_JID.to_owned(),
            title: "Sender".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "hello".to_owned(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
        })
    }

    /// An informed chat upsert: it carries a real unread
    /// count, the way the fake's initial chat list does.
    pub(super) fn counted_upsert(unread: u32) -> BackendEvent {
        BackendEvent::ChatUpsert(LiveChat {
            jid: CHAT_JID.to_owned(),
            title: "Sender".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "hello".to_owned(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
        })
    }

    pub(super) fn seed_chat(
        app: &mut App,
        tx: &std::sync::mpsc::Sender<BackendEvent>,
        ctx: &egui::Context,
    ) {
        tx.send(arrival_upsert()).unwrap();
        drain_all(app, ctx);
        assert!(app.chats.iter().any(|c| c.id == jid_id(CHAT_JID)), "the upsert seeds the chat");
    }

    pub(super) fn chat_muted(app: &App) -> bool {
        app.chats.iter().find(|c| c.id == jid_id(CHAT_JID)).expect("chat exists").muted
    }

    pub(super) fn chat_favorite(app: &App) -> bool {
        app.chats.iter().find(|c| c.id == jid_id(CHAT_JID)).expect("chat exists").favorite
    }

    pub(super) fn archived_favorites(app: &App) -> Vec<String> {
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        store.chat_favorites().unwrap()
    }

    /// An arrival-style upsert for a second chat, so the
    /// favorites filter test can tell flagged and unflagged
    /// chats apart.
    pub(super) fn arrival_upsert_for(jid: &str) -> BackendEvent {
        BackendEvent::ChatUpsert(LiveChat {
            jid: jid.to_owned(),
            title: "Other".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "hello".to_owned(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
        })
    }

    pub(super) fn archived_muted(app: &App) -> bool {
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        store
            .chats()
            .unwrap()
            .iter()
            .find(|c| c.jid == CHAT_JID)
            .expect("the chat reached the archive")
            .muted
    }

    pub(super) fn chat_pinned(app: &App) -> bool {
        app.chats.iter().find(|c| c.id == jid_id(CHAT_JID)).expect("chat exists").pinned
    }

    pub(super) fn chat_archived(app: &App) -> bool {
        app.chats.iter().find(|c| c.id == jid_id(CHAT_JID)).expect("chat exists").archived
    }

    pub(super) fn store_row(app: &App) -> LiveChat {
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        store
            .chats()
            .unwrap()
            .into_iter()
            .find(|c| c.jid == CHAT_JID)
            .expect("the chat reached the archive")
    }

    pub(super) fn archived_pinned(app: &App) -> bool {
        store_row(app).pinned
    }

    pub(super) fn archived_archived(app: &App) -> bool {
        store_row(app).archived
    }

    /// An informed chat upsert: it carries real chat-action
    /// state, the way the fake's chat-action and LeaveGroup
    /// echoes do (they clone the fake's full chat state).
    pub(super) fn state_upsert(archived: bool, pinned: bool) -> BackendEvent {
        BackendEvent::ChatUpsert(LiveChat {
            jid: CHAT_JID.to_owned(),
            title: "Sender".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "hello".to_owned(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread: 0,
            archived,
            locked: false,
            pinned,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
        })
    }

    /// The header-menu / profile-panel toggle path: both call
    /// apply_chat_action with the flipped value. In live mode
    /// that must put the matching ChatAction command on the
    /// backend channel, flip the local mirror at once, and
    /// write the state through to the archive.
    #[test]
    pub(super) fn mute_toggle_sends_the_backend_command() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);

        app.apply_chat_action(chat_id, false, false, true);
        assert_eq!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::ChatAction {
                chat_jid: CHAT_JID.to_owned(),
                action: ChatAction::Mute
            }),
            "muting in live mode must reach the backend"
        );
        assert!(chat_muted(&app), "the local flip applies at once");
        assert!(archived_muted(&app), "the archive write-through carries the mute");

        app.apply_chat_action(chat_id, false, false, false);
        assert_eq!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::ChatAction {
                chat_jid: CHAT_JID.to_owned(),
                action: ChatAction::Unmute
            }),
            "unmuting in live mode must reach the backend"
        );
        assert!(!chat_muted(&app));
        assert!(!archived_muted(&app));
    }

    pub(super) fn chat_locked(app: &App) -> bool {
        app.chats.iter().find(|c| c.id == jid_id(CHAT_JID)).expect("chat exists").locked
    }

    /// Row 103: locking a chat sends the Lock command, flips
    /// the mirror and the archive row at once, and the
    /// LockChanged report settles the pending entry.
    #[test]
    pub(super) fn lock_toggle_sends_applies_and_confirms() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);
        app.apply_lock_chat(chat_id, true);
        assert_eq!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::ChatAction {
                chat_jid: CHAT_JID.to_owned(),
                action: ChatAction::Lock
            }),
            "locking in live mode must reach the backend"
        );
        assert!(chat_locked(&app), "the local flip applies at once");
        assert!(store_row(&app).locked, "the archive write-through carries the lock");
        assert!(app.pending_lock_actions.contains_key(&chat_id));
        tx.send(BackendEvent::LockChanged { chat_jid: CHAT_JID.to_owned(), locked: true }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.pending_lock_actions.is_empty(), "the report settles the lock");
        assert!(chat_locked(&app));
    }

    /// Row 103: a failed lock restores the exact previous
    /// state in the mirror and the archive.
    #[test]
    pub(super) fn failed_lock_rolls_back() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);
        app.apply_lock_chat(chat_id, true);
        assert!(chat_locked(&app));
        tx.send(BackendEvent::ChatActionFailed {
            chat_jid: CHAT_JID.to_owned(),
            action: ChatAction::Lock,
            message: "Changing the chat failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!chat_locked(&app), "the failed lock rolls back");
        assert!(!store_row(&app).locked, "the archive row rolls back with it");
        assert!(app.pending_lock_actions.is_empty());
    }

    /// Row 103: a lock made on another device (a LockChanged
    /// with no local pending entry) lands in the mirror and
    /// the archive, and an uninformed upsert does not clear
    /// the known lock afterwards.
    #[test]
    pub(super) fn lock_changed_from_elsewhere_lands() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        tx.send(BackendEvent::LockChanged { chat_jid: CHAT_JID.to_owned(), locked: true }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_locked(&app));
        assert!(store_row(&app).locked);
        tx.send(state_upsert(false, false)).unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_locked(&app), "an uninformed upsert keeps the known lock");
    }

    /// Item 116: the row menu's favorite toggle (apply_favorite,
    /// the handler the menu pick lands in) flips the mirror at
    /// once and writes the flag through to the archive, and it
    /// never sends a backend command: a favorite is a local
    /// concept with no server counterpart.
    #[test]
    pub(super) fn favorite_toggle_persists_locally_without_a_backend_command() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);
        assert!(!chat_favorite(&app));
        assert!(archived_favorites(&app).is_empty());

        app.apply_favorite(chat_id, true);
        assert!(chat_favorite(&app), "the local flip applies at once");
        assert_eq!(
            archived_favorites(&app),
            vec![CHAT_JID.to_owned()],
            "the archive write-through carries the flag"
        );
        assert!(cmd_rx.try_recv().is_err(), "no backend command may be sent for a favorite");

        app.apply_favorite(chat_id, false);
        assert!(!chat_favorite(&app));
        assert!(archived_favorites(&app).is_empty());
        assert!(cmd_rx.try_recv().is_err());
    }

    /// Item 116: a chat-list re-sync must not wipe a favorite.
    /// The arrival upsert knows nothing about favorites (the
    /// wire has no such field) and the merge leaves the mirror
    /// flag alone; the archive flag sits in a column the sync
    /// write never names, so it survives there by construction
    /// too.
    #[test]
    pub(super) fn arrival_upsert_keeps_the_known_favorite() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);
        app.apply_favorite(chat_id, true);

        tx.send(arrival_upsert()).unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_favorite(&app), "an arrival upsert must not unfavorite");
        assert_eq!(archived_favorites(&app), vec![CHAT_JID.to_owned()]);
    }

    /// Item 116: the Favorites filter chip reads the live
    /// flags: over two live chats it lists exactly the flagged
    /// one.
    #[test]
    pub(super) fn favorites_filter_lists_only_flagged_live_chats() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        const OTHER_JID: &str = "628999888777@s.whatsapp.net";
        tx.send(arrival_upsert_for(OTHER_JID)).unwrap();
        drain_all(&mut app, &ctx);
        app.apply_favorite(jid_id(CHAT_JID), true);

        let out = core::filter::filter_chats(&app.chats, core::filter::ChatFilter::Favorites, "");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, jid_id(CHAT_JID));
        let all = core::filter::filter_chats(&app.chats, core::filter::ChatFilter::All, "");
        assert_eq!(all.len(), 2, "the unfiltered list still shows both chats");
    }

    /// Item 116: the flags are durable across sessions: a new
    /// session loading the same archive (the conversion plus
    /// overlay start_live performs) renders the favorite the
    /// previous session recorded.
    #[test]
    pub(super) fn favorite_survives_into_a_new_session_from_the_same_archive() {
        let dir = std::env::temp_dir()
            .join(format!("zaprust-app-favorites-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("archive.db");
        let _ = std::fs::remove_file(&path);
        let key = [3u8; 32];
        {
            let (mut app, tx, ctx, _cmd_rx) = live_app();
            app.live.as_mut().unwrap().store = Some(store::Store::open(&path, &key).unwrap());
            seed_chat(&mut app, &tx, &ctx);
            app.apply_favorite(jid_id(CHAT_JID), true);
            assert!(chat_favorite(&app));
        }
        let (mut app, _tx, _ctx, _cmd_rx) = live_app();
        app.live.as_mut().unwrap().store = Some(store::Store::open(&path, &key).unwrap());
        // The start_live load: convert the archived rows, then
        // overlay the archive's favorite flags.
        let stored = app.live.as_ref().unwrap().store.as_ref().unwrap().chats().unwrap();
        app.chats = stored.iter().map(to_chat).collect();
        assert!(!chat_favorite(&app), "the conversion alone carries no favorite");
        app.apply_stored_favorites();
        assert!(chat_favorite(&app), "the overlay restores the recorded favorite");
        let _ = std::fs::remove_file(&path);
    }

    /// Item 116: mode gating. A live session's archive starts
    /// with no favorites, whatever the demo fixtures carry
    /// (some demo chats are favorited); and in demo mode, with
    /// no session and no archive behind it, the toggle is the
    /// in-memory fixture flip only.
    #[test]
    pub(super) fn favorite_toggle_in_demo_mode_stays_in_memory() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        assert!(
            archived_favorites(&app).is_empty(),
            "a fresh archive has no favorites, whatever the demo fixtures carry"
        );
        app.live = None;
        app.live_mode = false;
        app.apply_favorite(jid_id(CHAT_JID), true);
        assert!(chat_favorite(&app), "the in-memory flip still lands for the demo session");
    }

    /// A mute change the server reports (another device, or a
    /// full sync) adopts unconditionally, even over a local
    /// optimistic toggle: the server value is the truth the
    /// local mirror reconciles to.
    #[test]
    pub(super) fn server_mute_changed_overrides_local_state() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);

        tx.send(BackendEvent::MuteChanged {
            chat_jid: CHAT_JID.to_owned(),
            muted: true,
            mute_until: 0,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_muted(&app), "a server mute lands in the mirror");
        assert!(archived_muted(&app), "and in the archive");

        // The user unmutes here (optimistic local flip plus the
        // backend command), then the server re-asserts the mute,
        // as it would if that command failed or another device
        // muted again: the server value wins.
        app.apply_chat_action(chat_id, false, false, false);
        assert!(!chat_muted(&app));
        tx.send(BackendEvent::MuteChanged {
            chat_jid: CHAT_JID.to_owned(),
            muted: true,
            mute_until: 0,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_muted(&app), "the server value overrides the local toggle");
        assert!(archived_muted(&app));
    }

    /// The counterpart rule: an arrival upsert knows nothing
    /// about mute, so it must not unmute a chat the mirror
    /// knows is muted (the merge protection stays).
    #[test]
    pub(super) fn arrival_upsert_keeps_the_known_mute() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);

        tx.send(BackendEvent::MuteChanged {
            chat_jid: CHAT_JID.to_owned(),
            muted: true,
            mute_until: 0,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_muted(&app));

        tx.send(arrival_upsert()).unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_muted(&app), "an arrival upsert must not unmute");
        assert!(archived_muted(&app), "nor rewrite the archive row as unmuted");
    }

    /// The connect-time blocklist adopts wholesale: the
    /// server's set replaces the locally persisted copy, so a
    /// block made on another device (or before a restart)
    /// shows correctly, and an unblock elsewhere clears it
    /// (audit finding 12).
    #[test]
    pub(super) fn server_blocklist_replaces_the_local_set() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);

        // A stale local entry the server does not report.
        app.blocked_chats.insert(424_242);
        tx.send(BackendEvent::Blocklist(vec![CHAT_JID.to_owned()])).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.blocked_chats, std::collections::HashSet::from([chat_id]));

        tx.send(BackendEvent::Blocklist(Vec::new())).unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.blocked_chats.is_empty(), "an empty server list clears the set");
    }

    pub(super) fn own_profile(name: &str, about: &str) -> BackendEvent {
        BackendEvent::OwnProfile(backend::OwnProfile {
            name: name.to_owned(),
            about: about.to_owned(),
            photo: None,
        })
    }

    /// The connect-time profile read-back populates the editor:
    /// the account becomes the editor's truth and the buffers
    /// take its values, so the panel opens on the account's real
    /// name and About, not on the settings cache.
    #[test]
    pub(super) fn own_profile_read_back_populates_the_editor() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(own_profile("Real Name", "Real about")).unwrap();
        drain_all(&mut app, &ctx);
        let account = app.my_profile.account.as_ref().expect("the account is adopted");
        assert_eq!(account.name, "Real Name");
        assert_eq!(app.my_profile.name, "Real Name");
        assert_eq!(app.my_profile.about, "Real about");
        assert!(!app.my_profile.live_dirty());
        assert!(cmd_rx.try_recv().is_err(), "adopting a read-back sends nothing back");
    }

    /// A read-back landing mid-edit updates the account truth
    /// but never the buffers: what the user typed stays theirs.
    #[test]
    pub(super) fn own_profile_read_back_does_not_clobber_a_dirty_editor() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(own_profile("Account Name", "Account about")).unwrap();
        drain_all(&mut app, &ctx);
        app.my_profile.name = "Typed Name".to_owned();
        tx.send(own_profile("Changed Elsewhere", "Account about")).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.my_profile.name, "Typed Name", "the edit buffer is untouched");
        assert_eq!(
            app.my_profile.account.as_ref().unwrap().name,
            "Changed Elsewhere",
            "the account truth still advances"
        );
        assert!(app.my_profile.live_dirty(), "the buffer now differs from the account");
    }

    /// Opening the editor asks the backend for a fresh read-back.
    #[test]
    pub(super) fn open_my_profile_requests_a_fresh_read_back() {
        let (mut app, _tx, _ctx, cmd_rx) = live_app();
        app.open_my_profile();
        assert!(app.my_profile_open);
        assert_eq!(cmd_rx.try_recv().unwrap(), BackendCommand::FetchOwnProfile);
    }

    /// A save sends exactly the changed fields, marks the save
    /// in flight, and resolves when the backend's re-read lands:
    /// the phase clears, the saved notice shows, and the buffers
    /// adopt the re-read values.
    #[test]
    pub(super) fn own_profile_save_sends_changed_fields_and_resolves_on_the_read_back() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(own_profile("Account Name", "Account about")).unwrap();
        drain_all(&mut app, &ctx);

        app.my_profile.name = "New Name".to_owned();
        app.save_my_profile();
        assert_eq!(app.my_profile.save_phase, crate::profile::SavePhase::Saving);
        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            BackendCommand::UpdateOwnProfile {
                name: Some("New Name".to_owned()),
                about: None,
                photo: None,
            },
            "only the changed field is sent"
        );

        tx.send(own_profile("New Name", "Account about")).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.my_profile.save_phase, crate::profile::SavePhase::Idle);
        assert_eq!(app.my_profile.name, "New Name");
        assert_eq!(app.notice_str(), Some(crate::i18n::t("profile.my_saved").as_str()));
    }

    /// A failed save reports the server's reason in the panel
    /// state and reverts the buffers to the account values the
    /// server kept; the reconciling re-read that follows the
    /// failure changes nothing.
    #[test]
    pub(super) fn own_profile_save_failure_reverts_and_reports() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(own_profile("Account Name", "Account about")).unwrap();
        drain_all(&mut app, &ctx);

        app.my_profile.name = "Rejected Name".to_owned();
        app.save_my_profile();
        tx.send(BackendEvent::OwnProfileSaveFailed {
            message: "Profile change failed: the name cannot be empty.".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.my_profile.save_phase,
            crate::profile::SavePhase::Failed(
                "Profile change failed: the name cannot be empty.".to_owned()
            )
        );
        assert_eq!(app.my_profile.name, "Account Name", "buffers revert to the account");

        tx.send(own_profile("Account Name", "Account about")).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.my_profile.name, "Account Name");
        assert!(
            matches!(app.my_profile.save_phase, crate::profile::SavePhase::Failed(_)),
            "the failure stays visible until the next attempt"
        );
    }

    /// Before the device is linked there is no account to save
    /// to: the editor reports the not-linked mode and a save
    /// sends nothing.
    #[test]
    pub(super) fn own_profile_save_is_refused_until_linked() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(own_profile("Account Name", "Account about")).unwrap();
        drain_all(&mut app, &ctx);
        app.live.as_mut().unwrap().status = LinkStatus::WaitingQr;
        assert_eq!(app.my_profile_mode(), crate::profile::MyProfileMode::NotLinked);
        app.my_profile.name = "New Name".to_owned();
        app.save_my_profile();
        assert!(cmd_rx.try_recv().is_err(), "no command leaves while unlinked");
        assert_eq!(app.my_profile.save_phase, crate::profile::SavePhase::Idle);
    }

    /// The blocked set no longer persists in the plaintext
    /// settings document (privacy audit A6): its ids are FNV-1a
    /// hashes of phone-number JIDs, and the phone-number space
    /// is small enough to dictionary-reverse, so the set's home
    /// is the encrypted archive store (covered by the store
    /// round-trip test). The settings JSON must not carry the
    /// field at all, which also scrubs the legacy copy older
    /// builds wrote on the next settings save.
    #[test]
    pub(super) fn settings_json_never_carries_the_blocked_set() {
        let (mut app, _tx, _ctx, _cmd_rx) = live_app();
        app.blocked_chats.insert(424_242);
        let json = app.settings_json();
        assert!(
            json.get("blocked_chats").is_none(),
            "the settings document must not hold the blocked set: {json}"
        );
        assert!(
            !json.to_string().contains("424242"),
            "no blocked id leaks into any other settings field"
        );
    }

    /// The font-size preference persists in the settings
    /// document: what the Appearance chips set is what
    /// persist_settings writes, at every offered step, so a
    /// restart reopens at the chosen size (the loader reads
    /// the same key back, clamped to the offered range).
    #[test]
    pub(super) fn settings_json_carries_the_font_size_preference() {
        let (mut app, _tx, _ctx, _cmd_rx) = live_app();
        for percent in [85_u32, 100, 115] {
            app.zoom_percent = percent;
            assert_eq!(
                app.settings_json()["zoom_percent"],
                serde_json::json!(percent),
                "the {percent}% step must reach the settings document"
            );
        }
    }

    /// The removed Custom title bar toggle leaves no trace in
    /// the settings document: its field was never persisted
    /// (nothing read it back either), and the field itself is
    /// gone, so no key may appear here.
    #[test]
    pub(super) fn settings_json_has_no_custom_title_bar_key() {
        let (app, _tx, _ctx, _cmd_rx) = live_app();
        assert!(app.settings_json().get("custom_title_bar").is_none());
    }

    /// The connect-time privacy fetch reconciles the local
    /// read-receipts toggle with the account's setting (audit
    /// finding D3): the server value is adopted into the flag
    /// that gates receipt sending and into the settings document
    /// persist_settings writes, in both directions, and adopting
    /// sends nothing back to the backend.
    #[test]
    pub(super) fn server_read_receipts_setting_is_adopted() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        // Set the starting flag explicitly: adoption persists,
        // and a parallel test's write must not leak into this
        // one's starting state through the settings file.
        app.read_receipts = true;

        // The account has receipts off (changed on the phone):
        // the fetch adopts that.
        let mut state = std::collections::BTreeMap::new();
        state.insert("last".to_owned(), "contacts".to_owned());
        state.insert("readreceipts".to_owned(), "none".to_owned());
        tx.send(BackendEvent::PrivacyState(state)).unwrap();
        drain_all(&mut app, &ctx);
        assert!(!app.read_receipts, "the server value drives the toggle");
        assert_eq!(
            app.settings_json()["read_receipts"],
            serde_json::json!(false),
            "the adopted value is what persist_settings writes"
        );
        assert_eq!(app.privacy_state.get("last").map(String::as_str), Some("contacts"));
        assert!(cmd_rx.try_recv().is_err(), "adopting a fetched state sends nothing back");

        // And back on, the same way.
        let mut state = std::collections::BTreeMap::new();
        state.insert("readreceipts".to_owned(), "all".to_owned());
        tx.send(BackendEvent::PrivacyState(state)).unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.read_receipts);
        assert_eq!(app.settings_json()["read_receipts"], serde_json::json!(true));
    }

    /// A fetched readreceipts value the toggle cannot represent
    /// (and a state that does not carry the category at all)
    /// leaves the local flag alone.
    #[test]
    pub(super) fn unrecognized_server_read_receipts_value_is_not_adopted() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.read_receipts = true;

        let mut state = std::collections::BTreeMap::new();
        state.insert("readreceipts".to_owned(), "contact_blacklist".to_owned());
        tx.send(BackendEvent::PrivacyState(state)).unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.read_receipts, "an unrepresentable value changes nothing");

        let mut state = std::collections::BTreeMap::new();
        state.insert("last".to_owned(), "none".to_owned());
        tx.send(BackendEvent::PrivacyState(state)).unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.read_receipts, "a missing category changes nothing");
    }

    /// The Privacy page toggle path: flipping the switch flips
    /// the local gate at once and puts the matching SetPrivacy
    /// command on the backend channel, so the account setting
    /// follows the toggle.
    #[test]
    pub(super) fn read_receipts_toggle_sends_set_privacy() {
        let (mut app, _tx, _ctx, cmd_rx) = live_app();

        app.set_read_receipts(false);
        assert!(!app.read_receipts, "the local gate flips at once");
        assert_eq!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::SetPrivacy {
                category: "readreceipts".to_owned(),
                value: "none".to_owned(),
            }),
            "turning receipts off must reach the account setting"
        );

        app.set_read_receipts(true);
        assert!(app.read_receipts);
        assert_eq!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::SetPrivacy {
                category: "readreceipts".to_owned(),
                value: "all".to_owned(),
            }),
            "turning receipts on must reach the account setting"
        );
    }

    /// The categories the Privacy page has no row for still
    /// survive into app state (audit finding D6): the fetched
    /// map is stored whole, so the account's call-add, messages
    /// and defense-mode settings are there for anything that
    /// needs the account's actual state.
    #[test]
    pub(super) fn rowless_privacy_categories_survive_into_app_state() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mut state = std::collections::BTreeMap::new();
        state.insert("calladd".to_owned(), "known".to_owned());
        state.insert("messages".to_owned(), "contacts".to_owned());
        state.insert("defense".to_owned(), "on_standard".to_owned());
        tx.send(BackendEvent::PrivacyState(state)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.privacy_state.get("calladd").map(String::as_str), Some("known"));
        assert_eq!(app.privacy_state.get("messages").map(String::as_str), Some("contacts"));
        assert_eq!(app.privacy_state.get("defense").map(String::as_str), Some("on_standard"));
    }

    /// The server-pushed account default timer is adopted into
    /// the local control and persisted (audit finding D8), the
    /// same server-wins rule as read receipts, and adopting it
    /// sends nothing back.
    #[test]
    pub(super) fn server_default_timer_is_adopted() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        app.default_ephemeral_secs = 0;
        tx.send(BackendEvent::DefaultDisappearingTimerChanged { duration_secs: 86_400 }).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.default_ephemeral_secs, 86_400);
        assert_eq!(
            app.settings_json()["default_ephemeral_secs"],
            serde_json::json!(86_400),
            "the adopted value is what persist_settings writes"
        );
        assert!(cmd_rx.try_recv().is_err(), "adopting a pushed value sends nothing back");
    }

    /// Turning typing indicators off retracts a composing state
    /// already sent (audit finding D10): the peer must not keep
    /// seeing "typing..." until the server-side timeout from a
    /// client whose user just turned indicators off.
    #[test]
    pub(super) fn typing_off_retracts_the_composing_chat() {
        let (mut app, _tx, _ctx, cmd_rx) = live_app();
        let chat_id = 4242;
        app.live.as_mut().unwrap().jids.insert(chat_id, "6281234567890@s.whatsapp.net".to_owned());
        app.typing_indicators = true;
        app.typing_sent_for = Some(chat_id);

        app.set_typing_indicators(false);
        assert!(!app.typing_indicators);
        assert_eq!(app.typing_sent_for, None);
        assert_eq!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::SetTyping {
                chat_jid: "6281234567890@s.whatsapp.net".to_owned(),
                typing: false,
            }),
            "flipping the toggle off must retract the composing state"
        );

        // Turning it back on sends nothing by itself.
        app.set_typing_indicators(true);
        assert!(cmd_rx.try_recv().is_err());
    }

    /// Cycling away from `contact_blacklist` asks first (audit
    /// finding D7): the value names an exception list configured
    /// on the phone, and replacing it discards the association
    /// with that list, which this client cannot restore. Any
    /// other current value cycles at once, without asking.
    #[test]
    pub(super) fn leaving_contact_blacklist_asks_first() {
        let (mut app, _tx, _ctx, cmd_rx) = live_app();
        app.privacy_state.insert("last".to_owned(), "contact_blacklist".to_owned());
        app.request_cycle_privacy("last", "Last seen");
        assert!(
            matches!(app.ui_confirm, Some(crate::state::UiConfirm::ReplaceExceptionList { .. })),
            "leaving the exception list parks a confirmation"
        );
        assert!(cmd_rx.try_recv().is_err(), "nothing is sent before the user confirms");

        app.ui_confirm = None;
        app.privacy_state.insert("last".to_owned(), "contacts".to_owned());
        app.request_cycle_privacy("last", "Last seen");
        assert!(app.ui_confirm.is_none(), "an ordinary value cycles without asking");
        assert_eq!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::SetPrivacy {
                category: "last".to_owned(),
                value: "none".to_owned(),
            })
        );
    }

    /// The Online row's cycle is exactly the set the category
    /// accepts: `all` and `match_last_seen`, wrapping. The values
    /// the shared cycle used to offer it (`contacts`, `none`) are
    /// not in the set, and the sibling rows keep their own set.
    #[test]
    pub(super) fn online_cycle_visits_exactly_its_valid_set_and_wraps() {
        assert_eq!(privacy_audiences("online"), ["all", "match_last_seen"]);
        assert_eq!(next_privacy_audience("online", "all"), Some("match_last_seen"));
        assert_eq!(next_privacy_audience("online", "match_last_seen"), Some("all"));

        for category in ["last", "profile", "status", "groupadd"] {
            assert_eq!(privacy_audiences(category), ["all", "contacts", "none"], "{category}");
            assert_eq!(next_privacy_audience(category, "all"), Some("contacts"));
            assert_eq!(next_privacy_audience(category, "contacts"), Some("none"));
            assert_eq!(next_privacy_audience(category, "none"), Some("all"));
        }

        // `contact_blacklist` is a real fetched value for the
        // four list categories, but it is never a cycle stop:
        // sending it bare would discard the exception list it
        // names.
        for category in ["online", "last", "profile", "status", "groupadd"] {
            assert!(
                !privacy_audiences(category).contains(&"contact_blacklist"),
                "{category} must not offer contact_blacklist"
            );
        }
        // Categories the page does not show have no cycle.
        assert_eq!(next_privacy_audience("readreceipts", "all"), None);
    }

    /// A fetched value the page cannot represent is shown as
    /// itself and never silently presented as a neighbor, and
    /// cycling away from it lands somewhere valid and predictable:
    /// an exception list advances past "contacts", anything else
    /// unknown restarts the cycle at its first audience.
    #[test]
    pub(super) fn unrecognized_privacy_value_is_not_coerced() {
        assert_eq!(privacy_value_label("all"), "Everyone");
        assert_eq!(privacy_value_label("contacts"), "My contacts");
        assert_eq!(privacy_value_label("none"), "Nobody");
        assert_eq!(privacy_value_label("match_last_seen"), "Same as last seen");
        assert_eq!(privacy_value_label("contact_blacklist"), "My contacts except…");
        // Unknown spellings display verbatim: the page does not
        // pretend they are an audience it knows.
        assert_eq!(privacy_value_label("known"), "known");
        assert_eq!(privacy_value_label("some_future_value"), "some_future_value");

        assert_eq!(next_privacy_audience("last", "contact_blacklist"), Some("none"));
        assert_eq!(next_privacy_audience("profile", "contact_blacklist"), Some("none"));
        assert_eq!(next_privacy_audience("online", "some_future_value"), Some("all"));
        assert_eq!(next_privacy_audience("last", "some_future_value"), Some("all"));
    }

    /// The Privacy page's Online row: cycling sends SetPrivacy
    /// with the server's exact spelling for "same as last seen",
    /// the value the old shared cycle could never produce.
    #[test]
    pub(super) fn cycling_online_sends_match_last_seen_with_exact_spelling() {
        let (mut app, _tx, _ctx, cmd_rx) = live_app();
        app.privacy_state.insert("online".to_owned(), "all".to_owned());

        app.cycle_privacy("online");
        assert_eq!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::SetPrivacy {
                category: "online".to_owned(),
                value: "match_last_seen".to_owned(),
            }),
            "cycling Online from Everyone must offer same-as-last-seen"
        );
        // The displayed value does not move on its own: only the
        // backend's answering PrivacyState moves it.
        assert_eq!(app.privacy_state.get("online").map(String::as_str), Some("all"));

        app.privacy_state.insert("online".to_owned(), "match_last_seen".to_owned());
        app.cycle_privacy("online");
        assert_eq!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::SetPrivacy {
                category: "online".to_owned(),
                value: "all".to_owned()
            }),
            "the cycle wraps back to Everyone"
        );
    }

    /// The Settings cache-clear must empty every in-memory
    /// cache, including the media and avatar texture caches it
    /// used to miss (audit finding 10).
    #[test]
    pub(super) fn cache_clear_covers_all_caches() {
        let (mut app, _tx, ctx, _cmd_rx) = live_app();
        let handle = ctx.load_texture(
            "test",
            egui::ColorImage::new([1, 1], vec![egui::Color32::RED]),
            egui::TextureOptions::NEAREST,
        );
        app.media.textures.insert(1, handle.clone());
        app.media.avatar_textures.insert("Sender".to_owned(), handle.clone());
        app.picker.giphy_textures.insert("https://example.com/g".to_owned(), handle.clone());
        app.picker.sticker_textures.insert(std::path::PathBuf::from("sticker.png"), handle);

        app.clear_in_memory_caches();

        assert!(app.media.textures.get(&1).is_none());
        assert!(app.media.avatar_textures.get("Sender").is_none());
        assert!(app.picker.giphy_textures.get("https://example.com/g").is_none());
        assert!(app
            .picker
            .sticker_textures
            .get(&std::path::PathBuf::from("sticker.png"))
            .is_none());
        assert!(app.cache_cleared);
    }

    /// A pin change the server reports (another device, or a
    /// full sync) adopts unconditionally, even over a local
    /// optimistic toggle: the server value is the truth the
    /// local mirror reconciles to.
    #[test]
    pub(super) fn server_pin_changed_overrides_local_state() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);

        tx.send(BackendEvent::PinChanged { chat_jid: CHAT_JID.to_owned(), pinned: true }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_pinned(&app), "a server pin lands in the mirror");
        assert!(archived_pinned(&app), "and in the archive");

        // The user unpins here (optimistic local flip plus the
        // backend command), then the server re-asserts the pin,
        // as it would if that command failed or another device
        // pinned again: the server value wins.
        app.apply_chat_action(chat_id, false, false, false);
        assert!(!chat_pinned(&app));
        tx.send(BackendEvent::PinChanged { chat_jid: CHAT_JID.to_owned(), pinned: true }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_pinned(&app), "the server value overrides the local toggle");
        assert!(archived_pinned(&app));

        // And an unpin from another device unpins here too.
        tx.send(BackendEvent::PinChanged { chat_jid: CHAT_JID.to_owned(), pinned: false }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(!chat_pinned(&app), "a server unpin lands in the mirror");
        assert!(!archived_pinned(&app), "and in the archive");
    }

    /// An archive change the server reports adopts
    /// unconditionally, in both directions, exactly like pin.
    #[test]
    pub(super) fn server_archive_changed_overrides_local_state() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);

        tx.send(BackendEvent::ArchiveChanged { chat_jid: CHAT_JID.to_owned(), archived: true })
            .unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_archived(&app), "a server archive lands in the mirror");
        assert!(archived_archived(&app), "and in the archive");

        // The user unarchives here, then the server re-asserts
        // the archive: the server value wins.
        app.apply_chat_action(chat_id, false, false, false);
        assert!(!chat_archived(&app));
        tx.send(BackendEvent::ArchiveChanged { chat_jid: CHAT_JID.to_owned(), archived: true })
            .unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_archived(&app), "the server value overrides the local toggle");
        assert!(archived_archived(&app));

        // And an unarchive from another device restores the
        // chat to the main list here too.
        tx.send(BackendEvent::ArchiveChanged { chat_jid: CHAT_JID.to_owned(), archived: false })
            .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!chat_archived(&app), "a server unarchive lands in the mirror");
        assert!(!archived_archived(&app), "and in the archive");
    }

    /// The counterpart rule for pin and archive: an arrival
    /// upsert knows nothing about either, so it must not unpin
    /// or unarchive a chat the mirror knows is pinned and
    /// archived (the merge protection).
    #[test]
    pub(super) fn arrival_upsert_keeps_the_known_pin_and_archive() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);

        tx.send(BackendEvent::PinChanged { chat_jid: CHAT_JID.to_owned(), pinned: true }).unwrap();
        tx.send(BackendEvent::ArchiveChanged { chat_jid: CHAT_JID.to_owned(), archived: true })
            .unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_pinned(&app) && chat_archived(&app));

        tx.send(arrival_upsert()).unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_pinned(&app), "an arrival upsert must not unpin");
        assert!(chat_archived(&app), "an arrival upsert must not unarchive");
        assert!(archived_pinned(&app), "nor rewrite the archive row as unpinned");
        assert!(archived_archived(&app), "nor rewrite the archive row as unarchived");
    }

    const OTHER_JID: &str = "6281987654321@s.whatsapp.net";

    /// An arrival-style chat upsert for an arbitrary JID, the
    /// `arrival_upsert` shape with a different identity.
    pub(super) fn arrival_upsert_jid(jid: &str) -> BackendEvent {
        BackendEvent::ChatUpsert(LiveChat {
            jid: jid.to_owned(),
            title: "Other".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "hello".to_owned(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
        })
    }

    pub(super) fn chat_unread(app: &App) -> u32 {
        app.chats.iter().find(|c| c.id == jid_id(CHAT_JID)).expect("chat exists").unread
    }

    pub(super) fn mirror_starred(app: &App, chat_id: u64, msg_id: u64) -> bool {
        app.live
            .as_ref()
            .unwrap()
            .messages
            .get(&chat_id)
            .and_then(|msgs| msgs.iter().find(|(_, m)| m.id == msg_id))
            .expect("message exists")
            .1
            .starred
    }

    pub(super) fn archived_flags(app: &App, protocol_id: &str) -> i64 {
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        store
            .messages(CHAT_JID)
            .unwrap()
            .into_iter()
            .find(|(m, _)| m.id == protocol_id)
            .expect("the message reached the archive")
            .1
    }

    pub(super) fn mirror_bodies(app: &App, chat_id: u64) -> Vec<String> {
        app.live
            .as_ref()
            .unwrap()
            .messages
            .get(&chat_id)
            .map(|msgs| msgs.iter().map(|(_, m)| m.body.clone()).collect())
            .unwrap_or_default()
    }

    pub(super) fn archived_bodies(app: &App) -> Vec<String> {
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        store.messages(CHAT_JID).unwrap().into_iter().map(|(m, _)| m.body).collect()
    }

    /// A star change the server reports (another device, or a
    /// full sync) adopts unconditionally in both directions:
    /// the session mirror flips and the archive flags follow,
    /// and the adoption sends no command back (the change is
    /// already on the server).
    #[test]
    pub(super) fn server_star_changed_overrides_local_state() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "s1");
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "s1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "star me",
            BASE_TS,
            false,
        )))
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!mirror_starred(&app, chat_id, mid));
        assert_eq!(archived_flags(&app, "s1"), 0);

        tx.send(BackendEvent::StarChanged {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "s1".to_owned(),
            starred: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(mirror_starred(&app, chat_id, mid), "a server star lands in the mirror");
        assert_eq!(archived_flags(&app, "s1"), store::FLAG_STARRED, "and in the archive flags");
        assert!(cmd_rx.try_recv().is_err(), "adopting a server star sends nothing back");

        tx.send(BackendEvent::StarChanged {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "s1".to_owned(),
            starred: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!mirror_starred(&app, chat_id, mid), "a server unstar lands in the mirror");
        assert_eq!(archived_flags(&app, "s1"), 0, "and the archive flags clear");
    }

    /// The counterpart rule for star: an arrival upsert reports
    /// `starred: false` without knowing better, so it must not
    /// unstar a message the session flag map knows is starred
    /// (the upsert resolves its flags from that map).
    #[test]
    pub(super) fn arrival_upsert_keeps_the_known_star() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "s1");
        let upsert = || {
            BackendEvent::MessageUpsert(LiveMessage::text(
                "s1",
                CHAT_JID,
                Some("Sender".to_owned()),
                "star me",
                BASE_TS,
                false,
            ))
        };
        tx.send(upsert()).unwrap();
        tx.send(BackendEvent::StarChanged {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "s1".to_owned(),
            starred: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(mirror_starred(&app, chat_id, mid));

        tx.send(upsert()).unwrap();
        drain_all(&mut app, &ctx);
        assert!(mirror_starred(&app, chat_id, mid), "an arrival upsert must not unstar");
        assert_eq!(archived_flags(&app, "s1"), store::FLAG_STARRED);
    }

    /// A chat read on another device adopts as read here: the
    /// badge clears in the mirror and the archive, the pending
    /// receipt queue is dropped rather than flushed, and no
    /// MarkAsRead command ever goes out for it (the other
    /// device already sent the receipts).
    #[test]
    pub(super) fn server_chat_read_changed_clears_unread_without_sending_receipts() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);
        for (pid, ts) in [("r1", BASE_TS), ("r2", BASE_TS + 60)] {
            tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
                pid,
                CHAT_JID,
                Some("Sender".to_owned()),
                "unread",
                ts,
                false,
            )))
            .unwrap();
        }
        drain_all(&mut app, &ctx);
        assert_eq!(chat_unread(&app), 2, "two arrivals raise the badge");
        assert_eq!(
            app.unread_live.get(&chat_id).map(Vec::len),
            Some(2),
            "both receipts are pending locally"
        );

        tx.send(BackendEvent::ChatReadChanged {
            chat_jid: CHAT_JID.to_owned(),
            read: true,
            unread_count: 0,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_unread(&app), 0, "the remote read clears the badge");
        assert_eq!(store_row(&app).unread, 0, "and the archive row follows");
        assert!(
            !app.unread_live.contains_key(&chat_id),
            "the pending receipts are dropped, not kept for a later flush"
        );
        assert!(cmd_rx.try_recv().is_err(), "adopting a remote read sends nothing back");

        // Even a genuine local read action afterwards finds
        // nothing to mark: the receipts are gone for good.
        app.selected_chat = chat_id;
        app.note_read_engagement();
        assert!(cmd_rx.try_recv().is_err(), "no receipts go out after the remote read");
    }

    /// A chat marked unread on another device adopts the
    /// reported count (floored at one: the chat IS unread, so a
    /// zero badge would hide the asserted state), in the mirror
    /// and the archive.
    #[test]
    pub(super) fn server_chat_read_changed_marks_unread_with_the_reported_count() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);

        tx.send(BackendEvent::ChatReadChanged {
            chat_jid: CHAT_JID.to_owned(),
            read: false,
            unread_count: 3,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_unread(&app), 3, "the reported count lands in the mirror");
        assert_eq!(store_row(&app).unread, 3, "and in the archive");

        tx.send(BackendEvent::ChatReadChanged {
            chat_jid: CHAT_JID.to_owned(),
            read: false,
            unread_count: 0,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_unread(&app), 1, "a count of zero still shows the chat as unread");
        assert_eq!(store_row(&app).unread, 1);
    }

    /// An upsert that reports a real count (the fake's initial
    /// chat list is the informed form) is adopted as reported,
    /// in the mirror and the archive, even over a known count:
    /// adoption is not a max rule. Only a reported 0 is treated
    /// as "not carried" (see the next test).
    #[test]
    pub(super) fn informed_upsert_adopts_the_reported_unread_count() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(counted_upsert(4)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_unread(&app), 4, "the reported count lands in the mirror");
        assert_eq!(store_row(&app).unread, 4, "and in the archive");

        tx.send(counted_upsert(2)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_unread(&app), 2, "a later informed count replaces the known one");
        assert_eq!(store_row(&app).unread, 2);
    }

    /// The real backend pairs every incoming message with an
    /// arrival chat upsert that reports unread 0 (the wire
    /// carries no count there). Those upserts must not reset
    /// the badge: a burst accumulates the full count on top of
    /// the count the chat already had, in the mirror and in the
    /// archive row, and a ChatReadChanged afterwards still
    /// overrides the accumulated count outright, in both
    /// directions.
    #[test]
    pub(super) fn arrival_upserts_do_not_reset_the_unread_count() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        // Initial load: an informed upsert reports 2 unread.
        tx.send(counted_upsert(2)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_unread(&app), 2);
        assert_eq!(store_row(&app).unread, 2);

        // Three arrivals, each preceded by its arrival upsert,
        // exactly the real backend's pairing.
        for (pid, ts) in [("b1", BASE_TS), ("b2", BASE_TS + 60), ("b3", BASE_TS + 120)] {
            tx.send(arrival_upsert()).unwrap();
            tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
                pid,
                CHAT_JID,
                Some("Sender".to_owned()),
                "burst",
                ts,
                false,
            )))
            .unwrap();
        }
        drain_all(&mut app, &ctx);
        assert_eq!(chat_unread(&app), 5, "the burst accumulates on top of the known count");
        assert_eq!(store_row(&app).unread, 5, "the archive row carries the same count");

        // The authoritative read path still wins outright.
        tx.send(BackendEvent::ChatReadChanged {
            chat_jid: CHAT_JID.to_owned(),
            read: true,
            unread_count: 0,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_unread(&app), 0, "a remote read clears the accumulated badge");
        assert_eq!(store_row(&app).unread, 0);

        tx.send(BackendEvent::ChatReadChanged {
            chat_jid: CHAT_JID.to_owned(),
            read: false,
            unread_count: 3,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_unread(&app), 3, "a remote mark-unread sets its count");
        assert_eq!(store_row(&app).unread, 3);
    }

    /// A chat deleted on the phone leaves everywhere it exists
    /// locally: the chat list, the session mirror and helper
    /// maps, and the archive rows; the open selection moves to
    /// a surviving chat, other chats are untouched, and the
    /// adoption itself sends nothing back (the presence watch
    /// following the moved selection aside).
    #[test]
    pub(super) fn server_chat_deleted_removes_the_chat_everywhere() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        tx.send(arrival_upsert_jid(OTHER_JID)).unwrap();
        let chat_id = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "d1");
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "d1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "delete me",
            BASE_TS,
            false,
        )))
        .unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = chat_id;
        app.sync_conversation();
        assert_eq!(app.conversation.len(), 1, "the doomed chat is open");

        tx.send(BackendEvent::ChatDeleted { chat_jid: CHAT_JID.to_owned() }).unwrap();
        drain_all(&mut app, &ctx);

        assert!(app.chats.iter().all(|c| c.id != chat_id), "the chat leaves the list");
        assert_eq!(app.chats.len(), 1, "the other chat stays");
        let session = app.live.as_ref().unwrap();
        assert!(!session.messages.contains_key(&chat_id), "the mirror drops its messages");
        assert!(!session.jids.contains_key(&chat_id), "and its JID mapping");
        assert!(!session.times.contains_key(&chat_id), "and its activity stamp");
        assert!(!session.flags.contains_key(&mid), "and its message flags");
        assert!(!app.protocol_ids.contains_key(&mid), "and its protocol-id entry");
        assert!(!app.unread_live.contains_key(&chat_id), "and its pending receipts");
        let store = session.store.as_ref().unwrap();
        assert!(
            store.chats().unwrap().iter().all(|c| c.jid != CHAT_JID),
            "the archive chat row is gone"
        );
        assert!(store.messages(CHAT_JID).unwrap().is_empty(), "and its archive messages");
        assert_eq!(store.chats().unwrap().len(), 1, "the other archive row stays");
        assert_eq!(app.selected_chat, jid_id(OTHER_JID), "the selection moves to a survivor");
        assert!(app.conversation.is_empty(), "the open conversation follows the selection");
        // The adoption itself sends nothing back. The only
        // commands on the channel are the presence watch
        // following the moved selection: it subscribes to the
        // survivor now open in the deleted chat's place.
        while let Ok(command) = cmd_rx.try_recv() {
            assert!(
                matches!(command, BackendCommand::SetPresenceSubscription { .. }),
                "adopting a deletion sent a command beyond the presence watch"
            );
        }
    }

    /// A chat cleared on the phone empties here too, on the
    /// clear's own terms: messages past the bound survive,
    /// starred messages survive a keep-starred clear, and the
    /// chat itself (row, settings, list position) is exactly
    /// as it was. A second, unbounded delete-starred clear
    /// takes the rest. Nothing is sent back at any point.
    #[test]
    pub(super) fn server_chat_cleared_empties_messages_but_keeps_the_chat() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);
        // Settings the clear must not disturb, set the way the
        // server reports them.
        tx.send(BackendEvent::PinChanged { chat_jid: CHAT_JID.to_owned(), pinned: true }).unwrap();
        tx.send(BackendEvent::MuteChanged {
            chat_jid: CHAT_JID.to_owned(),
            muted: true,
            mute_until: 0,
        })
        .unwrap();
        for (pid, body, ts) in [
            ("c1", "first", BASE_TS),
            ("c2", "starred", BASE_TS + 60),
            ("c3", "newest", BASE_TS + 120),
        ] {
            tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
                pid,
                CHAT_JID,
                Some("Sender".to_owned()),
                body,
                ts,
                false,
            )))
            .unwrap();
        }
        tx.send(BackendEvent::StarChanged {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "c2".to_owned(),
            starred: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.selected_chat = chat_id;
        app.sync_conversation();
        assert_eq!(app.conversation.len(), 3, "the chat is open with all three messages");
        // One settle frame: opening the conversation starts
        // its presence watch (a subscription command), which
        // belongs to the opening, not to the server events
        // measured below.
        app.poll_backend(&ctx);
        while cmd_rx.try_recv().is_ok() {}

        // Keep-starred clear bounded at the starred message:
        // the plain message at the bound goes, the starred one
        // at the bound stays, the newer one is out of reach.
        tx.send(BackendEvent::ChatCleared {
            chat_jid: CHAT_JID.to_owned(),
            delete_starred: false,
            last_message_timestamp: Some(BASE_TS + 60),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_bodies(&app, chat_id), ["starred", "newest"]);
        assert_eq!(
            archived_bodies(&app),
            ["starred", "newest"],
            "the archive clears with the mirror"
        );
        assert_eq!(
            app.conversation.len(),
            2,
            "the open conversation rebuilds without the cleared message"
        );
        assert!(
            !app.protocol_ids.contains_key(&message_id(CHAT_JID, "c1")),
            "the cleared message's helper entries go with it"
        );
        let chat = app.chats.iter().find(|c| c.id == chat_id).expect("the chat stays in the list");
        assert!(chat.pinned && chat.muted, "its settings are untouched");
        assert_eq!(chat.preview, "newest", "the preview follows the newest survivor");
        let row = store_row(&app);
        assert!(row.pinned && row.muted, "the archive row keeps its settings");
        assert_eq!(row.preview, "newest", "and its preview follows too");
        assert_eq!(
            row.timestamp,
            BASE_TS + 120,
            "its list position input is the activity stamp the session already held"
        );
        assert!(cmd_rx.try_recv().is_err(), "adopting a clear sends nothing back");

        // An unbounded delete-starred clear takes the rest;
        // the chat still stays.
        tx.send(BackendEvent::ChatCleared {
            chat_jid: CHAT_JID.to_owned(),
            delete_starred: true,
            last_message_timestamp: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(mirror_bodies(&app, chat_id).is_empty(), "the mirror empties");
        assert!(archived_bodies(&app).is_empty(), "and so does the archive");
        assert!(app.conversation.is_empty(), "the open conversation empties with it");
        let chat = app.chats.iter().find(|c| c.id == chat_id).expect("the chat still stays");
        assert_eq!(chat.preview, "", "with nothing left, the preview empties");
        assert!(chat.pinned && chat.muted, "and its settings still survive");
        assert!(cmd_rx.try_recv().is_err(), "adopting a clear sends nothing back");
    }

    /// A message deleted for the account on the phone is
    /// removed here too: exactly that message, from the
    /// mirror, its helper entries, and the archive; its
    /// neighbours and the chat are untouched, the open
    /// conversation rebuilds, and deleting the newest message
    /// moves the preview to the new newest. Nothing is sent
    /// back.
    #[test]
    pub(super) fn server_message_deleted_for_me_removes_exactly_that_message() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);
        for (pid, body, ts) in
            [("m1", "one", BASE_TS), ("m2", "two", BASE_TS + 60), ("m3", "three", BASE_TS + 120)]
        {
            tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
                pid,
                CHAT_JID,
                Some("Sender".to_owned()),
                body,
                ts,
                false,
            )))
            .unwrap();
        }
        drain_all(&mut app, &ctx);
        app.selected_chat = chat_id;
        app.sync_conversation();
        assert_eq!(app.conversation.len(), 3);
        // One settle frame: opening the conversation starts
        // its presence watch (a subscription command), which
        // belongs to the opening, not to the server events
        // measured below.
        app.poll_backend(&ctx);
        while cmd_rx.try_recv().is_ok() {}

        // The middle message goes; its neighbours stay.
        tx.send(BackendEvent::MessageDeletedForMe {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "m2".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_bodies(&app, chat_id), ["one", "three"]);
        assert_eq!(archived_bodies(&app), ["one", "three"], "the archive removes it too");
        assert_eq!(app.conversation.len(), 2, "the open conversation rebuilds without it");
        assert!(
            !app.protocol_ids.contains_key(&message_id(CHAT_JID, "m2")),
            "its helper entries go with it"
        );
        assert_eq!(
            app.chats.iter().find(|c| c.id == chat_id).unwrap().preview,
            "three",
            "the preview still shows the newest message"
        );
        assert!(cmd_rx.try_recv().is_err(), "adopting a delete-for-me sends nothing back");

        // Deleting the newest moves the preview back.
        tx.send(BackendEvent::MessageDeletedForMe {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "m3".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_bodies(&app, chat_id), ["one"]);
        assert_eq!(
            app.chats.iter().find(|c| c.id == chat_id).unwrap().preview,
            "one",
            "the preview follows the new newest message"
        );
        assert_eq!(store_row(&app).preview, "one", "and the archive row follows");

        // A delete for a message that is not here changes
        // nothing and does not panic.
        tx.send(BackendEvent::MessageDeletedForMe {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "nope".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_bodies(&app, chat_id), ["one"]);
        assert_eq!(archived_bodies(&app), ["one"]);
        assert!(cmd_rx.try_recv().is_err());
    }

    /// The other half of the merge rule: an upsert that does
    /// carry the state (the fake's chat-action and LeaveGroup
    /// echoes) still lands, so demo-mode flows that report
    /// archive/pin through upserts keep working (logic review
    /// F14).
    #[test]
    pub(super) fn informed_upsert_still_lands() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);

        tx.send(state_upsert(true, true)).unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_archived(&app), "an informed upsert archives the mirror");
        assert!(chat_pinned(&app), "an informed upsert pins the mirror");
        assert!(archived_archived(&app), "and the archive row follows");
        assert!(archived_pinned(&app));
    }
}
