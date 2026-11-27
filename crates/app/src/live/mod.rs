//! Live mode: the bridge between the `backend` crate and the UI.
//!
//! In live mode the chat list and conversation are fed by backend
//! events (and, on start, by the `store` archive) instead of the demo
//! dataset. Everything protocol-shaped stays in this module: JID →
//! numeric ID mapping, unix timestamps → display times, and the
//! write-through of every upsert into the archive.

mod actions;
mod business;
mod changed;
mod events;
mod media;
mod messages;
mod profile;
#[cfg(test)]
mod replay;
mod session;
mod zip;

pub(crate) use business::{format_money, label_color, quick_reply_matches};
pub(crate) use session::note_window_visibility;
pub(crate) use zip::{ZipGather, ZipItem, ZipItemState, ZipPayload};

use std::collections::{HashMap, VecDeque};

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

/// During an active sync episode the deferred chat-list sort
/// runs at most once per this many batch flushes (qc1644): a
/// first link files thousands of upserts, and sorting the whole
/// list after every 256-event batch made the flush itself the
/// frame cost. The order between sorts may lag, never the final
/// order: a sort is forced when the episode completes, and
/// outside a sync episode every flush sorts as before.
pub(crate) const SYNC_SORT_FLUSH_INTERVAL: u32 = 16;

/// The chat-list sort decision for one batch flush, as a pure
/// function so the cadence is testable without a session:
/// returns whether this flush sorts, and the flushes-since-sort
/// counter to carry into the next flush. A forced sort (a sync
/// completion) and any flush outside a sync episode always sort
/// and reset the counter; inside an episode the counter advances
/// and only every [`SYNC_SORT_FLUSH_INTERVAL`]-th flush sorts.
pub(crate) fn chat_sort_cadence(
    syncing: bool,
    force: bool,
    flushes_since_sort: u32,
) -> (bool, u32) {
    if force || !syncing {
        return (true, 0);
    }
    let next = flushes_since_sort + 1;
    if next >= SYNC_SORT_FLUSH_INTERVAL {
        (true, 0)
    } else {
        (false, next)
    }
}

/// One step of the batch flush, in the order the flush runs
/// them (qc1644). The flush is resumable: it advances this
/// cursor as it works and stops between work units when the
/// frame budget is spent, so a flush that does not finish in
/// one frame continues at the cursor on the next, in the same
/// order the old single-pass flush ran every step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FlushStep {
    /// Sort each touched chat's messages and rebuild its
    /// position index.
    #[default]
    SortMessages,
    /// Write the batch's merged chat rows.
    ChatUpserts,
    /// Apply the batch's favorite flags.
    Favorites,
    /// Write the batch's message upserts.
    StoreWrites,
    /// Write the batch's delivery advances.
    DeliveryWrites,
    /// Write the batch's poll tallies.
    PollTallies,
    /// Write the batch's event tallies.
    EventTallies,
    /// Write the rows of chats whose unread badge changed.
    ChatRows,
    /// Apply the batch's deferred message-row updates.
    DeferredWrites,
    /// Sort the chat list (subject to the sync cadence).
    SortChats,
    /// Rebuild the open conversation.
    Conversation,
    /// Nothing left: the flush is complete.
    Done,
}

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
    /// Chat rows a batch chat upsert merged, queued instead of
    /// written one commit per event (qc1634 H2: a first link
    /// delivers around a thousand). The flush writes them first,
    /// deduplicated per chat with the last queued row winning,
    /// so the archive sees the same final row the old immediate
    /// writes produced, in one transaction.
    pub(crate) chat_upserts: Vec<LiveChat>,
    /// Delivery advances a batch's receipts proved, queued
    /// instead of written one commit per protocol ID (qc1634
    /// H1). The flush writes them after the batch's message
    /// upserts, deduplicated per message with the strongest
    /// state winning, in one transaction.
    pub(crate) delivery_writes: Vec<(String, String, backend::DeliveryState)>,
    /// The strongest delivery state each message reached inside
    /// this batch, recorded where the advance happens (a message
    /// upsert's merged state, a receipt). The flush overlays it
    /// on the batch's recorded upsert copies, replacing the old
    /// full-mirror rescan that rebuilt this map from every
    /// message on every flush (qc1634 H3).
    pub(crate) delivery_overlay: HashMap<u64, backend::DeliveryState>,
    /// Favorite flags a batch's FavoritesSync set, queued
    /// instead of written during the batch: `set_chat_favorite`
    /// is an UPDATE against the chat row, and a row whose first
    /// upsert is still queued in this same batch does not exist
    /// yet when the sync event applies, so the immediate write
    /// silently landed nowhere. The flush applies these after
    /// the batch's chat upserts, in event order.
    pub(crate) favorite_writes: Vec<(String, bool)>,
    /// Store UPDATEs against message rows that an event in this
    /// batch produced, queued for the same reason as
    /// `favorite_writes`: a sender rename, an event edit, a
    /// media path or a video thumbnail that lands while the
    /// message's own upsert is still queued would UPDATE a row
    /// that does not exist yet, and the flush's upsert would
    /// then overwrite with the pre-update copy. The flush
    /// applies these last, after every upsert and delivery
    /// write, in event order.
    pub(crate) deferred_writes: Vec<DeferredStoreWrite>,
    /// The flush step the next `finish_event_batch` call resumes
    /// at (qc1644). While a flush is unfinished this cursor and
    /// the queues above hold its remaining work; event
    /// application never starts a newer batch until the cursor
    /// reaches `Done`, so a later batch can never overtake an
    /// unfinished flush.
    pub(crate) flush_step: FlushStep,
    /// True while a flush stopped mid-way on the frame budget
    /// and still holds work. The next frame's drain continues
    /// the flush before it applies any newer event.
    pub(crate) flush_pending: bool,
    /// A sort of the chat list is owed regardless of the sync
    /// cadence: set when a sync completion lands in this batch,
    /// because the order the list settles into at the end of an
    /// episode must be the fully sorted one.
    pub(crate) force_chat_sort: bool,
    /// Flushes that requested a chat-list sort since the sort
    /// last ran, carried across batches for the sync cadence
    /// (see [`chat_sort_cadence`]). This is cadence bookkeeping,
    /// not flush work: it survives the per-batch reset.
    pub(crate) flushes_since_sort: u32,
}

/// One store UPDATE against a message row, deferred to the
/// batch flush (see [`EventBatch::deferred_writes`]).
pub(crate) enum DeferredStoreWrite {
    /// `store.rename_sender`: a sender's stored labels take a
    /// resolved name.
    RenameSender {
        /// The sender whose labels are rewritten.
        sender_jid: String,
        /// The resolved display name.
        name: String,
    },
    /// `store.set_event_fields`: an event message's edited
    /// fields.
    SetEventFields {
        /// Chat the event message lives in.
        chat_jid: String,
        /// Protocol ID of the event message.
        message_id: String,
        /// Edited event name.
        name: String,
        /// Edited description.
        description: String,
        /// Edited start (unix seconds).
        start_time: i64,
        /// Edited end (unix seconds).
        end_time: i64,
        /// Edited location.
        location: String,
        /// Edited join link.
        join_link: String,
        /// Whether the event is a scheduled call.
        scheduled_call: bool,
        /// Whether extra guests are allowed.
        extra_guests: bool,
        /// Whether the event is canceled.
        canceled: bool,
    },
    /// `store.update_media_direct_path`: a media message's
    /// direct download path.
    MediaDirectPath {
        /// Chat the media message lives in.
        chat_jid: String,
        /// Protocol ID of the media message.
        message_id: String,
        /// The direct path the server reported.
        direct_path: String,
    },
    /// `store.set_video_thumbnail`: a video message's poster
    /// bytes.
    VideoThumbnail {
        /// Chat the video message lives in.
        chat_jid: String,
        /// Protocol ID of the video message.
        message_id: String,
        /// The poster image bytes.
        bytes: Vec<u8>,
    },
}

impl EventBatch {
    /// Records one delivery advance into the batch overlay,
    /// keeping the strongest state per message (qc1634 H3).
    pub(crate) fn record_delivery(&mut self, msg_id: u64, state: backend::DeliveryState) {
        let slot = self.delivery_overlay.entry(msg_id).or_insert(state);
        *slot = backend::DeliveryState::max_opt(Some(*slot), Some(state)).unwrap_or(state);
    }

