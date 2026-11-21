//! Live mode: the bridge between the `backend` crate and the UI.
//!
//! In live mode the chat list and conversation are fed by backend
//! events (and, on start, by the `store` archive) instead of the demo
//! dataset. Everything protocol-shaped stays in this module: JID →
//! numeric ID mapping, unix timestamps → display times, and the
//! write-through of every upsert into the archive.

use std::collections::HashMap;

use backend::{
    spawn_backend, Backend, BackendChannels, BackendCommand, BackendConfig, BackendEvent,
    FakeBackend, LiveChat, LiveChatKind, LiveMessage, QrMatrix, QuotedRef, ReceiptKind,
    WhatsappBackend,
};
use core::model::{Chat, ChatKind, Delivery, Message, MessageKind};
use eframe::egui;

/// Where the link flow currently stands (drives the connect screen's
/// status line and the account modal).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LinkStatus {
    /// Backend started, nothing heard yet.
    Starting,
    /// A QR payload is on screen, waiting for a scan.
    WaitingQr,
    /// A pairing code is on screen.
    CodeReady,
    /// Linked and connected.
    Linked,
    /// Logged out (phone or server side).
    LoggedOut,
    /// Temporarily banned by the server.
    Banned(u32),
    /// Connection lost; the backend reconnects on its own.
    Lost(String),
    /// The worker crashed (the supervisor restarted it when possible).
    Crashed(String),
    /// A persistent failure state.
    Failed(String),
}

impl LinkStatus {
    /// Short status line for the connect screen.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Starting => "Starting the connection worker…".to_owned(),
            Self::WaitingQr => "Waiting for scan, point your phone at the code.".to_owned(),
            Self::CodeReady => "Code ready, enter it on your phone.".to_owned(),
            Self::Linked => "Linked, this computer is connected.".to_owned(),
            Self::LoggedOut => "Logged out, link again to continue.".to_owned(),
            Self::Banned(code) => {
                format!("This account is temporarily banned (reason {code}).")
            }
            Self::Lost(reason) => reason.clone(),
            Self::Crashed(summary) => format!("Connection worker crashed: {summary}"),
            Self::Failed(message) => message.clone(),
        }
    }

    pub(crate) fn is_linked(&self) -> bool {
        matches!(self, Self::Linked)
    }
}

/// How long a pairing code stays valid. The library does not carry
/// the expiry across the backend boundary yet, so the app tracks it
/// from receipt; WhatsApp codes live for roughly three minutes.
pub(crate) const PAIR_CODE_TTL: std::time::Duration = std::time::Duration::from_secs(180);

/// Per-frame budget for applying backend events. A first link or a
/// reconnect can queue thousands of upserts at once; applying all
/// of them in one frame froze the UI for seconds to minutes. One
/// batch applies at most this much wall time (and never more than
/// [`MAX_EVENTS_PER_FRAME`] events), then the rest of the queue
/// waits for the next frame, which `poll_backend` requests itself
/// while a backlog remains.
const EVENT_FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(8);

/// Hard cap on events applied in one frame, whatever the clock
/// says: the wall-time budget alone would let a burst of trivial
/// events (typing, receipts) run far past a frame's share.
const MAX_EVENTS_PER_FRAME: usize = 256;

/// Side effects deferred while one batch of backend events is
/// being applied, coalesced to once per batch by
/// `App::finish_event_batch`. Without this, every upsert in a
/// burst paid a full sort of its chat's messages, a full sort of
/// the chat list, a full clone of the open conversation and its
/// own archive transaction: quadratic work on the UI thread.
#[derive(Default)]
pub(crate) struct EventBatch {
    /// True between the first and the last event of a batch.
    pub(crate) active: bool,
    /// Chats whose message vec a batch upsert appended to; each is
    /// sorted once when the batch finishes. Order inside the batch
    /// does not matter: every in-batch reader of a chat's messages
    /// (echo matching, receipts, reactions, edits) looks messages
    /// up by ID or by minimum timestamp, never by position.
    pub(crate) unsorted_chats: std::collections::HashSet<u64>,
    /// The chat list needs its one sort for this batch.
    pub(crate) chats_dirty: bool,
    /// The open conversation needs its one rebuild for this batch.
    pub(crate) conversation_dirty: bool,
    /// Message upserts whose archive write is deferred to the
    /// batch flush, which writes them in a single transaction.
    /// Flags are NOT captured here: they are resolved from the
    /// session's flag map at flush time, so a star or revoke that
    /// lands later in the same batch is part of the stored row,
    /// exactly as if each write had happened in event order.
    pub(crate) store_writes: Vec<LiveMessage>,
    /// Chats whose unread badge a batch upsert raised. The
    /// badge lives in the mirror, but the archive row must carry
    /// the same count (a restart reloads from the archive), so
    /// the flush writes these chats' rows from the mirror once
    /// per batch instead of once per message.
    pub(crate) chat_rows_dirty: std::collections::HashSet<u64>,
}

/// One running live session: channels, link state, and the live data
/// mirrors the UI renders.
pub(crate) struct LiveSession {
    pub(crate) channels: BackendChannels,
    pub(crate) status: LinkStatus,
    /// Latest QR matrix to draw, when waiting for a scan.
    pub(crate) qr: Option<QrMatrix>,
    /// When the current QR payload stops being valid, from the
    /// validity window the library reported with it. A payload
    /// past this instant is dead even if nothing else has
    /// happened yet, so the expiry sweep drops it rather than
    /// leaving a dead code on screen (the pairing-code field
    /// below follows the same discipline).
    pub(crate) qr_expires_at: Option<std::time::Instant>,
    /// Latest pairing code, when in code mode.
    pub(crate) pair_code: Option<String>,
    /// When the current pairing code arrived. Pair codes die after
    /// roughly three minutes; the UI must never present a dead code
    /// as live (v0.25 logic review F13).
    pub(crate) pair_code_at: Option<std::time::Instant>,
    /// Account label reported on connect.
    pub(crate) account: String,
    /// Wall-clock unix second at which the current connection
    /// came up, restamped on every (re)connect. A message stamped
    /// before it was sent while this client was offline: it
    /// arrives as history sync or catch-up, not live, and must
    /// never raise an OS notification. `None` until the first
    /// connect completes; nothing is live before that either.
    pub(crate) connected_at: Option<i64>,
    /// The chat archive (write-through). `None` if it failed to open.
    pub(crate) store: Option<store::Store>,
    /// Numeric chat ID → JID.
    pub(crate) jids: HashMap<u64, String>,
    /// Numeric chat ID → (timestamp, message) pairs, time-ordered.
    pub(crate) messages: HashMap<u64, Vec<(i64, Message)>>,
    /// Numeric chat ID → latest activity timestamp (list ordering).
    pub(crate) times: HashMap<u64, i64>,
    /// True when running on the fake backend (UI labels it as such).
    pub(crate) fake: bool,
    /// Archive flags per numeric message ID (the store's `FLAG_*`
    /// bits: deleted, starred, pinned), preserved across upserts
    /// and persisted in the store.
    pub(crate) flags: HashMap<u64, i64>,
    /// Photo re-download references per numeric message ID,
    /// seeded from the archive at start and extended by every
    /// photo upsert. A display fetch for a photo the backend no
    /// longer holds in its session cache passes the reference
    /// along, so photos from before this session stay fetchable.
    pub(crate) photo_refs: HashMap<u64, backend::PhotoRef>,
    /// Chats where the user performed a real read action (scrolled
    /// to the newest messages, focused or typed in the composer,
    /// sent a reply) since the last receipt went out. Read
    /// receipts are gated on this set (the owner's read-by-action
    /// rule): merely opening a chat, or a message arriving while
    /// its screen happens to be open, never marks anything read.
    pub(crate) engaged_chats: std::collections::HashSet<u64>,
}

impl LiveSession {
    /// Drops the held QR payload and its validity window. Every
    /// transition that ends the payload's session (connect,
    /// logout, ban, connection loss, worker crash, failure,
    /// expiry) goes through here, so a dead code can never linger
    /// as if it were still scannable.
    pub(crate) fn clear_qr(&mut self) {
        self.qr = None;
        self.qr_expires_at = None;
    }
}

