//! Data model for chats and messages, plus the deterministic demo dataset.
//!
//! The demo dataset is what the UI demo renders. It deliberately uses
//! generic names, no real people, no phone numbers, no student IDs.

use serde::{Deserialize, Serialize};

/// What kind of conversation a chat is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatKind {
    /// One-to-one conversation.
    Dm,
    /// A group with members.
    Group,
    /// A one-to-many channel (newsletter).
    Channel,
}

/// Delivery state of an outgoing message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Delivery {
    /// Still being sent.
    Sending,
    /// Reached the server (one tick).
    Sent,
    /// Reached the recipient (two ticks).
    Delivered,
    /// Seen by the recipient (colored ticks).
    Read,
    /// Sending failed; the message never left this device.
    Failed,
}

/// One person's answer to an event invitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventRsvp {
    /// Attending.
    Going,
    /// Undecided.
    Maybe,
    /// Not attending.
    NotGoing,
}

/// The kind of content a message carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MessageKind {
    /// Plain (rich-formattable) text.
    Text,
    /// An image, described by its caption for the demo.
    Image {
        /// Caption shown under the image (or inside the viewer for
        /// view-once photos).
        caption: String,
        /// View-once photo: it can be opened a single time, then it is
        /// gone, the bubble collapses to an "Opened" receipt.
        view_once: bool,
    },
    /// A video message (live media: caption + duration; the bytes
    /// stay on the phone/CDN until downloaded).
    Video {
        /// Caption shown under the video.
        caption: String,
        /// View-once video: opens once, then collapses to a receipt.
        view_once: bool,
        /// Duration in seconds (0 when the protocol did not say).
        secs: u32,
    },
    /// A voice message with a duration in seconds and a fake waveform.
    Voice {
        /// Duration in seconds.
        secs: u32,
        /// Normalised bar heights (0.0–1.0).
        waveform: Vec<f32>,
    },
    /// An audio file (a song or a recording sent as a file, the
    /// wire audio payload without the voice-note flag): a file
    /// card with a duration, never the voice-note bubble.
    Audio {
        /// Duration in seconds (0 when the protocol did not say).
        secs: u32,
        /// Display name: the format label the payload supports
        /// (an audio payload carries no file name).
        name: String,
        /// Size in bytes, as announced (0 when unknown).
        size_bytes: u64,
    },
    /// A document/file with a display name and its real size.
    Document {
        /// File name.
        name: String,
        /// Size in bytes, as announced (0 when unknown). Bytes,
        /// not KiB: a sub-1 KiB file must not round down to a
        /// displayed zero.
        size_bytes: u64,
        /// Page count when the sender's client stated one.
        page_count: Option<u32>,
    },
    /// A poll with options and vote counts.
    Poll {
        /// Poll question.
        question: String,
        /// (label, votes) pairs.
        options: Vec<(String, u32)>,
        /// Whether several options may be selected at once.
        multiple: bool,
        /// Indices of the options *I* voted for.
        my_votes: Vec<usize>,
        /// For quiz polls: the correct option index, when known.
        correct: Option<usize>,
    },
    /// An event: a named gathering with a start, an optional
    /// end, and responses from the invited. The counts are the
    /// aggregation of the per-responder ballots the session
    /// keeps (last response wins, like poll ballots).
    Event {
        /// Event name.
        name: String,
        /// Free-text details (may be empty).
        description: String,
        /// Start, unix seconds (0 when the source stated none).
        start_time: i64,
        /// End, unix seconds (0 when the event has no end).
        end_time: i64,
        /// Place label (may be empty). A label, not a map
        /// point: the place name or address the source states.
        location: String,
        /// Join/call link (may be empty).
        join_link: String,
        /// The event is a scheduled call.
        scheduled_call: bool,
        /// Extra guests are allowed.
        extra_guests: bool,
        /// The organizer cancelled the event.
        canceled: bool,
        /// My current response, when I have answered.
        my_rsvp: Option<EventRsvp>,
        /// Responses counted Going.
        going: u32,
        /// Responses counted Maybe.
        maybe: u32,
        /// Responses counted Not going.
        not_going: u32,
    },
    /// A shared location (render-only card).
    Location {
        /// Place name or address label.
        name: String,
        /// Latitude in degrees.
        latitude: f64,
        /// Longitude in degrees.
        longitude: f64,
    },
    /// A shared contact card (render-only).
    Contact {
        /// Contact display name.
        name: String,
        /// Contact phone/JID label.
        phone: String,
    },
    /// A carousel (interactive template) message: a row of
    /// cards. Received carousels render their cards with the
    /// buttons as static labels; the composer can also send
    /// a carousel built from raw protos (its cards cap at
    /// what the renderer shows).
    Carousel {
        /// Optional body text above the cards.
        body: String,
        /// The cards, in order.
        cards: Vec<CarouselCard>,
    },
    /// An interactive message with quick-reply buttons. The
    /// body text is the message's own body. A received
    /// message's buttons can be answered; the answer names
    /// the button's wire id.
    Buttons {
        /// Footer line under the body (may be empty).
        footer: String,
        /// The buttons, in wire order.
        buttons: Vec<MsgButton>,
    },
    /// An interactive list message: sections of pickable
    /// rows under a title. The message body carries the
    /// title; an answer names the row's wire id.
    ListMsg {
        /// Description under the title (may be empty).
        description: String,
        /// Label of the button that opens the list on a phone.
        button_label: String,
        /// Footer line (may be empty).
        footer: String,
        /// The sections, in wire order.
        sections: Vec<MsgListSection>,
    },
    /// An order placed with a business: a summary card whose
    /// line items load on demand through the order-details
    /// fetch the id and token authorize.
    Order {
        /// Order id from the payload.
        order_id: String,
        /// Per-order token from the payload (may be empty;
        /// the details fetch then has nothing to authorize
        /// with and the card offers no fetch).
        token: String,
        /// Number of items the payload states.
        item_count: u32,
        /// Total in thousandths of the currency unit, as the
        /// payload states it (0 when it states none).
        total_amount_1000: i64,
        /// ISO 4217 currency code of the total.
        currency: String,
        /// Order status, wire spelling (`INQUIRY`,
        /// `ACCEPTED`, `DECLINED`; empty when unstated).
        status: String,
    },
    /// A sticker: a small square image rendered at sticker
    /// size inside the bubble, never as a photo card. Its bytes
    /// are fetched on demand like a photo's, through the sticker
    /// fetch path.
    Sticker,
    /// A centred system notice (encryption notice, member joined, …).
    System,
}

/// One quick-reply button of a [`MessageKind::Buttons`]
/// message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MsgButton {
    /// The button's wire id (an answer names it).
    pub id: String,
    /// The label shown on the button.
    pub text: String,
}

/// One pickable row of a [`MessageKind::ListMsg`] section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MsgListRow {
    /// The row's wire id (an answer names it).
    pub id: String,
    /// The row's title.
    pub title: String,
    /// The row's description (may be empty).
    pub description: String,
}

/// One section of a [`MessageKind::ListMsg`] message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MsgListSection {
    /// The section title (may be empty for a single section).
    pub title: String,
    /// The rows, in wire order.
    pub rows: Vec<MsgListRow>,
}

/// One card of a [`MessageKind::Carousel`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CarouselCard {
    /// Card title.
    pub title: String,
    /// Card body text.
    pub body: String,
    /// Image description/URL label (no image bytes are carried).
    pub image: Option<String>,
    /// Button labels (rendered as static labels; template buttons
    /// are business actions the client does not execute).
    pub buttons: Vec<String>,
}

/// How a link preview's image renders, when the preview carries
/// one. The preview pipeline carries no image bytes: demo data
/// points at the bundled demo photo, and live OpenGraph fetches
/// do not download images, so live previews use `None` and render
/// the text card alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PreviewImage {
    /// No image: the text card alone.
    #[default]
    None,
    /// A small or square image: a thumbnail at the card's
    /// left edge, as tall as the card's text block, with the
    /// text at its right.
    Thumb,
    /// A large image: full width above the card text.
    Wide,
}

