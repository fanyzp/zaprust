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
    Backend, BackendCommand, BackendConfig, BackendEvent, EventSink, LiveChannel, LiveChat,
    LiveChatKind, LiveMessage, LiveStatus, OwnProfile, SavedContact,
};

/// The payload the fake backend asks the UI to render as a QR code.
/// Obviously not a real linking payload.
pub const FAKE_QR_PAYLOAD: &str = "2@FAKE0PAYLOAD,AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=,\
     BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=,\
     CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC=,1";

/// JID of the fake account itself (the "me" of the script).
pub const FAKE_OWN_JID: &str = "10000000000@s.whatsapp.net";
/// LID of the fake account itself: the account's other address
/// form, so a self chat addressed under it is exercised the
/// same way a real account's can be.
pub const FAKE_OWN_LID: &str = "10000000000@lid";
/// JID of the scripted DM chat.
pub const FAKE_DM_JID: &str = "10000000001@s.whatsapp.net";
/// JID of the scripted marathon chat: 600 generated messages
/// for exercising the conversation stream's windowing. Only
/// present when the ZAPRUST_FAKE_MARATHON environment variable
/// is set, so the standard scripted roster (and every
/// screenshot pair built on it) is unchanged by default.
pub const FAKE_MARATHON_JID: &str = "10000000005@s.whatsapp.net";

/// Whether the marathon fixture joins the scripted roster.
fn marathon_enabled() -> bool {
    std::env::var_os("ZAPRUST_FAKE_MARATHON").is_some()
}

/// Whether the lock-fixture chats are scripted: a locked DM
/// and a blocked promo DM, for the chat-lock and
/// blocked-contacts surfaces. Gated like the marathon chat
/// so the standard fixtures (and every screenshot pair built
/// on them) are unchanged unless a capture asks for them.
fn lock_fixture_enabled() -> bool {
    std::env::var_os("ZAPRUST_FAKE_LOCKED").is_some()
}

/// JID of the scripted locked DM (lock-fixture gate only).
pub const FAKE_LOCKED_DM_JID: &str = "10000000031@s.whatsapp.net";
/// JID of the scripted blocked promo DM (lock-fixture gate
/// only): the account blocked it before this session, so the
/// connect-time blocklist carries it.
pub const FAKE_PROMO_JID: &str = "10000000032@s.whatsapp.net";
/// Whether the bidi-fixture chats are scripted: an Arabic
/// and a Hebrew DM, for the RTL display surfaces. Gated like
/// the lock fixture so the standard fixtures are unchanged
/// unless a capture asks for them.
fn bidi_fixture_enabled() -> bool {
    std::env::var_os("ZAPRUST_FAKE_BIDI").is_some()
}

/// JID of the scripted Arabic DM (bidi-fixture gate only).
pub const FAKE_BIDI_AR_JID: &str = "10000000033@s.whatsapp.net";
/// JID of the scripted Hebrew DM (bidi-fixture gate only).
pub const FAKE_BIDI_HE_JID: &str = "10000000034@s.whatsapp.net";
/// Whether the business fixture is scripted: a verified
/// business DM with a profile, a two-page catalog, one
/// collection and an order message, plus two synced labels
/// and two synced quick replies delivered at connect. Gated
/// like the other fixture families so the standard fixtures
/// (and every screenshot pair built on them) are unchanged
/// unless a capture asks for them.
fn business_fixture_enabled() -> bool {
    std::env::var_os("ZAPRUST_FAKE_BUSINESS").is_some()
}

/// JID of the scripted business DM (business-fixture gate
/// only).
pub const FAKE_BIZ_JID: &str = "10000000035@s.whatsapp.net";
/// Label ids of the scripted synced labels (business-fixture
/// gate only).
pub const FAKE_LABEL_URGENT_ID: &str = "fake-label-urgent";
/// Second scripted label id.
pub const FAKE_LABEL_VIP_ID: &str = "fake-label-vip";
/// Order id of the scripted order message (business-fixture
/// gate only).
pub const FAKE_ORDER_ID: &str = "fake-order-1";
/// Order token of the scripted order message.
pub const FAKE_ORDER_TOKEN: &str = "fake-order-token";

/// Which scripted incoming call the linked script rings with,
/// from `ZAPRUST_FAKE_CALL`: "video" rings a 1:1 video call
/// from the DM contact, "group" a group voice call, and
/// "group-video" a group video call. Unset (the default) rings
/// the plain 1:1 voice call every existing capture is built
/// on. Gated like the other fixture families so the standard
/// fixtures are unchanged unless a capture asks for them.
fn call_fixture() -> Option<&'static str> {
    match std::env::var("ZAPRUST_FAKE_CALL").ok().as_deref() {
        Some("video") => Some("video"),
        Some("group") => Some("group"),
        Some("group-video") => Some("group-video"),
        _ => None,
    }
}

/// Whether the waiting-room fixture is scripted: during any
/// active call, a guest arrives in the waiting room (the fake
/// hosts every call it is asked about) and Admit/Deny answer.
/// From `ZAPRUST_FAKE_WAITING`, gated like the other fixture
/// families.
fn waiting_fixture_enabled() -> bool {
    std::env::var_os("ZAPRUST_FAKE_WAITING").is_some()
}

/// Whether the stress fixture is scripted: one DM whose
/// messages are adversarial layout content (unbroken tokens,
/// multi-paragraph text, long quotes, RTL runs, long media
/// captions, long interactive labels) plus a few long-sender
/// group messages, for visual QA of the conversation stream.
/// From `ZAPRUST_FAKE_STRESS`, gated like the other fixture
/// families so the standard fixtures (and every screenshot
/// pair built on them) are unchanged unless a capture asks
/// for them.
fn stress_fixture_enabled() -> bool {
    std::env::var_os("ZAPRUST_FAKE_STRESS").is_some()
}

/// JID of the scripted stress DM (stress-fixture gate only).
pub const FAKE_STRESS_JID: &str = "10000000036@s.whatsapp.net";
/// Display name of the stress DM contact: deliberately far
/// longer than any real contact name, so chat rows, the
/// conversation header and quote blocks must truncate or
/// wrap it gracefully.
pub const FAKE_STRESS_NAME: &str =
    "Alexandria Konstantinopoulou Papadimitriou van der Berg Smith-Johnson";

/// JIDs of the scripted group call's other members and of the
/// waiting-room guest.
pub const FAKE_MEMBER2_JID: &str = "10000000004@s.whatsapp.net";
/// Third scripted group member.
pub const FAKE_MEMBER3_JID: &str = "10000000007@s.whatsapp.net";
/// The waiting-room guest's JID.
pub const FAKE_GUEST_JID: &str = "10000000013@s.whatsapp.net";

/// JID of the scripted group chat.
pub const FAKE_GROUP_JID: &str = "10000000002@g.us";
/// JID of the scripted channel.
pub const FAKE_CHANNEL_JID: &str = "10000000003@newsletter";
/// JID of the scripted community parent group.
pub const FAKE_COMMUNITY_JID: &str = "10000000004@g.us";
/// JID of the scripted group that belongs to no community: the
/// link picker's candidate in demo and tests.
pub const FAKE_UNLINKED_GROUP_JID: &str = "10000000009@g.us";

/// The deterministic fake backend. See the module docs.
#[derive(Debug, Clone)]
pub struct FakeBackend {
    /// Whether pairing flows complete on their own (scripted scan)
    /// instead of waiting forever at the QR/code step.
    pub auto_connect: bool,
    /// Optional command log (privacy/state-machine tests): the fake
    /// appends one line per command it records, among them
    /// MarkAsRead / SetTyping / SetPrivacy / SetPresenceSubscription
    /// and PostTextStatus.
    pub command_log: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
    /// Whether the scheduled incoming-event script ([`demo_script`])
    /// plays after the linked script. Only the app demo enables
    /// this; tests keep the deterministic linked script alone.
    pub demo_script: bool,
    /// One-shot failure injection: command kind names (the
    /// `BackendCommand` variant names, e.g. `"VotePoll"`). The
    /// next command of a named kind fails with the same typed
    /// failure event the real backend emits for it, and the name
    /// is consumed. This is what makes the app's rollback paths
    /// rehearsable at all: without it the fake could only ever
    /// succeed. The app demo seeds the list from the
    /// `ZAPRUST_FAKE_FAIL` environment variable (comma-separated
    /// kind names) so failure states are reachable in QA captures.
    pub fail_next: Vec<&'static str>,
}

/// The failure-injection list the app demo starts with, from
/// `ZAPRUST_FAKE_FAIL` (comma-separated command kind names).
/// Names are leaked to `&'static str`: the list is built once at
/// startup and lives as long as the process.
fn env_failures() -> Vec<&'static str> {
    std::env::var("ZAPRUST_FAKE_FAIL")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| &*Box::leak(name.to_owned().into_boxed_str()))
        .collect()
}

impl FakeBackend {
    /// A fake that completes pairing on its own, for tests and the
    /// "linked" screenshots.
    #[must_use]
    pub fn auto() -> Self {
        Self { auto_connect: true, command_log: None, demo_script: false, fail_next: Vec::new() }
    }

    /// A fake that waits at the pairing step, for the QR/code
    /// screenshots and for tests of the waiting states.
    #[must_use]
    pub fn waiting() -> Self {
        Self { auto_connect: false, command_log: None, demo_script: false, fail_next: Vec::new() }
    }

