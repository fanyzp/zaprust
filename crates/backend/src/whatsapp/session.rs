//! The backend session loop: connects the library `Bot`, bridges
//! UI commands from the std channel into library calls, and routes
//! library events through `super::translate` into `BackendEvent`s.

use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use whatsapp_rust::pair_code::PairCodeOptions;
use whatsapp_rust::prelude::*;
use whatsapp_rust::store::{SqliteDatabase, SqliteDatabaseConfig};
use whatsapp_rust::wacore::types::events::Event;

use crate::{
    BackendCommand, BackendConfig, BackendEvent, ChatAction, EventSink, LiveChat, LiveChatKind,
    ParticipantOp,
};

#[cfg(feature = "calls")]
use super::calls::{
    accept_incoming_call, handle_call_event, offer_call_id, start_outgoing_call, CallActive,
    CallIncoming,
};
use super::send::{
    outgoing_location_message, outgoing_text_message, post_text_status, send_media,
    stamp_ephemeral, OutgoingMedia,
};
use super::translate::{
    participant_name, privacy_map, translate_event, translate_message, translate_special,
};
use super::util::{
    abort_latest, build_message_target, cap_poll_secrets, download_proto, fetch_photo_by_ref,
    fetch_photo_bytes, fetch_video_by_ref, fetch_video_bytes, fetch_video_file_by_ref,
    fetch_video_to_file, fetch_voice_by_ref, fetch_voice_bytes, lock_cache, parse_address,
    poll_vote_payload, proto_is_view_once, received_poll_secret, selected_indices, spawn_latest,
    unix_now, LatestTask, PollSecrets, ProtoCache,
};

/// How many session events may wait for the event callbacks.
/// Delivery is ordered (see the builder call in `run_async`), and a
/// reconnect after time offline can release thousands of queued
/// events at once; the capacity is sized so that burst fits in
/// practice. Ordered delivery stays lossy at overflow by the
/// library's documented contract (a full mailbox drops the newest
/// event), so this is headroom, not a guarantee.
const EVENT_QUEUE_CAPACITY: usize = 4096;

/// Total budget for one media send (upload plus send) at the
/// dispatch loop's call site. The library's media HTTP client
/// bounds only the connect (10 seconds), not the transfer, so a
/// connected but silent CDN could otherwise hold the serial
/// dispatch loop for as long as the OS keeps the connection.
/// The upload precedes the send, so expiring here fails the send
/// truthfully in the common case: nothing was transmitted. Ten
/// minutes sits far beyond a healthy upload of anything the app
/// sends, and a send that hits it reports failure instead of
/// wedging every later command.
const SEND_MEDIA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Budget for the graceful shutdown flush. The flush writes the
/// device snapshot, buffered receipts and message secrets over
/// the network; on a dead connection it must not hold app exit
/// hostage, so past the budget the session ends anyway and the
/// runtime drop aborts the bot.
const SHUTDOWN_FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long an on-demand history request may wait for the
/// phone's answer before the backend closes it as failed. The
/// library's fetch is fire-and-forget with no timeout of its
/// own: a phone that is off would otherwise leave the app's
/// loading state latched forever.
const HISTORY_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Messages requested per older-history page: the service's
/// own default page size (its AB-prop default), so a page is
/// what the phone would send a first-party client.
const HISTORY_PAGE_SIZE: i32 = 50;

/// On-demand history requests in flight: request ID to chat
/// JID. The command handler records an entry BEFORE the
/// request goes out (the library's own docs warn the answer
/// can arrive before the send completes); the event handler
/// removes the entry when the answering history sync lands
/// and reports the page; the watchdog removes it when the
/// phone never answers.
type PendingHistory = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>;

/// What one landed on-demand history page means for its chat:
/// how many messages it carried for the chat, and whether the
/// phone's history for the chat is exhausted. Exhaustion is
/// the payload's own completion marker, or an empty page: the
/// library codifies no empty-page rule, but an on-demand
/// answer carrying nothing for the chat means the phone had
/// nothing older to give, and reading it as "maybe more"
/// would re-request the same anchor forever.
fn history_page_outcome(
    sync: &whatsapp_rust::prelude::wa::HistorySync,
    chat_jid: &str,
) -> (u32, bool) {
    use whatsapp_rust::prelude::wa::conversation::EndOfHistoryTransferType;
    let Some(conv) = sync.conversations.iter().find(|c| c.id == chat_jid) else {
        return (0, true);
    };
    let added = u32::try_from(conv.messages.len()).unwrap_or(0);
    let marked_complete = matches!(
        conv.end_of_history_transfer_type,
        Some(EndOfHistoryTransferType::CompleteAndNoMoreMessageRemainOnPrimary)
    );
    (added, marked_complete || added == 0)
}

/// (jid, display name) rows for a membership-request list. The
/// protocol carries no name for a requester, so the display name
/// is empty: the app resolves a label from its own data or masks
/// the identifier. The raw phone number or LID must never be
/// substituted as the name (privacy finding D5).
fn request_rows(list: &[whatsapp_rust::MembershipRequest]) -> Vec<(String, String)> {
    list.iter().map(|r| (r.jid.to_string(), String::new())).collect()
}

/// What the user should be told about a logout's deregistration
/// outcome, if anything. The library always performs its local
/// teardown, so the local session ends either way; the question
/// is only whether the server confirmed the deregistration. An
/// attempt the server did not confirm, and one that could not be
/// made because the socket was down, both leave this device
/// listed as linked on the phone, and reporting neither would
/// let the user believe the logout fully landed. A logout that
/// was never attempted because there is no phone-number identity
/// has nothing registered to remove, so it stays silent.
fn logout_problem(outcome: &whatsapp_rust::DeregistrationOutcome) -> Option<String> {
    use whatsapp_rust::{DeregistrationOutcome, DeregistrationSkipReason};
    match outcome {
        DeregistrationOutcome::Failed { source } => Some(format!(
            "Logging out did not reach the server: {source}. \
             This device may still appear linked on your phone."
        )),
        DeregistrationOutcome::NotAttempted(DeregistrationSkipReason::Offline) => Some(
            "Logging out could not reach the server while offline. \
             This device may still appear linked on your phone."
                .to_owned(),
        ),
        _ => None,
    }
}

/// How awaiting one command handler ended.
enum HandlerEnd<O> {
    /// The handler ran to completion.
    Finished,
    /// Shutdown arrived (the Shutdown command, or every command
    /// sender went away): the handler was dropped mid-flight.
    Shutdown,
    /// The bot run loop ended on its own: the handler was
    /// dropped mid-flight, and this is the run's outcome.
    RunEnded(O),
}

/// Awaits one command handler without letting it hold the
/// session hostage. While the handler runs, the command channel
/// and the bot handle stay watched: commands that arrive are
/// queued into `pending`, in arrival order, for the dispatch
/// loop to handle next, but Shutdown (or the channel closing,
/// which means the same thing) and the bot run loop ending
/// preempt the handler, dropping its future wherever it is
/// suspended. A dropped library call fails harmlessly: its
/// response waiter is dropped with it, and the events it would
/// have emitted simply never arrive.
async fn await_handler<O, R>(
    handler: impl std::future::Future<Output = ()>,
    cmd_rx: &mut tokio::sync::mpsc::Receiver<BackendCommand>,
    pending: &mut VecDeque<BackendCommand>,
    run: &mut R,
) -> HandlerEnd<O>
where
    R: std::future::Future<Output = O> + Unpin,
{
    tokio::pin!(handler);
    loop {
        tokio::select! {
            biased;
            () = &mut handler => return HandlerEnd::Finished,
            next = cmd_rx.recv() => match next {
                Some(BackendCommand::Shutdown) | None => return HandlerEnd::Shutdown,
                Some(next) => pending.push_back(next),
            },
            outcome = &mut *run => return HandlerEnd::RunEnded(outcome),
        }
    }
}

/// Heals chat titles a history payload could not resolve from
/// its own identity data, using the client's stores: a group's
/// subject comes from its metadata (whose participant list also
/// names senders whose stored labels are still raw), and a direct
/// chat's phone number comes from the library's learned
/// LID-to-phone cache. Each chat is attempted once per run
/// (`attempts`), and every correction crosses as a rename event
/// whose store guard makes it monotonic: a resolved name is never
/// downgraded. Failures are silent by design: healing is a
/// best-effort improvement pass over a roster that already works,
/// and a title that stays raw is retried on the next run or when
/// the chat's member list is fetched.
fn heal_unresolved_titles(
    client: std::sync::Arc<whatsapp_rust::Client>,
    sink: crate::EventSink,
    unresolved: Vec<(String, crate::LiveChatKind)>,
    attempts: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
) {
    let fresh: Vec<(String, crate::LiveChatKind)> = {
        let mut guard = attempts.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        unresolved.into_iter().filter(|(jid, _)| guard.insert(jid.clone())).collect()
    };
    if fresh.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for (jid, kind) in fresh {
            heal_one(&client, &sink, &jid, kind).await;
        }
    });
}

/// Heals one chat's title from the client's stores, returning
/// true when the chat is settled (a rename was emitted, or the
/// lookup succeeded and had nothing better to offer) and false
/// when the lookup itself failed and a retry could succeed.
async fn heal_one(
    client: &whatsapp_rust::Client,
    sink: &crate::EventSink,
    jid: &str,
    kind: crate::LiveChatKind,
) -> bool {
    match kind {
        crate::LiveChatKind::Group => {
            let Ok(parsed) = jid.parse::<Jid>() else { return true };
            let Ok(meta) = client.groups().fetch_metadata(&parsed).await else { return false };
            if let Some(subject) = meta.subject.clone().filter(|s| !s.trim().is_empty()) {
                sink.emit(BackendEvent::ChatRenamed { chat_jid: jid.to_owned(), title: subject });
            }
            for participant in &meta.participants {
                let name = participant_name(participant);
                if !name.is_empty() {
                    sink.emit(BackendEvent::SenderNamed {
                        sender_jid: participant.jid.to_string(),
                        name,
                    });
                }
            }
            true
        }
        crate::LiveChatKind::Dm => {
            if jid.ends_with("@s.whatsapp.net") {
                // The title is the phone number itself; the
                // formatted form is the improvement the
                // precedence order asks for, and it needs no
                // lookup.
                sink.emit(BackendEvent::ChatRenamed {
                    chat_jid: jid.to_owned(),
                    title: format!("+{}", super::translate::user_part(jid)),
                });
                true
            } else if let Ok(parsed) = jid.parse::<Jid>() {
                match client.get_lid_pn_entry(&parsed).await {
                    Ok(Some(entry)) => {
                        let phone = format!("+{}", entry.phone_number);
                        sink.emit(BackendEvent::ChatRenamed {
                            chat_jid: jid.to_owned(),
                            title: phone.clone(),
                        });
                        sink.emit(BackendEvent::SenderNamed {
                            sender_jid: jid.to_owned(),
                            name: phone,
                        });
                        true
                    }
                    Ok(None) => true,
                    Err(_) => false,
                }
            } else {
                true
            }
        }
        _ => true,
    }
}

/// Heals titles the app explicitly reported as raw (the
/// `ResolveTitles` command, sent when a session connects with
/// every chat whose stored title is still a raw identifier).
/// Unlike the history-driven pass this one retries: a metadata
/// fetch that fails during the pairing sync storm is exactly
/// the failure this command exists to outlast, so each chat
/// gets up to three attempts with growing pauses. The kind is
/// derived from the JID form, the only thing the app needs to
/// send.
fn heal_titles_with_retry(
    client: std::sync::Arc<whatsapp_rust::Client>,
    sink: crate::EventSink,
    jids: Vec<String>,
) {
    if jids.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for jid in jids {
            let kind = if jid.ends_with("@g.us") {
                crate::LiveChatKind::Group
            } else {
                crate::LiveChatKind::Dm
            };
            for attempt in 0..3 {
                if attempt > 0 {
                    let pause = if attempt == 1 { 3 } else { 8 };
                    tokio::time::sleep(std::time::Duration::from_secs(pause)).await;
                }
                if heal_one(&client, &sink, &jid, kind).await {
                    break;
                }
            }
        }
    });
}

/// One media-retry recovery for a media fetch or download
/// that already failed: asks the message author's device to
/// re-upload the media (the protocol's media-retry flow),
/// reports the fresh CDN path so the app can persist it with
/// the archived reference, and returns it for the caller's
/// second attempt. `None` when the recovery is unavailable
/// (the message's addressing is unknown this session, or the
/// flow did not answer with a fresh path): the original
/// failure stands either way.
async fn media_retry_path(
    client: &whatsapp_rust::Client,
    events: &crate::EventSink,
    targets: &super::util::RetryTargets,
    chat_jid: &str,
    message_id: &str,
    fields: &super::util::MediaRefFields,
) -> Option<String> {
    let mark = lock_cache(targets).0.get(&(chat_jid.to_owned(), message_id.to_owned())).cloned()?;
    let direct_path =
        super::util::request_media_reupload(client, chat_jid, message_id, &mark, &fields.media_key)
            .await?;
    events.emit(BackendEvent::MediaRefreshed {
        chat_jid: chat_jid.to_owned(),
        message_id: message_id.to_owned(),
        direct_path: direct_path.clone(),
    });
    Some(direct_path)
}