    /// Whether any deferred side effect is still queued. The
    /// flush consults this before it may call itself done
    /// (qc1644): while a flush sits pending between frames the
    /// batch stays active, so a side path can queue fresh work
    /// into a step the cursor already passed, and that work must
    /// send the flush around again instead of being dropped
    /// with the reset.
    pub(crate) fn has_queued_work(&self) -> bool {
        !self.unsorted_chats.is_empty()
            || self.chats_dirty
            || self.conversation_dirty
            || !self.store_writes.is_empty()
            || !self.poll_tally_writes.is_empty()
            || !self.event_tally_writes.is_empty()
            || !self.chat_rows_dirty.is_empty()
            || !self.chat_upserts.is_empty()
            || !self.delivery_writes.is_empty()
            || !self.favorite_writes.is_empty()
            || !self.deferred_writes.is_empty()
            || self.force_chat_sort
    }
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

/// How many of a chat's newest messages the in-RAM mirror
/// retains per chat (qc1644). The archive store is the source
/// of truth and stays complete; the mirror is a working set.
/// 1500 is roughly ten times the stream's painted window plus
/// overscan, so ordinary scrolling, in-window search and the
/// dock never page in the common case, while a chat with tens
/// of thousands of archived messages no longer holds them all
/// in RAM for the whole session. Older rows come back on
/// demand through the store's paged read (see
/// `store::Store::messages_before` and
/// `App::load_older_from_store`).
pub(crate) const MIRROR_RETENTION: usize = 1500;

/// How many older messages one scroll-to-top page loads from
/// the archive. A page is a bounded store read plus a bounded
/// prepend; several pages of deep scrollback stay cheap
/// individually and the stream's anchor keeps the view still
/// across each prepend.
pub(crate) const MIRROR_PAGE: usize = 200;

/// Combined byte budget for the session's photo and video
/// thumbnail bytes. Wire thumbnails run roughly 5 to 30 KB, so
/// 96 MB keeps several thousand recent thumbnails resident: a
/// large account's working set of recent media stays in memory,
/// and only the long tail is evicted. An evicted thumbnail
/// behaves exactly like a cold cache miss (the archive still
/// holds the bytes for the next session, and a video tile falls
/// back to its poster fetch), so eviction costs a repaint path,
/// never correctness.
pub(crate) const MEDIA_THUMB_BUDGET_BYTES: usize = 96 * 1024 * 1024;

/// Byte budget for the session's staged profile picture bytes,
/// sized the same way: thousands of avatars stay resident and
/// the long tail is evicted into the `ensure_avatar` refetch
/// path, which is the same path a cold start takes.
pub(crate) const AVATAR_BUDGET_BYTES: usize = 32 * 1024 * 1024;

/// Which thumbnail map one entry of the combined media budget
/// lives in, so a single eviction order can span both maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaThumbKey {
    Photo(u64),
    Video(u64),
}

/// Byte accounting for one capped byte map (or, for the media
/// thumbnails, a pair of maps under one combined budget): the
/// total bytes held and the keys in insertion order, oldest
/// first. The maps themselves stay plain `HashMap`s and every
/// read site is unchanged; all writes route through the
/// `LiveSession` helpers, which keep this tracker in step.
///
/// The accounting is only as exact as its call sites: a removal
/// that bypassed the tracker would leave its bytes counted, so
/// the total can only over-count, which evicts earlier than
/// strictly needed and never leaks. Every insert and remove
/// site found in the codebase routes through the helpers, so
/// in practice the total is exact.
#[derive(Debug)]
pub(crate) struct ByteBudget<K, const LIMIT: usize> {
    total_bytes: usize,
    order: VecDeque<K>,
}

impl<K: Clone + PartialEq, const LIMIT: usize> ByteBudget<K, LIMIT> {
    pub(crate) fn new() -> Self {
        Self { total_bytes: 0, order: VecDeque::new() }
    }

    pub(crate) fn over_budget(&self) -> bool {
        self.total_bytes > LIMIT
    }

    /// The oldest-inserted key still tracked, if any.
    pub(crate) fn oldest(&self) -> Option<K> {
        self.order.front().cloned()
    }

    /// Records an insert of `new_bytes` under `key`;
    /// `replaced` is the byte count the key already held, when
    /// it held an entry. A re-insert refreshes the key's
    /// eviction position: its bytes are the freshest copy, so
    /// it should be evicted last, and the replaced bytes must
    /// not count twice. A fresh insert skips the order scan, so
    /// seeding thousands of entries at session start stays
    /// linear instead of quadratic.
    pub(crate) fn track_insert(&mut self, key: K, replaced: Option<usize>, new_bytes: usize) {
        if replaced.is_some() {
            self.order.retain(|held| held != &key);
        }
        self.order.push_back(key);
        self.total_bytes = self.total_bytes.saturating_sub(replaced.unwrap_or(0)) + new_bytes;
    }

    /// Records the removal of `removed_bytes` under `key`.
    pub(crate) fn track_remove(&mut self, key: &K, removed_bytes: usize) {
        self.order.retain(|held| held != key);
        self.total_bytes = self.total_bytes.saturating_sub(removed_bytes);
    }

    /// Records the eviction of the oldest key, whose map entry
    /// held `removed_bytes`. The caller obtained the key from
    /// [`Self::oldest`], so it sits at the front of the order
    /// and no scan is needed.
    pub(crate) fn track_eviction(&mut self, removed_bytes: usize) {
        self.order.pop_front();
        self.total_bytes = self.total_bytes.saturating_sub(removed_bytes);
    }
}

