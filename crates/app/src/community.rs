//! Community home screen: the landing page for one WhatsApp
//! Community. A cover, a centered identity block (avatar, name,
//! group count, description with a Read more toggle), an
//! Announcements section, the groups the user is in, and the
//! other groups of the community they can still join.
//!
//! Integration: the shell keeps the open community in
//! `App::open_community`; the Communities overview panel (the
//! rail's Communities entry) sets it, and the shell then draws
//! [`App::draw_community_home`] (below) in the content area
//! instead of the conversation. That method resolves the
//! community, its announcement chat and its group chats: demo
//! mode uses `core::model::demo_communities()`, live mode
//! groups the group chats carrying a `community` name tag.
//! Communities never appear as rows in the chat list; the
//! overview panel is the only door to a community home.
//!
//! The announcement group is NOT a chat-list entry. In demo mode
//! it lives outside `demo_chats()` for good: opening it loads it
//! into `App::detached_chat`, a slot the chat list never renders,
//! so it can be read like any conversation without ever joining
//! the list as a regular row. In live mode the announcement is a
//! stand-in chosen from the community's real groups (already
//! listed as themselves); nothing is added to the list either.
//!
//! Not-yet-joined groups behave the same way: they live outside
//! `demo_chats()` (see `core::model::demo_community_other_groups`)
//! and only join the chat list when the user joins one, through
//! `App::join_community_group`.

use crate::state::{App, Notice};
use crate::theme::{rgb, SP_1, SP_1_5, SP_2, SP_3, SP_6, SP_SECTION};
use crate::widgets::icon_button;
use core::model::{
    demo_announcement_chat, demo_announcement_messages, demo_communities,
    demo_community_other_groups, Chat, ChatKind, Community,
};
use core::theme::Tokens;
use eframe::egui::{self, CornerRadius, FontId, Pos2, RichText, Sense, Vec2};

/// The center-crop UV rect for painting the demo photo (600x400)
/// into a target rect of `target` size: the largest centered
/// sub-rect of the source with the target's aspect ratio, in UV
/// coordinates, so the photo fills the target without stretching.
fn center_crop_uv(target: Vec2) -> egui::Rect {
    const SRC_ASPECT: f32 = 600.0 / 400.0;
    let target_aspect = target.x / target.y.max(1.0);
    if target_aspect >= SRC_ASPECT {
        // Target is wider than the source: full width, crop height.
        let f = SRC_ASPECT / target_aspect;
        let y0 = (1.0 - f) / 2.0;
        egui::Rect::from_min_max(Pos2::new(0.0, y0), Pos2::new(1.0, y0 + f))
    } else {
        // Target is narrower: full height, crop width.
        let f = target_aspect / SRC_ASPECT;
        let x0 = (1.0 - f) / 2.0;
        egui::Rect::from_min_max(Pos2::new(x0, 0.0), Pos2::new(x0 + f, 1.0))
    }
}

/// What the user asked the community home to do.
#[derive(Debug, Default)]
pub struct CommunityIntents {
    /// Open this chat (the announcement chat or one of the
    /// community's groups).
    pub open_chat: Option<u64>,
    /// Go back to the chat list.
    pub back: bool,
    /// Join this not-yet-joined group.
    pub join_group: Option<u64>,
    /// Toggle the description between collapsed and expanded.
    pub toggle_desc: bool,
    /// Fetch the community invite link.
    pub fetch_invite: bool,
    /// Reset the community invite link (invalidates the old one).
    pub reset_invite: bool,
    /// Copy the community invite link to the clipboard.
    pub copy_invite: bool,
    /// Toggle the link-a-group picker.
    pub toggle_link_picker: bool,
    /// Link this group into the community.
    pub link_group: Option<u64>,
    /// Unlink this group from the community (the caller
    /// confirms first).
    pub unlink_group: Option<u64>,
    /// Create a subgroup with the drafted name.
    pub create_subgroup: bool,
    /// Promote or demote this member of the parent group:
    /// (address, operation).
    pub member_op: Option<(String, backend::ParticipantOp)>,
    /// Remove this member from the whole community (the caller
    /// confirms first): (address, display name).
    pub remove_member: Option<(String, String)>,
    /// Deactivate the community (the caller confirms first).
    pub deactivate: bool,
}

/// The live data behind the community home's member and
/// management sections, resolved by the caller from the live
/// stores. Absent in demo mode: the sections render only
/// against a live (or fake) backend, where the parent group's
/// member list, the account's own role and the invite link are
/// real reported state rather than layout inventions.
pub struct CommunityAdmin<'a> {
    /// The parent (announcement) group's member rows.
    pub members: &'a [crate::state::MemberRow],
    /// Avatar photos aligned with `members`.
    pub member_photos: Vec<Option<egui::TextureId>>,
    /// Whether my account admins the community. Gates every
    /// management control; the member list itself is visible to
    /// members, with no menus, when this is false.
    pub own_admin: bool,
    /// The community invite link, once fetched.
    pub invite_link: Option<&'a str>,
    /// Groups that could be linked into this community: my
    /// admin groups carrying no community of their own.
    pub link_candidates: Vec<&'a Chat>,
    /// Whether the link-a-group picker is expanded.
    pub link_picker_open: bool,
    /// The create-subgroup form's name draft.
    pub subgroup_name: &'a mut String,
}

/// A section caption on the community home ("Announcements",
/// "Groups you're in").
fn section_label(ui: &mut egui::Ui, t: Tokens, text: &str) {
    ui.label(RichText::new(text).size(10.0).strong().color(rgb(t.text_faint)));
    ui.add_space(SP_1);
}

/// Characters of description shown while collapsed (about two
/// lines at the identity block's width). Longer descriptions are
/// cut on a word boundary with an ellipsis and a Read more toggle.
const DESC_COLLAPSED_CHARS: usize = 140;

fn collapsed_description(desc: &str) -> String {
    if desc.chars().count() <= DESC_COLLAPSED_CHARS {
        return desc.to_owned();
    }
    let cut: String = desc.chars().take(DESC_COLLAPSED_CHARS).collect();
    // Back the cut up to the last word boundary inside it, so the
    // ellipsis never lands mid-word. A single word longer than
    // the whole budget has no boundary to back up to and keeps
    // the hard char cut.
    let cut = match cut.rfind(char::is_whitespace) {
        Some(idx) => &cut[..idx],
        None => &cut[..],
    };
    let cut = cut.trim_end();
    format!("{cut}...")
}

