//! Backend trait contract suite, run against the FakeBackend
//! through the public supervisor API exactly as the app uses it:
//! spawn → connect → events → commands → shutdown. Timeouts are
//! generous but bounded; the fake is deterministic.

use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

use backend::{
    spawn_backend, BackendChannels, BackendCommand, BackendConfig, BackendEvent, FakeBackend,
};

fn config() -> BackendConfig {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    BackendConfig {
        data_dir: std::env::temp_dir().join(format!("zaprust-contract-{}-{n}", std::process::id())),
        account_id: "default".to_owned(),
        db_key: [0; 32],
    }
}

fn spawn(fake: FakeBackend) -> BackendChannels {
    spawn_backend(move || Box::new(fake.clone()), config(), Arc::new(|| {}))
}

fn next(ch: &BackendChannels, what: &str) -> BackendEvent {
    ch.events
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

fn collect_until(
    events: &Receiver<BackendEvent>,
    seconds: u64,
    mut done: impl FnMut(&BackendEvent) -> bool,
) -> Vec<BackendEvent> {
    let mut out = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    while std::time::Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(e) => {
                let stop = done(&e);
                out.push(e);
                if stop {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => panic!("event channel closed: {e}"),
        }
    }
    out
}

#[test]
fn disappearing_timer_sets_state_and_stamps_sent_messages() {
    let (ch, log) = linked_backend();
    // Drain the linked script first.
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    let dm = "10000000001@s.whatsapp.net";
    ch.commands
        .send(BackendCommand::SetDisappearingTimer {
            chat_jid: dm.to_owned(),
            duration_secs: 86_400,
        })
        .unwrap();
    let events =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::DisappearingChanged { .. }));
    assert!(
        events.iter().any(
            |e| matches!(e, BackendEvent::DisappearingChanged { chat_jid, duration_secs: 86_400 } if chat_jid == dm)
        ),
        "the change is echoed back: {events:?}"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, BackendEvent::ChatUpsert(c) if c.jid == dm && c.ephemeral_secs == 86_400)
        ),
        "the chat row carries the new timer: {events:?}"
    );
    // A message sent with the timer carries an absolute expiry.
    ch.commands
        .send(BackendCommand::SendText {
            chat_jid: dm.to_owned(),
            send_id: None,
            body: "gone tomorrow".to_owned(),
            quoted: None,
            ephemeral_secs: 86_400,
        })
        .unwrap();
    let events = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.body == "gone tomorrow"),
    );
    let echo = events
        .iter()
        .find_map(|e| match e {
            BackendEvent::MessageUpsert(m) if m.body == "gone tomorrow" => Some(m),
            _ => None,
        })
        .expect("the sent message is upserted");
    assert_eq!(echo.expires_at, echo.timestamp + 86_400);
    // The account default timer command reaches the backend too
    // (fire-and-forget; the ordered command stream means the log
    // line exists once the next command's event comes back).
    ch.commands
        .send(BackendCommand::SetDefaultDisappearingTimer { duration_secs: 604_800 })
        .unwrap();
    ch.commands
        .send(BackendCommand::SetDisappearingTimer { chat_jid: dm.to_owned(), duration_secs: 0 })
        .unwrap();
    let _ = collect_until(&ch.events, 5, |e| {
        matches!(e, BackendEvent::DisappearingChanged { duration_secs: 0, .. })
    });
    let log = log.lock().unwrap();
    assert!(
        log.iter().any(|l| l == "default_disappearing:604800"),
        "the default timer command is logged: {log:?}"
    );
    drop(log);
    ch.commands.send(BackendCommand::Shutdown).unwrap();
}

#[test]
fn invite_link_approval_and_join_requests_flow_through_the_fake() {
    let (ch, log) = linked_backend();
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    let group = "10000000002@g.us";
    // Invite link: fetch it, then a reset mints a different one.
    ch.commands
        .send(BackendCommand::FetchGroupInviteLink { chat_jid: group.to_owned(), reset: false })
        .unwrap();
    let events =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::GroupInviteLink { .. }));
    let link1 = events
        .iter()
        .find_map(|e| match e {
            BackendEvent::GroupInviteLink { link, .. } => Some(link.clone()),
            _ => None,
        })
        .expect("an invite link is reported");
    assert!(link1.starts_with("https://chat.whatsapp.com/"), "link shape: {link1}");
    ch.commands
        .send(BackendCommand::FetchGroupInviteLink { chat_jid: group.to_owned(), reset: true })
        .unwrap();
    let events =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::GroupInviteLink { .. }));
    let link2 = events
        .iter()
        .find_map(|e| match e {
            BackendEvent::GroupInviteLink { link, .. } => Some(link.clone()),
            _ => None,
        })
        .expect("the reset link is reported");
    assert_ne!(link1, link2, "a reset must mint a fresh link");
    // The scripted group starts with approval on; the member
    // fetch reports it, and the toggle flips it.
    ch.commands.send(BackendCommand::FetchGroupMembers { chat_jid: group.to_owned() }).unwrap();
    let events =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::GroupApprovalState { .. }));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::GroupApprovalState { enabled: true, .. })),
        "the scripted group requires approval: {events:?}"
    );
    ch.commands
        .send(BackendCommand::SetMembershipApproval { chat_jid: group.to_owned(), enabled: false })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| {
        matches!(e, BackendEvent::GroupApprovalState { enabled: false, .. })
    });
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::GroupApprovalState { enabled: false, .. })),
        "the new state is echoed: {events:?}"
    );
    // Two scripted requests wait; approving one shrinks the list
    // and lands the requester in the member list.
    ch.commands.send(BackendCommand::FetchJoinRequests { chat_jid: group.to_owned() }).unwrap();
    let events = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::JoinRequests { requests, .. } if requests.len() == 2),
    );
    assert!(
        events.iter().any(
            |e| matches!(e, BackendEvent::JoinRequests { requests, .. } if requests.len() == 2)
        ),
        "both scripted requests arrive: {events:?}"
    );
    ch.commands
        .send(BackendCommand::ReviewJoinRequest {
            chat_jid: group.to_owned(),
            participant: "10000000021@s.whatsapp.net".to_owned(),
            approve: true,
        })
        .unwrap();
    let events = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::JoinRequests { requests, .. } if requests.len() == 1),
    );
    assert!(
        events.iter().any(
            |e| matches!(e, BackendEvent::JoinRequests { requests, .. } if requests.len() == 1)
        ),
        "one request remains after the approval: {events:?}"
    );
    ch.commands.send(BackendCommand::FetchGroupMembers { chat_jid: group.to_owned() }).unwrap();
    let events = collect_until(&ch.events, 5, |e| {
        matches!(e, BackendEvent::GroupMembers { members, .. }
            if members.iter().any(|(j, _, _, _)| j == "10000000021@s.whatsapp.net"))
    });
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::GroupMembers { members, .. }
            if members.iter().any(|(j, _, _, _)| j == "10000000021@s.whatsapp.net"))),
        "the approved requester is a member now: {events:?}"
    );
    // Modes: both start off, and each setter echoes the pair.
    ch.commands.send(BackendCommand::FetchGroupMembers { chat_jid: group.to_owned() }).unwrap();
    let events =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::GroupModesState { .. }));
    assert!(
        events.iter().any(|e| matches!(
            e,
            BackendEvent::GroupModesState { announce: false, locked: false, .. }
        )),
        "both modes start off: {events:?}"
    );
    ch.commands
        .send(BackendCommand::SetGroupAnnounce { chat_jid: group.to_owned(), announce: true })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| {
        matches!(e, BackendEvent::GroupModesState { announce: true, .. })
    });
    assert!(
        events.iter().any(|e| matches!(
            e,
            BackendEvent::GroupModesState { announce: true, locked: false, .. }
        )),
        "announce turns on, locked stays off: {events:?}"
    );
    ch.commands
        .send(BackendCommand::SetGroupLocked { chat_jid: group.to_owned(), locked: true })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| {
        matches!(e, BackendEvent::GroupModesState { locked: true, .. })
    });
    assert!(
        events.iter().any(|e| matches!(
            e,
            BackendEvent::GroupModesState { announce: true, locked: true, .. }
        )),
        "both modes on: {events:?}"
    );
    // Joining by link upserts the joined group as a chat.
    ch.commands.send(BackendCommand::JoinGroupByLink { link: link1.clone() }).unwrap();
    let events = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::ChatUpsert(c) if c.jid == "10000000077@g.us"),
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BackendEvent::ChatUpsert(c) if c.jid == "10000000077@g.us")),
        "the joined group arrives as a chat: {events:?}"
    );
    let log = log.lock().unwrap();
    for needle in [
        "invite_link:",
        "membership_approval:",
        "review_join_request:",
        "join_group_by_link:",
        "group_announce:",
        "group_locked:",
    ] {
        assert!(log.iter().any(|l| l.starts_with(needle)), "log carries {needle}: {log:?}");
    }
    drop(log);
    ch.commands.send(BackendCommand::Shutdown).unwrap();
}