/// What the connect screen's QR area may present, decided from
/// session state alone. This is the pairing gate: the `Live`
/// variant, the only one that carries a matrix to paint, is
/// reachable only while a session is in a pairable status AND
/// holds a payload that session's backend delivered (and whose
/// validity window the expiry sweep has not yet closed). Every
/// other state presents words, never a code-shaped graphic, so a
/// dead QR with a waiting-for-scan status is impossible by
/// construction rather than by painter discipline.
pub(crate) enum ConnectQr<'a> {
    /// A current server-issued payload is on screen.
    Live(&'a QrMatrix),
    /// A live start was attempted and failed before any session
    /// existed; carries the plain-language reason.
    StartFailed(&'a str),
    /// A session exists and is working toward a payload (or a
    /// fresh one, after a rotation gap).
    Connecting,
    /// The session is down, ended, or already linked: pairing is
    /// not possible from it right now. Carries the status so the
    /// screen can name it.
    Unavailable(&'a LinkStatus),
    /// No live start was ever requested (demo mode): pairing here
    /// is not real and must say so.
    Demo,
}

impl crate::state::App {
    /// The connect screen's QR-area state (see [`ConnectQr`]).
    pub(crate) fn connect_qr(&self) -> ConnectQr<'_> {
        if let Some(message) = &self.live_start_error {
            return ConnectQr::StartFailed(message);
        }
        let Some(session) = &self.live else {
            return ConnectQr::Demo;
        };
        match session.status {
            LinkStatus::Starting => ConnectQr::Connecting,
            LinkStatus::WaitingQr | LinkStatus::CodeReady => match &session.qr {
                Some(matrix) => ConnectQr::Live(matrix),
                None => ConnectQr::Connecting,
            },
            _ => ConnectQr::Unavailable(&session.status),
        }
    }

    /// Drops the held QR payload once its validity window has
    /// closed. Runs beside the pairing-code expiry in the
    /// per-frame backend poll: a rotation normally replaces the
    /// payload long before this fires, so it is the backstop for
    /// a library that goes quiet without an exhaustion event.
    fn sweep_qr_expiry(&mut self) {
        if let Some(session) = &mut self.live {
            let expired = session.qr.is_some()
                && session.qr_expires_at.is_some_and(|at| at <= std::time::Instant::now());
            if expired {
                session.clear_qr();
            }
        }
    }
}

/// Stable numeric ID for a JID (FNV-1a). The UI's model uses `u64`
/// IDs; JIDs stay strings at the backend boundary.
pub(crate) fn jid_id(jid: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in jid.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn message_id(chat_jid: &str, proto_id: &str) -> u64 {
    jid_id(&format!("{chat_jid}/{proto_id}"))
}

/// Maps a backend error string to a human sentence for the
/// notice banner. The caller (the BackendEvent::Error arm) has
/// already scrubbed the text for display; this function only
/// rewrites the known patterns to fixed sentences and passes the
/// rest through, trimmed, because a specific honest message beats
/// a generic one.
pub(crate) fn human_error(message: &str) -> String {
    let m = message.to_lowercase();
    if m.contains("invalid chat address") || m.contains("invalid jid") {
        "That chat address was not accepted. The message was not sent.".to_owned()
    } else if m.contains("no incoming call") {
        "There is no call waiting to answer.".to_owned()
    } else if m.contains("already in progress") {
        "A call is already in progress.".to_owned()
    } else if m.contains("secret") && m.contains("poll") {
        "This poll cannot be voted on from Zaprust right now.".to_owned()
    } else if m.contains("timed out") || m.contains("timeout") || m.contains("network") {
        "The connection had a problem. Check your internet and try again.".to_owned()
    } else {
        message.trim().to_owned()
    }
}

/// Unix seconds → "HH:MM" in UTC (display only).
pub(crate) fn fmt_time(timestamp: i64) -> String {
    let secs = timestamp.rem_euclid(86_400);
    format!("{:02}:{:02}", secs / 3600, (secs % 3600) / 60)
}

pub(crate) const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Converts a count of days since the unix epoch into a
/// (year, month, day) civil date (the standard days-from-civil
/// inverse, valid across the whole timestamp range we handle).
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The day-separator label for a unix timestamp: "Today" or
/// "Yesterday" relative to the current day, else the full date
/// ("14 November 2026"). Day boundaries follow the same UTC day
/// grid [`fmt_time`] uses, so a message's day label always
/// agrees with its clock time.
pub(crate) fn fmt_day(timestamp: i64) -> String {
    let day = timestamp.div_euclid(86_400);
    let today = (crate::state::now_unix() / 86_400) as i64;
    match day - today {
        0 => "Today".to_owned(),
        -1 => "Yesterday".to_owned(),
        _ => {
            let (y, m, d) = civil_from_days(day);
            format!("{d} {} {y}", MONTH_NAMES[(m - 1) as usize])
        }
    }
}

fn kind_of(kind: LiveChatKind) -> ChatKind {
    match kind {
        LiveChatKind::Dm => ChatKind::Dm,
        LiveChatKind::Group => ChatKind::Group,
        LiveChatKind::Channel => ChatKind::Channel,
    }
}

fn to_chat(live: &LiveChat) -> Chat {
    Chat {
        id: jid_id(&live.jid),
        kind: kind_of(live.kind),
        title: live.title.clone(),
        subtitle: match live.kind {
            LiveChatKind::Dm => "live chat".to_owned(),
            LiveChatKind::Group => "live group".to_owned(),
            LiveChatKind::Channel => "live channel".to_owned(),
        },
        preview: live.preview.clone(),
        preview_sender: live.preview_sender.clone(),
        // The backend chat summary carries no last-message
        // anatomy; the arrival path fills it in for chats with
        // activity this session.
        preview_mine: false,
        preview_delivery: None,
        preview_media: None,
        time: fmt_time(live.timestamp),
        unread: live.unread,
        mentions: 0,
        pinned: live.pinned,
        archived: live.archived,
        ephemeral_secs: live.ephemeral_secs,
        muted: live.muted,
        favorite: false,
        avatar_seed: (jid_id(&live.jid) % 251) as u8,
        verified: false,
        // The library surfaces no AI-contact flag; Meta's assistant
        // is recognized by its exact chat title (documented
        // heuristic, no address is hardcoded).
        meta_ai: live.title.trim() == "Meta AI",
        community: None,
        messages: Vec::new(),
    }
}

/// Converts a backend rich kind into the core model kind.
pub(crate) fn to_kind(kind: &backend::LiveMessageKind) -> MessageKind {
    match kind {
        backend::LiveMessageKind::Text => MessageKind::Text,
        backend::LiveMessageKind::Poll { question, options, multiple, my_votes, correct } => {
            MessageKind::Poll {
                question: question.clone(),
                options: options.clone(),
                multiple: *multiple,
                my_votes: my_votes.clone(),
                correct: *correct,
            }
        }
        backend::LiveMessageKind::Location { name, latitude, longitude } => {
            MessageKind::Location { name: name.clone(), latitude: *latitude, longitude: *longitude }
        }
        backend::LiveMessageKind::Contact { name, phone } => {
            MessageKind::Contact { name: name.clone(), phone: phone.clone() }
        }
        backend::LiveMessageKind::Carousel { body, cards } => MessageKind::Carousel {
            body: body.clone(),
            cards: cards
                .iter()
                .map(|c| core::model::CarouselCard {
                    title: c.title.clone(),
                    body: c.body.clone(),
                    image: c.image.clone(),
                    buttons: c.buttons.clone(),
                })
                .collect(),
        },
        backend::LiveMessageKind::Media { media, caption, seconds, size_bytes } => {
            use backend::MediaKind;
            match media {
                MediaKind::Photo => {
                    MessageKind::Image { caption: caption.clone(), view_once: false }
                }
                MediaKind::Video => MessageKind::Video {
                    caption: caption.clone(),
                    view_once: false,
                    secs: *seconds,
                },
                MediaKind::Voice => MessageKind::Voice {
                    secs: *seconds,
                    waveform: (0..28)
                        .map(|i| {
                            let h = (caption.len() as u32 * 31 + i * 17) % 10;
                            0.3 + f32::from(h as u8) * 0.07
                        })
                        .collect(),
                },
                MediaKind::Document => MessageKind::Document {
                    name: if caption.is_empty() { "Document".to_owned() } else { caption.clone() },
                    size_kib: u32::try_from(size_bytes / 1024).unwrap_or(0),
                },
                MediaKind::Sticker => {
                    MessageKind::Image { caption: "Sticker".to_owned(), view_once: false }
                }
            }
        }
        backend::LiveMessageKind::Other(_) => MessageKind::Text,
    }
}

/// Converts a backend message (+ archive flags) into a core message.
pub(crate) fn to_message_with_flags(live: &LiveMessage, flags: i64) -> Message {
    let mut kind = to_kind(&live.kind);
    if live.view_once {
        match &mut kind {
            MessageKind::Image { view_once, .. } | MessageKind::Video { view_once, .. } => {
                *view_once = true;
            }
            _ => {}
        }
    }
    let forwarded = live.forwarded;
    Message {
        id: message_id(&live.chat_jid, &live.id),
        sender: live.sender.clone(),
        body: live.body.clone(),
        kind,
        time: fmt_time(live.timestamp),
        day: fmt_day(live.timestamp),
        delivery: if live.from_me { Some(Delivery::Delivered) } else { None },
        quoted_sender: live.quoted_sender.clone(),
        quoted_body: live.quoted_body.clone(),
        reactions: live.reactions.clone(),
        edited: live.edited,
        deleted: flags & store::FLAG_DELETED != 0,
        deleted_by_sender: flags & store::FLAG_DELETED_BY_SENDER != 0,
        link_preview: live.link_preview.as_ref().map(|p| core::model::LinkPreview {
            title: p.title.clone(),
            description: p.description.clone(),
            url: p.url.clone(),
            // The backend preview payload carries no image.
            image: core::model::PreviewImage::None,
        }),
        starred: flags & store::FLAG_STARRED != 0 || live.starred,
        pinned: flags & store::FLAG_PINNED_MESSAGE != 0 || live.pinned,
        forwarded,
        expires_at: u64::try_from(live.expires_at).unwrap_or(0),
    }
}

/// The text a live message may contribute to glanceable
/// surfaces: the chat-list preview and the notification body.
/// A view-once message's caption is part of its sealed payload
/// (shown only with the media itself, in the viewer), so it
/// contributes only the placeholder its bubble shows, never
/// the caption the backend carried on the body. Mirrors
/// [`Message::preview_text`] for the converted mirror messages.
fn glanceable_text(live: &LiveMessage) -> String {
    if live.view_once {
        core::model::VIEW_ONCE_PREVIEW.to_owned()
    } else {
        live.body.clone()
    }
}

/// What a live start may do about its database key.
enum KeyDecision {
    /// Start under this key.
    Run([u8; 32]),
    /// Do not start; show the user this reason instead.
    Abort(String),
}

/// Decides a live start's database key from the key load.
///
/// A real start fails closed on a load error (audit finding 6):
/// a substitute key would open or create the session store under
/// the wrong key, stranding the linked account behind a publicly
/// known key and hiding the real failure (a locked keyring, an
/// unreadable key file). Demo mode is the one exception: the fake
/// backend never opens a database, so the placeholder key it runs
/// under never encrypts anything, and the archive still reports
/// its own unavailability through the usual note.
fn key_decision(key_result: &Result<[u8; 32], store::StoreError>, fake: bool) -> KeyDecision {
    match key_result {
        Ok(key) => KeyDecision::Run(*key),
        Err(_) if fake => KeyDecision::Run([0; 32]),
        Err(e) => KeyDecision::Abort(format!(
            "Zaprust could not unlock your saved chats, so it did not connect. \
             The unlock key stored on this device could not be read ({e}). \
             Nothing was changed. Make the key available again (for example, \
             unlock your keyring) and try connecting again."
        )),
    }
}

impl crate::state::App {
    /// Starts live mode: opens the archive, spawns the supervised
    /// backend, loads archived chats, and kicks off the link flow.
    /// Called from `App::new` for `--live`, and from the account
    /// modal's "Add account" for the in-app path.
    pub(crate) fn start_live(&mut self, ctx: &egui::Context, fake: bool, fake_auto: bool) {
        if self.live.is_some() {
            return;
        }
        let data_dir = backend::data_dir();
        // Both local databases are encrypted under one key from
        // the OS keyring (SEC-F01). Without the key neither can
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
                self.notice = Some(message);
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
        }
        let repaint_ctx = ctx.clone();
        let wake: backend::WakeFn = std::sync::Arc::new(move || repaint_ctx.request_repaint());
        let config = BackendConfig { data_dir, account_id: "default".to_owned(), db_key };
        if !fake {
            // Convert a plaintext session store left by an older
            // build before the backend opens it with the key; a
            // failure here surfaces as the backend's own
            // session-store error.
            let _ = store::encrypt_file_in_place(&config.session_db_path(), &config.db_key);
        }
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
        let mut session = LiveSession {
            channels,
            status: LinkStatus::Starting,
            qr: None,
            qr_expires_at: None,
            pair_code: None,
            pair_code_at: None,
            account: String::new(),
            connected_at: None,
            store: store_opt,
            jids: HashMap::new(),
            messages: HashMap::new(),
            times: HashMap::new(),
            fake,
            flags: HashMap::new(),
            engaged_chats: std::collections::HashSet::new(),
            photo_refs: HashMap::new(),
        };
        // Load the archive into the rendered data. Live mode never
        // shows demo data: start from empty and fill from the archive.
        self.chats = Vec::new();
        if let Some(store) = &session.store {
            if let Ok(chats) = store.chats() {
                for chat in &chats {
                    session.jids.insert(jid_id(&chat.jid), chat.jid.clone());
                    session.times.insert(jid_id(&chat.jid), chat.timestamp);
                    if let Ok(msgs) = store.messages(&chat.jid) {
                        for (m, flags) in &msgs {
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
                            // A photo's re-download reference is
                            // seeded the same way, so a photo from
                            // before this session can still be
                            // fetched for display.
                            if let Some(reference) = &m.photo_ref {
                                session
                                    .photo_refs
                                    .insert(message_id(&m.chat_jid, &m.id), reference.clone());
                            }
                        }
                        session.messages.insert(
                            jid_id(&chat.jid),
                            msgs.iter()
                                .map(|(m, flags)| (m.timestamp, to_message_with_flags(m, *flags)))
                                .collect(),
                        );
                    }
                }
                self.chats = chats.iter().map(to_chat).collect();
            }
        }
        self.live = Some(session);
        self.live_start_error = None;
        if let Some(note) = archive_note {
            self.notice = Some(note);
        }
        self.live_mode = true;
        // Entering live mode is a chat switch too (the Add
        // account path starts it from a running demo session):
        // the composer state of the chat being left parks under
        // its own id, and the first live chat starts from a
        // clean composer, never with demo state inside it.
        let first_live = self.chats.first().map_or(0, |c| c.id);
        let previous = self.selected_chat;
        // A new session never inherits a recording: cancel any
        // take in progress outright, even when the first live
        // chat's id matches the current selection and the
        // switch below would be a no-op. A take recorded before
        // this session must not be sendable inside it.
        self.cancel_recording();
        self.switch_composer_chat(previous, first_live);
        self.selected_chat = first_live;
        self.sync_conversation();
        self.send_backend(BackendCommand::ConnectQr);
        if fake {
            self.trace_fake_script();
        }
    }

    pub(crate) fn send_backend(&mut self, cmd: BackendCommand) {
        if self.debug.mode {
            let name = format!("{cmd:?}");
            let name = name.split([' ', '{', '(']).next().unwrap_or("Command");
            self.trace_push("command", &format!("Command {name} sent to backend"), None);
        }
        if let Some(session) = &self.live {
            let _ = session.channels.commands.send(cmd);
        }
    }

    /// Requests a pairing code for the typed phone number (digits
    /// only, the protocol wants international format without `+`).
    pub(crate) fn request_pair_code(&mut self) {
        let digits: String = self.pair_phone.chars().filter(char::is_ascii_digit).collect();
        if digits.is_empty() {
            self.notice =
                Some("Type your phone number first (digits, with country code).".to_owned());
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
        Some(QuotedRef { message_id, sender_jid, excerpt: excerpt.chars().take(200).collect() })
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
        if let Some(session) = &mut self.live {
            session.messages.entry(self.selected_chat).or_default().push((now, echo));
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
            if let Some(entry) = session.messages.get_mut(&chat_id) {
                if let Some((_, echo)) = entry
                    .iter_mut()
                    .filter(|(_, m)| m.delivery == Some(Delivery::Sending) && m.is_mine())
                    .min_by_key(|(ts, _)| *ts)
                {
                    echo.id = mid;
                    echo.delivery = Some(Delivery::Sent);
                    bound = true;
                }
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
    fn sync_conversation_now(&mut self) {
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
        // Top the queue up from the channel. Moving an event is
        // cheap; applying it is what costs, and that is budgeted
        // below.
        if let Some(session) = &self.live {
            while let Ok(event) = session.channels.events.try_recv() {
                self.event_queue.push_back(event);
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
        // QR payloads die on the validity window the library
        // reported with them; drop an expired one so a dead code
        // is never presented as scannable (the same discipline as
        // the pairing-code sweep below).
        self.sweep_qr_expiry();
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
                    self.notice = Some("That pairing code expired. Request a new one.".to_owned());
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
    fn finish_event_batch(&mut self) {
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
                    let mut latest: HashMap<u64, (LiveMessage, i64)> = HashMap::new();
                    let mut order: Vec<u64> = Vec::new();
                    for msg in batch.store_writes {
                        let mid = message_id(&msg.chat_jid, &msg.id);
                        if !latest.contains_key(&mid) {
                            order.push(mid);
                        }
                        let flags = session.flags.get(&mid).copied().unwrap_or(0);
                        latest.insert(mid, (msg, flags));
                    }
                    let rows: Vec<(LiveMessage, i64)> =
                        order.into_iter().filter_map(|mid| latest.remove(&mid)).collect();
                    let _ = store.upsert_messages(&rows);
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

    fn apply_event(&mut self, ctx: &egui::Context, event: BackendEvent) {
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
                    session.clear_qr();
                    // A used code must never resurface on the
                    // connect screen (logic review F13).
                    session.pair_code = None;
                    session.pair_code_at = None;
                }
                if self.screen == crate::state::Screen::Connect {
                    self.screen = crate::state::Screen::Chats;
                }
            }
            BackendEvent::LoggedOut => {
                if let Some(session) = &mut self.live {
                    session.status = LinkStatus::LoggedOut;
                    session.clear_qr();
                }
                self.notice = Some("The device was logged out.".to_owned());
                // A logged-out client belongs on the link screen,
                // not on an empty chat list.
                if self.live_mode {
                    self.screen = crate::state::Screen::Connect;
                }
            }
            BackendEvent::TemporaryBan { code } => {
                if let Some(session) = &mut self.live {
                    session.status = LinkStatus::Banned(code);
                    session.clear_qr();
                }
            }
            BackendEvent::ChatUpsert(chat) => self.apply_chat_upsert(&chat),
            BackendEvent::MessageUpsert(msg) => {
                // Applying an upsert never triggers a network
                // fetch: a link preview renders only when the
                // sender's client attached one to the message.
                self.apply_message_upsert(&msg);
            }
            BackendEvent::MessageSent { chat_jid, message_id } => {
                self.apply_message_sent(&chat_jid, &message_id);
            }
            BackendEvent::Receipt { chat_jid, message_ids, kind, at } => {
                let chat_id = jid_id(&chat_jid);
                for pid in &message_ids {
                    let mid = message_id(&chat_jid, pid);
                    let delivery = match kind {
                        ReceiptKind::Sent => Delivery::Sent,
                        ReceiptKind::Delivered => Delivery::Delivered,
                        ReceiptKind::Read | ReceiptKind::Played => Delivery::Read,
                    };
                    let log = self.receipt_log.entry(mid).or_default();
                    log.push((kind, fmt_time(at)));
                    // The receipt log only feeds the message-info
                    // popover; keep it bounded (logic review F21).
                    if log.len() > 32 {
                        log.drain(..log.len() - 32);
                    }
                    if self.receipt_log.len() > 4096 {
                        self.receipt_log.clear();
                    }
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
                if self.selected_chat == chat_id {
                    self.sync_conversation();
                }
            }
            BackendEvent::ReactionUpdate { chat_jid, message_id: pid, actor, emoji } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                let key = format!("{chat_id}:{mid}");
                if actor == "Me" {
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
            BackendEvent::MessageEdited { chat_jid, message_id: pid, new_body } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
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
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                self.apply_revoke(chat_id, mid);
            }
            BackendEvent::DisappearingChanged { chat_jid, duration_secs } => {
                let chat_id = jid_id(&chat_jid);
                self.apply_disappearing_changed(chat_id, duration_secs);
            }
            BackendEvent::DefaultDisappearingTimerChanged { duration_secs } => {
                self.apply_default_disappearing_timer(duration_secs);
            }
            BackendEvent::MuteChanged { chat_jid, muted } => {
                let chat_id = jid_id(&chat_jid);
                self.apply_mute_changed(chat_id, muted);
            }
            BackendEvent::PinChanged { chat_jid, pinned } => {
                let chat_id = jid_id(&chat_jid);
                self.apply_pin_changed(chat_id, pinned);
            }
            BackendEvent::ArchiveChanged { chat_jid, archived } => {
                let chat_id = jid_id(&chat_jid);
                self.apply_archive_changed(chat_id, archived);
            }
            BackendEvent::StarChanged { chat_jid, message_id: pid, starred } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                self.apply_star_changed(chat_id, mid, &pid, starred);
            }
            BackendEvent::ChatReadChanged { chat_jid, read, unread_count } => {
                let chat_id = jid_id(&chat_jid);
                self.apply_chat_read_changed(chat_id, read, unread_count);
            }
            BackendEvent::ChatDeleted { chat_jid } => {
                let chat_id = jid_id(&chat_jid);
                self.apply_chat_deleted(chat_id, &chat_jid);
            }
            BackendEvent::ChatCleared { chat_jid, delete_starred, last_message_timestamp } => {
                let chat_id = jid_id(&chat_jid);
                self.apply_chat_cleared(chat_id, &chat_jid, delete_starred, last_message_timestamp);
            }
            BackendEvent::MessageDeletedForMe { chat_jid, message_id: pid } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                self.apply_message_deleted_for_me(chat_id, &chat_jid, mid, &pid);
            }
            BackendEvent::PrivacyState(map) => {
                self.apply_privacy_state(map);
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
            }
            BackendEvent::MediaSaved { path, .. } => {
                self.notice = Some(format!("Saved to {path}"));
            }
            BackendEvent::PhotoBytes { chat_jid, message_id: pid, bytes } => {
                // The fetched bytes become the message's texture:
                // from here on its bubble, album cell and viewer
                // paint the photo itself. Bytes that do not decode
                // are a failed fetch, never a reason to substitute
                // another image.
                let mid = message_id(&chat_jid, &pid);
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
            BackendEvent::PhotoFetchFailed { chat_jid, message_id: pid, .. } => {
                let mid = message_id(&chat_jid, &pid);
                // Only a fetch still in flight can fail; a late
                // failure must not clobber a photo that has since
                // landed.
                if self.media.photo_fetch.get(&mid) == Some(&crate::state::PhotoFetchState::Loading)
                {
                    self.media.photo_fetch.insert(mid, crate::state::PhotoFetchState::Failed);
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
            BackendEvent::GroupMembers { chat_jid, members } => {
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
                // An error while a call action is unconfirmed almost
                // always belongs to it; release the buttons (F18).
                if let Some(call) = &mut self.calls.active {
                    call.action_pending = false;
                }
                self.notice = Some(human_error(&message));
                // A failed send must surface on its own bubble, not
                // only as a transient notice (logic review F6).
                self.fail_pending_echoes(false);
                if let Some(session) = &mut self.live {
                    if !session.status.is_linked() {
                        session.status = LinkStatus::Failed(message);
                        session.clear_qr();
                    }
                }
            }
            BackendEvent::IncomingCall { call_id, chat_jid, video } => {
                let chat_id = jid_id(&chat_jid);
                self.on_incoming_call(&call_id, chat_id, video);
            }
            BackendEvent::CallChanged { call_id, chat_jid, phase, seconds, reason } => {
                let chat_id = jid_id(&chat_jid);
                self.on_call_changed(&call_id, chat_id, phase, seconds, reason);
            }
        }
    }

    fn apply_chat_upsert(&mut self, live: &LiveChat) {
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
            chat.ephemeral_secs = fresh.ephemeral_secs;
            chat.muted = fresh.muted;
        } else {
            self.chats.push(to_chat(&merged));
        }
        self.sort_live_chats();
    }

    /// Applies a disappearing-timer change for one chat, from the
    /// local dock action or from a backend DisappearingChanged
    /// event (a change made on another device, or by the other
    /// participant). State and archive update either way; the
    /// in-conversation system notice is added only when the value
    /// actually changes, which is what keeps the echo of our own
    /// change from announcing itself twice.
    pub(crate) fn apply_disappearing_changed(&mut self, chat_id: u64, duration_secs: u32) {
        let current =
            self.chats.iter().find(|c| c.id == chat_id).map(|c| c.ephemeral_secs).unwrap_or(0);
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.ephemeral_secs = duration_secs;
        }
        if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            c.ephemeral_secs = duration_secs;
        }
        if let Some(session) = &self.live {
            if let (Some(store), Some(jid)) = (&session.store, session.jids.get(&chat_id)) {
                if let Some(chat) = self.chats.iter().find(|c| c.id == chat_id) {
                    let ts = session.times.get(&chat_id).copied().unwrap_or(0);
                    let _ = store.upsert_chat(&chat_to_live(chat, jid, ts));
                }
            }
        }
        if current != duration_secs {
            self.push_system_notice(
                chat_id,
                &crate::extras::disappearing_notice_text(duration_secs),
            );
        }
    }

    /// Applies a mute change the server reported (a mute or
    /// unmute done on another device, or the account state from
    /// a full sync). The reported value wins over the local
    /// mirror unconditionally, including over an optimistic
    /// local toggle whose backend command failed: this event is
    /// the reconciliation path for mute, the counterpart of
    /// DisappearingChanged for the timer. State and archive
    /// update either way. A mute set from this client does not
    /// wait for this event; it applies through apply_chat_action
    /// at once.
    pub(crate) fn apply_mute_changed(&mut self, chat_id: u64, muted: bool) {
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.muted = muted;
        }
        if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            c.muted = muted;
        }
        if let Some(session) = &self.live {
            if let (Some(store), Some(jid)) = (&session.store, session.jids.get(&chat_id)) {
                if let Some(chat) = self.chats.iter().find(|c| c.id == chat_id) {
                    let ts = session.times.get(&chat_id).copied().unwrap_or(0);
                    let _ = store.upsert_chat(&chat_to_live(chat, jid, ts));
                }
            }
        }
    }

    /// Applies a pin change the server reported (a pin or unpin
    /// done on another device, or the account state from a full
    /// sync). The reported value wins over the local mirror
    /// unconditionally, including over an optimistic local
    /// toggle whose backend command failed: this event is the
    /// reconciliation path for pin, the counterpart of
    /// MuteChanged for mute. State and archive update either
    /// way; the pinned-first list order reads the field at
    /// render time, so no re-sort is needed here. A pin set
    /// from this client does not wait for this event; it
    /// applies through apply_chat_action at once.
    pub(crate) fn apply_pin_changed(&mut self, chat_id: u64, pinned: bool) {
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.pinned = pinned;
        }
        if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            c.pinned = pinned;
        }
        if let Some(session) = &self.live {
            if let (Some(store), Some(jid)) = (&session.store, session.jids.get(&chat_id)) {
                if let Some(chat) = self.chats.iter().find(|c| c.id == chat_id) {
                    let ts = session.times.get(&chat_id).copied().unwrap_or(0);
                    let _ = store.upsert_chat(&chat_to_live(chat, jid, ts));
                }
            }
        }
    }

    /// Applies an archive change the server reported (an
    /// archive or unarchive done on another device, or the
    /// account state from a full sync). The reported value wins
    /// over the local mirror unconditionally, including over an
    /// optimistic local toggle whose backend command failed:
    /// this event is the reconciliation path for archive, the
    /// counterpart of MuteChanged for mute. State and archive
    /// update either way; the list filters read the field at
    /// render time, so an archived chat leaves the main list
    /// views (and an unarchived one returns) exactly as with a
    /// local archive action. An archive set from this client
    /// does not wait for this event; it applies through
    /// apply_chat_action at once.
    pub(crate) fn apply_archive_changed(&mut self, chat_id: u64, archived: bool) {
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.archived = archived;
        }
        if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            c.archived = archived;
        }
        if let Some(session) = &self.live {
            if let (Some(store), Some(jid)) = (&session.store, session.jids.get(&chat_id)) {
                if let Some(chat) = self.chats.iter().find(|c| c.id == chat_id) {
                    let ts = session.times.get(&chat_id).copied().unwrap_or(0);
                    let _ = store.upsert_chat(&chat_to_live(chat, jid, ts));
                }
            }
        }
    }

    /// Applies a star change the server reported (a star or
    /// unstar done on another device, or the account state from
    /// a full sync). The reported value wins over the local
    /// mirror unconditionally, exactly as a local toggle would
    /// land it, minus the backend command: the session mirror
    /// flips, and the flag write-through (`set_live_flag`)
    /// updates the session flag map and the archive row, which
    /// is also what protects the value from uninformed arrival
    /// upserts (they resolve their flags from that map). A star
    /// set from this client does not wait for this event; it
    /// applies through the composer's toggle at once.
    pub(crate) fn apply_star_changed(
        &mut self,
        chat_id: u64,
        msg_id: u64,
        protocol_id: &str,
        starred: bool,
    ) {
        // The protocol-id bookkeeping an upsert would have done,
        // so the flag write-through below finds the archive row
        // even for a message this session has not re-upserted.
        self.protocol_ids.insert(msg_id, protocol_id.to_owned());
        if let Some(entry) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            for (_, m) in entry.iter_mut() {
                if m.id == msg_id {
                    m.starred = starred;
                }
            }
        }
        self.set_live_flag(chat_id, msg_id, store::FLAG_STARRED, starred);
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Writes one chat's archive row from the mirror, so the
    /// stored row carries the state the list shows. For mirror
    /// changes that arrive without a chat upsert of their own
    /// (the unread badge an incoming message raised), whose own
    /// write-through would otherwise never happen.
    fn persist_chat_row(&self, chat_id: u64) {
        if let Some(session) = &self.live {
            if let (Some(store), Some(jid)) = (&session.store, session.jids.get(&chat_id)) {
                if let Some(chat) = self.chats.iter().find(|c| c.id == chat_id) {
                    let ts = session.times.get(&chat_id).copied().unwrap_or(0);
                    let _ = store.upsert_chat(&chat_to_live(chat, jid, ts));
                }
            }
        }
    }

    /// Applies a read-state change the server reported (a chat
    /// marked read or unread on another device, or the account
    /// state from a full sync). State adoption only, in both
    /// directions: the mirror and the archive take the reported
    /// state, and nothing is ever sent from here. A remote read
    /// in particular must not produce read receipts from this
    /// client (the other device already sent them), so the
    /// chat's pending receipt queue is dropped, not flushed:
    /// those messages are read, and a later local engagement
    /// finds nothing left to mark. A remote mark-as-unread
    /// adopts the reported count, floored at one: the chat IS
    /// unread, so a zero badge would hide the state the server
    /// just asserted.
    pub(crate) fn apply_chat_read_changed(&mut self, chat_id: u64, read: bool, unread_count: u32) {
        let unread = if read { 0 } else { unread_count.max(1) };
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.unread = unread;
        }
        if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            c.unread = unread;
        }
        if read {
            self.unread_live.remove(&chat_id);
        }
        if let Some(session) = &self.live {
            if let (Some(store), Some(jid)) = (&session.store, session.jids.get(&chat_id)) {
                if let Some(chat) = self.chats.iter().find(|c| c.id == chat_id) {
                    let ts = session.times.get(&chat_id).copied().unwrap_or(0);
                    let _ = store.upsert_chat(&chat_to_live(chat, jid, ts));
                }
            }
        }
    }

    /// Applies a chat deletion the server reported (the chat
    /// was deleted on the phone). This client has no
    /// delete-chat action of its own, so there is no local
    /// behavior to mirror and nothing is sent back; the
    /// adoption is the removal itself, everywhere the chat
    /// exists locally: the chat list, the detached slot, the
    /// session mirror and its per-message helper entries, the
    /// pending receipt queue (receipts for a deleted chat must
    /// never go out), the parked composer state (a chat that
    /// later reappears under the same JID starts clean), and
    /// the archive rows. The event's own JID keys the archive
    /// deletion, so a chat that exists only in the archive is
    /// removed too. If the deleted chat was open, the selection
    /// moves to the first remaining chat, as a live start does.
    pub(crate) fn apply_chat_deleted(&mut self, chat_id: u64, chat_jid: &str) {
        // Per-message helper entries go first, while the mirror
        // still lists the messages they belong to.
        let helpers: Vec<(u64, Option<String>)> = self
            .live
            .as_ref()
            .and_then(|s| s.messages.get(&chat_id))
            .map(|msgs| {
                msgs.iter().map(|(_, m)| (m.id, self.protocol_ids.get(&m.id).cloned())).collect()
            })
            .unwrap_or_default();
        for (msg_id, protocol_id) in helpers {
            self.forget_live_message(chat_id, msg_id, protocol_id.as_deref());
        }
        if let Some(session) = &mut self.live {
            session.messages.remove(&chat_id);
            session.jids.remove(&chat_id);
            session.times.remove(&chat_id);
            session.engaged_chats.remove(&chat_id);
            if let Some(store) = &session.store {
                let _ = store.delete_chat(chat_jid);
            }
        }
        // Media the user saved from this chat sits in the
        // downloads folder as plaintext files of their own: the
        // chat's deletion takes them too, or the messages would
        // survive their own deletion indefinitely (privacy
        // audit A3). Chat exports are not touched: an export is
        // a document the user deliberately created, and the
        // Storage page already discloses that exports are not
        // encrypted.
        backend::remove_downloads_for_chat(&backend::data_dir(), chat_jid);
        self.unread_live.remove(&chat_id);
        self.live_typing.remove(&chat_id);
        self.pending_notifications.retain(|n| n.chat_id != chat_id);
        self.chats.retain(|c| c.id != chat_id);
        if self.detached_chat.as_ref().is_some_and(|c| c.id == chat_id) {
            self.detached_chat = None;
        }
        self.composer.chat_drafts.remove(&chat_id);
        self.composer.chat_parked.remove(&chat_id);
        if self.selected_chat == chat_id {
            let next = self.chats.first().map_or(0, |c| c.id);
            self.switch_composer_chat(chat_id, next);
            // The switch parked the deleted chat's composer
            // state under its id; a deleted chat keeps nothing.
            self.composer.chat_drafts.remove(&chat_id);
            self.composer.chat_parked.remove(&chat_id);
            self.selected_chat = next;
            self.sync_conversation();
        }
    }

    /// Applies a chat clearing the server reported (the chat
    /// was cleared on the phone). This client has no
    /// clear-chat action of its own, so there is no local
    /// behavior to mirror and nothing is sent back; the
    /// adoption is the removal of the cleared messages,
    /// everywhere they live locally: the session mirror, the
    /// per-message helper entries, and the archive rows. The
    /// chat itself stays: its row, its recorded activity stamp
    /// (its list position), and its settings (pin, mute,
    /// archive, disappearing timer) are exactly as they were,
    /// and its unread state is untouched (clearing asserts
    /// nothing about read state; a read change arrives as its
    /// own sync event). The clear's own terms are honored:
    /// messages stamped after the bound are not the clear's to
    /// remove, and starred messages survive unless the
    /// clearing device deleted them too. The event's own JID
    /// keys the archive deletion, so rows the mirror does not
    /// hold are cleared too.
    pub(crate) fn apply_chat_cleared(
        &mut self,
        chat_id: u64,
        chat_jid: &str,
        delete_starred: bool,
        last_message_timestamp: Option<i64>,
    ) {
        // Collect the doomed messages first (app id + protocol
        // id), while the mirror still lists them: the helper
        // cleanup below needs both.
        let doomed: Vec<(u64, Option<String>)> = self
            .live
            .as_ref()
            .and_then(|s| s.messages.get(&chat_id))
            .map(|msgs| {
                msgs.iter()
                    .filter(|(ts, m)| {
                        last_message_timestamp.is_none_or(|bound| *ts <= bound)
                            && (delete_starred || !m.starred)
                    })
                    .map(|(_, m)| (m.id, self.protocol_ids.get(&m.id).cloned()))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(msgs) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            let doomed_ids: std::collections::HashSet<u64> =
                doomed.iter().map(|(id, _)| *id).collect();
            msgs.retain(|(_, m)| !doomed_ids.contains(&m.id));
        }
        if let Some(session) = &self.live {
            if let Some(store) = &session.store {
                let _ =
                    store.delete_chat_messages(chat_jid, !delete_starred, last_message_timestamp);
            }
        }
        for (msg_id, protocol_id) in &doomed {
            self.forget_live_message(chat_id, *msg_id, protocol_id.as_deref());
            // A cleared message's saved download goes with it
            // (privacy audit A3); kept (starred) messages are
            // not in `doomed`, so their downloads stay, exactly
            // like their archive rows.
            if let Some(protocol_id) = protocol_id {
                backend::remove_download_for_message(&backend::data_dir(), chat_jid, protocol_id);
            }
        }
        if !doomed.is_empty() {
            self.refresh_preview_after_removal(chat_id);
            if self.selected_chat == chat_id {
                self.sync_conversation();
            }
        }
    }

    /// Applies a delete-for-me the server reported (one
    /// message deleted for the account on the phone). This
    /// client has no delete-for-me action of its own, so there
    /// is no local behavior to mirror and nothing is sent
    /// back; the adoption is the removal of exactly that
    /// message: from the session mirror, from its per-message
    /// helper entries, and from the archive. The store
    /// deletion is keyed by the event's own JID and protocol
    /// ID, so a row the mirror does not hold is removed too.
    /// Neighbouring messages, the chat, and its settings are
    /// untouched.
    pub(crate) fn apply_message_deleted_for_me(
        &mut self,
        chat_id: u64,
        chat_jid: &str,
        msg_id: u64,
        protocol_id: &str,
    ) {
        let mut removed_newest = false;
        if let Some(msgs) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            if let Some(pos) = msgs.iter().position(|(_, m)| m.id == msg_id) {
                removed_newest = pos + 1 == msgs.len();
                msgs.remove(pos);
            }
        }
        if let Some(session) = &self.live {
            if let Some(store) = &session.store {
                let _ = store.delete_message(chat_jid, protocol_id);
            }
        }
        // The message's saved download, if any, goes with it
        // (privacy audit A3).
        backend::remove_download_for_message(&backend::data_dir(), chat_jid, protocol_id);
        self.forget_live_message(chat_id, msg_id, Some(protocol_id));
        if removed_newest {
            self.refresh_preview_after_removal(chat_id);
        }
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Points the chat list row at the mirror's newest
    /// surviving message after messages were removed from a
    /// chat (the expiry pass's rule: the new newest message
    /// becomes the preview, or the preview empties when none
    /// survive), and writes the row through to the archive so
    /// its stored preview cannot resurrect a removed message's
    /// body on the next start. The row's timestamp is the
    /// session's recorded one, so the chat keeps its list
    /// position.
    pub(crate) fn refresh_preview_after_removal(&mut self, chat_id: u64) {
        let last = self.live.as_ref().and_then(|s| {
            s.messages.get(&chat_id).and_then(|v| v.last().map(|(ts, m)| (*ts, m.clone())))
        });
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            match &last {
                Some((ts, m)) => {
                    chat.preview = m.preview_text();
                    chat.time = fmt_time(*ts);
                }
                None => {
                    chat.preview = String::new();
                }
            }
        }
        if let Some(session) = &self.live {
            if let (Some(store), Some(jid)) = (&session.store, session.jids.get(&chat_id)) {
                if let Some(chat) = self.chats.iter().find(|c| c.id == chat_id) {
                    let ts = session.times.get(&chat_id).copied().unwrap_or(0);
                    let _ = store.upsert_chat(&chat_to_live(chat, jid, ts));
                }
            }
        }
    }

    /// Applies one backend message upsert. Returns true only when
    /// the upsert pushed a genuinely new incoming message (not an
    /// echo replacement and not a re-upsert of a known id).
    pub(crate) fn apply_message_upsert(&mut self, live: &LiveMessage) -> bool {
        let chat_id = jid_id(&live.chat_jid);
        // Set inside the session block when this upsert pushes a
        // genuinely new incoming message (not an echo replacement,
        // not a re-upsert of a known id): the OS notification queue
        // below consumes it, so notifications can never double-fire
        // on edits, stars or receipt-driven re-upserts.
        let mut new_incoming = false;
        if let Some(session) = &mut self.live {
            let msg_id = message_id(&live.chat_jid, &live.id);
            if let Some(reference) = &live.photo_ref {
                session.photo_refs.insert(msg_id, reference.clone());
            }
            let mut flags = session.flags.get(&msg_id).copied().unwrap_or(0);
            if live.starred {
                flags |= store::FLAG_STARRED;
            }
            if live.pinned {
                flags |= store::FLAG_PINNED_MESSAGE;
            }
            if flags != session.flags.get(&msg_id).copied().unwrap_or(0) {
                if flags == 0 {
                    // Same rule as `set_live_flag`: the flag map
                    // holds only messages that carry flags.
                    session.flags.remove(&msg_id);
                } else {
                    session.flags.insert(msg_id, flags);
                }
            }
            if session.store.is_some() {
                if self.event_batch.active {
                    // Deferred to the batch flush, which writes
                    // every queued upsert in one transaction; the
                    // flags are resolved there, not captured here.
                    self.event_batch.store_writes.push(live.clone());
                } else if let Some(store) = &session.store {
                    let _ = store.upsert_message(live, flags);
                }
            }
            session.jids.entry(chat_id).or_insert_with(|| live.chat_jid.clone());
            session.times.insert(chat_id, live.timestamp);
            let entry = session.messages.entry(chat_id).or_default();
            let converted = to_message_with_flags(live, flags);
            // Replace a pending local echo of this outgoing message.
            // Text echoes match on body; media echoes match on kind,
            // because their display bodies legitimately differ
            // between the echo and the backend upsert (a sticker
            // echoes as "Sticker" but upserts as "[sticker]", so
            // body matching could never retire it, logic review F7).
            // Oldest-first, so quick repeats retire in send order.
            let echo_pos = if live.from_me {
                entry.iter().position(|(_, m)| {
                    m.delivery == Some(Delivery::Sending)
                        && (m.body == live.body
                            || (!matches!(converted.kind, MessageKind::Text)
                                && std::mem::discriminant(&m.kind)
                                    == std::mem::discriminant(&converted.kind)))
                })
            } else {
                None
            };
            let mut is_new_push = false;
            if let Some(pos) = echo_pos {
                entry[pos] = (live.timestamp, converted);
            } else if let Some(pos) = entry.iter().position(|(_, m)| m.id == converted.id) {
                entry[pos] = (live.timestamp, converted);
            } else {
                entry.push((live.timestamp, converted));
                is_new_push = true;
            }

            new_incoming = is_new_push && !live.from_me;
            if self.event_batch.active {
                // Sorted once when the batch finishes. In-batch
                // readers of this vec look messages up by ID or
                // by minimum timestamp, never by position, so the
                // deferral cannot change what they find.
                self.event_batch.unsorted_chats.insert(chat_id);
            } else {
                entry.sort_by_key(|(ts, _)| *ts);
            }
        }
        // "Keep chats archived" OFF: a new incoming message brings
        // an archived chat back to the main list (WhatsApp's rule).
        if !live.from_me && !self.keep_archived {
            let was_archived =
                self.chats.iter().find(|c| c.id == chat_id).is_some_and(|c| c.archived);
            if was_archived {
                if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                    c.archived = false;
                }
                let snapshot = self.live.as_ref().and_then(|session| {
                    let jid = session.jids.get(&chat_id)?.clone();
                    let ts = session.times.get(&chat_id).copied().unwrap_or(0);
                    let chat = self.chats.iter().find(|c| c.id == chat_id)?.clone();
                    Some((chat, jid, ts))
                });
                if let (Some((chat, jid, ts)), Some(session)) = (snapshot, &self.live) {
                    if let Some(store) = &session.store {
                        let _ = store.upsert_chat(&crate::live::chat_to_live(&chat, &jid, ts));
                    }
                }
            }
        }
        // Keep the chat list preview in step even without a ChatUpsert.
        if !self.chats.iter().any(|c| c.id == chat_id) {
            self.apply_chat_upsert(&LiveChat {
                jid: live.chat_jid.clone(),
                title: live.sender.clone().unwrap_or_else(|| "Live chat".to_owned()),
                kind: if live.chat_jid.ends_with("@g.us") {
                    LiveChatKind::Group
                } else if live.chat_jid.ends_with("@newsletter") {
                    LiveChatKind::Channel
                } else {
                    LiveChatKind::Dm
                },
                preview: glanceable_text(live),
                preview_sender: None,
                timestamp: live.timestamp,
                unread: 0,
                archived: false,
                pinned: false,
                ephemeral_secs: 0,
                muted: false,
            });
        }
        // Protocol-id bookkeeping (receipts, reactions, read marks).
        // Read-by-action: a new incoming message queues for a read
        // receipt even when its chat is the one on screen. Arrival
        // alone never sends the receipt; the queue drains only
        // after a real user action (see `note_read_engagement`).
        let app_msg_id = message_id(&live.chat_jid, &live.id);
        self.protocol_ids.insert(app_msg_id, live.id.clone());
        if let Some(jid) = &live.sender_jid {
            self.sender_jids.insert(app_msg_id, jid.clone());
        }
        if new_incoming {
            self.unread_live.entry(chat_id).or_default().push(live.id.clone());
        }
        // Queue the OS notification for a genuinely new incoming
        // message; the per-frame drain applies the user's settings
        // (toggles, mute, preview, focus) before firing. History
        // never queues: a message stamped before this connection
        // came up is first-sync or offline catch-up, and a popup
        // for it would be noise about the past. The fake backend
        // is exempt: its clock is scripted, so the stamp
        // comparison is meaningless there and the demo keeps its
        // previous behavior.
        let history = self
            .live
            .as_ref()
            .is_some_and(|s| !s.fake && crate::notify::is_history(live.timestamp, s.connected_at));
        if new_incoming && !history {
            if let Some(chat) = self.chats.iter().find(|c| c.id == chat_id) {
                // Blocked state gates the popup like mute does:
                // the chat itself for a direct chat with a
                // blocked contact, the message's sender inside a
                // group. The server stops delivering a blocked
                // contact's messages, but anything already in
                // flight or synced can still land here.
                let blocked = self.blocked_chats.contains(&chat_id)
                    || live
                        .sender_jid
                        .as_deref()
                        .is_some_and(|j| self.blocked_chats.contains(&jid_id(j)));
                self.pending_notifications.push(crate::notify::NotifyRequest {
                    chat_id,
                    title: chat.title.clone(),
                    kind: chat.kind,
                    muted: chat.muted,
                    blocked,
                    sender: live.sender.clone(),
                    // The queued body is the only text the popup
                    // planner can ever show, so a view-once
                    // message queues its placeholder here: its
                    // caption cannot reach a popup (or the lock
                    // screen) whatever the preview toggle says.
                    body: glanceable_text(live),
                });
            }
        }
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.preview = glanceable_text(live);
            chat.time = fmt_time(live.timestamp);
            // Snippet anatomy for the row: ticks when the latest
            // message is ours, a media label when it is not text.
            chat.preview_mine = live.from_me;
            chat.preview_delivery = if live.from_me { Some(Delivery::Delivered) } else { None };
            chat.preview_media = core::model::PreviewMedia::of(&to_kind(&live.kind));
            // Only a genuinely new message raises the unread badge;
            // re-upserts (edits, votes, stars) must not (F8). The
            // badge also rises in the currently open chat: under
            // read-by-action the message is not read yet just
            // because it arrived while the screen was open, and it
            // clears when the user acts (see the flush in
            // conversation.rs).
            if new_incoming {
                chat.unread += 1;
            }
        }
        // The archive row must carry the badge the mirror now
        // shows: the increment above is the only place the count
        // grows, and the arrival upsert's own row write carries
        // the pre-increment count (merged from the known value,
        // see apply_chat_upsert), so without this write a restart
        // would reload a smaller count than the list showed.
        // Inside a batch the write defers to the flush, like the
        // message rows.
        if new_incoming {
            if self.event_batch.active {
                self.event_batch.chat_rows_dirty.insert(chat_id);
            } else {
                self.persist_chat_row(chat_id);
            }
        }
        self.sort_live_chats();
        if chat_id == self.selected_chat {
            self.sync_conversation();
        }
        new_incoming
    }

    /// Sorts the chat list by latest activity. Inside an event
    /// batch the sort is deferred to the end of the batch (see
    /// [`EventBatch`]): a burst would otherwise re-sort the whole
    /// list per event. The list is only ever rendered between
    /// batches, so the deferral is invisible.
    fn sort_live_chats(&mut self) {
        if self.event_batch.active {
            self.event_batch.chats_dirty = true;
            return;
        }
        self.sort_live_chats_now();
    }

    /// The immediate chat-list sort behind [`App::sort_live_chats`].
    fn sort_live_chats_now(&mut self) {
        if let Some(session) = &self.live {
            let times = &session.times;
            self.chats.sort_by(|a, b| {
                times.get(&b.id).copied().unwrap_or(0).cmp(&times.get(&a.id).copied().unwrap_or(0))
            });
        }
    }

    /// Drops every per-message helper entry kept for one live
    /// message: its protocol ID, sender JID, receipt log,
    /// reaction state, archive flags, and any pending read
    /// receipt. Called when a message leaves the session mirror
    /// (today: a disappearing message whose timer ran out), so
    /// the helper maps stay scoped to the messages the mirror
    /// actually holds instead of gaining an entry for every
    /// message ever seen. Nothing here is consulted for a
    /// message that is no longer rendered, so lookups for the
    /// messages that remain are unaffected.
    pub(crate) fn forget_live_message(
        &mut self,
        chat_id: u64,
        msg_id: u64,
        protocol_id: Option<&str>,
    ) {
        self.protocol_ids.remove(&msg_id);
        if let Some(session) = &mut self.live {
            session.photo_refs.remove(&msg_id);
        }
        self.media.photo_fetch.remove(&msg_id);
        self.sender_jids.remove(&msg_id);
        self.receipt_log.remove(&msg_id);
        self.my_reactions.remove(&msg_id);
        self.reaction_actors.remove(&format!("{chat_id}:{msg_id}"));
        if let Some(pid) = protocol_id {
            let mut drained = false;
            if let Some(pending) = self.unread_live.get_mut(&chat_id) {
                pending.retain(|p| p != pid);
                drained = pending.is_empty();
            }
            if drained {
                self.unread_live.remove(&chat_id);
            }
        }
        if let Some(session) = &mut self.live {
            session.flags.remove(&msg_id);
        }
    }

    /// Sets (or clears with `flags = 0`) archive flags for one
    /// message and persists them when a store row exists.
    pub(crate) fn set_live_flag(&mut self, chat_id: u64, msg_id: u64, bit: i64, on: bool) {
        let protocol_id = self.protocol_ids.get(&msg_id).cloned();
        if let Some(session) = &mut self.live {
            let flags = {
                let entry = session.flags.entry(msg_id).or_insert(0);
                if on {
                    *entry |= bit;
                } else {
                    *entry &= !bit;
                }
                *entry
            };
            if flags == 0 {
                // Absence and zero mean the same to every reader
                // (all read with `unwrap_or(0)`); keep the map
                // scoped to messages that actually carry flags.
                // The store write below still persists the zero.
                session.flags.remove(&msg_id);
            }
            // Persist through a targeted flags-only UPDATE. The old
            // path rebuilt the whole row from the UI model, which
            // zeroed the timestamp and erased quote, preview and
            // view-once data on every star/pin/revoke (logic review
            // F1, fixed in the store in v0.25).
            if let (Some(store), Some(jid), Some(pid)) =
                (&session.store, session.jids.get(&chat_id), protocol_id)
            {
                let _ = store.set_message_flags(jid, &pid, flags);
            }
        }
    }

    /// Marks a view-once message opened, in memory and in the
    /// archive. The in-memory set drives the bubble's opened
    /// presentation for this session; the stored flag
    /// ([`store::FLAG_VIEW_ONCE_OPENED`]) is what start_live
    /// seeds that set from after a restart, so an opened
    /// view-once message never presents as unopened again:
    /// once opened, it stays opened (privacy audit D13).
    pub(crate) fn mark_view_once_opened(&mut self, msg_id: u64) {
        if !self.media.opened_view_once.insert(msg_id) {
            return;
        }
        let chat_id = self.live.as_ref().and_then(|session| {
            session
                .messages
                .iter()
                .find(|(_, msgs)| msgs.iter().any(|(_, m)| m.id == msg_id))
                .map(|(chat_id, _)| *chat_id)
        });
        if let Some(chat_id) = chat_id {
            self.set_live_flag(chat_id, msg_id, store::FLAG_VIEW_ONCE_OPENED, true);
        }
    }

    /// Whether the generated demo photo may stand in for a photo
    /// message's pixels: demo mode, and the fake backend's test
    /// mode (whose photos are scripted fixtures, labelled test
    /// data). In a real live session it may never: a received
    /// photo shows its own bytes or an honest placeholder, never
    /// a substitute image.
    pub(crate) fn photo_demo_allowed(&self) -> bool {
        !self.live_mode || self.live.as_ref().is_some_and(|s| s.fake)
    }

    /// What a photo message's tile may honestly show right now
    /// (see [`crate::media::PhotoTile`]): the decoded real bytes
    /// when they are here, the demo fixture where that is the
    /// disclosed content, otherwise the fetch state.
    pub(crate) fn photo_tile(&self, msg_id: u64) -> crate::media::PhotoTile {
        if self.media.textures.contains_key(&msg_id) {
            return crate::media::PhotoTile::Real;
        }
        if self.photo_demo_allowed() {
            return crate::media::PhotoTile::Demo;
        }
        match self.media.photo_fetch.get(&msg_id) {
            Some(crate::state::PhotoFetchState::Loading) => crate::media::PhotoTile::Loading,
            Some(crate::state::PhotoFetchState::Failed) => crate::media::PhotoTile::Failed,
            None => crate::media::PhotoTile::Idle,
        }
    }

    /// Starts the display fetch for one photo message of the open
    /// chat, unless its bytes are already here or a fetch is
    /// already running. The fetch passes the message's archived
    /// re-download reference along, so a photo the backend no
    /// longer holds in its session cache is still fetchable.
    /// Only real live sessions fetch: demo and test mode paint
    /// their fixture photo instead.
    pub(crate) fn ensure_photo_fetch(&mut self, msg_id: u64) {
        if self.photo_demo_allowed() || self.media.textures.contains_key(&msg_id) {
            return;
        }
        if self.media.photo_fetch.contains_key(&msg_id) {
            return;
        }
        let (chat_jid, photo_ref) = {
            let Some(session) = &self.live else { return };
            let Some(chat_jid) = session.jids.get(&self.selected_chat).cloned() else {
                return;
            };
            (chat_jid, session.photo_refs.get(&msg_id).cloned())
        };
        let Some(protocol_id) = self.protocol_ids.get(&msg_id).cloned() else {
            // A live photo without a protocol id can never be
            // addressed; record the failure instead of spinning.
            self.media.photo_fetch.insert(msg_id, crate::state::PhotoFetchState::Failed);
            return;
        };
        self.media.photo_fetch.insert(msg_id, crate::state::PhotoFetchState::Loading);
        self.send_backend(BackendCommand::FetchPhoto {
            chat_jid,
            message_id: protocol_id,
            photo_ref,
        });
    }

    /// Re-runs a failed display fetch (opening the viewer on a
    /// failed photo is the retry gesture).
    pub(crate) fn retry_photo_fetch(&mut self, msg_id: u64) {
        if self.media.photo_fetch.get(&msg_id) == Some(&crate::state::PhotoFetchState::Failed) {
            self.media.photo_fetch.remove(&msg_id);
        }
        self.ensure_photo_fetch(msg_id);
    }

    /// Closes the photo viewer on one message. A view-once photo
    /// consumes its single view here, and only under the honesty
    /// rule: in a real live session the view is spent solely when
    /// the photo's real pixels were actually painted during this
    /// viewing (tracked in `MediaState::view_once_shown`). When
    /// they never arrived, the allowance survives, the failure is
    /// said out loud, and the photo can be opened again later.
    /// Demo and test mode keep the standing behaviour (their
    /// fixture photo is the content, so closing spends the view).
    /// A spent live view also drops the decoded pixels and the
    /// archived re-download reference, so nothing can bring the
    /// photo back afterwards.
    pub(crate) fn close_photo_viewer(&mut self, id: u64, view_once: bool) {
        if view_once {
            let spent = self.photo_demo_allowed() || self.media.view_once_shown == Some(id);
            if spent {
                self.mark_view_once_opened(id);
                if !self.photo_demo_allowed() {
                    self.media.textures.remove(&id);
                    self.drop_photo_ref(id);
                }
            } else {
                self.notice = Some(crate::i18n::t("photo.view_once_not_loaded"));
            }
        }
        if self.media.view_once_shown == Some(id) {
            self.media.view_once_shown = None;
        }
        self.media.viewing_photo = None;
    }

    /// Removes one message's photo re-download reference from the
    /// session map and from the archive.
    fn drop_photo_ref(&mut self, msg_id: u64) {
        let protocol_id = self.protocol_ids.get(&msg_id).cloned();
        let Some(session) = &mut self.live else { return };
        session.photo_refs.remove(&msg_id);
        let chat_jid = session
            .messages
            .iter()
            .find(|(_, msgs)| msgs.iter().any(|(_, m)| m.id == msg_id))
            .and_then(|(chat_id, _)| session.jids.get(chat_id).cloned());
        if let (Some(store), Some(chat_jid), Some(protocol_id)) =
            (&session.store, chat_jid, protocol_id)
        {
            let _ = store.clear_photo_ref(&chat_jid, &protocol_id);
        }
    }

    /// Handles a delete-for-everyone for one message: the native
    /// behaviour, the message is replaced by the placeholder.
    pub(crate) fn apply_revoke(&mut self, chat_id: u64, msg_id: u64) {
        if let Some(entry) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            for (_, m) in entry.iter_mut() {
                if m.id == msg_id {
                    m.deleted = true;
                    m.body = "This message was deleted".to_owned();
                    m.reactions.clear();
                }
            }
        }
        self.set_live_flag(chat_id, msg_id, store::FLAG_DELETED, true);
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Sends a composing/paused chat-state for the selected live
    /// chat, only when the typing-indicators toggle is ON and the
    /// state actually changed (no per-keystroke spam). Tracked per
    /// chat: switching chats mid-composing pauses the old chat
    /// instead of orphaning its indicator (logic review F16).
    pub(crate) fn set_typing(&mut self, typing: bool) {
        if !should_send_typing(self.typing_indicators, self.live_mode) {
            return;
        }
        let chat_id = self.selected_chat;
        if typing {
            if self.typing_sent_for == Some(chat_id) {
                return;
            }
            if let Some(old) = self.typing_sent_for.take() {
                self.send_typing_to(old, false);
            }
            self.typing_sent_for = Some(chat_id);
            self.send_typing_to(chat_id, true);
        } else if let Some(old) = self.typing_sent_for.take() {
            self.send_typing_to(old, false);
        }
    }

    fn send_typing_to(&mut self, chat_id: u64, typing: bool) {
        if let Some(jid) = self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned()) {
            self.send_backend(BackendCommand::SetTyping { chat_jid: jid, typing });
        }
    }

    /// Applies a typing-indicators toggle change from the
    /// Privacy page. Turning the toggle off must also retract a
    /// composing state already sent: the peer would otherwise
    /// keep seeing "typing..." until it times out server-side,
    /// from a client whose user just said indicators are off.
    /// The retraction goes through [`App::send_typing_to`], which
    /// does not consult the toggle: the gate in
    /// [`App::set_typing`] would swallow it.
    pub(crate) fn set_typing_indicators(&mut self, on: bool) {
        self.typing_indicators = on;
        if !on {
            if let Some(chat_id) = self.typing_sent_for.take() {
                self.send_typing_to(chat_id, false);
            }
        }
        self.persist_settings();
    }

    /// Adopts a privacy state fetched from the account. The map
    /// itself drives the account rows on the Privacy page, and
    /// one value in it is also a local setting: read receipts.
    /// That flag gates whether this client sends read receipts at
    /// all, and it is loaded from the settings file at startup,
    /// before any fetch lands, so the server's value (the user
    /// may have changed it on their phone) is adopted into the
    /// flag and written through to the persisted settings. Left
    /// unreconciled the two silently disagree in the exposing
    /// direction: receipts the account has off keep going out
    /// until the toggle here happens to be flipped. Only the
    /// spellings the toggle itself writes are adopted; an
    /// unrecognized value leaves the flag alone. Demo mode never
    /// adopts: the fake backend's scripted state is not an
    /// account and must not rewrite real settings.
    fn apply_privacy_state(&mut self, map: std::collections::BTreeMap<String, String>) {
        if self.live_mode {
            if let Some(on) = server_read_receipts(map.get("readreceipts")) {
                if on != self.read_receipts {
                    self.read_receipts = on;
                    self.persist_settings();
                }
            }
        }
        self.privacy_state = map;
    }

    /// Adopts the account-wide default disappearing timer the
    /// server pushed (a change made on another device, the phone
    /// included). The control is a local value loaded from the
    /// settings file at startup; without this adoption a default
    /// changed on the phone and the value this client applies to
    /// new chats silently disagree, in the exposing direction
    /// both ways. There is no fetch for the account default (the
    /// library offers only the set), so this push is the only
    /// reconciliation channel, the same server-wins rule as
    /// [`App::apply_privacy_state`]. Demo mode never adopts.
    fn apply_default_disappearing_timer(&mut self, duration_secs: u32) {
        if self.live_mode && duration_secs != self.default_ephemeral_secs {
            self.default_ephemeral_secs = duration_secs;
            self.persist_settings();
        }
    }

    /// Persists the blocked-chat set to its home, the encrypted
    /// archive store (privacy audit A6: the set used to ride in
    /// the plaintext settings file as FNV-1a hashes of
    /// phone-number JIDs, reversible by dictionary over the
    /// phone-number space). No live session (demo mode, or the
    /// archive failed to open) means no persisted copy: the set
    /// then lives in memory for the session, and in live mode
    /// the server's connect-time blocklist re-seeds it anyway.
    pub(crate) fn persist_blocked_chats(&self) {
        if let Some(store) = self.live.as_ref().and_then(|s| s.store.as_ref()) {
            let mut ids: Vec<u64> = self.blocked_chats.iter().copied().collect();
            ids.sort_unstable();
            let _ = store.set_blocked_chats(&ids);
        }
    }

    /// Names this session already knows for people, keyed by
    /// JID: the titles of 1:1 chats (the push name the chat list
    /// resolved for that partner) and the sender names carried
    /// on mirrored messages. Built once per member/request
    /// fetch so resolving a whole list stays a single pass over
    /// the mirror. A chat title that is just the partner's own
    /// number (the fallback when no push name was ever seen) is
    /// not a name and is skipped.
    fn known_person_names(&self) -> HashMap<String, String> {
        let mut names: HashMap<String, String> = HashMap::new();
        let Some(session) = &self.live else {
            return names;
        };
        for (chat_id, jid) in &session.jids {
            let Some(chat) = self.chats.iter().find(|c| c.id == *chat_id) else {
                continue;
            };
            if chat.kind != ChatKind::Dm {
                continue;
            }
            let title = chat.title.trim();
            if !title.is_empty() && title != jid_user_part(jid) {
                names.insert(jid.clone(), chat.title.clone());
            }
        }
        for messages in session.messages.values() {
            for (_, msg) in messages {
                let Some(name) = &msg.sender else { continue };
                if name.trim().is_empty() {
                    continue;
                }
                if let Some(jid) = self.sender_jids.get(&msg.id) {
                    names.entry(jid.clone()).or_insert_with(|| name.clone());
                }
            }
        }
        names
    }

    /// Resolves backend member rows into renderable rows (see
    /// [`crate::state::MemberRow`]): the label is the best name
    /// available for the participant and the detail the number
    /// line. Backend addresses pass through untouched; they are
    /// command targets, never labels.
    fn resolve_member_rows(
        &self,
        members: Vec<(String, String, bool)>,
    ) -> Vec<crate::state::MemberRow> {
        let known = self.known_person_names();
        members
            .into_iter()
            .map(|(address, name, admin)| {
                let (label, named) = resolve_person_label(&name, &address, &known);
                crate::state::MemberRow {
                    detail: member_detail(&address, named),
                    address,
                    label,
                    admin,
                }
            })
            .collect()
    }

    /// Resolves backend join-request rows into (jid, label)
    /// rows, with the same label rule as member rows.
    fn resolve_request_rows(&self, requests: Vec<(String, String)>) -> Vec<(String, String)> {
        let known = self.known_person_names();
        requests
            .into_iter()
            .map(|(jid, name)| {
                let (label, _) = resolve_person_label(&name, &jid, &known);
                (jid, label)
            })
            .collect()
    }

    /// Applies a read-receipts toggle change from the Privacy
    /// page. The local flag flips at once (it gates receipt
    /// sending, so turning receipts off takes effect immediately),
    /// the choice is persisted, and in live mode the account
    /// setting follows via SetPrivacy. The backend answers a
    /// successful change with a fresh PrivacyState, which
    /// [`App::apply_privacy_state`] adopts as confirmation; a
    /// failure surfaces as a notice and the same fetch re-adopts
    /// the value the server kept, so the toggle never keeps
    /// claiming a state the server rejected.
    pub(crate) fn set_read_receipts(&mut self, on: bool) {
        self.read_receipts = on;
        self.persist_settings();
        if self.live_mode {
            self.send_backend(BackendCommand::SetPrivacy {
                category: "readreceipts".to_owned(),
                value: if on { "all" } else { "none" }.to_owned(),
            });
        }
    }

    /// Cycles one account-privacy row on the Privacy page to its
    /// next audience and sends it to the account. The displayed
    /// value is not touched here: it comes from `privacy_state`,
    /// which only a fresh fetch updates, on success and on failure
    /// alike (the backend re-asserts the fetched state after a
    /// rejected change), so the page never claims a value the
    /// server did not take.
    pub(crate) fn cycle_privacy(&mut self, category: &str) {
        let current = self.privacy_state.get(category).cloned().unwrap_or_default();
        let Some(next) = next_privacy_audience(category, &current) else {
            return;
        };
        if self.live_mode {
            self.send_backend(BackendCommand::SetPrivacy {
                category: category.to_owned(),
                value: next.to_owned(),
            });
        }
    }

    /// The Privacy page's entry point for cycling a row. Leaving
    /// `contact_blacklist` is the one cycle step that destroys
    /// data this client cannot restore: the value names an
    /// exception list configured on the phone, and replacing it
    /// with a plain audience discards the association with that
    /// list (the backend refuses to set the list back without
    /// its contacts). That step asks first, through the same
    /// confirmation the other destructive actions use; every
    /// other step is a reversible value-to-value change and
    /// cycles at once.
    pub(crate) fn request_cycle_privacy(&mut self, category: &str, label: &str) {
        let current = self.privacy_state.get(category).cloned().unwrap_or_default();
        if current == "contact_blacklist" {
            self.ui_confirm = Some(crate::state::UiConfirm::ReplaceExceptionList {
                category: category.to_owned(),
                label: label.to_owned(),
            });
            return;
        }
        self.cycle_privacy(category);
    }

    /// Records a real read action in the selected chat (the
    /// conversation screen calls this when the user scrolls to the
    /// newest messages, focuses or types in the composer, or sends
    /// a reply) and immediately flushes any pending read receipts
    /// for it. Returns how many messages were marked read.
    pub(crate) fn note_read_engagement(&mut self) -> usize {
        let chat_id = self.selected_chat;
        if let Some(session) = &mut self.live {
            session.engaged_chats.insert(chat_id);
        }
        self.mark_selected_chat_read()
    }

    /// Marks the selected live chat's unread incoming messages as
    /// read. Two gates apply: the read-receipts toggle must be ON
    /// (the privacy contract), and the user must have performed a
    /// real read action in this chat since the pending messages
    /// arrived (the read-by-action rule). Without engagement the
    /// pending protocol ids stay queued and nothing is sent:
    /// selection changes, screen switches and passive arrivals all
    /// call this and all stop at the gate. Returns how many
    /// messages were marked.
    pub(crate) fn mark_selected_chat_read(&mut self) -> usize {
        let chat_id = self.selected_chat;
        let engaged = self.live.as_ref().is_some_and(|s| s.engaged_chats.contains(&chat_id));
        if !should_send_receipt(self.read_receipts, self.live_mode, engaged) {
            return 0;
        }
        let Some(session) = &self.live else {
            return 0;
        };
        let Some(jid) = session.jids.get(&chat_id).cloned() else {
            return 0;
        };
        let protocol_ids: Vec<String> = self.unread_live.remove(&chat_id).unwrap_or_default();
        // The engagement is spent by this mark, even when the
        // queue was empty: messages that arrive later need a fresh
        // action before their receipts go out.
        if let Some(session) = &mut self.live {
            session.engaged_chats.remove(&chat_id);
        }
        if protocol_ids.is_empty() {
            return 0;
        }
        let count = protocol_ids.len();
        self.send_backend(BackendCommand::MarkAsRead {
            chat_jid: jid,
            sender_jid: None,
            message_ids: protocol_ids,
        });
        count
    }
}