/// One not-yet-joined group row: the shared chat row's metrics
/// and avatar/title/snippet anatomy (chat_list.rs owns the
/// constants and the preview-line helper), plus a right-aligned
/// Join pill. The pill is its own interactive target and the
/// ONLY way to join: returns true when the pill was clicked;
/// clicking the row itself does nothing.
fn other_group_row(ui: &mut egui::Ui, chat: &Chat, t: Tokens, pill_key: &str) -> bool {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), crate::chat_list::ROW_H),
        Sense::hover(),
    );
    let hov = crate::anim::hover_factor(ui, ("other_group_row", chat.id), response.hovered());
    if hov > 0.001 {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(crate::theme::RADIUS_SMALL),
            crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
        );
    }
    let painter = ui.painter();
    let center = Pos2::new(rect.left() + crate::chat_list::AVATAR_CX, rect.center().y);
    crate::widgets::paint_chat_avatar(painter, center, crate::chat_list::AVATAR_R, chat);
    // The Join pill occupies a right slot; the title and snippet
    // are ellipsized to the width left of it.
    let join_label = crate::i18n::t(pill_key);
    let join_font = crate::theme::medium(12.0);
    let join_w = ui.ctx().fonts_mut(|f| {
        f.layout_no_wrap(join_label.clone(), join_font.clone(), rgb(t.badge_text)).size().x
    }) + 24.0;
    let pill = egui::Rect::from_center_size(
        Pos2::new(rect.right() - 8.0 - join_w / 2.0, rect.center().y),
        Vec2::new(join_w, 28.0),
    );
    let text_left = rect.left() + crate::chat_list::TEXT_X;
    let budget = (pill.left() - 10.0 - text_left).max(0.0);
    let title_font = crate::theme::medium(15.0);
    let title = crate::widgets::ellipsize(ui, &chat.title, title_font.clone(), budget);
    painter.text(
        Pos2::new(text_left, rect.top() + crate::chat_list::TITLE_DY),
        egui::Align2::LEFT_CENTER,
        title,
        title_font,
        rgb(t.text_primary),
    );
    let preview = crate::chat_list::chat_preview_line(chat);
    let snippet_font = FontId::proportional(12.0);
    let preview = crate::widgets::ellipsize(ui, &preview, snippet_font.clone(), budget);
    if !preview.is_empty() {
        painter.text(
            Pos2::new(text_left, rect.top() + crate::chat_list::SNIPPET_DY),
            egui::Align2::LEFT_CENTER,
            preview,
            snippet_font,
            rgb(t.text_secondary),
        );
    }
    // The pill as its own button: a quiet tinted outline, never
    // a filled button sitting inside a list row. Hover and press
    // only deepen the tint, with the pointing hand. The tint now
    // crossfades through the shared factors between the same
    // three endpoints (32 rest, 64 hovered, 96 held) instead of
    // snapping between them. The row itself stays hover-only:
    // it is not a click target, so a press wash on it would
    // promise a click it does not take.
    let pill_resp = ui
        .interact(pill, egui::Id::unique(("join_pill", chat.id)), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let pill_hov = crate::anim::hover_factor(ui, ("join_pill", chat.id), pill_resp.hovered());
    let pill_press = crate::anim::press_factor(
        ui,
        ("join_pill", chat.id),
        pill_resp.is_pointer_button_down_on(),
    );
    let pill_alpha = (32.0 + 32.0 * pill_hov + 32.0 * pill_press).round() as u8;
    painter.rect_filled(
        pill,
        CornerRadius::same(14),
        crate::theme::glass_wash(t, t.accent, pill_alpha),
    );
    painter.rect_stroke(
        pill,
        CornerRadius::same(14),
        egui::Stroke::new(1.0, rgb(t.accent)),
        egui::StrokeKind::Inside,
    );
    painter.text(pill.center(), egui::Align2::CENTER_CENTER, join_label, join_font, rgb(t.accent));
    pill_resp.clicked()
}

/// What a managed group row asked for.
enum ManagedGroupAction {
    None,
    Open,
    Unlink,
}

/// One linked-group row for community admins: the shared row
/// metrics and avatar/title/snippet anatomy of
/// `other_group_row`, plus a kebab at the right edge whose menu
/// carries the unlink action (per-row actions live behind
/// kebabs, never as buttons in the row). The row's own click
/// target stops short of the kebab, so the two never compete.
fn managed_group_row(ui: &mut egui::Ui, t: Tokens, chat: &Chat) -> ManagedGroupAction {
    let mut action = ManagedGroupAction::None;
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), crate::chat_list::ROW_H),
        Sense::hover(),
    );
    let hov = crate::anim::hover_factor(ui, ("managed_group_row", chat.id), response.hovered());
    if hov > 0.001 {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(crate::theme::RADIUS_SMALL),
            crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
        );
    }
    // The row's click target is the separate open rect below
    // (the row itself is hover-sense, so its response cannot
    // report a press); the press wash reads the pointer state
    // over that same rect, short of the kebab's slot.
    let press = crate::anim::press_factor(
        ui,
        ("managed_group_row", chat.id),
        ui.input(|i| {
            i.pointer.button_down(egui::PointerButton::Primary)
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| rect.contains(p) && p.x < rect.right() - 37.0)
        }),
    );
    if press > 0.001 {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(crate::theme::RADIUS_SMALL),
            crate::theme::glass_wash(t, t.bg_panel_alt, (press * 170.0) as u8),
        );
    }
    let painter = ui.painter();
    let center = Pos2::new(rect.left() + crate::chat_list::AVATAR_CX, rect.center().y);
    crate::widgets::paint_chat_avatar(painter, center, crate::chat_list::AVATAR_R, chat);
    let kebab_rect = egui::Rect::from_center_size(
        Pos2::new(rect.right() - 22.0, rect.center().y),
        Vec2::splat(30.0),
    );
    let text_left = rect.left() + crate::chat_list::TEXT_X;
    let budget = (kebab_rect.left() - 10.0 - text_left).max(0.0);
    let title_font = crate::theme::medium(15.0);
    let title = crate::widgets::ellipsize(ui, &chat.title, title_font.clone(), budget);
    painter.text(
        Pos2::new(text_left, rect.top() + crate::chat_list::TITLE_DY),
        egui::Align2::LEFT_CENTER,
        title,
        title_font,
        rgb(t.text_primary),
    );
    let preview = crate::chat_list::chat_preview_line(chat);
    let snippet_font = FontId::proportional(12.0);
    let preview = crate::widgets::ellipsize(ui, &preview, snippet_font.clone(), budget);
    if !preview.is_empty() {
        painter.text(
            Pos2::new(text_left, rect.top() + crate::chat_list::SNIPPET_DY),
            egui::Align2::LEFT_CENTER,
            preview,
            snippet_font,
            rgb(t.text_secondary),
        );
    }
    let open_rect = egui::Rect::from_min_max(rect.min, Pos2::new(kebab_rect.left(), rect.bottom()));
    if ui
        .interact(open_rect, egui::Id::unique(("managed_group_open", chat.id)), Sense::click())
        .clicked()
    {
        action = ManagedGroupAction::Open;
    }
    let kebab = ui
        .interact(kebab_rect, egui::Id::unique(("managed_group_kebab", chat.id)), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    crate::icons::draw(
        painter,
        crate::icons::Icon::MoreVert,
        kebab_rect.center(),
        18.0,
        rgb(t.text_secondary),
        rgb(t.bg_panel),
    );
    egui::Popup::menu(&kebab)
        .frame(crate::theme::glass_panel_frame(t, crate::theme::RADIUS_PANEL))
        .width(208.0)
        .show(|ui| {
            if crate::widgets::menu_item_danger(
                ui,
                t,
                crate::icons::Icon::Delete,
                &crate::i18n::t("community.unlink_group"),
            )
            .clicked()
            {
                action = ManagedGroupAction::Unlink;
                ui.close();
            }
        });
    crate::anim::stock_popup_enter(
        ui.ctx(),
        egui::Popup::default_response_id(&kebab),
        kebab.rect.center(),
    );
    action
}

/// The menu hairline before a danger item, in the standard
/// soft form (design standard, hairline discipline): inset to
/// the item text line at the soft alpha.
fn menu_hairline(ui: &mut egui::Ui, t: Tokens) {
    ui.add_space(SP_1);
    let (line, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    let inset = crate::theme::LIST_PANEL_INSET_X as f32;
    let c = rgb(t.border);
    ui.painter().hline(
        (line.left() + inset)..=(line.right() - inset),
        line.center().y,
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 100)),
    );
    ui.add_space(SP_1);
}