/// An OpenGraph link preview attached to a text message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkPreview {
    /// Page title.
    pub title: String,
    /// Page description (may be empty).
    pub description: String,
    /// The URL the preview describes.
    pub url: String,
    /// The preview's image treatment (older archives carry no
    /// field and default to no image).
    #[serde(default)]
    pub image: PreviewImage,
}

/// One message in a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Stable unique ID (demo IDs are deterministic).
    pub id: u64,
    /// Display name of the sender; `None` means "me".
    pub sender: Option<String>,
    /// Message body text (for `System`, the notice text).
    pub body: String,
    /// Kind of content.
    pub kind: MessageKind,
    /// Display time, e.g. "14:02".
    pub time: String,
    /// Display label of the calendar day this message belongs to
    /// ("Today", "Yesterday", or a full date such as
    /// "14 November 2026"). The conversation derives its day
    /// separators from these labels. Empty when the source
    /// carried no date; [`Message::day_label`] resolves that to
    /// "Today" (locally composed messages are same-day by
    /// construction).
    #[serde(default)]
    pub day: String,
    /// Delivery state for outgoing messages; `None` for incoming/system.
    pub delivery: Option<Delivery>,
    /// Name of the message this one quotes, if any.
    pub quoted_sender: Option<String>,
    /// Short quoted excerpt, if any.
    pub quoted_body: Option<String>,
    /// (emoji, count) reaction summary.
    pub reactions: Vec<(String, u32)>,
    /// Whether this message was edited.
    pub edited: bool,
    /// Whether this message was deleted for everyone (placeholder
    /// state: body already replaced by the host).
    pub deleted: bool,
    /// Whether the sender deleted this message while its archived
    /// body remains present locally. Only older archives carry
    /// this (a revoked message is now always replaced by the
    /// placeholder); the export renderer still marks these rows.
    pub deleted_by_sender: bool,
    /// An OpenGraph preview for a URL in the body, when fetched.
    pub link_preview: Option<LinkPreview>,
    /// Whether I starred this message.
    pub starred: bool,
    /// Whether this disappearing message is kept in the chat.
    /// A kept message never counts as expired, whatever its
    /// timer says.
    #[serde(default)]
    pub kept: bool,
    /// Whether this message is pinned in the chat.
    pub pinned: bool,
    /// Whether this message was forwarded into the chat.
    pub forwarded: bool,
    /// Unix second at which this message disappears, for chats
    /// with a disappearing-messages timer. `0` means the message
    /// never expires (every message sent before a timer was set,
    /// and every message in a timer-off chat). Older archives
    /// carry no field and default to `0`.
    #[serde(default)]
    pub expires_at: u64,
}

impl Message {
    /// A plain outgoing text message from me.
    #[must_use]
    pub fn mine(id: u64, body: impl Into<String>, time: impl Into<String>) -> Self {
        Self {
            id,
            sender: None,
            body: body.into(),
            kind: MessageKind::Text,
            time: time.into(),
            day: String::new(),
            delivery: Some(Delivery::Read),
            quoted_sender: None,
            quoted_body: None,
            reactions: Vec::new(),
            edited: false,
            deleted: false,
            deleted_by_sender: false,
            link_preview: None,
            starred: false,
            kept: false,
            pinned: false,
            forwarded: false,
            expires_at: 0,
        }
    }

    /// A plain incoming text message.
    #[must_use]
    pub fn theirs(
        id: u64,
        sender: impl Into<String>,
        body: impl Into<String>,
        time: impl Into<String>,
    ) -> Self {
        Self {
            id,
            sender: Some(sender.into()),
            body: body.into(),
            kind: MessageKind::Text,
            time: time.into(),
            day: String::new(),
            delivery: None,
            quoted_sender: None,
            quoted_body: None,
            reactions: Vec::new(),
            edited: false,
            deleted: false,
            deleted_by_sender: false,
            link_preview: None,
            starred: false,
            kept: false,
            pinned: false,
            forwarded: false,
            expires_at: 0,
        }
    }

    /// Is this message outgoing (sent by me)?
    #[must_use]
    pub fn is_mine(&self) -> bool {
        self.sender.is_none() && self.kind != MessageKind::System
    }

    /// Whether this message is view-once media: a photo or video
    /// the sender marked view-once. The flag lives on the kind, the
    /// same place the backend's unwrap of the view-once protocol
    /// wrapper puts it; a disappearing (ephemeral) message is a
    /// different thing and does not set it. View-once media must
    /// never be saved to disk, forwarded, or exported by the
    /// client, so every such path checks this first.
    #[must_use]
    pub fn is_view_once(&self) -> bool {
        matches!(
            self.kind,
            MessageKind::Image { view_once: true, .. } | MessageKind::Video { view_once: true, .. }
        )
    }

    /// The text this message contributes to glanceable surfaces
    /// (the chat-list preview, a starred row's snippet): its
    /// body, except for view-once media, which contributes only
    /// [`VIEW_ONCE_PREVIEW`]. The caption of a view-once message
    /// is part of its sealed payload: the bubble hides it and
    /// the viewer shows it only with the media itself, so no
    /// glanceable surface may carry it either.
    #[must_use]
    pub fn preview_text(&self) -> String {
        if self.is_view_once() {
            VIEW_ONCE_PREVIEW.to_owned()
        } else {
            self.body.clone()
        }
    }

    /// The day-separator label for this message: its stored day,
    /// or "Today" when no day was recorded.
    #[must_use]
    pub fn day_label(&self) -> &str {
        if self.day.is_empty() {
            "Today"
        } else {
            &self.day
        }
    }

    /// Whether this message's disappearing timer has run out at
    /// `now` (unix seconds). Messages with no timer never expire.
    #[must_use]
    pub fn is_expired(&self, now: u64) -> bool {
        !self.kept && self.expires_at != 0 && self.expires_at <= now
    }
}

/// The disappearing-message durations a chat can pick, in
/// seconds: off, 24 hours, 7 days, 90 days. These are the exact
/// values the protocol carries.
pub const EPHEMERAL_OPTIONS: [u32; 4] = [0, 86_400, 604_800, 7_776_000];

/// The only text a view-once message may contribute to
/// glanceable surfaces (chat-list previews, notification
/// bodies, starred rows): the placeholder its bubble shows.
/// A view-once caption is part of the sealed payload and is
/// displayed solely with the media itself, in the viewer, so
/// no other surface may ever carry it.
pub const VIEW_ONCE_PREVIEW: &str = "View once";

/// Ids of the messages in `messages` whose disappearing timer has
/// run out at `now` (unix seconds). Pure: the caller supplies the
/// clock, so the expiry pass is testable without sleeping.
#[must_use]
pub fn expired_message_ids(messages: &[Message], now: u64) -> Vec<u64> {
    messages.iter().filter(|m| m.is_expired(now)).map(|m| m.id).collect()
}

/// The media, link and document counts the info dock shows for
/// one conversation, counted from the conversation's own
/// messages. A message counts once per category it belongs to:
/// a photo with a link in its caption is both media and a link.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConversationCounts {
    /// Photo, video and carousel messages.
    pub media: usize,
    /// Messages carrying a link, in the body, a caption, a
    /// carousel card, or an attached link preview.
    pub links: usize,
    /// Document messages.
    pub docs: usize,
}

fn text_has_link(text: &str) -> bool {
    text.contains("http://") || text.contains("https://")
}

fn message_has_link(message: &Message) -> bool {
    if message.link_preview.is_some() || text_has_link(&message.body) {
        return true;
    }
    match &message.kind {
        MessageKind::Image { caption, .. } | MessageKind::Video { caption, .. } => {
            text_has_link(caption)
        }
        MessageKind::Carousel { body, cards } => {
            text_has_link(body)
                || cards.iter().any(|c| text_has_link(&c.title) || text_has_link(&c.body))
        }
        _ => false,
    }
}