/// Counts reactions per emoji from the actor → emoji map.
pub(crate) fn count_reactions(
    actors: &std::collections::HashMap<String, String>,
) -> Vec<(String, u32)> {
    let mut counts: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for emoji in actors.values() {
        *counts.entry(emoji.as_str()).or_insert(0) += 1;
    }
    let mut out: Vec<(String, u32)> = counts.into_iter().map(|(e, n)| (e.to_owned(), n)).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

// NOTE: the old `message_to_live` / `kind_to_live` rebuilders were
// removed in v0.25. They existed only so flag changes could be
// re-upserted, and rebuilding a store row from the UI model is what
// corrupted archived rows (logic review F1). Flag changes now go
// through `Store::set_message_flags`; nothing may rebuild a stored
// message from a UI `Message` again.

/// The read-receipts privacy contract, as a pure predicate so it is
/// testable without a window: receipts are sent only in live mode
/// with the toggle ON. With the toggle OFF the app never calls
/// mark-as-read, so senders keep grey ticks.
#[must_use]
pub(crate) fn should_mark_read(read_receipts: bool, live_mode: bool) -> bool {
    read_receipts && live_mode
}

/// The user part of a JID (before the `@`).
fn jid_user_part(jid: &str) -> &str {
    jid.split('@').next().unwrap_or(jid)
}

/// A masked stand-in label for a person whose name is unknown:
/// enough of the identifier to tell rows apart, never the full
/// number or LID and never a full JID (`6281234567890@…` becomes
/// `+628 ••• 7890`; a LID is masked the same way without the
/// `+`). A user part that is not a long digit string is not a
/// phone-like identifier and is returned unchanged.
#[must_use]
pub(crate) fn masked_identifier(jid: &str) -> String {
    let user = jid_user_part(jid);
    if user.len() < 7 || !user.bytes().all(|b| b.is_ascii_digit()) {
        return user.to_owned();
    }
    let masked = format!("{} ••• {}", &user[..3], &user[user.len() - 4..]);
    if jid.ends_with("@s.whatsapp.net") {
        format!("+{masked}")
    } else {
        masked
    }
}

/// The rendered label for one member or join-request row, and
/// whether that label is a real name. The name the backend
/// carried wins (for members, the name the server rendered);
/// next, a name this session already knows for the JID (see
/// `known_person_names`); last, a masked identifier, so
/// a raw phone number or LID is never the label (privacy
/// finding D5).
#[must_use]
pub(crate) fn resolve_person_label(
    backend_name: &str,
    jid: &str,
    known: &HashMap<String, String>,
) -> (String, bool) {
    let backend_name = backend_name.trim();
    if !backend_name.is_empty() {
        return (backend_name.to_owned(), true);
    }
    if let Some(name) = known.get(jid) {
        return (name.clone(), true);
    }
    (masked_identifier(jid), false)
}

/// The secondary line under a member row's label: the phone
/// number, only under a resolved name and only for a
/// number-addressed participant. A masked label never gets the
/// full number beneath it (that would undo the masking), and a
/// LID-addressed row shows no number at all, matching how
/// WhatsApp hides numbers in LID-mode groups.
#[must_use]
pub(crate) fn member_detail(jid: &str, named: bool) -> Option<String> {
    if named && jid.ends_with("@s.whatsapp.net") {
        Some(format!("+{}", jid_user_part(jid)))
    } else {
        None
    }
}

/// What a fetched `readreceipts` value means for the local
/// read-receipts toggle: `all` is ON and `none` is OFF, the two
/// spellings the toggle itself writes. Any other value (or a
/// state that does not carry the category) is not the toggle's
/// to adopt and returns `None`.
#[must_use]
pub(crate) fn server_read_receipts(fetched: Option<&String>) -> Option<bool> {
    match fetched.map(String::as_str) {
        Some("all") => Some(true),
        Some("none") => Some(false),
        _ => None,
    }
}

/// The audiences one account-privacy category accepts, in cycle
/// order (protocol spellings), matching the library's validity
/// table for the categories the Privacy page shows. `online`
/// accepts only `all` and `match_last_seen`; the other four rows
/// share `all`, `contacts`, `none`. `contact_blacklist` is
/// deliberately not a stop on any cycle: it names a per-contact
/// exception list that a bare value change cannot express, and
/// sending it without the list would discard the list the phone
/// configured. It is display-only here (see
/// [`privacy_value_label`]).
#[must_use]
pub(crate) fn privacy_audiences(category: &str) -> &'static [&'static str] {
    match category {
        "online" => &["all", "match_last_seen"],
        "last" | "profile" | "status" | "groupadd" => &["all", "contacts", "none"],
        _ => &[],
    }
}