impl<K: Clone + PartialEq, const LIMIT: usize> Default for ByteBudget<K, LIMIT> {
    fn default() -> Self {
        Self::new()
    }
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
    /// section 14). The percentage the row may show is kept in
    /// `sync_percent` below under strict honesty rules: only a
    /// number the library actually reported, max-clamped within
    /// the episode, never extrapolated, and 100 reserved for a
    /// real completion event. Phases that report no number keep
    /// the row indeterminate.
    pub(crate) syncing: bool,
    /// When sync activity was last reported (a connect, a
    /// history chunk, an offline-sync preview). The quiet-window
    /// fallback clears `syncing` when this goes stale, because
    /// the library's completion event covers only the offline
    /// drain and a catch-up can end without it.
    pub(crate) sync_last_activity: Option<std::time::Instant>,
    /// Whether a catch-up sync has completed this session.
    pub(crate) synced_once: bool,
    /// The strongest history-sync progress stamp the library has
    /// reported for the current sync episode, when it reported
    /// one at all. The stamp is the phone's own per-chunk number
    /// with no monotonicity promise, so the session keeps the
    /// maximum and displays it as reported, never extrapolated;
    /// phases that report no number leave this `None` and the
    /// status row stays indeterminate. Set to 100 only by a real
    /// completion event. Reset when a new episode begins.
    pub(crate) sync_percent: Option<u32>,
    /// When the current sync episode truly completed. The status
    /// row shows its short completed state (with the 100 above)
    /// while this is fresh, then clears; a new progress report
    /// cancels the hold because a new episode has begun.
    pub(crate) sync_completed_at: Option<std::time::Instant>,
    /// The episode's reported progress has reached its ceiling
    /// (the max-clamped 99) and sync work is still being
    /// applied after it: the honest "finishing" phase, in which
    /// the remaining work is real but carries no number. Set by
    /// [`LiveSession::note_sync_work_applied`] (the drain notes
    /// every applied event) and by
    /// [`LiveSession::note_app_state_patch`]; cleared on sync
    /// completion, on sync interruption, and when a new episode
    /// begins. Never set from an extrapolated or invented
    /// progress value: it is a phase label, not a number.
    pub(crate) sync_finishing: bool,
    /// Chats whose unread badge a history upsert seeded from the
    /// payload's own count. Their history messages carry the
    /// already-counted marker and must not increment the badge a
    /// second time (the first-link double count, qc1634 claim 2).
    pub(crate) seeded_unread: std::collections::HashSet<u64>,
    /// Per-chat message ID to position index over `messages`,
    /// sparing message upserts the linear position scan that
    /// made history filing quadratic per chat (qc1634 H5). A
    /// validated cache only: every lookup confirms the cached
    /// position still holds the ID and falls back to a scan when
    /// it does not, so a stale entry costs time, never
    /// correctness. Rebuilt whenever a chat's vec is sorted
    /// after mutation; dropped when a vec is mutated anywhere
    /// else.
    pub(crate) msg_index: HashMap<u64, HashMap<u64, usize>>,
    /// Chats whose `msg_index` entry is known to map EVERY row
    /// of the chat's vec (built by a full rebuild and afterwards
    /// maintained by the push and replacement sites that record
    /// their rows). For those chats an index miss proves the
    /// message is absent, so a lookup for a genuinely new
    /// message is O(1) instead of a scan: without this, filing
    /// N new messages into one chat stayed quadratic in scans
    /// even with the index. Any structural mutation that does
    /// not rebuild (a purge, a chat delete, an expiry sweep)
    /// drops the chat from both maps together
    /// (`invalidate_msg_index`), and a partial index built by
    /// incremental inserts is never marked complete.
    pub(crate) msg_index_complete: std::collections::HashSet<u64>,
    /// Numeric chat ID → position in the app's chat list, the
    /// chat counterpart of [`Self::msg_index`]: the batch flush
    /// resolves a queued row's mirror title and builds unread
    /// rows through it instead of scanning the whole list per
    /// row (qc1644). A validated cache with the same discipline
    /// as the message index: a lookup counts only when the row
    /// at the cached position still carries the ID, and any
    /// doubt falls back to a scan, so a stale entry (a sort or
    /// a removal outside the event path rewrote the list) costs
    /// time, never correctness. Maintained on chat insert and
    /// rebuilt wherever the flush itself reorders or shrinks
    /// the list.
    pub(crate) chat_positions: HashMap<u64, usize>,
    /// Bumped whenever event application changes anything
    /// the chat-list filter reads (qc1644): a chat inserted or
    /// removed, or any filter-relevant field of a chat row
    /// changed (title, preview, preview sender, unread count,
    /// archived, locked, pinned, favorite, kind), or a chat's
    /// activity timestamp moved (the list order reads it), or
    /// a chat's verified state changed. Chat-list surfaces key
    /// their caches on it, so a roster change they did not
    /// witness directly still invalidates them. The invariant
    /// the caches rely on is the converse: if this number did
    /// not move, no filter input and no list position changed.
    /// Starts at 0 and only ever moves forward.
    pub(crate) roster_generation: u64,
    /// The ephemeral index (qc1644): expiry second to the
    /// (chat ID, message ID) pairs whose disappearing timer
    /// ends then. Only messages that carry an expiry are
    /// indexed, so the per-second expiry sweep consumes due
    /// buckets from here instead of scanning every message of
    /// every chat. Maintained at the mirror's message choke
    /// points (the upsert placement, `push_message`, the local
    /// echo push, the archive apply, the delete-for-me
    /// restore) and dropped from in `forget_live_message`,
    /// which every removal path funnels through. The sweep
    /// verifies each due pair against the mirror before
    /// acting, and a full reconciliation scan runs at most
    /// once a minute, so a missed registration can only delay
    /// an expiry, never lose it.
    pub(crate) ephemeral_index: std::collections::BTreeMap<u64, Vec<(u64, u64)>>,
    /// The chat archive (write-through). `None` if it failed to open.
    pub(crate) store: Option<store::Store>,
    /// The archive snapshot's staged apply, while one is in
    /// flight (see `session::PendingArchiveApply`): the snapshot
    /// has landed and its store is installed above, but its
    /// messages are still being seeded into the mirrors slice by
    /// slice across frames, so no single frame pays the whole
    /// archive. Backend event application holds until this
    /// clears, exactly as it held while the load itself was in
    /// flight, so queued events still apply only after the
    /// snapshot is fully installed, in their original order.
    pub(crate) pending_archive: Option<session::PendingArchiveApply>,
    /// Numeric chat ID → JID.
    pub(crate) jids: HashMap<u64, String>,
    /// The account's saved contacts (JID → saved name), as the
    /// backend's identity book reports them through
    /// `BackendEvent::SavedContacts`: each report replaces this
    /// map whole. The status audience and the privacy people
    /// pickers draw from it; it is never merged with the chat
    /// list, so a DM partner who is not a saved contact is not
    /// in it.
    pub(crate) saved_contacts: std::collections::HashMap<String, String>,
    /// Every known JID form of a saved contact (their
    /// canonical JID and each alternate form the backend
    /// reports) mapped to that contact's canonical JID, as
    /// reported through `BackendEvent::SavedContacts`: each
    /// report replaces this map whole, alongside
    /// `saved_contacts`. Lists recorded in another form (an
    /// exception list carrying a LID form, a DM chat addressed
    /// by LID) are canonicalized through this map before they
    /// are compared against the canonical saved set.
    pub(crate) saved_contact_forms: std::collections::HashMap<String, String>,
    /// JID forms a saved-contacts report retired (a contact's
    /// previous number, above all) mapped to the canonical
    /// key the person stands under in the current report and
    /// the report step that learned the edge (see
    /// [`Self::saved_contacts_report_step`]), learned by
    /// comparing successive reports in the SavedContacts
    /// handler: the old form leaves the form map while the
    /// person's stable alternate form persists under the new
    /// key, and that pair of facts is the only continuity
    /// evidence a number change leaves behind. Stored lists
    /// (the status privacy list) resolve through this map
    /// when the current form map no longer knows the form
    /// they name, so an entry recorded before the change
    /// follows the person instead of dangling. A form the
    /// current report knows is never answered from here: if
    /// a later report assigns the form to someone else, the
    /// current map wins by being consulted first (see
    /// [`Self::resolve_stored_jid`]).
    pub(crate) retired_contact_forms: std::collections::HashMap<String, (String, u64)>,
    /// The report step of the most recent saved-contacts
    /// report this session has applied: the SavedContacts
    /// handler advances it once per report, and every
    /// retired edge that report learns is stamped with it
    /// (see [`Self::retired_contact_forms`]). The stamp is
    /// the only record of whether one move followed another
    /// across reports or landed with it inside one report,
    /// and the heal walk reads it for exactly that
    /// distinction (see
    /// [`Self::resolve_stored_jid_follow_first`]).
    pub(crate) saved_contacts_report_step: u64,
    /// JID forms a saved-contacts report proved changed
    /// hands from an owner who left the saved set: the old
    /// entry carried a LID form and no LID form of it
    /// survives in the new report (a LID is never recycled,
    /// so its absence is the owner leaving, not moving),
    /// while the form itself stands under a different
    /// identified person's entry. A stored entry naming
    /// such a form was written for the owner who left; no
    /// follow can be learned for it (the owner stands
    /// nowhere in the new report), and resolving it through
    /// the current form map would retarget it to the form's
    /// new holder, so the stored-list heals drop it instead
    /// (see [`Self::heal_stored_entry`]). A mark is cleared
    /// when a later report learns a follow for the form,
    /// when the form leaves the current form map, or when
    /// the user records the form in a fresh editor write
    /// (the write names the form's current holder, so the
    /// form means them from then on).
    pub(crate) orphaned_contact_forms: std::collections::HashSet<String>,
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
    /// Byte-budget tracker for `photo_thumbs` and `video_thumbs`
    /// under their one combined budget; see [`ByteBudget`].
    /// Writes to those maps go through `put_photo_thumb`,
    /// `put_video_thumb` and the matching remove helpers, never
    /// directly, so this accounting cannot drift from the maps.
    pub(crate) media_thumb_budget: ByteBudget<MediaThumbKey, MEDIA_THUMB_BUDGET_BYTES>,
    /// Byte-budget tracker for `avatar_bytes`; see
    /// [`ByteBudget`]. Writes go through `put_avatar_bytes` and
    /// `remove_avatar_bytes`.
    pub(crate) avatar_byte_budget: ByteBudget<String, AVATAR_BUDGET_BYTES>,
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
    /// Chats whose mirrored window the user has extended past
    /// the retention cap this session (a scroll-to-top page, a
    /// full page-in for search, export or the profile dock, a
    /// locate of an older message). Trimming skips these chats:
    /// a window the user deliberately grew must not shrink
    /// under them mid-view. The cap re-applies to them at the
    /// next session's archive seed, which starts every chat
    /// from the retained window again.
    pub(crate) paged_chats: std::collections::HashSet<u64>,
    /// Chats whose mirrored window already reaches the
    /// archive's oldest row for that chat (qc1644): a store
    /// page came back short or empty, so scrolling to the top
    /// goes straight to the phone fetch (the section 22 path)
    /// instead of re-reading a page that cannot exist. Set
    /// only from a real store answer, never guessed.
    pub(crate) paged_to_start: std::collections::HashSet<u64>,
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

/// One in-flight deferred archive load (qc1636 Step 7, H7):
/// the worker thread reads the whole archive into a snapshot
/// (see `session::load_archive_snapshot`) and sends it back
/// with the store it read from; the poll on the UI thread
/// applies it (see `session::App::poll_archive_load`). The
/// generation stamps which live start the load belongs to, so
/// a snapshot from a session that has since ended can never
/// land in its successor.
pub(crate) struct PendingArchiveLoad {
    pub(crate) rx: std::sync::mpsc::Receiver<ArchiveLoad>,
    pub(crate) gen: u64,
    pub(crate) started: std::time::Instant,
}

/// The archive snapshot one load delivers: everything the old
/// synchronous startup load read from the store, in the order
/// it read it, plus the store itself (the session takes it
/// back when the snapshot applies; the store's single
/// connection is never duplicated, it is only away with the
/// worker while the load runs).
pub(crate) struct ArchiveLoad {
    pub(crate) store: store::Store,
    pub(crate) avatars: Vec<store::AvatarListing>,
    pub(crate) chats: Vec<backend::LiveChat>,
    /// Archived messages per chat, aligned by index with
    /// [`Self::chats`].
    pub(crate) messages: Vec<Vec<(backend::LiveMessage, i64)>>,
    pub(crate) call_records: Vec<backend::CallRecord>,
    /// Read failures collected on the way, in the wording the
    /// startup load has always reported them with; the apply
    /// surfaces them exactly once, as it did.
    pub(crate) problems: Vec<String>,
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

