//! Synthetic first-link sync replay: the measurement harness
//! for the event pipeline's burst cost (qc1635). It feeds a
//! realistic first-link event stream (1,000 chat upserts,
//! 20,000 history message upserts across DMs, groups and
//! LID-addressed chats, then a receipt burst and a wave of
//! sender namings) through `poll_backend` against a real
//! on-disk encrypted archive, and reports per phase: wall time,
//! store write commit units (see `Store::write_commit_count`),
//! the worst single-frame time and how many frames overran the
//! 8 ms event budget. The numbers are the before/after evidence
//! for the sync hotspots; the test is ignored because it is a
//! benchmark, not a correctness check. Run it with:
//!
//! `cargo test -p app sync_replay -- --ignored --nocapture`

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
        store: Some(store::Store::open(&dir.join("archive.db"), &[7u8; 32]).unwrap()),
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
        if app.event_queue.is_empty() {
            break;
        }
    }
    phase.wall_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let store = app.live.as_ref().and_then(|s| s.store.as_ref());
    phase.commits = store.map_or(0, store::Store::write_commit_count) - commits_before;
    assert!(app.event_queue.is_empty(), "the replay queue did not drain");
}

fn chat_upsert(jid: &str, kind: LiveChatKind, title: String, unread: u32, ts: i64) -> BackendEvent {
    BackendEvent::ChatUpsert(LiveChat {
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
    })
}

#[test]
#[ignore = "benchmark harness; run explicitly with --ignored --nocapture"]
fn sync_replay() {
    let dir = std::env::temp_dir().join(format!("zaprust-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (mut app, tx, ctx, _cmd_rx) = replay_app(&dir);

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

    // Phase 2: 20,000 message upserts. Group messages rotate
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

    // Phase 3: receipts covering 2,000 outgoing messages
    // (chunked per chat, as the wire delivers them), then 60
    // sender namings of group participants.
    let mut receipts_phase = Phase::default();
    let mut covered = 0usize;
    for (ids, (chat_jid, _, _)) in outgoing.iter().zip(chat_jids.iter()) {
        if covered >= 2_000 {
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
    for g in 0..60 {
        let ci = 700 + (g % 250);
        let p = g % 20;
        let sender_jid = format!("62855{ci:04}{p:03}@s.whatsapp.net");
        tx.send(BackendEvent::SenderNamed { sender_jid, name: format!("Named Sender {g}") })
            .unwrap();
    }
    drain_timed(&mut app, &ctx, &mut receipts_phase);

    let total_messages: usize =
        app.live.as_ref().map(|s| s.messages.values().map(Vec::len).sum()).unwrap_or(0);
    println!("SYNC REPLAY RESULTS");
    println!("{}", chats_phase.report("chats (1000 upserts)"));
    println!("{}", messages_phase.report("messages (20000 upserts)"));
    println!("{}", receipts_phase.report("receipts (2000 ids) + namings (60)"));
    println!(
        "total: wall {:.0} ms, commits {}",
        chats_phase.wall_ms + messages_phase.wall_ms + receipts_phase.wall_ms,
        chats_phase.commits + messages_phase.commits + receipts_phase.commits
    );
    assert_eq!(total_messages, 20_000, "every replayed message was filed");
    let _ = std::fs::remove_dir_all(&dir);
}
