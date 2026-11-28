//! The inbound path: draining backend events each frame and
//! applying them to the mirror (chat upserts, renames, call
//! recording), plus conversation re-sync.

use super::*;

/// One archived message row for the reported-messages list
/// (row 65): protocol id, sender address, sender display
/// name, own-message flag, and body.
type ReportedArchiveRow = (String, Option<String>, Option<String>, bool, String);

/// Splits one receipt's protocol IDs into the chunk this frame
/// applies and the remainder (qc1644): a single receipt can name
/// thousands of messages, and applying them all inside one event
/// call ignored the frame budget entirely. The caller re-queues
/// the remainder at the front of the event queue as a
/// continuation of the same receipt, so the drain's budget check
/// runs between chunks and no newer event overtakes the rest of
/// the receipt. Both parts keep the original order.
fn split_receipt_ids(message_ids: Vec<String>, cap: usize) -> (Vec<String>, Vec<String>) {
    if message_ids.len() > cap {
        let mut ids = message_ids;
        let rest = ids.split_off(cap);
        (ids, rest)
    } else {
        (message_ids, Vec::new())
    }
}

/// Groups one report's form map into persons: canonical
/// key -> the entry's forms (the canonical JID itself and
/// each alternate form the report paired with it), sorted.
/// The flat map already carries the grouping (every form of
/// an entry maps to the entry's canonical key); this
/// recovers it, because the comparisons the report pair
/// supports are between entries, not between individual
/// forms: a form that changed person without ever being
/// absent is invisible in the flat maps and plain in the
/// groupings (in the old report the form sat in its owner's
/// entry; in the new one it sits in a different entry, while
/// the owner's stable form sits in the owner's new entry).
fn forms_by_person(
    forms: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut people: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (form, canonical) in forms {
        people.entry(canonical.clone()).or_default().push(form.clone());
    }
    for entry_forms in people.values_mut() {
        entry_forms.sort();
    }
    people
}

/// Where one person's old entry stands in the new report,
/// as the canonical key of their entry there, when the
/// evidence settles it. The entry holding the person's LID
/// form settles it on its own: a LID is never recycled, so
/// the entry that holds it is the person's continuation.
/// With no LID form in play, the surviving forms settle it
/// when they agree on one canonical key, discounting a
/// surviving form whose new entry carries a LID form the
/// old entry did not have (that form's holder is identified
/// as a different person, so it cannot speak for where this
/// person stands). Contradictory or absent evidence
/// settles nothing.
fn person_continuation(
    entry_forms: &[String],
    new_forms: &std::collections::HashMap<String, String>,
    new_people: &std::collections::HashMap<String, Vec<String>>,
) -> Option<String> {
    if let Some(target) = entry_forms
        .iter()
        .filter(|form| form.ends_with("@lid"))
        .find_map(|form| new_forms.get(form))
    {
        return Some(target.clone());
    }
    let mut targets: Vec<String> = Vec::new();
    for form in entry_forms {
        let Some(owner) = new_forms.get(form) else {
            continue;
        };
        let holder_is_someone_else = new_people.get(owner).is_some_and(|holder_forms| {
            holder_forms.iter().any(|held| held.ends_with("@lid") && !entry_forms.contains(held))
        });
        if !holder_is_someone_else && !targets.contains(owner) {
            targets.push(owner.clone());
        }
    }
    if targets.len() == 1 {
        targets.into_iter().next()
    } else {
        None
    }
}

/// The form retirements one saved-contacts report works
/// over the previous one, as (retired form, canonical key
/// the person stands under now) pairs, learned at entry
/// granularity: each form of each old entry is read against
/// where its person's entry stands in the new report (see
/// [`person_continuation`]). A form absent from the new
/// report has retired when its person stands somewhere in
/// it, and follows them there. A form present in the new
/// report under a different person's entry than its
/// person's continuation has changed hands inside the one
/// step between the reports (the number was recycled to a
/// new saved contact in the same step that moved its old
/// owner), and it follows its old owner exactly as an
/// absent form does: the stored entries naming it were
/// written for the old owner. A form whose person stands
/// nowhere in the new report has not retired but vanished,
/// and nothing follows it: there is no one for a stored
/// entry naming it to follow to.
fn retired_follows(
    old_forms: &std::collections::HashMap<String, String>,
    new_forms: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    let new_people = forms_by_person(new_forms);
    let mut follows = Vec::new();
    for entry_forms in forms_by_person(old_forms).into_values() {
        let continuation = person_continuation(&entry_forms, new_forms, &new_people);
        for form in &entry_forms {
            match new_forms.get(form) {
                None => {
                    if let Some(target) = &continuation {
                        follows.push((form.clone(), target.clone()));
                    }
                }
                Some(owner) => {
                    let Some(target) = &continuation else {
                        continue;
                    };
                    if target != owner {
                        follows.push((form.clone(), target.clone()));
                    }
                }
            }
        }
    }
    follows
}

