//! Session lifecycle and the outbound path: starting and
//! ending the live session, the backend command channel, the
//! pair code, the presence watch, and sending.

use super::*;

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
        let wake: backend::WakeFn = std::sync::Arc::new(move || repaint_ctx.request_repaint());
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
            // The store starts unset: when it opened, it is
            // away with the archive-load worker and is installed
            // here when the snapshot lands (see poll_archive_load).
            // An archive that failed to open leaves it unset for
            // the session, exactly as before.
            store: None,
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
                self.apply_archive_load(load);
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

    /// Installs one archive snapshot into the mirror: the UI
    /// half of the deferred startup load. This is the old
    /// synchronous load's seeding, unchanged in what it fills
    /// and in what order; it runs when the snapshot lands, with
    /// the session taken out of `self.live` so the seeding can
    /// interleave session state and app state exactly as the
    /// in-place load did.
    fn apply_archive_load(&mut self, load: ArchiveLoad) {
        let ArchiveLoad { store, avatars, chats, messages, call_records, problems } = load;
        let Some(mut session) = self.live.take() else {
            // The session ended while its load was in flight:
            // the snapshot, and the store it carries, belonged
            // to it and go with it.
            return;
        };
        session.store = Some(store);
        // Polls whose creation secret the archive kept: after
        // the session is reinstalled, each is handed back to
        // the backend so the poll stays votable and its votes
        // stay decryptable in this session too.
        let mut archived_poll_secrets: Vec<BackendCommand> = Vec::new();
        // Chats whose messages the load installs; their
        // position indexes are built after the seeding loop.
        let mut loaded_ids: Vec<u64> = Vec::new();
        // Stored profile pictures seed the avatar cache:
        // positives stage their bytes for first display,
        // negatives remember when they were answered.
        for (jid, _picture_id, bytes, fetched_at) in avatars {
            match bytes {
                Some(bytes) => {
                    session.avatar_bytes.insert(jid, bytes);
                }
                None => {
                    session.avatar_missing.insert(jid, fetched_at);
                }
            }
        }
        for (chat, msgs) in chats.iter().zip(messages.iter()) {
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
            {
                for (m, flags) in msgs {
                    if *flags != 0 {
                        session.flags.insert(message_id(&m.chat_jid, &m.id), *flags);
                    }
                    // An opened view-once message stays
                    // opened across restarts: seed the
                    // session's opened set from the stored
                    // flag (privacy audit D13).
                    if *flags & store::FLAG_VIEW_ONCE_OPENED != 0 {
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
                        session
                            .photo_refs
                            .insert(message_id(&m.chat_jid, &m.id), reference.clone());
                    }
                    // A voice note's reference is seeded
                    // the same way, for playback.
                    if let Some(reference) = &m.voice_ref {
                        session
                            .voice_refs
                            .insert(message_id(&m.chat_jid, &m.id), reference.clone());
                    }
                    // A video's reference and thumbnail
                    // are seeded the same way, for
                    // playback and presentation.
                    if let Some(reference) = &m.video_ref {
                        session
                            .video_refs
                            .insert(message_id(&m.chat_jid, &m.id), reference.clone());
                    }
                    if let Some(thumb) = &m.video_thumbnail {
                        session.video_thumbs.insert(message_id(&m.chat_jid, &m.id), thumb.clone());
                    }
                    if let Some(reference) = &m.poster_ref {
                        session
                            .poster_refs
                            .insert(message_id(&m.chat_jid, &m.id), reference.clone());
                    }
                    if let (Some(w), Some(h)) = (m.video_width, m.video_height) {
                        session.video_dims.insert(message_id(&m.chat_jid, &m.id), (w, h));
                    }
                    // A sticker's reference and a
                    // photo's thumbnail are seeded the
                    // same way.
                    if let Some(reference) = &m.sticker_ref {
                        session
                            .sticker_refs
                            .insert(message_id(&m.chat_jid, &m.id), reference.clone());
                    }
                    if let Some(thumb) = &m.photo_thumbnail {
                        session.photo_thumbs.insert(message_id(&m.chat_jid, &m.id), thumb.clone());
                    }
                    // The media extras seed the same
                    // way: document references and
                    // thumbnails, the GIF flag, sticker
                    // animation state, location
                    // snapshots, preview images and the
                    // view-once set.
                    if let Some(reference) = &m.document_ref {
                        session
                            .document_refs
                            .insert(message_id(&m.chat_jid, &m.id), reference.clone());
                    }
                    if let Some(thumb) = &m.document_thumbnail {
                        session
                            .document_thumbs
                            .insert(message_id(&m.chat_jid, &m.id), thumb.clone());
                    }
                    if m.gif_playback {
                        session.gif_videos.insert(message_id(&m.chat_jid, &m.id));
                    }
                    if m.sticker_animated || m.sticker_lottie {
                        session.sticker_anim.insert(
                            message_id(&m.chat_jid, &m.id),
                            (m.sticker_animated, m.sticker_lottie),
                        );
                    }
                    if let Some(thumb) = &m.sticker_thumbnail {
                        session
                            .sticker_thumbs
                            .insert(message_id(&m.chat_jid, &m.id), thumb.clone());
                    }
                    if let Some(thumb) = &m.location_thumbnail {
                        session
                            .location_thumbs
                            .insert(message_id(&m.chat_jid, &m.id), thumb.clone());
                    }
                    if let Some(thumb) = m.link_preview.as_ref().and_then(|p| p.thumbnail.as_ref())
                    {
                        session
                            .preview_thumbs
                            .insert(message_id(&m.chat_jid, &m.id), thumb.clone());
                    }
                    if m.view_once {
                        session.view_once_ids.insert(message_id(&m.chat_jid, &m.id));
                    }
                    if !m.mentions.is_empty() {
                        session
                            .message_mentions
                            .insert(message_id(&m.chat_jid, &m.id), m.mentions.clone());
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
                        session
                            .poll_ballots
                            .insert(message_id(&m.chat_jid, &m.id), m.poll_ballots.clone());
                    }
                    // A poll whose creation secret was
                    // archived is re-registered with the
                    // backend below, once the session
                    // exists to send commands through.
                    if let Some(cmd) = poll_secret_registration(m) {
                        archived_poll_secrets.push(cmd);
                    }
                    // Events ride the same two rails:
                    // ballots seeded so the next response
                    // replaces the right previous answer,
                    // and the creation secret re-registered
                    // so responses to an event created
                    // before this start still decrypt.
                    if !m.event_ballots.is_empty() {
                        session
                            .event_ballots
                            .insert(message_id(&m.chat_jid, &m.id), m.event_ballots.clone());
                    }
                    if let Some(cmd) = event_secret_registration(m) {
                        archived_poll_secrets.push(cmd);
                    }
                }
                let loaded_id = jid_id(&chat.jid);
                session.messages.insert(
                    loaded_id,
                    msgs.iter()
                        .map(|(m, flags)| (m.timestamp, to_message_with_flags(m, *flags)))
                        .collect(),
                );
                loaded_ids.push(loaded_id);
            }
        }
        // Archives written before the status fix can hold the
        // bogus "status" DM row for status@broadcast: it never
        // becomes a chat here (its stored messages stay in the
        // archive, unrendered, for the Updates round).
        self.chats = chats.iter().filter(|c| c.jid != "status@broadcast").map(to_chat).collect();
        // The loaded vecs are final for now: their position
        // indexes start complete, so the first sync's lookups
        // are O(1) from the first message (qc1634 H5).
        for chat_id in loaded_ids {
            session.rebuild_msg_index(chat_id);
        }
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
        for cmd in archived_poll_secrets {
            self.send_backend(cmd);
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
        self.send_backend(BackendCommand::SendText {
            chat_jid: jid.clone(),
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
        let echo = Message {
            delivery: Some(Delivery::Sending),
            expires_at: if ephemeral_secs > 0 {
                u64::try_from(now + i64::from(ephemeral_secs)).unwrap_or(0)
            } else {
                0
            },
            ..Message::mine(9_000_000 + self.sent_counter, body.to_owned(), "…")
        };
        let echo_id = echo.id;
        if let Some(session) = &mut self.live {
            let entry = session.messages.entry(self.selected_chat).or_default();
            entry.push((now, echo));
            // The append is recorded like every other push, so
            // the position index stays a faithful map (qc1634 H5).
            session
                .msg_index
                .entry(self.selected_chat)
                .or_default()
                .insert(echo_id, entry.len() - 1);
        }
        self.sync_conversation();
    }

    /// Binds the oldest pending local echo of a chat to the
    /// protocol ID the backend's send returned, and marks it Sent:
    /// the server accepted the message, which is what the single
    /// tick means. From here the backend's own upsert replaces the
    /// echo by ID and receipts find it directly. The content
    /// matching in `apply_message_upsert` and the receipt fallback
    /// remain for messages that never produced this event (sent
    /// from another linked device, or a backend that predates it).
    pub(crate) fn apply_message_sent(&mut self, chat_jid: &str, proto_id: &str) {
        let chat_id = jid_id(chat_jid);
        let mid = message_id(chat_jid, proto_id);
        self.protocol_ids.insert(mid, proto_id.to_owned());
        let mut bound = false;
        if let Some(session) = &mut self.live {
            // The search enumerates positions so the index entry
            // can follow the bind: the echo's row keeps its slot,
            // only its ID changes (qc1642 fix 1).
            let bind = session.messages.get_mut(&chat_id).and_then(|entry| {
                let pos = entry
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| row.1.delivery == Some(Delivery::Sending) && row.1.is_mine())
                    .min_by_key(|(_, row)| row.0)
                    .map(|(pos, _)| pos)?;
                let echo = &mut entry[pos].1;
                let old_id = echo.id;
                echo.id = mid;
                echo.delivery = Some(Delivery::Sent);
                Some((pos, old_id))
            });
            if let Some((pos, old_id)) = bind {
                session.reindex_message_id(chat_id, pos, old_id, mid);
                bound = true;
            }
        }
        if bound && self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Marks pending send echoes as failed after a backend error.
    /// Without this, a failed send sat in "Sending" forever and
    /// silently vanished on restart (logic review F6). A crash takes
    /// every in-flight send with it (`all`); a single command error
    /// fails only the oldest pending echo, matching the one command
    /// the error belongs to. Errors unrelated to sending find no
    /// pending echo in the common case and change nothing.
    pub(crate) fn fail_pending_echoes(&mut self, all: bool) {
        let mut oldest: Option<(u64, usize, i64)> = None; // (chat, index, ts)
        let mut touched = false;
        if let Some(session) = &mut self.live {
            for (chat_id, msgs) in &mut session.messages {
                for (idx, (ts, m)) in msgs.iter_mut().enumerate() {
                    if m.delivery == Some(Delivery::Sending) {
                        if all {
                            m.delivery = Some(Delivery::Failed);
                            touched = true;
                        } else if oldest.is_none_or(|(_, _, ots)| *ts < ots) {
                            oldest = Some((*chat_id, idx, *ts));
                        }
                    }
                }
            }
            if let Some((chat_id, idx, _)) = oldest {
                if let Some(msgs) = session.messages.get_mut(&chat_id) {
                    if let Some((_, m)) = msgs.get_mut(idx) {
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
            store: None,
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

/// Reads the whole archive into an [`ArchiveLoad`] snapshot:
/// the worker half of the deferred startup load (qc1636
/// Step 7, H7). It performs exactly the reads the synchronous
/// load performed, in the same order, collecting failures in
/// the same words; turning the rows into mirror state is the
/// apply's job ([`App::apply_archive_load`]), on the UI
/// thread, once the snapshot lands.
fn load_archive_snapshot(store: store::Store) -> ArchiveLoad {
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
