//! Live mode: the bridge between the `backend` crate and the UI.
//!
//! In live mode the chat list and conversation are fed by backend
//! events (and, on start, by the `store` archive) instead of the demo
//! dataset. Everything protocol-shaped stays in this module: JID →
//! numeric ID mapping, unix timestamps → display times, and the
//! write-through of every upsert into the archive.

mod actions;
mod changed;
mod events;
mod media;
mod messages;
mod profile;
mod session;
mod zip;

pub(crate) use zip::{ZipGather, ZipItem, ZipItemState, ZipPayload};

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
            Self::Starting => crate::i18n::t("connect.status_connecting"),
            Self::WaitingQr => crate::i18n::t("connect.status_waiting_scan"),
            Self::CodeReady => crate::i18n::t("connect.status_code_ready"),
            Self::Linked => crate::i18n::t("connect.status_linked"),
            Self::LoggingOut => crate::i18n::t("connect.status_logging_out"),
            Self::LoggedOut => crate::i18n::t("connect.status_logged_out"),
            Self::Banned(code) => {
                crate::i18n::tf("connect.status_banned", "code", &code.to_string())
            }
            Self::Lost(reason) => reason.clone(),
            Self::Crashed(summary) => crate::i18n::tf("connect.status_crashed", "summary", summary),
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
    /// Event tally states produced by response events in this
    /// batch, with the same snapshot contract as
    /// [`Self::poll_tally_writes`]: the flush writes them after
    /// the batch's upserts and the last snapshot wins.
    pub(crate) event_tally_writes: Vec<EventTallyWrite>,
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