    /// A fake that records privacy-relevant commands into `log`.
    #[must_use]
    pub fn auto_with_log(log: std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> Self {
        Self {
            auto_connect: true,
            command_log: Some(log),
            demo_script: false,
            fail_next: Vec::new(),
        }
    }

    /// The app demo: links on its own, then plays the scheduled
    /// incoming-event script so the demo stays alive after the
    /// linked script lands.
    #[must_use]
    pub fn demo() -> Self {
        Self { auto_connect: true, command_log: None, demo_script: true, fail_next: env_failures() }
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::auto()
    }
}

/// A deterministic fixture picture (a valid PNG) for the
/// demo world's avatar and sticker fetches: a light disc on a
/// two-tone field, hue derived from the seed so different
/// identities get visibly different pictures. Demo and test
/// fixtures only; the real backend serves real bytes.
/// One second of silence as a real Ogg/Opus stream (the same
/// encoding WhatsApp voice notes use), served by the demo
/// script's voice fetches so the playback path can be
/// rehearsed end to end in demo (P2-44). Generated once with
/// ffmpeg's libopus from a silent source; it decodes with the
/// app's own player (its decode-of-silence test pins the
/// format contract).
const FIXTURE_VOICE_OPUS: &[u8] = &[
    0x4f, 0x67, 0x67, 0x53, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x72, 0x91,
    0xd9, 0x3b, 0x00, 0x00, 0x00, 0x00, 0xed, 0xe8, 0xbc, 0x6b, 0x01, 0x13, 0x4f, 0x70, 0x75, 0x73,
    0x48, 0x65, 0x61, 0x64, 0x01, 0x01, 0x38, 0x01, 0x80, 0xbb, 0x00, 0x00, 0x00, 0x00, 0x00, 0x4f,
    0x67, 0x67, 0x53, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x72, 0x91, 0xd9,
    0x3b, 0x01, 0x00, 0x00, 0x00, 0x1f, 0xb7, 0x9b, 0xa4, 0x01, 0x3e, 0x4f, 0x70, 0x75, 0x73, 0x54,
    0x61, 0x67, 0x73, 0x0d, 0x00, 0x00, 0x00, 0x4c, 0x61, 0x76, 0x66, 0x36, 0x32, 0x2e, 0x31, 0x32,
    0x2e, 0x31, 0x30, 0x32, 0x01, 0x00, 0x00, 0x00, 0x1d, 0x00, 0x00, 0x00, 0x65, 0x6e, 0x63, 0x6f,
    0x64, 0x65, 0x72, 0x3d, 0x4c, 0x61, 0x76, 0x63, 0x36, 0x32, 0x2e, 0x32, 0x38, 0x2e, 0x31, 0x30,
    0x32, 0x20, 0x6c, 0x69, 0x62, 0x6f, 0x70, 0x75, 0x73, 0x4f, 0x67, 0x67, 0x53, 0x00, 0x00, 0x80,
    0xbb, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x72, 0x91, 0xd9, 0x3b, 0x02, 0x00, 0x00, 0x00, 0x30,
    0x38, 0x80, 0xb8, 0x32, 0x0f, 0x11, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12,
    0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12,
    0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12,
    0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x78, 0x0b, 0xe4, 0xc1, 0x36, 0xec, 0xc5, 0x8d, 0x8c, 0x49,
    0x46, 0x99, 0x9d, 0xac, 0x00, 0x78, 0x07, 0xc9, 0x72, 0x27, 0xe1, 0x44, 0xea, 0x55, 0xf1, 0xf0,
    0xc0, 0xd5, 0xde, 0x97, 0x8c, 0x90, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12,
    0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0,
    0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9,
    0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79,
    0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07,
    0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20,
    0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d,
    0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67,
    0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa,
    0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12,
    0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0,
    0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9,
    0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79,
    0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07,
    0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20,
    0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d,
    0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67,
    0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa,
    0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12,
    0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0,
    0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9,
    0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79,
    0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07,
    0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20,
    0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d,
    0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67,
    0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa,
    0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12,
    0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0,
    0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9,
    0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79,
    0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07,
    0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20,
    0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d,
    0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67,
    0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa,
    0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12,
    0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0,
    0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9,
    0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79,
    0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07,
    0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20,
    0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d,
    0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67,
    0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa,
    0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12,
    0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0,
    0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9,
    0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79,
    0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07,
    0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20,
    0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67, 0xf3, 0x1d,
    0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67,
    0xf3, 0x1d, 0xc1, 0x20, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa,
    0xef, 0x67, 0xf3, 0x1d, 0xc1, 0x20, 0x4f, 0x67, 0x67, 0x53, 0x00, 0x04, 0xb8, 0xbc, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x72, 0x91, 0xd9, 0x3b, 0x03, 0x00, 0x00, 0x00, 0xa9, 0x7a, 0x61, 0x25,
    0x01, 0x12, 0x78, 0x07, 0xc9, 0x79, 0xc8, 0xc9, 0x57, 0xc0, 0xa2, 0x12, 0x23, 0xfa, 0xef, 0x67,
    0xf3, 0x1d, 0xc1, 0x20,
];

fn fixture_png(seed: &str) -> Vec<u8> {
    const SIZE: usize = 96;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let hue = (hash % 360) as f32;
    let (br, bg, bb) = hsv_to_rgb(hue, 0.45, 0.55);
    let (fr, fg, fb) = hsv_to_rgb(hue, 0.30, 0.85);
    // Raw scanlines, one filter byte per row.
    let mut raw = Vec::with_capacity(SIZE * (1 + SIZE * 3));
    for y in 0..SIZE {
        raw.push(0u8);
        for x in 0..SIZE {
            let dx = x as i32 - 48;
            let dy = y as i32 - 42;
            let in_disc = dx * dx + dy * dy <= 26 * 26;
            let shade = 1.0 - (y as f32 / SIZE as f32) * 0.25;
            let (r, g, b) =
                if in_disc { (fr, fg, fb) } else { (br * shade, bg * shade, bb * shade) };
            raw.push((r * 255.0) as u8);
            raw.push((g * 255.0) as u8);
            raw.push((b * 255.0) as u8);
        }
    }
    // zlib stream of stored (uncompressed) blocks.
    let mut idat = vec![0x78, 0x01];
    let chunks = raw.chunks(65_535);
    let total = chunks.len();
    for (i, block) in chunks.enumerate() {
        let last = i + 1 == total;
        idat.push(u8::from(last));
        let len = block.len() as u16;
        idat.extend_from_slice(&len.to_le_bytes());
        idat.extend_from_slice(&(!len).to_le_bytes());
        idat.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &raw {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    idat.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut png = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(SIZE as u32).to_be_bytes());
    ihdr.extend_from_slice(&(SIZE as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    png_chunk(&mut png, b"IHDR", &ihdr);
    png_chunk(&mut png, b"IDAT", &idat);
    png_chunk(&mut png, b"IEND", &[]);
    png
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (r + v - c, g + v - c, b + v - c)
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc: u32 = 0xffff_ffff;
    for byte in kind.iter().chain(data.iter()) {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    out.extend_from_slice(&(crc ^ 0xffff_ffff).to_be_bytes());
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

/// The scripted group's default member rows: (jid, name,
/// admin, member label). Emma carries a label so the member
/// label surface (row 65) is visible in demo mode.
fn default_members() -> Vec<crate::GroupMemberRow> {
    vec![
        ("10000000001@s.whatsapp.net".to_owned(), "Sarah Miller".to_owned(), true, None),
        ("10000000004@s.whatsapp.net".to_owned(), "Mike Johnson".to_owned(), false, None),
        (
            "10000000007@s.whatsapp.net".to_owned(),
            "Emma Wilson".to_owned(),
            false,
            Some("Design".to_owned()),
        ),
    ]
}

/// The short kind name the command log records for an
/// interactive send.
fn spec_log_name(spec: &crate::InteractiveSpec) -> &'static str {
    match spec {
        crate::InteractiveSpec::Buttons { .. } => "buttons",
        crate::InteractiveSpec::List { .. } => "list",
        crate::InteractiveSpec::Carousel { .. } => "carousel",
    }
}

/// The echo body and kind an interactive send produces in the
/// fake: the same shapes the live builder puts on the wire,
/// with the same positional choice ids.
fn interactive_echo_parts(spec: &crate::InteractiveSpec) -> (String, crate::LiveMessageKind) {
    match spec {
        crate::InteractiveSpec::Buttons { text, footer, buttons } => (
            text.clone(),
            crate::LiveMessageKind::Buttons {
                footer: footer.clone(),
                buttons: buttons
                    .iter()
                    .enumerate()
                    .map(|(index, label)| crate::LiveButton {
                        id: (index + 1).to_string(),
                        text: label.clone(),
                        action: None,
                    })
                    .collect(),
            },
        ),
        crate::InteractiveSpec::List { title, description, button_label, sections } => {
            let mut next_row = 0usize;
            let sections = sections
                .iter()
                .map(|(section_title, rows)| crate::LiveListSection {
                    title: section_title.clone(),
                    rows: rows
                        .iter()
                        .map(|(row_title, row_description)| {
                            next_row += 1;
                            crate::LiveListRow {
                                id: next_row.to_string(),
                                title: row_title.clone(),
                                description: row_description.clone(),
                            }
                        })
                        .collect(),
                })
                .collect();
            (
                title.clone(),
                crate::LiveMessageKind::ListMsg {
                    description: description.clone(),
                    button_label: button_label.clone(),
                    footer: String::new(),
                    sections,
                },
            )
        }
        crate::InteractiveSpec::Carousel { body, cards } => (
            body.clone(),
            crate::LiveMessageKind::Carousel { body: body.clone(), cards: cards.clone() },
        ),
    }
}

/// The scripted chats, in display order.
#[must_use]
pub fn scripted_chats() -> Vec<LiveChat> {
    let mut chats = vec![
        LiveChat {
            jid: FAKE_DM_JID.to_owned(),
            title: "Sarah Miller (live demo)".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "Sure, I'll send the notes in a bit".to_owned(),
            preview_sender: None,
            timestamp: 1_700_000_300,
            unread: 1,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
            from_history: false,
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
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: Some(FAKE_COMMUNITY_JID.to_owned()),
            is_parent: false,
            from_history: false,
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
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
            from_history: false,
        },
        LiveChat {
            jid: FAKE_OWN_CHANNEL_JID.to_owned(),
            title: "Weekend Builds (live demo)".to_owned(),
            kind: LiveChatKind::Channel,
            preview: "Weekend build: a screenshot tour of the new channels screen".to_owned(),
            preview_sender: None,
            timestamp: 1_699_999_000,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
            from_history: false,
        },
        LiveChat {
            jid: FAKE_COMMUNITY_JID.to_owned(),
            title: "Weekend Community (live demo)".to_owned(),
            kind: LiveChatKind::Group,
            preview: "Welcome to the community".to_owned(),
            preview_sender: None,
            timestamp: 1_700_000_050,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: true,
            from_history: false,
        },
        LiveChat {
            jid: FAKE_UNLINKED_GROUP_JID.to_owned(),
            title: "Trail Crew (live demo)".to_owned(),
            kind: LiveChatKind::Group,
            preview: "Trail cleanup moved to Saturday".to_owned(),
            preview_sender: None,
            timestamp: 1_700_000_020,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
            from_history: false,
        },
    ];
    if lock_fixture_enabled() {
        chats.push(LiveChat {
            jid: FAKE_LOCKED_DM_JID.to_owned(),
            title: "Emma Collins (live demo)".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "The spare key is under the blue pot".to_owned(),
            preview_sender: None,
            timestamp: 1_700_000_250,
            unread: 2,
            archived: false,
            locked: true,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
            from_history: false,
        });
        chats.push(LiveChat {
            jid: FAKE_PROMO_JID.to_owned(),
            title: "Promo Blast (live demo)".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "You won a prize! Claim it now".to_owned(),
            preview_sender: None,
            timestamp: 1_699_999_500,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
            from_history: false,
        });
    }
    if bidi_fixture_enabled() {
        chats.push(LiveChat {
            jid: FAKE_BIDI_AR_JID.to_owned(),
            title: "Layla Haddad (live demo)".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "سنلتقي غدا at 5 قرب المكتبة".to_owned(),
            preview_sender: None,
            timestamp: 1_700_000_260,
            unread: 1,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
            from_history: false,
        });
        chats.push(LiveChat {
            jid: FAKE_BIDI_HE_JID.to_owned(),
            title: "Noa Levi (live demo)".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "פגישה tomorrow ב-9 בבוקר".to_owned(),
            preview_sender: None,
            timestamp: 1_700_000_255,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
            from_history: false,
        });
    }
    if business_fixture_enabled() {
        chats.push(LiveChat {
            jid: FAKE_BIZ_JID.to_owned(),
            title: "Bumi Roast Coffee (live demo)".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "It ships tomorrow morning, tracking follows by noon.".to_owned(),
            preview_sender: None,
            timestamp: 1_700_000_430,
            unread: 1,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
            from_history: false,
        });
    }
    if stress_fixture_enabled() {
        chats.push(LiveChat {
            jid: FAKE_STRESS_JID.to_owned(),
            title: format!("{FAKE_STRESS_NAME} (live demo)"),
            kind: LiveChatKind::Dm,
            preview: format!("https://example.com/{}", "a".repeat(120)),
            preview_sender: None,
            timestamp: 1_700_000_900,
            unread: 3,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
            from_history: false,
        });
    }
    if marathon_enabled() {
        chats.push(LiveChat {
            jid: FAKE_MARATHON_JID.to_owned(),
            title: "Marathon Thread (live demo)".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "Marathon message 600: the finish line".to_owned(),
            preview_sender: None,
            timestamp: 1_699_000_000 + 599 * 1_500,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
            from_history: false,
        });
    }
    chats
}

/// The fake account's saved contacts: its scripted DM
/// partners under their fake names, the set the real backend
/// reports from its identity book at link time. The fake
/// roster is phone-form only, so no entry carries an
/// alternate JID form.
pub fn scripted_saved_contacts() -> Vec<SavedContact> {
    scripted_chats()
        .into_iter()
        .filter(|chat| chat.kind == LiveChatKind::Dm)
        .map(|chat| SavedContact { jid: chat.jid, name: chat.title, alt_jids: Vec::new() })
        .collect()
}

/// JID of the scripted discoverable channel: not followed,
/// found through its invite code.
pub const FAKE_DISCOVERABLE_CHANNEL_JID: &str = "10000000007@newsletter";
/// JID of the scripted channel this account owns.
pub const FAKE_OWN_CHANNEL_JID: &str = "10000000008@newsletter";
/// Invite code of the scripted followed channel.
pub const FAKE_CHANNEL_INVITE: &str = "CityNewsDemo";
/// Invite code of the scripted discoverable channel.
pub const FAKE_DISCOVERABLE_INVITE: &str = "HarborWeatherDemo";
/// Invite code of the scripted owned channel.
pub const FAKE_OWN_INVITE: &str = "WeekendBuildsDemo";

/// The scripted channel directory: the channels the fake
/// account follows or owns, as FetchChannels reports them.
/// The discoverable channel is deliberately absent: it is
/// found by invite lookup, not by listing.
#[must_use]
pub fn scripted_channels() -> Vec<LiveChannel> {
    vec![
        LiveChannel {
            jid: FAKE_CHANNEL_JID.to_owned(),
            name: "City News (live demo)".to_owned(),
            description: "City news, roadworks and events, posted by the city desk.".to_owned(),
            followers: 12_483,
            verified: true,
            role: crate::LiveChannelRole::Follower,
            muted: false,
            invite_code: Some(FAKE_CHANNEL_INVITE.to_owned()),
            created_at: 1_680_000_000,
        },
        LiveChannel {
            jid: FAKE_OWN_CHANNEL_JID.to_owned(),
            name: "Weekend Builds (live demo)".to_owned(),
            description: "Build logs from the workshop.".to_owned(),
            followers: 312,
            verified: false,
            role: crate::LiveChannelRole::Owner,
            muted: false,
            invite_code: Some(FAKE_OWN_INVITE.to_owned()),
            created_at: 1_690_000_000,
        },
    ]
}

/// The scripted discoverable channel, as an invite lookup
/// reports it: real metadata, Guest role (not followed).
#[must_use]
pub fn scripted_discoverable_channel() -> LiveChannel {
    LiveChannel {
        jid: FAKE_DISCOVERABLE_CHANNEL_JID.to_owned(),
        name: "Harbor Weather (live demo)".to_owned(),
        description: "Harbor forecasts and tide tables, twice a day.".to_owned(),
        followers: 3_208,
        verified: false,
        role: crate::LiveChannelRole::Guest,
        muted: false,
        invite_code: Some(FAKE_DISCOVERABLE_INVITE.to_owned()),
        created_at: 1_685_000_000,
    }
}

/// The scripted channel history: the updates a history fetch
/// serves, carrying the server IDs channel reactions and
/// votes key on. City News gets a text/photo/poll run plus
/// the announcement the roster preview quotes; the owned
/// channel gets two own posts, the edit and revoke targets.
#[must_use]
pub fn scripted_channel_history() -> Vec<LiveMessage> {
    let mut photo = LiveMessage::text(
        "fake-ch-2",
        FAKE_CHANNEL_JID,
        Some("City News".to_owned()),
        "The new riverfront park, opened this morning",
        1_699_950_000,
        false,
    );
    photo.server_id = Some(102);
    photo.kind = crate::LiveMessageKind::Media {
        media: crate::MediaKind::Photo,
        caption: "The new riverfront park, opened this morning".to_owned(),
        seconds: 0,
        size_bytes: 204_000,
    };
    photo.reactions = vec![("❤️".to_owned(), 32)];
    let mut poll = LiveMessage::text(
        "fake-ch-4",
        FAKE_CHANNEL_JID,
        Some("City News".to_owned()),
        "",
        1_699_995_000,
        false,
    );
    poll.server_id = Some(104);
    poll.kind = crate::LiveMessageKind::Poll {
        question: "Should the night market run every weekend?".to_owned(),
        options: vec![("Every weekend".to_owned(), 412), ("Once a month".to_owned(), 187)],
        multiple: false,
        my_votes: Vec::new(),
        correct: None,
    };
    let mut first = LiveMessage::text(
        "fake-ch-1",
        FAKE_CHANNEL_JID,
        Some("City News".to_owned()),
        "Morning briefing: roadworks on Harbor Avenue start Monday",
        1_699_900_000,
        false,
    );
    first.server_id = Some(101);
    first.reactions = vec![("👍".to_owned(), 14)];
    let mut third = LiveMessage::text(
        "fake-ch-3",
        FAKE_CHANNEL_JID,
        Some("City News".to_owned()),
        "City council approves the night market permit",
        1_699_990_000,
        false,
    );
    third.server_id = Some(103);
    let mut fifth = LiveMessage::text(
        "fake-ch-5",
        FAKE_CHANNEL_JID,
        Some("City News".to_owned()),
        "Announcement: the holiday events schedule is out",
        1_700_000_100,
        false,
    );
    fifth.server_id = Some(105);
    fifth.reactions = vec![("👍".to_owned(), 58), ("🎉".to_owned(), 9)];
    let mut own_first = LiveMessage::text(
        "fake-own-1",
        FAKE_OWN_CHANNEL_JID,
        None,
        "Build log: the new release pipeline is green",
        1_699_980_000,
        true,
    );
    own_first.server_id = Some(201);
    let mut own_second = LiveMessage::text(
        "fake-own-2",
        FAKE_OWN_CHANNEL_JID,
        None,
        "Weekend build: a screenshot tour of the new channels screen",
        1_699_999_000,
        true,
    );
    own_second.server_id = Some(202);
    own_second.reactions = vec![("👍".to_owned(), 3)];
    vec![first, photo, third, poll, fifth, own_first, own_second]
}

/// A real 1-second H.264 MP4 (128x96, 10 fps), generated with
/// ffmpeg's testsrc: the same generated fixture the app crate's
/// player tests demux, copied so the fake can serve video bytes
/// and downloads (P2-44) that actually decode and play.
const FIXTURE_MP4: &[u8] = include_bytes!("testdata/tiny.mp4");

/// The scripted status feed: one text, one photo and one video
/// post from scripted people, plus one earlier post of the
/// account's own, so the Updates screen's feed, viewer and My
/// Status delete flow are all exercisable in test mode. Media
/// references are `None`, the same convention as the scripted
/// chat media: the demo fetch path serves fixture bytes by
/// message ID, and the waiting fake fails honestly.
#[must_use]
pub fn scripted_statuses() -> Vec<LiveStatus> {
    let base = LiveStatus {
        id: String::new(),
        author_jid: String::new(),
        author_name: None,
        body: String::new(),
        timestamp: 0,
        from_me: false,
        kind: crate::LiveMessageKind::Text,
        background_argb: None,
        reactions: Vec::new(),
        my_reaction: None,
        photo_ref: None,
        photo_thumbnail: None,
        video_ref: None,
        video_thumbnail: None,
        poster_ref: None,
        video_width: None,
        video_height: None,
    };
    vec![
        LiveStatus {
            id: "fake-status-sarah-1".to_owned(),
            author_jid: FAKE_DM_JID.to_owned(),
            author_name: Some("Sarah Miller".to_owned()),
            body: "At the lake house this weekend".to_owned(),
            timestamp: 1_700_000_240,
            background_argb: Some(0xFF7A_4A21),
            ..base.clone()
        },
        LiveStatus {
            id: "fake-status-emma-1".to_owned(),
            author_jid: "10000000007@s.whatsapp.net".to_owned(),
            author_name: Some("Emma Wilson".to_owned()),
            body: "Morning run, done".to_owned(),
            timestamp: 1_700_000_180,
            kind: crate::LiveMessageKind::Media {
                media: crate::MediaKind::Photo,
                caption: "Morning run, done".to_owned(),
                seconds: 0,
                size_bytes: 144_000,
            },
            ..base.clone()
        },
        LiveStatus {
            id: "fake-status-mike-1".to_owned(),
            author_jid: "10000000004@s.whatsapp.net".to_owned(),
            author_name: Some("Mike Johnson".to_owned()),
            body: "Trail cam from yesterday".to_owned(),
            timestamp: 1_700_000_120,
            kind: crate::LiveMessageKind::Media {
                media: crate::MediaKind::Video,
                caption: "Trail cam from yesterday".to_owned(),
                seconds: 12,
                size_bytes: 2_400_000,
            },
            video_thumbnail: Some(fixture_png("fake-status-mike-1")),
            video_width: Some(128),
            video_height: Some(96),
            ..base.clone()
        },
        LiveStatus {
            id: "fake-status-mine-1".to_owned(),
            author_jid: FAKE_OWN_JID.to_owned(),
            body: "Shipping day".to_owned(),
            timestamp: 1_700_000_060,
            from_me: true,
            background_argb: Some(0xFF00_A884),
            ..base
        },
    ]
}

/// The scripted messages for the DM chat, including one poll, one
/// link message with a scripted preview, and one message that a
/// later scripted revoke targets.
#[must_use]
/// The marathon fixture: 600 deterministic messages over
/// ten days, alternating sides, with varied lengths, one poll
/// and one event, so the windowed stream path (and its day
/// dividers) can be exercised end to end. Message bodies carry
/// their ordinal, so a screenshot can be checked against the
/// fixture: message N says "Marathon message N".
pub fn scripted_marathon_messages() -> Vec<LiveMessage> {
    let base = 1_699_000_000_i64;
    let mut out = Vec::new();
    for i in 1..=600_i64 {
        let from_me = i % 2 == 0;
        let body = match i % 10 {
            0 => format!(
                "Marathon message {i}. This is one of the long ones: it wraps over several lines so the stream measures a tall bubble, with a second sentence to fill the width and a third for good measure."
            ),
            5 => format!("Marathon message {i}.\nA second line follows the first."),
            _ => format!("Marathon message {i}"),
        };
        let mut msg = LiveMessage::text(
            format!("marathon-{i}"),
            FAKE_MARATHON_JID,
            if from_me { None } else { Some("Marathon Partner".to_owned()) },
            body,
            base + (i - 1) * 1_500,
            from_me,
        );
        if !from_me {
            msg.sender_jid = Some(FAKE_MARATHON_JID.to_owned());
        }
        if i == 300 {
            msg.kind = crate::LiveMessageKind::Poll {
                question: "Marathon checkpoint: keep going?".to_owned(),
                options: vec![("Yes".to_owned(), 4), ("Almost there".to_owned(), 2)],
                multiple: false,
                my_votes: vec![0],
                correct: None,
            };
            msg.poll_secret = Some(vec![3u8; 32]);
            msg.poll_creator = Some(FAKE_MARATHON_JID.to_owned());
        }
        if i == 450 {
            msg.kind = crate::LiveMessageKind::Event {
                name: "Marathon finish line party".to_owned(),
                description: "At the park gate".to_owned(),
                start_time: base + 700_000,
                end_time: base + 710_800,
                location: "City park".to_owned(),
                join_link: String::new(),
                scheduled_call: false,
                extra_guests: true,
                canceled: false,
                my_rsvp: Some(crate::EventRsvp::Going),
                going: 3,
                maybe: 1,
                not_going: 0,
            };
            msg.event_secret = Some(vec![5u8; 32]);
            msg.event_creator = Some(FAKE_MARATHON_JID.to_owned());
            msg.event_ballots = vec![
                (FAKE_MARATHON_JID.to_owned(), crate::EventRsvp::Going),
                ("10000000007@s.whatsapp.net".to_owned(), crate::EventRsvp::Going),
                ("10000000008@s.whatsapp.net".to_owned(), crate::EventRsvp::Maybe),
            ];
        }
        out.push(msg);
    }
    out
}

/// The scripted business's profile (business-fixture gate
/// only): a coffee roastery with hours, categories and a
/// website, shaped like the library's read.
fn fixture_business_profile() -> crate::LiveBusinessProfile {
    let day = |day: &str, mode: &str, open: Option<u32>, close: Option<u32>| {
        crate::LiveBusinessDayHours {
            day: day.to_owned(),
            mode: mode.to_owned(),
            open_minutes: open,
            close_minutes: close,
        }
    };
    crate::LiveBusinessProfile {
        description: "Small-batch coffee roastery and cafe. Beans are roasted every Tuesday."
            .to_owned(),
        email: Some("hello@bumiroast.example".to_owned()),
        websites: vec!["https://bumiroast.example".to_owned()],
        categories: vec![
            crate::LiveBusinessCategory { id: "cat-cafe".to_owned(), name: "Cafe".to_owned() },
            crate::LiveBusinessCategory {
                id: "cat-roastery".to_owned(),
                name: "Coffee roastery".to_owned(),
            },
        ],
        address: Some("Jl. Kopi No. 8, Balikpapan".to_owned()),
        hours_timezone: Some("Asia/Makassar".to_owned()),
        hours: vec![
            day("mon", "specific_hours", Some(480), Some(1020)),
            day("tue", "specific_hours", Some(480), Some(1020)),
            day("wed", "specific_hours", Some(480), Some(1020)),
            day("thu", "specific_hours", Some(480), Some(1020)),
            day("fri", "specific_hours", Some(480), Some(1020)),
            day("sat", "specific_hours", Some(540), Some(840)),
            day("sun", "open_24h", None, None),
        ],
    }
}

/// The fake account's own business profile (business-fixture
/// gate only): the account runs a small shop of its own.
fn fixture_own_business_profile() -> crate::LiveBusinessProfile {
    crate::LiveBusinessProfile {
        description: "The fake account's own shop.".to_owned(),
        email: None,
        websites: Vec::new(),
        categories: vec![crate::LiveBusinessCategory {
            id: "cat-cafe".to_owned(),
            name: "Cafe".to_owned(),
        }],
        address: None,
        hours_timezone: None,
        hours: Vec::new(),
    }
}

/// One scripted catalog product.
fn fixture_product(
    id: &str,
    name: &str,
    price: Option<i64>,
    sale: Option<i64>,
    availability: Option<&str>,
) -> crate::LiveProduct {
    crate::LiveProduct {
        id: id.to_owned(),
        name: Some(name.to_owned()),
        description: None,
        price_amount_1000: price,
        sale_amount_1000: sale,
        price_currency: Some("IDR".to_owned()),
        availability: availability.map(str::to_owned),
        hidden: false,
        image_url: Some(format!("https://cdn.example/{id}.jpg")),
    }
}

/// One page of the scripted catalog: three products and a
/// cursor, then two more and the end.
fn fixture_catalog_page(after: Option<&str>) -> (Vec<crate::LiveProduct>, Option<String>) {
    match after {
        None => (
            vec![
                fixture_product(
                    "beans-arabica",
                    "Arabica Beans 200g",
                    Some(85_000_000),
                    None,
                    Some("IN_STOCK"),
                ),
                fixture_product(
                    "beans-robusta",
                    "Robusta Beans 200g",
                    Some(65_000_000),
                    None,
                    Some("IN_STOCK"),
                ),
                fixture_product(
                    "cold-brew",
                    "Cold Brew Bottle 250ml",
                    Some(35_000_000),
                    Some(28_000_000),
                    Some("IN_STOCK"),
                ),
            ],
            Some("page-2".to_owned()),
        ),
        Some(_) => (
            vec![
                fixture_product(
                    "drip-set",
                    "Drip Coffee Set",
                    Some(240_000_000),
                    None,
                    Some("OUT_OF_STOCK"),
                ),
                fixture_product("gift-card", "Gift Card", None, None, None),
            ],
            None,
        ),
    }
}

/// The scripted collections: one, with its products inline.
fn fixture_collections() -> Vec<crate::LiveCollection> {
    vec![crate::LiveCollection {
        id: Some("starter-kits".to_owned()),
        name: Some("Starter kits".to_owned()),
        products: vec![
            fixture_product(
                "drip-set",
                "Drip Coffee Set",
                Some(240_000_000),
                None,
                Some("OUT_OF_STOCK"),
            ),
            fixture_product(
                "beans-arabica",
                "Arabica Beans 200g",
                Some(85_000_000),
                None,
                Some("IN_STOCK"),
            ),
        ],
    }]
}

/// The scripted order's details, matching the order
/// message's totals.
fn fixture_order() -> crate::LiveOrder {
    let item = |name: &str, price: i64, quantity: i64| crate::LiveOrderItem {
        name: Some(name.to_owned()),
        price_amount_1000: Some(price),
        price_currency: Some("IDR".to_owned()),
        quantity: Some(quantity),
        variant: Vec::new(),
    };
    crate::LiveOrder {
        items: vec![
            item("Arabica Beans 200g", 85_000_000, 2),
            item("Cold Brew Bottle 250ml", 35_000_000, 1),
        ],
        subtotal_amount_1000: Some(205_000_000),
        total_amount_1000: Some(205_000_000),
        currency: Some("IDR".to_owned()),
    }
}

/// The scripted business DM's messages (business-fixture
/// gate only): a greeting, a text and the order message.
pub fn scripted_business_messages() -> Vec<LiveMessage> {
    let mut order = LiveMessage::text(
        "fake-order-msg-1",
        FAKE_BIZ_JID,
        Some("Bumi Roast Coffee".to_owned()),
        "Order",
        1_700_000_420,
        false,
    );
    order.kind = crate::LiveMessageKind::Order {
        order_id: FAKE_ORDER_ID.to_owned(),
        token: FAKE_ORDER_TOKEN.to_owned(),
        item_count: 3,
        total_amount_1000: 205_000_000,
        currency: "IDR".to_owned(),
        status: "ACCEPTED".to_owned(),
    };
    let mut messages = vec![
        LiveMessage::text(
            "fake-biz-1",
            FAKE_BIZ_JID,
            Some("Bumi Roast Coffee".to_owned()),
            "Hi! Our catalog is in the profile if you want to browse.",
            1_700_000_400,
            false,
        ),
        order,
        LiveMessage::text(
            "fake-biz-2",
            FAKE_BIZ_JID,
            Some("Bumi Roast Coffee".to_owned()),
            "It ships tomorrow morning, tracking follows by noon.",
            1_700_000_430,
            false,
        ),
    ];
    // The scripted business is a verified one: its messages
    // carry the certified name and the top asserted tier, the
    // same pair a real verified business's envelopes carry.
    for message in &mut messages {
        message.sender_verified_name = Some("Bumi Roast Coffee".to_owned());
        message.sender_verified_level = Some("high".to_owned());
    }
    messages
}

/// The scripted DM conversation the fake serves on link.
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
    // The scripted poll carries a (placeholder) creation secret
    // and its creator, like a real creation message does, so the
    // app archives them and re-registers the secret next session.
    // The fake does no poll crypto; the bytes are a stand-in.
    poll.poll_secret = Some(vec![0xFA; 32]);
    poll.poll_creator = Some(FAKE_DM_JID.to_owned());
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
        thumbnail: None,
    });
    link.reactions = vec![("👍".to_owned(), 1)];
    // Sarah's event: she is going (her ballot is scripted with
    // it, so the card's tally and the archived ballots agree),
    // I have not answered yet. The creation secret is a
    // placeholder like the poll's: the fake does no event
    // crypto, the bytes are a stand-in the app archives and
    // re-registers next session.
    let mut event = LiveMessage::text(
        "fake-event-1",
        FAKE_DM_JID,
        Some("Sarah Miller".to_owned()),
        "",
        1_700_000_280,
        false,
    );
    event.kind = crate::LiveMessageKind::Event {
        name: "Rooftop dinner".to_owned(),
        description: "Bringing the city guide notes; booking is under my name.".to_owned(),
        start_time: 1_700_086_400,
        end_time: 1_700_097_200,
        location: "Skyline Rooftop, Harbor Avenue".to_owned(),
        join_link: String::new(),
        scheduled_call: false,
        extra_guests: true,
        canceled: false,
        my_rsvp: None,
        going: 1,
        maybe: 0,
        not_going: 0,
    };
    event.event_secret = Some(vec![5u8; 32]);
    event.event_creator = Some(FAKE_DM_JID.to_owned());
    event.event_ballots = vec![(FAKE_DM_JID.to_owned(), crate::EventRsvp::Going)];
    // My own event in the same chat: the edit and cancel
    // targets for the demo, with no answers yet.
    let mut my_event =
        LiveMessage::text("fake-event-mine-1", FAKE_DM_JID, None, "", 1_700_000_285, true);
    my_event.kind = crate::LiveMessageKind::Event {
        name: "Release retrospective".to_owned(),
        description: String::new(),
        start_time: 1_700_172_800,
        end_time: 1_700_176_400,
        location: String::new(),
        join_link: String::new(),
        scheduled_call: true,
        extra_guests: false,
        canceled: false,
        my_rsvp: None,
        going: 0,
        maybe: 0,
        not_going: 0,
    };
    my_event.event_secret = Some(vec![5u8; 32]);
    my_event.event_creator = Some(FAKE_OWN_JID.to_owned());
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
        event,
        my_event,
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

/// The scripted messages of the lock-fixture chats (the
/// locked DM and the blocked promo DM), delivered with the
/// linked script under the same gate as the chats
/// themselves.
#[must_use]
pub fn scripted_lock_fixture_messages() -> Vec<LiveMessage> {
    vec![
        LiveMessage::text(
            "fake-locked-1",
            FAKE_LOCKED_DM_JID,
            Some("Emma Collins".to_owned()),
            "Moved the house paperwork to the drawer by the door",
            1_700_000_240,
            false,
        ),
        LiveMessage::text(
            "fake-locked-2",
            FAKE_LOCKED_DM_JID,
            Some("Emma Collins".to_owned()),
            "The spare key is under the blue pot",
            1_700_000_250,
            false,
        ),
        LiveMessage::text(
            "fake-promo-9",
            FAKE_PROMO_JID,
            Some("Promo Blast".to_owned()),
            "You won a prize! Claim it now",
            1_699_999_500,
            false,
        ),
    ]
}

/// The scripted messages for the bidi fixture (an Arabic and
/// a Hebrew DM), delivered with the linked script under the
/// same gate as the chats themselves.
#[must_use]
pub fn scripted_bidi_fixture_messages() -> Vec<LiveMessage> {
    vec![
        LiveMessage::text(
            "fake-bidi-ar-1",
            FAKE_BIDI_AR_JID,
            Some("Layla Haddad".to_owned()),
            "مرحبا! وصلت الملفات، شكرا لك",
            1_700_000_240,
            false,
        ),
        LiveMessage::text(
            "fake-bidi-ar-2",
            FAKE_BIDI_AR_JID,
            Some("Layla Haddad".to_owned()),
            "سنلتقي غدا at 5 قرب المكتبة، والمبلغ 1234 ريال جاهز",
            1_700_000_260,
            false,
        ),
        LiveMessage::text(
            "fake-bidi-he-1",
            FAKE_BIDI_HE_JID,
            Some("Noa Levi".to_owned()),
            "שלום עולם, ההודעה הזו בעברית",
            1_700_000_245,
            false,
        ),
        LiveMessage::text(
            "fake-bidi-he-2",
            FAKE_BIDI_HE_JID,
            Some("Noa Levi".to_owned()),
            "פגישה tomorrow ב-9 בבוקר",
            1_700_000_255,
            false,
        ),
    ]
}

/// The scripted messages of the stress DM (stress-fixture
/// gate only): adversarial layout content for visual QA of
/// the conversation stream. Every message is a case a real
/// chat can produce and a bubble must survive without
/// clipping: unbroken tokens, multi-paragraph text, long
/// quotes, RTL runs, emoji-only messages, long media
/// captions and file names, long interactive labels, an
/// order card, and a crowded reaction row.
#[must_use]
pub fn scripted_stress_messages() -> Vec<LiveMessage> {
    let name = Some(FAKE_STRESS_NAME.to_owned());
    let long_token = format!("https://example.com/{}", "a".repeat(180));
    let long_prose = "The community hall booking is confirmed for the whole weekend, \
        and the caretaker said we can arrive from seven in the morning to set up \
        the tables, the projector, and the coffee station before the first \
        workshop starts, as long as we leave the kitchen exactly as we found it \
        and take the recycling out on Sunday evening when the last session ends, \
        because another group has the hall on Monday morning and they complained \
        last time about the chairs being stacked in the wrong room.";
    let mut quote_long = LiveMessage::text(
        "fake-stress-5",
        FAKE_STRESS_JID,
        None,
        "Agreed, let us book it today.",
        1_700_000_640,
        true,
    );
    quote_long.quoted_sender = name.clone();
    quote_long.quoted_body = Some(long_prose.to_owned());
    let mut quote_token = LiveMessage::text(
        "fake-stress-6",
        FAKE_STRESS_JID,
        name.clone(),
        "That link again, for the record.",
        1_700_000_650,
        false,
    );
    quote_token.quoted_sender = name.clone();
    quote_token.quoted_body = Some(long_token.clone());
    let mut voice = LiveMessage::text(
        "fake-stress-11",
        FAKE_STRESS_JID,
        name.clone(),
        "Voice message",
        1_700_000_700,
        false,
    );
    voice.kind = crate::LiveMessageKind::Media {
        media: crate::MediaKind::Voice,
        caption: String::new(),
        seconds: 75,
        size_bytes: 912_000,
    };
    let audio_name =
        "Interview-recording-session-12-final-mix-unedited-take-three-with-room-tone.mp3";
    let mut audio = LiveMessage::text(
        "fake-stress-12",
        FAKE_STRESS_JID,
        name.clone(),
        audio_name,
        1_700_000_710,
        false,
    );
    audio.kind = crate::LiveMessageKind::Media {
        media: crate::MediaKind::Audio,
        caption: audio_name.to_owned(),
        seconds: 245,
        size_bytes: 5_900_000,
    };
    let doc_name = "Quarterly-financial-report-2026-final-version-revised-approved-signed-copy.pdf";
    let mut document = LiveMessage::text(
        "fake-stress-13",
        FAKE_STRESS_JID,
        name.clone(),
        doc_name,
        1_700_000_720,
        false,
    );
    document.kind = crate::LiveMessageKind::Media {
        media: crate::MediaKind::Document,
        caption: doc_name.to_owned(),
        seconds: 0,
        size_bytes: 4_800_000,
    };
    let long_caption = "The whole crew at the summit marker after the last climb of the \
        season, with the valley, the lake, the old lighthouse, and three very tired \
        dogs in the back row, taken minutes before the weather turned.";
    let mut photo = LiveMessage::text(
        "fake-stress-14",
        FAKE_STRESS_JID,
        name.clone(),
        long_caption,
        1_700_000_730,
        false,
    );
    photo.kind = crate::LiveMessageKind::Media {
        media: crate::MediaKind::Photo,
        caption: long_caption.to_owned(),
        seconds: 0,
        size_bytes: 3_400_000,
    };
    let mut video = LiveMessage::text(
        "fake-stress-15",
        FAKE_STRESS_JID,
        None,
        long_caption,
        1_700_000_740,
        true,
    );
    video.kind = crate::LiveMessageKind::Media {
        media: crate::MediaKind::Video,
        caption: long_caption.to_owned(),
        seconds: 95,
        size_bytes: 18_200_000,
    };
    let mut sticker = LiveMessage::text(
        "fake-stress-16",
        FAKE_STRESS_JID,
        name.clone(),
        "Sticker",
        1_700_000_750,
        false,
    );
    sticker.kind = crate::LiveMessageKind::Media {
        media: crate::MediaKind::Sticker,
        caption: String::new(),
        seconds: 0,
        size_bytes: 84_000,
    };
    let mut poll = LiveMessage::text(
        "fake-stress-17",
        FAKE_STRESS_JID,
        name.clone(),
        "",
        1_700_000_760,
        false,
    );
    poll.kind = crate::LiveMessageKind::Poll {
        question: "Which of the proposed weekends should we pick for the annual \
            neighborhood cleanup, the tool library inventory, and the shared \
            garden replanting day?"
            .to_owned(),
        options: vec![
            ("The first weekend of next month, starting early on Saturday morning".to_owned(), 4),
            ("The second weekend, with the rain date held on the Sunday after".to_owned(), 2),
            ("The last weekend of the month, combined with the street festival".to_owned(), 7),
            ("Split it across two weekends so nobody has to give up a full day".to_owned(), 1),
        ],
        multiple: true,
        my_votes: Vec::new(),
        correct: None,
    };
    poll.poll_secret = Some(vec![0xFA; 32]);
    poll.poll_creator = Some(FAKE_STRESS_JID.to_owned());
    let mut event = LiveMessage::text(
        "fake-stress-18",
        FAKE_STRESS_JID,
        name.clone(),
        "",
        1_700_000_770,
        false,
    );
    event.kind = crate::LiveMessageKind::Event {
        name: "Annual general meeting of the neighborhood association, the tool \
            library collective, and the community garden committee"
            .to_owned(),
        description: "Agenda: the yearly budget, the roof repair quotes, the new \
            booking rules for the hall, and the election of two board members. \
            Please read the attached report before the meeting so the discussion \
            can stay short."
            .to_owned(),
        start_time: 1_700_345_600,
        end_time: 1_700_352_800,
        location: "Community hall, second floor, Jalan Merdeka Raya number 128, \
            next to the old post office and across from the central market"
            .to_owned(),
        join_link: String::new(),
        scheduled_call: false,
        extra_guests: true,
        canceled: false,
        my_rsvp: None,
        going: 3,
        maybe: 1,
        not_going: 0,
    };
    event.event_secret = Some(vec![5u8; 32]);
    event.event_creator = Some(FAKE_STRESS_JID.to_owned());
    let mut buttons = LiveMessage::text(
        "fake-stress-19",
        FAKE_STRESS_JID,
        name.clone(),
        "The courier could not deliver the parcel today because the gate was \
            locked. How should we proceed with the delivery tomorrow?",
        1_700_000_780,
        false,
    );
    buttons.kind = crate::LiveMessageKind::Buttons {
        footer: "Choose one option; the courier sees your answer right away.".to_owned(),
        buttons: vec![
            crate::LiveButton {
                id: "1".to_owned(),
                text: "Deliver to the front desk instead".to_owned(),
                action: None,
            },
            crate::LiveButton {
                id: "2".to_owned(),
                text: "Leave it with the neighbor at number 12".to_owned(),
                action: None,
            },
            crate::LiveButton {
                id: "3".to_owned(),
                text: "Reschedule to next week, same time".to_owned(),
                action: None,
            },
        ],
    };
    let mut list = LiveMessage::text(
        "fake-stress-20",
        FAKE_STRESS_JID,
        name.clone(),
        "Workshop registration",
        1_700_000_790,
        false,
    );
    list.kind = crate::LiveMessageKind::ListMsg {
        description: "Pick the session you want to attend. Every session includes \
            materials, lunch, and a certificate, and places are limited to twenty \
            participants per room."
            .to_owned(),
        button_label: "See all available sessions".to_owned(),
        footer: "Registration closes on Friday evening.".to_owned(),
        sections: vec![crate::LiveListSection {
            title: "Saturday sessions in the main hall".to_owned(),
            rows: vec![
                crate::LiveListRow {
                    id: "1".to_owned(),
                    title: "Introduction to woodworking for complete beginners".to_owned(),
                    description: "Tools, safety, and your first cutting board".to_owned(),
                },
                crate::LiveListRow {
                    id: "2".to_owned(),
                    title: "Advanced joinery and furniture restoration workshop".to_owned(),
                    description: "Bring a small piece of furniture to work on".to_owned(),
                },
            ],
        }],
    };
    let mut carousel = LiveMessage::text(
        "fake-stress-21",
        FAKE_STRESS_JID,
        name.clone(),
        "",
        1_700_000_800,
        false,
    );
    carousel.kind = crate::LiveMessageKind::Carousel {
        body: "This month's featured rentals from the tool library, all serviced \
            and ready to borrow for up to two weeks at a time:"
            .to_owned(),
        cards: vec![
            crate::LiveCard {
                title: "Professional pressure washer with the extended lance".to_owned(),
                body: "Suitable for driveways, fences, and the community hall's \
                    outer walls; includes detergent tank and two nozzles."
                    .to_owned(),
                image: None,
                buttons: vec!["Reserve this item for next weekend".to_owned()],
            },
            crate::LiveCard {
                title: "Twelve-person event tent with side walls".to_owned(),
                body: "Sets up in about twenty minutes with two people; stakes, \
                    ropes, and a repair kit are in the bag."
                    .to_owned(),
                image: None,
                buttons: vec!["Check availability".to_owned(), "See the setup guide".to_owned()],
            },
        ],
    };
    let mut order = LiveMessage::text(
        "fake-stress-22",
        FAKE_STRESS_JID,
        name.clone(),
        "Order for the community kitchen restock: rice, cooking oil, coffee, \
            tea, sugar, flour, eggs, vegetables, chicken, spices, cleaning \
            supplies, and paper goods for the whole month.",
        1_700_000_810,
        false,
    );
    order.kind = crate::LiveMessageKind::Order {
        order_id: "fake-stress-order".to_owned(),
        token: concat!("fake-stress", "-token").to_owned(),
        item_count: 12,
        total_amount_1000: 1_250_000_000,
        currency: "IDR".to_owned(),
        status: "INQUIRY".to_owned(),
    };
    let mut contact = LiveMessage::text(
        "fake-stress-23",
        FAKE_STRESS_JID,
        name.clone(),
        "",
        1_700_000_820,
        false,
    );
    contact.kind = crate::LiveMessageKind::Contact {
        name: FAKE_STRESS_NAME.to_owned(),
        phone: "Contact card shared by Alexandria, including her work number".to_owned(),
    };
    let mut location = LiveMessage::text(
        "fake-stress-24",
        FAKE_STRESS_JID,
        name.clone(),
        "",
        1_700_000_830,
        false,
    );
    location.kind = crate::LiveMessageKind::Location {
        name: "The old lighthouse at the end of the harbor breakwater, past the \
            fish market and the ferry terminal, Balikpapan"
            .to_owned(),
        latitude: -1.2768,
        longitude: 116.8312,
    };
    let mut link = LiveMessage::text(
        "fake-stress-25",
        FAKE_STRESS_JID,
        name.clone(),
        "Full write-up here: https://example.com/reports/annual-neighborhood-report-2026",
        1_700_000_840,
        false,
    );
    link.link_preview = Some(crate::LivePreview {
        title: "Annual neighborhood report 2026: everything the association did, \
            spent, repaired, planted, and planned over the last twelve months"
            .to_owned(),
        description: "A complete record of the year's meetings, budgets, repair \
            work, garden harvests, tool library lending statistics, and the plans \
            the board approved for next year, with photos and charts throughout."
            .to_owned(),
        url: "https://example.com/reports/annual-neighborhood-report-2026".to_owned(),
        thumbnail: None,
    });
    let mut reacted = LiveMessage::text(
        "fake-stress-26",
        FAKE_STRESS_JID,
        name.clone(),
        "We passed the inspection, the hall is ours for another year.",
        1_700_000_850,
        false,
    );
    reacted.reactions = vec![
        ("👍".to_owned(), 128),
        ("❤️".to_owned(), 96),
        ("🎉".to_owned(), 54),
        ("🙏".to_owned(), 12),
        ("😮".to_owned(), 3),
    ];
    let mut flagged = LiveMessage::text(
        "fake-stress-27",
        FAKE_STRESS_JID,
        None,
        "Minutes from the last meeting are in the shared folder, under the \
            longest file name anyone has ever given a document.",
        1_700_000_860,
        true,
    );
    flagged.edited = true;
    flagged.forwarded = true;
    flagged.starred = true;
    let mut buttons_long = LiveMessage::text(
        "fake-stress-28",
        FAKE_STRESS_JID,
        name.clone(),
        "One more choice, with much longer labels this time:",
        1_700_000_870,
        false,
    );
    buttons_long.kind = crate::LiveMessageKind::Buttons {
        footer: "The caretaker answers in the evening, usually after the last \
            booking of the day is done."
            .to_owned(),
        buttons: vec![
            crate::LiveButton {
                id: "1".to_owned(),
                text: "Reserve the community hall for the whole weekend, including \
                    setup on Friday and cleanup on Monday morning"
                    .to_owned(),
                action: None,
            },
            crate::LiveButton {
                id: "2".to_owned(),
                text: "Ask the caretaker about the deposit, the keys, and the \
                    parking arrangement first"
                    .to_owned(),
                action: None,
            },
            crate::LiveButton { id: "3".to_owned(), text: "Cancel".to_owned(), action: None },
        ],
    };
    let mut list_long = LiveMessage::text(
        "fake-stress-29",
        FAKE_STRESS_JID,
        name.clone(),
        "Sessions with long section names",
        1_700_000_880,
        false,
    );
    list_long.kind = crate::LiveMessageKind::ListMsg {
        description: "Every section and row below carries a deliberately long \
            text, so the stacked lines have to wrap."
            .to_owned(),
        button_label: "See all available sessions for the autumn term".to_owned(),
        footer: "Registration closes on Friday evening before the term starts; \
            late entries go to the waiting list."
            .to_owned(),
        sections: vec![crate::LiveListSection {
            title: "Saturday sessions in the main hall and the adjoining \
                workshop rooms"
                .to_owned(),
            rows: vec![
                crate::LiveListRow {
                    id: "1".to_owned(),
                    title: "Introduction to woodworking for complete beginners \
                        who have never held a saw"
                        .to_owned(),
                    description: "Tools, safety, and your first cutting board, \
                        plus sharpening and care"
                        .to_owned(),
                },
                crate::LiveListRow {
                    id: "2".to_owned(),
                    title: "Advanced joinery and furniture restoration workshop \
                        for returning students"
                        .to_owned(),
                    description: "Bring a small piece of furniture to work on \
                        and your own clamps if you have them"
                        .to_owned(),
                },
            ],
        }],
    };
    vec![
        LiveMessage::text(
            "fake-stress-1",
            FAKE_STRESS_JID,
            name.clone(),
            &long_token,
            1_700_000_600,
            false,
        ),
        LiveMessage::text(
            "fake-stress-2",
            FAKE_STRESS_JID,
            None,
            format!("ticket{}", "X7".repeat(90)),
            1_700_000_610,
            true,
        ),
        LiveMessage::text(
            "fake-stress-3",
            FAKE_STRESS_JID,
            name.clone(),
            "First paragraph: the ferry was two hours late, so we missed the \
             morning market entirely.\n\nSecond paragraph: by the time we got \
             there, the fish stalls were nearly empty and the vegetable sellers \
             were already packing up.\n\nThird paragraph: we bought what was \
             left, cooked it at the hall, and nobody complained.\n\nFourth \
             paragraph: next time we take the early boat, no matter what the \
             weather looks like at dawn.",
            1_700_000_620,
            false,
        ),
        LiveMessage::text(
            "fake-stress-4",
            FAKE_STRESS_JID,
            name.clone(),
            long_prose,
            1_700_000_630,
            false,
        ),
        quote_long,
        quote_token,
        LiveMessage::text(
            "fake-stress-7",
            FAKE_STRESS_JID,
            name.clone(),
            "😀😃😄😁😆",
            1_700_000_660,
            false,
        ),
        LiveMessage::text(
            "fake-stress-8",
            FAKE_STRESS_JID,
            name.clone(),
            "🎉🎉🎉 we did it, the whole team, every single one of us 🎉🎉🎉",
            1_700_000_670,
            false,
        ),
        LiveMessage::text(
            "fake-stress-9",
            FAKE_STRESS_JID,
            name.clone(),
            "مرحبا بالجميع، الاجتماع القادم سيكون يوم السبت في القاعة الكبيرة، \
             وسنناقش الميزانية السنوية وخطة الإصلاحات والجدول الجديد للأنشطة، \
             لذا يرجى قراءة التقرير قبل الحضور وإحضار أي مقترحات مكتوبة",
            1_700_000_680,
            false,
        ),
        LiveMessage::text(
            "fake-stress-10",
            FAKE_STRESS_JID,
            name.clone(),
            "הפגישה הבאה תתקיים ביום שבת בבוקר באולם הקהילה, ונדון בה את התקציב \
             השנתי, את תוכנית התיקונים, ואת לוח הזמנים החדש של הפעילויות, אז \
             בבקשה לקרוא את הדוח לפני שמגיעים ולהביא הצעות כתובות",
            1_700_000_690,
            false,
        ),
        voice,
        audio,
        document,
        photo,
        video,
        sticker,
        poll,
        event,
        buttons,
        list,
        carousel,
        order,
        contact,
        location,
        link,
        reacted,
        flagged,
        buttons_long,
        list_long,
    ]
}

/// The stress additions to the group chat (stress-fixture
/// gate only): bubbles from a sender with a very long name,
/// so group sender labels and group quote blocks face the
/// same adversarial content as the stress DM.
#[must_use]
pub fn scripted_stress_group_messages() -> Vec<LiveMessage> {
    let mut quote = LiveMessage::text(
        "fake-stress-g3",
        FAKE_GROUP_JID,
        Some("Mike Johnson".to_owned()),
        "Seconded.",
        1_700_000_720,
        false,
    );
    quote.quoted_sender = Some(FAKE_STRESS_NAME.to_owned());
    quote.quoted_body = Some(
        "The community hall booking is confirmed for the whole weekend, and the \
         caretaker said we can arrive from seven in the morning to set up."
            .to_owned(),
    );
    vec![
        LiveMessage::text(
            "fake-stress-g1",
            FAKE_GROUP_JID,
            Some(FAKE_STRESS_NAME.to_owned()),
            "Hello everyone, I booked the hall for the whole weekend and the \
             caretaker confirmed we can set up from seven in the morning.",
            1_700_000_700,
            false,
        ),
        LiveMessage::text(
            "fake-stress-g2",
            FAKE_GROUP_JID,
            Some(FAKE_STRESS_NAME.to_owned()),
            format!("https://example.com/{}", "b".repeat(150)),
            1_700_000_710,
            false,
        ),
        quote,
    ]
}

/// The scripted messages for the group chat: a location share, a
/// received carousel sample, a contact card, a group event, and
/// received buttons and list samples.
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
    // Mike's group event: three scripted ballots (Mike and
    // Emma going, Sarah maybe) with tallies that agree with
    // them, so the group card shows a real aggregation.
    let mut event = LiveMessage::text(
        "fake-event-group-1",
        FAKE_GROUP_JID,
        Some("Mike Johnson".to_owned()),
        "",
        1_700_000_220,
        false,
    );
    event.kind = crate::LiveMessageKind::Event {
        name: "Sunday hike".to_owned(),
        description: "Trailhead at the north gate; bring water.".to_owned(),
        start_time: 1_700_259_200,
        end_time: 1_700_280_000,
        location: "North gate trailhead".to_owned(),
        join_link: String::new(),
        scheduled_call: false,
        extra_guests: false,
        canceled: false,
        my_rsvp: None,
        going: 2,
        maybe: 1,
        not_going: 0,
    };
    event.event_secret = Some(vec![5u8; 32]);
    event.event_creator = Some("10000000004@s.whatsapp.net".to_owned());
    event.event_ballots = vec![
        ("10000000004@s.whatsapp.net".to_owned(), crate::EventRsvp::Going),
        ("10000000007@s.whatsapp.net".to_owned(), crate::EventRsvp::Going),
        (FAKE_DM_JID.to_owned(), crate::EventRsvp::Maybe),
    ];
    // Received interactive samples (row 19): a buttons
    // message from Sarah and a list message from Mike, so the
    // answer surfaces are exercisable in demo mode.
    let mut buttons_msg = LiveMessage::text(
        "fake-buttons-1",
        FAKE_GROUP_JID,
        Some("Sarah Miller".to_owned()),
        "Lunch on Friday?",
        1_700_000_240,
        false,
    );
    buttons_msg.kind = crate::LiveMessageKind::Buttons {
        footer: "Tap to answer".to_owned(),
        buttons: vec![
            crate::LiveButton { id: "1".to_owned(), text: "Yes".to_owned(), action: None },
            crate::LiveButton { id: "2".to_owned(), text: "No".to_owned(), action: None },
            crate::LiveButton { id: "3".to_owned(), text: "Maybe".to_owned(), action: None },
        ],
    };
    let mut list_msg = LiveMessage::text(
        "fake-list-1",
        FAKE_GROUP_JID,
        Some("Mike Johnson".to_owned()),
        "Pick a meeting slot",
        1_700_000_260,
        false,
    );
    list_msg.kind = crate::LiveMessageKind::ListMsg {
        description: "All times are local.".to_owned(),
        button_label: "See slots".to_owned(),
        footer: String::new(),
        sections: vec![crate::LiveListSection {
            title: "Friday".to_owned(),
            rows: vec![
                crate::LiveListRow {
                    id: "1".to_owned(),
                    title: "09:00".to_owned(),
                    description: "Right after standup".to_owned(),
                },
                crate::LiveListRow {
                    id: "2".to_owned(),
                    title: "13:00".to_owned(),
                    description: "After lunch".to_owned(),
                },
            ],
        }],
    };
    vec![location, carousel, contact, event, buttons_msg, list_msg]
}

/// The scripted older-history pages for the DM chat, keyed by
/// the anchor message the app asks older-than. Each page is
/// (messages in timestamp order, end_of_history). The first
/// page answers the linked script's oldest message; the second
/// page answers the first page's oldest and ends the history.
/// Any other anchor (and any other chat) gets an empty,
/// history-ending page: the fake's world is finite and says so.
fn older_page(chat_jid: &str, anchor_id: &str) -> (Vec<LiveMessage>, bool) {
    if chat_jid != FAKE_DM_JID {
        return (Vec::new(), true);
    }
    let text = |id: &str, sender: Option<&str>, body: &str, ts: i64, from_me: bool| {
        LiveMessage::text(id, FAKE_DM_JID, sender.map(ToOwned::to_owned), body, ts, from_me)
    };
    match anchor_id {
        "fake-1" => (
            vec![
                text(
                    "fake-old-a1",
                    Some("Sarah Miller"),
                    "Found the old apartment listing, sending it over",
                    1_700_000_020,
                    false,
                ),
                text("fake-old-a2", None, "That one looks promising", 1_700_000_050, true),
                text(
                    "fake-old-a3",
                    Some("Sarah Miller"),
                    "Let me check the commute first",
                    1_700_000_080,
                    false,
                ),
            ],
            false,
        ),
        "fake-old-a1" => (
            vec![
                text(
                    "fake-old-b1",
                    Some("Sarah Miller"),
                    "We met at the workshop last spring",
                    1_699_999_900,
                    false,
                ),
                text("fake-old-b2", None, "Right, the Rust workshop", 1_699_999_950, true),
            ],
            true,
        ),
        _ => (Vec::new(), true),
    }
}

/// The fake's contact directory for number lookups: the numbers
/// test mode knows, mapped to the person behind them. Sarah is
/// already a roster chat; Emma and Mike resolve to chats the
/// roster does not have yet.
fn directory_person(digits: &str) -> Option<(&'static str, &'static str)> {
    match digits {
        "10000000001" => Some(("10000000001@s.whatsapp.net", "Sarah Miller")),
        "10000000004" => Some(("10000000004@s.whatsapp.net", "Mike Johnson")),
        "10000000007" => Some(("10000000007@s.whatsapp.net", "Emma Wilson")),
        _ => None,
    }
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
    // The linked lunch poll collects two votes, reported as vote
    // events the way the real backend reports decrypted votes
    // (tallies are the app's aggregation job, never an upsert).
    let poll_vote = |voter: &str, selected: Vec<usize>| BackendEvent::PollVote {
        chat_jid: FAKE_DM_JID.to_owned(),
        message_id: "fake-poll-1".to_owned(),
        voter_jid: voter.to_owned(),
        from_me: false,
        selected,
    };
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
        step(19, poll_vote(FAKE_DM_JID, vec![0])),
        step(21, poll_vote("10000000007@s.whatsapp.net", vec![2])),
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
                actor: None,
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
                actor: None,
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

/// Defers a Read receipt for a fake send (P2-46): a send is
/// acknowledged Delivered at once, but Read is a separate,
/// later stage, exactly as on the wire. The fake used to emit
/// both in the same instant, which flattered send proof in
/// demo and made the delivered state unobservable.
fn defer_read_receipt(events: EventSink, chat_jid: String, message_id: String, at: i64) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1_500));
        events.emit(BackendEvent::Receipt {
            chat_jid,
            message_ids: vec![message_id],
            kind: crate::ReceiptKind::Read,
            at,
            actor: None,
        });
    });
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
        config: &BackendConfig,
        commands: Receiver<BackendCommand>,
        events: EventSink,
    ) -> Result<(), String> {
        let mut sent_counter: u64 = 0;
        let mut status_counter: u64 = 0;
        let command_log = self.command_log.clone();
        // Session state: commands mutate these maps, so sequences
        // like archive-then-pin compose instead of restarting from
        // the script on every command.
        let mut group_counter: u32 = 0;
        let mut community_counter: u32 = 0;
        let mut subgroup_counter: u32 = 0;
        let mut group_state: std::collections::HashMap<String, Vec<crate::GroupMemberRow>> =
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
        // Reports to admins per group (row 65), seeded with one
        // report of a scripted group message so the admin view
        // has content before anything is reported in the demo.
        let mut reported_state: std::collections::HashMap<
            String,
            Vec<crate::ReportedMessageEntry>,
        > = std::collections::HashMap::from([(
            FAKE_GROUP_JID.to_owned(),
            vec![crate::ReportedMessageEntry {
                message_id: "fake-carousel-1".to_owned(),
                reporters: vec!["Emma Wilson".to_owned()],
            }],
        )]);
        // The scripted account's own member label per group
        // (row 65), set through SetOwnMemberLabel.
        let mut own_label_state: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        // A history share whose send failed retryably (row
        // 64): the eligible message count kept per group until
        // the retry is spent, mirroring the live token.
        let mut history_retry_state: std::collections::HashMap<String, usize> =
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
        // Group pictures changed through SetGroupPicture, keyed
        // by group JID: Some(bytes) after a set, None after a
        // remove. FetchAvatar consults this before its default
        // answer, so a change is visible through the same funnel
        // the real backend's refetch uses.
        let mut group_picture_state: std::collections::HashMap<String, Option<Vec<u8>>> =
            std::collections::HashMap::new();
        // Blocked contact JIDs, mirroring the server-side list the
        // real backend fetches at connect: BlockContact mutates it
        // and the full list is re-emitted as a Blocklist event.
        // The lock fixture starts with the promo sender blocked,
        // as the account left it before this session.
        let mut blocked_state: std::collections::HashSet<String> = std::collections::HashSet::new();
        if lock_fixture_enabled() {
            blocked_state.insert(FAKE_PROMO_JID.to_owned());
        }
        // The account's privacy settings, mirroring the real
        // backend's fetch: SetPrivacy mutates one entry and the
        // full map is re-emitted, as the real refetch reports it.
        let mut privacy_state = baseline_privacy_state();
        // The account's own profile, mirroring the real backend's
        // read-back: UpdateOwnProfile mutates the given fields and
        // the full profile is re-emitted, as the real re-read
        // reports it.
        let mut own_profile_state = baseline_own_profile();
        // The account's own business profile (business
        // fixture only): UpdateOwnBusinessProfile merges the
        // delta into it, and the fetch reads it back, like
        // the server-side state the real backend queries.
        let mut own_business_state: Option<crate::LiveBusinessProfile> =
            if business_fixture_enabled() { Some(fixture_own_business_profile()) } else { None };
        let mut message_state: std::collections::HashMap<(String, String), LiveMessage> =
            scripted_messages()
                .into_iter()
                .chain(scripted_group_messages())
                .chain(scripted_channel_history())
                .chain(if marathon_enabled() { scripted_marathon_messages() } else { Vec::new() })
                .map(|m| ((m.chat_jid.clone(), m.id.clone()), m))
                .collect();
        // The channel directory state: FetchChannels reports
        // it, follow/unfollow/create mutate it, and the info
        // fetch reads roles from it, mirroring the server-side
        // state the real backend queries per command.
        let mut channel_state: Vec<LiveChannel> = scripted_channels();
        // Channels created during the run get sequential JIDs
        // above the scripted range.
        let mut created_channel_counter: u64 = 0;
        let mut polls: std::collections::HashMap<String, LiveMessage> = scripted_messages()
            .into_iter()
            .filter(|m| matches!(m.kind, crate::LiveMessageKind::Poll { .. }))
            .map(|m| (m.id.clone(), m))
            .collect();
        // Events by message ID, from both scripted chats: the
        // state RespondEvent and EditEvent mutate and answer
        // from, like the polls map above.
        let mut events_map: std::collections::HashMap<String, LiveMessage> = scripted_messages()
            .into_iter()
            .chain(scripted_group_messages())
            .chain(if marathon_enabled() { scripted_marathon_messages() } else { Vec::new() })
            .filter(|m| matches!(m.kind, crate::LiveMessageKind::Event { .. }))
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
        // Failure injection (see the field docs): one-shot, by
        // command kind name. Arms consult it before doing any
        // work, so an injected failure changes no fake state,
        // exactly like a send that never reached a server.
        let mut fail_next = self.fail_next.clone();
        let mut take_failure = |kind: &'static str| -> bool {
            match fail_next.iter().position(|name| *name == kind) {
                Some(pos) => {
                    fail_next.remove(pos);
                    true
                }
                None => false,
            }
        };
        while let Ok(cmd) = commands.recv() {
            match cmd {
                BackendCommand::ConnectQr => {
                    events.emit(BackendEvent::QrCode {
                        payload: FAKE_QR_PAYLOAD.to_owned(),
                        // A test double: its one scripted code does
                        // not rotate, so the window is nominal.
                        valid_for: std::time::Duration::from_secs(3600),
                    });
                    if self.auto_connect {
                        emit_linked_script(
                            &events,
                            &call_state,
                            &privacy_state,
                            &own_profile_state,
                        );
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
                        emit_linked_script(
                            &events,
                            &call_state,
                            &privacy_state,
                            &own_profile_state,
                        );
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
                BackendCommand::WarmChatCache { .. } => {
                    // No-op: the fake holds no send caches to
                    // warm, and the command is fire-and-forget
                    // with no answer event in either backend.
                }
                BackendCommand::SendText { chat_jid, send_id, body, ephemeral_secs, .. } => {
                    sent_counter += 1;
                    // A caller-minted ID is honored exactly, the
                    // way the real backend sends under it: the
                    // confirmation and the echo carry the supplied
                    // ID, so the app's binding path is the real one.
                    let id = send_id.unwrap_or_else(|| format!("fake-sent-{sent_counter}"));
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
                    // Receipt stages are separate (P2-46):
                    // Delivered now, Read deferred.
                    events.emit(BackendEvent::Receipt {
                        chat_jid: chat_jid.clone(),
                        message_ids: vec![id.clone()],
                        kind: crate::ReceiptKind::Delivered,
                        at: ts,
                        actor: None,
                    });
                    defer_read_receipt(events.clone(), chat_jid.clone(), id, ts);
                    if chat_jid == FAKE_DM_JID {
                        // The scripted reply lives under the
                        // chat's current timer, like a real
                        // incoming ephemeral message would.
                        let reply_timer = chat_state.get(&chat_jid).map_or(0, |c| c.ephemeral_secs);
                        let mut reply = LiveMessage::text(
                            format!("fake-reply-{sent_counter}"),
                            chat_jid,
                            Some("Sarah Miller".to_owned()),
                            "Received!",
                            ts + 1,
                            false,
                        );
                        if reply_timer > 0 {
                            reply.expires_at = ts + 1 + i64::from(reply_timer);
                        }
                        events.emit(BackendEvent::MessageUpsert(reply));
                    }
                }
                BackendCommand::SendInteractive { chat_jid, send_id, spec } => {
                    let send_id =
                        send_id.unwrap_or_else(|| format!("fake-sent-{}", sent_counter + 1));
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("send_interactive:{chat_jid}:{}", spec_log_name(&spec)));
                    }
                    if take_failure("SendInteractive") {
                        // The real backend answers a failed chat
                        // send with the structured event, never a
                        // worded Error, so the fake does the same:
                        // demo and tests exercise the real path.
                        events.emit(BackendEvent::MessageSendFailed {
                            chat_jid: chat_jid.clone(),
                            message_id: send_id.clone(),
                            failure: crate::SendFailure {
                                kind: crate::SendFailureKind::Offline,
                                stage: crate::SendFailureStage::Other,
                            },
                        });
                    } else {
                        sent_counter += 1;
                        let id = send_id;
                        let ts = 1_700_000_300 + i64::try_from(sent_counter).unwrap_or(0);
                        let (body, kind) = interactive_echo_parts(&spec);
                        let mut echo =
                            LiveMessage::text(id.clone(), chat_jid.clone(), None, body, ts, true);
                        echo.kind = kind;
                        events.emit(BackendEvent::MessageSent {
                            chat_jid: chat_jid.clone(),
                            message_id: id.clone(),
                        });
                        events.emit(BackendEvent::MessageUpsert(echo));
                        events.emit(BackendEvent::Receipt {
                            chat_jid: chat_jid.clone(),
                            message_ids: vec![id.clone()],
                            kind: crate::ReceiptKind::Delivered,
                            at: ts,
                            actor: None,
                        });
                        defer_read_receipt(events.clone(), chat_jid, id, ts);
                    }
                }
                BackendCommand::SendInteractiveResponse {
                    chat_jid,
                    send_id,
                    message_id: _,
                    sender_jid: _,
                    from_me: _,
                    is_list: _,
                    choice_id: _,
                    choice_text,
                } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("interactive_response:{chat_jid}:{choice_text}"));
                    }
                    let send_id =
                        send_id.unwrap_or_else(|| format!("fake-sent-{}", sent_counter + 1));
                    if take_failure("SendInteractiveResponse") {
                        // Same structured failure as the sibling
                        // interactive send above.
                        events.emit(BackendEvent::MessageSendFailed {
                            chat_jid: chat_jid.clone(),
                            message_id: send_id.clone(),
                            failure: crate::SendFailure {
                                kind: crate::SendFailureKind::Offline,
                                stage: crate::SendFailureStage::Other,
                            },
                        });
                    } else {
                        sent_counter += 1;
                        let id = send_id;
                        let ts = 1_700_000_300 + i64::try_from(sent_counter).unwrap_or(0);
                        let echo = LiveMessage::text(
                            id.clone(),
                            chat_jid.clone(),
                            None,
                            choice_text,
                            ts,
                            true,
                        );
                        events.emit(BackendEvent::MessageSent {
                            chat_jid: chat_jid.clone(),
                            message_id: id.clone(),
                        });
                        events.emit(BackendEvent::MessageUpsert(echo));
                        events.emit(BackendEvent::Receipt {
                            chat_jid,
                            message_ids: vec![id],
                            kind: crate::ReceiptKind::Delivered,
                            at: ts,
                            actor: None,
                        });
                    }
                }
                BackendCommand::PostTextStatus { text, recipients, privacy } => {
                    if let Some(log) = &command_log {
                        log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(
                            format!("post_text_status:{text}:{}:{privacy}", recipients.len()),
                        );
                    }
                    // The recorded success is deterministic, but
                    // the refusals are the real backend's too: an
                    // empty text or an empty audience never posts.
                    if text.trim().is_empty() {
                        events.emit(BackendEvent::StatusPostFailed {
                            message: "That status is empty, nothing was posted.".to_owned(),
                        });
                    } else if recipients.is_empty() {
                        events.emit(BackendEvent::StatusPostFailed {
                            message: "There is nobody to post that status to.".to_owned(),
                        });
                    } else if take_failure("PostTextStatus") {
                        events.emit(BackendEvent::StatusPostFailed {
                            message: "Posting the status failed: connection lost".to_owned(),
                        });
                    } else {
                        status_counter += 1;
                        let message_id = format!("fake-status-post-{status_counter}");
                        events.emit(BackendEvent::StatusPosted {
                            text: text.clone(),
                            message_id: message_id.clone(),
                        });
                        // The post is delivered back like a real
                        // one: the account's own status arrives
                        // under the broadcast too.
                        events.emit(BackendEvent::StatusReceived {
                            status: Box::new(LiveStatus {
                                id: message_id,
                                author_jid: FAKE_OWN_JID.to_owned(),
                                author_name: None,
                                body: text,
                                timestamp: 1_700_000_400
                                    + i64::try_from(status_counter).unwrap_or(0),
                                from_me: true,
                                kind: crate::LiveMessageKind::Text,
                                background_argb: Some(0xFF00_A884),
                                reactions: Vec::new(),
                                my_reaction: None,
                                photo_ref: None,
                                photo_thumbnail: None,
                                video_ref: None,
                                video_thumbnail: None,
                                poster_ref: None,
                                video_width: None,
                                video_height: None,
                            }),
                        });
                    }
                }
                BackendCommand::PostMediaStatus {
                    kind,
                    data,
                    caption,
                    thumbnail,
                    seconds,
                    recipients,
                    ..
                } => {
                    if let Some(log) = &command_log {
                        log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(
                            format!(
                                "post_media_status:{kind:?}:{}:{}",
                                data.len(),
                                recipients.len()
                            ),
                        );
                    }
                    // The real backend's refusal rules, in its
                    // order: empty payload, unpostable kind,
                    // empty audience.
                    if data.is_empty() {
                        events.emit(BackendEvent::StatusPostFailed {
                            message: "That file is empty, nothing was posted.".to_owned(),
                        });
                    } else if !matches!(kind, crate::MediaKind::Photo | crate::MediaKind::Video) {
                        events.emit(BackendEvent::StatusPostFailed {
                            message: "Only a photo or a video can be posted as a status."
                                .to_owned(),
                        });
                    } else if recipients.is_empty() {
                        events.emit(BackendEvent::StatusPostFailed {
                            message: "There is nobody to post that status to.".to_owned(),
                        });
                    } else if take_failure("PostMediaStatus") {
                        events.emit(BackendEvent::StatusPostFailed {
                            message: "Posting the status failed: connection lost".to_owned(),
                        });
                    } else {
                        status_counter += 1;
                        let status = LiveStatus {
                            id: format!("fake-status-post-{status_counter}"),
                            author_jid: FAKE_OWN_JID.to_owned(),
                            author_name: None,
                            body: caption.clone(),
                            timestamp: 1_700_000_400 + i64::try_from(status_counter).unwrap_or(0),
                            from_me: true,
                            kind: crate::LiveMessageKind::Media {
                                media: kind,
                                caption,
                                seconds,
                                size_bytes: data.len() as u64,
                            },
                            background_argb: None,
                            reactions: Vec::new(),
                            my_reaction: None,
                            photo_ref: None,
                            photo_thumbnail: if matches!(kind, crate::MediaKind::Photo)
                                && !thumbnail.is_empty()
                            {
                                Some(thumbnail.clone())
                            } else {
                                None
                            },
                            video_ref: None,
                            video_thumbnail: if matches!(kind, crate::MediaKind::Video)
                                && !thumbnail.is_empty()
                            {
                                Some(thumbnail)
                            } else {
                                None
                            },
                            poster_ref: None,
                            video_width: None,
                            video_height: None,
                        };
                        events.emit(BackendEvent::StatusMediaPosted {
                            status: Box::new(status.clone()),
                        });
                        events.emit(BackendEvent::StatusReceived { status: Box::new(status) });
                    }
                }
                BackendCommand::DeleteStatus { message_id, recipients } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("delete_status:{message_id}:{}", recipients.len()));
                    }
                    if recipients.is_empty() {
                        events.emit(BackendEvent::StatusDeleteFailed {
                            message_id,
                            message: "There is nobody to send that delete to.".to_owned(),
                        });
                    } else if take_failure("DeleteStatus") {
                        events.emit(BackendEvent::StatusDeleteFailed {
                            message_id,
                            message: "Deleting the status failed: connection lost".to_owned(),
                        });
                    } else {
                        events.emit(BackendEvent::StatusDeleted { message_id: message_id.clone() });
                        // The revoke is delivered back like a
                        // real one: a protocol revoke addressed
                        // to the broadcast.
                        events.emit(BackendEvent::MessageRevoked {
                            chat_jid: "status@broadcast".to_owned(),
                            message_id,
                        });
                    }
                }
                BackendCommand::FetchChannels => {
                    if take_failure("FetchChannels") {
                        events.emit(BackendEvent::ChannelsListFailed {
                            message: "Loading the channel list failed: connection lost".to_owned(),
                        });
                    } else {
                        let channels = channel_state
                            .iter()
                            .filter(|c| c.role != crate::LiveChannelRole::Guest)
                            .cloned()
                            .collect();
                        events.emit(BackendEvent::ChannelsListed { channels });
                    }
                }
                BackendCommand::LookupChannel { code } => {
                    let tail = code.rsplit("channel/").next().unwrap_or(&code);
                    let pasted: String =
                        tail.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
                    let found = if pasted.eq_ignore_ascii_case(FAKE_CHANNEL_INVITE) {
                        channel_state.iter().find(|c| c.jid == FAKE_CHANNEL_JID).cloned()
                    } else if pasted.eq_ignore_ascii_case(FAKE_DISCOVERABLE_INVITE) {
                        Some(scripted_discoverable_channel())
                    } else if pasted.eq_ignore_ascii_case(FAKE_OWN_INVITE) {
                        channel_state.iter().find(|c| c.jid == FAKE_OWN_CHANNEL_JID).cloned()
                    } else {
                        None
                    };
                    if take_failure("LookupChannel") {
                        events.emit(BackendEvent::ChannelLookupFailed {
                            message: "No channel was found for that link: connection lost"
                                .to_owned(),
                        });
                    } else if let Some(channel) = found {
                        events.emit(BackendEvent::ChannelFound { channel });
                    } else {
                        events.emit(BackendEvent::ChannelLookupFailed {
                            message: "No channel in test mode uses that invite link.".to_owned(),
                        });
                    }
                }
                BackendCommand::FollowChannel { jid } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("follow_channel:{jid}"));
                    }
                    if take_failure("FollowChannel") {
                        events.emit(BackendEvent::ChannelFollowFailed {
                            jid: jid.clone(),
                            message: "Following the channel failed: connection lost".to_owned(),
                        });
                    } else if jid == FAKE_DISCOVERABLE_CHANNEL_JID {
                        let mut channel = scripted_discoverable_channel();
                        channel.role = crate::LiveChannelRole::Follower;
                        channel.followers += 1;
                        channel_state.retain(|c| c.jid != jid);
                        channel_state.push(channel.clone());
                        let chat = LiveChat {
                            jid: jid.clone(),
                            title: channel.name.clone(),
                            kind: LiveChatKind::Channel,
                            from_history: false,
                            preview: String::new(),
                            preview_sender: None,
                            timestamp: 1_700_000_400,
                            unread: 0,
                            archived: false,
                            locked: false,
                            pinned: false,
                            ephemeral_secs: 0,
                            muted: false,
                            mute_until: 0,
                            parent_jid: None,
                            is_parent: false,
                        };
                        chat_state.insert(jid.clone(), chat.clone());
                        events.emit(BackendEvent::ChatUpsert(chat));
                        events.emit(BackendEvent::ChannelFollowed { channel });
                    } else if let Some(channel) =
                        channel_state.iter().find(|c| c.jid == jid).cloned()
                    {
                        events.emit(BackendEvent::ChannelFollowed { channel });
                    } else {
                        events.emit(BackendEvent::ChannelFollowFailed {
                            jid: jid.clone(),
                            message: "That channel is not part of the test-mode script.".to_owned(),
                        });
                    }
                }
                BackendCommand::UnfollowChannel { jid } => {
                    if take_failure("UnfollowChannel") {
                        events.emit(BackendEvent::ChannelUnfollowFailed {
                            jid: jid.clone(),
                            message: "Unfollowing the channel failed: connection lost".to_owned(),
                        });
                    } else if let Some(channel) = channel_state
                        .iter_mut()
                        .find(|c| c.jid == jid && c.role == crate::LiveChannelRole::Follower)
                    {
                        channel.role = crate::LiveChannelRole::Guest;
                        channel.followers = channel.followers.saturating_sub(1);
                        events.emit(BackendEvent::ChannelUnfollowed { jid: jid.clone() });
                    } else {
                        events.emit(BackendEvent::ChannelUnfollowFailed {
                            jid: jid.clone(),
                            message: "That channel is not followed in the test-mode script."
                                .to_owned(),
                        });
                    }
                }
                BackendCommand::CreateChannel { name, description } => {
                    if name.trim().is_empty() {
                        events.emit(BackendEvent::ChannelCreateFailed {
                            message: "A channel needs a name.".to_owned(),
                        });
                    } else if take_failure("CreateChannel") {
                        events.emit(BackendEvent::ChannelCreateFailed {
                            message: "Creating the channel failed: connection lost".to_owned(),
                        });
                    } else {
                        created_channel_counter += 1;
                        let jid = format!("{}@newsletter", 1_000_000_010 + created_channel_counter);
                        let channel = LiveChannel {
                            jid: jid.clone(),
                            name: name.clone(),
                            description: description.clone(),
                            followers: 0,
                            verified: false,
                            role: crate::LiveChannelRole::Owner,
                            muted: false,
                            invite_code: None,
                            created_at: 1_700_000_400,
                        };
                        channel_state.push(channel.clone());
                        let chat = LiveChat {
                            jid: jid.clone(),
                            title: name.clone(),
                            kind: LiveChatKind::Channel,
                            from_history: false,
                            preview: String::new(),
                            preview_sender: None,
                            timestamp: 1_700_000_400,
                            unread: 0,
                            archived: false,
                            locked: false,
                            pinned: false,
                            ephemeral_secs: 0,
                            muted: false,
                            mute_until: 0,
                            parent_jid: None,
                            is_parent: false,
                        };
                        chat_state.insert(jid.clone(), chat.clone());
                        events.emit(BackendEvent::ChatUpsert(chat));
                        events.emit(BackendEvent::ChannelCreated { channel });
                    }
                }
                BackendCommand::FetchChannelHistory { jid, count, before } => {
                    if take_failure("FetchChannelHistory") {
                        events.emit(BackendEvent::ChannelHistoryFailed {
                            jid: jid.clone(),
                            message: "Loading the channel's updates failed: connection lost"
                                .to_owned(),
                        });
                    } else {
                        let mut page: Vec<LiveMessage> = message_state
                            .values()
                            .filter(|m| {
                                m.chat_jid == jid
                                    && m.server_id.is_some_and(|s| before.is_none_or(|b| s < b))
                            })
                            .cloned()
                            .collect();
                        page.sort_by_key(|m| std::cmp::Reverse(m.server_id.unwrap_or(0)));
                        let total = page.len();
                        page.truncate(usize::try_from(count).unwrap_or(usize::MAX));
                        let added = u32::try_from(page.len()).unwrap_or(u32::MAX);
                        let end_of_history = page.len() >= total;
                        for msg in page {
                            events.emit(BackendEvent::MessageUpsert(msg));
                        }
                        events.emit(BackendEvent::ChannelHistoryFetched {
                            jid: jid.clone(),
                            added,
                            end_of_history,
                        });
                    }
                }
                BackendCommand::WatchChannel { .. } => {
                    // Counters in test mode arrive through the
                    // history fetch; the watch arms nothing and
                    // answers nothing, exactly the live
                    // contract's fire-and-forget shape.
                }
                BackendCommand::ReactToChannelUpdate { jid, server_id, emoji } => {
                    let key = message_state
                        .iter()
                        .find(|(_, m)| m.chat_jid == jid && m.server_id == Some(server_id))
                        .map(|(k, _)| k.clone());
                    if take_failure("ReactToChannelUpdate") {
                        events.emit(BackendEvent::ChannelReactionFailed {
                            jid: jid.clone(),
                            server_id,
                            message: "That reaction could not be sent: connection lost".to_owned(),
                        });
                    } else if let Some(key) = key {
                        if let Some(msg) = message_state.get_mut(&key) {
                            msg.my_reaction =
                                if emoji.is_empty() { None } else { Some(emoji.clone()) };
                            let updated = msg.clone();
                            events.emit(BackendEvent::MessageUpsert(updated));
                        }
                        events.emit(BackendEvent::ChannelReactionSet {
                            jid: jid.clone(),
                            server_id,
                            emoji: emoji.clone(),
                        });
                    } else {
                        events.emit(BackendEvent::ChannelReactionFailed {
                            jid: jid.clone(),
                            server_id,
                            message: "That update is not part of the test-mode script.".to_owned(),
                        });
                    }
                }
                BackendCommand::CreateChannelPoll { jid, question, options, multiple } => {
                    let can_post = channel_state
                        .iter()
                        .find(|c| c.jid == jid)
                        .is_some_and(|c| c.role.can_post());
                    if take_failure("CreateChannelPoll") {
                        events.emit(BackendEvent::ChannelPollFailed {
                            jid: jid.clone(),
                            message: "Posting the poll failed: connection lost".to_owned(),
                        });
                    } else if !can_post {
                        events.emit(BackendEvent::ChannelPollFailed {
                            jid: jid.clone(),
                            message: "Only admins can post to that channel.".to_owned(),
                        });
                    } else {
                        let server_id = message_state
                            .values()
                            .filter(|m| m.chat_jid == jid)
                            .filter_map(|m| m.server_id)
                            .max()
                            .unwrap_or(200)
                            + 1;
                        sent_counter += 1;
                        let id = format!("fake-ch-poll-{server_id}");
                        let mut poll = LiveMessage::text(
                            id.clone(),
                            jid.clone(),
                            None,
                            "",
                            1_700_000_400 + i64::try_from(sent_counter).unwrap_or(0),
                            true,
                        );
                        poll.server_id = Some(server_id);
                        poll.kind = crate::LiveMessageKind::Poll {
                            question: question.clone(),
                            options: options.iter().map(|o| (o.clone(), 0)).collect(),
                            multiple,
                            my_votes: Vec::new(),
                            correct: None,
                        };
                        message_state.insert((jid.clone(), id), poll.clone());
                        events.emit(BackendEvent::MessageUpsert(poll));
                        events.emit(BackendEvent::ChannelPollCreated { jid: jid.clone() });
                    }
                }
                BackendCommand::VoteChannelPoll { jid, server_id, options } => {
                    let key = message_state
                        .iter()
                        .find(|(_, m)| m.chat_jid == jid && m.server_id == Some(server_id))
                        .map(|(k, _)| k.clone());
                    if take_failure("VoteChannelPoll") {
                        events.emit(BackendEvent::ChannelPollVoteFailed {
                            jid: jid.clone(),
                            server_id,
                            message: "That vote could not be sent: connection lost".to_owned(),
                        });
                    } else if let Some(key) = key {
                        let mut voted = false;
                        if let Some(msg) = message_state.get_mut(&key) {
                            if let crate::LiveMessageKind::Poll {
                                options: poll_options,
                                my_votes,
                                ..
                            } = &mut msg.kind
                            {
                                // The server record moves
                                // with my ballot: tallies gain
                                // and lose my vote as the
                                // selection changes, like the
                                // real backend's next history
                                // fetch reports.
                                let new_votes: Vec<usize> = poll_options
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, (name, _))| options.contains(name))
                                    .map(|(index, _)| index)
                                    .collect();
                                for (index, (_, count)) in poll_options.iter_mut().enumerate() {
                                    let was = my_votes.contains(&index);
                                    let now = new_votes.contains(&index);
                                    if now && !was {
                                        *count += 1;
                                    } else if was && !now {
                                        *count = count.saturating_sub(1);
                                    }
                                }
                                *my_votes = new_votes;
                                voted = true;
                            }
                            let updated = msg.clone();
                            events.emit(BackendEvent::MessageUpsert(updated));
                        }
                        if voted {
                            events.emit(BackendEvent::ChannelPollVoted {
                                jid: jid.clone(),
                                server_id,
                            });
                        } else {
                            events.emit(BackendEvent::ChannelPollVoteFailed {
                                jid: jid.clone(),
                                server_id,
                                message: "That update is not a poll.".to_owned(),
                            });
                        }
                    } else {
                        events.emit(BackendEvent::ChannelPollVoteFailed {
                            jid: jid.clone(),
                            server_id,
                            message: "That update is not part of the test-mode script.".to_owned(),
                        });
                    }
                }
                BackendCommand::EditChannelUpdate { jid, message_id, text } => {
                    let key = (jid.clone(), message_id.clone());
                    if take_failure("EditChannelUpdate") {
                        events.emit(BackendEvent::ChannelUpdateEditFailed {
                            jid: jid.clone(),
                            message_id: message_id.clone(),
                            message: "Editing the update failed: connection lost".to_owned(),
                        });
                    } else if message_state.get(&key).is_some_and(|m| m.from_me) {
                        if let Some(msg) = message_state.get_mut(&key) {
                            msg.body = text.clone();
                            if let crate::LiveMessageKind::Media { caption, .. } = &mut msg.kind {
                                *caption = text.clone();
                            }
                            msg.edited = true;
                            let updated = msg.clone();
                            events.emit(BackendEvent::MessageUpsert(updated));
                        }
                        events.emit(BackendEvent::ChannelUpdateEdited {
                            jid: jid.clone(),
                            message_id: message_id.clone(),
                        });
                    } else {
                        events.emit(BackendEvent::ChannelUpdateEditFailed {
                            jid: jid.clone(),
                            message_id: message_id.clone(),
                            message: "That update cannot be edited in test mode.".to_owned(),
                        });
                    }
                }
                BackendCommand::RevokeChannelUpdate { jid, message_id } => {
                    let key = (jid.clone(), message_id.clone());
                    if take_failure("RevokeChannelUpdate") {
                        events.emit(BackendEvent::ChannelUpdateRevokeFailed {
                            jid: jid.clone(),
                            message_id: message_id.clone(),
                            message: "Deleting the update failed: connection lost".to_owned(),
                        });
                    } else if message_state.get(&key).is_some_and(|m| m.from_me) {
                        message_state.remove(&key);
                        events.emit(BackendEvent::MessageRevoked {
                            chat_jid: jid.clone(),
                            message_id: message_id.clone(),
                        });
                        events.emit(BackendEvent::ChannelUpdateRevoked {
                            jid: jid.clone(),
                            message_id: message_id.clone(),
                        });
                    } else {
                        events.emit(BackendEvent::ChannelUpdateRevokeFailed {
                            jid: jid.clone(),
                            message_id: message_id.clone(),
                            message: "That update cannot be deleted in test mode.".to_owned(),
                        });
                    }
                }
                BackendCommand::FetchChannelInfo { jid } => {
                    let known =
                        channel_state.iter().find(|c| c.jid == jid).cloned().or_else(|| {
                            if jid == FAKE_DISCOVERABLE_CHANNEL_JID {
                                Some(scripted_discoverable_channel())
                            } else {
                                None
                            }
                        });
                    if take_failure("FetchChannelInfo") {
                        events.emit(BackendEvent::ChannelInfoFailed {
                            jid: jid.clone(),
                            message: "Loading the channel info failed: connection lost".to_owned(),
                        });
                    } else if let Some(channel) = known {
                        let (admin_count, follower_names) = if channel.role.can_post() {
                            (
                                Some(2),
                                vec![
                                    "Emma Wilson".to_owned(),
                                    "Liam Chen".to_owned(),
                                    "Sofia Rossi".to_owned(),
                                ],
                            )
                        } else {
                            (None, Vec::new())
                        };
                        events.emit(BackendEvent::ChannelInfoFetched {
                            info: crate::LiveChannelInfo { channel, admin_count, follower_names },
                        });
                    } else {
                        events.emit(BackendEvent::ChannelInfoFailed {
                            jid: jid.clone(),
                            message: "That channel is not part of the test-mode script.".to_owned(),
                        });
                    }
                }
                BackendCommand::SetChannelMuted { jid, muted } => {
                    if take_failure("SetChannelMuted") {
                        events.emit(BackendEvent::ChannelMuteFailed {
                            jid: jid.clone(),
                            message: "Changing the mute setting failed: connection lost".to_owned(),
                        });
                    } else if let Some(channel) = channel_state.iter_mut().find(|c| c.jid == jid) {
                        channel.muted = muted;
                        events.emit(BackendEvent::ChannelMuteSet { jid: jid.clone(), muted });
                    } else {
                        events.emit(BackendEvent::ChannelMuteFailed {
                            jid: jid.clone(),
                            message: "That channel is not part of the test-mode script.".to_owned(),
                        });
                    }
                }
                BackendCommand::SetDisappearingTimer { chat_jid, duration_secs } => {
                    if take_failure("SetDisappearingTimer") {
                        events.emit(BackendEvent::DisappearingTimerFailed {
                            chat_jid,
                            message: "Changing disappearing messages failed: connection lost"
                                .to_owned(),
                        });
                    } else {
                        if let Some(chat) = chat_state.get_mut(&chat_jid) {
                            chat.ephemeral_secs = duration_secs;
                            events.emit(BackendEvent::ChatUpsert(chat.clone()));
                        }
                        events.emit(BackendEvent::DisappearingChanged { chat_jid, duration_secs });
                    }
                }
                BackendCommand::SendMedia {
                    chat_jid,
                    send_id,
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
                    let id = send_id.unwrap_or_else(|| format!("fake-media-{sent_counter}"));
                    let ts = 1_700_000_300 + i64::try_from(sent_counter).unwrap_or(0);
                    let label = match kind {
                        crate::MediaKind::Photo => "[photo]",
                        crate::MediaKind::Video => "[video]",
                        crate::MediaKind::Voice => "[voice note]",
                        crate::MediaKind::Audio => "[audio]",
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
                        actor: None,
                    });
                    defer_read_receipt(events.clone(), chat_jid.clone(), id, ts);
                }
                BackendCommand::SendLocation {
                    chat_jid,
                    send_id,
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
                    let id = send_id.unwrap_or_else(|| format!("fake-location-{sent_counter}"));
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
                        actor: None,
                    });
                    defer_read_receipt(events.clone(), chat_jid.clone(), id, ts);
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
                        state.current =
                            Some(FakeCall::plain(call_id.clone(), chat_jid.clone(), true, video));
                        events.emit(BackendEvent::CallChanged {
                            call_id,
                            chat_jid,
                            phase: crate::CallPhase::Ringing,
                            seconds: 0,
                            reason: None,
                        });
                    }
                }
                BackendCommand::StartGroupCall { chat_jid, video } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("start_group_call:{chat_jid}:{video}"));
                    }
                    let mut state =
                        call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if take_failure("StartGroupCall") {
                        events.emit(BackendEvent::Error {
                            message: "Starting the group call failed: connection lost".to_owned(),
                        });
                    } else if state.current.is_some() {
                        events.emit(BackendEvent::Error {
                            message: "A call is already in progress.".to_owned(),
                        });
                    } else {
                        state.counter += 1;
                        let call_id = format!("fake-call-{}", state.counter);
                        let mut call =
                            FakeCall::plain(call_id.clone(), chat_jid.clone(), true, video);
                        call.group = true;
                        call.participants = scripted_group_participants();
                        state.current = Some(call);
                        let current = state.current.as_ref().expect("just stored");
                        events.emit(BackendEvent::CallChanged {
                            call_id,
                            chat_jid,
                            phase: crate::CallPhase::Ringing,
                            seconds: 0,
                            reason: None,
                        });
                        emit_fake_roster(&events, current);
                    }
                }
                BackendCommand::CreateCallLink { video } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("create_call_link:{video}"));
                    }
                    if take_failure("CreateCallLink") {
                        events.emit(BackendEvent::CallLinkFailed {
                            message: "Creating the call link failed: connection lost".to_owned(),
                        });
                    } else {
                        let kind = if video { "video" } else { "audio" };
                        events.emit(BackendEvent::CallLinkCreated {
                            url: format!("https://call.whatsapp.com/{kind}/fake-link-1622"),
                            video,
                        });
                    }
                }
                BackendCommand::PreviewCallLink { link } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push("preview_call_link".to_owned());
                    }
                    if take_failure("PreviewCallLink") {
                        events.emit(BackendEvent::CallLinkFailed {
                            message: "That call link could not be read: connection lost".to_owned(),
                        });
                    } else if link.contains("call.whatsapp.com/") || link.contains("fake-link") {
                        events.emit(BackendEvent::CallLinkPreview {
                            url: if link.starts_with("http") {
                                link.clone()
                            } else {
                                format!("https://call.whatsapp.com/audio/{link}")
                            },
                            creator_jid: FAKE_DM_JID.to_owned(),
                            video: link.contains("/video/"),
                            waiting_room: true,
                        });
                    } else {
                        events.emit(BackendEvent::CallLinkFailed {
                            message: "That call link could not be read: not a call link".to_owned(),
                        });
                    }
                }
                BackendCommand::JoinCallLink { link: _, video } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("join_call_link:{video}"));
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
                        // The overlay opens in Connecting first,
                        // like the live join; a failure then ends
                        // it with the reason.
                        events.emit(BackendEvent::CallChanged {
                            call_id: call_id.clone(),
                            chat_jid: String::new(),
                            phase: crate::CallPhase::Connecting,
                            seconds: 0,
                            reason: None,
                        });
                        if take_failure("JoinCallLink") {
                            events.emit(BackendEvent::CallChanged {
                                call_id,
                                chat_jid: String::new(),
                                phase: crate::CallPhase::Ended,
                                seconds: 0,
                                reason: Some("Joining the call failed: connection lost".to_owned()),
                            });
                        } else {
                            let mut call =
                                FakeCall::plain(call_id.clone(), String::new(), true, video);
                            call.group = true;
                            call.link = true;
                            call.phase = crate::CallPhase::Connecting;
                            // The link's creator is already in
                            // their call; one more guest is
                            // still connecting (the ticker
                            // connects them shortly).
                            call.participants = vec![
                                FakeParticipant {
                                    jid: FAKE_DM_JID.to_owned(),
                                    state: "connected".to_owned(),
                                    hand_raised: false,
                                    sharing: false,
                                },
                                FakeParticipant {
                                    jid: FAKE_MEMBER2_JID.to_owned(),
                                    state: "ringing".to_owned(),
                                    hand_raised: false,
                                    sharing: false,
                                },
                            ];
                            state.current = Some(call);
                        }
                    }
                }
                BackendCommand::SetCallVideo { on } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("call_video:{on}"));
                    }
                    if take_failure("SetCallVideo") {
                        events.emit(BackendEvent::CallControlFailed {
                            message: "Starting video failed: connection lost".to_owned(),
                        });
                    } else {
                        let mut state =
                            call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match &mut state.current {
                            Some(call) => {
                                call.video = on;
                                if !on {
                                    call.video_stream_started = false;
                                }
                                // The scripted peer side mirrors
                                // the call's kind: on a video
                                // call their video is on.
                                events.emit(BackendEvent::CallVideoState {
                                    local_on: on,
                                    remote_on: call.video,
                                    local_note: None,
                                });
                            }
                            None => events.emit(BackendEvent::CallControlFailed {
                                message: "No call is in progress.".to_owned(),
                            }),
                        }
                    }
                }
                BackendCommand::SendCallReaction { emoji } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("call_reaction:{emoji}"));
                    }
                    if take_failure("SendCallReaction") {
                        events.emit(BackendEvent::CallControlFailed {
                            message: "Sending the reaction failed: connection lost".to_owned(),
                        });
                    } else {
                        let state =
                            call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if state.current.is_none() {
                            events.emit(BackendEvent::CallControlFailed {
                                message: "No call is in progress.".to_owned(),
                            });
                        }
                    }
                }
                BackendCommand::SetCallHandRaised { raised } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("call_hand:{raised}"));
                    }
                    if take_failure("SetCallHandRaised") {
                        events.emit(BackendEvent::CallControlFailed {
                            message: "Raising your hand failed: connection lost".to_owned(),
                        });
                    } else {
                        let state =
                            call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if state.current.is_none() {
                            events.emit(BackendEvent::CallControlFailed {
                                message: "No call is in progress.".to_owned(),
                            });
                        }
                    }
                }
                BackendCommand::InviteCallParticipant { jid } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("call_invite:{jid}"));
                    }
                    if take_failure("InviteCallParticipant") {
                        events.emit(BackendEvent::CallControlFailed {
                            message: "Inviting them failed: connection lost".to_owned(),
                        });
                    } else {
                        let mut state =
                            call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match &mut state.current {
                            Some(call) if call.group => {
                                let at = call.active_ticks + 8;
                                call.pending_invite = Some((jid.clone(), at));
                                call.participants.push(FakeParticipant {
                                    jid,
                                    state: "ringing".to_owned(),
                                    hand_raised: false,
                                    sharing: false,
                                });
                                let current = state.current.as_ref().expect("present");
                                emit_fake_roster(&events, current);
                            }
                            _ => events.emit(BackendEvent::CallControlFailed {
                                message: "No group call is in progress.".to_owned(),
                            }),
                        }
                    }
                }
                BackendCommand::AdmitWaitingUser { jid } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("call_admit:{jid}"));
                    }
                    if take_failure("AdmitWaitingUser") {
                        events.emit(BackendEvent::CallControlFailed {
                            message: "Letting them in failed: connection lost".to_owned(),
                        });
                    } else {
                        let mut state =
                            call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match &mut state.current {
                            Some(call) if call.waiting.as_deref() == Some(jid.as_str()) => {
                                call.waiting = None;
                                call.participants.push(FakeParticipant {
                                    jid: jid.clone(),
                                    state: "connected".to_owned(),
                                    hand_raised: false,
                                    sharing: false,
                                });
                                let approval = call.approval;
                                let current = state.current.as_ref().expect("present");
                                emit_fake_roster(&events, current);
                                events.emit(BackendEvent::CallWaitingRoom {
                                    is_admin: true,
                                    approval_required: approval,
                                    users: Vec::new(),
                                });
                            }
                            _ => events.emit(BackendEvent::CallControlFailed {
                                message: "That person is not waiting.".to_owned(),
                            }),
                        }
                    }
                }
                BackendCommand::DenyWaitingUser { jid } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("call_deny:{jid}"));
                    }
                    if take_failure("DenyWaitingUser") {
                        events.emit(BackendEvent::CallControlFailed {
                            message: "Refusing them failed: connection lost".to_owned(),
                        });
                    } else {
                        let mut state =
                            call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match &mut state.current {
                            Some(call) if call.waiting.as_deref() == Some(jid.as_str()) => {
                                call.waiting = None;
                                let approval = call.approval;
                                events.emit(BackendEvent::CallWaitingRoom {
                                    is_admin: true,
                                    approval_required: approval,
                                    users: Vec::new(),
                                });
                            }
                            _ => events.emit(BackendEvent::CallControlFailed {
                                message: "That person is not waiting.".to_owned(),
                            }),
                        }
                    }
                }
                BackendCommand::SetCallApproval { enabled } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("call_approval:{enabled}"));
                    }
                    if take_failure("SetCallApproval") {
                        events.emit(BackendEvent::CallControlFailed {
                            message: "Changing the waiting room failed: connection lost".to_owned(),
                        });
                    } else {
                        let mut state =
                            call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match &mut state.current {
                            Some(call) => {
                                call.approval = enabled;
                                let users: Vec<crate::CallWaitingUser> = call
                                    .waiting
                                    .iter()
                                    .map(|jid| crate::CallWaitingUser {
                                        jid: jid.clone(),
                                        phone: None,
                                    })
                                    .collect();
                                events.emit(BackendEvent::CallWaitingRoom {
                                    is_admin: true,
                                    approval_required: enabled,
                                    users,
                                });
                            }
                            None => events.emit(BackendEvent::CallControlFailed {
                                message: "No call is in progress.".to_owned(),
                            }),
                        }
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
                    if take_failure("SetCallMuted") {
                        events.emit(BackendEvent::CallAudioFailed {
                            message: "Setting mute failed: connection lost".to_owned(),
                        });
                    } else {
                        let state =
                            call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if state.current.is_none() {
                            events.emit(BackendEvent::CallAudioFailed {
                                message: "No call is in progress.".to_owned(),
                            });
                        } else {
                            events.emit(BackendEvent::CallAudioState {
                                muted: Some(muted),
                                speaker: None,
                            });
                        }
                    }
                }
                BackendCommand::SetCallSpeaker { speaker } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("call_speaker:{speaker}"));
                    }
                    if take_failure("SetCallSpeaker") {
                        events.emit(BackendEvent::CallAudioFailed {
                            message: "Setting speaker failed: connection lost".to_owned(),
                        });
                    } else {
                        let state =
                            call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if state.current.is_none() {
                            events.emit(BackendEvent::CallAudioFailed {
                                message: "No call is in progress.".to_owned(),
                            });
                        } else {
                            events.emit(BackendEvent::CallAudioState {
                                muted: None,
                                speaker: Some(speaker),
                            });
                        }
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
                    // A created poll carries its (placeholder)
                    // secret and creator, like the real backend's
                    // creation upsert does.
                    msg.poll_secret = Some(vec![0xFA; 32]);
                    msg.poll_creator = Some(FAKE_OWN_JID.to_owned());
                    polls.insert(msg.id.clone(), msg.clone());
                    events.emit(BackendEvent::MessageUpsert(msg));
                }
                BackendCommand::RegisterPollSecret { chat_jid, message_id, .. } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("poll_secret:{chat_jid}:{message_id}"));
                    }
                }
                BackendCommand::VotePoll { chat_jid, message_id, options } => {
                    // The contract matches the real backend: a
                    // vote is answered by a PollVote event carrying
                    // my new full selection (replace-set semantics:
                    // the payload is my whole current vote set, not
                    // a sequence of toggle clicks, and an empty set
                    // retracts). Tallies are the app's aggregation
                    // job; the fake only keeps its own record of my
                    // votes so its model of the poll stays coherent.
                    // An unknown poll answers with the honest
                    // failure, like the real backend does when a
                    // poll's secret never reached the device.
                    if take_failure("VotePoll") {
                        events.emit(BackendEvent::PollVoteFailed {
                            chat_jid,
                            message_id,
                            message: "Voting failed: connection lost".to_owned(),
                        });
                    } else if let Some(msg) = polls.get_mut(&message_id) {
                        if let crate::LiveMessageKind::Poll { my_votes, .. } = &mut msg.kind {
                            *my_votes = options.clone();
                        }
                        events.emit(BackendEvent::PollVote {
                            chat_jid,
                            message_id,
                            voter_jid: FAKE_OWN_JID.to_owned(),
                            from_me: true,
                            selected: options,
                        });
                    } else {
                        events.emit(BackendEvent::PollVoteFailed {
                            chat_jid,
                            message_id,
                            message: "Voting failed: that poll is not part of this chat."
                                .to_owned(),
                        });
                    }
                }
                BackendCommand::CreateEvent {
                    chat_jid,
                    name,
                    description,
                    start_time,
                    end_time,
                    location,
                    join_link,
                    scheduled_call,
                    extra_guests,
                } => {
                    sent_counter += 1;
                    let mut msg = LiveMessage::text(
                        format!("fake-event-sent-{sent_counter}"),
                        chat_jid,
                        None,
                        "",
                        1_700_000_300 + i64::try_from(sent_counter).unwrap_or(0),
                        true,
                    );
                    msg.kind = crate::LiveMessageKind::Event {
                        name,
                        description,
                        start_time,
                        end_time,
                        location,
                        join_link,
                        scheduled_call,
                        extra_guests,
                        canceled: false,
                        my_rsvp: None,
                        going: 0,
                        maybe: 0,
                        not_going: 0,
                    };
                    // A created event carries its (placeholder)
                    // secret and creator, like the real backend's
                    // creation upsert does.
                    msg.event_secret = Some(vec![5u8; 32]);
                    msg.event_creator = Some(FAKE_OWN_JID.to_owned());
                    events_map.insert(msg.id.clone(), msg.clone());
                    events.emit(BackendEvent::MessageUpsert(msg));
                }
                BackendCommand::RegisterEventSecret { chat_jid, message_id, .. } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("event_secret_registration:{chat_jid}:{message_id}"));
                    }
                }
                BackendCommand::RespondEvent {
                    chat_jid,
                    message_id,
                    response,
                    extra_guest_count,
                } => {
                    // The contract matches the real backend: an
                    // answer is answered by an EventResponse event
                    // carrying my new current answer (last-response-
                    // wins replace semantics). Tallies are the app's
                    // aggregation job; the fake only keeps its own
                    // record of my answer so its model of the event
                    // stays coherent. An unknown event answers with
                    // the honest failure, like the real backend does
                    // when an event's secret never reached the device.
                    if take_failure("RespondEvent") {
                        events.emit(BackendEvent::EventRespondFailed {
                            chat_jid,
                            message_id,
                            message: "Answering failed: connection lost".to_owned(),
                        });
                    } else if let Some(msg) = events_map.get_mut(&message_id) {
                        if let crate::LiveMessageKind::Event { my_rsvp, .. } = &mut msg.kind {
                            *my_rsvp = Some(response);
                        }
                        events.emit(BackendEvent::EventResponse {
                            chat_jid,
                            message_id,
                            responder_jid: FAKE_OWN_JID.to_owned(),
                            from_me: true,
                            response,
                            extra_guest_count,
                        });
                    } else {
                        events.emit(BackendEvent::EventRespondFailed {
                            chat_jid,
                            message_id,
                            message: "Answering failed: that event is not part of this chat."
                                .to_owned(),
                        });
                    }
                }
                BackendCommand::EditEvent {
                    chat_jid,
                    message_id,
                    name,
                    description,
                    start_time,
                    end_time,
                    location,
                    join_link,
                    scheduled_call,
                    extra_guests,
                    canceled,
                } => {
                    // The contract matches the real backend: a
                    // landed edit is answered by an EventEdited
                    // event carrying the whole new field set (a
                    // cancellation is the same shape with the flag
                    // set), and the fake's stored event takes the
                    // same fields so its model stays coherent.
                    if take_failure("EditEvent") {
                        events.emit(BackendEvent::EventEditFailed {
                            chat_jid,
                            message_id,
                            message: "Editing the event failed: connection lost".to_owned(),
                        });
                    } else if let Some(msg) = events_map.get_mut(&message_id) {
                        if let crate::LiveMessageKind::Event {
                            name: n,
                            description: d,
                            start_time: s,
                            end_time: e,
                            location: l,
                            join_link: j,
                            scheduled_call: sc,
                            extra_guests: eg,
                            canceled: c,
                            ..
                        } = &mut msg.kind
                        {
                            *n = name.clone();
                            *d = description.clone();
                            *s = start_time;
                            *e = end_time;
                            *l = location.clone();
                            *j = join_link.clone();
                            *sc = scheduled_call;
                            *eg = extra_guests;
                            *c = canceled;
                        }
                        events.emit(BackendEvent::EventEdited {
                            chat_jid,
                            message_id,
                            name,
                            description,
                            start_time,
                            end_time,
                            location,
                            join_link,
                            scheduled_call,
                            extra_guests,
                            canceled,
                        });
                    } else {
                        events.emit(BackendEvent::EventEditFailed {
                            chat_jid,
                            message_id,
                            message:
                                "Editing the event failed: that event is not part of this chat."
                                    .to_owned(),
                        });
                    }
                }
                BackendCommand::React { chat_jid, message_id, emoji, .. } => {
                    if take_failure("React") {
                        events.emit(BackendEvent::ReactionFailed {
                            chat_jid,
                            message_id,
                            message: "Reaction failed: connection lost".to_owned(),
                        });
                    } else {
                        events.emit(BackendEvent::ReactionUpdate {
                            chat_jid,
                            message_id,
                            actor: "Me".to_owned(),
                            emoji,
                        });
                    }
                }
                BackendCommand::MarkAsRead { chat_jid, message_ids, .. } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("mark_as_read:{chat_jid}:{}", message_ids.len()));
                    }
                    // The read state syncs back like a real one:
                    // the server echo reports the chat read (the
                    // *Changed family is the adoption channel for
                    // state the server holds; the fake simulates
                    // the echo at zero latency, see the contract
                    // note on BackendEvent).
                    if let Some(chat) = chat_state.get_mut(&chat_jid) {
                        chat.unread = 0;
                    }
                    events.emit(BackendEvent::ChatReadChanged {
                        chat_jid,
                        read: true,
                        unread_count: 0,
                    });
                }
                BackendCommand::MarkAsPlayed { chat_jid, message_ids, .. } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("mark_as_played:{chat_jid}:{}", message_ids.len()));
                    }
                    // No event: a played receipt changes no state
                    // this client mirrors (the note's own read
                    // state rides the MarkAsRead path), exactly
                    // like the real backend, whose handler only
                    // sends the receipt.
                }
                BackendCommand::SetTyping { chat_jid, typing } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("typing:{chat_jid}:{typing}"));
                    }
                }
                BackendCommand::SetPresenceSubscription { chat_jid, subscribed } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("presence:{chat_jid}:{subscribed}"));
                    }
                    // The contract the real backend honors: a
                    // subscription is answered with the contact's
                    // current presence. Generalized (P2-49):
                    // every scripted DM in the roster answers,
                    // not only the one scripted DM: contacts the
                    // script marks online answer online, the
                    // rest answer with a last-seen stamp. Groups
                    // and channels have no presence to report.
                    if subscribed {
                        let online = chat_jid == FAKE_DM_JID
                            || chat_state
                                .get(&chat_jid)
                                .is_some_and(|c| c.kind == LiveChatKind::Dm && c.unread == 0);
                        let is_dm =
                            chat_state.get(&chat_jid).is_some_and(|c| c.kind == LiveChatKind::Dm)
                                || chat_jid.ends_with("@s.whatsapp.net")
                                || chat_jid.ends_with("@lid");
                        if is_dm {
                            events.emit(BackendEvent::PresenceReceived {
                                chat_jid,
                                online,
                                last_seen: if online { None } else { Some(1_700_000_000) },
                            });
                        }
                    }
                }
                BackendCommand::EditMessage { chat_jid, message_id, new_body } => {
                    if take_failure("EditMessage") {
                        events.emit(BackendEvent::EditMessageFailed {
                            chat_jid,
                            message_id,
                            message: "Editing failed: connection lost".to_owned(),
                        });
                    } else {
                        events.emit(BackendEvent::MessageEdited { chat_jid, message_id, new_body });
                    }
                }
                BackendCommand::RevokeMessage { chat_jid, message_id, .. } => {
                    if take_failure("RevokeMessage") {
                        events.emit(BackendEvent::RevokeMessageFailed {
                            chat_jid,
                            message_id,
                            message: "Deleting for everyone failed: connection lost".to_owned(),
                        });
                    } else {
                        events.emit(BackendEvent::MessageRevoked { chat_jid, message_id });
                    }
                }
                BackendCommand::DeleteMessageForMe { chat_jid, message_id, .. } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("delete_for_me:{chat_jid}:{message_id}"));
                    }
                    if take_failure("DeleteMessageForMe") {
                        events.emit(BackendEvent::DeleteForMeFailed {
                            chat_jid,
                            message_id,
                            message: "Deleting the message failed: connection lost".to_owned(),
                        });
                    } else {
                        message_state.remove(&(chat_jid.clone(), message_id.clone()));
                        events.emit(BackendEvent::MessageDeletedForMe { chat_jid, message_id });
                    }
                }
                BackendCommand::KeepMessage { chat_jid, message_id, keep, .. } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("keep:{chat_jid}:{message_id}:{keep}"));
                    }
                    if message_state.contains_key(&(chat_jid.clone(), message_id.clone())) {
                        events.emit(BackendEvent::MessageKept { chat_jid, message_id, kept: keep });
                    } else {
                        events.emit(BackendEvent::KeepFailed {
                            chat_jid,
                            message_id,
                            message: "That message is not part of the test-mode script.".to_owned(),
                        });
                    }
                }
                BackendCommand::ClearChat { chat_jid, keep_starred } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("clear_chat:{chat_jid}:{keep_starred}"));
                    }
                    if take_failure("ClearChat") {
                        events.emit(BackendEvent::ClearChatFailed {
                            chat_jid,
                            message: "Clearing the chat failed: connection lost".to_owned(),
                        });
                    } else {
                        message_state.retain(|(chat, _), msg| {
                            *chat != chat_jid || (keep_starred && msg.starred)
                        });
                        events.emit(BackendEvent::ChatCleared {
                            chat_jid,
                            delete_starred: !keep_starred,
                            last_message_timestamp: None,
                        });
                    }
                }
                BackendCommand::DeleteChat { chat_jid } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("delete_chat:{chat_jid}"));
                    }
                    if take_failure("DeleteChat") {
                        events.emit(BackendEvent::DeleteChatFailed {
                            chat_jid,
                            message: "Deleting the chat failed: connection lost".to_owned(),
                        });
                    } else {
                        chat_state.remove(&chat_jid);
                        message_state.retain(|(chat, _), _| *chat != chat_jid);
                        events.emit(BackendEvent::ChatDeleted { chat_jid });
                    }
                }
                BackendCommand::MarkChatUnread { chat_jid } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("mark_unread:{chat_jid}"));
                    }
                    if take_failure("MarkChatUnread") {
                        events.emit(BackendEvent::MarkUnreadFailed {
                            chat_jid,
                            message: "Marking the chat as unread failed: connection lost"
                                .to_owned(),
                        });
                    } else if let Some(chat) = chat_state.get_mut(&chat_jid) {
                        chat.unread = chat.unread.max(1);
                        let chat = chat.clone();
                        events.emit(BackendEvent::ChatUpsert(chat));
                    }
                }
                BackendCommand::MarkChatRead { chat_jid } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("mark_read:{chat_jid}"));
                    }
                    if take_failure("MarkChatRead") {
                        events.emit(BackendEvent::MarkReadFailed {
                            chat_jid,
                            message: "Marking the chat as read failed: connection lost".to_owned(),
                        });
                    } else if let Some(chat) = chat_state.get_mut(&chat_jid) {
                        chat.unread = 0;
                        let chat = chat.clone();
                        events.emit(BackendEvent::ChatUpsert(chat));
                    }
                }
                BackendCommand::SendContact { chat_jid, send_id, name, phone, ephemeral_secs } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("send_contact:{chat_jid}:{phone}"));
                    }
                    sent_counter += 1;
                    let id = send_id.unwrap_or_else(|| format!("fake-sent-{sent_counter}"));
                    let ts = 1_700_000_300 + i64::try_from(sent_counter).unwrap_or(0);
                    let mut echo = LiveMessage {
                        kind: crate::LiveMessageKind::Contact { name: name.clone(), phone },
                        ..LiveMessage::text(id.clone(), chat_jid.clone(), None, name, ts, true)
                    };
                    if ephemeral_secs > 0 {
                        echo.expires_at = ts + i64::from(ephemeral_secs);
                    }
                    message_state.insert((chat_jid.clone(), id.clone()), echo.clone());
                    events.emit(BackendEvent::MessageSent {
                        chat_jid: chat_jid.clone(),
                        message_id: id,
                    });
                    events.emit(BackendEvent::MessageUpsert(echo));
                }
                BackendCommand::ReportSpam { chat_jid, message_id, .. } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("report_spam:{chat_jid}:{message_id}"));
                    }
                    if message_state.contains_key(&(chat_jid.clone(), message_id.clone())) {
                        events.emit(BackendEvent::SpamReported { chat_jid });
                    } else {
                        events.emit(BackendEvent::SpamReportFailed {
                            message: "That message is not part of the test-mode script.".to_owned(),
                        });
                    }
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
                BackendCommand::SetPrivacyExceptions { category, jids } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("privacy_exceptions:{category}:{}", jids.len()));
                    }
                    // Mirrors the real backend: the category
                    // becomes contact_blacklist with this list,
                    // and the fresh state is re-emitted.
                    privacy_state.insert(category, "contact_blacklist".to_owned());
                    events.emit(BackendEvent::PrivacyState(privacy_state.clone()));
                }
                BackendCommand::SetStatusMute { author_jid, muted } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("status_mute:{author_jid}:{muted}"));
                    }
                }
                BackendCommand::SetStatusPrivacy { mode, jids } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("status_privacy:{mode}:{}", jids.len()));
                    }
                    // The fake carries the audience in the same
                    // map keys the real backend's merged
                    // PrivacyState uses, so the app's adoption
                    // path is exercised identically.
                    privacy_state.insert("statusprivacy".to_owned(), mode);
                    privacy_state.insert("statusprivacy_list".to_owned(), jids.join(","));
                    events.emit(BackendEvent::PrivacyState(privacy_state.clone()));
                }
                BackendCommand::FetchStatusPrivacy => {
                    events.emit(BackendEvent::PrivacyState(privacy_state.clone()));
                }
                BackendCommand::FetchOwnProfile => {
                    events.emit(BackendEvent::OwnProfile(own_profile_state.clone()));
                }
                BackendCommand::UpdateOwnProfile { name, about, photo } => {
                    if let Some(log) = &command_log {
                        let mut log = log.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(name) = &name {
                            log.push(format!("profile:name={name}"));
                        }
                        if let Some(about) = &about {
                            log.push(format!("profile:about={about}"));
                        }
                        if let Some(photo) = &photo {
                            match photo {
                                Some(bytes) => {
                                    log.push(format!("profile:photo=set:{}", bytes.len()))
                                }
                                None => log.push("profile:photo=remove".to_owned()),
                            }
                        }
                    }
                    // Mirrors the real backend (and the library
                    // behind it): an empty name is refused before
                    // anything is applied, the failure is reported,
                    // and the unchanged profile is re-emitted so
                    // the UI reconciles with what was kept.
                    if name.as_deref().is_some_and(|n| n.trim().is_empty()) {
                        events.emit(BackendEvent::OwnProfileSaveFailed {
                            message: "Profile change failed: the name cannot be empty.".to_owned(),
                        });
                        events.emit(BackendEvent::OwnProfile(own_profile_state.clone()));
                    } else {
                        if let Some(name) = name {
                            own_profile_state.name = name.trim().to_owned();
                        }
                        if let Some(about) = about {
                            own_profile_state.about = about.trim().to_owned();
                        }
                        if let Some(photo) = photo {
                            own_profile_state.photo = photo;
                        }
                        events.emit(BackendEvent::OwnProfile(own_profile_state.clone()));
                    }
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
                    if take_failure("StarMessage") {
                        events.emit(BackendEvent::StarFailed {
                            chat_jid,
                            message_id,
                            message: "Starring failed: connection lost".to_owned(),
                        });
                    } else {
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
                        // The server echo, as app-state sync
                        // delivers it from a real session.
                        events.emit(BackendEvent::StarChanged {
                            chat_jid,
                            message_id,
                            starred: star,
                        });
                    }
                }
                BackendCommand::PinMessage { chat_jid, message_id, pin, .. } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("pin_message:{chat_jid}:{message_id}:{pin}"));
                    }
                    if take_failure("PinMessage") {
                        events.emit(BackendEvent::PinFailed {
                            chat_jid,
                            message_id,
                            message: "Pinning failed: connection lost".to_owned(),
                        });
                    } else {
                        let key = (chat_jid.clone(), message_id.clone());
                        if let Some(msg) = message_state.get_mut(&key) {
                            msg.pinned = pin;
                            let msg = msg.clone();
                            events.emit(BackendEvent::MessageUpsert(msg));
                        }
                        // The pin protocol echo, as a real
                        // session delivers it.
                        events.emit(BackendEvent::MessagePinChanged {
                            chat_jid,
                            message_id,
                            pinned: pin,
                        });
                    }
                }
                BackendCommand::ChatAction { chat_jid, action } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("chat_action:{chat_jid}:{action:?}"));
                    }
                    if take_failure("ChatAction") {
                        events.emit(BackendEvent::ChatActionFailed {
                            chat_jid,
                            action,
                            message: "That chat action failed: connection lost".to_owned(),
                        });
                    } else if let Some(chat) = chat_state.get_mut(&chat_jid) {
                        match action {
                            crate::ChatAction::Archive => chat.archived = true,
                            crate::ChatAction::Unarchive => chat.archived = false,
                            crate::ChatAction::Pin => chat.pinned = true,
                            crate::ChatAction::Unpin => chat.pinned = false,
                            crate::ChatAction::Mute => {
                                chat.muted = true;
                                chat.mute_until = 0;
                            }
                            crate::ChatAction::MuteUntil(until_ms) => {
                                chat.muted = true;
                                chat.mute_until = until_ms / 1000;
                            }
                            crate::ChatAction::Unmute => {
                                chat.muted = false;
                                chat.mute_until = 0;
                            }
                            crate::ChatAction::Lock => chat.locked = true,
                            crate::ChatAction::Unlock => chat.locked = false,
                        }
                        let chat = chat.clone();
                        events.emit(BackendEvent::ChatUpsert(chat.clone()));
                        // The server echoes, as app-state sync
                        // delivers them from a real session: the
                        // archive and mute states are *Changed
                        // events there, and the pin state rides
                        // PinChanged.
                        match action {
                            crate::ChatAction::Archive | crate::ChatAction::Unarchive => {
                                events.emit(BackendEvent::ArchiveChanged {
                                    chat_jid: chat.jid.clone(),
                                    archived: chat.archived,
                                });
                            }
                            crate::ChatAction::Pin | crate::ChatAction::Unpin => {
                                events.emit(BackendEvent::PinChanged {
                                    chat_jid: chat.jid.clone(),
                                    pinned: chat.pinned,
                                });
                            }
                            crate::ChatAction::Mute
                            | crate::ChatAction::MuteUntil(_)
                            | crate::ChatAction::Unmute => {
                                events.emit(BackendEvent::MuteChanged {
                                    chat_jid: chat.jid.clone(),
                                    muted: chat.muted,
                                    mute_until: chat.mute_until,
                                });
                            }
                            crate::ChatAction::Lock | crate::ChatAction::Unlock => {
                                events.emit(BackendEvent::LockChanged {
                                    chat_jid: chat.jid.clone(),
                                    locked: chat.locked,
                                });
                            }
                        }
                    }
                }
                BackendCommand::BlockContact { chat_jid, block } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("block:{chat_jid}:{block}"));
                    }
                    if take_failure("BlockContact") {
                        events.emit(BackendEvent::BlockFailed {
                            chat_jid,
                            message: "Blocking failed: connection lost".to_owned(),
                        });
                    } else {
                        if block {
                            blocked_state.insert(chat_jid);
                        } else {
                            blocked_state.remove(&chat_jid);
                        }
                        let mut list: Vec<String> = blocked_state.iter().cloned().collect();
                        list.sort();
                        events.emit(BackendEvent::Blocklist(list));
                    }
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
                        participants
                            .iter()
                            .map(|p| (p.clone(), String::new(), false, None))
                            .collect(),
                    );
                    chat_state.insert(
                        new_jid.clone(),
                        LiveChat {
                            jid: new_jid.clone(),
                            title: name.clone(),
                            kind: LiveChatKind::Group,
                            from_history: false,
                            preview: "Group created".to_owned(),
                            preview_sender: None,
                            timestamp: 1_700_000_500,
                            unread: 0,
                            archived: false,
                            locked: false,
                            pinned: false,
                            ephemeral_secs: 0,
                            muted: false,
                            mute_until: 0,
                            parent_jid: None,
                            is_parent: false,
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
                        locked: false,
                        pinned: false,
                        ephemeral_secs: 0,
                        muted: false,
                        mute_until: 0,
                        parent_jid: None,
                        is_parent: false,
                        from_history: false,
                    }));
                }
                BackendCommand::CreateCommunity { name } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("create_community:{name}"));
                    }
                    community_counter += 1;
                    let new_jid = format!("300000000{community_counter}@g.us");
                    events.emit(BackendEvent::CommunityCreated { jid: new_jid, name });
                }
                BackendCommand::LinkCommunityGroups { community_jid, group_jids } => {
                    if let Some(log) = &command_log {
                        log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(
                            format!(
                                "link_community_groups:{community_jid}:{}",
                                group_jids.join(",")
                            ),
                        );
                    }
                    let mut linked = Vec::new();
                    let mut failed = Vec::new();
                    for group_jid in &group_jids {
                        match chat_state.get_mut(group_jid) {
                            Some(chat)
                                if chat.kind == LiveChatKind::Group
                                    && *group_jid != community_jid =>
                            {
                                chat.parent_jid = Some(community_jid.clone());
                                linked.push(group_jid.clone());
                            }
                            _ => failed.push((group_jid.clone(), 404)),
                        }
                    }
                    events.emit(BackendEvent::CommunityGroupsLinked {
                        community_jid: community_jid.clone(),
                        linked: linked.clone(),
                        failed,
                    });
                    for group_jid in &linked {
                        events.emit(BackendEvent::ChatCommunity {
                            chat_jid: group_jid.clone(),
                            parent_jid: Some(community_jid.clone()),
                            is_parent: false,
                        });
                    }
                }
                BackendCommand::UnlinkCommunityGroups { community_jid, group_jids } => {
                    if let Some(log) = &command_log {
                        log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(
                            format!(
                                "unlink_community_groups:{community_jid}:{}",
                                group_jids.join(",")
                            ),
                        );
                    }
                    let mut unlinked = Vec::new();
                    let mut failed = Vec::new();
                    for group_jid in &group_jids {
                        match chat_state.get_mut(group_jid) {
                            Some(chat)
                                if chat.parent_jid.as_deref() == Some(community_jid.as_str()) =>
                            {
                                chat.parent_jid = None;
                                unlinked.push(group_jid.clone());
                            }
                            _ => failed.push((group_jid.clone(), 404)),
                        }
                    }
                    events.emit(BackendEvent::CommunityGroupsUnlinked {
                        community_jid: community_jid.clone(),
                        unlinked: unlinked.clone(),
                        failed,
                    });
                    for group_jid in &unlinked {
                        events.emit(BackendEvent::ChatCommunity {
                            chat_jid: group_jid.clone(),
                            parent_jid: None,
                            is_parent: false,
                        });
                    }
                }
                BackendCommand::CreateCommunitySubgroup { community_jid, name } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("create_community_subgroup:{community_jid}:{name}"));
                    }
                    subgroup_counter += 1;
                    let new_jid = format!("400000000{subgroup_counter}@g.us");
                    let group = LiveChat {
                        jid: new_jid.clone(),
                        title: name.clone(),
                        kind: LiveChatKind::Group,
                        preview: "Group created".to_owned(),
                        preview_sender: None,
                        timestamp: 1_700_000_500,
                        unread: 0,
                        archived: false,
                        locked: false,
                        pinned: false,
                        ephemeral_secs: 0,
                        muted: false,
                        mute_until: 0,
                        parent_jid: Some(community_jid.clone()),
                        is_parent: false,
                        from_history: false,
                    };
                    chat_state.insert(new_jid, group.clone());
                    events.emit(BackendEvent::CommunitySubgroupCreated { community_jid, group });
                }
                BackendCommand::RemoveCommunityParticipants { community_jid, participants } => {
                    if let Some(log) = &command_log {
                        log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(
                            format!(
                                "remove_community_participants:{community_jid}:{}",
                                participants.join(",")
                            ),
                        );
                    }
                    // Removal covers the parent group and every
                    // linked subgroup, like the real backend.
                    let affected: Vec<String> = chat_state
                        .values()
                        .filter(|c| {
                            c.jid == community_jid
                                || c.parent_jid.as_deref() == Some(community_jid.as_str())
                        })
                        .map(|c| c.jid.clone())
                        .collect();
                    for jid in &affected {
                        if let Some(members) = group_state.get_mut(jid) {
                            members.retain(|(j, _, _, _)| !participants.contains(j));
                        }
                    }
                    events.emit(BackendEvent::CommunityParticipantsRemoved {
                        community_jid: community_jid.clone(),
                        participants: participants.clone(),
                    });
                    let members = group_state
                        .entry(community_jid.clone())
                        .or_insert_with(default_members)
                        .clone();
                    events.emit(BackendEvent::GroupMembers {
                        own_label: own_label_state.get(&community_jid).cloned(),
                        chat_jid: community_jid,
                        members,
                        own_admin: true,
                    });
                }
                BackendCommand::DeactivateCommunity { community_jid } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("deactivate_community:{community_jid}"));
                    }
                    // Subgroups survive as ordinary groups; only
                    // the parent chat goes away.
                    for chat in chat_state.values_mut() {
                        if chat.parent_jid.as_deref() == Some(community_jid.as_str()) {
                            chat.parent_jid = None;
                        }
                    }
                    chat_state.remove(&community_jid);
                    group_state.remove(&community_jid);
                    events.emit(BackendEvent::CommunityDeactivated { community_jid });
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
                                if !members.iter().any(|(j, _, _, _)| j == p) {
                                    members.push((p.clone(), String::new(), false, None));
                                }
                            }
                        }
                        crate::ParticipantOp::Remove => {
                            members.retain(|(j, _, _, _)| !participants.contains(j));
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
                    events.emit(BackendEvent::GroupMembers {
                        own_label: own_label_state.get(&chat_jid).cloned(),
                        chat_jid,
                        members: members.clone(),
                        own_admin: true,
                    });
                }
                BackendCommand::AddGroupParticipantsWithHistory {
                    chat_jid,
                    participants,
                    history,
                } => {
                    if let Some(log) = &command_log {
                        log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(
                            format!(
                                "add_with_history:{chat_jid}:{}:{}",
                                participants.join(","),
                                history.len()
                            ),
                        );
                    }
                    if take_failure("AddGroupParticipantsWithHistory") {
                        events.emit(BackendEvent::Error {
                            message: "That group change failed: connection lost".to_owned(),
                        });
                    } else {
                        // Scripted refusal: an address carrying
                        // "refused" is not on WhatsApp.
                        let (added_refs, refused_refs): (Vec<&String>, Vec<&String>) =
                            participants.iter().partition(|p| !p.contains("refused"));
                        let added: Vec<String> = added_refs.iter().map(|p| (*p).clone()).collect();
                        let refused: Vec<(String, String)> = refused_refs
                            .iter()
                            .map(|p| ((*p).clone(), "not on WhatsApp".to_owned()))
                            .collect();
                        let members =
                            group_state.entry(chat_jid.clone()).or_insert_with(default_members);
                        for p in &added {
                            if !members.iter().any(|(j, _, _, _)| j == p) {
                                members.push((p.clone(), String::new(), false, None));
                            }
                        }
                        let members = members.clone();
                        events.emit(BackendEvent::GroupMembers {
                            own_label: own_label_state.get(&chat_jid).cloned(),
                            chat_jid: chat_jid.clone(),
                            members,
                            own_admin: true,
                        });
                        // Scripted history outcomes: a "flaky"
                        // participant makes the share send fail
                        // retryably (the retry arm below then
                        // succeeds); no records means nothing
                        // was eligible; otherwise shared.
                        let report = if participants.iter().any(|p| p.contains("flaky")) {
                            if !history.is_empty() {
                                history_retry_state.insert(chat_jid.clone(), history.len());
                            }
                            crate::HistoryShareReport::Failed {
                                error: "sharing the history failed: connection lost".to_owned(),
                                retryable: true,
                            }
                        } else if history.is_empty() {
                            crate::HistoryShareReport::Skipped {
                                reason: "there were no messages the protocol can share".to_owned(),
                            }
                        } else {
                            crate::HistoryShareReport::Shared { messages: history.len() }
                        };
                        events.emit(BackendEvent::GroupHistoryReported {
                            chat_jid,
                            added,
                            refused,
                            report,
                        });
                    }
                }
                BackendCommand::RetryGroupHistoryShare { chat_jid } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("retry_history:{chat_jid}"));
                    }
                    let report = if take_failure("RetryGroupHistoryShare") {
                        // An injected failure changes no fake
                        // state, like a send that never reached a
                        // server: the prepared share survives and
                        // the answer is the retryable failure, so
                        // a later retry can still land it.
                        crate::HistoryShareReport::Failed {
                            error: "sharing the history failed: connection lost".to_owned(),
                            retryable: true,
                        }
                    } else {
                        match history_retry_state.remove(&chat_jid) {
                            Some(messages) => crate::HistoryShareReport::Shared { messages },
                            None => crate::HistoryShareReport::Failed {
                                error: "the prepared share did not survive; add the members again to share history"
                                    .to_owned(),
                                retryable: false,
                            },
                        }
                    };
                    events.emit(BackendEvent::GroupHistoryReported {
                        chat_jid,
                        added: Vec::new(),
                        refused: Vec::new(),
                        report,
                    });
                }
                BackendCommand::SetOwnMemberLabel { chat_jid, label } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("member_label:{chat_jid}:{label}"));
                    }
                    if take_failure("SetOwnMemberLabel") {
                        events.emit(BackendEvent::MemberLabelFailed {
                            chat_jid,
                            message: "Setting your member label failed: connection lost".to_owned(),
                        });
                    } else {
                        if label.is_empty() {
                            own_label_state.remove(&chat_jid);
                        } else {
                            own_label_state.insert(chat_jid.clone(), label.clone());
                        }
                        events.emit(BackendEvent::MemberLabelSet { chat_jid, label });
                    }
                }
                BackendCommand::ReportMessageToAdmins { chat_jid, message_id } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("report_to_admins:{chat_jid}:{message_id}"));
                    }
                    if take_failure("ReportMessageToAdmins") {
                        events.emit(BackendEvent::ReportToAdminsFailed {
                            chat_jid,
                            message_id,
                            message: "Reporting that message failed: connection lost".to_owned(),
                        });
                    } else {
                        reported_state.entry(chat_jid.clone()).or_default().push(
                            crate::ReportedMessageEntry {
                                message_id: message_id.clone(),
                                reporters: vec!["You".to_owned()],
                            },
                        );
                        events.emit(BackendEvent::MessageReportedToAdmins { chat_jid, message_id });
                    }
                }
                BackendCommand::FetchReportedMessages { chat_jid } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("fetch_reported:{chat_jid}"));
                    }
                    if take_failure("FetchReportedMessages") {
                        events.emit(BackendEvent::ReportedMessagesFailed {
                            chat_jid,
                            message: "Loading the reported messages failed: connection lost"
                                .to_owned(),
                        });
                    } else {
                        let reports = reported_state.get(&chat_jid).cloned().unwrap_or_default();
                        events.emit(BackendEvent::ReportedMessages { chat_jid, reports });
                    }
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
                    if take_failure("SetGroupInfo") {
                        events.emit(BackendEvent::GroupInfoFailed {
                            chat_jid,
                            message: "Renaming the group failed: connection lost".to_owned(),
                        });
                    } else {
                        if let (Some(subject), Some(chat)) =
                            (subject, chat_state.get_mut(&chat_jid))
                        {
                            chat.title = subject;
                            let chat = chat.clone();
                            events.emit(BackendEvent::ChatUpsert(chat));
                        }
                        // The outcome event the real backend
                        // answers with; the app applies its title
                        // and its saved notice only from it.
                        events.emit(BackendEvent::GroupInfoSaved { chat_jid });
                    }
                }
                BackendCommand::FetchGroupMembers { chat_jid } => {
                    let members =
                        group_state.entry(chat_jid.clone()).or_insert_with(default_members).clone();
                    // The scripted account is an admin of the
                    // scripted group (Sarah's row carries the
                    // admin flag in the demo data's own terms);
                    // the flag is what the app gates on.
                    events.emit(BackendEvent::GroupMembers {
                        own_label: own_label_state.get(&chat_jid).cloned(),
                        chat_jid: chat_jid.clone(),
                        members,
                        own_admin: true,
                    });
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
                        if !members.iter().any(|(j, _, _, _)| j == &participant) {
                            members.push((participant.clone(), name, false, None));
                            let members = members.clone();
                            events.emit(BackendEvent::GroupMembers {
                                own_label: own_label_state.get(&chat_jid).cloned(),
                                chat_jid: chat_jid.clone(),
                                members,
                                own_admin: true,
                            });
                        }
                    }
                    let requests = join_request_state.get(&chat_jid).cloned().unwrap_or_default();
                    events.emit(BackendEvent::JoinRequests { chat_jid, requests });
                }
                BackendCommand::SetLinkPreviews { enabled } => {
                    // The fake never fetches a page (demo mode is
                    // offline by construction), so the flag is only
                    // recorded like the other preference commands:
                    // the app's plumbing is exercised, nothing is
                    // generated.
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("link_previews:{enabled}"));
                    }
                }
                BackendCommand::SetDefaultDisappearingTimer { duration_secs } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("default_disappearing:{duration_secs}"));
                    }
                    // The real backend hears its own change back
                    // as the account sync notification; the fake
                    // answers with the same event so the app's
                    // adoption path runs in demo mode too.
                    events.emit(BackendEvent::DefaultDisappearingTimerChanged { duration_secs });
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
                    } else if code.contains("approval") {
                        // Scripted approval shape (P2-47): the
                        // same outcome the live backend reports
                        // for an approval-gated group, so the
                        // pending state can be rehearsed. No chat
                        // row appears.
                        events.emit(BackendEvent::JoinRequestPending);
                    } else if code.contains("invalid") || code.contains("revoked") {
                        // Scripted refusal shape (P2-47).
                        events.emit(BackendEvent::Error {
                            message:
                                "Joining that group failed: the invite link is invalid or revoked."
                                    .to_owned(),
                        });
                    } else {
                        let jid = "10000000077@g.us".to_owned();
                        let chat = LiveChat {
                            jid: jid.clone(),
                            title: "Joined via link".to_owned(),
                            kind: LiveChatKind::Group,
                            from_history: false,
                            preview: "Group joined".to_owned(),
                            preview_sender: None,
                            timestamp: 1_700_000_600,
                            unread: 0,
                            archived: false,
                            locked: false,
                            pinned: false,
                            ephemeral_secs: 0,
                            muted: false,
                            mute_until: 0,
                            parent_jid: None,
                            is_parent: false,
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
                    if take_failure("LeaveGroup") {
                        events.emit(BackendEvent::LeaveGroupFailed {
                            chat_jid,
                            message: "Leaving the group failed: connection lost".to_owned(),
                        });
                    } else if let Some(chat) = chat_state.get_mut(&chat_jid) {
                        chat.archived = true;
                        let chat = chat.clone();
                        events.emit(BackendEvent::ChatUpsert(chat));
                    }
                }
                BackendCommand::DownloadMedia { chat_jid, message_id, .. } => {
                    if !self.demo_script {
                        events.emit(BackendEvent::Error {
                            message: "That file is not available in the demo.".to_owned(),
                        });
                    } else {
                        // The demo download (P2-44): the fixture
                        // bytes for the message's kind, written
                        // under the downloads folder with the
                        // shared stem rule, answered with
                        // MediaSaved exactly as the real backend
                        // answers. A view-once message refuses,
                        // as the real one does; an unknown
                        // message keeps the honest demo error.
                        let scripted = message_state
                            .get(&(chat_jid.clone(), message_id.clone()))
                            .map(|m| (m.kind.clone(), m.view_once))
                            .or_else(|| {
                                scripted_statuses()
                                    .into_iter()
                                    .find(|s| s.id == message_id)
                                    .map(|s| (s.kind, false))
                            });
                        match scripted {
                            Some((_, true)) => {
                                events.emit(BackendEvent::Error {
                                    message: "View-once media cannot be saved.".to_owned(),
                                });
                            }
                            Some((kind, false)) => {
                                let (bytes, ext) = match kind {
                                    crate::LiveMessageKind::Media {
                                        media: crate::MediaKind::Video,
                                        ..
                                    } => (FIXTURE_MP4.to_vec(), "mp4"),
                                    crate::LiveMessageKind::Media {
                                        media: crate::MediaKind::Voice | crate::MediaKind::Audio,
                                        ..
                                    } => (FIXTURE_VOICE_OPUS.to_vec(), "opus"),
                                    _ => (fixture_png(&message_id), "png"),
                                };
                                let dir = config.data_dir.join("downloads");
                                let stem = crate::downloaded_media_stem(&chat_jid, &message_id);
                                let path = dir.join(format!("{stem}.{ext}"));
                                let saved = std::fs::create_dir_all(&dir)
                                    .and_then(|()| std::fs::write(&path, &bytes));
                                match saved {
                                    Ok(()) => events.emit(BackendEvent::MediaSaved {
                                        chat_jid,
                                        message_id,
                                        path: path.to_string_lossy().into_owned(),
                                    }),
                                    Err(_) => events.emit(BackendEvent::Error {
                                        message: "Saving that file failed.".to_owned(),
                                    }),
                                }
                            }
                            None => {
                                events.emit(BackendEvent::Error {
                                    message: "That file is not available in the demo.".to_owned(),
                                });
                            }
                        }
                    }
                }
                // In the demo script the media fetches serve
                // fixture bytes (P2-44), the way the sticker
                // fetch already did: media QA (fetch state,
                // decode, display, playback) can be rehearsed
                // without a live account. The waiting fake
                // keeps the honest per-message failures. The
                // video fixture is a real generated MP4 (see
                // FIXTURE_MP4), so playback rehearses end to
                // end: a regular video lands in the video
                // cache as VideoReady, a view-once one answers
                // with its bytes, exactly the real backend's
                // two shapes.
                BackendCommand::FetchPhoto { chat_jid, message_id, .. } => {
                    if self.demo_script {
                        let bytes = fixture_png(&message_id);
                        events.emit(BackendEvent::PhotoBytes { chat_jid, message_id, bytes });
                    } else {
                        events.emit(BackendEvent::PhotoFetchFailed {
                            chat_jid,
                            message_id,
                            message: "That media is not available in the demo.".to_owned(),
                        });
                    }
                }
                BackendCommand::FetchVoice { chat_jid, message_id, .. } => {
                    if self.demo_script {
                        events.emit(BackendEvent::VoiceBytes {
                            chat_jid,
                            message_id,
                            bytes: FIXTURE_VOICE_OPUS.to_vec(),
                        });
                    } else {
                        events.emit(BackendEvent::VoiceFetchFailed {
                            chat_jid,
                            message_id,
                            message: "That media is not available in the demo.".to_owned(),
                        });
                    }
                }
                BackendCommand::FetchVideo { chat_jid, message_id, .. } => {
                    if !self.demo_script {
                        events.emit(BackendEvent::VideoFetchFailed {
                            chat_jid,
                            message_id,
                            message: "That media is not available in the demo.".to_owned(),
                        });
                    } else if message_state
                        .get(&(chat_jid.clone(), message_id.clone()))
                        .is_some_and(|m| m.view_once)
                    {
                        events.emit(BackendEvent::VideoBytes {
                            chat_jid,
                            message_id,
                            bytes: FIXTURE_MP4.to_vec(),
                        });
                    } else {
                        let dir = config.data_dir.join("video-cache");
                        let stem = crate::downloaded_media_stem(&chat_jid, &message_id);
                        let path = dir.join(format!("{stem}.mp4"));
                        let saved = std::fs::create_dir_all(&dir)
                            .and_then(|()| std::fs::write(&path, FIXTURE_MP4));
                        match saved {
                            Ok(()) => events.emit(BackendEvent::VideoReady {
                                chat_jid,
                                message_id,
                                path: path.to_string_lossy().into_owned(),
                            }),
                            Err(_) => events.emit(BackendEvent::VideoFetchFailed {
                                chat_jid,
                                message_id,
                                message: "That media is not available in the demo.".to_owned(),
                            }),
                        }
                    }
                }
                BackendCommand::FetchVideoPoster { chat_jid, message_id, .. } => {
                    if self.demo_script {
                        let bytes = fixture_png(&message_id);
                        events.emit(BackendEvent::VideoPosterBytes { chat_jid, message_id, bytes });
                    } else {
                        events.emit(BackendEvent::VideoPosterFetchFailed {
                            chat_jid,
                            message_id,
                            message: "That media is not available in the demo.".to_owned(),
                        });
                    }
                }
                BackendCommand::FetchSticker { chat_jid, message_id, .. } => {
                    if self.demo_script {
                        let bytes = fixture_png(&message_id);
                        events.emit(BackendEvent::StickerBytes { chat_jid, message_id, bytes });
                    } else {
                        events.emit(BackendEvent::StickerFetchFailed { chat_jid, message_id });
                    }
                }
                BackendCommand::FetchStickerByRef { filehash, .. } => {
                    if self.demo_script {
                        let bytes = fixture_png(&filehash);
                        events.emit(BackendEvent::StickerRefBytes { filehash, bytes });
                    } else {
                        events.emit(BackendEvent::StickerRefFetchFailed { filehash });
                    }
                }
                BackendCommand::FetchVerifiedName { jid } => {
                    // The scripted business holds a certified
                    // name under its gate; nothing else in the
                    // fake roster holds one.
                    let name = if business_fixture_enabled() && jid == FAKE_BIZ_JID {
                        Some("Bumi Roast Coffee".to_owned())
                    } else {
                        None
                    };
                    events.emit(BackendEvent::VerifiedNameResult { jid, name });
                }
                BackendCommand::FetchContactAbout { chat_jid } => {
                    // The scripted direct-chat partners carry an
                    // About; anyone else answers empty, the same
                    // shape as a real contact with none set.
                    let known_dm =
                        chat_state.get(&chat_jid).is_some_and(|c| c.kind == LiveChatKind::Dm);
                    events.emit(BackendEvent::ContactAbout {
                        chat_jid,
                        about: if known_dm {
                            "Available (test contact)".to_owned()
                        } else {
                            String::new()
                        },
                    });
                }
                BackendCommand::FetchBusinessProfile { chat_jid } => {
                    if take_failure("FetchBusinessProfile") {
                        events.emit(BackendEvent::BusinessProfileFetchFailed {
                            chat_jid,
                            message: "Could not load the business profile: connection lost"
                                .to_owned(),
                        });
                    } else {
                        let profile = if business_fixture_enabled() && chat_jid == FAKE_BIZ_JID {
                            Some(fixture_business_profile())
                        } else {
                            None
                        };
                        events.emit(BackendEvent::BusinessProfileFetched { chat_jid, profile });
                    }
                }
                BackendCommand::FetchCatalog { chat_jid, after } => {
                    if take_failure("FetchCatalog") {
                        events.emit(BackendEvent::CatalogFetchFailed {
                            chat_jid,
                            message: "Could not load the catalog: connection lost".to_owned(),
                        });
                    } else if business_fixture_enabled() && chat_jid == FAKE_BIZ_JID {
                        let (products, after_cursor) = fixture_catalog_page(after.as_deref());
                        events.emit(BackendEvent::CatalogFetched {
                            chat_jid,
                            products,
                            after_cursor,
                        });
                    } else {
                        events.emit(BackendEvent::CatalogFetched {
                            chat_jid,
                            products: Vec::new(),
                            after_cursor: None,
                        });
                    }
                }
                BackendCommand::FetchCollections { chat_jid } => {
                    if take_failure("FetchCollections") {
                        events.emit(BackendEvent::CollectionsFetchFailed {
                            chat_jid,
                            message: "Could not load the collections: connection lost".to_owned(),
                        });
                    } else {
                        let collections = if business_fixture_enabled() && chat_jid == FAKE_BIZ_JID
                        {
                            fixture_collections()
                        } else {
                            Vec::new()
                        };
                        events.emit(BackendEvent::CollectionsFetched { chat_jid, collections });
                    }
                }
                BackendCommand::FetchProductImage { url } => {
                    if url.is_empty() {
                        events.emit(BackendEvent::ProductImageFetchFailed { url });
                    } else {
                        let bytes = fixture_png(&url);
                        events.emit(BackendEvent::ProductImageBytes { url, bytes });
                    }
                }
                BackendCommand::FetchOrderDetails { chat_jid, order_id, token } => {
                    if take_failure("FetchOrderDetails") {
                        events.emit(BackendEvent::OrderDetailsFetchFailed {
                            chat_jid,
                            order_id,
                            message: "Could not load the order: connection lost".to_owned(),
                        });
                    } else if business_fixture_enabled()
                        && order_id == FAKE_ORDER_ID
                        && token == FAKE_ORDER_TOKEN
                    {
                        events.emit(BackendEvent::OrderDetailsFetched {
                            chat_jid,
                            order_id,
                            order: fixture_order(),
                        });
                    } else {
                        events.emit(BackendEvent::OrderDetailsFetchFailed {
                            chat_jid,
                            order_id,
                            message: "Could not load the order: not found".to_owned(),
                        });
                    }
                }
                BackendCommand::FetchOwnBusinessProfile => {
                    if take_failure("FetchOwnBusinessProfile") {
                        events.emit(BackendEvent::OwnBusinessProfileFetchFailed {
                            message: "Could not load the business profile: connection lost"
                                .to_owned(),
                        });
                    } else {
                        events.emit(BackendEvent::OwnBusinessProfileFetched {
                            profile: own_business_state.clone(),
                        });
                    }
                }
                BackendCommand::UpdateOwnBusinessProfile { update } => {
                    if take_failure("UpdateOwnBusinessProfile") {
                        events.emit(BackendEvent::OwnBusinessProfileSaveFailed {
                            message: "Business profile change failed: connection lost".to_owned(),
                        });
                    } else {
                        if let Some(profile) = &mut own_business_state {
                            merge_business_update(profile, &update);
                        }
                        events.emit(BackendEvent::OwnBusinessProfileSaved);
                    }
                }
                BackendCommand::SaveLabel { label_id, name, color } => {
                    if take_failure("SaveLabel") {
                        events.emit(BackendEvent::LabelActionFailed {
                            action: crate::LabelAction::Save { label_id, name, color },
                            message: "Label change failed: connection lost".to_owned(),
                        });
                    } else {
                        events.emit(BackendEvent::LabelEdited {
                            label_id,
                            name: Some(name),
                            color: Some(color),
                            predefined_id: None,
                            deleted: false,
                        });
                    }
                }
                BackendCommand::DeleteLabel { label_id } => {
                    if take_failure("DeleteLabel") {
                        events.emit(BackendEvent::LabelDeleteFailed {
                            label_id,
                            message: "Deleting the label failed: connection lost".to_owned(),
                        });
                    } else {
                        events.emit(BackendEvent::LabelDeleted { label_id });
                    }
                }
                BackendCommand::SetChatLabel { label_id, chat_jid, labeled } => {
                    if take_failure("SetChatLabel") {
                        events.emit(BackendEvent::LabelActionFailed {
                            action: crate::LabelAction::SetChat { label_id, chat_jid, labeled },
                            message: "Label change failed: connection lost".to_owned(),
                        });
                    } else {
                        events.emit(BackendEvent::ChatLabelChanged { label_id, chat_jid, labeled });
                    }
                }
                BackendCommand::SetMessageLabel { label_id, chat_jid, message_id, labeled } => {
                    if take_failure("SetMessageLabel") {
                        events.emit(BackendEvent::LabelActionFailed {
                            action: crate::LabelAction::SetMessage {
                                label_id,
                                chat_jid,
                                message_id,
                                labeled,
                            },
                            message: "Label change failed: connection lost".to_owned(),
                        });
                    } else {
                        events.emit(BackendEvent::MessageLabelChanged {
                            label_id,
                            chat_jid,
                            message_id,
                            labeled,
                        });
                    }
                }
                BackendCommand::SaveQuickReply { id, shortcut, message, keywords, count } => {
                    if take_failure("SaveQuickReply") {
                        events.emit(BackendEvent::QuickReplyFailed {
                            id,
                            message: "Quick reply change failed: connection lost".to_owned(),
                        });
                    } else {
                        events.emit(BackendEvent::QuickReplyEdited {
                            id,
                            shortcut,
                            message,
                            keywords,
                            count,
                            deleted: false,
                        });
                    }
                }
                BackendCommand::DeleteQuickReply { id } => {
                    if take_failure("DeleteQuickReply") {
                        events.emit(BackendEvent::QuickReplyDeleteFailed {
                            id,
                            message: "Deleting the quick reply failed: connection lost".to_owned(),
                        });
                    } else {
                        events.emit(BackendEvent::QuickReplyDeleted { id });
                    }
                }
                BackendCommand::FetchAvatar { jid, .. } => {
                    // A group picture changed this session wins
                    // over the defaults below: it is the state
                    // the change command left behind.
                    if let Some(changed) = group_picture_state.get(&jid) {
                        match changed {
                            Some(bytes) => events.emit(BackendEvent::AvatarBytes {
                                jid,
                                picture_id: Some("fake-picture-set".to_owned()),
                                bytes: bytes.clone(),
                            }),
                            None => events.emit(BackendEvent::AvatarMissing { jid }),
                        }
                        continue;
                    }
                    // The demo world has pictures for everyone;
                    // the deterministic test world has none, so
                    // tests see the honest negative path.
                    if self.demo_script {
                        let bytes = fixture_png(&jid);
                        events.emit(BackendEvent::AvatarBytes {
                            jid,
                            picture_id: Some("fake-picture".to_owned()),
                            bytes,
                        });
                    } else {
                        events.emit(BackendEvent::AvatarMissing { jid });
                    }
                }
                BackendCommand::FetchOlderMessages { chat_jid, oldest_message_id, .. } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("fetch_older:{chat_jid}:{oldest_message_id}"));
                    }
                    if take_failure("FetchOlderMessages") {
                        events.emit(BackendEvent::HistoryFetchFailed {
                            chat_jid,
                            message: "Loading older messages failed: connection lost".to_owned(),
                        });
                        continue;
                    }
                    // The answer takes a phone round trip even
                    // in test mode: a short delay on a side
                    // thread, so the app's loading state is a
                    // real state and not a one-frame flash. The
                    // page's messages land as ordinary upserts,
                    // then the completion event, exactly the
                    // real backend's order.
                    let (page, end) = older_page(&chat_jid, &oldest_message_id);
                    let tick_events = events.clone();
                    let alive = call_alive.clone();
                    std::thread::Builder::new()
                        .name("zaprust-fake-history-page".to_owned())
                        .spawn(move || {
                            std::thread::sleep(Duration::from_millis(900));
                            if !alive.load(std::sync::atomic::Ordering::Relaxed) {
                                return;
                            }
                            let added = u32::try_from(page.len()).unwrap_or(0);
                            for msg in page {
                                tick_events.emit(BackendEvent::MessageUpsert(msg));
                            }
                            tick_events.emit(BackendEvent::HistoryPageFetched {
                                chat_jid,
                                added,
                                end_of_history: end,
                            });
                        })
                        .ok();
                }
                BackendCommand::SetGroupPicture { chat_jid, image } => {
                    let is_group =
                        chat_state.get(&chat_jid).is_some_and(|c| c.kind == LiveChatKind::Group);
                    if !is_group {
                        events.emit(BackendEvent::GroupPictureFailed {
                            chat_jid,
                            message: "That group is not available in the demo.".to_owned(),
                        });
                    } else if image.as_ref().is_some_and(|bytes| bytes.is_empty()) {
                        events.emit(BackendEvent::GroupPictureFailed {
                            chat_jid,
                            message: "That picture file is empty.".to_owned(),
                        });
                    } else {
                        group_picture_state.insert(chat_jid.clone(), image);
                        events.emit(BackendEvent::GroupPictureChanged { chat_jid });
                    }
                }
                BackendCommand::LookupContact { phone } => match crate::phone_digits(&phone) {
                    None => events.emit(BackendEvent::ContactLookupFailed {
                        phone,
                        message: "That does not look like a phone number: \
                                      use the full international format, digits only."
                            .to_owned(),
                    }),
                    Some(digits) => match directory_person(&digits) {
                        Some((jid, name)) => {
                            let chat = chat_state.get(jid).cloned().unwrap_or_else(|| LiveChat {
                                jid: jid.to_owned(),
                                title: name.to_owned(),
                                kind: LiveChatKind::Dm,
                                preview: String::new(),
                                preview_sender: None,
                                timestamp: 0,
                                unread: 0,
                                archived: false,
                                locked: false,
                                pinned: false,
                                ephemeral_secs: 0,
                                muted: false,
                                mute_until: 0,
                                parent_jid: None,
                                is_parent: false,
                                from_history: false,
                            });
                            events.emit(BackendEvent::ContactResolved { chat, alt_jid: None });
                        }
                        None => events.emit(BackendEvent::ContactNotFound { phone }),
                    },
                },
                BackendCommand::ResolveTitles { jids } => {
                    if let Some(log) = &command_log {
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("resolve_titles:{}", jids.len()));
                    }
                    // The fake heals from its own scripted
                    // roster: a reported chat whose scripted
                    // title is a real name gets its rename.
                    for jid in jids {
                        if let Some(chat) = chat_state.get(&jid) {
                            let raw = chat.jid.split('@').next().unwrap_or("");
                            if !chat.title.is_empty() && chat.title != raw {
                                events.emit(BackendEvent::ChatRenamed {
                                    chat_jid: jid,
                                    title: chat.title.clone(),
                                    authoritative: false,
                                });
                            }
                        }
                    }
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

