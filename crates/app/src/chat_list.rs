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

/// What a chat-row context-menu pick asks for. The menu
/// closure only records the pick; the list applies it after
/// rendering, once the borrow of the chat list the rows were
/// drawn from has been released.
#[derive(Clone, Copy)]
enum RowPick {
    /// Set the chat's archived state.
    SetArchived(bool),
    /// Set the chat's favorite state.
    SetFavorite(bool),
}

/// The chat row's preview line: in a group the sender's name
/// prefixes the preview ("Sender: text"); everywhere else the
/// preview stands alone. Shared by the chat row and the
/// community home's other-group rows.
/// The filter chip's label key in the locale system. Chip
/// labels are interface vocabulary, so they live with the other
/// interface strings rather than in the data model: the one-to-one
/// filter reads "Personal", not the model's "Private".
fn filter_label_key(f: ChatFilter) -> &'static str {
    match f {
        ChatFilter::All => "chats.filter_all",
        ChatFilter::Unread => "chats.filter_unread",
        ChatFilter::Private => "chats.filter_personal",
        ChatFilter::Favorites => "chats.filter_favorites",
        ChatFilter::Groups => "chats.filter_groups",
        ChatFilter::Channels => "chats.filter_channels",
        ChatFilter::Archived => "chats.filter_archived",
    }
}

pub(crate) fn chat_preview_line(chat: &Chat) -> String {
    match (&chat.preview_sender, chat.kind) {
        (Some(s), ChatKind::Group) => format!("{s}: {}", chat.preview),
        _ => chat.preview.clone(),
    }
}

