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

use crate::state::App;
use crate::theme::rgb;
use crate::widgets::{icon_button, paint_avatar};
use core::model::{
    demo_announcement_chat, demo_announcement_messages, demo_communities,
    demo_community_other_groups, Chat, ChatKind, Community,
};
use core::theme::Tokens;
use eframe::egui::{self, CornerRadius, FontId, Pos2, RichText, Sense, Vec2};

// Spacing scale: the same four steps the info dock uses
// (profile.rs is the scale's home; the values are mirrored here
// so this page snaps to the same rhythm).
const SP_1: f32 = 4.0;
const SP_2: f32 = 8.0;
const SP_3: f32 = 12.0;
const SP_SECTION: f32 = 20.0;

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
}

/// A section caption on the community home ("Announcements",
/// "Groups you're in").
fn section_label(ui: &mut egui::Ui, t: Tokens, text: &str) {
    ui.label(RichText::new(text).size(11.0).strong().color(rgb(t.text_faint)));
    ui.add_space(SP_1);
}

/// Characters of description shown while collapsed (about two
/// lines at the identity block's width). Longer descriptions are
/// cut on a char boundary with an ellipsis and a Read more toggle.
const DESC_COLLAPSED_CHARS: usize = 140;

fn collapsed_description(desc: &str) -> String {
    if desc.chars().count() <= DESC_COLLAPSED_CHARS {
        return desc.to_owned();
    }
    let cut: String = desc.chars().take(DESC_COLLAPSED_CHARS).collect();
    let cut = cut.trim_end();
    format!("{cut}...")
}