/// The community home's member and management sections
/// (parity P6): the member list for every member, and for
/// admins the invite link, the group management (link picker
/// and create-subgroup form) and the deactivate action. Pure
/// rendering: every action leaves as a `CommunityIntents`
/// field, and the destructive ones are confirmed by the
/// caller, never executed here.
fn draw_community_admin_sections(
    ui: &mut egui::Ui,
    t: Tokens,
    admin: &mut CommunityAdmin<'_>,
    intents: &mut CommunityIntents,
) {
    if !admin.members.is_empty() {
        ui.add_space(SP_SECTION);
        section_label(
            ui,
            t,
            &format!("{} ({})", crate::i18n::t("profile.members"), admin.members.len()),
        );
        const MEMBER_LIST_CAP: usize = 200;
        let shown = admin.members.len().min(MEMBER_LIST_CAP);
        for (row, photo) in admin.members.iter().take(shown).zip(admin.member_photos.iter()) {
            let kebab = crate::profile::dock::dock_member_row_labeled(
                ui,
                t,
                &row.label,
                row.detail.as_deref(),
                row.admin,
                admin.own_admin,
                *photo,
                row.member_label.as_deref(),
            );
            if let Some(kebab) = kebab {
                egui::Popup::menu(&kebab)
                    .frame(crate::theme::glass_panel_frame(t, crate::theme::RADIUS_PANEL))
                    .width(208.0)
                    .show(|ui| {
                        let (label_key, op) = if row.admin {
                            ("profile.remove_admin", backend::ParticipantOp::Demote)
                        } else {
                            ("profile.make_admin", backend::ParticipantOp::Promote)
                        };
                        if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::Shield),
                            &crate::i18n::t(label_key),
                            None,
                        )
                        .clicked()
                        {
                            intents.member_op = Some((row.address.clone(), op));
                            ui.close();
                        }
                        menu_hairline(ui, t);
                        if crate::widgets::menu_item_danger(
                            ui,
                            t,
                            crate::icons::Icon::Delete,
                            &crate::i18n::t("community.remove_from_community"),
                        )
                        .clicked()
                        {
                            intents.remove_member = Some((row.address.clone(), row.label.clone()));
                            ui.close();
                        }
                    });
                crate::anim::stock_popup_enter(
                    ui.ctx(),
                    egui::Popup::default_response_id(&kebab),
                    kebab.rect.center(),
                );
            }
            ui.add_space(SP_1);
        }
        if admin.members.len() > shown {
            ui.label(
                RichText::new(format!("... and {} more", admin.members.len() - shown))
                    .size(12.0)
                    .color(rgb(t.text_faint)),
            );
        }
    }
    if !admin.own_admin {
        return;
    }
    // Invite link: fetched on demand, like the group dock's;
    // copy and reset act on the last reported link.
    ui.add_space(SP_SECTION);
    section_label(ui, t, &crate::i18n::t("community.invite"));
    match admin.invite_link {
        Some(link) => {
            ui.label(RichText::new(link).size(10.0).color(rgb(t.text_faint)));
            ui.horizontal(|ui| {
                if crate::widgets::button(
                    ui,
                    t,
                    &crate::i18n::t("profile.copy_link"),
                    crate::widgets::Btn::Secondary,
                )
                .clicked()
                {
                    intents.copy_invite = true;
                }
                if crate::widgets::button(
                    ui,
                    t,
                    &crate::i18n::t("profile.reset_link"),
                    crate::widgets::Btn::Secondary,
                )
                .clicked()
                {
                    intents.reset_invite = true;
                }
            });
        }
        None => {
            if crate::widgets::button(
                ui,
                t,
                &crate::i18n::t("profile.show_link"),
                crate::widgets::Btn::Secondary,
            )
            .clicked()
            {
                intents.fetch_invite = true;
            }
        }
    }
    // Group management: the link picker and the
    // create-subgroup form.
    ui.add_space(SP_SECTION);
    section_label(ui, t, &crate::i18n::t("community.manage_groups"));
    if crate::widgets::button(
        ui,
        t,
        &crate::i18n::t("community.link_group"),
        crate::widgets::Btn::Secondary,
    )
    .clicked()
    {
        intents.toggle_link_picker = true;
    }
    if admin.link_picker_open {
        ui.add_space(SP_2);
        if admin.link_candidates.is_empty() {
            ui.label(
                RichText::new(crate::i18n::t("community.no_link_candidates"))
                    .size(12.0)
                    .color(rgb(t.text_faint)),
            );
        }
        for cand in &admin.link_candidates {
            if other_group_row(ui, cand, t, "community.link") {
                intents.link_group = Some(cand.id);
            }
        }
    }
    ui.add_space(SP_3);
    ui.label(
        RichText::new(crate::i18n::t("community.new_subgroup"))
            .size(12.0)
            .color(rgb(t.text_secondary)),
    );
    ui.horizontal(|ui| {
        let w = (ui.available_width() - 148.0).max(120.0);
        crate::widgets::text_field(ui, t, admin.subgroup_name, "", w);
        if crate::widgets::button(
            ui,
            t,
            &crate::i18n::t("community.create_subgroup"),
            crate::widgets::Btn::Secondary,
        )
        .clicked()
        {
            intents.create_subgroup = true;
        }
    });
    // Danger zone: deactivation is confirmed by the caller.
    ui.add_space(SP_SECTION);
    if crate::widgets::button(
        ui,
        t,
        &crate::i18n::t("community.deactivate"),
        crate::widgets::Btn::Danger,
    )
    .clicked()
    {
        intents.deactivate = true;
    }
}

