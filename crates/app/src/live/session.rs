//! Session lifecycle and the outbound path: starting and
//! ending the live session, the backend command channel, the
//! pair code, the presence watch, and sending.

use super::*;

/// Whether the window is currently visible, as last
/// reported by the shell's per-frame check (qc1644): the
/// backend wake hook reads it from the backend's threads,
/// where asking egui for the viewport state directly would
/// contend with the UI thread's own input lock. Starts
/// visible: before the first frame reports, visibility is
/// unknown and unknown counts as visible, today's behavior.
static WINDOW_VISIBLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Records the window's visibility for the backend wake
/// hook: the shell calls this once per frame with the same
/// answer its own expiry gate uses.
pub(crate) fn note_window_visibility(visible: bool) {
    WINDOW_VISIBLE.store(visible, std::sync::atomic::Ordering::Relaxed);
}

impl crate::state::App {
    /// Start fresh after an unlock failure: the recovery the
    /// connect screen's failed-start panel offers once the user
    /// confirms (see `UiConfirm::StartFresh`). The stranded
    /// databases and key file are set aside into a backup
    /// folder, then a normal live start runs: with nothing left
    /// that a key must unlock, it mints a fresh key and links
    /// from zero. When the move itself fails, the failure is
    /// reported as a notice and the start error stays, so the
    /// panel keeps telling the truth about where things stand.
    pub(crate) fn start_fresh(&mut self, ctx: &egui::Context) {
        let data_dir = backend::data_dir();
        match move_stranded_data_aside(&data_dir) {
            Ok(_) => {
                self.live_start_error = None;
                self.notice = None;
                self.start_live(ctx, false, false);
            }
            Err(e) => {
                self.notice = Some(Notice::info(format!(
                    "Zaprust could not set the old data aside, so nothing was changed. {e}"
                )));
            }
        }
    }