    /// Inserts one photo's thumbnail bytes, accounting them
    /// against the combined media thumbnail budget and evicting
    /// the oldest thumbnails (photo or video, whichever was
    /// inserted first) until the budget holds again. The only
    /// write path for `photo_thumbs`.
    pub(crate) fn put_photo_thumb(&mut self, id: u64, bytes: Vec<u8>) {
        let new_bytes = bytes.len();
        let replaced = self.photo_thumbs.insert(id, bytes).map(|old| old.len());
        self.media_thumb_budget.track_insert(MediaThumbKey::Photo(id), replaced, new_bytes);
        self.evict_media_thumbs_over_budget();
    }

    /// Inserts one video's thumbnail bytes under the same
    /// combined budget as [`Self::put_photo_thumb`]. The only
    /// write path for `video_thumbs`.
    pub(crate) fn put_video_thumb(&mut self, id: u64, bytes: Vec<u8>) {
        let new_bytes = bytes.len();
        let replaced = self.video_thumbs.insert(id, bytes).map(|old| old.len());
        self.media_thumb_budget.track_insert(MediaThumbKey::Video(id), replaced, new_bytes);
        self.evict_media_thumbs_over_budget();
    }

    /// Inserts one identity's staged profile picture bytes,
    /// accounting them against the avatar budget and evicting
    /// the oldest pictures until the budget holds again. The
    /// only write path for `avatar_bytes`.
    pub(crate) fn put_avatar_bytes(&mut self, jid: String, bytes: Vec<u8>) {
        let new_bytes = bytes.len();
        let replaced = self.avatar_bytes.insert(jid.clone(), bytes).map(|old| old.len());
        self.avatar_byte_budget.track_insert(jid, replaced, new_bytes);
        while self.avatar_byte_budget.over_budget() {
            let Some(oldest) = self.avatar_byte_budget.oldest() else { break };
            let removed = self.avatar_bytes.remove(&oldest).map_or(0, |old| old.len());
            self.avatar_byte_budget.track_eviction(removed);
        }
    }

    /// Evicts oldest-inserted media thumbnails until the
    /// combined budget holds. Only the byte entry goes: the
    /// message's references and stated dimensions stay, because
    /// they are the fallback that makes an evicted thumbnail a
    /// plain cold miss (a video re-asks for its poster through
    /// `poster_refs`, a photo tile paints its placeholder and
    /// full-bytes fetch as it does for a thumbnail-less photo),
    /// and the decoded texture caches are capped separately.
    /// An entry larger than the whole budget evicts itself, so
    /// it is simply never cached. Each pass removes one tracked
    /// key, so the loop always terminates even if a key's map
    /// entry is already gone.
    fn evict_media_thumbs_over_budget(&mut self) {
        while self.media_thumb_budget.over_budget() {
            let Some(key) = self.media_thumb_budget.oldest() else { break };
            let removed = match key {
                MediaThumbKey::Photo(id) => self.photo_thumbs.remove(&id),
                MediaThumbKey::Video(id) => self.video_thumbs.remove(&id),
            }
            .map_or(0, |old| old.len());
            self.media_thumb_budget.track_eviction(removed);
        }
    }

    /// Removes one photo's thumbnail bytes and drops their
    /// accounting. The remove counterpart of
    /// [`Self::put_photo_thumb`].
    pub(crate) fn remove_photo_thumb(&mut self, id: u64) {
        if let Some(old) = self.photo_thumbs.remove(&id) {
            self.media_thumb_budget.track_remove(&MediaThumbKey::Photo(id), old.len());
        }
    }

    /// Removes one video's thumbnail bytes and drops their
    /// accounting. The remove counterpart of
    /// [`Self::put_video_thumb`].
    pub(crate) fn remove_video_thumb(&mut self, id: u64) {
        if let Some(old) = self.video_thumbs.remove(&id) {
            self.media_thumb_budget.track_remove(&MediaThumbKey::Video(id), old.len());
        }
    }

    /// Removes one identity's staged picture bytes and drops
    /// their accounting. The remove counterpart of
    /// [`Self::put_avatar_bytes`]; an evicted or dropped picture
    /// is re-fetched by `ensure_avatar`, exactly as on a cold
    /// start, because neither the negative cache nor the
    /// in-flight mark is set for it.
    pub(crate) fn remove_avatar_bytes(&mut self, jid: &str) {
        if let Some(old) = self.avatar_bytes.remove(jid) {
            let removed = old.len();
            self.avatar_byte_budget.track_remove(&jid.to_owned(), removed);
        }
    }

    /// The position of one message in its chat's vec, through
    /// the validated position index (qc1634 H5): a cached
    /// position counts only when the row there still carries
    /// the ID; on any doubt the vec is scanned, so the answer
    /// is always the scan's answer, just usually faster. A scan
    /// that finds the row corrects only that one index entry:
    /// rebuilding the whole index per miss made filing a burst
    /// of genuinely new messages quadratic again (each new
    /// message missed, scanned, and rebuilt), which is why the
    /// index is otherwise maintained incrementally (push sites
    /// record their row; sort sites rebuild once per sort).
    pub(crate) fn message_position(&mut self, chat_id: u64, msg_id: u64) -> Option<usize> {
        let entry = self.messages.get(&chat_id)?;
        if let Some(pos) =
            self.msg_index.get(&chat_id).and_then(|index| index.get(&msg_id)).copied()
        {
            if entry.get(pos).is_some_and(|(_, m)| m.id == msg_id) {
                return Some(pos);
            }
            // A cached position that fails validation is a
            // bookkeeping bug somewhere: the scan below decides,
            // never the stale entry.
        } else if self.msg_index.contains_key(&chat_id)
            && self.msg_index_complete.contains(&chat_id)
        {
            // A complete index that lacks the ID proves the row
            // is absent (the index maps every row of this vec):
            // a genuinely new message costs one hash lookup, not
            // a scan of the whole chat.
            return None;
        }
        let found = entry.iter().position(|(_, m)| m.id == msg_id);
        if let Some(pos) = found {
            self.msg_index.entry(chat_id).or_default().insert(msg_id, pos);
        }
        found
    }

