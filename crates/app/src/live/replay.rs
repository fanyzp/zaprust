//! Synthetic first-link sync replay: the measurement harness
//! for the event pipeline's burst cost (qc1635). It feeds a
//! realistic first-link event stream (chat upserts, history
//! message upserts across DMs, groups and LID-addressed chats,
//! then a receipt burst and a wave of sender namings) through
//! `poll_backend` against a real on-disk encrypted archive,
//! and reports per phase: wall time, store write commit units
//! (see `Store::write_commit_count`), the worst single-frame
//! time and how many frames overran the 8 ms event budget. The
//! numbers are the before/after evidence for the sync
//! hotspots; the tests are ignored because they are
//! benchmarks, not correctness checks.
//!
//! Two scales run from the same driver. `sync_replay` is the
//! qc1635 baseline scale: 1,000 chats, 20,000 messages, 2,000
//! receipt ids and 60 sender namings.
//! `sync_replay_owner_scale` is the owner scale: 1,200 chats,
//! 60,000 messages, 5,000 receipt ids and 500 sender namings
//! spread over distinct sender pairs. Run both with:
//!
//! `cargo test -p app sync_replay -- --ignored --nocapture`
//!
//! Run the owner-scale harness on its own with:
//!
//! `cargo test -p app sync_replay_owner_scale -- --ignored --nocapture`
//!
//! The second harness, `archive_load_replay`, measures the
//! startup archive load the same way: a real on-disk archive
//! in the same baseline shape, the snapshot build, and the
//! staged apply driven through `poll_backend`. Run it with:
//!
//! `cargo test -p app archive_load_replay -- --ignored --nocapture`

use super::*;
use crate::state::{App, Args};
use std::time::Instant;

const BASE_TS: i64 = 1_700_000_000;
/// The event budget one frame is allowed (mirrors the budget
/// constants in this module; the harness reports against the
/// same 8 ms the pipeline grants itself).
const FRAME_BUDGET_MS: f64 = 8.0;

/// One replay phase's measured cost.
#[derive(Default)]
struct Phase {
    wall_ms: f64,
    commits: u64,
    polls: usize,
    worst_frame_ms: f64,
    frames_over_budget: usize,
}

impl Phase {
    fn report(&self, name: &str) -> String {
        format!(
            "{name}: wall {:.0} ms, commits {}, polls {}, worst frame {:.2} ms, frames over {} ms: {}",
            self.wall_ms, self.commits, self.polls, self.worst_frame_ms, FRAME_BUDGET_MS as u32, self.frames_over_budget
        )
    }
}