/// The in-memory second attempt after [`media_retry_path`].
async fn retry_fetch_bytes(
    client: &whatsapp_rust::Client,
    events: &crate::EventSink,
    targets: &super::util::RetryTargets,
    chat_jid: &str,
    message_id: &str,
    fields: &super::util::MediaRefFields,
) -> Option<Vec<u8>> {
    let direct_path =
        media_retry_path(client, events, targets, chat_jid, message_id, fields).await?;
    super::util::fetch_media_by_fields(client, &fields.with_direct_path(direct_path)).await.ok()
}

/// The to-file second attempt after [`media_retry_path`].
#[allow(clippy::too_many_arguments)]
async fn retry_download_file(
    client: &whatsapp_rust::Client,
    events: &crate::EventSink,
    targets: &super::util::RetryTargets,
    chat_jid: &str,
    message_id: &str,
    fields: &super::util::MediaRefFields,
    dir: &std::path::Path,
    stem: &str,
    ext: &str,
) -> Option<std::path::PathBuf> {
    let direct_path =
        media_retry_path(client, events, targets, chat_jid, message_id, fields).await?;
    super::util::download_fields_to_file(
        client,
        &fields.with_direct_path(direct_path),
        dir,
        stem,
        ext,
    )
    .await
    .ok()
}

