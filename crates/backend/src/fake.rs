//! [`FakeBackend`], a deterministic, in-process implementation of
//! the backend contract.
//!
//! It exists for three honest purposes: unit/integration tests,
//! reproducible screenshots of live mode, and letting contributors
//! explore the live UI without a WhatsApp account. It is labeled as
//! a fake everywhere it surfaces in the UI.
//!
//! Script: on `ConnectQr` it emits a QR payload (a syntactically
//! plausible but meaningless string, it encodes no real linking
//! data). With `auto_connect` set, it then emits `Connected`, three
//! chats, and a few messages. `ConnectPairCode` emits a deterministic
//! 8-character code derived from the phone digits, then (with
//! `auto_connect`) the same linked script. `SendText` echoes the
//! message back as an upsert and, in the scripted DM, produces one
//! scripted reply.
//!
//! The [`FakeBackend::demo`] variant goes one step further: after
//! the linked script it plays [`demo_script`], a short scheduled
//! sequence of incoming events (typing, messages, reactions, poll
//! votes, receipts), so the app demo feels alive and every surface
//! moves on its own. The script is finite: it plays once, over a
//! couple of minutes, and then stops.

use std::sync::mpsc::Receiver;
use std::time::Duration;

use crate::{
    Backend, BackendCommand, BackendConfig, BackendEvent, EventSink, LiveChat, LiveChatKind,
    LiveMessage,
};

/// The payload the fake backend asks the UI to render as a QR code.
/// Obviously not a real linking payload.
pub const FAKE_QR_PAYLOAD: &str = "2@FAKE0PAYLOAD,AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=,\
     BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=,\
     CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC=,1";

/// JID of the scripted DM chat.
pub const FAKE_DM_JID: &str = "10000000001@s.whatsapp.net";
/// JID of the scripted group chat.
pub const FAKE_GROUP_JID: &str = "10000000002@g.us";
/// JID of the scripted channel.
pub const FAKE_CHANNEL_JID: &str = "10000000003@newsletter";

/// The deterministic fake backend. See the module docs.
#[derive(Debug, Clone)]
pub struct FakeBackend {
    /// Whether pairing flows complete on their own (scripted scan)
    /// instead of waiting forever at the QR/code step.
    pub auto_connect: bool,
    /// Optional command log (privacy/state-machine tests): the fake
    /// appends one line per MarkAsRead / SetTyping / SetPrivacy
    /// command it receives.
    pub command_log: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
    /// Whether the scheduled incoming-event script ([`demo_script`])
    /// plays after the linked script. Only the app demo enables
    /// this; tests keep the deterministic linked script alone.
    pub demo_script: bool,
}

impl FakeBackend {
    /// A fake that completes pairing on its own, for tests and the
    /// "linked" screenshots.
    #[must_use]
    pub fn auto() -> Self {
        Self { auto_connect: true, command_log: None, demo_script: false }
    }

    /// A fake that waits at the pairing step, for the QR/code
    /// screenshots and for tests of the waiting states.
    #[must_use]
    pub fn waiting() -> Self {
        Self { auto_connect: false, command_log: None, demo_script: false }
    }

    /// A fake that records privacy-relevant commands into `log`.
    #[must_use]
    pub fn auto_with_log(log: std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> Self {
        Self { auto_connect: true, command_log: Some(log), demo_script: false }
    }

    /// The app demo: links on its own, then plays the scheduled
    /// incoming-event script so the demo stays alive after the
    /// linked script lands.
    #[must_use]
    pub fn demo() -> Self {
        Self { auto_connect: true, command_log: None, demo_script: true }
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::auto()
    }
}

/// A deterministic fake invite code for a group, changing with
/// each reset generation so a reset visibly mints a new link.
fn fake_invite_code(chat_jid: &str, generation: u32) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZabcdefghjkmnpqrstvwxyz";
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in chat_jid.bytes().chain(generation.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut out = String::with_capacity(22);
    let mut state = hash;
    for _ in 0..22 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push(ALPHABET[(state % ALPHABET.len() as u64) as usize] as char);
    }
    out
}

/// The scripted group's default member rows: (jid, name, admin).
fn default_members() -> Vec<(String, String, bool)> {
    vec![
        ("10000000001@s.whatsapp.net".to_owned(), "Sarah Miller".to_owned(), true),
        ("10000000004@s.whatsapp.net".to_owned(), "Mike Johnson".to_owned(), false),
        ("10000000007@s.whatsapp.net".to_owned(), "Emma Wilson".to_owned(), false),
    ]
}

/// The scripted chats, in display order.
#[must_use]
pub fn scripted_chats() -> Vec<LiveChat> {
    vec![
        LiveChat {
            jid: FAKE_DM_JID.to_owned(),
            title: "Sarah Miller (live demo)".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "Sure, I'll send the notes in a bit".to_owned(),
            preview_sender: None,
            timestamp: 1_700_000_300,
            unread: 1,
            archived: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
        },
        LiveChat {
            jid: FAKE_GROUP_JID.to_owned(),
            title: "Weekend Plans (live demo)".to_owned(),
            kind: LiveChatKind::Group,
            preview: "Mike: Don't forget Sunday".to_owned(),
            preview_sender: Some("Mike".to_owned()),
            timestamp: 1_700_000_200,
            unread: 0,
            archived: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
        },
        LiveChat {
            jid: FAKE_CHANNEL_JID.to_owned(),
            title: "City News (live demo)".to_owned(),
            kind: LiveChatKind::Channel,
            preview: "Announcement: the holiday events schedule is out".to_owned(),
            preview_sender: None,
            timestamp: 1_700_000_100,
            unread: 4,
            archived: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
        },
    ]
}

