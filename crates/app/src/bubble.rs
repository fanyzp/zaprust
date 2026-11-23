//! Message bubble rendering, painted manually so bubbles hug their
//! content the way WhatsApp's do (see the function docs).

use crate::theme::rgb;
use crate::widgets::sender_color;
use core::model::{Chat, ChatKind, Delivery, Message, MessageKind};
use core::theme::Tokens;
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Layout, Pos2, Rect, Sense, Stroke, Vec2,
};

fn layout_text(
    ui: &egui::Ui,
    text: &str,
    font: FontId,
    color: Color32,
    wrap: f32,
) -> std::sync::Arc<egui::Galley> {
    ui.ctx().fonts_mut(|f| f.layout(text.to_owned(), font, color, wrap))
}

fn measure_text(ui: &egui::Ui, text: &str, font: FontId) -> f32 {
    ui.ctx().fonts_mut(|f| f.layout_no_wrap(text.to_owned(), font, Color32::WHITE).size().x)
}

pub(crate) fn tick_count(delivery: Option<Delivery>) -> usize {
    match delivery {
        Some(Delivery::Read | Delivery::Delivered) => 2,
        Some(Delivery::Sent) => 1,
        _ => 0,
    }
}

/// Paints the delivery ticks (one for sent, two for delivered
/// or read) from the shared icon font: the Material `done` and
/// `done_all` glyphs, the same family as every other icon in
/// the app. Shared with the chat list, whose snippet paints the
/// same ticks for an outgoing last message. The span reserved
/// by the old hand-drawn ticks (7 pt per tick) is kept, so the
/// meta layouts around the call sites do not move.
pub(crate) fn paint_ticks(painter: &egui::Painter, x0: f32, y: f32, count: usize, color: Color32) {
    let icon = if count >= 2 { crate::icons::Icon::DoneAll } else { crate::icons::Icon::Done };
    let span = count as f32 * 7.0 + 0.5;
    crate::icons::draw(
        painter,
        icon,
        Pos2::new(x0 + span / 2.0, y + 3.5),
        crate::icons::SIZE_BADGE,
        color,
        color,
    );
}

/// Paints the small clock shown while a message is still
/// sending, in place of the ticks: the icon font's `schedule`
/// glyph, centred on the same point the hand-drawn clock used.
pub(crate) fn paint_clock(painter: &egui::Painter, center: Pos2, color: Color32) {
    crate::icons::draw(
        painter,
        crate::icons::Icon::Schedule,
        center,
        crate::icons::SIZE_BADGE,
        color,
        color,
    );
}

/// What a click on a bubble asked the app to do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum BubbleAction {
    /// Toggle voice-note playback for this message id.
    ToggleVoice(u64),
    /// Cycle the voice-note playback speed pill for this message id.
    CycleVoiceSpeed(u64),
    /// Seek this message's voice playback to a fraction (0.0 to 1.0)
    /// of its duration, starting playback there when no session is
    /// active for the message.
    SeekVoice(u64, f32),
    /// Open the view-once photo viewer for this message id.
    OpenViewOnce(u64),
    /// Open the regular photo viewer for this message id (albums
    /// and single photos the viewer can reopen freely).
    OpenPhoto(u64),
    /// Download this message's media to disk (documents, voice
    /// notes, photos received from the other side).
    Download(u64),
    /// Vote for poll option `usize` in the poll message `u64`.
    Vote(u64, usize),
    /// Secondary click: open the message context menu.
    ContextMenu(u64),
}

/// Playback snapshot for one voice message: how far through it the
/// player is, whether it is audibly playing right now, and the rate
/// chosen on the speed pill.
#[derive(Clone, Copy)]
pub(crate) struct VoiceSnap {
    /// Progress through the message, 0.0 to 1.0.
    pub(crate) progress: f32,
    /// True while playing; false while paused mid-message.
    pub(crate) playing: bool,
    /// Playback rate shown on the speed pill (1.0, 1.5 or 2.0).
    pub(crate) speed: f32,
    /// Seconds of the note the output has actually consumed,
    /// read from the playback engine (not derived from the
    /// declared duration).
    pub(crate) elapsed_secs: f32,
}

/// Media state a bubble needs from the app: the demo photo texture and
/// this message's voice playback snapshot.
#[derive(Clone, Copy, Default)]
pub(crate) struct MediaCtx {
    /// Demo photo texture id, once created.
    pub(crate) photo: Option<egui::TextureId>,
    /// This message's own photo texture id, when the photo came
    /// from disk and its pixels were decoded; wins over the demo
    /// photo.
    pub(crate) own_photo: Option<egui::TextureId>,
    /// What this message's photo tile may honestly show (real
    /// bytes, the demo fixture, or a fetch state). Decided by the
    /// app, identical for the bubble, the album grid and the
    /// viewer.
    pub(crate) photo_tile: crate::media::PhotoTile,
    /// What this message's video tile may honestly show (its own
    /// thumbnail or the neutral tile), decided by the app.
    pub(crate) video_tile: crate::media::VideoTile,
    /// This video's decoded thumbnail texture, when its wire
    /// thumbnail bytes are here: a real frame of the video.
    pub(crate) video_thumb: Option<egui::TextureId>,
    /// Playback snapshot for this voice message, when a playback
    /// session is active for it (playing or paused).
    pub(crate) voice: Option<VoiceSnap>,
    /// This voice message's fetch state, when its bytes were
    /// requested but no session could start: loading, or failed
    /// (the note is honestly unavailable until retried).
    pub(crate) voice_fetch: Option<crate::state::VoiceFetchState>,
    /// The group sender's profile photo, when the sender is a demo
    /// persona with a bundled portrait.
    pub(crate) sender_photo: Option<egui::TextureId>,
    /// The sender-name color assigned for this conversation (the
    /// stream assigns palette colors without replacement per
    /// conversation, so adjacent senders never collide); falls
    /// back to the standalone per-name color when unset.
    pub(crate) sender_color: Option<Color32>,
    /// The quoted sender's color from the same per-conversation
    /// assignment as `sender_color`.
    pub(crate) quoted_color: Option<Color32>,
    /// This view-once photo was already opened (receipt state).
    pub(crate) view_once_opened: bool,
    /// A save-to-disk download of this message's media is in
    /// flight: its download chip spins instead of offering the
    /// download again.
    pub(crate) downloading: bool,
}

/// Playback rates the speed pill cycles through, in order.
const VOICE_SPEEDS: [f32; 3] = [1.0, 1.5, 2.0];

/// The pill step after `current` (1x to 1.5x to 2x, then back
/// to 1x). Pure: the per-message rates themselves live in the
/// app's playback state (see `App::voice_speed`), this module
/// only owns the pill's geometry and labels.
pub(crate) fn next_voice_speed(current: f32) -> f32 {
    VOICE_SPEEDS.iter().copied().find(|s| *s > current + 0.01).unwrap_or(VOICE_SPEEDS[0])
}

/// The speed pill's label for a rate: "1x", "1.5x" or "2x".
fn speed_label(speed: f32) -> &'static str {
    if (speed - 1.5).abs() < 0.01 {
        "1.5x"
    } else if (speed - 2.0).abs() < 0.01 {
        "2x"
    } else {
        "1x"
    }
}

/// Voice note geometry, shared by the measure, paint and hit-test
/// passes so the speed pill's click target is exactly what is drawn.
/// The kind area is 48 px tall: play disc on the left, waveform and
/// speed pill on the upper row, duration under the waveform.
const VOICE_H: f32 = 48.0;
const VOICE_PILL_W: f32 = 42.0;
const VOICE_PILL_H: f32 = 22.0;

/// The speed pill's rect for a voice bubble whose content origin is
/// `(x, y)` with inner width `inner_w`.
fn voice_pill_rect(x: f32, y: f32, inner_w: f32) -> Rect {
    Rect::from_min_size(
        Pos2::new(x + inner_w - VOICE_PILL_W, y + 5.0),
        Vec2::new(VOICE_PILL_W, VOICE_PILL_H),
    )
}

/// The waveform strip's rect for a voice bubble whose content
/// origin is `(x, y)` with inner width `inner_w`: the bars start
/// right of the play disc and stop short of the speed pill while a
/// session is active, and short of the download chip while a
/// received note has no session, so the bars never run under
/// either control and read as clipped by the bubble's right edge.
/// Shared by the paint and hit-test passes so
/// seeking lands exactly on the bars the user sees. Vertically it
/// covers the bars (centred on `y + 16`) with a little slack.
fn voice_wave_rect(x: f32, y: f32, inner_w: f32, session: bool, download: bool) -> Rect {
    let reserve = if session {
        VOICE_PILL_W + 8.0
    } else if download {
        VOICE_DL + 8.0
    } else {
        0.0
    };
    let x1 = x + inner_w - reserve;
    Rect::from_min_max(Pos2::new(x + 52.0, y + 4.0), Pos2::new(x1, y + 28.0))
}

/// The seek fraction for a pointer x inside the waveform strip.
fn voice_seek_fraction(wave: Rect, px: f32) -> f32 {
    if wave.width() <= 0.0 {
        return 0.0;
    }
    ((px - wave.left()) / wave.width()).clamp(0.0, 1.0)
}

/// The download chip's rect for an incoming voice note's top-right
/// corner. Hidden while a playback session parks the speed pill
/// there; see the paint and hit-test passes.
const VOICE_DL: f32 = 26.0;