#[test]
fn waiting_backend_emits_qr_then_pair_code_in_crockford_set() {
    const CROCKFORD: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let ch = spawn(FakeBackend::waiting());
    ch.commands.send(BackendCommand::ConnectQr).unwrap();
    let qr = next(&ch, "QrCode");
    assert!(matches!(qr, BackendEvent::QrCode { ref payload, .. } if !payload.is_empty()));
    ch.commands.send(BackendCommand::ConnectPairCode { phone: "12025550123".to_owned() }).unwrap();
    let code = next(&ch, "PairCode");
    let BackendEvent::PairCode { code } = code else {
        panic!("expected PairCode, got {code:?}");
    };
    assert_eq!(code.len(), 8, "pair codes are 8 characters");
    assert!(
        code.chars().all(|c| CROCKFORD.contains(c)),
        "pair code {code} uses only the Crockford set"
    );
    ch.commands.send(BackendCommand::Shutdown).unwrap();
}

#[test]
fn auto_backend_connects_and_scripts_chats_and_rich_messages() {
    let ch = spawn(FakeBackend::auto());
    ch.commands.send(BackendCommand::ConnectQr).unwrap();
    let events = collect_until(&ch.events, 10, |e| matches!(e, BackendEvent::Connected { .. }));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::Connected { .. })),
        "auto backend must connect"
    );
    let more = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if matches!(m.kind, backend::LiveMessageKind::Poll { .. })),
    );
    let all: Vec<&BackendEvent> = events.iter().chain(more.iter()).collect();
    assert!(all.iter().any(|e| matches!(e, BackendEvent::ChatUpsert(_))), "scripted chats arrive");
    assert!(
        all.iter().any(|e| matches!(e, BackendEvent::MessageUpsert(_))),
        "scripted messages arrive"
    );
    ch.commands.send(BackendCommand::Shutdown).unwrap();
}

#[test]
fn e2e_fake_backend_events_persist_in_the_archive_and_reload() {
    // The app-level flow without a window: connect → linked →
    // receive → write-through to the archive → restart → the
    // archive still holds chats and messages.
    let dir = std::env::temp_dir().join(format!("zaprust-e2e-{}", std::process::id()));
    let db = dir.join("archive.db");
    let _ = std::fs::remove_file(&db);
    let ch = spawn(FakeBackend::auto());
    ch.commands.send(BackendCommand::ConnectQr).unwrap();
    let events = collect_until(
        &ch.events,
        10,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if matches!(m.kind, backend::LiveMessageKind::Poll { .. })),
    );
    assert!(events.iter().any(|e| matches!(e, BackendEvent::Connected { .. })));
    {
        let store = store::Store::open(&db, &[5u8; 32]).unwrap();
        for e in &events {
            match e {
                BackendEvent::ChatUpsert(chat) => store.upsert_chat(chat).unwrap(),
                BackendEvent::MessageUpsert(m) => store.upsert_message(m, 0).unwrap(),
                _ => {}
            }
        }
        assert!(!store.chats().unwrap().is_empty(), "chats archived");
    }
    ch.commands.send(BackendCommand::Shutdown).unwrap();
    // "Restart": a fresh store handle over the same file.
    let store = store::Store::open(&db, &[5u8; 32]).unwrap();
    let chats = store.chats().unwrap();
    assert!(!chats.is_empty());
    let msgs = store.messages(&chats[0].jid).unwrap();
    assert!(!msgs.is_empty(), "messages survive the restart");
}

#[test]
fn privacy_commands_are_logged_by_the_fake() {
    let log: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fake = FakeBackend::auto_with_log(log.clone());
    let ch = spawn(fake);
    ch.commands.send(BackendCommand::ConnectQr).unwrap();
    let _ = collect_until(&ch.events, 10, |e| matches!(e, BackendEvent::Connected { .. }));
    ch.commands
        .send(BackendCommand::SetPrivacy {
            category: "readreceipts".to_owned(),
            value: "none".to_owned(),
        })
        .unwrap();
    ch.commands
        .send(BackendCommand::MarkAsRead {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            sender_jid: None,
            message_ids: vec!["m1".to_owned()],
        })
        .unwrap();
    ch.commands.send(BackendCommand::Shutdown).unwrap();
    // Give the worker a moment to drain the log writes.
    std::thread::sleep(Duration::from_millis(300));
    let lines = log.lock().unwrap().join("\n");
    assert!(lines.contains("privacy:readreceipts=none"), "log: {lines}");
    assert!(lines.contains("mark_as_read:"), "log: {lines}");
}

/// Item 111: the presence contract. Subscribing to the scripted
/// DM contact is answered with that contact's current presence
/// (online), exactly as the real backend answers a subscription
/// with the server's report; both halves of the subscription
/// land in the command log.
#[test]
fn presence_subscription_is_answered_with_the_contacts_presence() {
    let (ch, log) = linked_backend();
    // Drain the linked script first.
    let _ = collect_until(&ch.events, 3, |e| matches!(e, BackendEvent::ChatUpsert(_)));

    ch.commands
        .send(BackendCommand::SetPresenceSubscription {
            chat_jid: backend::FAKE_DM_JID.to_owned(),
            subscribed: true,
        })
        .unwrap();
    let events =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::PresenceReceived { .. }));
    let report = events.iter().find_map(|e| match e {
        BackendEvent::PresenceReceived { chat_jid, online, last_seen } => {
            Some((chat_jid.clone(), *online, *last_seen))
        }
        _ => None,
    });
    assert_eq!(
        report,
        Some((backend::FAKE_DM_JID.to_owned(), true, None)),
        "the subscription is answered with the contact's current presence"
    );

    ch.commands
        .send(BackendCommand::SetPresenceSubscription {
            chat_jid: backend::FAKE_DM_JID.to_owned(),
            subscribed: false,
        })
        .unwrap();
    ch.commands.send(BackendCommand::Shutdown).unwrap();
    // Give the worker a moment to drain the log writes.
    std::thread::sleep(Duration::from_millis(300));
    let lines = log.lock().unwrap().join("\n");
    let dm = backend::FAKE_DM_JID;
    assert!(lines.contains(&format!("presence:{dm}:true")), "log: {lines}");
    assert!(lines.contains(&format!("presence:{dm}:false")), "log: {lines}");
}