/// The scripted messages for the DM chat, including one poll, one
/// link message with a scripted preview, and one message that a
/// later scripted revoke targets.
#[must_use]
pub fn scripted_messages() -> Vec<LiveMessage> {
    // The poll sits after the three-photo album run: the archive
    // returns messages in timestamp order, so the scripted vector
    // must agree with that order for the round-trip test.
    let mut poll = LiveMessage::text(
        "fake-poll-1",
        FAKE_DM_JID,
        Some("Sarah Miller".to_owned()),
        "",
        1_700_000_253,
        false,
    );
    poll.kind = crate::LiveMessageKind::Poll {
        question: "Where should we have lunch today?".to_owned(),
        options: vec![
            ("The deli on 5th".to_owned(), 3),
            ("Sushi place".to_owned(), 5),
            ("Delivery".to_owned(), 1),
        ],
        multiple: false,
        my_votes: Vec::new(),
        correct: None,
    };
    let mut link = LiveMessage::text(
        "fake-link-1",
        FAKE_DM_JID,
        Some("Sarah Miller".to_owned()),
        "Read this: https://example.com/city-guide, the guide is complete.",
        1_700_000_275,
        false,
    );
    link.link_preview = Some(crate::LivePreview {
        title: "City Guide 2026 (sample)".to_owned(),
        description: "Scripted link preview from test mode, not a real fetch result.".to_owned(),
        url: "https://example.com/city-guide".to_owned(),
    });
    link.reactions = vec![("👍".to_owned(), 1)];
    let mut photo = LiveMessage::text(
        "fake-photo-1",
        FAKE_DM_JID,
        Some("Sarah Miller".to_owned()),
        "The new living room setup",
        1_700_000_250,
        false,
    );
    photo.kind = crate::LiveMessageKind::Media {
        media: crate::MediaKind::Photo,
        caption: "The new living room setup".to_owned(),
        seconds: 0,
        size_bytes: 182_000,
    };
    // Two more photos straight after the first: consecutive media
    // from one sender is what the stream collapses into an album.
    let mut photo2 = LiveMessage::text(
        "fake-photo-2",
        FAKE_DM_JID,
        Some("Sarah Miller".to_owned()),
        "",
        1_700_000_251,
        false,
    );
    photo2.kind = crate::LiveMessageKind::Media {
        media: crate::MediaKind::Photo,
        caption: String::new(),
        seconds: 0,
        size_bytes: 160_000,
    };
    let mut photo3 = LiveMessage::text(
        "fake-photo-3",
        FAKE_DM_JID,
        Some("Sarah Miller".to_owned()),
        "",
        1_700_000_252,
        false,
    );
    photo3.kind = crate::LiveMessageKind::Media {
        media: crate::MediaKind::Photo,
        caption: String::new(),
        seconds: 0,
        size_bytes: 144_000,
    };
    let mut view_once_video = LiveMessage::text(
        "fake-vo-1",
        FAKE_DM_JID,
        Some("Sarah Miller".to_owned()),
        "",
        1_700_000_270,
        false,
    );
    view_once_video.kind = crate::LiveMessageKind::Media {
        media: crate::MediaKind::Video,
        caption: String::new(),
        seconds: 12,
        size_bytes: 2_400_000,
    };
    view_once_video.view_once = true;
    vec![
        LiveMessage::text(
            "fake-1",
            FAKE_DM_JID,
            Some("Sarah Miller".to_owned()),
            "Hello! This is a message from test mode, not real WhatsApp.",
            1_700_000_100,
            false,
        ),
        LiveMessage::text(
            "fake-2",
            FAKE_DM_JID,
            None,
            "Sure, this is my reply (local echo)",
            1_700_000_200,
            true,
        ),
        photo,
        photo2,
        photo3,
        poll,
        view_once_video,
        link,
        LiveMessage::text(
            "fake-promo-1",
            FAKE_DM_JID,
            Some("Sarah Miller".to_owned()),
            "PROMO: this message will be revoked again.",
            1_700_000_290,
            false,
        ),
        LiveMessage::text(
            "fake-3",
            FAKE_DM_JID,
            Some("Sarah Miller".to_owned()),
            "Sure, I'll send the notes in a bit",
            1_700_000_300,
            false,
        ),
    ]
}

/// The scripted messages for the group chat: a location share and a
/// received carousel sample (render-only rich types).
#[must_use]
pub(crate) fn scripted_group_messages() -> Vec<LiveMessage> {
    let mut location = LiveMessage::text(
        "fake-loc-1",
        FAKE_GROUP_JID,
        Some("Mike Johnson".to_owned()),
        "",
        1_700_000_150,
        false,
    );
    location.kind = crate::LiveMessageKind::Location {
        name: "Central Park, New York".to_owned(),
        latitude: 40.7812,
        longitude: -73.9665,
    };
    let mut carousel = LiveMessage::text(
        "fake-carousel-1",
        FAKE_GROUP_JID,
        Some("Mike Johnson".to_owned()),
        "",
        1_700_000_180,
        false,
    );
    carousel.kind = crate::LiveMessageKind::Carousel {
        body: "This week's data plan picks:".to_owned(),
        cards: vec![
            crate::LiveCard {
                title: "10 GB plan".to_owned(),
                body: "Valid 30 days, all networks.".to_owned(),
                image: None,
                buttons: vec!["Detail".to_owned()],
            },
            crate::LiveCard {
                title: "25 GB plan".to_owned(),
                body: "Valid 30 days + bonus night quota.".to_owned(),
                image: None,
                buttons: vec!["Detail".to_owned(), "Buy".to_owned()],
            },
        ],
    };
    let mut contact = LiveMessage::text(
        "fake-contact-1",
        FAKE_GROUP_JID,
        Some("Mike Johnson".to_owned()),
        "",
        1_700_000_210,
        false,
    );
    contact.kind = crate::LiveMessageKind::Contact {
        name: "Emma Wilson".to_owned(),
        phone: "Contact card shared by Mike".to_owned(),
    };
    vec![location, carousel, contact]
}

/// One step of the scheduled demo script: an event to emit `at`
/// after the demo links. The script is plain data on purpose, so
/// tests can assert its shape (order, coverage, bounds) without
/// waiting for real time to pass.
#[derive(Debug, Clone, PartialEq)]
pub struct DemoStep {
    /// Delay after the linked script lands.
    pub at: Duration,
    /// The event to emit when the delay elapses.
    pub event: BackendEvent,
}

/// The scheduled incoming-event script the demo backend plays
/// after linking: typing indicators that resolve into messages, a
/// poll collecting votes, reactions landing, a quote reply, a
/// voice note and a photo arriving, and the seeded outgoing
/// message's ticks moving from delivered to read. Message
/// timestamps continue the linked script's clock (its last message
/// sits at 1_700_000_300), one per step offset, so the archive
/// order stays sane. The script is finite by design: it plays
/// once and stops, it never loops.
#[must_use]
pub fn demo_script() -> Vec<DemoStep> {
    const BASE: i64 = 1_700_000_300;
    let step = |secs: u64, event: BackendEvent| DemoStep { at: Duration::from_secs(secs), event };
    // The linked lunch poll, re-upserted with two more votes.
    let mut poll = scripted_messages()
        .into_iter()
        .find(|m| matches!(m.kind, crate::LiveMessageKind::Poll { .. }))
        .expect("the linked script carries a poll");
    if let crate::LiveMessageKind::Poll { options, .. } = &mut poll.kind {
        options[0].1 += 1;
        options[2].1 += 1;
    }
    let mut voice = LiveMessage::text(
        "fake-live-4",
        FAKE_GROUP_JID,
        Some("Emma Wilson".to_owned()),
        "",
        BASE + 34,
        false,
    );
    voice.kind = crate::LiveMessageKind::Media {
        media: crate::MediaKind::Voice,
        caption: String::new(),
        seconds: 9,
        size_bytes: 48_000,
    };
    let mut quote = LiveMessage::text(
        "fake-live-5",
        FAKE_DM_JID,
        Some("Sarah Miller".to_owned()),
        "exactly. and ngl, preload day is gonna eat everyone's storage",
        BASE + 42,
        false,
    );
    quote.quoted_sender = Some("You".to_owned());
    quote.quoted_body = Some("Sure, this is my reply (local echo)".to_owned());
    let mut photo = LiveMessage::text(
        "fake-live-6",
        FAKE_GROUP_JID,
        Some("Mike Johnson".to_owned()),
        "my launch night setup is ready",
        BASE + 50,
        false,
    );
    photo.kind = crate::LiveMessageKind::Media {
        media: crate::MediaKind::Photo,
        caption: "my launch night setup is ready".to_owned(),
        seconds: 0,
        size_bytes: 214_000,
    };
    vec![
        step(4, BackendEvent::TypingReceived { chat_jid: FAKE_DM_JID.to_owned(), typing: true }),
        step(
            7,
            BackendEvent::MessageUpsert(LiveMessage::text(
                "fake-live-1",
                FAKE_DM_JID,
                Some("Sarah Miller".to_owned()),
                "the launch night poll is live in the group chat, go vote before it closes fr",
                BASE + 7,
                false,
            )),
        ),
        step(7, BackendEvent::TypingReceived { chat_jid: FAKE_DM_JID.to_owned(), typing: false }),
        step(
            13,
            BackendEvent::MessageUpsert(LiveMessage::text(
                "fake-live-2",
                FAKE_GROUP_JID,
                Some("Mike Johnson".to_owned()),
                "voted. bringing the extra controller too, we're locked in",
                BASE + 13,
                false,
            )),
        ),
        step(19, BackendEvent::MessageUpsert(poll)),
        step(
            25,
            BackendEvent::ReactionUpdate {
                chat_jid: FAKE_DM_JID.to_owned(),
                message_id: "fake-2".to_owned(),
                actor: "Sarah Miller".to_owned(),
                emoji: "❤️".to_owned(),
            },
        ),
        step(
            31,
            BackendEvent::TypingReceived { chat_jid: FAKE_GROUP_JID.to_owned(), typing: true },
        ),
        step(34, BackendEvent::MessageUpsert(voice)),
        step(
            34,
            BackendEvent::TypingReceived { chat_jid: FAKE_GROUP_JID.to_owned(), typing: false },
        ),
        step(42, BackendEvent::MessageUpsert(quote)),
        step(50, BackendEvent::MessageUpsert(photo)),
        step(
            58,
            BackendEvent::Receipt {
                chat_jid: FAKE_DM_JID.to_owned(),
                message_ids: vec!["fake-2".to_owned()],
                kind: crate::ReceiptKind::Delivered,
                at: BASE + 58,
            },
        ),
        step(
            72,
            BackendEvent::ReactionUpdate {
                chat_jid: FAKE_GROUP_JID.to_owned(),
                message_id: "fake-live-2".to_owned(),
                actor: "Emma Wilson".to_owned(),
                emoji: "👍".to_owned(),
            },
        ),
        step(
            84,
            BackendEvent::Receipt {
                chat_jid: FAKE_DM_JID.to_owned(),
                message_ids: vec!["fake-2".to_owned()],
                kind: crate::ReceiptKind::Read,
                at: BASE + 84,
            },
        ),
        step(
            96,
            BackendEvent::MessageUpsert(LiveMessage::text(
                "fake-live-7",
                FAKE_CHANNEL_JID,
                Some("City News".to_owned()),
                "Reminder: the holiday market opens this Saturday on Main Street",
                BASE + 96,
                false,
            )),
        ),
        step(110, BackendEvent::TypingReceived { chat_jid: FAKE_DM_JID.to_owned(), typing: true }),
        step(
            113,
            BackendEvent::MessageUpsert(LiveMessage::text(
                "fake-live-8",
                FAKE_DM_JID,
                Some("Sarah Miller".to_owned()),
                "see you Thursday then, this is gonna hit different",
                BASE + 113,
                false,
            )),
        ),
        step(113, BackendEvent::TypingReceived { chat_jid: FAKE_DM_JID.to_owned(), typing: false }),
    ]
}