/// Counts media, links and documents in `messages`. Pure, so
/// the dock's numbers are testable without a window and can
/// never drift from the conversation they describe.
#[must_use]
pub fn conversation_counts(messages: &[Message]) -> ConversationCounts {
    let mut counts = ConversationCounts::default();
    for message in messages {
        match &message.kind {
            MessageKind::Image { .. }
            | MessageKind::Video { .. }
            | MessageKind::Sticker
            | MessageKind::Carousel { .. } => {
                counts.media += 1;
            }
            MessageKind::Document { .. } => counts.docs += 1,
            _ => {}
        }
        if message_has_link(message) {
            counts.links += 1;
        }
    }
    counts
}

/// The media messages of one conversation, in conversation
/// order, for the profile dock's gallery (design standard
/// section 25). The set matches the media arm of
/// [`conversation_counts`] exactly, minus deleted messages,
/// whose media no longer exists to show. View-once messages
/// are included as rows: the gallery renders them as
/// placeholders that locate the message, never as content.
#[must_use]
/// Flattens a message body into a one-line preview snippet:
/// every run of whitespace (newlines included) collapses to a
/// single space and the ends are trimmed. Chat previews are
/// one line by construction; a multi-line body (a bridge bot's
/// structured post, a pasted log) must never reach a fixed-
/// height row with its line breaks intact. The stored body
/// itself is never changed; only the snippet is flattened.
pub fn flatten_preview(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The media messages of one conversation, in conversation
/// order, for the gallery's Media section: the media arm of
/// [`conversation_counts`] (photos, videos, stickers, and
/// carousels), minus deleted messages.
#[must_use]
pub fn gallery_media(messages: &[Message]) -> Vec<&Message> {
    messages
        .iter()
        .filter(|m| {
            !m.deleted
                && matches!(
                    m.kind,
                    MessageKind::Image { .. }
                        | MessageKind::Video { .. }
                        | MessageKind::Sticker
                        | MessageKind::Carousel { .. }
                )
        })
        .collect()
}

/// The document messages of one conversation, in conversation
/// order, for the gallery's Docs section: the document arm of
/// [`conversation_counts`], minus deleted messages.
#[must_use]
pub fn gallery_docs(messages: &[Message]) -> Vec<&Message> {
    messages
        .iter()
        .filter(|m| !m.deleted && matches!(m.kind, MessageKind::Document { .. }))
        .collect()
}

/// One link collected from a conversation for the gallery's
/// Links section: the message that carried it and the URL
/// itself.
#[derive(Debug, Clone, PartialEq)]
pub struct GalleryLink<'a> {
    /// The message the link arrived in.
    pub message: &'a Message,
    /// The link's URL (the attached preview's URL when the
    /// message carries one, else the first URL in its text).
    pub url: String,
}

/// The first http(s) URL span in a text, or `None`. A URL runs
/// to the next whitespace character.
fn first_url_in(text: &str) -> Option<String> {
    let start = text.find("http://").or_else(|| text.find("https://"))?;
    let rest = &text[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(rest[..end].to_owned())
}

/// The links of one conversation, in conversation order, one
/// per message that carries one (the link arm of
/// [`conversation_counts`], minus deleted messages): the
/// attached preview's URL wins, else the first URL found in
/// the message's body, caption, or carousel text.
#[must_use]
pub fn gallery_links<'a>(messages: &'a [Message]) -> Vec<GalleryLink<'a>> {
    let mut out = Vec::new();
    for message in messages {
        if message.deleted || !message_has_link(message) {
            continue;
        }
        let url = message.link_preview.as_ref().map(|p| p.url.clone()).or_else(|| {
            first_url_in(&message.body).or_else(|| match &message.kind {
                MessageKind::Image { caption, .. } | MessageKind::Video { caption, .. } => {
                    first_url_in(caption)
                }
                MessageKind::Carousel { body, cards } => first_url_in(body).or_else(|| {
                    cards
                        .iter()
                        .find_map(|c| first_url_in(&c.title).or_else(|| first_url_in(&c.body)))
                }),
                _ => None,
            })
        });
        if let Some(url) = url {
            out.push(GalleryLink { message, url });
        }
    }
    out
}

/// A one-line summary of a chat's latest message when it is not
/// plain text, carried on [`Chat`] so the chat row can label its
/// snippet without loading the whole conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreviewMedia {
    /// A photo (including a view-once photo).
    Photo,
    /// A video.
    Video,
    /// A voice message, with its duration in seconds.
    Voice(u32),
    /// An audio file, with its duration in seconds.
    Audio(u32),
    /// A document or file.
    Document,
    /// A poll.
    Poll,
    /// An event.
    Event,
    /// A sticker.
    Sticker,
}

impl PreviewMedia {
    /// The chat list label for this media summary, exactly as
    /// the chat row paints it.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Photo => "Photo".to_owned(),
            Self::Video => "Video".to_owned(),
            Self::Voice(secs) => format!("Voice message · {}:{:02}", secs / 60, secs % 60),
            Self::Audio(secs) => format!("Audio · {}:{:02}", secs / 60, secs % 60),
            Self::Document => "Document".to_owned(),
            Self::Poll => "Poll".to_owned(),
            Self::Event => "Event".to_owned(),
            Self::Sticker => "Sticker".to_owned(),
        }
    }

    /// The summary for a message kind, or `None` for the kinds
    /// the snippet shows as plain text.
    #[must_use]
    pub fn of(kind: &MessageKind) -> Option<Self> {
        match kind {
            MessageKind::Image { .. } => Some(Self::Photo),
            MessageKind::Video { .. } => Some(Self::Video),
            MessageKind::Voice { secs, .. } => Some(Self::Voice(*secs)),
            MessageKind::Audio { secs, .. } => Some(Self::Audio(*secs)),
            MessageKind::Document { .. } => Some(Self::Document),
            MessageKind::Poll { .. } => Some(Self::Poll),
            MessageKind::Event { .. } => Some(Self::Event),
            MessageKind::Sticker => Some(Self::Sticker),
            _ => None,
        }
    }
}

/// Whether a call was voice or video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallKind {
    /// Audio only.
    Voice,
    /// Audio and video.
    Video,
}

/// Which way a call went, from the user's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallDirection {
    /// The other side called the user.
    Incoming,
    /// The user called the other side.
    Outgoing,
}

/// One call-history entry: a past voice or video call with a
/// chat in the list. `timestamp` is unix seconds; the Calls
/// panel formats it with the same day and clock helpers the
/// message stream uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallEntry {
    /// Stable unique ID.
    pub id: u64,
    /// The chat the call belongs to (its title and avatar are
    /// resolved from the chat list at draw time).
    pub chat_id: u64,
    /// Voice or video.
    pub kind: CallKind,
    /// Incoming or outgoing.
    pub direction: CallDirection,
    /// The call never connected (it rang out or was declined):
    /// the row renders in the danger color and shows no
    /// duration.
    pub missed: bool,
    /// Talk time in seconds; 0 for missed calls and for calls
    /// whose duration the source did not record.
    pub duration_secs: u32,
    /// When the call started, unix seconds.
    pub timestamp: i64,
}

