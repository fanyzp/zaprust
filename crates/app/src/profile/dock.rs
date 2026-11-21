//! Dock building blocks for the profile panel: the card, row and
//! section primitives in the WhatsApp info layout language, the
//! shared header, and the per-kind body sections the panel in
//! `super::panel` composes.

use super::ProfileIntents;
use crate::icons::Icon;
use crate::theme::{medium, rgb};
use crate::widgets::{chip, paint_chat_avatar_photo, verified_badge};
use core::model::Chat;
use core::theme::Tokens;
use eframe::egui::{self, CornerRadius, RichText, Stroke};

/// The centered profile header shared by every variant: large
/// avatar, name with verified / AI badges, one status line under
/// it. `subtitle` is resolved by the caller (presence for users,
/// member count for groups, the AI line for AI contacts).
pub(super) fn profile_header(
    ui: &mut egui::Ui,
    t: Tokens,
    chat: &Chat,
    subtitle: &str,
    photo: Option<egui::TextureId>,
) {
    // The whole identity block centres on the dock's axis through
    // layout alone: the avatar is allocated inside a centred
    // layout and painted into the rect it was allocated (the old
    // code painted at a centre computed from a different rect),
    // and the name row centres as one unit, badges included, via
    // the layout's main alignment (the old code centred it by
    // hand-measured arithmetic that under-counted the badges and
    // forgot the item spacing, landing the group left of centre
    // whenever a badge was present).
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(96.0, 96.0), egui::Sense::hover());
        paint_chat_avatar_photo(ui.painter(), rect.center(), 48.0, chat, photo);
    });
    ui.add_space(SP_2);
    // The name row is painted on one measured line. egui has no
    // primitive that centres a row of siblings as a group
    // (`main_align` aligns content inside a widget, not between
    // widgets), and nothing in this row is interactive, so the
    // group is composed directly: measure the name, add the real
    // badge widths and the real item spacing, and start the
    // group at centre minus half the total. Allocation and paint
    // use the same numbers, so the group is centred by
    // construction, badges or not.
    let name_font = crate::theme::semibold(18.0);
    let name_w = ui
        .painter()
        .layout_no_wrap(chat.title.clone(), name_font.clone(), rgb(t.text_primary))
        .size()
        .x;
    let gap = ui.spacing().item_spacing.x;
    let mut total = name_w;
    if chat.verified {
        total += gap + 18.0;
    }
    if chat.meta_ai {
        total += gap + 22.0;
    }
    let (row, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), egui::Sense::hover());
    let mut x = row.center().x - total / 2.0;
    let cy = row.center().y;
    ui.painter().text(
        egui::pos2(x, cy),
        egui::Align2::LEFT_CENTER,
        &chat.title,
        name_font,
        rgb(t.text_primary),
    );
    x += name_w;
    if chat.verified {
        x += gap;
        verified_badge(ui.painter(), egui::pos2(x + 9.0, cy), 8.0, t);
        x += 18.0;
    }
    if chat.meta_ai {
        x += gap;
        crate::widgets::ai_chip(ui.painter(), egui::pos2(x, cy), t);
    }
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(subtitle).size(12.5).color(rgb(t.text_secondary)));
    });
}

// ----------------------------------------------------------------
// Dock layout metrics: one grid for the whole info dock, so every
// section lines up on the same axes instead of each section
// hand-rolling its own row. The icon axis and label line reuse
// the app-wide popup-row convention (`menu_item_row` in
// widgets.rs centres its icon 19 px in and puts the label at
// 38 px), so the dock reads as part of the same system. Member
// avatars centre on the icon axis and their names start on the
// label line, like every other row.

/// Height of one simple dock row (action rows, stat rows).
pub(super) const DOCK_ROW_H: f32 = 42.0;

/// X of the row icon axis, from the dock content edge.
pub(super) const DOCK_ICON_X: f32 = 19.0;

/// X of the row label line, from the dock content edge.
pub(super) const DOCK_LABEL_X: f32 = 38.0;

/// Glyph size for row icons.
pub(super) const DOCK_ICON_SIZE: f32 = 20.0;

