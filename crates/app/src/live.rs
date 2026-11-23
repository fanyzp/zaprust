//! Live mode: the bridge between the `backend` crate and the UI.
//!
//! In live mode the chat list and conversation are fed by backend
//! events (and, on start, by the `store` archive) instead of the demo
//! dataset. Everything protocol-shaped stays in this module: JID →
//! numeric ID mapping, unix timestamps → display times, and the
//! write-through of every upsert into the archive.

use std::collections::HashMap;

use crate::state::Notice;
use backend::{
    spawn_backend, Backend, BackendChannels, BackendCommand, BackendConfig, BackendEvent,
    ChatAction, FakeBackend, LiveChat, LiveChatKind, LiveMessage, QrMatrix, QuotedRef, ReceiptKind,
    WhatsappBackend,
};

/// The mute duration choices the header menu offers (design
/// standard section 24), matching the durations WhatsApp
/// itself presents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MuteChoice {
    /// Mute for 8 hours.
    Hours8,
    /// Mute for 1 week.
    Week,
    /// Mute with no end.
    Always,
}

/// The current time as unix seconds (0 if the clock is somehow
/// before the epoch).
pub(crate) fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
use core::model::{
    CallDirection, CallEntry, CallKind, Chat, ChatKind, Delivery, Message, MessageKind,
};
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
    /// A log out was confirmed here and the disconnect command
    /// is in flight; the session ends when the backend reports
    /// it (or its channel closes).
    LoggingOut,
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
            Self::Starting => "Connecting…".to_owned(),
            Self::WaitingQr => "Waiting for scan, point your phone at the code.".to_owned(),
            Self::CodeReady => "Code ready, enter it on your phone.".to_owned(),
            Self::Linked => "Linked, this computer is connected.".to_owned(),
            Self::LoggingOut => "Logging out…".to_owned(),
            Self::LoggedOut => "Logged out, link again to continue.".to_owned(),
            Self::Banned(code) => {
                format!("This account is temporarily banned (reason {code}).")
            }
            Self::Lost(reason) => reason.clone(),
            Self::Crashed(summary) => format!("The connection stopped: {summary}"),
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
    /// Poll tally states produced by vote events in this batch.
    /// They are full snapshots (counts, my votes, ballots) taken
    /// from the mirror as each vote was applied, and the flush
    /// writes them after the batch's upserts, so a vote on a poll
    /// whose creation upsert sits in the same batch still lands
    /// on an existing row, and the last snapshot wins.
    pub(crate) poll_tally_writes: Vec<PollTallyWrite>,
    /// Chats whose unread badge a batch upsert raised. The
    /// badge lives in the mirror, but the archive row must carry
    /// the same count (a restart reloads from the archive), so
    /// the flush writes these chats' rows from the mirror once
    /// per batch instead of once per message.
    pub(crate) chat_rows_dirty: std::collections::HashSet<u64>,
}

/// One poll's aggregated tally state, on its way to the archive:
/// per-option counts, my votes, and the per-voter ballots they
/// were derived from.
pub(crate) struct PollTallyWrite {
    /// Chat JID the poll lives in.
    pub(crate) chat_jid: String,
    /// Protocol ID of the poll message.
    pub(crate) message_id: String,
    /// Per-option vote counts, in option order.
    pub(crate) counts: Vec<u32>,
    /// Option indices I voted for.
    pub(crate) my_votes: Vec<usize>,
    /// Latest ballot per voter.
    pub(crate) ballots: Vec<(String, Vec<usize>)>,
}

/// One chat's older-history fetch state within a session
/// (design standard section 22).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryFetch {
    /// A page request is outstanding with the phone.
    Fetching,
    /// The phone's history for this chat is exhausted: the
    /// completion marker arrived, or a page came back empty.
    /// Latched for the session; scrolling never re-asks.
    Exhausted,
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
    /// Catch-up sync state, fed by the backend's sync events:
    /// `syncing` is true from a (re)connect until the backend
    /// reports the sync complete or sync activity goes quiet
    /// (see `poll_backend`), and `synced_once` records that at
    /// least one full catch-up has completed this session (the
    /// chat list's status row reads these; design standard
    /// section 14). No percentage is kept: the library's only
    /// numeric progress is a per-chunk stamp from the phone with
    /// no monotonicity and no guaranteed final value, so any
    /// number shown would be fake precision that can strand on
    /// screen (it sat at 4% for minutes on a populated roster).
    /// The row is an honest indeterminate "syncing" state.
    pub(crate) syncing: bool,
    /// When sync activity was last reported (a connect, a
    /// history chunk, an offline-sync preview). The quiet-window
    /// fallback clears `syncing` when this goes stale, because
    /// the library's completion event covers only the offline
    /// drain and a catch-up can end without it.
    pub(crate) sync_last_activity: Option<std::time::Instant>,
    /// Whether a catch-up sync has completed this session.
    pub(crate) synced_once: bool,
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
    /// The user confirmed a log out for this session and the
    /// disconnect command was sent. The session is torn down
    /// once the backend's event channel closes (the real backend
    /// ends its run on logout; the fake is shut down explicitly),
    /// so everything the backend still has to say, including a
    /// deregistration failure, is applied first.
    pub(crate) logout_requested: bool,
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
    /// Voice re-download references per numeric message ID,
    /// seeded and extended exactly like `photo_refs`. A playback
    /// fetch passes the reference along, so a voice note from
    /// before this session stays playable.
    pub(crate) voice_refs: HashMap<u64, backend::VoiceRef>,
    /// Video re-download references per numeric message ID,
    /// seeded and extended exactly like `photo_refs`. A playback
    /// fetch passes the reference along, so a video from before
    /// this session stays playable.
    pub(crate) video_refs: HashMap<u64, backend::VideoRef>,
    /// Video thumbnails (the wire JPEG bytes) per numeric
    /// message ID, seeded and extended exactly like
    /// `video_refs`, so a video's bubble and viewer can present
    /// its own frame in later sessions without fetching its
    /// bytes.
    pub(crate) video_thumbs: HashMap<u64, Vec<u8>>,
    /// Sticker re-download references per numeric message ID,
    /// seeded and extended exactly like `photo_refs`.
    pub(crate) sticker_refs: HashMap<u64, backend::StickerRef>,
    /// Photo thumbnails (the wire JPEG bytes) per numeric
    /// message ID, seeded and extended exactly like
    /// `video_thumbs`, so a photo's tile can paint the photo
    /// itself at preview fidelity before its full bytes are
    /// fetched, and size itself from the real aspect ratio.
    pub(crate) photo_thumbs: HashMap<u64, Vec<u8>>,
    /// Mentioned JIDs per numeric message ID, seeded from the
    /// archive and extended by every upsert. The bubble
    /// substitutes display names for the body's `@<number>`
    /// spans at render time, resolving through `names`.
    pub(crate) message_mentions: HashMap<u64, Vec<String>>,
    /// The session's name directory: JID (either form) to the
    /// best known display name, fed by sender-name events,
    /// group member lists, and chat titles. Mention rendering
    /// resolves through it; entries only ever improve (a raw
    /// identifier never overwrites a name), the same monotonic
    /// rule the store enforces.
    pub(crate) names: HashMap<String, String>,
    /// Profile picture bytes per JID, staged from the store's
    /// avatar rows at start. Decoded into
    /// `media.live_avatar_textures` on first display (decoding
    /// needs a graphics context the session does not own).
    pub(crate) avatar_bytes: HashMap<String, Vec<u8>>,
    /// JIDs whose picture lookup answered definitively (no
    /// picture, or privacy refused), with the unix second of
    /// the answer. A negative younger than a day is not
    /// re-asked; an older one is, because people set pictures.
    pub(crate) avatar_missing: HashMap<String, i64>,
    /// JIDs with a picture fetch in flight, plus JIDs whose
    /// fetch failed this session (transport failures stay
    /// retryable across sessions, not within one frame loop).
    /// Cleared per JID when the wire reports a picture change.
    pub(crate) avatar_requested: std::collections::HashSet<String>,
    /// Per-chat older-history fetch state (design standard
    /// section 22): a chat is present while a page request is
    /// outstanding, or once the phone's history for it is
    /// exhausted; absence is idle. Session-scoped by
    /// construction: a new session re-earns every latch.
    pub(crate) history: HashMap<u64, HistoryFetch>,
    /// Chats whose scroll-trigger for an older page is armed
    /// (design standard section 22): armed when a chat opens
    /// and after the stream has scrolled away from the top,
    /// disarmed when a fetch fires, so resting at the top
    /// never loops requests.
    pub(crate) history_armed: std::collections::HashSet<u64>,
    /// Per-voter poll ballots per numeric message ID, seeded
    /// from the archive at start and extended by every vote
    /// event. A vote replaces the voter's whole previous ballot
    /// (last-vote-wins), so the tallies can only be kept honest
    /// while the previous ballots are remembered; the archive
    /// copy rides on the stored message (see
    /// `store::Store::set_poll_tally`).
    pub(crate) poll_ballots: HashMap<u64, Vec<(String, Vec<usize>)>>,
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
    /// A session ended by logging out and none has started
    /// since: the device is unlinked and can link again.
    LoggedOut,
}

impl crate::state::App {
    /// The connect screen's QR-area state (see [`ConnectQr`]).
    pub(crate) fn connect_qr(&self) -> ConnectQr<'_> {
        if let Some(message) = &self.live_start_error {
            return ConnectQr::StartFailed(message);
        }
        let Some(session) = &self.live else {
            if self.logged_out {
                return ConnectQr::LoggedOut;
            }
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

pub(crate) fn message_id(chat_jid: &str, proto_id: &str) -> u64 {
    jid_id(&format!("{chat_jid}/{proto_id}"))
}

/// Whether a display string is a raw identifier rather than a
/// name: nine or more characters, all digits. The same predicate
/// guards renames in the backend and the archive; identity
/// healing may only replace one of these, never a real name.
pub(crate) fn is_raw_identifier(text: &str) -> bool {
    text.len() >= 9 && text.bytes().all(|b| b.is_ascii_digit())
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

/// The header's last-seen line for a unix timestamp, in the
/// shape the demo data uses: today and yesterday get their
/// word, older reports get the full date.
fn last_seen_label(timestamp: i64) -> String {
    let day = timestamp.div_euclid(86_400);
    let today = (crate::state::now_unix() / 86_400) as i64;
    let time = fmt_time(timestamp);
    match day - today {
        0 => crate::i18n::tf("presence.last_seen_today", "time", &time),
        -1 => crate::i18n::tf("presence.last_seen_yesterday", "time", &time),
        _ => crate::i18n::tf("presence.last_seen_date", "date", &fmt_day(timestamp))
            .replace("{time}", &time),
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
        // No subtitle: a chat row carries nothing truthful for
        // the header's second line. That line is derived at
        // render time from live state instead (the watched
        // contact's presence for a DM, the fetched member list
        // for a group; see `App::header_subtitle`), and this
        // field stays empty for live chats so no surface can
        // present a placeholder as data.
        subtitle: String::new(),
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
        mute_until: u64::try_from(live.mute_until).unwrap_or(0),
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

/// Builds the command that hands one archived poll's creation
/// secret back to the backend, or `None` when the message is not
/// a poll whose secret and creator were both kept. A poll from an
/// earlier session needs this before it can be voted on or have
/// its incoming votes decrypted in the new one.
fn poll_secret_registration(msg: &LiveMessage) -> Option<BackendCommand> {
    let secret = msg.poll_secret.as_ref()?;
    let creator = msg.poll_creator.as_ref()?;
    let backend::LiveMessageKind::Poll { options, .. } = &msg.kind else {
        return None;
    };
    Some(BackendCommand::RegisterPollSecret {
        chat_jid: msg.chat_jid.clone(),
        message_id: msg.id.clone(),
        creator_jid: creator.clone(),
        secret: secret.clone(),
        options: options.iter().map(|(label, _)| label.clone()).collect(),
    })
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
                    // The neutral form until the message-level
                    // conversion replaces it with the wire
                    // waveform's bars (see voice_waveform_bars):
                    // a received voice note's shape comes from
                    // its payload or not at all.
                    waveform: voice_waveform_bars(None),
                },
                MediaKind::Document => MessageKind::Document {
                    name: if caption.is_empty() { "Document".to_owned() } else { caption.clone() },
                    size_kib: u32::try_from(size_bytes / 1024).unwrap_or(0),
                },
                MediaKind::Sticker => MessageKind::Sticker,
            }
        }
        // An untranslated kind is a typed summary (its label
        // is the message body), never a text message: it
        // renders as the neutral system line, so it cannot
        // impersonate something the sender typed.
        backend::LiveMessageKind::Other(_) => MessageKind::System,
    }
}

/// Converts one persisted call-history record into the Calls
/// panel's entry shape (the peer resolved to its numeric chat ID
/// the same way chats and messages resolve theirs).
fn to_call_entry(record: &backend::CallRecord) -> CallEntry {
    CallEntry {
        id: record.id,
        chat_id: jid_id(&record.chat_jid),
        kind: if record.video { CallKind::Video } else { CallKind::Voice },
        direction: if record.outgoing { CallDirection::Outgoing } else { CallDirection::Incoming },
        missed: record.missed,
        duration_secs: record.duration_secs,
        timestamp: record.started_at,
    }
}

/// Converts a backend message (+ archive flags) into a core message.
/// The bars a voice note renders: the wire waveform's bytes
/// scaled into bar heights, or the neutral flat form when the
/// payload carried none. A received waveform is never invented
/// locally; an earlier conversion derived bar heights from the
/// caption's length, a shape with no relation to the audio.
fn voice_waveform_bars(wire: Option<&[u8]>) -> Vec<f32> {
    match wire {
        Some(bytes) if !bytes.is_empty() => {
            bytes.iter().map(|b| 0.12 + f32::from(*b) / 255.0 * 0.88).collect()
        }
        _ => vec![0.3; 28],
    }
}

pub(crate) fn to_message_with_flags(live: &LiveMessage, flags: i64) -> Message {
    let mut kind = to_kind(&live.kind);
    if let MessageKind::Voice { waveform, .. } = &mut kind {
        *waveform = voice_waveform_bars(live.voice_waveform.as_deref());
    }
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
        kept: flags & store::FLAG_KEPT != 0,
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
        // A preview is one line: a multi-line body is flattened
        // here, at the one point every glanceable surface reads
        // (the stored body keeps its line breaks).
        core::model::flatten_preview(&live.body)
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
             Nothing was changed. Make the key available again (the key file \
             in the app data folder, or unlock your keyring) and try connecting again."
        )),
    }
}

/// The files a start-fresh move sets aside, of those present in
/// `data_dir`: the chat archive and its SQLCipher sidecars, the
/// key file, and every account's session store with its
/// sidecars. These are exactly the files whose presence makes
/// the key loader fail closed (see `store::dbkey`), so setting
/// them aside is what lets a fresh key be minted afterwards.
/// Anything else in the data dir (settings, crash reports,
/// downloads) is not stranded and stays where it is.
fn stranded_files(data_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn push_with_sidecars(files: &mut Vec<std::path::PathBuf>, base: std::path::PathBuf) {
        for suffix in ["", "-wal", "-shm"] {
            files.push(std::path::PathBuf::from(format!("{}{suffix}", base.display())));
        }
    }
    let mut files = Vec::new();
    push_with_sidecars(&mut files, data_dir.join("archive.db"));
    files.push(data_dir.join("db.key"));
    if let Ok(entries) = std::fs::read_dir(data_dir.join("accounts")) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|f| f.is_dir()) {
                push_with_sidecars(&mut files, entry.path().join("session.db"));
            }
        }
    }
    files.retain(|p| p.exists());
    files
}