/// An app in live mode fed by hand, with a real on-disk
/// encrypted archive in `dir` (an in-memory store would hide
/// the SQLCipher commit cost this harness exists to measure).
fn replay_app(
    dir: &std::path::Path,
) -> (
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
        sync_finishing: false,
        chat_positions: std::collections::HashMap::new(),
        roster_generation: 0,
        channels: BackendChannels { commands: cmd_tx, events: evt_rx },
        status: LinkStatus::Linked,
        qr: None,
        qr_expires_at: None,
        pair_code: None,
        pair_code_at: None,
        account: "Replay account".to_owned(),
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
        store: Some(store::Store::open(&dir.join("archive.db"), &[7u8; 32]).unwrap()),
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

/// Drains the queue through budgeted polls, timing each poll
/// as one frame.
fn drain_timed(app: &mut App, ctx: &egui::Context, phase: &mut Phase) {
    let store = app.live.as_ref().and_then(|s| s.store.as_ref());
    let commits_before = store.map_or(0, store::Store::write_commit_count);
    let started = Instant::now();
    for _ in 0..20_000 {
        let frame = Instant::now();
        app.poll_backend(ctx);
        let frame_ms = frame.elapsed().as_secs_f64() * 1_000.0;
        phase.polls += 1;
        if frame_ms > phase.worst_frame_ms {
            phase.worst_frame_ms = frame_ms;
        }
        if frame_ms > FRAME_BUDGET_MS {
            phase.frames_over_budget += 1;
        }
        // The drain is done only when no deferred flush is
        // outstanding, because the flush carries batched store
        // writes whose time and commits this harness exists to
        // count; stopping on an empty queue alone would leave
        // that work unexecuted and uncounted.
        if app.event_queue.is_empty() && !app.event_batch.flush_pending {
            break;
        }
    }
    phase.wall_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let store = app.live.as_ref().and_then(|s| s.store.as_ref());
    phase.commits = store.map_or(0, store::Store::write_commit_count) - commits_before;
    assert!(
        app.event_queue.is_empty() && !app.event_batch.flush_pending,
        "the replay queue did not drain"
    );
}

fn live_chat(jid: &str, kind: LiveChatKind, title: String, unread: u32, ts: i64) -> LiveChat {
    LiveChat {
        jid: jid.to_owned(),
        title,
        kind,
        preview: String::new(),
        preview_sender: None,
        timestamp: ts,
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
    }
}

fn chat_upsert(jid: &str, kind: LiveChatKind, title: String, unread: u32, ts: i64) -> BackendEvent {
    BackendEvent::ChatUpsert(live_chat(jid, kind, title, unread, ts))
}

/// Drives one sync replay scale through `poll_backend`:
/// the chat upserts, the history message upserts, then the
/// receipt burst and the sender namings, each phase timed by
/// `drain_timed`. `naming_group_offset` maps a naming index
/// and the group count to the group offset from the first
/// group in the mix, so each scale controls how its namings
/// spread over groups and participants.
fn run_sync_replay(
    dir: &std::path::Path,
    chat_jids: Vec<(String, LiveChatKind, usize)>,
    receipt_id_target: usize,
    naming_count: usize,
    naming_group_offset: impl Fn(usize, usize) -> usize,
) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let (mut app, tx, ctx, _cmd_rx) = replay_app(dir);

    let total_chats = chat_jids.len();
    let expected_messages: usize = chat_jids.iter().map(|(_, _, count)| count).sum();
    // The group index arithmetic is derived from the mix, so
    // a scale with a different DM count still names senders in
    // its own groups.
    let first_group = chat_jids
        .iter()
        .position(|(_, kind, _)| *kind == LiveChatKind::Group)
        .expect("the replay mix contains groups");
    let group_count = chat_jids.iter().filter(|(_, kind, _)| *kind == LiveChatKind::Group).count();

    // Phase 1: the chat upserts, a third of them still carrying
    // a raw identifier as the title (unresolved LIDs and
    // numbers), the rest named.
    let mut chats_phase = Phase::default();
    for (i, (jid, kind, _)) in chat_jids.iter().enumerate() {
        let user = jid.split('@').next().unwrap_or(jid);
        let title = if i % 3 == 0 { user.to_owned() } else { format!("Contact {i}") };
        let unread = u32::try_from(i % 6).unwrap_or(0);
        tx.send(chat_upsert(jid, *kind, title, unread, BASE_TS)).unwrap();
    }
    drain_timed(&mut app, &ctx, &mut chats_phase);

    // Phase 2: the message upserts. Group messages rotate
    // through 20 participants per group whose labels are still
    // raw numbers; a quarter of all messages are outgoing (the
    // receipt burst below advances those).
    let mut messages_phase = Phase::default();
    let mut outgoing: Vec<Vec<String>> = Vec::new();
    for (ci, (jid, kind, count)) in chat_jids.iter().enumerate() {
        let mut chat_outgoing: Vec<String> = Vec::new();
        for mi in 0..*count {
            let id = format!("m{ci}-{mi}");
            let from_me = mi % 4 == 3;
            let mut msg = LiveMessage::text(
                id.clone(),
                jid.clone(),
                None,
                format!("replay message {mi} in chat {ci}"),
                BASE_TS + i64::try_from(mi).unwrap_or(0) * 60,
                from_me,
            );
            if *kind == LiveChatKind::Group && !from_me {
                let p = mi % 20;
                let sender_jid = format!("62855{ci:04}{p:03}@s.whatsapp.net");
                msg.sender = Some(sender_jid.split('@').next().unwrap_or("").to_owned());
                msg.sender_jid = Some(sender_jid);
            }
            if from_me {
                chat_outgoing.push(id);
            }
            tx.send(BackendEvent::MessageUpsert(msg)).unwrap();
        }
        outgoing.push(chat_outgoing);
    }
    drain_timed(&mut app, &ctx, &mut messages_phase);

    // Phase 3: receipts covering the target number of outgoing
    // messages (chunked per chat, as the wire delivers them),
    // then the sender namings of group participants.
    let mut receipts_phase = Phase::default();
    let mut covered = 0usize;
    for (ids, (chat_jid, _, _)) in outgoing.iter().zip(chat_jids.iter()) {
        if covered >= receipt_id_target {
            break;
        }
        for chunk in ids.chunks(4) {
            covered += chunk.len();
            tx.send(BackendEvent::Receipt {
                chat_jid: chat_jid.clone(),
                message_ids: chunk.to_vec(),
                kind: ReceiptKind::Read,
                at: BASE_TS + 100_000,
                actor: None,
            })
            .unwrap();
        }
    }
    for g in 0..naming_count {
        let ci = first_group + naming_group_offset(g, group_count);
        let p = g % 20;
        let sender_jid = format!("62855{ci:04}{p:03}@s.whatsapp.net");
        tx.send(BackendEvent::SenderNamed { sender_jid, name: format!("Named Sender {g}") })
            .unwrap();
    }
    drain_timed(&mut app, &ctx, &mut receipts_phase);

    let total_messages: usize =
        app.live.as_ref().map(|s| s.messages.values().map(Vec::len).sum()).unwrap_or(0);
    println!("SYNC REPLAY RESULTS");
    println!("{}", chats_phase.report(&format!("chats ({total_chats} upserts)")));
    println!("{}", messages_phase.report(&format!("messages ({expected_messages} upserts)")));
    println!(
        "{}",
        receipts_phase
            .report(&format!("receipts ({receipt_id_target} ids) + namings ({naming_count})"))
    );
    println!(
        "total: wall {:.0} ms, commits {}",
        chats_phase.wall_ms + messages_phase.wall_ms + receipts_phase.wall_ms,
        chats_phase.commits + messages_phase.commits + receipts_phase.commits
    );
    assert_eq!(total_messages, expected_messages, "every replayed message was filed");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore = "benchmark harness; run explicitly with --ignored --nocapture"]
