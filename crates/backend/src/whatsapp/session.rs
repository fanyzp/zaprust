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
    outgoing_location_message, outgoing_text_message, send_media, stamp_ephemeral, OutgoingMedia,
};
use super::translate::{
    privacy_map, translate_event, translate_message, translate_special, user_part,
};
use super::util::{
    abort_latest, build_message_target, cap_poll_secrets, download_proto, lock_cache,
    parse_address, received_poll_secret, sanitize_id, spawn_latest, unix_now, LatestTask,
    PollSecrets, ProtoCache,
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

pub(super) async fn run_async(
    config: &BackendConfig,
    commands: Receiver<BackendCommand>,
    events: EventSink,
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

    let sink = events.clone();
    let sink_pair = events.clone();
    let sink_conn = events.clone();
    let sink_out = events.clone();
    let sink_msg = events.clone();
    let sink_all = events.clone();

    let bot = match Bot::builder()
        .with_backend(store)
        // Arrival order matters here: the callbacks below upsert
        // messages and apply their receipts, and a receipt that
        // overtakes its upsert lands on a message that does not
        // exist yet. The library default runs callbacks concurrently
        // and drops events when its smaller queue fills, so ordered
        // delivery is requested explicitly.
        .with_event_delivery(EventDelivery::Ordered { capacity: EVENT_QUEUE_CAPACITY })
        .on_qr_code(move |code, _timeout| {
            let sink = sink.clone();
            async move {
                sink.emit(BackendEvent::QrCode { payload: code });
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
                spawn_latest(&fetch_slot, async move {
                    if let Ok(resp) = client.fetch_privacy_settings().await {
                        sink.emit(BackendEvent::PrivacyState(privacy_map(&resp)));
                    }
                });
            }
        })
        .on_logged_out(move |_info| {
            let sink = sink_out.clone();
            async move {
                sink.emit(BackendEvent::LoggedOut);
            }
        })
        .on_message(move |ctx| {
            let sink = sink_msg.clone();
            let cache = cache_for_messages.clone();
            let secrets = secrets_for_messages.clone();
            async move {
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
                    sink.emit(BackendEvent::ChatUpsert(chat));
                    sink.emit(BackendEvent::MessageUpsert(msg));
                }
            }
        })
        .on_event(move |event: Arc<Event>, _client| {
            let sink = sink_all.clone();
            #[cfg(feature = "calls")]
            let incoming_slot = call_incoming_events.clone();
            async move {
                #[cfg(feature = "calls")]
                handle_call_event(&event, &incoming_slot, &sink);
                translate_event(&event, &sink);
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
                BackendCommand::VotePoll { chat_jid, message_id, options } => {
                    // Voting needs the poll's creation secret.
                    // It is cached at creation for own polls and
                    // on receipt for other people's polls (the
                    // secret travels inside the creation message,
                    // see docs/research/poll-voting.md). A poll
                    // whose secret this session never saw gets an
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
                                          creation secret was not in the message data this \
                                          session received, so the poll predates this session \
                                          or the sender's client did not include it. Secrets \
                                          are not kept across restarts."
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
                                        events.emit(BackendEvent::Error {
                                            message: format!("Reaction failed: {e}"),
                                        });
                                    }
                                }
                                Err(e) => events.emit(BackendEvent::Error {
                                    message: format!("Reaction target invalid: {e}"),
                                }),
                            }
                        }
                        Err(e) => events.emit(BackendEvent::Error {
                            message: format!("Invalid chat address: {e}"),
                        }),
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
                                        events.emit(BackendEvent::Error {
                                            message: format!("Starring failed: {e}"),
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
                BackendCommand::PinMessage { chat_jid, message_id, sender_jid, from_me, pin } => {
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
                                        events.emit(BackendEvent::Error {
                                            message: format!("Pinning the message failed: {e}"),
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
                                .map(|p| {
                                    (p.jid.to_string(), user_part(&p.jid.to_string()), p.is_admin())
                                })
                                .collect();
                            events.emit(BackendEvent::GroupMembers {
                                chat_jid: chat_jid.clone(),
                                members,
                            });
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
                            let requests = list
                                .iter()
                                .map(|r| {
                                    let jid = r.jid.to_string();
                                    let name = user_part(&jid);
                                    (jid, name)
                                })
                                .collect();
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
                                        let requests = list
                                            .iter()
                                            .map(|r| {
                                                let j = r.jid.to_string();
                                                let name = user_part(&j);
                                                (j, name)
                                            })
                                            .collect();
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
                            let dir = config.data_dir.join("downloads");
                            let stem =
                                format!("{}-{}", user_part(&chat_jid), sanitize_id(&message_id));
                            tokio::spawn(async move {
                                match download_proto(&client, &proto, &dir, &stem).await {
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
                BackendCommand::SetPrivacy { category, value } => {
                    use whatsapp_rust::privacy_settings::{PrivacyCategory, PrivacyValue};
                    let category = match category.as_str() {
                        "last" => Some(PrivacyCategory::Last),
                        "online" => Some(PrivacyCategory::Online),
                        "profile" => Some(PrivacyCategory::Profile),
                        "status" => Some(PrivacyCategory::Status),
                        "groupadd" => Some(PrivacyCategory::GroupAdd),
                        "readreceipts" => Some(PrivacyCategory::ReadReceipts),
                        _ => None,
                    };
                    let value = match value.as_str() {
                        "all" => Some(PrivacyValue::All),
                        "contacts" => Some(PrivacyValue::Contacts),
                        "none" => Some(PrivacyValue::None),
                        "contact_blacklist" => Some(PrivacyValue::ContactBlacklist),
                        "match_last_seen" => Some(PrivacyValue::MatchLastSeen),
                        "known" => Some(PrivacyValue::Known),
                        _ => None,
                    };
                    match (category, value) {
                        (Some(c), Some(v)) => match client.set_privacy_setting(c, v).await {
                            Ok(_) => {
                                if let Ok(resp) = client.fetch_privacy_settings().await {
                                    events.emit(BackendEvent::PrivacyState(privacy_map(&resp)));
                                }
                            }
                            Err(e) => events.emit(BackendEvent::Error {
                                message: format!("Privacy change failed: {e}"),
                            }),
                        },
                        _ => events.emit(BackendEvent::Error {
                            message: "Unknown privacy category or value.".to_owned(),
                        }),
                    }
                }
                BackendCommand::Disconnect => {
                    let _ = client.logout().await;
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