/// Moves the stranded data in `data_dir` into a fresh
/// `stranded-<unix seconds>` backup folder inside it, keeping
/// the relative layout, and returns the backup folder. Files
/// are moved, never deleted: the data stays on the device,
/// unreadable without its missing key, until the user removes
/// the folder by hand.
///
/// # Errors
/// The first I/O failure (creating the folder, moving a file);
/// files already moved stay moved, and the caller reports the
/// failure instead of starting over a half-moved data dir.
fn move_stranded_data_aside(data_dir: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut backup = data_dir.join(format!("stranded-{stamp}"));
    let mut n = 1;
    while backup.exists() {
        n += 1;
        backup = data_dir.join(format!("stranded-{stamp}-{n}"));
    }
    std::fs::create_dir_all(&backup)?;
    for file in stranded_files(data_dir) {
        let rel = file
            .strip_prefix(data_dir)
            .expect("stranded files are collected from under the data dir");
        let dest = backup.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&file, &dest)?;
    }
    Ok(backup)
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
    /// backend, loads archived chats, and kicks off the link flow.
    /// Called from `App::new` for `--live`, and from the account
    /// modal's "Add account" for the in-app path.
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
            syncing: false,
            sync_last_activity: None,
            synced_once: false,
            store: store_opt,
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
            sticker_refs: HashMap::new(),
            photo_thumbs: HashMap::new(),
            message_mentions: HashMap::new(),
            names: HashMap::new(),
            avatar_bytes: HashMap::new(),
            avatar_missing: HashMap::new(),
            avatar_requested: std::collections::HashSet::new(),
            history: HashMap::new(),
            history_armed: std::collections::HashSet::new(),
            poll_ballots: HashMap::new(),
        };
        // Load the archive into the rendered data. Live mode never
        // shows demo data: start from empty and fill from the archive.
        self.chats = Vec::new();
        // Polls whose creation secret the archive kept: after the
        // session is installed, each is handed back to the backend
        // so the poll stays votable and its votes stay decryptable
        // in this session too.
        let mut archived_poll_secrets: Vec<BackendCommand> = Vec::new();
        if let Some(store) = &session.store {
            // Stored profile pictures seed the avatar cache:
            // positives stage their bytes for first display,
            // negatives remember when they were answered.
            if let Ok(rows) = store.avatars() {
                for (jid, _picture_id, bytes, fetched_at) in rows {
                    match bytes {
                        Some(bytes) => {
                            session.avatar_bytes.insert(jid, bytes);
                        }
                        None => {
                            session.avatar_missing.insert(jid, fetched_at);
                        }
                    }
                }
            }
            if let Ok(chats) = store.chats() {
                for chat in &chats {
                    session.jids.insert(jid_id(&chat.jid), chat.jid.clone());
                    session.times.insert(jid_id(&chat.jid), chat.timestamp);
                    // A direct chat's resolved title is the
                    // contact's display name: it seeds the name
                    // directory under the chat's JID.
                    if chat.kind == backend::LiveChatKind::Dm
                        && !chat.title.is_empty()
                        && !is_raw_identifier(&chat.title)
                    {
                        session.names.insert(chat.jid.clone(), chat.title.clone());
                    }
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
                            // The author's JID is seeded the same
                            // way: a group reaction must name the
                            // author, and an archived message whose
                            // author was never seeded could not be
                            // reacted to after a restart.
                            if let Some(jid) = &m.sender_jid {
                                self.sender_jids
                                    .insert(message_id(&m.chat_jid, &m.id), jid.clone());
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
                                session
                                    .video_thumbs
                                    .insert(message_id(&m.chat_jid, &m.id), thumb.clone());
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
                                session
                                    .photo_thumbs
                                    .insert(message_id(&m.chat_jid, &m.id), thumb.clone());
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
                                if !name.is_empty() && !is_raw_identifier(name) {
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
                        }
                        session.messages.insert(
                            jid_id(&chat.jid),
                            msgs.iter()
                                .map(|(m, flags)| (m.timestamp, to_message_with_flags(m, *flags)))
                                .collect(),
                        );
                    }
                }
                // Archives written before the status fix can
                // hold the bogus "status" DM row for
                // status@broadcast: it never becomes a chat
                // here (its stored messages stay in the
                // archive, unrendered, for the Updates round).
                self.chats =
                    chats.iter().filter(|c| c.jid != "status@broadcast").map(to_chat).collect();
            }
        }
        self.live = Some(session);
        // The archive's favorite flags belong to the chats just
        // loaded: the conversion cannot carry them (they are
        // not part of the synced chat row), so they are applied
        // from the archive's own record here.
        self.apply_stored_favorites();
        self.live_start_error = None;
        self.logged_out = false;
        if let Some(note) = archive_note {
            self.notice = Some(Notice::info(note));
        }
        self.live_mode = true;
        // The Calls panel's live history loads from the same
        // archive, at the same point as the chats above.
        self.load_call_history();
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
        for cmd in archived_poll_secrets {
            self.send_backend(cmd);
        }
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
    fn sync_presence_watch(&mut self) {
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

    /// Loads the archive's persisted call history into the Calls
    /// panel's live list. Called when a live session starts, the
    /// same point the archived chats load; the loaded call IDs
    /// seed the recorded set, so a call the archive already holds
    /// (a missed call replayed again on a later reconnect) is
    /// never entered twice.
    fn load_call_history(&mut self) {
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
    fn record_ended_call(&mut self, call_id: &str, chat_jid: &str, seconds: u32) {
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
    fn record_elsewhere_call(&mut self, call_id: &str, chat_jid: &str, answered: bool, at: i64) {
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
    fn record_missed_call(&mut self, call_id: &str, chat_jid: &str, at: i64) {
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
    fn record_call(&mut self, mut record: backend::CallRecord) {
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
        // The sync row's quiet-window fallback: the library's
        // completion event covers the offline drain, and a
        // catch-up can end without one (a reconnect with an
        // empty backlog produces no end marker), which used to
        // latch the row forever. Twenty-five seconds with no
        // sync activity reported means the catch-up is over in
        // practice; the row clears and the session counts as
        // synced.
        if let Some(session) = &mut self.live {
            if session.syncing {
                let quiet = session
                    .sync_last_activity
                    .is_some_and(|last| last.elapsed() > std::time::Duration::from_secs(25));
                if quiet {
                    session.syncing = false;
                    session.sync_last_activity = None;
                    session.synced_once = true;
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
            BackendEvent::ReactionFailed { chat_jid, message_id: pid, message } => {
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
            BackendEvent::StatusPosted { text } => {
                self.apply_status_posted(text);
            }
            BackendEvent::StatusPostFailed { message } => {
                self.apply_status_post_failed(message);
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
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                self.pending_revokes.remove(&mid);
                self.apply_revoke(chat_id, mid);
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
            BackendEvent::StarChanged { chat_jid, message_id: pid, starred } => {
                let chat_id = jid_id(&chat_jid);
                let mid = message_id(&chat_jid, &pid);
                self.apply_star_changed(chat_id, mid, &pid, starred);
            }
            BackendEvent::ChatReadChanged { chat_jid, read, unread_count } => {
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
                };
                if let Some(snapshot) = self.pending_chat_actions.remove(&(chat_id, group)) {
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
                                let _ = store.rename_chat(&chat_jid, &subject);
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
            BackendEvent::AvatarBytes { jid, picture_id, bytes } => {
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
                    session.avatar_missing.insert(jid, now);
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
                // not worth a global notice.
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
                        .filter(|chat| is_raw_identifier(&chat.title))
                        .filter_map(|chat| session.jids.get(&chat.id).cloned())
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
            BackendEvent::ChatRenamed { chat_jid, title } => {
                self.apply_chat_renamed(&chat_jid, &title);
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

    /// Applies a resolved display name for a chat whose title is
    /// still a raw identifier (identity healing from the backend:
    /// a contact update, a group subject, a LID-to-phone mapping).
    /// The archive rename carries the same guard, so memory and
    /// disk only ever move from identifier to name.
    fn apply_chat_renamed(&mut self, chat_jid: &str, title: &str) {
        if let Some(session) = &mut self.live {
            if let Some(store) = &session.store {
                let _ = store.rename_chat(chat_jid, title);
            }
            // A direct chat's title is the contact's name: it
            // joins the name directory under the chat's JID.
            if !chat_jid.ends_with("@g.us") && !title.is_empty() && !is_raw_identifier(title) {
                session.names.insert(chat_jid.to_owned(), title.to_owned());
            }
        }
        let id = jid_id(chat_jid);
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == id) {
            if chat.title.is_empty() || is_raw_identifier(&chat.title) {
                chat.title = title.to_owned();
            }
        }
    }

    /// Applies a resolved display name for one sender: stored
    /// message labels and previews that still show the sender's
    /// raw identifier are rewritten in the archive and in memory.
    /// Labels that already hold a name are never touched.
    fn apply_sender_named(&mut self, sender_jid: &str, name: &str) {
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

    fn apply_chat_upsert(&mut self, live: &LiveChat) {
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
        self.set_ephemeral_state(chat_id, duration_secs);
        if current != duration_secs {
            self.push_system_notice(
                chat_id,
                &crate::extras::disappearing_notice_text(duration_secs),
            );
        }
    }

    /// Sets a chat's disappearing-timer state (mirror and
    /// archive) without any in-stream announcement. The state
    /// half of [`App::apply_disappearing_changed`], shared with
    /// the DisappearingTimerFailed rollback, which must restore
    /// the previous timer quietly: the announcement the
    /// optimistic apply posted belongs to the failed change,
    /// and a second one would claim a change that never
    /// happened.
    pub(crate) fn set_ephemeral_state(&mut self, chat_id: u64, duration_secs: u32) {
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
    pub(crate) fn apply_mute_changed(&mut self, chat_id: u64, muted: bool, mute_until: i64) {
        // The end counts only while the chat is muted; a past
        // end with muted = false is how a lapsed timed mute
        // reconciles, and the field clears with the flag.
        let until =
            if muted && mute_until > 0 { u64::try_from(mute_until).unwrap_or(0) } else { 0 };
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.muted = muted;
            c.mute_until = until;
        }
        if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            c.muted = muted;
            c.mute_until = until;
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
        // A server report settles any star change of mine still
        // pending for this message: the reported state is the
        // state now, whether it confirms my change or supersedes
        // it, so nothing remains to roll back.
        self.pending_stars.remove(&msg_id);
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

    /// Restores the pin state a refused pin change started
    /// from (see [`BackendEvent::PinFailed`]): the mirror flips
    /// back and the flag write-through updates the session flag
    /// map and the archive row, exactly how `apply_star_changed`
    /// lands a star rollback, minus any backend command (the
    /// change being undone never reached the server).
    pub(crate) fn apply_pin_restored(
        &mut self,
        chat_id: u64,
        msg_id: u64,
        protocol_id: &str,
        pinned: bool,
    ) {
        // The protocol-id bookkeeping an upsert would have done,
        // so the flag write-through below finds the archive row.
        self.protocol_ids.insert(msg_id, protocol_id.to_owned());
        if let Some(entry) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            for (_, m) in entry.iter_mut() {
                if m.id == msg_id {
                    m.pinned = pinned;
                }
            }
        }
        self.set_live_flag(chat_id, msg_id, store::FLAG_PINNED_MESSAGE, pinned);
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Applies a pin state the server reported (a pin made on
    /// another device, or by another member): the mirror flips
    /// and the flag write-through persists it, the same landing
    /// a rollback takes. Nothing of mine is in flight for
    /// someone else's pin, so no pending entry is involved.
    fn apply_server_pin_changed(&mut self, chat_jid: &str, protocol_id: &str, pinned: bool) {
        let chat_id = jid_id(chat_jid);
        let Some((&msg_id, _)) = self.protocol_ids.iter().find(|(_, p)| p.as_str() == protocol_id)
        else {
            return;
        };
        if let Some(entry) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            for (_, m) in entry.iter_mut() {
                if m.id == msg_id {
                    m.pinned = pinned;
                }
            }
        }
        self.set_live_flag(chat_id, msg_id, store::FLAG_PINNED_MESSAGE, pinned);
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Applies a keep change for a disappearing message, from
    /// any source: my own toggle, a MessageKept report (a keep
    /// made on another device), or a KeepFailed rollback. One
    /// path for all three, like apply_star_changed: the mirror
    /// flips and the flag write-through persists it, which is
    /// also what protects the value from uninformed arrival
    /// upserts. A server report settles any pending change of
    /// mine for the message.
    pub(crate) fn apply_keep_changed(
        &mut self,
        chat_id: u64,
        msg_id: u64,
        protocol_id: &str,
        kept: bool,
    ) {
        self.protocol_ids.insert(msg_id, protocol_id.to_owned());
        self.pending_keeps.remove(&msg_id);
        if let Some(entry) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            for (_, m) in entry.iter_mut() {
                if m.id == msg_id {
                    m.kept = kept;
                }
            }
        }
        self.set_live_flag(chat_id, msg_id, store::FLAG_KEPT, kept);
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Writes one chat's archive row from the mirror, so the
    /// stored row carries the state the list shows. For mirror
    /// changes that arrive without a chat upsert of their own
    /// (the unread badge an incoming message raised), whose own
    /// write-through would otherwise never happen.
    pub(crate) fn persist_chat_row(&self, chat_id: u64) {
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

    /// Applies a chat deletion the server reported: the chat
    /// was deleted on the phone, or my own DeleteChat command
    /// was confirmed back (section 24: a whole-chat removal is
    /// destructive, so its local apply waits for the outcome;
    /// a DeleteChatFailed leaves the chat fully in place). The
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
        // The deleted chat's draft leaves the persisted set
        // with the chat, or a restart would resurrect it.
        self.persist_settings();
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

    /// Applies a delete-for-me: the server reported it (one
    /// message deleted for the account on the phone), or my own
    /// delete-for-me action applied it locally ahead of its
    /// command (section 24, the revoke precedent). Nothing is
    /// sent back from here; the adoption is the removal of
    /// exactly that
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

    /// Rolls a chat action back after a ChatActionFailed: the
    /// fields the failed action owns (archive, pin, or the mute
    /// pair) return to the snapshot taken when the action was
    /// sent, in the mirror and the archive alike. Fields other
    /// actions own are untouched, so a settled sibling action
    /// keeps its state.
    pub(crate) fn restore_chat_action(
        &mut self,
        chat_id: u64,
        group: &str,
        snapshot: (bool, bool, bool, i64),
    ) {
        let (archived, pinned, muted, mute_until) = snapshot;
        let until = u64::try_from(mute_until).unwrap_or(0);
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            match group {
                "archive" => c.archived = archived,
                "pin" => c.pinned = pinned,
                _ => {
                    c.muted = muted;
                    c.mute_until = until;
                }
            }
        }
        if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            match group {
                "archive" => c.archived = archived,
                "pin" => c.pinned = pinned,
                _ => {
                    c.muted = muted;
                    c.mute_until = until;
                }
            }
        }
        self.persist_chat_row(chat_id);
    }

    /// Puts a message back after a DeleteForMeFailed: the
    /// delete never reached the server, so the message is still
    /// the account's. Its archived row is written back first;
    /// then the session mirror re-adopts it with the helper
    /// entries its upsert would have built (media refs,
    /// thumbnails, mentions, flags, protocol bookkeeping), the
    /// row preview is recomputed, and the open conversation
    /// re-syncs. The message's saved download, removed by the
    /// optimistic apply, is not restored: it is a local copy of
    /// media the message still names, fetchable again.
    pub(crate) fn restore_deleted_for_me(
        &mut self,
        chat_id: u64,
        row: &backend::LiveMessage,
        flags: i64,
    ) {
        if let Some(session) = &self.live {
            if let Some(store) = &session.store {
                let _ = store.upsert_message(row, flags);
            }
        }
        let msg_id = message_id(&row.chat_jid, &row.id);
        if let Some(session) = &mut self.live {
            if let Some(reference) = &row.photo_ref {
                session.photo_refs.insert(msg_id, reference.clone());
            }
            if let Some(reference) = &row.voice_ref {
                session.voice_refs.insert(msg_id, reference.clone());
            }
            if let Some(reference) = &row.video_ref {
                session.video_refs.insert(msg_id, reference.clone());
            }
            if let Some(thumb) = &row.video_thumbnail {
                session.video_thumbs.insert(msg_id, thumb.clone());
            }
            if let Some(reference) = &row.sticker_ref {
                session.sticker_refs.insert(msg_id, reference.clone());
            }
            if let Some(thumb) = &row.photo_thumbnail {
                session.photo_thumbs.insert(msg_id, thumb.clone());
            }
            if !row.mentions.is_empty() {
                session.message_mentions.insert(msg_id, row.mentions.clone());
            }
            if flags != 0 {
                session.flags.insert(msg_id, flags);
            }
            let entry = session.messages.entry(chat_id).or_default();
            if !entry.iter().any(|(_, m)| m.id == msg_id) {
                entry.push((row.timestamp, to_message_with_flags(row, flags)));
                entry.sort_by_key(|(ts, _)| *ts);
            }
        }
        self.protocol_ids.insert(msg_id, row.id.clone());
        if let Some(jid) = &row.sender_jid {
            self.sender_jids.insert(msg_id, jid.clone());
        }
        self.refresh_preview_after_removal(chat_id);
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Copies a poll's aggregated state from the session mirror
    /// into the open conversation's copy of the message. The
    /// mirror is where apply_poll_vote aggregates; the
    /// conversation is a separate rendering copy, and a rollback
    /// must move both.
    pub(crate) fn sync_poll_from_mirror(&mut self, chat_id: u64, mid: u64) {
        let restored = self
            .live
            .as_ref()
            .and_then(|s| s.messages.get(&chat_id))
            .and_then(|msgs| msgs.iter().find(|(_, m)| m.id == mid))
            .and_then(|(_, m)| match &m.kind {
                MessageKind::Poll { options, my_votes, .. } => {
                    Some((options.clone(), my_votes.clone()))
                }
                _ => None,
            });
        if let Some((options, my_votes)) = restored {
            for m in self.conversation.iter_mut().filter(|m| m.id == mid) {
                if let MessageKind::Poll { options: o, my_votes: v, .. } = &mut m.kind {
                    *o = options.clone();
                    *v = my_votes.clone();
                }
            }
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

    /// Applies one decrypted poll vote: the voter's whole current
    /// selection replaces their previous ballot, so counts move by
    /// the difference only and re-applying the same ballot is a
    /// no-op. The mirror poll is updated first; a poll the mirror
    /// does not hold (its chat was never loaded this session) is
    /// updated straight in the archive. Either way the resulting
    /// tally snapshot is persisted, so the counts and the ballots
    /// they came from survive a restart.
    pub(crate) fn apply_poll_vote(
        &mut self,
        chat_jid: &str,
        poll_id: &str,
        voter: &str,
        from_me: bool,
        selected: &[usize],
    ) {
        let chat_id = jid_id(chat_jid);
        let mid = message_id(chat_jid, poll_id);
        let mut tally: Option<PollTallyWrite> = None;
        if let Some(session) = &mut self.live {
            let ballots = session.poll_ballots.get(&mid).cloned().unwrap_or_default();
            if let Some(entry) = session.messages.get_mut(&chat_id) {
                if let Some((_, msg)) = entry.iter_mut().find(|(_, m)| m.id == mid) {
                    if let MessageKind::Poll { options, my_votes, .. } = &mut msg.kind {
                        let core::rich::PollTally {
                            options: new_options,
                            my_votes: new_mine,
                            ballots: new_ballots,
                        } = core::rich::apply_poll_ballot(
                            options, my_votes, &ballots, voter, from_me, selected,
                        );
                        *options = new_options.clone();
                        *my_votes = new_mine.clone();
                        tally = Some(PollTallyWrite {
                            chat_jid: chat_jid.to_owned(),
                            message_id: poll_id.to_owned(),
                            counts: new_options.iter().map(|(_, c)| *c).collect(),
                            my_votes: new_mine,
                            ballots: new_ballots,
                        });
                    }
                }
            }
            if let Some(tally) = &tally {
                if tally.ballots.is_empty() {
                    session.poll_ballots.remove(&mid);
                } else {
                    session.poll_ballots.insert(mid, tally.ballots.clone());
                }
            }
        }
        if tally.is_none() {
            // The mirror never loaded this poll (its chat is not in
            // memory): aggregate against the archived copy and
            // write the tally straight back to the archive.
            if let Some(store) = self.live.as_ref().and_then(|s| s.store.as_ref()) {
                if let Ok(rows) = store.messages(chat_jid) {
                    if let Some((archived, _)) = rows.into_iter().find(|(m, _)| m.id == poll_id) {
                        if let backend::LiveMessageKind::Poll { options, my_votes, .. } =
                            &archived.kind
                        {
                            let core::rich::PollTally {
                                options: new_options,
                                my_votes: new_mine,
                                ballots: new_ballots,
                            } = core::rich::apply_poll_ballot(
                                options,
                                my_votes,
                                &archived.poll_ballots,
                                voter,
                                from_me,
                                selected,
                            );
                            let counts: Vec<u32> = new_options.iter().map(|(_, c)| *c).collect();
                            let _ = store.set_poll_tally(
                                chat_jid,
                                poll_id,
                                &counts,
                                &new_mine,
                                &new_ballots,
                            );
                        }
                    }
                }
            }
            return;
        }
        if let Some(tally) = tally {
            self.persist_poll_tally(tally);
        }
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Routes one poll tally snapshot to the archive: queued into
    /// the running event batch when there is one (its flush writes
    /// tallies after the batch's upserts), written immediately
    /// otherwise.
    fn persist_poll_tally(&mut self, tally: PollTallyWrite) {
        if self.event_batch.active {
            self.event_batch.poll_tally_writes.push(tally);
            return;
        }
        if let Some(store) = self.live.as_ref().and_then(|s| s.store.as_ref()) {
            let _ = store.set_poll_tally(
                &tally.chat_jid,
                &tally.message_id,
                &tally.counts,
                &tally.my_votes,
                &tally.ballots,
            );
        }
    }

    /// Applies one backend message upsert. Returns true only when
    /// the upsert pushed a genuinely new incoming message (not an
    /// echo replacement and not a re-upsert of a known id).
    pub(crate) fn apply_message_upsert(&mut self, live: &LiveMessage) -> bool {
        if live.chat_jid == "status@broadcast" {
            // A status post is not a message in a conversation
            // (the backend drops these at translation; this is
            // the same rule at the app boundary, so no source
            // can file one as a fake DM): no mirror row, no
            // archive write, no unread, no notification. The
            // Updates feed round owns received statuses.
            return false;
        }
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
            if let Some(reference) = &live.voice_ref {
                session.voice_refs.insert(msg_id, reference.clone());
            }
            if let Some(reference) = &live.video_ref {
                session.video_refs.insert(msg_id, reference.clone());
            }
            if let Some(thumb) = &live.video_thumbnail {
                session.video_thumbs.insert(msg_id, thumb.clone());
            }
            if let Some(reference) = &live.sticker_ref {
                session.sticker_refs.insert(msg_id, reference.clone());
            }
            if let Some(thumb) = &live.photo_thumbnail {
                session.photo_thumbs.insert(msg_id, thumb.clone());
            }
            if !live.mentions.is_empty() {
                session.message_mentions.insert(msg_id, live.mentions.clone());
            }
            // The sender's resolved name joins the name
            // directory under both of the sender's JID forms
            // when the session knows them, so mentions of this
            // sender resolve wherever they appear.
            if let (Some(jid), Some(name)) = (&live.sender_jid, &live.sender) {
                if !name.is_empty() && !is_raw_identifier(name) {
                    session.names.insert(jid.clone(), name.clone());
                }
            }
            let mut flags = session.flags.get(&msg_id).copied().unwrap_or(0);
            if live.starred {
                flags |= store::FLAG_STARRED;
            }
            if live.pinned {
                flags |= store::FLAG_PINNED_MESSAGE;
            }
            // A server copy already carrying the state a pending
            // star or pin change was aiming at settles that
            // change: the backend accepted it, so a failure
            // report arriving later for it must not roll anything
            // back. A copy showing neither state settles nothing
            // (it may predate the change). No event reports a
            // message pin adopted elsewhere, so this adoption is
            // the pin's only confirmation path.
            if live.starred && self.pending_stars.get(&msg_id) == Some(&false) {
                self.pending_stars.remove(&msg_id);
            }
            if live.pinned && self.pending_pins.get(&msg_id) == Some(&false) {
                self.pending_pins.remove(&msg_id);
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
            let mut converted = to_message_with_flags(live, flags);
            // A poll creation re-delivered (history sync, a
            // receipt-driven re-upsert) carries zero counts and no
            // ballots: keep the tallies the mirror has already
            // aggregated, the same merge the archive write applies.
            if let MessageKind::Poll { options: new_options, my_votes: new_mine, .. } =
                &mut converted.kind
            {
                if let Some((_, old)) = entry.iter().find(|(_, m)| m.id == converted.id) {
                    if let MessageKind::Poll { options: old_options, my_votes: old_mine, .. } =
                        &old.kind
                    {
                        if old_options.len() == new_options.len() {
                            for (new, old_count) in new_options.iter_mut().zip(old_options.iter()) {
                                new.1 = old_count.1;
                            }
                            *new_mine = old_mine.clone();
                        }
                    }
                }
            }
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
                mute_until: 0,
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

    /// Writes my own reaction into the actor map and derives
    /// the message's counts from it, the exact bookkeeping the
    /// ReactionUpdate handler runs when the server reports a
    /// reaction. The optimistic send path and the failure
    /// rollback both go through here so local state has a single
    /// representation: when the server's echo of my change
    /// arrives it re-applies the identical write and nothing
    /// moves, and a rollback is the same write with the older
    /// value. `mine` is my current emoji, or None for no
    /// reaction.
    pub(crate) fn apply_my_reaction_to_actors(
        &mut self,
        chat_id: u64,
        msg_id: u64,
        mine: Option<&str>,
    ) {
        let key = format!("{chat_id}:{msg_id}");
        let counts = {
            let actors = self.reaction_actors.entry(key.clone()).or_default();
            match mine {
                Some(emoji) => {
                    actors.insert("Me".to_owned(), emoji.to_owned());
                }
                None => {
                    actors.remove("Me");
                }
            }
            count_reactions(actors)
        };
        if let Some(entry) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            for (_, m) in entry.iter_mut() {
                if m.id == msg_id {
                    m.reactions = counts.clone();
                }
            }
        }
        if counts.is_empty() {
            self.reaction_actors.remove(&key);
        }
        if self.selected_chat == chat_id {
            if let Some(msg) = self.conversation.iter_mut().find(|m| m.id == msg_id) {
                msg.reactions = counts;
            }
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
        self.pending_reactions.remove(&msg_id);
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

    /// Whether bundled demo fixtures may stand in as content:
    /// demo mode, and the fake backend's test mode (whose data
    /// is scripted fixtures, labelled test data). In a real live
    /// session they may never: demo material presented as a real
    /// person's data is a fabrication, however small. Every
    /// fixture lookup (demo photo, demo avatars) gates on this
    /// one predicate.
    pub(crate) fn demo_fixtures_allowed(&self) -> bool {
        !self.live_mode || self.live.as_ref().is_some_and(|s| s.fake)
    }

    /// Whether the generated demo photo may stand in for a photo
    /// message's pixels (see [`App::demo_fixtures_allowed`]). In
    /// a real live session a received photo shows its own bytes
    /// or an honest placeholder, never a substitute image.
    pub(crate) fn photo_demo_allowed(&self) -> bool {
        self.demo_fixtures_allowed()
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

    /// What one sticker message's tile may honestly show,
    /// decided exactly like [`App::photo_tile`]: the sticker's
    /// own decoded bytes, a fetch in flight, a failed fetch, or
    /// not yet requested. There is no demo variant: a sticker is
    /// never substituted with another image.
    pub(crate) fn sticker_tile(&self, msg_id: u64) -> crate::media::PhotoTile {
        if self.media.textures.contains_key(&msg_id) {
            return crate::media::PhotoTile::Real;
        }
        match self.media.sticker_fetch.get(&msg_id) {
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

    /// Drops every cached form of one identity's picture
    /// (decoded texture, staged bytes, the store row, the
    /// negative and the in-flight marks), so the next display
    /// refetches through `ensure_avatar`. The one invalidation
    /// path, shared by the wire's picture-change notification
    /// and by our own confirmed picture writes.
    pub(crate) fn drop_avatar(&mut self, jid: &str) {
        self.media.live_avatar_textures.remove(jid);
        if let Some(session) = &mut self.live {
            if let Some(store) = &session.store {
                let _ = store.delete_avatar(jid);
            }
            session.avatar_bytes.remove(jid);
            session.avatar_missing.remove(jid);
            session.avatar_requested.remove(jid);
        }
    }

    /// Ensures one identity's profile picture is on its way or
    /// already here, called by every surface that paints an
    /// avatar. Resolution order: the decoded texture cache, the
    /// store-staged bytes (decoded here, where a graphics
    /// context exists), a fresh-enough negative, and finally a
    /// backend fetch, deduplicated per JID. In demo mode the
    /// demo portraits answer instead and no fetch is sent.
    pub(crate) fn ensure_avatar(&mut self, ctx: &egui::Context, jid: &str) {
        if self.live.is_none() {
            return;
        }
        if self.media.live_avatar_textures.contains_key(jid) {
            return;
        }
        let staged = self.live.as_ref().and_then(|s| s.avatar_bytes.get(jid).cloned());
        if let Some(bytes) = staged {
            if let Some(tex) = crate::media::decode_photo(ctx, &bytes) {
                self.media.live_avatar_textures.insert(jid.to_owned(), tex);
                return;
            }
        }
        let Some(session) = &mut self.live else { return };
        if session.avatar_requested.contains(jid) {
            return;
        }
        let now = i64::try_from(crate::state::now_unix()).unwrap_or(0);
        if let Some(answered_at) = session.avatar_missing.get(jid) {
            // A negative younger than a day stands; an older
            // one is re-asked, because people set pictures.
            if now - answered_at < 86_400 {
                return;
            }
        }
        session.avatar_requested.insert(jid.to_owned());
        let _ = session.channels.commands.send(BackendCommand::FetchAvatar { jid: jid.to_owned() });
    }

    /// The decoded live profile picture for one identity, when
    /// the avatar pipeline has produced one. Surfaces call
    /// `ensure_avatar` first and paint this through the shared
    /// avatar dispatch; `None` paints the initial disc.
    pub(crate) fn live_avatar(&self, jid: &str) -> Option<&egui::TextureHandle> {
        self.media.live_avatar_textures.get(jid)
    }

    /// The profile photo for one identity addressed by JID (a
    /// group member, a message sender): the real picture through
    /// the avatar pipeline in a live session, the bundled
    /// portrait for the name in demo mode.
    pub(crate) fn member_avatar_photo(
        &mut self,
        ctx: &egui::Context,
        jid: &str,
        label: &str,
    ) -> Option<egui::TextureId> {
        if self.live.is_some() {
            self.ensure_avatar(ctx, jid);
            self.live_avatar(jid).map(|tex| tex.id())
        } else {
            self.demo_avatar(ctx, label)
        }
    }

    /// The avatar photo for one chat, for every surface that
    /// paints a chat's avatar (list row, conversation header,
    /// call card): in a live session, the chat JID's real
    /// profile picture through the avatar pipeline; otherwise
    /// the demo portrait for a DM persona. `None` paints the
    /// initial disc or group glyph.
    pub(crate) fn chat_avatar_photo(
        &mut self,
        ctx: &egui::Context,
        chat: &core::model::Chat,
    ) -> Option<egui::TextureId> {
        if self.live.is_some() {
            let jid = self.live.as_ref().and_then(|s| s.jids.get(&chat.id)).cloned();
            if let Some(jid) = jid {
                self.ensure_avatar(ctx, &jid);
                return self.live_avatar(&jid).map(|tex| tex.id());
            }
            return None;
        }
        if chat.kind == core::model::ChatKind::Dm {
            return self.demo_avatar(ctx, &chat.title);
        }
        None
    }

    /// Ensures one sticker's bytes are on their way, mirroring
    /// [`App::ensure_photo_fetch`] through the sticker fetch
    /// path.
    pub(crate) fn ensure_sticker_fetch(&mut self, msg_id: u64) {
        if self.media.textures.contains_key(&msg_id) {
            return;
        }
        if self.media.sticker_fetch.contains_key(&msg_id) {
            return;
        }
        let (chat_jid, sticker_ref) = {
            let Some(session) = &self.live else { return };
            let Some(chat_jid) = session.jids.get(&self.selected_chat).cloned() else {
                return;
            };
            (chat_jid, session.sticker_refs.get(&msg_id).cloned())
        };
        let Some(protocol_id) = self.protocol_ids.get(&msg_id).cloned() else {
            self.media.sticker_fetch.insert(msg_id, crate::state::PhotoFetchState::Failed);
            return;
        };
        self.media.sticker_fetch.insert(msg_id, crate::state::PhotoFetchState::Loading);
        self.send_backend(BackendCommand::FetchSticker {
            chat_jid,
            message_id: protocol_id,
            sticker_ref,
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

    /// The play/pause gesture on one voice message. A note already
    /// loaded in the engine pauses or resumes (a finished note
    /// restarts from the top); any other note ends the current
    /// session and begins through [`App::begin_voice`], so what
    /// the user hears is always real decoded audio or a stated
    /// failure, never a moving playhead over silence.
    pub(crate) fn toggle_voice(&mut self, msg_id: u64) {
        let current = self.media.voice_player.as_ref().is_some_and(|p| p.id == msg_id);
        if current {
            let player = self.media.voice_player.as_ref().expect("checked above");
            if player.is_ended() {
                player.seek_fraction(0.0);
                player.resume();
            } else {
                player.toggle_pause();
            }
            return;
        }
        self.media.voice_player = None;
        self.begin_voice(msg_id, None);
    }

    /// The seek gesture on one voice message's waveform. A loaded
    /// note seeks in the engine; any other note begins at the
    /// sought position (the seek is parked and applied when its
    /// player is created, if its bytes still have to be fetched).
    pub(crate) fn seek_voice(&mut self, msg_id: u64, fraction: f32) {
        let current = self.media.voice_player.as_ref().is_some_and(|p| p.id == msg_id);
        if current {
            self.media
                .voice_player
                .as_ref()
                .expect("checked above")
                .seek_fraction(f64::from(fraction));
            return;
        }
        self.media.voice_player = None;
        self.begin_voice(msg_id, Some(fraction));
    }

    /// Begins hearing one voice note: from the decoded PCM still
    /// held for it when there is some, otherwise by fetching its
    /// bytes. Demo mode has no session and its notes are scripted
    /// fixtures with no audio behind them; that is said plainly
    /// instead of animating a playhead over nothing.
    fn begin_voice(&mut self, msg_id: u64, seek: Option<f32>) {
        self.media.voice_wanted = Some(msg_id);
        if let Some((held, pcm)) = &self.media.voice_pcm {
            if *held == msg_id {
                let pcm = std::sync::Arc::clone(pcm);
                self.start_voice_player(msg_id, pcm, seek);
                return;
            }
        }
        if self.live.is_none() {
            self.media.voice_fetch.insert(msg_id, crate::state::VoiceFetchState::Failed);
            self.notice = Some(Notice::info(crate::i18n::t("voice.demo_unavailable")));
            return;
        }
        if self.media.voice_fetch.get(&msg_id) == Some(&crate::state::VoiceFetchState::Loading) {
            // A fetch for this note is already running; only the
            // parked seek can still change.
            if let Some(fraction) = seek {
                self.media.voice_pending_seek.insert(msg_id, fraction);
            }
            return;
        }
        let (chat_jid, voice_ref) = {
            let Some(session) = &self.live else { return };
            let Some(chat_jid) = session.jids.get(&self.selected_chat).cloned() else {
                return;
            };
            (chat_jid, session.voice_refs.get(&msg_id).cloned())
        };
        let Some(protocol_id) = self.protocol_ids.get(&msg_id).cloned() else {
            // A live voice note without a protocol id can never be
            // addressed; record the failure instead of spinning.
            self.media.voice_fetch.insert(msg_id, crate::state::VoiceFetchState::Failed);
            return;
        };
        self.media.voice_fetch.insert(msg_id, crate::state::VoiceFetchState::Loading);
        if let Some(fraction) = seek {
            self.media.voice_pending_seek.insert(msg_id, fraction);
        }
        self.send_backend(BackendCommand::FetchVoice {
            chat_jid,
            message_id: protocol_id,
            voice_ref,
        });
    }

    /// Creates the playback engine session for one note whose PCM
    /// is here. A player that will not start (no output device,
    /// a refused stream) fails the note honestly and says why.
    fn start_voice_player(&mut self, msg_id: u64, pcm: std::sync::Arc<[i16]>, seek: Option<f32>) {
        let rate = f64::from(self.voice_speed(msg_id));
        match crate::player::VoicePlayer::start(msg_id, pcm, rate, self.media.voice_sink) {
            Ok(player) => {
                if let Some(fraction) = seek {
                    player.seek_fraction(f64::from(fraction));
                }
                self.media.voice_fetch.remove(&msg_id);
                self.media.voice_player = Some(player);
            }
            Err(message) => {
                self.media.voice_fetch.insert(msg_id, crate::state::VoiceFetchState::Failed);
                self.notice = Some(Notice::error(message));
            }
        }
    }

    /// Closes the media viewer on one message (the viewer is
    /// shared by photos and videos). A view-once photo consumes
    /// its single view here, and only under the honesty rule: in
    /// a real live session the view is spent solely when the
    /// photo's real pixels were actually painted during this
    /// viewing (tracked in `MediaState::view_once_shown`). When
    /// they never arrived, the allowance survives, the failure is
    /// said out loud, and the photo can be opened again later.
    /// Demo and test mode keep the standing behaviour (their
    /// fixture photo is the content, so closing spends the view).
    /// A spent live view also drops the decoded pixels and the
    /// archived re-download reference, so nothing can bring the
    /// photo back afterwards. A view-once video follows the same
    /// rule against its own presentation record
    /// (`MediaState::video_presented`).
    pub(crate) fn close_photo_viewer(&mut self, id: u64, view_once: bool) {
        // The video player belongs to this viewing; closing the
        // viewer stops it (dropping the player stops its worker
        // thread).
        if self.media.video_player.as_ref().is_some_and(|p| p.key == id) {
            self.media.video_player = None;
        }
        if view_once {
            // A view-once video follows the same honesty rule as
            // a view-once photo, gated on its own presentation
            // record: the view is spent only when the video's
            // real content (its thumbnail or a decoded frame)
            // was actually presented during this viewing.
            let is_video = self
                .conversation
                .iter()
                .any(|m| m.id == id && matches!(m.kind, core::model::MessageKind::Video { .. }));
            if is_video {
                let spent = self.photo_demo_allowed() || self.media.video_presented == Some(id);
                if spent {
                    self.mark_view_once_opened(id);
                    if !self.photo_demo_allowed() {
                        self.media.video_bytes.remove(&id);
                        self.media.video_textures.remove(&id);
                        self.drop_video_ref(id);
                    }
                } else {
                    self.notice = Some(Notice::info(crate::i18n::t("video.view_once_not_loaded")));
                }
            } else {
                let spent = self.photo_demo_allowed() || self.media.view_once_shown == Some(id);
                if spent {
                    self.mark_view_once_opened(id);
                    if !self.photo_demo_allowed() {
                        self.media.textures.remove(&id);
                        self.drop_photo_ref(id);
                    }
                } else {
                    self.notice = Some(Notice::info(crate::i18n::t("photo.view_once_not_loaded")));
                }
            }
        }
        if self.media.view_once_shown == Some(id) {
            self.media.view_once_shown = None;
        }
        if self.media.video_presented == Some(id) {
            self.media.video_presented = None;
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

    /// What a video message's tile may honestly show right now
    /// (see [`crate::media::VideoTile`]): the video's own
    /// embedded thumbnail when those bytes are here, otherwise
    /// the neutral tile. Unlike photos there is no demo variant:
    /// a video never paints a generated picture, in any mode.
    pub(crate) fn video_tile(&self, msg_id: u64) -> crate::media::VideoTile {
        if self.media.video_textures.contains_key(&msg_id)
            || self.live.as_ref().is_some_and(|s| s.video_thumbs.contains_key(&msg_id))
        {
            crate::media::VideoTile::Thumb
        } else {
            crate::media::VideoTile::Neutral
        }
    }

    /// The decoded texture for one video's thumbnail, with its
    /// aspect ratio (width over height), decoding and caching
    /// the wire bytes on first use. The poster's own dimensions
    /// are the frame's, so the aspect sizes the tile exactly
    /// like a photo's. `None` when the video carries no
    /// thumbnail bytes (or they do not decode): the neutral
    /// tile paints instead. An associated function over the two
    /// state fields it touches, so paint code that already
    /// borrows other parts of the app can still call it.
    pub(crate) fn video_thumb_texture(
        media: &mut crate::state::MediaState,
        live: Option<&LiveSession>,
        ctx: &egui::Context,
        msg_id: u64,
    ) -> Option<(egui::TextureId, f32)> {
        if let Some(handle) = media.video_textures.get(&msg_id) {
            let size = handle.size_vec2();
            return Some((handle.id(), size.x / size.y.max(1.0)));
        }
        let bytes = live?.video_thumbs.get(&msg_id)?.clone();
        let handle = crate::media::decode_photo(ctx, &bytes)?;
        let size = handle.size_vec2();
        let id = handle.id();
        media.video_textures.insert(msg_id, handle);
        Some((id, size.x / size.y.max(1.0)))
    }

    /// The decoded texture of one photo's wire thumbnail, with
    /// its aspect ratio (width over height): the photo itself
    /// at preview fidelity, decoded once into the thumbnail
    /// cache. Mirrors [`App::video_thumb_texture`].
    pub(crate) fn photo_thumb_texture(
        media: &mut crate::state::MediaState,
        live: Option<&LiveSession>,
        ctx: &egui::Context,
        msg_id: u64,
    ) -> Option<(egui::TextureId, f32)> {
        if let Some(handle) = media.photo_thumb_textures.get(&msg_id) {
            let size = handle.size_vec2();
            return Some((handle.id(), size.x / size.y.max(1.0)));
        }
        let bytes = live?.photo_thumbs.get(&msg_id)?.clone();
        let handle = crate::media::decode_photo(ctx, &bytes)?;
        let size = handle.size_vec2();
        let id = handle.id();
        media.photo_thumb_textures.insert(msg_id, handle);
        Some((id, size.x / size.y.max(1.0)))
    }

    /// Where pressing play on one video message stands (see
    /// [`crate::media::VideoPlayRoute`]). One decision function
    /// for the bubble and the viewer, so the play gesture can
    /// never drift from the fetch state it depends on.
    pub(crate) fn video_play_route(&self, msg_id: u64) -> crate::media::VideoPlayRoute {
        if !self.live_mode {
            return crate::media::VideoPlayRoute::DemoUnavailable;
        }
        if self.media.video_files.contains_key(&msg_id)
            || self.media.video_bytes.contains_key(&msg_id)
        {
            return crate::media::VideoPlayRoute::Ready;
        }
        match self.media.video_fetch.get(&msg_id) {
            Some(crate::state::PhotoFetchState::Loading) => crate::media::VideoPlayRoute::Loading,
            Some(crate::state::PhotoFetchState::Failed) => crate::media::VideoPlayRoute::Failed,
            None => crate::media::VideoPlayRoute::Idle,
        }
    }

    /// Starts the byte fetch for one video message of the open
    /// chat, unless its bytes are already here or a fetch is
    /// already running. The fetch passes the message's archived
    /// re-download reference along, so a video the backend no
    /// longer holds in its session cache is still fetchable.
    /// Only live sessions fetch; demo mode's scripted videos
    /// have no bytes to fetch (see `video_play_route`).
    pub(crate) fn ensure_video_fetch(&mut self, msg_id: u64, view_once: bool) {
        if !self.live_mode
            || self.media.video_files.contains_key(&msg_id)
            || self.media.video_bytes.contains_key(&msg_id)
            || self.media.video_fetch.contains_key(&msg_id)
        {
            return;
        }
        let (chat_jid, video_ref) = {
            let Some(session) = &self.live else { return };
            let Some(chat_jid) = session.jids.get(&self.selected_chat).cloned() else {
                return;
            };
            (chat_jid, session.video_refs.get(&msg_id).cloned())
        };
        let Some(protocol_id) = self.protocol_ids.get(&msg_id).cloned() else {
            // A live video without a protocol id can never be
            // addressed; record the failure instead of spinning.
            self.media.video_fetch.insert(msg_id, crate::state::PhotoFetchState::Failed);
            return;
        };
        self.media.video_fetch.insert(msg_id, crate::state::PhotoFetchState::Loading);
        self.send_backend(BackendCommand::FetchVideo {
            chat_jid,
            message_id: protocol_id,
            video_ref,
            view_once,
        });
    }

    /// Re-runs a failed video byte fetch (the viewer's retry
    /// gesture).
    pub(crate) fn retry_video_fetch(&mut self, msg_id: u64, view_once: bool) {
        if self.media.video_fetch.get(&msg_id) == Some(&crate::state::PhotoFetchState::Failed) {
            self.media.video_fetch.remove(&msg_id);
        }
        self.ensure_video_fetch(msg_id, view_once);
    }

    /// Creates the video player for one message whose bytes are
    /// here: a regular video plays from its cache file, a
    /// view-once video from memory. A player for a different
    /// message is replaced (dropping it stops its worker).
    /// `seconds` is the message's stated duration; the player
    /// corrects it from the stream itself once demuxed.
    pub(crate) fn start_video_player(&mut self, msg_id: u64, seconds: u32) {
        if self.media.video_player.as_ref().is_some_and(|p| p.key == msg_id) {
            return;
        }
        let source = if let Some(path) = self.media.video_files.get(&msg_id) {
            crate::video::VideoSource::File(std::path::PathBuf::from(path))
        } else if let Some(bytes) = self.media.video_bytes.get(&msg_id) {
            crate::video::VideoSource::Bytes(bytes.clone())
        } else {
            return;
        };
        let info =
            crate::video::VideoInfo { width: 0, height: 0, duration_secs: f64::from(seconds) };
        self.media.video_player = Some(crate::video::VideoPlayer::start(msg_id, source, info));
    }

    /// Removes one message's video re-download reference and
    /// thumbnail from the session maps and from the archive.
    fn drop_video_ref(&mut self, msg_id: u64) {
        let protocol_id = self.protocol_ids.get(&msg_id).cloned();
        let Some(session) = &mut self.live else { return };
        session.video_refs.remove(&msg_id);
        session.video_thumbs.remove(&msg_id);
        let chat_jid = session
            .messages
            .iter()
            .find(|(_, msgs)| msgs.iter().any(|(_, m)| m.id == msg_id))
            .and_then(|(chat_id, _)| session.jids.get(chat_id).cloned());
        if let (Some(store), Some(chat_jid), Some(protocol_id)) =
            (&session.store, chat_jid, protocol_id)
        {
            let _ = store.clear_video_ref(&chat_jid, &protocol_id);
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

    /// Which world the My Profile editor runs in, from the
    /// session state: no session is demo mode; a linked session
    /// (real or test mode, both honor the profile commands) edits
    /// the account; a session that is not linked yet has no
    /// account profile to edit.
    pub(crate) fn my_profile_mode(&self) -> crate::profile::MyProfileMode {
        match &self.live {
            // Live mode without a session (a start that failed,
            // or a session ended by logging out) is not demo
            // mode: there is no account to edit until a device
            // links again.
            None if self.live_mode => crate::profile::MyProfileMode::NotLinked,
            None => crate::profile::MyProfileMode::Demo,
            Some(session) if session.status.is_linked() => crate::profile::MyProfileMode::Live,
            Some(_) => crate::profile::MyProfileMode::NotLinked,
        }
    }

    /// Opens the My Profile editor. In live mode it also asks the
    /// backend for a fresh read-back, so the editor opens on the
    /// account's current values rather than the connect-time
    /// ones. A failure left over from a previous save is cleared:
    /// it described that attempt, not this one.
    pub(crate) fn open_my_profile(&mut self) {
        self.my_profile_open = true;
        if self.my_profile.save_phase != crate::profile::SavePhase::Saving {
            self.my_profile.save_phase = crate::profile::SavePhase::Idle;
        }
        if self.my_profile_mode() == crate::profile::MyProfileMode::Live {
            self.send_backend(BackendCommand::FetchOwnProfile);
        }
    }

    /// Applies a Save click in the My Profile editor. Demo mode
    /// persists the buffers locally, as it always has. Live mode
    /// builds the pending update (exactly the fields that differ
    /// from the account read-back), marks the save in flight, and
    /// sends it; the outcome arrives as the backend's re-read or
    /// its failure event, applied by [`App::apply_own_profile`]
    /// and [`App::apply_own_profile_save_failed`]. A save with no
    /// changes just closes the panel.
    pub(crate) fn save_my_profile(&mut self) {
        match self.my_profile_mode() {
            crate::profile::MyProfileMode::Demo => {
                self.persist_settings();
                self.notice = Some(Notice::info(crate::i18n::t("profile.my_saved")));
                self.my_profile_open = false;
            }
            crate::profile::MyProfileMode::Live => match self.my_profile.pending_update() {
                Ok(Some(update)) => {
                    self.my_profile.save_phase = crate::profile::SavePhase::Saving;
                    self.send_backend(BackendCommand::UpdateOwnProfile {
                        name: update.name,
                        about: update.about,
                        photo: update.photo,
                    });
                }
                Ok(None) => {
                    self.my_profile_open = false;
                }
                Err(message) => {
                    self.notice = Some(Notice::error(message));
                }
            },
            crate::profile::MyProfileMode::NotLinked => {}
        }
    }

    /// Opens the photo picker for the My Profile editor and loads
    /// the chosen file. Live mode keeps the file's bytes as the
    /// pending picture, validated as a decodable image here so a
    /// bad pick is refused at pick time rather than at save time;
    /// demo mode keeps the file's path, as it always has.
    pub(crate) fn pick_my_profile_photo(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"])
            .set_title(crate::i18n::t("profile.my_change_photo"))
            .pick_file()
        else {
            return;
        };
        match self.my_profile_mode() {
            crate::profile::MyProfileMode::Live => match std::fs::read(&path) {
                Ok(bytes) if image::load_from_memory(&bytes).is_ok() => {
                    self.my_profile.picked_photo = Some(bytes);
                    self.my_profile.photo_removed = false;
                    self.my_profile.photo_error = false;
                }
                _ => {
                    self.my_profile.photo_error = true;
                }
            },
            _ => {
                self.my_profile.photo_path = Some(path.to_string_lossy().into_owned());
            }
        }
    }

    /// Adopts the account's own profile as read back from the
    /// server: the editor's account truth, and the values the
    /// account surfaces show. A save in flight is resolved by the
    /// read-back (the backend re-reads after every save), so it
    /// completes with the saved notice. A read-back landing while
    /// the user is mid-edit updates the account truth but never
    /// the buffers: what they typed stays theirs until they save
    /// or the panel closes. The settings cache follows the
    /// adopted values only for a real session: a test-mode
    /// session's scripted profile is not the user's account and
    /// must not rewrite it (the same rule as
    /// [`App::apply_privacy_state`]).
    fn apply_own_profile(&mut self, profile: backend::OwnProfile) {
        let account = crate::profile::AccountProfile {
            name: profile.name,
            about: profile.about,
            photo: profile.photo,
        };
        let was_saving = self.my_profile.save_phase == crate::profile::SavePhase::Saving;
        let dirty = self.my_profile.live_dirty();
        let changed = self.my_profile.account.as_ref() != Some(&account);
        if was_saving {
            self.my_profile.save_phase = crate::profile::SavePhase::Idle;
            self.my_profile.adopt_account(account);
            self.notice = Some(Notice::info(crate::i18n::t("profile.my_saved")));
        } else if !dirty {
            self.my_profile.adopt_account(account);
        } else {
            self.my_profile.account = Some(account);
        }
        if changed && !self.my_profile.live_dirty() && self.live.as_ref().is_some_and(|s| !s.fake) {
            self.persist_settings();
        }
    }

    /// A profile save failed on the server: the panel shows the
    /// reason and the buffers revert to the account values the
    /// server kept. The reconciling read-back that follows lands
    /// on a clean editor and changes nothing.
    fn apply_own_profile_save_failed(&mut self, message: String) {
        if self.my_profile.save_phase != crate::profile::SavePhase::Saving {
            return;
        }
        self.my_profile.save_phase = crate::profile::SavePhase::Failed(message);
        if let Some(account) = self.my_profile.account.clone() {
            self.my_profile.adopt_account(account);
        }
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
    /// Applies a favorite toggle for one chat: the mirror flips
    /// at once, and in live mode the flag is written through to
    /// the archive, so it survives restarts. Favorites are an
    /// account property (the phone syncs the set through app
    /// state, and `apply_favorites_sync` adopts it), but the
    /// pinned library exposes no outbound setter for it, so a
    /// toggle made here stays local until the phone's next sync
    /// replaces the set; the archive column the write lands in
    /// is one the chat sync paths never touch, so a later
    /// re-sync of the chat list cannot wipe the flag either. In
    /// demo mode there is no session and no archive: the toggle
    /// is the in-memory fixture flip only, like every other demo
    /// interaction.
    pub(crate) fn apply_favorite(&mut self, chat_id: u64, favorite: bool) {
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.favorite = favorite;
        }
        if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            c.favorite = favorite;
        }
        if let Some(session) = &self.live {
            if let (Some(store), Some(jid)) = (&session.store, session.jids.get(&chat_id)) {
                let _ = store.set_chat_favorite(jid, favorite);
            }
        }
    }

    /// Applies the archive's recorded favorite flags to the
    /// chat mirror. The chat conversion cannot carry them (the
    /// synced chat row has no favorite field), so a live session
    /// overlays them from the archive's own record when it loads
    /// the chat list; every chat the archive does not flag is
    /// set unfavorited, so the mirror never inherits a flag from
    /// anywhere else (least of all from demo fixtures).
    pub(crate) fn apply_stored_favorites(&mut self) {
        let favorites: std::collections::HashSet<u64> = self
            .live
            .as_ref()
            .and_then(|session| session.store.as_ref())
            .and_then(|store| store.chat_favorites().ok())
            .map(|jids| jids.iter().map(|j| jid_id(j)).collect())
            .unwrap_or_default();
        for chat in &mut self.chats {
            chat.favorite = favorites.contains(&chat.id);
        }
        if let Some(c) = self.detached_chat.as_mut() {
            c.favorite = favorites.contains(&c.id);
        }
    }

    /// Adopts the phone's synced favorite set: the list the
    /// backend reports is complete, so every chat not in it is
    /// unfavorited. The mirror and the archive are both updated
    /// (the archive through the same per-chat accessor the local
    /// toggle uses).
    pub(crate) fn apply_favorites_sync(&mut self, chat_jids: &[String]) {
        let favorites: std::collections::HashSet<u64> =
            chat_jids.iter().map(|j| jid_id(j)).collect();
        for chat in &mut self.chats {
            chat.favorite = favorites.contains(&chat.id);
        }
        if let Some(c) = self.detached_chat.as_mut() {
            c.favorite = favorites.contains(&c.id);
        }
        if let Some(session) = &self.live {
            if let Some(store) = &session.store {
                for (id, jid) in &session.jids {
                    let _ = store.set_chat_favorite(jid, favorites.contains(id));
                }
            }
        }
    }

    /// Inserts the placeholder for a message the wire could not
    /// decrypt (its failure mode said to show one), but only
    /// when the message is genuinely absent: a stored or
    /// in-memory copy always wins, and a later successful
    /// decryption upserts the real message over the placeholder
    /// through the normal path.
    pub(crate) fn apply_message_unavailable(
        &mut self,
        chat_jid: &str,
        proto_id: &str,
        sender_jid: Option<String>,
        from_me: bool,
        timestamp: i64,
    ) {
        let chat_id = jid_id(chat_jid);
        let mid = message_id(chat_jid, proto_id);
        if let Some(session) = &self.live {
            if session
                .messages
                .get(&chat_id)
                .is_some_and(|rows| rows.iter().any(|(_, m)| m.id == mid))
            {
                return;
            }
            if let Some(store) = &session.store {
                if store.has_message(chat_jid, proto_id).unwrap_or(false) {
                    return;
                }
            }
        } else {
            return;
        }
        let sender_name = sender_jid
            .as_deref()
            .and_then(|jid| self.live.as_ref().and_then(|s| s.names.get(jid).cloned()));
        let placeholder = backend::LiveMessage::text(
            proto_id.to_owned(),
            chat_jid.to_owned(),
            sender_name,
            "This message could not be decrypted. It may arrive again when your phone reconnects."
                .to_owned(),
            timestamp,
            from_me,
        );
        self.apply_message_upsert(&placeholder);
    }

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
        let (was_archived, was_pinned, was_muted, was_mute_until) = self
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .or_else(|| self.detached_chat.as_ref().filter(|c| c.id == chat_id))
            .map_or((archived, pinned, muted, 0), |c| {
                (c.archived, c.pinned, c.muted, c.mute_until)
            });
        // Snapshot the pre-action state per field group, so a
        // ChatActionFailed can restore exactly the fields its
        // action owns. First change wins: a second action on the
        // same group while the first is unsettled rolls back to
        // the state before both, the last server-known state.
        if self.live.is_some() {
            let snap =
                (was_archived, was_pinned, was_muted, i64::try_from(was_mute_until).unwrap_or(0));
            if archived != was_archived {
                self.pending_chat_actions.entry((chat_id, "archive")).or_insert(snap);
            }
            if pinned != was_pinned {
                self.pending_chat_actions.entry((chat_id, "pin")).or_insert(snap);
            }
            if muted != was_muted {
                self.pending_chat_actions.entry((chat_id, "mute")).or_insert(snap);
            }
        }
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.archived = archived;
            c.pinned = pinned;
            c.muted = muted;
            // This path only ever sets an indefinite mute or an
            // unmute; a timed mute goes through apply_mute_for.
            // Either way any previous timed end is spent.
            c.mute_until = 0;
        } else if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            // The detached announcement chat lives outside the
            // chat list, so the header menu actions fall back to
            // it instead of silently doing nothing.
            c.archived = archived;
            c.pinned = pinned;
            c.muted = muted;
            c.mute_until = 0;
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

    /// Marks a chat as unread (design standard section 24):
    /// the badge appears at once and persists through the
    /// archive, and the wire state follows through the chat
    /// actions facade (the read-state action with read =
    /// false), never through the receipt path, which would
    /// tell the sender their messages were read. Demo mode
    /// applies the same local change with no command behind it.
    pub(crate) fn mark_chat_unread(&mut self, chat_id: u64) {
        let current = self.chats.iter().find(|c| c.id == chat_id).map_or(0, |c| c.unread);
        if self.live.is_some() {
            self.pending_unread.entry(chat_id).or_insert(current);
        }
        self.apply_chat_read_changed(chat_id, false, current);
        if self.live.is_some() {
            if let Some(jid) = self.live_jid(chat_id) {
                self.send_backend(BackendCommand::MarkChatUnread { chat_jid: jid });
            }
        }
    }

    /// Clears a chat's messages (design standard section 24),
    /// after the confirmation modal. In live mode the command
    /// goes first and NOTHING is deleted locally yet: the local
    /// apply is the confirming ChatCleared report's, through
    /// the same adoption an inbound clear takes, and a
    /// ClearChatFailed leaves every message in place. A clear
    /// whose local half ran before its outcome existed could
    /// destroy the archive copy of messages the server still
    /// holds; that trade is never made. `keep_starred` is the
    /// modal's checkbox choice, honored on both sides.
    pub(crate) fn clear_chat(&mut self, chat_id: u64, keep_starred: bool) {
        if let Some(jid) = self.live_jid(chat_id) {
            self.pending_clears.insert(chat_id, keep_starred);
            self.notice = Some(Notice::busy(crate::i18n::t("notice.clearing_chat")));
            self.send_backend(BackendCommand::ClearChat { chat_jid: jid, keep_starred });
        } else if !self.live_mode {
            // Demo mode: the chat's messages live in the model
            // itself; the same keep-starred rule applies.
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                chat.messages.retain(|m| keep_starred && m.starred);
                chat.preview = String::new();
            }
            if self.selected_chat == chat_id {
                self.conversation.retain(|m| keep_starred && m.starred);
            }
        }
    }

    /// Deletes a chat (design standard section 24), after the
    /// confirmation modal. Live mode follows the same rule as
    /// clear: the command goes first, the local removal is the
    /// confirming ChatDeleted report's, and a DeleteChatFailed
    /// leaves the chat fully in place.
    pub(crate) fn delete_chat(&mut self, chat_id: u64) {
        if let Some(jid) = self.live_jid(chat_id) {
            self.pending_chat_deletes.insert(chat_id);
            self.notice = Some(Notice::busy(crate::i18n::t("notice.deleting_chat")));
            self.send_backend(BackendCommand::DeleteChat { chat_jid: jid });
        } else if self.live_mode {
            // A live session whose address book does not know
            // this chat: nothing to delete against, and no
            // silent pretend-delete.
        } else {
            // Demo mode: the chat exists only locally.
            self.chats.retain(|c| c.id != chat_id);
            if self.selected_chat == chat_id {
                self.selected_chat = 0;
            }
        }
    }

    /// Applies one of the mute duration choices (design
    /// standard section 24): 8 hours, 1 week, or always. The
    /// mirror updates at once with the end the wire will carry;
    /// the per-second tick lifts a timed mute when its end
    /// passes, and the archive row persists both fields.
    pub(crate) fn apply_mute_for(&mut self, chat_id: u64, choice: MuteChoice) {
        let now = unix_now_secs();
        let (until, action) = match choice {
            MuteChoice::Hours8 => {
                let until = now + 8 * 3_600;
                (
                    until,
                    ChatAction::MuteUntil(i64::try_from(until).unwrap_or(0).saturating_mul(1000)),
                )
            }
            MuteChoice::Week => {
                let until = now + 7 * 86_400;
                (
                    until,
                    ChatAction::MuteUntil(i64::try_from(until).unwrap_or(0).saturating_mul(1000)),
                )
            }
            MuteChoice::Always => (0, ChatAction::Mute),
        };
        // The mute snapshot a ChatActionFailed restores,
        // taken before the mirror moves (see apply_chat_action).
        if self.live.is_some() {
            if let Some(c) = self.chats.iter().find(|c| c.id == chat_id) {
                let snap =
                    (c.archived, c.pinned, c.muted, i64::try_from(c.mute_until).unwrap_or(0));
                self.pending_chat_actions.entry((chat_id, "mute")).or_insert(snap);
            }
        }
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            c.muted = true;
            c.mute_until = until;
        }
        if let Some(c) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
            c.muted = true;
            c.mute_until = until;
        }
        if self.live.is_some() {
            if let Some(jid) = self.live_jid(chat_id) {
                self.send_backend(BackendCommand::ChatAction { chat_jid: jid, action });
            }
        }
        self.persist_chat_row(chat_id);
    }

    /// Sends a contact card into a chat (design standard
    /// section 24). Live mode goes through the backend's
    /// generic send; the sent card arrives back as a normal
    /// message upsert and renders through the received-card
    /// path. Demo mode appends the card locally, the way the
    /// demo's other sends land.
    pub(crate) fn send_contact_card(&mut self, chat_id: u64, name: &str, phone: &str) {
        if self.live.is_some() {
            if let Some(jid) = self.live_jid(chat_id) {
                let ephemeral_secs =
                    self.chats.iter().find(|c| c.id == chat_id).map_or(0, |c| c.ephemeral_secs);
                self.send_backend(BackendCommand::SendContact {
                    chat_jid: jid,
                    name: name.to_owned(),
                    phone: phone.to_owned(),
                    ephemeral_secs,
                });
            }
        } else if !self.live_mode {
            // Demo mode: append the card locally, the shape the
            // demo's other sends take (the location send in
            // attach.rs is the model).
            self.sent_counter += 1;
            let (stamp, expires_at) = self.demo_send_stamp();
            let msg = Message {
                kind: MessageKind::Contact { name: name.to_owned(), phone: phone.to_owned() },
                expires_at,
                ..Message::mine(900 + self.sent_counter, name.to_owned(), stamp)
            };
            if self.selected_chat == chat_id {
                self.conversation.push(msg.clone());
            }
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                chat.messages.push(msg);
                chat.sync_preview_from_last();
            }
        }
    }

    /// The live JID for a chat id, when known.
    pub(crate) fn live_jid(&self, chat_id: u64) -> Option<String> {
        self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned())
    }

    /// Asks the phone for the page of history older than the
    /// oldest message on hand for one chat (design standard
    /// section 22). The anchor is the oldest archived message:
    /// its protocol identity, direction and timestamp are what
    /// the phone pages from. Firing marks the chat Fetching
    /// (the stream's loading row keys off it) and disarms the
    /// scroll trigger; the outcome events move the state from
    /// there. A chat that cannot be anchored (nothing archived
    /// yet) is left idle and disarmed, never marked fetching
    /// for a request that was not sent.
    pub(crate) fn request_older_page(&mut self, chat_id: u64) {
        if let Some(session) = &mut self.live {
            session.history_armed.remove(&chat_id);
        }
        let anchor = self.live.as_ref().and_then(|session| {
            let jid = session.jids.get(&chat_id)?.clone();
            let store = session.store.as_ref()?;
            let rows = store.messages(&jid).ok()?;
            rows.into_iter()
                .min_by_key(|(m, _)| m.timestamp)
                .map(|(m, _)| (jid, m.id, m.from_me, m.timestamp))
        });
        let Some((chat_jid, oldest_message_id, oldest_from_me, oldest_ts)) = anchor else {
            return;
        };
        if let Some(session) = &mut self.live {
            session.history.insert(chat_id, HistoryFetch::Fetching);
        }
        self.send_backend(BackendCommand::FetchOlderMessages {
            chat_jid,
            oldest_message_id,
            oldest_from_me,
            oldest_timestamp_ms: oldest_ts.saturating_mul(1000),
        });
    }

    /// Starts the new-chat dialog's lookup for the number as
    /// typed (design standard section 23). The dialog tracks
    /// the in-flight input so a late outcome for a replaced
    /// input is ignored by the event handlers.
    pub(crate) fn start_new_chat_lookup(&mut self) {
        let phone = self.new_chat.phone.trim().to_owned();
        if phone.is_empty() || self.new_chat.checking.is_some() {
            return;
        }
        self.new_chat.not_found = None;
        self.new_chat.error = None;
        self.new_chat.checking = Some(phone.clone());
        self.send_backend(BackendCommand::LookupContact { phone });
    }

    /// A number lookup resolved to a person (design standard
    /// section 23): the conversation opens. One person, one
    /// chat: when the roster already holds a chat under either
    /// of the person's address forms, that chat is selected
    /// as-is (its known title stands; the lookup never renames
    /// a chat the session already knows). Otherwise the
    /// resolved chat enters through the normal upsert path and
    /// is selected, and its picture is asked for through the
    /// avatar pipeline.
    pub(crate) fn apply_contact_resolved(
        &mut self,
        chat: backend::LiveChat,
        alt_jid: Option<String>,
    ) {
        self.new_chat.checking = None;
        self.new_chat.not_found = None;
        self.new_chat.error = None;
        let known = |jid: &str| {
            let id = jid_id(jid);
            self.live.as_ref().is_some_and(|s| s.jids.contains_key(&id)).then_some(id)
        };
        let existing = known(&chat.jid).or_else(|| alt_jid.as_deref().and_then(known));
        let target = match existing {
            Some(id) => id,
            None => {
                let id = jid_id(&chat.jid);
                self.apply_chat_upsert(&chat);
                id
            }
        };
        self.new_chat.open = false;
        self.new_chat.phone.clear();
        self.select_chat_now(target);
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
        mute_until: i64::try_from(chat.mute_until).unwrap_or(0),
    }
}

/// Which fetch rail one ZIP export attachment rides
/// (section 25): the same fetch the message's own display or
/// download uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZipKind {
    /// FetchPhoto, answered with photo bytes.
    Photo,
    /// FetchVideo, answered with a cache file path.
    Video,
    /// FetchVoice, answered with voice bytes.
    Voice,
    /// DownloadMedia, answered with a saved file path.
    Document,
    /// FetchSticker, answered with sticker bytes.
    Sticker,
}

impl ZipKind {
    /// The kind word the manifest and the media file names use.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Photo => "photo",
            Self::Video => "video",
            Self::Voice => "voice",
            Self::Document => "document",
            Self::Sticker => "sticker",
        }
    }
}

/// One attachment's gathered payload: bytes carried in memory,
/// or a file already on disk (a video cache file, a saved
/// download) whose bytes the ZIP writer streams in.
pub(crate) enum ZipPayload {
    /// Bytes in memory.
    Bytes(Vec<u8>),
    /// A file on disk.
    File(std::path::PathBuf),
}

/// Where one ZIP attachment stands in the collection.
pub(crate) enum ZipItemState {
    /// Not fetched yet.
    Pending,
    /// Gathered, ready for the writer.
    Included(ZipPayload),
    /// Not included; the manifest carries the reason.
    Missing(String),
}

/// One attachment of a ZIP export: a media message of the
/// exported chat, in conversation order.
pub(crate) struct ZipItem {
    /// The message's app id.
    pub msg_id: u64,
    /// Its fetch rail.
    pub kind: ZipKind,
    /// Its name inside the ZIP's media folder.
    pub file_name: String,
    /// Its manifest line context: "22 November 2026 · 14:03".
    pub stamp: String,
    /// The sender's display name ("You" for own messages).
    pub sender: String,
    /// The caption or file name, when the message carries one.
    pub caption: String,
    /// The collection state.
    pub state: ZipItemState,
}

/// A ZIP export's attachment collection in flight
/// (section 25): the plan is captured whole at start (chat,
/// transcript, items), then fetches are issued one at a time
/// and the answering events settle their item, so an export
/// belongs to the chat it started from even if the user moves
/// on while it gathers.
pub(crate) struct ZipGather {
    /// The exported chat's app id.
    pub chat_id: u64,
    /// The exported chat's title (the manifest's heading and
    /// the completion notice's subject).
    pub chat_title: String,
    /// The export file's base name, captured at start (the
    /// gather belongs to its chat even if the user moves on).
    pub file_base: String,
    /// The rendered Markdown transcript (the ZIP's chat.md).
    pub transcript: String,
    /// How many messages the transcript covers.
    pub message_count: usize,
    /// The attachments, in conversation order.
    pub items: Vec<ZipItem>,
    /// The message whose fetch is currently in flight.
    pub waiting: Option<u64>,
    /// egui time the current wait began (the watchdog's base).
    pub wait_started: f64,
}

impl crate::state::App {
    /// Settles the ZIP gather's waiting item with gathered
    /// bytes, when the event that landed is the answer the
    /// gather is waiting for. Called from the media byte
    /// events before their display handling, so bytes for a
    /// note the user is not playing (which the playback path
    /// would drop) are still the export's.
    pub(crate) fn zip_collect_bytes(&mut self, msg_id: u64, kind: ZipKind, bytes: Vec<u8>) {
        self.zip_settle(msg_id, Some(kind), ZipItemState::Included(ZipPayload::Bytes(bytes)));
    }

    /// Settles the waiting item with a file on disk (a video
    /// cache file or a saved download).
    pub(crate) fn zip_collect_file(
        &mut self,
        msg_id: u64,
        kind: ZipKind,
        path: std::path::PathBuf,
    ) {
        self.zip_settle(msg_id, Some(kind), ZipItemState::Included(ZipPayload::File(path)));
    }

    /// Settles the waiting item as not included, with the
    /// reason the manifest will carry. `kind` `None` settles
    /// whatever kind is waiting (the generic Error event
    /// carries no kind; only a document wait may be settled
    /// by it, which the caller enforces).
    pub(crate) fn zip_collect_missing(
        &mut self,
        msg_id: u64,
        kind: Option<ZipKind>,
        reason: String,
    ) {
        self.zip_settle(msg_id, kind, ZipItemState::Missing(reason));
    }

    /// The one settle path: an event settles the gather only
    /// when it answers the item actually being waited on, of
    /// the kind expected. Late or foreign answers change
    /// nothing.
    fn zip_settle(&mut self, msg_id: u64, kind: Option<ZipKind>, state: ZipItemState) {
        let Some(gather) = &mut self.zip_gather else { return };
        if gather.waiting != Some(msg_id) {
            return;
        }
        let Some(item) = gather.items.iter_mut().find(|i| i.msg_id == msg_id) else {
            return;
        };
        if let Some(expected) = kind {
            if item.kind != expected {
                return;
            }
        }
        item.state = state;
        gather.waiting = None;
    }

    /// Drives the collection (section 25): issues the next
    /// pending item's fetch, settles items whose fetch cannot
    /// be issued, applies the wait watchdog, and finishes the
    /// export when every item has settled. Called every frame
    /// from the worker poll, so event handlers only settle and
    /// this one place advances.
    pub(crate) fn zip_gather_advance(&mut self, now: f64) {
        // The watchdog: a fetch that never answers must not
        // stall the export forever. Twenty seconds is far
        // beyond any of these rails' honest latency.
        if let Some(gather) = &mut self.zip_gather {
            if gather.waiting.is_some() && now - gather.wait_started > 20.0 {
                let stuck = gather.waiting.take().unwrap_or(0);
                if let Some(item) = gather.items.iter_mut().find(|i| i.msg_id == stuck) {
                    item.state = ZipItemState::Missing("the phone did not answer".to_owned());
                }
            }
        }
        loop {
            let Some(gather) = &self.zip_gather else { return };
            if gather.waiting.is_some() {
                return;
            }
            let Some(item) = gather.items.iter().find(|i| matches!(i.state, ZipItemState::Pending))
            else {
                // Every item has settled: the collection is
                // complete and the writer takes over.
                let gather = self.zip_gather.take().unwrap();
                self.finish_zip_export(gather);
                return;
            };
            let (chat_id, msg_id, kind) = (gather.chat_id, item.msg_id, item.kind);
            if self.zip_issue_fetch(chat_id, msg_id, kind) {
                if let Some(gather) = &mut self.zip_gather {
                    gather.waiting = Some(msg_id);
                    gather.wait_started = now;
                }
                return;
            }
            // The fetch cannot even be addressed (no session,
            // no protocol id): the item settles as missing and
            // the loop moves to the next one.
            if let Some(gather) = &mut self.zip_gather {
                if let Some(item) = gather.items.iter_mut().find(|i| i.msg_id == msg_id) {
                    item.state = ZipItemState::Missing("not available in this session".to_owned());
                }
            }
        }
    }

    /// Issues one item's fetch on its rail, mirroring the
    /// display paths' commands exactly (archived references
    /// included, so media from before this session is as
    /// fetchable for the export as it is for the screen).
    /// Returns whether the command was sent.
    fn zip_issue_fetch(&mut self, chat_id: u64, msg_id: u64, kind: ZipKind) -> bool {
        let Some(protocol_id) = self.protocol_ids.get(&msg_id).cloned() else {
            return false;
        };
        let Some(session) = &self.live else { return false };
        let Some(chat_jid) = session.jids.get(&chat_id).cloned() else {
            return false;
        };
        let command = match kind {
            ZipKind::Photo => BackendCommand::FetchPhoto {
                chat_jid,
                message_id: protocol_id,
                photo_ref: session.photo_refs.get(&msg_id).cloned(),
            },
            ZipKind::Voice => BackendCommand::FetchVoice {
                chat_jid,
                message_id: protocol_id,
                voice_ref: session.voice_refs.get(&msg_id).cloned(),
            },
            ZipKind::Video => BackendCommand::FetchVideo {
                chat_jid,
                message_id: protocol_id,
                video_ref: session.video_refs.get(&msg_id).cloned(),
                view_once: false,
            },
            ZipKind::Sticker => BackendCommand::FetchSticker {
                chat_jid,
                message_id: protocol_id,
                sticker_ref: session.sticker_refs.get(&msg_id).cloned(),
            },
            ZipKind::Document => {
                BackendCommand::DownloadMedia { chat_jid, message_id: protocol_id }
            }
        };
        self.send_backend(command);
        true
    }

    /// Hands a completed collection to the ZIP writer worker
    /// (the same thread, channel and Busy discipline as the
    /// Markdown export) and arms its result channel.
    pub(crate) fn finish_zip_export(&mut self, gather: ZipGather) {
        let dir = backend::data_dir().join("exports");
        let base = gather.file_base.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned =
            std::thread::Builder::new().name("zaprust-export-zip".to_owned()).spawn(move || {
                let result = crate::conversation::write_zip_export(&dir, &base, &gather);
                let _ = tx.send(result);
            });
        match spawned {
            Ok(_) => {
                self.zip_export_rx = Some(rx);
            }
            Err(e) => {
                self.notice = Some(crate::state::Notice::error(format!("Export failed: {e}")));
            }
        }
    }
}

#[cfg(test)]
mod link_status_label_tests {
    use super::LinkStatus;

    #[test]
    fn status_labels_never_narrate_the_machinery() {
        // The connect screen's status line is user guidance, not
        // a log: no label may name the connection worker or any
        // other implementation part.
        let statuses = [
            LinkStatus::Starting,
            LinkStatus::WaitingQr,
            LinkStatus::CodeReady,
            LinkStatus::Linked,
            LinkStatus::LoggingOut,
            LinkStatus::LoggedOut,
            LinkStatus::Banned(401),
            LinkStatus::Lost("Connection lost.".to_owned()),
            LinkStatus::Crashed("summary".to_owned()),
            LinkStatus::Failed("message".to_owned()),
        ];
        for status in statuses {
            let label = status.label();
            assert!(!label.to_lowercase().contains("worker"), "label: {label}");
        }
        assert_eq!(LinkStatus::Starting.label(), "Connecting…");
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
            sticker_refs: HashMap::new(),
            photo_thumbs: HashMap::new(),
            message_mentions: HashMap::new(),
            names: HashMap::new(),
            avatar_bytes: HashMap::new(),
            avatar_missing: HashMap::new(),
            avatar_requested: std::collections::HashSet::new(),
            history: HashMap::new(),
            history_armed: std::collections::HashSet::new(),
            poll_ballots: HashMap::new(),
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

    fn call_changed(
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

    fn archived_calls(app: &App) -> Vec<backend::CallRecord> {
        app.live.as_ref().unwrap().store.as_ref().unwrap().call_history().unwrap()
    }

    /// v1.6.7: a call record synced from the phone's history
    /// lands in the archive and in the Calls panel data, filed
    /// under its chat, without any live call taking place.
    #[test]
    fn synced_call_history_lands_in_the_archive_and_panel() {
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
    fn favorites_sync_adopts_the_complete_phone_set() {
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
    fn avatar_events_cache_invalidate_and_negative() {
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

    /// v1.6.7: the defect behind the owner's largest group
    /// showing its raw JID: a live upsert titled with the raw
    /// identifier must not re-stamp a healed group title, in the
    /// mirror or the archive, while a resolved rename still
    /// lands.
    #[test]
    fn healed_group_title_survives_a_raw_live_upsert() {
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
                pinned: false,
                ephemeral_secs: 0,
                muted: false,
                mute_until: 0,
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

    /// v1.6.7: an undecryptable message the wire says to show
    /// becomes a placeholder in the conversation, exactly once:
    /// a copy the archive already holds is never duplicated.
    #[test]
    fn undecryptable_placeholder_is_inserted_once() {
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
    fn connected_outgoing_call_is_recorded_with_its_duration() {
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
    fn unanswered_outgoing_call_is_recorded_not_missed() {
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
    fn unanswered_incoming_call_is_recorded_as_missed() {
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
    fn declined_incoming_call_is_recorded_but_not_missed() {
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
    fn missed_call_event_records_an_entry_stamped_when_it_rang() {
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
    fn missed_call_event_names_the_caller_in_a_notice() {
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
    fn media_saved_clears_the_download_in_flight() {
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
    fn incoming_call_answered_elsewhere_ends_the_overlay_and_is_not_missed() {
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
    fn incoming_call_declined_elsewhere_is_recorded_not_missed() {
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
    fn unshown_call_ended_elsewhere_is_recorded_not_missed() {
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
    fn elsewhere_report_for_another_call_leaves_the_ringing_one_alone() {
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
    fn call_history_is_reloaded_from_the_archive_by_a_new_session() {
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

    /// A voice-note upsert carrying its re-download reference,
    /// the shape the backend delivers for every voice note whose
    /// wire payload was complete.
    fn voice_message(id: &str, ts: i64) -> LiveMessage {
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
    fn voice_bytes() -> Vec<u8> {
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

    /// Item 110: pressing play on a live voice note fetches its
    /// bytes with the archived reference riding along, and a
    /// second press while the fetch runs sends nothing new.
    #[test]
    fn voice_play_fetches_with_the_archived_reference() {
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
    fn voice_bytes_start_the_playback_engine() {
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
    fn voice_failures_mark_the_note_failed() {
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
    fn demo_voice_press_states_unavailable() {
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

    /// A small real JPEG, encoded in memory, standing in for a
    /// video's wire thumbnail bytes.
    fn jpeg_bytes() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(4, 3, image::Rgba([40, 120, 200, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut buf, image::ImageFormat::Jpeg).unwrap();
        buf.into_inner()
    }

    /// A video upsert carrying its re-download reference and,
    /// optionally, its embedded thumbnail: the shape the backend
    /// now delivers for every video whose wire payload was
    /// complete.
    fn video_message(id: &str, ts: i64, view_once: bool, with_thumb: bool) -> LiveMessage {
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
    fn video_tile_is_the_wire_thumbnail_or_neutral() {
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
    fn video_fetch_sends_the_command_with_the_archived_reference() {
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
    fn video_fetch_failure_marks_the_route_failed() {
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

    /// Item 111: view-once video bytes land in memory (never a
    /// file), the player starts from them and presents a real
    /// decoded frame of the actual video.
    #[test]
    fn view_once_video_bytes_start_a_real_player() {
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
            bytes: include_bytes!("testdata/tiny.mp4").to_vec(),
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
    fn view_once_video_close_without_presentation_keeps_the_view() {
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
    fn view_once_video_close_after_presentation_spends_the_view() {
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
    fn video_route_without_a_session_is_demo_unavailable() {
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
    fn backend_error_text_is_scrubbed_before_display() {
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
    fn plain_backend_error_text_is_displayed_unchanged() {
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

    /// The message-info popover renders exactly what the
    /// receipt log holds for a message (draw_message_info
    /// reads `receipt_log` for its rows), so the log's
    /// population from Receipt events is the feature: kinds
    /// in arrival order, each stamped with its own event
    /// time, kept per message.
    #[test]
    fn receipt_events_populate_the_message_info_log() {
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
        })
        .unwrap();
        tx.send(BackendEvent::Receipt {
            chat_jid: CHAT_JID.to_owned(),
            message_ids: vec!["mine-1".to_owned()],
            kind: ReceiptKind::Delivered,
            at: BASE_TS + 1_200,
        })
        .unwrap();
        // One receipt event can cover several messages at once.
        tx.send(BackendEvent::Receipt {
            chat_jid: CHAT_JID.to_owned(),
            message_ids: vec!["mine-1".to_owned(), "mine-2".to_owned()],
            kind: ReceiptKind::Read,
            at: BASE_TS + 1_800,
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
    fn receipt_log_keeps_only_the_newest_entries() {
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

    /// Incoming typing: a TypingReceived event marks the chat
    /// (the conversation header and the chat list read the
    /// mark's freshness, under six seconds), and the matching
    /// stop event clears it, per chat.
    #[test]
    fn typing_received_marks_and_clears_per_chat() {
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
    fn history_fetch_state_moves_with_the_outcome_events() {
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
    fn contact_resolved_opens_one_chat_per_person() {
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
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
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
    fn lookup_outcomes_move_only_the_dialog_that_asked() {
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
    fn group_picture_events_invalidate_and_report() {
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

    fn dm_upsert(jid: &str) -> BackendEvent {
        BackendEvent::ChatUpsert(LiveChat {
            jid: jid.to_owned(),
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
            mute_until: 0,
        })
    }

    fn chat_of(app: &App, id: u64) -> Chat {
        app.chats.iter().find(|c| c.id == id).expect("the chat exists").clone()
    }

    /// Item 111: the live conversion must not invent a header
    /// line. Every live kind converts with an empty subtitle;
    /// the header derives its line from live state instead.
    #[test]
    fn live_conversion_carries_no_subtitle() {
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
                pinned: false,
                ephemeral_secs: 0,
                muted: false,
                mute_until: 0,
            });
            assert!(chat.subtitle.is_empty(), "{kind:?} carries no invented subtitle");
        }
    }

    /// Item 111: opening a DM subscribes to the contact's
    /// presence, and the backend's reports drive the header
    /// line: online, then a last seen carrying the real
    /// instant, then nothing when last seen is hidden.
    #[test]
    fn presence_watch_subscribes_and_reports_drive_the_header_line() {
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
    fn presence_watch_follows_the_open_chat_and_drops_stale_state() {
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
    fn connection_loss_drops_reported_presence() {
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
    fn group_header_line_counts_the_fetched_members() {
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
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
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
    fn last_seen_label_names_today_yesterday_and_older_days() {
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
            mute_until: 0,
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
            sticker_refs: HashMap::new(),
            photo_thumbs: HashMap::new(),
            message_mentions: HashMap::new(),
            names: HashMap::new(),
            avatar_bytes: HashMap::new(),
            avatar_missing: HashMap::new(),
            avatar_requested: std::collections::HashSet::new(),
            history: HashMap::new(),
            history_armed: std::collections::HashSet::new(),
            poll_ballots: HashMap::new(),
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

    /// Section 25: the ZIP gather issues one fetch at a time
    /// on the item's own rail, settles the item when the
    /// answering event lands, and hands the completed
    /// collection to the writer, whose ZIP holds the
    /// transcript, the gathered media, and a manifest that
    /// accounts for the item that failed.
    #[test]
    fn zip_gather_fetches_settles_and_writes() {
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
        let mut archive = zip::ZipArchive::new(file).unwrap();
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
    fn zip_gather_watchdog_settles_a_silent_fetch() {
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
            mute_until: 0,
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
            mute_until: 0,
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

    fn chat_favorite(app: &App) -> bool {
        app.chats.iter().find(|c| c.id == jid_id(CHAT_JID)).expect("chat exists").favorite
    }

    fn archived_favorites(app: &App) -> Vec<String> {
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        store.chat_favorites().unwrap()
    }

    /// An arrival-style upsert for a second chat, so the
    /// favorites filter test can tell flagged and unflagged
    /// chats apart.
    fn arrival_upsert_for(jid: &str) -> BackendEvent {
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
            mute_until: 0,
        })
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
            mute_until: 0,
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

    /// Item 116: the row menu's favorite toggle (apply_favorite,
    /// the handler the menu pick lands in) flips the mirror at
    /// once and writes the flag through to the archive, and it
    /// never sends a backend command: a favorite is a local
    /// concept with no server counterpart.
    #[test]
    fn favorite_toggle_persists_locally_without_a_backend_command() {
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
    fn arrival_upsert_keeps_the_known_favorite() {
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
    fn favorites_filter_lists_only_flagged_live_chats() {
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
    fn favorite_survives_into_a_new_session_from_the_same_archive() {
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
    fn favorite_toggle_in_demo_mode_stays_in_memory() {
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
    fn server_mute_changed_overrides_local_state() {
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
    fn arrival_upsert_keeps_the_known_mute() {
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

    fn own_profile(name: &str, about: &str) -> BackendEvent {
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
    fn own_profile_read_back_populates_the_editor() {
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
    fn own_profile_read_back_does_not_clobber_a_dirty_editor() {
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
    fn open_my_profile_requests_a_fresh_read_back() {
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
    fn own_profile_save_sends_changed_fields_and_resolves_on_the_read_back() {
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
    fn own_profile_save_failure_reverts_and_reports() {
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
    fn own_profile_save_is_refused_until_linked() {
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

    /// The removed Custom title bar toggle leaves no trace in
    /// the settings document: its field was never persisted
    /// (nothing read it back either), and the field itself is
    /// gone, so no key may appear here.
    #[test]
    fn settings_json_has_no_custom_title_bar_key() {
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
            mute_until: 0,
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
    /// adoption itself sends nothing back (the presence watch
    /// following the moved selection aside).
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
    fn server_chat_cleared_empties_messages_but_keeps_the_chat() {
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
            sticker_refs: HashMap::new(),
            photo_thumbs: HashMap::new(),
            message_mentions: HashMap::new(),
            names: HashMap::new(),
            avatar_bytes: HashMap::new(),
            avatar_missing: HashMap::new(),
            avatar_requested: std::collections::HashSet::new(),
            history: HashMap::new(),
            history_armed: std::collections::HashSet::new(),
            poll_ballots: HashMap::new(),
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
            mute_until: 0,
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

    /// A poll creation as the backend delivers it in this
    /// harness: three options, zero counts, and the creation
    /// secret plus creator the translation layer extracted.
    fn poll_upsert(id: &str) -> LiveMessage {
        let mut msg = LiveMessage::text(
            id,
            CHAT_JID,
            Some("Alice".to_owned()),
            "Lunch?",
            1_700_000_000,
            false,
        );
        msg.kind = backend::LiveMessageKind::Poll {
            question: "Lunch?".to_owned(),
            options: vec![
                ("Deli".to_owned(), 0),
                ("Sushi".to_owned(), 0),
                ("Delivery".to_owned(), 0),
            ],
            multiple: true,
            my_votes: Vec::new(),
            correct: None,
        };
        msg.poll_secret = Some(vec![7u8; 32]);
        msg.poll_creator = Some(CHAT_JID.to_owned());
        msg
    }

    fn poll_vote(id: &str, voter: &str, from_me: bool, selected: &[usize]) -> BackendEvent {
        BackendEvent::PollVote {
            chat_jid: CHAT_JID.to_owned(),
            message_id: id.to_owned(),
            voter_jid: voter.to_owned(),
            from_me,
            selected: selected.to_vec(),
        }
    }

    /// (counts, my_votes) of the poll in the mirror.
    fn mirror_poll(app: &App) -> (Vec<u32>, Vec<usize>) {
        let msg = app
            .live
            .as_ref()
            .unwrap()
            .messages
            .get(&jid_id(CHAT_JID))
            .expect("the chat has messages")
            .iter()
            .map(|(_, m)| m)
            .find(|m| matches!(m.kind, MessageKind::Poll { .. }))
            .expect("the poll is in the mirror");
        match &msg.kind {
            MessageKind::Poll { options, my_votes, .. } => {
                (options.iter().map(|(_, v)| *v).collect(), my_votes.clone())
            }
            _ => unreachable!(),
        }
    }

    /// (counts, my_votes, ballots) of one poll, read from the archive.
    type ArchivedPoll = (Vec<u32>, Vec<usize>, Vec<(String, Vec<usize>)>);

    fn archived_poll(app: &App) -> ArchivedPoll {
        let rows = app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let (msg, _) = rows
            .iter()
            .find(|(m, _)| matches!(m.kind, backend::LiveMessageKind::Poll { .. }))
            .expect("the poll is in the archive");
        match &msg.kind {
            backend::LiveMessageKind::Poll { options, my_votes, .. } => (
                options.iter().map(|(_, v)| *v).collect(),
                my_votes.clone(),
                msg.poll_ballots.clone(),
            ),
            _ => unreachable!(),
        }
    }

    /// Item 123: incoming votes aggregate onto the poll, in the
    /// mirror and in the archive. A voter changing their
    /// selection replaces their ballot (counts move, they do not
    /// stack), and an empty selection retracts it.
    #[test]
    fn poll_votes_aggregate_with_replacement_and_clear() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(poll_upsert("poll-1"))).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_poll(&app).0, vec![0, 0, 0]);

        tx.send(poll_vote("poll-1", "10000000007@s.whatsapp.net", false, &[0])).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_poll(&app).0, vec![1, 0, 0]);

        tx.send(poll_vote("poll-1", "10000000007@s.whatsapp.net", false, &[0, 2])).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_poll(&app).0, vec![1, 0, 1], "a changed vote replaces, not stacks");

        tx.send(poll_vote("poll-1", "10000000008@s.whatsapp.net", false, &[0])).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_poll(&app).0, vec![2, 0, 1]);

        tx.send(poll_vote("poll-1", "10000000007@s.whatsapp.net", false, &[])).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_poll(&app).0, vec![1, 0, 0], "an empty selection retracts the ballot");

        let (counts, _mine, ballots) = archived_poll(&app);
        assert_eq!(counts, vec![1, 0, 0], "the archive carries the same tally");
        assert_eq!(
            ballots,
            vec![("10000000008@s.whatsapp.net".to_owned(), vec![0])],
            "and the one surviving ballot it was derived from"
        );
    }

    /// Item 123: my vote is applied optimistically when I cast it
    /// (what `vote_poll` does after sending the command); the
    /// server's echo of the same ballot must not count it twice.
    #[test]
    fn my_poll_vote_applies_once_when_the_echo_repeats_it() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(poll_upsert("poll-1"))).unwrap();
        drain_all(&mut app, &ctx);

        app.apply_poll_vote(CHAT_JID, "poll-1", "", true, &[1]);
        assert_eq!(mirror_poll(&app), (vec![0, 1, 0], vec![1]));

        tx.send(poll_vote("poll-1", "10000000000@s.whatsapp.net", true, &[1])).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_poll(&app), (vec![0, 1, 0], vec![1]), "the echo changes nothing");
        let (counts, mine, _) = archived_poll(&app);
        assert_eq!((counts, mine), (vec![0, 1, 0], vec![1]), "and the archive agrees");
    }

    /// Item 123: a vote for a poll whose chat is not loaded in
    /// the mirror still lands, straight on the archived copy.
    #[test]
    fn poll_vote_for_an_unloaded_chat_updates_the_archive() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(poll_upsert("poll-1"))).unwrap();
        drain_all(&mut app, &ctx);
        app.live.as_mut().unwrap().messages.remove(&jid_id(CHAT_JID));

        tx.send(poll_vote("poll-1", "10000000007@s.whatsapp.net", false, &[2])).unwrap();
        drain_all(&mut app, &ctx);
        let (counts, _mine, ballots) = archived_poll(&app);
        assert_eq!(counts, vec![0, 0, 1]);
        assert_eq!(ballots, vec![("10000000007@s.whatsapp.net".to_owned(), vec![2])]);
    }

    /// Item 123: the ballots seeded from the archive at session
    /// start (the load start_live performs) make the next vote
    /// from the same voter replace their pre-restart ballot
    /// instead of stacking on top of it.
    #[test]
    fn seeded_ballots_make_the_next_vote_replace() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(poll_upsert("poll-1"))).unwrap();
        tx.send(poll_vote("poll-1", "10000000007@s.whatsapp.net", false, &[0])).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(archived_poll(&app).0, vec![1, 0, 0]);

        // The restart load, performed the way start_live performs
        // it: mirror rebuilt from the archive rows, ballot map
        // seeded from the archived ballots.
        {
            let session = app.live.as_mut().unwrap();
            let rows = session.store.as_ref().unwrap().messages(CHAT_JID).unwrap();
            session.poll_ballots.clear();
            for (m, _) in &rows {
                if !m.poll_ballots.is_empty() {
                    session
                        .poll_ballots
                        .insert(message_id(&m.chat_jid, &m.id), m.poll_ballots.clone());
                }
            }
            session.messages.insert(
                jid_id(CHAT_JID),
                rows.iter()
                    .map(|(m, flags)| (m.timestamp, to_message_with_flags(m, *flags)))
                    .collect(),
            );
        }

        tx.send(poll_vote("poll-1", "10000000007@s.whatsapp.net", false, &[2])).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            mirror_poll(&app).0,
            vec![0, 0, 1],
            "the pre-restart ballot is replaced, not stacked"
        );
        assert_eq!(archived_poll(&app).0, vec![0, 0, 1]);
    }

    /// Item 123: an archived poll that kept its creation secret
    /// builds the registration command the next session sends to
    /// the backend; anything without a kept secret builds none.
    #[test]
    fn archived_poll_secret_builds_a_registration_command() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(poll_upsert("poll-1"))).unwrap();
        drain_all(&mut app, &ctx);

        let rows = app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let (msg, _) = rows.iter().find(|(m, _)| m.id == "poll-1").unwrap();
        let cmd = poll_secret_registration(msg).expect("the archived poll carries its secret");
        let BackendCommand::RegisterPollSecret {
            chat_jid,
            message_id,
            creator_jid,
            secret,
            options,
        } = cmd
        else {
            panic!("the registration is a RegisterPollSecret command");
        };
        assert_eq!(chat_jid, CHAT_JID);
        assert_eq!(message_id, "poll-1");
        assert_eq!(creator_jid, CHAT_JID);
        assert_eq!(secret, vec![7u8; 32]);
        assert_eq!(options, vec!["Deli".to_owned(), "Sushi".to_owned(), "Delivery".to_owned()]);

        let plain = LiveMessage::text("t-1", CHAT_JID, None, "hi", 1_700_000_000, false);
        assert!(poll_secret_registration(&plain).is_none(), "no secret, no registration");
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
            syncing: false,
            sync_last_activity: None,
            synced_once: false,
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
            sticker_refs: HashMap::new(),
            photo_thumbs: HashMap::new(),
            message_mentions: HashMap::new(),
            names: HashMap::new(),
            avatar_bytes: HashMap::new(),
            avatar_missing: HashMap::new(),
            avatar_requested: std::collections::HashSet::new(),
            history: HashMap::new(),
            history_armed: std::collections::HashSet::new(),
            poll_ballots: HashMap::new(),
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
    fn live_app() -> (
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
            sticker_refs: HashMap::new(),
            photo_thumbs: HashMap::new(),
            message_mentions: HashMap::new(),
            names: HashMap::new(),
            avatar_bytes: HashMap::new(),
            avatar_missing: HashMap::new(),
            avatar_requested: std::collections::HashSet::new(),
            history: HashMap::new(),
            history_armed: std::collections::HashSet::new(),
            poll_ballots: HashMap::new(),
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

    fn sent_commands(cmd_rx: &std::sync::mpsc::Receiver<BackendCommand>) -> Vec<BackendCommand> {
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
    fn asking_and_cancelling_send_no_disconnect() {
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
    fn confirmed_log_out_sends_disconnect_exactly_once() {
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
    fn log_out_refuses_sessions_that_are_not_linked() {
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
    fn fake_logout_event_tears_the_session_down() {
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
    fn deregistration_failure_is_shown_before_the_teardown() {
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
    fn phone_side_logout_tears_down_when_the_run_ends() {
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
    fn channel_close_without_logout_keeps_the_session() {
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
mod reaction_tests {
    use super::*;
    use crate::state::{App, Args};

    const GROUP_JID: &str = "120363001234567890@g.us";
    const DM_JID: &str = "6281234567890@s.whatsapp.net";
    const ALICE_JID: &str = "6281111111111@s.whatsapp.net";

    /// An app in live mode on a hand-fed backend channel, the
    /// same harness shape as the other live test modules.
    fn live_app() -> (
        App,
        std::sync::mpsc::Sender<BackendEvent>,
        egui::Context,
        std::sync::mpsc::Receiver<BackendCommand>,
    ) {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = App::new(Args::parse(), &ctx);
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
            store: Some(store::Store::open_in_memory().unwrap()),
            jids: HashMap::new(),
            messages: HashMap::new(),
            times: HashMap::new(),
            fake: true,
            logout_requested: false,
            flags: HashMap::new(),
            engaged_chats: std::collections::HashSet::new(),
            poll_ballots: HashMap::new(),
            photo_refs: HashMap::new(),
            voice_refs: HashMap::new(),
            video_refs: HashMap::new(),
            video_thumbs: HashMap::new(),
            sticker_refs: HashMap::new(),
            photo_thumbs: HashMap::new(),
            message_mentions: HashMap::new(),
            names: HashMap::new(),
            avatar_bytes: HashMap::new(),
            avatar_missing: HashMap::new(),
            avatar_requested: std::collections::HashSet::new(),
            history: HashMap::new(),
            history_armed: std::collections::HashSet::new(),
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

    fn sent_commands(cmd_rx: &std::sync::mpsc::Receiver<BackendCommand>) -> Vec<BackendCommand> {
        let mut out = Vec::new();
        while let Ok(cmd) = cmd_rx.try_recv() {
            out.push(cmd);
        }
        out
    }

    /// Feeds one message into the live mirror and opens its
    /// chat, returning the app message ID.
    fn open_message(
        app: &mut App,
        tx: &std::sync::mpsc::Sender<BackendEvent>,
        ctx: &egui::Context,
        msg: LiveMessage,
    ) -> u64 {
        let chat_jid = msg.chat_jid.clone();
        let proto = msg.id.clone();
        tx.send(BackendEvent::MessageUpsert(msg)).unwrap();
        drain_all(app, ctx);
        app.selected_chat = jid_id(&chat_jid);
        app.sync_conversation();
        message_id(&chat_jid, &proto)
    }

    fn alice_group_message() -> LiveMessage {
        let mut msg = LiveMessage::text(
            "g1",
            GROUP_JID,
            Some("Alice".to_owned()),
            "hello group",
            1_700_000_000,
            false,
        );
        msg.sender_jid = Some(ALICE_JID.to_owned());
        msg
    }

    fn mirror_reactions(app: &App, chat_id: u64, mid: u64) -> Vec<(String, u32)> {
        let mirror = app.live.as_ref().unwrap().messages.get(&chat_id).unwrap();
        mirror.iter().find(|(_, m)| m.id == mid).unwrap().1.reactions.clone()
    }

    fn react_command(
        cmd_rx: &std::sync::mpsc::Receiver<BackendCommand>,
    ) -> (String, String, Option<String>, bool, String) {
        let sent = sent_commands(cmd_rx);
        let found = sent.iter().find_map(|c| match c {
            BackendCommand::React { chat_jid, message_id, sender_jid, from_me, emoji } => Some((
                chat_jid.clone(),
                message_id.clone(),
                sender_jid.clone(),
                *from_me,
                emoji.clone(),
            )),
            _ => None,
        });
        found.expect("a reaction command is sent")
    }

    /// A group reaction names the participant who authored the
    /// target message: the JID recorded for the message when it
    /// arrived is what the command carries, not None.
    #[test]
    fn group_reaction_names_the_message_author() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.apply_reaction(mid, "👍");
        let (chat_jid, message_id, sender_jid, from_me, emoji) = react_command(&cmd_rx);
        assert_eq!(chat_jid, GROUP_JID);
        assert_eq!(message_id, "g1");
        assert_eq!(sender_jid.as_deref(), Some(ALICE_JID), "the author is named");
        assert!(!from_me);
        assert_eq!(emoji, "👍");
    }

    /// Reacting to my own group message needs no sender: the
    /// library resolves my group identity itself, and a sender
    /// in my name could even be the wrong one (PN vs LID
    /// addressing is the library's call).
    #[test]
    fn own_group_reaction_omits_the_sender() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let msg = LiveMessage::text("g2", GROUP_JID, None, "my own", 1_700_000_000, true);
        let mid = open_message(&mut app, &tx, &ctx, msg);
        app.apply_reaction(mid, "👍");
        let (_, _, sender_jid, from_me, _) = react_command(&cmd_rx);
        assert_eq!(sender_jid, None);
        assert!(from_me);
    }

    /// Direct-chat reactions are unchanged: no sender is
    /// attached even though the partner's JID is on record,
    /// because a DM key carries no participant.
    #[test]
    fn dm_reaction_carries_no_sender() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mut msg =
            LiveMessage::text("d1", DM_JID, Some("Partner".to_owned()), "hi", 1_700_000_000, false);
        msg.sender_jid = Some(DM_JID.to_owned());
        let mid = open_message(&mut app, &tx, &ctx, msg);
        app.apply_reaction(mid, "👍");
        let (chat_jid, _, sender_jid, from_me, _) = react_command(&cmd_rx);
        assert_eq!(chat_jid, DM_JID);
        assert_eq!(sender_jid, None);
        assert!(!from_me);
    }

    /// The optimistic write and the server's echo of my own
    /// reaction are the same write into the actor map: the echo
    /// confirms the pending change and the count does not move.
    #[test]
    fn my_reaction_echo_confirms_without_double_counting() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            actor: "Alice".to_owned(),
            emoji: "👍".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_reactions(&app, chat_id, mid), vec![("👍".to_owned(), 1)]);

        app.apply_reaction(mid, "👍");
        assert_eq!(mirror_reactions(&app, chat_id, mid), vec![("👍".to_owned(), 2)]);
        assert_eq!(app.my_reactions.get(&mid).map(String::as_str), Some("👍"));
        assert!(app.pending_reactions.contains_key(&mid), "the change awaits settlement");

        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            actor: "Me".to_owned(),
            emoji: "👍".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            mirror_reactions(&app, chat_id, mid),
            vec![("👍".to_owned(), 2)],
            "the echo does not count me twice"
        );
        assert!(!app.pending_reactions.contains_key(&mid), "the echo confirms the change");
        assert_eq!(app.my_reactions.get(&mid).map(String::as_str), Some("👍"));
    }

    /// My reaction disappearing server-side (removed from
    /// another device) converges the local state to the
    /// server's set: my picker state clears and my count goes.
    #[test]
    fn server_removal_of_my_reaction_converges() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.apply_reaction(mid, "❤️");
        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            actor: "Me".to_owned(),
            emoji: "❤️".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_reactions(&app, chat_id, mid), vec![("❤️".to_owned(), 1)]);

        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            actor: "Me".to_owned(),
            emoji: String::new(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(mirror_reactions(&app, chat_id, mid).is_empty(), "my count is gone");
        assert!(!app.my_reactions.contains_key(&mid), "my picker state clears");
        assert!(
            !app.reaction_actors.contains_key(&format!("{chat_id}:{mid}")),
            "the emptied actor entry is dropped"
        );
    }

    /// A reaction the backend reports as failed rolls the
    /// optimistic state back to what the server last confirmed,
    /// and the failure reaches the notice banner.
    #[test]
    fn failed_group_reaction_rolls_back() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            actor: "Alice".to_owned(),
            emoji: "👍".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);

        app.apply_reaction(mid, "❤️");
        let mut optimistic = mirror_reactions(&app, chat_id, mid);
        optimistic.sort();
        assert_eq!(optimistic, vec![("❤️".to_owned(), 1), ("👍".to_owned(), 1)]);
        assert_eq!(app.my_reactions.get(&mid).map(String::as_str), Some("❤️"));

        tx.send(BackendEvent::ReactionFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Reaction failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            mirror_reactions(&app, chat_id, mid),
            vec![("👍".to_owned(), 1)],
            "only Alice's confirmed reaction remains"
        );
        assert!(!app.my_reactions.contains_key(&mid), "my optimistic reaction is undone");
        assert!(!app.pending_reactions.contains_key(&mid), "nothing stays pending");
        let view = app.conversation.iter().find(|m| m.id == mid).unwrap();
        assert_eq!(view.reactions, vec![("👍".to_owned(), 1)], "the open view rolls back too");
        let notice = app.notice_str().unwrap_or_default().to_owned();
        assert!(notice.contains("Reaction failed"), "the failure is surfaced: {notice}");
    }

    /// Rolling back a *change* of reaction restores the emoji
    /// the server had confirmed before, not an empty state.
    #[test]
    fn failed_reaction_change_restores_the_previous_emoji() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.apply_reaction(mid, "👍");
        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            actor: "Me".to_owned(),
            emoji: "👍".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_reactions(&app, chat_id, mid), vec![("👍".to_owned(), 1)]);

        app.apply_reaction(mid, "❤️");
        assert_eq!(mirror_reactions(&app, chat_id, mid), vec![("❤️".to_owned(), 1)]);

        tx.send(BackendEvent::ReactionFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Reaction failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_reactions(&app, chat_id, mid), vec![("👍".to_owned(), 1)]);
        assert_eq!(app.my_reactions.get(&mid).map(String::as_str), Some("👍"));
    }
}

#[cfg(test)]
mod star_pin_tests {
    use super::*;
    use crate::state::{App, Args};

    const GROUP_JID: &str = "120363001234567890@g.us";
    const DM_JID: &str = "6281234567890@s.whatsapp.net";
    const ALICE_JID: &str = "6281111111111@s.whatsapp.net";

    /// An app in live mode on a hand-fed backend channel, the
    /// same harness shape as the other live test modules.
    fn live_app() -> (
        App,
        std::sync::mpsc::Sender<BackendEvent>,
        egui::Context,
        std::sync::mpsc::Receiver<BackendCommand>,
    ) {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = App::new(Args::parse(), &ctx);
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
            store: Some(store::Store::open_in_memory().unwrap()),
            jids: HashMap::new(),
            messages: HashMap::new(),
            times: HashMap::new(),
            fake: true,
            logout_requested: false,
            flags: HashMap::new(),
            engaged_chats: std::collections::HashSet::new(),
            poll_ballots: HashMap::new(),
            photo_refs: HashMap::new(),
            voice_refs: HashMap::new(),
            video_refs: HashMap::new(),
            video_thumbs: HashMap::new(),
            sticker_refs: HashMap::new(),
            photo_thumbs: HashMap::new(),
            message_mentions: HashMap::new(),
            names: HashMap::new(),
            avatar_bytes: HashMap::new(),
            avatar_missing: HashMap::new(),
            avatar_requested: std::collections::HashSet::new(),
            history: HashMap::new(),
            history_armed: std::collections::HashSet::new(),
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

    fn sent_commands(cmd_rx: &std::sync::mpsc::Receiver<BackendCommand>) -> Vec<BackendCommand> {
        let mut out = Vec::new();
        while let Ok(cmd) = cmd_rx.try_recv() {
            out.push(cmd);
        }
        out
    }

    /// Feeds one message into the live mirror and opens its
    /// chat, returning the app message ID.
    fn open_message(
        app: &mut App,
        tx: &std::sync::mpsc::Sender<BackendEvent>,
        ctx: &egui::Context,
        msg: LiveMessage,
    ) -> u64 {
        let chat_jid = msg.chat_jid.clone();
        let proto = msg.id.clone();
        tx.send(BackendEvent::MessageUpsert(msg)).unwrap();
        drain_all(app, ctx);
        app.selected_chat = jid_id(&chat_jid);
        app.sync_conversation();
        message_id(&chat_jid, &proto)
    }

    fn alice_group_message() -> LiveMessage {
        let mut msg = LiveMessage::text(
            "g1",
            GROUP_JID,
            Some("Alice".to_owned()),
            "hello group",
            1_700_000_000,
            false,
        );
        msg.sender_jid = Some(ALICE_JID.to_owned());
        msg
    }

    fn alice_dm_message() -> LiveMessage {
        let mut msg =
            LiveMessage::text("d1", DM_JID, Some("Partner".to_owned()), "hi", 1_700_000_000, false);
        msg.sender_jid = Some(DM_JID.to_owned());
        msg
    }

    fn archived_has(app: &App, chat_jid: &str, protocol_id: &str) -> bool {
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        store.messages(chat_jid).unwrap().iter().any(|(m, _)| m.id == protocol_id)
    }

    fn notice_kind(app: &App) -> Option<crate::state::NoticeKind> {
        app.notice.as_ref().map(|n| n.kind)
    }

    fn mirror_flag(app: &App, chat_id: u64, mid: u64, pin: bool) -> bool {
        let mirror = app.live.as_ref().unwrap().messages.get(&chat_id).unwrap();
        let msg = &mirror.iter().find(|(_, m)| m.id == mid).unwrap().1;
        if pin {
            msg.pinned
        } else {
            msg.starred
        }
    }

    fn view_flag(app: &App, mid: u64, pin: bool) -> bool {
        let msg = app.conversation.iter().find(|m| m.id == mid).unwrap();
        if pin {
            msg.pinned
        } else {
            msg.starred
        }
    }

    fn archived_flags(app: &App, chat_jid: &str, protocol_id: &str) -> i64 {
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        store
            .messages(chat_jid)
            .unwrap()
            .into_iter()
            .find(|(m, _)| m.id == protocol_id)
            .expect("the message reached the archive")
            .1
    }

    fn star_command(
        cmd_rx: &std::sync::mpsc::Receiver<BackendCommand>,
    ) -> (String, String, Option<String>, bool, bool) {
        let sent = sent_commands(cmd_rx);
        let found = sent.iter().find_map(|c| match c {
            BackendCommand::StarMessage { chat_jid, message_id, sender_jid, from_me, star } => {
                Some((chat_jid.clone(), message_id.clone(), sender_jid.clone(), *from_me, *star))
            }
            _ => None,
        });
        found.expect("a star command is sent")
    }

    fn pin_command(
        cmd_rx: &std::sync::mpsc::Receiver<BackendCommand>,
    ) -> (String, String, Option<String>, bool, bool) {
        let sent = sent_commands(cmd_rx);
        let found = sent.iter().find_map(|c| match c {
            BackendCommand::PinMessage {
                chat_jid, message_id, sender_jid, from_me, pin, ..
            } => Some((chat_jid.clone(), message_id.clone(), sender_jid.clone(), *from_me, *pin)),
            _ => None,
        });
        found.expect("a pin command is sent")
    }

    /// A group star names the participant who authored the
    /// target message: the JID recorded for the message when it
    /// arrived is what the command carries, not None.
    #[test]
    fn group_star_names_the_message_author() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.toggle_star_message(mid);
        let (chat_jid, message_id, sender_jid, from_me, star) = star_command(&cmd_rx);
        assert_eq!(chat_jid, GROUP_JID);
        assert_eq!(message_id, "g1");
        assert_eq!(sender_jid.as_deref(), Some(ALICE_JID), "the author is named");
        assert!(!from_me);
        assert!(star);
        assert_eq!(app.pending_stars.get(&mid), Some(&false), "the change awaits settlement");
    }

    /// A group pin follows the same author rule as the star.
    #[test]
    fn group_pin_names_the_message_author() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.toggle_pin_message(mid, backend::PinDuration::Days7);
        let (chat_jid, message_id, sender_jid, from_me, pin) = pin_command(&cmd_rx);
        assert_eq!(chat_jid, GROUP_JID);
        assert_eq!(message_id, "g1");
        assert_eq!(sender_jid.as_deref(), Some(ALICE_JID), "the author is named");
        assert!(!from_me);
        assert!(pin);
        assert_eq!(app.pending_pins.get(&mid), Some(&false), "the change awaits settlement");
    }

    /// Starring my own group message needs no sender: the
    /// library resolves my group identity itself, and a sender
    /// in my name could even be the wrong one (PN vs LID
    /// addressing is the library's call).
    #[test]
    fn own_group_star_omits_the_sender() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let msg = LiveMessage::text("g2", GROUP_JID, None, "my own", 1_700_000_000, true);
        let mid = open_message(&mut app, &tx, &ctx, msg);
        app.toggle_star_message(mid);
        let (_, _, sender_jid, from_me, _) = star_command(&cmd_rx);
        assert_eq!(sender_jid, None);
        assert!(from_me);
    }

    /// Direct-chat stars are unchanged: no sender is attached
    /// even though the partner's JID is on record, because a DM
    /// key carries no participant.
    #[test]
    fn dm_star_carries_no_sender() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mut msg =
            LiveMessage::text("d1", DM_JID, Some("Partner".to_owned()), "hi", 1_700_000_000, false);
        msg.sender_jid = Some(DM_JID.to_owned());
        let mid = open_message(&mut app, &tx, &ctx, msg);
        app.toggle_star_message(mid);
        let (chat_jid, _, sender_jid, from_me, _) = star_command(&cmd_rx);
        assert_eq!(chat_jid, DM_JID);
        assert_eq!(sender_jid, None);
        assert!(!from_me);
    }

    /// A star the backend reports as failed rolls the
    /// optimistic state back everywhere it landed: the open
    /// view, the session mirror, and the persisted archive
    /// flag. The failure reaches the notice banner.
    #[test]
    fn failed_group_star_rolls_back() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.toggle_star_message(mid);
        assert!(view_flag(&app, mid, false), "the optimistic star shows");
        assert!(mirror_flag(&app, chat_id, mid, false));
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), store::FLAG_STARRED);

        tx.send(BackendEvent::StarFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Starring failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!view_flag(&app, mid, false), "the open view rolls back");
        assert!(!mirror_flag(&app, chat_id, mid, false), "the mirror rolls back");
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), 0, "the archive flag clears");
        assert!(!app.pending_stars.contains_key(&mid), "nothing stays pending");
        let notice = app.notice_str().unwrap_or_default().to_owned();
        assert!(notice.contains("Starring failed"), "the failure is surfaced: {notice}");
    }

    /// The pin counterpart: a refused pin restores the
    /// unpinned state in the view, the mirror, and the archive.
    #[test]
    fn failed_group_pin_rolls_back() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.toggle_pin_message(mid, backend::PinDuration::Days7);
        assert!(view_flag(&app, mid, true), "the optimistic pin shows");
        assert!(mirror_flag(&app, chat_id, mid, true));
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), store::FLAG_PINNED_MESSAGE);

        tx.send(BackendEvent::PinFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Pinning the message failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!view_flag(&app, mid, true), "the open view rolls back");
        assert!(!mirror_flag(&app, chat_id, mid, true), "the mirror rolls back");
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), 0, "the archive flag clears");
        assert!(!app.pending_pins.contains_key(&mid), "nothing stays pending");
        let notice = app.notice_str().unwrap_or_default().to_owned();
        assert!(notice.contains("Pinning the message failed"), "the failure is surfaced: {notice}");
    }

    /// Section 24: revoking another member's group message
    /// (the admin path) names the message's author in the
    /// command, exactly like star and pin do; before this round
    /// the revoke command carried no author at all, so the
    /// library could only ever take the sender path.
    #[test]
    fn admin_revoke_names_the_group_author() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.delete_for_everyone(mid);
        let sent = sent_commands(&cmd_rx);
        let revoke = sent.iter().find_map(|c| match c {
            BackendCommand::RevokeMessage { chat_jid, message_id, sender_jid, from_me } => {
                Some((chat_jid.clone(), message_id.clone(), sender_jid.clone(), *from_me))
            }
            _ => None,
        });
        assert_eq!(
            revoke.expect("a revoke command is sent"),
            (GROUP_JID.to_owned(), "g1".to_owned(), Some(ALICE_JID.to_owned()), false),
            "the revoke target names the author and the admin path"
        );
    }

    /// Section 24: delete for me removes the message from the
    /// open view and the mirror at once, and the command names
    /// the author and carries the archive timestamp.
    #[test]
    fn delete_for_me_removes_and_sends() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.delete_for_me(mid);
        assert!(app.conversation.iter().all(|m| m.id != mid), "the message leaves the open view");
        let mirror = app.live.as_ref().unwrap().messages.get(&chat_id).unwrap();
        assert!(mirror.iter().all(|(_, m)| m.id != mid), "and the session mirror");
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(|c| matches!(
                c,
                BackendCommand::DeleteMessageForMe { chat_jid, message_id, sender_jid, message_timestamp, .. }
                    if chat_jid == GROUP_JID
                        && message_id == "g1"
                        && sender_jid.as_deref() == Some(ALICE_JID)
                        && *message_timestamp == Some(1_700_000_000)
            )),
            "the command names the author and the archived timestamp: {sent:?}"
        );
    }

    /// Section 24: keeping a disappearing message applies at
    /// once (view, archive flag, command), and a KeepFailed
    /// rolls all three back, the star discipline exactly.
    #[test]
    fn keep_toggle_applies_and_failed_keep_rolls_back() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mut msg = alice_group_message();
        msg.expires_at = 1_700_003_600;
        let mid = open_message(&mut app, &tx, &ctx, msg);
        app.toggle_keep_message(mid);
        assert!(
            app.conversation.iter().find(|m| m.id == mid).is_some_and(|m| m.kept),
            "the optimistic keep shows"
        );
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), store::FLAG_KEPT);
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(|c| matches!(
                c,
                BackendCommand::KeepMessage { message_id, keep, .. } if message_id == "g1" && *keep
            )),
            "the keep command is sent: {sent:?}"
        );

        tx.send(BackendEvent::KeepFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Keeping the message failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            !app.conversation.iter().find(|m| m.id == mid).is_some_and(|m| m.kept),
            "the view rolls back"
        );
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), 0, "the archive flag clears");
        assert!(!app.pending_keeps.contains_key(&mid), "nothing stays pending");
    }

    /// Section 24: a keep made on another device arrives as a
    /// MessageKept report and lands in the view and the
    /// archive without any command of mine.
    #[test]
    fn kept_report_from_the_server_lands() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mut msg = alice_group_message();
        msg.expires_at = 1_700_003_600;
        let mid = open_message(&mut app, &tx, &ctx, msg);
        tx.send(BackendEvent::MessageKept {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            kept: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            app.conversation.iter().find(|m| m.id == mid).is_some_and(|m| m.kept),
            "the reported keep shows"
        );
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), store::FLAG_KEPT);
    }

    /// Section 24: mark as unread raises the badge at once,
    /// persists it in the archive, and sends the read-state
    /// command (never a receipt).
    #[test]
    fn mark_unread_raises_and_persists_the_badge() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let _ = mid;
        let chat_id = jid_id(GROUP_JID);
        // Start from a known read state, whatever the arrival
        // itself did to the badge.
        app.apply_chat_read_changed(chat_id, true, 0);
        assert_eq!(app.chats.iter().find(|c| c.id == chat_id).map_or(9, |c| c.unread), 0);
        app.mark_chat_unread(chat_id);
        assert_eq!(
            app.chats.iter().find(|c| c.id == chat_id).map_or(0, |c| c.unread),
            1,
            "the badge floors at one"
        );
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        let archived = store.chats().unwrap();
        assert_eq!(
            archived.iter().find(|c| c.jid == GROUP_JID).map_or(0, |c| c.unread),
            1,
            "the archive row carries the badge"
        );
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(|c| matches!(c, BackendCommand::MarkChatUnread { chat_jid } if chat_jid == GROUP_JID)),
            "the mark-unread command is sent: {sent:?}"
        );
    }

    /// Section 24: a timed mute carries its end in the mirror
    /// and the archive, sends MuteUntil in milliseconds, and
    /// lifts itself when the end passes.
    #[test]
    fn timed_mute_sets_end_and_lifts_when_it_passes() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.apply_mute_for(chat_id, MuteChoice::Hours8);
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(chat.muted);
        let until = chat.mute_until;
        let now = unix_now_secs();
        assert!(
            until > now && until <= now + 8 * 3_600,
            "the end is 8 hours out: {until} vs {now}"
        );
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(|c| matches!(
                c,
                BackendCommand::ChatAction { action: ChatAction::MuteUntil(ms), .. }
                    if *ms == i64::try_from(until).unwrap_or(0) * 1000
            )),
            "the wire end is the mirror end in milliseconds: {sent:?}"
        );
        app.lift_expired_mutes(until);
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(!chat.muted && chat.mute_until == 0, "the mute lifts itself at its end");
    }

    /// Section 24: clearing is ack-gated. The command goes out
    /// with the keep-starred choice and a Busy notice shows,
    /// but nothing is removed until the server confirms; the
    /// confirmation then keeps the starred message and drops
    /// the rest.
    #[test]
    fn clear_chat_keeps_starred_when_asked() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mut starred = alice_group_message();
        starred.starred = true;
        let mid = open_message(&mut app, &tx, &ctx, starred);
        let mut other = alice_group_message();
        other.id = "g2".to_owned();
        let mid2 = open_message(&mut app, &tx, &ctx, other);
        let chat_id = jid_id(GROUP_JID);
        app.clear_chat(chat_id, true);
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(|c| matches!(
                c,
                BackendCommand::ClearChat { chat_jid, keep_starred } if chat_jid == GROUP_JID && *keep_starred
            )),
            "the clear command carries the keep-starred choice: {sent:?}"
        );
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Busy), "the wait is announced");
        assert!(
            app.conversation.iter().any(|m| m.id == mid2),
            "nothing is cleared before the server answers"
        );
        tx.send(BackendEvent::ChatCleared {
            chat_jid: GROUP_JID.to_owned(),
            delete_starred: false,
            last_message_timestamp: None,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.conversation.iter().any(|m| m.id == mid), "the starred message stays");
        assert!(app.conversation.iter().all(|m| m.id != mid2), "the rest is cleared");
        assert!(app.pending_clears.is_empty(), "the wait is settled");
    }

    /// Section 24: a failed clear destroys nothing: every
    /// message stays, in the view and in the archive, and the
    /// failure is the notice.
    #[test]
    fn clear_chat_failure_keeps_everything() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.clear_chat(chat_id, false);
        tx.send(BackendEvent::ClearChatFailed {
            chat_jid: GROUP_JID.to_owned(),
            message: "Clearing the chat failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.conversation.iter().any(|m| m.id == mid), "the message survives");
        assert!(archived_has(&app, GROUP_JID, "g1"), "the archive row survives");
        assert!(
            notice_kind(&app) == Some(crate::state::NoticeKind::Error),
            "the failure is announced"
        );
        assert!(app.pending_clears.is_empty(), "the wait is settled");
    }

    /// Section 24: deleting a chat is ack-gated too: the chat
    /// stays in the list until the server confirms the delete.
    #[test]
    fn delete_chat_removes_and_sends() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        assert!(app.chats.iter().any(|c| c.id == chat_id));
        app.delete_chat(chat_id);
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(
                |c| matches!(c, BackendCommand::DeleteChat { chat_jid } if chat_jid == GROUP_JID)
            ),
            "the delete command is sent: {sent:?}"
        );
        assert!(
            app.chats.iter().any(|c| c.id == chat_id),
            "the chat stays until the server confirms"
        );
        tx.send(BackendEvent::ChatDeleted { chat_jid: GROUP_JID.to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.chats.iter().all(|c| c.id != chat_id), "the chat leaves the list");
        assert!(app.pending_chat_deletes.is_empty(), "the wait is settled");
    }

    /// Section 24: a failed delete keeps the chat, its
    /// messages, and its archive rows.
    #[test]
    fn delete_chat_failure_keeps_the_chat() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.delete_chat(chat_id);
        tx.send(BackendEvent::DeleteChatFailed {
            chat_jid: GROUP_JID.to_owned(),
            message: "Deleting the chat failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.chats.iter().any(|c| c.id == chat_id), "the chat stays");
        assert!(archived_has(&app, GROUP_JID, "g1"), "its messages stay");
        assert!(
            notice_kind(&app) == Some(crate::state::NoticeKind::Error),
            "the failure is announced"
        );
    }

    /// Section 24: a failed timed mute restores the exact
    /// previous state (unmuted, no end), and a failed unmute of
    /// a timed mute restores the end with it.
    #[test]
    fn failed_chat_action_restores_the_mute_snapshot() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.apply_mute_for(chat_id, MuteChoice::Hours8);
        let until = app.chats.iter().find(|c| c.id == chat_id).unwrap().mute_until;
        assert!(until > 0);
        tx.send(BackendEvent::ChatActionFailed {
            chat_jid: GROUP_JID.to_owned(),
            action: backend::ChatAction::MuteUntil(1),
            message: "Changing the chat failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(!chat.muted && chat.mute_until == 0, "the failed mute rolls back fully");
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
        // And the other direction: a settled timed mute whose
        // unmute fails keeps its end.
        app.apply_mute_for(chat_id, MuteChoice::Hours8);
        let until = app.chats.iter().find(|c| c.id == chat_id).unwrap().mute_until;
        tx.send(BackendEvent::MuteChanged {
            chat_jid: GROUP_JID.to_owned(),
            muted: true,
            mute_until: i64::try_from(until).unwrap_or(0),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.pending_chat_actions.is_empty(), "the report settles the mute");
        app.apply_chat_action(chat_id, false, false, false);
        tx.send(BackendEvent::ChatActionFailed {
            chat_jid: GROUP_JID.to_owned(),
            action: backend::ChatAction::Unmute,
            message: "Changing the chat failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(
            chat.muted && chat.mute_until == until,
            "the failed unmute restores the timed mute"
        );
    }

    /// Section 24: mark-as-unread rolls the badge back when
    /// the server refuses it.
    #[test]
    fn failed_mark_unread_restores_the_badge() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.apply_chat_read_changed(chat_id, true, 0);
        app.mark_chat_unread(chat_id);
        assert_eq!(app.chats.iter().find(|c| c.id == chat_id).map_or(0, |c| c.unread), 1);
        tx.send(BackendEvent::MarkUnreadFailed {
            chat_jid: GROUP_JID.to_owned(),
            message: "Marking the chat unread failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.chats.iter().find(|c| c.id == chat_id).map_or(9, |c| c.unread),
            0,
            "the badge rolls back"
        );
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
    }

    /// Section 24: a block is announced only when the
    /// server's blocklist confirms it, and a failed block
    /// restores the set.
    #[test]
    fn block_waits_for_the_blocklist_and_rolls_back_on_failure() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_dm_message());
        let chat_id = jid_id(DM_JID);
        app.notice = None;
        app.toggle_block_contact(chat_id);
        assert!(app.blocked_chats.contains(&chat_id), "the set flips at once");
        assert!(app.notice.is_none(), "no success is claimed before the server answers");
        tx.send(BackendEvent::Blocklist(vec![DM_JID.to_owned()])).unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            app.notice_str() == Some("Contact blocked."),
            "the confirmation speaks the success: {:?}",
            app.notice
        );
        // Unblock, then fail it: the contact stays blocked.
        app.toggle_block_contact(chat_id);
        assert!(!app.blocked_chats.contains(&chat_id));
        tx.send(BackendEvent::BlockFailed {
            chat_jid: DM_JID.to_owned(),
            message: "Changing the block failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.blocked_chats.contains(&chat_id), "the failed unblock restores the block");
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
    }

    /// Section 24: a group info save applies its title only
    /// when the backend confirms it; a failure keeps the old
    /// title and says so.
    #[test]
    fn group_info_save_waits_for_confirmation() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        let old_title = app.chats.iter().find(|c| c.id == chat_id).unwrap().title.clone();
        app.group_subject_edit = "Renamed Group".to_owned();
        app.group_desc_edit = String::new();
        app.notice = None;
        app.save_group_info(chat_id);
        assert_eq!(
            app.chats.iter().find(|c| c.id == chat_id).unwrap().title,
            old_title,
            "the title does not move at click time"
        );
        assert!(app.notice.is_none(), "nothing is claimed at click time");
        tx.send(BackendEvent::GroupInfoFailed {
            chat_jid: GROUP_JID.to_owned(),
            message: "Saving the group info failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.chats.iter().find(|c| c.id == chat_id).unwrap().title, old_title);
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
        app.save_group_info(chat_id);
        tx.send(BackendEvent::GroupInfoSaved { chat_jid: GROUP_JID.to_owned() }).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            app.chats.iter().find(|c| c.id == chat_id).unwrap().title,
            "Renamed Group",
            "the confirmed save applies the title"
        );
        assert!(
            app.notice_str() == Some("Group info saved."),
            "the confirmation speaks the success"
        );
    }

    /// Section 24: a failed leave restores the row the
    /// optimistic archive took out of the archived view.
    #[test]
    fn failed_leave_restores_the_row() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.leave_group(chat_id);
        assert!(app.chats.iter().find(|c| c.id == chat_id).unwrap().archived);
        tx.send(BackendEvent::LeaveGroupFailed {
            chat_jid: GROUP_JID.to_owned(),
            message: "Leaving the group failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            !app.chats.iter().find(|c| c.id == chat_id).unwrap().archived,
            "the failed leave un-archives the row"
        );
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
    }

    /// Section 24: a failed timer change restores the
    /// previous duration quietly.
    #[test]
    fn failed_timer_restores_the_previous_duration() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.set_disappearing_timer(chat_id, 86_400);
        assert_eq!(app.ephemeral_secs_of(chat_id), 86_400);
        tx.send(BackendEvent::DisappearingTimerFailed {
            chat_jid: GROUP_JID.to_owned(),
            message: "Changing the timer failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(app.ephemeral_secs_of(chat_id), 0, "the timer rolls back");
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
    }

    /// Section 24: a failed edit restores the original body,
    /// and a failed revoke restores the message itself.
    #[test]
    fn failed_edit_and_revoke_restore_the_message() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mut mine = alice_group_message();
        mine.sender = None;
        mine.from_me = true;
        let mid = open_message(&mut app, &tx, &ctx, mine);
        let original = app.conversation.iter().find(|m| m.id == mid).unwrap().body.clone();
        app.apply_edit(mid, "a corrected body");
        assert_eq!(app.conversation.iter().find(|m| m.id == mid).unwrap().body, "a corrected body");
        tx.send(BackendEvent::EditMessageFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Editing the message failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let msg = app.conversation.iter().find(|m| m.id == mid).unwrap();
        assert_eq!(msg.body, original, "the failed edit restores the body");
        assert!(!msg.edited, "and the edited mark");
        app.delete_for_everyone(mid);
        assert!(app.conversation.iter().find(|m| m.id == mid).unwrap().deleted);
        tx.send(BackendEvent::RevokeMessageFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Deleting the message failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            !app.conversation.iter().find(|m| m.id == mid).unwrap().deleted,
            "the failed revoke restores the message"
        );
    }

    /// Section 24: a failed delete-for-me restores the
    /// message everywhere the optimistic removal took it
    /// from: the view, the mirror, and the archive.
    #[test]
    fn failed_delete_for_me_restores_the_message() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.delete_for_me(mid);
        assert!(app.conversation.iter().all(|m| m.id != mid), "the removal applies at once");
        tx.send(BackendEvent::DeleteForMeFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Deleting the message failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(app.conversation.iter().any(|m| m.id == mid), "the view gets it back");
        assert!(archived_has(&app, GROUP_JID, "g1"), "the archive gets it back");
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
        assert!(app.pending_delete_for_me.is_empty(), "the wait is settled");
    }

    /// Section 24: a failed poll vote restores my previous
    /// selection in the view, and the notice names the
    /// failure.
    #[test]
    fn failed_poll_vote_restores_my_selection() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mut poll = alice_group_message();
        poll.kind = backend::LiveMessageKind::Poll {
            question: "Lunch?".to_owned(),
            options: vec![("Deli".to_owned(), 1), ("Sushi".to_owned(), 0)],
            multiple: false,
            my_votes: vec![0],
            correct: None,
        };
        let mid = open_message(&mut app, &tx, &ctx, poll);
        let my_votes =
            |app: &App| match &app.conversation.iter().find(|m| m.id == mid).unwrap().kind {
                MessageKind::Poll { ref my_votes, .. } => my_votes.clone(),
                _ => panic!("not a poll"),
            };
        app.vote_poll(mid, 1);
        assert_eq!(my_votes(&app), vec![1], "the vote applies at once");
        tx.send(BackendEvent::PollVoteFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Voting failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(my_votes(&app), vec![0], "the failed vote restores my selection");
        assert!(notice_kind(&app) == Some(crate::state::NoticeKind::Error));
        assert!(app.pending_poll_votes.is_empty(), "the wait is settled");
    }

    /// A pin made on another device arrives as a
    /// MessagePinChanged report and lands on the message in
    /// the view, the mirror, and the archive.
    #[test]
    fn server_pin_change_lands_on_the_message() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        tx.send(BackendEvent::MessagePinChanged {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            pinned: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(view_flag(&app, mid, true), "the view shows the pin");
        assert!(mirror_flag(&app, chat_id, mid, true), "the mirror shows the pin");
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), store::FLAG_PINNED_MESSAGE);
        tx.send(BackendEvent::MessagePinChanged {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            pinned: false,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!view_flag(&app, mid, true), "the unpin lands too");
    }

    /// The status broadcast address never becomes a chat:
    /// neither a chat upsert nor a message filed under it
    /// creates the fake "status" DM an earlier build showed.
    #[test]
    fn status_broadcast_never_becomes_a_chat() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::ChatUpsert(LiveChat {
            jid: "status@broadcast".to_owned(),
            title: "status".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "a status".to_owned(),
            preview_sender: None,
            timestamp: 1_700_000_000,
            unread: 1,
            archived: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
        }))
        .unwrap();
        let mut status_msg = alice_dm_message();
        status_msg.chat_jid = "status@broadcast".to_owned();
        tx.send(BackendEvent::MessageUpsert(status_msg)).unwrap();
        drain_all(&mut app, &ctx);
        assert!(
            app.chats.iter().all(|c| c.title != "status"),
            "no status chat exists: {:?}",
            app.chats.iter().map(|c| &c.title).collect::<Vec<_>>()
        );
        assert!(
            !app.live.as_ref().unwrap().messages.contains_key(&jid_id("status@broadcast")),
            "no mirror is kept under the broadcast address"
        );
    }

    /// A timed mute survives message-arrival upserts, which
    /// report mute_until 0 without knowing better; the merged
    /// row keeps the end in the view and the archive.
    #[test]
    fn timed_mute_survives_arrival_upserts() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let _mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        let chat_id = jid_id(GROUP_JID);
        app.apply_mute_for(chat_id, MuteChoice::Hours8);
        let until = app.chats.iter().find(|c| c.id == chat_id).unwrap().mute_until;
        tx.send(BackendEvent::ChatUpsert(LiveChat {
            jid: GROUP_JID.to_owned(),
            title: "Group".to_owned(),
            kind: LiveChatKind::Group,
            preview: "new message".to_owned(),
            preview_sender: None,
            timestamp: 1_700_000_060,
            unread: 0,
            archived: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
        }))
        .unwrap();
        drain_all(&mut app, &ctx);
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(chat.muted && chat.mute_until == until, "the timed mute keeps its end");
        let store = app.live.as_ref().unwrap().store.as_ref().unwrap();
        let archived = store.chats().unwrap();
        let row = archived.iter().find(|c| c.jid == GROUP_JID).unwrap();
        assert_eq!(row.mute_until, i64::try_from(until).unwrap_or(0), "the archive keeps it too");
    }

    /// The dock's Report row opens the spam-report
    /// confirmation for the chat's latest incoming message,
    /// and refuses honestly when there is nothing to report.
    #[test]
    fn report_chat_opens_the_spam_confirmation() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_dm_message());
        let chat_id = jid_id(DM_JID);
        app.report_chat(chat_id);
        assert!(
            matches!(
                app.ui_confirm,
                Some(crate::state::UiConfirm::ReportSpam { chat_id: c, msg_id: m, .. })
                    if c == chat_id && m == mid
            ),
            "the confirmation names the latest incoming message: {:?}",
            app.ui_confirm.as_ref().map(|_| "other")
        );
        // A chat with no incoming message: an honest refusal,
        // no modal that could only fail.
        let (mut app2, tx2, ctx2, _rx2) = live_app();
        let mut mine = alice_dm_message();
        mine.sender = None;
        mine.from_me = true;
        let _ = open_message(&mut app2, &tx2, &ctx2, mine);
        app2.report_chat(chat_id);
        assert!(app2.ui_confirm.is_none(), "no confirmation opens");
        assert!(
            notice_kind(&app2) == Some(crate::state::NoticeKind::Error),
            "the refusal is spoken"
        );
    }

    /// A received voice note renders the waveform its payload
    /// carried; a payload without one renders the neutral
    /// flat form, never a shape derived from the caption.
    #[test]
    fn voice_waveform_comes_from_the_wire_or_is_neutral() {
        let bars = voice_waveform_bars(Some(&[0, 128, 255]));
        assert_eq!(bars.len(), 3);
        assert!(bars[0] < bars[1] && bars[1] < bars[2], "the bars follow the bytes: {bars:?}");
        let flat = voice_waveform_bars(None);
        assert!(flat.iter().all(|b| *b == flat[0]), "no payload, one flat height");
        let mut msg = alice_dm_message();
        msg.kind = backend::LiveMessageKind::Media {
            media: backend::MediaKind::Voice,
            caption: String::new(),
            seconds: 3,
            size_bytes: 10,
        };
        msg.voice_waveform = Some(vec![255, 0]);
        let converted = to_message_with_flags(&msg, 0);
        match converted.kind {
            MessageKind::Voice { waveform, .. } => {
                assert_eq!(waveform.len(), 2);
                assert!(waveform[0] > waveform[1], "the wire shape survives conversion");
            }
            other => panic!("expected a voice message, got {other:?}"),
        }
    }

    /// Section 24: the pin command carries the duration the
    /// menu picked, not a fixed one.
    #[test]
    fn pin_command_carries_the_chosen_duration() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.toggle_pin_message(mid, backend::PinDuration::Hours24);
        let sent = sent_commands(&cmd_rx);
        assert!(
            sent.iter().any(|c| matches!(
                c,
                BackendCommand::PinMessage { duration: backend::PinDuration::Hours24, .. }
            )),
            "the 24-hour duration reaches the command: {sent:?}"
        );
    }

    /// Rolling back an *unstar* restores the star the server
    /// had confirmed before, not an empty state.
    #[test]
    fn failed_unstar_restores_the_star() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mut msg = alice_group_message();
        msg.starred = true;
        let mid = open_message(&mut app, &tx, &ctx, msg);
        assert!(view_flag(&app, mid, false), "the message starts starred");

        app.toggle_star_message(mid);
        assert!(!view_flag(&app, mid, false), "the optimistic unstar shows");
        assert_eq!(app.pending_stars.get(&mid), Some(&true));

        tx.send(BackendEvent::StarFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Starring failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(view_flag(&app, mid, false), "the confirmed star is restored");
        assert!(mirror_flag(&app, chat_id, mid, false));
        assert_eq!(archived_flags(&app, GROUP_JID, "g1"), store::FLAG_STARRED);
        assert!(!app.pending_stars.contains_key(&mid), "nothing stays pending");
    }

    /// A server report of the star settles the pending change:
    /// the value stays, and the snapshot is gone, so a later
    /// failure report for a different change starts from the
    /// confirmed state.
    #[test]
    fn server_star_report_confirms_the_pending_change() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.toggle_star_message(mid);
        assert!(app.pending_stars.contains_key(&mid));

        tx.send(BackendEvent::StarChanged {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            starred: true,
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(mirror_flag(&app, chat_id, mid, false), "the star stands");
        assert!(!app.pending_stars.contains_key(&mid), "the report confirms the change");
    }

    /// An upsert that already carries the pinned state settles
    /// a pending pin (the pin has no report event of its own):
    /// the value stays and the snapshot is gone.
    #[test]
    fn pin_upsert_confirms_the_pending_change() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.toggle_pin_message(mid, backend::PinDuration::Days7);
        assert!(app.pending_pins.contains_key(&mid));

        let mut confirmed = alice_group_message();
        confirmed.pinned = true;
        tx.send(BackendEvent::MessageUpsert(confirmed)).unwrap();
        drain_all(&mut app, &ctx);
        assert!(mirror_flag(&app, chat_id, mid, true), "the pin stands");
        assert!(!app.pending_pins.contains_key(&mid), "the upsert confirms the change");
    }
}

#[cfg(test)]
mod start_fresh_tests {
    use super::{move_stranded_data_aside, stranded_files};

    fn temp_data_dir(tag: &str) -> std::path::PathBuf {
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
    fn stranded_files_cover_both_databases_and_the_key_file() {
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
    fn start_fresh_move_keeps_the_data_and_clears_the_way() {
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
