//! Chat list panel: search, filter chips, rows, and the row
//! context menu (archiving lives here, on the left).

use crate::state::App;
use crate::theme::rgb;
use crate::widgets::chip;
use core::filter::{filter_chats, ChatFilter};
use core::model::{Chat, ChatKind};
use core::theme::Tokens;
use eframe::egui::{self, CornerRadius, FontId, Margin, Pos2, RichText, Sense, Vec2};

/// Shared chat-row metrics: the one geometry every row built on
/// the chat row's anatomy uses (the chat list itself and the
/// community home's group rows), so the rows can never drift
/// apart. All values are in points, relative to the row rect.
pub(crate) const ROW_H: f32 = 62.0;
/// Avatar centre x offset from the row's left edge.
pub(crate) const AVATAR_CX: f32 = 33.0;
/// Avatar radius.
pub(crate) const AVATAR_R: f32 = 23.0;
/// Text column x offset from the row's left edge.
pub(crate) const TEXT_X: f32 = 66.0;
/// Title baseline y offset from the row's top edge.
pub(crate) const TITLE_DY: f32 = 21.0;
/// Snippet baseline y offset from the row's top edge.
pub(crate) const SNIPPET_DY: f32 = 43.0;

/// The chat row's preview line: in a group the sender's name
/// prefixes the preview ("Sender: text"); everywhere else the
/// preview stands alone. Shared by the chat row and the
/// community home's other-group rows.
pub(crate) fn chat_preview_line(chat: &Chat) -> String {
    match (&chat.preview_sender, chat.kind) {
        (Some(s), ChatKind::Group) => format!("{s}: {}", chat.preview),
        _ => chat.preview.clone(),
    }
}

