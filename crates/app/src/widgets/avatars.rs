//! Avatars and identity painting: avatar discs, initials, the verified
//! badge and per-sender name colors.

use crate::theme::rgb;
use core::theme::{Rgb, Tokens};
use eframe::egui::{self, Color32, CornerRadius, FontId, Pos2, Sense, Stroke, Vec2};

/// WhatsApp-style default avatar: a plain grey disc with a white
/// silhouette (one person, or a group glyph for groups). No initials,
/// contacts without a photo get this in WhatsApp, and it reads cleaner.
pub(crate) fn avatar(ui: &mut egui::Ui, group: bool, size: f32, t: Tokens) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    // The signed-in profile wears the accent color.
    paint_avatar_disc(ui.painter(), rect.center(), size / 2.0, group, rgb(t.accent));
}

/// Soft, friendly disc colors for contacts without a photo (the disc
/// itself carries the color; the silhouette stays white).
pub(crate) fn avatar_color(seed: u8) -> Color32 {
    const PALETTE: [Color32; 8] = [
        Color32::from_rgb(0xE1, 0x70, 0x76),
        Color32::from_rgb(0xED, 0x9F, 0x5A),
        Color32::from_rgb(0xA6, 0x95, 0xE7),
        Color32::from_rgb(0x7B, 0xC8, 0x62),
        Color32::from_rgb(0x6E, 0xC9, 0xCB),
        Color32::from_rgb(0x65, 0xAA, 0xDD),
        Color32::from_rgb(0xEE, 0x7A, 0xAE),
        Color32::from_rgb(0x5E, 0xB8, 0x8F),
    ];
    PALETTE[(seed as usize) % PALETTE.len()]
}

/// Paints the default avatar centred at `center`.
pub(crate) fn paint_avatar(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    group: bool,
    seed: u8,
) {
    paint_avatar_disc(painter, center, radius, group, avatar_color(seed));
}

/// A colored disc with the white silhouette on top.
pub(crate) fn paint_avatar_disc(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    group: bool,
    disc: Color32,
) {
    let fg = Color32::from_rgba_unmultiplied(255, 255, 255, 235);
    painter.circle_filled(center, radius, disc);
    if group {
        // The Material "groups" glyph, from the icon font.
        crate::icons::draw(painter, crate::icons::Icon::Group, center, radius * 1.3, fg, disc);
    } else {
        person(painter, center, radius, fg);
    }
}

/// One silhouette: head circle + shoulders ellipse, sized to fit a disc
/// of radius `r` around `center`.
fn person(painter: &egui::Painter, center: Pos2, r: f32, color: Color32) {
    painter.circle_filled(center + Vec2::new(0.0, -r * 0.28), r * 0.30, color);
    painter.add(egui::Shape::ellipse_filled(
        center + Vec2::new(0.0, r * 0.52),
        Vec2::new(r * 0.55, r * 0.36),
        color,
    ));
}

/// The avatar for AI chats (Meta AI): a tinted disc with the
/// Material "auto_awesome" glyph instead of the generic person
/// silhouette.
pub(crate) fn paint_ai_avatar(painter: &egui::Painter, center: Pos2, radius: f32, disc: Color32) {
    painter.circle_filled(center, radius, disc);
    crate::icons::draw(
        painter,
        crate::icons::Icon::Sparkles,
        center,
        radius * 1.25,
        Color32::from_rgba_unmultiplied(255, 255, 255, 240),
        disc,
    );
}

/// Avatar dispatch for a chat row / header: AI chats get the
/// sparkles avatar, groups keep the group glyph, and one-to-one
/// chats wear their initials on the colored disc (Telegram style),
/// so every row carries an identity instead of the same silhouette.
pub(crate) fn paint_chat_avatar(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    chat: &core::model::Chat,
) {
    paint_chat_avatar_photo(painter, center, radius, chat, None);
}

/// The same dispatch with an optional profile photo: a one-to-one
/// chat with a photo wears the photo, clipped to the disc; every
/// other case falls through to the painted forms.
pub(crate) fn paint_chat_avatar_photo(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    chat: &core::model::Chat,
    photo: Option<egui::TextureId>,
) {
    if chat.meta_ai {
        paint_ai_avatar(painter, center, radius, avatar_color(2));
    } else if chat.kind == core::model::ChatKind::Group {
        paint_avatar(painter, center, radius, true, chat.avatar_seed);
    } else if let Some(tex) = photo {
        paint_photo_avatar(painter, center, radius, tex);
    } else {
        paint_initials_avatar(painter, center, radius, &chat.title, chat.avatar_seed);
    }
}

