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

/// One running live session: channels, link state, and the live data
/// mirrors the UI renders.
pub(crate) struct LiveSession {
    pub(crate) channels: BackendChannels,
    pub(crate) status: LinkStatus,
    /// Latest QR matrix to draw, when waiting for a scan.
    pub(crate) qr: Option<QrMatrix>,
    /// Latest pairing code, when in code mode.
    pub(crate) pair_code: Option<String>,
    /// When the current pairing code arrived. Pair codes die after
    /// roughly three minutes; the UI must never present a dead code
    /// as live (v0.25 logic review F13).
    pub(crate) pair_code_at: Option<std::time::Instant>,
    /// Account label reported on connect.
    pub(crate) account: String,
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
    /// Chats where the user performed a real read action (scrolled
    /// to the newest messages, focused or typed in the composer,
    /// sent a reply) since the last receipt went out. Read
    /// receipts are gated on this set (the owner's read-by-action
    /// rule): merely opening a chat, or a message arriving while
    /// its screen happens to be open, never marks anything read.
    pub(crate) engaged_chats: std::collections::HashSet<u64>,
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

/// Maps a raw backend error string to a human sentence for the
/// notice banner. The raw text still goes to the log and to the
/// (pre-link) status line; the banner never shows library internals
/// (typed-errors review, display layer). Unknown errors pass
/// through, trimmed, because a specific honest message beats a
/// generic one.
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
        // open; the archive reports through its usual note and the
        // backend through its session-store error.
        let key_result = store::load_or_create_key(&data_dir);
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
        let repaint_ctx = ctx.clone();
        let wake: backend::WakeFn = std::sync::Arc::new(move || repaint_ctx.request_repaint());
        let key_loaded = key_result.is_ok();
        let config = BackendConfig {
            data_dir,
            account_id: "default".to_owned(),
            db_key: key_result.unwrap_or([0; 32]),
        };
        if !fake && key_loaded {
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
            pair_code: None,
            pair_code_at: None,
            account: String::new(),
            store: store_opt,
            jids: HashMap::new(),
            messages: HashMap::new(),
            times: HashMap::new(),
            fake,
            flags: HashMap::new(),
            engaged_chats: std::collections::HashSet::new(),
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
                            // Archived messages must stay actionable
                            // after a restart: per-message live actions
                            // look the protocol id up here, and before
                            // v0.25 only fresh upserts ever filled it,
                            // so every action on history silently
                            // no-oped (logic review F2).
                            self.protocol_ids.insert(message_id(&m.chat_jid, &m.id), m.id.clone());
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
    pub(crate) fn sync_conversation(&mut self) {
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

    /// Drains backend events and applies them. Called every frame.
    pub(crate) fn poll_backend(&mut self) {
        let mut pending: Vec<BackendEvent> = Vec::new();
        if let Some(session) = &self.live {
            while let Ok(event) = session.channels.events.try_recv() {
                pending.push(event);
            }
        }
        for event in pending {
            self.trace_event(&event);
            self.apply_event(event);
        }
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
            if self.chats.iter().any(|c| c.id == id) {
                self.pending_select_chat = None;
                self.select_chat_now(id);
            } else if id >= 1 && (id as usize) < self.chats.len() {
                // Live chat ids are JID hashes, so a small number is
                // a 1-based index into the chat list (index 0 is the
                // pinned AI Assistant).
                let target = self.chats[id as usize].id;
                self.pending_select_chat = None;
                self.select_chat_now(target);
            }
        }
    }

    fn apply_event(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::QrCode { payload } => {
                if let Some(session) = &mut self.live {
                    if let Ok(matrix) = backend::qr_matrix(&payload) {
                        session.qr = Some(matrix);
                    }
                    if !session.status.is_linked() {
                        session.status = LinkStatus::WaitingQr;
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
                    session.qr = None;
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
                }
            }
            BackendEvent::ChatUpsert(chat) => self.apply_chat_upsert(&chat),
            BackendEvent::MessageUpsert(msg) => {
                let chat_id = jid_id(&msg.chat_jid);
                let msg_key = message_id(&msg.chat_jid, &msg.id);
                // Side effects (preview fetches) run only for
                // genuinely new messages, not
                // for every re-upsert of a known one (logic review
                // F8). The upsert itself reports which case this is.
                let new_incoming = self.apply_message_upsert(&msg);
                if new_incoming {
                    self.on_incoming_live_message(chat_id, msg_key, &msg);
                }
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
                    let actors = self.reaction_actors.entry(key).or_default();
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
            BackendEvent::PrivacyState(map) => {
                self.privacy_state = map;
            }
            BackendEvent::MediaSaved { path, .. } => {
                self.notice = Some(format!("Saved to {path}"));
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
                self.groups.live_members.insert(jid_id(&chat_jid), members);
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
                self.groups.join_requests = Some((jid_id(&chat_jid), requests));
            }
            BackendEvent::ConnectionLost { reason } => {
                if let Some(session) = &mut self.live {
                    if session.status.is_linked() || session.status == LinkStatus::WaitingQr {
                        session.status = LinkStatus::Lost(reason);
                    }
                }
            }
            BackendEvent::Crashed { summary, restarted } => {
                if let Some(session) = &mut self.live {
                    session.status = LinkStatus::Crashed(summary);
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
        // a chat this client knows is muted. Unmuting has its
        // own path (the chat action and its upsert).
        if !merged.muted {
            if let Some(known) = self.chats.iter().find(|c| c.id == id) {
                merged.muted = known.muted;
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
            // The backend row is authoritative for these two; dropping
            // them here made phone-side archive/pin (and the fake's
            // LeaveGroup) invisible until restart (logic review F14).
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
            let mut flags = session.flags.get(&msg_id).copied().unwrap_or(0);
            if live.starred {
                flags |= store::FLAG_STARRED;
            }
            if live.pinned {
                flags |= store::FLAG_PINNED_MESSAGE;
            }
            if flags != session.flags.get(&msg_id).copied().unwrap_or(0) {
                session.flags.insert(msg_id, flags);
            }
            if let Some(store) = &session.store {
                let _ = store.upsert_message(live, flags);
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
            entry.sort_by_key(|(ts, _)| *ts);
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
                preview: live.body.clone(),
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
        // (toggles, mute, preview, focus) before firing.
        if new_incoming {
            if let Some(chat) = self.chats.iter().find(|c| c.id == chat_id) {
                self.pending_notifications.push(crate::notify::NotifyRequest {
                    chat_id,
                    title: chat.title.clone(),
                    kind: chat.kind,
                    muted: chat.muted,
                    sender: live.sender.clone(),
                    body: live.body.clone(),
                });
            }
        }
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.preview = live.body.clone();
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
        self.sort_live_chats();
        if chat_id == self.selected_chat {
            self.sync_conversation();
        }
        new_incoming
    }

    fn sort_live_chats(&mut self) {
        if let Some(session) = &self.live {
            let times = &session.times;
            self.chats.sort_by(|a, b| {
                times.get(&b.id).copied().unwrap_or(0).cmp(&times.get(&a.id).copied().unwrap_or(0))
            });
        }
    }

    /// Sets (or clears with `flags = 0`) archive flags for one
    /// message and persists them when a store row exists.
    pub(crate) fn set_live_flag(&mut self, chat_id: u64, msg_id: u64, bit: i64, on: bool) {
        let protocol_id = self.protocol_ids.get(&msg_id).cloned();
        if let Some(session) = &mut self.live {
            let entry = session.flags.entry(msg_id).or_insert(0);
            if on {
                *entry |= bit;
            } else {
                *entry &= !bit;
            }
            let flags = *entry;
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

    /// Per-message work for one incoming live message: the
    /// link-preview fetch.
    pub(crate) fn on_incoming_live_message(
        &mut self,
        _chat_id: u64,
        msg_id: u64,
        live: &LiveMessage,
    ) {
        // Link preview fetch (live mode only, setting-gated).
        if self.link_previews && !self.live.as_ref().is_some_and(|s| s.fake) {
            if let Some(url) = core::rich::first_url(&live.body) {
                self.maybe_fetch_preview(msg_id, url);
            }
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