/// Height of one cell in the identity action row.
pub(super) const DOCK_ACTION_H: f32 = 66.0;

/// Diameter of the disc behind an action cell's icon.
pub(super) const DOCK_ACTION_DISC: f32 = 44.0;

/// The dock spacing scale, a 4 px ladder. Every vertical gap in
/// the dock is one of these, never an ad-hoc number.
pub(super) const SP_1: f32 = 4.0;

pub(super) const SP_2: f32 = 8.0;

pub(super) const SP_3: f32 = 12.0;

/// Gap above a section card.
pub(super) const SP_SECTION: f32 = 20.0;

/// The resting fill of a dock section card: the elevated-card
/// step above the dock panel, so a grouped section reads as a
/// sheet resting on the panel instead of more of the same flat
/// fill. Every row helper in the dock paints its hover and
/// pressed states as steps of this fill, which is what makes
/// the card the dock's one interactive surface.
pub(super) fn dock_card_fill(t: Tokens) -> core::theme::Rgb {
    core::theme::elevated_step(t.bg_panel)
}

/// One grouped dock section as an elevated card: the card fill,
/// a hairline border and rounded corners, with the dock's
/// content inset on every side. The grid constants (icon axis,
/// label line) are measured from the card's content edge, so
/// rows in every card share one alignment.
pub(super) fn dock_card<R>(
    ui: &mut egui::Ui,
    t: Tokens,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    egui::Frame::new()
        .fill(rgb(dock_card_fill(t)))
        .corner_radius(CornerRadius::same(12))
        .stroke(Stroke::new(1.0_f32, rgb(t.border)))
        .inner_margin(egui::Margin::symmetric(10, 10))
        .show(ui, |ui| {
            // Every card spans the dock's full content width:
            // a card whose body is one short row (the requests
            // card's Review button) must not hug its content
            // while its neighbours stretch.
            ui.set_min_width(ui.available_width());
            body(ui)
        })
}

/// Prose on the dock's label line (section captions, the
/// About and description texts): indented by a margin frame,
/// never by a horizontal row. A label inside a horizontal row
/// takes its unwrapped width, so a long caption would size the
/// row, and with it the whole dock, to hundreds of pixels
/// beyond the panel; in a margin frame the same label wraps at
/// the card's inner width like prose should.
pub(super) fn dock_prose<R>(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .inner_margin(egui::Margin { left: DOCK_LABEL_X as i8, right: 0, top: 0, bottom: 0 })
        .show(ui, body)
        .inner
}

/// A dock section card with its heading inside, on the label
/// line, and an optional caption under it: the standard grouped
/// section of the dock.
pub(super) fn dock_card_section<R>(
    ui: &mut egui::Ui,
    t: Tokens,
    title: &str,
    caption: Option<&str>,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.add_space(SP_SECTION);
    dock_card(ui, t, |ui| {
        ui.horizontal(|ui| {
            ui.add_space(DOCK_LABEL_X);
            ui.label(RichText::new(title).size(13.0).strong().color(rgb(t.text_primary)));
        });
        if let Some(caption) = caption {
            dock_prose(ui, |ui| {
                ui.label(RichText::new(caption).size(11.5).color(rgb(t.text_faint)));
            });
        }
        ui.add_space(SP_1);
        body(ui)
    })
    .inner
}