#[test]
fn fake_privacy_state_round_trips_through_set_privacy() {
    let (ch, _log) = linked_backend();
    // The linked script reports the baseline privacy state, as
    // the real backend reports its connect-time fetch.
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::PrivacyState(_)));
    let baseline = events.iter().find_map(|e| match e {
        BackendEvent::PrivacyState(map) => Some(map),
        _ => None,
    });
    let baseline = baseline.expect("the linked script reports a privacy state");
    assert_eq!(baseline.get("readreceipts").map(String::as_str), Some("all"));
    assert_eq!(baseline.get("last").map(String::as_str), Some("contacts"));

    // A change is adopted by the fake account and the full
    // updated state is re-emitted, mirroring the real backend's
    // refetch after a successful SetPrivacy: the category the
    // command did not touch is still reported.
    ch.commands
        .send(BackendCommand::SetPrivacy {
            category: "readreceipts".to_owned(),
            value: "none".to_owned(),
        })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::PrivacyState(_)));
    let updated = events.iter().find_map(|e| match e {
        BackendEvent::PrivacyState(map) => Some(map),
        _ => None,
    });
    let updated = updated.expect("SetPrivacy re-emits the privacy state");
    assert_eq!(updated.get("readreceipts").map(String::as_str), Some("none"));
    assert_eq!(
        updated.get("last").map(String::as_str),
        Some("contacts"),
        "the re-emitted state is the full map, not just the changed entry"
    );
    ch.commands.send(BackendCommand::Shutdown).unwrap();
}

#[test]
fn fake_linked_script_reports_the_baseline_own_profile() {
    let (ch, _log) = linked_backend();
    // The linked script reports the account's own profile, as
    // the real backend reports its connect-time read-back.
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::OwnProfile(_)));
    let profile = events.iter().find_map(|e| match e {
        BackendEvent::OwnProfile(p) => Some(p),
        _ => None,
    });
    let profile = profile.expect("the linked script reports the own profile");
    assert_eq!(profile.name, "Fake User");
    assert_eq!(profile.about, "Available (test account)");
    assert_eq!(profile.photo, None);
    ch.commands.send(BackendCommand::Shutdown).unwrap();
}

#[test]
fn own_profile_update_round_trips_through_the_fake() {
    let (ch, log) = linked_backend();
    let _ = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::OwnProfile(_)));

    // FetchOwnProfile is answered with the current profile.
    ch.commands.send(BackendCommand::FetchOwnProfile).unwrap();
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::OwnProfile(_)));
    assert!(events
        .iter()
        .any(|e| matches!(e, BackendEvent::OwnProfile(p) if p.name == "Fake User")));

    // One save carrying a new name and a new picture: both are
    // adopted, the field the command did not carry (the About)
    // is untouched, and the full updated profile is re-emitted,
    // mirroring the real backend's re-read after a change.
    ch.commands
        .send(BackendCommand::UpdateOwnProfile {
            name: Some("New Name".to_owned()),
            about: None,
            photo: Some(Some(vec![1, 2, 3])),
        })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::OwnProfile(_)));
    let updated = events.iter().find_map(|e| match e {
        BackendEvent::OwnProfile(p) => Some(p),
        _ => None,
    });
    let updated = updated.expect("UpdateOwnProfile re-emits the profile");
    assert_eq!(updated.name, "New Name");
    assert_eq!(updated.about, "Available (test account)", "uncarried fields stay");
    assert_eq!(updated.photo.as_deref(), Some([1, 2, 3].as_slice()));

    // A second save changing only the About (to empty: a clear)
    // and removing the picture leaves the name alone.
    ch.commands
        .send(BackendCommand::UpdateOwnProfile {
            name: None,
            about: Some(String::new()),
            photo: Some(None),
        })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::OwnProfile(_)));
    let updated = events.iter().find_map(|e| match e {
        BackendEvent::OwnProfile(p) => Some(p),
        _ => None,
    });
    let updated = updated.expect("the second update re-emits the profile");
    assert_eq!(updated.name, "New Name");
    assert_eq!(updated.about, "");
    assert_eq!(updated.photo, None);

    ch.commands.send(BackendCommand::Shutdown).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let lines = log.lock().unwrap().join("\n");
    assert!(lines.contains("profile:name=New Name"), "log: {lines}");
    assert!(lines.contains("profile:photo=set:3"), "log: {lines}");
    assert!(lines.contains("profile:about="), "log: {lines}");
    assert!(lines.contains("profile:photo=remove"), "log: {lines}");
}

#[test]
fn own_profile_update_with_an_empty_name_fails_and_changes_nothing() {
    let (ch, _log) = linked_backend();
    let _ = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::OwnProfile(_)));

    // An empty name is refused before anything is applied (the
    // library refuses it too), so the About carried by the same
    // save must not land either: the failure is reported and the
    // reconciling re-read still shows the baseline profile.
    ch.commands
        .send(BackendCommand::UpdateOwnProfile {
            name: Some("   ".to_owned()),
            about: Some("Should not land".to_owned()),
            photo: None,
        })
        .unwrap();
    let events =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::OwnProfileSaveFailed { .. }));
    let failed = events.iter().find_map(|e| match e {
        BackendEvent::OwnProfileSaveFailed { message } => Some(message),
        _ => None,
    });
    let failed = failed.expect("the empty name is reported as a save failure");
    assert!(failed.contains("name cannot be empty"), "message: {failed}");
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::OwnProfile(_)));
    let reconciled = events.iter().find_map(|e| match e {
        BackendEvent::OwnProfile(p) => Some(p),
        _ => None,
    });
    let reconciled = reconciled.expect("a reconciling profile re-read follows the failure");
    assert_eq!(reconciled.name, "Fake User");
    assert_eq!(reconciled.about, "Available (test account)");
    ch.commands.send(BackendCommand::Shutdown).unwrap();
}

#[test]
fn fake_baseline_includes_online_and_round_trips_it() {
    let (ch, _log) = linked_backend();
    // The baseline carries the Online audience with a value the
    // category actually accepts (`all` or `match_last_seen`), so
    // the Privacy page's Online row is populated in demo mode.
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::PrivacyState(_)));
    let baseline = events.iter().find_map(|e| match e {
        BackendEvent::PrivacyState(map) => Some(map),
        _ => None,
    });
    let baseline = baseline.expect("the linked script reports a privacy state");
    assert_eq!(baseline.get("online").map(String::as_str), Some("match_last_seen"));

    // Setting Online re-emits the full map with the new value,
    // like the real backend's refetch after a change.
    ch.commands
        .send(BackendCommand::SetPrivacy { category: "online".to_owned(), value: "all".to_owned() })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::PrivacyState(_)));
    let updated = events.iter().find_map(|e| match e {
        BackendEvent::PrivacyState(map) => Some(map),
        _ => None,
    });
    let updated = updated.expect("SetPrivacy re-emits the privacy state");
    assert_eq!(updated.get("online").map(String::as_str), Some("all"));
    assert_eq!(
        updated.get("last").map(String::as_str),
        Some("contacts"),
        "the re-emitted state is the full map, not just the changed entry"
    );
    ch.commands.send(BackendCommand::Shutdown).unwrap();
}

#[test]
fn fake_baseline_carries_the_rowless_categories() {
    let (ch, _log) = linked_backend();
    // The categories the Privacy page has no row for are part
    // of the account state the real backend reports (audit
    // finding D6), so the fake's baseline carries them too,
    // with values their categories accept.
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::PrivacyState(_)));
    let baseline = events.iter().find_map(|e| match e {
        BackendEvent::PrivacyState(map) => Some(map),
        _ => None,
    });
    let baseline = baseline.expect("the linked script reports a privacy state");
    assert_eq!(baseline.get("calladd").map(String::as_str), Some("known"));
    assert_eq!(baseline.get("messages").map(String::as_str), Some("contacts"));
    assert_eq!(baseline.get("defense").map(String::as_str), Some("off"));
    ch.commands.send(BackendCommand::Shutdown).unwrap();
}

#[test]
fn scripted_group_delivers_location_carousel_and_contact_kinds() {
    let ch = spawn(FakeBackend::auto());
    ch.commands.send(BackendCommand::ConnectQr).unwrap();
    let events = collect_until(&ch.events, 10, |e| {
        matches!(e, BackendEvent::MessageUpsert(m)
            if matches!(m.kind, backend::LiveMessageKind::Contact { .. }))
    });
    let kinds: Vec<&backend::LiveMessageKind> = events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::MessageUpsert(m) => Some(&m.kind),
            _ => None,
        })
        .collect();
    assert!(
        kinds.iter().any(|k| matches!(k, backend::LiveMessageKind::Location { .. })),
        "location card arrives"
    );
    assert!(
        kinds.iter().any(
            |k| matches!(k, backend::LiveMessageKind::Carousel { cards, .. } if cards.len() == 2)
        ),
        "carousel with two cards arrives"
    );
    assert!(
        kinds.iter().any(|k| matches!(k, backend::LiveMessageKind::Contact { .. })),
        "contact card arrives"
    );
    ch.commands.send(BackendCommand::Shutdown).unwrap();
}