/// One conversation in the chat list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chat {
    /// Stable unique ID.
    pub id: u64,
    /// Kind of conversation.
    pub kind: ChatKind,
    /// Display title.
    pub title: String,
    /// Subtitle for the header: member list (group), subscribers (channel),
    /// presence text (DM).
    pub subtitle: String,
    /// Preview line of the last message.
    pub preview: String,
    /// Who sent the preview message, for groups ("Sarah: …").
    pub preview_sender: Option<String>,
    /// Whether the previewed (latest) message is ours, so the
    /// chat row can paint delivery ticks ahead of the snippet.
    #[serde(default)]
    pub preview_mine: bool,
    /// Delivery state of the previewed message, when it is ours.
    #[serde(default)]
    pub preview_delivery: Option<Delivery>,
    /// Media summary of the previewed message when it is not
    /// plain text, so the chat row can label the snippet instead
    /// of showing a bare caption.
    #[serde(default)]
    pub preview_media: Option<PreviewMedia>,
    /// Display time of the last message.
    pub time: String,
    /// Unread message count.
    pub unread: u32,
    /// Unread mention count.
    pub mentions: u32,
    /// Pinned to the top of the list.
    pub pinned: bool,
    /// Muted.
    pub muted: bool,
    /// The end of a timed mute, unix seconds; 0 when the mute
    /// has no end (indefinite) or the chat is not muted. The
    /// archive persists it (schema v12) so a timed mute survives
    /// a restart and lifts itself when it passes.
    #[serde(default)]
    pub mute_until: u64,
    /// Favorite.
    pub favorite: bool,
    /// Avatar background hue seed (the UI derives a color from it).
    pub avatar_seed: u8,
    /// Verified account (business/contact/channel): the UI draws
    /// the verified badge next to the name. Live chats earn it
    /// from the verified-name sources (history payloads and the
    /// usync business query); demo data carries examples.
    pub verified: bool,
    /// The chat is Meta's AI assistant contact (in live data this
    /// is recognized by the exact chat title "Meta AI", the
    /// library surfaces no dedicated AI-contact flag; see
    /// docs/ARCHITECTURE.md). Distinct from Zaprust's own
    /// user-provider "AI Assistant" chat.
    pub meta_ai: bool,
    /// Community this group belongs to, if any (group profile
    /// panel lists sibling groups in the same community).
    pub community: Option<String>,
    /// Whether the chat is archived (hidden from the main list,
    /// shown under the Archived filter).
    pub archived: bool,
    /// Whether the chat is locked (chat lock): hidden from
    /// every list except the locked folder, which sits behind
    /// the local lock code. The lock state is account state
    /// and syncs like archive; the code that gates the folder
    /// is local to this client and never syncs. Older
    /// archives carry no field and default to unlocked.
    #[serde(default)]
    pub locked: bool,
    /// Disappearing-messages timer for this chat, in seconds:
    /// new messages disappear this long after they are sent.
    /// `0` is off. One of [`EPHEMERAL_OPTIONS`]. Older archives
    /// carry no field and default to off.
    #[serde(default)]
    pub ephemeral_secs: u32,
    /// Messages of this conversation (demo data fills the opened ones).
    pub messages: Vec<Message>,
}

impl Chat {
    /// Refreshes the snippet anatomy fields (`preview_mine`,
    /// `preview_delivery`, `preview_media`) from the last loaded
    /// message. A no-op when the conversation is not in memory
    /// (live chats loaded from the archive keep whatever anatomy
    /// the arrival path recorded).
    pub fn sync_preview_from_last(&mut self) {
        if let Some(m) = self.messages.last() {
            self.preview_mine = m.is_mine();
            self.preview_delivery = if m.is_mine() { m.delivery } else { None };
            self.preview_media = PreviewMedia::of(&m.kind);
        }
    }
}

/// Builds the deterministic demo dataset.
///
/// Same input, same output, every run: screenshot reviews and tests rely
/// on this. Names are generic on purpose.
#[must_use]
pub fn demo_chats() -> Vec<Chat> {
    let mut chats = vec![
        group(
            1,
            "GTA VI Launch Crew",
            "Sarah, Mike, Emma, David, +128",
            "Sarah",
            "@everyone launch night is Thursday, doors at 7, don't be late",
            "18:12",
            1,
            1,
            true,
        ),
        dm(2, "Sarah Miller", "online", "Sure, I'll send the meeting notes in a bit", "17:48", 0),
        group(
            3,
            "Sunrise Yoga Club",
            "Alex, James, Lily, Noah, +312",
            "Noah",
            "Haha all good, see you at the studio tomorrow",
            "17:07",
            2,
            0,
            false,
        ),
        dm(
            4,
            "Mike Johnson",
            "last seen today at 16:40",
            "I've pushed the design file",
            "16:21",
            2,
        ),
        channel(
            5,
            "City News Daily",
            "24.1K subscribers",
            "",
            "Announcement: national holiday next week",
            "15:30",
            3,
        ),
        group(
            6,
            "Maple Street Neighbors",
            "Ava, David, Emma, Mike",
            "Ava",
            "The fence repair is done, just needs painting",
            "14:15",
            0,
            0,
            false,
        ),
        dm(7, "Emma Wilson", "online", "yesterday's hike", "12:51", 1),
        group(
            8,
            "Downtown Book Club",
            "Sarah, Alex, Noah, +44",
            "Sarah",
            "The discussion guide for this month's pick is up",
            "11:20",
            2,
            1,
            false,
        ),
        channel(
            9,
            "Rust Weekly",
            "8.4K subscribers",
            "",
            "This month's deep-dive: egui in production",
            "10:05",
            2,
        ),
        dm(10, "David Kim", "typing…", "Typing…", "09:58", 0),
        dm(11, "Mom", "last seen today at 09:12", "Don't forget to have lunch", "09:07", 0),
        group(
            12,
            "Family Trip Planning",
            "James, Lily, Mike, Sarah, +21",
            "Lily",
            "The December itinerary is final, check the pinned message",
            "08:43",
            1,
            0,
            false,
        ),
    ];
    // Verified examples + communities.
    chats[4].verified = true; // City News Daily (channel)
    chats[8].verified = true; // Rust Weekly (channel)
                              // Two chats run with a disappearing-messages timer, so the
                              // setting is visible in the demo data: the Sunrise Yoga
                              // Club group at 24 hours and the Mike Johnson DM at 7
                              // days. Their scripted messages predate the timers and
                              // carry no expiry.
    chats[2].ephemeral_secs = 86_400; // Sunrise Yoga Club (group)
    chats[3].ephemeral_secs = 604_800; // Mike Johnson (DM)
    for i in [0usize, 5, 7] {
        chats[i].community = Some("Downtown Community".to_owned());
    }
    chats
}

/// A WhatsApp Community: a named home that groups several group
/// chats together and carries one announcement chat whose messages
/// reach every member. `announcement_chat_id` and `group_chat_ids`
/// reference [`Chat`] ids; the announcement chat is itself a chat
/// (a group-kind chat admins post to), and it is also listed in
/// `group_chat_ids` consumers only through the announcement field,
/// never duplicated in the group list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Community {
    /// Stable unique ID (demo IDs are deterministic).
    pub id: u64,
    /// Display name. Group chats tag membership through
    /// [`Chat::community`] carrying this same name.
    pub name: String,
    /// Optional longer description shown on the community home.
    pub description: Option<String>,
    /// Accent color for the community avatar disc (same hue-seed
    /// space as [`Chat::avatar_seed`]: the UI derives the color).
    pub avatar_seed: u8,
    /// The announcement chat: a [`Chat`] whose posts broadcast to
    /// the whole community.
    pub announcement_chat_id: u64,
    /// The group chats that belong to this community.
    pub group_chat_ids: Vec<u64>,
    /// Groups in this community the user has not joined yet.
    /// They are not part of [`demo_chats`]; the community home
    /// resolves them through [`demo_community_other_groups`].
    #[serde(default)]
    pub other_group_ids: Vec<u64>,
}

/// Builds the deterministic demo communities.
///
/// One community wraps the demo groups already tagged with its name
/// in [`demo_chats`] (chats 1, 6 and 8) and adds one announcement
/// chat (id 14) that lives outside the plain [`demo_chats`] list:
/// the chat list shows announcements through the community home,
/// not as a loose row.
#[must_use]
pub fn demo_communities() -> Vec<Community> {
    vec![Community {
        id: 1,
        name: "Downtown Community".to_owned(),
        description: Some(
            "Announcements and groups run by the Downtown community center: \
             social clubs, neighborhood groups and hobby circles. This is the \
             home for event notices, shared planning and coordination across \
             the center's groups. Join the groups that match your interests, \
             and check Announcements for updates from the admins."
                .to_owned(),
        ),
        avatar_seed: 137,
        announcement_chat_id: 14,
        group_chat_ids: vec![1, 6, 8],
        other_group_ids: vec![15, 16],
    }]
}