/// Draws the community home into `ui` (the shell gives it the
/// content area next to the rail). `announcement` is the
/// community's announcement chat, `groups` the groups the user
/// is in and `other_groups` the ones they can still join, all
/// resolved by the caller. `cover` is the demo photo texture
/// used as the cover image, when available.
#[allow(clippy::too_many_arguments)]
pub fn community_home(
    ui: &mut egui::Ui,
    t: Tokens,
    community: &Community,
    announcement: &Chat,
    groups: &[&Chat],
    other_groups: &[&Chat],
    desc_expanded: bool,
    cover: Option<&egui::TextureHandle>,
    scroll_offset: f32,
    community_photo: Option<egui::TextureId>,
    admin: Option<CommunityAdmin<'_>>,
) -> CommunityIntents {
    let mut intents = CommunityIntents::default();
    let total_groups = community.group_chat_ids.len() + community.other_group_ids.len();
    egui::ScrollArea::vertical().vertical_scroll_offset(scroll_offset).show(ui, |ui| {
        ui.add_space(SP_2);
        ui.horizontal(|ui| {
            if icon_button(
                ui,
                t,
                crate::icons::Icon::ChevronLeft,
                34.0,
                &crate::i18n::t("common.back"),
                None,
            )
            .clicked()
            {
                intents.back = true;
            }
        });
        ui.add_space(SP_1);
        // Cover: full content width, about 150 px tall, rounded.
        // The rect is allocated exactly and the texture painted
        // into it with a center-crop UV, so the photo fills the
        // whole width instead of sizing to its own aspect.
        let cover_size = Vec2::new(ui.available_width(), 150.0);
        let (cover_rect, _) = ui.allocate_exact_size(cover_size, egui::Sense::hover());
        match cover {
            Some(tex) => {
                egui::Image::new(tex)
                    .uv(center_crop_uv(cover_size))
                    .corner_radius(12)
                    .paint_at(ui, cover_rect);
            }
            None => {
                ui.painter().rect_filled(
                    cover_rect,
                    CornerRadius::same(12),
                    crate::widgets::avatar_color(community.avatar_seed),
                );
            }
        }
        // The identity block's avatar overlaps the cover's bottom
        // edge by half its height (a negative advance pulls the
        // cursor back over the cover, and the avatar paints after
        // it, so it sits on top of the banner's edge).
        ui.add_space(-40.0);
        // Centered identity block: avatar, name, the community
        // subtitle line and the description with its toggle.
        ui.vertical_centered(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(80.0), egui::Sense::hover());
            // The community's own picture through the shared
            // avatar dispatch (total audit P1-31): the parent
            // group's chat carries the identity, so its real
            // photo paints when the caller resolved one, exactly
            // like every other surface. Before, this block called
            // the non-photo painter unconditionally and a
            // community's real picture could never show.
            crate::widgets::paint_chat_avatar_photo(
                ui.painter(),
                rect.center(),
                40.0,
                announcement,
                community_photo,
            );
            ui.add_space(SP_2);
            ui.label(
                RichText::new(&community.name)
                    .font(crate::theme::semibold(19.0))
                    .color(rgb(t.text_primary)),
            );
            ui.label(
                RichText::new(crate::i18n::tf(
                    "community.subtitle",
                    "count",
                    &total_groups.to_string(),
                ))
                .size(12.0)
                .color(rgb(t.text_faint)),
            );
            if let Some(desc) = &community.description {
                ui.add_space(SP_2);
                let needs_toggle = desc.chars().count() > DESC_COLLAPSED_CHARS;
                let shown = if desc_expanded { desc.clone() } else { collapsed_description(desc) };
                ui.allocate_ui_with_layout(
                    Vec2::new(440.0_f32.min(ui.available_width()), 0.0),
                    egui::Layout::top_down(egui::Align::Center),
                    |ui| {
                        // The expander rides inline at the end of
                        // the description text (accent section of
                        // the same layout job), not on its own line
                        // below it; the whole label is the toggle's
                        // click target.
                        let mut job = egui::text::LayoutJob {
                            halign: egui::Align::Center,
                            ..Default::default()
                        };
                        job.append(
                            &shown,
                            0.0,
                            egui::TextFormat {
                                font_id: FontId::proportional(12.0),
                                color: rgb(t.text_secondary),
                                ..Default::default()
                            },
                        );
                        if needs_toggle {
                            let label = if desc_expanded {
                                crate::i18n::t("community.show_less")
                            } else {
                                crate::i18n::t("community.read_more")
                            };
                            job.append(
                                &format!(" {label}"),
                                0.0,
                                egui::TextFormat {
                                    font_id: FontId::proportional(12.0),
                                    color: rgb(t.accent),
                                    ..Default::default()
                                },
                            );
                        }
                        let label = egui::Label::new(job);
                        let resp = if needs_toggle {
                            ui.add(label.sense(Sense::click()))
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                        } else {
                            ui.add(label)
                        };
                        if needs_toggle && resp.clicked() {
                            intents.toggle_desc = true;
                        }
                    },
                );
            }
        });
        ui.add_space(SP_SECTION);
        // The sections share one inset axis with the chat list:
        // the frame's inner margin is the shared list panel
        // inset, so section labels start where chat row text
        // starts and row timestamps and badges end the same
        // distance from the pane edge as they do in the list,
        // instead of running to the edge of the content area.
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(crate::theme::LIST_PANEL_INSET_X, 8))
            .show(ui, |ui| {
                // Announcement chat: the shared chat row with a
                // megaphone avatar override, keeping its own unread
                // badge.
                section_label(ui, t, &crate::i18n::t("community.announcements"));
                if crate::chat_list::chat_row(
                    ui,
                    announcement,
                    false,
                    false,
                    t,
                    None,
                    Some(crate::icons::Icon::Megaphone),
                    None,
                    &[],
                )
                .clicked()
                {
                    intents.open_chat = Some(announcement.id);
                }
                ui.add_space(SP_SECTION);
                // The groups the user is in. Admins get the
                // managed row (same metrics, a kebab at the right
                // edge carrying the unlink action); everyone else
                // gets the plain shared row.
                section_label(ui, t, &crate::i18n::t("community.your_groups"));
                let admin_view = admin.as_ref().is_some_and(|a| a.own_admin);
                for group in groups {
                    if admin_view {
                        match managed_group_row(ui, t, group) {
                            ManagedGroupAction::Open => intents.open_chat = Some(group.id),
                            ManagedGroupAction::Unlink => {
                                intents.unlink_group = Some(group.id);
                            }
                            ManagedGroupAction::None => {}
                        }
                    } else if crate::chat_list::chat_row(
                        ui,
                        group,
                        false,
                        false,
                        t,
                        None,
                        None,
                        None,
                        &[],
                    )
                    .clicked()
                    {
                        intents.open_chat = Some(group.id);
                    }
                }
                if let Some(mut admin) = admin {
                    draw_community_admin_sections(ui, t, &mut admin, &mut intents);
                }
                // Groups of this community the user has not joined yet.
                if !other_groups.is_empty() {
                    ui.add_space(SP_SECTION);
                    section_label(ui, t, &crate::i18n::t("community.other_groups"));
                    for group in other_groups {
                        if other_group_row(ui, group, t, "community.join") {
                            intents.join_group = Some(group.id);
                        }
                    }
                }
            });
    });
    intents
}

impl App {
    /// The communities the chat list can offer right now. Demo
    /// mode: the deterministic demo set. Live mode: derived from
    /// group chats carrying a `community` name tag, grouped by
    /// name; live data has no dedicated announcement group, so
    /// the first tagged group (by id) stands in as the
    /// announcement chat and the rest are its groups. Empty when
    /// nothing is tagged, which hides the chat-list entry.
    pub(crate) fn communities(&self) -> Vec<Community> {
        if !self.live_mode {
            let mut all = demo_communities();
            all.extend(self.session_communities.iter().cloned());
            return all;
        }
        let mut names: Vec<&str> = Vec::new();
        for chat in &self.chats {
            if chat.kind == ChatKind::Group {
                if let Some(name) = chat.community.as_deref() {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
        }
        names.sort_unstable();
        let mut derived: Vec<Community> = names
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                let mut tagged: Vec<&Chat> = self
                    .chats
                    .iter()
                    .filter(|c| c.kind == ChatKind::Group && c.community.as_deref() == Some(name))
                    .collect();
                tagged.sort_by_key(|c| c.id);
                // The announcement chat is the community parent
                // itself when the real hierarchy is known (it
                // maps to itself in chat_parents); the first
                // tagged group is only the fallback for rosters
                // whose metadata has not landed yet.
                let announcement_chat_id = tagged
                    .iter()
                    .find(|c| self.chat_parents.get(&c.id) == Some(&c.id))
                    .map_or_else(|| tagged.first().map_or(0, |c| c.id), |c| c.id);
                let avatar_seed = name.bytes().fold(0u8, u8::wrapping_add);
                Community {
                    id: i as u64 + 1,
                    name: name.to_owned(),
                    description: None,
                    avatar_seed,
                    announcement_chat_id,
                    group_chat_ids: tagged
                        .iter()
                        .map(|c| c.id)
                        .filter(|id| *id != announcement_chat_id)
                        .collect(),
                    other_group_ids: Vec::new(),
                }
            })
            .collect();
        // Session-created communities merge in unless the tagged
        // chats already derive one under the same name.
        for created in &self.session_communities {
            if !derived.iter().any(|c| c.name == created.name) {
                derived.push(created.clone());
            }
        }
        derived
    }