    /// Rebuilds one chat's position index from its whole vec
    /// and marks it complete: the only operation that may set
    /// the completeness mark, called where a vec's final order
    /// for now is known (a sort, the archive load, a restore).
    pub(crate) fn rebuild_msg_index(&mut self, chat_id: u64) {
        if let Some(entry) = self.messages.get(&chat_id) {
            let index: HashMap<u64, usize> =
                entry.iter().enumerate().map(|(pos, (_, m))| (m.id, pos)).collect();
            self.msg_index.insert(chat_id, index);
            self.msg_index_complete.insert(chat_id);
        }
    }

    /// Moves one chat's index entry from an old message ID to
    /// a new one at the same position, in step with an in-place
    /// ID bind (a local echo taking its protocol ID). The row
    /// count does not change, so a complete index stays
    /// truthfully complete; leaving the entry on the old ID
    /// would make the completeness mark lie about both IDs,
    /// and `message_position` would then prove a false absence
    /// for the protocol ID (qc1642 fix 1).
    pub(crate) fn reindex_message_id(
        &mut self,
        chat_id: u64,
        pos: usize,
        old_id: u64,
        new_id: u64,
    ) {
        if let Some(index) = self.msg_index.get_mut(&chat_id) {
            index.remove(&old_id);
            index.insert(new_id, pos);
        }
    }

    /// Appends one row to a chat's vec and records it in the
    /// position index at the new last position, the push
    /// discipline of `apply_message_upsert` as one operation
    /// for the append sites outside it (local echoes, system
    /// notices). Appending moves no other row, so a complete
    /// index stays truthfully complete (qc1642 fix 2). The
    /// caller must push an ID that is unique in the chat: the
    /// index maps one position per ID, and the echo bands the
    /// callers draw from are monotonic counters, so no two
    /// live rows share one.
    pub(crate) fn push_message(&mut self, chat_id: u64, ts: i64, msg: Message) {
        let msg_id = msg.id;
        let expires_at = msg.expires_at;
        let entry = self.messages.entry(chat_id).or_default();
        entry.push((ts, msg));
        let pos = entry.len() - 1;
        self.msg_index.entry(chat_id).or_default().insert(msg_id, pos);
        self.note_ephemeral(chat_id, msg_id, 0, expires_at);
        // The push grew the vec: the retention window applies
        // here like at every mirror growth point (qc1644).
        self.trim_chat_window(chat_id);
    }

    /// Registers one message's expiry in the ephemeral
    /// index, moving its entry when the expiry changed: the
    /// old bucket is named by the caller (the mirror row being
    /// replaced), so the hot upsert path never scans. An
    /// unchanged expiry keeps the existing entry; a zero new
    /// expiry registers nothing.
    pub(crate) fn note_ephemeral(
        &mut self,
        chat_id: u64,
        msg_id: u64,
        old_expires_at: u64,
        new_expires_at: u64,
    ) {
        if old_expires_at != 0 && old_expires_at != new_expires_at {
            self.drop_ephemeral_entry(chat_id, msg_id, old_expires_at);
        }
        if new_expires_at != 0 && old_expires_at != new_expires_at {
            self.ephemeral_index.entry(new_expires_at).or_default().push((chat_id, msg_id));
        }
    }

    /// Registers one message's expiry without disturbing any
    /// existing entry, for rows that are new to the mirror
    /// (the archive apply's seeded rows, a restored
    /// delete-for-me, an un-keep): a duplicate pair is not
    /// added twice.
    pub(crate) fn seed_ephemeral(&mut self, chat_id: u64, msg_id: u64, expires_at: u64) {
        if expires_at == 0 {
            return;
        }
        let bucket = self.ephemeral_index.entry(expires_at).or_default();
        if !bucket.contains(&(chat_id, msg_id)) {
            bucket.push((chat_id, msg_id));
        }
    }

    /// Removes one message's entry from the bucket its expiry
    /// names, pruning the bucket when it empties.
    pub(crate) fn drop_ephemeral_entry(&mut self, chat_id: u64, msg_id: u64, expires_at: u64) {
        if expires_at == 0 {
            return;
        }
        if let Some(bucket) = self.ephemeral_index.get_mut(&expires_at) {
            bucket.retain(|&pair| pair != (chat_id, msg_id));
            if bucket.is_empty() {
                self.ephemeral_index.remove(&expires_at);
            }
        }
    }

    /// Removes every entry naming one message, wherever it
    /// sits: the removal paths' version, which does not know
    /// the message's expiry (the row may already be gone).
    /// The index holds only ephemeral messages, so the scan
    /// is bounded by their count, not the mirror's.
    pub(crate) fn drop_ephemeral(&mut self, chat_id: u64, msg_id: u64) {
        self.ephemeral_index.retain(|_, bucket| {
            bucket.retain(|&pair| pair != (chat_id, msg_id));
            !bucket.is_empty()
        });
    }

    /// Takes every indexed pair whose expiry has arrived at
    /// `now`, leaving later buckets in place. The caller
    /// verifies each pair against the mirror before acting:
    /// an entry can outlive its message (a keep that landed
    /// after registration), and verification is what makes a
    /// stale entry harmless.
    pub(crate) fn take_due_ephemeral(&mut self, now: u64) -> Vec<(u64, u64)> {
        let rest = self.ephemeral_index.split_off(&now.saturating_add(1));
        let due = std::mem::replace(&mut self.ephemeral_index, rest);
        due.into_values().flatten().collect()
    }

    /// Drops one chat's position index and its completeness
    /// mark together, after a structural mutation the index
    /// does not track (a purge, a delete, an expiry sweep).
    /// Lookups for the chat fall back to scans until the next
    /// rebuild.
    pub(crate) fn invalidate_msg_index(&mut self, chat_id: u64) {
        self.msg_index.remove(&chat_id);
        self.msg_index_complete.remove(&chat_id);
    }

    /// Trims one chat's mirrored vec to the retention window
    /// (see [`MIRROR_RETENTION`]): the oldest rows beyond the
    /// cap leave the RAM copy only. The archive store already
    /// holds every row (writes are write-through, and this runs
    /// after the batch flush's store writes in the flush order,
    /// or outside a batch entirely), so a trimmed row is a
    /// paged read away, never lost. Called at the mirror's
    /// growth choke points: the push sites, the upsert
    /// placement's sort, the batch flush's sort step, and the
    /// archive seed's install.
    ///
    /// The position index is rebuilt over the retained vec, so
    /// its completeness mark stays truthful (it covers exactly
    /// the current vec, qc1642 semantics). The drained rows'
    /// per-message session state goes with them: ephemeral
    /// index entries (the sweep verifies against the mirror,
    /// but a dead entry would still fire a lookup per due
    /// bucket), media references and thumbnails, mentions,
    /// ballots, flags and the view-once and GIF sets. App-level
    /// state keyed by message ID (protocol IDs, sender JIDs,
    /// the receipt log) deliberately stays: its consumers are
    /// all driven by mirrored rows or verify against the
    /// mirror first, and a late receipt for a trimmed message
    /// must still resolve its protocol ID (the receipt arm's
    /// echo-bind fallback is guarded on exactly this, see
    /// `events.rs`). Chats in [`Self::paged_chats`] are never
    /// trimmed: the user extended their window on purpose.
    pub(crate) fn trim_chat_window(&mut self, chat_id: u64) {
        if self.paged_chats.contains(&chat_id) {
            return;
        }
        let drained: Vec<(i64, Message)> = {
            let Some(entry) = self.messages.get_mut(&chat_id) else {
                return;
            };
            if entry.len() <= MIRROR_RETENTION {
                return;
            }
            let excess = entry.len() - MIRROR_RETENTION;
            entry.drain(..excess).collect()
        };
        if drained.is_empty() {
            return;
        }
        let drained_ids: std::collections::HashSet<u64> =
            drained.iter().map(|(_, m)| m.id).collect();
        for (_, m) in &drained {
            // The by-message removal, not the by-expiry one:
            // a trimmed message must leave no entry behind
            // even if its row's recorded expiry and the index
            // ever drifted apart.
            self.drop_ephemeral(chat_id, m.id);
            self.photo_refs.remove(&m.id);
            self.voice_refs.remove(&m.id);
            self.video_refs.remove(&m.id);
            self.remove_video_thumb(m.id);
            self.poster_refs.remove(&m.id);
            self.video_dims.remove(&m.id);
            self.sticker_refs.remove(&m.id);
            self.sticker_anim.remove(&m.id);
            self.sticker_thumbs.remove(&m.id);
            self.document_refs.remove(&m.id);
            self.document_thumbs.remove(&m.id);
            self.location_thumbs.remove(&m.id);
            self.preview_thumbs.remove(&m.id);
            self.remove_photo_thumb(m.id);
            self.message_mentions.remove(&m.id);
            self.poll_ballots.remove(&m.id);
            self.event_ballots.remove(&m.id);
            self.flags.remove(&m.id);
            self.view_once_ids.remove(&m.id);
            self.gif_videos.remove(&m.id);
        }
        self.server_ids
            .retain(|(chat, _), msg_id| *chat != chat_id || !drained_ids.contains(msg_id));
        // The vec changed shape at the front: rebuild the
        // position index over exactly the retained rows.
        self.rebuild_msg_index(chat_id);
    }