#[test]
fn create_poll_vote_and_react_commands_flow_through_the_fake() {
    let ch = spawn(FakeBackend::auto());
    ch.commands.send(BackendCommand::ConnectQr).unwrap();
    let _ = collect_until(&ch.events, 10, |e| matches!(e, BackendEvent::Connected { .. }));
    ch.commands
        .send(BackendCommand::CreatePoll {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            question: "Contract poll?".to_owned(),
            options: vec!["Yes".to_owned(), "No".to_owned()],
            multiple: false,
        })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| {
        matches!(e, BackendEvent::MessageUpsert(m)
            if matches!(&m.kind, backend::LiveMessageKind::Poll { question, .. } if question == "Contract poll?"))
    });
    let poll_id = events
        .iter()
        .find_map(|e| match e {
            BackendEvent::MessageUpsert(m)
                if matches!(&m.kind, backend::LiveMessageKind::Poll { question, .. } if question == "Contract poll?") =>
            {
                Some(m.id.clone())
            }
            _ => None,
        })
        .expect("created poll is upserted");
    ch.commands
        .send(BackendCommand::VotePoll {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: poll_id.clone(),
            options: vec![0],
        })
        .unwrap();
    let voted = collect_until(&ch.events, 5, |e| {
        matches!(e, BackendEvent::PollVote { message_id: id, from_me: true, selected, .. }
            if *id == poll_id && selected == &vec![0])
    });
    assert!(
        voted.iter().any(|e| matches!(e, BackendEvent::PollVote { from_me: true, .. })),
        "the vote is answered by a PollVote event carrying my selection"
    );
    ch.commands
        .send(BackendCommand::React {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: "fake-1".to_owned(),
            sender_jid: None,
            from_me: false,
            emoji: "\u{1F44D}".to_owned(),
        })
        .unwrap();
    let reacted =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::ReactionUpdate { .. }));
    assert!(
        reacted.iter().any(|e| matches!(e, BackendEvent::ReactionUpdate { .. })),
        "the reaction update is emitted"
    );
    ch.commands.send(BackendCommand::Shutdown).unwrap();
}

/// Creates a poll on a linked fake backend and returns its
/// protocol id. The poll has three options so vote sets can grow.
fn create_poll(ch: &BackendChannels, question: &str, multiple: bool) -> String {
    ch.commands
        .send(BackendCommand::CreatePoll {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            question: question.to_owned(),
            options: vec!["Alpha".to_owned(), "Beta".to_owned(), "Gamma".to_owned()],
            multiple,
        })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| {
        matches!(e, BackendEvent::MessageUpsert(m)
            if matches!(&m.kind, backend::LiveMessageKind::Poll { question: q, .. } if q == question))
    });
    events
        .iter()
        .find_map(|e| match e {
            BackendEvent::MessageUpsert(m)
                if matches!(&m.kind, backend::LiveMessageKind::Poll { question: q, .. } if q == question) =>
            {
                Some(m.id.clone())
            }
            _ => None,
        })
        .expect("created poll is upserted")
}

/// Waits for the PollVote event answering a vote of mine on
/// `poll_id` with exactly `want_selected` as the selection. The
/// event carries my whole current vote set (replace-set
/// semantics); aggregating it into counts is the app's job.
fn await_poll_vote(ch: &BackendChannels, poll_id: &str, want_selected: &[usize]) {
    let is_want = |e: &BackendEvent| {
        matches!(e, BackendEvent::PollVote { message_id, from_me: true, selected, .. }
            if message_id == poll_id && selected == want_selected)
    };
    let events = collect_until(&ch.events, 5, is_want);
    assert!(
        events.iter().any(is_want),
        "poll {poll_id} must answer with a PollVote selecting {want_selected:?}: {events:?}"
    );
}

fn vote(ch: &BackendChannels, poll_id: &str, options: Vec<usize>) {
    ch.commands
        .send(BackendCommand::VotePoll {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: poll_id.to_owned(),
            options,
        })
        .unwrap();
}

#[test]
fn vote_poll_with_an_empty_set_retracts_my_vote() {
    // Contract: VotePoll replaces my previous vote set, so sending
    // an empty set after voting answers with a PollVote whose
    // selection is empty: the retraction. (The old fake applied
    // the payload as toggle clicks, so an empty payload changed
    // nothing and retraction was impossible.)
    let (ch, _log) = linked_backend();
    let poll_id = create_poll(&ch, "Retract poll?", false);
    vote(&ch, &poll_id, vec![0]);
    await_poll_vote(&ch, &poll_id, &[0]);
    vote(&ch, &poll_id, vec![]);
    await_poll_vote(&ch, &poll_id, &[]);
    ch.commands.send(BackendCommand::Shutdown).unwrap();
}

#[test]
fn vote_poll_multi_select_change_keeps_the_votes_it_replaces() {
    // Contract: after voting [0], sending [0, 1] answers with the
    // grown set [0, 1]: a replace, not a sequence of toggles.
    // (The old fake toggled 0 off and 1 on, ending at [1].)
    let (ch, _log) = linked_backend();
    let poll_id = create_poll(&ch, "Multi change poll?", true);
    vote(&ch, &poll_id, vec![0]);
    await_poll_vote(&ch, &poll_id, &[0]);
    vote(&ch, &poll_id, vec![0, 1]);
    await_poll_vote(&ch, &poll_id, &[0, 1]);
    ch.commands.send(BackendCommand::Shutdown).unwrap();
}

#[test]
fn register_poll_secret_reaches_the_backend() {
    // Contract: a creation secret the app archived in an earlier
    // session is handed back with RegisterPollSecret, silently
    // (no event answers it); the fake records it in the command
    // log like every other command.
    let (ch, log) = linked_backend();
    ch.commands
        .send(BackendCommand::RegisterPollSecret {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: "archived-poll-1".to_owned(),
            creator_jid: "10000000001@s.whatsapp.net".to_owned(),
            secret: vec![7u8; 32],
            options: vec!["Alpha".to_owned(), "Beta".to_owned()],
        })
        .unwrap();
    let want = ["poll_secret:", "10000000001@s.whatsapp.net", ":", "archived-poll-1"].concat();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut seen = false;
    while std::time::Instant::now() < deadline && !seen {
        seen = log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(&want);
        if !seen {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    assert!(seen, "the registration reaches the backend: {log:?}");
    ch.commands.send(BackendCommand::Shutdown).unwrap();
}

/// Drives a fresh auto backend to the linked state and returns the
/// channels plus the command log (shared with the fake).
fn linked_backend() -> (BackendChannels, Arc<std::sync::Mutex<Vec<String>>>) {
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fake = FakeBackend::auto_with_log(log.clone());
    let ch = spawn(fake);
    ch.commands.send(BackendCommand::ConnectQr).unwrap();
    let _ = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::Connected { .. }));
    (ch, log)
}

#[test]
fn star_and_pin_message_commands_update_state_and_log() {
    let (ch, log) = linked_backend();
    // Drain the scripted flow first.
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    ch.commands
        .send(BackendCommand::StarMessage {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: "fake-1".to_owned(),
            sender_jid: None,
            from_me: false,
            star: true,
        })
        .unwrap();
    let events = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-1" && m.starred),
    );
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::MessageUpsert(m) if m.starred)),
        "the star command re-upserts the message with starred set"
    );
    ch.commands
        .send(BackendCommand::PinMessage {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: "fake-1".to_owned(),
            sender_jid: None,
            from_me: false,
            pin: true,
            duration: backend::PinDuration::Days7,
        })
        .unwrap();
    let events = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-1" && m.pinned),
    );
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::MessageUpsert(m) if m.pinned)),
        "the pin command re-upserts the message with pinned set"
    );
    let log = log.lock().unwrap();
    assert!(log.iter().any(|l| l.starts_with("star:")), "log: {log:?}");
    assert!(log.iter().any(|l| l.starts_with("pin_message:")), "log: {log:?}");
}