/// One not-yet-joined group row: the shared chat row's metrics
/// and avatar/title/snippet anatomy (chat_list.rs owns the
/// constants and the preview-line helper), plus a right-aligned
/// Join pill. The pill is its own interactive target and the
/// ONLY way to join: returns true when the pill was clicked;
/// clicking the row itself does nothing.
fn other_group_row(ui: &mut egui::Ui, chat: &Chat, t: Tokens) -> bool {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), crate::chat_list::ROW_H),
        Sense::hover(),
    );
    let hov = crate::anim::hover_factor(ui, ("other_group_row", chat.id), response.hovered());
    if hov > 0.001 {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(10),
            crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
        );
    }
    let painter = ui.painter();
    let center = Pos2::new(rect.left() + crate::chat_list::AVATAR_CX, rect.center().y);
    crate::widgets::paint_chat_avatar(painter, center, crate::chat_list::AVATAR_R, chat);
    // The Join pill occupies a right slot; the title and snippet
    // are ellipsized to the width left of it.
    let join_label = crate::i18n::t("community.join");
    let join_font = crate::theme::medium(12.5);
    let join_w = ui.ctx().fonts_mut(|f| {
        f.layout_no_wrap(join_label.clone(), join_font.clone(), rgb(t.badge_text)).size().x
    }) + 24.0;
    let pill = egui::Rect::from_center_size(
        Pos2::new(rect.right() - 8.0 - join_w / 2.0, rect.center().y),
        Vec2::new(join_w, 28.0),
    );
    let text_left = rect.left() + crate::chat_list::TEXT_X;
    let budget = (pill.left() - 10.0 - text_left).max(0.0);
    let title_font = crate::theme::medium(15.5);
    let title = crate::widgets::ellipsize(ui, &chat.title, title_font.clone(), budget);
    painter.text(
        Pos2::new(text_left, rect.top() + crate::chat_list::TITLE_DY),
        egui::Align2::LEFT_CENTER,
        title,
        title_font,
        rgb(t.text_primary),
    );
    let preview = crate::chat_list::chat_preview_line(chat);
    let snippet_font = FontId::proportional(13.0);
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
    // The pill as its own button: hover and pressed fills step
    // like the house buttons, with the pointing hand.
    let pill_resp = ui
        .interact(pill, egui::Id::unique(("join_pill", chat.id)), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let pill_fill = if pill_resp.is_pointer_button_down_on() {
        rgb(core::theme::pressed_step(t.accent))
    } else if pill_resp.hovered() {
        rgb(core::theme::hover_step(t.accent))
    } else {
        rgb(t.accent)
    };
    painter.rect_filled(pill, CornerRadius::same(14), pill_fill);
    painter.text(
        pill.center(),
        egui::Align2::CENTER_CENTER,
        join_label,
        join_font,
        rgb(t.on_accent()),
    );
    pill_resp.clicked()
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
        ui.add_space(SP_3);
        // Centered identity block: avatar, name, the community
        // subtitle line and the description with its toggle.
        ui.vertical_centered(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(80.0), egui::Sense::hover());
            paint_avatar(ui.painter(), rect.center(), 40.0, true, community.avatar_seed);
            ui.add_space(SP_2);
            ui.label(
                RichText::new(&community.name)
                    .font(crate::theme::semibold(20.0))
                    .color(rgb(t.text_primary)),
            );
            ui.label(
                RichText::new(crate::i18n::tf(
                    "community.subtitle",
                    "count",
                    &total_groups.to_string(),
                ))
                .size(12.5)
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
                        ui.label(RichText::new(shown).size(13.0).color(rgb(t.text_secondary)));
                    },
                );
                if needs_toggle {
                    let label = if desc_expanded {
                        crate::i18n::t("community.show_less")
                    } else {
                        crate::i18n::t("community.read_more")
                    };
                    if ui
                        .add(
                            egui::Button::new(RichText::new(label).size(13.0).color(rgb(t.accent)))
                                .frame(false),
                        )
                        .clicked()
                    {
                        intents.toggle_desc = true;
                    }
                }
            }
        });
        ui.add_space(SP_SECTION);
        // Announcement chat: the shared chat row with a megaphone
        // avatar override, keeping its own unread badge.
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
        )
        .clicked()
        {
            intents.open_chat = Some(announcement.id);
        }
        ui.add_space(SP_SECTION);
        // The groups the user is in.
        section_label(ui, t, &crate::i18n::t("community.your_groups"));
        for group in groups {
            if crate::chat_list::chat_row(ui, group, false, false, t, None, None, None).clicked() {
                intents.open_chat = Some(group.id);
            }
        }
        // Groups of this community the user has not joined yet.
        if !other_groups.is_empty() {
            ui.add_space(SP_SECTION);
            section_label(ui, t, &crate::i18n::t("community.other_groups"));
            for group in other_groups {
                if other_group_row(ui, group, t) {
                    intents.join_group = Some(group.id);
                }
            }
        }
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
            return demo_communities();
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
        names
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                let mut tagged: Vec<&Chat> = self
                    .chats
                    .iter()
                    .filter(|c| c.kind == ChatKind::Group && c.community.as_deref() == Some(name))
                    .collect();
                tagged.sort_by_key(|c| c.id);
                let announcement_chat_id = tagged.first().map_or(0, |c| c.id);
                let avatar_seed = name.bytes().fold(0u8, u8::wrapping_add);
                Community {
                    id: i as u64 + 1,
                    name: name.to_owned(),
                    description: None,
                    avatar_seed,
                    announcement_chat_id,
                    group_chat_ids: tagged.iter().skip(1).map(|c| c.id).collect(),
                    other_group_ids: Vec::new(),
                }
            })
            .collect()
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
        // opened), then the demo data itself. It never resolves
        // by joining the chat list.
        let announcement: Chat =
            match self.chats.iter().find(|c| c.id == community.announcement_chat_id) {
                Some(c) => c.clone(),
                None => match self
                    .detached_chat
                    .as_ref()
                    .filter(|c| c.id == community.announcement_chat_id)
                {
                    Some(c) => c.clone(),
                    None if !self.live_mode
                        && community.announcement_chat_id == demo_announcement_chat().id =>
                    {
                        demo_announcement_chat()
                    }
                    None => {
                        self.open_community = None;
                        self.community_home_from_overview = false;
                        self.draw_conversation(root);
                        return;
                    }
                },
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
                self.notice = Some(crate::i18n::tf("community.joined", "name", &title));
            }
        }
        if let Some(id) = intents.open_chat {
            self.open_community_chat(id);
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
        let panel = self
            .list_panel(root)
            .frame(
                egui::Frame::new()
                    .fill(rgb(t.bg_panel))
                    .inner_margin(egui::Margin::symmetric(10, 8)),
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
                            .font(crate::theme::semibold(17.0))
                            .color(rgb(t.text_primary)),
                    );
                });
                ui.add_space(SP_3);
                if communities.is_empty() {
                    ui.add_space(24.0);
                    ui.label(
                        RichText::new(crate::i18n::t("communities.empty"))
                            .size(13.5)
                            .color(rgb(t.text_secondary)),
                    );
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
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
                                CornerRadius::same(10),
                                crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
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
                            crate::i18n::tf("community.groups_count", "count", &total.to_string()),
                            FontId::proportional(12.5),
                            rgb(t.text_faint),
                        );
                        crate::icons::draw(
                            ui.painter(),
                            crate::icons::Icon::ChevronRight,
                            Pos2::new(rect.right() - 18.0, rect.center().y),
                            20.0,
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
        if !self.live_mode
            && id == demo_announcement_chat().id
            && !self.chats.iter().any(|c| c.id == id)
            && self.detached_chat.as_ref().is_none_or(|c| c.id != id)
        {
            let mut announcement = demo_announcement_chat();
            announcement.messages = demo_announcement_messages();
            self.detached_chat = Some(announcement);
        }
        self.select_chat_now(id);
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
    fn truncation_trims_trailing_space_before_the_ellipsis() {
        let mut desc = "a".repeat(DESC_COLLAPSED_CHARS - 1);
        desc.push(' ');
        desc.push_str(&"b".repeat(20));
        let out = collapsed_description(&desc);
        assert_eq!(out, format!("{}...", "a".repeat(DESC_COLLAPSED_CHARS - 1)));
    }
}