    /// Starts live mode: opens the archive, spawns the supervised
    /// backend, and kicks off the link flow. The archive itself
    /// is NOT loaded here: its read (every message of every
    /// chat) is the one proven structural startup cost (qc1634
    /// H7), so it runs on a worker behind the first paint and
    /// lands through [`App::poll_archive_load`]; the first paint
    /// and the link flow never wait for it. Called from
    /// `App::new` for `--live`, and from the account modal's
    /// "Add account" for the in-app path.
    pub(crate) fn start_live(&mut self, ctx: &egui::Context, fake: bool, fake_auto: bool) {
        if self.live.is_some() {
            return;
        }
        let data_dir = backend::data_dir();
        // Both local databases are encrypted under one key from
        // the app key file (SEC-F01; see store::dbkey). Without the key neither can
        // open, and a real start does not try: it fails closed
        // before anything is spawned or opened, leaving the app
        // untouched so the user can retry (audit finding 6).
        let key_result = store::load_or_create_key(&data_dir);
        let db_key = match key_decision(&key_result, fake) {
            KeyDecision::Run(key) => key,
            KeyDecision::Abort(message) => {
                // Recorded as state, not only as a notice: the
                // connect screen must present this failure instead
                // of a pairing surface for as long as no session
                // exists (the notice banner alone can be dismissed,
                // and the screen behind it used to fall through to
                // a demo QR with a waiting-for-scan status).
                self.live_start_error = Some(message.clone());
                self.notice = Some(Notice::error(message));
                return;
            }
        };
        let archive = match &key_result {
            Ok(key) => store::Store::open(&data_dir.join("archive.db"), key),
            Err(e) => Err(store::StoreError::Io(std::io::Error::other(e.to_string()))),
        };
        // The archive holds the user's chat history; keep it
        // owner-only (security audit SEC-06).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                data_dir.join("archive.db"),
                std::fs::Permissions::from_mode(0o600),
            );
        }
        let (store_opt, archive_note) = match archive {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(format!("Chat archive unavailable: {e}"))),
        };
        if store_opt.is_some() {
            // The store is about to go to the archive-load
            // worker; keep its reopen coordinates so a load
            // failure can open a fresh store on the same file
            // instead of leaving the session without one
            // (qc1642 fix 3, see `fail_archive_load`).
            if let Ok(key) = &key_result {
                self.archive_reopen = Some((data_dir.join("archive.db"), *key));
            }
        }
        if store_opt.is_some() {
            // The archive in the current data directory opened and
            // unlocked, which proves the app runs from the
            // post-rename directory: the legacy copy the one-time
            // adoption left behind can be removed (privacy audit
            // A1). Best-effort; a later start retries.
            backend::remove_legacy_data_dir();
        }
        if let Some(store) = &store_opt {
            // The blocked set's persisted home is this encrypted
            // archive (privacy audit A6). A set already stored
            // here is the last known truth and replaces whatever
            // the settings file's legacy copy said. With nothing
            // stored yet, a non-empty legacy copy (loaded from
            // settings.json at startup) migrates in, so blocks
            // made under an older build survive the move; from
            // here on the settings file never carries the set
            // again (settings_json omits it).
            match store.blocked_chats() {
                Ok(Some(ids)) => {
                    self.blocked_chats = ids.into_iter().collect();
                }
                Ok(None) if !self.blocked_chats.is_empty() => {
                    let mut ids: Vec<u64> = self.blocked_chats.iter().copied().collect();
                    ids.sort_unstable();
                    let _ = store.set_blocked_chats(&ids);
                }
                _ => {}
            }
            // The label and quick-reply sets' persisted home is
            // the same archive: the stored sets are the launch
            // seed the backend's sync events then correct.
            self.load_business_archive(store);
        }
        let repaint_ctx = ctx.clone();
        // The wake is visibility-aware (qc1644): while the
        // window is hidden, a backend event does not need a
        // frame now, it needs the state applied by the next
        // one, so routine events ask for a delayed repaint
        // instead of an immediate one and a hidden window
        // stops burning frames on sync traffic. When the
        // window comes back, egui's own visibility change
        // raises a frame at once, so nothing waits for the
        // delay. An unknown visibility (no frame has reported
        // yet) defaults to visible, today's behavior.
        let wake: backend::WakeFn = std::sync::Arc::new(move || {
            if WINDOW_VISIBLE.load(std::sync::atomic::Ordering::Relaxed) {
                repaint_ctx.request_repaint();
            } else {
                repaint_ctx.request_repaint_after(std::time::Duration::from_secs(3));
            }
        });
        let config = BackendConfig { data_dir, account_id: "default".to_owned(), db_key };
        // The stored-session probe for the first paint (qc1636
        // Step 4b): read before the backend spawns, while the
        // answer still describes what the previous run left
        // behind. It only chooses the startup gate; the
        // backend's outcome events arbitrate the restore.
        self.stored_session = backend::stored_session_exists(&config);
        // Convert a plaintext session store left by an older
        // build before the backend opens it with the key. A
        // failure is no longer swallowed (P2-42): it is kept and
        // posted as an error once the session exists, because a
        // store that could not be converted will fail the
        // backend's open too, and the user should see why.
        let conversion_error = if fake {
            None
        } else {
            store::encrypt_file_in_place(&config.session_db_path(), &config.db_key).err()
        };
        let channels = if fake {
            // The factory must build a fresh backend per (re)start.
            spawn_backend(
                move || {
                    Box::new(if fake_auto { FakeBackend::demo() } else { FakeBackend::waiting() })
                        as Box<dyn Backend>
                },
                config,
                wake,
            )
        } else {
            spawn_backend(|| Box::new(WhatsappBackend) as Box<dyn Backend>, config, wake)
        };
        let session = LiveSession {
            saved_contacts: std::collections::HashMap::new(),
            saved_contact_forms: std::collections::HashMap::new(),
            retired_contact_forms: std::collections::HashMap::new(),
            saved_contacts_report_step: 0,
            orphaned_contact_forms: std::collections::HashSet::new(),
            sync_finishing: false,
            chat_positions: std::collections::HashMap::new(),
            roster_generation: 0,
            channels,
            status: LinkStatus::Starting,
            qr: None,
            qr_expires_at: None,
            pair_code: None,
            pair_code_at: None,
            account: String::new(),
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
            // The store starts unset: when it opened, it is
            // away with the archive-load worker and is installed
            // here when the snapshot lands (see poll_archive_load).
            // An archive that failed to open leaves it unset for
            // the session, exactly as before.
            store: None,
            pending_archive: None,
            jids: HashMap::new(),
            messages: HashMap::new(),
            times: HashMap::new(),
            fake,
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
            poll_ballots: HashMap::new(),
            event_ballots: HashMap::new(),
        };
        // Live mode never shows demo data: start from empty
        // and fill from the archive when its load lands.
        self.chats = Vec::new();
        self.live = Some(session);
        self.live_start_error = None;
        self.logged_out = false;
        if let Some(e) = conversion_error {
            self.notice = Some(Notice::error(format!(
                "Zaprust could not convert the stored session to its encrypted form: {e}"
            )));
        }
        if let Some(note) = archive_note {
            // An archive that failed to open is an error state,
            // not a footnote: the session is running without
            // its history and every archive write is being
            // skipped, so the failure is recorded as start
            // state (which the connect screen presents) and
            // posted as an error, never an info note.
            self.live_start_error = Some(note.clone());
            self.notice = Some(Notice::error(note));
        }
        self.live_mode = true;
        // A new session never inherits a recording: cancel any
        // take in progress outright. A take recorded before
        // this session must not be sendable inside it.
        self.cancel_recording();
        // The link flow starts now, in parallel with the
        // archive load, instead of waiting behind it as it used
        // to: restoring a session should not cost the archive's
        // whole read before the backend even begins to connect.
        self.send_backend(BackendCommand::ConnectQr);
        if fake {
            self.trace_fake_script();
        }
        // The archive load itself (qc1636 Step 7, H7): the
        // store goes to a worker thread, which reads the whole
        // archive into a snapshot and sends it back with the
        // store (see load_archive_snapshot). The session's
        // store slot stays empty until the snapshot lands, and
        // poll_backend holds event application meanwhile, so
        // nothing can write to or read from a half-loaded
        // archive. With no store (the archive failed to open)
        // there is nothing to load and no worker runs.
        if let Some(store) = store_opt {
            let (tx, rx) = std::sync::mpsc::channel();
            let wake_ctx = ctx.clone();
            self.archive_load_gen += 1;
            self.archive_load = Some(PendingArchiveLoad {
                rx,
                gen: self.archive_load_gen,
                started: std::time::Instant::now(),
            });
            let spawned = std::thread::Builder::new()
                .name("zaprust-archive-load".to_owned())
                .spawn(move || {
                    let _ = tx.send(load_archive_snapshot(store));
                    // Wake the UI so the snapshot applies on the
                    // next frame even when nothing else is asking
                    // for a repaint.
                    wake_ctx.request_repaint();
                });
            if spawned.is_err() {
                // The worker could not start and the store went
                // with it: fail the load into the same
                // archive-unavailable state as every other load
                // failure, rather than leaving a pending load
                // that can never land.
                self.archive_load = None;
                self.fail_archive_load("the archive load worker could not start");
            }
        }
    }

    /// Applies the deferred archive load when its snapshot
    /// lands, and fails it honestly when it never does. Called
    /// at the top of `poll_backend`, before any event is
    /// applied: while a load is in flight event application
    /// holds (see `poll_backend`), so the snapshot always lands
    /// on the untouched mirror the synchronous load used to
    /// fill, and no event can observe a half-loaded archive.
    pub(crate) fn poll_archive_load(&mut self) {
        // A staged apply in flight drives its next slice here
        // (qc1644): the formal pending load the begin installed
        // stays open for the staging's whole lifetime, and this
        // branch runs before the channel checks below so the
        // watchdog and the disconnect path, which belong to the
        // worker phase, can never fire mid-staging. The slices
        // are infallible mirror writes, so a staging has no
        // failure path of its own: the only way it ends early
        // is the session ending, which drops it with the
        // session, exactly as a snapshot landing on a dead
        // session was dropped before.
        if self.live.as_ref().is_some_and(|session| session.pending_archive.is_some()) {
            self.advance_archive_apply();
            return;
        }
        let Some(pending) = &self.archive_load else {
            return;
        };
        if pending.gen != self.archive_load_gen {
            // A load from a superseded start: its snapshot (and
            // the store it carries) belongs to a session that no
            // longer exists. Drop it unapplied.
            self.archive_load = None;
            return;
        }
        match pending.rx.try_recv() {
            Ok(load) => {
                self.archive_load = None;
                self.begin_archive_apply(load);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // The watchdog: a load that never lands must
                // not trap the session behind held events and a
                // missing store. Real loads take seconds even on
                // a large archive; two minutes is a wedge, not
                // a slow disk. The worker's late snapshot is
                // dropped with the receiver, store and all.
                if pending.started.elapsed() > ARCHIVE_LOAD_TIMEOUT {
                    self.archive_load = None;
                    self.fail_archive_load("the archive load did not finish in time");
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // The worker died without reporting (a panic in
                // the read): the store went with it, so the
                // session continues without its archive, the
                // designed archive-unavailable state.
                self.archive_load = None;
                self.fail_archive_load("the archive load worker stopped");
            }
        }
    }

    /// Settles a deferred archive load that will never land.
    /// The history snapshot is lost, so the failure is recorded
    /// as start state, which the connect screen presents, with
    /// the wording an archive that failed to open uses. The
    /// store, though, is recovered: the one that went to the
    /// load worker never comes back, so a fresh store opens on
    /// the same file from the coordinates `start_live` kept.
    /// The archive runs in WAL mode and the worker only ever
    /// reads, so the fresh connection coexists with a wedged
    /// worker, and on the worker-death path the worker's
    /// connection is already gone. With a store installed,
    /// every archive write from here on lands again instead of
    /// being silently skipped for the rest of the session
    /// (qc1642 fix 3). Only when the reopen itself fails does
    /// the session stay in the presented archive-unavailable
    /// state, exactly like an archive that failed to open.
    fn fail_archive_load(&mut self, reason: &str) {
        if self.live.as_ref().is_some_and(|session| session.store.is_none()) {
            if let Some((path, key)) = &self.archive_reopen {
                if let Ok(store) = store::Store::open(path, key) {
                    if let Some(session) = &mut self.live {
                        session.store = Some(store);
                    }
                }
            }
        }
        let message = format!("Chat archive unavailable: {reason}");
        self.live_start_error = Some(message.clone());
        self.notice = Some(Notice::error(message));
    }

    /// Begins installing one archive snapshot into the mirror:
    /// stage 0 of the staged apply (qc1644). The chat list and
    /// the per-chat metadata (JIDs, activity times, DM names)
    /// land here, in the frame the snapshot arrives, so the
    /// list is usable immediately; the store is installed too,
    /// so archive writes work from here on. The messages
    /// themselves are NOT seeded here: they move into
    /// [`PendingArchiveApply`] on the session and are seeded
    /// slice by slice by [`App::advance_archive_apply`] on the
    /// following frames. The old synchronous load installed
    /// everything in this one call, which for a large archive
    /// was one frame of O(total messages) on the UI thread
    /// (qc1643 audit claim 3).
    ///
    /// The event-ordering invariant survives the staging
    /// because `poll_backend` holds event application while
    /// `self.archive_load` is set, so this begin keeps a formal
    /// pending load open (a fresh channel whose sender the
    /// staging state holds, which therefore never delivers and
    /// never disconnects) until the last slice lands. Queued
    /// backend events still apply only after the snapshot is
    /// fully installed, in their original order, and the
    /// restore gate keeps presenting the load as in flight for
    /// exactly as long as it did before.
    fn begin_archive_apply(&mut self, load: ArchiveLoad) {
        let ArchiveLoad { store, avatars, chats, messages, call_records, problems } = load;
        let Some(mut session) = self.live.take() else {
            // The session ended while its load was in flight:
            // the snapshot, and the store it carries, belonged
            // to it and go with it.
            return;
        };
        session.store = Some(store);
        // Stored profile pictures seed the avatar cache:
        // positives stage their bytes for first display,
        // negatives remember when they were answered.
        for (jid, _picture_id, bytes, fetched_at) in avatars {
            match bytes {
                Some(bytes) => {
                    session.put_avatar_bytes(jid, bytes);
                }
                None => {
                    session.avatar_missing.insert(jid, fetched_at);
                }
            }
        }
        for chat in &chats {
            session.jids.insert(jid_id(&chat.jid), chat.jid.clone());
            session.times.insert(jid_id(&chat.jid), chat.timestamp);
            // A direct chat's resolved title is the
            // contact's display name: it seeds the name
            // directory under the chat's JID.
            if chat.kind == backend::LiveChatKind::Dm
                && !chat.title.is_empty()
                && !is_unresolved_identifier(&chat.title)
            {
                session.names.insert(chat.jid.clone(), chat.title.clone());
            }
        }
        // Archives written before the status fix can hold the
        // bogus "status" DM row for status@broadcast: it never
        // becomes a chat here (its stored messages stay in the
        // archive, unrendered, for the Updates round).
        self.chats = chats.iter().filter(|c| c.jid != "status@broadcast").map(to_chat).collect();
        // The formal pending load that keeps event application
        // held while the slices run (see the doc comment
        // above): its sender lives in the staging state, so
        // the channel stays open and silent for exactly the
        // staging's lifetime.
        let (hold_tx, hold_rx) = std::sync::mpsc::channel();
        self.archive_load = Some(PendingArchiveLoad {
            rx: hold_rx,
            gen: self.archive_load_gen,
            started: std::time::Instant::now(),
        });
        session.pending_archive = Some(PendingArchiveApply {
            _hold: hold_tx,
            chat_jids: chats.iter().map(|chat| chat.jid.clone()).collect(),
            messages,
            call_records,
            problems,
            poll_secrets: Vec::new(),
            next_chat: 0,
            next_message: 0,
            current_rows: Vec::new(),
            current_index: HashMap::new(),
        });
        self.live = Some(session);
    }

    /// Seeds one bounded slice of a staged archive apply and,
    /// when the last chat is seeded, finishes the install.
    /// Called from [`App::poll_archive_load`] once per frame
    /// while a staging is in flight: each call seeds at most
    /// [`ARCHIVE_SLICE_MESSAGE_CAP`] messages and stops at the
    /// event drain's frame budget ([`EVENT_FRAME_BUDGET`]), so
    /// the UI paints between slices. A slice always makes
    /// progress (the budget is only consulted after some work
    /// has been done), so the staging terminates structurally,
    /// chat by chat, with no watchdog needed of its own.
    fn advance_archive_apply(&mut self) {
        let Some(mut session) = self.live.take() else {
            return;
        };
        let Some(mut staging) = session.pending_archive.take() else {
            self.live = Some(session);
            return;
        };
        let started = std::time::Instant::now();
        let mut seeded = 0usize;
        let mut completed = 0usize;
        while staging.next_chat < staging.chat_jids.len() {
            if (seeded > 0 || completed > 0) && started.elapsed() >= EVENT_FRAME_BUDGET {
                break;
            }
            let chat_index = staging.next_chat;
            let chat_id = jid_id(&staging.chat_jids[chat_index]);
            let total = staging.messages[chat_index].len();
            if staging.next_message == 0 && total > MIRROR_RETENTION {
                // The retention window applies at the seed too
                // (qc1644): a chat larger than the window seeds
                // only its newest rows. The skipped rows stay
                // in the archive and page back on demand; their
                // per-message side state is seeded then, by the
                // paging path, exactly as it is here.
                staging.next_message = total - MIRROR_RETENTION;
            }
            let take = archive_slice_len(total - staging.next_message, seeded);
            for _ in 0..take {
                let (m, flags) = &staging.messages[chat_index][staging.next_message];
                self.seed_archived_message(&mut session, m, *flags, &mut staging.poll_secrets);
                let converted = to_message_with_flags(m, *flags);
                staging.current_index.insert(converted.id, staging.current_rows.len());
                staging.current_rows.push((m.timestamp, converted));
                staging.next_message += 1;
                seeded += 1;
                if started.elapsed() >= EVENT_FRAME_BUDGET {
                    break;
                }
            }
            if staging.next_message < total {
                break;
            }
            // The chat is fully seeded: its rows and position
            // index land together, so no reader can observe a
            // chat whose index disagrees with its vec. The
            // loaded vec is final for now, so its index starts
            // complete and the first sync's lookups are O(1)
            // from the first message (qc1634 H5).
            let rows = std::mem::take(&mut staging.current_rows);
            let index = std::mem::take(&mut staging.current_index);
            // The seeded rows join the ephemeral index as they
            // land (qc1644): an archived disappearing message
            // must expire on schedule after a restart, not
            // wait for the reconciliation scan to find it.
            for (_, m) in &rows {
                session.seed_ephemeral(chat_id, m.id, m.expires_at);
            }
            if let Some(existing) = session.messages.remove(&chat_id) {
                // Rows filed while the staging ran (a local
                // echo sent into a chat whose history had not
                // landed yet) stay after the archived rows, and
                // the index is rebuilt over the merged vec.
                let mut merged = rows;
                merged.extend(existing);
                session.messages.insert(chat_id, merged);
                session.rebuild_msg_index(chat_id);
                // The merge can exceed the retention window
                // when many rows were filed during the staging:
                // the same trim as every other install path.
                session.trim_chat_window(chat_id);
            } else {
                session.messages.insert(chat_id, rows);
                session.msg_index.insert(chat_id, index);
                session.msg_index_complete.insert(chat_id);
            }
            staging.next_chat += 1;
            staging.next_message = 0;
            completed += 1;
        }
        if staging.next_chat >= staging.chat_jids.len() {
            self.finish_archive_apply(session, staging);
        } else {
            session.pending_archive = Some(staging);
            self.live = Some(session);
        }
    }

    /// Finishes a staged archive apply whose last chat has
    /// been seeded: everything the old wholesale install did
    /// after its seeding loop, in the same order. The formal
    /// pending load closes last of all, so the events that
    /// queued behind the staging apply only from the next
    /// application block on, in their original order.
    fn finish_archive_apply(&mut self, session: LiveSession, staging: PendingArchiveApply) {
        let PendingArchiveApply { call_records, problems, poll_secrets, .. } = staging;
        self.live = Some(session);
        // The archive's favorite flags belong to the chats just
        // loaded: the conversion cannot carry them (they are
        // not part of the synced chat row), so they are applied
        // from the archive's own record here.
        self.apply_stored_favorites();
        if !problems.is_empty() {
            // A partial load: the archive opened and the rest
            // loaded, but something was lost on the way in and
            // the user hears about it exactly once.
            self.notice = Some(Notice::error(format!(
                "Some saved data could not be read: {}. Everything else loaded normally.",
                problems.join("; ")
            )));
        }
        // The Calls panel's live history came with the
        // snapshot, read from the same archive at the same
        // point the chats were. Entries recorded while the
        // load was in flight (their archive rows could not be
        // written yet) are kept after the archived ones, and
        // the recorded set unions both, so no call is ever
        // entered twice.
        for record in &call_records {
            self.calls.recorded_ids.insert(record.call_id.clone());
        }
        let archived_entry_ids: std::collections::HashSet<u64> =
            call_records.iter().map(|record| record.id).collect();
        let mut entries: Vec<core::model::CallEntry> =
            call_records.iter().map(to_call_entry).collect();
        entries.extend(
            self.calls
                .live_entries
                .iter()
                .filter(|entry| !archived_entry_ids.contains(&entry.id))
                .cloned(),
        );
        self.calls.live_entries = entries;
        // Entering live mode is a chat switch too (the Add
        // account path starts it from a running demo session):
        // the composer state of the chat being left parks under
        // its own id, and the first live chat starts from a
        // clean composer, never with demo state inside it.
        let first_live = self.chats.first().map_or(0, |c| c.id);
        let previous = self.selected_chat;
        self.switch_composer_chat(previous, first_live);
        self.selected_chat = first_live;
        self.sync_conversation();
        // The deferred --community hook resolves now that the
        // chat list it derives from exists.
        if let Some(id) = self.pending_community.take() {
            if self.communities().iter().any(|c| c.id == id) {
                self.open_community = Some(id);
            }
        }
        for cmd in poll_secrets {
            self.send_backend(cmd);
        }
        // The snapshot is fully installed: the formal pending
        // load closes here, which is what releases the held
        // backend events.
        self.archive_load = None;
    }

    /// Seeds one archived message's per-message state into the
    /// session and app mirrors: the body of the old wholesale
    /// install's inner loop, unchanged in what it fills. The
    /// message's row itself is converted and filed by the
    /// caller ([`App::advance_archive_apply`]); this fills
    /// everything around the row. Poll and event creation
    /// secrets are not sent from here: they collect into
    /// `poll_secrets` and are handed back to the backend when
    /// the apply finishes, once the session exists to send
    /// commands through.
    pub(super) fn seed_archived_message(
        &mut self,
        session: &mut LiveSession,
        m: &backend::LiveMessage,
        flags: i64,
        poll_secrets: &mut Vec<BackendCommand>,
    ) {
        if flags != 0 {
            session.flags.insert(message_id(&m.chat_jid, &m.id), flags);
        }
        // An opened view-once message stays
        // opened across restarts: seed the
        // session's opened set from the stored
        // flag (privacy audit D13).
        if flags & store::FLAG_VIEW_ONCE_OPENED != 0 {
            self.media.opened_view_once.insert(message_id(&m.chat_jid, &m.id));
        }
        // Archived messages must stay actionable
        // after a restart: per-message live actions
        // look the protocol id up here, and before
        // v0.25 only fresh upserts ever filled it,
        // so every action on history silently
        // no-oped (logic review F2).
        self.protocol_ids.insert(message_id(&m.chat_jid, &m.id), m.id.clone());
        // The author's JID is seeded the same
        // way: a group reaction must name the
        // author, and an archived message whose
        // author was never seeded could not be
        // reacted to after a restart.
        if let Some(jid) = &m.sender_jid {
            self.note_sender_jid(message_id(&m.chat_jid, &m.id), jid);
        }
        // A photo's re-download reference is
        // seeded the same way, so a photo from
        // before this session can still be
        // fetched for display.
        if let Some(reference) = &m.photo_ref {
            session.photo_refs.insert(message_id(&m.chat_jid, &m.id), reference.clone());
        }
        // A voice note's reference is seeded
        // the same way, for playback.
        if let Some(reference) = &m.voice_ref {
            session.voice_refs.insert(message_id(&m.chat_jid, &m.id), reference.clone());
        }
        // A video's reference and thumbnail
        // are seeded the same way, for
        // playback and presentation.
        if let Some(reference) = &m.video_ref {
            session.video_refs.insert(message_id(&m.chat_jid, &m.id), reference.clone());
        }
        if let Some(thumb) = &m.video_thumbnail {
            session.put_video_thumb(message_id(&m.chat_jid, &m.id), thumb.clone());
        }
        if let Some(reference) = &m.poster_ref {
            session.poster_refs.insert(message_id(&m.chat_jid, &m.id), reference.clone());
        }
        if let (Some(w), Some(h)) = (m.video_width, m.video_height) {
            session.video_dims.insert(message_id(&m.chat_jid, &m.id), (w, h));
        }
        // A sticker's reference and a
        // photo's thumbnail are seeded the
        // same way.
        if let Some(reference) = &m.sticker_ref {
            session.sticker_refs.insert(message_id(&m.chat_jid, &m.id), reference.clone());
        }
        if let Some(thumb) = &m.photo_thumbnail {
            session.put_photo_thumb(message_id(&m.chat_jid, &m.id), thumb.clone());
        }
        // The media extras seed the same
        // way: document references and
        // thumbnails, the GIF flag, sticker
        // animation state, location
        // snapshots, preview images and the
        // view-once set.
        if let Some(reference) = &m.document_ref {
            session.document_refs.insert(message_id(&m.chat_jid, &m.id), reference.clone());
        }
        if let Some(thumb) = &m.document_thumbnail {
            session.document_thumbs.insert(message_id(&m.chat_jid, &m.id), thumb.clone());
        }
        if m.gif_playback {
            session.gif_videos.insert(message_id(&m.chat_jid, &m.id));
        }
        if m.sticker_animated || m.sticker_lottie {
            session
                .sticker_anim
                .insert(message_id(&m.chat_jid, &m.id), (m.sticker_animated, m.sticker_lottie));
        }
        if let Some(thumb) = &m.sticker_thumbnail {
            session.sticker_thumbs.insert(message_id(&m.chat_jid, &m.id), thumb.clone());
        }
        if let Some(thumb) = &m.location_thumbnail {
            session.location_thumbs.insert(message_id(&m.chat_jid, &m.id), thumb.clone());
        }
        if let Some(thumb) = m.link_preview.as_ref().and_then(|p| p.thumbnail.as_ref()) {
            session.preview_thumbs.insert(message_id(&m.chat_jid, &m.id), thumb.clone());
        }
        if m.view_once {
            session.view_once_ids.insert(message_id(&m.chat_jid, &m.id));
        }
        if !m.mentions.is_empty() {
            session.message_mentions.insert(message_id(&m.chat_jid, &m.id), m.mentions.clone());
        }
        // A stored sender label seeds the
        // name directory under its JID, so
        // mentions in archived messages
        // resolve on the first frame too.
        if let (Some(jid), Some(name)) = (&m.sender_jid, &m.sender) {
            if !name.is_empty() && !is_unresolved_identifier(name) {
                session.names.insert(jid.clone(), name.clone());
            }
        }
        // A poll's per-voter ballots are seeded
        // the same way, so the next vote event
        // replaces the right previous ballot
        // instead of double-counting.
        if !m.poll_ballots.is_empty() {
            session.poll_ballots.insert(message_id(&m.chat_jid, &m.id), m.poll_ballots.clone());
        }
        // A poll whose creation secret was
        // archived is re-registered with the
        // backend below, once the session
        // exists to send commands through.
        if let Some(cmd) = poll_secret_registration(m) {
            poll_secrets.push(cmd);
        }
        // Events ride the same two rails:
        // ballots seeded so the next response
        // replaces the right previous answer,
        // and the creation secret re-registered
        // so responses to an event created
        // before this start still decrypt.
        if !m.event_ballots.is_empty() {
            session.event_ballots.insert(message_id(&m.chat_jid, &m.id), m.event_ballots.clone());
        }
        if let Some(cmd) = event_secret_registration(m) {
            poll_secrets.push(cmd);
        }
    }

    pub(crate) fn send_backend(&mut self, cmd: BackendCommand) {
        if self.debug.mode {
            let name = format!("{cmd:?}");
            let name = name.split([' ', '{', '(']).next().unwrap_or("Command");
            self.trace_push("command", &format!("Command {name} sent to backend"), None);
        }
        if let Some(session) = &self.live {
            // A failed dispatch is never silent (P2-70): the
            // channel only fails when the backend worker is
            // gone, and a user action vanishing without a word
            // reads as the app ignoring the click. The failure
            // is logged with the command's name and surfaced
            // once per burst (the notice is only replaced when
            // it is not already this one).
            if session.channels.commands.send(cmd).is_err() {
                tracing::warn!("backend command could not be dispatched (worker gone)");
                let text = crate::i18n::t("notice.dispatch_failed");
                if self.notice.as_ref().map(|n| n.text.as_str()) != Some(text.as_str()) {
                    self.notice = Some(Notice::error(text));
                }
            }
        }
    }

    /// Whether the account menu may offer Log out: a session
    /// exists and this device is linked (connected now, or
    /// temporarily disconnected, in which case the backend still
    /// holds the link to end). A session that is still pairing
    /// has nothing to unlink, and a dead session cannot act.
    pub(crate) fn can_log_out(&self) -> bool {
        self.live.as_ref().is_some_and(|session| {
            !session.logout_requested
                && matches!(session.status, LinkStatus::Linked | LinkStatus::Lost(_))
        })
    }

    /// Logs the linked device out; the account menu parks the
    /// confirmation and its confirm button lands here. Sends the
    /// backend's disconnect command exactly once: the request is
    /// latched on the session, so a repeat call sends nothing.
    /// The backend unlinks the device and deletes its sign-in
    /// data when the run ends; the session object itself is
    /// dropped by [`App::end_live_session`] once the backend has
    /// finished reporting, so a deregistration failure it emits
    /// is applied (and shown) before the teardown.
    pub(crate) fn log_out(&mut self) {
        let started = self.live.as_mut().is_some_and(|session| {
            if session.logout_requested
                || !matches!(session.status, LinkStatus::Linked | LinkStatus::Lost(_))
            {
                return false;
            }
            session.logout_requested = true;
            session.status = LinkStatus::LoggingOut;
            session.clear_qr();
            true
        });
        if !started {
            return;
        }
        self.my_profile_open = false;
        // The logout plays out on the connect screen: its status
        // line names the in-flight state, then the logged-out
        // state once the session is gone.
        self.screen = crate::state::Screen::Connect;
        self.send_backend(BackendCommand::Disconnect);
    }

    /// Ends the current live session for good after a logout:
    /// the session object is dropped, the live data mirrors are
    /// cleared so nothing from the unlinked account is presented
    /// as current, and the app lands on the connect screen's
    /// logged-out state, from which a fresh link can start. The
    /// chat archive on disk is untouched: logging out ends the
    /// link, it does not erase the local archive, and the next
    /// start reloads it exactly as before.
    pub(crate) fn end_live_session(&mut self) {
        if self.live.take().is_none() {
            return;
        }
        // A load still in flight belonged to the ended session:
        // dropping its receiver discards the late snapshot (and
        // the store it carries) instead of letting it land in
        // whatever session starts next.
        self.archive_load = None;
        self.archive_reopen = None;
        self.logged_out = true;
        self.chats = Vec::new();
        self.conversation = Vec::new();
        // The presence feed belonged to the ended session: the
        // watch is dropped without an unsubscribe (the backend
        // is gone; there is nothing left to answer it) and the
        // reported state goes with it.
        self.presence_watch = None;
        self.live_presence.clear();
        self.sync_conversation();
        // The call history mirror goes with the chats: it is
        // reloaded from the archive when the next session starts.
        self.calls.live_entries = Vec::new();
        self.calls.recorded_ids.clear();
        self.my_profile.forget_account();
        self.my_profile_open = false;
        // The lock code goes with the session: it is the
        // documented recovery for a forgotten code (log out to
        // unlink), and a fresh link starts without one. The
        // chats' lock state itself is account state and is
        // untouched; only this device's local gate is removed.
        if self.lock_code.is_some() || self.app_lock_enabled {
            self.lock_code = None;
            self.app_lock_enabled = false;
            self.persist_settings();
        }
        self.app_locked_now = false;
        self.lock_logout_armed = false;
        self.screen = crate::state::Screen::Connect;
    }

    /// Keeps the presence watch on the open conversation. The
    /// watch is a single subscription: the DM chat currently
    /// open, while the session is linked. Moving it unsubscribes
    /// the chat being left and drops its reported presence, so
    /// the header only ever shows presence that is still being
    /// fed. Runs once per frame from `poll_backend`; every
    /// selection path (chat list, search, calls, notifications)
    /// is covered without each one having to know.
    ///
    /// The same reconciliation asks for the open group's member
    /// list the first time it is needed: the header's group line
    /// is the real member count, and the list is shared with the
    /// profile panel (same request set, same answers).
    pub(super) fn sync_presence_watch(&mut self) {
        let desired: Option<(u64, String)> = self.live.as_ref().and_then(|session| {
            if !session.status.is_linked() {
                return None;
            }
            let id = self.selected_chat;
            let chat = self.chats.iter().find(|c| c.id == id)?;
            if chat.kind != ChatKind::Dm {
                return None;
            }
            session.jids.get(&id).map(|jid| (id, jid.clone()))
        });
        if desired != self.presence_watch {
            if let Some((old_id, old_jid)) = self.presence_watch.take() {
                self.live_presence.remove(&old_id);
                self.send_backend(BackendCommand::SetPresenceSubscription {
                    chat_jid: old_jid,
                    subscribed: false,
                });
            }
            if let Some((id, jid)) = desired {
                self.send_backend(BackendCommand::SetPresenceSubscription {
                    chat_jid: jid.clone(),
                    subscribed: true,
                });
                self.presence_watch = Some((id, jid));
            }
        }
        let group_jid: Option<String> = self.live.as_ref().and_then(|session| {
            if !session.status.is_linked() {
                return None;
            }
            let id = self.selected_chat;
            let chat = self.chats.iter().find(|c| c.id == id)?;
            if chat.kind != ChatKind::Group || self.groups.members_requested.contains(&id) {
                return None;
            }
            session.jids.get(&id).cloned()
        });
        if let Some(jid) = group_jid {
            self.groups.members_requested.insert(self.selected_chat);
            self.send_backend(BackendCommand::FetchGroupMembers { chat_jid: jid });
        }
    }

    /// The conversation header's second line for a chat. Demo
    /// chats carry their own (presence text for DMs, member
    /// names for groups). For live chats the line is derived
    /// from live state only: the watched contact's presence for
    /// a DM, the fetched member count for a group, and nothing
    /// for a channel (the backend surfaces no channel counts).
    /// A live chat whose state has not arrived gets an empty
    /// line, never a placeholder.
    pub(crate) fn header_subtitle(&self, chat: &Chat) -> String {
        if self.live.is_none() {
            return chat.subtitle.clone();
        }
        match chat.kind {
            ChatKind::Dm => match self.live_presence.get(&chat.id) {
                Some(presence) if presence.online => crate::i18n::t("presence.online"),
                Some(presence) => presence.last_seen.map(last_seen_label).unwrap_or_default(),
                None => String::new(),
            },
            ChatKind::Group => self
                .groups
                .live_members
                .get(&chat.id)
                .map(|rows| {
                    crate::i18n::tf("conversation.member_count", "count", &rows.len().to_string())
                })
                .unwrap_or_default(),
            ChatKind::Channel => String::new(),
        }
    }

    /// Requests a pairing code for the typed phone number (digits
    /// only, the protocol wants international format without `+`).
    pub(crate) fn request_pair_code(&mut self) {
        let digits: String = self.pair_phone.chars().filter(char::is_ascii_digit).collect();
        if digits.is_empty() {
            self.notice = Some(Notice::info(
                "Type your phone number first (digits, with country code).".to_owned(),
            ));
            return;
        }
        self.send_backend(BackendCommand::ConnectPairCode { phone: digits });
    }

    /// Resolves the composer's reply target into the quote that
    /// rides on the wire for the OPEN chat. The protocol id map
    /// is global across chats, so the target must first prove it
    /// belongs to the open conversation: a quote set in another
    /// chat resolves to nothing here instead of landing in the
    /// wrong conversation. (The per-chat parking keeps the
    /// target from ever being foreign in the first place; this
    /// is the boundary check behind it.)
    pub(crate) fn live_quote_for_reply(&self) -> Option<QuotedRef> {
        let (id, _, excerpt) = self.composer.reply_to.as_ref()?;
        if !self.in_open_conversation(*id) {
            return None;
        }
        let message_id = self.protocol_ids.get(id)?.clone();
        let sender_jid = self.sender_jids.get(id).cloned();
        Some(QuotedRef {
            message_id,
            sender_jid,
            excerpt: excerpt.chars().take(200).collect(),
            remote_jid: None,
        })
    }

    /// Sends a text message to the currently selected live chat,
    /// carrying the reply context when `quoted` is present so the
    /// quote reaches the wire (extendedText contextInfo).
    pub(crate) fn send_live_text(&mut self, body: &str, quoted: Option<QuotedRef>) {
        let Some(jid) = self.live.as_ref().and_then(|s| s.jids.get(&self.selected_chat).cloned())
        else {
            return;
        };
        // The chat's disappearing timer rides on the command so
        // the wire message carries the expiration, and on the
        // local echo so the expiry pass treats it like the
        // backend's upsert will.
        let ephemeral_secs =
            self.chats.iter().find(|c| c.id == self.selected_chat).map_or(0, |c| c.ephemeral_secs);
        // The protocol ID is minted HERE, at compose time, in
        // the library's own format: the echo is filed under it,
        // the command carries it to the backend, and the wire
        // stanza goes out under it, so the echo, the send and
        // every later receipt name one ID from birth. A retry of
        // a failed send reuses the same ID (see
        // `resend_failed_message`), which is what makes a retry
        // after a post-write timeout safe: the recipient dedupes
        // on the ID instead of showing the message twice.
        let proto_id = backend::mint_message_id();
        self.send_backend(BackendCommand::SendText {
            chat_jid: jid.clone(),
            send_id: Some(proto_id.clone()),
            body: body.to_owned(),
            quoted,
            ephemeral_secs,
        });
        // Local echo, marked Sending; the backend's own upsert (when
        // it comes back) replaces it, see `apply_message_upsert`.
        self.sent_counter += 1;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        let echo_app_id = message_id(&jid, &proto_id);
        let echo = Message {
            delivery: Some(Delivery::Sending),
            expires_at: if ephemeral_secs > 0 {
                u64::try_from(now + i64::from(ephemeral_secs)).unwrap_or(0)
            } else {
                0
            },
            ..Message::mine(echo_app_id, body.to_owned(), "…")
        };
        self.protocol_ids.insert(echo_app_id, proto_id.clone());
        if let Some(session) = &mut self.live {
            // push_message records the append in the position
            // index and the ephemeral index, the same discipline
            // every other push follows (qc1642 fix 2, qc1644).
            session.push_message(self.selected_chat, now, echo);
            // The pending echo is persisted BEFORE the send can
            // resolve: if the app dies mid-send, the archive
            // restores the row as Sending (never as sent), and a
            // later upsert or receipt for the same protocol ID
            // reconciles it. Without this write the echo existed
            // only in memory and vanished on restart.
            if let Some(store) = &session.store {
                let mut archived = backend::LiveMessage::text(
                    proto_id.clone(),
                    jid.clone(),
                    None,
                    body.to_owned(),
                    now,
                    true,
                );
                archived.delivery = Some(backend::DeliveryState::Sending);
                if ephemeral_secs > 0 {
                    archived.expires_at = now + i64::from(ephemeral_secs);
                }
                let _ = store.upsert_message(&archived, 0);
            }
        }
        self.sync_conversation();
    }

    /// Binds a pending local echo of a chat to the protocol ID
    /// the backend's send went out under, and marks it Sent: the
    /// server accepted the message, which is what the single
    /// tick means. The bind is by ID first: a text echo is filed
    /// under its compose-time protocol ID, so it matches
    /// directly (and a Failed echo re-confirmed by a successful
    /// resend binds the same way). Echoes filed before stable
    /// IDs (media, location and contact echoes carry synthetic
    /// IDs until their send resolves) fall back to the
    /// oldest-pending bind. From here the backend's own upsert
    /// replaces the echo by ID and receipts find it directly.
    pub(crate) fn apply_message_sent(&mut self, chat_jid: &str, proto_id: &str) {
        let chat_id = jid_id(chat_jid);
        let mid = message_id(chat_jid, proto_id);
        self.protocol_ids.insert(mid, proto_id.to_owned());
        let mut bound = false;
        if let Some(session) = &mut self.live {
            // The search enumerates positions so the index entry
            // can follow the bind when the echo still carries a
            // synthetic ID: the echo's row keeps its slot, only
            // its ID changes (qc1642 fix 1).
            let bind = session.messages.get_mut(&chat_id).and_then(|entry| {
                let exact = entry.iter().position(|row| {
                    row.1.id == mid
                        && row.1.is_mine()
                        && matches!(row.1.delivery, Some(Delivery::Sending | Delivery::Failed))
                });
                let pos = exact.or_else(|| {
                    entry
                        .iter()
                        .enumerate()
                        .filter(|item| {
                            item.1 .1.delivery == Some(Delivery::Sending) && item.1 .1.is_mine()
                        })
                        .min_by_key(|item| item.1 .0)
                        .map(|(pos, _)| pos)
                })?;
                let echo = &mut entry[pos].1;
                let old_id = echo.id;
                echo.id = mid;
                echo.delivery = Some(Delivery::Sent);
                Some((pos, old_id))
            });
            if let Some((pos, old_id)) = bind {
                if old_id != mid {
                    session.reindex_message_id(chat_id, pos, old_id, mid);
                }
                bound = true;
            }
        }
        if bound {
            // The confirmation is archived through the monotonic
            // delivery path: a pending row advances to Sent, and
            // a row the receipts already advanced further is
            // never lowered. Inside a batch the write queues
            // with the batch's other delivery writes.
            if self.event_batch.active {
                self.event_batch.delivery_writes.push((
                    chat_jid.to_owned(),
                    proto_id.to_owned(),
                    backend::DeliveryState::Sent,
                ));
            } else if let Some(store) =
                self.live.as_ref().and_then(|session| session.store.as_ref())
            {
                let _ =
                    store.set_message_delivery(chat_jid, proto_id, backend::DeliveryState::Sent);
            }
            if self.selected_chat == chat_id {
                self.sync_conversation();
            }
        }
    }

    /// Marks every pending send echo as failed after a backend
    /// crash. Without this, a failed send sat in "Sending"
    /// forever and silently vanished on restart (logic review
    /// F6). A crash takes every in-flight send with it, across
    /// all chats; a single send's failure is not this path's to
    /// mark, it arrives typed as
    /// [`BackendEvent::MessageSendFailed`] and binds per chat
    /// through [`Self::fail_chat_sending_echo`].
    pub(crate) fn fail_pending_echoes(&mut self) {
        let mut touched = false;
        if let Some(session) = &mut self.live {
            for msgs in session.messages.values_mut() {
                for (_, m) in msgs.iter_mut() {
                    if m.delivery == Some(Delivery::Sending) {
                        m.delivery = Some(Delivery::Failed);
                        touched = true;
                    }
                }
            }
        }
        if touched {
            self.sync_conversation();
        }
    }

    /// Marks the echo a send failure names as failed and
    /// returns a copy of it, the binding for
    /// [`BackendEvent::MessageSendFailed`]. The event names its
    /// chat AND the protocol ID the send carried, so the bind
    /// is exact: the echo filed under that ID (a compose-time
    /// text echo, or a synthetic echo this failure is the first
    /// news of, which takes the protocol ID here so later
    /// evidence finds it) is the one that failed. The
    /// oldest-pending bind remains only as the fallback for an
    /// ID no echo can claim. The failed echo keeps its protocol
    /// ID bound in `protocol_ids`: Failed means unconfirmed, so
    /// a later upsert or receipt carrying that ID reconciles
    /// the echo instead of filing a duplicate (see
    /// `apply_message_upsert` and the receipt handler).
    pub(crate) fn fail_chat_sending_echo(
        &mut self,
        chat_jid: &str,
        proto_id: &str,
    ) -> Option<Message> {
        let chat_id = jid_id(chat_jid);
        let mid = message_id(chat_jid, proto_id);
        // The echo this failure names, when an earlier bind
        // (or a resend) recorded it under a synthetic ID.
        let recorded = self
            .protocol_ids
            .iter()
            .find(|(_, proto)| proto.as_str() == proto_id)
            .map(|(app_id, _)| *app_id);
        let mut failed = None;
        let mut rebind: Option<(usize, u64)> = None;
        if let Some(session) = &mut self.live {
            if let Some(entry) = session.messages.get_mut(&chat_id) {
                let exact = entry.iter().position(|row| {
                    row.1.id == mid
                        && row.1.is_mine()
                        && matches!(row.1.delivery, Some(Delivery::Sending | Delivery::Failed))
                });
                let by_record = recorded.and_then(|app_id| {
                    entry.iter().position(|row| {
                        row.1.id == app_id
                            && row.1.is_mine()
                            && row.1.delivery == Some(Delivery::Sending)
                    })
                });
                // Oldest by stored timestamp, the ordering the
                // legacy success bind used.
                let oldest = entry
                    .iter()
                    .enumerate()
                    .filter(|item| {
                        item.1 .1.delivery == Some(Delivery::Sending) && item.1 .1.is_mine()
                    })
                    .min_by_key(|item| item.1 .0)
                    .map(|(pos, _)| pos);
                if let Some(pos) = exact.or(by_record).or(oldest) {
                    let echo = &mut entry[pos].1;
                    let old_id = echo.id;
                    echo.id = mid;
                    echo.delivery = Some(Delivery::Failed);
                    failed = Some(echo.clone());
                    if old_id != mid {
                        rebind = Some((pos, old_id));
                    }
                }
            }
            if let Some((pos, old_id)) = rebind {
                session.reindex_message_id(chat_id, pos, old_id, mid);
            }
            // The failure is a local outbox transition, archived
            // exactly (Sending and Failed share a rank, so the
            // monotonic receipt path could never record it).
            if failed.is_some() {
                if let Some(store) = &session.store {
                    let _ = store.set_local_delivery(
                        chat_jid,
                        proto_id,
                        backend::DeliveryState::Failed,
                    );
                }
            }
        }
        if failed.is_some() {
            self.protocol_ids.insert(mid, proto_id.to_owned());
            if self.selected_chat == chat_id {
                self.sync_conversation();
            }
        }
        failed
    }

    /// Restores a failed text send's body into its chat's
    /// composer draft, through the same per-chat draft parking
    /// the composer itself uses (the live draft for the open
    /// chat, the parked map for any other), and persists it the
    /// way a typed draft is persisted. Only an empty draft is
    /// filled: text the user has typed since the send is theirs
    /// and is never clobbered by a late failure.
    pub(crate) fn restore_failed_text_draft(&mut self, chat_id: u64, body: &str) {
        if body.is_empty() {
            return;
        }
        if self.selected_chat == chat_id {
            if self.composer.draft.trim().is_empty() {
                self.composer.draft = body.to_owned();
                self.persist_settings();
            }
        } else if self
            .composer
            .chat_drafts
            .get(&chat_id)
            .is_none_or(|draft| draft.trim().is_empty())
        {
            self.composer.chat_drafts.insert(chat_id, body.to_owned());
            self.persist_settings();
        }
    }

    /// Warms the backend's send caches for one chat (the
    /// fire-and-forget [`BackendCommand::WarmChatCache`]): a
    /// later send to the chat then skips the preparatory
    /// queries it would otherwise pay inline. Called when a
    /// conversation is opened and for the open chat when a
    /// sync episode begins. No session (demo mode) is a no-op,
    /// and there is no answer event to wait for.
    pub(crate) fn warm_chat_cache(&mut self, chat_id: u64) {
        if let Some(jid) = self.live_jid(chat_id) {
            self.send_backend(BackendCommand::WarmChatCache { chat_jid: jid });
        }
    }

    /// Re-dispatches the send behind one failed echo of the
    /// open chat and flips the echo back to Sending, so the
    /// usual MessageSent bind confirms the retry on the same
    /// bubble. Returns false (and sends nothing) when the
    /// message is not a failed own echo, or its kind carries no
    /// reusable payload: media echoes keep no bytes (the file
    /// would have to be picked again), and interactive echoes
    /// keep only the rendered kind, not the wire spec, so
    /// neither can be re-dispatched honestly from the echo.
    ///
    /// The retry goes out under the echo's OWN protocol ID (the
    /// ID bound when the original send was filed or when its
    /// failure landed), not a fresh one: a failure that landed
    /// after the write (see [`Delivery::Failed`]) is then a
    /// duplicate the recipient dedupes on the ID, not a second
    /// copy of the message. An echo whose ID was never bound
    /// (no failure event ever named it) has nothing to reuse
    /// and is refused rather than resent under a guessed ID.
    pub(crate) fn resend_failed_message(&mut self, msg_id: u64) -> bool {
        let chat_id = self.selected_chat;
        let Some(jid) = self.live_jid(chat_id) else {
            return false;
        };
        let Some(msg) = self
            .live
            .as_ref()
            .and_then(|s| s.messages.get(&chat_id))
            .and_then(|entry| entry.iter().find(|(_, m)| m.id == msg_id))
            .map(|(_, m)| m.clone())
        else {
            return false;
        };
        if msg.delivery != Some(Delivery::Failed) || !msg.is_mine() {
            return false;
        }
        let Some(proto_id) = self.protocol_ids.get(&msg_id).cloned() else {
            return false;
        };
        let ephemeral_secs = self.ephemeral_secs_of(chat_id);
        let command = match &msg.kind {
            MessageKind::Text => BackendCommand::SendText {
                chat_jid: jid.clone(),
                send_id: Some(proto_id.clone()),
                body: msg.body.clone(),
                // The quote context is not retained on the echo
                // in wire form, so a resent text goes out plain.
                quoted: None,
                ephemeral_secs,
            },
            MessageKind::Location { name, latitude, longitude } => BackendCommand::SendLocation {
                chat_jid: jid.clone(),
                send_id: Some(proto_id.clone()),
                name: name.clone(),
                latitude: *latitude,
                longitude: *longitude,
                ephemeral_secs,
            },
            MessageKind::Contact { name, phone } => BackendCommand::SendContact {
                chat_jid: jid.clone(),
                send_id: Some(proto_id.clone()),
                name: name.clone(),
                phone: phone.clone(),
                ephemeral_secs,
            },
            _ => return false,
        };
        if let Some(session) = &mut self.live {
            if let Some(entry) = session.messages.get_mut(&chat_id) {
                if let Some((_, m)) = entry.iter_mut().find(|(_, m)| m.id == msg_id) {
                    m.delivery = Some(Delivery::Sending);
                }
            }
            // The retry is a local transition too: the archived
            // row returns to Sending until the retry resolves.
            if let Some(store) = &session.store {
                let _ = store.set_local_delivery(&jid, &proto_id, backend::DeliveryState::Sending);
            }
        }
        self.send_backend(command);
        self.sync_conversation();
        true
    }
}