/// The next audience for a privacy row, cycling within
/// [`privacy_audiences`]. A current value outside the cycle is
/// never "corrected" in place; cycling from it lands somewhere
/// valid and predictable: `contact_blacklist` (contacts with an
/// exception list this page cannot edit) advances to the stop
/// after "contacts", and any other unrecognized value restarts
/// the cycle at its first audience.
#[must_use]
pub(crate) fn next_privacy_audience(category: &str, current: &str) -> Option<&'static str> {
    let cycle = privacy_audiences(category);
    if cycle.is_empty() {
        return None;
    }
    let pos = cycle.iter().position(|v| *v == current).or_else(|| {
        // `contact_blacklist` refines the "contacts" audience, so
        // its cycle position is that of "contacts".
        if current == "contact_blacklist" {
            cycle.iter().position(|v| *v == "contacts")
        } else {
            None
        }
    });
    Some(match pos {
        Some(i) => cycle[(i + 1) % cycle.len()],
        None => cycle[0],
    })
}

/// The button label for a fetched privacy value: the audience's
/// plain name, in the wording WhatsApp itself uses. A value this
/// page does not know is shown verbatim, never silently presented
/// as a neighboring audience.
#[must_use]
pub(crate) fn privacy_value_label(value: &str) -> String {
    match value {
        "all" => "Everyone".to_owned(),
        "contacts" => "My contacts".to_owned(),
        "none" => "Nobody".to_owned(),
        "contact_blacklist" => "My contacts except…".to_owned(),
        "match_last_seen" => "Same as last seen".to_owned(),
        other => other.to_owned(),
    }
}