#[test]
fn forward_command_copies_the_message_into_the_target_chat() {
    let (ch, _log) = linked_backend();
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    ch.commands
        .send(BackendCommand::ForwardMessage {
            from_chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: "fake-1".to_owned(),
            to_chat_jid: "10000000002@g.us".to_owned(),
        })
        .unwrap();
    let events = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.chat_jid.ends_with("@g.us") && m.forwarded),
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BackendEvent::MessageUpsert(m) if m.forwarded && m.from_me)),
        "the forwarded copy lands in the group, marked forwarded and from me"
    );
}

#[test]
fn forward_command_refuses_view_once_media() {
    let (ch, _log) = linked_backend();
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    // fake-vo-1 is the scripted view-once video in the DM.
    ch.commands
        .send(BackendCommand::ForwardMessage {
            from_chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: "fake-vo-1".to_owned(),
            to_chat_jid: "10000000002@g.us".to_owned(),
        })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::Error { .. }));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BackendEvent::Error { message } if message.contains("View-once"))),
        "forwarding view-once media must be refused with an error"
    );
    assert!(
        !events.iter().any(|e| matches!(e, BackendEvent::MessageUpsert(m) if m.forwarded)),
        "no forwarded copy of the view-once message may appear"
    );
}

#[test]
fn chat_action_commands_update_the_chat_row() {
    let (ch, _log) = linked_backend();
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    ch.commands
        .send(BackendCommand::ChatAction {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            action: backend::ChatAction::Archive,
        })
        .unwrap();
    let events =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::ChatUpsert(c) if c.archived));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::ChatUpsert(c) if c.archived && !c.pinned)),
        "archive flips only the archived flag"
    );
    ch.commands
        .send(BackendCommand::ChatAction {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            action: backend::ChatAction::Pin,
        })
        .unwrap();
    let events = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::ChatUpsert(c) if c.pinned && c.archived),
    );
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::ChatUpsert(c) if c.pinned && c.archived)),
        "pin keeps the archived state"
    );
}

#[test]
fn create_community_answers_with_the_created_event() {
    let (ch, log) = linked_backend();
    ch.commands
        .send(BackendCommand::CreateCommunity { name: "Harbor Community".to_owned() })
        .unwrap();
    let events = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::CommunityCreated { name, .. } if name == "Harbor Community"),
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            BackendEvent::CommunityCreated { jid, name }
                if name == "Harbor Community" && jid.ends_with("@g.us")
        )),
        "create community answers with its event and JID"
    );
    let log = log.lock().unwrap();
    assert!(
        log.iter().any(|entry| entry == "create_community:Harbor Community"),
        "the command is logged"
    );
}

#[test]
fn create_group_fetch_members_and_download_honesty() {
    let (ch, _log) = linked_backend();
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    ch.commands
        .send(BackendCommand::CreateGroup {
            name: "Weekend Plans".to_owned(),
            participants: vec!["10000000001@s.whatsapp.net".to_owned()],
        })
        .unwrap();
    let events = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::ChatUpsert(c) if c.title == "Weekend Plans"),
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BackendEvent::ChatUpsert(c) if c.title == "Weekend Plans")),
        "create group upserts the new chat"
    );
    ch.commands
        .send(BackendCommand::FetchGroupMembers { chat_jid: "10000000002@g.us".to_owned() })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::GroupMembers { .. }));
    let members_ok = events.iter().any(|e| match e {
        BackendEvent::GroupMembers { members, .. } => {
            members.iter().any(|(_, _, admin, _)| *admin) && members.len() >= 2
        }
        _ => false,
    });
    assert!(members_ok, "the scripted member list carries an admin flag");
    ch.commands
        .send(BackendCommand::DownloadMedia {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: "fake-photo-1".to_owned(),
            document_ref: None,
        })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::Error { .. }));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::Error { message } if message.contains("not available in the demo"))),
        "test mode answers the download with an honest error, not fake bytes"
    );
}

#[test]
fn scripted_dm_includes_photo_and_view_once_video() {
    let (ch, _log) = linked_backend();
    let events = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    let has_photo = events.iter().any(|e| matches!(e, BackendEvent::MessageUpsert(m)
        if matches!(m.kind, backend::LiveMessageKind::Media { media: backend::MediaKind::Photo, .. })));
    let has_vo = events.iter().any(|e| matches!(e, BackendEvent::MessageUpsert(m)
        if m.view_once && matches!(m.kind, backend::LiveMessageKind::Media { media: backend::MediaKind::Video, .. })));
    assert!(has_photo, "scripted DM carries a photo media message");
    assert!(has_vo, "scripted DM carries a view-once video");
}

#[test]
fn edit_and_revoke_commands_emit_their_events() {
    let (ch, _log) = linked_backend();
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    ch.commands
        .send(BackendCommand::EditMessage {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: "fake-1".to_owned(),
            new_body: "Edited body".to_owned(),
        })
        .unwrap();
    let events = collect_until(&ch.events, 8, |e| {
        matches!(e, BackendEvent::MessageEdited { message_id, new_body, .. }
            if message_id == "fake-1" && new_body == "Edited body")
    });
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::MessageEdited { .. })),
        "edit round-trips"
    );
    ch.commands
        .send(BackendCommand::RevokeMessage {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: "fake-1".to_owned(),
            sender_jid: None,
            from_me: true,
        })
        .unwrap();
    let events = collect_until(
        &ch.events,
        8,
        |e| matches!(e, BackendEvent::MessageRevoked { message_id, .. } if message_id == "fake-1"),
    );
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::MessageRevoked { .. })),
        "revoke round-trips"
    );
}

#[test]
fn group_participant_ops_mutate_the_member_list() {
    let (ch, _log) = linked_backend();
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    let group = backend::FAKE_GROUP_JID.to_owned();
    // Add a new participant, then fetch: the member must be there.
    ch.commands
        .send(BackendCommand::GroupParticipants {
            chat_jid: group.clone(),
            participants: vec!["10000000009@s.whatsapp.net".to_owned()],
            op: backend::ParticipantOp::Add,
        })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| {
        matches!(e, BackendEvent::GroupMembers { members, .. }
            if members.iter().any(|(j, _, _, _)| j == "10000000009@s.whatsapp.net"))
    });
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::GroupMembers { .. })),
        "add emits the updated member list"
    );
    // Promote Mike, then fetch shows the admin flag.
    ch.commands
        .send(BackendCommand::GroupParticipants {
            chat_jid: group.clone(),
            participants: vec!["10000000004@s.whatsapp.net".to_owned()],
            op: backend::ParticipantOp::Promote,
        })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| {
        matches!(e, BackendEvent::GroupMembers { members, .. }
            if members.iter().any(|(j, _, a, _)| j == "10000000004@s.whatsapp.net" && *a))
    });
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::GroupMembers { .. })),
        "promote flips the admin flag"
    );
    // Remove the added participant; a fetch must not list them.
    ch.commands
        .send(BackendCommand::GroupParticipants {
            chat_jid: group.clone(),
            participants: vec!["10000000009@s.whatsapp.net".to_owned()],
            op: backend::ParticipantOp::Remove,
        })
        .unwrap();
    let _ = collect_until(&ch.events, 5, |e| {
        matches!(e, BackendEvent::GroupMembers { members, .. }
            if !members.iter().any(|(j, _, _, _)| j == "10000000009@s.whatsapp.net"))
    });
    ch.commands.send(BackendCommand::FetchGroupMembers { chat_jid: group.clone() }).unwrap();
    let events = collect_until(&ch.events, 8, |e| {
        matches!(e, BackendEvent::GroupMembers { members, .. }
            if !members.iter().any(|(j, _, _, _)| j == "10000000009@s.whatsapp.net")
               && members.iter().any(|(j, _, a, _)| j == "10000000004@s.whatsapp.net" && *a))
    });
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::GroupMembers { .. })),
        "fetch reflects add/promote/remove state"
    );
    // SetGroupInfo renames through state, LeaveGroup archives.
    ch.commands
        .send(BackendCommand::SetGroupInfo {
            chat_jid: group.clone(),
            subject: Some("Renamed Group".to_owned()),
            description: None,
        })
        .unwrap();
    let events = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::ChatUpsert(c) if c.title == "Renamed Group"),
    );
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::ChatUpsert(_))),
        "set info updates the chat row"
    );
    ch.commands.send(BackendCommand::LeaveGroup { chat_jid: group.clone() }).unwrap();
    let events =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::ChatUpsert(c) if c.archived));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::ChatUpsert(_))),
        "leave archives the chat"
    );
}