#[cfg(test)]
mod qr_gate_tests {
    //! The pairing gate (item 107): the connect screen may paint
    //! a QR only from `ConnectQr::Live`, and that state must be
    //! unreachable without a pairable session holding a current
    //! server-issued payload. These tests pin the gate and the
    //! payload lifecycle around it (receipt, expiry, clearing on
    //! every session-ending transition). The incident they guard
    //! against: a failed live start (database key unreadable) left
    //! no session, and the connect screen painted a decorative
    //! placeholder code with a waiting-for-scan status; WhatsApp's
    //! scanner ignored it completely because it encoded nothing.

    use super::*;
    use crate::state::{App, Args};

    pub(super) fn app() -> (App, egui::Context) {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = App::new(Args::parse(), &ctx);
        // App::new may have attempted a real live start against
        // this machine's data dir; the gate tests own the state
        // from here.
        app.live = None;
        app.live_mode = false;
        app.live_start_error = None;
        (app, ctx)
    }

    pub(super) fn session(status: LinkStatus, with_payload: bool) -> LiveSession {
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<BackendCommand>();
        let (_evt_tx, evt_rx) = std::sync::mpsc::channel::<BackendEvent>();
        let mut session = LiveSession {
            saved_contacts: std::collections::HashMap::new(),
            saved_contact_forms: std::collections::HashMap::new(),
            retired_contact_forms: std::collections::HashMap::new(),
            saved_contacts_report_step: 0,
            orphaned_contact_forms: std::collections::HashSet::new(),
            sync_finishing: false,
            chat_positions: std::collections::HashMap::new(),
            roster_generation: 0,
            channels: BackendChannels { commands: cmd_tx, events: evt_rx },
            status,
            qr: None,
            qr_expires_at: None,
            pair_code: None,
            pair_code_at: None,
            account: String::new(),
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
            store: None,
            pending_archive: None,
            jids: HashMap::new(),
            messages: HashMap::new(),
            times: HashMap::new(),
            fake: false,
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
            poll_ballots: HashMap::new(),
            event_ballots: HashMap::new(),
        };
        if with_payload {
            session.qr = Some(backend::qr_matrix("ref,noise,identity,adv,1").unwrap());
            session.qr_expires_at =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
        }
        session
    }

