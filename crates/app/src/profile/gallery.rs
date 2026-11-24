//! The profile dock's gallery (design standard section 25):
//! the open chat's media, links and documents, collected from
//! the conversation by the pure core derivations beside the
//! counts that advertise them. Every item acts through the
//! path its kind already owns: a photo opens the photo
//! viewer, a video, sticker, carousel or document locates its
//! message in the stream (its own player and download live
//! there), and a link opens through the system opener. Like
//! the dock bodies, this is a free function over state pieces;
//! the clicks it gathers land in [`GalleryActions`] and the
//! panel applies them after the draw.

use crate::icons::Icon;
use crate::state::GallerySection;
use crate::theme::{SP_2, rgb};
use core::model::{Message, MessageKind};
use core::theme::Tokens;
use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, RichText, Sense, Vec2};

/// Clicks gathered while the gallery renders, applied by the
/// panel after the draw.
#[derive(Default)]
pub(super) struct GalleryActions {
    /// Back to the profile body.
    pub back: bool,
    /// Switch to this gallery section.
    pub section: Option<GallerySection>,
    /// Open this photo in the viewer.
    pub view_photo: Option<u64>,
    /// Locate this message in the stream.
    pub locate: Option<u64>,
    /// Start the display fetch for these photo messages.
    pub fetch_photos: Vec<u64>,
    /// Start the display fetch for these sticker messages.
    pub fetch_stickers: Vec<u64>,
}

/// Paints a texture into a cell with cover cropping (the cell
/// is filled; the overflow is cropped symmetrically), the
/// gallery grid's one image treatment.
fn paint_cover(painter: &egui::Painter, tex: egui::TextureId, rect: Rect, tex_aspect: f32) {
    let cell_aspect = rect.width() / rect.height().max(1.0);
    let uv = if tex_aspect > cell_aspect {
        let w = cell_aspect / tex_aspect;
        let x0 = (1.0 - w) / 2.0;
        Rect::from_min_max(Pos2::new(x0, 0.0), Pos2::new(x0 + w, 1.0))
    } else {
        let h = tex_aspect / cell_aspect.max(0.01);
        let y0 = (1.0 - h) / 2.0;
        Rect::from_min_max(Pos2::new(0.0, y0), Pos2::new(1.0, y0 + h))
    };
    painter.image(tex, rect, uv, Color32::WHITE);
}

/// The URL's host for a link row's title line: the scheme and
/// path fall away, leaving the site the link leads to.
fn link_host(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.split('/').next().unwrap_or(rest).to_owned()
}

/// A document's size line, in the units the document bubble
/// uses: bytes below 1 KiB, KiB below a MiB, one-decimal MiB
/// above it.
fn doc_size(size_bytes: u64) -> String {
    if size_bytes < 1024 {
        format!("{size_bytes} B")
    } else if size_bytes < 1024 * 1024 {
        format!("{} KB", size_bytes / 1024)
    } else {
        format!("{:.1} MB", size_bytes as f64 / (1024.0 * 1024.0))
    }
}

/// The gallery body: back row, the section chips, and the
/// active section's content, newest first.
#[allow(clippy::too_many_arguments)]
pub(super) fn profile_gallery_body(
    ui: &mut egui::Ui,
    t: Tokens,
    media: &mut crate::state::MediaState,
    live: Option<&crate::live::LiveSession>,
    demo_allowed: bool,
    source: &[Message],
    section: GallerySection,
    actions: &mut GalleryActions,
) {
    // Back row: the way back to the profile, and the section's
    // shared title (the counts section's own heading).
    ui.horizontal(|ui| {
        if crate::widgets::icon_button(
            ui,
            t,
            Icon::ChevronLeft,
            28.0,
            &crate::i18n::t("common.back"),
            None,
        )
        .clicked()
        {
            actions.back = true;
        }
        ui.label(
            RichText::new(crate::i18n::t("profile.media"))
                .font(crate::theme::semibold(15.0))
                .color(rgb(t.text_primary)),
        );
    });
    ui.add_space(SP_2);
    // Section chips: the filter-chip component, one per
    // section, the active one selected.
    ui.horizontal(|ui| {
        for (label, sec) in [
            ("Media", GallerySection::Media),
            ("Links", GallerySection::Links),
            ("Docs", GallerySection::Docs),
        ] {
            if crate::widgets::chip(ui, label, section == sec, t).clicked() {
                actions.section = Some(sec);
            }
        }
    });
    ui.add_space(10.0);
    match section {
        GallerySection::Media => media_section(ui, t, media, live, demo_allowed, source, actions),
        GallerySection::Links => links_section(ui, t, source, actions),
        GallerySection::Docs => docs_section(ui, t, source, actions),
    }
}