pub(super) async fn run_async(
    config: &BackendConfig,
    commands: Receiver<BackendCommand>,
    events: EventSink,
    logged_out: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), String> {
    // Startup failures return Err instead of emitting an Error event
    // and returning quietly: a quiet return looks like a clean exit
    // to the supervisor, which then never restarts and never closes
    // the command channel, leaving live mode silently dead. An Err
    // is treated like a crash (see `Backend::run`), so the failure
    // gets the restart, and its text reaches the user as the Crashed
    // summary.
    if let Err(e) = std::fs::create_dir_all(config.account_dir()) {
        return Err(format!("Could not create the account data folder: {e}"));
    }
    let db_path = config.session_db_path().to_string_lossy().into_owned();
    // This client pairs once and stays connected for weeks, which is
    // the long-lived profile the library documents for its SQLite
    // store: a warm page cache for a database that grows far beyond
    // what the defaults assume, two reader connections so lookups
    // never queue behind a write, and memory-mapped reads. The store
    // defaults are tuned for many small per-session stores instead.
    // Device 1 is the account this database has always held.
    // The session store is encrypted at rest (SEC-F01): the
    // library's connection init hook exists for exactly this, it
    // runs `PRAGMA key` before any other statement on every pooled
    // connection, and the schema read proves the key before the
    // store's own setup proceeds. The tree links a SQLCipher build
    // of SQLite, so the pragma actually encrypts; the plaintext
    // database of an older build is converted by the app before
    // this runs (see `store::encrypt_file_in_place`).
    // The same hook switches on secure_delete (privacy audit
    // A2), so protocol state the library deletes (consumed
    // prekeys, replaced sessions) is zeroed as its pages are
    // freed instead of lingering in the file. That is as far as
    // this repo's control reaches: the library owns the pooled
    // connections' lifecycle, so no VACUUM or checkpoint can be
    // driven from here. The proportionate measure is the pragma:
    // this store holds device keys and protocol state only,
    // never message bodies (those live in the archive, whose
    // store does reclaim; see the store crate).
    let key_hex = {
        use std::fmt::Write;
        let mut hex = String::with_capacity(64);
        for b in config.db_key {
            let _ = write!(hex, "{b:02x}");
        }
        hex
    };
    let store_config =
        SqliteDatabaseConfig { cache_size_kib: 16 * 1024, read_pool_size: 2, ..Default::default() }
            .with_mmap_size(256 * 1024 * 1024)
            .with_connection_init(move |conn| {
                use diesel::RunQueryDsl;
                diesel::sql_query(format!("PRAGMA key = \"x'{key_hex}'\"")).execute(conn)?;
                diesel::sql_query("PRAGMA secure_delete = ON").execute(conn)?;
                diesel::sql_query("SELECT count(*) FROM sqlite_master").execute(conn)?;
                Ok(())
            });
    let store = match SqliteDatabase::open(&db_path, store_config).await {
        Ok(db) => db.store(1),
        Err(e) => {
            return Err(format!("Could not open the session store: {e}"));
        }
    };

    // Original protocol payloads for messages seen this session.
    // Forwarding and media download need the source proto (CDN
    // references, encryption keys), which the archive deliberately
    // does not store; both operations are therefore session-scoped
    // and say so when a message is not in the cache.
    let proto_cache: ProtoCache = std::sync::Arc::new(std::sync::Mutex::new((
        std::collections::HashMap::new(),
        std::collections::VecDeque::new(),
    )));
    let cache_for_messages = proto_cache.clone();
    // Media-retry addressing for every message this session
    // translates (live and history): when a media download
    // fails, the retry flow asks the author's device to
    // re-upload, and it needs the author's identity, which the
    // archive does not store.
    let retry_targets: super::util::RetryTargets = std::sync::Arc::new(std::sync::Mutex::new((
        std::collections::HashMap::new(),
        std::collections::VecDeque::new(),
    )));
    let retry_for_messages = retry_targets.clone();
    let retry_for_events = retry_targets.clone();
    let retry_for_commands = retry_targets.clone();
    let poll_secrets: PollSecrets =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let secrets_for_messages = poll_secrets.clone();
    // Call state: the ringing incoming call (kept as its event so
    // accept can borrow the offer) and the live call, if any.
    #[cfg(feature = "calls")]
    let call_incoming: CallIncoming = std::sync::Arc::new(std::sync::Mutex::new(None));
    #[cfg(feature = "calls")]
    let call_incoming_events = call_incoming.clone();
    #[cfg(feature = "calls")]
    let call_active: CallActive = std::sync::Arc::new(std::sync::Mutex::new(None));
    // The privacy-settings fetch spawned by `on_connected`. The slot
    // keeps only the newest fetch: a reconnect aborts the fetch it
    // replaces, so a slow fetch from an earlier connect can never
    // land after a newer one and overwrite it with stale state.
    let privacy_fetch: LatestTask = std::sync::Arc::new(std::sync::Mutex::new(None));
    let privacy_fetch_conn = privacy_fetch.clone();
    // The blocklist fetch spawned by `on_connected`, same slot
    // discipline as the privacy fetch: only the newest fetch may
    // land, so a reconnect never adopts a stale blocklist.
    let blocklist_fetch: LatestTask = std::sync::Arc::new(std::sync::Mutex::new(None));
    let blocklist_fetch_conn = blocklist_fetch.clone();
    // The own-profile fetch, spawned by `on_connected` and by the
    // FetchOwnProfile command, same slot discipline as the privacy
    // fetch: only the newest read-back may land, so the editor
    // never adopts a profile older than the one it just saved.
    let own_profile_fetch: LatestTask = std::sync::Arc::new(std::sync::Mutex::new(None));
    let own_profile_fetch_conn = own_profile_fetch.clone();

    let sink = events.clone();
    let sink_pair = events.clone();
    let sink_conn = events.clone();
    let sink_out = events.clone();
    let sink_msg = events.clone();
    let sink_all = events.clone();
    // Chat titles already sent to the identity healer this run,
    // so repeated history chunks do not refetch the same group
    // metadata or LID mapping.
    let heal_attempts: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let heal_attempts_events = heal_attempts.clone();
    let heal_attempts_msg = heal_attempts.clone();
    // On-demand history requests in flight (see PendingHistory).
    let pending_history: PendingHistory =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let pending_history_events = pending_history.clone();
    let pending_history_commands = pending_history.clone();
    // The logout flag the caller wipes the account directory on:
    // set by the library's logged-out callback (this device was
    // unlinked, here or from the phone) and by the Disconnect
    // handler below, whose own logout is what ends the run.
    let logged_out_flag = logged_out.clone();

    let bot = match Bot::builder()
        .with_backend(store)
        // The linked-device identity the phone shows in its
        // Linked devices list. Without this override the library
        // presents its own defaults (os "rust", platform Unknown)
        // and the account owner sees an unknown device; the
        // documented mechanism is DevicePropsOverride (library
        // bot.rs, `with_device_props`). Applied at pairing time
        // only, which is the library's contract.
        .with_device_props(
            whatsapp_rust::wacore::store::DevicePropsOverride::new().with_os("ZapRust".to_owned()),
        )
        // Arrival order matters here: the callbacks below upsert
        // messages and apply their receipts, and a receipt that
        // overtakes its upsert lands on a message that does not
        // exist yet. The library default runs callbacks concurrently
        // and drops events when its smaller queue fills, so ordered
        // delivery is requested explicitly.
        .with_event_delivery(EventDelivery::Ordered { capacity: EVENT_QUEUE_CAPACITY })
        .on_qr_code(move |code, timeout| {
            let sink = sink.clone();
            async move {
                sink.emit(BackendEvent::QrCode { payload: code, valid_for: timeout });
            }
        })
        .on_pair_code(move |code, _timeout| {
            let sink = sink_pair.clone();
            async move {
                sink.emit(BackendEvent::PairCode { code });
            }
        })
        .on_connected(move |client| {
            let sink = sink_conn.clone();
            let fetch_slot = privacy_fetch_conn.clone();
            let blocklist_slot = blocklist_fetch_conn.clone();
            let profile_slot = own_profile_fetch_conn.clone();
            async move {
                sink.emit(BackendEvent::Connected { account: "WhatsApp account".to_owned() });
                // The privacy fetch is a network round trip and this
                // callback runs on the library's single ordered
                // delivery worker: awaiting it here would stall every
                // queued message and receipt behind it on each
                // connect, worst case for the library's IQ timeout.
                // It runs as its own task instead and emits
                // PrivacyState when it lands; a failed fetch stays
                // silent, exactly as before.
                let sink_privacy = sink.clone();
                let client_privacy = client.clone();
                spawn_latest(&fetch_slot, async move {
                    if let Ok(resp) = client_privacy.fetch_privacy_settings().await {
                        sink_privacy.emit(BackendEvent::PrivacyState(privacy_map(&resp)));
                    }
                });
                // The own-profile fetch rides the same pattern: the
                // profile editor and the account surfaces show the
                // account's real name, About and picture, read back
                // from the server (see whatsapp::profile). Parts
                // that cannot be read come back empty rather than
                // failing the fetch.
                let sink_profile = sink.clone();
                let client_profile = client.clone();
                spawn_latest(&profile_slot, async move {
                    let profile = super::profile::fetch_own_profile(&client_profile).await;
                    sink_profile.emit(BackendEvent::OwnProfile(profile));
                });
                // The blocklist fetch rides the same pattern: the
                // app seeds its blocked set from the server's list,
                // so blocks made on the phone (or before a restart)
                // show correctly. A failed fetch stays silent and
                // the app keeps its persisted copy.
                spawn_latest(&blocklist_slot, async move {
                    if let Ok(entries) = client.blocking().get_blocklist().await {
                        let jids = entries.iter().map(|e| e.jid.to_string()).collect::<Vec<_>>();
                        sink.emit(BackendEvent::Blocklist(jids));
                    }
                });
            }
        })
        .on_logged_out(move |_info| {
            let sink = sink_out.clone();
            let logged_out = logged_out_flag.clone();
            async move {
                logged_out.store(true, std::sync::atomic::Ordering::SeqCst);
                sink.emit(BackendEvent::LoggedOut);
            }
        })
        .on_message(move |ctx| {
            let sink = sink_msg.clone();
            let cache = cache_for_messages.clone();
            let secrets = secrets_for_messages.clone();
            let retry = retry_for_messages.clone();
            let heal_attempts = heal_attempts_msg.clone();
            async move {
                super::util::record_retry_target(
                    &retry,
                    &ctx.info.source.chat.to_string(),
                    ctx.info.id.as_ref(),
                    super::util::RetryMark {
                        from_me: ctx.info.source.is_from_me,
                        sender_jid: Some(ctx.info.source.sender.to_string()),
                    },
                );
                // A received poll creation carries its secret in
                // the message context info; caching it here is what
                // makes voting on other people's polls possible
                // later in the session (see docs/research/poll-voting.md).
                if let Some((secret, names)) = received_poll_secret(&ctx.message) {
                    lock_cache(&secrets).insert(
                        (ctx.info.source.chat.to_string(), ctx.info.id.to_string()),
                        (ctx.info.source.sender.to_string(), secret, names),
                    );
                    cap_poll_secrets(&secrets);
                }
                // A poll vote is a standalone encrypted message,
                // never a chat message: decrypt it against the
                // poll's creation secret and report the voter's
                // current selection. The decryption runs inline:
                // it is local work (the client's alias cache plus
                // the vote crypto, no network), and inline keeps
                // votes in arrival order behind the poll creation
                // they belong to, which last-vote-wins relies on.
                // A vote whose secret this session does not know
                // (and the app has not registered from its
                // archive) cannot be read and is dropped, never
                // guessed at.
                if let Some((poll_id, payload, iv)) = poll_vote_payload(&ctx.message) {
                    let chat_jid = ctx.info.source.chat.to_string();
                    let cached =
                        lock_cache(&secrets).get(&(chat_jid.clone(), poll_id.clone())).cloned();
                    if let Some((creator, secret, names)) = cached {
                        let voter = ctx.info.source.sender.clone();
                        if let Ok(creator_jid) = creator.parse::<Jid>() {
                            let ciphertext = whatsapp_rust::PollVoteCiphertext {
                                enc_payload: &payload,
                                enc_iv: &iv,
                            };
                            if let Ok(hashes) = ctx
                                .client
                                .polls()
                                .decrypt_vote_raw(
                                    ciphertext,
                                    &secret,
                                    &poll_id,
                                    &creator_jid,
                                    &voter,
                                )
                                .await
                            {
                                sink.emit(BackendEvent::PollVote {
                                    chat_jid,
                                    message_id: poll_id,
                                    voter_jid: voter.to_non_ad().to_string(),
                                    from_me: ctx.info.source.is_from_me,
                                    selected: selected_indices(&names, &hashes),
                                });
                            }
                        }
                    }
                    return;
                }
                {
                    let key = (ctx.info.source.chat.to_string(), ctx.info.id.to_string());
                    let mut guard = lock_cache(&cache);
                    // True FIFO eviction: the order queue tracks
                    // insertion, so the cap drops the OLDEST entries
                    // (clearing everything used to drop recent
                    // messages' forward/download payloads too).
                    if guard.0.len() > 1000 {
                        for _ in 0..100 {
                            if let Some(old) = guard.1.pop_front() {
                                guard.0.remove(&old);
                            }
                        }
                    }
                    if !guard.0.contains_key(&key) {
                        guard.1.push_back(key.clone());
                    }
                    guard.0.insert(key, ctx.message.clone());
                }
                if translate_special(&ctx, &sink) {
                    return;
                }
                if let Some((chat, msg)) = translate_message(&ctx) {
                    sink.emit(BackendEvent::ChatUpsert(chat.clone()));
                    sink.emit(BackendEvent::MessageUpsert(msg));
                    // A chat whose live upsert still carries its
                    // raw identifier as the title is reported to
                    // the healer: live traffic creates chats
                    // outside the history path the healer used to
                    // depend on, and the attempts set keeps the
                    // report to once per run per chat.
                    if chat.title == super::translate::user_part(&chat.jid) {
                        heal_unresolved_titles(
                            ctx.client.clone(),
                            sink.clone(),
                            vec![(chat.jid.clone(), chat.kind)],
                            heal_attempts.clone(),
                        );
                    }
                }
            }
        })
        .on_event(move |event: Arc<Event>, client| {
            let sink = sink_all.clone();
            let heal_attempts = heal_attempts_events.clone();
            let pending_history = pending_history_events.clone();
            let retry = retry_for_events.clone();
            #[cfg(feature = "calls")]
            let incoming_slot = call_incoming_events.clone();
            async move {
                #[cfg(feature = "calls")]
                handle_call_event(&event, &incoming_slot, &sink);
                // The account's own JIDs (non-AD spellings, the
                // form notifications carry): translate_event uses
                // them to tell the account-wide disappearing
                // default apart from a per-chat timer change.
                let own_jids: Vec<String> = [client.pn(), client.lid()]
                    .into_iter()
                    .flatten()
                    .map(|jid| jid.to_non_ad().to_string())
                    .collect();
                // An on-demand history answer is claimed before
                // translation: the pending entry comes out now,
                // but the completion is reported only after
                // translate_event has filed the page's messages,
                // so the app clears its loading state against
                // content that is already on screen.
                let history_claim = match &*event {
                    Event::HistorySync(lazy) => {
                        lazy.peer_data_request_session_id().and_then(|sid| {
                            pending_history
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .remove(sid)
                        })
                    }
                    _ => None,
                };
                let (unresolved, marks) = translate_event(&event, &sink, &own_jids);
                if let Some(chat_jid) = history_claim {
                    if let Event::HistorySync(lazy) = &*event {
                        match lazy.get() {
                            Some(sync) => {
                                let (added, end_of_history) = history_page_outcome(sync, &chat_jid);
                                sink.emit(BackendEvent::HistoryPageFetched {
                                    chat_jid,
                                    added,
                                    end_of_history,
                                });
                            }
                            None => sink.emit(BackendEvent::HistoryFetchFailed {
                                chat_jid,
                                message: "The phone's answer could not be read.".to_owned(),
                            }),
                        }
                    }
                }
                for (chat_jid, message_id, mark) in marks {
                    super::util::record_retry_target(&retry, &chat_jid, &message_id, mark);
                }
                if !unresolved.is_empty() {
                    heal_unresolved_titles(
                        client.clone(),
                        sink.clone(),
                        unresolved,
                        heal_attempts.clone(),
                    );
                }
            }
        })
        .build()
        .await
    {
        Ok(bot) => bot,
        Err(e) => {
            return Err(format!("Could not start the client: {e}"));
        }
    };

    let mut run_handle = bot.spawn();
    let client = run_handle.client();

    // Bridge std commands into the async loop.
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<BackendCommand>(64);
    std::thread::Builder::new()
        .name("zaprust-command-bridge".to_owned())
        .spawn(move || {
            while let Ok(cmd) = commands.recv() {
                if cmd_tx.blocking_send(cmd).is_err() {
                    break;
                }
            }
        })
        .ok();

    // Set when the bot run loop ends on its own (logout, a stopped
    // connection); stays None when the UI asks to shut down first.
    let mut run_outcome = None;

    // Commands that arrive while a handler is running wait here,
    // in arrival order, and are handled before anything newer is
    // received.
    let mut pending: VecDeque<BackendCommand> = VecDeque::new();

    // Dispatch contract: commands run one at a time, in arrival
    // order. The order is load-bearing, not an accident: the app
    // binds each MessageSent (and each send error) to the OLDEST
    // pending echo of its chat, so send completions must land in
    // issue order, and call commands must stay serialized against
    // the shared call slots. What keeps the serial loop from
    // becoming a stall is that every handler runs interruptibly
    // (see `await_handler`): Shutdown, a closed command channel,
    // or the bot run loop ending drops the in-flight handler and
    // proceeds to teardown at once. The waits that remain are
    // bounded: one library IQ round trip by the library's
    // 75-second default IQ timeout, call audio readiness by the
    // audio module's 10-second timeout, and a media send by
    // SEND_MEDIA_TIMEOUT at its call site (the upload's HTTP
    // layer bounds only the connect, not the transfer).
    'dispatch: loop {
        let received = if let Some(cmd) = pending.pop_front() {
            Some(cmd)
        } else {
            tokio::select! {
                cmd = cmd_rx.recv() => cmd,
                outcome = &mut run_handle => {
                    run_outcome = Some(outcome);
                    break 'dispatch;
                }
            }
        };
        let cmd = match received {
            Some(BackendCommand::Shutdown) | None => break 'dispatch,
            Some(cmd) => cmd,
        };
        // The handler runs as a future so `await_handler` can
        // watch the command channel and the bot handle while it
        // runs, and drop it the moment either says the session
        // is over.
        let handler = async {
            match cmd {
                BackendCommand::ConnectQr => {
                    // The bot emits QR payloads on its own while
                    // unpaired, and Connected when already linked;
                    // there is nothing extra to request.
                }
                BackendCommand::ConnectPairCode { phone } => {
                    let options = PairCodeOptions { phone_number: phone, ..Default::default() };
                    if let Err(e) = client.pair_with_code(options).await {
                        events.emit(BackendEvent::Error {
                            message: format!("Pairing code request failed: {e}"),
                        });
                    }
                }
                BackendCommand::SendText { chat_jid, body, quoted, ephemeral_secs } => {
                    match chat_jid.parse::<Jid>() {
                        Ok(jid) => {
                            let mut message = outgoing_text_message(&body, quoted.as_ref(), &jid);
                            stamp_ephemeral(&mut message, ephemeral_secs);
                            match client.send_message(jid, message).await {
                                Ok(sent) => events.emit(BackendEvent::MessageSent {
                                    chat_jid: chat_jid.clone(),
                                    message_id: sent.message_id.to_string(),
                                }),
                                Err(e) => events.emit(BackendEvent::Error {
                                    message: format!("Sending failed: {e}"),
                                }),
                            }
                        }
                        Err(e) => {
                            events.emit(BackendEvent::Error {
                                message: format!("Invalid chat address: {e}"),
                            });
                        }
                    }
                }
                BackendCommand::SendMedia {
                    chat_jid,
                    kind,
                    data,
                    mime,
                    caption,
                    file_name,
                    gif_playback,
                    seconds,
                    view_once,
                    ephemeral_secs,
                } => {
                    if data.is_empty() {
                        events.emit(BackendEvent::Error {
                            message: "That media file is empty, nothing was sent.".to_owned(),
                        });
                    } else {
                        match chat_jid.parse::<Jid>() {
                            Ok(jid) => {
                                let outgoing = OutgoingMedia {
                                    kind,
                                    data,
                                    mime: &mime,
                                    caption: &caption,
                                    file_name: file_name.as_deref(),
                                    gif_playback,
                                    seconds,
                                    view_once,
                                    ephemeral_secs,
                                };
                                // The upload's HTTP layer has
                                // no total timeout (see
                                // SEND_MEDIA_TIMEOUT), so the
                                // budget is enforced here.
                                match tokio::time::timeout(
                                    SEND_MEDIA_TIMEOUT,
                                    send_media(&client, &jid, outgoing),
                                )
                                .await
                                {
                                    Ok(Ok(sent)) => {
                                        events.emit(BackendEvent::MessageSent {
                                            chat_jid: chat_jid.clone(),
                                            message_id: sent.message_id.to_string(),
                                        });
                                    }
                                    Ok(Err(e)) => events.emit(BackendEvent::Error {
                                        message: format!("Media sending failed: {e}"),
                                    }),
                                    Err(_) => events.emit(BackendEvent::Error {
                                        message: format!(
                                            "Media sending timed out after {} minutes \
                                                 and was abandoned. Nothing was confirmed \
                                                 sent; try again on a better connection.",
                                            SEND_MEDIA_TIMEOUT.as_secs() / 60
                                        ),
                                    }),
                                }
                            }
                            Err(e) => {
                                events.emit(BackendEvent::Error {
                                    message: format!("Invalid chat address: {e}"),
                                });
                            }
                        }
                    }
                }
                BackendCommand::SendLocation {
                    chat_jid,
                    name,
                    latitude,
                    longitude,
                    ephemeral_secs,
                } => match chat_jid.parse::<Jid>() {
                    Ok(jid) => {
                        let mut message = outgoing_location_message(&name, latitude, longitude);
                        stamp_ephemeral(&mut message, ephemeral_secs);
                        match client.send_message(jid, message).await {
                            Ok(sent) => events.emit(BackendEvent::MessageSent {
                                chat_jid: chat_jid.clone(),
                                message_id: sent.message_id.to_string(),
                            }),
                            Err(e) => events.emit(BackendEvent::Error {
                                message: format!("Sending the location failed: {e}"),
                            }),
                        }
                    }
                    Err(e) => {
                        events.emit(BackendEvent::Error {
                            message: format!("Invalid chat address: {e}"),
                        });
                    }
                },
                BackendCommand::PostTextStatus { text, recipients } => {
                    // A status post is a sender-key fan-out to
                    // its audience, so the command carries the
                    // audience JIDs. Unusable entries are dropped;
                    // an empty text, or a post with nobody left
                    // to receive it, is refused honestly instead
                    // of being sent to nobody.
                    let audience: Vec<Jid> =
                        recipients.iter().filter_map(|r| r.parse::<Jid>().ok()).collect();
                    if text.trim().is_empty() {
                        events.emit(BackendEvent::StatusPostFailed {
                            message: "That status is empty, nothing was posted.".to_owned(),
                        });
                    } else if audience.is_empty() {
                        events.emit(BackendEvent::StatusPostFailed {
                            message: "There is nobody to post that status to.".to_owned(),
                        });
                    } else {
                        match post_text_status(&client, &text, &audience).await {
                            Ok(_) => events.emit(BackendEvent::StatusPosted { text }),
                            Err(e) => events.emit(BackendEvent::StatusPostFailed {
                                message: format!("Posting the status failed: {e}"),
                            }),
                        }
                    }
                }
                BackendCommand::SetDisappearingTimer { chat_jid, duration_secs } => {
                    match chat_jid.parse::<Jid>() {
                        Ok(jid) => {
                            // Groups take the group IQ path;
                            // direct chats take the protocol
                            // message path (the library rejects
                            // group JIDs there, and vice versa).
                            let result = if chat_jid.ends_with("@g.us") {
                                client
                                    .groups()
                                    .set_ephemeral(jid, duration_secs)
                                    .await
                                    .map_err(|e| format!("{e}"))
                            } else {
                                client
                                    .set_chat_disappearing_timer(jid, duration_secs)
                                    .await
                                    .map(|_| ())
                                    .map_err(|e| format!("{e}"))
                            };
                            match result {
                                Ok(()) => events.emit(BackendEvent::DisappearingChanged {
                                    chat_jid,
                                    duration_secs,
                                }),
                                Err(e) => events.emit(BackendEvent::Error {
                                    message: format!("Changing disappearing messages failed: {e}"),
                                }),
                            }
                        }
                        Err(e) => {
                            events.emit(BackendEvent::Error {
                                message: format!("Invalid chat address: {e}"),
                            });
                        }
                    }
                }
                BackendCommand::CreatePoll { chat_jid, question, options, multiple } => {
                    match chat_jid.parse::<Jid>() {
                        Ok(jid) => {
                            let selectable = if multiple {
                                u32::try_from(options.len()).unwrap_or(1).max(1)
                            } else {
                                1
                            };
                            match client
                                .polls()
                                .create(jid.clone(), &question, &options, selectable)
                                .await
                            {
                                Ok(created) => {
                                    // Cache the creation secret so
                                    // this poll stays votable for
                                    // the rest of the session.
                                    lock_cache(&poll_secrets).insert(
                                        (
                                            jid.to_string(),
                                            created.send_result().message_id.to_string(),
                                        ),
                                        (
                                            created.creator().to_string(),
                                            created.secret().as_bytes().to_vec(),
                                            options.clone(),
                                        ),
                                    );
                                    cap_poll_secrets(&poll_secrets);
                                }
                                Err(e) => events.emit(BackendEvent::Error {
                                    message: format!("Creating the poll failed: {e}"),
                                }),
                            }
                        }
                        Err(e) => events.emit(BackendEvent::Error {
                            message: format!("Invalid chat address: {e}"),
                        }),
                    }
                }
                BackendCommand::RegisterPollSecret {
                    chat_jid,
                    message_id,
                    creator_jid,
                    secret,
                    options,
                } => {
                    // The app kept this poll's creation secret in
                    // its archive from an earlier session; putting
                    // it in the session cache makes the poll
                    // votable and its incoming votes decryptable
                    // again. Silent by contract: nothing answers.
                    lock_cache(&poll_secrets)
                        .insert((chat_jid, message_id), (creator_jid, secret, options));
                    cap_poll_secrets(&poll_secrets);
                }
                BackendCommand::VotePoll { chat_jid, message_id, options } => {
                    // Voting needs the poll's creation secret.
                    // It is cached at creation for own polls and
                    // on receipt for other people's polls (the
                    // secret travels inside the creation message,
                    // see docs/research/poll-voting.md), and the
                    // app re-registers the secrets it archived for
                    // polls from earlier sessions. A poll whose
                    // secret none of those paths supplied gets an
                    // honest error, never a fake vote.
                    let cached = lock_cache(&poll_secrets)
                        .get(&(chat_jid.clone(), message_id.clone()))
                        .cloned();
                    match (cached, chat_jid.parse::<Jid>()) {
                        (Some((creator, secret, names)), Ok(jid)) => {
                            let options: Vec<String> =
                                options.iter().filter_map(|i| names.get(*i).cloned()).collect();
                            match creator.parse::<Jid>() {
                                Ok(creator_jid) => {
                                    if let Err(e) = client
                                        .polls()
                                        .vote_raw(jid, &message_id, &creator_jid, &secret, &options)
                                        .await
                                    {
                                        events.emit(BackendEvent::Error {
                                            message: format!("Voting failed: {e}"),
                                        });
                                    }
                                }
                                Err(e) => events.emit(BackendEvent::Error {
                                    message: format!("Invalid poll creator address: {e}"),
                                }),
                            }
                        }
                        (None, _) => events.emit(BackendEvent::Error {
                            message: "This poll cannot be voted on from Zaprust: its \
                                          creation secret is not available. The secret \
                                          travels with the poll when it is created or \
                                          received, and Zaprust keeps it with the archived \
                                          poll; this poll's secret never reached this \
                                          device, so the poll predates the archive or the \
                                          sender's client did not include it."
                                .to_owned(),
                        }),
                        (_, Err(e)) => events.emit(BackendEvent::Error {
                            message: format!("Invalid chat address: {e}"),
                        }),
                    }
                }
                #[cfg(feature = "calls")]
                BackendCommand::StartCall { chat_jid, video } => {
                    start_outgoing_call(&client, &events, &call_active, &chat_jid, video).await;
                }
                #[cfg(feature = "calls")]
                BackendCommand::AcceptCall => {
                    accept_incoming_call(&client, &events, &call_active, &call_incoming).await;
                }
                #[cfg(feature = "calls")]
                BackendCommand::RejectCall => {
                    let stored = lock_cache(&call_incoming).take();
                    match stored {
                        Some(event) => {
                            if let Event::IncomingCall(ic) = &*event {
                                let (call_id, chat) = offer_call_id(ic);
                                match client.voip().reject(ic).await {
                                    Ok(()) => events.emit(BackendEvent::CallChanged {
                                        call_id,
                                        chat_jid: chat,
                                        phase: crate::CallPhase::Ended,
                                        seconds: 0,
                                        reason: Some("Declined".to_owned()),
                                    }),
                                    Err(e) => events.emit(BackendEvent::Error {
                                        message: format!("Rejecting the call failed: {e}"),
                                    }),
                                }
                            }
                        }
                        None => events.emit(BackendEvent::Error {
                            message: "There is no incoming call to reject.".to_owned(),
                        }),
                    }
                }
                #[cfg(feature = "calls")]
                BackendCommand::EndCall => {
                    let handle = lock_cache(&call_active).as_ref().map(|a| a.handle.clone());
                    if let Some(handle) = handle {
                        // The pump notices the end and
                        // emits the final CallChanged.
                        let _ = handle.terminate().await;
                    } else {
                        // Ringing incoming call: EndCall
                        // declines it.
                        let stored = lock_cache(&call_incoming).take();
                        match stored {
                            Some(event) => {
                                if let Event::IncomingCall(ic) = &*event {
                                    let _ = client.voip().reject(ic).await;
                                    let (call_id, chat) = offer_call_id(ic);
                                    events.emit(BackendEvent::CallChanged {
                                        call_id,
                                        chat_jid: chat,
                                        phase: crate::CallPhase::Ended,
                                        seconds: 0,
                                        reason: Some("Declined".to_owned()),
                                    });
                                }
                            }
                            None => events.emit(BackendEvent::Error {
                                message: "No call is in progress.".to_owned(),
                            }),
                        }
                    }
                }
                #[cfg(feature = "calls")]
                BackendCommand::SetCallMuted { muted } => {
                    let handle = lock_cache(&call_active).as_ref().map(|a| a.handle.clone());
                    match handle {
                        Some(handle) => {
                            if let Err(e) = handle.set_muted(muted).await {
                                events.emit(BackendEvent::Error {
                                    message: format!("Setting mute failed: {e}"),
                                });
                            }
                        }
                        None => events.emit(BackendEvent::Error {
                            message: "No call is in progress.".to_owned(),
                        }),
                    }
                }
                #[cfg(feature = "calls")]
                BackendCommand::SetCallSpeaker { speaker } => {
                    let audio_speaker =
                        lock_cache(&call_active).as_ref().map(|a| a.audio.speaker.clone());
                    match audio_speaker {
                        Some(flag) => flag.store(speaker, std::sync::atomic::Ordering::Relaxed),
                        None => events.emit(BackendEvent::Error {
                            message: "No call is in progress.".to_owned(),
                        }),
                    }
                }
                #[cfg(not(feature = "calls"))]
                BackendCommand::StartCall { .. }
                | BackendCommand::AcceptCall
                | BackendCommand::RejectCall
                | BackendCommand::EndCall
                | BackendCommand::SetCallMuted { .. }
                | BackendCommand::SetCallSpeaker { .. } => {
                    events.emit(BackendEvent::Error {
                        message: "Calls are not compiled into this build.".to_owned(),
                    });
                }
                BackendCommand::React { chat_jid, message_id, sender_jid, from_me, emoji } => {
                    // Failures answer with ReactionFailed, not the
                    // generic Error: the app must roll the one
                    // optimistic reaction back, and the generic
                    // path would also fail pending message echoes
                    // that have nothing to do with this reaction.
                    let fail = |message: String| {
                        events.emit(BackendEvent::ReactionFailed {
                            chat_jid: chat_jid.clone(),
                            message_id: message_id.clone(),
                            message,
                        });
                    };
                    match chat_jid.parse::<Jid>() {
                        Ok(jid) => {
                            let sender = sender_jid.and_then(|s| s.parse::<Jid>().ok());
                            let id = MessageId::new(message_id.as_str());
                            let target = id.and_then(|id| {
                                whatsapp_rust::MessageRef::new(&jid, id, sender.as_ref(), from_me)
                            });
                            match target {
                                Ok(target) => {
                                    if let Err(e) = client.send_reaction(&target, &emoji).await {
                                        fail(format!("Reaction failed: {e}"));
                                    }
                                }
                                Err(e) => fail(format!("Reaction target invalid: {e}")),
                            }
                        }
                        Err(e) => fail(format!("Invalid chat address: {e}")),
                    }
                }
                BackendCommand::MarkAsRead { chat_jid, sender_jid, message_ids } => {
                    match chat_jid.parse::<Jid>() {
                        Ok(jid) => {
                            let sender = sender_jid.and_then(|s| s.parse::<Jid>().ok());
                            let ids: Vec<&str> = message_ids.iter().map(String::as_str).collect();
                            let _ = client.mark_as_read(&jid, sender.as_ref(), &ids).await;
                        }
                        Err(e) => events.emit(BackendEvent::Error {
                            message: format!("Invalid chat address: {e}"),
                        }),
                    }
                }
                BackendCommand::SetTyping { chat_jid, typing } => match chat_jid.parse::<Jid>() {
                    Ok(jid) => {
                        if typing {
                            let _ = client.chatstate().send_composing(&jid).await;
                        } else {
                            let _ = client.chatstate().send_paused(&jid).await;
                        }
                    }
                    Err(e) => events.emit(BackendEvent::Error {
                        message: format!("Invalid chat address: {e}"),
                    }),
                },
                BackendCommand::SetPresenceSubscription { chat_jid, subscribed } => {
                    match chat_jid.parse::<Jid>() {
                        Ok(jid) => {
                            // The library tracks the subscription
                            // and re-subscribes on its own after a
                            // reconnect; a failed send (mid-reconnect)
                            // is dropped like the typing sends above,
                            // and the app re-asks when the link state
                            // settles.
                            if subscribed {
                                let _ = client.presence().subscribe(jid).await;
                            } else {
                                let _ = client.presence().unsubscribe(&jid).await;
                            }
                        }
                        Err(e) => events.emit(BackendEvent::Error {
                            message: format!("Invalid chat address: {e}"),
                        }),
                    }
                }
                BackendCommand::EditMessage { chat_jid, message_id, new_body } => {
                    use whatsapp_rust::wacore::proto_helpers::MessageBuilderExt;
                    match build_message_target(&chat_jid, &message_id, None, true) {
                        Ok((jid, sender, mid)) => {
                            match whatsapp_rust::MessageRef::new(&jid, mid, sender.as_ref(), true) {
                                Ok(target) => {
                                    let request = whatsapp_rust::EditRequest::new(
                                        target,
                                        whatsapp_rust::prelude::wa::Message::text(new_body),
                                    );
                                    if let Err(e) = client.edit_message(request).await {
                                        events.emit(BackendEvent::Error {
                                            message: format!("Editing failed: {e}"),
                                        });
                                    }
                                }
                                Err(_) => events.emit(BackendEvent::Error {
                                    message: "That message reference is invalid.".to_owned(),
                                }),
                            }
                        }
                        Err(e) => events.emit(BackendEvent::Error { message: e }),
                    }
                }
                BackendCommand::RevokeMessage { chat_jid, message_id } => {
                    match build_message_target(&chat_jid, &message_id, None, true) {
                        Ok((jid, sender, mid)) => {
                            match whatsapp_rust::MessageRef::new(&jid, mid, sender.as_ref(), true) {
                                Ok(target) => {
                                    if let Err(e) = client.revoke_message(&target).await {
                                        events.emit(BackendEvent::Error {
                                            message: format!("Deleting for everyone failed: {e}"),
                                        });
                                    }
                                }
                                Err(_) => events.emit(BackendEvent::Error {
                                    message: "That message reference is invalid.".to_owned(),
                                }),
                            }
                        }
                        Err(e) => events.emit(BackendEvent::Error { message: e }),
                    }
                }
                BackendCommand::ForwardMessage { from_chat_jid, message_id, to_chat_jid } => {
                    let proto = lock_cache(&proto_cache)
                        .0
                        .get(&(from_chat_jid.clone(), message_id.clone()))
                        .cloned();
                    match (proto, to_chat_jid.parse::<Jid>()) {
                        (Some(proto), Ok(_)) if proto_is_view_once(&proto) => {
                            // The app gates this too; the backend
                            // refuses as well so no caller can relay
                            // media the sender marked view-once.
                            events.emit(BackendEvent::Error {
                                message: "View-once media cannot be forwarded.".to_owned(),
                            });
                        }
                        (Some(proto), Ok(to)) => {
                            if let Err(e) = client.forward_message(to, &proto).await {
                                events.emit(BackendEvent::Error {
                                    message: format!("Forwarding failed: {e}"),
                                });
                            }
                        }
                        (None, _) => events.emit(BackendEvent::Error {
                            message: "Only messages received or sent during this session \
                                          can be forwarded."
                                .to_owned(),
                        }),
                        (_, Err(e)) => events.emit(BackendEvent::Error {
                            message: format!("Invalid chat address: {e}"),
                        }),
                    }
                }
                BackendCommand::StarMessage { chat_jid, message_id, sender_jid, from_me, star } => {
                    // Failures answer with StarFailed, not the
                    // generic Error: the app must roll the one
                    // optimistic star back, and the generic path
                    // would also fail pending message echoes that
                    // have nothing to do with this star.
                    let fail = |message: String| {
                        events.emit(BackendEvent::StarFailed {
                            chat_jid: chat_jid.clone(),
                            message_id: message_id.clone(),
                            message,
                        });
                    };
                    match build_message_target(&chat_jid, &message_id, sender_jid.as_ref(), from_me)
                    {
                        Ok((jid, sender, mid)) => {
                            match whatsapp_rust::MessageRef::new(
                                &jid,
                                mid,
                                sender.as_ref(),
                                from_me,
                            ) {
                                Ok(target) => {
                                    let result = if star {
                                        client.chat_actions().star_message(&target).await
                                    } else {
                                        client.chat_actions().unstar_message(&target).await
                                    };
                                    if let Err(e) = result {
                                        fail(format!("Starring failed: {e}"));
                                    }
                                }
                                Err(_) => {
                                    fail("That message reference is invalid.".to_owned());
                                }
                            }
                        }
                        Err(e) => fail(e),
                    }
                }
                BackendCommand::PinMessage { chat_jid, message_id, sender_jid, from_me, pin } => {
                    // PinFailed, not the generic Error, for the
                    // same reason as StarFailed above: only the
                    // app can roll the optimistic pin back, and
                    // only if the failure names its message.
                    let fail = |message: String| {
                        events.emit(BackendEvent::PinFailed {
                            chat_jid: chat_jid.clone(),
                            message_id: message_id.clone(),
                            message,
                        });
                    };
                    match build_message_target(&chat_jid, &message_id, sender_jid.as_ref(), from_me)
                    {
                        Ok((jid, sender, mid)) => {
                            match whatsapp_rust::MessageRef::new(
                                &jid,
                                mid,
                                sender.as_ref(),
                                from_me,
                            ) {
                                Ok(target) => {
                                    let result = if pin {
                                        client
                                            .pin_message(&target, whatsapp_rust::PinDuration::Days7)
                                            .await
                                    } else {
                                        client.unpin_message(&target).await
                                    };
                                    if let Err(e) = result {
                                        fail(format!("Pinning the message failed: {e}"));
                                    }
                                }
                                Err(_) => {
                                    fail("That message reference is invalid.".to_owned());
                                }
                            }
                        }
                        Err(e) => fail(e),
                    }
                }
                BackendCommand::ChatAction { chat_jid, action } => match chat_jid.parse::<Jid>() {
                    Ok(jid) => {
                        let actions = client.chat_actions();
                        let result = match action {
                            ChatAction::Archive => actions.archive_chat(&jid, None).await,
                            ChatAction::Unarchive => actions.unarchive_chat(&jid, None).await,
                            ChatAction::Pin => actions.pin_chat(&jid).await,
                            ChatAction::Unpin => actions.unpin_chat(&jid).await,
                            ChatAction::Mute => actions.mute_chat(&jid).await,
                            ChatAction::Unmute => actions.unmute_chat(&jid).await,
                        };
                        if let Err(e) = result {
                            events.emit(BackendEvent::Error {
                                message: format!("That chat action failed: {e}"),
                            });
                        }
                    }
                    Err(e) => events.emit(BackendEvent::Error {
                        message: format!("Invalid chat address: {e}"),
                    }),
                },
                BackendCommand::BlockContact { chat_jid, block } => match chat_jid.parse::<Jid>() {
                    Ok(jid) => {
                        let result = if block {
                            client.blocking().block(&jid).await
                        } else {
                            client.blocking().unblock(&jid).await
                        };
                        if let Err(e) = result {
                            events.emit(BackendEvent::Error {
                                message: format!("Blocking failed: {e}"),
                            });
                        }
                    }
                    Err(e) => events.emit(BackendEvent::Error {
                        message: format!("Invalid chat address: {e}"),
                    }),
                },
                BackendCommand::CreateCommunity { name } => {
                    match whatsapp_rust::CreateCommunityOptions::new(name.clone()) {
                        Ok(options) => match client.community().create(options).await {
                            Ok(result) => {
                                events.emit(BackendEvent::CommunityCreated {
                                    jid: result.metadata.id.to_string(),
                                    name,
                                });
                            }
                            Err(e) => events.emit(BackendEvent::Error {
                                message: format!("Creating the community failed: {e}"),
                            }),
                        },
                        Err(e) => events.emit(BackendEvent::Error {
                            message: format!("Creating the community failed: {e}"),
                        }),
                    }
                }
                BackendCommand::CreateGroup { name, participants } => {
                    let jids: Result<Vec<Jid>, String> =
                        participants.iter().map(|p| parse_address(p)).collect();
                    match jids {
                        Ok(jids) => {
                            let options = whatsapp_rust::GroupCreateOptions::new(name.clone())
                                .with_participants(
                                    jids.into_iter()
                                        .map(whatsapp_rust::GroupParticipantOptions::new)
                                        .collect(),
                                );
                            match client.groups().create_group(options).await {
                                Ok(result) => {
                                    let meta = result.metadata;
                                    events.emit(BackendEvent::ChatUpsert(LiveChat {
                                        jid: meta.id.to_string(),
                                        title: name,
                                        kind: LiveChatKind::Group,
                                        preview: "Group created".to_owned(),
                                        preview_sender: None,
                                        timestamp: unix_now(),
                                        unread: 0,
                                        archived: false,
                                        pinned: false,
                                        ephemeral_secs: 0,
                                        muted: false,
                                    }));
                                }
                                Err(e) => events.emit(BackendEvent::Error {
                                    message: format!("Creating the group failed: {e}"),
                                }),
                            }
                        }
                        Err(e) => events.emit(BackendEvent::Error { message: e }),
                    }
                }
                BackendCommand::GroupParticipants { chat_jid, participants, op } => {
                    let parsed = (
                        chat_jid.parse::<Jid>(),
                        participants
                            .iter()
                            .map(|p| parse_address(p))
                            .collect::<Result<Vec<Jid>, String>>(),
                    );
                    match parsed {
                        (Ok(jid), Ok(parts)) => {
                            let groups = client.groups();
                            let result = match op {
                                ParticipantOp::Add => {
                                    groups.add_participants(jid, &parts).await.map(|_| ())
                                }
                                ParticipantOp::Remove => {
                                    groups.remove_participants(jid, &parts).await.map(|_| ())
                                }
                                ParticipantOp::Promote => {
                                    groups.promote_participants(jid, &parts).await.map(|_| ())
                                }
                                ParticipantOp::Demote => {
                                    groups.demote_participants(jid, &parts).await.map(|_| ())
                                }
                            };
                            if let Err(e) = result {
                                events.emit(BackendEvent::Error {
                                    message: format!("That group change failed: {e}"),
                                });
                            }
                        }
                        (Err(e), _) => events.emit(BackendEvent::Error {
                            message: format!("Invalid group address: {e}"),
                        }),
                        (_, Err(e)) => events.emit(BackendEvent::Error { message: e }),
                    }
                }
                BackendCommand::SetGroupInfo { chat_jid, subject, description } => {
                    match chat_jid.parse::<Jid>() {
                        Ok(jid) => {
                            if let Some(subject) = subject {
                                match whatsapp_rust::GroupSubject::new(subject) {
                                    Ok(subject) => {
                                        if let Err(e) =
                                            client.groups().set_subject(jid.clone(), subject).await
                                        {
                                            events.emit(BackendEvent::Error {
                                                message: format!("Renaming the group failed: {e}"),
                                            });
                                        }
                                    }
                                    Err(e) => events.emit(BackendEvent::Error {
                                        message: format!("That group name is not allowed: {e}"),
                                    }),
                                }
                            }
                            if let Some(description) = description {
                                // Empty means an intentional clear;
                                // anything else must VALIDATE. A
                                // validation failure is an error,
                                // never a silent clear (that wiped
                                // descriptions before).
                                let parsed = if description.is_empty() {
                                    Ok(None)
                                } else {
                                    whatsapp_rust::GroupDescription::new(description)
                                        .map(Some)
                                        .map_err(|e| {
                                            format!("That description is not allowed: {e}")
                                        })
                                };
                                match parsed {
                                    Ok(description) => {
                                        if let Err(e) = client
                                            .groups()
                                            .set_description(
                                                jid,
                                                description,
                                                whatsapp_rust::PreviousDescription::Resolve,
                                            )
                                            .await
                                        {
                                            events.emit(BackendEvent::Error {
                                                message: format!(
                                                    "Changing the description failed: {e}"
                                                ),
                                            });
                                        }
                                    }
                                    Err(message) => {
                                        events.emit(BackendEvent::Error { message });
                                    }
                                }
                            }
                        }
                        Err(e) => events.emit(BackendEvent::Error {
                            message: format!("Invalid group address: {e}"),
                        }),
                    }
                }
                BackendCommand::FetchGroupMembers { chat_jid } => match chat_jid.parse::<Jid>() {
                    Ok(jid) => match client.groups().fetch_metadata(&jid).await {
                        Ok(meta) => {
                            let members = meta
                                .participants
                                .iter()
                                .map(|p| (p.jid.to_string(), participant_name(p), p.is_admin()))
                                .collect();
                            events.emit(BackendEvent::GroupMembers {
                                chat_jid: chat_jid.clone(),
                                members,
                            });
                            // The same metadata is the group's
                            // identity source: its subject heals a
                            // raw group title, and its participant
                            // names heal raw sender labels, under
                            // the host's monotonic guards.
                            if let Some(subject) =
                                meta.subject.clone().filter(|s| !s.trim().is_empty())
                            {
                                events.emit(BackendEvent::ChatRenamed {
                                    chat_jid: chat_jid.clone(),
                                    title: subject,
                                });
                            }
                            for participant in &meta.participants {
                                let name = participant_name(participant);
                                if !name.is_empty() {
                                    events.emit(BackendEvent::SenderNamed {
                                        sender_jid: participant.jid.to_string(),
                                        name,
                                    });
                                }
                            }
                            events.emit(BackendEvent::GroupApprovalState {
                                chat_jid: chat_jid.clone(),
                                enabled: meta.membership_approval,
                            });
                            events.emit(BackendEvent::GroupModesState {
                                chat_jid: chat_jid.clone(),
                                announce: meta.is_announcement,
                                locked: meta.is_locked,
                            });
                        }
                        Err(e) => events.emit(BackendEvent::Error {
                            message: format!("Could not load the member list: {e}"),
                        }),
                    },
                    Err(e) => events.emit(BackendEvent::Error {
                        message: format!("Invalid group address: {e}"),
                    }),
                },
                BackendCommand::FetchGroupInviteLink { chat_jid, reset } => {
                    match chat_jid.parse::<Jid>() {
                        Ok(jid) => match client.groups().get_invite_link(jid, reset).await {
                            Ok(link) => events.emit(BackendEvent::GroupInviteLink {
                                chat_jid: chat_jid.clone(),
                                link,
                            }),
                            Err(e) => events.emit(BackendEvent::Error {
                                message: format!("Could not load the invite link: {e}"),
                            }),
                        },
                        Err(e) => events.emit(BackendEvent::Error {
                            message: format!("Invalid group address: {e}"),
                        }),
                    }
                }
                BackendCommand::SetMembershipApproval { chat_jid, enabled } => {
                    match chat_jid.parse::<Jid>() {
                        Ok(jid) => {
                            let mode = if enabled {
                                whatsapp_rust::MembershipApprovalMode::On
                            } else {
                                whatsapp_rust::MembershipApprovalMode::Off
                            };
                            match client.groups().set_membership_approval(jid, mode).await {
                                Ok(()) => events.emit(BackendEvent::GroupApprovalState {
                                    chat_jid: chat_jid.clone(),
                                    enabled,
                                }),
                                Err(e) => events.emit(BackendEvent::Error {
                                    message: format!("Changing the approval setting failed: {e}"),
                                }),
                            }
                        }
                        Err(e) => events.emit(BackendEvent::Error {
                            message: format!("Invalid group address: {e}"),
                        }),
                    }
                }
                BackendCommand::FetchJoinRequests { chat_jid } => match chat_jid.parse::<Jid>() {
                    Ok(jid) => match client.groups().get_membership_requests(jid).await {
                        Ok(list) => {
                            let requests = request_rows(&list);
                            events.emit(BackendEvent::JoinRequests {
                                chat_jid: chat_jid.clone(),
                                requests,
                            });
                        }
                        Err(e) => events.emit(BackendEvent::Error {
                            message: format!("Could not load the requests: {e}"),
                        }),
                    },
                    Err(e) => events.emit(BackendEvent::Error {
                        message: format!("Invalid group address: {e}"),
                    }),
                },
                BackendCommand::ReviewJoinRequest { chat_jid, participant, approve } => {
                    let parsed = (chat_jid.parse::<Jid>(), parse_address(&participant));
                    match parsed {
                        (Ok(jid), Ok(who)) => {
                            let groups = client.groups();
                            let result = if approve {
                                groups
                                    .approve_membership_requests(jid.clone(), &[who])
                                    .await
                                    .map(|_| ())
                            } else {
                                groups
                                    .reject_membership_requests(jid.clone(), &[who])
                                    .await
                                    .map(|_| ())
                            };
                            match result {
                                Ok(()) => match groups.get_membership_requests(jid).await {
                                    Ok(list) => {
                                        let requests = request_rows(&list);
                                        events.emit(BackendEvent::JoinRequests {
                                            chat_jid: chat_jid.clone(),
                                            requests,
                                        });
                                    }
                                    Err(e) => events.emit(BackendEvent::Error {
                                        message: format!("Could not reload the requests: {e}"),
                                    }),
                                },
                                Err(e) => events.emit(BackendEvent::Error {
                                    message: format!("That review did not go through: {e}"),
                                }),
                            }
                        }
                        (Err(e), _) => events.emit(BackendEvent::Error {
                            message: format!("Invalid group address: {e}"),
                        }),
                        (_, Err(e)) => events.emit(BackendEvent::Error { message: e }),
                    }
                }
                BackendCommand::SetGroupAnnounce { chat_jid, announce } => {
                    match chat_jid.parse::<Jid>() {
                        Ok(jid) => {
                            match client.groups().set_announce(jid.clone(), announce).await {
                                Ok(()) => match client.groups().fetch_metadata(&jid).await {
                                    Ok(meta) => events.emit(BackendEvent::GroupModesState {
                                        chat_jid: chat_jid.clone(),
                                        announce: meta.is_announcement,
                                        locked: meta.is_locked,
                                    }),
                                    Err(e) => events.emit(BackendEvent::Error {
                                        message: format!("Could not reload the group modes: {e}"),
                                    }),
                                },
                                Err(e) => events.emit(BackendEvent::Error {
                                    message: format!("Changing who can send messages failed: {e}"),
                                }),
                            }
                        }
                        Err(e) => events.emit(BackendEvent::Error {
                            message: format!("Invalid group address: {e}"),
                        }),
                    }
                }
                BackendCommand::SetGroupLocked { chat_jid, locked } => {
                    match chat_jid.parse::<Jid>() {
                        Ok(jid) => match client.groups().set_locked(jid.clone(), locked).await {
                            Ok(()) => match client.groups().fetch_metadata(&jid).await {
                                Ok(meta) => events.emit(BackendEvent::GroupModesState {
                                    chat_jid: chat_jid.clone(),
                                    announce: meta.is_announcement,
                                    locked: meta.is_locked,
                                }),
                                Err(e) => events.emit(BackendEvent::Error {
                                    message: format!("Could not reload the group modes: {e}"),
                                }),
                            },
                            Err(e) => events.emit(BackendEvent::Error {
                                message: format!("Changing who can edit the group failed: {e}"),
                            }),
                        },
                        Err(e) => events.emit(BackendEvent::Error {
                            message: format!("Invalid group address: {e}"),
                        }),
                    }
                }
                BackendCommand::SetDefaultDisappearingTimer { duration_secs } => {
                    if let Err(e) = client.set_default_disappearing_mode(duration_secs).await {
                        events.emit(BackendEvent::Error {
                            message: format!("Setting the default timer failed: {e}"),
                        });
                    }
                }
                BackendCommand::JoinGroupByLink { link } => {
                    match client.groups().join_with_invite_code(&link).await {
                        Ok(whatsapp_rust::JoinGroupResult::PendingApproval(_)) => {
                            // Not a member yet: no chat row
                            // may appear until an admin approves.
                            events.emit(BackendEvent::Error {
                                    message: "Join request sent. The group appears here once an admin approves it.".to_owned(),
                                });
                        }
                        Ok(joined) => {
                            let group_jid = joined.group_jid().clone();
                            let jid = group_jid.to_string();
                            let title = client
                                .groups()
                                .fetch_metadata(&group_jid)
                                .await
                                .ok()
                                .and_then(|m| m.subject.as_ref().map(|s| s.as_str().to_owned()))
                                .unwrap_or_else(|| "Group".to_owned());
                            events.emit(BackendEvent::ChatUpsert(crate::LiveChat {
                                jid,
                                title,
                                kind: crate::LiveChatKind::Group,
                                preview: "Group joined".to_owned(),
                                preview_sender: None,
                                timestamp: 0,
                                unread: 0,
                                archived: false,
                                pinned: false,
                                ephemeral_secs: 0,
                                muted: false,
                            }));
                        }
                        Err(e) => events.emit(BackendEvent::Error {
                            message: format!("Joining that group failed: {e}"),
                        }),
                    }
                }
                BackendCommand::LeaveGroup { chat_jid } => match chat_jid.parse::<Jid>() {
                    Ok(jid) => {
                        if let Err(e) = client.groups().leave(jid).await {
                            events.emit(BackendEvent::Error {
                                message: format!("Leaving the group failed: {e}"),
                            });
                        }
                    }
                    Err(e) => events.emit(BackendEvent::Error {
                        message: format!("Invalid group address: {e}"),
                    }),
                },
                BackendCommand::DownloadMedia { chat_jid, message_id } => {
                    let proto = lock_cache(&proto_cache)
                        .0
                        .get(&(chat_jid.clone(), message_id.clone()))
                        .cloned();
                    match proto {
                        Some(proto) => {
                            // The download runs as its own task: a
                            // transfer takes as long as the network
                            // takes, and the dispatch loop (Shutdown
                            // included) must not wait behind it. The
                            // outcome arrives as the same MediaSaved
                            // / Error events as before.
                            let client = client.clone();
                            let events = events.clone();
                            let targets = retry_for_commands.clone();
                            let dir = config.data_dir.join("downloads");
                            // The stem is the naming contract the
                            // app's deletion paths match downloads
                            // by (see `downloaded_media_stem`).
                            let stem = crate::downloaded_media_stem(&chat_jid, &message_id);
                            tokio::spawn(async move {
                                let saved = download_proto(&client, &proto, &dir, &stem).await;
                                // One media-retry recovery on
                                // failure, same as the display
                                // fetches: an expired CDN object
                                // is re-uploaded by the author's
                                // device and saved under the same
                                // file name.
                                let saved = match saved {
                                    Ok(path) => Ok(path),
                                    Err(message) => {
                                        let fields = super::util::media_fields_of(&proto);
                                        match fields {
                                            Some(fields) => {
                                                let ext = match fields.media_type {
                                                    whatsapp_rust::download::MediaType::Image => {
                                                        "jpg"
                                                    }
                                                    whatsapp_rust::download::MediaType::Video => {
                                                        "mp4"
                                                    }
                                                    whatsapp_rust::download::MediaType::Audio => {
                                                        "ogg"
                                                    }
                                                    whatsapp_rust::download::MediaType::Sticker => {
                                                        "webp"
                                                    }
                                                    _ => "bin",
                                                };
                                                retry_download_file(
                                                    &client,
                                                    &events,
                                                    &targets,
                                                    &chat_jid,
                                                    &message_id,
                                                    &fields,
                                                    &dir,
                                                    &stem,
                                                    ext,
                                                )
                                                .await
                                                .ok_or(message)
                                            }
                                            None => Err(message),
                                        }
                                    }
                                };
                                match saved {
                                    Ok(path) => events.emit(BackendEvent::MediaSaved {
                                        chat_jid,
                                        message_id,
                                        path: path.to_string_lossy().into_owned(),
                                    }),
                                    Err(e) => {
                                        events.emit(BackendEvent::Error { message: e });
                                    }
                                }
                            });
                        }
                        None => events.emit(BackendEvent::Error {
                            message: "Only media received during this session can be \
                                          downloaded."
                                .to_owned(),
                        }),
                    }
                }
                BackendCommand::FetchPhoto { chat_jid, message_id, photo_ref } => {
                    // Display fetch, in memory only (see the
                    // command's docs). Resolution order: the
                    // session's protocol payload first (it is the
                    // message as received, wrappers and all), then
                    // the archived reference the caller archived
                    // with the message. Like the download, the
                    // transfer runs as its own task so the dispatch
                    // loop never waits behind the network.
                    let proto = lock_cache(&proto_cache)
                        .0
                        .get(&(chat_jid.clone(), message_id.clone()))
                        .cloned();
                    let client = client.clone();
                    let events = events.clone();
                    let targets = retry_for_commands.clone();
                    tokio::spawn(async move {
                        let fetched = match (&proto, &photo_ref) {
                            (Some(proto), _) => fetch_photo_bytes(&client, proto).await,
                            (None, Some(reference)) => fetch_photo_by_ref(&client, reference).await,
                            (None, None) => Err("That photo is not available: it was received \
                                                 before this session and has no stored \
                                                 download reference."
                                .to_owned()),
                        };
                        // A failed download gets one media-retry
                        // recovery: WhatsApp media objects expire
                        // from the CDN, history photos above all,
                        // and the author's device can re-upload.
                        let fetched = match fetched {
                            Ok(bytes) => Ok(bytes),
                            Err(message) => {
                                let fields = proto
                                    .as_ref()
                                    .and_then(|p| super::util::media_fields_of(p))
                                    .or_else(|| photo_ref.as_ref().map(super::util::photo_fields));
                                match fields {
                                    Some(fields) => retry_fetch_bytes(
                                        &client,
                                        &events,
                                        &targets,
                                        &chat_jid,
                                        &message_id,
                                        &fields,
                                    )
                                    .await
                                    .ok_or(message),
                                    None => Err(message),
                                }
                            }
                        };
                        match fetched {
                            Ok(bytes) => events.emit(BackendEvent::PhotoBytes {
                                chat_jid,
                                message_id,
                                bytes,
                            }),
                            Err(message) => events.emit(BackendEvent::PhotoFetchFailed {
                                chat_jid,
                                message_id,
                                message,
                            }),
                        }
                    });
                }
                BackendCommand::FetchVoice { chat_jid, message_id, voice_ref } => {
                    // Playback fetch, in memory only (see the
                    // command's docs). Resolution order mirrors
                    // FetchPhoto: the session's protocol payload
                    // first, then the archived reference. The
                    // transfer runs as its own task so the dispatch
                    // loop never waits behind the network.
                    let proto = lock_cache(&proto_cache)
                        .0
                        .get(&(chat_jid.clone(), message_id.clone()))
                        .cloned();
                    let client = client.clone();
                    let events = events.clone();
                    let targets = retry_for_commands.clone();
                    tokio::spawn(async move {
                        let fetched = match (&proto, &voice_ref) {
                            (Some(proto), _) => fetch_voice_bytes(&client, proto).await,
                            (None, Some(reference)) => fetch_voice_by_ref(&client, reference).await,
                            (None, None) => {
                                Err("That voice note is not available: it was received \
                                                 before this session and has no stored \
                                                 download reference."
                                    .to_owned())
                            }
                        };
                        // One media-retry recovery on failure,
                        // same as FetchPhoto.
                        let fetched = match fetched {
                            Ok(bytes) => Ok(bytes),
                            Err(message) => {
                                let fields = proto
                                    .as_ref()
                                    .and_then(|p| super::util::media_fields_of(p))
                                    .or_else(|| voice_ref.as_ref().map(super::util::voice_fields));
                                match fields {
                                    Some(fields) => retry_fetch_bytes(
                                        &client,
                                        &events,
                                        &targets,
                                        &chat_jid,
                                        &message_id,
                                        &fields,
                                    )
                                    .await
                                    .ok_or(message),
                                    None => Err(message),
                                }
                            }
                        };
                        match fetched {
                            Ok(bytes) => events.emit(BackendEvent::VoiceBytes {
                                chat_jid,
                                message_id,
                                bytes,
                            }),
                            Err(message) => events.emit(BackendEvent::VoiceFetchFailed {
                                chat_jid,
                                message_id,
                                message,
                            }),
                        }
                    });
                }
                BackendCommand::FetchVideo { chat_jid, message_id, video_ref, view_once } => {
                    // Playback fetch (see the command's docs).
                    // Resolution order mirrors FetchPhoto: the
                    // session's protocol payload first, then the
                    // archived reference. The view-once flag picks
                    // the discipline: view-once bytes travel in
                    // memory only, regular videos stream into the
                    // video cache folder. The transfer runs as its
                    // own task so the dispatch loop never waits
                    // behind the network.
                    let proto = lock_cache(&proto_cache)
                        .0
                        .get(&(chat_jid.clone(), message_id.clone()))
                        .cloned();
                    let client = client.clone();
                    let events = events.clone();
                    let targets = retry_for_commands.clone();
                    let dir = config.data_dir.join("video-cache");
                    // The stem follows the same naming contract
                    // as media downloads (see FetchPhoto's sibling
                    // DownloadMedia arm), so the app's bookkeeping
                    // can name the cached file without a second
                    // scheme.
                    let stem = crate::downloaded_media_stem(&chat_jid, &message_id);
                    tokio::spawn(async move {
                        if view_once {
                            let fetched = match (&proto, &video_ref) {
                                (Some(proto), _) => fetch_video_bytes(&client, proto).await,
                                (None, Some(reference)) => {
                                    fetch_video_by_ref(&client, reference).await
                                }
                                (None, None) => {
                                    Err("That video is not available: it was received \
                                                     before this session and has no stored \
                                                     download reference."
                                        .to_owned())
                                }
                            };
                            // One media-retry recovery on failure,
                            // same as FetchPhoto.
                            let fetched = match fetched {
                                Ok(bytes) => Ok(bytes),
                                Err(message) => {
                                    let fields = proto
                                        .as_ref()
                                        .and_then(|p| super::util::media_fields_of(p))
                                        .or_else(|| {
                                            video_ref.as_ref().map(super::util::video_fields)
                                        });
                                    match fields {
                                        Some(fields) => retry_fetch_bytes(
                                            &client,
                                            &events,
                                            &targets,
                                            &chat_jid,
                                            &message_id,
                                            &fields,
                                        )
                                        .await
                                        .ok_or(message),
                                        None => Err(message),
                                    }
                                }
                            };
                            match fetched {
                                Ok(bytes) => events.emit(BackendEvent::VideoBytes {
                                    chat_jid,
                                    message_id,
                                    bytes,
                                }),
                                Err(message) => events.emit(BackendEvent::VideoFetchFailed {
                                    chat_jid,
                                    message_id,
                                    message,
                                }),
                            }
                        } else {
                            let fetched = match (&proto, &video_ref) {
                                (Some(proto), _) => {
                                    fetch_video_to_file(&client, proto, &dir, &stem).await
                                }
                                (None, Some(reference)) => {
                                    fetch_video_file_by_ref(&client, reference, &dir, &stem).await
                                }
                                (None, None) => {
                                    Err("That video is not available: it was received \
                                                     before this session and has no stored \
                                                     download reference."
                                        .to_owned())
                                }
                            };
                            // One media-retry recovery on failure,
                            // same as FetchPhoto, streaming into
                            // the same cache file name.
                            let fetched = match fetched {
                                Ok(path) => Ok(path),
                                Err(message) => {
                                    let fields = proto
                                        .as_ref()
                                        .and_then(|p| super::util::media_fields_of(p))
                                        .or_else(|| {
                                            video_ref.as_ref().map(super::util::video_fields)
                                        });
                                    match fields {
                                        Some(fields) => retry_download_file(
                                            &client,
                                            &events,
                                            &targets,
                                            &chat_jid,
                                            &message_id,
                                            &fields,
                                            &dir,
                                            &stem,
                                            "mp4",
                                        )
                                        .await
                                        .ok_or(message),
                                        None => Err(message),
                                    }
                                }
                            };
                            match fetched {
                                Ok(path) => events.emit(BackendEvent::VideoReady {
                                    chat_jid,
                                    message_id,
                                    path: path.to_string_lossy().into_owned(),
                                }),
                                Err(message) => events.emit(BackendEvent::VideoFetchFailed {
                                    chat_jid,
                                    message_id,
                                    message,
                                }),
                            }
                        }
                    });
                }
                BackendCommand::FetchSticker { chat_jid, message_id, sticker_ref } => {
                    // Display fetch, in memory only, mirroring
                    // FetchPhoto: the session's protocol payload
                    // first, then the archived reference, with
                    // one media-retry recovery on failure.
                    let proto = lock_cache(&proto_cache)
                        .0
                        .get(&(chat_jid.clone(), message_id.clone()))
                        .cloned();
                    let client = client.clone();
                    let events = events.clone();
                    let targets = retry_for_commands.clone();
                    tokio::spawn(async move {
                        let fetched = match (&proto, &sticker_ref) {
                            (Some(proto), _) => {
                                super::util::fetch_sticker_bytes(&client, proto).await
                            }
                            (None, Some(reference)) => {
                                super::util::fetch_sticker_by_ref(&client, reference).await
                            }
                            (None, None) => Err("That sticker is not available: it was received \
                                                 before this session and has no stored \
                                                 download reference."
                                .to_owned()),
                        };
                        let fetched = match fetched {
                            Ok(bytes) => Ok(bytes),
                            Err(message) => {
                                let fields = proto
                                    .as_ref()
                                    .and_then(|p| super::util::media_fields_of(p))
                                    .or_else(|| {
                                        sticker_ref.as_ref().map(super::util::sticker_fields)
                                    });
                                match fields {
                                    Some(fields) => retry_fetch_bytes(
                                        &client,
                                        &events,
                                        &targets,
                                        &chat_jid,
                                        &message_id,
                                        &fields,
                                    )
                                    .await
                                    .ok_or(message),
                                    None => Err(message),
                                }
                            }
                        };
                        match fetched {
                            Ok(bytes) => events.emit(BackendEvent::StickerBytes {
                                chat_jid,
                                message_id,
                                bytes,
                            }),
                            Err(_) => events
                                .emit(BackendEvent::StickerFetchFailed { chat_jid, message_id }),
                        }
                    });
                }
                BackendCommand::FetchAvatar { jid } => {
                    // Profile picture lookup (see the command's
                    // docs). Runs as its own task: the lookup and
                    // the CDN download take network time, and the
                    // dispatch loop must not wait behind them.
                    let client = client.clone();
                    let events = events.clone();
                    tokio::spawn(async move {
                        use super::profile::AvatarOutcome;
                        match super::profile::fetch_avatar(&client, &jid).await {
                            AvatarOutcome::Bytes { picture_id, bytes } => {
                                events.emit(BackendEvent::AvatarBytes { jid, picture_id, bytes });
                            }
                            AvatarOutcome::Missing => {
                                events.emit(BackendEvent::AvatarMissing { jid });
                            }
                            AvatarOutcome::Unavailable => {}
                        }
                    });
                }
                BackendCommand::ResolveTitles { jids } => {
                    heal_titles_with_retry(client.clone(), events.clone(), jids);
                }
                BackendCommand::FetchOlderMessages {
                    chat_jid,
                    oldest_message_id,
                    oldest_from_me,
                    oldest_timestamp_ms,
                } => {
                    // On-demand history (see the command's docs).
                    // Runs as its own task: the send and the
                    // watchdog take time, and the dispatch loop
                    // must not wait behind them. The pending
                    // entry is recorded BEFORE the request goes
                    // out, because the phone's answer can arrive
                    // before the send completes.
                    match chat_jid.parse::<Jid>() {
                        Ok(jid) => {
                            let request_id = client.generate_message_id();
                            let session_id = request_id.as_str().to_owned();
                            pending_history_commands
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .insert(session_id.clone(), chat_jid.clone());
                            let client = client.clone();
                            let events = events.clone();
                            let pending = pending_history_commands.clone();
                            tokio::spawn(async move {
                                let sent = client
                                    .fetch_message_history_with_id(
                                        &jid,
                                        &oldest_message_id,
                                        oldest_from_me,
                                        oldest_timestamp_ms,
                                        HISTORY_PAGE_SIZE,
                                        &request_id,
                                    )
                                    .await;
                                if let Err(e) = sent {
                                    pending
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                                        .remove(&session_id);
                                    events.emit(BackendEvent::HistoryFetchFailed {
                                        chat_jid,
                                        message: format!(
                                            "Asking the phone for older messages failed: {e}"
                                        ),
                                    });
                                    return;
                                }
                                // The watchdog: the library's
                                // fetch has no timeout, so a phone
                                // that never answers is closed
                                // here as failed, never left
                                // latched. A late answer still
                                // files its messages through the
                                // normal path; it just finds no
                                // pending entry to complete.
                                tokio::time::sleep(HISTORY_FETCH_TIMEOUT).await;
                                let unanswered = pending
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                                    .remove(&session_id)
                                    .is_some();
                                if unanswered {
                                    events.emit(BackendEvent::HistoryFetchFailed {
                                        chat_jid,
                                        message: "The phone did not answer the request \
                                                  for older messages."
                                            .to_owned(),
                                    });
                                }
                            });
                        }
                        Err(e) => events.emit(BackendEvent::HistoryFetchFailed {
                            chat_jid,
                            message: format!("Invalid chat address: {e}"),
                        }),
                    }
                }
                BackendCommand::SetGroupPicture { chat_jid, image } => {
                    // Group picture write (see the command's
                    // docs). Runs as its own task like the other
                    // picture operations.
                    let client = client.clone();
                    let events = events.clone();
                    tokio::spawn(async move {
                        match super::profile::set_group_picture(&client, &chat_jid, image).await {
                            Ok(()) => {
                                events.emit(BackendEvent::GroupPictureChanged { chat_jid });
                            }
                            Err(message) => {
                                events.emit(BackendEvent::GroupPictureFailed { chat_jid, message });
                            }
                        }
                    });
                }
                BackendCommand::LookupContact { phone } => {
                    // Number lookup (see the command's docs).
                    // Runs as its own task: the usync round trip
                    // takes network time.
                    let client = client.clone();
                    let events = events.clone();
                    tokio::spawn(async move {
                        use super::profile::ContactOutcome;
                        match crate::phone_digits(&phone) {
                            None => events.emit(BackendEvent::ContactLookupFailed {
                                phone,
                                message: "That does not look like a phone number: \
                                          use the full international format, digits only."
                                    .to_owned(),
                            }),
                            Some(digits) => {
                                match super::profile::lookup_contact(&client, &digits).await {
                                    ContactOutcome::Resolved { chat, alt_jid } => {
                                        events
                                            .emit(BackendEvent::ContactResolved { chat, alt_jid });
                                    }
                                    ContactOutcome::NotFound => {
                                        events.emit(BackendEvent::ContactNotFound { phone });
                                    }
                                    ContactOutcome::Failed(message) => {
                                        events.emit(BackendEvent::ContactLookupFailed {
                                            phone,
                                            message,
                                        });
                                    }
                                }
                            }
                        }
                    });
                }
                BackendCommand::SetPrivacy { category, value } => {
                    use whatsapp_rust::privacy_settings::{PrivacyCategory, PrivacyValue};
                    let (category_name, value_name) = (category.clone(), value.clone());
                    let category = match category.as_str() {
                        "last" => Some(PrivacyCategory::Last),
                        "online" => Some(PrivacyCategory::Online),
                        "profile" => Some(PrivacyCategory::Profile),
                        "status" => Some(PrivacyCategory::Status),
                        "groupadd" => Some(PrivacyCategory::GroupAdd),
                        "readreceipts" => Some(PrivacyCategory::ReadReceipts),
                        "calladd" => Some(PrivacyCategory::CallAdd),
                        "messages" => Some(PrivacyCategory::Messages),
                        "defense" => Some(PrivacyCategory::DefenseMode),
                        _ => None,
                    };
                    let value = match value.as_str() {
                        "all" => Some(PrivacyValue::All),
                        "contacts" => Some(PrivacyValue::Contacts),
                        "none" => Some(PrivacyValue::None),
                        "contact_blacklist" => Some(PrivacyValue::ContactBlacklist),
                        "match_last_seen" => Some(PrivacyValue::MatchLastSeen),
                        "known" => Some(PrivacyValue::Known),
                        "off" => Some(PrivacyValue::Off),
                        "on_standard" => Some(PrivacyValue::OnStandard),
                        _ => None,
                    };
                    // The pair is validated here, at the boundary,
                    // before anything is sent. The library only
                    // debug-asserts that a value fits its category,
                    // so an invalid pair (e.g. `contacts` for
                    // `online`, which accepts only `all` and
                    // `match_last_seen`) would otherwise go out and
                    // fail at the server. And `contact_blacklist`,
                    // though a valid value for its categories,
                    // names a per-contact exception list this
                    // command cannot carry: sending it bare would
                    // discard the list the phone configured.
                    let refusal = match (&category, &value) {
                        (Some(_), Some(PrivacyValue::ContactBlacklist)) => Some(
                            "Privacy change failed: an exception list cannot be set without its contacts."
                                .to_owned(),
                        ),
                        (Some(c), Some(v)) if !c.is_valid_value(v) => Some(format!(
                            "Privacy change failed: {category_name} does not accept {value_name}."
                        )),
                        _ => None,
                    };
                    match refusal {
                        Some(message) => events.emit(BackendEvent::Error { message }),
                        None => match (category, value) {
                            (Some(c), Some(v)) => match client.set_privacy_setting(c, v).await {
                                Ok(_) => {
                                    if let Ok(resp) = client.fetch_privacy_settings().await {
                                        events.emit(BackendEvent::PrivacyState(privacy_map(&resp)));
                                    }
                                }
                                Err(e) => {
                                    events.emit(BackendEvent::Error {
                                        message: format!("Privacy change failed: {e}"),
                                    });
                                    // Re-assert the account's actual
                                    // settings after the failure: the
                                    // app applies privacy changes
                                    // optimistically, and this fetch is
                                    // what reconciles its state with
                                    // the value the server kept.
                                    if let Ok(resp) = client.fetch_privacy_settings().await {
                                        events.emit(BackendEvent::PrivacyState(privacy_map(&resp)));
                                    }
                                }
                            },
                            _ => events.emit(BackendEvent::Error {
                                message: "Unknown privacy category or value.".to_owned(),
                            }),
                        },
                    }
                }
                BackendCommand::FetchOwnProfile => {
                    // Answered by a fresh read-back on the profile
                    // fetch slot, so a fetch already in flight from
                    // the connect is replaced rather than doubled.
                    let sink_profile = events.clone();
                    let client_profile = client.clone();
                    spawn_latest(&own_profile_fetch, async move {
                        let profile = super::profile::fetch_own_profile(&client_profile).await;
                        sink_profile.emit(BackendEvent::OwnProfile(profile));
                    });
                }
                BackendCommand::UpdateOwnProfile { name, about, photo } => {
                    // Applied inline, like SetPrivacy: one save is
                    // one outcome. Either way the profile is read
                    // back afterwards, so the editor reconciles
                    // with the state the server actually kept; a
                    // failure is reported first, in user words.
                    match super::profile::update_own_profile(&client, name, about, photo).await {
                        Ok(()) => {}
                        Err(message) => {
                            events.emit(BackendEvent::OwnProfileSaveFailed { message });
                        }
                    }
                    let profile = super::profile::fetch_own_profile(&client).await;
                    events.emit(BackendEvent::OwnProfile(profile));
                }
                BackendCommand::Disconnect => {
                    // The local teardown always happens inside
                    // logout; what can silently fail is the
                    // deregistration reaching the server. Surface
                    // that, or the user believes they logged out
                    // while the phone still lists this device as
                    // linked. The loop's own ending is unchanged:
                    // termination still arrives via the run loop's
                    // outcome, as it does for a clean logout.
                    let report = client.logout().await;
                    // This device is unlinked from this side,
                    // whatever the server answered (an offline
                    // logout is reported below, but the user's
                    // intent and the local teardown stand): the
                    // caller wipes the account directory once the
                    // session has fully ended (privacy audit A8).
                    logged_out.store(true, std::sync::atomic::Ordering::SeqCst);
                    if let Some(problem) = logout_problem(&report.deregistration) {
                        events.emit(BackendEvent::Error { message: problem });
                    }
                }
                // Shutdown never reaches the handler: the
                // dispatch loop intercepts it while obtaining the
                // command, and `await_handler` intercepts it while
                // a handler runs. If that invariant ever breaks,
                // failing loudly (the supervisor contains the
                // panic and restarts) beats silently ignoring a
                // shutdown request.
                BackendCommand::Shutdown => {
                    unreachable!("Shutdown is intercepted before dispatch")
                }
            }
        };
        match await_handler(handler, &mut cmd_rx, &mut pending, &mut run_handle).await {
            HandlerEnd::Finished => {}
            HandlerEnd::Shutdown => break 'dispatch,
            HandlerEnd::RunEnded(outcome) => {
                run_outcome = Some(outcome);
                break 'dispatch;
            }
        }
    }

    // A privacy fetch still in flight belongs to this session: stop
    // it so it cannot emit PrivacyState after the session has ended.
    // (The runtime drop at the end of `Backend::run` would abort it
    // anyway; doing it here keeps the guarantee local.)
    abort_latest(&privacy_fetch);
    // Same for a blocklist fetch: it must not land after the
    // session it was fetched for has ended.
    abort_latest(&blocklist_fetch);
    // Same for an own-profile fetch.
    abort_latest(&own_profile_fetch);

    match run_outcome {
        Some(outcome) => {
            tracing::info!("session run ended: {outcome:?}");
        }
        None => {
            // The UI asked to stop while the bot was still running. Shut
            // it down gracefully so the library flushes the device
            // snapshot, buffered receipts and message secrets instead of
            // dropping them the way an abort would. The flush talks to
            // the network, so it gets a budget (SHUTDOWN_FLUSH_TIMEOUT):
            // on a dead connection the session ends anyway once the
            // budget expires, and whatever the flush could not deliver
            // is the same state an abrupt exit would have lost.
            match tokio::time::timeout(SHUTDOWN_FLUSH_TIMEOUT, run_handle.shutdown()).await {
                Ok(report) => {
                    if let Err(e) = &report.shutdown.device {
                        tracing::warn!("flushing the session snapshot on shutdown failed: {e}");
                    }
                    if report.shutdown.message_secrets.failed_batches > 0 {
                        tracing::warn!(
                            "flushing message secrets on shutdown failed for {} batches",
                            report.shutdown.message_secrets.failed_batches
                        );
                    }
                    tracing::info!("session shut down: {:?}", report.run);
                }
                Err(_) => {
                    tracing::warn!(
                        "the shutdown flush did not finish within {} seconds; \
                         ending the session anyway",
                        SHUTDOWN_FLUSH_TIMEOUT.as_secs()
                    );
                }
            }
        }
    }
    // Reaching here means the run ended on purpose: the UI sent
    // Shutdown, the command channel closed, or the bot run loop
    // ended on its own (logout, a stopped connection). All are
    // clean exits the supervisor must not restart.
    Ok(())
}