/// One full-width tappable dock row: an optional icon centred on
/// the shared icon axis, the label on the shared label line, an
/// optional chevron (only for rows that navigate somewhere;
/// action rows pass `false`). Rows without an icon keep the
/// empty column, so every label in the dock starts on the same
/// vertical line.
pub(super) fn dock_row(
    ui: &mut egui::Ui,
    t: Tokens,
    icon: Option<(Icon, core::theme::Rgb)>,
    label: &str,
    label_color: core::theme::Rgb,
    chevron: bool,
) -> egui::Response {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, DOCK_ROW_H), egui::Sense::click());
    if resp.is_pointer_button_down_on() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(10),
            rgb(core::theme::pressed_step(dock_card_fill(t))),
        );
    } else if resp.hovered() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(10),
            rgb(core::theme::hover_step(dock_card_fill(t))),
        );
    }
    if let Some((icon, color)) = icon {
        crate::icons::draw(
            ui.painter(),
            icon,
            egui::pos2(rect.left() + DOCK_ICON_X, rect.center().y),
            DOCK_ICON_SIZE,
            rgb(color),
            rgb(t.bg_panel),
        );
    }
    ui.painter().text(
        egui::pos2(rect.left() + DOCK_LABEL_X, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        crate::theme::medium(13.0),
        rgb(label_color),
    );
    if chevron {
        crate::icons::draw(
            ui.painter(),
            Icon::ChevronRight,
            egui::pos2(rect.right() - 14.0, rect.center().y),
            16.0,
            rgb(t.text_faint),
            rgb(t.bg_panel),
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// One non-interactive dock stat row: the same geometry as
/// [`dock_row`], with a right-aligned value and an optional
/// hairline under it, inset to the label line.
pub(super) fn dock_stat_row(
    ui: &mut egui::Ui,
    t: Tokens,
    label: &str,
    value: &str,
    separator: bool,
) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, DOCK_ROW_H), egui::Sense::hover());
    ui.painter().text(
        egui::pos2(rect.left() + DOCK_LABEL_X, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        crate::theme::medium(13.0),
        rgb(t.text_primary),
    );
    ui.painter().text(
        egui::pos2(rect.right(), rect.center().y),
        egui::Align2::RIGHT_CENTER,
        value,
        crate::theme::medium(13.0),
        rgb(t.text_secondary),
    );
    if separator {
        ui.painter().hline(
            (rect.left() + DOCK_LABEL_X)..=rect.right(),
            rect.bottom() - 0.5,
            Stroke::new(1.0_f32, rgb(t.border)),
        );
    }
}

/// The identity action row: equal-width cells spanning the dock,
/// each cell's icon disc and caption centred on the cell's own
/// axis as one unit (the structure Telegram's info panel uses,
/// in this app's tokens). Painting the cell directly is what
/// guarantees the centring: the previous implementation nested
/// two sub-layouts with different alignment rules, so the icon
/// and its caption were centred by different maths and never
/// shared an axis. Returns the clicked cell's index, if any.
pub(super) fn dock_action_row(
    ui: &mut egui::Ui,
    t: Tokens,
    actions: &[(Icon, &str)],
) -> Option<usize> {
    if actions.is_empty() {
        return None;
    }
    ui.spacing_mut().item_spacing.x = SP_2;
    let n = actions.len() as f32;
    let cell_w = ((ui.available_width() - SP_2 * (n - 1.0)) / n).max(0.0);
    let mut clicked = None;
    ui.horizontal(|ui| {
        for (i, (icon, label)) in actions.iter().enumerate() {
            let (rect, resp) =
                ui.allocate_exact_size(egui::vec2(cell_w, DOCK_ACTION_H), egui::Sense::click());
            if resp.is_pointer_button_down_on() {
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::same(12),
                    rgb(core::theme::pressed_step(t.bg_panel_alt)),
                );
            } else if resp.hovered() {
                ui.painter().rect_filled(rect, CornerRadius::same(12), rgb(t.bg_panel_alt));
            }
            let disc_c = egui::pos2(rect.center().x, rect.top() + DOCK_ACTION_DISC / 2.0 + 2.0);
            ui.painter().circle_filled(disc_c, DOCK_ACTION_DISC / 2.0, rgb(t.bg_panel_alt));
            crate::icons::draw(
                ui.painter(),
                *icon,
                disc_c,
                22.0,
                rgb(t.text_primary),
                rgb(t.bg_panel_alt),
            );
            ui.painter().text(
                egui::pos2(rect.center().x, rect.bottom() - 2.0),
                egui::Align2::CENTER_BOTTOM,
                label,
                medium(11.0),
                rgb(t.text_secondary),
            );
            if resp.clicked() {
                clicked = Some(i);
            }
        }
    });
    clicked
}