    #[test]
    pub(super) fn no_session_and_no_error_is_demo() {
        let (app, _ctx) = app();
        assert!(matches!(app.connect_qr(), ConnectQr::Demo));
    }

    #[test]
    pub(super) fn failed_start_is_reported_not_painted() {
        let (mut app, _ctx) = app();
        app.live_start_error = Some("the database key could not be read".to_owned());
        match app.connect_qr() {
            ConnectQr::StartFailed(message) => {
                assert!(message.contains("database key"));
            }
            _ => panic!("a failed start must surface as StartFailed"),
        }
    }

    #[test]
    pub(super) fn failed_start_beats_a_leftover_session() {
        // A restart that aborts on the key leaves the previous
        // session struct in place; the newest fact is the failure,
        // and the gate must not resurrect the old payload.
        let (mut app, _ctx) = app();
        app.live = Some(session(LinkStatus::WaitingQr, true));
        app.live_start_error = Some("the database key could not be read".to_owned());
        assert!(matches!(app.connect_qr(), ConnectQr::StartFailed(_)));
    }

    #[test]
    pub(super) fn pairable_status_without_payload_is_connecting() {
        let (mut app, _ctx) = app();
        app.live = Some(session(LinkStatus::Starting, false));
        assert!(matches!(app.connect_qr(), ConnectQr::Connecting));
        app.live = Some(session(LinkStatus::WaitingQr, false));
        assert!(matches!(app.connect_qr(), ConnectQr::Connecting));
    }