fn voice_dl_rect(x: f32, y: f32, inner_w: f32) -> Rect {
    Rect::from_min_size(Pos2::new(x + inner_w - VOICE_DL - 2.0, y + 2.0), Vec2::splat(VOICE_DL))
}

/// Document card geometry, shared by the measure, paint and
/// hit-test passes: icon disc + name/size column + optional
/// download chip.
const DOC_DISC: f32 = 38.0;
const DOC_GAP: f32 = 10.0;
const DOC_ROW_H: f32 = 46.0;
const DOC_DL: f32 = 34.0;

/// The download chip's slot width: received documents carry one,
/// sent documents do not.
fn doc_dl_w(mine: bool) -> f32 {
    if mine {
        0.0
    } else {
        DOC_DL + 4.0
    }
}

/// The download chip's rect for a document card with content
/// origin (x, y) and inner width `inner_w`.
fn doc_dl_rect(x: f32, y: f32, inner_w: f32) -> Rect {
    Rect::from_min_size(
        Pos2::new(x + inner_w - DOC_DL, y + (DOC_ROW_H - DOC_DL) / 2.0),
        Vec2::splat(DOC_DL),
    )
}

/// Formats a KiB count the way the reference wireframe reads:
/// whole KB under a megabyte ("470 KB"), one decimal above
/// ("1.2 MB").
fn human_size(size_kib: u32) -> String {
    if size_kib < 1024 {
        format!("{size_kib} KB")
    } else {
        format!("{:.1} MB", size_kib as f32 / 1024.0)
    }
}

/// Where this message sits in its sender group (consecutive
/// messages from the same sender, computed by the conversation):
/// grouping is carried by the run spacing the caller applies,
/// the sender name on the first message of an incoming group
/// run, and the sender avatar on its last. Bubbles carry no
/// tail; the radius is the same on every message.
#[derive(Clone, Copy)]
pub(crate) struct BubbleLayout {
    /// First message of its group: show the sender name (group
    /// chats, incoming messages).
    pub(crate) group_first: bool,
    /// Group chat, incoming message: reserve the sender-avatar
    /// column at the bubble's left, so every bubble of a run
    /// aligns on the same edge. The avatar disc itself paints
    /// only on the run's last bubble.
    pub(crate) avatar_column: bool,
    /// Paint the sender's avatar in the reserved column: the
    /// last message of an incoming group run.
    pub(crate) show_avatar: bool,
}

/// Width of the group sender-avatar column reserved at the left
/// of incoming group bubbles: a 26 px disc plus the gap to the
/// bubble.
const AVATAR_SLOT: f32 = 36.0;

/// Bubble corner radius: the rounded-modern geometry rounds
/// every bubble at 15 px on all sides, with no tail. The typing
/// indicator bubble in conversation.rs uses the same constant.
pub(crate) const BUBBLE_RADIUS: u8 = 15;

/// The bubble's share of the message-stream width.
const BUBBLE_SHARE: f32 = 0.72;

/// The widest a bubble may grow, however wide the stream gets.
const MAX_BUBBLE_W: f32 = 620.0;

/// Inner cards inside a bubble (the reply quote block, the link
/// embed, media placeholders) share one radius, stepped in from
/// the bubble radius by roughly the bubble's own padding.
pub(crate) const INNER_CARD_RADIUS: u8 = 10;

/// Inner-card text grammar: the quoted sender's name and the
/// embed's title are semibold at this size; their body text
/// (quoted text, embed description, poll options) is regular at
/// the same size, so every inner card reads as one system.
pub(crate) const INNER_TITLE_SIZE: f32 = 12.5;
pub(crate) const INNER_BODY_SIZE: f32 = 12.5;

/// Body-text line height as a multiple of the font size: the
/// airier reading rhythm of the rounded-modern bubble.
const BODY_LEADING: f32 = 1.5;

/// Height of the wide link-embed image band.
const EMBED_WIDE_IMG_H: f32 = 118.0;

/// Horizontal room an inner card reserves around its text: the
/// 3 px accent bar, the gap after it, and the right padding.
const QUOTE_INSET: f32 = 18.0;
const EMBED_INSET: f32 = 18.0;

/// Width reserved for a link embed's thumbnail image.
const EMBED_THUMB_W: f32 = 56.0;

/// The measured reply quote block: the ellipsized name and body
/// lines plus the block's natural width and painted height.
struct QuoteLayout {
    name: String,
    body: String,
    w: f32,
    h: f32,
}

/// The measured link embed card.
struct EmbedLayout {
    site: String,
    title: String,
    desc: String,
    text_h: f32,
    card_h: f32,
    natural_w: f32,
    thumb: bool,
    wide: bool,
}

/// The inner-card fill: the bubble's own fill stepped one shade
/// by the theme's pressed step (darker on dark fills, lighter
/// on light ones), always flat. Quote blocks, link embeds and
/// poll tracks all derive from the bubble this way, never from
/// a panel token, so an inner card belongs to its bubble in
/// both colour schemes.
pub(crate) fn inner_card_fill(t: Tokens, mine: bool) -> Color32 {
    rgb(core::theme::pressed_step(if mine { t.bubble_out } else { t.bubble_in }))
}

/// The site name shown on a link embed: the URL's host with any
/// "www." prefix dropped ("blog.example.com" from
/// "https://blog.example.com/guide"). Falls back to
/// the raw string when it has no host shape.
pub(crate) fn preview_site(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host = after_scheme.split('/').next().unwrap_or(after_scheme);
    host.strip_prefix("www.").unwrap_or(host).to_owned()
}