/// The Media section: a three-column grid of square cells,
/// one per media message, newest first. Photos paint their
/// own thumbnail or bytes (the same tile states the stream
/// decides by); videos paint their embedded frame; stickers
/// their bytes; view-once and carousel cells are placeholders
/// that locate the message, never content.
fn media_section(
    ui: &mut egui::Ui,
    t: Tokens,
    media: &mut crate::state::MediaState,
    live: Option<&crate::live::LiveSession>,
    demo_allowed: bool,
    source: &[Message],
    actions: &mut GalleryActions,
) {
    let items = core::model::gallery_media(source);
    if items.is_empty() {
        crate::widgets::empty_state(ui, t, &crate::i18n::t("profile.gallery_media_empty"));
        return;
    }
    let gap = 4.0;
    let cols = 3usize;
    let cell = ((ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32).max(48.0);
    let ctx = ui.ctx().clone();
    for row in items.iter().rev().collect::<Vec<_>>().chunks(cols) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for msg in row {
                media_cell(ui, t, media, live, demo_allowed, &ctx, msg, cell, actions);
            }
        });
        ui.add_space(gap);
    }
}

/// One media grid cell: paints the message's tile and records
/// its click.
#[allow(clippy::too_many_arguments)]
fn media_cell(
    ui: &mut egui::Ui,
    t: Tokens,
    media: &mut crate::state::MediaState,
    live: Option<&crate::live::LiveSession>,
    demo_allowed: bool,
    ctx: &egui::Context,
    msg: &Message,
    cell: f32,
    actions: &mut GalleryActions,
) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(cell), Sense::click());
    let painter = ui.painter();
    // Cell base: the panel-alt surface every placeholder and
    // not-yet-loaded tile stands on.
    painter.rect_filled(rect, CornerRadius::same(6), rgb(t.bg_panel_alt));
    let view_once = msg.is_view_once();
    match &msg.kind {
        MessageKind::Image { .. } if !view_once => {
            let tex = crate::state::App::photo_thumb_texture(media, live, ctx, msg.id)
                .or_else(|| {
                    media
                        .textures
                        .get(&msg.id)
                        .map(|h| (h.id(), h.size_vec2().x / h.size_vec2().y.max(1.0)))
                })
                .or_else(|| demo_allowed.then(|| (crate::media::demo_photo(ctx).id(), 1.0)));
            match tex {
                Some((id, aspect)) => paint_cover(painter, id, rect, aspect),
                None => {
                    // No pixels yet: the photo glyph, and the
                    // display fetch starts (the stream's rule:
                    // fetch on render while the tile is idle).
                    crate::icons::draw(
                        painter,
                        Icon::Image,
                        rect.center(),
                        crate::icons::SIZE_CONTROL,
                        rgb(t.text_faint),
                        rgb(t.bg_panel_alt),
                    );
                    if !media.textures.contains_key(&msg.id)
                        && !media.photo_fetch.contains_key(&msg.id)
                    {
                        actions.fetch_photos.push(msg.id);
                    }
                }
            }
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                actions.view_photo = Some(msg.id);
            }
        }
        MessageKind::Video { secs, .. } if !view_once => {
            if let Some((id, aspect)) =
                crate::state::App::video_thumb_texture(media, live, ctx, msg.id)
            {
                // The frame's own aspect is the texture's; the
                // cell crops it like a photo.
                paint_cover(painter, id, rect, aspect);
            } else {
                crate::icons::draw(
                    painter,
                    Icon::Video,
                    rect.center(),
                    crate::icons::SIZE_CONTROL,
                    rgb(t.text_faint),
                    rgb(t.bg_panel_alt),
                );
            }
            // Play marker and duration: the cell leads to the
            // stream's player, and says so.
            crate::icons::draw(
                painter,
                Icon::Play,
                rect.center(),
                crate::icons::SIZE_CONTROL,
                rgb(t.text_primary),
                rgb(t.bg_panel_alt),
            );
            painter.text(
                Pos2::new(rect.right() - 5.0, rect.bottom() - 5.0),
                egui::Align2::RIGHT_BOTTOM,
                format!("{}:{:02}", secs / 60, secs % 60),
                egui::FontId::proportional(10.0),
                rgb(t.text_primary),
            );
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                actions.locate = Some(msg.id);
            }
        }
        MessageKind::Sticker if !view_once => {
            match media.textures.get(&msg.id) {
                Some(handle) => {
                    let size = handle.size_vec2();
                    let scale =
                        (rect.width() / size.x.max(1.0)).min(rect.height() / size.y.max(1.0));
                    let fitted = Rect::from_center_size(rect.center(), size * scale);
                    painter.image(
                        handle.id(),
                        fitted,
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
                None => {
                    crate::icons::draw(
                        painter,
                        Icon::Smile,
                        rect.center(),
                        crate::icons::SIZE_CONTROL,
                        rgb(t.text_faint),
                        rgb(t.bg_panel_alt),
                    );
                    if !media.sticker_fetch.contains_key(&msg.id) {
                        actions.fetch_stickers.push(msg.id);
                    }
                }
            }
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                actions.locate = Some(msg.id);
            }
        }
        _ => {
            // View-once media and carousels: a placeholder cell
            // (the lock for view-once, the blocks glyph for a
            // carousel) that locates the message in the stream,
            // where its own rules apply. View-once content is
            // never painted here.
            let icon = if view_once { Icon::Lock } else { Icon::Blocks };
            crate::icons::draw(
                painter,
                icon,
                rect.center(),
                crate::icons::SIZE_CONTROL,
                rgb(t.text_faint),
                rgb(t.bg_panel_alt),
            );
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                actions.locate = Some(msg.id);
            }
        }
    }
}