impl App {
    pub(crate) fn draw_chat_list(&mut self, root: &mut egui::Ui) {
        // The Archived filter changes only which rows this list
        // shows. Entering the view (rail button, filter chip, a
        // startup screen) never touches the selection: the open
        // conversation on the right stays exactly as it was, even
        // when it is not an archived chat (owner's ruling; an
        // earlier version force-opened the first archived chat
        // here and that sync was removed on purpose, do not
        // re-add it).
        // Clicking an archived row below opens that chat
        // through the normal row-click path.
        let t = self.tokens();
        let panel = egui::Panel::left("chat_list")
            .resizable(true)
            .default_size(350.0)
            .size_range(280.0..=460.0)
            .frame(egui::Frame::new().fill(rgb(t.bg_panel)).inner_margin(Margin::symmetric(10, 8)))
            .show(root, |ui| {
                // Header row: the "Chats" title, then the panel
                // controls on the right (collapse, compose,
                // settings). No profile avatar here: the rail's
                // bottom avatar is the single profile entry and
                // the account menu's only anchor (owner's ruling:
                // a second one above the search was redundant).
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(crate::i18n::t("nav.chats"))
                            .font(crate::theme::semibold(17.0))
                            .color(rgb(t.text_primary)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if crate::widgets::icon_button(
                            ui,
                            t,
                            crate::icons::Icon::Sliders,
                            28.0,
                            &crate::i18n::t("nav.settings"),
                            None,
                        )
                        .clicked()
                        {
                            self.screen = crate::state::Screen::Settings;
                        }
                        if crate::widgets::icon_button(
                            ui,
                            t,
                            crate::icons::Icon::Edit,
                            28.0,
                            &crate::i18n::t("chats.new_group"),
                            None,
                        )
                        .clicked()
                        {
                            self.groups.create_open = true;
                        }
                        if crate::widgets::icon_button(
                            ui,
                            t,
                            crate::icons::Icon::ChevronLeft,
                            28.0,
                            &crate::i18n::t("chats.collapse"),
                            None,
                        )
                        .clicked()
                        {
                            self.sidebar_collapsed = true;
                        }
                    });
                });
                ui.add_space(10.0);
                // Search owns its own row below the title: the
                // field sizes from the full row width now that the
                // title row no longer shares it. The pill itself
                // is the shared list-panel search component.
                crate::widgets::search_pill(
                    ui,
                    t,
                    egui::Id::unique("chat_search"),
                    &crate::i18n::t("chats.search_hint"),
                    &mut self.search,
                );
                ui.add_space(8.0);
                // The filter chips live on a horizontally scrolling
                // strip (the Telegram folder pattern): a wrapped row
                // squeezed the last chip until its label broke
                // mid-word once the counts made the row too wide.
                egui::ScrollArea::horizontal()
                    .id_salt("chat_filter_chips")
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .scroll_source(egui::scroll_area::ScrollSource::ALL)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for f in [
                                ChatFilter::All,
                                ChatFilter::Unread,
                                ChatFilter::Private,
                                ChatFilter::Favorites,
                                ChatFilter::Groups,
                            ]
                            .into_iter()
                            .chain(
                                self.chats
                                    .iter()
                                    .any(|c| c.archived)
                                    .then_some(ChatFilter::Archived),
                            ) {
                                let selected = self.filter == f;
                                // Filter chips carry their counts, the
                                // way mainstream chat clients show them
                                // (Unread 6, Groups 3, and so on).
                                let count = match f {
                                    ChatFilter::All | ChatFilter::Archived => 0,
                                    ChatFilter::Unread => self
                                        .chats
                                        .iter()
                                        .filter(|c| !c.archived && c.unread > 0)
                                        .count(),
                                    ChatFilter::Private => self
                                        .chats
                                        .iter()
                                        .filter(|c| !c.archived && c.kind == ChatKind::Dm)
                                        .count(),
                                    ChatFilter::Favorites => self
                                        .chats
                                        .iter()
                                        .filter(|c| !c.archived && c.favorite)
                                        .count(),
                                    ChatFilter::Groups => self
                                        .chats
                                        .iter()
                                        .filter(|c| !c.archived && c.kind == ChatKind::Group)
                                        .count(),
                                    ChatFilter::Channels => self
                                        .chats
                                        .iter()
                                        .filter(|c| !c.archived && c.kind == ChatKind::Channel)
                                        .count(),
                                };
                                let label = if count > 0 {
                                    format!("{} {count}", f.label())
                                } else {
                                    f.label().to_owned()
                                };
                                if chip(ui, &label, selected, t).clicked() {
                                    self.filter = f;
                                }
                            }
                        });
                    });
                let photos = self.demo_photos(ui.ctx());
                let visible = filter_chats(&self.chats, self.filter, &self.search);
                let mut clicked: Option<u64> = None;
                // A row context-menu pick (chat id, target archived
                // state), applied after the list renders. Archiving
                // lives on the left, on the rows themselves (the
                // WhatsApp Web pattern): the conversation header
                // menu carries no archive item, and there is no
                // separate Archived entry row in this panel (the
                // rail and the filter chip are the entries).
                let row_action = std::cell::Cell::new(None::<(u64, bool)>);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(
                    ui,
                    64.0,
                    visible.len(),
                    |ui, range| {
                        for idx in range {
                            let chat = visible[idx];
                            let state = self.row_state_for(chat);
                            let resp = chat_row(
                                ui,
                                chat,
                                chat.id == self.selected_chat,
                                chat.kind == ChatKind::Dm && self.blocked_chats.contains(&chat.id),
                                t,
                                state,
                                None,
                                photos.get(&chat.id).copied(),
                            );
                            if resp.clicked() {
                                clicked = Some(chat.id);
                            }
                            resp.context_menu(|ui| {
                                ui.set_min_width(170.0);
                                let label = if chat.archived {
                                    crate::i18n::t("conversation.unarchive")
                                } else {
                                    crate::i18n::t("conversation.archive")
                                };
                                if crate::widgets::menu_item_icon(
                                    ui,
                                    t,
                                    Some(crate::icons::Icon::Archive),
                                    &label,
                                    None,
                                )
                                .clicked()
                                {
                                    row_action.set(Some((chat.id, !chat.archived)));
                                    ui.close();
                                }
                            });
                        }
                    },
                );
                let no_visible = visible.is_empty();
                // End the immutable borrow of `self.chats` held by
                // `visible` before mutating selection state below.
                drop(visible);
                if let Some(id) = clicked {
                    // Picking a chat leaves the community home: the
                    // conversation takes the content area back, and
                    // the chat was not entered through a community,
                    // so the header's way back to one goes away.
                    self.open_community = None;
                    self.community_home_from_overview = false;
                    self.community_origin = None;
                    let previous = self.selected_chat;
                    if id != previous {
                        // Per-chat composer state: park
                        // everything the composer holds for the
                        // chat being left (draft, staged files,
                        // quote and edit targets), restore the
                        // target chat's. The same helper runs on
                        // every switch path (see state.rs).
                        self.switch_composer_chat(previous, id);
                    }
                    // The unread count at open time, captured
                    // before the demo branch below clears the
                    // badge: the divider snapshot is taken from
                    // it once the conversation is loaded.
                    let unread_at_open =
                        self.chats.iter().find(|c| c.id == id).map_or(0, |c| c.unread);
                    self.selected_chat = id;
                    if self.live_mode {
                        self.sync_conversation();
                        // Read-by-action: merely opening a chat no
                        // longer marks anything read. Receipts and
                        // the badge wait for a real action inside
                        // the conversation (scroll to the newest
                        // messages, use the composer, send a reply);
                        // see live.rs `note_read_engagement`.
                    } else {
                        // Demo mode: ONE state machine, the
                        // rendered conversation is a working copy.
                        // Write it back into the chat being left,
                        // then load the newly selected chat.
                        if previous != id {
                            // The detached case is the announcement
                            // view: its working copy goes back to
                            // the detached slot, never to the list.
                            self.write_back_conversation(previous);
                        }
                        // Opening a chat clears its unread badge;
                        // receipt semantics live in the demo
                        // data itself (see the demo audit).
                        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == id) {
                            chat.unread = 0;
                        }
                        self.conversation = self
                            .chats
                            .iter()
                            .find(|c| c.id == id)
                            .map(|c| c.messages.clone())
                            .unwrap_or_default();
                    }
                    self.unread_divider =
                        crate::state::unread_divider_for(unread_at_open, &self.conversation);
                }
                if let Some((id, target_archived)) = row_action.get() {
                    if let Some(c) = self.chats.iter().find(|c| c.id == id) {
                        let (pinned, muted) = (c.pinned, c.muted);
                        self.apply_chat_action(id, target_archived, pinned, muted);
                    }
                }
                if no_visible {
                    ui.label(
                        RichText::new(crate::i18n::t("chats.no_match"))
                            .color(rgb(t.text_secondary)),
                    );
                }
            });
        crate::widgets::soft_right_edge(root, panel.response.rect, t);
    }
}