/// Plays [`demo_script`] on its own thread, one event at a time,
/// until the script ends or `alive` drops (backend shutdown).
/// Waits in short slices so a shutdown lands promptly instead of
/// after the current step's full delay.
fn demo_script_ticker(events: EventSink, alive: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let start = std::time::Instant::now();
    for step in demo_script() {
        loop {
            if !alive.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let remaining = step.at.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(100)));
        }
        if !alive.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        events.emit(step.event);
    }
}

/// Deterministic 8-character pairing code from phone digits
/// (Crockford Base32, the same alphabet the real flow uses).
#[must_use]
pub(crate) fn fake_pair_code(phone: &str) -> String {
    const CHARS: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut h: u32 = 0x811C_9DC5;
    for b in phone.bytes() {
        h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    (0..8)
        .map(|_| {
            h = h.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            CHARS[((h >> 16) & 31) as usize % CHARS.len()] as char
        })
        .collect()
}

impl Backend for FakeBackend {
    fn run(
        self: Box<Self>,
        _config: &BackendConfig,
        commands: Receiver<BackendCommand>,
        events: EventSink,
    ) -> Result<(), String> {
        let mut sent_counter: u64 = 0;
        let command_log = self.command_log.clone();
        // Session state: commands mutate these maps, so sequences
        // like archive-then-pin compose instead of restarting from
        // the script on every command.
        let mut group_counter: u32 = 0;
        let mut group_state: std::collections::HashMap<String, Vec<(String, String, bool)>> =
            std::collections::HashMap::new();
        // Group invite, approval and join-request state. The
        // scripted group starts with approval on and two pending
        // requests so the review flow is exercisable end to end.
        let mut invite_resets: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        let mut approval_state: std::collections::HashMap<String, bool> =
            std::collections::HashMap::from([(FAKE_GROUP_JID.to_owned(), true)]);
        // (announce, locked) modes per group; both start off.
        let mut mode_state: std::collections::HashMap<String, (bool, bool)> =
            std::collections::HashMap::new();
        let mut join_request_state: std::collections::HashMap<String, Vec<(String, String)>> =
            std::collections::HashMap::from([(
                FAKE_GROUP_JID.to_owned(),
                vec![
                    ("10000000021@s.whatsapp.net".to_owned(), "Ava Thompson".to_owned()),
                    ("10000000022@s.whatsapp.net".to_owned(), "David Kim".to_owned()),
                ],
            )]);
        let mut chat_state: std::collections::HashMap<String, LiveChat> =
            scripted_chats().into_iter().map(|c| (c.jid.clone(), c)).collect();
        // Blocked contact JIDs, mirroring the server-side list the
        // real backend fetches at connect: BlockContact mutates it
        // and the full list is re-emitted as a Blocklist event.
        let mut blocked_state: std::collections::HashSet<String> = std::collections::HashSet::new();
        // The account's privacy settings, mirroring the real
        // backend's fetch: SetPrivacy mutates one entry and the
        // full map is re-emitted, as the real refetch reports it.
        let mut privacy_state = baseline_privacy_state();
        let mut message_state: std::collections::HashMap<(String, String), LiveMessage> =
            scripted_messages()
                .into_iter()
                .chain(scripted_group_messages())
                .map(|m| ((m.chat_jid.clone(), m.id.clone()), m))
                .collect();
        let mut polls: std::collections::HashMap<String, LiveMessage> = scripted_messages()
            .into_iter()
            .filter(|m| matches!(m.kind, crate::LiveMessageKind::Poll { .. }))
            .map(|m| (m.id.clone(), m))
            .collect();
        // Call simulation: shared with a ticker thread that walks
        // the phase machine (ringing, connecting, active, ticking
        // duration) so command handlers stay synchronous.
        let call_state: std::sync::Arc<std::sync::Mutex<FakeCallState>> =
            std::sync::Arc::new(std::sync::Mutex::new(FakeCallState::default()));
        let call_alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        {
            let state = call_state.clone();
            let alive = call_alive.clone();
            let tick_events = events.clone();
            std::thread::Builder::new()
                .name("zaprust-fake-call-ticker".to_owned())
                .spawn(move || fake_call_ticker(state, alive, tick_events))
                .ok();
        }
        // The demo event script starts at most once per session,
        // on the first link, and shares the call ticker's lifetime
        // flag so Shutdown stops it too.
        let mut script_started = false;
        while let Ok(cmd) = commands.recv() {
            match cmd {
                BackendCommand::ConnectQr => {
                    events.emit(BackendEvent::QrCode { payload: FAKE_QR_PAYLOAD.to_owned() });
                    if self.auto_connect {
                        emit_linked_script(&events, &call_state, &privacy_state);
                        if self.demo_script && !script_started {
                            script_started = true;
                            let tick_events = events.clone();
                            let alive = call_alive.clone();
                            std::thread::Builder::new()
                                .name("zaprust-fake-demo-script".to_owned())
                                .spawn(move || demo_script_ticker(tick_events, alive))
                                .ok();
                        }
                    }
                }
                BackendCommand::ConnectPairCode { phone } => {
                    events.emit(BackendEvent::PairCode { code: fake_pair_code(&phone) });
                    if self.auto_connect {
                        emit_linked_script(&events, &call_state, &privacy_state);
                        if self.demo_script && !script_started {
                            script_started = true;
                            let tick_events = events.clone();
                            let alive = call_alive.clone();
                            std::thread::Builder::new()
                                .name("zaprust-fake-demo-script".to_owned())
                                .spawn(move || demo_script_ticker(tick_events, alive))
                                .ok();
                        }
                    }
                }
                BackendCommand::SendText { chat_jid, body, ephemeral_secs, .. } => {
                    sent_counter += 1;
                    let id = format!("fake-sent-{sent_counter}");
                    let ts = 1_700_000_300 + i64::try_from(sent_counter).unwrap_or(0);
                    let mut echo = LiveMessage::text(
                        id.clone(),
                        chat_jid.clone(),
                        None,
                        body.clone(),
                        ts,
                        true,
                    );
                    if ephemeral_secs > 0 {
                        echo.expires_at = ts + i64::from(ephemeral_secs);
                    }
                    events.emit(BackendEvent::MessageSent {
                        chat_jid: chat_jid.clone(),
                        message_id: id.clone(),
                    });
                    events.emit(BackendEvent::MessageUpsert(echo));
                    // Deterministic receipt progression: delivered,
                    // then read (the simulated driver of the same
                    // receipt state machine the live backend feeds).
                    events.emit(BackendEvent::Receipt {
                        chat_jid: chat_jid.clone(),
                        message_ids: vec![id.clone()],
                        kind: crate::ReceiptKind::Delivered,
                        at: ts,
                    });
                    events.emit(BackendEvent::Receipt {
                        chat_jid: chat_jid.clone(),
                        message_ids: vec![id],
                        kind: crate::ReceiptKind::Read,
                        at: ts,
                    });
                    if chat_jid == FAKE_DM_JID {
                        // The scripted reply lives under the
                        // chat's current timer, like a real
                        // incoming ephemeral message would.
                        let reply_timer = chat_state.get(&chat_jid).map_or(0, |c| c.ephemeral_secs);
                        let mut reply = LiveMessage::text(
                            format!("fake-reply-{sent_counter}"),
                            chat_jid,
                            Some("Sarah Miller".to_owned()),
                            "Received! (scripted reply from test mode)",
                            ts + 1,
                            false,
                        );
                        if reply_timer > 0 {
                            reply.expires_at = ts + 1 + i64::from(reply_timer);
                        }
                        events.emit(BackendEvent::MessageUpsert(reply));
                    }
                }
                BackendCommand::SetDisappearingTimer { chat_jid, duration_secs } => {
                    if let Some(chat) = chat_state.get_mut(&chat_jid) {
                        chat.ephemeral_secs = duration_secs;
                        events.emit(BackendEvent::ChatUpsert(chat.clone()));
                    }
                    events.emit(BackendEvent::DisappearingChanged { chat_jid, duration_secs });
                }
                BackendCommand::SendMedia {
                    chat_jid,
                    kind,
                    data,
                    caption,
                    file_name,
                    seconds,
                    ephemeral_secs,
                    ..
                } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("send_media:{chat_jid}:{kind:?}:{}", data.len()));
                    }
                    if data.is_empty() {
                        events.emit(BackendEvent::Error {
                            message: "That media file is empty, nothing was sent.".to_owned(),
                        });
                        continue;
                    }
                    sent_counter += 1;
                    let id = format!("fake-media-{sent_counter}");
                    let ts = 1_700_000_300 + i64::try_from(sent_counter).unwrap_or(0);
                    let label = match kind {
                        crate::MediaKind::Photo => "[photo]",
                        crate::MediaKind::Video => "[video]",
                        crate::MediaKind::Voice => "[voice note]",
                        crate::MediaKind::Document => "[document]",
                        crate::MediaKind::Sticker => "[sticker]",
                    };
                    let body = if !caption.is_empty() {
                        caption.clone()
                    } else if kind == crate::MediaKind::Document {
                        file_name.clone().unwrap_or_else(|| label.to_owned())
                    } else {
                        label.to_owned()
                    };
                    let mut msg =
                        LiveMessage::text(id.clone(), chat_jid.clone(), None, body, ts, true);
                    if ephemeral_secs > 0 {
                        msg.expires_at = ts + i64::from(ephemeral_secs);
                    }
                    msg.kind = crate::LiveMessageKind::Media {
                        media: kind,
                        caption: if kind == crate::MediaKind::Document {
                            file_name.unwrap_or(caption)
                        } else {
                            caption
                        },
                        seconds,
                        size_bytes: data.len() as u64,
                    };
                    events.emit(BackendEvent::MessageSent {
                        chat_jid: chat_jid.clone(),
                        message_id: id.clone(),
                    });
                    events.emit(BackendEvent::MessageUpsert(msg));
                    events.emit(BackendEvent::Receipt {
                        chat_jid: chat_jid.clone(),
                        message_ids: vec![id.clone()],
                        kind: crate::ReceiptKind::Delivered,
                        at: ts,
                    });
                    events.emit(BackendEvent::Receipt {
                        chat_jid,
                        message_ids: vec![id],
                        kind: crate::ReceiptKind::Read,
                        at: ts,
                    });
                }
                BackendCommand::SendLocation {
                    chat_jid,
                    name,
                    latitude,
                    longitude,
                    ephemeral_secs,
                } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("send_location:{chat_jid}:{latitude}:{longitude}"));
                    }
                    sent_counter += 1;
                    let id = format!("fake-location-{sent_counter}");
                    let ts = 1_700_000_300 + i64::try_from(sent_counter).unwrap_or(0);
                    let label = if name.is_empty() { "Location".to_owned() } else { name.clone() };
                    let mut msg =
                        LiveMessage::text(id.clone(), chat_jid.clone(), None, label, ts, true);
                    if ephemeral_secs > 0 {
                        msg.expires_at = ts + i64::from(ephemeral_secs);
                    }
                    msg.kind = crate::LiveMessageKind::Location { name, latitude, longitude };
                    events.emit(BackendEvent::MessageSent {
                        chat_jid: chat_jid.clone(),
                        message_id: id.clone(),
                    });
                    events.emit(BackendEvent::MessageUpsert(msg));
                    events.emit(BackendEvent::Receipt {
                        chat_jid: chat_jid.clone(),
                        message_ids: vec![id.clone()],
                        kind: crate::ReceiptKind::Delivered,
                        at: ts,
                    });
                    events.emit(BackendEvent::Receipt {
                        chat_jid,
                        message_ids: vec![id],
                        kind: crate::ReceiptKind::Read,
                        at: ts,
                    });
                }
                BackendCommand::StartCall { chat_jid, video } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("start_call:{chat_jid}:{video}"));
                    }
                    let mut state =
                        call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if state.current.is_some() {
                        events.emit(BackendEvent::Error {
                            message: "A call is already in progress.".to_owned(),
                        });
                    } else {
                        state.counter += 1;
                        let call_id = format!("fake-call-{}", state.counter);
                        state.current = Some(FakeCall {
                            call_id: call_id.clone(),
                            chat_jid: chat_jid.clone(),
                            outgoing: true,
                            video,
                            phase: crate::CallPhase::Ringing,
                            seconds: 0,
                            ticks: 0,
                        });
                        events.emit(BackendEvent::CallChanged {
                            call_id,
                            chat_jid,
                            phase: crate::CallPhase::Ringing,
                            seconds: 0,
                            reason: None,
                        });
                    }
                }
                BackendCommand::AcceptCall => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push("accept_call".to_owned());
                    }
                    let mut state =
                        call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    match &mut state.current {
                        Some(call) if !call.outgoing && call.phase == crate::CallPhase::Ringing => {
                            call.phase = crate::CallPhase::Connecting;
                            call.ticks = 0;
                            events.emit(BackendEvent::CallChanged {
                                call_id: call.call_id.clone(),
                                chat_jid: call.chat_jid.clone(),
                                phase: crate::CallPhase::Connecting,
                                seconds: 0,
                                reason: None,
                            });
                        }
                        _ => events.emit(BackendEvent::Error {
                            message: "There is no incoming call to accept.".to_owned(),
                        }),
                    }
                }
                BackendCommand::RejectCall => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push("reject_call".to_owned());
                    }
                    let mut state =
                        call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    match state.current.take() {
                        Some(call) if !call.outgoing => {
                            emit_call_end(&events, &mut state, &call, "Declined");
                        }
                        other => {
                            state.current = other;
                            events.emit(BackendEvent::Error {
                                message: "There is no incoming call to reject.".to_owned(),
                            });
                        }
                    }
                }
                BackendCommand::EndCall => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push("end_call".to_owned());
                    }
                    let mut state =
                        call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    match state.current.take() {
                        Some(call) => {
                            let reason = if call.phase == crate::CallPhase::Ringing && call.outgoing
                            {
                                "No answer"
                            } else {
                                "Call ended"
                            };
                            emit_call_end(&events, &mut state, &call, reason);
                        }
                        None => events.emit(BackendEvent::Error {
                            message: "No call is in progress.".to_owned(),
                        }),
                    }
                }
                BackendCommand::SetCallMuted { muted } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("call_muted:{muted}"));
                    }
                    let state =
                        call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if state.current.is_none() {
                        events.emit(BackendEvent::Error {
                            message: "No call is in progress.".to_owned(),
                        });
                    }
                }
                BackendCommand::SetCallSpeaker { speaker } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("call_speaker:{speaker}"));
                    }
                    let state =
                        call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if state.current.is_none() {
                        events.emit(BackendEvent::Error {
                            message: "No call is in progress.".to_owned(),
                        });
                    }
                }
                BackendCommand::CreatePoll { chat_jid, question, options, multiple } => {
                    sent_counter += 1;
                    let mut msg = LiveMessage::text(
                        format!("fake-poll-sent-{sent_counter}"),
                        chat_jid,
                        None,
                        "",
                        1_700_000_300 + i64::try_from(sent_counter).unwrap_or(0),
                        true,
                    );
                    msg.kind = crate::LiveMessageKind::Poll {
                        question,
                        options: options.into_iter().map(|o| (o, 0)).collect(),
                        multiple,
                        my_votes: Vec::new(),
                        correct: None,
                    };
                    polls.insert(msg.id.clone(), msg.clone());
                    events.emit(BackendEvent::MessageUpsert(msg));
                }
                BackendCommand::VotePoll { chat_jid: _, message_id, options } => {
                    if let Some(msg) = polls.get_mut(&message_id) {
                        if let crate::LiveMessageKind::Poll { options: opts, my_votes, .. } =
                            &mut msg.kind
                        {
                            // Replace-set semantics (the contract on
                            // BackendCommand::VotePoll): the payload
                            // is my new full vote set, not a sequence
                            // of toggle clicks. Recompute the counts
                            // as old counts minus my old votes plus
                            // my new votes, so an empty set retracts
                            // and a grown multi-select set keeps the
                            // votes it already had.
                            for old in my_votes.iter() {
                                if let Some(entry) = opts.get_mut(*old) {
                                    entry.1 = entry.1.saturating_sub(1);
                                }
                            }
                            for idx in &options {
                                if let Some(entry) = opts.get_mut(*idx) {
                                    entry.1 = entry.1.saturating_add(1);
                                }
                            }
                            *my_votes = options;
                        }
                        events.emit(BackendEvent::MessageUpsert(msg.clone()));
                    }
                }
                BackendCommand::React { chat_jid, message_id, emoji, .. } => {
                    events.emit(BackendEvent::ReactionUpdate {
                        chat_jid,
                        message_id,
                        actor: "Me".to_owned(),
                        emoji,
                    });
                }
                BackendCommand::MarkAsRead { chat_jid, message_ids, .. } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("mark_as_read:{chat_jid}:{}", message_ids.len()));
                    }
                }
                BackendCommand::SetTyping { chat_jid, typing } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("typing:{chat_jid}:{typing}"));
                    }
                }
                BackendCommand::EditMessage { chat_jid, message_id, new_body } => {
                    events.emit(BackendEvent::MessageEdited { chat_jid, message_id, new_body });
                }
                BackendCommand::RevokeMessage { chat_jid, message_id } => {
                    events.emit(BackendEvent::MessageRevoked { chat_jid, message_id });
                }
                BackendCommand::SetPrivacy { category, value } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("privacy:{category}={value}"));
                    }
                    privacy_state.insert(category, value);
                    events.emit(BackendEvent::PrivacyState(privacy_state.clone()));
                }
                BackendCommand::ForwardMessage { from_chat_jid, message_id, to_chat_jid } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("forward:{from_chat_jid}:{message_id}->{to_chat_jid}"));
                    }
                    let source = message_state
                        .get(&(from_chat_jid.clone(), message_id.clone()))
                        .cloned()
                        .or_else(|| polls.values().find(|m| m.id == message_id).cloned());
                    match source {
                        Some(msg) if msg.view_once => events.emit(BackendEvent::Error {
                            message: "View-once media cannot be forwarded.".to_owned(),
                        }),
                        Some(mut msg) => {
                            sent_counter += 1;
                            msg.id = format!("fake-fwd-{sent_counter}");
                            msg.chat_jid = to_chat_jid;
                            msg.from_me = true;
                            msg.sender = None;
                            msg.forwarded = true;
                            msg.timestamp =
                                1_700_000_400 + i64::try_from(sent_counter).unwrap_or(0);
                            message_state
                                .insert((msg.chat_jid.clone(), msg.id.clone()), msg.clone());
                            events.emit(BackendEvent::MessageUpsert(msg));
                        }
                        None => events.emit(BackendEvent::Error {
                            message: "That message is not part of the test-mode script.".to_owned(),
                        }),
                    }
                }
                BackendCommand::StarMessage { chat_jid, message_id, star, .. } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("star:{chat_jid}:{message_id}:{star}"));
                    }
                    let key = (chat_jid.clone(), message_id.clone());
                    let entry = message_state
                        .get(&key)
                        .cloned()
                        .or_else(|| polls.values().find(|m| m.id == message_id).cloned());
                    if let Some(mut msg) = entry {
                        msg.starred = star;
                        message_state.insert(key, msg.clone());
                        if matches!(msg.kind, crate::LiveMessageKind::Poll { .. }) {
                            polls.insert(msg.id.clone(), msg.clone());
                        }
                        events.emit(BackendEvent::MessageUpsert(msg));
                    }
                }
                BackendCommand::PinMessage { chat_jid, message_id, pin, .. } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("pin_message:{chat_jid}:{message_id}:{pin}"));
                    }
                    let key = (chat_jid.clone(), message_id.clone());
                    if let Some(msg) = message_state.get_mut(&key) {
                        msg.pinned = pin;
                        let msg = msg.clone();
                        events.emit(BackendEvent::MessageUpsert(msg));
                    }
                }
                BackendCommand::ChatAction { chat_jid, action } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("chat_action:{chat_jid}:{action:?}"));
                    }
                    if let Some(chat) = chat_state.get_mut(&chat_jid) {
                        match action {
                            crate::ChatAction::Archive => chat.archived = true,
                            crate::ChatAction::Unarchive => chat.archived = false,
                            crate::ChatAction::Pin => chat.pinned = true,
                            crate::ChatAction::Unpin => chat.pinned = false,
                            crate::ChatAction::Mute => chat.muted = true,
                            crate::ChatAction::Unmute => chat.muted = false,
                        }
                        let chat = chat.clone();
                        events.emit(BackendEvent::ChatUpsert(chat));
                    }
                }
                BackendCommand::BlockContact { chat_jid, block } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("block:{chat_jid}:{block}"));
                    }
                    if block {
                        blocked_state.insert(chat_jid);
                    } else {
                        blocked_state.remove(&chat_jid);
                    }
                    let mut list: Vec<String> = blocked_state.iter().cloned().collect();
                    list.sort();
                    events.emit(BackendEvent::Blocklist(list));
                }
                BackendCommand::CreateGroup { name, participants } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("create_group:{name}:{}", participants.len()));
                    }
                    group_counter += 1;
                    let new_jid = format!("200000000{group_counter}@g.us");
                    group_state.insert(
                        new_jid.clone(),
                        participants.iter().map(|p| (p.clone(), String::new(), false)).collect(),
                    );
                    chat_state.insert(
                        new_jid.clone(),
                        LiveChat {
                            jid: new_jid.clone(),
                            title: name.clone(),
                            kind: LiveChatKind::Group,
                            preview: "Group created".to_owned(),
                            preview_sender: None,
                            timestamp: 1_700_000_500,
                            unread: 0,
                            archived: false,
                            pinned: false,
                            ephemeral_secs: 0,
                            muted: false,
                        },
                    );
                    events.emit(BackendEvent::ChatUpsert(LiveChat {
                        jid: new_jid,
                        title: name,
                        kind: LiveChatKind::Group,
                        preview: "Group created".to_owned(),
                        preview_sender: None,
                        timestamp: 1_700_000_500,
                        unread: 0,
                        archived: false,
                        pinned: false,
                        ephemeral_secs: 0,
                        muted: false,
                    }));
                }
                BackendCommand::GroupParticipants { chat_jid, participants, op } => {
                    if let Some(log) = &command_log {
                        log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(
                            format!(
                                "group_participants:{chat_jid}:{op:?}:{}",
                                participants.join(",")
                            ),
                        );
                    }
                    let members =
                        group_state.entry(chat_jid.clone()).or_insert_with(default_members);
                    match op {
                        crate::ParticipantOp::Add => {
                            for p in &participants {
                                if !members.iter().any(|(j, _, _)| j == p) {
                                    members.push((p.clone(), String::new(), false));
                                }
                            }
                        }
                        crate::ParticipantOp::Remove => {
                            members.retain(|(j, _, _)| !participants.contains(j));
                        }
                        crate::ParticipantOp::Promote => {
                            for m in members.iter_mut() {
                                if participants.contains(&m.0) {
                                    m.2 = true;
                                }
                            }
                        }
                        crate::ParticipantOp::Demote => {
                            for m in members.iter_mut() {
                                if participants.contains(&m.0) {
                                    m.2 = false;
                                }
                            }
                        }
                    }
                    events.emit(BackendEvent::GroupMembers { chat_jid, members: members.clone() });
                }
                BackendCommand::SetGroupInfo { chat_jid, subject, description } => {
                    if let Some(log) = &command_log {
                        log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(
                            format!(
                                "group_info:{chat_jid}:{}:{}",
                                subject.as_deref().unwrap_or("-"),
                                description.as_deref().unwrap_or("-")
                            ),
                        );
                    }
                    if let (Some(subject), Some(chat)) = (subject, chat_state.get_mut(&chat_jid)) {
                        chat.title = subject;
                        let chat = chat.clone();
                        events.emit(BackendEvent::ChatUpsert(chat));
                    }
                }
                BackendCommand::FetchGroupMembers { chat_jid } => {
                    let members =
                        group_state.entry(chat_jid.clone()).or_insert_with(default_members).clone();
                    events.emit(BackendEvent::GroupMembers { chat_jid: chat_jid.clone(), members });
                    events.emit(BackendEvent::GroupApprovalState {
                        enabled: approval_state.get(&chat_jid).copied().unwrap_or(false),
                        chat_jid: chat_jid.clone(),
                    });
                    let (announce, locked) =
                        mode_state.get(&chat_jid).copied().unwrap_or((false, false));
                    events.emit(BackendEvent::GroupModesState { chat_jid, announce, locked });
                }
                BackendCommand::SetGroupAnnounce { chat_jid, announce } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("group_announce:{chat_jid}:{announce}"));
                    }
                    let modes = mode_state.entry(chat_jid.clone()).or_insert((false, false));
                    modes.0 = announce;
                    let (announce, locked) = *modes;
                    events.emit(BackendEvent::GroupModesState { chat_jid, announce, locked });
                }
                BackendCommand::SetGroupLocked { chat_jid, locked } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("group_locked:{chat_jid}:{locked}"));
                    }
                    let modes = mode_state.entry(chat_jid.clone()).or_insert((false, false));
                    modes.1 = locked;
                    let (announce, locked) = *modes;
                    events.emit(BackendEvent::GroupModesState { chat_jid, announce, locked });
                }
                BackendCommand::FetchGroupInviteLink { chat_jid, reset } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("invite_link:{chat_jid}:{reset}"));
                    }
                    if reset {
                        *invite_resets.entry(chat_jid.clone()).or_insert(0) += 1;
                    }
                    let generation = invite_resets.get(&chat_jid).copied().unwrap_or(0);
                    events.emit(BackendEvent::GroupInviteLink {
                        link: format!(
                            "https://chat.whatsapp.com/{}",
                            fake_invite_code(&chat_jid, generation)
                        ),
                        chat_jid,
                    });
                }
                BackendCommand::SetMembershipApproval { chat_jid, enabled } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("membership_approval:{chat_jid}:{enabled}"));
                    }
                    approval_state.insert(chat_jid.clone(), enabled);
                    events.emit(BackendEvent::GroupApprovalState { chat_jid, enabled });
                }
                BackendCommand::FetchJoinRequests { chat_jid } => {
                    let requests = join_request_state.get(&chat_jid).cloned().unwrap_or_default();
                    events.emit(BackendEvent::JoinRequests { chat_jid, requests });
                }
                BackendCommand::ReviewJoinRequest { chat_jid, participant, approve } => {
                    if let Some(log) = &command_log {
                        log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(
                            format!("review_join_request:{chat_jid}:{participant}:{approve}"),
                        );
                    }
                    let mut approved_name = None;
                    if let Some(list) = join_request_state.get_mut(&chat_jid) {
                        if let Some(pos) = list.iter().position(|(j, _)| j == &participant) {
                            let (_, name) = list.remove(pos);
                            if approve {
                                approved_name = Some(name);
                            }
                        }
                    }
                    if let Some(name) = approved_name {
                        let members =
                            group_state.entry(chat_jid.clone()).or_insert_with(default_members);
                        if !members.iter().any(|(j, _, _)| j == &participant) {
                            members.push((participant.clone(), name, false));
                            let members = members.clone();
                            events.emit(BackendEvent::GroupMembers {
                                chat_jid: chat_jid.clone(),
                                members,
                            });
                        }
                    }
                    let requests = join_request_state.get(&chat_jid).cloned().unwrap_or_default();
                    events.emit(BackendEvent::JoinRequests { chat_jid, requests });
                }
                BackendCommand::SetDefaultDisappearingTimer { duration_secs } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("default_disappearing:{duration_secs}"));
                    }
                }
                BackendCommand::JoinGroupByLink { link } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("join_group_by_link:{link}"));
                    }
                    let code = link.rsplit('/').next().unwrap_or("").trim();
                    if code.is_empty() {
                        events.emit(BackendEvent::Error {
                            message: "That invite link does not carry a code.".to_owned(),
                        });
                    } else {
                        let jid = "10000000077@g.us".to_owned();
                        let chat = LiveChat {
                            jid: jid.clone(),
                            title: "Joined via link".to_owned(),
                            kind: LiveChatKind::Group,
                            preview: "Group joined".to_owned(),
                            preview_sender: None,
                            timestamp: 1_700_000_600,
                            unread: 0,
                            archived: false,
                            pinned: false,
                            ephemeral_secs: 0,
                            muted: false,
                        };
                        chat_state.insert(jid, chat.clone());
                        events.emit(BackendEvent::ChatUpsert(chat));
                    }
                }
                BackendCommand::LeaveGroup { chat_jid } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("leave_group:{chat_jid}"));
                    }
                    if let Some(chat) = chat_state.get_mut(&chat_jid) {
                        chat.archived = true;
                        let chat = chat.clone();
                        events.emit(BackendEvent::ChatUpsert(chat));
                    }
                }
                BackendCommand::DownloadMedia { .. } => {
                    events.emit(BackendEvent::Error {
                        message: "Test mode has no downloadable media: its messages are scripted."
                            .to_owned(),
                    });
                }
                BackendCommand::Disconnect => {
                    events.emit(BackendEvent::LoggedOut);
                }
                BackendCommand::Shutdown => {
                    call_alive.store(false, std::sync::atomic::Ordering::Relaxed);
                    return Ok(());
                }
            }
        }
        call_alive.store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