    #[test]
    pub(super) fn pairable_status_with_payload_is_live() {
        let (mut app, _ctx) = app();
        app.live = Some(session(LinkStatus::WaitingQr, true));
        assert!(matches!(app.connect_qr(), ConnectQr::Live(_)));
        app.live = Some(session(LinkStatus::CodeReady, true));
        assert!(matches!(app.connect_qr(), ConnectQr::Live(_)));
    }

    #[test]
    pub(super) fn down_session_holding_a_payload_is_never_live() {
        // The pin for the whole gate: whatever a session still
        // holds, a status that cannot pair must not reach the
        // painter. (The lifecycle handlers also clear the payload
        // on these transitions; the gate is the second wall.)
        let (mut app, _ctx) = app();
        for status in [
            LinkStatus::Failed("boom".to_owned()),
            LinkStatus::Lost("gone".to_owned()),
            LinkStatus::Crashed("dead".to_owned()),
            LinkStatus::LoggedOut,
            LinkStatus::Banned(401),
            LinkStatus::Linked,
        ] {
            app.live = Some(session(status, true));
            assert!(
                matches!(app.connect_qr(), ConnectQr::Unavailable(_)),
                "status {:?} must not be Live",
                app.live.as_ref().unwrap().status
            );
        }
    }

    #[test]
    pub(super) fn qr_event_stores_payload_with_its_validity_window() {
        let (mut app, ctx) = app();
        app.live = Some(session(LinkStatus::Starting, false));
        let before = std::time::Instant::now();
        app.apply_event(
            &ctx,
            BackendEvent::QrCode {
                payload: "ref,noise,identity,adv,1".to_owned(),
                valid_for: std::time::Duration::from_secs(60),
            },
        );
        let live = app.live.as_ref().unwrap();
        assert_eq!(live.status, LinkStatus::WaitingQr);
        assert!(live.qr.is_some());
        let expires = live.qr_expires_at.expect("the window is stored");
        assert!(expires > before + std::time::Duration::from_secs(50));
        assert!(expires <= before + std::time::Duration::from_secs(61));
    }