/// Truncates `text` to at most `max_lines` wrapped lines at
/// `wrap` width, ending the last line with an ellipsis when the
/// text had to be cut. Used by the quote block and the link
/// embed, whose blocks have a fixed line budget.
fn ellipsize_lines(ui: &egui::Ui, text: &str, font: FontId, wrap: f32, max_lines: usize) -> String {
    let rows = |s: &str| {
        ui.ctx()
            .fonts_mut(|f| f.layout(s.to_owned(), font.clone(), Color32::WHITE, wrap))
            .rows
            .len()
    };
    if rows(text) <= max_lines {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect::<String>() + "\u{2026}";
        if rows(&candidate) <= max_lines {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut out: String = chars[..lo].iter().collect();
    out.push('\u{2026}');
    out
}

/// Deterministic avatar seed for a group sender's name (the
/// wrapping byte fold the live community derivation also uses),
/// so a sender wears the same disc color in every group.
fn sender_seed(name: &str) -> u8 {
    name.bytes().fold(0u8, u8::wrapping_add)
}

/// The hover chevron's rect: a 24 px square fully inside the
/// bubble's top-right corner, with a 6 px margin from the right
/// edge and 4 px from the top, so its disc never straddles the
/// bubble edge. Shared by the paint and hit-test passes so the
/// button is exactly where it is drawn.
fn chevron_rect(rect: Rect) -> Rect {
    Rect::from_min_size(Pos2::new(rect.right() - 30.0, rect.top() + 4.0), Vec2::splat(24.0))
}

/// `c` at `f` of its own alpha (the chevron's fade-in).
fn faded(c: Color32, f: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(
        c.r(),
        c.g(),
        c.b(),
        (f32::from(c.a()) * f.clamp(0.0, 1.0)) as u8,
    )
}

/// Paints the link embed card at (x, y) with width `inner_w`, in
/// the shared inner-card grammar: the stepped fill, a 3 px
/// accent bar over the text block, the site line, the title and
/// the description. The preview image renders when the preview
/// carries one: a wide band above the text, or a thumbnail at
/// the card's right at text height. The bundled demo photo
/// stands in for the image; previews without image data render
/// the text card alone. (OpenGraph data itself arrives via the
/// existing preview machinery and its SSRF guard; nothing is
/// fetched here.)
#[allow(clippy::too_many_arguments)]
fn paint_embed(
    ui: &egui::Ui,
    painter: &egui::Painter,
    e: &EmbedLayout,
    media: MediaCtx,
    x: f32,
    y: f32,
    inner_w: f32,
    t: Tokens,
    mine: bool,
) {
    let card = Rect::from_min_size(Pos2::new(x, y), Vec2::new(inner_w, e.card_h));
    painter.rect_filled(card, CornerRadius::same(INNER_CARD_RADIUS), inner_card_fill(t, mine));
    let mut ty = y;
    if e.wide {
        let img = Rect::from_min_size(Pos2::new(x, y), Vec2::new(inner_w, EMBED_WIDE_IMG_H));
        painter.rect_filled(
            img,
            CornerRadius { nw: INNER_CARD_RADIUS, ne: INNER_CARD_RADIUS, se: 0, sw: 0 },
            rgb(t.bg_panel_alt),
        );
        if let Some(tex) = media.photo {
            painter.image(
                tex,
                img,
                Rect::from_min_max(Pos2::new(0.0, 0.30), Pos2::new(1.0, 0.70)),
                Color32::WHITE,
            );
        }
        ty += EMBED_WIDE_IMG_H;
    }
    painter.rect_filled(
        Rect::from_min_size(Pos2::new(x, ty), Vec2::new(3.0, e.text_h)),
        CornerRadius {
            nw: if e.wide { 0 } else { INNER_CARD_RADIUS },
            ne: 0,
            se: 0,
            sw: INNER_CARD_RADIUS,
        },
        rgb(t.accent),
    );
    let text_w = inner_w - EMBED_INSET - if e.thumb { EMBED_THUMB_W + 8.0 } else { 0.0 };
    let secondary = rgb(t.text_secondary);
    let site_g = layout_text(ui, &e.site, FontId::proportional(11.0), rgb(t.text_faint), text_w);
    let site_h = site_g.size().y;
    painter.galley(Pos2::new(x + 10.0, ty + 4.0), site_g, rgb(t.text_faint));
    let title_g = layout_text(
        ui,
        &e.title,
        crate::theme::semibold(INNER_TITLE_SIZE),
        rgb(t.text_primary),
        text_w,
    );
    let title_h = title_g.size().y;
    painter.galley(Pos2::new(x + 10.0, ty + 4.0 + site_h), title_g, rgb(t.text_primary));
    let desc_g = layout_text(ui, &e.desc, FontId::proportional(INNER_BODY_SIZE), secondary, text_w);
    painter.galley(Pos2::new(x + 10.0, ty + 4.0 + site_h + title_h), desc_g, secondary);
    if e.thumb {
        let side = (e.text_h - 8.0).clamp(24.0, 64.0);
        let thumb =
            Rect::from_min_size(Pos2::new(x + inner_w - side - 6.0, ty + 4.0), Vec2::splat(side));
        painter.rect_filled(thumb, CornerRadius::same(6), rgb(t.bg_panel_alt));
        if let Some(tex) = media.photo {
            painter.image(
                tex,
                thumb,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }
    }
}

/// Draws one message: the bubble hugs its content, rounds at
/// 15 px with no tail, and the time + ticks sit inside the
/// bubble at the bottom right. Painted manually because egui
/// frames always stretch to the available width, which is
/// exactly what made earlier versions look off. Returns an
/// action when the bubble was clicked (voice notes and
/// view-once photos are interactive).
pub(crate) fn message_bubble(
    ui: &mut egui::Ui,
    msg: &Message,
    chat: &Chat,
    t: Tokens,
    media: MediaCtx,
    layout: BubbleLayout,
) -> Option<BubbleAction> {
    if msg.kind == MessageKind::System {
        // The stream intercepts system messages before this
        // renderer, but any other caller gets the same
        // centered system pill, never a bubble-shaped frame.
        crate::conversation::system_pill(
            ui,
            t,
            &msg.body,
            rgb(t.text_secondary),
            false,
            crate::conversation::SystemPillKind::Event,
        );
        return None;
    }

    let mine = msg.is_mine();
    let pad_x = 9.0;
    // Vertical rhythm on the shared 4 px scale: bubbles breathe
    // a little more than the old 6/7 padding, which read cramped
    // next to other chat apps.
    let pad_top = 8.0;
    let pad_bottom = 8.0;
    // The group avatar column comes off the row before the
    // bubble's own width share is measured, so an indented run
    // never pushes the bubble past the usual cap.
    let avatar_slot = if layout.avatar_column && !mine { AVATAR_SLOT } else { 0.0 };
    let max_bubble = ((ui.available_width() - avatar_slot) * BUBBLE_SHARE).min(MAX_BUBBLE_W);
    let inner_max = max_bubble - pad_x * 2.0;

    let body_font = FontId::proportional(14.5);
    let small_font = FontId::proportional(11.0);
    let text_color = rgb(t.text_primary);
    let secondary = rgb(t.text_secondary);
    // WhatsApp mention color: light blue on dark, deeper blue on light.
    let mention_color =
        if t.is_light() { Color32::from_rgb(2, 115, 165) } else { rgb(t.tick_read) };

    // Grouping: the sender name belongs to the first message of
    // a sender group only, not to every bubble in the run.
    let show_sender =
        layout.group_first && !mine && chat.kind == ChatKind::Group && msg.sender.is_some();
    let sender_w = if show_sender {
        measure_text(ui, msg.sender.as_deref().unwrap_or(""), FontId::proportional(13.0))
    } else {
        0.0
    };
    // The reply quote block, in the shared inner-card grammar:
    // the quoted sender's name on one line (ellipsized) and the
    // quoted text on at most two lines (ellipsized), so the
    // block's height follows the lines it actually shows.
    let quote = match (&msg.quoted_sender, &msg.quoted_body) {
        (Some(qs), Some(qb)) => {
            let text_w = inner_max - QUOTE_INSET;
            let name =
                crate::widgets::ellipsize(ui, qs, crate::theme::semibold(INNER_TITLE_SIZE), text_w);
            let body = ellipsize_lines(ui, qb, FontId::proportional(INNER_BODY_SIZE), text_w, 2);
            let name_g = layout_text(
                ui,
                &name,
                crate::theme::semibold(INNER_TITLE_SIZE),
                text_color,
                text_w,
            );
            let body_g =
                layout_text(ui, &body, FontId::proportional(INNER_BODY_SIZE), secondary, text_w);
            Some(QuoteLayout {
                w: name_g.size().x.max(body_g.size().x) + QUOTE_INSET,
                h: name_g.size().y + body_g.size().y + 9.0,
                name,
                body,
            })
        }
        _ => None,
    };
    let quote_w = quote.as_ref().map_or(0.0, |q| q.w);
    let quote_h = quote.as_ref().map_or(0.0, |q| q.h + 6.0);

    // The link embed card, same grammar: a site line, the title
    // on one line, the description on at most two. A wide image
    // adds a band above the text; a thumbnail reserves its slot
    // at the card's right.
    let embed = msg.link_preview.as_ref().map(|p| {
        let thumb = p.image == core::model::PreviewImage::Thumb;
        let wide = p.image == core::model::PreviewImage::Wide;
        let mut text_w = inner_max - EMBED_INSET;
        if thumb {
            text_w -= EMBED_THUMB_W + 8.0;
        }
        let site = preview_site(&p.url);
        let title = crate::widgets::ellipsize(
            ui,
            &p.title,
            crate::theme::semibold(INNER_TITLE_SIZE),
            text_w,
        );
        let desc =
            ellipsize_lines(ui, &p.description, FontId::proportional(INNER_BODY_SIZE), text_w, 2);
        let site_g = layout_text(ui, &site, FontId::proportional(11.0), secondary, text_w);
        let title_g =
            layout_text(ui, &title, crate::theme::semibold(INNER_TITLE_SIZE), text_color, text_w);
        let desc_g =
            layout_text(ui, &desc, FontId::proportional(INNER_BODY_SIZE), secondary, text_w);
        let text_h = site_g.size().y + title_g.size().y + desc_g.size().y + 9.0;
        let mut natural_w =
            site_g.size().x.max(title_g.size().x).max(desc_g.size().x) + EMBED_INSET;
        if thumb {
            natural_w += EMBED_THUMB_W + 8.0;
        }
        EmbedLayout {
            card_h: text_h + if wide { EMBED_WIDE_IMG_H } else { 0.0 },
            natural_w: natural_w.min(inner_max),
            site,
            title,
            desc,
            text_h,
            thumb,
            wide,
        }
    });

    // Delivery status shows inline in every chat, groups included,
    // the way WhatsApp renders own-message ticks in group bubbles.
    let ticks = tick_count(msg.delivery);
    let sending = msg.delivery == Some(Delivery::Sending);
    // A revoked message renders as the standard placeholder;
    // its body is never painted.
    let body_text: &str =
        if msg.deleted && !msg.deleted_by_sender { "This message was deleted" } else { &msg.body };
    let failed = msg.delivery == Some(Delivery::Failed);
    let meta_text = if failed {
        format!("Not sent  {}", msg.time)
    } else if msg.edited {
        format!("edited  {}", msg.time)
    } else {
        msg.time.clone()
    };
    let meta_w = measure_text(ui, &meta_text, small_font.clone())
        + if msg.starred { 15.0 } else { 0.0 }
        + if sending {
            13.0
        } else if ticks == 0 {
            0.0
        } else {
            ticks as f32 * 7.0 + 8.0
        };

    let mut content_w = sender_w.max(quote_w);
    let body_galley;
    let kind_h;
    match &msg.kind {
        MessageKind::Text | MessageKind::System => {
            let est = crate::markdown::measure(
                ui,
                body_text,
                14.5,
                text_color,
                rgb(t.bg_panel_alt),
                mention_color,
            );
            body_galley = crate::markdown::galley_leading(
                ui,
                body_text,
                14.5,
                text_color,
                rgb(t.bg_panel_alt),
                mention_color,
                inner_max,
                Some(14.5 * BODY_LEADING),
            );
            content_w = content_w.max(body_galley.size().x.min(est.max(body_galley.size().x)));
            if let Some(e) = &embed {
                content_w = content_w.max(e.natural_w);
            }
            kind_h = body_galley.size().y + embed.as_ref().map_or(0.0, |e| e.card_h + 6.0);
        }
        MessageKind::Image { caption: _, view_once: true } => {
            body_galley = layout_text(ui, "", body_font.clone(), text_color, inner_max);
            content_w = content_w.max(170.0);
            kind_h = 34.0;
        }
        MessageKind::Image { caption, view_once: false } => {
            body_galley = crate::markdown::galley_leading(
                ui,
                caption,
                14.5,
                text_color,
                rgb(t.bg_panel_alt),
                mention_color,
                inner_max,
                Some(14.5 * BODY_LEADING),
            );
            content_w = content_w.max(300.0_f32.min(inner_max));
            kind_h = 200.0 + if caption.is_empty() { 0.0 } else { body_galley.size().y + 4.0 };
        }
        MessageKind::Video { caption, view_once, .. } => {
            if *view_once {
                body_galley = layout_text(ui, "", body_font.clone(), text_color, inner_max);
                content_w = content_w.max(170.0);
                kind_h = 34.0;
            } else {
                body_galley = crate::markdown::galley_leading(
                    ui,
                    caption,
                    14.5,
                    text_color,
                    rgb(t.bg_panel_alt),
                    mention_color,
                    inner_max,
                    Some(14.5 * BODY_LEADING),
                );
                content_w = content_w.max(300.0_f32.min(inner_max));
                kind_h = 200.0 + if caption.is_empty() { 0.0 } else { body_galley.size().y + 4.0 };
            }
        }
        MessageKind::Voice { .. } => {
            body_galley = layout_text(ui, "", body_font.clone(), text_color, inner_max);
            content_w = content_w.max(264.0_f32.min(inner_max));
            kind_h = VOICE_H;
        }
        MessageKind::Document { name, size_kib } => {
            // The reference wireframe's document card: an icon disc,
            // the file name over the human size, a download button
            // on received files, the caption below.
            let col_w = (inner_max - DOC_DISC - DOC_GAP - doc_dl_w(mine)).max(80.0);
            let name_g = layout_text(ui, name, crate::theme::semibold(14.0), text_color, col_w);
            let size_g = layout_text(
                ui,
                &human_size(*size_kib),
                FontId::proportional(12.5),
                secondary,
                col_w,
            );
            body_galley = layout_text(ui, &msg.body, body_font.clone(), text_color, inner_max);
            let row_w = DOC_DISC + DOC_GAP + name_g.size().x.max(size_g.size().x) + doc_dl_w(mine);
            content_w = content_w.max(row_w.max(220.0).min(inner_max)).max(body_galley.size().x);
            kind_h = DOC_ROW_H.max(name_g.size().y + size_g.size().y + 4.0)
                + if msg.body.is_empty() { 0.0 } else { body_galley.size().y + 4.0 };
        }
        MessageKind::Poll { question, options, .. } => {
            body_galley = layout_text(ui, question, body_font.clone(), text_color, inner_max);
            let mut w = body_galley.size().x;
            for (label, votes) in options {
                let ow = measure_text(ui, label, FontId::proportional(INNER_BODY_SIZE))
                    + measure_text(ui, &format!("{votes}"), FontId::proportional(12.0))
                    + 44.0;
                w = w.max(ow);
            }
            content_w = content_w.max(w.max(250.0));
            kind_h = body_galley.size().y + 8.0 + options.len() as f32 * 36.0;
        }
        MessageKind::Location { name, latitude, longitude } => {
            let title = if name.is_empty() { "Location" } else { name.as_str() };
            body_galley = layout_text(ui, title, body_font.clone(), text_color, inner_max);
            let coords = format!("{latitude:.5}, {longitude:.5}");
            let cw = measure_text(ui, &coords, FontId::proportional(12.0));
            // The paint pass offsets text by 34px (pin disc), the
            // measure must budget that offset plus slack, or a long
            // title wraps into the coordinates row.
            content_w = content_w.max((body_galley.size().x + 44.0).max(cw + 44.0));
            kind_h = 42.0;
        }
        MessageKind::Contact { name, phone } => {
            body_galley = layout_text(ui, name, body_font.clone(), text_color, inner_max);
            let pw = measure_text(ui, phone, FontId::proportional(12.5));
            content_w = content_w.max((body_galley.size().x + 52.0).max(pw + 52.0));
            kind_h = 42.0;
        }
        MessageKind::Carousel { body, cards } => {
            body_galley = crate::markdown::galley_leading(
                ui,
                body,
                14.5,
                text_color,
                rgb(t.bg_panel_alt),
                mention_color,
                inner_max,
                Some(14.5 * BODY_LEADING),
            );
            let row_w = (cards.len() as f32 * 158.0).min(316.0);
            content_w = content_w.max(body_galley.size().x.max(row_w));
            kind_h = body_galley.size().y + if cards.is_empty() { 0.0 } else { 132.0 };
        }
    }

    // WhatsApp puts the time inside the bubble's last text line when the
    // last line leaves room for it; otherwise the meta gets its own row.
    let last_line_w = body_galley.rows.last().map_or(0.0, |r| r.row.size.x);
    // Text bubbles keep the meta on the last line by widening the
    // bubble to fit it (WhatsApp's rule): a one-word message stays
    // one compact line instead of growing a second meta row.
    let meta_inline =
        matches!(msg.kind, MessageKind::Text) && last_line_w + meta_w + 14.0 <= inner_max;
    if meta_inline {
        content_w = content_w.max(last_line_w + meta_w + 14.0);
    } else {
        content_w = content_w.max(meta_w + 6.0);
    }
    let bubble_w = (content_w + pad_x * 2.0).clamp(84.0, max_bubble);
    let inner_w = bubble_w - pad_x * 2.0;

    let sender_h = if show_sender { 20.0 } else { 0.0 };
    let meta_h = if meta_inline { 3.0 } else { 16.0 };
    let bubble_h = pad_top + sender_h + quote_h + kind_h + meta_h + pad_bottom;

    let align_layout =
        if mine { Layout::right_to_left(Align::Min) } else { Layout::left_to_right(Align::Min) };
    let mut action = None;
    ui.with_layout(align_layout, |ui| {
        // Every bubble senses clicks: primary drives voice /
        // view-once / poll voting, secondary opens the context menu.
        // The allocation covers the avatar column too; the bubble
        // itself starts after it, and every paint/hit coordinate
        // below is relative to the bubble rect.
        let (alloc, response) =
            ui.allocate_exact_size(Vec2::new(avatar_slot + bubble_w, bubble_h), Sense::click());
        let rect = Rect::from_min_size(
            Pos2::new(alloc.left() + avatar_slot, alloc.top()),
            Vec2::new(bubble_w, bubble_h),
        );
        if response.secondary_clicked() && msg.kind != MessageKind::System {
            action = Some(BubbleAction::ContextMenu(msg.id));
        } else if response.clicked() {
            // The hover chevron in the bubble's top-right corner
            // opens the context menu, like the secondary click;
            // it wins over the kind-specific click targets.
            if response.interact_pointer_pos().is_some_and(|p| chevron_rect(rect).contains(p)) {
                action = Some(BubbleAction::ContextMenu(msg.id));
            }
            if action.is_none() {
                action = match msg.kind {
                    MessageKind::Voice { .. } => {
                        // A click on the speed pill cycles the rate; a
                        // click on the waveform seeks; a click on the
                        // download chip (received notes, no active
                        // session) saves the audio; a click anywhere
                        // else toggles playback.
                        let vx = rect.left() + pad_x;
                        let vy = rect.top() + pad_top + sender_h + quote_h;
                        let pos = response.interact_pointer_pos();
                        let on_pill = media.voice.is_some()
                            && pos.is_some_and(|p| voice_pill_rect(vx, vy, inner_w).contains(p));
                        let on_dl = !mine
                            && media.voice.is_none()
                            && pos.is_some_and(|p| voice_dl_rect(vx, vy, inner_w).contains(p));
                        let wave = voice_wave_rect(
                            vx,
                            vy,
                            inner_w,
                            media.voice.is_some(),
                            !mine && media.voice.is_none(),
                        );
                        if on_pill {
                            Some(BubbleAction::CycleVoiceSpeed(msg.id))
                        } else if on_dl {
                            Some(BubbleAction::Download(msg.id))
                        } else if let Some(p) = pos.filter(|p| wave.contains(*p)) {
                            Some(BubbleAction::SeekVoice(msg.id, voice_seek_fraction(wave, p.x)))
                        } else {
                            Some(BubbleAction::ToggleVoice(msg.id))
                        }
                    }
                    MessageKind::Document { .. } if !mine => {
                        response.interact_pointer_pos().and_then(|pos| {
                            let dx = rect.left() + pad_x;
                            let dy = rect.top() + pad_top + sender_h + quote_h;
                            doc_dl_rect(dx, dy, inner_w)
                                .contains(pos)
                                .then_some(BubbleAction::Download(msg.id))
                        })
                    }
                    MessageKind::Image { view_once: true, .. }
                    | MessageKind::Video { view_once: true, .. }
                        if !media.view_once_opened =>
                    {
                        Some(BubbleAction::OpenViewOnce(msg.id))
                    }
                    MessageKind::Image { view_once: false, .. } => {
                        Some(BubbleAction::OpenPhoto(msg.id))
                    }
                    MessageKind::Poll { ref options, .. } => {
                        // Rows start below the question galley; each row
                        // is 36px tall (matches the paint pass).
                        response.interact_pointer_pos().and_then(|pos| {
                            let rows_top = rect.top()
                                + pad_top
                                + sender_h
                                + quote_h
                                + body_galley.size().y
                                + 8.0;
                            let idx = ((pos.y - rows_top) / 36.0) as usize;
                            if pos.y >= rows_top && idx < options.len() {
                                Some(BubbleAction::Vote(msg.id, idx))
                            } else {
                                None
                            }
                        })
                    }
                    _ => None,
                };
            }
        }
        // The painter is cloned, not borrowed: the media
        // tiles below hand the ui to a child for the shared
        // loading spinner while the painter is still in use.
        // Call sites take a reference, so the clone is
        // immediately shadowed by one.
        let painter = ui.painter().clone();
        let painter = &painter;
        let bubble_fill = if mine { t.bubble_out } else { t.bubble_in };
        let bubble_color = rgb(bubble_fill);
        // Depth: the bubble shadow tier under the bubble, a
        // hairline edge mixed from the theme border, and the
        // raised top edge, so the flat fill still reads as a
        // surface sitting on the chat background instead of a
        // pasted rectangle.
        painter.add(egui::Shape::Rect(
            crate::theme::bubble_shadow(t).as_shape(rect, CornerRadius::same(BUBBLE_RADIUS)),
        ));
        painter.rect_filled(rect, CornerRadius::same(BUBBLE_RADIUS), bubble_color);
        painter.rect_stroke(
            rect,
            CornerRadius::same(BUBBLE_RADIUS),
            Stroke::new(1.0_f32, rgb(core::theme::mix(bubble_fill, t.border, 0.45))),
            egui::StrokeKind::Inside,
        );
        crate::theme::paint_raised_edge(painter, rect, BUBBLE_RADIUS as f32, bubble_fill, t);

        // Group sender avatar: the column is reserved for every
        // incoming bubble of the run, but the disc paints only
        // under the run's last bubble, bottom-aligned with it,
        // the way WhatsApp anchors the sender's avatar.
        if layout.show_avatar && !mine {
            let center = Pos2::new(alloc.left() + 14.0, rect.bottom() - 14.0);
            match (msg.sender.as_deref(), media.sender_photo) {
                (Some(_), Some(tex)) => {
                    crate::widgets::paint_photo_avatar(painter, center, 13.0, tex)
                }
                (Some(name), None) => crate::widgets::paint_initials_avatar(
                    painter,
                    center,
                    13.0,
                    name,
                    sender_seed(name),
                ),
                (None, _) => crate::widgets::paint_avatar(painter, center, 13.0, false, 0),
            }
        }

        let x = rect.left() + pad_x;
        let mut y = rect.top() + pad_top;

        if show_sender {
            let name = msg.sender.as_deref().unwrap_or("");
            let name_color = media.sender_color.unwrap_or_else(|| sender_color(name, t));
            let g = layout_text(ui, name, FontId::proportional(13.0), name_color, inner_w);
            painter.galley(Pos2::new(x, y), g, name_color);
            y += sender_h;
        }
        if let Some(q) = &quote {
            // The quote block in the shared inner-card grammar:
            // the stepped fill, a 3 px bar in the quoted sender's
            // color, their name in that color, then the quoted
            // text. Same for incoming and outgoing, DM and group.
            let qs_color = media
                .quoted_color
                .unwrap_or_else(|| sender_color(msg.quoted_sender.as_deref().unwrap_or(""), t));
            let block = Rect::from_min_size(Pos2::new(x, y), Vec2::new(inner_w, q.h));
            painter.rect_filled(
                block,
                CornerRadius::same(INNER_CARD_RADIUS),
                inner_card_fill(t, mine),
            );
            painter.rect_filled(
                Rect::from_min_size(block.min, Vec2::new(3.0, block.height())),
                CornerRadius { nw: INNER_CARD_RADIUS, ne: 0, se: 0, sw: INNER_CARD_RADIUS },
                qs_color,
            );
            let text_w = inner_w - QUOTE_INSET;
            let name_g = layout_text(
                ui,
                &q.name,
                crate::theme::semibold(INNER_TITLE_SIZE),
                qs_color,
                text_w,
            );
            let name_h = name_g.size().y;
            painter.galley(Pos2::new(x + 10.0, y + 4.0), name_g, qs_color);
            let body_g2 =
                layout_text(ui, &q.body, FontId::proportional(INNER_BODY_SIZE), secondary, text_w);
            painter.galley(Pos2::new(x + 10.0, y + 4.0 + name_h), body_g2, secondary);
            y += quote_h;
        }

        match &msg.kind {
            MessageKind::Text | MessageKind::System => {
                // The link embed rides above the body text, the
                // way Telegram stacks it: the card is the
                // message's headline, the text its caption.
                if let Some(e) = &embed {
                    paint_embed(ui, painter, e, media, x, y, inner_w, t, mine);
                    y += e.card_h + 6.0;
                }
                let g = crate::markdown::galley_leading(
                    ui,
                    body_text,
                    14.5,
                    text_color,
                    rgb(t.bg_panel_alt),
                    mention_color,
                    inner_w,
                    Some(14.5 * BODY_LEADING),
                );
                painter.galley(Pos2::new(x, y), g, text_color);
            }
            MessageKind::Image { caption: _, view_once: true } => {
                // View-once: just an icon + label until opened; after that
                // only the greyed "Opened" receipt remains.
                let opened = media.view_once_opened;
                let icon_color = if opened { rgb(t.text_faint) } else { rgb(t.accent) };
                let center = Pos2::new(x + 13.0, y + 15.0);
                crate::icons::draw(
                    painter,
                    crate::icons::Icon::LooksOne,
                    center,
                    crate::icons::SIZE_CONTROL,
                    icon_color,
                    icon_color,
                );
                let label = if opened { "Opened" } else { "Photo" };
                let label_color = if opened { rgb(t.text_faint) } else { text_color };
                let g =
                    layout_text(ui, label, FontId::proportional(14.0), label_color, inner_w - 34.0);
                painter.galley(Pos2::new(x + 32.0, y + 3.0), g, label_color);
                if !opened {
                    let g = layout_text(
                        ui,
                        "View once",
                        FontId::proportional(11.0),
                        rgb(t.text_faint),
                        inner_w - 34.0,
                    );
                    painter.galley(Pos2::new(x + 32.0, y + 21.0), g, rgb(t.text_faint));
                }
            }
            MessageKind::Image { caption, view_once: false } => {
                let img_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(inner_w, 200.0));
                // The tile paints the photo itself when its bytes
                // are here (or the demo fixture, where that is the
                // disclosed content). While the bytes are missing
                // it paints a neutral placeholder that is plainly
                // not a photo: a real live session never
                // substitutes another image for a received photo.
                let tex = match media.photo_tile {
                    crate::media::PhotoTile::Real => media.own_photo,
                    crate::media::PhotoTile::Demo => media.own_photo.or(media.photo),
                    _ => None,
                };
                if let Some(tex) = tex {
                    painter.image(
                        tex,
                        img_rect,
                        Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                } else {
                    painter.rect_filled(
                        img_rect,
                        CornerRadius::same(INNER_CARD_RADIUS),
                        rgb(t.bg_panel_alt),
                    );
                    let center = img_rect.center();
                    if media.photo_tile == crate::media::PhotoTile::Loading {
                        // A fetch in flight must read as waiting,
                        // never as the idle placeholder: the shared
                        // spinner sits in the tile instead of the
                        // glyph the idle tile wears.
                        let spinner_rect = Rect::from_center_size(center, Vec2::splat(20.0));
                        let mut slot = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(spinner_rect)
                                .layout(Layout::centered_and_justified(egui::Direction::TopDown)),
                        );
                        crate::widgets::spinner(&mut slot, 20.0);
                    } else {
                        crate::icons::draw(
                            painter,
                            crate::icons::Icon::Image,
                            Pos2::new(center.x, center.y - 8.0),
                            crate::icons::SIZE_LARGE,
                            rgb(t.text_faint),
                            rgb(t.bg_panel_alt),
                        );
                    }
                    if media.photo_tile == crate::media::PhotoTile::Failed {
                        let g = layout_text(
                            ui,
                            &crate::i18n::t("photo.unavailable"),
                            FontId::proportional(11.0),
                            rgb(t.text_faint),
                            inner_w - 16.0,
                        );
                        painter.galley(
                            Pos2::new(center.x - g.size().x / 2.0, center.y + 10.0),
                            g,
                            rgb(t.text_faint),
                        );
                    }
                }
                if !caption.is_empty() {
                    let g = crate::markdown::galley(
                        ui,
                        caption,
                        14.5,
                        text_color,
                        rgb(t.bg_panel_alt),
                        mention_color,
                        inner_w,
                    );
                    painter.galley(Pos2::new(x, y + 204.0), g, text_color);
                }
            }
            MessageKind::Video { caption, view_once, secs } => {
                if *view_once {
                    let opened = media.view_once_opened;
                    let icon_color = if opened { rgb(t.text_faint) } else { rgb(t.accent) };
                    let center = Pos2::new(x + 13.0, y + 15.0);
                    crate::icons::draw(
                        painter,
                        crate::icons::Icon::LooksOne,
                        center,
                        crate::icons::SIZE_CONTROL,
                        icon_color,
                        icon_color,
                    );
                    let label = if opened { "Opened" } else { "Video" };
                    let label_color = if opened { rgb(t.text_faint) } else { text_color };
                    let g = layout_text(
                        ui,
                        label,
                        FontId::proportional(14.0),
                        label_color,
                        inner_w - 34.0,
                    );
                    painter.galley(Pos2::new(x + 32.0, y + 3.0), g, label_color);
                    if !opened {
                        let g = layout_text(
                            ui,
                            "View once",
                            FontId::proportional(11.0),
                            rgb(t.text_faint),
                            inner_w - 34.0,
                        );
                        painter.galley(Pos2::new(x + 32.0, y + 21.0), g, rgb(t.text_faint));
                    }
                } else {
                    let img_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(inner_w, 200.0));
                    painter.rect_filled(
                        img_rect,
                        CornerRadius::same(INNER_CARD_RADIUS),
                        rgb(t.bg_panel_alt),
                    );
                    // The video's own thumbnail paints when its
                    // bytes are here: a real frame of the video,
                    // never a generated picture. Without one the
                    // neutral panel above is the presentation.
                    if media.video_tile == crate::media::VideoTile::Thumb {
                        if let Some(tex) = media.video_thumb {
                            painter.image(
                                tex,
                                img_rect,
                                Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        }
                    }
                    // Play glyph + duration chip, WhatsApp-style.
                    let c = img_rect.center();
                    painter.circle_filled(c, 22.0, rgb(t.accent));
                    crate::icons::draw(
                        painter,
                        crate::icons::Icon::Play,
                        c,
                        crate::icons::SIZE_CONTROL,
                        rgb(t.bg_panel),
                        rgb(t.accent),
                    );
                    let dur = format!("{}:{:02}", secs / 60, secs % 60);
                    let g = layout_text(ui, &dur, FontId::proportional(11.0), text_color, 60.0);
                    painter.galley(Pos2::new(x + 8.0, y + 176.0), g, rgb(t.text_secondary));
                    if !caption.is_empty() {
                        let g = crate::markdown::galley(
                            ui,
                            caption,
                            14.5,
                            text_color,
                            rgb(t.bg_panel_alt),
                            mention_color,
                            inner_w,
                        );
                        painter.galley(Pos2::new(x, y + 204.0), g, text_color);
                    }
                }
            }
            MessageKind::Voice { secs, waveform } => {
                // WhatsApp layout: a round play button on the left,
                // the bar waveform beside it with the played portion
                // in the accent and the rest faint, the duration
                // under the waveform, and a speed pill at the right
                // while a playback session is active.
                let snap = media.voice;
                let (progress, playing) = snap.map_or((0.0, false), |s| (s.progress, s.playing));
                let disc = Pos2::new(x + 20.0, y + 24.0);
                // A note whose bytes were requested but produced
                // no session (still fetching, or failed) wears a
                // muted disc instead of the accent one: it is not
                // playable right now, and the button must not
                // promise otherwise. Clicking it retries.
                let unavailable = if snap.is_none() { media.voice_fetch } else { None };
                match unavailable {
                    Some(state) => {
                        let fill = rgb(t.bg_panel_alt);
                        painter.circle_filled(disc, 19.0, fill);
                        if state == crate::state::VoiceFetchState::Loading {
                            // The muted disc alone reads as
                            // disabled, not as working: the shared
                            // spinner rides on the disc while the
                            // bytes are in flight, so the wait
                            // reads as a wait.
                            let disc_rect = Rect::from_center_size(disc, Vec2::splat(38.0));
                            let mut slot =
                                ui.new_child(egui::UiBuilder::new().max_rect(disc_rect).layout(
                                    Layout::centered_and_justified(egui::Direction::TopDown),
                                ));
                            crate::widgets::spinner(&mut slot, 17.0);
                        } else {
                            // Failed is the only other fetch state:
                            // the glyph swap to Info is its signal,
                            // and clicking the disc retries.
                            crate::icons::draw(
                                painter,
                                crate::icons::Icon::Info,
                                disc,
                                crate::icons::SIZE_CONTROL,
                                rgb(t.text_faint),
                                fill,
                            );
                        }
                    }
                    None => {
                        painter.circle_filled(disc, 19.0, rgb(t.accent));
                        crate::icons::draw(
                            painter,
                            if playing {
                                crate::icons::Icon::Pause
                            } else {
                                crate::icons::Icon::Play
                            },
                            disc,
                            crate::icons::SIZE_CONTROL,
                            rgb(core::theme::on_color(t.accent)),
                            bubble_color,
                        );
                    }
                }
                let wave = voice_wave_rect(x, y, inner_w, snap.is_some(), !mine && snap.is_none());
                let bar_x0 = wave.left();
                let bar_x1 = wave.right();
                let bar_cy = wave.center().y;
                let n = waveform.len().max(1);
                let pitch = ((bar_x1 - bar_x0) / n as f32).max(1.0);
                let bar_w = (pitch * 0.55).clamp(1.8, 3.2);
                for (i, h) in waveform.iter().enumerate() {
                    let bx = bar_x0 + (i as f32 + 0.5) * pitch;
                    let bh = (h * 22.0).max(3.0);
                    let played = (i as f32 + 0.5) / n as f32 <= progress;
                    let color = if played { rgb(t.accent) } else { rgb(t.text_faint) };
                    painter.rect_filled(
                        Rect::from_min_size(
                            Pos2::new(bx - bar_w / 2.0, bar_cy - bh / 2.0),
                            Vec2::new(bar_w, bh),
                        ),
                        CornerRadius::same(1),
                        color,
                    );
                }
                // The waveform is the scrubber: clicks on it are
                // dispatched by position with the bubble's click
                // handling above, and this strip adds drag-to-seek
                // plus a hover playhead line. It senses only the
                // strip, so drags starting anywhere else on the
                // bubble still scroll the stream.
                let wave_resp = ui
                    .interact(
                        wave,
                        egui::Id::unique(("zaprust_voice_wave", msg.id)),
                        egui::Sense::click_and_drag(),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if wave_resp.hovered() || wave_resp.dragged() {
                    if let Some(p) = wave_resp.interact_pointer_pos() {
                        let lx = p.x.clamp(wave.left(), wave.right());
                        painter.line_segment(
                            [Pos2::new(lx, wave.top() + 1.0), Pos2::new(lx, wave.bottom() - 1.0)],
                            Stroke::new(1.0_f32, rgb(t.text_primary)),
                        );
                    }
                }
                if wave_resp.clicked() || wave_resp.dragged() {
                    if let Some(p) = wave_resp.interact_pointer_pos() {
                        action =
                            Some(BubbleAction::SeekVoice(msg.id, voice_seek_fraction(wave, p.x)));
                    }
                }
                // Elapsed time while a session is active (the
                // engine's own consumed position), the total
                // duration otherwise; both as m:ss. Received notes
                // wear a small download chip in the top-right corner
                // while no playback session occupies it (the speed
                // pill parks there when one is).
                let shown_secs = match snap {
                    Some(s) => s.elapsed_secs.round() as u32,
                    None => *secs,
                };
                let dur = format!("{}:{:02}", shown_secs / 60, shown_secs % 60);
                let g = layout_text(ui, &dur, small_font.clone(), secondary, 48.0);
                painter.galley(Pos2::new(bar_x0, y + 33.0), g, secondary);
                if !mine && snap.is_none() {
                    let dl = voice_dl_rect(x, y, inner_w);
                    let resp = ui
                        .interact(dl, egui::Id::unique(("voice_dl", msg.id)), egui::Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    // The disc fill derives from the bubble fill
                    // (the inner-card step), not the panel alt:
                    // on an incoming bubble the panel alt is the
                    // bubble's own color, which made the disc
                    // invisible and the glyph read as clipped
                    // against the bubble's right edge.
                    let dl_base = core::theme::pressed_step(t.bubble_in);
                    let fill = if resp.is_pointer_button_down_on() {
                        rgb(core::theme::pressed_step(dl_base))
                    } else if resp.hovered() {
                        rgb(core::theme::hover_step(dl_base))
                    } else {
                        rgb(dl_base)
                    };
                    painter.circle_filled(dl.center(), VOICE_DL / 2.0, fill);
                    if media.downloading {
                        // The download is in flight: the chip
                        // spins (the shared spinner at the voice
                        // disc size) instead of offering the same
                        // download again.
                        let slot_rect = Rect::from_center_size(dl.center(), Vec2::splat(VOICE_DL));
                        let mut slot = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(slot_rect)
                                .layout(Layout::centered_and_justified(egui::Direction::TopDown)),
                        );
                        crate::widgets::spinner(&mut slot, 17.0);
                    } else {
                        crate::icons::draw(
                            painter,
                            crate::icons::Icon::Download,
                            dl.center(),
                            crate::icons::SIZE_ROW,
                            rgb(t.accent),
                            fill,
                        );
                    }
                }
                if let Some(s) = snap {
                    let prect = voice_pill_rect(x, y, inner_w);
                    painter.rect_filled(prect, CornerRadius::same(11), inner_card_fill(t, mine));
                    let label = speed_label(s.speed);
                    let g = layout_text(
                        ui,
                        label,
                        FontId::proportional(11.5),
                        text_color,
                        VOICE_PILL_W,
                    );
                    painter.galley(
                        Pos2::new(
                            prect.center().x - g.size().x / 2.0,
                            prect.center().y - g.size().y / 2.0,
                        ),
                        g,
                        text_color,
                    );
                }
            }
            MessageKind::Document { name, size_kib } => {
                // The document card: icon disc, name over the human
                // size, download chip on received files, caption
                // below. Geometry matches the measure pass.
                let disc = Rect::from_min_size(
                    Pos2::new(x, y + (DOC_ROW_H - DOC_DISC) / 2.0),
                    Vec2::splat(DOC_DISC),
                );
                painter.rect_filled(disc, CornerRadius::same(10), inner_card_fill(t, mine));
                crate::icons::draw(
                    painter,
                    crate::icons::Icon::Document,
                    disc.center(),
                    crate::icons::SIZE_CONTROL,
                    rgb(t.accent),
                    rgb(t.bg_panel_alt),
                );
                let col_x = x + DOC_DISC + DOC_GAP;
                let col_w = (inner_w - DOC_DISC - DOC_GAP - doc_dl_w(mine)).max(60.0);
                let name_g = layout_text(ui, name, crate::theme::semibold(14.0), text_color, col_w);
                painter.galley(Pos2::new(col_x, y + 5.0), name_g.clone(), text_color);
                let size_g = layout_text(
                    ui,
                    &human_size(*size_kib),
                    FontId::proportional(12.5),
                    secondary,
                    col_w,
                );
                painter.galley(
                    Pos2::new(col_x, y + 5.0 + name_g.size().y + 1.0),
                    size_g,
                    secondary,
                );
                if !mine {
                    let dl = doc_dl_rect(x, y, inner_w);
                    let resp = ui
                        .interact(dl, egui::Id::unique(("doc_dl", msg.id)), egui::Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    let fill = if resp.is_pointer_button_down_on() {
                        rgb(core::theme::pressed_step(t.bg_panel_alt))
                    } else if resp.hovered() {
                        rgb(core::theme::hover_step(t.bg_panel_alt))
                    } else {
                        rgb(t.bg_panel_alt)
                    };
                    painter.circle_filled(dl.center(), DOC_DL / 2.0, fill);
                    if media.downloading {
                        // In flight: the shared spinner, same as
                        // the voice chip.
                        let slot_rect = Rect::from_center_size(dl.center(), Vec2::splat(DOC_DL));
                        let mut slot = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(slot_rect)
                                .layout(Layout::centered_and_justified(egui::Direction::TopDown)),
                        );
                        crate::widgets::spinner(&mut slot, 17.0);
                    } else {
                        crate::icons::draw(
                            painter,
                            crate::icons::Icon::Download,
                            dl.center(),
                            crate::icons::SIZE_ROW,
                            rgb(t.accent),
                            fill,
                        );
                    }
                }
                if !msg.body.is_empty() {
                    let g = layout_text(ui, &msg.body, body_font.clone(), text_color, inner_w);
                    painter.galley(Pos2::new(x, y + DOC_ROW_H + 4.0), g, text_color);
                }
            }
            MessageKind::Poll { question, options, my_votes, .. } => {
                let g = layout_text(ui, question, body_font.clone(), text_color, inner_w);
                painter.galley(Pos2::new(x, y), g, text_color);
                let mut oy = y + body_galley.size().y + 8.0;
                let total: u32 = options.iter().map(|o| o.1).sum::<u32>().max(1);
                for (opt_idx, (label, votes)) in options.iter().enumerate() {
                    painter.rect_stroke(
                        Rect::from_min_size(Pos2::new(x, oy + 2.0), Vec2::splat(13.0)),
                        CornerRadius::same(3),
                        Stroke::new(1.2_f32, secondary),
                        egui::StrokeKind::Inside,
                    );
                    // An option the user voted for carries the
                    // icon font's check inside its box, the same
                    // mark the rest of the app uses.
                    if my_votes.contains(&opt_idx) {
                        crate::icons::draw(
                            painter,
                            crate::icons::Icon::Check,
                            Pos2::new(x + 6.5, oy + 8.5),
                            crate::icons::SIZE_BADGE,
                            rgb(t.accent),
                            bubble_color,
                        );
                    }
                    let g = layout_text(
                        ui,
                        label,
                        FontId::proportional(INNER_BODY_SIZE),
                        text_color,
                        inner_w - 60.0,
                    );
                    painter.galley(Pos2::new(x + 21.0, oy), g, text_color);
                    let vt = format!("{votes}");
                    let vw = measure_text(ui, &vt, FontId::proportional(12.0));
                    let g = layout_text(ui, &vt, FontId::proportional(12.0), secondary, 40.0);
                    painter.galley(Pos2::new(x + inner_w - vw, oy + 1.0), g, secondary);
                    let bar = Rect::from_min_size(
                        Pos2::new(x + 21.0, oy + 22.0),
                        Vec2::new(inner_w - 21.0, 4.0),
                    );
                    painter.rect_filled(bar, CornerRadius::same(2), inner_card_fill(t, mine));
                    let mut fill = bar;
                    fill.set_width(bar.width() * (*votes as f32 / total as f32));
                    painter.rect_filled(fill, CornerRadius::same(2), rgb(t.accent));
                    oy += 36.0;
                }
            }
            MessageKind::Location { name, latitude, longitude } => {
                // Render-only location card: pin + name + coordinates.
                let cy = y + 21.0;
                painter.circle_filled(Pos2::new(x + 15.0, cy), 13.0, rgb(t.bg_panel_alt));
                crate::icons::draw(
                    painter,
                    crate::icons::Icon::LocationOn,
                    Pos2::new(x + 15.0, cy),
                    crate::icons::SIZE_MENU,
                    text_color,
                    rgb(t.bg_panel_alt),
                );
                let title = if name.is_empty() { "Location" } else { name.as_str() };
                let g = layout_text(ui, title, body_font.clone(), text_color, inner_w - 34.0);
                painter.galley(Pos2::new(x + 34.0, y + 2.0), g, text_color);
                let coords = format!("{latitude:.5}, {longitude:.5}");
                let g =
                    layout_text(ui, &coords, FontId::proportional(12.0), secondary, inner_w - 34.0);
                painter.galley(Pos2::new(x + 34.0, y + 22.0), g, secondary);
            }
            MessageKind::Contact { name, phone } => {
                // Render-only contact card: initial disc + name + number.
                let cy = y + 21.0;
                painter.circle_filled(Pos2::new(x + 15.0, cy), 13.0, rgb(t.accent));
                let initial = name.chars().next().unwrap_or('?').to_uppercase().to_string();
                let g =
                    layout_text(ui, &initial, FontId::proportional(13.0), rgb(t.bg_panel), 26.0);
                painter.galley(Pos2::new(x + 10.0, cy - 8.0), g, rgb(t.bg_panel));
                let g = layout_text(ui, name, body_font.clone(), text_color, inner_w - 40.0);
                painter.galley(Pos2::new(x + 36.0, y + 2.0), g, text_color);
                let g =
                    layout_text(ui, phone, FontId::proportional(12.5), secondary, inner_w - 40.0);
                painter.galley(Pos2::new(x + 36.0, y + 22.0), g, secondary);
            }
            MessageKind::Carousel { body, cards } => {
                // Render-only carousel: body text, then a row of
                // cards (title, body, up to two static button
                // labels). Business-template buttons are inert,
                // the protocol actions are not safe to synthesize.
                let g = crate::markdown::galley(
                    ui,
                    body,
                    14.5,
                    text_color,
                    rgb(t.bg_panel_alt),
                    mention_color,
                    inner_w,
                );
                let body_h = g.size().y;
                painter.galley(Pos2::new(x, y), g, text_color);
                let mut cx = x;
                let cy = y + body_h + 6.0;
                for card in cards.iter().take(2) {
                    let card_rect = Rect::from_min_size(Pos2::new(cx, cy), Vec2::new(150.0, 118.0));
                    painter.rect_filled(
                        card_rect,
                        CornerRadius::same(INNER_CARD_RADIUS),
                        rgb(t.bg_panel_alt),
                    );
                    painter.rect_stroke(
                        card_rect,
                        CornerRadius::same(INNER_CARD_RADIUS),
                        Stroke::new(1.0_f32, rgb(t.border)),
                        egui::StrokeKind::Inside,
                    );
                    if !card.title.is_empty() {
                        let g = layout_text(
                            ui,
                            &card.title,
                            FontId::proportional(13.0),
                            text_color,
                            134.0,
                        );
                        painter.galley(Pos2::new(cx + 8.0, cy + 7.0), g, text_color);
                    }
                    if !card.body.is_empty() {
                        let g = layout_text(
                            ui,
                            &card.body,
                            FontId::proportional(12.0),
                            secondary,
                            134.0,
                        );
                        painter.galley(Pos2::new(cx + 8.0, cy + 27.0), g, secondary);
                    }
                    // Template buttons cannot be tapped (their
                    // actions are not safe to synthesize), so they
                    // render as muted outlined labels, never in the
                    // accent color the app reserves for real actions.
                    let mut by = cy + 118.0 - 34.0;
                    for button in card.buttons.iter().take(2) {
                        let g = layout_text(
                            ui,
                            button,
                            FontId::proportional(11.5),
                            rgb(t.text_secondary),
                            118.0,
                        );
                        let chip =
                            Rect::from_min_size(Pos2::new(cx + 8.0, by), Vec2::new(134.0, 15.0));
                        painter.rect_stroke(
                            chip,
                            CornerRadius::same(4),
                            Stroke::new(1.0_f32, rgb(t.border)),
                            egui::StrokeKind::Inside,
                        );
                        painter.galley(Pos2::new(cx + 12.0, by + 1.0), g, rgb(t.text_secondary));
                        by += 17.0;
                    }
                    cx += 158.0;
                }
            }
        }

        let meta_y = rect.bottom() - pad_bottom - 12.0;
        let tick_color =
            if msg.delivery == Some(Delivery::Read) { rgb(t.tick_read) } else { rgb(t.text_faint) };
        let meta_color = if failed { rgb(t.danger) } else { rgb(t.text_faint) };
        let mut mx = rect.right() - pad_x - meta_w;
        if msg.starred {
            crate::icons::draw(
                painter,
                crate::icons::Icon::Star,
                Pos2::new(mx + 5.5, meta_y + 6.0),
                crate::icons::SIZE_BADGE,
                rgb(t.text_faint),
                bubble_color,
            );
            mx += 15.0;
        }
        let g = layout_text(ui, &meta_text, small_font.clone(), meta_color, meta_w + 4.0);
        painter.galley(Pos2::new(mx, meta_y), g, meta_color);
        mx += measure_text(ui, &meta_text, small_font.clone()) + 4.0;
        if ticks > 0 {
            paint_ticks(painter, mx, meta_y + 3.0, ticks, tick_color);
        } else if sending {
            paint_clock(painter, Pos2::new(mx + 4.2, meta_y + 6.0), rgb(t.text_faint));
        }

        if !msg.reactions.is_empty() {
            let mut rx = rect.left() + 8.0;
            let ry = rect.bottom() - 9.0;
            for (emoji, count) in &msg.reactions {
                let label = format!("{emoji} {count}");
                let w = measure_text(ui, &label, small_font.clone()) + 13.0;
                let pill = Rect::from_min_size(Pos2::new(rx, ry), Vec2::new(w, 18.0));
                painter.rect_filled(pill, CornerRadius::same(9), rgb(t.bg_panel_alt));
                painter.rect_stroke(
                    pill,
                    CornerRadius::same(9),
                    Stroke::new(1.0_f32, rgb(t.border)),
                    egui::StrokeKind::Inside,
                );
                let g = layout_text(ui, &label, small_font.clone(), text_color, w);
                painter.galley(Pos2::new(rx + 6.5, ry + 3.0), g, text_color);
                rx += w + 5.0;
            }
        }

        // Hover chevron: a small disc that fades in at the
        // bubble's top-right corner while the pointer is over
        // the bubble; clicking it opens the context menu (the
        // dispatch sits with the click handling above). The
        // chevron is the icon font's glyph, the same one the
        // scroll-to-bottom button draws.
        let chev = crate::anim::hover_factor(ui, ("bubble_chevron", msg.id), response.hovered());
        if chev > 0.01 {
            let cr = chevron_rect(rect);
            let center = cr.center();
            painter.circle_filled(center, 12.0, faded(rgb(t.bg_panel_alt), chev * 0.95));
            crate::icons::draw(
                painter,
                crate::icons::Icon::ChevronDown,
                center,
                crate::icons::SIZE_ROW,
                faded(rgb(t.text_secondary), chev),
                faded(rgb(t.bg_panel_alt), chev),
            );
            if response.interact_pointer_pos().is_some_and(|p| cr.contains(p)) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
    });
    action
}

/// Album geometry: two columns of tiles in one card, at most four
/// tiles shown, the rest behind a "+N" overlay on the last tile.
const ALBUM_TILE_W: f32 = 166.0;
const ALBUM_TILE_H: f32 = 124.0;
const ALBUM_GAP: f32 = 2.0;
const ALBUM_MAX_TILES: usize = 4;

/// Paints an album: consecutive photos/videos from one sender as a
/// single grid card, WhatsApp-style. Every shown tile opens its own
/// photo in the viewer; the first caption rides under the grid.
/// Reactions stay per message: the stream still draws the strips
/// for each item after the card.
pub(crate) fn message_album(
    ui: &mut egui::Ui,
    items: &[Message],
    t: Tokens,
    textures: &crate::extras::TextureCache<u64>,
    video_textures: &crate::extras::TextureCache<u64>,
    demo: Option<egui::TextureId>,
) -> Vec<BubbleAction> {
    let mut actions = Vec::new();
    let mine = items.first().is_some_and(|m| m.is_mine());
    let shown = items.len().min(ALBUM_MAX_TILES);
    let rows = shown.div_ceil(2);
    let grid_w = ALBUM_TILE_W * 2.0 + ALBUM_GAP;
    let grid_h = rows as f32 * ALBUM_TILE_H + (rows.saturating_sub(1)) as f32 * ALBUM_GAP;
    let caption = items.iter().find_map(|m| match &m.kind {
        MessageKind::Image { caption, .. } if !caption.is_empty() => Some(caption.clone()),
        _ => None,
    });
    let caption_galley = caption.as_ref().map(|c| {
        layout_text(ui, c, eframe::egui::FontId::proportional(13.5), rgb(t.text_primary), grid_w)
    });
    let card_h = grid_h + caption_galley.as_ref().map_or(0.0, |g| g.size().y + 8.0);
    let (alloc, _) = ui.allocate_exact_size(Vec2::new(grid_w, card_h), egui::Sense::hover());
    let painter = ui.painter();
    let card = alloc;
    let bubble_fill = if mine { t.bubble_out } else { t.bubble_in };
    let bubble_color = rgb(bubble_fill);
    painter
        .add(crate::theme::bubble_shadow(t).as_shape(card, eframe::egui::CornerRadius::same(14)));
    painter.rect_filled(card, eframe::egui::CornerRadius::same(14), bubble_color);
    painter.rect_stroke(
        card,
        eframe::egui::CornerRadius::same(14),
        Stroke::new(1.0_f32, rgb(core::theme::mix(bubble_fill, t.border, 0.45))),
        egui::StrokeKind::Inside,
    );
    crate::theme::paint_raised_edge(painter, card, 14.0, bubble_fill, t);
    for (i, item) in items.iter().take(shown).enumerate() {
        let row = (i / 2) as f32;
        let col = (i % 2) as f32;
        let rect = eframe::egui::Rect::from_min_size(
            Pos2::new(
                card.left() + col * (ALBUM_TILE_W + ALBUM_GAP),
                card.top() + row * (ALBUM_TILE_H + ALBUM_GAP),
            ),
            Vec2::new(ALBUM_TILE_W, ALBUM_TILE_H),
        );
        // A video cell paints its own thumbnail or the neutral
        // placeholder: the demo photo and the photo texture cache
        // belong to photos, and a video must never borrow them.
        let tex = if matches!(item.kind, MessageKind::Video { .. }) {
            video_textures.get(&item.id).map(egui::TextureHandle::id)
        } else {
            textures.get(&item.id).map(egui::TextureHandle::id).or(demo)
        };
        if let Some(tex) = tex {
            painter.image(
                tex,
                rect,
                Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        } else {
            painter.rect_filled(rect, eframe::egui::CornerRadius::ZERO, rgb(t.bg_panel_alt));
        }
        // Videos wear the play disc; the "+N" overlay caps the grid.
        if let MessageKind::Video { .. } = &item.kind {
            let c = rect.center();
            painter.circle_filled(c, 14.0, rgb(t.accent));
            crate::icons::draw(
                painter,
                crate::icons::Icon::Play,
                c,
                crate::icons::SIZE_ROW,
                rgb(t.bg_panel),
                rgb(t.accent),
            );
        }
        if i == shown - 1 && items.len() > shown {
            painter.rect_filled(
                rect,
                eframe::egui::CornerRadius::ZERO,
                Color32::from_rgba_unmultiplied(10, 12, 14, 150),
            );
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("+{}", items.len() - shown),
                eframe::egui::FontId::proportional(20.0),
                Color32::WHITE,
            );
        }
        // Hairline between tiles keeps the grid legible when the
        // photos are bright.
        painter.rect_stroke(
            rect,
            eframe::egui::CornerRadius::ZERO,
            Stroke::new(0.5_f32, rgb(core::theme::mix(t.border, t.bg_chat, 0.4))),
            egui::StrokeKind::Inside,
        );
        let resp = ui
            .interact(rect, egui::Id::unique(("album_tile", item.id)), egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if resp.clicked() {
            actions.push(BubbleAction::OpenPhoto(item.id));
        }
    }
    if let (Some(caption), Some(g)) = (&caption, &caption_galley) {
        painter.galley(
            Pos2::new(card.left() + 8.0, card.top() + grid_h + 4.0),
            g.clone(),
            rgb(t.text_primary),
        );
        let _ = caption;
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::{chevron_rect, human_size, preview_site, voice_wave_rect, VOICE_DL, VOICE_PILL_W};
    use eframe::egui::{Pos2, Rect, Vec2};

    #[test]
    fn chevron_sits_fully_inside_the_bubble() {
        let bubble = Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(220.0, 64.0));
        let chev = chevron_rect(bubble);
        assert!(chev.left() >= bubble.left());
        assert!(chev.top() >= bubble.top());
        assert!(chev.right() <= bubble.right());
        assert!(chev.bottom() <= bubble.bottom());
        // The disc painted on the rect (radius 12 around its
        // centre) also stays inside, with margin to spare.
        assert!(bubble.right() - chev.right() >= 6.0);
        assert!(chev.top() - bubble.top() >= 4.0);
    }

    #[test]
    fn voice_waveform_reserves_the_download_and_speed_slots() {
        let plain = voice_wave_rect(0.0, 0.0, 264.0, false, false);
        assert_eq!(plain.right(), 264.0);
        let with_dl = voice_wave_rect(0.0, 0.0, 264.0, false, true);
        assert_eq!(with_dl.right(), 264.0 - VOICE_DL - 8.0);
        let with_pill = voice_wave_rect(0.0, 0.0, 264.0, true, false);
        assert_eq!(with_pill.right(), 264.0 - VOICE_PILL_W - 8.0);
    }

    #[test]
    fn preview_site_takes_the_host_and_drops_www() {
        assert_eq!(preview_site("https://blog.example.com/travel-guide"), "blog.example.com");
        assert_eq!(preview_site("https://www.example.com/a/b"), "example.com");
        assert_eq!(preview_site("example.com/page"), "example.com");
        assert_eq!(preview_site(""), "");
    }

    #[test]
    fn human_size_reads_like_the_reference() {
        assert_eq!(human_size(470), "470 KB");
        assert_eq!(human_size(1023), "1023 KB");
        assert_eq!(human_size(1024), "1.0 MB");
        assert_eq!(human_size(1229), "1.2 MB");
    }
}