/// The full receipt gate: the privacy contract above, plus the
/// read-by-action rule. A receipt goes out only when the user did
/// something that means "I have read this" in the chat: scrolled
/// to the newest messages, focused or typed in the composer, or
/// sent a reply. A message that merely arrived while its chat
/// screen was open stays unmarked.
#[must_use]
pub(crate) fn should_send_receipt(read_receipts: bool, live_mode: bool, engaged: bool) -> bool {
    should_mark_read(read_receipts, live_mode) && engaged
}

/// The typing-indicators privacy contract: composing state is sent
/// only in live mode with the toggle ON.
#[must_use]
pub(crate) fn should_send_typing(typing_indicators: bool, live_mode: bool) -> bool {
    typing_indicators && live_mode
}

impl crate::state::App {
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
        let (was_archived, was_pinned, was_muted) = self
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .or_else(|| self.detached_chat.as_ref().filter(|c| c.id == chat_id))
            .map_or((archived, pinned, muted), |c| (c.archived, c.pinned, c.muted));
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.archived = archived;
            c.pinned = pinned;
            c.muted = muted;
        } else if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            // The detached announcement chat lives outside the
            // chat list, so the header menu actions fall back to
            // it instead of silently doing nothing.
            c.archived = archived;
            c.pinned = pinned;
            c.muted = muted;
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

    /// The live JID for a chat id, when known.
    pub(crate) fn live_jid(&self, chat_id: u64) -> Option<String> {
        self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned())
    }
}