/// The demo announcement chat for a community (id 14). It is a
/// group-kind chat admins post announcements to; it is not part of
/// [`demo_chats`] because the chat list reaches it through the
/// community home screen.
#[must_use]
pub fn demo_announcement_chat() -> Chat {
    Chat {
        id: 14,
        kind: ChatKind::Group,
        title: "Announcements".to_owned(),
        subtitle: "Community announcements".to_owned(),
        preview: "Group rules and the December events calendar are pinned in each group".to_owned(),
        preview_sender: Some("Admin".to_owned()),
        preview_mine: false,
        preview_delivery: None,
        preview_media: None,
        time: "09:00".to_owned(),
        unread: 2,
        mentions: 0,
        pinned: false,
        muted: false,
        mute_until: 0,
        favorite: false,
        avatar_seed: 137,
        verified: false,
        meta_ai: false,
        community: Some("Downtown Community".to_owned()),
        archived: false,
        locked: false,
        ephemeral_secs: 0,
        messages: Vec::new(),
    }
}

/// The not-yet-joined groups of a demo community, as full chat
/// values. They deliberately live outside [`demo_chats`]: a group
/// only joins the chat list once the user joins it from the
/// community home. Each returned chat carries the community name
/// tag on the chat value itself, is a group, and has no unread
/// messages. Unknown community ids return an empty list.
#[must_use]
pub fn demo_community_other_groups(community_id: u64) -> Vec<Chat> {
    if community_id != 1 {
        return Vec::new();
    }
    let mut football = group(
        15,
        "Weekend Football League",
        "Members, Sarah, Mike, +84",
        "Ava",
        "Sharing sign-up openings for the winter season",
        "13:05",
        0,
        0,
        false,
    );
    football.community = Some("Downtown Community".to_owned());
    let mut robotics = group(
        16,
        "Riverside Robotics Club",
        "Members, David, Emma, +42",
        "David",
        "The line follower test run is on Saturday morning",
        "12:20",
        0,
        0,
        false,
    );
    robotics.community = Some("Downtown Community".to_owned());
    vec![football, robotics]
}

/// The demo conversation for the community announcement chat:
/// admin posts only, last message matches the chat preview.
#[must_use]
pub fn demo_announcement_messages() -> Vec<Message> {
    vec![
        Message::theirs(14001, "Admin", "Welcome to the Downtown Community", "09:00"),
        Message::theirs(
            14002,
            "Admin",
            "Group rules and the December events calendar are pinned in each group",
            "09:00",
        ),
    ]
}

/// Opens the demo conversation for the group used as the showcase chat.
#[must_use]
pub fn demo_conversation() -> Vec<Message> {
    let mut msgs = vec![
        Message {
            id: 100,
            sender: None,
            // A stored system row (group creation). The
            // end-to-end encryption notice is not stored data:
            // the app renders it at the top of every conversation.
            body: "Sarah created this group".to_owned(),
            kind: MessageKind::System,
            time: String::new(),
            day: String::new(),
            delivery: None,
            quoted_sender: None,
            quoted_body: None,
            reactions: Vec::new(),
            edited: false,
            deleted: false,
            deleted_by_sender: false,
            link_preview: None,
            starred: false,
            kept: false,
            pinned: false,
            forwarded: false,
            expires_at: 0,
        },
        Message::theirs(
            101,
            "Sarah",
            "ngl I'm so locked in for launch night, the watch party plan is in the pinned post",
            "17:32",
        ),
        Message::theirs(
            102,
            "Mike",
            "Got it. If you haven't yet: the snack and seating list is in this group's pinned message.",
            "17:35",
        ),
        Message::mine(103, "Thanks for setting this up, I'll grab the extra controllers this afternoon", "17:36"),
        voice(104, "Emma", 42, "17:41"),
        Message {
            quoted_sender: Some("Sarah".to_owned()),
            quoted_body: Some("the watch party plan is in the pinned post".to_owned()),
            ..Message::theirs(
                105,
                "David",
                "I just booked the big screen room, it's giving full movie night, the rest tomorrow morning",
                "17:44",
            )
        },
        poll(106, "Emma"),
        Message::mine(107, "Let's lock the start time in the poll, my vote is in", "17:47"),
        // An album: three consecutive photos from one sender render
        // as a single grid card in the stream.
        image(108, "Ava", "Last year's launch night setup"),
        image(118, "Ava", ""),
        image(119, "Ava", ""),
        Message {
            reactions: vec![("👍".to_owned(), 4), ("❤️".to_owned(), 2)],
            ..Message::theirs(
                109,
                "Emma",
                "launch night is so close fr, if you haven't voted in the poll yet, do it soon",
                "18:02",
            )
        },
        Message {
            link_preview: Some(LinkPreview {
                title: "GTA VI: Everything We Know".to_owned(),
                description: "The release window, the editions and what to expect \
                              from launch week, gathered in one place."
                    .to_owned(),
                url: "https://example.com/gta-vi-launch-guide".to_owned(),
                image: PreviewImage::Wide,
            }),
            ..Message::theirs(
                116,
                "Sarah",
                "The full launch guide is here: https://example.com/gta-vi-launch-guide",
                "18:03",
            )
        },
        Message::theirs(
            110,
            "Sarah",
            "The *preload* opens Tuesday. lowkey free up space now, the install is `95 GB`, day-one patch ~probably~ included.",
            "18:05",
        ),
        voice(111, "Mike", 7, "18:06"),
        view_once_photo(112, "Emma", "Photo of my launch night setup, view once", "18:07"),
        Message {
            // The document card in the reference wireframe: icon
            // disc, file name, human size and a download chip.
            kind: MessageKind::Document {
                name: "Launch Night Checklist.pdf".to_owned(),
                size_bytes: 470 * 1024,
                page_count: Some(6),
            },
            ..Message::theirs(120, "Mike", "", "18:08")
        },
        Message::mine(113, "bet, *I've preloaded* it, we're so ready", "18:09"),
        Message::mine(115, "I'll bring the printed badges too", "18:10"),
        Message::theirs(
            114,
            "Sarah",
            "@everyone launch night is Thursday, doors at 7, don't be late",
            "18:12",
        ),
    ];
    // The showcase conversation exercises the full delivery
    // language: one tick (sent), two ticks (delivered), blue ticks
    // (read), and a clock (still sending).
    for m in &mut msgs {
        m.delivery = match m.id {
            103 => Some(Delivery::Sent),
            107 => Some(Delivery::Delivered),
            113 => Some(Delivery::Read),
            115 => Some(Delivery::Sending),
            _ => m.delivery,
        };
    }
    // The showcase conversation spans two days so the stream's
    // data-driven day separators are visible: the first stretch
    // (through the 17:47 message) is yesterday's, the rest is
    // today's.
    for m in &mut msgs {
        m.day = if m.id <= 107 { "Yesterday".to_owned() } else { "Today".to_owned() };
    }
    msgs
}