    /// Records one roster change (see
    /// [`Self::roster_generation`]): event application changed
    /// a filter input or a list position. Every mutation path
    /// that writes a filter-relevant chat field or a chat's
    /// activity timestamp must call this when the value
    /// actually moves; a write that restates the current value
    /// must not, so identical re-deliveries stay free.
    pub(crate) fn note_roster_change(&mut self) {
        self.roster_generation = self.roster_generation.wrapping_add(1);
    }

    /// Resolves a JID recorded in a stored list (the status
    /// privacy list above all) to the canonical person key
    /// it names now. The current report's form map answers
    /// first. A form that map no longer knows may be one a
    /// re-point retired, in which case the retired-form
    /// chain leads to the person, followed transitively (a
    /// second number change retires the first target the
    /// same way) until a form the current map knows or the
    /// chain's end. A form neither map knows passes through
    /// unchanged: it names nobody this device can identify,
    /// which is the foreign-written entry case by design.
    pub(crate) fn resolve_stored_jid(&self, jid: &str) -> String {
        let mut current = jid;
        let mut seen = std::collections::HashSet::new();
        loop {
            if let Some(canonical) = self.saved_contact_forms.get(current) {
                return canonical.clone();
            }
            if !seen.insert(current) {
                break;
            }
            match self.retired_contact_forms.get(current) {
                Some((next, _step)) => current = next.as_str(),
                None => break,
            }
        }
        current.to_owned()
    }

    /// Resolves a stored-list entry the way a heal at
    /// data-landing time must: the retired-form chain is
    /// consulted BEFORE the current form map. The two
    /// resolutions differ only for a form the current map
    /// knows that also carries a retired edge, and by the
    /// learning discipline that combination exists only in
    /// the window after the report that learned the edge: a
    /// form that changed hands inside one report step (its
    /// old owner re-pointed and the form was re-saved under
    /// a new contact in the same step) is in the new map
    /// under its new holder, with a fresh edge to its old
    /// owner. An entry recorded before that step names the
    /// old owner, so a heal reads it through the edge; an
    /// entry written after the step names the current
    /// holder, which is why the post-time and editor paths
    /// keep [`Self::resolve_stored_jid`]'s current-map-first
    /// order and only the heals (the report migration and
    /// the account-record adoption, the two places an
    /// entry's vintage is known to predate the report) use
    /// this one.
    ///
    /// The chain is followed transitively under the step
    /// discipline each edge is stamped with (see
    /// [`Self::retired_contact_forms`]): the first edge is
    /// unconstrained, and an edge out of a node the walk
    /// arrived at is followed only when its step is strictly
    /// later than the step of the edge that arrived there.
    /// A same-step or older edge ends the walk at the
    /// arrived-at node, which the closing lookup then
    /// settles through the current form map. The step is
    /// what separates the two ways an edge can sit out of a
    /// key the walk lands on. Learned in the SAME step as
    /// the arriving edge, it is a different person's move:
    /// a follow target is by construction a canonical key
    /// of the report that learned the arriving edge, so the
    /// walk has landed on the entry's person's current key,
    /// and an edge out of that key learned with the same
    /// report is its previous holder vacating it (the
    /// number handoff); following it would retarget the
    /// entry one person too far. A same-step swap is the
    /// same shape twice over, and the walk ends at the
    /// first key reached, the entry's own person's new key.
    /// Learned in a LATER step, the edge speaks for the
    /// person who stood under that key once the arriving
    /// move landed: they moved again in a subsequent
    /// report, and a heal that runs later still (an account
    /// record first fetched after both moves) follows both
    /// edges to reach them, even when the intermediate key
    /// has since been recycled to another contact, whose
    /// claim on it the current map records but who did not
    /// make the later move the edge records. The cycle
    /// guard remains only as a net for shapes the learning
    /// discipline does not produce.
    pub(crate) fn resolve_stored_jid_follow_first(&self, jid: &str) -> String {
        let mut current = jid;
        let mut seen = std::collections::HashSet::new();
        let mut arrived_step: Option<u64> = None;
        loop {
            if !seen.insert(current) {
                break;
            }
            match self.retired_contact_forms.get(current) {
                Some((next, step)) => {
                    if arrived_step.is_some_and(|arrived| *step <= arrived) {
                        break;
                    }
                    arrived_step = Some(*step);
                    current = next.as_str();
                }
                None => break,
            }
        }
        if let Some(canonical) = self.saved_contact_forms.get(current) {
            return canonical.clone();
        }
        current.to_owned()
    }

    /// Carries one stored-list entry across a saved-contacts
    /// report (or an account-record adoption): the healed
    /// form of the entry, or `None` when the entry names
    /// nobody any more and must drop out of the list. An
    /// entry carrying a retired edge follows its person
    /// through [`Self::resolve_stored_jid_follow_first`]. An
    /// entry naming an orphaned form (see
    /// [`Self::orphaned_contact_forms`]) drops: its person
    /// left the saved set, the form belongs to someone else
    /// now, and no follow exists, so keeping the entry would
    /// retarget it to the form's new holder. Any other entry
    /// the current report knows is left exactly as recorded,
    /// and one no map knows resolves through the post-time
    /// resolution (the retired chain for a form that retired
    /// in an earlier step, else the entry itself, the
    /// foreign-written case).
    pub(crate) fn heal_stored_entry(&self, jid: &str) -> Option<String> {
        if self.retired_contact_forms.contains_key(jid) {
            return Some(self.resolve_stored_jid_follow_first(jid));
        }
        if self.orphaned_contact_forms.contains(jid) {
            return None;
        }
        if self.saved_contact_forms.contains_key(jid) {
            return Some(jid.to_owned());
        }
        Some(self.resolve_stored_jid(jid))
    }

    /// Records one unit of sync work being applied: any event
    /// the drain applies while the episode's reported progress
    /// sits at its ceiling. The work still landing then (a
    /// tail of receipts, messages and history chunks, not just
    /// app-state patches) is the finishing phase: the flag the
    /// status row reads for its "finishing" label. Work applied
    /// before the ceiling, or outside an episode, changes
    /// nothing. The flag is a phase label only: no progress
    /// value is extrapolated or invented here.
    pub(crate) fn note_sync_work_applied(&mut self) {
        if self.syncing && self.sync_percent == Some(99) {
            self.sync_finishing = true;
        }
    }

    /// Records one app-state patch event (an archive, pin, mute,
    /// star or read-state change) being applied: the patch arms'
    /// share of [`Self::note_sync_work_applied`], kept as its
    /// own entry point because a patch can also be applied
    /// outside the drain's per-event note.
    pub(crate) fn note_app_state_patch(&mut self) {
        self.note_sync_work_applied();
    }