/// One scripted participant in a fake group or link call.
#[derive(Debug, Clone)]
struct FakeParticipant {
    jid: String,
    state: String,
    hand_raised: bool,
    sharing: bool,
}

/// The scripted group call's roster at ring time: the DM
/// contact plus the two scripted members, all ringing.
fn scripted_group_participants() -> Vec<FakeParticipant> {
    [FAKE_DM_JID, FAKE_MEMBER2_JID, FAKE_MEMBER3_JID]
        .iter()
        .map(|jid| FakeParticipant {
            jid: (*jid).to_owned(),
            state: "ringing".to_owned(),
            hand_raised: false,
            sharing: false,
        })
        .collect()
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
    /// A group or call-link call (its roster is scripted).
    group: bool,
    /// Joined through a call link.
    link: bool,
    /// The scripted roster (group and link calls only).
    participants: Vec<FakeParticipant>,
    /// Ticks (250 ms) spent Active; the group script's beats
    /// are scheduled against it.
    active_ticks: u64,
    /// Script beats already played (bit flags, see the BEAT_
    /// constants in the ticker).
    beats: u32,
    /// The waiting-room guest's JID while they wait (the
    /// waiting-room fixture and hosted link calls).
    waiting: Option<String>,
    /// The waiting room currently requires approval.
    approval: bool,
    /// An invited participant and the active-tick at which
    /// they connect.
    pending_invite: Option<(String, u64)>,
    /// The fixture video stream thread is running for this
    /// call.
    video_stream_started: bool,
}