/// The demo conversation for any chat in [`demo_chats`]: a short,
/// internally consistent history whose LAST message is exactly the
/// chat-list preview (same sender, same time), whose timestamps
/// never go backwards, and whose trailing incoming run covers the
/// chat's unread badge. Chat 1 returns [`demo_conversation`].
#[must_use]
pub fn demo_messages_for(chat_id: u64) -> Vec<Message> {
    match chat_id {
        1 => demo_conversation(),
        2 => vec![
            Message::mine(2001, "Sarah, are you sending yesterday's meeting notes?", "17:40"),
            Message::theirs(2002, "Sarah Miller", "Yes, one sec, I'm tidying them up", "17:44"),
            Message::theirs(
                2003,
                "Sarah Miller",
                "Sure, I'll send the meeting notes in a bit",
                "17:48",
            ),
        ],
        3 => vec![
            Message::theirs(3001, "Alex", "Are we still on for the park session tomorrow?", "16:58"),
            Message::mine(3002, "Yes, at 9", "17:02"),
            // A link embed in the small (thumbnail) variant;
            // chat 1 carries the wide variant. Demo data for
            // the embed card, mid-conversation so the chat
            // list preview (the last message) is unchanged.
            {
                let mut m = Message::mine(
                    3005,
                    "The routine guide is here: https://example.com/morning-yoga-routines",
                    "17:03",
                );
                m.link_preview = Some(LinkPreview {
                    url: "https://example.com/morning-yoga-routines".to_owned(),
                    title: "Morning Yoga Routines".to_owned(),
                    description:
                        "A beginner-friendly set of morning flows, with a short video for each pose."
                            .to_owned(),
                    image: PreviewImage::Thumb,
                });
                m
            },
            Message::theirs(3003, "Alex", "Sure, I'll bring a spare mat", "17:05"),
            Message::theirs(
                3004,
                "Noah",
                "Haha all good, see you at the studio tomorrow",
                "17:07",
            ),
        ],
        4 => vec![
            Message::mine(4001, "Mike, how's the landing page design going?", "16:02"),
            Message::theirs(4002, "Mike Johnson", "Pushing it to the repo now, one moment", "16:15"),
            Message::theirs(4003, "Mike Johnson", "I've pushed the design file", "16:21"),
        ],
        5 => vec![
            Message::theirs(
                5001,
                "City News Daily",
                "Applications for the community grant program are open",
                "14:02",
            ),
            Message::theirs(
                5002,
                "City News Daily",
                "The city marathon route is out on the event page",
                "14:40",
            ),
            Message::theirs(
                5003,
                "City News Daily",
                "Announcement: national holiday next week",
                "15:30",
            ),
        ],
        6 => vec![
            Message::theirs(
                6001,
                "David",
                "I've been working on the community garden beds since yesterday",
                "13:58",
            ),
            Message::mine(6002, "I'm on the tool shed, done tonight", "14:03"),
            Message::theirs(6003, "Ava", "The fence repair is done, just needs painting", "14:15"),
        ],
        7 => vec![
            Message::mine(7001, "Emma, do you have yesterday's hike photo?", "12:40"),
            Message {
                kind: MessageKind::Image { caption: "yesterday's hike".to_owned(), view_once: false },
                ..Message::theirs(7002, "Emma Wilson", "", "12:51")
            },
        ],
        8 => vec![
            Message::theirs(8001, "Alex", "Is the meeting room booked for tonight?", "10:48"),
            Message::mine(8002, "It is, it just opens 10 minutes late", "10:52"),
            Message::theirs(
                8003,
                "Noah",
                "@everyone the discussion guide is available starting today",
                "11:15",
            ),
            Message::theirs(
                8004,
                "Sarah",
                "The discussion guide for this month's pick is up",
                "11:20",
            ),
        ],
        9 => vec![
            Message::theirs(9001, "Rust Weekly", "Rust 1.100 is out", "09:31"),
            Message::theirs(9002, "Rust Weekly", "This month's deep-dive: egui in production", "10:05"),
        ],
        10 => vec![
            Message::theirs(
                10001,
                "David Kim",
                "hey, did you finish the slides? ngl the deadline is stressing me out",
                "09:41",
            ),
            Message::mine(10002, "Done, just needs a final pass", "09:58"),
        ],
        11 => vec![
            Message::theirs(11001, "Mom", "Have you had breakfast yet?", "08:57"),
            Message::mine(11002, "I have, Mom. On my way to work now", "09:03"),
            Message::theirs(11003, "Mom", "Don't forget to have lunch", "09:07"),
        ],
        12 => vec![
            Message::theirs(
                12001,
                "James",
                "I let the AI redo yesterday's itinerary draft, lowkey it fixed the layover problem",
                "08:31",
            ),
            Message::mine(12002, "Okay, I'll check it with Lily later", "08:36"),
            Message::theirs(
                12003,
                "Lily",
                "The December itinerary is final, check the pinned message",
                "08:43",
            ),
        ],
        15 => vec![
            Message::theirs(
                15001,
                "Emma",
                "Still not over that World Cup summer, the winter season sign-up sheet is ready",
                "12:48",
            ),
            Message::theirs(
                15002,
                "Ava",
                "Sharing sign-up openings for the winter season",
                "13:05",
            ),
        ],
        16 => vec![
            Message::theirs(
                16001,
                "Emma",
                "The new motor driver boards arrived today",
                "11:47",
            ),
            Message::theirs(
                16002,
                "David",
                "The line follower test run is on Saturday morning",
                "12:20",
            ),
        ],
        _ => Vec::new(),
    }
}

#[allow(clippy::cast_precision_loss)] // demo ids are tiny; the waveform only needs a rough phase
fn voice(id: u64, sender: &str, secs: u32, time: &str) -> Message {
    // Deterministic pseudo-waveform from the message id.
    let waveform = (0..28)
        .map(|i| {
            let x = (id as f32 * 7.0 + i as f32 * 13.0).sin().abs();
            0.25 + 0.7 * x
        })
        .collect();
    Message { kind: MessageKind::Voice { secs, waveform }, ..Message::theirs(id, sender, "", time) }
}

fn poll(id: u64, sender: &str) -> Message {
    Message {
        kind: MessageKind::Poll {
            question: "When should the launch night watch party start?".to_owned(),
            options: vec![
                ("Thursday 6:00 PM".to_owned(), 6),
                ("Thursday 7:00 PM".to_owned(), 11),
                ("Thursday 8:00 PM".to_owned(), 4),
            ],
            multiple: false,
            my_votes: Vec::new(),
            correct: None,
        },
        ..Message::theirs(id, sender, "", "17:45")
    }
}

fn image(id: u64, sender: &str, caption: &str) -> Message {
    Message {
        kind: MessageKind::Image { caption: caption.to_owned(), view_once: false },
        ..Message::theirs(id, sender, "", "17:52")
    }
}

fn view_once_photo(id: u64, sender: &str, caption: &str, time: &str) -> Message {
    Message {
        kind: MessageKind::Image { caption: caption.to_owned(), view_once: true },
        ..Message::theirs(id, sender, "", time)
    }
}

#[allow(clippy::too_many_arguments)]
fn group(
    id: u64,
    title: &str,
    subtitle: &str,
    sender: &str,
    preview: &str,
    time: &str,
    unread: u32,
    mentions: u32,
    pinned: bool,
) -> Chat {
    Chat {
        id,
        kind: ChatKind::Group,
        title: title.to_owned(),
        subtitle: subtitle.to_owned(),
        preview: preview.to_owned(),
        preview_sender: Some(sender.to_owned()),
        preview_mine: false,
        preview_delivery: None,
        preview_media: None,
        time: time.to_owned(),
        unread,
        mentions,
        pinned,
        muted: false,
        mute_until: 0,
        favorite: pinned,
        avatar_seed: (id * 37 % 251) as u8,
        verified: false,
        meta_ai: false,
        community: None,
        archived: false,
        locked: false,
        ephemeral_secs: 0,
        messages: Vec::new(),
    }
}

fn dm(id: u64, title: &str, subtitle: &str, preview: &str, time: &str, unread: u32) -> Chat {
    Chat {
        id,
        kind: ChatKind::Dm,
        title: title.to_owned(),
        subtitle: subtitle.to_owned(),
        preview: preview.to_owned(),
        preview_sender: None,
        preview_mine: false,
        preview_delivery: None,
        preview_media: None,
        time: time.to_owned(),
        unread,
        mentions: 0,
        pinned: false,
        muted: false,
        mute_until: 0,
        favorite: false,
        avatar_seed: (id * 53 % 251) as u8,
        verified: false,
        meta_ai: false,
        community: None,
        archived: false,
        locked: false,
        ephemeral_secs: 0,
        messages: Vec::new(),
    }
}