/// One simulated call in the fake's phase machine.
#[derive(Debug)]
struct FakeCall {
    call_id: String,
    chat_jid: String,
    outgoing: bool,
    video: bool,
    phase: crate::CallPhase,
    seconds: u32,
    /// Ticks (250 ms) spent in the current phase.
    ticks: u32,
}

/// FakeBackend call state, shared between the command loop and
/// the ticker thread.
#[derive(Debug, Default)]
struct FakeCallState {
    current: Option<FakeCall>,
    /// Total calls started or received (id source).
    counter: u64,
    /// Call-log message ids.
    log_counter: u64,
}

/// Walks the fake call phase machine: outgoing calls ring for
/// about a second, connect, go active and tick the duration once
/// a second; accepted incoming calls connect the same way.
fn fake_call_ticker(
    state: std::sync::Arc<std::sync::Mutex<FakeCallState>>,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: EventSink,
) {
    while alive.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let mut emit: Option<BackendEvent> = None;
        {
            let mut guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(call) = &mut guard.current {
                call.ticks += 1;
                match call.phase {
                    crate::CallPhase::Ringing if call.outgoing && call.ticks >= 4 => {
                        call.phase = crate::CallPhase::Connecting;
                        call.ticks = 0;
                        emit = Some(BackendEvent::CallChanged {
                            call_id: call.call_id.clone(),
                            chat_jid: call.chat_jid.clone(),
                            phase: crate::CallPhase::Connecting,
                            seconds: 0,
                            reason: None,
                        });
                    }
                    crate::CallPhase::Connecting if call.ticks >= 3 => {
                        call.phase = crate::CallPhase::Active;
                        call.ticks = 0;
                        emit = Some(BackendEvent::CallChanged {
                            call_id: call.call_id.clone(),
                            chat_jid: call.chat_jid.clone(),
                            phase: crate::CallPhase::Active,
                            seconds: 0,
                            reason: None,
                        });
                    }
                    crate::CallPhase::Active if call.ticks % 4 == 0 => {
                        call.seconds += 1;
                        emit = Some(BackendEvent::CallChanged {
                            call_id: call.call_id.clone(),
                            chat_jid: call.chat_jid.clone(),
                            phase: crate::CallPhase::Active,
                            seconds: call.seconds,
                            reason: None,
                        });
                    }
                    _ => {}
                }
            }
        }
        if let Some(event) = emit {
            events.emit(event);
        }
    }
}