impl FakeCall {
    /// A plain 1:1 call with no scripted roster.
    fn plain(call_id: String, chat_jid: String, outgoing: bool, video: bool) -> Self {
        Self {
            call_id,
            chat_jid,
            outgoing,
            video,
            phase: crate::CallPhase::Ringing,
            seconds: 0,
            ticks: 0,
            group: false,
            link: false,
            participants: Vec::new(),
            active_ticks: 0,
            beats: 0,
            waiting: None,
            approval: true,
            pending_invite: None,
            video_stream_started: false,
        }
    }
}

/// Emits the call's roster snapshot, the way the real backend
/// re-sends the whole roster on every change.
fn emit_fake_roster(events: &EventSink, call: &FakeCall) {
    events.emit(BackendEvent::CallParticipants {
        participants: call
            .participants
            .iter()
            .map(|p| crate::CallParticipant {
                jid: p.jid.clone(),
                state: p.state.clone(),
                hand_raised: p.hand_raised,
                sharing: p.sharing,
            })
            .collect(),
    });
}

/// The call-video fixture's access units, parsed once from
/// the checked-in stream the generator example produced:
/// records of [u32 LE length][u8 keyframe][AU bytes]. The
/// stream is a real H.264 encode of a test pattern, so the
/// app's real decoder is what renders it.
fn fixture_aus() -> &'static [(Vec<u8>, bool)] {
    static AUS: std::sync::OnceLock<Vec<(Vec<u8>, bool)>> = std::sync::OnceLock::new();
    AUS.get_or_init(|| {
        let bytes: &[u8] = include_bytes!("../testdata/call-video-fixture.h264");
        let mut out = Vec::new();
        let mut at = 0usize;
        while at + 5 <= bytes.len() {
            let len = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
                as usize;
            let keyframe = bytes[at + 4] != 0;
            let start = at + 5;
            let Some(au) = bytes.get(start..start + len) else { break };
            out.push((au.to_vec(), keyframe));
            at = start + len;
        }
        out
    })
}