#[test]
fn mute_and_unpin_chat_actions_round_trip() {
    let (ch, log) = linked_backend();
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    // Mute rides the chat row like archive and pin (and is also
    // in the command log): the pin upsert below must report the
    // chat as muted.
    ch.commands
        .send(BackendCommand::ChatAction {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            action: backend::ChatAction::Mute,
        })
        .unwrap();
    ch.commands
        .send(BackendCommand::ChatAction {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            action: backend::ChatAction::Pin,
        })
        .unwrap();
    let events =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::ChatUpsert(c) if c.pinned));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::ChatUpsert(c) if c.pinned && c.muted)),
        "the pinned upsert carries the mute: {events:?}"
    );
    ch.commands
        .send(BackendCommand::ChatAction {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            action: backend::ChatAction::Unpin,
        })
        .unwrap();
    let events =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::ChatUpsert(c) if !c.pinned));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::ChatUpsert(_))),
        "unpin flips the row back"
    );
    let log = log.lock().unwrap();
    assert!(
        log.iter().any(|l| l.contains("Mute")),
        "mute reached the backend command log: {log:?}"
    );
}

#[test]
fn send_media_commands_emit_media_upserts_for_each_kind() {
    type MediaCase = (MediaKind, &'static str, &'static str, Option<String>, bool, u32);
    use backend::{LiveMessageKind, MediaKind};
    let (ch, log) = linked_backend();
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    let dm = "10000000001@s.whatsapp.net";
    // (kind, mime, caption, file name, gif playback, seconds)
    let cases: [MediaCase; 5] = [
        (MediaKind::Photo, "image/jpeg", "a caption", None, false, 0),
        (MediaKind::Video, "video/mp4", "", None, true, 6),
        (MediaKind::Voice, "audio/ogg; codecs=opus", "", None, false, 11),
        (MediaKind::Document, "application/pdf", "", Some("report.pdf".to_owned()), false, 0),
        (MediaKind::Sticker, "image/webp", "", None, false, 0),
    ];
    for (i, (kind, mime, caption, file_name, gif, seconds)) in cases.into_iter().enumerate() {
        let payload = vec![0xABu8; 100 + i];
        ch.commands
            .send(BackendCommand::SendMedia {
                chat_jid: dm.to_owned(),
                send_id: None,
                kind,
                data: payload.clone(),
                mime: mime.to_owned(),
                caption: caption.to_owned(),
                file_name,
                gif_playback: gif,
                seconds,
                view_once: false,
                ephemeral_secs: 0,
            })
            .unwrap();
        let events = collect_until(&ch.events, 5, |e| {
            matches!(e, BackendEvent::MessageUpsert(m)
                if matches!(&m.kind, LiveMessageKind::Media { media, .. } if *media == kind))
        });
        let upsert = events.iter().find_map(|e| match e {
            BackendEvent::MessageUpsert(m)
                if matches!(&m.kind, LiveMessageKind::Media { media, .. } if *media == kind) =>
            {
                Some(m)
            }
            _ => None,
        });
        let upsert = upsert.unwrap_or_else(|| panic!("no media upsert for {kind:?}"));
        assert!(upsert.from_me, "sent media is from me: {kind:?}");
        let LiveMessageKind::Media {
            caption: got_caption, seconds: got_seconds, size_bytes, ..
        } = &upsert.kind
        else {
            unreachable!()
        };
        assert_eq!(*size_bytes, payload.len() as u64, "size for {kind:?}");
        assert_eq!(*got_seconds, seconds, "duration for {kind:?}");
        if kind == MediaKind::Photo {
            assert_eq!(got_caption, "a caption");
        }
        if kind == MediaKind::Document {
            assert_eq!(got_caption, "report.pdf", "document caption carries the file name");
        }
    }
    let log = log.lock().unwrap();
    for kind in ["Photo", "Video", "Voice", "Document", "Sticker"] {
        assert!(
            log.iter().any(|l| l.starts_with(&format!("send_media:{dm}:{kind}:"))),
            "send_media for {kind} is log-observable: {log:?}"
        );
    }
}

#[test]
fn send_media_with_empty_bytes_is_an_honest_error() {
    use backend::MediaKind;
    let (ch, _log) = linked_backend();
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    ch.commands
        .send(BackendCommand::SendMedia {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            send_id: None,
            kind: MediaKind::Photo,
            data: Vec::new(),
            mime: "image/jpeg".to_owned(),
            caption: String::new(),
            file_name: None,
            gif_playback: false,
            seconds: 0,
            view_once: false,
            ephemeral_secs: 0,
        })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::Error { .. }));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BackendEvent::Error { message } if message.contains("empty"))),
        "empty media is refused with an honest error: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, BackendEvent::MessageUpsert(m)
            if matches!(m.kind, backend::LiveMessageKind::Media { .. }))),
        "no media upsert may follow the refusal"
    );
}

#[test]
fn scripted_incoming_call_can_be_accepted_and_ended() {
    use backend::CallPhase;
    let (ch, log) = linked_backend();
    // The linked script includes an incoming call from the DM.
    let events = collect_until(&ch.events, 40, |e| matches!(e, BackendEvent::IncomingCall { .. }));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BackendEvent::IncomingCall { call_id, chat_jid, video, .. }
            if call_id == "fake-call-1" && chat_jid == "10000000001@s.whatsapp.net" && !video)),
        "the script rings an incoming voice call: {events:?}"
    );
    ch.commands.send(BackendCommand::AcceptCall).unwrap();
    let events = collect_until(&ch.events, 10, |e| {
        matches!(e, BackendEvent::CallChanged { phase: CallPhase::Active, .. })
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BackendEvent::CallChanged { phase: CallPhase::Connecting, .. })),
        "accepting connects first: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            BackendEvent::CallChanged { phase: CallPhase::Active, seconds: 0, .. }
        )),
        "then the call goes active: {events:?}"
    );
    // Duration ticks while active.
    let events = collect_until(
        &ch.events,
        10,
        |e| matches!(e, BackendEvent::CallChanged { phase: CallPhase::Active, seconds, .. } if *seconds >= 1),
    );
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::CallChanged { phase: CallPhase::Active, seconds, .. } if *seconds >= 1)),
        "the duration ticks: {events:?}"
    );
    ch.commands.send(BackendCommand::SetCallMuted { muted: true }).unwrap();
    ch.commands.send(BackendCommand::SetCallSpeaker { speaker: true }).unwrap();
    ch.commands.send(BackendCommand::EndCall).unwrap();
    let events = collect_until(&ch.events, 10, |e| {
        matches!(e, BackendEvent::CallChanged { phase: CallPhase::Ended, .. })
    });
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::CallChanged { phase: CallPhase::Ended, reason: Some(r), .. } if r == "Call ended")),
        "ending reports the end: {events:?}"
    );
    // The call-log entry is emitted right after the Ended event.
    let events = collect_until(&ch.events, 10, |e| {
        matches!(e, BackendEvent::MessageUpsert(m)
            if matches!(m.kind, backend::LiveMessageKind::Other(_)) && m.body.starts_with("Voice call"))
    });
    assert!(
        events.iter().any(
            |e| matches!(e, BackendEvent::MessageUpsert(m) if m.body.starts_with("Voice call"))
        ),
        "a call-log entry lands in the chat: {events:?}"
    );
    let log = log.lock().unwrap();
    for needle in ["accept_call", "call_muted:true", "call_speaker:true", "end_call"] {
        assert!(log.iter().any(|l| l == needle), "log has {needle}: {log:?}");
    }
}