/// One member row: the avatar centred on the shared icon axis,
/// the name (and the address, in live lists) on the shared label
/// line, the admin tag and, when the row carries actions, ONE
/// kebab button at the right edge. Per-member actions live in
/// the kebab's menu (progressive disclosure): a member row is a
/// single visual unit, never a row of buttons. Returns the
/// kebab's response so the caller can anchor the menu to it;
/// rows without actions (demo lists) pass `kebab: false` and
/// get `None`.
pub(super) fn dock_member_row(
    ui: &mut egui::Ui,
    t: Tokens,
    name: &str,
    address: Option<&str>,
    admin: bool,
    kebab: bool,
) -> Option<egui::Response> {
    let seed = (name.bytes().map(u32::from).sum::<u32>() % 8) as u8;
    let mut kebab_resp = None;
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), 32.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            let (r, _) =
                ui.allocate_exact_size(egui::vec2(DOCK_LABEL_X, 32.0), egui::Sense::hover());
            crate::widgets::paint_avatar(
                ui.painter(),
                egui::pos2(r.left() + DOCK_ICON_X, r.center().y),
                15.0,
                false,
                seed,
            );
            ui.vertical(|ui| {
                ui.label(RichText::new(name).size(12.5).color(rgb(t.text_primary)));
                if let Some(address) = address {
                    ui.label(RichText::new(address).size(10.5).color(rgb(t.text_faint)));
                }
            });
            if kebab || admin {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if kebab {
                        kebab_resp = Some(crate::widgets::icon_button_on(
                            ui,
                            t,
                            Icon::MoreVert,
                            28.0,
                            &crate::i18n::t("profile.member_actions"),
                            None,
                            core::theme::hover_step(dock_card_fill(t)),
                            core::theme::pressed_step(dock_card_fill(t)),
                        ));
                    }
                    if admin {
                        if kebab {
                            ui.add_space(SP_1);
                        }
                        ui.label(
                            RichText::new(crate::i18n::t("profile.admin"))
                                .font(medium(10.5))
                                .color(rgb(t.accent)),
                        );
                    }
                });
            }
        },
    );
    kebab_resp
}

/// The "Media, links and docs" section: counts computed by the
/// caller from the loaded conversation.
pub(super) fn profile_counts_section(
    ui: &mut egui::Ui,
    t: Tokens,
    media: usize,
    links: usize,
    docs: usize,
) {
    dock_card_section(ui, t, &crate::i18n::t("profile.media"), None, |ui| {
        let rows = [("Media", media), ("Links", links), ("Docs", docs)];
        for (i, (label, count)) in rows.iter().enumerate() {
            dock_stat_row(ui, t, label, &count.to_string(), i + 1 < rows.len());
        }
    });
}