/// One event's aggregated tally state, on its way to the
/// archive: the three counts, my response, and the
/// per-responder ballots they were derived from.
pub(crate) struct EventTallyWrite {
    /// Chat JID the event lives in.
    pub(crate) chat_jid: String,
    /// Protocol ID of the event message.
    pub(crate) message_id: String,
    /// Responses counted Going.
    pub(crate) going: u32,
    /// Responses counted Maybe.
    pub(crate) maybe: u32,
    /// Responses counted Not going.
    pub(crate) not_going: u32,
    /// My current response, when I have answered.
    pub(crate) my_rsvp: Option<backend::EventRsvp>,
    /// Latest response per responder.
    pub(crate) ballots: Vec<(String, backend::EventRsvp)>,
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
    /// Video poster references per numeric message ID, seeded
    /// and extended exactly like `photo_refs`. A poster fetch
    /// for a video whose payload carried no embedded thumbnail
    /// passes the reference along, so the tile can present the
    /// video's own hosted frame.
    pub(crate) poster_refs: HashMap<u64, backend::PosterRef>,
    /// Video frame dimensions (width, height) per numeric
    /// message ID, as the wire payload stated them, seeded and
    /// extended exactly like `video_refs`. The tile takes the
    /// video's own proportions from here before any thumbnail
    /// or bytes are here.
    pub(crate) video_dims: HashMap<u64, (u32, u32)>,
    /// Sticker re-download references per numeric message ID,
    /// seeded and extended exactly like `photo_refs`.
    pub(crate) sticker_refs: HashMap<u64, backend::StickerRef>,
    /// Document re-download references per numeric message ID,
    /// seeded and extended exactly like `photo_refs`. A document
    /// download passes the reference along, so a document from
    /// before this session still downloads.
    pub(crate) document_refs: HashMap<u64, backend::DocumentRef>,
    /// GIF-marked videos per numeric message ID: the clip loops
    /// and wears no playback controls in the viewer, and its
    /// tile wears the GIF badge.
    pub(crate) gif_videos: std::collections::HashSet<u64>,
    /// Sticker animation flags per numeric message ID,
    /// `(animated, lottie)`, seeded and extended exactly like
    /// `sticker_refs`. The tile badges an animated sticker it
    /// can only show as a still, and presents a Lottie
    /// sticker's embedded frame instead of fetching its
    /// (non-image) bytes.
    pub(crate) sticker_anim: HashMap<u64, (bool, bool)>,
    /// Sticker static frames (the wire PNG bytes) per numeric
    /// message ID: a Lottie sticker's only picture.
    pub(crate) sticker_thumbs: HashMap<u64, Vec<u8>>,
    /// Document thumbnails (the wire JPEG bytes) per numeric
    /// message ID, seeded and extended exactly like
    /// `video_thumbs`, so a document's card can present the
    /// document's own preview image.
    pub(crate) document_thumbs: HashMap<u64, Vec<u8>>,
    /// Location map snapshots (the wire JPEG bytes) per numeric
    /// message ID, seeded and extended exactly like
    /// `video_thumbs`, so a location's card can present the
    /// place's own picture.
    pub(crate) location_thumbs: HashMap<u64, Vec<u8>>,
    /// Link preview images (the wire JPEG bytes) per numeric
    /// message ID, seeded and extended exactly like
    /// `video_thumbs`, so a preview card can present the page's
    /// own picture instead of a stand-in.
    pub(crate) preview_thumbs: HashMap<u64, Vec<u8>>,
    /// View-once message IDs whose kind carries no view-once
    /// flag of its own (voice notes, documents, stickers):
    /// the stream reads the message's sealed state from here.
    pub(crate) view_once_ids: std::collections::HashSet<u64>,
    /// Verified business names by identity JID, from history
    /// payloads and usync answers; presence is what badges a
    /// chat.
    pub(crate) verified_names: HashMap<String, String>,
    /// Identity JIDs whose verified-name state is settled for
    /// this session (answered, or asked and in flight), so the
    /// lookup runs at most once per identity per session.
    pub(crate) verified_checked: std::collections::HashSet<String>,
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
    /// Channel metadata by JID (role, followers, invite code),
    /// fed by the backend's channel events. The roster carries
    /// the channel's chat; this carries what the Channels
    /// surface and the channel conversation need beyond it.
    pub(crate) channel_meta: HashMap<String, backend::LiveChannel>,
    /// Channel info payloads by JID, from the last info fetch
    /// per channel (admin count and the follower sample are
    /// not part of the plain metadata).
    pub(crate) channel_infos: HashMap<String, backend::LiveChannelInfo>,
    /// Channel updates by (chat ID, server ID) to numeric
    /// message ID, extended by every upsert that carries a
    /// server ID: channel reactions, channel poll votes and
    /// channel counter updates all key on the server ID,
    /// never the protocol ID, and server IDs are only unique
    /// within their channel. Session-scoped: a channel's
    /// history fetch on open re-earns them after a restart.
    pub(crate) server_ids: HashMap<(u64, u64), u64>,
    /// Per-chat channel-history fetch state, the HistoryFetch
    /// discipline on the channel pagination: present while a
    /// page request is outstanding or the channel's history
    /// is exhausted; absence is idle.
    pub(crate) channel_history: HashMap<u64, HistoryFetch>,
    /// Optimistic channel poll votes by numeric message ID:
    /// the selection the app showed before a vote command, so
    /// a ChannelPollVoteFailed can restore it (the
    /// pending_reactions discipline for channel polls).
    pub(crate) pending_channel_votes: HashMap<u64, Vec<usize>>,
    /// The subscribed-channel list has been requested this
    /// session: the Channels panel asks once, and the answer
    /// (or its failure, shown in the panel) is what it shows
    /// until the panel's own retry.
    pub(crate) channels_list_requested: bool,
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
    /// Per-responder event ballots per numeric message ID, the
    /// event counterpart of [`Self::poll_ballots`]: seeded from
    /// the archive at start and extended by every response
    /// event, so last-response-wins stays honest across
    /// restarts (see `store::Store::set_event_tally`).
    pub(crate) event_ballots: HashMap<u64, Vec<(String, backend::EventRsvp)>>,
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

/// The broadcast address statuses are delivered under. Events
/// addressed to it (a status revoke, a reaction to a status)
/// belong to the Updates feed, never to a chat: no chat row may
/// ever be built from this address.
pub(crate) const STATUS_BROADCAST_JID: &str = "status@broadcast";

/// Whether a display string is a raw identifier rather than a
/// name: nine or more characters, all digits. The same predicate
/// guards renames in the backend and the archive; identity
/// healing may only replace one of these, never a real name.
pub(crate) fn is_raw_identifier(text: &str) -> bool {
    text.len() >= 9 && text.bytes().all(|b| b.is_ascii_digit())
}

/// Whether a display string is a formatted phone number (a plus
/// sign and at least nine digits, nothing else): the state a
/// direct chat's title sits in after the LID-to-phone heal,
/// before any name is known. It is a healable state like a raw
/// identifier, not a resolved name: when the contact's name
/// arrives later, it must still be able to replace the number.
pub(crate) fn is_formatted_phone(text: &str) -> bool {
    text.len() >= 10 && text.starts_with('+') && text[1..].bytes().all(|b| b.is_ascii_digit())
}

/// Whether a backend error message reports a failed send. The
/// backend's send paths word their failures so ("Sending
/// failed:", "Media sending failed:", "Media sending timed
/// out", "Sending the location failed:", "Sending the contact
/// failed:"); every other error leaves in-flight sends alone,
/// because it says nothing about them.
pub(crate) fn is_send_failure(message: &str) -> bool {
    message.starts_with("Sending failed:")
        || message.starts_with("Media sending failed:")
        || message.starts_with("Media sending timed out")
        || message.starts_with("Sending the location failed:")
        || message.starts_with("Sending the contact failed:")
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
    } else if m.contains("connection closed")
        || m.contains("connection reset")
        || m.contains("broken pipe")
        || m.contains("stream closed")
    {
        "The connection dropped. Check your internet and try again.".to_owned()
    } else if m.contains("not authorized")
        || m.contains("forbidden")
        || m.contains("401")
        || m.contains("403")
    {
        "WhatsApp refused that action for this account.".to_owned()
    } else if m.contains("rate") && m.contains("limit") {
        "WhatsApp is rate limiting this account. Wait a while and try again.".to_owned()
    } else if m.contains("no call is in progress") || m.contains("no incoming call") {
        "There is no call in progress.".to_owned()
    } else if m.contains("not a group admin") || m.contains("admin") && m.contains("required") {
        "That action needs a group admin.".to_owned()
    } else if m.contains("item-not-found") || m.contains("not found") && m.contains("group") {
        "That group could not be found. It may have been deleted or the link revoked.".to_owned()
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
        0 => crate::i18n::t("time.today"),
        -1 => crate::i18n::t("time.yesterday"),
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
        // Broadcast lists render with the direct-chat row
        // shape; the core model has no broadcast kind (P2-06).
        LiveChatKind::Broadcast => ChatKind::Dm,
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

/// Builds the command that hands one archived event's creation
/// secret back to the backend, or `None` when the message is not
/// an event whose secret and creator were both kept. The event
/// counterpart of [`poll_secret_registration`], with the same
/// contract: an event from an earlier session needs this before
/// it can be answered or have its incoming responses decrypted
/// in the new one.
fn event_secret_registration(msg: &LiveMessage) -> Option<BackendCommand> {
    let secret = msg.event_secret.as_ref()?;
    let creator = msg.event_creator.as_ref()?;
    if !matches!(msg.kind, backend::LiveMessageKind::Event { .. }) {
        return None;
    }
    Some(BackendCommand::RegisterEventSecret {
        chat_jid: msg.chat_jid.clone(),
        message_id: msg.id.clone(),
        creator_jid: creator.clone(),
        secret: <redacted>
    })
}

/// Converts a backend event response into the core model's.
pub(crate) fn to_rsvp(rsvp: backend::EventRsvp) -> core::model::EventRsvp {
    match rsvp {
        backend::EventRsvp::Going => core::model::EventRsvp::Going,
        backend::EventRsvp::Maybe => core::model::EventRsvp::Maybe,
        backend::EventRsvp::NotGoing => core::model::EventRsvp::NotGoing,
    }
}

/// Converts a core model event response into the backend's.
pub(crate) fn rsvp_to_backend(rsvp: core::model::EventRsvp) -> backend::EventRsvp {
    match rsvp {
        core::model::EventRsvp::Going => backend::EventRsvp::Going,
        core::model::EventRsvp::Maybe => backend::EventRsvp::Maybe,
        core::model::EventRsvp::NotGoing => backend::EventRsvp::NotGoing,
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
        backend::LiveMessageKind::Event {
            name,
            description,
            start_time,
            end_time,
            location,
            join_link,
            scheduled_call,
            extra_guests,
            canceled,
            my_rsvp,
            going,
            maybe,
            not_going,
        } => MessageKind::Event {
            name: name.clone(),
            description: description.clone(),
            start_time: *start_time,
            end_time: *end_time,
            location: location.clone(),
            join_link: join_link.clone(),
            scheduled_call: *scheduled_call,
            extra_guests: *extra_guests,
            canceled: *canceled,
            my_rsvp: my_rsvp.map(to_rsvp),
            going: *going,
            maybe: *maybe,
            not_going: *not_going,
        },
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
                MediaKind::Audio => MessageKind::Audio {
                    secs: *seconds,
                    // The caption slot of an audio file carries
                    // its format label (the payload has no file
                    // name); an empty one is just an audio file.
                    name: if caption.is_empty() {
                        "Audio file".to_owned()
                    } else {
                        caption.clone()
                    },
                    size_bytes: *size_bytes,
                },
                MediaKind::Document => MessageKind::Document {
                    // The name comes from the message's own
                    // document field at the message-level
                    // conversion; the kind alone falls back to
                    // the bare label.
                    name: "Document".to_owned(),
                    size_bytes: *size_bytes,
                    page_count: None,
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

/// The backend delivery state for a core delivery value (the
/// receipt handler's conversions run the other way).
fn backend_delivery(d: Delivery) -> backend::DeliveryState {
    match d {
        // The backend state has no sending or failed rung: both
        // sit at the Sent floor (a failed send never advances).
        Delivery::Sending | Delivery::Failed | Delivery::Sent => backend::DeliveryState::Sent,
        Delivery::Delivered => backend::DeliveryState::Delivered,
        Delivery::Read => backend::DeliveryState::Read,
    }
}

pub(crate) fn to_message_with_flags(live: &LiveMessage, flags: i64) -> Message {
    let mut kind = to_kind(&live.kind);
    if let MessageKind::Voice { waveform, .. } = &mut kind {
        *waveform = voice_waveform_bars(live.voice_waveform.as_deref());
    }
    if let MessageKind::Document { name, page_count, .. } = &mut kind {
        // The document's name and page count live in the
        // message's own document fields, not the kind's caption
        // slot (that slot holds only the sender's caption).
        if let Some(doc_name) = live.document_name.as_ref().filter(|n| !n.is_empty()) {
            *name = doc_name.clone();
        }
        *page_count = live.document_page_count;
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
        delivery: if live.from_me {
            // Delivery is synced state, never a display floor:
            // the carried state wins, and a state the wire did
            // not state reads as Sent (the message exists, so
            // the server accepted it), never as Delivered.
            Some(match live.delivery {
                Some(backend::DeliveryState::Read) => Delivery::Read,
                Some(backend::DeliveryState::Delivered) => Delivery::Delivered,
                Some(backend::DeliveryState::Sent) | None => Delivery::Sent,
            })
        } else {
            None
        },
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
            // The preview's own picture, when the sender's
            // client embedded one: the card paints the real
            // bytes (the stream decodes them from the session's
            // preview map); the treatment is the small thumb.
            image: if p.thumbnail.is_some() {
                core::model::PreviewImage::Thumb
            } else {
                core::model::PreviewImage::None
            },
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
        parent_jid: None,
        is_parent: false,
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