    #[test]
    pub(super) fn connection_loss_and_failure_clear_the_payload() {
        let (mut app, ctx) = app();
        app.live = Some(session(LinkStatus::WaitingQr, true));
        app.apply_event(&ctx, BackendEvent::ConnectionLost { reason: "stream ended".to_owned() });
        let live = app.live.as_ref().unwrap();
        assert!(matches!(live.status, LinkStatus::Lost(_)));
        assert!(live.qr.is_none() && live.qr_expires_at.is_none());

        app.live = Some(session(LinkStatus::WaitingQr, true));
        app.apply_event(&ctx, BackendEvent::Error { message: "socket closed".to_owned() });
        let live = app.live.as_ref().unwrap();
        assert!(matches!(live.status, LinkStatus::Failed(_)));
        assert!(live.qr.is_none() && live.qr_expires_at.is_none());
    }

    #[test]
    pub(super) fn qr_expired_clears_payload_and_fails_the_attempt() {
        let (mut app, ctx) = app();
        app.live = Some(session(LinkStatus::WaitingQr, true));
        app.apply_event(&ctx, BackendEvent::QrExpired);
        let live = app.live.as_ref().unwrap();
        assert!(live.qr.is_none() && live.qr_expires_at.is_none());
        assert!(matches!(live.status, LinkStatus::Failed(_)));
    }

    #[test]
    pub(super) fn qr_expired_keeps_a_pairing_code_flow_alive() {
        // The library leaves the connection up when a pair-code
        // flow is outstanding; the on-screen code must survive
        // the QR budget ending.
        let (mut app, ctx) = app();
        let mut s = session(LinkStatus::CodeReady, true);
        s.pair_code = Some("ABCD1234".to_owned());
        app.live = Some(s);
        app.apply_event(&ctx, BackendEvent::QrExpired);
        let live = app.live.as_ref().unwrap();
        assert!(live.qr.is_none());
        assert_eq!(live.status, LinkStatus::CodeReady);
        assert_eq!(live.pair_code.as_deref(), Some("ABCD1234"));
    }

    #[test]
    pub(super) fn expiry_sweep_drops_only_a_past_due_payload() {
        let (mut app, _ctx) = app();
        let mut stale = session(LinkStatus::WaitingQr, true);
        stale.qr_expires_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        app.live = Some(stale);
        app.sweep_qr_expiry();
        assert!(app.live.as_ref().unwrap().qr.is_none());

        app.live = Some(session(LinkStatus::WaitingQr, true));
        app.sweep_qr_expiry();
        assert!(app.live.as_ref().unwrap().qr.is_some());
    }
}

#[cfg(test)]
mod logout_tests {
    use super::*;
    use crate::state::{App, Args, Screen, UiConfirm};

    const CHAT_JID: &str = "6281234567890@s.whatsapp.net";

    /// An app in live mode on a hand-fed backend channel, the
    /// same harness shape as the other live test modules. The
    /// session starts linked on the fake backend; tests that
    /// need the real backend's end-of-run semantics flip
    /// `fake` off, which is the only difference that matters to
    /// the logout paths.
    pub(super) fn live_app() -> (
        App,
        std::sync::mpsc::Sender<BackendEvent>,
        egui::Context,
        std::sync::mpsc::Receiver<BackendCommand>,
    ) {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = App::new(Args::parse(), &ctx);
        // App::new may have attempted a real live start against
        // this machine's data dir; the harness owns the state
        // from here (the same normalization the gate tests use).
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
            pending_archive: None,
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

    /// qc1642 fix 2: a push through the recording append keeps
    /// a completed index truthful: the pushed row resolves at
    /// its position, an absent ID is still proven absent by
    /// the (still complete) index, and the rows before the
    /// push keep their positions.
    #[test]
    pub(super) fn push_message_keeps_a_complete_index_truthful() {
        let (mut app, _tx, _ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        let session = app.live.as_mut().unwrap();
        session.messages.insert(
            chat_id,
            vec![
                (1_700_000_000, Message::theirs(11, "Sender", "one", "10:00")),
                (1_700_000_060, Message::theirs(12, "Sender", "two", "10:01")),
            ],
        );
        session.rebuild_msg_index(chat_id);
        session.push_message(chat_id, 1_700_000_120, Message::mine(9_000_001, "echo", "…"));
        assert_eq!(session.message_position(chat_id, 9_000_001), Some(2));
        assert_eq!(session.message_position(chat_id, 11), Some(0));
        assert_eq!(session.message_position(chat_id, 12), Some(1));
        assert_eq!(
            session.message_position(chat_id, 424_242),
            None,
            "absence is still proven, not scanned into a duplicate-prone miss"
        );
        assert!(
            session.msg_index_complete.contains(&chat_id),
            "an append moves no row, so completeness survives it"
        );
        assert_eq!(session.messages[&chat_id].len(), 3);
    }

    /// The ephemeral index (qc1644): a push registers its
    /// expiry, a re-note with a new expiry moves the entry, a
    /// drop removes it, and `take_due_ephemeral` hands out
    /// exactly the pairs whose time has come, earliest buckets
    /// first, leaving later ones indexed.
    #[test]
    pub(super) fn ephemeral_index_tracks_push_move_drop_and_due() {
        let (mut app, _tx, _ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        let session = app.live.as_mut().unwrap();
        let mut timed = Message::mine(9_000_002, "timed", "10:00");
        timed.expires_at = 500;
        session.push_message(chat_id, 1_700_000_000, timed);
        session.push_message(chat_id, 1_700_000_060, Message::mine(9_000_003, "plain", "10:01"));
        assert_eq!(session.ephemeral_index.len(), 1, "only the timed message is indexed");
        assert!(session.take_due_ephemeral(499).is_empty(), "nothing is due before its second");
        // A re-note under a later expiry moves the entry.
        session.note_ephemeral(chat_id, 9_000_002, 500, 900);
        assert!(session.take_due_ephemeral(500).is_empty(), "the old bucket no longer holds it");
        // A drop removes the entry wherever it sits.
        session.drop_ephemeral(chat_id, 9_000_002);
        assert!(session.ephemeral_index.is_empty(), "a dropped message leaves no entry");
        // Due pairs come out at their second; later ones stay.
        session.note_ephemeral(chat_id, 9_000_002, 0, 900);
        session.note_ephemeral(chat_id, 9_000_003, 0, 1_200);
        assert_eq!(session.take_due_ephemeral(900), vec![(chat_id, 9_000_002)]);
        assert_eq!(session.ephemeral_index.len(), 1, "the later bucket stays indexed");
        assert_eq!(session.take_due_ephemeral(1_200), vec![(chat_id, 9_000_003)]);
    }

    /// Mirror paging (qc1644): trimming a chat past the
    /// retention window keeps exactly the newest rows, the
    /// rebuilt index resolves every retained row at its real
    /// position and still proves a trimmed row absent, and
    /// the trimmed rows' companion state (ephemeral entries,
    /// media references) leaves with them. A chat the user
    /// paged is never trimmed.
    #[test]
    pub(super) fn trim_chat_window_keeps_newest_and_a_truthful_index() {
        let (mut app, _tx, _ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        let total = MIRROR_RETENTION + 10;
        let session = app.live.as_mut().unwrap();
        let rows: Vec<(i64, Message)> = (0..total)
            .map(|i| {
                (1_700_000_000 + i as i64, Message::theirs(i as u64 + 1, "Sender", "x", "10:00"))
            })
            .collect();
        session.messages.insert(chat_id, rows);
        session.rebuild_msg_index(chat_id);
        // Companion state on the oldest row: an expiry entry
        // and a media reference, both scoped to that row.
        session.note_ephemeral(chat_id, 1, 0, 500);
        session.photo_refs.insert(
            1,
            backend::PhotoRef {
                direct_path: String::new(),
                media_key: Vec::new(),
                file_sha256: Vec::new(),
                file_enc_sha256: Vec::new(),
                file_length: 0,
            },
        );
        session.trim_chat_window(chat_id);
        let kept = &session.messages[&chat_id];
        assert_eq!(kept.len(), MIRROR_RETENTION, "exactly the window is retained");
        assert_eq!(kept.first().map(|(_, m)| m.id), Some(11), "the oldest rows left");
        assert_eq!(kept.last().map(|(_, m)| m.id), Some(total as u64), "the newest stayed");
        let kept_ids: Vec<u64> = kept.iter().map(|(_, m)| m.id).collect();
        for (pos, id) in kept_ids.iter().enumerate() {
            assert_eq!(
                session.message_position(chat_id, *id),
                Some(pos),
                "every retained row resolves at its position"
            );
        }
        assert_eq!(
            session.message_position(chat_id, 1),
            None,
            "a trimmed row is proven absent by the complete index"
        );
        assert!(session.msg_index_complete.contains(&chat_id));
        assert!(session.ephemeral_index.is_empty(), "the trimmed expiry entry is gone");
        assert!(!session.photo_refs.contains_key(&1), "the trimmed reference is gone");
        // A paged chat keeps its extended window whole.
        session.paged_chats.insert(chat_id);
        session.push_message(chat_id, 1_700_100_000, Message::mine(9_000_009, "more", "10:09"));
        assert_eq!(
            session.messages[&chat_id].len(),
            MIRROR_RETENTION + 1,
            "a user-extended window is not trimmed back"
        );
    }

    /// qc1642 fix 3 harness: an app whose session has no store
    /// (it is away with a load worker that will never report),
    /// reopen coordinates pointing at a real archive file in a
    /// temp dir, and a pending load stamped as the caller asks.
    /// The load channel's sender comes back with the app: held,
    /// the load looks wedged; dropped, the worker looks dead.
    pub(super) fn app_with_doomed_load(
        expired: bool,
    ) -> (App, std::path::PathBuf, std::sync::mpsc::Sender<ArchiveLoad>) {
        let (mut app, _tx, _ctx, _cmd_rx) = live_app();
        let dir = std::env::temp_dir().join(format!(
            "zaprust-qc1642-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("archive.db");
        let key = [7u8; 32];
        // Create the archive the reopen must land on, then
        // close it: the reopen has to succeed against the real
        // file, not an in-memory stand-in.
        drop(store::Store::open(&path, &key).unwrap());
        app.archive_reopen = Some((path, key));
        app.live.as_mut().unwrap().store = None;
        let (load_tx, load_rx) = std::sync::mpsc::channel::<ArchiveLoad>();
        app.archive_load = Some(PendingArchiveLoad {
            rx: load_rx,
            gen: app.archive_load_gen,
            started: if expired {
                std::time::Instant::now()
                    - (ARCHIVE_LOAD_TIMEOUT + std::time::Duration::from_secs(1))
            } else {
                std::time::Instant::now()
            },
        });
        (app, dir, load_tx)
    }

    /// qc1642 fix 3, watchdog path: a load that outlives the
    /// timeout settles as a failure, the failure stays visible
    /// as start state, and the store is reopened, so a write
    /// that the old code silently skipped for the rest of the
    /// session now lands in the archive.
    #[test]
    pub(super) fn archive_load_watchdog_failure_reopens_the_store_for_writes() {
        let (mut app, dir, _load_tx) = app_with_doomed_load(true);
        app.poll_archive_load();
        assert!(app.archive_load.is_none(), "the doomed load is settled");
        assert!(app.live_start_error.is_some(), "the failure stays visible as start state");
        let store =
            app.live.as_ref().unwrap().store.as_ref().expect("the failure path reopened a store");
        store.set_blocked_chats(&[42]).unwrap();
        assert_eq!(store.blocked_chats().unwrap(), Some(vec![42]), "the write landed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// qc1642 fix 3, worker-death path: the channel disconnects
    /// without a snapshot, and the same reopen restores the
    /// session's writes.
    #[test]
    pub(super) fn archive_load_worker_death_reopens_the_store_for_writes() {
        let (mut app, dir, load_tx) = app_with_doomed_load(false);
        drop(load_tx);
        app.poll_archive_load();
        assert!(app.archive_load.is_none(), "the dead load is settled");
        assert!(app.live_start_error.is_some(), "the failure stays visible as start state");
        let store =
            app.live.as_ref().unwrap().store.as_ref().expect("the failure path reopened a store");
        store.set_blocked_chats(&[7]).unwrap();
        assert_eq!(store.blocked_chats().unwrap(), Some(vec![7]), "the write landed");
        let _ = std::fs::remove_dir_all(&dir);
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

    /// Parking the confirmation (what the account menu's Log out
    /// row does) sends nothing, and cancelling it (what the
    /// modal's Cancel, Escape and dim click do) sends nothing
    /// either: the disconnect exists only behind the confirm.
    #[test]
    pub(super) fn asking_and_cancelling_send_no_disconnect() {
        let (mut app, _tx, _ctx, cmd_rx) = live_app();
        assert!(app.can_log_out());
        app.ui_confirm = Some(UiConfirm::LogOut);
        assert!(sent_commands(&cmd_rx).is_empty(), "asking must not send");
        app.ui_confirm = None;
        assert!(sent_commands(&cmd_rx).is_empty(), "cancelling must not send");
        assert!(app.can_log_out(), "the session is untouched");
    }

    /// Confirming sends Disconnect exactly once: the request is
    /// latched on the session, so even a repeat call (a second
    /// confirm landing late) cannot send it again.
    #[test]
    pub(super) fn confirmed_log_out_sends_disconnect_exactly_once() {
        let (mut app, _tx, _ctx, cmd_rx) = live_app();
        app.log_out();
        let sent = sent_commands(&cmd_rx);
        assert_eq!(sent.len(), 1, "exactly one command: {sent:?}");
        assert!(matches!(sent[0], BackendCommand::Disconnect));
        let session = app.live.as_ref().unwrap();
        assert!(session.logout_requested);
        assert!(matches!(session.status, LinkStatus::LoggingOut));
        assert_eq!(app.screen, Screen::Connect);
        assert!(!app.can_log_out(), "no second offer while one is in flight");
        app.log_out();
        assert!(sent_commands(&cmd_rx).is_empty(), "no second disconnect");
    }

    /// A session that is not linked has nothing to unlink, and
    /// no session at all is a no-op: neither sends a command.
    #[test]
    pub(super) fn log_out_refuses_sessions_that_are_not_linked() {
        let (mut app, _tx, _ctx, cmd_rx) = live_app();
        app.live.as_mut().unwrap().status = LinkStatus::WaitingQr;
        assert!(!app.can_log_out());
        app.log_out();
        assert!(sent_commands(&cmd_rx).is_empty());
        app.live = None;
        app.log_out();
        assert!(app.live.is_none());
    }

    /// The fake backend answers Disconnect with LoggedOut and
    /// keeps running: applying the event shuts the fake down and
    /// tears the session down in the same pass, leaving the app
    /// on the connect screen's logged-out state with the ended
    /// account's data no longer presented.
    #[test]
    pub(super) fn fake_logout_event_tears_the_session_down() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "fake-1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "hello".to_owned(),
            1_700_000_000,
            false,
        )))
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!app.chats.is_empty(), "the chat mirror is populated");
        app.log_out();
        let _ = sent_commands(&cmd_rx);
        tx.send(BackendEvent::LoggedOut).unwrap();
        drain_all(&mut app, &ctx);
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(|c| matches!(c, BackendCommand::Shutdown)),
            "the fake is shut down so its supervisor exits: {sent:?}"
        );
        assert!(app.live.is_none(), "the session is dropped");
        assert!(app.logged_out);
        assert_eq!(app.screen, Screen::Connect);
        assert!(app.chats.is_empty(), "no stale chats presented as live");
        assert!(
            matches!(app.connect_qr(), ConnectQr::LoggedOut),
            "the connect gate shows the logged-out state, not the demo panel"
        );
    }

    /// The real backend reports a deregistration failure AFTER
    /// the LoggedOut event and then ends its run. Both events
    /// are applied before the teardown, so the failure wording
    /// reaches the notice instead of a clean "logged out" claim,
    /// and the closed channel then drops the session.
    #[test]
    pub(super) fn deregistration_failure_is_shown_before_the_teardown() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.live.as_mut().unwrap().fake = false;
        app.log_out();
        tx.send(BackendEvent::LoggedOut).unwrap();
        tx.send(BackendEvent::Error {
            message: "Logging out did not reach the server: offline. \
                      This device may still appear linked on your phone."
                .to_owned(),
        })
        .unwrap();
        drop(tx);
        drain_all(&mut app, &ctx);
        let notice = app.notice_str().unwrap_or_default().to_owned();
        assert!(
            notice.contains("still appear linked on your phone"),
            "the deregistration failure is the visible outcome: {notice}"
        );
        assert!(app.live.is_none(), "the ended session is dropped");
        assert!(app.logged_out);
        assert_eq!(app.screen, Screen::Connect);
    }

    /// A logout from the phone side carries no local request:
    /// the session stands while the backend is still reporting,
    /// and is torn down when its run ends (the channel closes).
    #[test]
    pub(super) fn phone_side_logout_tears_down_when_the_run_ends() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.live.as_mut().unwrap().fake = false;
        tx.send(BackendEvent::LoggedOut).unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.live.is_some(), "no teardown before the run ends");
        drop(tx);
        app.poll_backend(&ctx);
        assert!(app.live.is_none());
        assert!(app.logged_out);
        assert!(matches!(app.connect_qr(), ConnectQr::LoggedOut));
    }