/// A profile photo clipped to a disc: a textured circle fan with
/// the image's centre mapped to the disc's centre.
pub(crate) fn paint_photo_avatar(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    tex: egui::TextureId,
) {
    const SEGMENTS: usize = 40;
    let mut mesh = egui::Mesh::with_texture(tex);
    mesh.vertices.push(egui::epaint::Vertex {
        pos: center,
        uv: egui::pos2(0.5, 0.5),
        color: egui::Color32::WHITE,
    });
    for i in 0..SEGMENTS {
        let angle = std::f32::consts::TAU * i as f32 / SEGMENTS as f32;
        let (sin, cos) = angle.sin_cos();
        mesh.vertices.push(egui::epaint::Vertex {
            pos: center + egui::vec2(cos * radius, sin * radius),
            uv: egui::pos2(0.5 + 0.5 * cos, 0.5 + 0.5 * sin),
            color: egui::Color32::WHITE,
        });
    }
    for i in 0..SEGMENTS {
        let rim = 1 + i as u32;
        let next = 1 + ((i + 1) % SEGMENTS) as u32;
        mesh.indices.extend_from_slice(&[0, rim, next]);
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// A colored disc with the contact's initials (first letters of the
/// first two name words), the no-photo avatar Telegram uses.
pub(crate) fn paint_initials_avatar(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    name: &str,
    seed: u8,
) {
    painter.circle_filled(center, radius, avatar_color(seed));
    let initials: String = name
        .split_whitespace()
        .take(2)
        .filter_map(|w| w.chars().next())
        .collect::<String>()
        .to_uppercase();
    if !initials.is_empty() {
        painter.text(
            center,
            egui::Align2::CENTER_CENTER,
            initials,
            crate::theme::semibold(radius * 0.82),
            Color32::from_rgba_unmultiplied(255, 255, 255, 242),
        );
    }
}

/// The verified badge: a scalloped disc in the theme's success
/// color with a white check, WhatsApp-style.
pub(crate) fn verified_badge(painter: &egui::Painter, center: Pos2, radius: f32, t: Tokens) {
    let mut pts = Vec::with_capacity(24);
    for i in 0..24 {
        let angle = std::f32::consts::TAU * i as f32 / 24.0;
        let rr = if i % 2 == 0 { radius } else { radius * 0.84 };
        pts.push(center + Vec2::new(angle.cos() * rr, angle.sin() * rr));
    }
    painter.add(egui::Shape::convex_polygon(pts, rgb(t.badge_bg), Stroke::NONE));
    let check = vec![
        center + Vec2::new(-radius * 0.52, radius * 0.02),
        center + Vec2::new(-radius * 0.16, radius * 0.38),
        center + Vec2::new(radius * 0.55, -radius * 0.34),
    ];
    painter.add(egui::Shape::line(check, Stroke::new(radius * 0.22, Color32::WHITE)));
}

/// A small "AI" chip (used next to Meta AI's name).
pub(crate) fn ai_chip(painter: &egui::Painter, left_center: Pos2, t: Tokens) -> f32 {
    let rect = egui::Rect::from_min_size(
        Pos2::new(left_center.x, left_center.y - 7.0),
        Vec2::new(22.0, 14.0),
    );
    painter.rect_filled(rect, CornerRadius::same(7), rgb(t.accent));
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "AI",
        FontId::proportional(9.0),
        rgb(t.bg_panel),
    );
    22.0
}

/// The sender-name palette for the scheme: six colors chosen
/// for contrast on the bubble background. The stream assigns
/// them per conversation without replacement (see
/// `assign_sender_colors` in the conversation stream); the
/// standalone `sender_color` below is the fallback for surfaces
/// that name a sender outside a conversation.
pub(crate) fn sender_palette(t: Tokens) -> [Rgb; 6] {
    if t.is_light() {
        [
            Rgb(0xD9, 0x2B, 0x2B),
            Rgb(0x00, 0x80, 0x69),
            Rgb(0x7B, 0x3F, 0xB5),
            Rgb(0xC7, 0x51, 0x03),
            Rgb(0x1D, 0x6F, 0xB8),
            Rgb(0xA8, 0x1C, 0x6B),
        ]
    } else {
        [
            Rgb(0xFF, 0x8A, 0x80),
            Rgb(0x82, 0xB1, 0xFF),
            Rgb(0xEA, 0x80, 0xFC),
            Rgb(0xFF, 0xD1, 0x80),
            Rgb(0xA7, 0xFF, 0xEB),
            Rgb(0xCC, 0xFF, 0x90),
        ]
    }
}

/// Deterministic per-person color for group sender names.
/// Real WhatsApp colors each member's name; so do we.
pub(crate) fn sender_color(name: &str, t: Tokens) -> Color32 {
    let palette = sender_palette(t);
    let hash: usize = name.bytes().map(usize::from).sum();
    rgb(palette[hash % palette.len()])
}