/// Emits the Ended transition plus the call-log entry in the
/// chat, the way a real call leaves a line in the transcript.
fn emit_call_end(events: &EventSink, state: &mut FakeCallState, call: &FakeCall, reason: &str) {
    events.emit(BackendEvent::CallChanged {
        call_id: call.call_id.clone(),
        chat_jid: call.chat_jid.clone(),
        phase: crate::CallPhase::Ended,
        seconds: call.seconds,
        reason: Some(reason.to_owned()),
    });
    state.log_counter += 1;
    let kind_label = if call.video { "Video call" } else { "Voice call" };
    let body = if call.seconds > 0 || call.phase == crate::CallPhase::Active {
        format!("{kind_label} · {}:{:02}", call.seconds / 60, call.seconds % 60)
    } else if call.outgoing {
        format!("{kind_label} (no answer)")
    } else if reason == "Declined" {
        format!("{kind_label} (declined)")
    } else {
        format!("Missed {}", kind_label.to_lowercase())
    };
    let mut msg = LiveMessage::text(
        format!("fake-call-log-{}", state.log_counter),
        call.chat_jid.clone(),
        if call.outgoing { None } else { Some("Sarah Miller".to_owned()) },
        body,
        1_700_000_400 + i64::try_from(state.log_counter).unwrap_or(0),
        call.outgoing,
    );
    msg.kind = crate::LiveMessageKind::Other("call".to_owned());
    events.emit(BackendEvent::MessageUpsert(msg));
}