    /// A closed channel with no logout behind it (the supervisor
    /// gave up after repeated crashes) must not drop the
    /// session: its failure state stays on screen.
    #[test]
    pub(super) fn channel_close_without_logout_keeps_the_session() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        app.live.as_mut().unwrap().fake = false;
        app.live.as_mut().unwrap().status =
            LinkStatus::Failed("The connection failed twice and stopped.".to_owned());
        drop(tx);
        app.poll_backend(&ctx);
        assert!(app.live.is_some(), "the failure state is kept");
        assert!(!app.logged_out);
    }
}

#[cfg(test)]
mod start_fresh_tests {
    use super::{move_stranded_data_aside, stranded_files};

    pub(super) fn temp_data_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zaprust-start-fresh-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("data dir");
        dir
    }

    /// The inventory is exactly the fail-closed set: both
    /// databases (with sidecars) and the key file. Settings and
    /// other artifacts are not stranded and must not be listed.
    #[test]
    pub(super) fn stranded_files_cover_both_databases_and_the_key_file() {
        let dir = temp_data_dir("inventory");
        std::fs::write(dir.join("archive.db"), b"a").unwrap();
        std::fs::write(dir.join("archive.db-wal"), b"a").unwrap();
        std::fs::write(dir.join("db.key"), b"k").unwrap();
        let account = dir.join("accounts").join("default");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(account.join("session.db"), b"s").unwrap();
        std::fs::write(dir.join("settings.json"), b"{}").unwrap();
        let names: Vec<String> = stranded_files(&dir)
            .iter()
            .map(|p| p.strip_prefix(&dir).unwrap().display().to_string())
            .collect();
        assert!(names.contains(&"archive.db".to_owned()), "{names:?}");
        assert!(names.contains(&"archive.db-wal".to_owned()), "{names:?}");
        assert!(names.contains(&"db.key".to_owned()), "{names:?}");
        assert!(names.contains(&"accounts/default/session.db".to_owned()), "{names:?}");
        assert!(!names.contains(&"settings.json".to_owned()), "settings stay: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The move preserves the data (bytes and layout) inside the
    /// backup folder, leaves unrelated files in place, and
    /// leaves nothing stranded behind, which is what lets the
    /// next key load mint a fresh key.
    #[test]
    pub(super) fn start_fresh_move_keeps_the_data_and_clears_the_way() {
        let dir = temp_data_dir("move");
        std::fs::write(dir.join("archive.db"), b"archive").unwrap();
        std::fs::write(dir.join("db.key"), b"malformed").unwrap();
        let account = dir.join("accounts").join("default");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(account.join("session.db"), b"session").unwrap();
        std::fs::write(dir.join("settings.json"), b"{}").unwrap();
        let backup = move_stranded_data_aside(&dir).expect("move");
        assert!(backup.starts_with(&dir));
        assert_eq!(std::fs::read(backup.join("archive.db")).unwrap(), b"archive");
        assert_eq!(std::fs::read(backup.join("db.key")).unwrap(), b"malformed");
        assert_eq!(std::fs::read(backup.join("accounts/default/session.db")).unwrap(), b"session");
        assert!(!dir.join("archive.db").exists());
        assert!(!dir.join("db.key").exists());
        assert!(!account.join("session.db").exists());
        assert!(dir.join("settings.json").exists(), "unrelated files stay put");
        assert!(stranded_files(&dir).is_empty(), "nothing stranded remains");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// How long a deferred archive load may take before the poll
/// treats it as wedged and fails it (see
/// [`App::poll_archive_load`]). Real loads take seconds even on
/// a large archive; this bound exists only so a stuck worker
/// can never trap the session behind held events.
const ARCHIVE_LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The most messages one staged-apply slice seeds, whatever
/// the clock says: the wall-time budget alone would let a
/// burst of trivial rows run far past a frame's share (the
/// same reasoning as the event drain's MAX_EVENTS_PER_FRAME).
const ARCHIVE_SLICE_MESSAGE_CAP: usize = 512;

/// How many of a chat's remaining archived messages one apply
/// slice may seed: the rest of the chat, bounded by the
/// per-slice cap minus what this slice has already seeded, so
/// a slice that crosses chat boundaries still seeds at most
/// [`ARCHIVE_SLICE_MESSAGE_CAP`] messages in total. Pure, so
/// the boundary arithmetic is unit-testable without a session
/// (see `archive_slice_tests`).
fn archive_slice_len(remaining_in_chat: usize, seeded_this_slice: usize) -> usize {
    remaining_in_chat.min(ARCHIVE_SLICE_MESSAGE_CAP.saturating_sub(seeded_this_slice))
}

/// One archive snapshot's staged apply, in flight on the
/// session (qc1644): stage 0 (see [`App::begin_archive_apply`])
/// has installed the store, the chat list and the per-chat
/// metadata; what remains here is the snapshot's messages,
/// seeded chat by chat across frames by
/// [`App::advance_archive_apply`]. The end state of the
/// mirrors is the old wholesale install's, spread over frames
/// so the UI paints between slices, with one deliberate
/// difference (qc1644): a chat larger than the mirror
/// retention window seeds only its newest rows, and the rest
/// page back from the archive on demand.
pub(crate) struct PendingArchiveApply {
    /// Sender half of the formal pending load's channel (see
    /// `begin_archive_apply`): held, never sent on, so the
    /// channel stays open and silent for exactly the staging's
    /// lifetime and `poll_backend` keeps holding event
    /// application behind it.
    _hold: std::sync::mpsc::Sender<ArchiveLoad>,
    /// Chat JIDs in snapshot order, aligned by index with
    /// [`Self::messages`].
    chat_jids: Vec<String>,
    /// Archived messages per chat, aligned by index with
    /// [`Self::chat_jids`], consumed by cursor as the seeding
    /// advances (`next_chat` / `next_message` are the record
    /// of what has been seeded; seeded vecs are never read
    /// again).
    messages: Vec<Vec<(backend::LiveMessage, i64)>>,
    /// The snapshot's call records, merged into the Calls
    /// panel when the apply finishes.
    call_records: Vec<backend::CallRecord>,
    /// Read failures the snapshot collected, surfaced once
    /// when the apply finishes, in the load's own wording.
    problems: Vec<String>,
    /// Backend commands re-registering archived poll and
    /// event creation secrets, collected while seeding and
    /// sent when the apply finishes (they need the finished
    /// session to send commands through).
    poll_secrets: Vec<BackendCommand>,
    /// Index of the chat currently being seeded.
    next_chat: usize,
    /// Messages of the current chat already seeded.
    next_message: usize,
    /// The current chat's converted rows, accumulated until
    /// the chat completes and they land in the mirror at once.
    current_rows: Vec<(i64, Message)>,
    /// The current chat's position index, built row by row
    /// alongside `current_rows`, so completing a chat costs
    /// one map insert instead of a rebuild pass over its
    /// whole vec.
    current_index: HashMap<u64, usize>,
}

/// Reads the whole archive into an [`ArchiveLoad`] snapshot:
/// the worker half of the deferred startup load (qc1636
/// Step 7, H7). It performs exactly the reads the synchronous
/// load performed, in the same order, collecting failures in
/// the same words; turning the rows into mirror state is the
/// staged apply's job ([`App::begin_archive_apply`] and
/// [`App::advance_archive_apply`]), on the UI thread, once
/// the snapshot lands. `pub(super)` so the archive-load
/// replay harness drives the real snapshot build, not a copy.
pub(super) fn load_archive_snapshot(store: store::Store) -> ArchiveLoad {
    let mut problems: Vec<String> = Vec::new();
    let avatars = match store.avatars() {
        Ok(rows) => rows,
        Err(e) => {
            problems.push(format!("saved profile pictures ({e})"));
            Vec::new()
        }
    };
    let chats = match store.chats() {
        Ok(chats) => chats,
        Err(e) => {
            problems.push(format!("the saved chat list ({e})"));
            Vec::new()
        }
    };
    let mut messages = Vec::with_capacity(chats.len());
    for chat in &chats {
        match store.messages_reporting(&chat.jid) {
            Ok((msgs, skipped)) => {
                if !skipped.is_empty() {
                    problems.push(format!("{} unreadable message(s) in one chat", skipped.len()));
                }
                messages.push(msgs);
            }
            Err(e) => {
                problems.push(format!("one chat's history ({e})"));
                messages.push(Vec::new());
            }
        }
    }
    // The Calls panel's live history rides the same snapshot:
    // it was read from the same archive at the same point as
    // the chats, and its read has always failed silently, so
    // an error here yields an empty list, not a problem entry.
    let call_records = store.call_history().unwrap_or_default();
    ArchiveLoad { store, avatars, chats, messages, call_records, problems }
}

#[cfg(test)]
mod archive_slice_tests {
    //! The staged apply's slice boundary arithmetic (qc1644):
    //! a slice seeds the rest of a small chat, never more than
    //! the cap in total, and nothing once the cap is spent.

    use super::{archive_slice_len, ARCHIVE_SLICE_MESSAGE_CAP};

    #[test]
    fn an_empty_chat_seeds_nothing() {
        assert_eq!(archive_slice_len(0, 0), 0);
    }

    #[test]
    fn a_chat_within_the_cap_seeds_in_one_slice() {
        assert_eq!(archive_slice_len(3, 0), 3);
        assert_eq!(archive_slice_len(ARCHIVE_SLICE_MESSAGE_CAP, 0), ARCHIVE_SLICE_MESSAGE_CAP);
    }

    #[test]
    fn a_large_chat_is_capped() {
        assert_eq!(archive_slice_len(ARCHIVE_SLICE_MESSAGE_CAP + 1, 0), ARCHIVE_SLICE_MESSAGE_CAP);
        assert_eq!(archive_slice_len(usize::MAX, 0), ARCHIVE_SLICE_MESSAGE_CAP);
    }

    #[test]
    fn messages_seeded_earlier_in_the_slice_consume_the_cap() {
        assert_eq!(archive_slice_len(100, ARCHIVE_SLICE_MESSAGE_CAP - 40), 40);
        assert_eq!(archive_slice_len(10, ARCHIVE_SLICE_MESSAGE_CAP - 40), 10);
    }

    #[test]
    fn a_spent_cap_seeds_nothing_more() {
        assert_eq!(archive_slice_len(100, ARCHIVE_SLICE_MESSAGE_CAP), 0);
        assert_eq!(archive_slice_len(100, ARCHIVE_SLICE_MESSAGE_CAP + 5), 0);
    }
}

#[cfg(test)]
mod archive_apply_tests {
    //! The staged archive apply (qc1644): stage 0 installs the
    //! chat list while the messages are still staged, slices
    //! seed the messages across polls, queued backend events
    //! stay held until the snapshot is fully installed, and
    //! the finished mirror matches the old wholesale install's
    //! end state.

    use super::logout_tests::live_app;
    use super::*;
    use crate::state::App;

    const CHAT_JID: &str = "6281234567890@s.whatsapp.net";
    const BIG_CHAT_JID: &str = "120363000000001@g.us";
    const EVENT_CHAT_JID: &str = "6281299999999@s.whatsapp.net";
    const SMALL_COUNT: usize = 5;

    fn live_chat(
        jid: &str,
        kind: backend::LiveChatKind,
        title: &str,
        ts: i64,
    ) -> backend::LiveChat {
        backend::LiveChat {
            jid: jid.to_owned(),
            title: title.to_owned(),
            kind,
            preview: String::new(),
            preview_sender: None,
            timestamp: ts,
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
        }
    }

    fn text_messages(chat_jid: &str, count: usize) -> Vec<(backend::LiveMessage, i64)> {
        (0..count)
            .map(|i| {
                (
                    backend::LiveMessage::text(
                        format!("m{i}"),
                        chat_jid,
                        Some("Sender".to_owned()),
                        format!("archived message {i}"),
                        1_700_000_000 + i64::try_from(i).unwrap_or(0) * 60,
                        false,
                    ),
                    0,
                )
            })
            .collect()
    }

    /// A snapshot of two chats, the second larger than one
    /// slice's cap, so the apply provably needs several slices.
    fn snapshot(big_count: usize) -> ArchiveLoad {
        ArchiveLoad {
            store: store::Store::open_in_memory().unwrap(),
            avatars: Vec::new(),
            chats: vec![
                live_chat(CHAT_JID, backend::LiveChatKind::Dm, "Sender", 1_700_000_000),
                live_chat(BIG_CHAT_JID, backend::LiveChatKind::Group, "Group chat", 1_700_000_100),
            ],
            messages: vec![
                text_messages(CHAT_JID, SMALL_COUNT),
                text_messages(BIG_CHAT_JID, big_count),
            ],
            call_records: Vec::new(),
            problems: Vec::new(),
        }
    }

    /// An app whose pending load channel will deliver `load`
    /// on the next poll, on the logout harness's live app.
    fn app_with_snapshot(
        load: ArchiveLoad,
    ) -> (App, egui::Context, std::sync::mpsc::Receiver<BackendCommand>) {
        let (mut app, _tx, ctx, cmd_rx) = live_app();
        let (load_tx, load_rx) = std::sync::mpsc::channel::<ArchiveLoad>();
        app.archive_load = Some(PendingArchiveLoad {
            rx: load_rx,
            gen: app.archive_load_gen,
            started: std::time::Instant::now(),
        });
        load_tx.send(load).unwrap();
        (app, ctx, cmd_rx)
    }

    /// Drives the staged apply to completion through the poll,
    /// one slice per call, and returns how many slices it took.
    fn drive_apply(app: &mut App) -> usize {
        let mut slices = 0;
        while app.live.as_ref().is_some_and(|session| session.pending_archive.is_some()) {
            app.poll_archive_load();
            slices += 1;
            assert!(slices < 1_000, "the staged apply did not terminate");
        }
        slices
    }

    #[test]
    fn stage_zero_installs_the_list_while_messages_are_staged() {
        let big_count = ARCHIVE_SLICE_MESSAGE_CAP + 10;
        let (mut app, _ctx, _cmd_rx) = app_with_snapshot(snapshot(big_count));
        app.poll_archive_load();
        let session = app.live.as_ref().unwrap();
        assert!(session.pending_archive.is_some(), "the messages wait in the staging");
        assert!(session.store.is_some(), "the store is installed at stage 0");
        assert!(session.messages.is_empty(), "no chat is seeded yet");
        assert_eq!(app.chats.len(), 2, "the chat list is usable from stage 0");
        assert!(app.archive_load.is_some(), "the formal load holds events while staging");
    }

    #[test]
    fn staged_apply_matches_the_wholesale_end_state() {
        let big_count = ARCHIVE_SLICE_MESSAGE_CAP + 10;
        let (mut app, _ctx, _cmd_rx) = app_with_snapshot(snapshot(big_count));
        app.poll_archive_load();
        let slices = drive_apply(&mut app);
        assert!(slices >= 2, "an over-cap archive takes several slices, took {slices}");
        assert!(app.archive_load.is_none(), "the formal load closed at the finish");
        let session = app.live.as_mut().unwrap();
        let small_id = jid_id(CHAT_JID);
        let big_id = jid_id(BIG_CHAT_JID);
        assert_eq!(session.messages[&small_id].len(), SMALL_COUNT);
        assert_eq!(session.messages[&big_id].len(), big_count);
        assert!(
            session.msg_index_complete.contains(&small_id),
            "the small chat's index is complete"
        );
        assert!(session.msg_index_complete.contains(&big_id), "the big chat's index is complete");
        let last_big = message_id(BIG_CHAT_JID, &format!("m{}", big_count - 1));
        assert_eq!(session.message_position(big_id, last_big), Some(big_count - 1));
        assert_eq!(app.selected_chat, small_id, "the finish selects the first chat");
        assert_eq!(app.conversation.len(), SMALL_COUNT, "the open conversation is synced");
    }

    #[test]
    fn queued_events_apply_only_after_the_snapshot_is_fully_installed() {
        let big_count = ARCHIVE_SLICE_MESSAGE_CAP + 10;
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let (load_tx, load_rx) = std::sync::mpsc::channel::<ArchiveLoad>();
        app.archive_load = Some(PendingArchiveLoad {
            rx: load_rx,
            gen: app.archive_load_gen,
            started: std::time::Instant::now(),
        });
        load_tx.send(snapshot(big_count)).unwrap();
        tx.send(BackendEvent::ChatUpsert(live_chat(
            EVENT_CHAT_JID,
            backend::LiveChatKind::Dm,
            "Queued",
            1_700_000_200,
        )))
        .unwrap();
        // The snapshot lands and the staging runs slice by
        // slice; for as long as any of it is outstanding, the
        // queued event must not apply.
        for _ in 0..1_000 {
            app.poll_backend(&ctx);
            let staging =
                app.live.as_ref().is_some_and(|session| session.pending_archive.is_some());
            if !staging {
                break;
            }
            assert!(
                !app.chats.iter().any(|chat| chat.id == jid_id(EVENT_CHAT_JID)),
                "the queued event applied before the snapshot was fully installed"
            );
        }
        assert!(
            app.live.as_ref().is_some_and(|session| session.pending_archive.is_none()),
            "the staged apply finished"
        );
        // With the snapshot installed, the held event applies,
        // in order, on a following poll.
        for _ in 0..64 {
            app.poll_backend(&ctx);
            if app.chats.iter().any(|chat| chat.id == jid_id(EVENT_CHAT_JID)) {
                break;
            }
        }
        assert!(
            app.chats.iter().any(|chat| chat.id == jid_id(EVENT_CHAT_JID)),
            "the queued event applied once the staging finished"
        );
    }
}