/// One radio row on the dock grid: the shared row geometry
/// with a radio dot at the right edge instead of a chevron.
pub(super) fn ephemeral_option_row(
    ui: &mut egui::Ui,
    t: Tokens,
    label: &str,
    selected: bool,
) -> bool {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, DOCK_ROW_H), egui::Sense::click());
    if resp.is_pointer_button_down_on() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(10),
            rgb(core::theme::pressed_step(dock_card_fill(t))),
        );
    } else if resp.hovered() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(10),
            rgb(core::theme::hover_step(dock_card_fill(t))),
        );
    }
    ui.painter().text(
        egui::pos2(rect.left() + DOCK_LABEL_X, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        crate::theme::medium(13.0),
        rgb(t.text_primary),
    );
    let dot_c = egui::pos2(rect.right() - 14.0, rect.center().y);
    let ring = if selected { t.accent } else { t.text_faint };
    ui.painter().circle_stroke(dot_c, 7.0, Stroke::new(1.5, rgb(ring)));
    if selected {
        ui.painter().circle_filled(dot_c, 3.5, rgb(t.accent));
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// The disappearing-messages section of the info dock: a
/// heading, a caption, and one radio row per duration. Shared
/// by the user and group bodies; channels do not carry the
/// feature (a newsletter is not an end-to-end chat).
pub(super) fn disappearing_section(
    ui: &mut egui::Ui,
    t: Tokens,
    current: u32,
    intents: &mut ProfileIntents,
) {
    dock_card_section(
        ui,
        t,
        &crate::i18n::t("profile.disappearing"),
        Some(&crate::i18n::t("profile.disappearing_caption")),
        |ui| {
            for secs in core::model::EPHEMERAL_OPTIONS {
                let label = crate::extras::ephemeral_duration_label(secs);
                if ephemeral_option_row(ui, t, &label, secs == current) {
                    intents.ephemeral_pick = Some(secs);
                }
            }
        },
    );
}

/// The user (DM contact) body, standard WhatsApp layout: the
/// action row, an identifier line (the live JID, or the demo
/// caption in demo data), an About card when the contact carries a
/// non-presence status line, the media counts, and the block /
/// report rows at the bottom.
#[allow(clippy::too_many_arguments)]
pub(crate) fn profile_user_body(
    ui: &mut egui::Ui,
    t: Tokens,
    id_line: Option<&str>,
    about: Option<&str>,
    counts: (usize, usize, usize),
    blocked: bool,
    export_active: bool,
    ephemeral_secs: u32,
    intents: &mut ProfileIntents,
) {
    // Action row: audio call (the same live-call path the chat
    // header uses), search, and export when the feature is on, as
    // equal-width cells. There is no video-call feature, so there
    // is no video cell.
    ui.add_space(SP_3);
    let audio = crate::i18n::t("profile.audio_call");
    let search = crate::i18n::t("profile.search");
    let export = crate::i18n::t("common.export_chat");
    let mut actions: Vec<(Icon, &str)> =
        vec![(Icon::Phone, audio.as_str()), (Icon::Search, search.as_str())];
    if export_active {
        actions.push((Icon::Download, export.as_str()));
    }
    if let Some(i) = dock_action_row(ui, t, &actions) {
        match actions[i].0 {
            Icon::Phone => intents.start_call = true,
            Icon::Search => intents.focus_search = true,
            _ => intents.export = true,
        }
    }
    if let Some(line) = id_line {
        ui.add_space(SP_2);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(line).size(12.0).color(rgb(t.text_faint)));
        });
    }
    if let Some(about) = about {
        dock_card_section(ui, t, &crate::i18n::t("profile.about"), None, |ui| {
            dock_prose(ui, |ui| {
                ui.label(RichText::new(about).size(12.5).color(rgb(t.text_secondary)));
            });
        });
    }
    profile_counts_section(ui, t, counts.0, counts.1, counts.2);
    disappearing_section(ui, t, ephemeral_secs, intents);
    ui.add_space(SP_SECTION);
    // Block / report rows in their own card: danger text on the
    // shared row grid, no chevron (they act immediately, they do
    // not navigate).
    dock_card(ui, t, |ui| {
        let block_label = crate::i18n::t(if blocked { "profile.unblock" } else { "profile.block" });
        if dock_row(ui, t, None, &block_label, t.danger, false).clicked() {
            intents.toggle_block = true;
        }
        if dock_row(ui, t, None, &crate::i18n::t("profile.report"), t.danger, false).clicked() {
            intents.toggle_block = true;
        }
    });
}

/// The AI profile body, in the style of Meta AI's profile: a short
/// capability description under the header, then suggestion chips
/// that drop a starter prompt into the composer. Flat fills only,
/// no gradients: the sparkle avatar in the header carries the AI
/// identity. `description` and `suggestions` are resolved by the
/// caller (the description differs between demo and live data).
pub(crate) fn profile_ai_body(
    ui: &mut egui::Ui,
    t: Tokens,
    description: &str,
    suggestions: &[String],
    intents: &mut ProfileIntents,
) {
    ui.add_space(SP_2);
    ui.label(RichText::new(description).size(12.5).color(rgb(t.text_secondary)));
    ui.add_space(SP_3);
    ui.label(
        RichText::new(crate::i18n::t("profile.ai_suggest_label"))
            .size(13.0)
            .strong()
            .color(rgb(t.text_primary)),
    );
    ui.add_space(SP_1);
    ui.horizontal_wrapped(|ui| {
        for s in suggestions {
            if chip(ui, s, false, t).clicked() {
                intents.suggestion = Some(s.clone());
            }
        }
    });
}