/// The fake account's privacy settings at link time, spelled
/// the way the real backend's `privacy_map` reports them. The
/// run loop keeps the live copy: SetPrivacy mutates it and the
/// full map is re-emitted, mirroring the real backend's
/// refetch-after-change.
fn baseline_privacy_state() -> std::collections::BTreeMap<String, String> {
    let mut privacy = std::collections::BTreeMap::new();
    privacy.insert("last".to_owned(), "contacts".to_owned());
    privacy.insert("online".to_owned(), "match_last_seen".to_owned());
    privacy.insert("profile".to_owned(), "contacts".to_owned());
    privacy.insert("status".to_owned(), "contacts".to_owned());
    privacy.insert("groupadd".to_owned(), "contacts".to_owned());
    privacy.insert("readreceipts".to_owned(), "all".to_owned());
    privacy
}

fn emit_linked_script(
    events: &EventSink,
    call_state: &std::sync::Arc<std::sync::Mutex<FakeCallState>>,
    privacy: &std::collections::BTreeMap<String, String>,
) {
    events.emit(BackendEvent::Connected { account: "Fake account (not WhatsApp)".to_owned() });
    for chat in scripted_chats() {
        events.emit(BackendEvent::ChatUpsert(chat));
    }
    for msg in scripted_messages() {
        events.emit(BackendEvent::MessageUpsert(msg));
    }
    for msg in scripted_group_messages() {
        events.emit(BackendEvent::MessageUpsert(msg));
    }
    // The scripted promo message is deleted for everyone; the
    // app shows the placeholder.
    events.emit(BackendEvent::MessageRevoked {
        chat_jid: FAKE_DM_JID.to_owned(),
        message_id: "fake-promo-1".to_owned(),
    });
    // The account's privacy state, as the real backend reports
    // it after fetching the account's settings.
    events.emit(BackendEvent::PrivacyState(privacy.clone()));
    // Baseline blocklist, as the real backend reports it after
    // fetching the account's list at connect: nobody blocked.
    events.emit(BackendEvent::Blocklist(Vec::new()));
    // One scripted incoming voice call from the DM contact, so
    // the whole call surface (ring, accept, talk, end, log entry)
    // is exercised in test mode without a phone. Registered in
    // the call state first so AcceptCall finds it.
    {
        let mut state = call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.current.is_none() {
            state.counter += 1;
            let call_id = format!("fake-call-{}", state.counter);
            state.current = Some(FakeCall {
                call_id: call_id.clone(),
                chat_jid: FAKE_DM_JID.to_owned(),
                outgoing: false,
                video: false,
                phase: crate::CallPhase::Ringing,
                seconds: 0,
                ticks: 0,
            });
            events.emit(BackendEvent::IncomingCall {
                call_id,
                chat_jid: FAKE_DM_JID.to_owned(),
                video: false,
            });
        }
    }
    // The scripted peer starts typing after the message flow, so
    // the header typing state is exercised in test mode too.
    events.emit(BackendEvent::TypingReceived { chat_jid: FAKE_DM_JID.to_owned(), typing: true });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{spawn_backend, BackendChannels, WakeFn};
    use std::sync::Arc;
    use std::time::Duration;

    fn start(fake: FakeBackend) -> BackendChannels {
        let wake: WakeFn = Arc::new(|| {});
        spawn_backend(
            move || Box::new(fake.clone()) as Box<dyn Backend>,
            BackendConfig {
                data_dir: std::path::PathBuf::from("test-data"),
                account_id: "default".to_owned(),
                db_key: [0; 32],
            },
            wake,
        )
    }

    fn next(ch: &BackendChannels) -> BackendEvent {
        ch.events.recv_timeout(Duration::from_secs(5)).expect("event must arrive")
    }

    #[test]
    fn qr_flow_emits_payload_then_linked_script() {
        let ch = start(FakeBackend::auto());
        ch.commands.send(BackendCommand::ConnectQr).unwrap();
        assert_eq!(next(&ch), BackendEvent::QrCode { payload: FAKE_QR_PAYLOAD.to_owned() });
        assert!(matches!(next(&ch), BackendEvent::Connected { .. }));
        for _ in 0..6 {
            let _ = next(&ch);
        }
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    #[test]
    fn waiting_fake_stops_at_the_qr() {
        let ch = start(FakeBackend::waiting());
        ch.commands.send(BackendCommand::ConnectQr).unwrap();
        assert!(matches!(next(&ch), BackendEvent::QrCode { .. }));
        assert!(
            ch.events.recv_timeout(Duration::from_millis(200)).is_err(),
            "no Connected may follow in waiting mode"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    #[test]
    fn demo_script_is_ordered_bounded_and_finite() {
        let script = demo_script();
        assert!((12..=20).contains(&script.len()), "a dozen-plus steps, got {}", script.len());
        assert!(script[0].at >= Duration::from_secs(3), "the script starts a few seconds in");
        assert!(
            script.last().expect("non-empty").at <= Duration::from_secs(180),
            "the script wraps up within a few minutes"
        );
        for pair in script.windows(2) {
            assert!(pair[0].at <= pair[1].at, "steps are scheduled in order");
        }
    }

    #[test]
    fn demo_script_covers_the_feature_surface() {
        let script = demo_script();
        let has = |pred: &dyn Fn(&BackendEvent) -> bool| script.iter().any(|s| pred(&s.event));
        // Typing resolves into a message in the same chat: a
        // typing-on step precedes the DM's first scripted upsert.
        let typing_on = script
            .iter()
            .position(|s| {
                matches!(s.event, BackendEvent::TypingReceived { typing: true, ref chat_jid } if chat_jid == FAKE_DM_JID)
            })
            .expect("typing starts in the DM");
        let first_dm_upsert = script
            .iter()
            .position(|s| {
                matches!(s.event, BackendEvent::MessageUpsert(ref m) if m.chat_jid == FAKE_DM_JID && m.id.starts_with("fake-live"))
            })
            .expect("a scripted DM message arrives");
        assert!(typing_on < first_dm_upsert, "typing comes before the message it announces");
        // Reactions, poll votes, media, a quote reply, a channel
        // post and the delivered-to-read receipt progression.
        assert!(has(&|e| matches!(e, BackendEvent::ReactionUpdate { .. })), "reactions land");
        assert!(
            has(
                &|e| matches!(e, BackendEvent::MessageUpsert(m) if matches!(m.kind, crate::LiveMessageKind::Poll { .. }))
            ),
            "poll votes arrive as an upsert"
        );
        assert!(
            has(
                &|e| matches!(e, BackendEvent::MessageUpsert(m) if matches!(m.kind, crate::LiveMessageKind::Media { .. }))
            ),
            "media arrives"
        );
        assert!(
            has(&|e| matches!(e, BackendEvent::MessageUpsert(m) if m.quoted_body.is_some())),
            "a quote reply arrives"
        );
        assert!(
            has(&|e| matches!(e, BackendEvent::MessageUpsert(m) if m.chat_jid == FAKE_CHANNEL_JID)),
            "the channel posts"
        );
        assert!(
            has(&|e| matches!(e, BackendEvent::Receipt { kind: crate::ReceiptKind::Read, .. })),
            "ticks reach read"
        );
        // New scripted messages continue the linked script's
        // clock instead of jumping backwards.
        for s in &script {
            if let BackendEvent::MessageUpsert(m) = &s.event {
                if m.id.starts_with("fake-live") {
                    assert!(m.timestamp > 1_700_000_300, "{} predates the linked script", m.id);
                }
            }
        }
    }

    #[test]
    fn only_the_demo_variant_plays_the_script() {
        assert!(FakeBackend::demo().demo_script);
        assert!(!FakeBackend::auto().demo_script);
        assert!(!FakeBackend::waiting().demo_script);
        assert!(!FakeBackend::waiting().auto_connect, "waiting behavior is unchanged");
    }

    #[test]
    fn demo_backend_emits_scripted_events_after_linking() {
        let ch = start(FakeBackend::demo());
        ch.commands.send(BackendCommand::ConnectQr).unwrap();
        // Drain until the first scheduled message lands (the
        // script opens with typing at 4s, the message at 7s; the
        // budget is generous for a loaded machine).
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "the scripted message never arrived");
            match ch.events.recv_timeout(left) {
                Ok(BackendEvent::MessageUpsert(m)) if m.id == "fake-live-1" => break,
                Ok(_) => {}
                Err(e) => panic!("event channel failed while waiting for the script: {e}"),
            }
        }
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    #[test]
    fn pair_code_flow_is_deterministic() {
        let ch = start(FakeBackend::auto());
        ch.commands
            .send(BackendCommand::ConnectPairCode { phone: "12025550123".to_owned() })
            .unwrap();
        let code = match next(&ch) {
            BackendEvent::PairCode { code } => code,
            other => panic!("expected PairCode, got {other:?}"),
        };
        assert_eq!(code.len(), 8);
        assert_eq!(code, fake_pair_code("12025550123"));
        assert!(code.chars().all(|c| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c)));
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    #[test]
    fn send_text_echoes_and_replies_in_the_dm() {
        let ch = start(FakeBackend::auto());
        ch.commands
            .send(BackendCommand::SendText {
                chat_jid: FAKE_DM_JID.to_owned(),
                body: "hello".to_owned(),
                quoted: None,
                ephemeral_secs: 0,
            })
            .unwrap();
        // The send is acknowledged with its protocol ID before the
        // echo upsert lands, mirroring the live backend.
        let sent_id = match next(&ch) {
            BackendEvent::MessageSent { chat_jid, message_id } => {
                assert_eq!(chat_jid, FAKE_DM_JID);
                message_id
            }
            other => panic!("expected MessageSent, got {other:?}"),
        };
        match next(&ch) {
            BackendEvent::MessageUpsert(m) => {
                assert!(m.from_me);
                assert_eq!(m.body, "hello");
                assert_eq!(m.id, sent_id);
            }
            other => panic!("expected echo upsert, got {other:?}"),
        }
        // Receipt progression (sent → delivered → read) legitimately
        // interleaves with the scripted reply; skip receipts and
        // wait for the reply upsert itself.
        loop {
            match next(&ch) {
                BackendEvent::Receipt { .. } => {}
                BackendEvent::MessageUpsert(m) => {
                    assert!(!m.from_me);
                    break;
                }
                other => panic!("expected scripted reply, got {other:?}"),
            }
        }
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }
}