#[test]
fn outgoing_call_rings_connects_and_ends_with_a_log_entry() {
    use backend::CallPhase;
    let (ch, log) = linked_backend();
    // The scripted incoming call occupies the line; decline it so
    // the outgoing call can start.
    let _ = collect_until(&ch.events, 40, |e| matches!(e, BackendEvent::IncomingCall { .. }));
    ch.commands.send(BackendCommand::RejectCall).unwrap();
    let _ = collect_until(&ch.events, 10, |e| {
        matches!(e, BackendEvent::CallChanged { phase: CallPhase::Ended, .. })
    });
    ch.commands
        .send(BackendCommand::StartCall {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            video: false,
        })
        .unwrap();
    let events = collect_until(&ch.events, 10, |e| {
        matches!(e, BackendEvent::CallChanged { phase: CallPhase::Active, .. })
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BackendEvent::CallChanged { phase: CallPhase::Ringing, .. })),
        "outgoing calls ring first: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BackendEvent::CallChanged { phase: CallPhase::Active, .. })),
        "then connect: {events:?}"
    );
    // A second call while one is live is an honest error.
    ch.commands
        .send(BackendCommand::StartCall {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            video: false,
        })
        .unwrap();
    let events = collect_until(&ch.events, 10, |e| matches!(e, BackendEvent::Error { .. }));
    assert!(
        events.iter().any(
            |e| matches!(e, BackendEvent::Error { message } if message.contains("already in progress"))
        ),
        "double calls are refused: {events:?}"
    );
    ch.commands.send(BackendCommand::EndCall).unwrap();
    let events = collect_until(&ch.events, 10, |e| {
        matches!(e, BackendEvent::MessageUpsert(m)
            if matches!(m.kind, backend::LiveMessageKind::Other(_)))
    });
    assert!(
        events.iter().any(
            |e| matches!(e, BackendEvent::MessageUpsert(m) if m.body.starts_with("Voice call"))
        ),
        "the outgoing call leaves a log entry: {events:?}"
    );
    let log = log.lock().unwrap();
    assert!(
        log.iter().any(|l| l == "start_call:10000000001@s.whatsapp.net:false"),
        "start is log-observable: {log:?}"
    );
    assert!(log.iter().any(|l| l == "reject_call"), "reject is log-observable: {log:?}");
}

#[test]
fn call_commands_without_a_call_are_honest_errors() {
    let (ch, _log) = linked_backend();
    let _ = collect_until(&ch.events, 40, |e| matches!(e, BackendEvent::IncomingCall { .. }));
    ch.commands.send(BackendCommand::RejectCall).unwrap();
    let _ = collect_until(&ch.events, 10, |e| {
        matches!(e, BackendEvent::CallChanged { phase: backend::CallPhase::Ended, .. })
    });
    ch.commands.send(BackendCommand::AcceptCall).unwrap();
    ch.commands.send(BackendCommand::EndCall).unwrap();
    let mut seen_errors = 0;
    let events = collect_until(&ch.events, 10, |e| {
        if matches!(e, BackendEvent::Error { .. }) {
            seen_errors += 1;
        }
        seen_errors >= 2
    });
    let errors: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::Error { message } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        errors.iter().any(|m| m.contains("no incoming call")),
        "accept without a call errors honestly: {errors:?}"
    );
    assert!(
        errors.iter().any(|m| m.contains("No call is in progress")),
        "end without a call errors honestly: {errors:?}"
    );
}

#[test]
fn block_contact_is_logged_by_the_fake() {
    let (ch, log) = linked_backend();
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    ch.commands
        .send(BackendCommand::BlockContact {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            block: true,
        })
        .unwrap();
    ch.commands
        .send(BackendCommand::FetchGroupMembers { chat_jid: backend::FAKE_GROUP_JID.to_owned() })
        .unwrap();
    let _ = collect_until(&ch.events, 8, |e| matches!(e, BackendEvent::GroupMembers { .. }));
    let log = log.lock().unwrap();
    assert!(
        log.iter().any(|l| l == "block:10000000001@s.whatsapp.net:true"),
        "block is log-observable: {log:?}"
    );
}

#[test]
fn blocklist_is_emitted_at_connect_and_after_block_toggles() {
    let (ch, _log) = linked_backend();
    // The connect script reports the baseline list (empty),
    // like the real backend's connect-time fetch.
    let events = collect_until(&ch.events, 10, |e| matches!(e, BackendEvent::Blocklist(_)));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::Blocklist(jids) if jids.is_empty())),
        "baseline blocklist is emitted: {events:?}"
    );
    ch.commands
        .send(BackendCommand::BlockContact {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            block: true,
        })
        .unwrap();
    let events = collect_until(&ch.events, 10, |e| matches!(e, BackendEvent::Blocklist(_)));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::Blocklist(jids) if *jids == vec!["10000000001@s.whatsapp.net".to_owned()])),
        "block re-emits the list: {events:?}"
    );
    ch.commands
        .send(BackendCommand::BlockContact {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            block: false,
        })
        .unwrap();
    let events = collect_until(&ch.events, 10, |e| matches!(e, BackendEvent::Blocklist(_)));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::Blocklist(jids) if jids.is_empty())),
        "unblock re-emits the list: {events:?}"
    );
}

/// Disconnect is the log out command: the backend answers it
/// with a LoggedOut event (the app ends the session on it). The
/// real backend additionally deregisters the device with the
/// server and wipes the local sign-in store when its run ends;
/// those live behind the whatsapp feature and are covered by
/// the session tests there.
#[test]
fn disconnect_answers_with_logged_out() {
    let (ch, _log) = linked_backend();
    // Drain the linked script first.
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    ch.commands.send(BackendCommand::Disconnect).unwrap();
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::LoggedOut));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::LoggedOut)),
        "disconnect is answered by LoggedOut: {events:?}"
    );
    ch.commands.send(BackendCommand::Shutdown).unwrap();
}

#[test]
fn delete_for_me_command_removes_the_message_and_logs() {
    let (ch, log) = linked_backend();
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    ch.commands
        .send(BackendCommand::DeleteMessageForMe {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: "fake-1".to_owned(),
            sender_jid: None,
            from_me: false,
            delete_media: true,
            message_timestamp: Some(1_700_000_000),
        })
        .unwrap();
    let events =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::MessageDeletedForMe { .. }));
    assert!(
        events.iter().any(|e| matches!(
            e,
            BackendEvent::MessageDeletedForMe { chat_jid, message_id }
                if chat_jid == "10000000001@s.whatsapp.net" && message_id == "fake-1"
        )),
        "the delete-for-me command reports the removal"
    );
    let log = log.lock().unwrap();
    assert!(log.iter().any(|l| l.starts_with("delete_for_me:")), "log: {log:?}");
}