/// Removes a logged-out account's session directory.
///
/// A logout unlinks the device, but unlinking is protocol-level:
/// without this, the encrypted session store (the device
/// identity keys and protocol state) would sit in
/// `accounts/<id>/` forever, an identity a later local attacker
/// or a copied directory could still pick up (privacy audit
/// A8). The chat archive is a separate, deliberate local store
/// at the data-dir root and is NOT removed: logging out ends
/// the link, it does not erase the user's saved chats, which
/// stay readable (and deletable) as a local archive.
///
/// The caller runs this only after the session has fully ended
/// (the runtime is down, the store's connections are closed),
/// so nothing can recreate the directory behind the removal.
/// Best-effort: a missing directory is success, and any other
/// failure is logged and left for a future logout to retry.
pub(super) fn remove_account_dir_after_logout(account_dir: &std::path::Path) {
    match std::fs::remove_dir_all(account_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(
                "removing the logged-out account folder {} failed: {e}",
                account_dir.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    fn run_test(future: impl std::future::Future<Output = ()>) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime builds")
            .block_on(future);
    }

    /// The post-logout wipe (privacy audit A8): the account's
    /// session directory, device identity included, is removed,
    /// and the chat archive at the data-dir root, a separate
    /// local store the logout does not own, is untouched.
    #[test]
    fn logout_wipe_removes_the_session_dir_and_keeps_the_archive() {
        let root =
            std::env::temp_dir().join(format!("zaprust-logout-wipe-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let account = root.join("accounts").join("default");
        std::fs::create_dir_all(&account).expect("account dir");
        std::fs::write(account.join("session.db"), b"device identity").expect("session store");
        std::fs::write(root.join("archive.db"), b"chat archive").expect("archive");
        remove_account_dir_after_logout(&account);
        assert!(!account.exists(), "the session directory is gone after logout");
        assert_eq!(
            std::fs::read(root.join("archive.db")).expect("archive survives"),
            b"chat archive",
            "the archive is a separate store and stays"
        );
        // A second wipe over a missing directory is not an error.
        remove_account_dir_after_logout(&account);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A server-confirmed logout is the clean case: nothing to
    /// tell the user.
    #[test]
    fn confirmed_logout_reports_no_problem() {
        use whatsapp_rust::DeregistrationOutcome;
        assert!(logout_problem(&DeregistrationOutcome::Confirmed).is_none());
    }

    /// A logout that never reached the server (the socket was
    /// down) must be reported: the phone still lists this device
    /// as linked, and silence would read as a completed logout.
    #[test]
    fn offline_logout_reports_a_problem() {
        use whatsapp_rust::{DeregistrationOutcome, DeregistrationSkipReason};
        let outcome = DeregistrationOutcome::NotAttempted(DeregistrationSkipReason::Offline);
        let problem = logout_problem(&outcome).expect("offline logout is reported");
        assert!(problem.contains("still appear linked"), "honest wording: {problem}");
    }

    /// A handler that announces it started, then sleeps far past
    /// every test budget: the stand-in for a slow network call
    /// occupying the dispatch loop.
    async fn slow_handler(
        started: std::sync::Arc<AtomicBool>,
        finished: std::sync::Arc<AtomicBool>,
    ) {
        started.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(30)).await;
        finished.store(true, Ordering::SeqCst);
    }

    /// Runs `await_handler` over a slow handler on its own task,
    /// resolving to how the wait ended.
    fn spawn_wait(
        started: std::sync::Arc<AtomicBool>,
        finished: std::sync::Arc<AtomicBool>,
    ) -> (tokio::sync::mpsc::Sender<BackendCommand>, tokio::task::JoinHandle<&'static str>) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BackendCommand>(8);
        let handle = tokio::spawn(async move {
            let mut pending: VecDeque<BackendCommand> = VecDeque::new();
            let mut run = std::future::pending::<()>();
            match await_handler(slow_handler(started, finished), &mut rx, &mut pending, &mut run)
                .await
            {
                HandlerEnd::Finished => "finished",
                HandlerEnd::Shutdown => "shutdown",
                HandlerEnd::RunEnded(()) => "run ended",
            }
        });
        (tx, handle)
    }

    async fn wait_until_started(started: &AtomicBool) {
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    }

    /// The core guarantee of the dispatch contract: Shutdown does
    /// not queue behind a slow in-flight command. It drops the
    /// handler where it sleeps and ends the wait at once.
    #[test]
    fn shutdown_preempts_a_slow_handler() {
        let started = std::sync::Arc::new(AtomicBool::new(false));
        let finished = std::sync::Arc::new(AtomicBool::new(false));
        let started_wait = started.clone();
        let finished_wait = finished.clone();
        run_test(async move {
            let (tx, waiting) = spawn_wait(started_wait, finished_wait);
            wait_until_started(&started).await;
            tx.send(BackendCommand::Shutdown).await.expect("channel open");
            let end = tokio::time::timeout(Duration::from_secs(5), waiting)
                .await
                .expect("shutdown must preempt promptly, not after the handler")
                .expect("the waiting task did not panic");
            assert_eq!(end, "shutdown");
            assert!(!finished.load(Ordering::SeqCst), "the handler never completed");
        });
    }

    /// A closed command channel means the UI is gone: the same
    /// preemption as Shutdown, with nothing ever sent.
    #[test]
    fn channel_closure_preempts_like_shutdown() {
        let started = std::sync::Arc::new(AtomicBool::new(false));
        let finished = std::sync::Arc::new(AtomicBool::new(false));
        let started_wait = started.clone();
        let finished_wait = finished.clone();
        run_test(async move {
            let (tx, waiting) = spawn_wait(started_wait, finished_wait);
            wait_until_started(&started).await;
            drop(tx);
            let end = tokio::time::timeout(Duration::from_secs(5), waiting)
                .await
                .expect("closure must preempt promptly, not after the handler")
                .expect("the waiting task did not panic");
            assert_eq!(end, "shutdown");
            assert!(!finished.load(Ordering::SeqCst), "the handler never completed");
        });
    }

    /// The bot run loop ending on its own (logout, a stopped
    /// connection) also preempts: the session is over, so the
    /// in-flight command's result no longer matters.
    #[test]
    fn run_end_preempts_a_slow_handler() {
        let started = std::sync::Arc::new(AtomicBool::new(false));
        let finished = std::sync::Arc::new(AtomicBool::new(false));
        let started_in = started.clone();
        let finished_in = finished.clone();
        run_test(async move {
            let (_tx, mut rx) = tokio::sync::mpsc::channel::<BackendCommand>(8);
            let mut pending: VecDeque<BackendCommand> = VecDeque::new();
            // The run is already over by the time the wait starts.
            let mut run = std::future::ready(7_u32);
            let end = tokio::time::timeout(
                Duration::from_secs(5),
                await_handler(
                    slow_handler(started_in, finished_in),
                    &mut rx,
                    &mut pending,
                    &mut run,
                ),
            )
            .await
            .expect("the run outcome must preempt promptly");
            assert!(matches!(end, HandlerEnd::RunEnded(7)));
            assert!(started.load(Ordering::SeqCst), "the handler was in flight");
            assert!(!finished.load(Ordering::SeqCst), "the handler never completed");
        });
    }

    /// Commands that arrive while a handler runs are not lost and
    /// not reordered: they queue into `pending`, in arrival order,
    /// and the handler still runs to completion.
    #[test]
    fn commands_arriving_mid_handler_queue_in_order() {
        run_test(async move {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<BackendCommand>(8);
            tx.send(BackendCommand::ConnectQr).await.expect("channel open");
            tx.send(BackendCommand::ConnectPairCode { phone: "123".to_owned() })
                .await
                .expect("channel open");
            let mut pending: VecDeque<BackendCommand> = VecDeque::new();
            let mut run = std::future::pending::<()>();
            let handler = async {
                tokio::time::sleep(Duration::from_millis(50)).await;
            };
            let end = await_handler(handler, &mut rx, &mut pending, &mut run).await;
            assert!(matches!(end, HandlerEnd::Finished));
            assert_eq!(
                pending.into_iter().collect::<Vec<_>>(),
                vec![
                    BackendCommand::ConnectQr,
                    BackendCommand::ConnectPairCode { phone: "123".to_owned() },
                ]
            );
        });
    }
}