fn sync_replay() {
    let dir = std::env::temp_dir().join(format!("zaprust-replay-{}", std::process::id()));

    // The chat mix: 700 phone DMs, 250 groups, 50 LID DMs.
    let mut chat_jids: Vec<(String, LiveChatKind, usize)> = Vec::new();
    for i in 0..700 {
        chat_jids.push((format!("62812{i:07}@s.whatsapp.net"), LiveChatKind::Dm, 12));
    }
    for i in 0..250 {
        chat_jids.push((format!("120363{i:09}@g.us"), LiveChatKind::Group, 40));
    }
    for i in 0..50 {
        chat_jids.push((format!("{i:015}@lid"), LiveChatKind::Dm, 32));
    }

    // The namings cycle through the groups one participant at
    // a time, so consecutive namings land in different groups.
    run_sync_replay(&dir, chat_jids, 2_000, 60, |g, group_count| g % group_count);
}

#[test]
#[ignore = "benchmark harness; run explicitly with --ignored --nocapture"]
fn sync_replay_owner_scale() {
    let dir = std::env::temp_dir().join(format!("zaprust-replay-owner-{}", std::process::id()));

    // The chat mix: 800 phone DMs, 350 groups, 50 LID DMs.
    let mut chat_jids: Vec<(String, LiveChatKind, usize)> = Vec::new();
    for i in 0..800 {
        chat_jids.push((format!("62812{i:07}@s.whatsapp.net"), LiveChatKind::Dm, 30));
    }
    for i in 0..350 {
        chat_jids.push((format!("120363{i:09}@g.us"), LiveChatKind::Group, 90));
    }
    for i in 0..50 {
        chat_jids.push((format!("{i:015}@lid"), LiveChatKind::Dm, 90));
    }

    // The namings walk 20 participants per group before moving
    // to the next group, so the 500 namings cover 500 distinct
    // sender pairs.
    run_sync_replay(&dir, chat_jids, 5_000, 500, |g, group_count| (g / 20) % group_count);
}