impl App {
    pub(crate) fn draw_chat_list(&mut self, root: &mut egui::Ui) {
        // Channels, Groups and Archived are modes of this list,
        // not separate screens: the header swaps to the filtered
        // construction (back chevron plus the destination's own
        // title, no chip row, no list actions) while the rows
        // below are the same list narrowed by the destination's
        // filter. Entering a filtered view never touches the
        // selection (an earlier version force-opened the first
        // matching chat and that sync was removed on purpose, do
        // not re-add it); when the selected chat is not a member
        // of the filter, the shell shows the empty conversation
        // placeholder beside this list instead of the foreign
        // chat (see shell.rs draw_content). Clicking a row below
        // opens that chat through the normal row-click path.
        let filtered = self.screen.is_filtered_list();
        let t = self.tokens();
        let panel = self
            .list_panel(root)
            .frame(
                egui::Frame::new()
                    .fill(rgb(t.bg_panel))
                    .inner_margin(Margin::symmetric(crate::theme::LIST_PANEL_INSET_X, 8)),
            )
            .show(root, |ui| {
                // Header row: the "Chats" title, then the list
                // actions on the right (collapse, new group).
                // Settings is not here: the rail's Settings entry
                // is the single entry point, and a second sliders
                // button in this header duplicated it. No profile
                // avatar here either: the rail's bottom avatar is
                // the single profile entry and the account menu's
                // only anchor (owner's ruling: a second one above
                // the search was redundant).
                // Portrait mode swaps the whole row for the phone
                // header (app name + kebab, or the archived
                // page's back header); see the helper below.
                if self.portrait {
                    self.draw_chat_list_header_portrait(ui, t);
                } else if filtered {
                    // The filtered construction: a back chevron
                    // returning to the plain Chats list and the
                    // destination's own title, in the same header
                    // frame the Chats title occupies. No actions
                    // ride along: the list actions (new group,
                    // collapse) belong to the Chats list itself,
                    // not to a narrowed view of it.
                    let mut back = false;
                    ui.horizontal(|ui| {
                        if crate::widgets::icon_button(
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
                            RichText::new(crate::i18n::t(self.screen.filtered_title_key()))
                                .font(crate::theme::medium(16.0))
                                .color(rgb(t.text_primary)),
                        );
                    });
                    if back {
                        self.screen = crate::state::Screen::Chats;
                        self.filter = ChatFilter::All;
                    }
                } else {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(crate::i18n::t("nav.chats"))
                                .font(crate::theme::medium(16.0))
                                .color(rgb(t.text_primary)),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
                }
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
                // Session state row (design standard section 14;
                // audit S11/S12): while linked, the list says what
                // the session is doing, so an empty or stale list
                // never reads as silently broken. Sync progress
                // rides the shared loading row; a lost connection
                // is a plain label in the same slot.
                if let Some(session) = &self.live {
                    if matches!(session.status, crate::live::LinkStatus::Lost(_)) {
                        ui.label(
                            RichText::new(crate::i18n::t("chats.connection_lost"))
                                .size(12.5)
                                .color(rgb(t.text_secondary)),
                        );
                        ui.add_space(8.0);
                    } else if session.syncing {
                        // Indeterminate on purpose: the library
                        // offers no honest percentage (its only
                        // number is a per-chunk stamp from the
                        // phone that can sit unchanged for minutes
                        // on a populated roster), so the row never
                        // shows one. The repaint tick keeps the
                        // quiet-window fallback in poll_backend
                        // timely while the row is up.
                        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
                        crate::widgets::loading_row(
                            ui,
                            &crate::i18n::t("chats.syncing"),
                            12.5,
                            t.text_secondary,
                        );
                        ui.add_space(8.0);
                    }
                }
                // The filter chips live on the shared scrolling
                // chip strip: a wrapped row squeezed the last chip
                // until its label broke mid-word once the counts
                // made the row too wide, and a plain clipped row
                // could leave the selected chip itself cut off at
                // the panel edge. The strip hides its scrollbar,
                // fades the clipped edge, and scrolls the selected
                // chip fully into view when the filter changes.
                // Filtered destinations show no chip row at all:
                // the destination itself is the filter, and its
                // header above already says which one.
                if !filtered {
                    crate::widgets::chip_scroll_row(
                        ui,
                        "chat_filter_chips",
                        t.bg_panel,
                        self.filter,
                        |ui| {
                            let mut selected_resp = None;
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
                                let name = crate::i18n::t(filter_label_key(f));
                                let label = if count > 0 {
                                    format!("{name} {}", core::filter::format_count(count as u64))
                                } else {
                                    name
                                };
                                let resp = chip(ui, &label, selected, t);
                                let clicked = resp.clicked();
                                if selected {
                                    selected_resp = Some(resp);
                                }
                                if clicked {
                                    self.filter = f;
                                }
                            }
                            selected_resp
                        },
                    );
                }
                // Portrait: the archived entry row under the
                // chips (the phone pattern), shown on the
                // unfiltered, unsearched list while archived
                // chats exist. The rail and the filter chip
                // carry the entry in the other modes; here the
                // row is the way into the full-screen archived
                // page, and it carries the archived count.
                if self.portrait
                    && self.screen != crate::state::Screen::Archived
                    && self.filter == ChatFilter::All
                    && self.search.trim().is_empty()
                {
                    let archived_count = self.chats.iter().filter(|c| c.archived).count();
                    if archived_count > 0 {
                        ui.add_space(4.0);
                        let (rect, resp) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 48.0),
                            Sense::click(),
                        );
                        let hov =
                            crate::anim::hover_factor(ui, "portrait_archived_row", resp.hovered());
                        if hov > 0.001 {
                            ui.painter().rect_filled(
                                rect,
                                CornerRadius::same(10),
                                crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
                            );
                        }
                        crate::icons::draw(
                            ui.painter(),
                            crate::icons::Icon::Archive,
                            Pos2::new(rect.left() + AVATAR_CX, rect.center().y),
                            crate::icons::SIZE_CONTROL,
                            rgb(t.text_secondary),
                            rgb(t.bg_panel),
                        );
                        ui.painter().text(
                            Pos2::new(rect.left() + TEXT_X, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            crate::i18n::t("nav.archived"),
                            crate::theme::medium(15.0),
                            rgb(t.text_primary),
                        );
                        ui.painter().text(
                            Pos2::new(rect.right() - 6.0, rect.center().y),
                            egui::Align2::RIGHT_CENTER,
                            archived_count.to_string(),
                            FontId::proportional(12.5),
                            rgb(t.text_faint),
                        );
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            self.screen = crate::state::Screen::Archived;
                            self.filter = ChatFilter::Archived;
                        }
                    }
                }
                let photos = self.demo_photos(ui.ctx());
                let visible = filter_chats(&self.chats, self.filter, &self.search);
                let mut clicked: Option<u64> = None;
                // A row context-menu pick (chat id, what it asks
                // for), applied after the list renders. Archiving
                // lives on the left, on the rows themselves (the
                // WhatsApp Web pattern): the conversation header
                // menu carries no archive item, and there is no
                // separate Archived entry row in this panel (the
                // rail and the filter chip are the entries).
                let row_action = std::cell::Cell::new(None::<(u64, RowPick)>);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .content_margin(egui::Margin {
                        bottom: self.nav_scroll_inset() as i8,
                        ..Default::default()
                    })
                    .show_rows(ui, 64.0, visible.len(), |ui, range| {
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
                                    row_action
                                        .set(Some((chat.id, RowPick::SetArchived(!chat.archived))));
                                    ui.close();
                                }
                                let fav_label = if chat.favorite {
                                    crate::i18n::t("conversation.remove_favorite")
                                } else {
                                    crate::i18n::t("conversation.add_favorite")
                                };
                                if crate::widgets::menu_item_icon(
                                    ui,
                                    t,
                                    Some(crate::icons::Icon::Star),
                                    &fav_label,
                                    None,
                                )
                                .clicked()
                                {
                                    row_action
                                        .set(Some((chat.id, RowPick::SetFavorite(!chat.favorite))));
                                    ui.close();
                                }
                            });
                        }
                    });
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
                if let Some((id, pick)) = row_action.get() {
                    match pick {
                        RowPick::SetArchived(target_archived) => {
                            if let Some(c) = self.chats.iter().find(|c| c.id == id) {
                                let (pinned, muted) = (c.pinned, c.muted);
                                self.apply_chat_action(id, target_archived, pinned, muted);
                            }
                        }
                        RowPick::SetFavorite(target) => {
                            self.apply_favorite(id, target);
                        }
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

impl App {
    /// The portrait chat list header: the app name on the left
    /// and a kebab menu on the right carrying the destinations
    /// the rail holds in the other modes (New group, Starred,
    /// Settings), the phone pattern of one title and one menu.
    /// On a filtered page (Channels, Groups, Archived) the
    /// header is instead a back chevron and the destination's
    /// own title: those pages are full-screen here, and the
    /// chevron is the way back to the chat list.
    fn draw_chat_list_header_portrait(&mut self, ui: &mut egui::Ui, t: Tokens) {
        let mut go: Option<crate::state::Screen> = None;
        let mut new_group = false;
        ui.horizontal(|ui| {
            if self.screen.is_filtered_list() {
                if crate::widgets::icon_button(
                    ui,
                    t,
                    crate::icons::Icon::ChevronLeft,
                    32.0,
                    &crate::i18n::t("common.back"),
                    None,
                )
                .clicked()
                {
                    go = Some(crate::state::Screen::Chats);
                }
                ui.label(
                    RichText::new(crate::i18n::t(self.screen.filtered_title_key()))
                        .font(crate::theme::medium(16.0))
                        .color(rgb(t.text_primary)),
                );
                return;
            }
            // The app name is the title, like the phone client:
            // the brand is a name, not a localized string.
            ui.label(
                RichText::new("Zaprust")
                    .font(crate::theme::semibold(20.0))
                    .color(rgb(t.text_primary)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let kebab = crate::widgets::icon_button(
                    ui,
                    t,
                    crate::icons::Icon::MoreVert,
                    32.0,
                    &crate::i18n::t("conversation.more_options"),
                    None,
                );
                egui::Popup::menu(&kebab)
                    .frame(crate::theme::glass_panel_frame(t, 12))
                    .width(220.0)
                    .show(|ui| {
                        if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::Edit),
                            &crate::i18n::t("chats.new_group"),
                            None,
                        )
                        .clicked()
                        {
                            new_group = true;
                            ui.close();
                        }
                        if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::Star),
                            &crate::i18n::t("nav.starred"),
                            None,
                        )
                        .clicked()
                        {
                            go = Some(crate::state::Screen::Starred);
                            ui.close();
                        }
                        if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::Sliders),
                            &crate::i18n::t("nav.settings"),
                            None,
                        )
                        .clicked()
                        {
                            go = Some(crate::state::Screen::Settings);
                            ui.close();
                        }
                    });
            });
        });
        if new_group {
            self.groups.create_open = true;
        }
        if let Some(screen) = go {
            self.screen = screen;
            if screen == crate::state::Screen::Chats {
                self.filter = ChatFilter::All;
            }
        }
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
        } else if chat.subtitle == "typing…" {
            // Demo data records a typing contact in the subtitle
            // (the model's typing exception, where the preview is
            // the typing label instead of the last message). The
            // row takes the typing state like any other typing
            // chat, so the localized typing line replaces the
            // snippet and no delivery ticks paint beside it: a
            // receipt glyph has no meaning next to a typing state.
            return Some(RowState::Typing(None));
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
    // between fills. The fills are the theme's step derivations
    // off the panel surface, the same language the shared button
    // uses: hover is one value step up from rest, pressed one
    // step the other way while the pointer is down. The selected
    // row keeps its steady, stronger wash, unchanged.
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
            let fill = core::theme::mix(t.bg_panel, core::theme::hover_step(t.bg_panel), hov);
            ui.painter().rect_filled(rect, CornerRadius::same(10), rgb(fill));
        }
        if press > 0.001 {
            let fill = core::theme::mix(t.bg_panel, core::theme::pressed_step(t.bg_panel), press);
            ui.painter().rect_filled(rect, CornerRadius::same(10), rgb(fill));
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
            crate::icons::SIZE_ROW,
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
        let count_label = core::filter::format_count(u64::from(chat.unread));
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
        let label = core::filter::format_count(u64::from(chat.unread));
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
        // still there, it just does not shout in the accent.
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