/// Streams the fixture's access units into the app for one
/// video call, at the stream's own 15 fps, until the call
/// leaves Active or stops being a video call. In a group call
/// the frames are attributed to the participant currently
/// sharing, or to the first connected participant when nobody
/// shares: attribution is how the app knows whose picture it
/// is painting.
fn fake_video_stream(
    state: std::sync::Arc<std::sync::Mutex<FakeCallState>>,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: EventSink,
    call_id: String,
) {
    let aus = fixture_aus();
    if aus.is_empty() {
        return;
    }
    let mut index = 0usize;
    while alive.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(66));
        let sender = {
            let guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(call) = &guard.current else { return };
            if call.call_id != call_id || call.phase != crate::CallPhase::Active || !call.video {
                return;
            }
            if call.group {
                call.participants
                    .iter()
                    .find(|p| p.sharing)
                    .or_else(|| call.participants.iter().find(|p| p.state == "connected"))
                    .map(|p| p.jid.clone())
            } else {
                None
            }
        };
        let (data, keyframe) = &aus[index % aus.len()];
        index += 1;
        events.try_emit(BackendEvent::CallVideoAu {
            sender,
            data: data.clone(),
            keyframe: *keyframe,
            orientation: 0,
        });
    }
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

/// Script beats for group calls, as bit flags on
/// [`FakeCall::beats`]: each fires once, at its scheduled
/// active-tick.
const BEAT_VIDEO_STATE: u32 = 1 << 0;
const BEAT_MEMBER2: u32 = 1 << 1;
const BEAT_MEMBER3: u32 = 1 << 2;
const BEAT_HAND: u32 = 1 << 3;
const BEAT_REACT: u32 = 1 << 4;
const BEAT_SHARE: u32 = 1 << 5;
const BEAT_WAITING: u32 = 1 << 6;
const BEAT_LINK_GUEST: u32 = 1 << 7;