fn channel(
    id: u64,
    title: &str,
    subtitle: &str,
    _s: &str,
    preview: &str,
    time: &str,
    unread: u32,
) -> Chat {
    Chat {
        id,
        kind: ChatKind::Channel,
        title: title.to_owned(),
        subtitle: subtitle.to_owned(),
        preview: preview.to_owned(),
        preview_sender: None,
        preview_mine: false,
        preview_delivery: None,
        preview_media: None,
        time: time.to_owned(),
        unread,
        mentions: 0,
        pinned: false,
        muted: true,
        mute_until: 0,
        favorite: false,
        avatar_seed: (id * 71 % 251) as u8,
        verified: false,
        meta_ai: false,
        community: None,
        archived: false,
        locked: false,
        ephemeral_secs: 0,
        messages: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_once_flag_lives_on_the_media_kind_only() {
        let mut photo = Message::theirs(1, "Sarah", "", "10:00");
        photo.kind = MessageKind::Image { caption: String::new(), view_once: true };
        assert!(photo.is_view_once());
        let mut video = Message::theirs(2, "Sarah", "", "10:01");
        video.kind = MessageKind::Video { caption: String::new(), view_once: true, secs: 3 };
        assert!(video.is_view_once());
        // Ordinary media is not view-once.
        let mut plain_photo = Message::theirs(3, "Sarah", "", "10:02");
        plain_photo.kind = MessageKind::Image { caption: String::new(), view_once: false };
        assert!(!plain_photo.is_view_once());
        // A disappearing text message is ephemeral, not view-once:
        // the two flags must not be conflated.
        let mut timed = Message::theirs(4, "Sarah", "gone soon", "10:03");
        timed.expires_at = 1_000;
        assert!(!timed.is_view_once());
        assert!(!Message::theirs(5, "Sarah", "hello", "10:04").is_view_once());
    }

    #[test]
    fn preview_text_never_carries_a_view_once_caption() {
        // The mirror body of a media message is its caption; a
        // view-once message must still contribute only the
        // placeholder to glanceable surfaces (privacy finding D2).
        let mut vo_photo = Message::theirs(1, "Sarah", "launch night setup", "10:00");
        vo_photo.kind =
            MessageKind::Image { caption: "launch night setup".to_owned(), view_once: true };
        assert_eq!(vo_photo.preview_text(), VIEW_ONCE_PREVIEW);
        let mut vo_video = Message::theirs(2, "Sarah", "watch this once", "10:01");
        vo_video.kind =
            MessageKind::Video { caption: "watch this once".to_owned(), view_once: true, secs: 4 };
        assert_eq!(vo_video.preview_text(), VIEW_ONCE_PREVIEW);
        // Ordinary media keeps its caption as the preview text.
        let mut photo = Message::theirs(3, "Sarah", "yesterday's hike", "10:02");
        photo.kind =
            MessageKind::Image { caption: "yesterday's hike".to_owned(), view_once: false };
        assert_eq!(photo.preview_text(), "yesterday's hike");
        // A disappearing text message is not view-once: its body
        // previews as usual until it expires.
        let mut timed = Message::theirs(4, "Sarah", "gone soon", "10:03");
        timed.expires_at = 1_000;
        assert_eq!(timed.preview_text(), "gone soon");
        assert_eq!(Message::theirs(5, "Sarah", "hello", "10:04").preview_text(), "hello");
    }

    #[test]
    fn conversation_counts_cover_media_links_and_docs() {
        let mut photo = Message::theirs(1, "Sarah", "", "10:00");
        photo.kind = MessageKind::Image {
            caption: "see https://example.com/x".to_owned(),
            view_once: false,
        };
        let mut doc = Message::theirs(2, "Sarah", "", "10:01");
        doc.kind = MessageKind::Document {
            name: "notes.pdf".to_owned(),
            size_bytes: 12 * 1024,
            page_count: None,
        };
        // A preview alone proves a link even when the body that
        // carried the URL is not the visible text.
        let mut preview_only = Message::theirs(3, "Sarah", "look at this", "10:02");
        preview_only.link_preview = Some(LinkPreview {
            title: "Example".to_owned(),
            description: String::new(),
            url: "https://example.com".to_owned(),
            image: PreviewImage::None,
        });
        let plain = Message::theirs(4, "Sarah", "no link here", "10:03");
        let counts = conversation_counts(&[photo, doc, preview_only, plain]);
        assert_eq!(counts, ConversationCounts { media: 1, links: 2, docs: 1 });
    }

    #[test]
    fn conversation_counts_of_the_demo_showcase_are_not_zero() {
        // The showcase conversation visibly contains photos, a
        // link and a document, so its dock counts must say so.
        let counts = conversation_counts(&demo_conversation());
        assert!(counts.media > 0, "showcase photos and view-once media count as media");
        assert!(counts.links > 0, "the showcase link message counts as a link");
        assert!(counts.docs > 0, "the showcase document counts as a doc");
        assert_eq!(conversation_counts(&[]), ConversationCounts::default());
    }

    #[test]
    fn expiry_uses_the_supplied_clock() {
        let mut timed = Message::mine(1, "secret", "10:00");
        timed.expires_at = 1_000;
        let plain = Message::mine(2, "kept forever", "10:00");
        let msgs = vec![timed, plain];
        // Before the deadline nothing expires; at the deadline
        // the timed message is gone; the untimed one never goes.
        assert!(expired_message_ids(&msgs, 999).is_empty());
        assert_eq!(expired_message_ids(&msgs, 1_000), vec![1]);
        assert_eq!(expired_message_ids(&msgs, u64::MAX), vec![1]);
        assert!(!msgs[1].is_expired(u64::MAX));
    }

    #[test]
    fn ephemeral_options_are_the_protocol_durations() {
        assert_eq!(EPHEMERAL_OPTIONS, [0, 86_400, 604_800, 7_776_000]);
        // Demo data exercises two of them (a group and a DM).
        let chats = demo_chats();
        assert_eq!(chats.iter().filter(|c| c.ephemeral_secs > 0).count(), 2);
        assert!(chats.iter().all(|c| EPHEMERAL_OPTIONS.contains(&c.ephemeral_secs)));
    }

    /// Demo-idiomatic invariants (regression test for the demo
    /// audit): for every demo chat, the stored conversation's last
    /// message IS the chat-list preview, timestamps never go
    /// backwards, and the unread badge never exceeds, and when
    /// positive, exactly matches, the trailing run of incoming
    /// messages after the last outgoing one.
    #[test]
    fn demo_conversations_match_chat_list_state() {
        for chat in demo_chats() {
            let msgs = demo_messages_for(chat.id);
            assert!(!msgs.is_empty(), "chat {} has no demo messages", chat.id);
            // Timestamps non-decreasing (HH:MM strings sort correctly).
            for pair in msgs.windows(2) {
                assert!(
                    pair[0].time <= pair[1].time,
                    "chat {}: time goes backwards at {} -> {}",
                    chat.id,
                    pair[0].time,
                    pair[1].time
                );
            }
            // Preview equals the last message (text), or the
            // caption for an image; the chat row paints the media
            // label itself, so the preview must not repeat it.
            let last = msgs.last().expect("non-empty");
            // Typing exception: while a contact is typing, WhatsApp
            // replaces the preview with the typing label, the chat
            // subtitle records that state, and it is the ONLY case
            // where the preview is not the last message.
            if chat.subtitle == "typing…" {
                assert_eq!(chat.preview, "Typing…");
                assert_eq!(last.time, chat.time);
                continue;
            }
            match &last.kind {
                MessageKind::Image { caption, .. } => {
                    assert_eq!(
                        &chat.preview, caption,
                        "chat {}: image preview should be the caption",
                        chat.id
                    );
                }
                _ => {
                    assert_eq!(
                        last.body, chat.preview,
                        "chat {}: preview != last message",
                        chat.id
                    );
                }
            }
            assert_eq!(last.time, chat.time, "chat {}: preview time mismatch", chat.id);
            if let Some(sender) = &chat.preview_sender {
                assert_eq!(
                    last.sender.as_deref(),
                    Some(sender.as_str()),
                    "chat {}: preview sender mismatch",
                    chat.id
                );
            }
            let trailing_incoming =
                msgs.iter().rev().take_while(|m| m.sender.is_some()).count() as u32;
            assert!(
                chat.unread <= trailing_incoming,
                "chat {}: unread {} exceeds trailing incoming {}",
                chat.id,
                chat.unread,
                trailing_incoming
            );
            if chat.unread > 0 {
                assert_eq!(
                    chat.unread, trailing_incoming,
                    "chat {}: unread must equal the unread tail",
                    chat.id
                );
            }
            // Mentions never exceed @-messages in the unread tail.
            let tail_mentions = msgs[msgs.len() - trailing_incoming as usize..]
                .iter()
                .filter(|m| m.body.contains('@'))
                .count() as u32;
            assert!(
                chat.mentions <= tail_mentions,
                "chat {}: mentions {} but tail has {} @-messages",
                chat.id,
                chat.mentions,
                tail_mentions
            );
        }
    }

    #[test]
    fn preview_media_labels_match_chat_list() {
        assert_eq!(PreviewMedia::Photo.label(), "Photo");
        assert_eq!(PreviewMedia::Video.label(), "Video");
        assert_eq!(PreviewMedia::Voice(75).label(), "Voice message · 1:15");
        assert_eq!(PreviewMedia::Voice(9).label(), "Voice message · 0:09");
        assert_eq!(PreviewMedia::Document.label(), "Document");
        assert_eq!(PreviewMedia::Poll.label(), "Poll");
    }

    #[test]
    fn demo_data_is_deterministic() {
        assert_eq!(demo_chats(), demo_chats());
        assert_eq!(demo_conversation(), demo_conversation());
        assert_eq!(demo_communities(), demo_communities());
        assert_eq!(demo_announcement_chat(), demo_announcement_chat());
        assert_eq!(demo_community_other_groups(1), demo_community_other_groups(1));
    }

    #[test]
    fn demo_other_groups_exist_but_are_not_listed() {
        let chats = demo_chats();
        for community in demo_communities() {
            let others = demo_community_other_groups(community.id);
            assert_eq!(others.len(), community.other_group_ids.len());
            let mut ids: Vec<u64> = others.iter().map(|c| c.id).collect();
            ids.sort_unstable();
            let mut listed = community.other_group_ids.clone();
            listed.sort_unstable();
            assert_eq!(ids, listed, "other group ids must match other_group_ids");
            for g in &others {
                assert_eq!(g.kind, ChatKind::Group, "other chat {} is not a group", g.id);
                assert_eq!(g.community.as_deref(), Some(community.name.as_str()));
                assert_eq!(g.unread, 0);
                assert!(
                    !chats.iter().any(|c| c.id == g.id),
                    "other group {} must not be in demo_chats",
                    g.id
                );
                // demo_messages_for covers it and matches its preview.
                let msgs = demo_messages_for(g.id);
                assert!(!msgs.is_empty(), "other group {} has no messages", g.id);
                for pair in msgs.windows(2) {
                    assert!(
                        pair[0].time <= pair[1].time,
                        "other group {} times go backwards",
                        g.id
                    );
                }
                let last = msgs.last().expect("non-empty");
                assert_eq!(last.body, g.preview, "other group {} preview mismatch", g.id);
                assert_eq!(last.time, g.time, "other group {} time mismatch", g.id);
                assert_eq!(
                    last.sender.as_deref(),
                    g.preview_sender.as_deref(),
                    "other group {} sender mismatch",
                    g.id
                );
            }
        }
        assert!(demo_community_other_groups(999).is_empty());
    }

    #[test]
    fn demo_announcement_chat_has_two_unread() {
        let ann = demo_announcement_chat();
        assert_eq!(ann.unread, 2);
        let msgs = demo_announcement_messages();
        assert_eq!(msgs.len(), 2);
        assert!(msgs.iter().all(|m| m.sender.is_some()));
    }

    /// Community consistency: every community group resolves to a
    /// demo chat that carries the community's name tag, the name
    /// tag appears on exactly those chats (plus the announcement
    /// chat), and the announcement conversation matches its chat
    /// row the same way every other demo chat does.
    #[test]
    fn demo_communities_are_consistent() {
        let chats = demo_chats();
        for community in demo_communities() {
            let mut tagged: Vec<u64> = chats
                .iter()
                .filter(|c| c.community.as_deref() == Some(community.name.as_str()))
                .map(|c| c.id)
                .collect();
            tagged.sort_unstable();
            let mut listed = community.group_chat_ids.clone();
            listed.sort_unstable();
            assert_eq!(tagged, listed, "community {} group list mismatch", community.name);
            for id in &community.group_chat_ids {
                let chat = chats.iter().find(|c| c.id == *id).expect("group exists");
                assert_eq!(chat.kind, ChatKind::Group, "chat {id} is not a group");
            }
            let ann = demo_announcement_chat();
            assert_eq!(ann.id, community.announcement_chat_id);
            assert_eq!(ann.community.as_deref(), Some(community.name.as_str()));
            assert!(!community.group_chat_ids.contains(&ann.id));
            let msgs = demo_announcement_messages();
            let last = msgs.last().expect("announcement messages");
            assert_eq!(last.body, ann.preview);
            assert_eq!(last.time, ann.time);
            assert_eq!(last.sender.as_deref(), ann.preview_sender.as_deref());
        }
    }

    #[test]
    fn demo_data_has_no_phone_like_numbers() {
        for chat in demo_chats() {
            assert!(
                !chat.title.chars().filter(char::is_ascii_digit).count().ge(&8),
                "title looks like it contains a phone number: {}",
                chat.title
            );
        }
    }

    #[test]
    fn showcase_group_exists_and_is_pinned() {
        let chats = demo_chats();
        let first = &chats[0];
        assert_eq!(first.kind, ChatKind::Group);
        assert!(first.pinned);
        assert!(first.mentions > 0);
    }

    #[test]
    fn conversation_covers_message_kinds() {
        let msgs = demo_conversation();
        assert!(msgs.iter().any(|m| m.kind == MessageKind::System));
        assert!(msgs.iter().any(|m| matches!(m.kind, MessageKind::Voice { .. })));
        assert!(msgs.iter().any(|m| matches!(m.kind, MessageKind::Poll { .. })));
        assert!(msgs.iter().any(|m| matches!(m.kind, MessageKind::Image { .. })));
        assert!(msgs.iter().any(super::Message::is_mine));
        assert!(msgs.iter().any(|m| !m.reactions.is_empty()));
        assert!(msgs.iter().any(|m| m.quoted_sender.is_some()));
    }

    #[test]
    fn flatten_preview_collapses_a_multiline_body_to_one_line() {
        // The owner's live capture (23 Nov 2026): a bot post
        // with newlines painted as overlapping lines in the
        // one-line chat row. The preview form is one line; the
        // stored body is never the preview form.
        assert_eq!(
            flatten_preview("GmbDownloader\nAuthor: Bot\n\nCode:  42\t"),
            "GmbDownloader Author: Bot Code: 42"
        );
        assert_eq!(flatten_preview("  spaced   out "), "spaced out");
        assert_eq!(flatten_preview(""), "");
    }

    #[test]
    fn gallery_collects_media_docs_and_links_in_order() {
        // The gallery derivations (section 25) classify the same
        // conversation the dock counts describe.
        let mut photo = Message::theirs(1, "Ana", "", "10:00");
        photo.kind = MessageKind::Image { caption: "cover".to_owned(), view_once: false };
        let mut doc = Message::theirs(2, "Ana", "", "10:01");
        doc.kind = MessageKind::Document {
            name: "notes.pdf".to_owned(),
            size_bytes: 9 * 1024,
            page_count: None,
        };
        let linked = Message::theirs(3, "Ana", "see https://example.com/spec now", "10:02");
        let mut gone = Message::theirs(4, "Ana", "", "10:03");
        gone.kind = MessageKind::Image { caption: String::new(), view_once: false };
        gone.deleted = true;
        let msgs = vec![photo, doc, linked, gone];
        let media = gallery_media(&msgs);
        assert_eq!(media.len(), 1, "the deleted photo is not gallery media");
        assert_eq!(media[0].id, 1);
        let docs = gallery_docs(&msgs);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].id, 2);
        let links = gallery_links(&msgs);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].url, "https://example.com/spec");
        assert_eq!(links[0].message.id, 3);
        // The attached preview's URL wins over the body text.
        let mut previewed = Message::theirs(5, "Ana", "look https://a.example/x", "10:04");
        previewed.link_preview = Some(LinkPreview {
            title: "T".to_owned(),
            description: String::new(),
            url: "https://b.example/y".to_owned(),
            image: PreviewImage::None,
        });
        let binding = [previewed];
        let links = gallery_links(&binding);
        assert_eq!(links[0].url, "https://b.example/y");
    }
}