/// Builds the archive row for a core chat (inverse of `to_chat`).
pub(crate) fn chat_to_live(chat: &Chat, jid: &str, timestamp: i64) -> LiveChat {
    LiveChat {
        jid: jid.to_owned(),
        title: chat.title.clone(),
        kind: match chat.kind {
            core::model::ChatKind::Group => LiveChatKind::Group,
            core::model::ChatKind::Channel => LiveChatKind::Channel,
            _ => LiveChatKind::Dm,
        },
        preview: chat.preview.clone(),
        preview_sender: chat.preview_sender.clone(),
        timestamp,
        unread: chat.unread,
        archived: chat.archived,
        pinned: chat.pinned,
        ephemeral_secs: chat.ephemeral_secs,
        muted: chat.muted,
    }
}

#[cfg(test)]
mod day_label_tests {
    use super::civil_from_days;

    #[test]
    fn civil_dates_convert_on_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29), "leap day");
        assert_eq!(civil_from_days(20_771), (2026, 11, 14));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
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
    fn live_app() -> (
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
            store: Some(store::Store::open_in_memory().unwrap()),
            jids: HashMap::new(),
            messages: HashMap::new(),
            times: HashMap::new(),
            fake: true,
            flags: HashMap::new(),
            engaged_chats: std::collections::HashSet::new(),
            photo_refs: HashMap::new(),
        });
        app.live_mode = true;
        (app, evt_tx, ctx, cmd_rx)
    }

    fn incoming(id: &str, ts: i64) -> BackendEvent {
        BackendEvent::MessageUpsert(LiveMessage::text(
            id,
            CHAT_JID,
            Some("Sender".to_owned()),
            format!("message {id}"),
            ts,
            false,
        ))
    }

    fn drain_all(app: &mut App, ctx: &egui::Context) {
        for _ in 0..64 {
            app.poll_backend(ctx);
            if app.event_queue.is_empty() {
                return;
            }
        }
        panic!("the event queue did not drain");
    }

    fn stored_messages(app: &App) -> &Vec<(i64, Message)> {
        app.live.as_ref().unwrap().messages.get(&jid_id(CHAT_JID)).expect("the chat has messages")
    }

    /// Privacy finding B1: a received message containing a URL
    /// must never make the client fetch a preview from the
    /// sender-chosen host (that leaks the recipient's address and
    /// online timing). Applying the upsert leaves the message
    /// without a preview; previews exist only when the sender's
    /// client attached one.
    #[test]
    fn received_url_message_gets_no_fetched_preview() {
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
    fn sender_attached_preview_reaches_the_mirror() {
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
    fn view_once_photo(id: &str, caption: &str, ts: i64) -> LiveMessage {
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
    fn view_once_caption_stays_out_of_preview_popups_and_archive() {
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
    fn view_once_opened_state_persists_with_the_message() {
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
    fn photo_message(id: &str, ts: i64, view_once: bool) -> LiveMessage {
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

    /// A small real image, encoded in memory, standing in for
    /// fetched photo bytes.
    fn png_bytes() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 40, 40, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    /// Turns the harness session into a real one: the harness
    /// runs the fake backend (test mode, where the fixture photo
    /// is the disclosed content), while the photo-fetch behaviour
    /// under test is the real live session's.
    fn make_real(app: &mut App) {
        app.live.as_mut().unwrap().fake = false;
    }

    /// Item 109: a real live session never paints the demo
    /// fixture for a received photo. Before any fetch the tile is
    /// Idle, the fetch command carries the archived reference,
    /// and a second request while one is running sends nothing.
    #[test]
    fn photo_fetch_sends_the_command_with_the_archived_reference() {
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
    fn photo_bytes_install_the_real_texture() {
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
    fn photo_fetch_failure_marks_the_tile_failed() {
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

    /// Item 109, the view-once half: closing the viewer on a
    /// view-once photo whose real pixels were never shown does
    /// NOT spend the view. The opened set stays empty, the user
    /// is told, and the archived re-download reference survives
    /// so the photo can still be opened later.
    #[test]
    fn view_once_close_without_shown_pixels_keeps_the_view() {
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
    fn view_once_close_after_shown_pixels_spends_the_view() {
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
    fn view_once_close_in_test_mode_still_spends_the_view() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(photo_message("vo-ph", BASE_TS, true))).unwrap();
        drain_all(&mut app, &ctx);
        let mid = message_id(CHAT_JID, "vo-ph");
        app.media.viewing_photo = Some(mid);
        app.close_photo_viewer(mid, true);
        assert!(app.media.opened_view_once.contains(&mid));
    }

    /// Privacy finding C6: backend error text is largely library
    /// Display output and can embed server-supplied strings,
    /// phone numbers, and secret shapes. The surfaces that
    /// display it (the notice banner, the pre-link status line)
    /// must receive the scrubbed form.
    #[test]
    fn backend_error_text_is_scrubbed_before_display() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        // Pre-link, so the status line takes the message too.
        app.live.as_mut().unwrap().status = LinkStatus::Starting;
        tx.send(BackendEvent::Error {
            message: "server refused the call from +62 812-3456-7890, token=zzz999888".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let notice = app.notice.clone().expect("the error surfaces as a notice");
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
    fn plain_backend_error_text_is_displayed_unchanged() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::Error {
            message: "The group settings could not be changed.".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.notice.as_deref(), Some("The group settings could not be changed."));
    }

    /// The removal-refresh path (delete-for-me, clear, expiry)
    /// recomputes the preview from the mirror: falling back to
    /// a view-once message must yield its placeholder, not the
    /// caption its mirror body still carries for the viewer,
    /// and the refreshed preview is written through to the
    /// archive row.
    #[test]
    fn preview_falls_back_to_the_view_once_placeholder() {
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
    fn burst_drains_over_multiple_frames_in_order() {
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
    fn receipt_and_edit_in_the_same_burst_still_apply() {
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

    /// The notification flood scenario: on a real (non-fake)
    /// session, a message stamped before the connection came up
    /// is offline catch-up and must not queue an OS notification,
    /// while a message stamped after it is live and must. Both
    /// still land in the chat and raise the unread badge: only
    /// the popup is gated.
    #[test]
    fn history_catch_up_queues_no_notification_but_live_does() {
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
    fn seed_named_people(
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
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
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
    fn member_rows_lead_with_names_and_mask_the_unknown() {
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
    fn join_request_rows_use_the_same_label_rule() {
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
    fn masked_identifier_never_shows_a_full_number_or_jid() {
        assert_eq!(masked_identifier("6281234567890@s.whatsapp.net"), "+628 ••• 7890");
        assert_eq!(masked_identifier("123456789012345@lid"), "123 ••• 2345");
        // Not phone-like: returned as the bare user part.
        assert_eq!(masked_identifier("12345@s.whatsapp.net"), "12345");
        let masked = masked_identifier(STRANGER_JID);
        assert!(!masked.contains("6284444444444"));
        assert!(!masked.contains('@'));
    }

    #[test]
    fn person_label_prefers_backend_then_known_then_masked() {
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
mod key_decision_tests {
    use super::*;

    fn unreadable_key() -> store::StoreError {
        store::StoreError::Io(std::io::Error::other(
            "the database key could not be read from the system keyring: locked",
        ))
    }

    /// Audit finding 6: a real start whose key cannot be loaded
    /// must abort with the reason surfaced, never run under a
    /// substitute key.
    #[test]
    fn key_load_error_aborts_a_real_start() {
        match key_decision(&Err(unreadable_key()), false) {
            KeyDecision::Abort(message) => {
                assert!(message.contains("could not be read"), "{message}");
                assert!(message.contains("did not connect"), "{message}");
            }
            KeyDecision::Run(_) => {
                panic!("a failed key load must not produce a key for a real start")
            }
        }
    }

    #[test]
    fn loaded_key_passes_through_in_both_modes() {
        let key = [7u8; 32];
        for fake in [false, true] {
            match key_decision(&Ok(key), fake) {
                KeyDecision::Run(k) => assert_eq!(k, key),
                KeyDecision::Abort(m) => panic!("a loaded key must start, got abort: {m}"),
            }
        }
    }

    /// Demo mode is exempt: the fake backend never opens a
    /// database, so a key load error must not block it.
    #[test]
    fn demo_start_proceeds_despite_a_key_load_error() {
        assert!(matches!(key_decision(&Err(unreadable_key()), true), KeyDecision::Run(_)));
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
    fn live_app() -> (
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
            store: Some(store::Store::open_in_memory().unwrap()),
            jids: HashMap::new(),
            messages: HashMap::new(),
            times: HashMap::new(),
            fake: true,
            flags: HashMap::new(),
            engaged_chats: std::collections::HashSet::new(),
            photo_refs: HashMap::new(),
        });
        app.live_mode = true;
        (app, evt_tx, ctx, cmd_rx)
    }

    fn drain_all(app: &mut App, ctx: &egui::Context) {
        for _ in 0..64 {
            app.poll_backend(ctx);
            if app.event_queue.is_empty() {
                return;
            }
        }
        panic!("the event queue did not drain");
    }

    /// An arrival-style chat upsert: it carries no chat-action
    /// state (mute least of all), exactly like the real
    /// backend's message-arrival upserts.
    fn arrival_upsert() -> BackendEvent {
        BackendEvent::ChatUpsert(LiveChat {
            jid: CHAT_JID.to_owned(),
            title: "Sender".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "hello".to_owned(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread: 0,
            archived: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
        })
    }

    /// An informed chat upsert: it carries a real unread
    /// count, the way the fake's initial chat list does.
    fn counted_upsert(unread: u32) -> BackendEvent {
        BackendEvent::ChatUpsert(LiveChat {
            jid: CHAT_JID.to_owned(),
            title: "Sender".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "hello".to_owned(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread,
            archived: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
        })
    }

    fn seed_chat(app: &mut App, tx: &std::sync::mpsc::Sender<BackendEvent>, ctx: &egui::Context) {
        tx.send(arrival_upsert()).unwrap();
        drain_all(app, ctx);
        assert!(app.chats.iter().any(|c| c.id == jid_id(CHAT_JID)), "the upsert seeds the chat");
    }

    fn chat_muted(app: &App) -> bool {
        app.chats.iter().find(|c| c.id == jid_id(CHAT_JID)).expect("chat exists").muted
    }

    fn archived_muted(app: &App) -> bool {
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        store
            .chats()
            .unwrap()
            .iter()
            .find(|c| c.jid == CHAT_JID)
            .expect("the chat reached the archive")
            .muted
    }

    fn chat_pinned(app: &App) -> bool {
        app.chats.iter().find(|c| c.id == jid_id(CHAT_JID)).expect("chat exists").pinned
    }

    fn chat_archived(app: &App) -> bool {
        app.chats.iter().find(|c| c.id == jid_id(CHAT_JID)).expect("chat exists").archived
    }

    fn store_row(app: &App) -> LiveChat {
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        store
            .chats()
            .unwrap()
            .into_iter()
            .find(|c| c.jid == CHAT_JID)
            .expect("the chat reached the archive")
    }

    fn archived_pinned(app: &App) -> bool {
        store_row(app).pinned
    }

    fn archived_archived(app: &App) -> bool {
        store_row(app).archived
    }

    /// An informed chat upsert: it carries real chat-action
    /// state, the way the fake's chat-action and LeaveGroup
    /// echoes do (they clone the fake's full chat state).
    fn state_upsert(archived: bool, pinned: bool) -> BackendEvent {
        BackendEvent::ChatUpsert(LiveChat {
            jid: CHAT_JID.to_owned(),
            title: "Sender".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "hello".to_owned(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread: 0,
            archived,
            pinned,
            ephemeral_secs: 0,
            muted: false,
        })
    }

    /// The header-menu / profile-panel toggle path: both call
    /// apply_chat_action with the flipped value. In live mode
    /// that must put the matching ChatAction command on the
    /// backend channel, flip the local mirror at once, and
    /// write the state through to the archive.
    #[test]
    fn mute_toggle_sends_the_backend_command() {
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

    /// A mute change the server reports (another device, or a
    /// full sync) adopts unconditionally, even over a local
    /// optimistic toggle: the server value is the truth the
    /// local mirror reconciles to.
    #[test]
    fn server_mute_changed_overrides_local_state() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);

        tx.send(BackendEvent::MuteChanged { chat_jid: CHAT_JID.to_owned(), muted: true }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_muted(&app), "a server mute lands in the mirror");
        assert!(archived_muted(&app), "and in the archive");

        // The user unmutes here (optimistic local flip plus the
        // backend command), then the server re-asserts the mute,
        // as it would if that command failed or another device
        // muted again: the server value wins.
        app.apply_chat_action(chat_id, false, false, false);
        assert!(!chat_muted(&app));
        tx.send(BackendEvent::MuteChanged { chat_jid: CHAT_JID.to_owned(), muted: true }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(chat_muted(&app), "the server value overrides the local toggle");
        assert!(archived_muted(&app));
    }

    /// The counterpart rule: an arrival upsert knows nothing
    /// about mute, so it must not unmute a chat the mirror
    /// knows is muted (the merge protection stays).
    #[test]
    fn arrival_upsert_keeps_the_known_mute() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);

        tx.send(BackendEvent::MuteChanged { chat_jid: CHAT_JID.to_owned(), muted: true }).unwrap();
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
    fn server_blocklist_replaces_the_local_set() {
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

    /// The blocked set no longer persists in the plaintext
    /// settings document (privacy audit A6): its ids are FNV-1a
    /// hashes of phone-number JIDs, and the phone-number space
    /// is small enough to dictionary-reverse, so the set's home
    /// is the encrypted archive store (covered by the store
    /// round-trip test). The settings JSON must not carry the
    /// field at all, which also scrubs the legacy copy older
    /// builds wrote on the next settings save.
    #[test]
    fn settings_json_never_carries_the_blocked_set() {
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
    fn settings_json_carries_the_font_size_preference() {
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

    /// The connect-time privacy fetch reconciles the local
    /// read-receipts toggle with the account's setting (audit
    /// finding D3): the server value is adopted into the flag
    /// that gates receipt sending and into the settings document
    /// persist_settings writes, in both directions, and adopting
    /// sends nothing back to the backend.
    #[test]
    fn server_read_receipts_setting_is_adopted() {
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
    fn unrecognized_server_read_receipts_value_is_not_adopted() {
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
    fn read_receipts_toggle_sends_set_privacy() {
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
    fn rowless_privacy_categories_survive_into_app_state() {
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
    fn server_default_timer_is_adopted() {
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
    fn typing_off_retracts_the_composing_chat() {
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
    fn leaving_contact_blacklist_asks_first() {
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
    fn online_cycle_visits_exactly_its_valid_set_and_wraps() {
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
    fn unrecognized_privacy_value_is_not_coerced() {
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
    fn cycling_online_sends_match_last_seen_with_exact_spelling() {
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
    fn cache_clear_covers_all_caches() {
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
    fn server_pin_changed_overrides_local_state() {
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
    fn server_archive_changed_overrides_local_state() {
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
    fn arrival_upsert_keeps_the_known_pin_and_archive() {
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
    fn arrival_upsert_jid(jid: &str) -> BackendEvent {
        BackendEvent::ChatUpsert(LiveChat {
            jid: jid.to_owned(),
            title: "Other".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "hello".to_owned(),
            preview_sender: None,
            timestamp: BASE_TS,
            unread: 0,
            archived: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
        })
    }

    fn chat_unread(app: &App) -> u32 {
        app.chats.iter().find(|c| c.id == jid_id(CHAT_JID)).expect("chat exists").unread
    }

    fn mirror_starred(app: &App, chat_id: u64, msg_id: u64) -> bool {
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

    fn mirror_bodies(app: &App, chat_id: u64) -> Vec<String> {
        app.live
            .as_ref()
            .unwrap()
            .messages
            .get(&chat_id)
            .map(|msgs| msgs.iter().map(|(_, m)| m.body.clone()).collect())
            .unwrap_or_default()
    }

    fn archived_bodies(app: &App) -> Vec<String> {
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        store.messages(CHAT_JID).unwrap().into_iter().map(|(m, _)| m.body).collect()
    }

    /// A star change the server reports (another device, or a
    /// full sync) adopts unconditionally in both directions:
    /// the session mirror flips and the archive flags follow,
    /// and the adoption sends no command back (the change is
    /// already on the server).
    #[test]
    fn server_star_changed_overrides_local_state() {
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
    fn arrival_upsert_keeps_the_known_star() {
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
    fn server_chat_read_changed_clears_unread_without_sending_receipts() {
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
    fn server_chat_read_changed_marks_unread_with_the_reported_count() {
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
    fn informed_upsert_adopts_the_reported_unread_count() {
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
    fn arrival_upserts_do_not_reset_the_unread_count() {
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
    /// adoption sends nothing back.
    #[test]
    fn server_chat_deleted_removes_the_chat_everywhere() {
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
        assert!(cmd_rx.try_recv().is_err(), "adopting a deletion sends nothing back");
    }

    /// A chat cleared on the phone empties here too, on the
    /// clear's own terms: messages past the bound survive,
    /// starred messages survive a keep-starred clear, and the
    /// chat itself (row, settings, list position) is exactly
    /// as it was. A second, unbounded delete-starred clear
    /// takes the rest. Nothing is sent back at any point.
    #[test]
    fn server_chat_cleared_empties_messages_but_keeps_the_chat() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        seed_chat(&mut app, &tx, &ctx);
        let chat_id = jid_id(CHAT_JID);
        // Settings the clear must not disturb, set the way the
        // server reports them.
        tx.send(BackendEvent::PinChanged { chat_jid: CHAT_JID.to_owned(), pinned: true }).unwrap();
        tx.send(BackendEvent::MuteChanged { chat_jid: CHAT_JID.to_owned(), muted: true }).unwrap();
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
    fn server_message_deleted_for_me_removes_exactly_that_message() {
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
    fn informed_upsert_still_lands() {
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

#[cfg(test)]
mod mirror_pruning_tests {
    use super::*;
    use crate::state::{App, Args};

    const CHAT_JID: &str = "6281234567890@s.whatsapp.net";

    /// The same live-mode harness as the burst tests: an app
    /// whose backend channels the test feeds and reads by hand,
    /// with an in-memory archive attached.
    fn live_app() -> (
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
            store: Some(store::Store::open_in_memory().unwrap()),
            jids: HashMap::new(),
            messages: HashMap::new(),
            times: HashMap::new(),
            fake: true,
            flags: HashMap::new(),
            engaged_chats: std::collections::HashSet::new(),
            photo_refs: HashMap::new(),
        });
        app.live_mode = true;
        (app, evt_tx, ctx, cmd_rx)
    }

    fn drain_all(app: &mut App, ctx: &egui::Context) {
        for _ in 0..64 {
            app.poll_backend(ctx);
            if app.event_queue.is_empty() {
                return;
            }
        }
        panic!("the event queue did not drain");
    }

    /// The one path that removes messages from the live mirror
    /// (a disappearing message expiring) must take the message's
    /// helper entries with it, and only that message's: every
    /// per-message map is seeded for an expiring and a surviving
    /// message, and after the pass the survivor's entries must
    /// still resolve while the expired one's are all gone.
    #[test]
    fn expiry_prunes_helper_entries_of_the_expired_message_only() {
        let (mut app, _tx, _ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        let old_mid = message_id(CHAT_JID, "old-pid");
        let new_mid = message_id(CHAT_JID, "new-pid");
        let old = Message { expires_at: 100, ..Message::mine(old_mid, "about to vanish", "09:00") };
        let new = Message { expires_at: 0, ..Message::mine(new_mid, "stays", "09:01") };
        {
            let session = app.live.as_mut().unwrap();
            session.jids.insert(chat_id, CHAT_JID.to_owned());
            session.messages.insert(chat_id, vec![(1_000, old), (1_060, new)]);
            session.flags.insert(old_mid, store::FLAG_STARRED);
            session.flags.insert(new_mid, store::FLAG_STARRED);
        }
        for mid in [old_mid, new_mid] {
            let pid = if mid == old_mid { "old-pid" } else { "new-pid" };
            app.protocol_ids.insert(mid, pid.to_owned());
            app.sender_jids.insert(mid, "6281234567890@s.whatsapp.net".to_owned());
            app.receipt_log.insert(mid, vec![(ReceiptKind::Read, "09:05".to_owned())]);
            app.my_reactions.insert(mid, "thumbs-up".to_owned());
            app.reaction_actors.insert(
                format!("{chat_id}:{mid}"),
                HashMap::from([("Alice".to_owned(), "thumbs-up".to_owned())]),
            );
        }
        app.unread_live.insert(chat_id, vec!["old-pid".to_owned()]);

        app.expire_disappearing_messages(200);

        let session = app.live.as_ref().unwrap();
        let mirror = session.messages.get(&chat_id).unwrap();
        assert_eq!(mirror.len(), 1, "only the expired message leaves the mirror");
        assert_eq!(mirror[0].1.id, new_mid);
        assert!(!session.flags.contains_key(&old_mid), "expired flags are pruned");
        assert!(session.flags.contains_key(&new_mid), "surviving flags stay");
        for (map, mid) in
            [("protocol", old_mid), ("sender", old_mid), ("receipt", old_mid), ("mine", old_mid)]
        {
            let present = match map {
                "protocol" => app.protocol_ids.contains_key(&mid),
                "sender" => app.sender_jids.contains_key(&mid),
                "receipt" => app.receipt_log.contains_key(&mid),
                _ => app.my_reactions.contains_key(&mid),
            };
            assert!(!present, "the expired message's {map} entry is pruned");
        }
        assert!(
            !app.reaction_actors.contains_key(&format!("{chat_id}:{old_mid}")),
            "the expired message's reaction entry is pruned"
        );
        assert!(
            !app.unread_live.contains_key(&chat_id),
            "the expired message's pending read receipt is pruned with it"
        );

        // Lookups for the retained message still resolve, through
        // the same paths the UI uses: the protocol map itself,
        // the flag path that reads it to persist, and the quote
        // resolution for a reply aimed at the survivor.
        assert_eq!(app.protocol_ids.get(&new_mid).map(String::as_str), Some("new-pid"));
        assert_eq!(
            app.sender_jids.get(&new_mid).map(String::as_str),
            Some("6281234567890@s.whatsapp.net")
        );
        assert!(app.receipt_log.contains_key(&new_mid));
        assert!(app.my_reactions.contains_key(&new_mid));
        assert!(app.reaction_actors.contains_key(&format!("{chat_id}:{new_mid}")));
        app.set_live_flag(chat_id, new_mid, store::FLAG_PINNED_MESSAGE, true);
        assert_eq!(
            app.live.as_ref().unwrap().flags.get(&new_mid).copied().unwrap_or(0),
            store::FLAG_STARRED | store::FLAG_PINNED_MESSAGE,
            "the flag path still finds the survivor's protocol id"
        );
        app.selected_chat = chat_id;
        app.sync_conversation();
        app.composer.reply_to = Some((new_mid, "Sender".to_owned(), "stays".to_owned()));
        let quote = app.live_quote_for_reply().expect("a reply to the survivor still resolves");
        assert_eq!(quote.message_id, "new-pid");
        app.composer.reply_to = Some((old_mid, "Sender".to_owned(), "gone".to_owned()));
        assert!(
            app.live_quote_for_reply().is_none(),
            "a reply to the expired message resolves to nothing, as before"
        );
    }

    /// The expiry pass writes the refreshed preview through to
    /// the archive chat row, not just the in-memory list: after
    /// the newest message expires, the stored preview is the
    /// newest survivor's body (a restart must not resurrect the
    /// expired body), and when the last message expires the
    /// stored preview empties with the mirror's.
    #[test]
    fn expiry_writes_the_refreshed_preview_through_to_the_archive() {
        let (mut app, _tx, _ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        let older_mid = message_id(CHAT_JID, "older-pid");
        let newer_mid = message_id(CHAT_JID, "newer-pid");
        let older = Message {
            expires_at: 300,
            ..Message::mine(older_mid, "older, expires later", "09:00")
        };
        let newer = Message {
            expires_at: 100,
            ..Message::mine(newer_mid, "newest, expires first", "09:01")
        };
        // The list row and the archive row both still preview
        // the newest message, as they did before the pass runs.
        let live_row = LiveChat {
            jid: CHAT_JID.to_owned(),
            title: "Sender".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "newest, expires first".to_owned(),
            preview_sender: None,
            timestamp: 1_060,
            unread: 0,
            archived: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
        };
        app.chats.push(to_chat(&live_row));
        {
            let session = app.live.as_mut().unwrap();
            session.jids.insert(chat_id, CHAT_JID.to_owned());
            session.times.insert(chat_id, 1_060);
            session.messages.insert(chat_id, vec![(1_000, older), (1_060, newer)]);
            session.store.as_ref().unwrap().upsert_chat(&live_row).unwrap();
        }
        app.protocol_ids.insert(older_mid, "older-pid".to_owned());
        app.protocol_ids.insert(newer_mid, "newer-pid".to_owned());

        let mirror_preview =
            |app: &App| app.chats.iter().find(|c| c.id == chat_id).unwrap().preview.clone();
        let stored_preview = |app: &App| {
            app.live
                .as_ref()
                .unwrap()
                .store
                .as_ref()
                .unwrap()
                .chats()
                .unwrap()
                .into_iter()
                .find(|c| c.jid == CHAT_JID)
                .expect("the archive chat row stays")
                .preview
        };

        app.expire_disappearing_messages(200);
        assert_eq!(mirror_preview(&app), "older, expires later");
        assert_eq!(
            stored_preview(&app),
            "older, expires later",
            "the stored preview follows the newest survivor, not the expired message"
        );

        app.expire_disappearing_messages(400);
        assert_eq!(mirror_preview(&app), "");
        assert_eq!(stored_preview(&app), "", "with no survivors the stored preview empties too");
    }

    /// When the last actor removes their reaction, the
    /// per-message actor entry itself is dropped, so the map
    /// holds only messages with live reactions.
    #[test]
    fn removing_the_last_reaction_drops_the_actor_entry() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "r1");
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "r1",
            CHAT_JID,
            Some("Alice".to_owned()),
            "react to me",
            1_700_000_000,
            false,
        )))
        .unwrap();
        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "r1".to_owned(),
            actor: "Alice".to_owned(),
            emoji: "thumbs-up".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let key = format!("{chat_id}:{mid}");
        assert!(app.reaction_actors.contains_key(&key), "the reaction registers its entry");
        let mirror = app.live.as_ref().unwrap().messages.get(&chat_id).unwrap();
        assert_eq!(mirror[0].1.reactions, vec![("thumbs-up".to_owned(), 1)]);

        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "r1".to_owned(),
            actor: "Alice".to_owned(),
            emoji: String::new(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!app.reaction_actors.contains_key(&key), "the emptied entry is dropped");
        let mirror = app.live.as_ref().unwrap().messages.get(&chat_id).unwrap();
        assert!(mirror[0].1.reactions.is_empty(), "the mirror count still clears");
    }

    /// Toggling a flag off removes the in-memory entry (absence
    /// and zero are the same to every reader) while the zero
    /// itself is still persisted to the archive row.
    #[test]
    fn clearing_the_last_flag_removes_the_entry_but_persists_zero() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "f1");
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "f1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "star me",
            1_700_000_000,
            false,
        )))
        .unwrap();
        drain_all(&mut app, &ctx);

        app.set_live_flag(chat_id, mid, store::FLAG_STARRED, true);
        assert_eq!(
            app.live.as_ref().unwrap().flags.get(&mid).copied().unwrap_or(0),
            store::FLAG_STARRED
        );
        app.set_live_flag(chat_id, mid, store::FLAG_STARRED, false);
        assert!(
            !app.live.as_ref().unwrap().flags.contains_key(&mid),
            "a zeroed flag entry leaves the map"
        );
        let archived =
            app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let row = archived.iter().find(|(m, _)| m.id == "f1").expect("the row is archived");
        assert_eq!(row.1, 0, "the archive row still records the cleared flag");
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

    fn app() -> (App, egui::Context) {
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

    fn session(status: LinkStatus, with_payload: bool) -> LiveSession {
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
            store: None,
            jids: HashMap::new(),
            messages: HashMap::new(),
            times: HashMap::new(),
            fake: false,
            flags: HashMap::new(),
            engaged_chats: std::collections::HashSet::new(),
            photo_refs: HashMap::new(),
        };
        if with_payload {
            session.qr = Some(backend::qr_matrix("ref,noise,identity,adv,1").unwrap());
            session.qr_expires_at =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
        }
        session
    }

    #[test]
    fn no_session_and_no_error_is_demo() {
        let (app, _ctx) = app();
        assert!(matches!(app.connect_qr(), ConnectQr::Demo));
    }

    #[test]
    fn failed_start_is_reported_not_painted() {
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
    fn failed_start_beats_a_leftover_session() {
        // A restart that aborts on the key leaves the previous
        // session struct in place; the newest fact is the failure,
        // and the gate must not resurrect the old payload.
        let (mut app, _ctx) = app();
        app.live = Some(session(LinkStatus::WaitingQr, true));
        app.live_start_error = Some("the database key could not be read".to_owned());
        assert!(matches!(app.connect_qr(), ConnectQr::StartFailed(_)));
    }

    #[test]
    fn pairable_status_without_payload_is_connecting() {
        let (mut app, _ctx) = app();
        app.live = Some(session(LinkStatus::Starting, false));
        assert!(matches!(app.connect_qr(), ConnectQr::Connecting));
        app.live = Some(session(LinkStatus::WaitingQr, false));
        assert!(matches!(app.connect_qr(), ConnectQr::Connecting));
    }

    #[test]
    fn pairable_status_with_payload_is_live() {
        let (mut app, _ctx) = app();
        app.live = Some(session(LinkStatus::WaitingQr, true));
        assert!(matches!(app.connect_qr(), ConnectQr::Live(_)));
        app.live = Some(session(LinkStatus::CodeReady, true));
        assert!(matches!(app.connect_qr(), ConnectQr::Live(_)));
    }

    #[test]
    fn down_session_holding_a_payload_is_never_live() {
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
    fn qr_event_stores_payload_with_its_validity_window() {
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
    fn connection_loss_and_failure_clear_the_payload() {
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
    fn qr_expired_clears_payload_and_fails_the_attempt() {
        let (mut app, ctx) = app();
        app.live = Some(session(LinkStatus::WaitingQr, true));
        app.apply_event(&ctx, BackendEvent::QrExpired);
        let live = app.live.as_ref().unwrap();
        assert!(live.qr.is_none() && live.qr_expires_at.is_none());
        assert!(matches!(live.status, LinkStatus::Failed(_)));
    }

    #[test]
    fn qr_expired_keeps_a_pairing_code_flow_alive() {
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
    fn expiry_sweep_drops_only_a_past_due_payload() {
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