/// The forms one saved-contacts report proves changed
/// hands from an owner who left the saved set, as bare form
/// strings. The old entry carried a LID form and no LID
/// form of it survives in the new report (a LID is never
/// recycled, so its absence is the owner leaving, not
/// moving), while the form itself stands in the new report
/// under an entry that carries its own LID form, a
/// different identified person. No follow can be learned
/// for such a form (its owner stands nowhere in the new
/// report), so a stored entry naming it names nobody now:
/// the heals drop it rather than let it resolve through
/// the current form map to the form's new holder. A form
/// whose new holder carries no LID form is not provable
/// either way (the holder may be the same person whose
/// pairing the book has not relearned), so it is left
/// alone: the evidence boundary of this learning.
fn orphaned_forms(
    old_forms: &std::collections::HashMap<String, String>,
    new_forms: &std::collections::HashMap<String, String>,
) -> Vec<String> {
    let new_people = forms_by_person(new_forms);
    let mut orphans = Vec::new();
    for entry_forms in forms_by_person(old_forms).into_values() {
        let carried_lid = entry_forms.iter().any(|form| form.ends_with("@lid"));
        let lid_survives = entry_forms
            .iter()
            .filter(|form| form.ends_with("@lid"))
            .any(|form| new_forms.contains_key(form));
        if !carried_lid || lid_survives {
            continue;
        }
        for form in &entry_forms {
            let Some(owner) = new_forms.get(form) else {
                continue;
            };
            let holder_identified = new_people
                .get(owner)
                .is_some_and(|holder_forms| holder_forms.iter().any(|held| held.ends_with("@lid")));
            if holder_identified {
                orphans.push(form.clone());
            }
        }
    }
    orphans
}

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
        // The deferred startup archive load settles first
        // (qc1636 Step 7, H7): while it is in flight, the top-up
        // below still drains the backend channel into the queue
        // (so the backend never blocks on a full channel), but
        // the application block holds. The snapshot must land
        // on an untouched mirror, and no event may write to the
        // archive while its store is away with the load worker.
        self.poll_archive_load();
        // The sync row's quiet-window fallback: the library's
        // completion event covers the offline drain, and a
        // catch-up can end without one (a reconnect with an
        // empty backlog produces no end marker), which used to
        // latch the row forever. After a long quiet the row
        // retires, but the session does not count as synced:
        // only a real completion latches that.
        let mut retire_sync_row = false;
        if let Some(session) = &mut self.live {
            if session.syncing {
                // Sync honesty (QC1614-A0-5): the quiet fallback
                // may only retire the visual row after a long
                // quiet (real inter-chunk gaps run minutes on a
                // populated roster). It never latches
                // synced_once: only a real completion event may
                // do that. The bound is unconditional (qc1654):
                // activity is stamped only by sync-bearing
                // events, so 180 s past the last one the episode
                // is over by definition, and letting an unrelated
                // event backlog or a pending batch flush hold
                // the row made the bound defeatable by the very
                // traffic a sync generates.
                let quiet = session
                    .sync_last_activity
                    .is_some_and(|last| last.elapsed() > std::time::Duration::from_secs(180));
                if quiet {
                    session.syncing = false;
                    session.sync_last_activity = None;
                    // The fallback is not a completion: no 100
                    // and no completion hold may survive it, and
                    // a stale stamp must not clamp the next
                    // episode's reports.
                    session.sync_percent = None;
                    session.sync_completed_at = None;
                    session.sync_finishing = false;
                    retire_sync_row = true;
                }
            }
        }
        if retire_sync_row {
            // The sync sort cadence may have skipped the chat
            // list's sort during the episode (qc1644): with the
            // episode over, the list settles into the same final
            // order an every-flush sort would have produced.
            self.sort_live_chats_now();
            if let Some(session) = &mut self.live {
                session.rebuild_chat_positions(&self.chats);
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
        if self.archive_load.is_none()
            && (!self.event_queue.is_empty() || self.event_batch.flush_pending)
        {
            // An unfinished flush from an earlier frame resumes
            // before any newer event is applied (qc1644): its
            // side effects belong to the older batch, and letting
            // a newer batch's writes land first would reorder
            // them. While the flush is still pending the queue
            // waits, however much budget is left.
            if self.event_batch.flush_pending {
                self.finish_event_batch();
            }
            if !self.event_batch.flush_pending && !self.event_queue.is_empty() {
                // The drain's budget clock starts here, after
                // the resumed flush: the flush bills its own
                // budget (see finish_event_batch), and charging
                // its time to the drain as well would shrink
                // every batch that follows a deferred flush to
                // a handful of events, multiplying the per-batch
                // fixed costs (transactions, sorts, rebuilds)
                // the batching exists to amortize.
                let started = std::time::Instant::now();
                let mut applied = 0usize;
                self.event_batch.active = true;
                while let Some(event) = self.event_queue.pop_front() {
                    self.trace_event(&event);
                    // An interruption ends the finishing phase
                    // with the episode it belonged to, so the
                    // interruption event itself must not re-enter
                    // it through the note below.
                    let interruption = matches!(event, BackendEvent::SyncInterrupted);
                    self.apply_event(ctx, event);
                    // Every event applied while the reported
                    // progress sits at its ceiling is finishing
                    // work (see `note_sync_work_applied`): the
                    // sync tail is receipts, messages and
                    // history chunks, not only the app-state
                    // patches that used to be the sole trigger,
                    // so the note belongs here at the drain, the
                    // one point every applied event passes.
                    if !interruption {
                        if let Some(session) = &mut self.live {
                            session.note_sync_work_applied();
                        }
                    }
                    applied += 1;
                    // At least one event always applies, so a single
                    // slow event can never starve the queue.
                    if applied >= MAX_EVENTS_PER_FRAME || started.elapsed() >= EVENT_FRAME_BUDGET {
                        break;
                    }
                }
                self.finish_event_batch();
            }
            if !self.event_queue.is_empty() || self.event_batch.flush_pending {
                ctx.request_repaint();
            }
        }
        // A closed channel after a logout means the session is
        // over: drop it so the app returns to a start-fresh
        // connect state instead of holding a dead session it can
        // never re-link from. A closure with no logout (the
        // supervisor gave up after repeated crashes) keeps the
        // session and its failure state on screen. A flush still
        // pending holds unapplied side effects of the session's
        // last events, so the teardown waits for it too.
        if backend_gone && self.event_queue.is_empty() && !self.event_batch.flush_pending {
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

    /// Notes one app-state patch event (an archive, pin, mute,
    /// star or read-state change) landing, for the sync row's
    /// finishing phase (see [`LiveSession::sync_finishing`]):
    /// the session method holds the rule; this is the patch
    /// arms' one shared call.
    fn note_sync_finishing_patch(&mut self) {
        if let Some(session) = &mut self.live {
            session.note_app_state_patch();
        }
    }

    /// Runs the side effects one event batch deferred (see
    /// [`EventBatch`]): sort each touched chat's messages once,
    /// flush the batch's archive writes in a single transaction,
    /// sort the chat list once, and rebuild the open conversation
    /// once. Runs before anything renders, so the state the user
    /// sees after a burst is exactly the state the old per-event
    /// application produced.
    ///
    /// The flush itself is budgeted (qc1644): it used to run
    /// unconditionally after the budgeted drain, so one frame
    /// could still pay every sort and transaction a burst had
    /// queued. The steps now run as a resumable sequence (see
    /// [`FlushStep`]) under the same [`EVENT_FRAME_BUDGET`] as
    /// event application, checked between work units: one chat's
    /// sort, one store transaction group, one row of a per-row
    /// step. When the budget is spent the flush stops where it
    /// is and stays pending in the batch; the next frame's drain
    /// resumes it before applying any newer event, so a later
    /// batch never overtakes an unfinished flush and every side
    /// effect lands in the old order, just spread over more
    /// frames. At least one work unit always runs per call, so a
    /// single expensive unit can never starve the flush.
    pub(super) fn finish_event_batch(&mut self) {
        if !self.event_batch.active && !self.event_batch.flush_pending {
            return;
        }
        let started = std::time::Instant::now();
        let mut units = 0usize;
        // The budget gate between work units: the first unit of
        // a call always runs (the starvation rule above), later
        // units run only while the frame budget lasts.
        macro_rules! budget_spent {
            () => {
                units > 0 && started.elapsed() >= EVENT_FRAME_BUDGET
            };
        }
        // Stops this call mid-flush: the batch keeps its cursor
        // and its remaining queues, marked pending so the next
        // drain resumes it before any newer event.
        macro_rules! defer_flush {
            () => {{
                self.event_batch.flush_pending = true;
                return;
            }};
        }
        loop {
            match self.event_batch.flush_step {
                FlushStep::SortMessages => {
                    while let Some(chat_id) = self.event_batch.unsorted_chats.iter().next().copied()
                    {
                        if budget_spent!() {
                            defer_flush!();
                        }
                        self.event_batch.unsorted_chats.remove(&chat_id);
                        // The trim records the drained rows'
                        // flag words into the batch's snapshot:
                        // their archive writes are still queued
                        // for this flush's write step, which
                        // resolves a drained row's flags from the
                        // snapshot, not from the flag map the
                        // trim empties (qc1655 H-1). The map is
                        // taken out for the length of the session
                        // borrow and put back after.
                        let mut trimmed_flags = std::mem::take(&mut self.event_batch.trimmed_flags);
                        if let Some(session) = &mut self.live {
                            if let Some(entry) = session.messages.get_mut(&chat_id) {
                                entry.sort_by_key(|(ts, _)| *ts);
                            }
                            // The vec is sorted and final for this
                            // batch: (re)build its position index
                            // so the next upserts find their rows
                            // in O(1) (qc1634 H5).
                            session.rebuild_msg_index(chat_id);
                            // The batch may have grown the vec
                            // past the retention window (qc1644):
                            // trim here, after this step's store
                            // writes are already queued from the
                            // untrimmed rows, so the archive keeps
                            // every message the mirror drops.
                            session.trim_chat_window(chat_id, Some(&mut trimmed_flags));
                        }
                        self.event_batch.trimmed_flags = trimmed_flags;
                        units += 1;
                    }
                    self.event_batch.flush_step = FlushStep::ChatUpserts;
                }
                FlushStep::ChatUpserts => {
                    if !self.event_batch.chat_upserts.is_empty() {
                        if budget_spent!() {
                            defer_flush!();
                        }
                        let queued = std::mem::take(&mut self.event_batch.chat_upserts);
                        if let Some(session) = &mut self.live {
                            if session.store.is_some() {
                                // Chat rows first, before the batch's
                                // message upserts, preserving the
                                // order the old immediate writes
                                // produced. Repeated upserts of one
                                // chat collapse to the last queued
                                // row.
                                let mut latest: HashMap<String, LiveChat> = HashMap::new();
                                let mut order: Vec<String> = Vec::new();
                                for chat in queued {
                                    if !latest.contains_key(&chat.jid) {
                                        order.push(chat.jid.clone());
                                    }
                                    latest.insert(chat.jid.clone(), chat);
                                }
                                let mut rows: Vec<LiveChat> = order
                                    .into_iter()
                                    .filter_map(|jid| latest.remove(&jid))
                                    .collect();
                                // A rename that landed later in this
                                // batch (a heal, a confirmed group
                                // subject) wrote the store directly
                                // and updated the mirror, but the
                                // queued copy still carries the
                                // older title: resolve titles from
                                // the mirror at flush time, exactly
                                // as the message flags below are
                                // resolved, so the archive row ends
                                // where event order says it should.
                                // The mirror lookup goes through the
                                // session's chat position map
                                // instead of a scan per row (qc1644).
                                // It runs before the store borrow
                                // below: the lookup repairs the map
                                // and needs the session mutably.
                                for row in &mut rows {
                                    let chat_id = jid_id(&row.jid);
                                    if let Some(pos) = session.chat_position(&self.chats, chat_id) {
                                        row.title = self.chats[pos].title.clone();
                                    }
                                }
                                if let Some(store) = &session.store {
                                    let _ = store.upsert_chats(&rows);
                                }
                            }
                        }
                        units += 1;
                    }
                    self.event_batch.flush_step = FlushStep::Favorites;
                }
                FlushStep::Favorites => {
                    // After the chat upserts above: the favorite
                    // writer UPDATEs the chat row, so its row must
                    // exist first (a first link can deliver the
                    // roster and the favorites in one batch). One
                    // store call per row, so the budget is checked
                    // per row and the rest stays queued.
                    let writes = std::mem::take(&mut self.event_batch.favorite_writes);
                    let mut rest = writes.into_iter();
                    let mut stopped = false;
                    while !rest.as_slice().is_empty() {
                        if budget_spent!() {
                            stopped = true;
                            break;
                        }
                        let (jid, favorite) = rest.next().expect("the queue was checked");
                        if let Some(session) = &self.live {
                            if let Some(store) = &session.store {
                                let _ = store.set_chat_favorite(&jid, favorite);
                            }
                        }
                        units += 1;
                    }
                    self.event_batch.favorite_writes = rest.collect();
                    if stopped {
                        defer_flush!();
                    }
                    self.event_batch.flush_step = FlushStep::StoreWrites;
                }
                FlushStep::StoreWrites => {
                    if !self.event_batch.store_writes.is_empty() {
                        if budget_spent!() {
                            defer_flush!();
                        }
                        let queued = std::mem::take(&mut self.event_batch.store_writes);
                        if let Some(session) = &mut self.live {
                            if let Some(store) = &session.store {
                                // Repeated upserts of one message
                                // inside the batch collapse to the
                                // newest copy. Flags are read from
                                // the flag map as it stands now, so
                                // a star or revoke applied later in
                                // the batch is part of the stored
                                // row, with one exception: a message
                                // this flush's trim already drained
                                // has no map entry left, so its word
                                // comes from the trim's snapshot,
                                // taken at drain time when the map
                                // still held every in-batch change
                                // (qc1655 H-1). The word is resolved
                                // once per message id and shared by
                                // every queued copy of it: the
                                // snapshot is consumed by the
                                // resolution, so a second copy
                                // resolving on its own would fall
                                // through to the emptied map and
                                // store 0 over the first copy's
                                // word (qc1656 MEDIUM-1). Delivery advances
                                // the batch applied after an upsert
                                // (a receipt in the same burst) live
                                // in the mirror, not in the upsert
                                // copy the batch recorded; overlay
                                // them so the flush cannot write a
                                // stale tick state over the receipt's
                                // own store write. The overlay is
                                // recorded incrementally where each
                                // advance happens, so the flush no
                                // longer rescans the whole mirror to
                                // rebuild it (qc1634 H3).
                                let mut latest: HashMap<u64, (LiveMessage, i64)> = HashMap::new();
                                let mut order: Vec<u64> = Vec::new();
                                let mut resolved_flags: HashMap<u64, i64> = HashMap::new();
                                for mut msg in queued {
                                    let mid = message_id(&msg.chat_jid, &msg.id);
                                    if !latest.contains_key(&mid) {
                                        order.push(mid);
                                    }
                                    if let Some(d) = self.event_batch.delivery_overlay.get(&mid) {
                                        msg.delivery =
                                            backend::DeliveryState::max_opt(msg.delivery, Some(*d));
                                    }
                                    let flags = match resolved_flags.get(&mid) {
                                        Some(word) => *word,
                                        None => {
                                            let word =
                                                match self.event_batch.trimmed_flags.remove(&mid) {
                                                    Some(word) => word,
                                                    None => session
                                                        .flags
                                                        .get(&mid)
                                                        .copied()
                                                        .unwrap_or(0),
                                                };
                                            resolved_flags.insert(mid, word);
                                            word
                                        }
                                    };
                                    latest.insert(mid, (msg, flags));
                                }
                                let rows: Vec<(LiveMessage, i64)> = order
                                    .into_iter()
                                    .filter_map(|mid| latest.remove(&mid))
                                    .collect();
                                if let Err(error) = store.upsert_messages(&rows) {
                                    self.surface_archive_write_failure(&error);
                                }
                            }
                        }
                        units += 1;
                    }
                    self.event_batch.flush_step = FlushStep::DeliveryWrites;
                }
                FlushStep::DeliveryWrites => {
                    if !self.event_batch.delivery_writes.is_empty() {
                        if budget_spent!() {
                            defer_flush!();
                        }
                        let queued = std::mem::take(&mut self.event_batch.delivery_writes);
                        if let Some(session) = &self.live {
                            if let Some(store) = &session.store {
                                // After the upserts, so a receipt for
                                // a message this same batch filed
                                // lands on the row the flush just
                                // wrote. Repeated advances of one
                                // message collapse to the strongest
                                // state (the store write is monotonic
                                // anyway; collapsing just skips the
                                // dead reads).
                                let mut latest: HashMap<(String, String), backend::DeliveryState> =
                                    HashMap::new();
                                let mut order: Vec<(String, String)> = Vec::new();
                                for (chat_jid, pid, state) in queued {
                                    let key = (chat_jid, pid);
                                    if !latest.contains_key(&key) {
                                        order.push(key.clone());
                                    }
                                    let entry = latest.entry(key).or_insert(state);
                                    *entry =
                                        backend::DeliveryState::max_opt(Some(*entry), Some(state))
                                            .unwrap_or(state);
                                }
                                let rows: Vec<(String, String, backend::DeliveryState)> = order
                                    .into_iter()
                                    .filter_map(|key| {
                                        latest.remove(&key).map(|state| (key.0, key.1, state))
                                    })
                                    .collect();
                                let _ = store.set_message_deliveries(&rows);
                            }
                        }
                        units += 1;
                    }
                    self.event_batch.flush_step = FlushStep::PollTallies;
                }
                FlushStep::PollTallies => {
                    // After the upserts, per the batch contract: a
                    // tally for a poll created in this same batch
                    // lands on the row the flush just wrote. One
                    // store call per tally, budgeted per tally.
                    let writes = std::mem::take(&mut self.event_batch.poll_tally_writes);
                    let mut rest = writes.into_iter();
                    let mut stopped = false;
                    while !rest.as_slice().is_empty() {
                        if budget_spent!() {
                            stopped = true;
                            break;
                        }
                        let tally = rest.next().expect("the queue was checked");
                        if let Some(session) = &self.live {
                            if let Some(store) = &session.store {
                                let _ = store.set_poll_tally(
                                    &tally.chat_jid,
                                    &tally.message_id,
                                    &tally.counts,
                                    &tally.my_votes,
                                    &tally.ballots,
                                );
                            }
                        }
                        units += 1;
                    }
                    self.event_batch.poll_tally_writes = rest.collect();
                    if stopped {
                        defer_flush!();
                    }
                    self.event_batch.flush_step = FlushStep::EventTallies;
                }
                FlushStep::EventTallies => {
                    // Same contract as the poll tallies above:
                    // after the upserts, last snapshot wins.
                    let writes = std::mem::take(&mut self.event_batch.event_tally_writes);
                    let mut rest = writes.into_iter();
                    let mut stopped = false;
                    while !rest.as_slice().is_empty() {
                        if budget_spent!() {
                            stopped = true;
                            break;
                        }
                        let tally = rest.next().expect("the queue was checked");
                        if let Some(session) = &self.live {
                            if let Some(store) = &session.store {
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
                        units += 1;
                    }
                    self.event_batch.event_tally_writes = rest.collect();
                    if stopped {
                        defer_flush!();
                    }
                    self.event_batch.flush_step = FlushStep::ChatRows;
                }
                FlushStep::ChatRows => {
                    if !self.event_batch.chat_rows_dirty.is_empty() {
                        if budget_spent!() {
                            defer_flush!();
                        }
                        let dirty = std::mem::take(&mut self.event_batch.chat_rows_dirty);
                        if let Some(session) = &mut self.live {
                            if session.store.is_some() {
                                // Written from the mirror as it
                                // stands now, so the stored count is
                                // the badge the list shows after the
                                // whole batch, whatever order the
                                // increments and read changes arrived
                                // in. A chat the batch went on to
                                // delete is no longer in the list and
                                // is skipped: its rows are already
                                // gone. One transaction for the whole
                                // set (qc1634 H2). The mirror lookup
                                // goes through the session's chat
                                // position map instead of a scan per
                                // row (qc1644); it runs before the
                                // store borrow below because the
                                // lookup repairs the map and needs
                                // the session mutably.
                                let mut rows: Vec<LiveChat> = Vec::new();
                                for chat_id in &dirty {
                                    let Some(pos) = session.chat_position(&self.chats, *chat_id)
                                    else {
                                        continue;
                                    };
                                    let Some(jid) = session.jids.get(chat_id) else {
                                        continue;
                                    };
                                    let ts = session.times.get(chat_id).copied().unwrap_or(0);
                                    // The batch flush is a
                                    // write-through like any other:
                                    // the row carries the
                                    // community parentage the
                                    // session knows (qc1654
                                    // Claim F), resolved here
                                    // because the flush holds the
                                    // session mutably.
                                    let mut row = chat_to_live(&self.chats[pos], jid, ts);
                                    if let Some(parent_id) = self.chat_parents.get(chat_id).copied()
                                    {
                                        row.is_parent = parent_id == *chat_id;
                                        row.parent_jid = session.jids.get(&parent_id).cloned();
                                    }
                                    rows.push(row);
                                }
                                if !rows.is_empty() {
                                    if let Some(store) = &session.store {
                                        let _ = store.upsert_chats(&rows);
                                    }
                                }
                            }
                        }
                        units += 1;
                    }
                    self.event_batch.flush_step = FlushStep::DeferredWrites;
                }
                FlushStep::DeferredWrites => {
                    // Last, after every upsert above: these UPDATE
                    // rows the batch may have only just created, in
                    // the order their events arrived. One store
                    // call per write, budgeted per write.
                    //
                    // The sender renames among them land as ONE
                    // batched call first (qc1644): the old flush
                    // paid one store transaction per naming, and a
                    // sync burst resolves hundreds. A rename touches
                    // only sender labels and chat preview senders,
                    // never the fields the other deferred writes
                    // own, so collecting the renames ahead of the
                    // rest (which keep their event order) changes
                    // no outcome; repeated renames of one sender
                    // keep their order inside the batch call.
                    if self
                        .event_batch
                        .deferred_writes
                        .iter()
                        .any(|w| matches!(w, DeferredStoreWrite::RenameSender { .. }))
                    {
                        if budget_spent!() {
                            defer_flush!();
                        }
                        let mut renames: Vec<(String, String)> = Vec::new();
                        let mut others: Vec<DeferredStoreWrite> = Vec::new();
                        for write in std::mem::take(&mut self.event_batch.deferred_writes) {
                            match write {
                                DeferredStoreWrite::RenameSender { sender_jid, name } => {
                                    renames.push((sender_jid, name));
                                }
                                other => others.push(other),
                            }
                        }
                        self.event_batch.deferred_writes = others;
                        if let Some(session) = &self.live {
                            if let Some(store) = &session.store {
                                let _ = store.rename_senders(&renames);
                            }
                        }
                        units += 1;
                    }
                    let writes = std::mem::take(&mut self.event_batch.deferred_writes);
                    let mut rest = writes.into_iter();
                    let mut stopped = false;
                    while !rest.as_slice().is_empty() {
                        if budget_spent!() {
                            stopped = true;
                            break;
                        }
                        let write = rest.next().expect("the queue was checked");
                        if let Some(session) = &self.live {
                            if let Some(store) = &session.store {
                                match write {
                                    DeferredStoreWrite::RenameSender { sender_jid, name } => {
                                        // The renames were collected
                                        // into the batch call above, so
                                        // this arm only covers a rename
                                        // queued after that collection
                                        // (a refill while the flush sat
                                        // pending). Re-queue it: the
                                        // flush's end-of-pass check
                                        // runs this step again and its
                                        // batch call applies it.
                                        self.event_batch.deferred_writes.push(
                                            DeferredStoreWrite::RenameSender { sender_jid, name },
                                        );
                                    }
                                    DeferredStoreWrite::SetEventFields {
                                        chat_jid,
                                        message_id,
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
                                        let _ = store.set_event_fields(
                                            &chat_jid,
                                            &message_id,
                                            &name,
                                            &description,
                                            start_time,
                                            end_time,
                                            &location,
                                            &join_link,
                                            scheduled_call,
                                            extra_guests,
                                            canceled,
                                        );
                                    }
                                    DeferredStoreWrite::MediaDirectPath {
                                        chat_jid,
                                        message_id,
                                        direct_path,
                                    } => {
                                        let _ = store.update_media_direct_path(
                                            &chat_jid,
                                            &message_id,
                                            &direct_path,
                                        );
                                    }
                                    DeferredStoreWrite::VideoThumbnail {
                                        chat_jid,
                                        message_id,
                                        bytes,
                                    } => {
                                        let _ = store.set_video_thumbnail(
                                            &chat_jid,
                                            &message_id,
                                            &bytes,
                                        );
                                    }
                                }
                            }
                        }
                        units += 1;
                    }
                    // Renames re-queued by the defensive arm above
                    // stay ahead of the unprocessed rest, matching
                    // this step's renames-first order.
                    let mut remaining = std::mem::take(&mut self.event_batch.deferred_writes);
                    remaining.extend(rest);
                    self.event_batch.deferred_writes = remaining;
                    if stopped {
                        defer_flush!();
                    }
                    self.event_batch.flush_step = FlushStep::SortChats;
                }
                FlushStep::SortChats => {
                    if self.event_batch.chats_dirty || self.event_batch.force_chat_sort {
                        if budget_spent!() {
                            defer_flush!();
                        }
                        // During a sync episode the full sort runs
                        // on a cadence instead of every flush
                        // (qc1644, see `chat_sort_cadence`); a
                        // completion in this batch forces it, and
                        // outside an episode every flush sorts as
                        // before.
                        let syncing = self.live.as_ref().is_some_and(|s| s.syncing);
                        let (run, next) = chat_sort_cadence(
                            syncing,
                            self.event_batch.force_chat_sort,
                            self.event_batch.flushes_since_sort,
                        );
                        self.event_batch.flushes_since_sort = next;
                        if run {
                            self.sort_live_chats_now();
                            // The sort just rewrote every position
                            // in the list: rebuild the position map
                            // from the final order (qc1644).
                            if let Some(session) = &mut self.live {
                                session.rebuild_chat_positions(&self.chats);
                            }
                        }
                        self.event_batch.chats_dirty = false;
                        self.event_batch.force_chat_sort = false;
                        units += 1;
                    }
                    self.event_batch.flush_step = FlushStep::Conversation;
                }
                FlushStep::Conversation => {
                    if self.event_batch.conversation_dirty {
                        if budget_spent!() {
                            defer_flush!();
                        }
                        self.sync_conversation_now();
                        self.event_batch.conversation_dirty = false;
                        units += 1;
                    }
                    self.event_batch.flush_step = FlushStep::Done;
                }
                FlushStep::Done => {
                    // While the flush sat pending between frames
                    // the batch stayed active, so a side path may
                    // have queued fresh work into a step the
                    // cursor already passed. That work belongs to
                    // this flush's order, not the next batch's:
                    // run the sequence again for it instead of
                    // dropping it with the reset. The budget gate
                    // inside each step bounds the extra pass like
                    // any other.
                    if self.event_batch.has_queued_work() {
                        self.event_batch.flush_step = FlushStep::SortMessages;
                    } else {
                        break;
                    }
                }
            }
        }
        // The flush ran to completion: reset the batch for the
        // next drain. Only the sort cadence counter carries over;
        // it is bookkeeping across batches, not flush work.
        let cadence = self.event_batch.flushes_since_sort;
        self.event_batch = EventBatch::default();
        self.event_batch.flushes_since_sort = cadence;
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
                // Chats whose titles are still healable states
                // (a raw or masked identifier, or the formatted
                // phone a DM title advances to once its number
                // is known), collected before the mutable
                // session borrow: they are reported for
                // resolution below. The formatted phone must be
                // reported like the raw forms: it is a healable
                // state, not a resolution (see
                // `is_formatted_phone`), and a chat that reached
                // it in an earlier session is otherwise never
                // re-asked, so a contact name the identity book
                // has since learned could never replace the
                // number on any later launch (qc1644).
                let healable_titles: Vec<String> = match &self.live {
                    Some(session) => self
                        .chats
                        .iter()
                        .filter(|chat| {
                            is_unresolved_identifier(&chat.title) || is_formatted_phone(&chat.title)
                        })
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
                    // A (re)connect starts a new sync episode:
                    // no reported percentage and no completion
                    // hold carry over from the previous one.
                    session.sync_percent = None;
                    session.sync_completed_at = None;
                    session.sync_finishing = false;
                    session.clear_qr();
                    // A used code must never resurface on the
                    // connect screen (logic review F13).
                    session.pair_code = None;
                    session.pair_code_at = None;
                    // The healable-titled chats collected above
                    // are reported for resolution: a later launch
                    // gets no fresh history sync to heal from,
                    // so the connect itself is the trigger (the
                    // backend retries each fetch a bounded
                    // number of times).
                    if !healable_titles.is_empty() {
                        let _ = session
                            .channels
                            .commands
                            .send(BackendCommand::ResolveTitles { jids: healable_titles });
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
            BackendEvent::MessageSendFailed { chat_jid, message_id, failure } => {
                // The failure binds to the named chat's echo for
                // the named protocol ID alone (see
                // `fail_chat_sending_echo`); sends in flight to
                // other chats are untouched. A failed TEXT echo
                // also gives its body back to that chat's
                // composer draft, the one place the text still
                // exists once the composer has cleared it, but
                // only into an empty draft. The notice wording
                // is the app's own, built from the structured
                // kind (see `send_failure_message`); the backend
                // sent no text with this event.
                if let Some(failed) = self.fail_chat_sending_echo(&chat_jid, &message_id) {
                    if matches!(failed.kind, MessageKind::Text) {
                        self.restore_failed_text_draft(jid_id(&chat_jid), &failed.body);
                    }
                }
                self.notice = Some(Notice::error(send_failure_message(failure)));
            }
            BackendEvent::Receipt { chat_jid, message_ids, kind, at, actor } => {
                // An oversized receipt applies in chunks of the
                // per-frame event cap (qc1644, see
                // `split_receipt_ids`): the remainder re-queues at
                // the front of the queue as the same receipt
                // continued, so it lands before any newer event
                // and the drain's budget check runs between
                // chunks. Receipt semantics per ID are unchanged;
                // only the scheduling is split.
                let (message_ids, remainder) = split_receipt_ids(message_ids, MAX_EVENTS_PER_FRAME);
                if !remainder.is_empty() {
                    self.event_queue.push_front(BackendEvent::Receipt {
                        chat_jid: chat_jid.clone(),
                        message_ids: remainder,
                        kind,
                        at,
                        actor: actor.clone(),
                    });
                }
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
                    // The fallback bind below, when it fires:
                    // (position, old local ID) of the echo it
                    // bound, so the position index can follow the
                    // bind once the vec borrow ends (qc1642 fix 1).
                    let mut bound_echo: Option<(usize, u64)> = None;
                    if let Some(session) = &mut self.live {
                        // The receipt names one message: find it
                        // through the position index instead of
                        // scanning the chat's whole vec per ID
                        // (qc1634 H5).
                        let pos = session.message_position(chat_id, mid);
                        if let Some(entry) = session.messages.get_mut(&chat_id) {
                            if let Some(pos) = pos {
                                let (_, m) = &mut entry[pos];
                                if m.delivery.is_some() {
                                    // Receipts only move forward.
                                    let rank = |d: Delivery| match d {
                                        Delivery::Failed | Delivery::Sending => 0,
                                        Delivery::Sent => 1,
                                        Delivery::Delivered => 2,
                                        Delivery::Read => 3,
                                    };
                                    if m.delivery.is_some_and(|cur| rank(delivery) > rank(cur)) {
                                        m.delivery = Some(delivery);
                                        if self.event_batch.active {
                                            self.event_batch
                                                .record_delivery(mid, backend_delivery(delivery));
                                        }
                                    }
                                    applied = true;
                                }
                            }
                            // The echo bind below is only for a
                            // message this session has never
                            // filed: a protocol ID the session
                            // already knows (every upsert and
                            // archive seed records one) whose row
                            // is absent was filed and later left
                            // the mirror (retention trim, delete,
                            // expiry), and binding a fresh pending
                            // echo to its receipt would move the
                            // echo onto the wrong message (qc1644).
                            if !applied && !self.protocol_ids.contains_key(&mid) {
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
                                // Enumerate positions, not just
                                // rows: the index entry has to
                                // follow this bind to the protocol
                                // ID (qc1642 fix 1).
                                let echo_pos = entry
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, row)| {
                                        row.1.delivery == Some(Delivery::Sending) && row.1.is_mine()
                                    })
                                    .min_by_key(|(_, row)| row.0)
                                    .map(|(pos, _)| pos);
                                if let Some(pos) = echo_pos {
                                    let echo = &mut entry[pos].1;
                                    let old_id = echo.id;
                                    if rank(delivery) > rank(Delivery::Sending) {
                                        echo.delivery = Some(delivery);
                                    }
                                    if echo.time == "…" {
                                        echo.time = fmt_time(at);
                                    }
                                    echo.id = mid;
                                    bound_echo = Some((pos, old_id));
                                    self.protocol_ids.insert(mid, pid.clone());
                                    if self.event_batch.active {
                                        self.event_batch
                                            .record_delivery(mid, backend_delivery(delivery));
                                    }
                                }
                            }
                        }
                        if let Some((pos, old_id)) = bound_echo {
                            session.reindex_message_id(chat_id, pos, old_id, mid);
                        }
                    }
                    // A receipt is confirmation evidence too: if
                    // a Failed echo bound to this ID still sits
                    // under a synthetic ID next to the confirmed
                    // row this receipt just advanced, the two are
                    // one message and the echo collapses into it.
                    self.collapse_failed_echo(chat_id, pid, mid);
                }
                // Persist the advanced state: ticks are synced
                // state, and a restart must not lose a Read the
                // receipts already proved. Inside a batch the
                // writes queue and land in one transaction at
                // the flush instead of one commit per ID
                // (qc1634 H1).
                if let Some(session) = &self.live {
                    if session.store.is_some() {
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
                        if self.event_batch.active {
                            for pid in &message_ids {
                                self.event_batch.delivery_writes.push((
                                    chat_jid.clone(),
                                    pid.clone(),
                                    state,
                                ));
                            }
                        } else if let Some(store) = &session.store {
                            for pid in &message_ids {
                                let _ = store.set_message_delivery(&chat_jid, pid, state);
                            }
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
                    self.apply_revoke(chat_id, &chat_jid, mid, &pid);
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
                self.apply_revoke(chat_id, &jid, mid, &pid);
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
                self.note_sync_finishing_patch();
            }
            BackendEvent::PinChanged { chat_jid, pinned } => {
                let chat_id = jid_id(&chat_jid);
                self.pending_chat_actions.remove(&(chat_id, "pin"));
                self.apply_pin_changed(chat_id, pinned);
                self.note_sync_finishing_patch();
            }
            BackendEvent::ArchiveChanged { chat_jid, archived } => {
                let chat_id = jid_id(&chat_jid);
                self.pending_chat_actions.remove(&(chat_id, "archive"));
                self.apply_archive_changed(chat_id, archived);
                self.note_sync_finishing_patch();
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
                self.note_sync_finishing_patch();
            }
            BackendEvent::ChatReadChanged { chat_jid, read, unread_count } => {
                self.stamp_sync_activity();
                let chat_id = jid_id(&chat_jid);
                self.pending_unread.remove(&chat_id);
                self.apply_chat_read_changed(chat_id, read, unread_count);
                self.note_sync_finishing_patch();
            }
            BackendEvent::ChatDeleted { chat_jid } => {
                let chat_id = jid_id(&chat_jid);
                if self.pending_chat_deletes.remove(&chat_id) {
                    // My own delete, confirmed: its Busy notice
                    // has done its work.
                    self.clear_busy_notice();
                }
                // A deletion of a chat the roster actually holds
                // is a roster change (qc1644); deleting one it
                // never held changes nothing.
                let was_present = self.chats.iter().any(|c| c.id == chat_id);
                self.apply_chat_deleted(chat_id, &chat_jid);
                // The removal shifted every row after the deleted
                // chat: rebuild the position map from the list as
                // it now stands (qc1644).
                if let Some(session) = &mut self.live {
                    session.rebuild_chat_positions(&self.chats);
                    if was_present {
                        session.note_roster_change();
                    }
                }
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
            BackendEvent::MarkReadFailed { message, .. } => {
                // The chat stays read here (the read really
                // happened in this app); only the sync to the
                // account's other devices failed, so there is
                // nothing to roll back, just the notice.
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("mark read failed: {message}");
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
                        let mut renamed = false;
                        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                            // A confirmed rename that lands is a
                            // roster change (qc1644).
                            renamed = chat.title != subject;
                            chat.title = subject.clone();
                        }
                        if renamed {
                            if let Some(session) = &mut self.live {
                                session.note_roster_change();
                            }
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
                    // The flag is a filter input, so a rollback
                    // that actually flips it is a roster change
                    // (qc1644).
                    let mut restored = false;
                    if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                        restored = c.archived != previous;
                        c.archived = previous;
                    }
                    if restored {
                        if let Some(session) = &mut self.live {
                            session.note_roster_change();
                        }
                    }
                    self.persist_chat_row(chat_id);
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::PrivacyState(map) => {
                self.apply_privacy_state(map);
            }
            BackendEvent::SavedContacts { contacts } => {
                // The backend reports the saved set whole, so
                // the session's copy is replaced, never merged:
                // a contact the book dropped must leave the
                // status audience and the people pickers too.
                // The form map is rebuilt from the same report:
                // each contact's canonical JID maps to itself
                // and each alternate form maps to the canonical
                // JID, so a list entry recorded in any known
                // form resolves to the one canonical person key.
                let mut healed_list: Option<Vec<String>> = None;
                if let Some(session) = &mut self.live {
                    let mut saved_contacts = std::collections::HashMap::new();
                    let mut saved_contact_forms = std::collections::HashMap::new();
                    for contact in contacts {
                        saved_contact_forms.insert(contact.jid.clone(), contact.jid.clone());
                        for alt_jid in &contact.alt_jids {
                            saved_contact_forms.insert(alt_jid.clone(), contact.jid.clone());
                        }
                        saved_contacts.insert(contact.jid, contact.name);
                    }
                    // A re-point retires the person's previous
                    // form: it leaves the new form map while the
                    // person's stable alternate form persists
                    // under the new canonical key. Successive
                    // reports are the only place that continuity
                    // is visible, so the retirements are learned
                    // here, before the old map is replaced, at
                    // entry granularity (see retired_follows):
                    // a form can also change hands inside the
                    // one step, re-saved under a new contact
                    // while its old owner moves, and that follow
                    // is learned the same way. A form the new
                    // report knows again stops being retired:
                    // the current map is the authority for it
                    // from then on. The same comparison marks
                    // the forms that changed hands from an
                    // owner who left the saved set entirely
                    // (see orphaned_forms): no follow exists
                    // for those, and the heal below drops the
                    // entries naming them. A form a fresh
                    // follow claims is spoken for by its old
                    // owner's move, so any older orphan mark
                    // on it clears; a mark also clears when the
                    // form leaves the new form map. Every edge
                    // this report learns is stamped with the
                    // session's report step, which advances
                    // once per report: whether a follow was
                    // learned with the move a heal is following
                    // or in a later report than it is exactly
                    // what the heal walk needs to tell a
                    // handoff inside one step from a second
                    // move across steps (see
                    // resolve_stored_jid_follow_first), and
                    // the stamp is the only place that ordering
                    // survives.
                    session.saved_contacts_report_step =
                        session.saved_contacts_report_step.wrapping_add(1);
                    let report_step = session.saved_contacts_report_step;
                    let follows =
                        retired_follows(&session.saved_contact_forms, &saved_contact_forms);
                    let orphans =
                        orphaned_forms(&session.saved_contact_forms, &saved_contact_forms);
                    session
                        .retired_contact_forms
                        .retain(|form, _| !saved_contact_forms.contains_key(form));
                    session.retired_contact_forms.extend(
                        follows.into_iter().map(|(form, target)| (form, (target, report_step))),
                    );
                    let retired = &session.retired_contact_forms;
                    session.orphaned_contact_forms.retain(|form| {
                        saved_contact_forms.contains_key(form) && !retired.contains_key(form)
                    });
                    session.orphaned_contact_forms.extend(orphans);
                    session.saved_contacts = saved_contacts;
                    session.saved_contact_forms = saved_contact_forms;
                    // The name directory learns the pairings
                    // this report carries (qc1654 Claim C):
                    // names seeded under one form before the
                    // pairing was known are copied under the
                    // person's other form, so mentions resolve
                    // under either form from this report on.
                    let name_pairs: Vec<(String, String)> = session
                        .names
                        .iter()
                        .flat_map(|(jid, name)| {
                            session
                                .counterpart_jids(jid)
                                .into_iter()
                                .map(|form| (form, name.clone()))
                                .collect::<Vec<_>>()
                        })
                        .collect();
                    for (form, name) in name_pairs {
                        session.names.entry(form).or_insert(name);
                    }
                    // The stored status privacy list is healed
                    // entry by entry (see heal_stored_entry):
                    // an entry naming a retired form follows
                    // the person to their new canonical key, or
                    // the exception (or the share_with choice)
                    // it records silently stops naming that
                    // person: in except mode they would receive
                    // the status at their new number, against
                    // the explicit pick. An entry whose person
                    // left the saved set while their form
                    // passed to a different identified person
                    // drops out: it names nobody now, and
                    // leaving it in would retarget it to the
                    // form's new holder. Entries the new report
                    // still knows, with no retirement learned
                    // for them, are left exactly as recorded,
                    // so a report that teaches nothing rewrites
                    // nothing.
                    let mut any_followed = false;
                    let mut followed: Vec<String> = Vec::new();
                    for jid in &self.updates.status_privacy_list {
                        if let Some(resolved) = session.heal_stored_entry(jid) {
                            any_followed |= resolved != *jid;
                            followed.push(resolved);
                        }
                    }
                    if any_followed {
                        // A follow can land two entries on the
                        // person they both name (the old form
                        // and the new one were both recorded):
                        // the healed list names each person
                        // once.
                        let mut seen = std::collections::HashSet::new();
                        followed.retain(|jid| seen.insert(jid.clone()));
                    }
                    if followed != self.updates.status_privacy_list {
                        healed_list = Some(followed);
                    }
                }
                if let Some(list) = healed_list {
                    // The stored list followed a contact across
                    // their number change: the mirror moves, and
                    // the account record is re-sent so it heals
                    // on the account too, not only here (the
                    // phone and other devices read the record).
                    // A report landing is a data change, not a
                    // viewing, so this dispatch does not cross
                    // the discipline the privacy editors keep:
                    // nothing is ever sent for merely opening an
                    // editor.
                    self.updates.status_privacy_list = list.clone();
                    if self.live_mode {
                        self.send_backend(BackendCommand::SetStatusPrivacy {
                            mode: self.updates.status_privacy_mode.clone(),
                            jids: list,
                        });
                    }
                }
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
            BackendEvent::ContactAbout { chat_jid, about } => {
                // The profile panel reads About only from this
                // cache in live mode, never from the presence
                // subtitle (qc1644 E5).
                self.contact_about.insert(chat_jid, about);
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
                        // A verified name landing or changing is
                        // a roster change (qc1644); the same name
                        // reported again is not.
                        if session.verified_names.get(&jid) != Some(&name) {
                            session.verified_names.insert(jid, name);
                            session.note_roster_change();
                        }
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
                        session.put_avatar_bytes(jid.clone(), bytes);
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
                    if self.event_batch.active {
                        self.event_batch.deferred_writes.push(
                            DeferredStoreWrite::MediaDirectPath {
                                chat_jid: chat_jid.clone(),
                                message_id: pid.clone(),
                                direct_path: direct_path.clone(),
                            },
                        );
                    } else if let Some(store) = &session.store {
                        let _ = store.update_media_direct_path(&chat_jid, &pid, &direct_path);
                    }
                }
            }
            BackendEvent::FavoritesSync { chat_jids } => {
                self.apply_favorites_sync(&chat_jids);
            }
            BackendEvent::StickerFavoriteChanged { filehash, favorite } => {
                self.apply_sticker_favorite_changed(filehash, favorite);
            }
            BackendEvent::RecentStickerRemoved { filehash } => {
                self.apply_recent_sticker_removed(filehash);
            }
            BackendEvent::StickerRefBytes { filehash, bytes } => {
                // A synced picker's sticker landed: the bytes
                // are kept for the send, and the texture
                // decodes from them for the tile.
                self.picker.synced_sticker_fetching.remove(&filehash);
                if let Some(tex) = crate::media::decode_photo(ctx, &bytes) {
                    self.picker.synced_sticker_textures.insert(filehash.clone(), tex);
                }
                self.picker.synced_sticker_bytes.insert(filehash, bytes);
            }
            BackendEvent::StickerRefFetchFailed { filehash } => {
                self.picker.synced_sticker_fetching.remove(&filehash);
                self.picker.synced_sticker_failed.insert(filehash);
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
                        session.put_video_thumb(mid, bytes.clone());
                        if self.event_batch.active {
                            self.event_batch.deferred_writes.push(
                                DeferredStoreWrite::VideoThumbnail {
                                    chat_jid: chat_jid.clone(),
                                    message_id: pid.clone(),
                                    bytes: bytes.clone(),
                                },
                            );
                        } else if let Some(store) = &session.store {
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
            BackendEvent::GroupMembers { chat_jid, members, own_admin, own_label } => {
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
                // The account's own member label (row 65)
                // rides the same answer; an unset label clears
                // any stale local copy.
                match own_label {
                    Some(label) => {
                        self.groups.own_member_labels.insert(jid_id(&chat_jid), label);
                    }
                    None => {
                        self.groups.own_member_labels.remove(&jid_id(&chat_jid));
                    }
                }
                // Member names join the name directory before
                // row resolution masks any of them: mentions of
                // this group's members resolve from here.
                if let Some(session) = &mut self.live {
                    for (jid, name, _admin, _label) in &members {
                        if !name.is_empty() && !is_unresolved_identifier(name) {
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
            BackendEvent::GroupHistoryReported { chat_jid, added, refused, report } => {
                let chat_id = jid_id(&chat_jid);
                // The outcome speaks in parts, each one only
                // what the backend actually reported (row 64):
                // who was added, who was refused and why, and
                // what became of the history share itself.
                let mut parts: Vec<String> = Vec::new();
                if !added.is_empty() {
                    parts.push(crate::i18n::tf(
                        "group.history_added",
                        "count",
                        &added.len().to_string(),
                    ));
                }
                for (address, reason) in &refused {
                    parts.push(
                        crate::i18n::tf("group.history_refused", "address", address)
                            .replace("{reason}", reason),
                    );
                }
                let mut failed = false;
                match &report {
                    backend::HistoryShareReport::Shared { messages } => {
                        parts.push(crate::i18n::tf(
                            "group.history_shared",
                            "count",
                            &messages.to_string(),
                        ));
                        if self.groups.history_retry_chat == Some(chat_id) {
                            self.groups.history_retry_chat = None;
                        }
                    }
                    backend::HistoryShareReport::NotRequested => {}
                    backend::HistoryShareReport::Skipped { reason } => {
                        parts.push(crate::i18n::tf("group.history_skipped", "reason", reason));
                    }
                    backend::HistoryShareReport::Failed { error, retryable } => {
                        failed = true;
                        parts.push(error.clone());
                        if *retryable {
                            self.groups.history_retry_chat = Some(chat_id);
                        }
                    }
                }
                if !parts.is_empty() {
                    let text = parts.join(" ");
                    self.notice =
                        Some(if failed { Notice::error(text) } else { Notice::info(text) });
                }
            }
            BackendEvent::MemberLabelSet { chat_jid, label } => {
                let chat_id = jid_id(&chat_jid);
                if self.groups.member_label_pending == Some(chat_id) {
                    self.groups.member_label_pending = None;
                }
                if label.is_empty() {
                    self.groups.own_member_labels.remove(&chat_id);
                    self.notice = Some(Notice::info(crate::i18n::t("group.member_label_cleared")));
                } else {
                    self.groups.own_member_labels.insert(chat_id, label);
                    self.notice = Some(Notice::info(crate::i18n::t("group.member_label_set")));
                }
            }
            BackendEvent::MemberLabelFailed { chat_jid, message } => {
                let chat_id = jid_id(&chat_jid);
                if self.groups.member_label_pending == Some(chat_id) {
                    self.groups.member_label_pending = None;
                }
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::MemberLabelChanged { chat_jid, member_jid, label } => {
                // Another member's label changed (or our own,
                // from another device): the roster row follows.
                let chat_id = jid_id(&chat_jid);
                if let Some(rows) = self.groups.live_members.get_mut(&chat_id) {
                    if let Some(row) = rows.iter_mut().find(|r| r.address == member_jid) {
                        row.member_label = label;
                    }
                }
            }
            BackendEvent::MessageReportedToAdmins { chat_jid: _, message_id: _ } => {
                self.notice = Some(Notice::info(crate::i18n::t("group.report_sent")));
            }
            BackendEvent::ReportToAdminsFailed { message, .. } => {
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::ReportedMessages { chat_jid, reports } => {
                let chat_id = jid_id(&chat_jid);
                // Reporter labels resolve through the name
                // directory when the reporter arrived as an
                // address; a name stays a name. The snippet
                // comes from this device's archive when it
                // holds the message (row 65).
                let names: std::collections::HashMap<String, String> =
                    self.live.as_ref().map(|session| session.names.clone()).unwrap_or_default();
                let archived: Vec<ReportedArchiveRow> = self
                    .live
                    .as_ref()
                    .and_then(|session| session.store.as_ref())
                    .and_then(|store| store.messages(&chat_jid).ok())
                    .map(|rows| {
                        rows.into_iter()
                            .map(|(message, _)| {
                                // Kinds whose words live outside
                                // the body field (a carousel's
                                // body, a location's name) lend
                                // them to the snippet, so a
                                // reported card never reads as an
                                // empty quote.
                                let body = if message.body.is_empty() {
                                    match &message.kind {
                                        backend::LiveMessageKind::Carousel { body, .. } => {
                                            body.clone()
                                        }
                                        backend::LiveMessageKind::Location { name, .. } => {
                                            name.clone()
                                        }
                                        _ => message.body.clone(),
                                    }
                                } else {
                                    message.body.clone()
                                };
                                (
                                    message.id,
                                    message.sender_jid,
                                    message.sender,
                                    message.from_me,
                                    body,
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let rows = reports
                    .into_iter()
                    .map(|entry| {
                        let reporters = entry
                            .reporters
                            .iter()
                            .map(|reporter| {
                                // An address resolves through
                                // the name directory and masks
                                // when unknown; a display name
                                // (the fake's form) stays as is.
                                if reporter.contains('@') {
                                    resolve_person_label("", reporter, &names).0
                                } else {
                                    reporter.clone()
                                }
                            })
                            .collect();
                        let snippet = archived.iter().find(|(id, ..)| *id == entry.message_id).map(
                            |(_, sender_jid, sender_name, from_me, body)| {
                                let sender = if *from_me {
                                    crate::i18n::t("profile.my_default_name")
                                } else if let Some(name) = sender_name {
                                    name.clone()
                                } else if let Some(jid) = sender_jid {
                                    resolve_person_label("", jid, &names).0
                                } else {
                                    String::new()
                                };
                                (sender, body.clone())
                            },
                        );
                        crate::state::ReportedRow { snippet, reporters }
                    })
                    .collect();
                if self.groups.reported_error.as_ref().is_some_and(|(id, _)| *id == chat_id) {
                    self.groups.reported_error = None;
                }
                self.groups.reported = Some((chat_id, rows));
            }
            BackendEvent::ReportedMessagesFailed { chat_jid, message } => {
                let chat_id = jid_id(&chat_jid);
                self.groups.reported_error = Some((chat_id, human_error(&message)));
            }
            BackendEvent::JoinRequests { chat_jid, requests } => {
                let rows = self.resolve_request_rows(requests);
                self.groups.join_requests = Some((jid_id(&chat_jid), rows));
            }
            BackendEvent::SyncProgress { percent } => {
                // A progress report while not syncing is a new
                // episode's first arrival: the open chat's send
                // caches are warmed once for it below, so a send
                // typed during the sync does not pay the cold
                // preparatory queries the sync is busy with.
                let episode_started = self.live.as_ref().is_some_and(|s| !s.syncing);
                if let Some(session) = &mut self.live {
                    // Progress arriving while not syncing starts
                    // a fresh episode: the previous episode's
                    // stamps (including a completed 100) must not
                    // clamp or leak into it.
                    if !session.syncing {
                        session.sync_percent = None;
                        session.sync_finishing = false;
                    }
                    session.syncing = true;
                    session.sync_last_activity = Some(std::time::Instant::now());
                    // Fresh progress means a live episode: any
                    // completion hold from the previous one ends.
                    session.sync_completed_at = None;
                    if let Some(reported) = percent {
                        // Honest display rules: show only what
                        // the library reported, keep the strongest
                        // stamp of this episode (the stamps carry
                        // no monotonicity promise), and never let
                        // a mid-sync stamp read as finished: 100
                        // belongs to the completion event alone.
                        let shown = reported.min(99);
                        session.sync_percent =
                            Some(session.sync_percent.map_or(shown, |best| best.max(shown)));
                    }
                }
                if episode_started {
                    let selected = self.selected_chat;
                    self.warm_chat_cache(selected);
                }
            }
            BackendEvent::SyncInterrupted => {
                // The drain did not finish: the client is not
                // caught up. The sync row stays up and
                // synced_once is never latched by an interruption
                // (QC1614-A0-5 W1).
                if let Some(session) = &mut self.live {
                    // An interruption after a completed episode
                    // starts a new one: the completed 100 must
                    // not ride into it (100 belongs to completion
                    // alone).
                    if !session.syncing {
                        session.sync_percent = None;
                    }
                    session.syncing = true;
                    session.sync_last_activity = Some(std::time::Instant::now());
                    session.sync_completed_at = None;
                    // An interruption ends the finishing phase
                    // with the episode it belonged to (qc1644).
                    session.sync_finishing = false;
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
                if let Some(tag) = tag.filter(|t| !t.is_empty() && !is_unresolved_identifier(t)) {
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
                // the healable titles now that the roster
                // exists, with the same predicate the connect
                // report uses (a formatted phone title is
                // healable, not resolved); later syncs do not
                // repeat it.
                let healable_titles: Vec<String> = match &self.live {
                    Some(session) if !session.synced_once => self
                        .chats
                        .iter()
                        .filter(|chat| {
                            is_unresolved_identifier(&chat.title)
                                || is_formatted_phone(&chat.title)
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
                    // True completion is the only place 100 is
                    // set; the status row holds it briefly via
                    // `sync_completed_at`, then clears.
                    session.sync_percent = Some(100);
                    session.sync_completed_at = Some(std::time::Instant::now());
                    // Completion ends the finishing phase too
                    // (qc1644).
                    session.sync_finishing = false;
                    if !healable_titles.is_empty() {
                        let _ = session
                            .channels
                            .commands
                            .send(BackendCommand::ResolveTitles { jids: healable_titles });
                    }
                }
                // The episode is over: this batch's flush owes
                // the chat list its final sort regardless of the
                // sync cadence (qc1644), so the order the list
                // settles into is the fully sorted one.
                if self.event_batch.active {
                    self.event_batch.force_chat_sort = true;
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
                self.fail_pending_echoes();
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
                // This arm carries every remaining backend
                // error, and none of them is a chat send outcome:
                // send failures arrive typed as
                // MessageSendFailed and mark their own chat's
                // echo there. An error here (a rename, a lookup)
                // says nothing about an in-flight send, so no
                // echo is marked from it.
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
            BackendEvent::IncomingCall { call_id, chat_jid, video, group_jid } => {
                let chat_id = jid_id(&chat_jid);
                self.on_incoming_call(&call_id, chat_id, video, group_jid.is_some());
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
                    // A call-link call has no chat behind it:
                    // it leaves no chat-bound history record.
                    let link_call = self.calls.active.as_ref().is_some_and(|call| call.link);
                    if !link_call {
                        self.record_ended_call(&call_id, &chat_jid, seconds);
                    }
                }
            }
            BackendEvent::CallParticipants { participants } => {
                self.on_call_participants(participants);
                // Roster names resolve through the group's
                // member list; fetch it once for a group call
                // instead of waiting for the invite picker.
                if self.calls.active.as_ref().is_some_and(|c| c.group) {
                    self.ensure_call_members();
                }
            }
            BackendEvent::CallReaction { jid, emoji } => {
                self.on_call_reaction(&jid, emoji);
            }
            BackendEvent::CallWaitingRoom { is_admin, approval_required, users } => {
                self.on_call_waiting_room(crate::call_ui::WaitingRoomUi {
                    is_admin,
                    approval_required,
                    users,
                });
            }
            BackendEvent::CallVideoState { local_on, remote_on, local_note } => {
                self.on_call_video_state(local_on, remote_on, local_note);
            }
            BackendEvent::CallVideoAu { sender, data, keyframe, orientation: _ } => {
                // Frame orientation is not applied when
                // painting: desktop peers send orientation 0,
                // and rotating a decoded frame needs a texture
                // pass this stage does not have yet (recorded
                // on parity row 91).
                self.on_call_video_au(ctx, sender, &data, keyframe);
            }
            BackendEvent::CallVideoFrame { rgba, width, height } => {
                self.on_call_video_frame(ctx, rgba, width, height);
            }
            BackendEvent::CallLinkCreated { url, video } => {
                self.calls.link_busy = false;
                self.calls.link_created = Some((url, video));
            }
            BackendEvent::CallLinkPreview { url, creator_jid, video, waiting_room } => {
                self.calls.link_busy = false;
                self.calls.link_preview =
                    Some(crate::call_ui::LinkPreviewUi { url, creator_jid, video, waiting_room });
            }
            BackendEvent::CallLinkFailed { message } => {
                self.calls.link_busy = false;
                let message = core::crash::scrub_for_display(&message);
                self.notice = Some(Notice::error(human_error(&message)));
            }
            BackendEvent::CallControlFailed { message } => {
                // Same discipline as CallAudioFailed: the
                // optimistic flips revert on this typed event,
                // never on the generic error path (VRF-04).
                let message = core::crash::scrub_for_display(&message);
                tracing::warn!("call control failed: {message}");
                self.on_call_control_failed();
                self.notice = Some(Notice::error(human_error(&message)));
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
        if title.is_empty() || is_unresolved_identifier(title) {
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
        let mut renamed = false;
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == id) {
            let healable = chat.title.is_empty()
                || is_unresolved_identifier(&chat.title)
                || is_formatted_phone(&chat.title);
            if authoritative || healable {
                // A rename that lands is a roster change
                // (qc1644); re-stating the current title is not.
                renamed = chat.title != title;
                chat.title = title.to_owned();
            }
        }
        if renamed {
            if let Some(session) = &mut self.live {
                session.note_roster_change();
            }
        }
    }

    /// Records one message's author JID in both directions of
    /// the sender index (qc1634 H4): the forward map answers
    /// "who wrote this message", the inverse map answers "which
    /// messages did this sender write" without a scan.
    pub(crate) fn note_sender_jid(&mut self, msg_id: u64, jid: &str) {
        if let Some(old) = self.sender_jids.get(&msg_id) {
            if old != jid {
                let old = old.clone();
                if let Some(set) = self.sender_msgs.get_mut(&old) {
                    set.remove(&msg_id);
                }
            }
        }
        self.sender_jids.insert(msg_id, jid.to_owned());
        self.sender_msgs.entry(jid.to_owned()).or_default().insert(msg_id);
    }

    /// Drops one message from both directions of the sender
    /// index (the message is being forgotten).
    pub(crate) fn drop_sender_jid(&mut self, msg_id: u64) {
        if let Some(old) = self.sender_jids.remove(&msg_id) {
            if let Some(set) = self.sender_msgs.get_mut(&old) {
                set.remove(&msg_id);
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
            if !name.is_empty() && !is_unresolved_identifier(name) {
                session.names.insert(sender_jid.to_owned(), name.to_owned());
            }
        }
        let raw = sender_jid.split('@').next().unwrap_or(sender_jid);
        if !is_unresolved_identifier(raw) {
            return;
        }
        // The masked form of the same identifier is the same
        // unresolved placeholder (qc1635): a label still showing
        // either form is this sender's to rewrite.
        let masked = masked_identifier(sender_jid);
        // The rename UPDATEs message rows; rows this same batch
        // filed are not in the archive yet, so the write always
        // defers to the batch flush, after their upserts, where
        // every rename queued in the batch lands as one
        // rename_senders call (qc1644).
        self.event_batch.deferred_writes.push(DeferredStoreWrite::RenameSender {
            sender_jid: sender_jid.to_owned(),
            name: name.to_owned(),
        });
        if !self.event_batch.active {
            // Event application outside a drain (a direct apply
            // in tests) has no flush coming: run the batch flush
            // now, so the rename lands at this same point the old
            // immediate write landed.
            self.event_batch.active = true;
            self.finish_event_batch();
        }
        if let Some(session) = &mut self.live {
            // Only this sender's messages can carry the label.
            // The inverse sender index names them directly, and
            // the per-chat position index locates each one, so
            // the rewrite touches exactly the sender's rows
            // instead of scanning every mirrored message
            // (qc1634 H4). Entries seeded into the forward map
            // without the inverse (test fixtures) fall back to
            // the scan, so no label can be missed.
            let mut targets: std::collections::HashSet<u64> =
                self.sender_msgs.get(sender_jid).cloned().unwrap_or_default();
            if targets.is_empty() {
                targets = self
                    .sender_jids
                    .iter()
                    .filter(|(_, jid)| jid.as_str() == sender_jid)
                    .map(|(msg_id, _)| *msg_id)
                    .collect();
            }
            if !targets.is_empty() {
                // Locate through the position index first,
                // validating each cached position against the
                // row itself; anything the index cannot place
                // (its chat's index was invalidated) is found by
                // the fallback scan below.
                let mut located: Vec<(u64, usize)> = Vec::new();
                let mut unlocated: std::collections::HashSet<u64> = targets.clone();
                for (chat_id, index) in &session.msg_index {
                    for msg_id in &targets {
                        if let Some(pos) = index.get(msg_id) {
                            if session
                                .messages
                                .get(chat_id)
                                .and_then(|entry| entry.get(*pos))
                                .is_some_and(|(_, m)| m.id == *msg_id)
                            {
                                located.push((*chat_id, *pos));
                                unlocated.remove(msg_id);
                            }
                        }
                    }
                }
                for (chat_id, pos) in located {
                    if let Some((_, msg)) =
                        session.messages.get_mut(&chat_id).and_then(|entry| entry.get_mut(pos))
                    {
                        if msg.sender.as_deref() == Some(raw)
                            || msg.sender.as_deref() == Some(masked.as_str())
                        {
                            msg.sender = Some(name.to_owned());
                        }
                    }
                }
                if !unlocated.is_empty() {
                    for list in session.messages.values_mut() {
                        for (_, msg) in list.iter_mut() {
                            if unlocated.contains(&msg.id)
                                && (msg.sender.as_deref() == Some(raw)
                                    || msg.sender.as_deref() == Some(masked.as_str()))
                            {
                                msg.sender = Some(name.to_owned());
                            }
                        }
                    }
                }
            }
        }
        // The preview sender is a search input, so a chat row
        // whose sender label this rename rewrites is a roster
        // change (qc1644): a search over the old label must
        // stop matching, and over the new one start.
        let mut sender_rewritten = false;
        for chat in self.chats.iter_mut() {
            if chat.preview_sender.as_deref() == Some(raw)
                || chat.preview_sender.as_deref() == Some(masked.as_str())
            {
                chat.preview_sender = Some(name.to_owned());
                sender_rewritten = true;
            }
        }
        if sender_rewritten {
            if let Some(session) = &mut self.live {
                session.note_roster_change();
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
        // A history upsert's unread count is the payload's own
        // seed: record the chat as seeded so its history
        // messages do not increment the badge a second time
        // when they file (qc1634 claim 2).
        if live.from_history {
            if let Some(session) = &mut self.live {
                session.seeded_unread.insert(id);
            }
        }
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
        if is_unresolved_identifier(&merged.title) {
            if let Some(known) = self.chats.iter().find(|c| c.id == id) {
                if !known.title.is_empty() && !is_unresolved_identifier(&known.title) {
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
                    if !name.is_empty() && !is_unresolved_identifier(name) {
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
        // The activity stamp this chat carried before the
        // upsert, read before the session bookkeeping below
        // overwrites it: the list order sorts on the stamp, so
        // a moved stamp is a roster change even when every
        // display field of the merged row matches the old one.
        let prev_timestamp = self.live.as_ref().and_then(|s| s.times.get(&id).copied());
        if let Some(session) = &mut self.live {
            if session.store.is_some() {
                if self.event_batch.active {
                    // Queued, not written: the batch flush writes
                    // the merged rows in one transaction instead
                    // of one commit per upsert (qc1634 H2).
                    self.event_batch.chat_upserts.push(merged.clone());
                } else if let Some(store) = &session.store {
                    let _ = store.upsert_chat(&merged);
                }
            }
            session.jids.insert(id, merged.jid.clone());
            session.times.insert(id, merged.timestamp);
        }
        // An insert, or a merge that moves any field the chat
        // filter reads, is a roster change (qc1644): the title
        // (search), the preview and its sender (search), the
        // unread count (the Unread filter), the archived,
        // locked and pinned flags (their filters and the
        // pinned-first partition), the kind (the kind filters)
        // and the activity stamp (the list order). A merge
        // that restates every one of them is not a change, so
        // identical re-deliveries keep the filtered views
        // cached.
        let roster_changed = if let Some(chat) = self.chats.iter_mut().find(|c| c.id == id) {
            let fresh = to_chat(&merged);
            let changed = chat.title != fresh.title
                || chat.preview != fresh.preview
                || chat.preview_sender != fresh.preview_sender
                || chat.time != fresh.time
                || chat.unread != fresh.unread
                || chat.archived != fresh.archived
                || chat.pinned != fresh.pinned
                || chat.locked != fresh.locked
                || chat.kind != fresh.kind
                || prev_timestamp.is_some_and(|prev| prev != merged.timestamp);
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
            changed
        } else {
            // An append moves no other row, so the position map
            // learns the new chat's position directly (qc1644).
            let pos = self.chats.len();
            self.chats.push(to_chat(&merged));
            if let Some(session) = &mut self.live {
                session.chat_positions.insert(id, pos);
            }
            true
        };
        if roster_changed {
            if let Some(session) = &mut self.live {
                session.note_roster_change();
            }
        }
        // The community tag is the parent community's title:
        // the parent's own tag is its title, a linked subgroup
        // takes its parent's title once the parent is known,
        // and a parent landing now retags its known children.
        if merged.is_parent && !is_unresolved_identifier(&merged.title) {
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
                .filter(|t| !t.is_empty() && !is_unresolved_identifier(t));
            if let Some(tag) = tag {
                if let Some(chat) = self.chats.iter_mut().find(|c| c.id == id) {
                    chat.community = Some(tag);
                }
            }
        }
        self.sort_live_chats();
        // A session that started on an empty archive has no
        // selection: the session-start pick found no chat and
        // left the sentinel, and every conversation sync keys
        // on the raw selection id, so the first chat's header
        // (the shell's fallback) would sit over a stream that
        // never loads and never marks read. The first chat to
        // land becomes the open conversation, the same pick
        // session start makes from a populated archive, with
        // the same no-read-marking semantics (selection alone
        // never marks anything read; engagement does).
        if self.selected_chat == 0 {
            if let Some(first) = self.chats.first().map(|c| c.id) {
                self.switch_composer_chat(0, first);
                self.selected_chat = first;
                self.sync_conversation();
            }
        }
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
            saved_contacts: std::collections::HashMap::new(),
            saved_contact_forms: std::collections::HashMap::new(),
            retired_contact_forms: std::collections::HashMap::new(),
            saved_contacts_report_step: 0,
            orphaned_contact_forms: std::collections::HashSet::new(),
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
            sync_finishing: false,
            seeded_unread: std::collections::HashSet::new(),
            msg_index: HashMap::new(),
            msg_index_complete: std::collections::HashSet::new(),
            chat_positions: HashMap::new(),
            roster_generation: 0,
            archive_starred_generation: 0,
            ephemeral_index: std::collections::BTreeMap::new(),
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
            verified_levels: HashMap::new(),
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
            channel_info_requested: std::collections::HashSet::new(),
            history_armed: std::collections::HashSet::new(),
            paged_chats: std::collections::HashSet::new(),
            paged_to_start: std::collections::HashSet::new(),
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
            // Quiescent means the queue is empty AND no budgeted
            // flush is still pending: a flush that deferred on
            // its frame budget holds unapplied work (including
            // the deferred conversation rebuild) even when every
            // event has left the queue (qc1644).
            if app.event_queue.is_empty() && !app.event_batch.flush_pending {
                return;
            }
        }
        panic!("the event queue did not drain");
    }

    pub(super) fn viewed_status(id: &str, author_jid: &str) -> backend::LiveStatus {
        backend::LiveStatus {
            id: id.to_owned(),
            author_jid: author_jid.to_owned(),
            author_name: Some("Sarah".to_owned()),
            body: "hello status".to_owned(),
            timestamp: BASE_TS,
            from_me: false,
            kind: backend::LiveMessageKind::Text,
            background_argb: Some(0xFF00_A884),
            reactions: Vec::new(),
            my_reaction: None,
            photo_ref: None,
            photo_thumbnail: None,
            video_ref: None,
            video_thumbnail: None,
            poster_ref: None,
            video_width: None,
            video_height: None,
        }
    }

    /// H-1 (qc1644 review 2): with read receipts off, viewing
    /// a status must not tell the author. The local viewed
    /// mark still lands, so the UI shows the status as seen,
    /// but no MarkAsRead command leaves the app.
    #[test]
    fn status_view_sends_no_receipt_when_read_receipts_are_off() {
        let (mut app, _tx, _ctx, cmd_rx) = live_app();
        app.read_receipts = false;
        let status = viewed_status("st-off", CHAT_JID);
        app.mark_status_viewed(&status);
        assert!(
            app.updates.viewed.contains("st-off"),
            "the local viewed mark lands regardless of the toggle"
        );
        assert!(
            cmd_rx.try_recv().is_err(),
            "no command, in particular no MarkAsRead, is sent with receipts off"
        );
    }

    /// H-1 (qc1644 review 2): with read receipts on, the
    /// first view sends exactly one MarkAsRead addressed to
    /// the broadcast with the author as participant, and the
    /// once-guard keeps a second view silent.
    #[test]
    fn status_view_sends_one_receipt_when_read_receipts_are_on() {
        let (mut app, _tx, _ctx, cmd_rx) = live_app();
        app.read_receipts = true;
        let status = viewed_status("st-on", CHAT_JID);
        app.mark_status_viewed(&status);
        match cmd_rx.try_recv() {
            Ok(BackendCommand::MarkAsRead { chat_jid, sender_jid, message_ids }) => {
                assert_eq!(chat_jid, crate::live::STATUS_BROADCAST_JID);
                assert_eq!(sender_jid.as_deref(), Some(CHAT_JID));
                assert_eq!(message_ids, vec!["st-on".to_owned()]);
            }
            other => panic!("expected one MarkAsRead, got {other:?}"),
        }
        app.mark_status_viewed(&status);
        assert!(cmd_rx.try_recv().is_err(), "the second view sends nothing more");
    }

    pub(super) const SAVED_JID: &str = "6281111111111@s.whatsapp.net";
    pub(super) const STRANGER_DM_JID: &str = "6289999999999@s.whatsapp.net";

    pub(super) fn dm_upsert_titled(jid: &str, title: &str) -> BackendEvent {
        BackendEvent::ChatUpsert(LiveChat {
            jid: jid.to_owned(),
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
            from_history: false,
        })
    }

    pub(super) fn posted_recipients(
        cmd_rx: &std::sync::mpsc::Receiver<BackendCommand>,
    ) -> Vec<Vec<String>> {
        cmd_rx
            .try_iter()
            .filter_map(|cmd| match cmd {
                BackendCommand::PostTextStatus { recipients, .. } => Some(recipients),
                _ => None,
            })
            .collect()
    }

    pub(super) fn status_privacy_writes(
        cmd_rx: &std::sync::mpsc::Receiver<BackendCommand>,
    ) -> Vec<(String, Vec<String>)> {
        cmd_rx
            .try_iter()
            .filter_map(|cmd| match cmd {
                BackendCommand::SetStatusPrivacy { mode, jids } => Some((mode, jids)),
                _ => None,
            })
            .collect()
    }

    pub(super) fn saved_contact(jid: &str, name: &str, alt_jids: &[&str]) -> backend::SavedContact {
        backend::SavedContact {
            jid: jid.to_owned(),
            name: name.to_owned(),
            alt_jids: alt_jids.iter().map(|alt| (*alt).to_owned()).collect(),
        }
    }

    /// H-2 (qc1644 review 2): a SavedContacts report replaces
    /// the session's saved-contacts copy whole: a later report
    /// that omits a contact removes them, it does not merge
    /// over the earlier set.
    #[test]
    fn saved_contacts_report_replaces_the_session_copy() {
        const SAVED_LID: &str = "111111111111111@lid";
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![saved_contact(SAVED_JID, "Saved Sarah", &[SAVED_LID])],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.live.as_ref().unwrap().saved_contacts.get(SAVED_JID).map(String::as_str),
            Some("Saved Sarah")
        );
        assert_eq!(
            app.live.as_ref().unwrap().saved_contact_forms.get(SAVED_JID).map(String::as_str),
            Some(SAVED_JID),
            "the canonical form maps to itself"
        );
        assert_eq!(
            app.live.as_ref().unwrap().saved_contact_forms.get(SAVED_LID).map(String::as_str),
            Some(SAVED_JID),
            "the alternate form maps to the canonical JID"
        );
        tx.send(BackendEvent::SavedContacts { contacts: Vec::new() }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            app.live.as_ref().unwrap().saved_contacts.is_empty(),
            "the second report replaced the first, it did not merge"
        );
        assert!(
            app.live.as_ref().unwrap().saved_contact_forms.is_empty(),
            "the form map is replaced whole with the same report, it does not merge either"
        );
    }

    /// H-2 (qc1644 review 2): the status audience is the
    /// saved contacts, never the DM partners. Both chats below
    /// are DM partners; only the saved one may receive a
    /// contacts-mode post, and in except mode the stranger is
    /// outside the base set entirely, exception list or not.
    #[test]
    fn status_audience_posts_only_to_saved_contacts() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(dm_upsert_titled(SAVED_JID, "Saved Sarah")).unwrap();
        tx.send(dm_upsert_titled(STRANGER_DM_JID, "Stranger")).unwrap();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![saved_contact(SAVED_JID, "Saved Sarah", &[])],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.updates.status_privacy_mode = "contacts".to_owned();
        app.updates.draft = "hello contacts".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![SAVED_JID.to_owned()]],
            "contacts mode posts to the saved contact only, not to every DM partner"
        );
        app.updates.status_privacy_mode = "except".to_owned();
        app.updates.status_privacy_list = vec![STRANGER_DM_JID.to_owned()];
        app.updates.draft = "hello except".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![SAVED_JID.to_owned()]],
            "except mode still yields only the saved contact: the stranger is outside the base set"
        );
    }

    /// H-1 (qc1645 review 3): an exception recorded under a
    /// person's LID form excepts that person exactly as the
    /// same exception recorded under their canonical PN form
    /// does: the privacy list is canonicalized through the
    /// saved-contact form map before it is compared against
    /// the canonical base set.
    #[test]
    fn status_except_mode_canonicalizes_the_exception_list() {
        const PERSON_A_LID: &str = "111111111111111@lid";
        const PERSON_B_JID: &str = "6282222222222@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(SAVED_JID, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_B_JID, "Person B", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.updates.status_privacy_mode = "except".to_owned();
        app.updates.status_privacy_list = vec![PERSON_A_LID.to_owned()];
        app.updates.draft = "hello except lid".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_B_JID.to_owned()]],
            "the exception under the LID form excepts person A, leaving only person B"
        );
        app.updates.status_privacy_list = vec![SAVED_JID.to_owned()];
        app.updates.draft = "hello except pn".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_B_JID.to_owned()]],
            "the exception under the PN form excepts person A, leaving only person B"
        );
    }

    /// M-1 (qc1646 review 4): the end-to-end close of the
    /// stale form map. The privacy mirror was fetched before
    /// the backend book learned person A's form mapping, so
    /// the exception stands under A's LID form while the
    /// first saved-contacts report (also pre-mapping) gives
    /// the form map no LID edge. Once the mapping is learned,
    /// the backend re-reports the set with A's alternate form;
    /// the rebuilt form map canonicalizes the stale exception
    /// at post time and A receives nothing.
    #[test]
    fn status_except_mode_excludes_once_a_late_report_teaches_the_form() {
        const PERSON_A_LID: &str = "111111111111111@lid";
        const PERSON_B_JID: &str = "6282222222222@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(SAVED_JID, "Person A", &[]),
                saved_contact(PERSON_B_JID, "Person B", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.updates.status_privacy_mode = "except".to_owned();
        app.updates.status_privacy_list = vec![PERSON_A_LID.to_owned()];
        assert_eq!(
            app.live.as_ref().unwrap().saved_contact_forms.get(PERSON_A_LID),
            None,
            "before the mapping is learned the form map has no edge for A's LID"
        );
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(SAVED_JID, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_B_JID, "Person B", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.live.as_ref().unwrap().saved_contact_forms.get(PERSON_A_LID).map(String::as_str),
            Some(SAVED_JID),
            "the re-report teaches the form map A's LID edge"
        );
        app.updates.draft = "hello except late form".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_B_JID.to_owned()]],
            "the exception recorded under the LID form excepts person A end to end"
        );
    }

    /// M-1 (qc1647 review 6): the report the backend now
    /// produces for a contact whose history record carried
    /// both JID forms with no mappings-table pair: ONE entry,
    /// the LID as its alternate (proven from the raw payload
    /// by the backend test
    /// history_inline_contact_with_both_forms_is_one_saved_
    /// person_without_a_mappings_entry). Fed that report, the
    /// people picker offers person A once, and an except-mode
    /// post excludes A under EITHER form the exception names:
    /// the person the user excepted receives nothing. Before
    /// the backend fix this payload reported two entries with
    /// no alternates, the picker listed both forms as
    /// separate rows, and the exception removed only the form
    /// it named.
    #[test]
    fn status_except_mode_excludes_a_both_forms_contact_under_either_form() {
        const PERSON_A_LID: &str = "111111111111111@lid";
        const PERSON_B_JID: &str = "6282222222222@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(SAVED_JID, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_B_JID, "Person B", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.privacy_contact_options(),
            vec![
                (SAVED_JID.to_owned(), "Person A".to_owned()),
                (PERSON_B_JID.to_owned(), "Person B".to_owned()),
            ],
            "one picker row per person: A's two forms are one row under the canonical JID"
        );
        app.updates.status_privacy_mode = "except".to_owned();
        app.updates.status_privacy_list = vec![PERSON_A_LID.to_owned()];
        app.updates.draft = "hello except lid form".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_B_JID.to_owned()]],
            "the exception under the LID form leaves person A with nothing"
        );
        app.updates.status_privacy_list = vec![SAVED_JID.to_owned()];
        app.updates.draft = "hello except pn form".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_B_JID.to_owned()]],
            "the exception under the phone form leaves person A with nothing"
        );
    }

    /// M-1 (qc1648 review 7): a saved contact changes
    /// numbers. The backend's re-report moves the person to
    /// the new number as ONE entry (the old number's entry is
    /// gone, not merely unlinked: the book's pairings stay
    /// inverses across a re-point, proven from the raw
    /// payloads by the backend re-point tests), the form map
    /// follows, and an exception recorded under the person's
    /// LID form still excepts them: the post reaches neither
    /// of the person's numbers. The same holds for the
    /// exception recorded against the new number's row.
    #[test]
    fn status_except_mode_follows_a_contact_to_their_new_number() {
        const PERSON_A_OLD: &str = "6281111111111@s.whatsapp.net";
        const PERSON_A_NEW: &str = "6289999999999@s.whatsapp.net";
        const PERSON_A_LID: &str = "111111111111111@lid";
        const PERSON_B_JID: &str = "6282222222222@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_OLD, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_B_JID, "Person B", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.updates.status_privacy_mode = "except".to_owned();
        app.updates.status_privacy_list = vec![PERSON_A_LID.to_owned()];
        // The re-point lands: one entry under the new number,
        // the LID as its alternate, no entry under the old.
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_NEW, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_B_JID, "Person B", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.live.as_ref().unwrap().saved_contact_forms.get(PERSON_A_LID).map(String::as_str),
            Some(PERSON_A_NEW),
            "the form map follows the person to the new number"
        );
        assert_eq!(
            app.live.as_ref().unwrap().saved_contact_forms.get(PERSON_A_OLD),
            None,
            "the old number has no form edge any more"
        );
        app.updates.draft = "hello except repoint".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_B_JID.to_owned()]],
            "the LID-form exception excepts person A at the new number too: neither number receives the post"
        );
        app.updates.status_privacy_list = vec![PERSON_A_NEW.to_owned()];
        app.updates.draft = "hello except new row".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_B_JID.to_owned()]],
            "the exception against the new number's row leaves person A with nothing"
        );
    }

    /// H-1 (qc1649 review 8): the placement the qc1649 test
    /// missed. The picker row for a both-forms contact carries
    /// the canonical JID (the number), so excepting the
    /// contact in the status editor stores the exception under
    /// their NUMBER, through the real set_status_privacy write
    /// path. When the contact then changes number, the
    /// re-point report retires that number: the stored list
    /// must follow the person to the new canonical key, the
    /// healed list must go back to the account, and an
    /// except-mode post must still leave the excepted person
    /// with nothing. Before this fix the retired number had
    /// no form edge, the stored entry dangled, and the
    /// excepted person received the status at their new
    /// number.
    #[test]
    fn status_except_mode_follows_an_exception_recorded_under_the_old_number() {
        const PERSON_A_OLD: &str = "6281111111111@s.whatsapp.net";
        const PERSON_A_NEW: &str = "6289999999999@s.whatsapp.net";
        const PERSON_A_LID: &str = "111111111111111@lid";
        const PERSON_B_JID: &str = "6282222222222@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_OLD, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_B_JID, "Person B", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            app.privacy_contact_options().iter().any(|(jid, _)| jid == PERSON_A_OLD),
            "the picker's row for person A carries their number, the canonical JID"
        );
        app.set_status_privacy("except", vec![PERSON_A_OLD.to_owned()]);
        assert_eq!(
            app.updates.status_privacy_list,
            vec![PERSON_A_OLD.to_owned()],
            "the editor stores the exception under the person's number"
        );
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("except".to_owned(), vec![PERSON_A_OLD.to_owned()])],
            "the editor's write carries the number placement"
        );
        // The re-point lands: one entry under the new number,
        // the LID as its alternate, no entry under the old.
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_NEW, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_B_JID, "Person B", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.updates.status_privacy_list,
            vec![PERSON_A_NEW.to_owned()],
            "the stored exception followed the person to the new number"
        );
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("except".to_owned(), vec![PERSON_A_NEW.to_owned()])],
            "the healed list is re-sent so the account record heals too"
        );
        app.updates.draft = "hello except old placement".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_B_JID.to_owned()]],
            "the excepted person receives nothing at the new number; only person B does"
        );
    }

    /// H-1 mirror (qc1649 review 8): the share_with direction
    /// of the same dangling entry. A person chosen under their
    /// number keeps receiving after their number change: the
    /// stored allowlist follows them, the heal goes back to
    /// the account, and the backend's answer (the record as it
    /// now stands) reads back as the healed list.
    #[test]
    fn status_share_with_mode_follows_a_choice_recorded_under_the_old_number() {
        const PERSON_A_OLD: &str = "6281111111111@s.whatsapp.net";
        const PERSON_A_NEW: &str = "6289999999999@s.whatsapp.net";
        const PERSON_A_LID: &str = "111111111111111@lid";
        const PERSON_B_JID: &str = "6282222222222@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_OLD, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_B_JID, "Person B", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.set_status_privacy("share_with", vec![PERSON_A_OLD.to_owned()]);
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("share_with".to_owned(), vec![PERSON_A_OLD.to_owned()])],
            "the editor's write carries the number placement"
        );
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_NEW, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_B_JID, "Person B", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.updates.status_privacy_list,
            vec![PERSON_A_NEW.to_owned()],
            "the stored choice followed the person to the new number"
        );
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("share_with".to_owned(), vec![PERSON_A_NEW.to_owned()])],
            "the healed list is re-sent so the account record heals too"
        );
        app.updates.draft = "hello share with old placement".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_A_NEW.to_owned()]],
            "the chosen person keeps receiving, at the new number"
        );
        // The backend's answer to the heal write re-asserts
        // the audience it now holds: a later read shows the
        // healed list, and adopting it sends nothing back.
        let mut state = std::collections::BTreeMap::new();
        state.insert("statusprivacy".to_owned(), "share_with".to_owned());
        state.insert("statusprivacy_list".to_owned(), PERSON_A_NEW.to_owned());
        tx.send(BackendEvent::PrivacyState(state)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.updates.status_privacy_list,
            vec![PERSON_A_NEW.to_owned()],
            "the read-back shows the healed list"
        );
        assert!(
            status_privacy_writes(&cmd_rx).is_empty(),
            "adopting the healed record sends nothing back"
        );
    }

    /// H-1 adoption order (qc1649 review 8): the cross-device
    /// path. The re-point reports land BEFORE the account's
    /// status record is fetched (the phone wrote the exception
    /// under the old number; the backend's canonical rewrite
    /// through the book returns a retired form unchanged, so
    /// the record arrives naming the old number). The adopted
    /// entry follows the person through the retired forms,
    /// across two successive number changes, the heal goes
    /// back to the account, and the post honors it.
    #[test]
    fn status_list_adopted_after_a_repoint_follows_the_person() {
        const PERSON_A_OLD: &str = "6281111111111@s.whatsapp.net";
        const PERSON_A_MID: &str = "6285555555555@s.whatsapp.net";
        const PERSON_A_NEW: &str = "6289999999999@s.whatsapp.net";
        const PERSON_A_LID: &str = "111111111111111@lid";
        const PERSON_B_JID: &str = "6282222222222@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        for number in [PERSON_A_OLD, PERSON_A_MID, PERSON_A_NEW] {
            tx.send(BackendEvent::SavedContacts {
                contacts: vec![
                    saved_contact(number, "Person A", &[PERSON_A_LID]),
                    saved_contact(PERSON_B_JID, "Person B", &[]),
                ],
            })
            .unwrap();
            drain_all(&mut app, &ctx);
        }
        assert!(
            status_privacy_writes(&cmd_rx).is_empty(),
            "reports landing with an empty stored list dispatch nothing"
        );
        let mut state = std::collections::BTreeMap::new();
        state.insert("statusprivacy".to_owned(), "except".to_owned());
        state.insert("statusprivacy_list".to_owned(), PERSON_A_OLD.to_owned());
        tx.send(BackendEvent::PrivacyState(state)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.updates.status_privacy_list,
            vec![PERSON_A_NEW.to_owned()],
            "the adopted entry followed the person across both number changes"
        );
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("except".to_owned(), vec![PERSON_A_NEW.to_owned()])],
            "the heal is re-sent so the account record follows too"
        );
        app.updates.draft = "hello adopted exception".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_B_JID.to_owned()]],
            "the adopted exception excepts person A at their current number"
        );
    }

    /// H-1 discipline pin (qc1649 review 8): a report landing
    /// with no re-point in it rewrites nothing and dispatches
    /// nothing. The stored list keeps the exact forms it was
    /// recorded in (here the editor's number placement), and
    /// the exception still bites at post time.
    #[test]
    fn saved_contacts_report_without_a_repoint_leaves_the_status_list_alone() {
        const PERSON_A_OLD: &str = "6281111111111@s.whatsapp.net";
        const PERSON_A_LID: &str = "111111111111111@lid";
        const PERSON_B_JID: &str = "6282222222222@s.whatsapp.net";
        const PERSON_C_JID: &str = "6283333333333@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_OLD, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_B_JID, "Person B", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.set_status_privacy("except", vec![PERSON_A_OLD.to_owned()]);
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("except".to_owned(), vec![PERSON_A_OLD.to_owned()])]
        );
        // A later report with no re-point: the same persons,
        // plus one new saved contact.
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_OLD, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_B_JID, "Person B", &[]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.updates.status_privacy_list,
            vec![PERSON_A_OLD.to_owned()],
            "no re-point: the stored list is byte-identical"
        );
        assert!(status_privacy_writes(&cmd_rx).is_empty(), "no re-point: nothing is dispatched");
        app.updates.draft = "hello no repoint".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_B_JID.to_owned(), PERSON_C_JID.to_owned()]],
            "the exception still excepts person A; the new contact is in the audience"
        );
    }

    /// H-1 (qc1650 review 9): the atomic step. Person A
    /// changes number and the old number is re-saved under a
    /// new contact B inside ONE report, so the old number
    /// never appears absent: the flattened form maps show it
    /// present throughout, under a canonical key that is
    /// string-identical for A's old entry and B's new one.
    /// The entry grouping is the evidence that separates the
    /// two (A's LID sits with the new number, the old number
    /// sits with B's LID), and the follow is learned from
    /// it: the stored exception, recorded under the old
    /// number through the real set_status_privacy, follows A
    /// to the new number, the heal goes back to the account,
    /// and an except-mode post reaches B and C but not A.
    /// Before this fix no follow was learned, the stored
    /// entry stayed byte-identical, and at post time it
    /// resolved to B: A received the status at the new
    /// number and B was the one excepted.
    #[test]
    fn status_except_mode_follows_an_exception_when_the_number_is_resaved_in_the_same_report() {
        const PERSON_A_OLD: &str = "6281111111111@s.whatsapp.net";
        const PERSON_A_NEW: &str = "6289999999999@s.whatsapp.net";
        const PERSON_A_LID: &str = "111111111111111@lid";
        const PERSON_B_LID: &str = "222222222222222@lid";
        const PERSON_C_JID: &str = "6283333333333@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_OLD, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.set_status_privacy("except", vec![PERSON_A_OLD.to_owned()]);
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("except".to_owned(), vec![PERSON_A_OLD.to_owned()])],
            "the editor's write carries the old number placement"
        );
        // The atomic step lands as a single report: A under
        // the new number with the same LID, B under the old
        // number with B's own LID, C unchanged.
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_NEW, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_A_OLD, "Person B", &[PERSON_B_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.updates.status_privacy_list,
            vec![PERSON_A_NEW.to_owned()],
            "the stored exception followed person A to the new number"
        );
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("except".to_owned(), vec![PERSON_A_NEW.to_owned()])],
            "the healed list is re-sent so the account record heals too"
        );
        app.updates.draft = "hello atomic except".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_A_OLD.to_owned(), PERSON_C_JID.to_owned()]],
            "person A receives nothing; B (the old number's new holder) and C do"
        );
    }

    /// H-1 share_with mirror (qc1650 review 9): the same
    /// atomic step against an allowlist recorded under the
    /// old number. The choice follows A to the new number,
    /// and the post reaches A there and NOT B at the old
    /// number: before this fix the entry resolved through
    /// the current form map to B, a person the user never
    /// chose, and A received nothing.
    #[test]
    fn status_share_with_mode_follows_a_choice_when_the_number_is_resaved_in_the_same_report() {
        const PERSON_A_OLD: &str = "6281111111111@s.whatsapp.net";
        const PERSON_A_NEW: &str = "6289999999999@s.whatsapp.net";
        const PERSON_A_LID: &str = "111111111111111@lid";
        const PERSON_B_LID: &str = "222222222222222@lid";
        const PERSON_C_JID: &str = "6283333333333@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_OLD, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.set_status_privacy("share_with", vec![PERSON_A_OLD.to_owned()]);
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("share_with".to_owned(), vec![PERSON_A_OLD.to_owned()])],
            "the editor's write carries the old number placement"
        );
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_NEW, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_A_OLD, "Person B", &[PERSON_B_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.updates.status_privacy_list,
            vec![PERSON_A_NEW.to_owned()],
            "the stored choice followed person A to the new number"
        );
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("share_with".to_owned(), vec![PERSON_A_NEW.to_owned()])],
            "the healed list is re-sent so the account record heals too"
        );
        app.updates.draft = "hello atomic share with".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_A_NEW.to_owned()]],
            "the post reaches person A at the new number, not B at the old one"
        );
    }

    /// H-1 vanished owner, share_with (qc1650 review 9):
    /// person X is removed from the saved set entirely and
    /// their number is re-saved under B in the same report.
    /// No follow can be learned (X stands nowhere in the new
    /// report), so the stored allowlist entry naming the
    /// number names nobody now: it drops out of the list
    /// rather than retargeting to B, the heal goes back to
    /// the account, and the post refuses with no recipients
    /// instead of delivering to B.
    #[test]
    fn status_share_with_entry_drops_when_its_person_leaves_and_the_number_is_resaved() {
        const PERSON_X_JID: &str = "6281111111111@s.whatsapp.net";
        const PERSON_X_LID: &str = "111111111111111@lid";
        const PERSON_B_LID: &str = "222222222222222@lid";
        const PERSON_C_JID: &str = "6283333333333@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_X_JID, "Person X", &[PERSON_X_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.set_status_privacy("share_with", vec![PERSON_X_JID.to_owned()]);
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("share_with".to_owned(), vec![PERSON_X_JID.to_owned()])]
        );
        // X is gone from the report entirely; the number
        // stands under B, with B's own LID.
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_X_JID, "Person B", &[PERSON_B_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.updates.status_privacy_list,
            Vec::<String>::new(),
            "the entry naming the vanished person's number drops out of the list"
        );
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("share_with".to_owned(), Vec::<String>::new())],
            "the healed list is re-sent so the account record drops it too"
        );
        app.updates.draft = "hello vanished share with".to_owned();
        app.post_my_status();
        assert!(
            posted_recipients(&cmd_rx).is_empty(),
            "the post delivers to nobody, in particular not to B at the recycled number"
        );
        assert!(app.updates.error.is_some(), "the post refuses with the no-recipients error");
    }

    /// H-1 vanished owner, except (qc1650 review 9): the
    /// same removal and re-save against an exception list.
    /// The entry was X's exception; with X gone it excludes
    /// nobody new, so it drops out and B, a saved contact
    /// the user never excepted, still receives the post.
    #[test]
    fn status_except_entry_drops_when_its_person_leaves_and_the_number_is_resaved() {
        const PERSON_X_JID: &str = "6281111111111@s.whatsapp.net";
        const PERSON_X_LID: &str = "111111111111111@lid";
        const PERSON_B_LID: &str = "222222222222222@lid";
        const PERSON_C_JID: &str = "6283333333333@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_X_JID, "Person X", &[PERSON_X_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.set_status_privacy("except", vec![PERSON_X_JID.to_owned()]);
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("except".to_owned(), vec![PERSON_X_JID.to_owned()])]
        );
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_X_JID, "Person B", &[PERSON_B_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.updates.status_privacy_list,
            Vec::<String>::new(),
            "the vanished person's exception drops out of the list"
        );
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("except".to_owned(), Vec::<String>::new())],
            "the healed list is re-sent so the account record drops it too"
        );
        app.updates.draft = "hello vanished except".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_X_JID.to_owned(), PERSON_C_JID.to_owned()]],
            "B (the number's new holder) and C receive the post; the dropped exception excludes nobody"
        );
    }

    /// L-1 (qc1650 review 9): the category exception
    /// editor's checked state follows the person. The mirror
    /// entry names person A under their old number; after a
    /// mid-session re-point their editor row carries the new
    /// number, and the row must display as excepted (the
    /// account still excepts them) and unchecking it must
    /// remove the retired-form entry. The comparison and
    /// the toggle go through the session's stored-JID
    /// resolution, the same one the status surfaces use;
    /// the draw itself is immediate-mode egui and not
    /// reachable in this harness, so the pin is on the two
    /// helpers the draw calls.
    #[test]
    fn privacy_exception_editor_follows_a_repoint() {
        const PERSON_A_OLD: &str = "6281111111111@s.whatsapp.net";
        const PERSON_A_NEW: &str = "6289999999999@s.whatsapp.net";
        const PERSON_A_LID: &str = "111111111111111@lid";
        const PERSON_B_JID: &str = "6282222222222@s.whatsapp.net";
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_OLD, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_B_JID, "Person B", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.set_privacy_exceptions("last", vec![PERSON_A_OLD.to_owned()]);
        let mirror = app.privacy_exception_lists.get("last").unwrap().clone();
        assert!(
            app.privacy_exception_contains(&mirror, PERSON_A_OLD),
            "checked before the re-point"
        );
        assert!(!app.privacy_exception_contains(&mirror, PERSON_B_JID), "person B is not excepted");
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_NEW, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_B_JID, "Person B", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            app.privacy_exception_contains(&mirror, PERSON_A_NEW),
            "the row under the new number displays as excepted"
        );
        assert_eq!(
            app.privacy_exception_toggled(&mirror, PERSON_A_NEW, true),
            Vec::<String>::new(),
            "unchecking the row removes the entry recorded under the retired number"
        );
        assert_eq!(
            app.privacy_exception_toggled(&mirror, PERSON_B_JID, false),
            vec![PERSON_A_OLD.to_owned(), PERSON_B_JID.to_owned()],
            "checking another row appends its current canonical key"
        );
    }

    /// M-1 (qc1651 review 10): a number handoff inside ONE
    /// report step. B changes number (pnB to pnC) and A
    /// changes into the number B just vacated (pnA to pnB),
    /// so the step learns two follows in a chain: pnA -> pnB
    /// (A's move) and pnB -> pnC (B's move, out of the key A
    /// now stands under). The stored exception names pnA.
    /// The heal must stop at pnB, the canonical key the
    /// first follow lands on: that key's own outgoing edge
    /// is the other person's move. Before this fix the
    /// follow-first walk followed both edges, healed the
    /// entry to pnC, and re-dispatched it, so the account
    /// record itself excepted B in A's place and A received
    /// the status at pnB.
    #[test]
    fn status_except_mode_follows_the_first_move_of_a_same_step_number_handoff() {
        const PERSON_A_OLD: &str = "6281111111111@s.whatsapp.net";
        const SHARED_NUMBER: &str = "6282222222222@s.whatsapp.net";
        const PERSON_B_NEW: &str = "6289999999999@s.whatsapp.net";
        const PERSON_A_LID: &str = "111111111111111@lid";
        const PERSON_B_LID: &str = "222222222222222@lid";
        const PERSON_C_JID: &str = "6283333333333@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_OLD, "Person A", &[PERSON_A_LID]),
                saved_contact(SHARED_NUMBER, "Person B", &[PERSON_B_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.set_status_privacy("except", vec![PERSON_A_OLD.to_owned()]);
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("except".to_owned(), vec![PERSON_A_OLD.to_owned()])],
            "the editor's write carries the old number placement"
        );
        // The handoff lands as a single report: B under the
        // new number with B's LID, A under B's vacated number
        // with A's LID, C unchanged.
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_B_NEW, "Person B", &[PERSON_B_LID]),
                saved_contact(SHARED_NUMBER, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.updates.status_privacy_list,
            vec![SHARED_NUMBER.to_owned()],
            "the stored exception followed person A to the number they took, not one move further"
        );
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("except".to_owned(), vec![SHARED_NUMBER.to_owned()])],
            "the healed list re-sent to the account record names person A's new number"
        );
        app.updates.draft = "hello handoff chain".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_C_JID.to_owned(), PERSON_B_NEW.to_owned()]],
            "person A receives nothing at the number they took; B (not excepted) and C receive"
        );
    }

    /// M-1 second manifestation (qc1651 review 10): a
    /// same-step swap. A and B exchange numbers in one
    /// report step, so the two follows form a cycle
    /// (pnA -> pnB, pnB -> pnA). The stored share_with
    /// choice names pnA. Each follow target is a canonical
    /// key of the new report, so the heal stops at the
    /// first key it reaches, which is the entry's own
    /// person's new key, and the post delivers to A at pnB.
    /// Before this fix the walk cycled, the cycle guard
    /// returned the starting form, the heal silently
    /// no-oped, and at post time the unhealed entry
    /// resolved through the current form map to B.
    #[test]
    fn status_share_with_mode_follows_each_person_across_a_same_step_number_swap() {
        const PERSON_A_NUMBER: &str = "6281111111111@s.whatsapp.net";
        const PERSON_B_NUMBER: &str = "6282222222222@s.whatsapp.net";
        const PERSON_A_LID: &str = "111111111111111@lid";
        const PERSON_B_LID: &str = "222222222222222@lid";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_A_NUMBER, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_B_NUMBER, "Person B", &[PERSON_B_LID]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.set_status_privacy("share_with", vec![PERSON_A_NUMBER.to_owned()]);
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("share_with".to_owned(), vec![PERSON_A_NUMBER.to_owned()])],
            "the editor's write carries the number placement"
        );
        // The swap lands as a single report: A under B's
        // number with A's LID, B under A's number with B's
        // LID.
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_B_NUMBER, "Person A", &[PERSON_A_LID]),
                saved_contact(PERSON_A_NUMBER, "Person B", &[PERSON_B_LID]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.updates.status_privacy_list,
            vec![PERSON_B_NUMBER.to_owned()],
            "the stored choice followed person A to the number they took in the swap"
        );
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("share_with".to_owned(), vec![PERSON_B_NUMBER.to_owned()])],
            "the healed list is re-sent so the account record heals too"
        );
        app.updates.draft = "hello swap".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_B_NUMBER.to_owned()]],
            "the post delivers to person A at their new number, not to B at A's old one"
        );
    }

    /// M-1 regression pin (qc1651 review 10): the stop rule
    /// must not shorten the legitimate transitive walk.
    /// Person A changes number across TWO successive steps
    /// (pn1 to pn2, then pn2 to pn3) while the stored list
    /// is empty; a record the phone wrote under pn1 is
    /// adopted only afterwards. The intermediate pn2 is
    /// absent from the current form map (it retired in the
    /// second step and nobody holds it), so the heal walk
    /// passes through it and lands on pn3: the adoption
    /// case qc1650 pinned, re-pinned against the walk that
    /// now stops at canonical keys.
    #[test]
    fn status_list_adopted_after_two_repoints_still_walks_the_whole_chain() {
        const PERSON_A_FIRST: &str = "6281111111111@s.whatsapp.net";
        const PERSON_A_SECOND: &str = "6285555555555@s.whatsapp.net";
        const PERSON_A_THIRD: &str = "6289999999999@s.whatsapp.net";
        const PERSON_A_LID: &str = "111111111111111@lid";
        const PERSON_B_JID: &str = "6282222222222@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        for number in [PERSON_A_FIRST, PERSON_A_SECOND, PERSON_A_THIRD] {
            tx.send(BackendEvent::SavedContacts {
                contacts: vec![
                    saved_contact(number, "Person A", &[PERSON_A_LID]),
                    saved_contact(PERSON_B_JID, "Person B", &[]),
                ],
            })
            .unwrap();
            drain_all(&mut app, &ctx);
        }
        assert!(
            status_privacy_writes(&cmd_rx).is_empty(),
            "reports landing with an empty stored list dispatch nothing"
        );
        let mut state = std::collections::BTreeMap::new();
        state.insert("statusprivacy".to_owned(), "except".to_owned());
        state.insert("statusprivacy_list".to_owned(), PERSON_A_FIRST.to_owned());
        tx.send(BackendEvent::PrivacyState(state)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.updates.status_privacy_list,
            vec![PERSON_A_THIRD.to_owned()],
            "the adopted entry walked both retired edges and landed on the current number"
        );
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("except".to_owned(), vec![PERSON_A_THIRD.to_owned()])],
            "the heal is re-sent so the account record follows too"
        );
        app.updates.draft = "hello adopted chain".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_B_JID.to_owned()]],
            "the adopted exception excepts person A at their current number"
        );
    }

    /// M-1 (qc1652 review 11): the late-adoption two-step
    /// case the qc1652 stop rule misanswered. Person P moves
    /// pn1 to pn2 in one report step and pn2 to pn3 in the
    /// next, and pn2 is recycled to saved contact R in the
    /// very step of the second move. A status record the
    /// phone wrote under pn1 is first fetched only after
    /// both steps. The session holds both retired edges and
    /// each speaks for P: the second was learned a step
    /// later than the first, so the adoption heal follows
    /// both and lands on pn3, even though pn2 stands in the
    /// current form map as R's key. Under the stop rule the
    /// walk halted at pn2 and the heal named R in P's place,
    /// in the mirror and in the account record alike.
    #[test]
    fn status_except_mode_follows_a_record_adopted_after_the_intermediate_number_was_recycled() {
        const PERSON_P_FIRST: &str = "6281111111111@s.whatsapp.net";
        const PERSON_P_SECOND: &str = "6285555555555@s.whatsapp.net";
        const PERSON_P_THIRD: &str = "6289999999999@s.whatsapp.net";
        const PERSON_P_LID: &str = "111111111111111@lid";
        const PERSON_R_LID: &str = "222222222222222@lid";
        const PERSON_C_JID: &str = "6283333333333@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        // First report: P under pn1, C alongside.
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_P_FIRST, "Person P", &[PERSON_P_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        // Second report: P moves pn1 to pn2.
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_P_SECOND, "Person P", &[PERSON_P_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        // Third report: P moves pn2 to pn3, and pn2 is
        // recycled to R in the same report.
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_P_THIRD, "Person P", &[PERSON_P_LID]),
                saved_contact(PERSON_P_SECOND, "Person R", &[PERSON_R_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            status_privacy_writes(&cmd_rx).is_empty(),
            "reports landing with an empty stored list dispatch nothing"
        );
        // The record naming pn1 is first fetched only now,
        // after both moves have landed.
        let mut state = std::collections::BTreeMap::new();
        state.insert("statusprivacy".to_owned(), "except".to_owned());
        state.insert("statusprivacy_list".to_owned(), PERSON_P_FIRST.to_owned());
        tx.send(BackendEvent::PrivacyState(state)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.updates.status_privacy_list,
            vec![PERSON_P_THIRD.to_owned()],
            "the adopted entry followed both of P's moves and landed on pn3, not on pn2's new holder"
        );
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("except".to_owned(), vec![PERSON_P_THIRD.to_owned()])],
            "the heal re-sent to the account record names pn3, not R"
        );
        app.updates.draft = "hello late adoption".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_C_JID.to_owned(), PERSON_P_SECOND.to_owned()]],
            "P is excepted at pn3; R (not excepted) and C receive"
        );
    }

    /// M-1 mirror (qc1652 review 11): the same two moves
    /// and the same late adoption, with the record naming
    /// pn1 as the only share_with choice. The heal follows
    /// both edges to pn3, so the post delivers to P at pn3;
    /// R, who merely holds the intermediate number now,
    /// receives nothing. Under the stop rule the choice
    /// healed to pn2 and the post delivered to R instead of
    /// P.
    #[test]
    fn status_share_with_mode_follows_a_record_adopted_after_the_intermediate_number_was_recycled()
    {
        const PERSON_P_FIRST: &str = "6281111111111@s.whatsapp.net";
        const PERSON_P_SECOND: &str = "6285555555555@s.whatsapp.net";
        const PERSON_P_THIRD: &str = "6289999999999@s.whatsapp.net";
        const PERSON_P_LID: &str = "111111111111111@lid";
        const PERSON_R_LID: &str = "222222222222222@lid";
        const PERSON_C_JID: &str = "6283333333333@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_P_FIRST, "Person P", &[PERSON_P_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_P_SECOND, "Person P", &[PERSON_P_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(PERSON_P_THIRD, "Person P", &[PERSON_P_LID]),
                saved_contact(PERSON_P_SECOND, "Person R", &[PERSON_R_LID]),
                saved_contact(PERSON_C_JID, "Person C", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            status_privacy_writes(&cmd_rx).is_empty(),
            "reports landing with an empty stored list dispatch nothing"
        );
        let mut state = std::collections::BTreeMap::new();
        state.insert("statusprivacy".to_owned(), "share_with".to_owned());
        state.insert("statusprivacy_list".to_owned(), PERSON_P_FIRST.to_owned());
        tx.send(BackendEvent::PrivacyState(state)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.updates.status_privacy_list,
            vec![PERSON_P_THIRD.to_owned()],
            "the adopted choice followed both of P's moves and landed on pn3, not on pn2's new holder"
        );
        assert_eq!(
            status_privacy_writes(&cmd_rx),
            vec![("share_with".to_owned(), vec![PERSON_P_THIRD.to_owned()])],
            "the heal re-sent to the account record names pn3, not R"
        );
        app.updates.draft = "hello late adoption".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_P_THIRD.to_owned()]],
            "the post delivers to P at pn3, not to R at the intermediate number"
        );
    }

    /// M-1 companion (qc1645 review 3): a SavedContacts report
    /// that omits a contact (the removal case) shrinks the
    /// status audience with it: the removed contact receives
    /// nothing from the next post.
    #[test]
    fn status_audience_shrinks_when_a_contact_is_removed() {
        const PERSON_B_JID: &str = "6282222222222@s.whatsapp.net";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![
                saved_contact(SAVED_JID, "Person A", &[]),
                saved_contact(PERSON_B_JID, "Person B", &[]),
            ],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.updates.status_privacy_mode = "contacts".to_owned();
        app.updates.draft = "hello both".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![SAVED_JID.to_owned(), PERSON_B_JID.to_owned()]],
            "contacts mode posts to both saved contacts"
        );
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![saved_contact(PERSON_B_JID, "Person B", &[])],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        app.updates.draft = "hello after removal".to_owned();
        app.post_my_status();
        assert_eq!(
            posted_recipients(&cmd_rx),
            vec![vec![PERSON_B_JID.to_owned()]],
            "after the removal report only the remaining contact receives the post"
        );
    }

    /// H-1 (qc1645 review 3): the people picker offers one
    /// row per person: a DM chat addressed under person A's
    /// LID form and A's saved-contact entry (canonical PN,
    /// LID as alternate) collapse into a single option keyed
    /// by the canonical PN JID.
    #[test]
    fn privacy_contact_options_dedup_by_person_across_jid_forms() {
        const PERSON_A_LID: &str = "111111111111111@lid";
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert_titled(PERSON_A_LID, "Saved Sarah")).unwrap();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![saved_contact(SAVED_JID, "Saved Sarah", &[PERSON_A_LID])],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.privacy_contact_options(),
            vec![(SAVED_JID.to_owned(), "Saved Sarah".to_owned())],
            "exactly one option for person A, keyed by the canonical PN JID"
        );
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

    /// A generic backend error must not mark an in-flight
    /// send failed: the error arm carries every remaining
    /// failure, and an unrelated one (a rename, a lookup) says
    /// nothing about the send. Even a worded error that names
    /// sending is not a send outcome anymore: chat send
    /// failures arrive typed as MessageSendFailed, and only
    /// that event marks an echo.
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
        assert_eq!(
            delivery_of(&app),
            Some(Delivery::Sending),
            "a worded error is not a send outcome either; the typed event is"
        );
    }

    /// qc1644: a MessageSendFailed binds per chat. Two chats
    /// hold pending echoes, the other chat's echo OLDER than
    /// the named chat's; the failure for one chat fails only
    /// its own echo (the old global-oldest binding would have
    /// failed the other chat's), and the notice is the
    /// localized sentence for the failure kind.
    #[test]
    pub(super) fn send_failure_binds_to_its_own_chat_only() {
        const OTHER_JID: &str = "6289999999999@s.whatsapp.net";
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        tx.send(dm_upsert(OTHER_JID)).unwrap();
        drain_all(&mut app, &ctx);
        let chat_id = jid_id(CHAT_JID);
        let other_id = jid_id(OTHER_JID);
        {
            let session = app.live.as_mut().unwrap();
            // The other chat's echo is the globally oldest:
            // exactly the row the old binding would have failed.
            let older = Message {
                delivery: Some(Delivery::Sending),
                ..Message::mine(9_000_002, "other chat first".to_owned(), "…")
            };
            session.messages.entry(other_id).or_default().push((BASE_TS, older));
            let newer = Message {
                delivery: Some(Delivery::Sending),
                ..Message::mine(9_000_003, "this chat second".to_owned(), "…")
            };
            session.messages.entry(chat_id).or_default().push((BASE_TS + 60, newer));
        }
        tx.send(BackendEvent::MessageSendFailed {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "3EB0FA11URE0000000001".to_owned(),
            failure: backend::SendFailure {
                kind: backend::SendFailureKind::Timeout,
                stage: backend::SendFailureStage::Write,
            },
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert_eq!(
            session.messages[&chat_id][0].1.delivery,
            Some(Delivery::Failed),
            "the named chat's echo fails"
        );
        assert_eq!(
            session.messages[&other_id][0].1.delivery,
            Some(Delivery::Sending),
            "the other chat's older echo is untouched"
        );
        assert_eq!(
            app.notice_str(),
            Some(crate::i18n::t("notice.send_failed_timeout").as_str()),
            "the notice is the localized timeout sentence for the failure kind"
        );
    }

    /// qc1644: Resend re-dispatches a failed text echo's send
    /// under the echo's OWN protocol ID (the ID its failure
    /// bound, so a retry after a post-write timeout is a
    /// duplicate the recipient dedupes, not a second copy) and
    /// flips the same echo back to Sending; a message that is
    /// not failed, and a failed media echo (no retained bytes),
    /// dispatch nothing.
    #[test]
    pub(super) fn resend_redispatches_a_failed_text_echo_only() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        drain_all(&mut app, &ctx);
        let chat_id = jid_id(CHAT_JID);
        app.selected_chat = chat_id;
        {
            let session = app.live.as_mut().unwrap();
            let failed = Message {
                delivery: Some(Delivery::Failed),
                ..Message::mine(9_000_006, "send me again".to_owned(), "…")
            };
            session.messages.entry(chat_id).or_default().push((BASE_TS, failed));
            let mut media = Message::mine(9_000_007, String::new(), "…");
            media.kind = MessageKind::Image { caption: String::new(), view_once: false };
            media.delivery = Some(Delivery::Failed);
            session.messages.entry(chat_id).or_default().push((BASE_TS + 60, media));
        }
        // The failure that marked the echo bound its protocol
        // ID, the binding a same-ID resend reuses.
        app.protocol_ids.insert(9_000_006, "3EB0RESEND000000000001".to_owned());
        assert!(app.resend_failed_message(9_000_006), "a failed text echo resends");
        match cmd_rx.try_recv() {
            Ok(BackendCommand::SendText { chat_jid, send_id, body, .. }) => {
                assert_eq!(chat_jid, CHAT_JID);
                assert_eq!(body, "send me again");
                assert_eq!(
                    send_id.as_deref(),
                    Some("3EB0RESEND000000000001"),
                    "the retry carries the original send's protocol ID"
                );
            }
            other => panic!("expected a same-ID SendText, got {other:?}"),
        }
        let session = app.live.as_ref().unwrap();
        assert_eq!(
            session.messages[&chat_id][0].1.delivery,
            Some(Delivery::Sending),
            "the same echo is pending again"
        );
        assert!(!app.resend_failed_message(9_000_006), "a pending echo does not resend twice");
        assert!(!app.resend_failed_message(9_000_007), "a media echo retains no bytes to resend");
        assert!(cmd_rx.try_recv().is_err(), "the refused resends sent nothing");
    }

    /// qc1644 outbox: a Failed echo is unconfirmed, not dead.
    /// When the backend's own upsert for the same protocol ID
    /// lands later (the send had in fact reached the server),
    /// the upsert takes the Failed echo's slot: one row, under
    /// the protocol ID, in the confirmed state.
    #[test]
    pub(super) fn failed_echo_reconciles_with_a_later_upsert() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        drain_all(&mut app, &ctx);
        let chat_id = jid_id(CHAT_JID);
        {
            let session = app.live.as_mut().unwrap();
            let failed = Message {
                delivery: Some(Delivery::Failed),
                ..Message::mine(9_000_020, "landed anyway".to_owned(), "\u{2026}")
            };
            session.messages.entry(chat_id).or_default().push((BASE_TS, failed));
        }
        // The binding the failure event recorded: this synthetic
        // echo is the send that carried proto-landed-1.
        app.protocol_ids.insert(9_000_020, "proto-landed-1".to_owned());
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "proto-landed-1",
            CHAT_JID,
            None,
            "landed anyway",
            BASE_TS,
            true,
        )))
        .unwrap();
        drain_all(&mut app, &ctx);
        let rows = &app.live.as_ref().unwrap().messages[&chat_id];
        assert_eq!(rows.len(), 1, "the upsert reconciles the echo, never duplicates it");
        assert_eq!(rows[0].1.id, message_id(CHAT_JID, "proto-landed-1"));
        assert_eq!(
            rows[0].1.delivery,
            Some(Delivery::Sent),
            "the confirmed state replaces the unconfirmed failure"
        );
    }

    /// qc1644 outbox: a delivery receipt carrying the protocol
    /// ID of a Failed echo filed under that ID (the compose-time
    /// text path) advances the echo itself to the receipt's
    /// state: the failure was a post-write timeout and the
    /// message had landed.
    #[test]
    pub(super) fn failed_echo_reconciles_with_a_later_receipt() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        drain_all(&mut app, &ctx);
        let chat_id = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "proto-landed-2");
        {
            let session = app.live.as_mut().unwrap();
            let failed = Message {
                delivery: Some(Delivery::Failed),
                ..Message::mine(mid, "landed anyway".to_owned(), "\u{2026}")
            };
            session.messages.entry(chat_id).or_default().push((BASE_TS, failed));
        }
        tx.send(BackendEvent::Receipt {
            chat_jid: CHAT_JID.to_owned(),
            message_ids: vec!["proto-landed-2".to_owned()],
            kind: ReceiptKind::Delivered,
            at: BASE_TS + 30,
            actor: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let rows = &app.live.as_ref().unwrap().messages[&chat_id];
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.delivery, Some(Delivery::Delivered));
    }

    /// qc1644 outbox: when the confirmed row was already filed
    /// under the protocol ID (an upsert that arrived while the
    /// echo could not be matched), a later receipt collapses
    /// the leftover Failed echo into it: one row remains, in
    /// the receipt's state, and the position index still
    /// resolves it.
    #[test]
    pub(super) fn receipt_collapses_a_failed_echo_duplicate() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        drain_all(&mut app, &ctx);
        let chat_id = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "proto-landed-3");
        {
            let session = app.live.as_mut().unwrap();
            let confirmed = Message {
                delivery: Some(Delivery::Sent),
                ..Message::mine(mid, "twice on paper".to_owned(), "10:00")
            };
            session.messages.entry(chat_id).or_default().push((BASE_TS, confirmed));
            let failed = Message {
                delivery: Some(Delivery::Failed),
                ..Message::mine(9_000_021, "twice on paper".to_owned(), "\u{2026}")
            };
            session.messages.entry(chat_id).or_default().push((BASE_TS, failed));
        }
        app.protocol_ids.insert(9_000_021, "proto-landed-3".to_owned());
        tx.send(BackendEvent::Receipt {
            chat_jid: CHAT_JID.to_owned(),
            message_ids: vec!["proto-landed-3".to_owned()],
            kind: ReceiptKind::Delivered,
            at: BASE_TS + 30,
            actor: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_mut().unwrap();
        let rows = &session.messages[&chat_id];
        assert_eq!(rows.len(), 1, "the failed duplicate collapses into the confirmed row");
        assert_eq!(rows[0].1.id, mid);
        assert_eq!(rows[0].1.delivery, Some(Delivery::Delivered));
        let session = app.live.as_mut().unwrap();
        assert_eq!(
            session.message_position(chat_id, mid),
            Some(0),
            "the index resolves the surviving row after the collapse"
        );
    }

    /// qc1644: a failed text send restores its body into the
    /// chat's composer draft when that draft is empty, and
    /// never overwrites text typed since. The failed chat here
    /// is not the open one, so the restore lands in the parked
    /// draft map, the same persistence a typed draft uses.
    #[test]
    pub(super) fn send_failure_restores_the_text_into_an_empty_draft_only() {
        const OTHER_JID: &str = "6289999999999@s.whatsapp.net";
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        tx.send(dm_upsert(OTHER_JID)).unwrap();
        drain_all(&mut app, &ctx);
        let other_id = jid_id(OTHER_JID);
        let fail = |tx: &std::sync::mpsc::Sender<BackendEvent>, message_id: &str| {
            tx.send(BackendEvent::MessageSendFailed {
                chat_jid: OTHER_JID.to_owned(),
                message_id: message_id.to_owned(),
                failure: backend::SendFailure {
                    kind: backend::SendFailureKind::Offline,
                    stage: backend::SendFailureStage::Other,
                },
            })
            .unwrap();
        };
        {
            let session = app.live.as_mut().unwrap();
            let echo = Message {
                delivery: Some(Delivery::Sending),
                ..Message::mine(9_000_004, "restore me".to_owned(), "…")
            };
            session.messages.entry(other_id).or_default().push((BASE_TS, echo));
        }
        fail(&tx, "3EB0FA11URE0000000002");
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.composer.chat_drafts.get(&other_id).map(String::as_str),
            Some("restore me"),
            "the failed text returns to its chat's draft"
        );
        // A second failure for the same chat, with a draft the
        // user has since typed, leaves that draft alone.
        {
            let session = app.live.as_mut().unwrap();
            let echo = Message {
                delivery: Some(Delivery::Sending),
                ..Message::mine(9_000_005, "do not restore over".to_owned(), "…")
            };
            session.messages.entry(other_id).or_default().push((BASE_TS + 60, echo));
        }
        app.composer.chat_drafts.insert(other_id, "typed since".to_owned());
        fail(&tx, "3EB0FA11URE0000000003");
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.composer.chat_drafts.get(&other_id).map(String::as_str),
            Some("typed since"),
            "a non-empty draft is never clobbered"
        );
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
                from_history: false,
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

    /// QC1626: a session that starts on an empty archive
    /// leaves the selection at the sentinel (0); the first
    /// chat to land must become the open conversation, or the
    /// shell's fallback header sits over a stream that never
    /// loads (every sync keys on the raw selection id) until
    /// the user clicks the row. Later chats never move the
    /// adopted selection.
    #[test]
    pub(super) fn first_chat_landing_adopts_the_selection() {
        let upsert = |jid: &str, title: &str| {
            BackendEvent::ChatUpsert(LiveChat {
                jid: jid.to_owned(),
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
                from_history: false,
            })
        };
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.selected_chat = 0;
        app.chats.clear();
        app.conversation.clear();
        tx.send(upsert(CHAT_JID, "First Chat")).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.selected_chat, jid_id(CHAT_JID), "the first chat is adopted");
        // A message for the adopted chat reaches the open
        // conversation with no click anywhere.
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "m-adopt-1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "hello".to_owned(),
            BASE_TS + 1,
            false,
        )))
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.conversation.len(), 1, "the adopted chat's stream loads");
        // A second chat landing later never steals it.
        const OTHER: &str = "6289999999999@s.whatsapp.net";
        tx.send(upsert(OTHER, "Second Chat")).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.selected_chat, jid_id(CHAT_JID), "the selection stays adopted");
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
                from_history: false,
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
            from_history: false,
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

    /// qc1644: a direct chat whose title already advanced to
    /// the formatted phone (the LID-to-phone heal landed, no
    /// name was known then) is still reported for resolution
    /// on connect, like a raw-titled chat: the formatted phone
    /// is a healable state, and without the report a name the
    /// identity book learns later can never replace the number
    /// on a later launch, so a saved contact would show as a
    /// bare number in every chat row indefinitely. A resolved
    /// title is never reported.
    #[test]
    pub(super) fn phone_titled_dm_is_reported_for_resolution_on_connect() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let dm = |jid: &str, title: &str| {
            BackendEvent::ChatUpsert(LiveChat {
                jid: jid.to_owned(),
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
                from_history: false,
            })
        };
        tx.send(dm(CHAT_JID, "+6281234567890")).unwrap();
        tx.send(dm("6285554443333@s.whatsapp.net", "6285554443333")).unwrap();
        tx.send(dm("6289998887777@s.whatsapp.net", "Named Nadia")).unwrap();
        drain_all(&mut app, &ctx);
        tx.send(BackendEvent::Connected { account: "Test account".to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        let reported: Vec<String> = cmd_rx
            .try_iter()
            .filter_map(|cmd| match cmd {
                BackendCommand::ResolveTitles { jids } => Some(jids),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(
            reported.iter().any(|jid| jid.as_str() == CHAT_JID),
            "the phone-titled chat is reported for resolution: {reported:?}"
        );
        assert!(
            reported.iter().any(|jid| jid.as_str() == "6285554443333@s.whatsapp.net"),
            "the raw-titled chat is still reported: {reported:?}"
        );
        assert!(
            !reported.iter().any(|jid| jid.as_str() == "6289998887777@s.whatsapp.net"),
            "a resolved title is never reported: {reported:?}"
        );
        // The heal the report exists to trigger: when the
        // backend answers with the learned name, the mirror
        // title upgrades from the number.
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
            group_jid: None,
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
            group_jid: None,
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
            group_jid: None,
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
            group_jid: None,
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
            group_jid: None,
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

    /// The badge hangs on the envelope's asserted tier, not
    /// on the certified name: a usync answer and a history
    /// payload can both prove the name (every business
    /// account holds one) without badging the chat, a lower
    /// asserted tier does not badge either, and only the top
    /// tier sets the flag the list and the header paint from.
    #[test]
    pub(super) fn verified_badge_requires_the_top_tier() {
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
            from_history: false,
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
        assert!(
            !chat.verified,
            "a proven certified name alone does not badge: every business account holds one"
        );
        // A history payload stating the name seeds the book
        // too, and still does not badge without a tier.
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
        {
            let session = app.live.as_ref().unwrap();
            assert_eq!(
                session.verified_names.get(CHAT_JID).map(String::as_str),
                Some("Toko Berkah")
            );
        }
        app.apply_verified_badges();
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(!chat.verified, "the name in the book is not the badge");
        // A lower asserted tier does not badge either.
        let mut low = LiveMessage::text(
            "biz-2",
            CHAT_JID,
            Some("Toko Berkah".to_owned()),
            "still not verified",
            BASE_TS,
            false,
        );
        low.sender_jid = Some(CHAT_JID.to_owned());
        low.sender_verified_level = Some("low".to_owned());
        tx.send(BackendEvent::MessageUpsert(low)).unwrap();
        drain_all(&mut app, &ctx);
        app.apply_verified_badges();
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(!chat.verified, "a low asserted tier is not the badge");
        // The top tier badges.
        let mut high = LiveMessage::text(
            "biz-3",
            CHAT_JID,
            Some("Toko Berkah".to_owned()),
            "verified at last",
            BASE_TS,
            false,
        );
        high.sender_jid = Some(CHAT_JID.to_owned());
        high.sender_verified_level = Some("high".to_owned());
        tx.send(BackendEvent::MessageUpsert(high)).unwrap();
        drain_all(&mut app, &ctx);
        app.apply_verified_badges();
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(chat.verified, "the top asserted tier badges the chat");
        // A later assertion below the top tier retracts the
        // badge within the same session (qc1655 L-1): the
        // book holds the latest word, not a ratchet.
        let mut downgraded = LiveMessage::text(
            "biz-4",
            CHAT_JID,
            Some("Toko Berkah".to_owned()),
            "tier dropped",
            BASE_TS,
            false,
        );
        downgraded.sender_jid = Some(CHAT_JID.to_owned());
        downgraded.sender_verified_level = Some("low".to_owned());
        tx.send(BackendEvent::MessageUpsert(downgraded)).unwrap();
        drain_all(&mut app, &ctx);
        app.apply_verified_badges();
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(!chat.verified, "a later low assertion retracts the badge");
    }

    /// qc1655 L-2: the level book is keyed by the form the
    /// envelope carried, and the chat is keyed by the person's
    /// other form once the session knows the pairing: the
    /// badge read follows the counterpart forms instead of
    /// missing, and a downgrade asserted under the exact form
    /// still retracts a badge the counterpart form granted.
    #[test]
    pub(super) fn verified_badge_resolves_the_assertion_across_jid_forms() {
        const LID_JID: &str = "123456789012345@lid";
        let (mut app, _tx, _ctx, _cmd_rx) = live_app();
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
            from_history: false,
            ephemeral_secs: 0,
        }));
        {
            let session = app.live.as_mut().unwrap();
            session.jids.insert(chat_id, CHAT_JID.to_owned());
            // The pairing, as a saved-contacts report teaches
            // it: the LID form maps to the PN canonical.
            session.saved_contact_forms.insert(LID_JID.to_owned(), CHAT_JID.to_owned());
            // The only assertion arrived under the LID form.
            session.verified_levels.insert(LID_JID.to_owned(), "high".to_owned());
        }
        app.apply_verified_badges();
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(chat.verified, "the LID-form top-tier assertion badges the PN-keyed chat");
        // A downgrade under the chat's own form retracts it:
        // the exact form is read first.
        app.live.as_mut().unwrap().verified_levels.insert(CHAT_JID.to_owned(), "low".to_owned());
        app.apply_verified_badges();
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(!chat.verified, "the exact-form downgrade retracts the badge");
    }

    /// qc1656 LOW-1: the tier fold covers the exact form
    /// too. A downgrade asserted under the counterpart form
    /// withholds the badge while the exact form's older
    /// top-tier entry stands: the tier is one account-level
    /// assertion, and the badge hangs on it being
    /// unambiguous, not on which form happens to be read
    /// first.
    #[test]
    pub(super) fn verified_badge_withholds_on_a_counterpart_downgrade() {
        const LID_JID: &str = "123456789012345@lid";
        let (mut app, _tx, _ctx, _cmd_rx) = live_app();
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
            from_history: false,
            ephemeral_secs: 0,
        }));
        {
            let session = app.live.as_mut().unwrap();
            session.jids.insert(chat_id, CHAT_JID.to_owned());
            session.saved_contact_forms.insert(LID_JID.to_owned(), CHAT_JID.to_owned());
            // The exact form asserts the top tier; the
            // counterpart form's latest word is a downgrade.
            session.verified_levels.insert(CHAT_JID.to_owned(), "high".to_owned());
            session.verified_levels.insert(LID_JID.to_owned(), "low".to_owned());
        }
        app.apply_verified_badges();
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(!chat.verified, "a counterpart downgrade withholds the badge");
        // Once the counterpart form asserts the top tier
        // too, the assertion is unambiguous and badges.
        app.live.as_mut().unwrap().verified_levels.insert(LID_JID.to_owned(), "high".to_owned());
        app.apply_verified_badges();
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(chat.verified, "agreement at the top tier badges the chat");
    }

    /// qc1656 LOW-4: the info dock's header resolves its
    /// title through the shared display_chat_title, the
    /// resolution the panel hands the header before painting
    /// it, so the self chat reads "<own name> (You)" in the
    /// dock like on every other surface, where it previously
    /// painted the stored title raw.
    #[test]
    pub(super) fn self_chat_dock_title_carries_the_you_suffix() {
        let (mut app, _tx, _ctx, _cmd_rx) = live_app();
        app.my_profile.name = "Rifan".to_owned();
        let own_jid = backend::fake::FAKE_OWN_JID;
        let live_chat = |jid: &str, title: &str| backend::LiveChat {
            jid: jid.to_owned(),
            kind: backend::LiveChatKind::Dm,
            title: title.to_owned(),
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
            from_history: false,
            ephemeral_secs: 0,
        };
        let self_chat = to_chat(&live_chat(own_jid, "+10000000000"));
        app.live.as_mut().unwrap().jids.insert(jid_id(own_jid), own_jid.to_owned());
        assert_eq!(app.display_chat_title(&self_chat), "Rifan (You)", "the self chat's dock title");
        let other = to_chat(&live_chat(CHAT_JID, "Sender"));
        app.live.as_mut().unwrap().jids.insert(jid_id(CHAT_JID), CHAT_JID.to_owned());
        assert_eq!(app.display_chat_title(&other), "Sender", "another chat keeps its stored title");
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
    /// first), lose nothing, and keep arrival order. Completeness
    /// lives in the archive: the batch flush writes every row
    /// through before the retention trim runs, so the mirror and
    /// the rendered conversation hold the retained window (the
    /// newest rows, qc1654 Item M) while the archive holds all.
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
        let archived =
            app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        assert_eq!(archived.len(), TOTAL as usize, "no event was dropped");
        assert_eq!(archived.first().unwrap().0.body, "message m0");
        assert_eq!(archived.last().unwrap().0.body, format!("message m{}", TOTAL - 1));
        let stored = stored_messages(&app);
        assert_eq!(stored.len(), MIRROR_RETENTION_BACKGROUND, "the mirror holds its window");
        assert!(stored.windows(2).all(|w| w[0].0 < w[1].0), "messages end up sorted by timestamp");
        assert_eq!(
            stored.first().unwrap().1.body,
            format!("message m{}", TOTAL as usize - MIRROR_RETENTION_BACKGROUND),
            "the window starts at the oldest retained message"
        );
        assert_eq!(stored.last().unwrap().1.body, format!("message m{}", TOTAL - 1));
        let chat = app.chats.iter().find(|c| c.id == jid_id(CHAT_JID)).unwrap();
        assert_eq!(chat.unread, u32::try_from(TOTAL).unwrap(), "every upsert counted once");
        assert_eq!(
            app.conversation.len(),
            MIRROR_RETENTION_BACKGROUND,
            "the open conversation is rebuilt from the window"
        );
    }

    /// The flag word the archive holds for one message of
    /// this module's chat, read straight from the store.
    fn archived_flags(app: &App, protocol_id: &str) -> i64 {
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        store
            .messages(CHAT_JID)
            .unwrap()
            .into_iter()
            .find(|(m, _)| m.id == protocol_id)
            .expect("the message reached the archive")
            .1
    }

    /// qc1655 H-1: a message filed in a batch burst and drained
    /// by the same flush's retention trim must keep its star
    /// and pin in the archive. The trim removes the drained
    /// rows' flag-map entries before the flush's write step
    /// resolves flags, so the write must resolve from the
    /// trim's snapshot, not the emptied map. The batch is
    /// applied directly so the construction is exact: one
    /// batch, one flush, the trim inside it.
    #[test]
    pub(super) fn burst_trimmed_in_the_same_flush_keeps_star_and_pin_flags() {
        const TOTAL: i64 = 300;
        let (mut app, _tx, ctx, _cmd_rx) = live_app();
        app.event_batch.active = true;
        for i in 0..TOTAL {
            let mut msg = LiveMessage::text(
                format!("m{i}"),
                CHAT_JID,
                Some("Sender".to_owned()),
                format!("message m{i}"),
                BASE_TS + i,
                false,
            );
            // The drained prefix of the flush (the oldest rows,
            // m0..m49 with a 250 window over 300 filed): one
            // starred, one pinned, one both, one at the drain
            // boundary. m50 is the first retained row and m299
            // sits deep inside the window: both are controls the
            // flag map always covered.
            if i == 0 || i == 2 || i == 49 || i == 50 || i == 299 {
                msg.starred = true;
            }
            if i == 1 || i == 2 {
                msg.pinned = true;
            }
            app.apply_event(&ctx, BackendEvent::MessageUpsert(msg));
        }
        // A star that lands inside the same batch, for a
        // message filed unstarred into the drained prefix: the
        // flag write-through finds no archive row yet, so the
        // flush's write is the only landing, and it must carry
        // the star the map held at drain time.
        app.apply_event(
            &ctx,
            BackendEvent::StarChanged {
                chat_jid: CHAT_JID.to_owned(),
                message_id: "m3".to_owned(),
                starred: true,
            },
        );
        for _ in 0..64 {
            if !app.event_batch.active && !app.event_batch.flush_pending {
                break;
            }
            app.finish_event_batch();
        }
        let archived =
            app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        assert_eq!(archived.len(), TOTAL as usize, "the archive holds the whole burst");
        assert_eq!(
            stored_messages(&app).len(),
            MIRROR_RETENTION_BACKGROUND,
            "the flush's trim really drained the prefix in the same flush"
        );
        assert_eq!(archived_flags(&app, "m0"), store::FLAG_STARRED, "starred at filing, drained");
        assert_eq!(
            archived_flags(&app, "m1"),
            store::FLAG_PINNED_MESSAGE,
            "pinned at filing, drained"
        );
        assert_eq!(
            archived_flags(&app, "m2"),
            store::FLAG_STARRED | store::FLAG_PINNED_MESSAGE,
            "starred and pinned at filing, drained"
        );
        assert_eq!(
            archived_flags(&app, "m3"),
            store::FLAG_STARRED,
            "starred inside the batch, drained"
        );
        assert_eq!(
            archived_flags(&app, "m49"),
            store::FLAG_STARRED,
            "starred at the drain boundary"
        );
        assert_eq!(
            archived_flags(&app, "m50"),
            store::FLAG_STARRED,
            "the first retained row keeps its star"
        );
        assert_eq!(
            archived_flags(&app, "m299"),
            store::FLAG_STARRED,
            "the in-window control keeps its star"
        );
        assert_eq!(archived_flags(&app, "m4"), 0, "an unflagged drained row stays unflagged");
    }

    /// qc1656 MEDIUM-1: the same message queued twice in one
    /// batch (an overlapping history page, a receipt-driven
    /// re-upsert) must keep its flags even when the flush's
    /// trim drained it. The trim snapshot is consumed by the
    /// first copy's flag resolution; the winning second copy
    /// must share that resolution, not fall through to the
    /// emptied flag map and store 0 over it. Same
    /// construction as the H-1 test, with the drained prefix's
    /// flagged message filed a second time late in the batch.
    #[test]
    pub(super) fn burst_trimmed_message_queued_twice_keeps_its_flags() {
        const TOTAL: i64 = 300;
        let (mut app, _tx, ctx, _cmd_rx) = live_app();
        app.event_batch.active = true;
        let flagged = |id: &str, ts: i64| {
            let mut msg = LiveMessage::text(
                id.to_owned(),
                CHAT_JID,
                Some("Sender".to_owned()),
                format!("message {id}"),
                ts,
                false,
            );
            msg.starred = true;
            msg.pinned = true;
            msg
        };
        for i in 0..TOTAL {
            if i == 0 {
                app.apply_event(&ctx, BackendEvent::MessageUpsert(flagged("m0", BASE_TS)));
            } else {
                app.apply_event(
                    &ctx,
                    BackendEvent::MessageUpsert(LiveMessage::text(
                        format!("m{i}"),
                        CHAT_JID,
                        Some("Sender".to_owned()),
                        format!("message m{i}"),
                        BASE_TS + i,
                        false,
                    )),
                );
            }
        }
        // The duplicate filing of the drained message, as an
        // overlapping page or a re-upsert in the same burst
        // delivers it: a second queued copy of m0.
        app.apply_event(&ctx, BackendEvent::MessageUpsert(flagged("m0", BASE_TS)));
        for _ in 0..64 {
            if !app.event_batch.active && !app.event_batch.flush_pending {
                break;
            }
            app.finish_event_batch();
        }
        let archived =
            app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        assert_eq!(archived.len(), TOTAL as usize, "the archive holds the whole burst");
        assert_eq!(
            stored_messages(&app).len(),
            MIRROR_RETENTION_BACKGROUND,
            "the flush's trim really drained the prefix in the same flush"
        );
        assert_eq!(
            archived_flags(&app, "m0"),
            store::FLAG_STARRED | store::FLAG_PINNED_MESSAGE,
            "the duplicated drained message keeps its star and pin"
        );
    }

    /// qc1655 H-1 sibling audit: a server flag report for a
    /// message the session no longer mirrors (its flag-map
    /// entry went with the retention trim; the archive row is
    /// the only record of its word) must change the one bit in
    /// the stored word, not rewrite the word from the session's
    /// empty view. The post-trim state is constructed directly:
    /// the archive carries STARRED | PINNED and the flag map
    /// holds no entry, exactly what the trim leaves behind.
    #[test]
    pub(super) fn server_flag_report_on_an_unmirrored_message_keeps_its_other_bits() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mid = message_id(CHAT_JID, "s1");
        let mut msg = LiveMessage::text(
            "s1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "star and pin me",
            BASE_TS,
            false,
        );
        msg.starred = true;
        msg.pinned = true;
        tx.send(BackendEvent::MessageUpsert(msg)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            archived_flags(&app, "s1"),
            store::FLAG_STARRED | store::FLAG_PINNED_MESSAGE,
            "the archive holds both bits before the report"
        );
        app.live.as_mut().unwrap().flags.remove(&mid);

        tx.send(BackendEvent::StarChanged {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "s1".to_owned(),
            starred: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            archived_flags(&app, "s1"),
            store::FLAG_PINNED_MESSAGE,
            "the unstar clears its own bit and keeps the pin"
        );

        tx.send(BackendEvent::StarChanged {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "s1".to_owned(),
            starred: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            archived_flags(&app, "s1"),
            store::FLAG_STARRED | store::FLAG_PINNED_MESSAGE,
            "the star sets its own bit and keeps the pin"
        );
    }

    /// qc1655 M-1: a delete-for-everyone on a starred message
    /// in a chat that is not open must rebuild the Starred
    /// panel's memoized view. The revoke moves no input the
    /// old key folded (the starred set, the roster generation,
    /// titles, the open conversation); it only adds
    /// FLAG_DELETED to the row's flag word, so the word itself
    /// rides the key now.
    #[test]
    pub(super) fn starred_panel_rebuilds_when_a_background_star_is_revoked() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        // The chat is never the open one in this test: its
        // panel rows come from the archive, the surface the
        // memo caches.
        app.selected_chat = 0;
        let mid = message_id(CHAT_JID, "s1");
        let mut msg = LiveMessage::text(
            "s1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "a secret worth revoking",
            BASE_TS,
            false,
        );
        msg.starred = true;
        tx.send(BackendEvent::MessageUpsert(msg)).unwrap();
        drain_all(&mut app, &ctx);
        let entry = {
            let memo = app.starred_view();
            memo.entries
                .iter()
                .find(|(_, _, m)| m.id == mid)
                .map(|(_, _, m)| (m.body.clone(), m.deleted))
        };
        assert_eq!(
            entry,
            Some(("a secret worth revoking".to_owned(), false)),
            "the panel first shows the starred body, not yet deleted"
        );

        tx.send(BackendEvent::MessageRevoked {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "s1".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let entry = {
            let memo = app.starred_view();
            memo.entries
                .iter()
                .find(|(_, _, m)| m.id == mid)
                .map(|(_, _, m)| (m.body.clone(), m.deleted))
        };
        assert_eq!(
            entry.map(|(_, deleted)| deleted),
            Some(true),
            "after the revoke the panel renders the row deleted, not its body"
        );
    }

    /// Files one starred message into a background chat, then
    /// puts it in the seed-window state (qc1657): the archive
    /// holds the row with its star, but the mirror no longer
    /// retains it and the session flag map holds no entry for
    /// it, exactly what the staged seed leaves for a starred
    /// message older than the newest 250 of its chat and what
    /// the trim leaves behind as the mirror slides. Returns
    /// the message's app id.
    pub(super) fn seed_unmirrored_star(
        app: &mut App,
        tx: &std::sync::mpsc::Sender<BackendEvent>,
        ctx: &egui::Context,
    ) -> u64 {
        let mid = message_id(CHAT_JID, "s1");
        let mut msg = LiveMessage::text(
            "s1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "an old star outside the window",
            BASE_TS,
            false,
        );
        msg.starred = true;
        tx.send(BackendEvent::MessageUpsert(msg)).unwrap();
        drain_all(app, ctx);
        assert_eq!(
            archived_flags(app, "s1"),
            store::FLAG_STARRED,
            "the archive holds the star before the surgery"
        );
        let chat_id = jid_id(CHAT_JID);
        let session = app.live.as_mut().unwrap();
        session.flags.remove(&mid);
        if let Some(msgs) = session.messages.get_mut(&chat_id) {
            msgs.retain(|(_, m)| m.id != mid);
        }
        assert!(
            !session.flags.contains_key(&mid)
                && session
                    .messages
                    .get(&chat_id)
                    .is_none_or(|msgs| msgs.iter().all(|(_, m)| m.id != mid)),
            "the message now stands entry-less and unmirrored"
        );
        mid
    }

    /// qc1657 M-1, construction 1: an unstar from another
    /// device for a starred message outside the seed window.
    /// The flag write-through clears the star in the archive,
    /// but the session flag map ends byte-identical (the entry
    /// is created at zero, cleared, and removed again), so
    /// before the fix no input the Starred panel's memo folds
    /// moved and the panel kept showing the star for the rest
    /// of the session.
    #[test]
    pub(super) fn starred_panel_rebuilds_when_an_unmirrored_star_is_unstarred() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.selected_chat = 0;
        let mid = seed_unmirrored_star(&mut app, &tx, &ctx);
        let key_before = app.starred_key();
        {
            let memo = app.starred_view();
            assert!(
                memo.entries.iter().any(|(_, _, m)| m.id == mid),
                "the panel first shows the archived star"
            );
        }

        tx.send(BackendEvent::StarChanged {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "s1".to_owned(),
            starred: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(archived_flags(&app, "s1"), 0, "the unstar landed in the archive");
        assert_ne!(app.starred_key(), key_before, "the memo key moved on the unstar");
        let memo = app.starred_view();
        assert!(
            memo.entries.iter().all(|(_, _, m)| m.id != mid),
            "the rebuilt panel no longer shows the unstarred message"
        );
    }

    /// qc1657 M-1, construction 2: a delete-for-me on another
    /// device for a starred message outside the seed window.
    /// The adoption deletes the archive row, and the forgotten
    /// message's only flag-map contact is a no-op remove, so
    /// before the fix the memo kept displaying a message the
    /// user removed from their account on every device, body
    /// included, for the rest of the session.
    #[test]
    pub(super) fn starred_panel_rebuilds_when_an_unmirrored_star_is_deleted_for_me() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.selected_chat = 0;
        let mid = seed_unmirrored_star(&mut app, &tx, &ctx);
        let key_before = app.starred_key();
        {
            let memo = app.starred_view();
            assert!(
                memo.entries.iter().any(|(_, _, m)| m.id == mid),
                "the panel first shows the archived star"
            );
        }

        tx.send(BackendEvent::MessageDeletedForMe {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "s1".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let archived =
            app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        assert!(archived.is_empty(), "the delete removed the archive row");
        assert_ne!(app.starred_key(), key_before, "the memo key moved on the delete");
        let memo = app.starred_view();
        assert!(
            memo.entries.iter().all(|(_, _, m)| m.id != mid),
            "the rebuilt panel no longer shows the deleted message"
        );
    }

    /// qc1658 M-1: a starred disappearing message outside the
    /// seed window, past its timer. Neither expiry mechanism
    /// can reach it (the index path verifies against the
    /// mirror and drops the pair; the reconciliation scan reads
    /// the mirror only), so before the fix the archive row, and
    /// with it the panel row and the search hit, lived forever.
    /// The archive-side pass on the reconciliation cadence
    /// deletes the row by its own stamp.
    #[test]
    pub(super) fn stranded_expired_star_is_deleted_by_the_archive_pass() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.selected_chat = 0;
        let mid = message_id(CHAT_JID, "s1");
        let mut msg = LiveMessage::text(
            "s1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "stranded timed secret",
            BASE_TS,
            false,
        );
        msg.starred = true;
        msg.expires_at = BASE_TS + 100;
        tx.send(BackendEvent::MessageUpsert(msg)).unwrap();
        drain_all(&mut app, &ctx);
        // The seed-window surgery, as in seed_unmirrored_star:
        // the archive holds the row, the mirror and the flag
        // map hold nothing for it. The drain auto-selected the
        // chat and synced its conversation from the mirror, so
        // the selection and the working copy are reset too:
        // the stranded state the seed leaves is a chat nobody
        // opened, whose row no working copy holds either.
        let chat_id = jid_id(CHAT_JID);
        {
            let session = app.live.as_mut().unwrap();
            session.flags.remove(&mid);
            if let Some(msgs) = session.messages.get_mut(&chat_id) {
                msgs.retain(|(_, m)| m.id != mid);
            }
        }
        app.selected_chat = 0;
        app.conversation.clear();
        assert_eq!(archived_flags(&app, "s1"), store::FLAG_STARRED);
        {
            let memo = app.starred_view();
            assert!(
                memo.entries.iter().any(|(_, _, m)| m.id == mid),
                "the panel first shows the archived star"
            );
        }
        let key_before = app.starred_key();

        app.expire_disappearing_messages(u64::try_from(BASE_TS + 200).unwrap());

        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        assert!(
            store.messages(CHAT_JID).unwrap().is_empty(),
            "the archive pass deleted the stranded expired row"
        );
        assert!(
            store.search_messages("stranded timed secret", 10).unwrap().is_empty(),
            "search no longer finds the expired body"
        );
        assert_ne!(app.starred_key(), key_before, "the memo key moved on the pass");
        let memo = app.starred_view();
        assert!(
            memo.entries.iter().all(|(_, _, m)| m.id != mid),
            "the rebuilt panel no longer shows the expired message"
        );
    }

    /// Seeds a saved download and a cached playback file for
    /// one message in the real data dir, under the stem the
    /// removal helpers match, and returns their paths. Any
    /// leftover from an earlier run is removed first.
    fn seed_media_files(
        chat_jid: &str,
        message_id: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let data_dir = backend::data_dir();
        backend::remove_download_for_message(&data_dir, chat_jid, message_id);
        let stem = backend::downloaded_media_stem(chat_jid, message_id);
        let download = data_dir.join("downloads").join(format!("{stem}.bin"));
        let cache = data_dir.join("video-cache").join(format!("{stem}.mp4"));
        std::fs::create_dir_all(download.parent().expect("the downloads dir")).unwrap();
        std::fs::create_dir_all(cache.parent().expect("the cache dir")).unwrap();
        std::fs::write(&download, b"media").unwrap();
        std::fs::write(&cache, b"media").unwrap();
        (download, cache)
    }

    /// qc1660 MEDIUM-1: the archive expiry pass deletes the
    /// row of an expired message outside the mirror window;
    /// the row's saved download and cached playback file go
    /// with it (privacy audit A3), as they do on the mirror
    /// path. Before the fix the pass returned only a count,
    /// so neither call site could name the rows whose files
    /// had to go, and the plaintext files stayed for the
    /// life of the install. A kept row past its timer and
    /// an unexpired row keep their rows and their files.
    #[test]
    pub(super) fn archive_pass_removes_the_expired_rows_media_files() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.selected_chat = 0;
        for (pid, expires) in
            [("dl1", BASE_TS + 100), ("dl2", BASE_TS + 100), ("dl3", 4_000_000_000)]
        {
            let mut msg = LiveMessage::text(
                pid,
                CHAT_JID,
                Some("Sender".to_owned()),
                format!("timed {pid}"),
                BASE_TS,
                false,
            );
            msg.expires_at = expires;
            tx.send(BackendEvent::MessageUpsert(msg)).unwrap();
        }
        drain_all(&mut app, &ctx);
        // The keep lands on dl2's archive row, the way a
        // keep-in-chat toggle lands it.
        {
            let session = app.live.as_ref().unwrap();
            let store = session.store.as_ref().unwrap();
            assert!(store.update_message_flag(CHAT_JID, "dl2", store::FLAG_KEPT, true).unwrap());
        }
        // The seed-window surgery, as in
        // seed_unmirrored_star: the archive holds the rows,
        // the mirror and the flag map hold nothing for them.
        let chat_id = jid_id(CHAT_JID);
        let mids: Vec<u64> =
            ["dl1", "dl2", "dl3"].iter().map(|pid| message_id(CHAT_JID, pid)).collect();
        {
            let session = app.live.as_mut().unwrap();
            for mid in &mids {
                session.flags.remove(mid);
            }
            if let Some(msgs) = session.messages.get_mut(&chat_id) {
                msgs.retain(|(_, m)| !mids.contains(&m.id));
            }
        }
        app.selected_chat = 0;
        app.conversation.clear();
        let (dl1_download, dl1_cache) = seed_media_files(CHAT_JID, "dl1");
        let (dl2_download, dl2_cache) = seed_media_files(CHAT_JID, "dl2");
        let (dl3_download, dl3_cache) = seed_media_files(CHAT_JID, "dl3");

        app.expire_disappearing_messages(u64::try_from(BASE_TS + 200).unwrap());

        assert!(!dl1_download.exists(), "the expired row's saved download is removed");
        assert!(!dl1_cache.exists(), "the expired row's cached playback file is removed");
        assert!(dl2_download.exists() && dl2_cache.exists(), "the kept row's files survive");
        assert!(dl3_download.exists() && dl3_cache.exists(), "the unexpired row's files survive");
        let archived =
            app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let ids: Vec<&str> = archived.iter().map(|(m, _)| m.id.as_str()).collect();
        assert_eq!(ids, ["dl2", "dl3"], "only the expired unkept row left the archive");
        // Leave no seeded files behind for other tests.
        backend::remove_download_for_message(&backend::data_dir(), CHAT_JID, "dl2");
        backend::remove_download_for_message(&backend::data_dir(), CHAT_JID, "dl3");
    }

    /// qc1661 MEDIUM-1: a settled revoke takes the message's
    /// saved download and cached playback file with it
    /// (privacy audit A3), as every other deletion verb does.
    /// Before the fix apply_revoke tombstoned the mirror entry
    /// and persisted FLAG_DELETED but touched no file, and a
    /// revoked message in a chat without a timer never becomes
    /// expiry-due, so no later pass could reach the files.
    /// An author's revoke of a mirrored message first.
    #[test]
    pub(super) fn settled_revoke_removes_the_messages_media_files() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.selected_chat = 0;
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "rv1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "a video worth revoking",
            BASE_TS,
            false,
        )))
        .unwrap();
        drain_all(&mut app, &ctx);
        let (download, cache) = seed_media_files(CHAT_JID, "rv1");

        tx.send(BackendEvent::MessageRevoked {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "rv1".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);

        assert_eq!(
            archived_flags(&app, "rv1"),
            store::FLAG_DELETED,
            "the tombstone landed in the archive"
        );
        assert!(!download.exists(), "the revoked message's saved download is removed");
        assert!(!cache.exists(), "the revoked message's cached playback file is removed");
    }

    /// qc1661 MEDIUM-1, the unmirrored construction: the same
    /// settle for a message the mirror no longer retains. The
    /// removal resolves the chat JID and protocol ID through
    /// the session's own maps, so the row's files go even
    /// though no mirror entry names the message.
    #[test]
    pub(super) fn settled_revoke_removes_an_unmirrored_messages_media_files() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.selected_chat = 0;
        let mid = message_id(CHAT_JID, "rv2");
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "rv2",
            CHAT_JID,
            Some("Sender".to_owned()),
            "an old video worth revoking",
            BASE_TS,
            false,
        )))
        .unwrap();
        drain_all(&mut app, &ctx);
        // The seed-window surgery, as in seed_unmirrored_star:
        // the archive holds the row, the mirror and the flag
        // map hold nothing for it. The protocol ID entry
        // stays: filing put it there, and only the expiry and
        // delete-for-me paths ever drop it.
        let chat_id = jid_id(CHAT_JID);
        {
            let session = app.live.as_mut().unwrap();
            session.flags.remove(&mid);
            if let Some(msgs) = session.messages.get_mut(&chat_id) {
                msgs.retain(|(_, m)| m.id != mid);
            }
        }
        app.selected_chat = 0;
        app.conversation.clear();
        let (download, cache) = seed_media_files(CHAT_JID, "rv2");

        tx.send(BackendEvent::MessageRevoked {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "rv2".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);

        assert_eq!(
            archived_flags(&app, "rv2"),
            store::FLAG_DELETED,
            "the tombstone landed in the archive"
        );
        assert!(!download.exists(), "the revoked message's saved download is removed");
        assert!(!cache.exists(), "the revoked message's cached playback file is removed");
    }

    /// qc1661 MEDIUM-1, my own delete-for-everyone: the same
    /// removal runs when the backend confirms my revoke. A
    /// revoke the server rejects restores the message and
    /// keeps its files, because the removal lives at the
    /// settled-event site the failure path never reaches.
    #[test]
    pub(super) fn own_revoke_settle_removes_files_and_failed_revoke_keeps_them() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        for pid in ["rv3", "rv4"] {
            tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
                pid,
                CHAT_JID,
                None,
                "my own video",
                BASE_TS,
                true,
            )))
            .unwrap();
        }
        drain_all(&mut app, &ctx);
        let chat_id = jid_id(CHAT_JID);
        app.selected_chat = chat_id;
        app.sync_conversation();
        assert_eq!(app.conversation.len(), 2, "the chat is open with both messages");
        let mid3 = message_id(CHAT_JID, "rv3");
        let mid4 = message_id(CHAT_JID, "rv4");
        let (rv3_download, rv3_cache) = seed_media_files(CHAT_JID, "rv3");
        let (rv4_download, rv4_cache) = seed_media_files(CHAT_JID, "rv4");

        app.delete_for_everyone(mid3);
        assert!(
            app.conversation.iter().find(|m| m.id == mid3).unwrap().deleted,
            "the optimistic apply marks the message deleted"
        );
        tx.send(BackendEvent::MessageRevoked {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "rv3".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!app.pending_revokes.contains_key(&mid3), "the wait is settled");
        assert!(!rv3_download.exists(), "the settled revoke's saved download is removed");
        assert!(!rv3_cache.exists(), "the settled revoke's cached playback file is removed");

        app.delete_for_everyone(mid4);
        tx.send(BackendEvent::RevokeMessageFailed {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "rv4".to_owned(),
            message: "Deleting the message failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            !app.conversation.iter().find(|m| m.id == mid4).unwrap().deleted,
            "the failed revoke restores the message"
        );
        assert!(rv4_download.exists(), "the failed revoke keeps the saved download");
        assert!(rv4_cache.exists(), "the failed revoke keeps the cached playback file");
        // Leave no seeded files behind for other tests.
        backend::remove_download_for_message(&backend::data_dir(), CHAT_JID, "rv4");
    }

    /// Builds the post-restart state the qc1662 tests need:
    /// an archive holding 300 rows for this module's chat, the
    /// oldest of them ("old1") carrying a saved download and a
    /// cached playback file, installed into a FRESH app through
    /// the real staged seed. The seed window keeps the newest
    /// 250 rows, so old1 ends up in the archive alone: no
    /// mirror row, no flag entry, and no protocol ID entry,
    /// exactly as after a restart before the user pages the
    /// chat back. Returns the app, its event channel, and the
    /// two seeded file paths.
    fn restarted_app_with_stranded_message() -> (
        App,
        std::sync::mpsc::Sender<BackendEvent>,
        egui::Context,
        std::sync::mpsc::Receiver<BackendCommand>,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let store = store::Store::open_in_memory().unwrap();
        store
            .upsert_chat(&backend::LiveChat {
                jid: CHAT_JID.to_owned(),
                title: "Sender".to_owned(),
                kind: backend::LiveChatKind::Dm,
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
                from_history: false,
            })
            .unwrap();
        store
            .upsert_message(
                &LiveMessage::text(
                    "old1",
                    CHAT_JID,
                    Some("Sender".to_owned()),
                    "an old video worth revoking",
                    BASE_TS,
                    false,
                ),
                0,
            )
            .unwrap();
        for i in 1..300i64 {
            store
                .upsert_message(
                    &LiveMessage::text(
                        format!("f{i}"),
                        CHAT_JID,
                        Some("Sender".to_owned()),
                        format!("filler {i}"),
                        BASE_TS + i * 60,
                        false,
                    ),
                    0,
                )
                .unwrap();
        }
        let (download, cache) = seed_media_files(CHAT_JID, "old1");
        let load = crate::live::session::load_archive_snapshot(store);
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let (load_tx, load_rx) = std::sync::mpsc::channel();
        app.archive_load = Some(crate::live::PendingArchiveLoad {
            rx: load_rx,
            gen: app.archive_load_gen,
            started: std::time::Instant::now(),
        });
        load_tx.send(load).unwrap();
        app.poll_archive_load();
        let mut slices = 0;
        while app.live.as_ref().is_some_and(|s| s.pending_archive.is_some()) {
            app.poll_archive_load();
            slices += 1;
            assert!(slices < 1_000, "the staged apply did not terminate");
        }
        assert!(app.archive_load.is_none(), "the formal load closed at the finish");
        let chat_id = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "old1");
        let session = app.live.as_ref().unwrap();
        assert_eq!(session.messages[&chat_id].len(), 250, "the seed kept only its window");
        assert!(
            session.messages[&chat_id].iter().all(|(_, m)| m.id != mid),
            "the stranded message is not mirrored"
        );
        assert!(
            !app.protocol_ids.contains_key(&mid),
            "the seed never created the stranded message's protocol ID entry"
        );
        (app, tx, ctx, cmd_rx, download, cache)
    }

    /// qc1662 MEDIUM-1: the post-restart construction the
    /// qc1661 tests could not see. Their unmirrored case was
    /// the trim state (filing had created the protocol ID
    /// entry, and only the mirror row went away); the seed
    /// boundary is a never-create, so a settled revoke for a
    /// message beyond the seed window resolved its protocol
    /// ID through a map with no entry for it: the flag
    /// write-through skipped the archive row and the file
    /// removal removed nothing. The event's own ID now lands
    /// first, as the sibling settled verbs record it.
    #[test]
    pub(super) fn settled_revoke_after_restart_removes_files_and_persists_the_tombstone() {
        let (mut app, tx, ctx, _cmd_rx, download, cache) = restarted_app_with_stranded_message();
        assert_eq!(archived_flags(&app, "old1"), 0, "the archive row starts unflagged");

        tx.send(BackendEvent::MessageRevoked {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "old1".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);

        assert_eq!(
            archived_flags(&app, "old1"),
            store::FLAG_DELETED,
            "the tombstone reached the archive row the seed skipped"
        );
        assert!(!download.exists(), "the stranded message's saved download is removed");
        assert!(!cache.exists(), "the stranded message's cached playback file is removed");
    }

    /// qc1662 sibling audit: apply_server_pin_changed had the
    /// same never-create shape. It resolved the message
    /// through the mirror rows and their bindings and returned
    /// early when no row matched, so a pin reported for a
    /// message beyond the seed window never reached the
    /// archive row either; the message paged in later rendered
    /// unpinned while pinned everywhere else. The canonical ID
    /// and the binding are now recorded as the other settled
    /// verbs record them, and the write-through lands.
    #[test]
    pub(super) fn server_pin_after_restart_persists_to_the_archive() {
        let (mut app, tx, ctx, _cmd_rx, _download, _cache) = restarted_app_with_stranded_message();

        tx.send(BackendEvent::MessagePinChanged {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "old1".to_owned(),
            pinned: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);

        assert_eq!(
            archived_flags(&app, "old1"),
            store::FLAG_PINNED_MESSAGE,
            "the reported pin reached the archive row the seed skipped"
        );
        // Leave no seeded files behind for other tests.
        backend::remove_download_for_message(&backend::data_dir(), CHAT_JID, "old1");
    }

    /// qc1658 L-1, the entry-less counterpart of
    /// arrival_upsert_keeps_the_known_star: the same uninformed
    /// arrival upsert (it reports starred: false without
    /// knowing better) aimed at a starred message whose
    /// flag-map entry the trim has drained. The merge resolves
    /// its word from the map and finds nothing, so before the
    /// fix the write carried zero and erased the archive's
    /// star wholesale; the store now floors the rewrite's word
    /// at the stored word.
    #[test]
    pub(super) fn arrival_upsert_keeps_an_entry_less_star() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.selected_chat = 0;
        seed_unmirrored_star(&mut app, &tx, &ctx);
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "s1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "an old star outside the window",
            BASE_TS,
            false,
        )))
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            archived_flags(&app, "s1"),
            store::FLAG_STARRED,
            "the entry-less star survives the uninformed re-file"
        );
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
            from_history: false,
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

    /// qc1654 Claim B: an episode whose completion never
    /// arrives still ends at the quiet bound, even while
    /// unrelated work sits in the event queue and a batch flush
    /// is pending: the bound is measured from the last
    /// sync-bearing activity only, and the fallback clear is
    /// not a completion, so synced_once stays unlatched.
    #[test]
    pub(super) fn missing_completion_episode_ends_at_the_quiet_bound() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::SyncProgress { percent: Some(100) }).unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert!(session.syncing && session.sync_finishing);
        // The last sync-bearing activity falls outside the
        // quiet window, an unrelated event waits unapplied in
        // the queue, and a batch flush is still pending: none
        // of that may hold the row past the bound.
        app.event_queue
            .push_back(BackendEvent::PinChanged { chat_jid: CHAT_JID.to_owned(), pinned: true });
        app.event_batch.flush_pending = true;
        if let Some(session) = &mut app.live {
            session.sync_last_activity =
                Some(std::time::Instant::now() - std::time::Duration::from_secs(181));
        }
        app.poll_backend(&ctx);
        let session = app.live.as_ref().unwrap();
        assert!(!session.syncing, "the episode ends at the quiet bound");
        assert!(!session.synced_once, "a fallback clear never latches synced_once");
        assert_eq!(session.sync_percent, None, "no invented completion percent survives");
        assert!(!session.sync_finishing, "the finishing phase ends with the episode");
    }

    /// qc1654 Claim C: a name learned under one of a person's
    /// JID forms resolves under the other form too. The sender
    /// name arrives under the LID form; the directory must
    /// answer a phone-form query, and a name seeded under the
    /// phone form before the pairing was known must answer a
    /// LID-form query once the saved-contacts report lands.
    #[test]
    pub(super) fn names_resolve_under_both_jid_forms() {
        const PERSON_PN: &str = "6285550001111@s.whatsapp.net";
        const PERSON_LID: &str = "5550001111@lid";
        const GROUP: &str = "120363000000000009@g.us";
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![saved_contact(PERSON_PN, "Sarah", &[PERSON_LID])],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        // A group message whose sender is addressed by LID.
        let mut msg =
            LiveMessage::text("c1", GROUP, Some("Sarah".to_owned()), "hello", BASE_TS, false);
        msg.sender_jid = Some(PERSON_LID.to_owned());
        tx.send(BackendEvent::MessageUpsert(msg)).unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert_eq!(session.names.get(PERSON_LID).map(String::as_str), Some("Sarah"));
        assert_eq!(
            session.names_resolve(PERSON_PN),
            Some("Sarah"),
            "a name learned under the LID form resolves for a phone-form query"
        );
        assert_eq!(session.names_resolve(PERSON_LID), Some("Sarah"));

        // The other direction: the name is already in the
        // directory under the phone form when the pairing
        // report lands; the report backfills the LID form.
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        if let Some(session) = &mut app.live {
            session.names.insert(PERSON_PN.to_owned(), "Sarah".to_owned());
        }
        tx.send(BackendEvent::SavedContacts {
            contacts: vec![saved_contact(PERSON_PN, "Sarah", &[PERSON_LID])],
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert_eq!(
            session.names_resolve(PERSON_LID),
            Some("Sarah"),
            "a name learned under the phone form resolves for a LID-form query"
        );
        assert_eq!(session.names.get(PERSON_LID).map(String::as_str), Some("Sarah"));
    }

    /// qc1635 sync percentage: only library-reported numbers are
    /// kept, max-clamped within the episode, and a mid-sync stamp
    /// never reads as 100: that value belongs to completion.
    #[test]
    pub(super) fn sync_percent_is_reported_max_clamped_and_never_100_mid_sync() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::SyncProgress { percent: None }).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.live.as_ref().unwrap().sync_percent, None, "no report, no number");
        tx.send(BackendEvent::SyncProgress { percent: Some(40) }).unwrap();
        tx.send(BackendEvent::SyncProgress { percent: Some(25) }).unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert_eq!(session.sync_percent, Some(40), "the strongest stamp of the episode wins");
        assert!(session.syncing);
        tx.send(BackendEvent::SyncProgress { percent: Some(100) }).unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert_eq!(session.sync_percent, Some(99), "a mid-sync stamp never displays as finished");
        assert!(session.syncing && !session.synced_once);
    }

    /// qc1635 sync percentage: completion is the only source of
    /// 100 and starts the brief hold; fresh progress cancels the
    /// hold and starts a clean episode.
    #[test]
    pub(super) fn sync_completion_holds_100_until_new_progress() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::SyncProgress { percent: Some(55) }).unwrap();
        tx.send(BackendEvent::SyncCompleted).unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert_eq!(session.sync_percent, Some(100));
        assert!(session.sync_completed_at.is_some(), "the completed state holds briefly");
        tx.send(BackendEvent::SyncProgress { percent: Some(10) }).unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert!(session.sync_completed_at.is_none(), "new progress cancels the hold");
        assert_eq!(session.sync_percent, Some(10), "a fresh episode starts from its own reports");
        assert!(session.syncing);
    }

    /// qc1635 sync percentage: an interruption that arrives
    /// after a completed episode starts a new episode, so the
    /// completed 100 never displays as mid-sync progress.
    #[test]
    pub(super) fn sync_interruption_after_completion_starts_a_clean_episode() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::SyncProgress { percent: Some(80) }).unwrap();
        tx.send(BackendEvent::SyncCompleted).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.live.as_ref().unwrap().sync_percent, Some(100));
        tx.send(BackendEvent::SyncInterrupted).unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert!(session.syncing, "the interrupted drain keeps the row up");
        assert_eq!(
            session.sync_percent, None,
            "the completed 100 does not ride into the new episode"
        );
        assert!(session.sync_completed_at.is_none());
    }

    /// qc1644 sync finishing: the flag needs both halves, the
    /// episode stamp at its 99 ceiling AND sync work still
    /// being applied after it (an app-state patch, or any
    /// other event: the tail is receipts, messages and history
    /// chunks too); completion and interruption clear it.
    #[test]
    pub(super) fn sync_finishing_needs_the_ceiling_and_late_work() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        // A patch before any progress: no episode, no flag.
        tx.send(BackendEvent::PinChanged { chat_jid: CHAT_JID.to_owned(), pinned: true }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(!app.live.as_ref().unwrap().sync_finishing);
        // Progress short of the ceiling, then a patch: still no.
        tx.send(BackendEvent::SyncProgress { percent: Some(50) }).unwrap();
        tx.send(BackendEvent::PinChanged { chat_jid: CHAT_JID.to_owned(), pinned: false }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            !app.live.as_ref().unwrap().sync_finishing,
            "a patch at 50 is mid-sync work, not finishing"
        );
        // The ceiling (a reported 100 clamps to 99 mid-sync),
        // then a patch: the finishing phase.
        tx.send(BackendEvent::SyncProgress { percent: Some(100) }).unwrap();
        tx.send(BackendEvent::MuteChanged {
            chat_jid: CHAT_JID.to_owned(),
            muted: true,
            mute_until: 0,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert_eq!(session.sync_percent, Some(99));
        assert!(session.sync_finishing, "a patch after the ceiling is the finishing phase");
        // Completion clears it.
        tx.send(BackendEvent::SyncCompleted).unwrap();
        drain_all(&mut app, &ctx);
        assert!(!app.live.as_ref().unwrap().sync_finishing, "completion clears the phase");
        // A new episode can reach finishing again through work
        // that is not an app-state patch at all: a roster
        // upsert landing while the stamp sits at the ceiling
        // is sync tail work, and the row must not sit at a bare
        // 99 through it.
        tx.send(BackendEvent::SyncProgress { percent: Some(100) }).unwrap();
        tx.send(dm_upsert("628999888777@s.whatsapp.net")).unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            app.live.as_ref().unwrap().sync_finishing,
            "non-patch work at the ceiling is the finishing phase too"
        );
        // An interruption clears it, and does not re-enter it
        // through the drain's own per-event note.
        tx.send(BackendEvent::SyncInterrupted).unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert!(!session.sync_finishing, "an interruption clears the finishing phase");
        assert!(session.syncing, "the interrupted drain keeps the row up");
    }

    /// qc1644 roster generation: inserts, renames, verified
    /// changes and removals bump it, and so does a re-upsert
    /// that moves only a filter field (the preview the search
    /// reads, the unread count the Unread filter reads); a
    /// re-upsert that changes nothing and a repeated verified
    /// name do not.
    #[test]
    pub(super) fn roster_generation_tracks_roster_changes_only() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let generation = |app: &App| app.live.as_ref().unwrap().roster_generation;
        assert_eq!(generation(&app), 0);
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(generation(&app), 1, "the insert bumps it");
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(generation(&app), 1, "an identical re-upsert changes no roster entry");
        tx.send(BackendEvent::ChatRenamed {
            chat_jid: CHAT_JID.to_owned(),
            title: "Sarah Connor".to_owned(),
            authoritative: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(generation(&app), 2, "a rename bumps it");
        tx.send(BackendEvent::VerifiedNameResult {
            jid: CHAT_JID.to_owned(),
            name: Some("Sarah Connor".to_owned()),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(generation(&app), 3, "a verified name landing bumps it");
        tx.send(BackendEvent::VerifiedNameResult {
            jid: CHAT_JID.to_owned(),
            name: Some("Sarah Connor".to_owned()),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(generation(&app), 3, "the same verified name again does not");
        // A re-upsert that moves only the preview: the title
        // and every flag restate the current row, but the
        // search reads the preview, so this is a roster
        // change.
        let mut preview_only = dm_upsert(CHAT_JID);
        if let BackendEvent::ChatUpsert(chat) = &mut preview_only {
            chat.title = "Sarah Connor".to_owned();
            chat.preview = "a different preview".to_owned();
        }
        tx.send(preview_only).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(generation(&app), 4, "a preview-only re-upsert bumps it");
        // The same re-upsert again changes nothing at all.
        let mut identical = dm_upsert(CHAT_JID);
        if let BackendEvent::ChatUpsert(chat) = &mut identical {
            chat.title = "Sarah Connor".to_owned();
            chat.preview = "a different preview".to_owned();
        }
        tx.send(identical).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(generation(&app), 4, "an identical re-upsert still does not");
        // A re-upsert that moves only the unread count: the
        // Unread filter reads it, so this is a roster change
        // too.
        let mut unread_only = dm_upsert(CHAT_JID);
        if let BackendEvent::ChatUpsert(chat) = &mut unread_only {
            chat.title = "Sarah Connor".to_owned();
            chat.preview = "a different preview".to_owned();
            chat.unread = 2;
        }
        tx.send(unread_only).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(generation(&app), 5, "an unread-only re-upsert bumps it");
        tx.send(BackendEvent::ChatDeleted { chat_jid: CHAT_JID.to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(generation(&app), 6, "the removal bumps it");
    }

    /// qc1644 review regression: the visible-chats memo keys
    /// on the roster generation, so a re-upsert that moves
    /// only a filter input must still move the visible set: a
    /// preview-only change under a search query (the chat
    /// stops matching), and an unread-only change under the
    /// Unread filter (the chat starts matching).
    #[test]
    pub(super) fn visible_chats_follow_filter_only_upserts() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        drain_all(&mut app, &ctx);
        app.search = "hello".to_owned();
        assert_eq!(app.visible_chats().len(), 1, "the preview matches the search");
        let mut preview_only = dm_upsert(CHAT_JID);
        if let BackendEvent::ChatUpsert(chat) = &mut preview_only {
            chat.preview = "goodbye".to_owned();
        }
        tx.send(preview_only).unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.visible_chats().is_empty(), "a preview-only upsert leaves the search");
        app.search = String::new();
        app.filter = core::filter::ChatFilter::Unread;
        assert!(app.visible_chats().is_empty(), "no unread chats yet");
        let mut unread_only = dm_upsert(CHAT_JID);
        if let BackendEvent::ChatUpsert(chat) = &mut unread_only {
            chat.preview = "goodbye".to_owned();
            chat.unread = 2;
        }
        tx.send(unread_only).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.visible_chats().len(), 1, "an unread-only upsert enters the Unread view");
    }

    /// qc1645 regression: the label-filter memo keys on a
    /// fold of the label's member set, not the member count,
    /// so a tag/untag swap that preserves the count still
    /// re-runs the filter and the visible set follows the
    /// new member.
    #[test]
    pub(super) fn visible_chats_follow_a_same_count_label_swap() {
        const OTHER_JID: &str = "6289876543210@s.whatsapp.net";
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        tx.send(dm_upsert(OTHER_JID)).unwrap();
        drain_all(&mut app, &ctx);
        let first_id = jid_id(CHAT_JID);
        let other_id = jid_id(OTHER_JID);
        let visible_ids = |app: &App| -> Vec<u64> {
            let indices: Vec<usize> = app.visible_chats().iter().copied().collect();
            indices.iter().map(|&index| app.chats[index].id).collect()
        };
        app.business.label_filter = Some("l1".to_owned());
        app.business.chat_labels.entry(CHAT_JID.to_owned()).or_default().insert("l1".to_owned());
        assert_eq!(visible_ids(&app), vec![first_id], "only the tagged chat shows");
        // Untag the first chat and tag the second: the count
        // carrying the label is still one.
        if let Some(set) = app.business.chat_labels.get_mut(CHAT_JID) {
            set.remove("l1");
        }
        app.business.chat_labels.entry(OTHER_JID.to_owned()).or_default().insert("l1".to_owned());
        assert_eq!(visible_ids(&app), vec![other_id], "the swap moves the visible set");
    }

    /// qc1644 receipt chunking, the split itself: at or under
    /// the cap nothing is held back; over the cap the first
    /// chunk is exactly the cap and the remainder keeps every
    /// remaining ID in order, so chunked application covers the
    /// receipt exactly once.
    #[test]
    pub(super) fn receipt_split_holds_back_only_the_overflow() {
        let ids: Vec<String> = (0..300).map(|i| format!("m-{i}")).collect();
        let (chunk, rest) = split_receipt_ids(ids.clone(), MAX_EVENTS_PER_FRAME);
        assert_eq!(chunk.len(), MAX_EVENTS_PER_FRAME);
        assert_eq!(rest.len(), 300 - MAX_EVENTS_PER_FRAME);
        let mut rejoined = chunk.clone();
        rejoined.extend(rest.iter().cloned());
        assert_eq!(rejoined, ids, "the chunks rejoin into the receipt, in order");
        let (chunk, rest) = split_receipt_ids(ids[..10].to_vec(), MAX_EVENTS_PER_FRAME);
        assert_eq!(chunk.len(), 10);
        assert!(rest.is_empty(), "an in-cap receipt has no continuation");
    }

    /// qc1644 receipt chunking, end to end: one receipt naming
    /// more messages than the per-frame cap still advances
    /// every named message: the continuation chunks lose no ID
    /// and duplicate none. The advance is durable at the levels
    /// the design keeps whole: the archive's rows (the mirror
    /// holds only its retained window, qc1654 Item M) and the
    /// receipt log, and the mirrored rows advance in place.
    #[test]
    pub(super) fn oversized_receipt_advances_every_message_across_chunks() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        let total = MAX_EVENTS_PER_FRAME + 44;
        let mut ids: Vec<String> = Vec::new();
        for i in 0..total {
            let pid = format!("own-{i}");
            ids.push(pid.clone());
            tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
                pid,
                CHAT_JID,
                None,
                "mine",
                BASE_TS + i as i64,
                true,
            )))
            .unwrap();
        }
        drain_all(&mut app, &ctx);
        tx.send(BackendEvent::Receipt {
            chat_jid: CHAT_JID.to_owned(),
            message_ids: ids,
            kind: ReceiptKind::Read,
            at: BASE_TS + 10_000,
            actor: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let chat_id = jid_id(CHAT_JID);
        let rows = &app.live.as_ref().unwrap().messages[&chat_id];
        assert_eq!(rows.len(), MIRROR_RETENTION_BACKGROUND, "the mirror holds its window");
        assert!(
            rows.iter().all(|(_, m)| m.delivery == Some(Delivery::Read)),
            "every mirrored message the receipt named advanced to Read"
        );
        let archived =
            app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        assert_eq!(archived.len(), total, "no message was filed twice");
        let archived_ids: std::collections::HashSet<&str> =
            archived.iter().map(|(m, _)| m.id.as_str()).collect();
        assert_eq!(archived_ids.len(), total, "the archive holds each message exactly once");
        assert!(
            archived.iter().all(|(m, _)| m.delivery == Some(backend::DeliveryState::Read)),
            "every message the receipt named advanced to Read in the archive"
        );
        assert_eq!(app.receipt_log.len(), total, "the receipt log recorded each ID exactly once");
    }

    /// qc1635 H5 completeness: once a batch flush has rebuilt a
    /// chat's position index, the index is complete, so a lookup
    /// for a genuinely new message proves absence from the index
    /// alone, and a second burst of new messages files each one
    /// exactly once (a completeness lie would duplicate rows).
    #[test]
    pub(super) fn complete_index_files_new_bursts_without_duplicates() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        let chat_id = jid_id(CHAT_JID);
        let burst = |tx: &std::sync::mpsc::Sender<BackendEvent>, from: i64, to: i64| {
            for i in from..to {
                tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
                    format!("c-{i}"),
                    CHAT_JID,
                    Some("Sender".to_owned()),
                    "burst",
                    BASE_TS + i,
                    false,
                )))
                .unwrap();
            }
        };
        burst(&tx, 0, 30);
        drain_all(&mut app, &ctx);
        {
            let session = app.live.as_mut().unwrap();
            assert!(
                session.msg_index_complete.contains(&chat_id),
                "the flush rebuild completes the index"
            );
            assert!(session.message_position(chat_id, message_id(CHAT_JID, "c-7")).is_some());
            assert_eq!(session.message_position(chat_id, message_id(CHAT_JID, "absent")), None);
        }
        burst(&tx, 30, 60);
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        assert_eq!(session.messages[&chat_id].len(), 60, "every new message filed exactly once");
    }

    /// Files one pending local echo the way `send_live_text`
    /// does: appended to the chat's vec and recorded in its
    /// position index.
    pub(super) fn file_sending_echo(
        app: &mut App,
        chat_id: u64,
        local_id: u64,
        body: &str,
        ts: i64,
    ) {
        let session = app.live.as_mut().unwrap();
        let mut echo = Message::mine(local_id, body.to_owned(), "…");
        echo.delivery = Some(Delivery::Sending);
        let entry = session.messages.entry(chat_id).or_default();
        entry.push((ts, echo));
        session.msg_index.entry(chat_id).or_default().insert(local_id, entry.len() - 1);
    }

    /// Seeds the chat and drives its position index complete
    /// the way a batch flush does, returning the chat ID.
    pub(super) fn seed_complete_chat(
        app: &mut App,
        tx: &std::sync::mpsc::Sender<BackendEvent>,
        ctx: &egui::Context,
    ) -> u64 {
        tx.send(dm_upsert(CHAT_JID)).unwrap();
        tx.send(incoming("seed-1", BASE_TS)).unwrap();
        drain_all(app, ctx);
        let chat_id = jid_id(CHAT_JID);
        assert!(
            app.live.as_ref().unwrap().msg_index_complete.contains(&chat_id),
            "the flush rebuild completes the index"
        );
        chat_id
    }

    /// qc1642 fix 1: binding an echo to its protocol ID must
    /// move the index entry with it. While the index claims
    /// completeness, a stale entry for the dead local ID and no
    /// entry for the protocol ID make `message_position` prove
    /// a false absence, and the backend's own upsert of the
    /// sent message then files a duplicate row.
    #[test]
    pub(super) fn message_sent_bind_keeps_complete_index_truthful() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = seed_complete_chat(&mut app, &tx, &ctx);
        let local_id = 9_000_001;
        file_sending_echo(&mut app, chat_id, local_id, "hello", BASE_TS + 10);
        tx.send(BackendEvent::MessageSent {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "proto-1".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let mid = message_id(CHAT_JID, "proto-1");
        {
            let session = app.live.as_mut().unwrap();
            assert_eq!(
                session.message_position(chat_id, mid),
                Some(1),
                "the bound protocol ID resolves to the echo's row"
            );
            assert_eq!(
                session.message_position(chat_id, local_id),
                None,
                "the dead local ID no longer resolves"
            );
        }
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "proto-1",
            CHAT_JID,
            None,
            "hello",
            BASE_TS + 10,
            true,
        )))
        .unwrap();
        drain_all(&mut app, &ctx);
        let rows = &app.live.as_ref().unwrap().messages[&chat_id];
        assert_eq!(rows.len(), 2, "the upsert replaces the bound echo, never duplicates it");
        assert_eq!(rows.iter().filter(|(_, m)| m.id == mid).count(), 1);
    }

    /// qc1642 fix 1, receipt path: the receipt fallback binds
    /// the oldest pending echo when the backend never upserted
    /// the send; that bind must move the index entry too, for
    /// the same completeness reason as the MessageSent bind.
    #[test]
    pub(super) fn receipt_fallback_bind_keeps_complete_index_truthful() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = seed_complete_chat(&mut app, &tx, &ctx);
        let local_id = 9_000_002;
        file_sending_echo(&mut app, chat_id, local_id, "hello again", BASE_TS + 10);
        tx.send(BackendEvent::Receipt {
            chat_jid: CHAT_JID.to_owned(),
            message_ids: vec!["proto-2".to_owned()],
            kind: ReceiptKind::Delivered,
            at: BASE_TS + 11,
            actor: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let mid = message_id(CHAT_JID, "proto-2");
        {
            let session = app.live.as_mut().unwrap();
            assert_eq!(
                session.message_position(chat_id, mid),
                Some(1),
                "the receipt-bound protocol ID resolves to the echo's row"
            );
            assert_eq!(session.message_position(chat_id, local_id), None);
        }
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "proto-2",
            CHAT_JID,
            None,
            "hello again",
            BASE_TS + 10,
            true,
        )))
        .unwrap();
        drain_all(&mut app, &ctx);
        let rows = &app.live.as_ref().unwrap().messages[&chat_id];
        assert_eq!(rows.len(), 2, "the upsert replaces the bound echo, never duplicates it");
        assert_eq!(rows.iter().filter(|(_, m)| m.id == mid).count(), 1);
    }

    /// qc1635 deferred writes: a sender naming that lands in the
    /// same batch as the message's own upsert still rewrites the
    /// ARCHIVE row: the rename defers to the batch flush instead
    /// of UPDATEing a row that does not exist yet and being
    /// overwritten by the queued upsert copy.
    #[test]
    pub(super) fn sender_naming_in_the_upserts_batch_rewrites_the_archive() {
        const GROUP: &str = "120363000000000002@g.us";
        const SENDER: &str = "6281234567890@s.whatsapp.net";
        let (mut app, tx, ctx, _cmd_rx) = live_app();
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
            from_history: false,
        }))
        .unwrap();
        let mut msg = LiveMessage::text("g-1", GROUP, None, "hello group", BASE_TS, false);
        msg.sender = Some("6281234567890".to_owned());
        msg.sender_jid = Some(SENDER.to_owned());
        // One drain applies both events in a single batch.
        tx.send(BackendEvent::MessageUpsert(msg)).unwrap();
        tx.send(BackendEvent::SenderNamed {
            sender_jid: SENDER.to_owned(),
            name: "Sarah".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let session = app.live.as_ref().unwrap();
        let mirror = &session.messages[&jid_id(GROUP)];
        assert_eq!(mirror[0].1.sender.as_deref(), Some("Sarah"), "the mirror label is renamed");
        let stored = session.store.as_ref().unwrap().messages(GROUP).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].0.sender.as_deref(),
            Some("Sarah"),
            "the archive row is renamed too, not overwritten by the queued copy"
        );
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
            from_history: false,
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
            from_history: false,
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
                from_history: false,
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
            from_history: false,
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
                ("6281111111111@s.whatsapp.net".to_owned(), "Alice Johnson".to_owned(), true, None),
                ("6282222222222@s.whatsapp.net".to_owned(), "Bob Carter".to_owned(), false, None),
                (
                    "6283333333333@s.whatsapp.net".to_owned(),
                    "Carol Danvers".to_owned(),
                    false,
                    None,
                ),
            ],
            own_admin: false,
            own_label: None,
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
            from_history: false,
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
                (ALICE_JID.to_owned(), String::new(), false, None),
                (BOB_JID.to_owned(), String::new(), true, None),
                (
                    CAROL_JID.to_owned(),
                    "Carol Danvers".to_owned(),
                    false,
                    Some("Design".to_owned()),
                ),
                (STRANGER_JID.to_owned(), String::new(), false, None),
                (LID_JID.to_owned(), String::new(), false, None),
            ],
            own_admin: false,
            own_label: None,
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
        // Carol's member label (row 65) rides her row; the
        // others have none.
        assert_eq!(rows[2].member_label.as_deref(), Some("Design"));
        assert_eq!(rows[0].member_label, None);
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
            saved_contacts: std::collections::HashMap::new(),
            saved_contact_forms: std::collections::HashMap::new(),
            retired_contact_forms: std::collections::HashMap::new(),
            saved_contacts_report_step: 0,
            orphaned_contact_forms: std::collections::HashSet::new(),
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
            sync_finishing: false,
            seeded_unread: std::collections::HashSet::new(),
            msg_index: HashMap::new(),
            msg_index_complete: std::collections::HashSet::new(),
            chat_positions: HashMap::new(),
            roster_generation: 0,
            archive_starred_generation: 0,
            ephemeral_index: std::collections::BTreeMap::new(),
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
            verified_levels: HashMap::new(),
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
            channel_info_requested: std::collections::HashSet::new(),
            history_armed: std::collections::HashSet::new(),
            paged_chats: std::collections::HashSet::new(),
            paged_to_start: std::collections::HashSet::new(),
            poll_ballots: HashMap::new(),
            event_ballots: HashMap::new(),
        });
        app.live_mode = true;
        (app, evt_tx, ctx, cmd_rx)
    }

    pub(super) fn drain_all(app: &mut App, ctx: &egui::Context) {
        for _ in 0..64 {
            app.poll_backend(ctx);
            // Quiescent means the queue is empty AND no budgeted
            // flush is still pending: a flush that deferred on
            // its frame budget holds unapplied work (including
            // the deferred conversation rebuild) even when every
            // event has left the queue (qc1644).
            if app.event_queue.is_empty() && !app.event_batch.flush_pending {
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
            from_history: false,
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
            from_history: false,
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
            from_history: false,
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
            from_history: false,
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
            jid: String::new(),
            lid: String::new(),
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
    /// persist_settings writes, in both directions. Adopting
    /// sends nothing back beyond the one-time status-audience
    /// ask (qc1644): a state that lands without the audience
    /// keys asks for them once per session.
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
        // The one legitimate command: the state carried no
        // status-audience keys, so adoption asks for the
        // audience once (the ask is guarded to once per
        // session). Nothing else may be sent back.
        match cmd_rx.try_recv() {
            Ok(BackendCommand::FetchStatusPrivacy) => {}
            other => panic!("the first adoption asks for the status audience once, got {other:?}"),
        }
        assert!(cmd_rx.try_recv().is_err(), "adopting a fetched state sends nothing else back");

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
        // "known" is a real audience for the Call adds row
        // (qc1644) and labels as such; genuinely unknown
        // spellings display verbatim: the page does not pretend
        // they are an audience it knows.
        assert_eq!(privacy_value_label("known"), "Known contacts");
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
            from_history: false,
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

    /// The first-link double count (qc1634 claim 2): a history
    /// payload seeds the badge from its own unread count and
    /// then delivers the very messages that count describes.
    /// The badge must end at the seed, not the seed plus the
    /// messages, in the mirror and the archive; the messages
    /// still queue for receipts, and a later live arrival still
    /// increments on top of the seed.
    #[test]
    pub(super) fn history_seed_and_its_messages_count_unread_once() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        let history_upsert = |unread: u32| {
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
                from_history: true,
            })
        };
        let history_msg = |pid: &str, ts: i64| {
            let mut msg = LiveMessage::text(
                pid,
                CHAT_JID,
                Some("Sender".to_owned()),
                "old unread",
                ts,
                false,
            );
            msg.from_history = true;
            BackendEvent::MessageUpsert(msg)
        };
        tx.send(history_upsert(2)).unwrap();
        tx.send(history_msg("h1", BASE_TS)).unwrap();
        tx.send(history_msg("h2", BASE_TS + 60)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_unread(&app), 2, "the seed is the count, not the seed plus its messages");
        assert_eq!(store_row(&app).unread, 2, "the archive row carries the seed, not double");
        assert_eq!(
            app.unread_live.get(&chat_id).map(Vec::len),
            Some(2),
            "the history messages still queue for receipts"
        );

        // A live arrival after the payload increments normally.
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "live1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "fresh",
            BASE_TS + 120,
            false,
        )))
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_unread(&app), 3, "a live arrival increments on top of the seed");
        assert_eq!(store_row(&app).unread, 3);
    }

    /// The seed rule is scoped to seeded chats: a history
    /// message in a chat the payload never seeded (it was
    /// created by live traffic) still increments, because no
    /// count ever covered it.
    #[test]
    pub(super) fn history_message_in_an_unseeded_chat_still_increments() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let mut msg = LiveMessage::text(
            "h1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "backfilled",
            BASE_TS,
            false,
        );
        msg.from_history = true;
        tx.send(BackendEvent::MessageUpsert(msg)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_unread(&app), 1, "no seed ever covered this chat's messages");
        assert_eq!(store_row(&app).unread, 1);
    }

    /// Reading a chat sends the app-state read patch
    /// (MarkChatRead) whether or not the read-receipts toggle
    /// allows the per-message receipts: the patch syncs state
    /// between the user's own devices, the toggle governs ticks
    /// shown to other people. With the toggle on, both go out.
    #[test]
    pub(super) fn engagement_sends_the_read_patch_with_or_without_receipts() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);
        let arrival = |pid: &str, ts: i64| {
            BackendEvent::MessageUpsert(LiveMessage::text(
                pid,
                CHAT_JID,
                Some("Sender".to_owned()),
                "unread",
                ts,
                false,
            ))
        };
        tx.send(arrival("u1", BASE_TS)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_unread(&app), 1);

        app.read_receipts = false;
        app.selected_chat = chat_id;
        assert_eq!(app.note_read_engagement(), 1, "the queued message is marked");
        // Other commands (a presence subscription for the open
        // chat) may share the channel; the claim is about which
        // read commands are among them.
        let sent: Vec<BackendCommand> = std::iter::from_fn(|| cmd_rx.try_recv().ok()).collect();
        assert!(
            sent.contains(&BackendCommand::MarkChatRead { chat_jid: CHAT_JID.to_owned() }),
            "the read patch goes out even with receipts off: {sent:?}"
        );
        // Since qc1644 the receipts go out too: the library
        // downgrades them to read-self for the user's own
        // devices when the toggle is off, exactly like
        // WhatsApp, so the app no longer suppresses them.
        assert!(
            sent.contains(&BackendCommand::MarkAsRead {
                chat_jid: CHAT_JID.to_owned(),
                sender_jid: None,
                message_ids: vec!["u1".to_owned()],
            }),
            "the receipts follow even with the toggle off: {sent:?}"
        );
        // The conversation caller adopts the read locally, as
        // it does on the real engagement path.
        app.apply_chat_read_changed(chat_id, true, 0);
        assert_eq!(chat_unread(&app), 0, "the local badge clears with the mark");

        tx.send(arrival("u2", BASE_TS + 60)).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(chat_unread(&app), 1);
        app.read_receipts = true;
        assert_eq!(app.note_read_engagement(), 1);
        let sent: Vec<BackendCommand> = std::iter::from_fn(|| cmd_rx.try_recv().ok()).collect();
        let patch_at = sent
            .iter()
            .position(|c| *c == BackendCommand::MarkChatRead { chat_jid: CHAT_JID.to_owned() });
        let receipts_at = sent.iter().position(|c| {
            *c == BackendCommand::MarkAsRead {
                chat_jid: CHAT_JID.to_owned(),
                sender_jid: None,
                message_ids: vec!["u2".to_owned()],
            }
        });
        assert!(patch_at.is_some(), "the read patch goes out with receipts on too: {sent:?}");
        assert!(receipts_at.is_some(), "and the receipts follow when allowed: {sent:?}");
        assert!(
            patch_at < receipts_at,
            "the patch is sent before the receipts it accompanies: {sent:?}"
        );
    }

    /// qc1654 Claim H, the restart shape: the badge stands at
    /// a seeded count whose messages are not in this session's
    /// receipt queue (the queue starts empty on every start).
    /// A genuine read action must still clear the badge in the
    /// mirror and the archive, send exactly one MarkChatRead
    /// patch, and send no MarkAsRead at all.
    #[test]
    pub(super) fn engagement_clears_a_seeded_badge_with_an_empty_receipt_queue() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(counted_upsert(281)).unwrap();
        drain_all(&mut app, &ctx);
        let chat_id = jid_id(CHAT_JID);
        assert_eq!(chat_unread(&app), 281, "the seeded count stands");
        assert!(
            app.unread_live.get(&chat_id).is_none_or(Vec::is_empty),
            "nothing is queued for receipts in this session"
        );
        app.selected_chat = chat_id;
        assert_eq!(
            app.note_read_engagement(),
            0,
            "no per-message receipts are marked, because none are queued"
        );
        assert_eq!(chat_unread(&app), 0, "the engagement itself clears the badge");
        assert_eq!(store_row(&app).unread, 0, "and the archive row follows");
        let sent: Vec<BackendCommand> = std::iter::from_fn(|| cmd_rx.try_recv().ok()).collect();
        let patches = sent
            .iter()
            .filter(|c| **c == BackendCommand::MarkChatRead { chat_jid: CHAT_JID.to_owned() })
            .count();
        assert_eq!(patches, 1, "exactly one read patch goes out: {sent:?}");
        assert!(
            !sent.iter().any(|c| matches!(c, BackendCommand::MarkAsRead { .. })),
            "no receipts go out for an empty queue: {sent:?}"
        );
    }

    /// qc1654 Claim H, the group shape of the same
    /// manifestation (the owner's report was group-heavy): a
    /// group chat's seeded badge with an empty receipt queue
    /// clears on engagement exactly like a direct chat's, in
    /// the mirror and the archive, with one read patch and no
    /// per-message receipts.
    #[test]
    pub(super) fn engagement_clears_a_seeded_group_badge_with_an_empty_receipt_queue() {
        const GROUP_JID: &str = "120363412811529147@g.us";
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::ChatUpsert(LiveChat {
            jid: GROUP_JID.to_owned(),
            title: "Group".to_owned(),
            kind: LiveChatKind::Group,
            preview: "hello".to_owned(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread: 16,
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
        let chat_id = jid_id(GROUP_JID);
        let unread_of =
            |app: &App| app.chats.iter().find(|c| c.id == chat_id).expect("group exists").unread;
        assert_eq!(unread_of(&app), 16, "the seeded group count stands");
        app.selected_chat = chat_id;
        assert_eq!(app.note_read_engagement(), 0, "no receipts are queued to mark");
        assert_eq!(unread_of(&app), 0, "the engagement clears the group badge");
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        let row = store
            .chats()
            .unwrap()
            .into_iter()
            .find(|c| c.jid == GROUP_JID)
            .expect("the group reached the archive");
        assert_eq!(row.unread, 0, "and the archive row follows");
        let sent: Vec<BackendCommand> = std::iter::from_fn(|| cmd_rx.try_recv().ok()).collect();
        let patches = sent
            .iter()
            .filter(|c| **c == BackendCommand::MarkChatRead { chat_jid: GROUP_JID.to_owned() })
            .count();
        assert_eq!(patches, 1, "exactly one read patch goes out: {sent:?}");
        assert!(
            !sent.iter().any(|c| matches!(c, BackendCommand::MarkAsRead { .. })),
            "no receipts go out for an empty queue: {sent:?}"
        );
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

    /// Seeds a saved download and a cached playback file for
    /// one message in the real data dir, under the stem the
    /// removal helpers match, and returns their paths. Any
    /// leftover from an earlier run is removed first.
    fn seed_media_files(
        chat_jid: &str,
        message_id: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let data_dir = backend::data_dir();
        backend::remove_download_for_message(&data_dir, chat_jid, message_id);
        let stem = backend::downloaded_media_stem(chat_jid, message_id);
        let download = data_dir.join("downloads").join(format!("{stem}.bin"));
        let cache = data_dir.join("video-cache").join(format!("{stem}.mp4"));
        std::fs::create_dir_all(download.parent().expect("the downloads dir")).unwrap();
        std::fs::create_dir_all(cache.parent().expect("the cache dir")).unwrap();
        std::fs::write(&download, b"media").unwrap();
        std::fs::write(&cache, b"media").unwrap();
        (download, cache)
    }

    /// qc1660 LOW-1: a chat clear removes archive rows the
    /// mirror never held (the deletion is keyed by the
    /// event's JID); those rows' saved downloads and cached
    /// playback files go with them (privacy audit A3), as
    /// the mirrored rows' files already did. Before the
    /// fix the download removal iterated only the mirrored
    /// doomed set, so an unmirrored cleared message's
    /// plaintext files stayed for the life of the install.
    /// A starred row a keep-starred clear spares keeps its
    /// files, like its row.
    #[test]
    pub(super) fn server_chat_cleared_removes_unmirrored_rows_media_files() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);
        for (pid, body, ts) in [("u1", "plain", BASE_TS), ("u2", "starred", BASE_TS + 60)] {
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
            message_id: "u2".to_owned(),
            starred: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        // Unmirror both rows: the archive holds them, the
        // mirror and the flag map hold nothing for them.
        let mids: Vec<u64> = ["u1", "u2"].iter().map(|pid| message_id(CHAT_JID, pid)).collect();
        {
            let session = app.live.as_mut().unwrap();
            for mid in &mids {
                session.flags.remove(mid);
            }
            if let Some(msgs) = session.messages.get_mut(&chat_id) {
                msgs.retain(|(_, m)| !mids.contains(&m.id));
            }
        }
        app.selected_chat = 0;
        app.conversation.clear();
        assert_eq!(archived_bodies(&app), ["plain", "starred"], "the archive holds both rows");
        let (u1_download, u1_cache) = seed_media_files(CHAT_JID, "u1");
        let (u2_download, u2_cache) = seed_media_files(CHAT_JID, "u2");

        // A keep-starred clear takes the plain unmirrored
        // row and its files; the starred row and its files
        // are spared.
        tx.send(BackendEvent::ChatCleared {
            chat_jid: CHAT_JID.to_owned(),
            delete_starred: false,
            last_message_timestamp: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(archived_bodies(&app), ["starred"], "the clear took the plain row");
        assert!(!u1_download.exists(), "the cleared row's saved download is removed");
        assert!(!u1_cache.exists(), "the cleared row's cached playback file is removed");
        assert!(u2_download.exists() && u2_cache.exists(), "the spared row's files survive");

        // A delete-starred clear takes the rest, files too.
        tx.send(BackendEvent::ChatCleared {
            chat_jid: CHAT_JID.to_owned(),
            delete_starred: true,
            last_message_timestamp: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(archived_bodies(&app).is_empty(), "the archive empties");
        assert!(!u2_download.exists() && !u2_cache.exists(), "the starred row's files go too");
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