    /// The position of one chat in the app's chat list, through
    /// the validated position map (see [`Self::chat_positions`]):
    /// a cached position counts only when the row there still
    /// carries the ID; on any doubt the list is scanned and the
    /// map repaired from the scan, so the answer is always the
    /// scan's answer, just usually faster.
    pub(crate) fn chat_position(&mut self, chats: &[Chat], chat_id: u64) -> Option<usize> {
        if let Some(pos) = self.chat_positions.get(&chat_id).copied() {
            if chats.get(pos).is_some_and(|c| c.id == chat_id) {
                return Some(pos);
            }
        }
        let found = chats.iter().position(|c| c.id == chat_id);
        match found {
            Some(pos) => {
                self.chat_positions.insert(chat_id, pos);
            }
            None => {
                self.chat_positions.remove(&chat_id);
            }
        }
        found
    }

    /// Rebuilds the chat position map from the whole list, at
    /// the points where the list's order is final for now (the
    /// flush's own sort, a removal the event path applied):
    /// every position the map then serves is exact until the
    /// next reorder.
    pub(crate) fn rebuild_chat_positions(&mut self, chats: &[Chat]) {
        self.chat_positions = chats.iter().enumerate().map(|(pos, c)| (c.id, pos)).collect();
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

/// Whether a display string is the masked form of an identifier
/// (see [`masked_identifier`]): an optional `+`, three digits,
/// the bullet separator and the last four digits. Since the
/// backend started masking its unresolved fallbacks (qc1635), a
/// masked string is a placeholder exactly like the raw form:
/// [`is_unresolved_identifier`] is the predicate the healing
/// guards actually mean, and it accepts both.
pub(crate) fn is_masked_identifier(text: &str) -> bool {
    let digits = text.strip_prefix('+').unwrap_or(text);
    let Some((head, tail)) = digits.split_once(" ••• ") else {
        return false;
    };
    head.len() == 3
        && head.bytes().all(|b| b.is_ascii_digit())
        && tail.len() == 4
        && tail.bytes().all(|b| b.is_ascii_digit())
}

/// Whether a display string is an unresolved identifier in
/// either form, raw or masked: not a name, and a state identity
/// healing may replace.
pub(crate) fn is_unresolved_identifier(text: &str) -> bool {
    is_raw_identifier(text) || is_masked_identifier(text)
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

/// The locale key for a chat send failure's notice text,
/// chosen from the failure kind alone. The stage deliberately
/// does not refine the wording: the data cannot support a more
/// specific claim (a timeout while preparing the recipient's
/// addressing reads exactly like a timeout while writing, and
/// naming a step the user cannot act on would be a guessed
/// diagnostic), so every stage of one kind shares its sentence.
pub(crate) fn send_failure_key(kind: backend::SendFailureKind) -> &'static str {
    match kind {
        backend::SendFailureKind::Timeout => "notice.send_failed_timeout",
        backend::SendFailureKind::Offline => "notice.send_failed_offline",
        backend::SendFailureKind::Rejected => "notice.send_failed_rejected",
        backend::SendFailureKind::Other => "notice.send_failed_generic",
    }
}

/// The localized notice text for one structured chat send
/// failure (see [`send_failure_key`]). The backend carries no
/// words across the boundary for these failures, so this is
/// the one place their wording comes from.
pub(crate) fn send_failure_message(failure: backend::SendFailure) -> String {
    crate::i18n::t(send_failure_key(failure.kind))
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

/// The event card's when line for a unix-second start (and
/// optional end): "29 November 2026 · 19:00", with the end
/// appended as " – 22:00" on the same day or with its own
/// date across days. Same UTC grid as [`fmt_day`]/[`fmt_time`].
pub(crate) fn fmt_event_when(start_time: i64, end_time: i64) -> String {
    let one = |ts: i64| {
        let (y, m, d) = civil_from_days(ts.div_euclid(86_400));
        format!("{d} {} {y} · {}", MONTH_NAMES[(m - 1) as usize], fmt_time(ts))
    };
    match end_time {
        0 => one(start_time),
        end if end.div_euclid(86_400) == start_time.div_euclid(86_400) => {
            format!("{} – {}", one(start_time), fmt_time(end))
        }
        end => format!("{} – {}", one(start_time), one(end)),
    }
}

/// Splits a unix-second instant into the event form's strict
/// text fields (`YYYY-MM-DD`, `HH:MM`), the inverse of
/// `core::rich::parse_event_datetime`, for pre-filling the
/// edit form.
pub(crate) fn fmt_event_form_fields(timestamp: i64) -> (String, String) {
    let (y, m, d) = civil_from_days(timestamp.div_euclid(86_400));
    (format!("{y:04}-{m:02}-{d:02}"), fmt_time(timestamp))
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
        locked: live.locked,
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
        secret: secret.clone(),
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
        backend::LiveMessageKind::Order {
            order_id,
            token,
            item_count,
            total_amount_1000,
            currency,
            status,
        } => MessageKind::Order {
            order_id: order_id.clone(),
            token: token.clone(),
            item_count: *item_count,
            total_amount_1000: *total_amount_1000,
            currency: currency.clone(),
            status: status.clone(),
        },
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
        backend::LiveMessageKind::Buttons { footer, buttons } => MessageKind::Buttons {
            footer: footer.clone(),
            buttons: buttons
                .iter()
                .map(|b| core::model::MsgButton { id: b.id.clone(), text: b.text.clone() })
                .collect(),
        },
        backend::LiveMessageKind::ListMsg { description, button_label, footer, sections } => {
            MessageKind::ListMsg {
                description: description.clone(),
                button_label: button_label.clone(),
                footer: footer.clone(),
                sections: sections
                    .iter()
                    .map(|s| core::model::MsgListSection {
                        title: s.title.clone(),
                        rows: s
                            .rows
                            .iter()
                            .map(|r| core::model::MsgListRow {
                                id: r.id.clone(),
                                title: r.title.clone(),
                                description: r.description.clone(),
                            })
                            .collect(),
                    })
                    .collect(),
            }
        }
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
        // The local outbox states have their own rungs now, so a
        // pending or failed echo the batch flush writes keeps its
        // state instead of being promoted to Sent on paper.
        Delivery::Sending => backend::DeliveryState::Sending,
        Delivery::Failed => backend::DeliveryState::Failed,
        Delivery::Sent => backend::DeliveryState::Sent,
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
                // The local outbox rungs: only archive rows
                // carry them (a pending echo persisted at send
                // time), and they restore as exactly what they
                // were, never promoted to Sent.
                Some(backend::DeliveryState::Sending) => Delivery::Sending,
                Some(backend::DeliveryState::Failed) => Delivery::Failed,
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
/// configured. The list itself is edited through
/// `BackendCommand::SetPrivacyExceptions`, never through the
/// cycle (see [`privacy_value_label`]).
#[must_use]
pub(crate) fn privacy_audiences(category: &str) -> &'static [&'static str] {
    match category {
        "online" => &["all", "match_last_seen"],
        "last" | "profile" | "status" | "groupadd" => &["all", "contacts", "none"],
        // The remaining categories the backend maps, with the
        // value sets the library validates for them: call adds
        // add `known` (people the user has messaged) between
        // everyone and contacts; messages is the two-way
        // anti-brigading choice; defense mode is a switch
        // between off and its standard level.
        "calladd" => &["all", "known", "contacts"],
        "messages" => &["all", "contacts"],
        "defense" => &["off", "on_standard"],
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
        "known" => "Known contacts".to_owned(),
        "off" => "Off".to_owned(),
        "on_standard" => "Standard".to_owned(),
        other => other.to_owned(),
    }
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
        locked: chat.locked,
        pinned: chat.pinned,
        ephemeral_secs: chat.ephemeral_secs,
        muted: chat.muted,
        mute_until: i64::try_from(chat.mute_until).unwrap_or(0),
        parent_jid: None,
        is_parent: false,
        from_history: false,
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
mod send_failure_text_tests {
    use super::send_failure_key;
    use backend::{SendFailureKind, SendFailureStage};

    /// Each failure kind selects its own locale key, and the
    /// stage never changes the selection: the wording must not
    /// pretend to a diagnosis the stage cannot support.
    #[test]
    fn kind_selects_the_key_and_stage_never_does() {
        let stages = [
            SendFailureStage::PrepLid,
            SendFailureStage::PrepDevices,
            SendFailureStage::PrepGroup,
            SendFailureStage::Write,
            SendFailureStage::Other,
        ];
        let kinds = [
            (SendFailureKind::Timeout, "notice.send_failed_timeout"),
            (SendFailureKind::Offline, "notice.send_failed_offline"),
            (SendFailureKind::Rejected, "notice.send_failed_rejected"),
            (SendFailureKind::Other, "notice.send_failed_generic"),
        ];
        for (kind, key) in kinds {
            assert_eq!(send_failure_key(kind), key);
            // The key function takes the kind alone, so stages
            // cannot leak into the selection; this loop pins
            // that every stage exists beside every kind without
            // a second wording anywhere.
            for stage in stages {
                let _ = backend::SendFailure { kind, stage };
            }
        }
    }

    /// The selected keys resolve to real locale text in the
    /// source locale, not to the key itself (the i18n fallback
    /// for a missing key).
    #[test]
    fn selected_keys_resolve_to_locale_text() {
        for kind in [
            SendFailureKind::Timeout,
            SendFailureKind::Offline,
            SendFailureKind::Rejected,
            SendFailureKind::Other,
        ] {
            let key = send_failure_key(kind);
            let text = crate::i18n::t(key);
            assert_ne!(text, key, "key {key} must exist in the locale files");
            assert!(!text.is_empty(), "key {key} must not resolve to an empty string");
        }
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

#[cfg(test)]
mod chat_sort_cadence_tests {
    use super::{chat_sort_cadence, SYNC_SORT_FLUSH_INTERVAL};

    /// qc1644: outside a sync episode every flush sorts, exactly
    /// as the flush always did, and the counter stays reset.
    #[test]
    fn no_sync_sorts_every_flush() {
        assert_eq!(chat_sort_cadence(false, false, 0), (true, 0));
        assert_eq!(chat_sort_cadence(false, false, 7), (true, 0));
    }

    /// qc1644: a forced sort (a sync completion) sorts at any
    /// counter value and resets the cadence.
    #[test]
    fn forced_sort_always_runs_and_resets() {
        assert_eq!(chat_sort_cadence(true, true, 0), (true, 0));
        assert_eq!(chat_sort_cadence(true, true, 3), (true, 0));
    }

    /// qc1644: inside an episode the sort runs once per
    /// interval of flushes, never more often, and the counter
    /// cycles back to zero when it runs.
    #[test]
    fn sync_episode_sorts_once_per_interval() {
        let mut counter = 0;
        let mut sorts = 0;
        for _ in 0..(SYNC_SORT_FLUSH_INTERVAL * 2) {
            let (sort, next) = chat_sort_cadence(true, false, counter);
            if sort {
                sorts += 1;
                assert_eq!(next, 0, "a run sort resets the counter");
            }
            counter = next;
        }
        assert_eq!(sorts, 2, "one sort per interval of flushes");
        assert_eq!(counter, 0);
    }
}

#[cfg(test)]
mod byte_budget_tests {
    use super::{ByteBudget, MediaThumbKey};
    use std::collections::HashMap;

    /// Inserts into a plain map under a 100-byte tracker,
    /// evicting oldest first exactly as the `LiveSession`
    /// helpers do, so the accounting contract is tested without
    /// building a whole session.
    fn put<const LIMIT: usize>(
        map: &mut HashMap<u64, Vec<u8>>,
        budget: &mut ByteBudget<u64, LIMIT>,
        id: u64,
        bytes: Vec<u8>,
    ) {
        let new_bytes = bytes.len();
        let replaced = map.insert(id, bytes).map(|old| old.len());
        budget.track_insert(id, replaced, new_bytes);
        while budget.over_budget() {
            let Some(oldest) = budget.oldest() else { break };
            let removed = map.remove(&oldest).map_or(0, |old| old.len());
            budget.track_eviction(removed);
        }
    }

    #[test]
    fn over_budget_eviction_drops_oldest_first_and_totals_stay_correct() {
        let mut map = HashMap::new();
        let mut budget = ByteBudget::<u64, 100>::new();
        put(&mut map, &mut budget, 1, vec![0; 40]);
        put(&mut map, &mut budget, 2, vec![0; 40]);
        assert_eq!(budget.total_bytes, 80);
        // The third insert crosses the budget: the oldest entry
        // goes, the two newest stay, and the total is the sum of
        // what the map actually holds.
        put(&mut map, &mut budget, 3, vec![0; 40]);
        assert!(!map.contains_key(&1), "the oldest entry is evicted");
        assert!(map.contains_key(&2) && map.contains_key(&3));
        assert_eq!(budget.total_bytes, 80);
        assert_eq!(budget.total_bytes, map.values().map(Vec::len).sum::<usize>());
    }

    #[test]
    fn replace_refreshes_position_and_does_not_double_count() {
        let mut map = HashMap::new();
        let mut budget = ByteBudget::<u64, 100>::new();
        put(&mut map, &mut budget, 1, vec![0; 60]);
        put(&mut map, &mut budget, 2, vec![0; 30]);
        // Replacing key 1 swaps its bytes (60 out, 50 in) and
        // makes it the freshest entry, so key 2 is now oldest.
        put(&mut map, &mut budget, 1, vec![0; 50]);
        assert_eq!(budget.total_bytes, 80, "replaced bytes count once");
        assert_eq!(budget.oldest(), Some(2));
        put(&mut map, &mut budget, 3, vec![0; 30]);
        assert!(!map.contains_key(&2), "the refreshed key outlives the older one");
        assert!(map.contains_key(&1) && map.contains_key(&3));
        assert_eq!(budget.total_bytes, 80);
    }

    #[test]
    fn remove_drops_accounting() {
        let mut map = HashMap::new();
        let mut budget = ByteBudget::<u64, 100>::new();
        put(&mut map, &mut budget, 1, vec![0; 40]);
        put(&mut map, &mut budget, 2, vec![0; 40]);
        if let Some(old) = map.remove(&1) {
            budget.track_remove(&1, old.len());
        }
        assert_eq!(budget.total_bytes, 40);
        assert_eq!(budget.oldest(), Some(2), "the removed key leaves the eviction order");
        // A remove for an absent key changes nothing.
        if let Some(old) = map.remove(&99) {
            budget.track_remove(&99, old.len());
        }
        assert_eq!(budget.total_bytes, 40);
    }

    #[test]
    fn an_entry_larger_than_the_budget_evicts_itself() {
        let mut map = HashMap::new();
        let mut budget = ByteBudget::<u64, 100>::new();
        put(&mut map, &mut budget, 1, vec![0; 150]);
        assert!(map.is_empty(), "an oversized entry is never cached");
        assert_eq!(budget.total_bytes, 0);
        assert!(!budget.over_budget());
    }

    #[test]
    fn the_combined_media_order_spans_both_maps() {
        // The media budget is shared: a video insert can evict a
        // photo, because eviction follows the one insertion
        // order across both maps.
        let mut photos: HashMap<u64, Vec<u8>> = HashMap::new();
        let mut videos: HashMap<u64, Vec<u8>> = HashMap::new();
        let mut budget = ByteBudget::<MediaThumbKey, 100>::new();
        let mut put_both =
            |budget: &mut ByteBudget<MediaThumbKey, 100>, key: MediaThumbKey, bytes: Vec<u8>| {
                let new_bytes = bytes.len();
                let replaced = match key {
                    MediaThumbKey::Photo(id) => photos.insert(id, bytes),
                    MediaThumbKey::Video(id) => videos.insert(id, bytes),
                }
                .map(|old| old.len());
                budget.track_insert(key, replaced, new_bytes);
                while budget.over_budget() {
                    let Some(oldest) = budget.oldest() else { break };
                    let removed = match oldest {
                        MediaThumbKey::Photo(id) => photos.remove(&id),
                        MediaThumbKey::Video(id) => videos.remove(&id),
                    }
                    .map_or(0, |old| old.len());
                    budget.track_eviction(removed);
                }
            };
        put_both(&mut budget, MediaThumbKey::Photo(1), vec![0; 60]);
        put_both(&mut budget, MediaThumbKey::Video(2), vec![0; 60]);
        assert!(photos.is_empty(), "the older photo is evicted by the video insert");
        assert_eq!(videos.len(), 1);
        assert_eq!(budget.total_bytes, 60);
    }
}