#[test]
fn clear_and_delete_chat_commands_apply_and_log() {
    let (ch, log) = linked_backend();
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    // Star one message first: a clear that keeps starred
    // messages must leave it in place.
    ch.commands
        .send(BackendCommand::StarMessage {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: "fake-1".to_owned(),
            sender_jid: None,
            from_me: false,
            star: true,
        })
        .unwrap();
    let _ = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-1" && m.starred),
    );
    ch.commands
        .send(BackendCommand::ClearChat {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            keep_starred: true,
        })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::ChatCleared { .. }));
    assert!(
        events.iter().any(|e| matches!(
            e,
            BackendEvent::ChatCleared { delete_starred, .. } if !delete_starred
        )),
        "the clear reports that starred messages were kept"
    );
    // The starred message survived: keeping it (a command that
    // requires the message to exist) still answers.
    ch.commands
        .send(BackendCommand::KeepMessage {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: "fake-1".to_owned(),
            sender_jid: None,
            from_me: false,
            keep: true,
        })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::MessageKept { .. }));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::MessageKept { kept, .. } if *kept)),
        "the kept report lands for the surviving starred message"
    );
    ch.commands
        .send(BackendCommand::DeleteChat { chat_jid: "10000000001@s.whatsapp.net".to_owned() })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::ChatDeleted { .. }));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::ChatDeleted { .. })),
        "the delete reports the chat removal"
    );
    let log = log.lock().unwrap();
    assert!(log.iter().any(|l| l.starts_with("clear_chat:")), "log: {log:?}");
    assert!(log.iter().any(|l| l.starts_with("delete_chat:")), "log: {log:?}");
}

#[test]
fn mark_unread_mute_until_contact_and_spam_commands() {
    let (ch, log) = linked_backend();
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    ch.commands
        .send(BackendCommand::MarkChatUnread { chat_jid: "10000000002@g.us".to_owned() })
        .unwrap();
    let events =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::ChatUpsert(c) if c.unread >= 1));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::ChatUpsert(c) if c.unread >= 1)),
        "marking unread raises the badge in the upsert"
    );
    ch.commands
        .send(BackendCommand::ChatAction {
            chat_jid: "10000000002@g.us".to_owned(),
            action: backend::ChatAction::MuteUntil(1_700_028_800_000),
        })
        .unwrap();
    let events = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::ChatUpsert(c) if c.mute_until == 1_700_028_800),
    );
    assert!(
        events.iter().any(
            |e| matches!(e, BackendEvent::ChatUpsert(c) if c.muted && c.mute_until == 1_700_028_800)
        ),
        "the timed mute upsert carries its end in seconds"
    );
    ch.commands
        .send(BackendCommand::SendContact {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            send_id: None,
            name: "Emma Wilson".to_owned(),
            phone: "10000000007".to_owned(),
            ephemeral_secs: 0,
        })
        .unwrap();
    let events = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if matches!(m.kind, backend::LiveMessageKind::Contact { .. })),
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            BackendEvent::MessageUpsert(m)
                if matches!(&m.kind, backend::LiveMessageKind::Contact { name, phone }
                    if name == "Emma Wilson" && phone == "10000000007")
        )),
        "the sent contact card upserts as a contact message"
    );
    ch.commands
        .send(BackendCommand::ReportSpam {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: "fake-1".to_owned(),
            sender_jid: None,
            message_timestamp: 1_700_000_000,
        })
        .unwrap();
    let events = collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::SpamReported { .. }));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::SpamReported { .. })),
        "the report answers with its reported event"
    );
    ch.commands
        .send(BackendCommand::ReportSpam {
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            message_id: "no-such-message".to_owned(),
            sender_jid: None,
            message_timestamp: 1_700_000_000,
        })
        .unwrap();
    let events =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::SpamReportFailed { .. }));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::SpamReportFailed { .. })),
        "an unknown message fails the report honestly"
    );
    let log = log.lock().unwrap();
    assert!(log.iter().any(|l| l.starts_with("mark_unread:")), "log: {log:?}");
    assert!(log.iter().any(|l| l.starts_with("send_contact:")), "log: {log:?}");
    assert!(log.iter().any(|l| l.starts_with("report_spam:")), "log: {log:?}");
}

/// MarkChatRead is MarkChatUnread's pair: the fake zeroes the
/// badge in its answering upsert and logs the command the same
/// way, so the app's read sync is provable end to end without a
/// phone.
#[test]
fn mark_chat_read_zeroes_the_badge() {
    let (ch, log) = linked_backend();
    let _ = collect_until(
        &ch.events,
        3,
        |e| matches!(e, BackendEvent::MessageUpsert(m) if m.id == "fake-3"),
    );
    // Raise the badge first so the zeroing is observable.
    ch.commands
        .send(BackendCommand::MarkChatUnread { chat_jid: "10000000002@g.us".to_owned() })
        .unwrap();
    let events =
        collect_until(&ch.events, 5, |e| matches!(e, BackendEvent::ChatUpsert(c) if c.unread >= 1));
    assert!(
        events.iter().any(|e| matches!(e, BackendEvent::ChatUpsert(c) if c.unread >= 1)),
        "marking unread raises the badge in the upsert"
    );
    ch.commands
        .send(BackendCommand::MarkChatRead { chat_jid: "10000000002@g.us".to_owned() })
        .unwrap();
    let events = collect_until(
        &ch.events,
        5,
        |e| matches!(e, BackendEvent::ChatUpsert(c) if c.jid == "10000000002@g.us" && c.unread == 0),
    );
    assert!(
        events.iter().any(
            |e| matches!(e, BackendEvent::ChatUpsert(c) if c.jid == "10000000002@g.us" && c.unread == 0)
        ),
        "marking read zeroes the badge in the upsert"
    );
    let log = log.lock().unwrap();
    assert!(log.iter().any(|l| l.starts_with("mark_read:")), "log: {log:?}");
}

/// Parity P10 (rows 131 and 132): the label and quick reply
/// commands are answered with the typed echo events the app
/// settles its optimistic state on, in the fake exactly as
/// the live backend's server echoes settle it.
#[test]
fn label_and_quick_reply_commands_echo_through_the_fake() {
    let ch = spawn(FakeBackend::auto());
    ch.commands.send(BackendCommand::ConnectQr).unwrap();
    let _ = collect_until(&ch.events, 10, |e| matches!(e, BackendEvent::Connected { .. }));

    ch.commands
        .send(BackendCommand::SaveLabel {
            label_id: "l1".to_owned(),
            name: "Urgent".to_owned(),
            color: 3,
        })
        .unwrap();
    let ev = collect_until(&ch.events, 10, |e| matches!(e, BackendEvent::LabelEdited { .. }));
    assert!(
        ev.iter().any(|e| matches!(e, BackendEvent::LabelEdited { label_id, name: Some(n), color: Some(3), deleted: false, .. } if label_id == "l1" && n == "Urgent")),
        "events: {ev:?}"
    );

    ch.commands
        .send(BackendCommand::SetChatLabel {
            label_id: "l1".to_owned(),
            chat_jid: "10000000001@s.whatsapp.net".to_owned(),
            labeled: true,
        })
        .unwrap();
    let ev = collect_until(&ch.events, 10, |e| matches!(e, BackendEvent::ChatLabelChanged { .. }));
    assert!(
        ev.iter().any(|e| matches!(e, BackendEvent::ChatLabelChanged { label_id, labeled: true, .. } if label_id == "l1")),
        "events: {ev:?}"
    );

    ch.commands
        .send(BackendCommand::SaveQuickReply {
            id: "q1".to_owned(),
            shortcut: "hours".to_owned(),
            message: "We open 8 to 21.".to_owned(),
            keywords: vec!["open".to_owned()],
            count: 0,
        })
        .unwrap();
    let ev = collect_until(&ch.events, 10, |e| matches!(e, BackendEvent::QuickReplyEdited { .. }));
    assert!(
        ev.iter().any(|e| matches!(e, BackendEvent::QuickReplyEdited { id, shortcut, deleted: false, .. } if id == "q1" && shortcut == "hours")),
        "events: {ev:?}"
    );

    ch.commands.send(BackendCommand::DeleteQuickReply { id: "q1".to_owned() }).unwrap();
    let ev = collect_until(&ch.events, 10, |e| matches!(e, BackendEvent::QuickReplyDeleted { .. }));
    assert!(
        ev.iter().any(|e| matches!(e, BackendEvent::QuickReplyDeleted { id } if id == "q1")),
        "events: {ev:?}"
    );

    ch.commands.send(BackendCommand::Shutdown).unwrap();
}