/// A transient state that takes over a chat row's snippet
/// slot: someone typing in the chat, or a saved composer
/// draft. The state REPLACES the snippet in the same slot (the
/// WhatsApp convention); the row's geometry never changes and
/// there is never a third line.
#[derive(Clone)]
pub(crate) enum RowState {
    /// Green typing line; groups name the typist when known.
    Typing(Option<String>),
    /// Accent "Draft: " prefix plus the draft text.
    Draft(String),
}

impl App {
    /// The row state for one chat, from real app state: the
    /// live layer's typing instants (fresh for a few seconds),
    /// the demo layer's named typing, and the per-chat parked
    /// composer drafts (the open chat's draft is the live one).
    fn row_state_for(&self, chat: &Chat) -> Option<RowState> {
        if self.live_mode {
            if self.live_typing.get(&chat.id).is_some_and(|at| at.elapsed().as_secs() < 6) {
                return Some(RowState::Typing(None));
            }
        } else if let Some(name) = self.typing_names.get(&chat.id) {
            return Some(RowState::Typing(Some(name.clone())));
        }
        if chat.id == self.selected_chat {
            if !self.composer.draft.trim().is_empty() {
                return Some(RowState::Draft(self.composer.draft.clone()));
            }
        } else if let Some(d) = self.composer.chat_drafts.get(&chat.id) {
            if !d.trim().is_empty() {
                return Some(RowState::Draft(d.clone()));
            }
        }
        None
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn chat_row(
    ui: &mut egui::Ui,
    chat: &Chat,
    selected: bool,
    blocked: bool,
    t: Tokens,
    row_state: Option<RowState>,
    avatar_icon: Option<crate::icons::Icon>,
    photo: Option<egui::TextureId>,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
    // Hover and press washes crossfade through the animation
    // helpers (about 130 ms in, 90 ms press) instead of snapping
    // between fills. The washes are glass: a translucent tint
    // over the panel instead of a flat fill swap, deepening from
    // hover to the pressed step while the pointer is down. The
    // selected row keeps a steady, stronger wash.
    if selected {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(10),
            crate::theme::glass_wash(t, t.bg_panel_alt, 175),
        );
    } else {
        let hov = crate::anim::hover_factor(ui, ("chat_row", chat.id), response.hovered());
        let press = crate::anim::press_factor(
            ui,
            ("chat_row", chat.id),
            response.is_pointer_button_down_on(),
        );
        if hov > 0.001 {
            ui.painter().rect_filled(
                rect,
                CornerRadius::same(10),
                crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
            );
        }
        if press > 0.001 {
            ui.painter().rect_filled(
                rect,
                CornerRadius::same(10),
                crate::theme::glass_wash(
                    t,
                    core::theme::pressed_step(t.bg_panel_alt),
                    (press * 80.0) as u8,
                ),
            );
        }
    }
    let painter = ui.painter();
    let center = Pos2::new(rect.left() + AVATAR_CX, rect.center().y);
    // A whisper of avatar growth while the row is hovered (about
    // 1 px of radius, eased as a scalar), so rows feel tactile
    // without anything else in the row moving.
    let lift = crate::anim::scalar(
        ui,
        ("chat_row_lift", chat.id),
        if response.hovered() { 1.0 } else { 0.0 },
        0.15,
    );
    if let Some(icon) = avatar_icon {
        // Avatar override (the community announcement row): a
        // seed-colored disc with the given icon instead of the
        // chat's own avatar.
        let disc = crate::widgets::avatar_color(chat.avatar_seed);
        painter.circle_filled(center, AVATAR_R + 1.0 * lift, disc);
        crate::icons::draw(
            painter,
            icon,
            center,
            (AVATAR_R + 1.0 * lift) * 1.15,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 235),
            disc,
        );
    } else {
        crate::widgets::paint_chat_avatar_photo(
            painter,
            center,
            AVATAR_R + 1.0 * lift,
            chat,
            photo,
        );
    }
    // Presence slot: a small dot at the avatar's lower right for
    // DM contacts whose subtitle says they are online right now.
    // It reads the existing subtitle data only; nothing here
    // invents a live presence feed. The ring is the panel colour,
    // so the dot reads as punched out of the row.
    if chat.kind == ChatKind::Dm && chat.subtitle == "online" {
        let r = AVATAR_R + 1.0 * lift;
        let dot = center + Vec2::new(r * 0.72, r * 0.72);
        painter.circle_filled(dot, 5.5, rgb(t.bg_panel));
        painter.circle_filled(dot, 4.0, rgb(t.badge_bg));
    }
    // Title line: the timestamp is measured first and the title
    // is handed exactly the width that remains (minus the badges
    // that follow it), so the two can never collide however
    // narrow the panel gets. The name steps up a weight while
    // the chat has unread messages.
    let text_left = rect.left() + TEXT_X;
    let time_font = FontId::proportional(11.5);
    let time_w = ui.ctx().fonts_mut(|f| {
        f.layout_no_wrap(chat.time.clone(), time_font.clone(), rgb(t.text_faint)).size().x
    });
    // A blocked direct chat says so beside the time, in the
    // same quiet register as the muted bell below. The row
    // still renders and opens like any other (WhatsApp keeps
    // blocked chats in the list); the label just stops it
    // pretending to be a normal conversation. Its width is
    // reserved up front so the title budget accounts for it.
    let blocked_label = if blocked { crate::i18n::t("chats.blocked") } else { String::new() };
    let blocked_w = if blocked {
        ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap(blocked_label.clone(), time_font.clone(), rgb(t.text_faint)).size().x
        }) + 8.0
    } else {
        0.0
    };
    let badges_w =
        (if chat.verified { 20.0 } else { 0.0 }) + (if chat.meta_ai { 28.0 } else { 0.0 });
    let title_font =
        if chat.unread > 0 { crate::theme::semibold(15.5) } else { crate::theme::medium(15.5) };
    let title_budget =
        (rect.right() - 6.0 - time_w - blocked_w - 8.0 - badges_w - text_left).max(0.0);
    let title = crate::widgets::ellipsize(ui, &chat.title, title_font.clone(), title_budget);
    let title_rect = painter.text(
        Pos2::new(text_left, rect.top() + TITLE_DY),
        egui::Align2::LEFT_CENTER,
        title,
        title_font,
        rgb(t.text_primary),
    );
    let mut badge_x = title_rect.right() + 6.0;
    if chat.verified {
        crate::widgets::verified_badge(
            painter,
            Pos2::new(badge_x + 7.0, title_rect.center().y),
            7.0,
            t,
        );
        badge_x += 20.0;
    }
    if chat.meta_ai {
        crate::widgets::ai_chip(painter, Pos2::new(badge_x, title_rect.center().y), t);
    }
    // Muted chats carry a small bell-off beside the time, the
    // reference wireframe's way of saying "notifications silenced"
    // without relying on the badge's grey variant alone. The
    // blocked label takes the far edge when present, and the
    // bell and time shift left past it.
    if blocked {
        painter.text(
            Pos2::new(rect.right() - 6.0, rect.top() + TITLE_DY),
            egui::Align2::RIGHT_CENTER,
            &blocked_label,
            time_font.clone(),
            rgb(t.text_faint),
        );
    }
    let time_right = if chat.muted {
        crate::icons::draw(
            painter,
            crate::icons::Icon::BellOff,
            Pos2::new(rect.right() - 11.0 - blocked_w, rect.top() + TITLE_DY),
            14.0,
            rgb(t.text_faint),
            rgb(t.bg_panel),
        );
        rect.right() - 22.0 - blocked_w
    } else {
        rect.right() - 6.0 - blocked_w
    };
    painter.text(
        Pos2::new(time_right, rect.top() + TITLE_DY),
        egui::Align2::RIGHT_CENTER,
        &chat.time,
        time_font,
        if chat.unread > 0 && !chat.muted { rgb(t.accent) } else { rgb(t.text_faint) },
    );
    // Snippet line, with the anatomy real chat lists carry:
    // delivery ticks (or the sending clock) when the last
    // message is ours, then a media label for a non-text last
    // message, then the text itself. The line's right edge is
    // set by whichever meta sits there (mention badge, unread
    // badge), and the text is ellipsized to whatever width is
    // left after the prefix, measured, never counted.
    let snippet_font = FontId::proportional(13.0);
    let line_y = rect.top() + SNIPPET_DY;
    let mut right_limit = rect.right() - 6.0;
    if chat.mentions > 0 {
        right_limit = right_limit.min(rect.right() - 56.0);
    }
    if chat.unread > 0 {
        let count_label = chat.unread.to_string();
        let count_w = ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap(count_label, FontId::proportional(11.0), rgb(t.badge_text)).size().x
        });
        right_limit = right_limit.min(rect.right() - 6.0 - (count_w + 10.0).max(19.0) - 6.0);
    }
    // A transient row state replaces the snippet outright:
    // typing and draft lines carry no ticks or media labels.
    match &row_state {
        Some(RowState::Typing(name)) => {
            let label = match (name, chat.kind) {
                (Some(n), ChatKind::Group) => crate::i18n::tf("chats.typing_named", "name", n),
                _ => crate::i18n::t("chats.typing"),
            };
            let label = crate::widgets::ellipsize(
                ui,
                &label,
                snippet_font.clone(),
                right_limit - text_left,
            );
            painter.text(
                Pos2::new(text_left, line_y),
                egui::Align2::LEFT_CENTER,
                label,
                snippet_font.clone(),
                rgb(t.accent),
            );
        }
        Some(RowState::Draft(draft)) => {
            let prefix = crate::i18n::t("chats.draft");
            let prefix_w = ui.ctx().fonts_mut(|f| {
                f.layout_no_wrap(prefix.clone(), crate::theme::semibold(13.0), rgb(t.accent))
                    .size()
                    .x
            });
            painter.text(
                Pos2::new(text_left, line_y),
                egui::Align2::LEFT_CENTER,
                prefix,
                crate::theme::semibold(13.0),
                rgb(t.accent),
            );
            let body = crate::widgets::ellipsize(
                ui,
                draft,
                snippet_font.clone(),
                right_limit - text_left - prefix_w - 6.0,
            );
            if !body.is_empty() {
                painter.text(
                    Pos2::new(text_left + prefix_w + 6.0, line_y),
                    egui::Align2::LEFT_CENTER,
                    body,
                    snippet_font.clone(),
                    rgb(t.text_secondary),
                );
            }
        }
        None => {}
    }
    let mut x = text_left;
    if row_state.is_none() && chat.preview_mine {
        match chat.preview_delivery {
            Some(core::model::Delivery::Sending) => {
                crate::bubble::paint_clock(painter, Pos2::new(x + 5.0, line_y), rgb(t.text_faint));
                x += 15.0;
            }
            Some(delivery) => {
                let ticks = crate::bubble::tick_count(Some(delivery));
                if ticks > 0 {
                    let color = if delivery == core::model::Delivery::Read {
                        rgb(t.tick_read)
                    } else {
                        rgb(t.text_faint)
                    };
                    crate::bubble::paint_ticks(painter, x, line_y - 3.0, ticks, color);
                    x += ticks as f32 * 7.0 + 5.0;
                }
            }
            None => {}
        }
    }
    if row_state.is_none() {
        if let Some(media) = chat.preview_media {
            let label = media.label();
            let label_rect = painter.text(
                Pos2::new(x, line_y),
                egui::Align2::LEFT_CENTER,
                label,
                snippet_font.clone(),
                rgb(t.text_secondary),
            );
            x = label_rect.right() + 6.0;
        }
    }
    let preview = chat_preview_line(chat);
    let preview = crate::widgets::ellipsize(ui, &preview, snippet_font.clone(), right_limit - x);
    if row_state.is_none() && !preview.is_empty() {
        painter.text(
            Pos2::new(x, line_y),
            egui::Align2::LEFT_CENTER,
            preview,
            snippet_font,
            rgb(t.text_secondary),
        );
    }
    if chat.unread > 0 {
        // The unread badge: a pill at least 19 px tall (wider when
        // the count needs it), 11 px text, and a short pop
        // (scale to about 1.3 and back over 300 ms) on the frame
        // the count changes, so a new message arriving in a closed
        // chat visibly lands.
        let scale = crate::anim::pop_scale(ui, ("unread_badge", chat.id), chat.unread);
        let label = chat.unread.to_string();
        let font = FontId::proportional(11.0 * scale);
        let text_w = ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap(label.clone(), font.clone(), rgb(t.badge_text)).size().x
        });
        let w = ((text_w + 10.0).max(19.0)) * scale;
        let h = 19.0 * scale;
        let brect = egui::Rect::from_min_size(
            Pos2::new(rect.right() - 6.0 - w, line_y - h / 2.0),
            Vec2::new(w, h),
        );
        // Muted chats wear the grey badge variant: the count is
        // still there, it just does not shout in the accent
        // green.
        let (badge_fill, badge_text_col) = if chat.muted {
            crate::theme::muted_badge(t)
        } else {
            (rgb(t.badge_bg), rgb(t.badge_text))
        };
        painter.rect_filled(brect, CornerRadius::same((h / 2.0).round() as u8), badge_fill);
        painter.text(brect.center(), egui::Align2::CENTER_CENTER, label, font, badge_text_col);
    }
    if chat.mentions > 0 {
        // Same 19 px diameter as the unread badge beside it, so
        // the two badges in a row read as one family.
        let mcenter = Pos2::new(rect.right() - 40.0, line_y);
        painter.circle_filled(mcenter, 9.5, rgb(t.badge_bg));
        painter.text(
            mcenter,
            egui::Align2::CENTER_CENTER,
            "@",
            FontId::proportional(11.0),
            rgb(t.badge_text),
        );
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}