/// Walks the fake call phase machine: outgoing calls ring for
/// about a second, connect, go active and tick the duration once
/// a second; accepted incoming calls connect the same way. While
/// a call is Active the scripted beats play: a video call
/// reports its video state and starts the fixture stream; a
/// group call connects its members in sequence, then one
/// raises a hand, one reacts, and one starts sharing; a link
/// call connects its second guest; and under the waiting-room
/// fixture a guest arrives to be admitted or denied.
fn fake_call_ticker(
    state: std::sync::Arc<std::sync::Mutex<FakeCallState>>,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: EventSink,
) {
    while alive.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let mut emits: Vec<BackendEvent> = Vec::new();
        let mut start_stream: Option<String> = None;
        {
            let mut guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(call) = &mut guard.current {
                call.ticks += 1;
                match call.phase {
                    crate::CallPhase::Ringing if call.outgoing && call.ticks >= 4 => {
                        call.phase = crate::CallPhase::Connecting;
                        call.ticks = 0;
                        emits.push(BackendEvent::CallChanged {
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
                        emits.push(BackendEvent::CallChanged {
                            call_id: call.call_id.clone(),
                            chat_jid: call.chat_jid.clone(),
                            phase: crate::CallPhase::Active,
                            seconds: 0,
                            reason: None,
                        });
                    }
                    crate::CallPhase::Active => {
                        call.active_ticks += 1;
                        if call.ticks % 4 == 0 {
                            call.seconds += 1;
                            emits.push(BackendEvent::CallChanged {
                                call_id: call.call_id.clone(),
                                chat_jid: call.chat_jid.clone(),
                                phase: crate::CallPhase::Active,
                                seconds: call.seconds,
                                reason: None,
                            });
                        }
                        fake_call_beats(call, &mut emits, &mut start_stream);
                    }
                    _ => {}
                }
            }
        }
        for event in emits {
            events.emit(event);
        }
        if let Some(call_id) = start_stream {
            let stream_state = state.clone();
            let stream_alive = alive.clone();
            let stream_events = events.clone();
            std::thread::Builder::new()
                .name("zaprust-fake-video".to_owned())
                .spawn(move || {
                    fake_video_stream(stream_state, stream_alive, stream_events, call_id);
                })
                .ok();
        }
    }
}

/// Plays one tick of the Active-phase script for `call`,
/// appending the events it produces. Roster changes are
/// emitted after this returns by the caller-visible helper
/// pattern: beats that change the roster set `roster_dirty`,
/// handled here by emitting the snapshot directly into
/// `emits` (the events are ordered, so a snapshot computed
/// now is the state at this tick).
fn fake_call_beats(
    call: &mut FakeCall,
    emits: &mut Vec<BackendEvent>,
    start_stream: &mut Option<String>,
) {
    let ticks = call.active_ticks;
    let mut roster_dirty = false;
    if call.video && call.beats & BEAT_VIDEO_STATE == 0 {
        call.beats |= BEAT_VIDEO_STATE;
        emits.push(BackendEvent::CallVideoState {
            local_on: true,
            remote_on: true,
            local_note: None,
        });
    }
    if call.video && !call.video_stream_started {
        call.video_stream_started = true;
        *start_stream = Some(call.call_id.clone());
    }
    if call.group && !call.link {
        // Members connect in sequence, then the control
        // script: a raised hand, a reaction, a screen share.
        if ticks >= 8 && call.beats & BEAT_MEMBER2 == 0 {
            call.beats |= BEAT_MEMBER2;
            set_participant_state(call, FAKE_MEMBER2_JID, "connected");
            roster_dirty = true;
        }
        if ticks >= 16 && call.beats & BEAT_MEMBER3 == 0 {
            call.beats |= BEAT_MEMBER3;
            set_participant_state(call, FAKE_MEMBER3_JID, "connected");
            roster_dirty = true;
        }
        if ticks == 1 {
            set_participant_state(call, FAKE_DM_JID, "connected");
            roster_dirty = true;
        }
        if ticks >= 24 && call.beats & BEAT_HAND == 0 {
            call.beats |= BEAT_HAND;
            if let Some(p) = call.participants.iter_mut().find(|p| p.jid == FAKE_MEMBER2_JID) {
                p.hand_raised = true;
            }
            roster_dirty = true;
        }
        if ticks >= 32 && call.beats & BEAT_REACT == 0 {
            call.beats |= BEAT_REACT;
            emits.push(BackendEvent::CallReaction {
                jid: FAKE_MEMBER2_JID.to_owned(),
                emoji: Some("👍".to_owned()),
            });
        }
        if ticks >= 40 && call.beats & BEAT_SHARE == 0 {
            call.beats |= BEAT_SHARE;
            if let Some(p) = call.participants.iter_mut().find(|p| p.jid == FAKE_MEMBER3_JID) {
                p.sharing = true;
            }
            roster_dirty = true;
        }
    }
    if call.link && ticks >= 8 && call.beats & BEAT_LINK_GUEST == 0 {
        call.beats |= BEAT_LINK_GUEST;
        set_participant_state(call, FAKE_MEMBER2_JID, "connected");
        roster_dirty = true;
    }
    if let Some((jid, at)) = &call.pending_invite {
        if ticks >= *at {
            let jid = jid.clone();
            call.pending_invite = None;
            set_participant_state(call, &jid, "connected");
            roster_dirty = true;
        }
    }
    if waiting_fixture_enabled()
        && ticks >= 20
        && call.waiting.is_none()
        && call.beats & BEAT_WAITING == 0
    {
        call.beats |= BEAT_WAITING;
        call.waiting = Some(FAKE_GUEST_JID.to_owned());
        emits.push(BackendEvent::CallWaitingRoom {
            is_admin: true,
            approval_required: call.approval,
            users: vec![crate::CallWaitingUser { jid: FAKE_GUEST_JID.to_owned(), phone: None }],
        });
    }
    if roster_dirty {
        emits.push(BackendEvent::CallParticipants {
            participants: call
                .participants
                .iter()
                .map(|p| crate::CallParticipant {
                    jid: p.jid.clone(),
                    state: p.state.clone(),
                    hand_raised: p.hand_raised,
                    sharing: p.sharing,
                })
                .collect(),
        });
    }
}

/// Sets one scripted participant's roster state word.
fn set_participant_state(call: &mut FakeCall, jid: &str, state: &str) {
    if let Some(p) = call.participants.iter_mut().find(|p| p.jid == jid) {
        p.state = state.to_owned();
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
    if call.chat_jid.is_empty() {
        // A call-link call belongs to no chat: there is no
        // transcript to leave its line in.
        return;
    }
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
    // The categories the Privacy page has no row for are part
    // of the account state too: the real backend's `privacy_map`
    // carries them, so the fake's baseline does as well.
    privacy.insert("calladd".to_owned(), "known".to_owned());
    privacy.insert("messages".to_owned(), "contacts".to_owned());
    privacy.insert("defense".to_owned(), "off".to_owned());
    privacy
}

/// Merges one own-business-profile delta into the stored
/// profile, with the contract's semantics: `None` leaves a
/// field untouched, `Some(None)` clears a text field, and
/// the hours replace wholesale.
fn merge_business_update(
    profile: &mut crate::LiveBusinessProfile,
    update: &crate::LiveBusinessProfileUpdate,
) {
    if let Some(description) = &update.description {
        profile.description = description.clone().unwrap_or_default();
    }
    if let Some(email) = &update.email {
        profile.email = email.clone();
    }
    if let Some(address) = &update.address {
        profile.address = address.clone();
    }
    if let Some(websites) = &update.websites {
        profile.websites = websites.clone();
    }
    if let Some(categories) = &update.categories {
        profile.categories = categories
            .iter()
            .map(|id| {
                let name = profile
                    .categories
                    .iter()
                    .find(|c| &c.id == id)
                    .map_or_else(|| id.clone(), |c| c.name.clone());
                crate::LiveBusinessCategory { id: id.clone(), name }
            })
            .collect();
    }
    if let Some(hours) = &update.hours {
        profile.hours_timezone = hours.timezone.clone();
        profile.hours = hours.days.clone();
    }
}

/// The fake account's own profile at link time. The run loop
/// keeps the live copy: UpdateOwnProfile mutates the given
/// fields and the full profile is re-emitted, mirroring the
/// real backend's re-read after a change.
fn baseline_own_profile() -> OwnProfile {
    OwnProfile {
        name: "Fake User".to_owned(),
        // The fake publishes its own addresses like the real
        // backend does, so the app's exact-address self chat
        // matching runs in test mode too.
        jid: FAKE_OWN_JID.to_owned(),
        lid: FAKE_OWN_LID.to_owned(),
        about: "Available (test account)".to_owned(),
        photo: None,
    }
}

fn emit_linked_script(
    events: &EventSink,
    call_state: &std::sync::Arc<std::sync::Mutex<FakeCallState>>,
    privacy: &std::collections::BTreeMap<String, String>,
    own_profile: &OwnProfile,
) {
    events.emit(BackendEvent::Connected { account: "Fake account (not WhatsApp)".to_owned() });
    // The fake has no catch-up to run: its scripted roster IS the
    // synced state, so the sync completes immediately, the same
    // contract the real backend's sync events give the app.
    events.emit(BackendEvent::SyncCompleted);
    for chat in scripted_chats() {
        events.emit(BackendEvent::ChatUpsert(chat));
    }
    for msg in scripted_messages() {
        events.emit(BackendEvent::MessageUpsert(msg));
    }
    for msg in scripted_group_messages() {
        events.emit(BackendEvent::MessageUpsert(msg));
    }
    if marathon_enabled() {
        for msg in scripted_marathon_messages() {
            events.emit(BackendEvent::MessageUpsert(msg));
        }
    }
    if lock_fixture_enabled() {
        for msg in scripted_lock_fixture_messages() {
            events.emit(BackendEvent::MessageUpsert(msg));
        }
    }
    if bidi_fixture_enabled() {
        for msg in scripted_bidi_fixture_messages() {
            events.emit(BackendEvent::MessageUpsert(msg));
        }
    }
    if stress_fixture_enabled() {
        for msg in scripted_stress_messages() {
            events.emit(BackendEvent::MessageUpsert(msg));
        }
        for msg in scripted_stress_group_messages() {
            events.emit(BackendEvent::MessageUpsert(msg));
        }
    }
    if business_fixture_enabled() {
        for msg in scripted_business_messages() {
            events.emit(BackendEvent::MessageUpsert(msg));
        }
        // The account's synced labels and quick replies, as
        // the app-state sync delivers them at connect: the
        // definitions first, then the associations.
        events.emit(BackendEvent::LabelEdited {
            label_id: FAKE_LABEL_URGENT_ID.to_owned(),
            name: Some("Urgent".to_owned()),
            color: Some(5),
            predefined_id: None,
            deleted: false,
        });
        events.emit(BackendEvent::LabelEdited {
            label_id: FAKE_LABEL_VIP_ID.to_owned(),
            name: Some("VIP".to_owned()),
            color: Some(12),
            predefined_id: None,
            deleted: false,
        });
        events.emit(BackendEvent::ChatLabelChanged {
            label_id: FAKE_LABEL_VIP_ID.to_owned(),
            chat_jid: FAKE_BIZ_JID.to_owned(),
            labeled: true,
        });
        events.emit(BackendEvent::ChatLabelChanged {
            label_id: FAKE_LABEL_URGENT_ID.to_owned(),
            chat_jid: FAKE_DM_JID.to_owned(),
            labeled: true,
        });
        events.emit(BackendEvent::MessageLabelChanged {
            label_id: FAKE_LABEL_URGENT_ID.to_owned(),
            chat_jid: FAKE_BIZ_JID.to_owned(),
            message_id: "fake-order-msg-1".to_owned(),
            labeled: true,
        });
        events.emit(BackendEvent::QuickReplyEdited {
            id: "fake-qr-hours".to_owned(),
            shortcut: "hours".to_owned(),
            message: "We are open Monday to Saturday, 8:00 to 17:00.".to_owned(),
            keywords: vec!["open".to_owned(), "opening".to_owned()],
            count: 4,
            deleted: false,
        });
        events.emit(BackendEvent::QuickReplyEdited {
            id: "fake-qr-ship".to_owned(),
            shortcut: "ship".to_owned(),
            message: "Orders ship the next morning by courier.".to_owned(),
            keywords: vec!["delivery".to_owned(), "shipping".to_owned()],
            count: 2,
            deleted: false,
        });
    }
    // The scripted statuses, as the real backend delivers them:
    // live arrivals and history redelivery both surface as
    // StatusReceived, so the linked script (which stands in for
    // a fresh link's history sync) delivers the feed the same
    // way.
    for status in scripted_statuses() {
        events.emit(BackendEvent::StatusReceived { status: Box::new(status) });
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
    // The account's saved contacts, as the real backend
    // reports them from its identity book when the book first
    // fills at link time.
    events.emit(BackendEvent::SavedContacts { contacts: scripted_saved_contacts() });
    // The account's own profile, as the real backend reports it
    // after reading the profile back at connect.
    events.emit(BackendEvent::OwnProfile(own_profile.clone()));
    // Baseline blocklist, as the real backend reports it after
    // fetching the account's list at connect: nobody blocked,
    // or the lock fixture's promo sender under its gate.
    events.emit(BackendEvent::Blocklist(if lock_fixture_enabled() {
        vec![FAKE_PROMO_JID.to_owned()]
    } else {
        Vec::new()
    }));
    // One scripted incoming voice call from the DM contact, so
    // the whole call surface (ring, accept, talk, end, log entry)
    // is exercised in test mode without a phone. Registered in
    // the call state first so AcceptCall finds it. The call
    // fixture gate (ZAPRUST_FAKE_CALL) swaps in a video call or
    // a group invitation for the call captures.
    {
        let mut state = call_state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.current.is_none() {
            state.counter += 1;
            let call_id = format!("fake-call-{}", state.counter);
            let (mut call, group_jid) = match call_fixture() {
                Some(kind @ ("group" | "group-video")) => {
                    let mut call = FakeCall::plain(
                        call_id.clone(),
                        FAKE_GROUP_JID.to_owned(),
                        false,
                        kind == "group-video",
                    );
                    call.group = true;
                    call.participants = scripted_group_participants();
                    (call, Some(FAKE_GROUP_JID.to_owned()))
                }
                Some("video") => {
                    (FakeCall::plain(call_id.clone(), FAKE_DM_JID.to_owned(), false, true), None)
                }
                _ => (FakeCall::plain(call_id.clone(), FAKE_DM_JID.to_owned(), false, false), None),
            };
            call.phase = crate::CallPhase::Ringing;
            let (chat_jid, video) = (call.chat_jid.clone(), call.video);
            state.current = Some(call);
            events.emit(BackendEvent::IncomingCall { call_id, chat_jid, video, group_jid });
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

    /// The generated fixture is a real PNG (the app's decoder
    /// must accept what the fake serves as avatar and sticker
    /// bytes): signature, IHDR dimensions, and per-seed variety.
    #[test]
    fn fixture_png_is_a_valid_deterministic_png() {
        let bytes = fixture_png("628123@s.whatsapp.net");
        assert_eq!(&bytes[..8], &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        // IHDR width and height sit at fixed offsets in the file.
        assert_eq!(u32::from_be_bytes(bytes[16..20].try_into().unwrap()), 96);
        assert_eq!(u32::from_be_bytes(bytes[20..24].try_into().unwrap()), 96);
        assert_eq!(bytes, fixture_png("628123@s.whatsapp.net"), "same seed, same picture");
        assert_ne!(bytes, fixture_png("628999@s.whatsapp.net"), "another seed, another hue");
    }

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

    /// The fake's own profile reports the account's published
    /// own addresses, mirroring the real backend: the app's
    /// self chat matching keys on them, so a fake that left
    /// them empty would silently exercise only the name
    /// fallback the real path no longer needs.
    #[test]
    fn baseline_own_profile_reports_the_fake_own_addresses() {
        let profile = baseline_own_profile();
        assert_eq!(profile.jid, FAKE_OWN_JID);
        assert_eq!(profile.lid, FAKE_OWN_LID);
    }

    /// The played receipt command is part of the contract: the
    /// fake records it in the command log like MarkAsRead, so
    /// app tests can prove a play gesture dispatched it.
    #[test]
    fn mark_as_played_is_logged_by_the_fake() {
        let log: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ch = start(FakeBackend::auto_with_log(log.clone()));
        ch.commands.send(BackendCommand::ConnectQr).unwrap();
        for _ in 0..12 {
            match ch.events.recv_timeout(Duration::from_secs(5)) {
                Ok(BackendEvent::Connected { .. }) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        ch.commands
            .send(BackendCommand::MarkAsPlayed {
                chat_jid: FAKE_DM_JID.to_owned(),
                sender_jid: None,
                message_ids: vec!["v1".to_owned(), "v2".to_owned()],
            })
            .unwrap();
        ch.commands.send(BackendCommand::Shutdown).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let lines = log.lock().unwrap().join("\n");
        assert!(lines.contains(&format!("mark_as_played:{FAKE_DM_JID}:2")), "log: {lines}");
    }

    #[test]
    fn qr_flow_emits_payload_then_linked_script() {
        let ch = start(FakeBackend::auto());
        ch.commands.send(BackendCommand::ConnectQr).unwrap();
        assert_eq!(
            next(&ch),
            BackendEvent::QrCode {
                payload: FAKE_QR_PAYLOAD.to_owned(),
                valid_for: Duration::from_secs(3600),
            }
        );
        assert!(matches!(next(&ch), BackendEvent::Connected { .. }));
        for _ in 0..6 {
            let _ = next(&ch);
        }
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    /// Rows 64, 65 and 19: the fake scripts the whole new
    /// group and interactive surface: add-with-history and
    /// its retry, member labels, reports to admins, and
    /// interactive sends and answers.
    #[test]
    fn group_long_tail_surface_is_scripted() {
        let ch = start(FakeBackend::auto());
        let record = |id: &str, body: &str| crate::HistoryMessageRecord {
            id: id.to_owned(),
            author_jid: Some(FAKE_MEMBER2_JID.to_owned()),
            from_me: false,
            timestamp: 1_700_000_100,
            body: body.to_owned(),
            mentions: Vec::new(),
            delivery: None,
        };
        // Add with history: the share reports what it carried.
        ch.commands
            .send(BackendCommand::AddGroupParticipantsWithHistory {
                chat_jid: FAKE_GROUP_JID.to_owned(),
                participants: vec!["10000000020@s.whatsapp.net".to_owned()],
                history: vec![record("h1", "first"), record("h2", "second")],
            })
            .unwrap();
        let events =
            collect_until(&ch, &|e| matches!(e, BackendEvent::GroupHistoryReported { .. }));
        let reported = events
            .iter()
            .find_map(|e| match e {
                BackendEvent::GroupHistoryReported { added, report, .. } => {
                    Some((added.clone(), report.clone()))
                }
                _ => None,
            })
            .expect("the add outcome is reported");
        assert_eq!(reported.0, vec!["10000000020@s.whatsapp.net".to_owned()]);
        assert!(
            matches!(reported.1, crate::HistoryShareReport::Shared { messages: 2 }),
            "two records shared: {:?}",
            reported.1
        );
        // No records: the share is honestly skipped.
        ch.commands
            .send(BackendCommand::AddGroupParticipantsWithHistory {
                chat_jid: FAKE_GROUP_JID.to_owned(),
                participants: vec!["10000000021@s.whatsapp.net".to_owned()],
                history: Vec::new(),
            })
            .unwrap();
        let events =
            collect_until(&ch, &|e| matches!(e, BackendEvent::GroupHistoryReported { .. }));
        assert!(
            events.iter().any(|e| matches!(
                e,
                BackendEvent::GroupHistoryReported {
                    report: crate::HistoryShareReport::Skipped { .. },
                    ..
                }
            )),
            "an empty history skips: {events:?}"
        );
        // A flaky add fails the share retryably; the retry
        // then shares it.
        ch.commands
            .send(BackendCommand::AddGroupParticipantsWithHistory {
                chat_jid: FAKE_GROUP_JID.to_owned(),
                participants: vec!["flaky1@s.whatsapp.net".to_owned()],
                history: vec![record("h3", "third")],
            })
            .unwrap();
        let events =
            collect_until(&ch, &|e| matches!(e, BackendEvent::GroupHistoryReported { .. }));
        assert!(
            events.iter().any(|e| matches!(
                e,
                BackendEvent::GroupHistoryReported {
                    report: crate::HistoryShareReport::Failed { retryable: true, .. },
                    ..
                }
            )),
            "the flaky share fails retryably: {events:?}"
        );
        ch.commands
            .send(BackendCommand::RetryGroupHistoryShare { chat_jid: FAKE_GROUP_JID.to_owned() })
            .unwrap();
        let events =
            collect_until(&ch, &|e| matches!(e, BackendEvent::GroupHistoryReported { .. }));
        assert!(
            events.iter().any(|e| matches!(
                e,
                BackendEvent::GroupHistoryReported {
                    report: crate::HistoryShareReport::Shared { messages: 1 },
                    ..
                }
            )),
            "the retry shares: {events:?}"
        );
        // Member labels: the set answers, and the member list
        // that follows carries the scripted labels.
        ch.commands
            .send(BackendCommand::SetOwnMemberLabel {
                chat_jid: FAKE_GROUP_JID.to_owned(),
                label: "Coordinator".to_owned(),
            })
            .unwrap();
        let events = collect_until(&ch, &|e| matches!(e, BackendEvent::MemberLabelSet { .. }));
        assert!(
            events.iter().any(|e| matches!(
                e,
                BackendEvent::MemberLabelSet { label, .. } if label == "Coordinator"
            )),
            "the label set is answered: {events:?}"
        );
        ch.commands
            .send(BackendCommand::FetchGroupMembers { chat_jid: FAKE_GROUP_JID.to_owned() })
            .unwrap();
        let events = collect_until(&ch, &|e| matches!(e, BackendEvent::GroupMembers { .. }));
        let members_event = events
            .iter()
            .find(|e| matches!(e, BackendEvent::GroupMembers { .. }))
            .expect("the member list answers");
        match members_event {
            BackendEvent::GroupMembers { members, own_label, .. } => {
                assert_eq!(own_label.as_deref(), Some("Coordinator"));
                assert!(
                    members.iter().any(|(jid, _, _, label)| {
                        jid == FAKE_MEMBER3_JID && label.as_deref() == Some("Design")
                    }),
                    "Emma's scripted label rides the list: {members:?}"
                );
            }
            _ => unreachable!(),
        }
        // Reports: reporting answers, and the admin fetch
        // lists the seeded report plus the new one.
        ch.commands
            .send(BackendCommand::ReportMessageToAdmins {
                chat_jid: FAKE_GROUP_JID.to_owned(),
                message_id: "fake-loc-1".to_owned(),
            })
            .unwrap();
        let events =
            collect_until(&ch, &|e| matches!(e, BackendEvent::MessageReportedToAdmins { .. }));
        assert!(
            events.iter().any(|e| matches!(e, BackendEvent::MessageReportedToAdmins { .. })),
            "the report is answered: {events:?}"
        );
        ch.commands
            .send(BackendCommand::FetchReportedMessages { chat_jid: FAKE_GROUP_JID.to_owned() })
            .unwrap();
        let events = collect_until(&ch, &|e| matches!(e, BackendEvent::ReportedMessages { .. }));
        let reports = events
            .iter()
            .find_map(|e| match e {
                BackendEvent::ReportedMessages { reports, .. } => Some(reports.clone()),
                _ => None,
            })
            .expect("the reported list answers");
        assert_eq!(reports.len(), 2, "seeded plus new: {reports:?}");
        assert!(reports.iter().any(|r| r.message_id == "fake-carousel-1"));
        assert!(reports.iter().any(|r| r.message_id == "fake-loc-1"));
        // Interactive sends echo with their kind; an answer
        // echoes as its chosen text.
        ch.commands
            .send(BackendCommand::SendInteractive {
                chat_jid: FAKE_GROUP_JID.to_owned(),
                send_id: None,
                spec: crate::InteractiveSpec::Buttons {
                    text: "Lunch?".to_owned(),
                    footer: String::new(),
                    buttons: vec!["Yes".to_owned(), "No".to_owned()],
                },
            })
            .unwrap();
        let events = collect_until(&ch, &|e| matches!(e, BackendEvent::MessageUpsert(_)));
        assert!(
            events.iter().any(|e| matches!(
                e,
                BackendEvent::MessageUpsert(m)
                    if matches!(m.kind, crate::LiveMessageKind::Buttons { .. }) && m.body == "Lunch?"
            )),
            "the buttons echo carries its kind: {events:?}"
        );
        ch.commands
            .send(BackendCommand::SendInteractiveResponse {
                chat_jid: FAKE_GROUP_JID.to_owned(),
                send_id: None,
                message_id: "fake-buttons-1".to_owned(),
                sender_jid: Some(FAKE_DM_JID.to_owned()),
                from_me: false,
                is_list: false,
                choice_id: "1".to_owned(),
                choice_text: "Yes".to_owned(),
            })
            .unwrap();
        let events = collect_until(&ch, &|e| matches!(e, BackendEvent::MessageUpsert(_)));
        assert!(
            events.iter().any(|e| matches!(
                e,
                BackendEvent::MessageUpsert(m) if m.body == "Yes" && m.from_me
            )),
            "the answer echoes as its text: {events:?}"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    /// Failure injection for the two arms the v1.6.23
    /// verification found missing it (its D2): StartGroupCall
    /// answers an injected failure with the same generic error
    /// the live backend uses, starts no call, and succeeds on
    /// the next attempt.
    #[test]
    fn injected_start_group_call_failure_then_success() {
        let fake = FakeBackend { fail_next: vec!["StartGroupCall"], ..FakeBackend::auto() };
        let ch = start(fake);
        linked(&ch);
        // The linked script leaves its scripted incoming call
        // ringing; decline it so no call is in progress.
        ch.commands.send(BackendCommand::RejectCall).unwrap();
        collect_until(&ch, &|e| {
            matches!(e, BackendEvent::CallChanged { phase: crate::CallPhase::Ended, .. })
        });
        ch.commands
            .send(BackendCommand::StartGroupCall {
                chat_jid: FAKE_GROUP_JID.to_owned(),
                video: false,
            })
            .unwrap();
        // The declined call's log entry may land first; collect
        // until the injected failure itself arrives.
        let events = collect_until(&ch, &|e| matches!(e, BackendEvent::Error { .. }));
        assert!(
            events.iter().any(|e| matches!(
                e,
                BackendEvent::Error { message }
                    if message == "Starting the group call failed: connection lost"
            )),
            "expected the injected failure: {events:?}"
        );
        ch.commands
            .send(BackendCommand::StartGroupCall {
                chat_jid: FAKE_GROUP_JID.to_owned(),
                video: false,
            })
            .unwrap();
        let events = collect_until(&ch, &|e| {
            matches!(e, BackendEvent::CallChanged { phase: crate::CallPhase::Ringing, .. })
        });
        assert!(
            events.iter().any(|e| matches!(
                e,
                BackendEvent::CallChanged { phase: crate::CallPhase::Ringing, .. }
            )),
            "the second attempt starts the call: {events:?}"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    /// The second D2 arm: an injected RetryGroupHistoryShare
    /// failure answers the retryable failure and keeps the
    /// prepared share, so the next retry still lands it.
    #[test]
    fn injected_history_retry_failure_keeps_the_share() {
        let fake = FakeBackend { fail_next: vec!["RetryGroupHistoryShare"], ..FakeBackend::auto() };
        let ch = start(fake);
        linked(&ch);
        // A flaky add fails the share retryably and prepares
        // the retry.
        ch.commands
            .send(BackendCommand::AddGroupParticipantsWithHistory {
                chat_jid: FAKE_GROUP_JID.to_owned(),
                participants: vec!["flaky1@s.whatsapp.net".to_owned()],
                history: vec![crate::HistoryMessageRecord {
                    id: "h1".to_owned(),
                    author_jid: Some(FAKE_MEMBER2_JID.to_owned()),
                    from_me: false,
                    timestamp: 1_700_000_100,
                    body: "first".to_owned(),
                    mentions: Vec::new(),
                    delivery: None,
                }],
            })
            .unwrap();
        let events =
            collect_until(&ch, &|e| matches!(e, BackendEvent::GroupHistoryReported { .. }));
        assert!(
            events.iter().any(|e| matches!(
                e,
                BackendEvent::GroupHistoryReported {
                    report: crate::HistoryShareReport::Failed { retryable: true, .. },
                    ..
                }
            )),
            "the flaky share fails retryably: {events:?}"
        );
        // The injected retry failure: same retryable shape,
        // and the prepared share survives it.
        ch.commands
            .send(BackendCommand::RetryGroupHistoryShare { chat_jid: FAKE_GROUP_JID.to_owned() })
            .unwrap();
        let events =
            collect_until(&ch, &|e| matches!(e, BackendEvent::GroupHistoryReported { .. }));
        assert!(
            events.iter().any(|e| matches!(
                e,
                BackendEvent::GroupHistoryReported {
                    report: crate::HistoryShareReport::Failed { error, retryable: true },
                    ..
                } if error == "sharing the history failed: connection lost"
            )),
            "the injected retry failure answers retryably: {events:?}"
        );
        // The next retry lands the share: the injected failure
        // consumed nothing.
        ch.commands
            .send(BackendCommand::RetryGroupHistoryShare { chat_jid: FAKE_GROUP_JID.to_owned() })
            .unwrap();
        let events =
            collect_until(&ch, &|e| matches!(e, BackendEvent::GroupHistoryReported { .. }));
        assert!(
            events.iter().any(|e| matches!(
                e,
                BackendEvent::GroupHistoryReported {
                    report: crate::HistoryShareReport::Shared { messages: 1 },
                    ..
                }
            )),
            "the kept share lands on the next retry: {events:?}"
        );
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
            has(&|e| matches!(e, BackendEvent::PollVote { .. })),
            "poll votes arrive as vote events"
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
                send_id: None,
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

    #[test]
    fn post_text_status_is_recorded_and_answers_posted() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ch = start(FakeBackend::auto_with_log(log.clone()));
        ch.commands
            .send(BackendCommand::PostTextStatus {
                text: "Out for a walk".to_owned(),
                recipients: vec![FAKE_DM_JID.to_owned()],
                privacy: "contacts".to_owned(),
            })
            .unwrap();
        match next(&ch) {
            BackendEvent::StatusPosted { text, message_id } => {
                assert_eq!(text, "Out for a walk");
                assert_eq!(message_id, "fake-status-post-1");
            }
            other => panic!("expected StatusPosted, got {other:?}"),
        }
        // The post is delivered back under the broadcast, like a
        // real one.
        match next(&ch) {
            BackendEvent::StatusReceived { status } => {
                assert!(status.from_me);
                assert_eq!(status.body, "Out for a walk");
            }
            other => panic!("expected the StatusReceived echo, got {other:?}"),
        }
        let recorded = log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        assert_eq!(recorded, ["post_text_status:Out for a walk:1:contacts"]);
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    /// Connects the fake and drains the linked script, so a
    /// test starts from a quiet channel.
    fn linked(ch: &BackendChannels) {
        ch.commands.send(BackendCommand::ConnectQr).unwrap();
        // The script's last word is the scripted incoming call;
        // drain until it has been seen, then until the channel
        // is quiet.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut saw_call = false;
        while std::time::Instant::now() < deadline {
            match ch.events.recv_timeout(Duration::from_millis(300)) {
                Ok(BackendEvent::IncomingCall { .. }) => {
                    saw_call = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(saw_call, "the linked script ends with its scripted call");
        while ch.events.recv_timeout(Duration::from_millis(100)).is_ok() {}
    }

    /// Failure injection: a kind named in `fail_next` fails
    /// once with its typed failure event and changes no state,
    /// so the same command succeeds on the next attempt. This
    /// is what lets the app prove its rollbacks.
    #[test]
    fn injected_clear_chat_failure_leaves_the_chat_intact() {
        let mut fake = FakeBackend::auto();
        fake.fail_next = vec!["ClearChat"];
        let ch = start(fake);
        linked(&ch);
        ch.commands
            .send(BackendCommand::ClearChat {
                chat_jid: FAKE_DM_JID.to_owned(),
                keep_starred: false,
            })
            .unwrap();
        let failed = collect_until(&ch, &|e| matches!(e, BackendEvent::ClearChatFailed { .. }));
        assert!(
            matches!(&failed[failed.len() - 1], BackendEvent::ClearChatFailed { chat_jid, .. } if chat_jid == FAKE_DM_JID),
            "the injected failure answers with the typed event: {failed:?}"
        );
        ch.commands
            .send(BackendCommand::ClearChat {
                chat_jid: FAKE_DM_JID.to_owned(),
                keep_starred: false,
            })
            .unwrap();
        let cleared = collect_until(&ch, &|e| matches!(e, BackendEvent::ChatCleared { .. }));
        assert!(
            matches!(&cleared[cleared.len() - 1], BackendEvent::ChatCleared { chat_jid, .. } if chat_jid == FAKE_DM_JID),
            "the injection is one-shot and the chat survived it: {cleared:?}"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    /// Failure injection for a poll vote: the failed vote emits
    /// no PollVote, and the retry lands.
    #[test]
    fn injected_vote_failure_then_success() {
        let mut fake = FakeBackend::auto();
        fake.fail_next = vec!["VotePoll"];
        let ch = start(fake);
        linked(&ch);
        let vote = BackendCommand::VotePoll {
            chat_jid: FAKE_DM_JID.to_owned(),
            message_id: "fake-poll-1".to_owned(),
            options: vec![1],
        };
        ch.commands.send(vote.clone()).unwrap();
        let failed = collect_until(&ch, &|e| matches!(e, BackendEvent::PollVoteFailed { .. }));
        assert!(
            failed.iter().all(|e| !matches!(e, BackendEvent::PollVote { .. })),
            "a failed vote emits no vote: {failed:?}"
        );
        ch.commands.send(vote).unwrap();
        let voted = collect_until(&ch, &|e| matches!(e, BackendEvent::PollVote { .. }));
        assert!(
            matches!(&voted[voted.len() - 1], BackendEvent::PollVote { selected, .. } if selected == &vec![1]),
            "the retry lands: {voted:?}"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    /// P7: an event round trip on the fake: create lands as a
    /// message, my RSVP echoes back as an event response, an
    /// edit (here a cancellation) lands as an edit event.
    #[test]
    fn event_create_respond_edit_round_trip() {
        let ch = start(FakeBackend::auto());
        linked(&ch);
        ch.commands
            .send(BackendCommand::CreateEvent {
                chat_jid: FAKE_DM_JID.to_owned(),
                name: "Team lunch".to_owned(),
                description: String::new(),
                start_time: 1_700_086_400,
                end_time: 1_700_097_200,
                location: "Canteen".to_owned(),
                join_link: String::new(),
                scheduled_call: false,
                extra_guests: true,
            })
            .unwrap();
        let created = collect_until(&ch, &|e| {
            matches!(e, BackendEvent::MessageUpsert(msg)
                if matches!(&msg.kind, crate::LiveMessageKind::Event { name, .. } if name == "Team lunch"))
        });
        assert!(
            matches!(&created[created.len() - 1], BackendEvent::MessageUpsert(msg)
                if msg.event_secret.is_some() && msg.event_creator.is_some()),
            "the created event carries its secret and creator: {created:?}"
        );
        ch.commands
            .send(BackendCommand::RespondEvent {
                chat_jid: FAKE_DM_JID.to_owned(),
                message_id: "fake-event-1".to_owned(),
                response: crate::EventRsvp::Going,
                extra_guest_count: None,
            })
            .unwrap();
        let answered = collect_until(&ch, &|e| matches!(e, BackendEvent::EventResponse { .. }));
        assert!(
            matches!(
                &answered[answered.len() - 1],
                BackendEvent::EventResponse {
                    from_me: true,
                    response: crate::EventRsvp::Going,
                    ..
                }
            ),
            "my answer echoes back: {answered:?}"
        );
        ch.commands
            .send(BackendCommand::EditEvent {
                chat_jid: FAKE_DM_JID.to_owned(),
                message_id: "fake-event-mine-1".to_owned(),
                name: "Release retrospective".to_owned(),
                description: String::new(),
                start_time: 1_700_200_000,
                end_time: 0,
                location: String::new(),
                join_link: "https://meet.example.com/retro".to_owned(),
                scheduled_call: true,
                extra_guests: false,
                canceled: true,
            })
            .unwrap();
        let edited = collect_until(&ch, &|e| matches!(e, BackendEvent::EventEdited { .. }));
        assert!(
            matches!(&edited[edited.len() - 1], BackendEvent::EventEdited { canceled: true, .. }),
            "the cancellation lands as an edit: {edited:?}"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    /// Failure injection for the event commands: each failure
    /// is the typed event and no success event leaks.
    #[test]
    fn injected_event_failures_are_typed() {
        let mut fake = FakeBackend::auto();
        fake.fail_next = vec!["RespondEvent", "EditEvent"];
        let ch = start(fake);
        linked(&ch);
        ch.commands
            .send(BackendCommand::RespondEvent {
                chat_jid: FAKE_DM_JID.to_owned(),
                message_id: "fake-event-1".to_owned(),
                response: crate::EventRsvp::Maybe,
                extra_guest_count: None,
            })
            .unwrap();
        let failed = collect_until(&ch, &|e| matches!(e, BackendEvent::EventRespondFailed { .. }));
        assert!(
            failed.iter().all(|e| !matches!(e, BackendEvent::EventResponse { .. })),
            "a failed RSVP emits no response: {failed:?}"
        );
        ch.commands
            .send(BackendCommand::EditEvent {
                chat_jid: FAKE_DM_JID.to_owned(),
                message_id: "fake-event-mine-1".to_owned(),
                name: "X".to_owned(),
                description: String::new(),
                start_time: 1_700_200_000,
                end_time: 0,
                location: String::new(),
                join_link: String::new(),
                scheduled_call: false,
                extra_guests: false,
                canceled: false,
            })
            .unwrap();
        let failed = collect_until(&ch, &|e| matches!(e, BackendEvent::EventEditFailed { .. }));
        assert!(
            failed.iter().all(|e| !matches!(e, BackendEvent::EventEdited { .. })),
            "a failed edit emits no edit: {failed:?}"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    /// Failure injection for delete-for-me: the message stays
    /// until the uninjected retry removes it.
    #[test]
    fn injected_delete_for_me_failure_then_success() {
        let mut fake = FakeBackend::auto();
        fake.fail_next = vec!["DeleteMessageForMe"];
        let ch = start(fake);
        linked(&ch);
        let delete = BackendCommand::DeleteMessageForMe {
            chat_jid: FAKE_DM_JID.to_owned(),
            message_id: "fake-poll-1".to_owned(),
            sender_jid: None,
            from_me: false,
            delete_media: false,
            message_timestamp: None,
        };
        ch.commands.send(delete.clone()).unwrap();
        let failed = collect_until(&ch, &|e| matches!(e, BackendEvent::DeleteForMeFailed { .. }));
        assert!(
            failed.iter().all(|e| !matches!(e, BackendEvent::MessageDeletedForMe { .. })),
            "a failed delete removes nothing: {failed:?}"
        );
        ch.commands.send(delete).unwrap();
        let deleted =
            collect_until(&ch, &|e| matches!(e, BackendEvent::MessageDeletedForMe { .. }));
        assert!(
            matches!(&deleted[deleted.len() - 1], BackendEvent::MessageDeletedForMe { message_id, .. } if message_id == "fake-poll-1"),
            "the retry lands: {deleted:?}"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    /// Collects events until `done` matches one, with a
    /// deadline; returns everything collected, match included.
    fn collect_until(
        ch: &BackendChannels,
        done: &dyn Fn(&BackendEvent) -> bool,
    ) -> Vec<BackendEvent> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut out = Vec::new();
        while std::time::Instant::now() < deadline {
            // A quiet slice is not the end: the page thread
            // answers after 900 ms, so only the deadline stops
            // the collection.
            if let Ok(event) = ch.events.recv_timeout(Duration::from_millis(500)) {
                let finished = done(&event);
                out.push(event);
                if finished {
                    break;
                }
            }
        }
        out
    }

    /// Channels: the subscribed list answers with the
    /// scripted metadata (roles included), an invite lookup
    /// finds the discoverable channel as a Guest, following
    /// files its chat row and flips the role, and unfollowing
    /// takes it back out of the list.
    #[test]
    fn channel_list_lookup_follow_and_unfollow_flow() {
        let ch = start(FakeBackend::auto());
        linked(&ch);
        ch.commands.send(BackendCommand::FetchChannels).unwrap();
        let listed = collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelsListed { .. }));
        let BackendEvent::ChannelsListed { channels } = &listed[listed.len() - 1] else {
            panic!("expected the channel list: {listed:?}");
        };
        assert_eq!(channels.len(), 2, "the two scripted subscriptions: {channels:?}");
        let city = channels.iter().find(|c| c.jid == FAKE_CHANNEL_JID).expect("city news listed");
        assert_eq!(city.role, crate::LiveChannelRole::Follower);
        assert_eq!(city.followers, 12_483);
        assert!(city.verified);
        let own = channels
            .iter()
            .find(|c| c.jid == FAKE_OWN_CHANNEL_JID)
            .expect("the owned channel listed");
        assert_eq!(own.role, crate::LiveChannelRole::Owner);

        ch.commands
            .send(BackendCommand::LookupChannel { code: FAKE_DISCOVERABLE_INVITE.to_owned() })
            .unwrap();
        let found = collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelFound { .. }));
        let BackendEvent::ChannelFound { channel } = &found[found.len() - 1] else {
            panic!("expected the found channel: {found:?}");
        };
        assert_eq!(channel.jid, FAKE_DISCOVERABLE_CHANNEL_JID);
        assert_eq!(channel.role, crate::LiveChannelRole::Guest);
        assert_eq!(channel.followers, 3_208);

        ch.commands
            .send(BackendCommand::FollowChannel { jid: FAKE_DISCOVERABLE_CHANNEL_JID.to_owned() })
            .unwrap();
        let followed = collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelFollowed { .. }));
        assert!(
            followed.iter().any(|e| matches!(e, BackendEvent::ChatUpsert(chat)
                if chat.jid == FAKE_DISCOVERABLE_CHANNEL_JID)),
            "following files the chat row first: {followed:?}"
        );
        let BackendEvent::ChannelFollowed { channel } = &followed[followed.len() - 1] else {
            panic!("expected the follow answer: {followed:?}");
        };
        assert_eq!(channel.role, crate::LiveChannelRole::Follower);

        ch.commands
            .send(BackendCommand::UnfollowChannel { jid: FAKE_DISCOVERABLE_CHANNEL_JID.to_owned() })
            .unwrap();
        let unfollowed =
            collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelUnfollowed { .. }));
        assert!(
            matches!(&unfollowed[unfollowed.len() - 1], BackendEvent::ChannelUnfollowed { jid }
                if jid == FAKE_DISCOVERABLE_CHANNEL_JID),
            "the unfollow answers: {unfollowed:?}"
        );
        ch.commands.send(BackendCommand::FetchChannels).unwrap();
        let listed = collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelsListed { .. }));
        let BackendEvent::ChannelsListed { channels } = &listed[listed.len() - 1] else {
            panic!("expected the channel list: {listed:?}");
        };
        assert!(
            channels.iter().all(|c| c.jid != FAKE_DISCOVERABLE_CHANNEL_JID),
            "an unfollowed channel leaves the subscribed list: {channels:?}"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    /// An unknown invite code fails with its typed event,
    /// and an injected follow failure changes no state: the
    /// retry follows for real.
    #[test]
    fn channel_lookup_and_follow_failures_are_typed() {
        let mut fake = FakeBackend::auto();
        fake.fail_next = vec!["FollowChannel"];
        let ch = start(fake);
        linked(&ch);
        ch.commands
            .send(BackendCommand::LookupChannel { code: "NoSuchChannelCode".to_owned() })
            .unwrap();
        let failed = collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelLookupFailed { .. }));
        assert!(
            matches!(&failed[failed.len() - 1], BackendEvent::ChannelLookupFailed { .. }),
            "an unknown code answers with the typed failure: {failed:?}"
        );
        ch.commands
            .send(BackendCommand::FollowChannel { jid: FAKE_DISCOVERABLE_CHANNEL_JID.to_owned() })
            .unwrap();
        let failed = collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelFollowFailed { .. }));
        assert!(
            matches!(&failed[failed.len() - 1], BackendEvent::ChannelFollowFailed { .. }),
            "the injected failure answers typed: {failed:?}"
        );
        ch.commands
            .send(BackendCommand::FollowChannel { jid: FAKE_DISCOVERABLE_CHANNEL_JID.to_owned() })
            .unwrap();
        let followed = collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelFollowed { .. }));
        assert!(
            matches!(&followed[followed.len() - 1], BackendEvent::ChannelFollowed { channel }
                if channel.role == crate::LiveChannelRole::Follower),
            "the retry lands: {followed:?}"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    /// Channel history pages walk server IDs newest first,
    /// two at a time, and the last page says the history is
    /// exhausted.
    #[test]
    fn channel_history_pages_by_server_id() {
        let ch = start(FakeBackend::auto());
        linked(&ch);
        let page = |ch: &BackendChannels, before: Option<u64>| {
            ch.commands
                .send(BackendCommand::FetchChannelHistory {
                    jid: FAKE_CHANNEL_JID.to_owned(),
                    count: 2,
                    before,
                })
                .unwrap();
            collect_until(ch, &|e| matches!(e, BackendEvent::ChannelHistoryFetched { .. }))
        };
        let first = page(&ch, None);
        let ids: Vec<&str> = first
            .iter()
            .filter_map(|e| match e {
                BackendEvent::MessageUpsert(m) => Some(m.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["fake-ch-5", "fake-ch-4"], "the newest page first: {first:?}");
        assert!(
            matches!(
                &first[first.len() - 1],
                BackendEvent::ChannelHistoryFetched { added: 2, end_of_history: false, .. }
            ),
            "more pages exist: {first:?}"
        );
        let second = page(&ch, Some(104));
        let ids: Vec<&str> = second
            .iter()
            .filter_map(|e| match e {
                BackendEvent::MessageUpsert(m) => Some(m.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["fake-ch-3", "fake-ch-2"], "the page before 104: {second:?}");
        let third = page(&ch, Some(102));
        assert!(
            matches!(
                &third[third.len() - 1],
                BackendEvent::ChannelHistoryFetched { added: 1, end_of_history: true, .. }
            ),
            "the last page ends the history: {third:?}"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    /// A channel reaction and a poll vote settle through
    /// their typed events, and the next history fetch carries
    /// both in the server record: my reaction on the update,
    /// my ballot and the moved tally on the poll.
    #[test]
    fn channel_reaction_and_poll_vote_land_in_the_record() {
        let ch = start(FakeBackend::auto());
        linked(&ch);
        ch.commands
            .send(BackendCommand::ReactToChannelUpdate {
                jid: FAKE_CHANNEL_JID.to_owned(),
                server_id: 101,
                emoji: "❤️".to_owned(),
            })
            .unwrap();
        let set = collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelReactionSet { .. }));
        assert!(
            matches!(&set[set.len() - 1], BackendEvent::ChannelReactionSet { server_id: 101, .. }),
            "the reaction settles: {set:?}"
        );
        ch.commands
            .send(BackendCommand::VoteChannelPoll {
                jid: FAKE_CHANNEL_JID.to_owned(),
                server_id: 104,
                options: vec!["Every weekend".to_owned()],
            })
            .unwrap();
        let voted = collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelPollVoted { .. }));
        assert!(
            matches!(
                &voted[voted.len() - 1],
                BackendEvent::ChannelPollVoted { server_id: 104, .. }
            ),
            "the vote settles: {voted:?}"
        );
        ch.commands
            .send(BackendCommand::FetchChannelHistory {
                jid: FAKE_CHANNEL_JID.to_owned(),
                count: 30,
                before: None,
            })
            .unwrap();
        let history =
            collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelHistoryFetched { .. }));
        let first = history
            .iter()
            .filter_map(|e| match e {
                BackendEvent::MessageUpsert(m) if m.id == "fake-ch-1" => Some(m),
                _ => None,
            })
            .next_back()
            .expect("the first update is in the page");
        assert_eq!(first.my_reaction.as_deref(), Some("❤️"), "my reaction is recorded");
        let poll = history
            .iter()
            .filter_map(|e| match e {
                BackendEvent::MessageUpsert(m) if m.id == "fake-ch-4" => Some(m),
                _ => None,
            })
            .next_back()
            .expect("the poll is in the page");
        match &poll.kind {
            crate::LiveMessageKind::Poll { options, my_votes, .. } => {
                assert_eq!(my_votes, &vec![0], "my ballot is recorded");
                assert_eq!(options[0].1, 413, "the tally moved by my vote");
            }
            other => panic!("expected the poll, got {other:?}"),
        }
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    /// Community management (parity P6): linking the scripted
    /// unlinked group files it under the community (and an
    /// unknown group is refused with its code), creating a
    /// subgroup answers with a parented roster entry, removing
    /// a member takes them out of the parent's refreshed list,
    /// unlinking stands a group alone again, and deactivation
    /// is answered while the subgroups survive.
    #[test]
    fn community_management_round_trip() {
        let ch = start(FakeBackend::auto());
        linked(&ch);
        ch.commands
            .send(BackendCommand::LinkCommunityGroups {
                community_jid: FAKE_COMMUNITY_JID.to_owned(),
                group_jids: vec![FAKE_UNLINKED_GROUP_JID.to_owned(), "99999999999@g.us".to_owned()],
            })
            .unwrap();
        let linked_events =
            collect_until(&ch, &|e| matches!(e, BackendEvent::ChatCommunity { .. }));
        let answer = linked_events
            .iter()
            .find_map(|e| match e {
                BackendEvent::CommunityGroupsLinked { linked, failed, .. } => {
                    Some((linked.clone(), failed.clone()))
                }
                _ => None,
            })
            .expect("the link answer arrives");
        assert_eq!(answer.0, vec![FAKE_UNLINKED_GROUP_JID.to_owned()], "the real group links");
        assert_eq!(
            answer.1,
            vec![("99999999999@g.us".to_owned(), 404)],
            "the unknown group is refused with its code"
        );
        assert!(
            linked_events.iter().any(|e| matches!(
                e,
                BackendEvent::ChatCommunity { chat_jid, parent_jid: Some(p), .. }
                    if chat_jid == FAKE_UNLINKED_GROUP_JID && p == FAKE_COMMUNITY_JID
            )),
            "the linked group re-reports its structure: {linked_events:?}"
        );
        ch.commands
            .send(BackendCommand::CreateCommunitySubgroup {
                community_jid: FAKE_COMMUNITY_JID.to_owned(),
                name: "Trail Food".to_owned(),
            })
            .unwrap();
        let created =
            collect_until(&ch, &|e| matches!(e, BackendEvent::CommunitySubgroupCreated { .. }));
        let subgroup_jid = match &created[created.len() - 1] {
            BackendEvent::CommunitySubgroupCreated { group, .. } => {
                assert_eq!(group.title, "Trail Food");
                assert_eq!(group.parent_jid.as_deref(), Some(FAKE_COMMUNITY_JID));
                group.jid.clone()
            }
            other => panic!("expected the subgroup answer, got {other:?}"),
        };
        ch.commands
            .send(BackendCommand::FetchGroupMembers { chat_jid: FAKE_COMMUNITY_JID.to_owned() })
            .unwrap();
        let _ = collect_until(&ch, &|e| matches!(e, BackendEvent::GroupMembers { .. }));
        ch.commands
            .send(BackendCommand::RemoveCommunityParticipants {
                community_jid: FAKE_COMMUNITY_JID.to_owned(),
                participants: vec!["10000000004@s.whatsapp.net".to_owned()],
            })
            .unwrap();
        let removed = collect_until(&ch, &|e| matches!(e, BackendEvent::GroupMembers { .. }));
        assert!(
            removed.iter().any(|e| matches!(
                e,
                BackendEvent::CommunityParticipantsRemoved { participants, .. }
                    if participants == &vec!["10000000004@s.whatsapp.net".to_owned()]
            )),
            "the removal is answered: {removed:?}"
        );
        match &removed[removed.len() - 1] {
            BackendEvent::GroupMembers { members, .. } => {
                assert!(
                    !members.iter().any(|(j, _, _, _)| j == "10000000004@s.whatsapp.net"),
                    "the refreshed parent list loses the removed member: {members:?}"
                );
            }
            other => panic!("expected the refreshed member list, got {other:?}"),
        }
        ch.commands
            .send(BackendCommand::UnlinkCommunityGroups {
                community_jid: FAKE_COMMUNITY_JID.to_owned(),
                group_jids: vec![subgroup_jid.clone()],
            })
            .unwrap();
        let unlinked_events =
            collect_until(&ch, &|e| matches!(e, BackendEvent::ChatCommunity { .. }));
        assert!(
            unlinked_events.iter().any(|e| matches!(
                e,
                BackendEvent::CommunityGroupsUnlinked { unlinked, .. }
                    if unlinked == &vec![subgroup_jid.clone()]
            )),
            "the unlink is answered: {unlinked_events:?}"
        );
        ch.commands
            .send(BackendCommand::DeactivateCommunity {
                community_jid: FAKE_COMMUNITY_JID.to_owned(),
            })
            .unwrap();
        let deactivated =
            collect_until(&ch, &|e| matches!(e, BackendEvent::CommunityDeactivated { .. }));
        assert!(
            matches!(
                &deactivated[deactivated.len() - 1],
                BackendEvent::CommunityDeactivated { .. }
            ),
            "the deactivation is answered: {deactivated:?}"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    /// The owned channel's admin powers: posting a poll,
    /// editing and revoking an own update, and the info
    /// fetch with its admin count and follower sample. A
    /// follower channel refuses the poll with its typed
    /// failure.
    #[test]
    fn channel_admin_powers_and_info() {
        let ch = start(FakeBackend::auto());
        linked(&ch);
        ch.commands
            .send(BackendCommand::CreateChannelPoll {
                jid: FAKE_CHANNEL_JID.to_owned(),
                question: "Follower poll?".to_owned(),
                options: vec!["Yes".to_owned(), "No".to_owned()],
                multiple: false,
            })
            .unwrap();
        let refused = collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelPollFailed { .. }));
        assert!(
            matches!(&refused[refused.len() - 1], BackendEvent::ChannelPollFailed { .. }),
            "a follower cannot post a poll: {refused:?}"
        );
        ch.commands
            .send(BackendCommand::CreateChannelPoll {
                jid: FAKE_OWN_CHANNEL_JID.to_owned(),
                question: "Ship the channels round this week?".to_owned(),
                options: vec!["Ship it".to_owned(), "One more day".to_owned()],
                multiple: false,
            })
            .unwrap();
        let created = collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelPollCreated { .. }));
        assert!(
            created.iter().any(|e| matches!(e, BackendEvent::MessageUpsert(m)
                if matches!(&m.kind, crate::LiveMessageKind::Poll { .. }))),
            "the created poll lands as an upsert: {created:?}"
        );
        ch.commands
            .send(BackendCommand::EditChannelUpdate {
                jid: FAKE_OWN_CHANNEL_JID.to_owned(),
                message_id: "fake-own-1".to_owned(),
                text: "Build log: the release pipeline is green, again".to_owned(),
            })
            .unwrap();
        let edited = collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelUpdateEdited { .. }));
        assert!(
            matches!(&edited[edited.len() - 1], BackendEvent::ChannelUpdateEdited { message_id, .. }
                if message_id == "fake-own-1"),
            "the edit settles: {edited:?}"
        );
        ch.commands
            .send(BackendCommand::RevokeChannelUpdate {
                jid: FAKE_OWN_CHANNEL_JID.to_owned(),
                message_id: "fake-own-2".to_owned(),
            })
            .unwrap();
        let revoked =
            collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelUpdateRevoked { .. }));
        assert!(
            matches!(&revoked[revoked.len() - 1], BackendEvent::ChannelUpdateRevoked { message_id, .. }
                if message_id == "fake-own-2"),
            "the revoke settles: {revoked:?}"
        );
        ch.commands
            .send(BackendCommand::FetchChannelInfo { jid: FAKE_OWN_CHANNEL_JID.to_owned() })
            .unwrap();
        let info = collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelInfoFetched { .. }));
        let BackendEvent::ChannelInfoFetched { info } = &info[info.len() - 1] else {
            panic!("expected the channel info: {info:?}");
        };
        assert_eq!(info.admin_count, Some(2));
        assert_eq!(info.follower_names.len(), 3);
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    /// Creating a channel answers with the created metadata
    /// (Owner role), files its chat row first, and lists it
    /// among the subscriptions afterwards.
    #[test]
    fn channel_create_flow() {
        let ch = start(FakeBackend::auto());
        linked(&ch);
        ch.commands
            .send(BackendCommand::CreateChannel {
                name: "Harbor Builds".to_owned(),
                description: "What the harbor crew is building".to_owned(),
            })
            .unwrap();
        let created = collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelCreated { .. }));
        assert!(
            created.iter().any(|e| matches!(e, BackendEvent::ChatUpsert(_))),
            "the chat row is filed first: {created:?}"
        );
        let BackendEvent::ChannelCreated { channel } = &created[created.len() - 1] else {
            panic!("expected the created channel: {created:?}");
        };
        assert_eq!(channel.name, "Harbor Builds");
        assert_eq!(channel.role, crate::LiveChannelRole::Owner);
        assert_eq!(channel.followers, 0);
        ch.commands.send(BackendCommand::FetchChannels).unwrap();
        let listed = collect_until(&ch, &|e| matches!(e, BackendEvent::ChannelsListed { .. }));
        let BackendEvent::ChannelsListed { channels } = &listed[listed.len() - 1] else {
            panic!("expected the channel list: {listed:?}");
        };
        assert!(
            channels.iter().any(|c| c.name == "Harbor Builds"),
            "the new channel is subscribed: {channels:?}"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    #[test]
    fn older_pages_land_in_order_and_the_last_ends_history() {
        let ch = start(FakeBackend::auto());
        linked(&ch);
        // Page A, anchored on the oldest scripted message.
        ch.commands
            .send(BackendCommand::FetchOlderMessages {
                chat_jid: FAKE_DM_JID.to_owned(),
                oldest_message_id: "fake-1".to_owned(),
                oldest_from_me: false,
                oldest_timestamp_ms: 1_700_000_100_000,
            })
            .unwrap();
        let events = collect_until(&ch, &|e| matches!(e, BackendEvent::HistoryPageFetched { .. }));
        let ids: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::MessageUpsert(m) => Some(m.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["fake-old-a1", "fake-old-a2", "fake-old-a3"], "page A lands whole");
        assert!(
            matches!(
                events.last(),
                Some(BackendEvent::HistoryPageFetched { added: 3, end_of_history: false, .. })
            ),
            "page A completes with more behind it: {:?}",
            events.last()
        );
        // Page B, anchored on page A's oldest: the end.
        ch.commands
            .send(BackendCommand::FetchOlderMessages {
                chat_jid: FAKE_DM_JID.to_owned(),
                oldest_message_id: "fake-old-a1".to_owned(),
                oldest_from_me: false,
                oldest_timestamp_ms: 1_700_000_020_000,
            })
            .unwrap();
        let events = collect_until(&ch, &|e| matches!(e, BackendEvent::HistoryPageFetched { .. }));
        assert!(
            matches!(
                events.last(),
                Some(BackendEvent::HistoryPageFetched { added: 2, end_of_history: true, .. })
            ),
            "page B completes the history: {:?}",
            events.last()
        );
        // Any other anchor (or chat) answers empty and ended:
        // the phone had nothing older to give.
        ch.commands
            .send(BackendCommand::FetchOlderMessages {
                chat_jid: FAKE_GROUP_JID.to_owned(),
                oldest_message_id: "fake-1".to_owned(),
                oldest_from_me: false,
                oldest_timestamp_ms: 1_700_000_100_000,
            })
            .unwrap();
        let events = collect_until(&ch, &|e| matches!(e, BackendEvent::HistoryPageFetched { .. }));
        assert_eq!(events.len(), 1, "an empty page carries no upserts");
        assert!(
            matches!(
                events[0],
                BackendEvent::HistoryPageFetched { added: 0, end_of_history: true, .. }
            ),
            "an empty page is the end: {:?}",
            events[0]
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    #[test]
    fn group_picture_set_and_remove_show_in_fetch_avatar() {
        let ch = start(FakeBackend::auto());
        linked(&ch);
        let picture = fixture_png(FAKE_GROUP_JID);
        ch.commands
            .send(BackendCommand::SetGroupPicture {
                chat_jid: FAKE_GROUP_JID.to_owned(),
                image: Some(picture.clone()),
            })
            .unwrap();
        assert!(
            matches!(next(&ch), BackendEvent::GroupPictureChanged { ref chat_jid } if chat_jid == FAKE_GROUP_JID),
            "the set confirms"
        );
        ch.commands
            .send(BackendCommand::FetchAvatar { jid: FAKE_GROUP_JID.to_owned(), community: false })
            .unwrap();
        match next(&ch) {
            BackendEvent::AvatarBytes { bytes, picture_id, .. } => {
                assert_eq!(bytes, picture, "the fetch serves the picture that was set");
                assert_eq!(picture_id.as_deref(), Some("fake-picture-set"));
            }
            other => panic!("expected the set picture, got {other:?}"),
        }
        ch.commands
            .send(BackendCommand::SetGroupPicture {
                chat_jid: FAKE_GROUP_JID.to_owned(),
                image: None,
            })
            .unwrap();
        assert!(
            matches!(next(&ch), BackendEvent::GroupPictureChanged { .. }),
            "the remove confirms"
        );
        ch.commands
            .send(BackendCommand::FetchAvatar { jid: FAKE_GROUP_JID.to_owned(), community: false })
            .unwrap();
        assert!(
            matches!(next(&ch), BackendEvent::AvatarMissing { .. }),
            "after removal the group has no picture in test mode"
        );
        // A group the fake does not know refuses honestly.
        ch.commands
            .send(BackendCommand::SetGroupPicture {
                chat_jid: "99999999999@g.us".to_owned(),
                image: Some(picture),
            })
            .unwrap();
        assert!(
            matches!(next(&ch), BackendEvent::GroupPictureFailed { .. }),
            "an unknown group fails, never silently succeeds"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    #[test]
    fn lookup_resolves_the_directory_and_refuses_the_rest() {
        let ch = start(FakeBackend::auto());
        linked(&ch);
        // A fresh person, typed with formatting.
        ch.commands
            .send(BackendCommand::LookupContact { phone: "+1 (000) 000-0007".to_owned() })
            .unwrap();
        match next(&ch) {
            BackendEvent::ContactResolved { chat, .. } => {
                assert_eq!(chat.jid, "10000000007@s.whatsapp.net");
                assert_eq!(chat.title, "Emma Wilson");
            }
            other => panic!("expected a resolution, got {other:?}"),
        }
        // A roster person resolves to the roster chat itself.
        ch.commands
            .send(BackendCommand::LookupContact { phone: "10000000001".to_owned() })
            .unwrap();
        match next(&ch) {
            BackendEvent::ContactResolved { chat, .. } => {
                assert_eq!(chat.jid, FAKE_DM_JID);
                assert_eq!(chat.title, "Sarah Miller (live demo)");
            }
            other => panic!("expected the roster chat, got {other:?}"),
        }
        // A well-formed number nobody owns: not on WhatsApp.
        ch.commands.send(BackendCommand::LookupContact { phone: "999999999".to_owned() }).unwrap();
        assert!(
            matches!(next(&ch), BackendEvent::ContactNotFound { .. }),
            "an unowned number is a plain not-found"
        );
        // A malformed number: a failed lookup, never a
        // not-found.
        ch.commands.send(BackendCommand::LookupContact { phone: "123".to_owned() }).unwrap();
        assert!(
            matches!(next(&ch), BackendEvent::ContactLookupFailed { .. }),
            "a malformed number fails the lookup"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    #[test]
    fn post_text_status_refuses_an_empty_text_or_audience() {
        let ch = start(FakeBackend::auto());
        ch.commands
            .send(BackendCommand::PostTextStatus {
                text: "  ".to_owned(),
                recipients: vec![FAKE_DM_JID.to_owned()],
                privacy: "contacts".to_owned(),
            })
            .unwrap();
        assert!(
            matches!(next(&ch), BackendEvent::StatusPostFailed { .. }),
            "an empty status text is refused"
        );
        ch.commands
            .send(BackendCommand::PostTextStatus {
                text: "Hello".to_owned(),
                recipients: Vec::new(),
                privacy: "contacts".to_owned(),
            })
            .unwrap();
        assert!(
            matches!(next(&ch), BackendEvent::StatusPostFailed { .. }),
            "an empty audience is refused"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    #[test]
    fn post_media_status_is_recorded_and_answers_posted() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ch = start(FakeBackend::auto_with_log(log.clone()));
        ch.commands
            .send(BackendCommand::PostMediaStatus {
                kind: crate::MediaKind::Photo,
                data: vec![1, 2, 3],
                mime: "image/png".to_owned(),
                caption: "Trailhead".to_owned(),
                thumbnail: vec![9],
                seconds: 0,
                recipients: vec![FAKE_DM_JID.to_owned()],
                privacy: "contacts".to_owned(),
            })
            .unwrap();
        match next(&ch) {
            BackendEvent::StatusMediaPosted { status } => {
                assert!(status.from_me);
                assert_eq!(status.id, "fake-status-post-1");
                assert_eq!(status.body, "Trailhead");
                assert!(matches!(
                    status.kind,
                    crate::LiveMessageKind::Media { media: crate::MediaKind::Photo, .. }
                ));
            }
            other => panic!("expected StatusMediaPosted, got {other:?}"),
        }
        // The post is delivered back under the broadcast too.
        assert!(matches!(next(&ch), BackendEvent::StatusReceived { .. }));
        let recorded = log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        assert_eq!(recorded, ["post_media_status:Photo:3:1"]);
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    #[test]
    fn post_media_status_refuses_like_the_real_backend() {
        let ch = start(FakeBackend::auto());
        let post = |data: Vec<u8>, kind: crate::MediaKind, recipients: Vec<String>| {
            BackendCommand::PostMediaStatus {
                kind,
                data,
                mime: "image/png".to_owned(),
                caption: String::new(),
                thumbnail: Vec::new(),
                seconds: 0,
                recipients,
                privacy: "contacts".to_owned(),
            }
        };
        ch.commands
            .send(post(Vec::new(), crate::MediaKind::Photo, vec![FAKE_DM_JID.to_owned()]))
            .unwrap();
        assert!(
            matches!(next(&ch), BackendEvent::StatusPostFailed { .. }),
            "empty payload refused"
        );
        ch.commands
            .send(post(vec![1], crate::MediaKind::Voice, vec![FAKE_DM_JID.to_owned()]))
            .unwrap();
        assert!(matches!(next(&ch), BackendEvent::StatusPostFailed { .. }), "voice kind refused");
        ch.commands.send(post(vec![1], crate::MediaKind::Photo, Vec::new())).unwrap();
        assert!(
            matches!(next(&ch), BackendEvent::StatusPostFailed { .. }),
            "empty audience refused"
        );
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    #[test]
    fn delete_status_answers_deleted_and_echoes_the_revoke() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ch = start(FakeBackend::auto_with_log(log.clone()));
        ch.commands
            .send(BackendCommand::DeleteStatus {
                message_id: "fake-status-mine-1".to_owned(),
                recipients: vec![FAKE_DM_JID.to_owned()],
            })
            .unwrap();
        match next(&ch) {
            BackendEvent::StatusDeleted { message_id } => {
                assert_eq!(message_id, "fake-status-mine-1");
            }
            other => panic!("expected StatusDeleted, got {other:?}"),
        }
        match next(&ch) {
            BackendEvent::MessageRevoked { chat_jid, message_id } => {
                assert_eq!(chat_jid, "status@broadcast");
                assert_eq!(message_id, "fake-status-mine-1");
            }
            other => panic!("expected the broadcast revoke echo, got {other:?}"),
        }
        let recorded = log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        assert_eq!(recorded, ["delete_status:fake-status-mine-1:1"]);
        // An empty audience refuses: there is nobody to send
        // the revoke to.
        ch.commands
            .send(BackendCommand::DeleteStatus {
                message_id: "fake-status-mine-1".to_owned(),
                recipients: Vec::new(),
            })
            .unwrap();
        assert!(matches!(next(&ch), BackendEvent::StatusDeleteFailed { .. }));
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    #[test]
    fn injected_failures_answer_the_typed_failure_events() {
        let fake = FakeBackend {
            fail_next: vec!["React", "StarMessage", "PinMessage", "FetchOlderMessages"],
            ..FakeBackend::auto()
        };
        let ch = start(fake);
        linked(&ch);
        ch.commands
            .send(BackendCommand::React {
                chat_jid: FAKE_DM_JID.to_owned(),
                message_id: "fake-1".to_owned(),
                sender_jid: None,
                from_me: false,
                emoji: "👍".to_owned(),
            })
            .unwrap();
        assert!(matches!(next(&ch), BackendEvent::ReactionFailed { .. }));
        ch.commands
            .send(BackendCommand::StarMessage {
                chat_jid: FAKE_DM_JID.to_owned(),
                message_id: "fake-1".to_owned(),
                sender_jid: None,
                from_me: false,
                star: true,
            })
            .unwrap();
        assert!(matches!(next(&ch), BackendEvent::StarFailed { .. }));
        ch.commands
            .send(BackendCommand::PinMessage {
                chat_jid: FAKE_DM_JID.to_owned(),
                message_id: "fake-1".to_owned(),
                sender_jid: None,
                from_me: false,
                pin: true,
                duration: crate::PinDuration::Days7,
            })
            .unwrap();
        assert!(matches!(next(&ch), BackendEvent::PinFailed { .. }));
        ch.commands
            .send(BackendCommand::FetchOlderMessages {
                chat_jid: FAKE_DM_JID.to_owned(),
                oldest_message_id: "fake-1".to_owned(),
                oldest_from_me: false,
                oldest_timestamp_ms: 1_700_000_100_000,
            })
            .unwrap();
        assert!(matches!(next(&ch), BackendEvent::HistoryFetchFailed { .. }));
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    #[test]
    fn own_changes_echo_the_changed_events() {
        let ch = start(FakeBackend::auto());
        linked(&ch);
        ch.commands
            .send(BackendCommand::StarMessage {
                chat_jid: FAKE_DM_JID.to_owned(),
                message_id: "fake-1".to_owned(),
                sender_jid: None,
                from_me: false,
                star: true,
            })
            .unwrap();
        // The upsert lands first, then the server echo.
        assert!(matches!(next(&ch), BackendEvent::MessageUpsert(_)));
        match next(&ch) {
            BackendEvent::StarChanged { starred, .. } => assert!(starred),
            other => panic!("expected StarChanged, got {other:?}"),
        }
        ch.commands
            .send(BackendCommand::ChatAction {
                chat_jid: FAKE_DM_JID.to_owned(),
                action: crate::ChatAction::Archive,
            })
            .unwrap();
        assert!(matches!(next(&ch), BackendEvent::ChatUpsert(_)));
        match next(&ch) {
            BackendEvent::ArchiveChanged { archived, .. } => assert!(archived),
            other => panic!("expected ArchiveChanged, got {other:?}"),
        }
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }

    #[test]
    fn demo_video_fetch_serves_the_fixture_and_download_saves_it() {
        let ch = start(FakeBackend::demo());
        linked(&ch);
        // A regular video fetch lands the fixture in the video
        // cache and answers with the path.
        ch.commands
            .send(BackendCommand::FetchVideo {
                chat_jid: "status@broadcast".to_owned(),
                message_id: "fake-status-mike-1".to_owned(),
                video_ref: None,
                view_once: false,
            })
            .unwrap();
        let path = match next(&ch) {
            BackendEvent::VideoReady { path, .. } => path,
            other => panic!("expected VideoReady, got {other:?}"),
        };
        let written = std::fs::read(&path).expect("the cached fixture exists");
        assert_eq!(written, FIXTURE_MP4);
        std::fs::remove_file(&path).ok();
        // A view-once video answers with its bytes instead.
        ch.commands
            .send(BackendCommand::FetchVideo {
                chat_jid: FAKE_DM_JID.to_owned(),
                message_id: "fake-vo-1".to_owned(),
                video_ref: None,
                view_once: true,
            })
            .unwrap();
        match next(&ch) {
            BackendEvent::VideoBytes { bytes, .. } => assert_eq!(bytes, FIXTURE_MP4),
            other => panic!("expected VideoBytes, got {other:?}"),
        }
        // A download saves the fixture under the downloads
        // folder and answers with the saved path.
        ch.commands
            .send(BackendCommand::DownloadMedia {
                chat_jid: "status@broadcast".to_owned(),
                message_id: "fake-status-mike-1".to_owned(),
                document_ref: None,
            })
            .unwrap();
        let saved = match next(&ch) {
            BackendEvent::MediaSaved { path, .. } => path,
            other => panic!("expected MediaSaved, got {other:?}"),
        };
        let written = std::fs::read(&saved).expect("the saved fixture exists");
        assert_eq!(written, FIXTURE_MP4);
        std::fs::remove_file(&saved).ok();
        // A view-once download refuses.
        ch.commands
            .send(BackendCommand::DownloadMedia {
                chat_jid: FAKE_DM_JID.to_owned(),
                message_id: "fake-vo-1".to_owned(),
                document_ref: None,
            })
            .unwrap();
        assert!(matches!(next(&ch), BackendEvent::Error { .. }));
        ch.commands.send(BackendCommand::Shutdown).unwrap();
    }
}