/// The archive-load counterpart of `sync_replay` (qc1644):
/// the measurement harness for the startup snapshot apply,
/// whose cost the qc1643 audit could only infer. It seeds a
/// real on-disk SQLCipher archive in the qc1635 baseline
/// shape (1,000 chats / 20,000 messages, the same mix as
/// `sync_replay`), runs the snapshot build (the store read,
/// `session::load_archive_snapshot`) and then drives the
/// staged apply through the same poll path the app uses
/// (`poll_backend`, which polls the load first), timing the
/// snapshot build, stage 0, and every apply slice as one
/// frame. Run it with:
///
/// `cargo test -p app archive_load_replay -- --ignored --nocapture`
#[test]
#[ignore = "benchmark harness; run explicitly with --ignored --nocapture"]
fn archive_load_replay() {
    let dir = std::env::temp_dir().join(format!("zaprust-archive-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let archive_path = dir.join("archive.db");

    // The chat mix, identical to sync_replay's: 700 phone DMs,
    // 250 groups, 50 LID DMs, 20,000 messages in total.
    let mut chat_jids: Vec<(String, LiveChatKind, usize)> = Vec::new();
    for i in 0..700 {
        chat_jids.push((format!("62812{i:07}@s.whatsapp.net"), LiveChatKind::Dm, 12));
    }
    for i in 0..250 {
        chat_jids.push((format!("120363{i:09}@g.us"), LiveChatKind::Group, 40));
    }
    for i in 0..50 {
        chat_jids.push((format!("{i:015}@lid"), LiveChatKind::Dm, 32));
    }

    // Seed the archive on disk: the harness measures loading
    // this shape back, so it is written with the store's own
    // batch writes and the seeding handle is dropped before
    // the load begins.
    let seed_started = Instant::now();
    {
        let seed = store::Store::open(&archive_path, &[7u8; 32]).unwrap();
        let chats: Vec<LiveChat> = chat_jids
            .iter()
            .enumerate()
            .map(|(i, (jid, kind, _))| {
                let user = jid.split('@').next().unwrap_or(jid);
                let title = if i % 3 == 0 { user.to_owned() } else { format!("Contact {i}") };
                live_chat(jid, *kind, title, u32::try_from(i % 6).unwrap_or(0), BASE_TS)
            })
            .collect();
        seed.upsert_chats(&chats).unwrap();
        for (ci, (jid, kind, count)) in chat_jids.iter().enumerate() {
            let rows: Vec<(LiveMessage, i64)> = (0..*count)
                .map(|mi| {
                    let from_me = mi % 4 == 3;
                    let mut msg = LiveMessage::text(
                        format!("m{ci}-{mi}"),
                        jid.clone(),
                        None,
                        format!("replay message {mi} in chat {ci}"),
                        BASE_TS + i64::try_from(mi).unwrap_or(0) * 60,
                        from_me,
                    );
                    if *kind == LiveChatKind::Group && !from_me {
                        let p = mi % 20;
                        let sender_jid = format!("62855{ci:04}{p:03}@s.whatsapp.net");
                        msg.sender = Some(sender_jid.split('@').next().unwrap_or("").to_owned());
                        msg.sender_jid = Some(sender_jid);
                    }
                    (msg, 0)
                })
                .collect();
            seed.upsert_messages(&rows).unwrap();
        }
    }
    let seed_ms = seed_started.elapsed().as_secs_f64() * 1_000.0;

    let (mut app, _tx, ctx, _cmd_rx) = replay_app(&dir);
    // The session's own handle closes: in the app the store
    // is away with the load worker while the snapshot is
    // built, and the session's slot stays empty until stage 0
    // installs the worker's store.
    app.live.as_mut().unwrap().store = None;

    // The snapshot build (the worker half of the load), timed
    // on its own: this is the store read the audit priced.
    let worker_store = store::Store::open(&archive_path, &[7u8; 32]).unwrap();
    let snapshot_started = Instant::now();
    let snapshot = super::session::load_archive_snapshot(worker_store);
    let snapshot_ms = snapshot_started.elapsed().as_secs_f64() * 1_000.0;
    let (load_tx, load_rx) = std::sync::mpsc::channel();
    app.archive_load = Some(PendingArchiveLoad {
        rx: load_rx,
        gen: app.archive_load_gen,
        started: Instant::now(),
    });
    load_tx.send(snapshot).unwrap();

    // Drive the app's own poll path, one poll per frame,
    // until the staged apply finishes: stage 0 is the poll
    // that lands the snapshot (the chat list appears), every
    // later poll that runs with a staging in flight is one
    // apply slice.
    let apply_started = Instant::now();
    let mut stage0_ms = 0.0f64;
    let mut slices = 0usize;
    let mut worst_slice_ms = 0.0f64;
    for _ in 0..100_000 {
        let was_staging =
            app.live.as_ref().is_some_and(|session| session.pending_archive.is_some());
        let frame = Instant::now();
        app.poll_backend(&ctx);
        let frame_ms = frame.elapsed().as_secs_f64() * 1_000.0;
        if was_staging {
            slices += 1;
            if frame_ms > worst_slice_ms {
                worst_slice_ms = frame_ms;
            }
        } else if !app.chats.is_empty() && stage0_ms == 0.0 {
            stage0_ms = frame_ms;
        }
        let staging = app.live.as_ref().is_some_and(|session| session.pending_archive.is_some());
        if !staging && app.archive_load.is_none() {
            break;
        }
    }
    let apply_ms = apply_started.elapsed().as_secs_f64() * 1_000.0;

    let session = app.live.as_ref().unwrap();
    let total_messages: usize = session.messages.values().map(Vec::len).sum();
    println!("ARCHIVE LOAD REPLAY RESULTS");
    println!("seed (1000 chats / 20000 messages written): wall {seed_ms:.0} ms");
    println!("snapshot build (store read): wall {snapshot_ms:.0} ms");
    println!("stage 0 (chat list + metadata): {stage0_ms:.2} ms");
    println!(
        "staged apply: {slices} slices, wall {apply_ms:.0} ms, worst slice {worst_slice_ms:.2} ms"
    );
    println!("total (snapshot + apply): wall {:.0} ms", snapshot_ms + apply_ms);
    assert_eq!(app.chats.len(), 1_000, "every archived chat is listed");
    assert_eq!(total_messages, 20_000, "every archived message was seeded");
    assert_eq!(
        session.msg_index_complete.len(),
        1_000,
        "every chat's position index is complete at the finish"
    );
    assert!(session.store.is_some(), "the worker's store is installed");
    let _ = std::fs::remove_dir_all(&dir);
}