    /// Draws the open community's home into the content area
    /// (the shell calls this instead of the conversation while
    /// `open_community` is set) and applies its intents.
    pub(crate) fn draw_community_home(&mut self, root: &mut egui::Ui) {
        let Some(open_id) = self.open_community else {
            return;
        };
        let t = self.tokens();
        // Live mode never uses the demo photo: its cover is the
        // seed-color fallback.
        if !self.live_mode && self.media.photo_texture.is_none() {
            self.media.photo_texture = Some(crate::media::demo_photo(root.ctx()));
        }
        let communities = self.communities();
        let Some(community) = communities.iter().find(|c| c.id == open_id).cloned() else {
            // The community vanished (live data changed under the
            // open screen): close it and fall back to the chat.
            self.open_community = None;
            self.community_home_from_overview = false;
            self.draw_conversation(root);
            return;
        };
        // Resolve the announcement chat: the chat store first
        // (live mode's stand-in is a real listed group), then the
        // detached slot (the demo announcement once it has been
        // opened), then the session store (a created community's
        // announcement), then the demo data itself. It never
        // resolves by joining the chat list.
        let announcement: Chat = {
            let resolved = self
                .chats
                .iter()
                .find(|c| c.id == community.announcement_chat_id)
                .cloned()
                .or_else(|| {
                    self.detached_chat
                        .as_ref()
                        .filter(|c| c.id == community.announcement_chat_id)
                        .cloned()
                })
                .or_else(|| {
                    self.session_announcements
                        .iter()
                        .find(|c| c.id == community.announcement_chat_id)
                        .cloned()
                })
                .or_else(|| {
                    (!self.live_mode
                        && community.announcement_chat_id == demo_announcement_chat().id)
                        .then(demo_announcement_chat)
                });
            match resolved {
                Some(c) => c,
                None => {
                    self.open_community = None;
                    self.community_home_from_overview = false;
                    self.draw_conversation(root);
                    return;
                }
            }
        };
        // Groups you're in: the community's listed groups plus
        // any other groups joined this session, all resolved
        // from the chat store.
        let mut your_ids: Vec<u64> = community.group_chat_ids.clone();
        for id in &community.other_group_ids {
            if self.joined_community_groups.contains(id) && !your_ids.contains(id) {
                your_ids.push(*id);
            }
        }
        let groups_owned: Vec<Chat> = your_ids
            .iter()
            .filter_map(|id| self.chats.iter().find(|c| c.id == *id).cloned())
            .collect();
        let groups: Vec<&Chat> = groups_owned.iter().collect();
        // Other groups: not joined yet, demo data only.
        let other_owned: Vec<Chat> = if self.live_mode {
            Vec::new()
        } else {
            let all = demo_community_other_groups(community.id);
            community
                .other_group_ids
                .iter()
                .filter(|id| !self.joined_community_groups.contains(id))
                .filter_map(|id| all.iter().find(|c| c.id == *id).cloned())
                .collect()
        };
        let other_groups: Vec<&Chat> = other_owned.iter().collect();
        let desc_expanded = self.community_desc_expanded == Some(community.id);
        let cover = if self.live_mode { None } else { self.media.photo_texture.clone() };
        // Live member and management data (parity P6): the
        // parent group's member list is fetched once per home
        // session, the way the profile dock fetches it per
        // opening. While the link picker is open, the own-admin
        // role of every unlinked group is learned the same way:
        // the picker can only list groups this account admins,
        // and the role exists only once a group's members were
        // fetched.
        let announcement_id = community.announcement_chat_id;
        if self.live_mode && !self.groups.members_requested.contains(&announcement_id) {
            self.groups.members_requested.insert(announcement_id);
            if let Some(jid) = self.live_jid(announcement_id) {
                self.send_backend(backend::BackendCommand::FetchGroupMembers { chat_jid: jid });
            }
        }
        if self.live_mode && self.community_link_picker_open {
            let unknown: Vec<u64> = self
                .chats
                .iter()
                .filter(|c| {
                    c.kind == ChatKind::Group
                        && c.community.is_none()
                        && !self.groups.members_requested.contains(&c.id)
                })
                .map(|c| c.id)
                .collect();
            for id in unknown {
                self.groups.members_requested.insert(id);
                if let Some(jid) = self.live_jid(id) {
                    self.send_backend(backend::BackendCommand::FetchGroupMembers { chat_jid: jid });
                }
            }
        }
        let ctx = root.ctx().clone();
        let members_owned: Vec<crate::state::MemberRow> = if self.live_mode {
            self.groups.live_members.get(&announcement_id).cloned().unwrap_or_default()
        } else {
            Vec::new()
        };
        let member_photos: Vec<Option<egui::TextureId>> = members_owned
            .iter()
            .map(|row| self.member_avatar_photo(&ctx, &row.address, &row.label))
            .collect();
        let own_admin = self.groups.own_admin.get(&announcement_id).copied().unwrap_or(false);
        let invite_owned: Option<String> = self
            .groups
            .invite_link
            .as_ref()
            .filter(|(id, _)| *id == announcement_id)
            .map(|(_, link)| link.clone());
        let candidates_owned: Vec<Chat> = if self.live_mode {
            self.chats
                .iter()
                .filter(|c| {
                    c.kind == ChatKind::Group
                        && c.community.is_none()
                        && c.id != announcement_id
                        && self.groups.own_admin.get(&c.id).copied().unwrap_or(false)
                })
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        // The community's own picture (P1-31): the announcement
        // chat IS the parent group, so its JID drives the shared
        // avatar pipeline; the decoded texture (when one exists)
        // goes to the home's identity block.
        let community_photo = if self.live_mode {
            let jid = self.live.as_ref().and_then(|s| s.jids.get(&announcement.id).cloned());
            match jid {
                Some(jid) => {
                    self.ensure_avatar(root.ctx(), &jid);
                    self.live_avatar(&jid).map(|h| h.id())
                }
                None => None,
            }
        } else {
            None
        };
        let mut intents = CommunityIntents::default();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(rgb(t.bg_panel))).show(
            root,
            |ui| {
                intents = community_home(
                    ui,
                    t,
                    &community,
                    &announcement,
                    &groups,
                    &other_groups,
                    desc_expanded,
                    cover.as_ref(),
                    self.qa_community_scroll,
                    community_photo,
                    if self.live_mode {
                        Some(CommunityAdmin {
                            members: &members_owned,
                            member_photos: member_photos.clone(),
                            own_admin,
                            invite_link: invite_owned.as_deref(),
                            link_candidates: candidates_owned.iter().collect(),
                            link_picker_open: self.community_link_picker_open,
                            subgroup_name: &mut self.community_subgroup_name,
                        })
                    } else {
                        None
                    },
                );
            },
        );
        if intents.back {
            if self.community_home_from_overview {
                // Entered from the Communities overview panel:
                // back returns the content area to the
                // conversation; the overview panel stays on the
                // left (the screen is already Communities, the
                // assignment just makes that explicit).
                self.community_home_from_overview = false;
                self.open_community = None;
                self.screen = crate::state::Screen::Communities;
            } else {
                self.open_community = None;
            }
        }
        if intents.toggle_desc {
            self.community_desc_expanded = if desc_expanded { None } else { Some(community.id) };
        }
        if let Some(id) = intents.join_group {
            let joined_title = other_owned.iter().find(|c| c.id == id).map(|c| c.title.clone());
            self.join_community_group(id);
            if let Some(title) = joined_title {
                self.notice =
                    Some(Notice::info(crate::i18n::tf("community.joined", "name", &title)));
            }
        }
        if let Some(id) = intents.open_chat {
            self.open_community_chat(id);
        }
        // Management intents (parity P6). Destructive ones park
        // a confirmation; the rest send their command at once.
        if intents.fetch_invite || intents.reset_invite {
            if let Some(jid) = self.live_jid(announcement_id) {
                self.send_backend(backend::BackendCommand::FetchGroupInviteLink {
                    chat_jid: jid,
                    reset: intents.reset_invite,
                });
            }
        }
        if intents.copy_invite {
            if let Some(link) = &invite_owned {
                root.ctx().copy_text(link.clone());
                self.notice = Some(Notice::info(crate::i18n::t("profile.link_copied")));
            }
        }
        if intents.toggle_link_picker {
            self.community_link_picker_open = !self.community_link_picker_open;
        }
        if let Some(id) = intents.link_group {
            self.link_community_group(announcement_id, id);
        }
        if let Some(id) = intents.unlink_group {
            let name = groups_owned
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.title.clone())
                .unwrap_or_default();
            self.ui_confirm = Some(crate::state::UiConfirm::UnlinkCommunityGroup {
                parent_id: announcement_id,
                group_id: id,
                name,
            });
        }
        if intents.create_subgroup {
            self.create_community_subgroup(announcement_id);
        }
        if let Some((address, op)) = intents.member_op {
            self.group_participant_op(announcement_id, Some(address), op);
        }
        if let Some((address, name)) = intents.remove_member {
            self.ui_confirm = Some(crate::state::UiConfirm::RemoveCommunityMember {
                parent_id: announcement_id,
                address,
                name,
            });
        }
        if intents.deactivate {
            self.ui_confirm = Some(crate::state::UiConfirm::DeactivateCommunity {
                parent_id: announcement_id,
                name: community.name.clone(),
            });
        }
    }

    /// The Communities overview: one row per community, opening
    /// its home. A LEFT panel (the owner's left-panel rule:
    /// secondary lists never take the full screen); the home
    /// opens in the content area beside it, and the home's back
    /// affordance returns the content area to the conversation
    /// while this panel stays.
    pub(crate) fn draw_communities_panel(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        // Live mode never uses the demo photo (seed-color thumbs).
        if !self.live_mode && self.media.photo_texture.is_none() {
            self.media.photo_texture = Some(crate::media::demo_photo(root.ctx()));
        }
        let communities = self.communities();
        let cover = if self.live_mode { None } else { self.media.photo_texture.clone() };
        let mut open: Option<u64> = None;
        let mut back = false;
        let mut create_new = false;
        let panel = self
            .list_panel(root)
            .frame(
                egui::Frame::new()
                    .fill(rgb(t.bg_panel))
                    .inner_margin(egui::Margin::symmetric(crate::theme::LIST_PANEL_INSET_X, 8)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    // In portrait mode the back chevron goes
                    // away: the bottom navigation bar is the way
                    // between tabs.
                    if !self.portrait
                        && icon_button(
                            ui,
                            t,
                            crate::icons::Icon::ChevronLeft,
                            28.0,
                            &crate::i18n::t("common.back"),
                            None,
                        )
                        .clicked()
                    {
                        back = true;
                    }
                    ui.label(
                        RichText::new(crate::i18n::t("communities.title"))
                            .font(crate::theme::medium(15.0))
                            .color(rgb(t.text_primary)),
                    );
                });
                ui.add_space(SP_3);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .content_margin(egui::Margin {
                        bottom: self.nav_scroll_inset() as i8,
                        ..Default::default()
                    })
                    .show(ui, |ui| {
                        // The lead row: New community, in the same row
                        // language as the community rows below it (a
                        // squircle where their cover thumbs sit, the
                        // same label position, the same chevron). It
                        // opens the create form; the backend exposes a
                        // real creation command, so the row is a working
                        // entry, not a placeholder.
                        let (rect, resp) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 62.0),
                            Sense::click(),
                        );
                        let hov =
                            crate::anim::hover_factor(ui, "community_new_row", resp.hovered());
                        if hov > 0.001 {
                            ui.painter().rect_filled(
                                rect,
                                CornerRadius::same(crate::theme::RADIUS_SMALL),
                                crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
                            );
                        }
                        let press = crate::anim::press_factor(
                            ui,
                            "community_new_row",
                            resp.is_pointer_button_down_on(),
                        );
                        if press > 0.001 {
                            ui.painter().rect_filled(
                                rect,
                                CornerRadius::same(crate::theme::RADIUS_SMALL),
                                crate::theme::glass_wash(t, t.bg_panel_alt, (press * 170.0) as u8),
                            );
                        }
                        let thumb = egui::Rect::from_min_size(
                            Pos2::new(rect.left() + 8.0, rect.center().y - 24.0),
                            Vec2::splat(48.0),
                        );
                        ui.painter().rect_filled(
                            thumb,
                            CornerRadius::same(10),
                            rgb(t.bg_panel_alt),
                        );
                        crate::icons::draw(
                            ui.painter(),
                            crate::icons::Icon::Plus,
                            thumb.center(),
                            crate::icons::SIZE_TAB,
                            rgb(t.accent),
                            rgb(t.bg_panel_alt),
                        );
                        ui.painter().text(
                            Pos2::new(thumb.right() + 12.0, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            crate::i18n::t("community.new"),
                            crate::theme::semibold(15.0),
                            rgb(t.text_primary),
                        );
                        crate::icons::draw(
                            ui.painter(),
                            crate::icons::Icon::ChevronRight,
                            Pos2::new(rect.right() - 18.0, rect.center().y),
                            crate::icons::SIZE_CONTROL,
                            rgb(t.text_faint),
                            rgb(t.bg_panel),
                        );
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            create_new = true;
                        }
                        if communities.is_empty() {
                            ui.add_space(SP_6);
                            ui.label(
                                RichText::new(crate::i18n::t("communities.empty"))
                                    .size(15.0)
                                    .color(rgb(t.text_secondary)),
                            );
                        }
                        for community in &communities {
                            let total =
                                community.group_chat_ids.len() + community.other_group_ids.len();
                            let (rect, resp) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), 62.0),
                                Sense::click(),
                            );
                            let hov = crate::anim::hover_factor(
                                ui,
                                ("community_overview_row", community.id),
                                resp.hovered(),
                            );
                            if hov > 0.001 {
                                ui.painter().rect_filled(
                                    rect,
                                    CornerRadius::same(crate::theme::RADIUS_SMALL),
                                    crate::theme::glass_wash(
                                        t,
                                        t.bg_panel_alt,
                                        (hov * 115.0) as u8,
                                    ),
                                );
                            }
                            let press = crate::anim::press_factor(
                                ui,
                                ("community_overview_row", community.id),
                                resp.is_pointer_button_down_on(),
                            );
                            if press > 0.001 {
                                ui.painter().rect_filled(
                                    rect,
                                    CornerRadius::same(crate::theme::RADIUS_SMALL),
                                    crate::theme::glass_wash(
                                        t,
                                        t.bg_panel_alt,
                                        (press * 170.0) as u8,
                                    ),
                                );
                            }
                            // Cover thumb: the demo photo center-cropped
                            // into a small rounded rect, or the seed color.
                            let thumb = egui::Rect::from_min_size(
                                Pos2::new(rect.left() + 8.0, rect.center().y - 24.0),
                                Vec2::splat(48.0),
                            );
                            match &cover {
                                Some(tex) => {
                                    egui::Image::new(tex)
                                        .uv(center_crop_uv(thumb.size()))
                                        .corner_radius(10)
                                        .paint_at(ui, thumb);
                                }
                                None => {
                                    ui.painter().rect_filled(
                                        thumb,
                                        CornerRadius::same(10),
                                        crate::widgets::avatar_color(community.avatar_seed),
                                    );
                                }
                            }
                            let text_left = thumb.right() + 12.0;
                            let name_budget = (rect.right() - 40.0 - text_left).max(0.0);
                            let name_text = crate::widgets::ellipsize(
                                ui,
                                &community.name,
                                crate::theme::semibold(15.0),
                                name_budget,
                            );
                            ui.painter().text(
                                Pos2::new(text_left, rect.center().y - 10.0),
                                egui::Align2::LEFT_CENTER,
                                name_text,
                                crate::theme::semibold(15.0),
                                rgb(t.text_primary),
                            );
                            ui.painter().text(
                                Pos2::new(text_left, rect.center().y + 12.0),
                                egui::Align2::LEFT_CENTER,
                                crate::i18n::tf(
                                    "community.groups_count",
                                    "count",
                                    &total.to_string(),
                                ),
                                FontId::proportional(12.0),
                                rgb(t.text_faint),
                            );
                            crate::icons::draw(
                                ui.painter(),
                                crate::icons::Icon::ChevronRight,
                                Pos2::new(rect.right() - 18.0, rect.center().y),
                                crate::icons::SIZE_CONTROL,
                                rgb(t.text_faint),
                                rgb(t.bg_panel),
                            );
                            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                open = Some(community.id);
                            }
                        }
                    });
            });
        crate::widgets::soft_right_edge(root, panel.response.rect, t);
        if create_new {
            self.community_create_open = true;
        }
        self.draw_create_community_dialog(root.ctx());
        if back {
            self.screen = crate::state::Screen::Chats;
            self.filter = core::filter::ChatFilter::All;
        }
        if let Some(id) = open {
            // The panel stays; the home opens in the content
            // area and its back affordance returns the content
            // area to the conversation (the from-overview branch
            // in draw_community_home).
            self.community_home_from_overview = true;
            self.open_community = Some(id);
        }
    }

    /// Opens a chat picked on the community home: closes the
    /// home, remembers it as the place the back affordance in
    /// the conversation header returns to, and selects the chat.
    /// The demo announcement chat is not part of demo_chats()
    /// and never joins it: it loads into the detached slot with
    /// its announcement messages, and the normal selection path
    /// reads it from there. An already-loaded detached copy is
    /// kept, so messages sent in the announcement view survive
    /// leaving and returning. In live mode every community chat
    /// is a real listed group already, so nothing is loaded.
    pub(crate) fn open_community_chat(&mut self, id: u64) {
        self.community_origin = self.open_community;
        self.open_community = None;
        self.community_home_from_overview = false;
        if !self.chats.iter().any(|c| c.id == id)
            && self.detached_chat.as_ref().is_none_or(|c| c.id != id)
        {
            if !self.live_mode && id == demo_announcement_chat().id {
                let mut announcement = demo_announcement_chat();
                announcement.messages = demo_announcement_messages();
                self.detached_chat = Some(announcement);
            } else if let Some(chat) =
                self.session_announcements.iter().find(|c| c.id == id).cloned()
            {
                // A session-created community's announcement chat
                // loads into the detached slot the same way, so it
                // can be read and posted to without ever joining
                // the chat list.
                self.detached_chat = Some(chat);
            }
        }
        self.select_chat_now(id);
    }

    /// Registers a community created this session: the community
    /// joins the session list (so the overview shows it and its
    /// home resolves) together with its announcement chat, which
    /// is stored outside the chat list under the announcement
    /// rule. `announcement_chat_id` is the chat's numeric id: in
    /// live mode the JID hash of the community JID (the
    /// announcement group shares that JID), in demo mode a fresh
    /// local id.
    fn add_session_community(&mut self, name: String, announcement_chat_id: u64) {
        let avatar_seed = name.bytes().fold(0u8, u8::wrapping_add);
        let id = self.communities().iter().map(|c| c.id).max().unwrap_or(0) + 1;
        self.session_announcements.push(Chat {
            id: announcement_chat_id,
            kind: ChatKind::Group,
            title: crate::i18n::t("community.announcements"),
            subtitle: "Community announcements".to_owned(),
            preview: String::new(),
            preview_sender: None,
            preview_mine: false,
            preview_delivery: None,
            preview_media: None,
            time: String::new(),
            unread: 0,
            mentions: 0,
            pinned: false,
            muted: false,
            mute_until: 0,
            favorite: false,
            avatar_seed,
            verified: false,
            meta_ai: false,
            community: Some(name.clone()),
            archived: false,
            locked: false,
            ephemeral_secs: 0,
            messages: Vec::new(),
        });
        self.session_communities.push(Community {
            id,
            name,
            description: None,
            avatar_seed,
            announcement_chat_id,
            group_chat_ids: Vec::new(),
            other_group_ids: Vec::new(),
        });
    }

    /// Applies the backend's CommunityCreated event: the new
    /// community is registered under the JID hash the live layer
    /// uses for chat ids, and the JID is recorded so opening its
    /// announcement chat resolves and sends against the real
    /// community JID.
    pub(crate) fn register_created_community(&mut self, jid: &str, name: &str) {
        let chat_id = crate::live::jid_id(jid);
        if let Some(session) = &mut self.live {
            session.jids.insert(chat_id, jid.to_owned());
        }
        if self.session_communities.iter().any(|c| c.name == name) {
            return;
        }
        self.add_session_community(name.to_owned(), chat_id);
        self.notice = Some(Notice::info(crate::i18n::tf("community.created_notice", "name", name)));
    }

    /// Links one group into a community (the home's link
    /// picker). The backend's per-group ChatCommunity answer
    /// files the group under the community; a refusal lands as
    /// the CommunityGroupsLinked failure notice.
    pub(crate) fn link_community_group(&mut self, parent_id: u64, group_id: u64) {
        let (Some(parent_jid), Some(group_jid)) =
            (self.live_jid(parent_id), self.live_jid(group_id))
        else {
            return;
        };
        self.send_backend(backend::BackendCommand::LinkCommunityGroups {
            community_jid: parent_jid,
            group_jids: vec![group_jid],
        });
        self.community_link_picker_open = false;
        self.notice = Some(Notice::busy(crate::i18n::t("community.linking")));
    }

    /// Unlinks one group from its community, after the
    /// confirmation modal. The group survives as a standalone
    /// group; the backend's ChatCommunity answer clears its
    /// parent locally.
    pub(crate) fn unlink_community_group(&mut self, parent_id: u64, group_id: u64) {
        let (Some(parent_jid), Some(group_jid)) =
            (self.live_jid(parent_id), self.live_jid(group_id))
        else {
            return;
        };
        self.send_backend(backend::BackendCommand::UnlinkCommunityGroups {
            community_jid: parent_jid,
            group_jids: vec![group_jid],
        });
        self.notice = Some(Notice::busy(crate::i18n::t("community.unlinking")));
    }

    /// Creates a subgroup in the community from the home's
    /// name draft. An empty name is spoken as a validation
    /// notice, never a silent no-op.
    pub(crate) fn create_community_subgroup(&mut self, parent_id: u64) {
        let name = self.community_subgroup_name.trim().to_owned();
        if name.is_empty() {
            self.notice = Some(Notice::info(crate::i18n::t("community.name_required")));
            return;
        }
        let Some(parent_jid) = self.live_jid(parent_id) else {
            return;
        };
        self.community_subgroup_name.clear();
        self.send_backend(backend::BackendCommand::CreateCommunitySubgroup {
            community_jid: parent_jid,
            name,
        });
        self.notice = Some(Notice::busy(crate::i18n::t("community.creating_subgroup")));
    }

    /// Removes one member from the whole community, after the
    /// confirmation modal: the parent group and every linked
    /// subgroup lose them at once. The refreshed parent member
    /// list is the backend's answer.
    pub(crate) fn remove_community_member(&mut self, parent_id: u64, address: String) {
        let Some(parent_jid) = self.live_jid(parent_id) else {
            return;
        };
        self.send_backend(backend::BackendCommand::RemoveCommunityParticipants {
            community_jid: parent_jid,
            participants: vec![address],
        });
        self.notice = Some(Notice::busy(crate::i18n::t("notice.updating_members")));
    }

    /// Deactivates the community, after the confirmation
    /// modal. The local removal waits for the backend's
    /// CommunityDeactivated answer.
    pub(crate) fn deactivate_community(&mut self, parent_id: u64) {
        let Some(parent_jid) = self.live_jid(parent_id) else {
            return;
        };
        self.send_backend(backend::BackendCommand::DeactivateCommunity {
            community_jid: parent_jid,
        });
        self.notice = Some(Notice::busy(crate::i18n::t("community.deactivating")));
    }

    /// Applies the backend's CommunityDeactivated answer: the
    /// subgroups keep their chats and lose the parent tag, a
    /// session-created registration under the same name goes,
    /// and the parent chat is deleted like any deleted chat.
    /// An open home for the community closes.
    pub(crate) fn apply_community_deactivated(&mut self, community_jid: &str) {
        let parent_id = crate::live::jid_id(community_jid);
        let children: Vec<u64> = self
            .chat_parents
            .iter()
            .filter(|(_, p)| **p == parent_id)
            .map(|(c, _)| *c)
            .filter(|c| *c != parent_id)
            .collect();
        for child in children {
            self.chat_parents.remove(&child);
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == child) {
                chat.community = None;
            }
        }
        self.chat_parents.remove(&parent_id);
        if let Some(parent) = self.chats.iter().find(|c| c.id == parent_id) {
            let name = parent.title.clone();
            self.session_communities.retain(|c| c.name != name);
        }
        self.apply_chat_deleted(parent_id, community_jid);
        self.open_community = None;
        self.community_home_from_overview = false;
        self.notice = Some(Notice::info(crate::i18n::t("community.deactivated")));
    }

    /// Executes the "New community" form. Returns false when
    /// validation fails; the dialog then stays open with the error
    /// shown inline. Live mode sends the backend command and the
    /// community lands when its CommunityCreated event arrives;
    /// demo mode creates the community locally, right away.
    pub(crate) fn do_create_community(&mut self) -> bool {
        let name = self.community_create_name.trim().to_owned();
        if name.is_empty() {
            self.community_create_error = Some(crate::i18n::t("community.name_required").clone());
            return false;
        }
        self.community_create_error = None;
        self.community_create_name.clear();
        if self.live.is_some() {
            self.send_backend(backend::BackendCommand::CreateCommunity { name: name.clone() });
            self.notice = Some(Notice::busy(crate::i18n::tf("community.creating", "name", &name)));
            return true;
        }
        // Demo: a local community at the end of the overview list.
        let announcement_chat_id = self
            .chats
            .iter()
            .map(|c| c.id)
            .chain(self.session_announcements.iter().map(|c| c.id))
            .max()
            .unwrap_or(0)
            + 1;
        self.add_session_community(name.clone(), announcement_chat_id);
        self.notice =
            Some(Notice::info(crate::i18n::tf("community.created_notice", "name", &name)));
        true
    }

    /// The "New community" dialog: a name field and Create/Cancel,
    /// mirroring the create-group dialog's shape and motion. Demo
    /// mode labels the local creation honestly, like the group
    /// form does.
    pub(crate) fn draw_create_community_dialog(&mut self, ctx: &egui::Context) {
        let Some(p) = crate::anim::popup_phase(
            ctx,
            "create_community",
            self.community_create_open,
            self.motion,
        ) else {
            return;
        };
        let exiting = !self.community_create_open;
        let t = self.tokens();
        let mut create = false;
        let mut close = false;
        egui::Area::new(egui::Id::unique("create_community"))
            .pivot(egui::Align2::CENTER_CENTER)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if p < 0.999 {
                    ui.set_opacity(p);
                }
                let frame = crate::theme::glass_panel_frame(t, crate::theme::RADIUS_PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 10));
                let _inner = frame.show(ui, |ui| {
                    ui.set_width(300.0);
                    ui.add_enabled_ui(!exiting, |ui| {
                        ui.label(
                            RichText::new(crate::i18n::t("community.new"))
                                .size(15.0)
                                .strong()
                                .color(rgb(t.text_primary)),
                        );
                        ui.add_space(SP_1_5);
                        ui.label(
                            RichText::new(crate::i18n::t("community.name_label"))
                                .size(12.0)
                                .color(rgb(t.text_secondary)),
                        );
                        crate::widgets::text_field(
                            ui,
                            t,
                            &mut self.community_create_name,
                            "",
                            280.0,
                        );
                        if !self.live_mode {
                            ui.label(
                                RichText::new(crate::i18n::t("community.demo_note"))
                                    .size(10.0)
                                    .color(rgb(t.text_faint)),
                            );
                        }
                        if let Some(error) = &self.community_create_error {
                            ui.add_space(SP_1);
                            ui.label(RichText::new(error.clone()).size(12.0).color(rgb(t.danger)));
                        }
                        ui.add_space(SP_2);
                        ui.horizontal(|ui| {
                            if crate::widgets::button(
                                ui,
                                t,
                                &crate::i18n::t("common.cancel"),
                                crate::widgets::Btn::Secondary,
                            )
                            .clicked()
                            {
                                close = true;
                            }
                            if crate::widgets::button(
                                ui,
                                t,
                                &crate::i18n::t("community.create"),
                                crate::widgets::Btn::Primary,
                            )
                            .clicked()
                            {
                                create = true;
                            }
                        });
                    });
                });
            });
        crate::anim::popup_transform(
            ctx,
            egui::Id::unique("create_community"),
            ctx.content_rect().center(),
            p,
        );
        if create && self.do_create_community() {
            close = true;
        }
        if close {
            self.community_create_open = false;
            self.community_create_error = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{collapsed_description, DESC_COLLAPSED_CHARS};

    #[test]
    fn short_description_is_unchanged() {
        assert_eq!(collapsed_description("A small community."), "A small community.");
        assert_eq!(collapsed_description(""), "");
    }

    #[test]
    fn exactly_at_limit_description_is_unchanged() {
        let desc = "a".repeat(DESC_COLLAPSED_CHARS);
        assert_eq!(collapsed_description(&desc), desc);
    }

    #[test]
    fn long_description_is_truncated_with_ellipsis() {
        let desc = "a".repeat(DESC_COLLAPSED_CHARS + 40);
        let out = collapsed_description(&desc);
        assert_eq!(out, format!("{}...", "a".repeat(DESC_COLLAPSED_CHARS)));
    }

    #[test]
    fn truncation_cuts_on_a_char_boundary() {
        // Multi-byte characters: the cut must land on a char
        // boundary, never inside a character.
        let desc = "é".repeat(DESC_COLLAPSED_CHARS + 10);
        let out = collapsed_description(&desc);
        assert_eq!(out, format!("{}...", "é".repeat(DESC_COLLAPSED_CHARS)));
    }

    #[test]
    fn truncation_lands_on_a_word_boundary() {
        // Ten-letter words separated by spaces: the raw char cut
        // would land inside the thirteenth word, so the collapsed
        // text backs up to the end of the twelfth word instead.
        let desc = ["abcdefghij"; 20].join(" ");
        let out = collapsed_description(&desc);
        let words = ["abcdefghij"; 12].join(" ");
        assert_eq!(out, format!("{words}..."));
    }

    #[test]
    fn truncation_trims_trailing_space_before_the_ellipsis() {
        let mut desc = "a".repeat(DESC_COLLAPSED_CHARS - 1);
        desc.push(' ');
        desc.push_str(&"b".repeat(20));
        let out = collapsed_description(&desc);
        assert_eq!(out, format!("{}...", "a".repeat(DESC_COLLAPSED_CHARS - 1)));
    }
}