/// The Links section: one row per link, newest first, the
/// preview's title (or the host) over the URL. A click opens
/// the URL through the system opener.
fn links_section(ui: &mut egui::Ui, t: Tokens, source: &[Message], _actions: &mut GalleryActions) {
    let links = core::model::gallery_links(source);
    if links.is_empty() {
        crate::widgets::empty_state(ui, t, &crate::i18n::t("profile.gallery_links_empty"));
        return;
    }
    for link in links.iter().rev() {
        let msg = link.message;
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(rect, CornerRadius::same(10), rgb(t.bg_panel_alt));
        }
        let painter = ui.painter();
        let title = msg
            .link_preview
            .as_ref()
            .map(|p| p.title.trim())
            .filter(|title| !title.is_empty())
            .map_or_else(|| link_host(&link.url), str::to_owned);
        let left = rect.left() + 10.0;
        let budget = (rect.width() - 20.0).max(0.0);
        let title = crate::widgets::ellipsize(ui, &title, crate::theme::medium(15.0), budget);
        painter.text(
            Pos2::new(left, rect.top() + 15.0),
            egui::Align2::LEFT_CENTER,
            title,
            crate::theme::medium(15.0),
            rgb(t.text_primary),
        );
        let url =
            crate::widgets::ellipsize(ui, &link.url, egui::FontId::proportional(12.0), budget);
        painter.text(
            Pos2::new(left, rect.top() + 35.0),
            egui::Align2::LEFT_CENTER,
            url,
            egui::FontId::proportional(12.0),
            rgb(t.accent),
        );
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            let _ = open::that(&link.url);
        }
        ui.add_space(2.0);
    }
}

/// The Docs section: one row per document, newest first, the
/// file's name over its size and date. A click locates the
/// message in the stream, where the document's own download
/// lives.
fn docs_section(ui: &mut egui::Ui, t: Tokens, source: &[Message], actions: &mut GalleryActions) {
    let docs = core::model::gallery_docs(source);
    if docs.is_empty() {
        crate::widgets::empty_state(ui, t, &crate::i18n::t("profile.gallery_docs_empty"));
        return;
    }
    for msg in docs.iter().rev() {
        let MessageKind::Document { name, size_bytes, .. } = &msg.kind else {
            continue;
        };
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(rect, CornerRadius::same(10), rgb(t.bg_panel_alt));
        }
        let painter = ui.painter();
        crate::icons::draw(
            painter,
            Icon::Document,
            Pos2::new(rect.left() + 18.0, rect.center().y),
            crate::icons::SIZE_CONTROL,
            rgb(t.text_secondary),
            rgb(t.bg_panel),
        );
        let left = rect.left() + 38.0;
        let budget = (rect.width() - 48.0).max(0.0);
        let name = crate::widgets::ellipsize(ui, name, crate::theme::medium(15.0), budget);
        painter.text(
            Pos2::new(left, rect.top() + 15.0),
            egui::Align2::LEFT_CENTER,
            name,
            crate::theme::medium(15.0),
            rgb(t.text_primary),
        );
        let day = msg.day_label();
        let meta = if day.is_empty() || day == "Today" {
            format!("{} · {}", doc_size(*size_bytes), msg.time)
        } else {
            format!("{} · {} · {}", doc_size(*size_bytes), day, msg.time)
        };
        painter.text(
            Pos2::new(left, rect.top() + 35.0),
            egui::Align2::LEFT_CENTER,
            meta,
            egui::FontId::proportional(12.0),
            rgb(t.text_secondary),
        );
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            actions.locate = Some(msg.id);
        }
        ui.add_space(2.0);
    }
}
