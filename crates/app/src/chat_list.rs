//! Chat list panel: search, filter chips, rows, and the row
//! context menu (archiving lives here, on the left).

use crate::state::App;
use crate::theme::{rgb, SP_1, SP_2, SP_2_5};
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
    /// Set the chat's locked state.
    SetLocked(bool),
    /// Set the chat's favorite state.
    SetFavorite(bool),
    /// Mark the chat as unread (section 24).
    MarkUnread,
    /// Open the clear-chat confirmation for the chat.
    ClearChat,
    /// Open the delete-chat confirmation for the chat.
    DeleteChat,
    /// Open the label checklist for the chat.
    LabelPicker,
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
        ChatFilter::Locked => "nav.locked",
    }
}

pub(crate) fn chat_preview_line(chat: &Chat) -> String {
    match (&chat.preview_sender, chat.kind) {
        (Some(s), ChatKind::Group) => format!("{s}: {}", chat.preview),
        _ => chat.preview.clone(),
    }
}

/// Whether a chat's preview text says anything beyond the
/// media label that stands beside it: a captioned photo's
/// preview is its caption (it adds), an uncaptioned photo's
/// preview is the bare word "Photo" (it adds nothing, and
/// painting both produced the duplicated "Photo Photo").
fn preview_adds_beyond_label(preview: &str, label: &str) -> bool {
    !(preview.is_empty() || preview == label || label.starts_with(preview))
}

/// The full preview segment a chat row paints, media label
/// included: the media word appears exactly once. When the
/// preview adds a caption, the label leads it as before; when
/// it does not, the composed line is the sender prefix (for
/// groups) plus the label, whose form also carries what the
/// bare preview lacks (a voice note's duration).
pub(crate) fn chat_preview_display(chat: &Chat) -> String {
    let line = chat_preview_line(chat);
    let Some(media) = chat.preview_media else { return line };
    let label = media.label();
    if preview_adds_beyond_label(&chat.preview, &label) {
        format!("{label} {line}")
    } else {
        match (&chat.preview_sender, chat.kind) {
            (Some(s), ChatKind::Group) => format!("{s}: {label}"),
            _ => label.to_owned(),
        }
    }
}

/// Minimum width of a chat row's context menu (P2-26).
const ROW_MENU_MIN_W: f32 = 170.0;

impl App {
    /// The visible chats for the current filter and query,
    /// memoized on a cheap roster fingerprint (P2-68): an
    /// FNV fold over everything the filter reads (ids, titles,
    /// previews, senders, pin/archive/mute state, timestamps),
    /// plus the filter and query themselves. Byte folding is
    /// O(text) with no allocation; the full filter (with its
    /// three lowercase allocations per chat) runs only when
    /// the fingerprint changes.
    pub(crate) fn visible_chats(&self) -> Vec<&Chat> {
        let mut fp: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |bytes: &[u8]| {
            for b in bytes {
                fp ^= u64::from(*b);
                fp = fp.wrapping_mul(0x0100_0000_01b3);
            }
        };
        mix(self.search.as_bytes());
        mix(&[self.filter as u8]);
        // The label filter reads the business state, so it is
        // folded in too: the filtered label's id and how many
        // chats carry it (association changes re-run the
        // filter, definition-only changes do not need to).
        if let Some(label_id) = &self.business.label_filter {
            mix(label_id.as_bytes());
            let tagged =
                self.business.chat_labels.values().filter(|set| set.contains(label_id)).count();
            mix(&tagged.to_le_bytes());
        }
        mix(&self.chats.len().to_le_bytes());
        for c in &self.chats {
            mix(&c.id.to_le_bytes());
            mix(c.title.as_bytes());
            mix(c.preview.as_bytes());
            if let Some(s) = &c.preview_sender {
                mix(s.as_bytes());
            }
            mix(c.time.as_bytes());
            mix(&c.unread.to_le_bytes());
            mix(&[c.kind as u8, u8::from(c.pinned), u8::from(c.archived), u8::from(c.favorite)]);
        }
        if let Some((key, indices)) = self.filter_memo.borrow().as_ref() {
            if *key == fp {
                return indices.iter().filter_map(|i| self.chats.get(*i)).collect();
            }
        }
        let mut filtered = filter_chats(&self.chats, self.filter, &self.search);
        if let Some(label_id) = &self.business.label_filter {
            // The label filter narrows the list to the chats
            // carrying the label, resolved through the live
            // session's JID book.
            let tagged_jids: std::collections::HashSet<&str> = self
                .business
                .chat_labels
                .iter()
                .filter(|(_, set)| set.contains(label_id))
                .map(|(jid, _)| jid.as_str())
                .collect();
            let jids: std::collections::HashMap<u64, &str> = self
                .live
                .as_ref()
                .map(|session| session.jids.iter().map(|(id, jid)| (*id, jid.as_str())).collect())
                .unwrap_or_default();
            filtered.retain(|c| jids.get(&c.id).is_some_and(|jid| tagged_jids.contains(jid)));
        }
        let base = self.chats.as_ptr() as usize;
        let indices: Vec<usize> = filtered
            .iter()
            .map(|c| ((*c as *const Chat) as usize - base) / std::mem::size_of::<Chat>())
            .collect();
        let out: Vec<&Chat> = indices.iter().filter_map(|i| self.chats.get(*i)).collect();
        *self.filter_memo.borrow_mut() = Some((fp, indices));
        out
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
                                .font(crate::theme::medium(15.0))
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
                                .font(crate::theme::medium(15.0))
                                .color(rgb(t.text_primary)),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if crate::widgets::icon_button(
                                ui,
                                t,
                                crate::icons::Icon::Chat,
                                28.0,
                                &crate::i18n::t("chats.new_chat"),
                                None,
                            )
                            .clicked()
                            {
                                self.new_chat.open = true;
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
                }
                ui.add_space(SP_2_5);
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
                ui.add_space(SP_2);
                // Session state row (design standard section 14;
                // audit S11/S12): while linked, the list says what
                // the session is doing, so an empty or stale list
                // never reads as silently broken. Sync progress
                // rides the shared loading row; a lost connection
                // is a plain label in the same slot.
                if let Some(session) = &self.live {
                    if matches!(session.status, crate::live::LinkStatus::Crashed(_)) {
                        // Terminal states win over the transient
                        // ones below: a crashed or banned session
                        // is not coming back on its own, and the
                        // row says so in the danger voice instead
                        // of leaving the list silently stale
                        // (audit S11 completion).
                        ui.label(
                            RichText::new(crate::i18n::t("chats.connection_stopped"))
                                .size(12.0)
                                .color(rgb(t.danger)),
                        );
                        ui.add_space(SP_2);
                    } else if matches!(session.status, crate::live::LinkStatus::Banned(_)) {
                        ui.label(
                            RichText::new(crate::i18n::t("chats.account_banned"))
                                .size(12.0)
                                .color(rgb(t.danger)),
                        );
                        ui.add_space(SP_2);
                    } else if matches!(session.status, crate::live::LinkStatus::Lost(_)) {
                        ui.label(
                            RichText::new(crate::i18n::t("chats.connection_lost"))
                                .size(12.0)
                                .color(rgb(t.text_secondary)),
                        );
                        ui.add_space(SP_2);
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
                            14.0,
                            t.text_secondary,
                        );
                        ui.add_space(SP_2);
                    }
                }
                // The filter chips live on the shared chip row:
                // whole chips flow onto a second row when the
                // panel is too narrow for one row, so every
                // filter is fully visible at rest (the scrolling
                // strip before v1.6.24 rested with the last chip
                // clipped at the panel edge). Filtered
                // destinations show no chip row at all: the
                // destination itself is the filter, and its
                // header above already says which one.
                if !filtered {
                    crate::widgets::chip_flow_row(ui, |ui| {
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
                                    // Locked is never a chip (the
                                    // folder's entry row is its
                                    // way in); the arm exists for
                                    // exhaustiveness. The counted
                                    // filters exclude locked chats
                                    // like the lists they label do.
                                    ChatFilter::All | ChatFilter::Archived | ChatFilter::Locked => {
                                        0
                                    }
                                    ChatFilter::Unread => self
                                        .chats
                                        .iter()
                                        .filter(|c| !c.archived && !c.locked && c.unread > 0)
                                        .count(),
                                    ChatFilter::Private => self
                                        .chats
                                        .iter()
                                        .filter(|c| {
                                            !c.archived && !c.locked && c.kind == ChatKind::Dm
                                        })
                                        .count(),
                                    ChatFilter::Favorites => self
                                        .chats
                                        .iter()
                                        .filter(|c| !c.archived && !c.locked && c.favorite)
                                        .count(),
                                    ChatFilter::Groups => self
                                        .chats
                                        .iter()
                                        .filter(|c| {
                                            !c.archived && !c.locked && c.kind == ChatKind::Group
                                        })
                                        .count(),
                                    ChatFilter::Channels => self
                                        .chats
                                        .iter()
                                        .filter(|c| {
                                            !c.archived && !c.locked && c.kind == ChatKind::Channel
                                        })
                                        .count(),
                                };
                                let name = crate::i18n::t(filter_label_key(f));
                                let label = if count > 0 {
                                    format!("{name} {}", core::filter::format_count(count as u64))
                                } else {
                                    name
                                };
                                let resp = chip(ui, &label, selected, t);
                                if resp.clicked() {
                                    self.filter = f;
                                }
                            }
                    });
                }
                // The active label filter, as a chip under
                // the filter row: the label's name on a
                // selected chip; clicking it clears the
                // filter. Shown whenever a filter is set, so
                // it can always be undone from here.
                if let Some(label_id) = self.business.label_filter.clone() {
                    if let Some(label) =
                        self.business.labels.iter().find(|l| l.id == label_id).cloned()
                    {
                        ui.add_space(SP_1);
                        ui.horizontal(|ui| {
                            let resp = chip(ui, &label.name, true, t);
                            if resp.clicked() {
                                self.business.label_filter = None;
                            }
                            ui.label(
                                RichText::new(crate::i18n::t("labels.clear_filter"))
                                    .size(11.5)
                                    .color(rgb(t.text_faint)),
                            );
                        });
                    }
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
                        ui.add_space(SP_1);
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
                            FontId::proportional(12.0),
                            rgb(t.text_faint),
                        );
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            self.screen = crate::state::Screen::Archived;
                            self.filter = ChatFilter::Archived;
                        }
                    }
                }
                // The locked-chats entry row, in every layout:
                // shown on the unfiltered, unsearched list
                // while locked chats exist or a lock code is
                // set. It carries the locked count and never a
                // preview: what is inside stays behind the code
                // prompt its click raises. No rail entry exists
                // for the folder; this row is the way in.
                if self.screen != crate::state::Screen::Locked
                    && self.filter == ChatFilter::All
                    && self.search.trim().is_empty()
                {
                    let locked_count = core::filter::locked_count(&self.chats);
                    if locked_count > 0 || self.lock_code.is_some() {
                        ui.add_space(SP_1);
                        let (rect, resp) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 48.0),
                            Sense::click(),
                        );
                        let hov = crate::anim::hover_factor(ui, "locked_entry_row", resp.hovered());
                        if hov > 0.001 {
                            ui.painter().rect_filled(
                                rect,
                                CornerRadius::same(10),
                                crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
                            );
                        }
                        crate::icons::draw(
                            ui.painter(),
                            crate::icons::Icon::Lock,
                            Pos2::new(rect.left() + AVATAR_CX, rect.center().y),
                            crate::icons::SIZE_CONTROL,
                            rgb(t.text_secondary),
                            rgb(t.bg_panel),
                        );
                        ui.painter().text(
                            Pos2::new(rect.left() + TEXT_X, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            crate::i18n::t("nav.locked"),
                            crate::theme::medium(15.0),
                            rgb(t.text_primary),
                        );
                        ui.painter().text(
                            Pos2::new(rect.right() - 6.0, rect.center().y),
                            egui::Align2::RIGHT_CENTER,
                            locked_count.to_string(),
                            FontId::proportional(12.0),
                            rgb(t.text_faint),
                        );
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            self.request_locked_folder();
                        }
                    }
                }
                // The labels entry row, beside the locked
                // folder's: shown on the unfiltered, unsearched
                // list while the account has labels. It carries
                // the label count; the panel it opens is the
                // way to manage them and to filter by one.
                if self.filter == ChatFilter::All
                    && self.search.trim().is_empty()
                    && !self.business.labels.is_empty()
                {
                    ui.add_space(SP_1);
                    let (rect, resp) = ui
                        .allocate_exact_size(Vec2::new(ui.available_width(), 48.0), Sense::click());
                    let hov = crate::anim::hover_factor(ui, "labels_entry_row", resp.hovered());
                    if hov > 0.001 {
                        ui.painter().rect_filled(
                            rect,
                            CornerRadius::same(10),
                            crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
                        );
                    }
                    crate::icons::draw(
                        ui.painter(),
                        crate::icons::Icon::Label,
                        Pos2::new(rect.left() + AVATAR_CX, rect.center().y),
                        crate::icons::SIZE_CONTROL,
                        rgb(t.text_secondary),
                        rgb(t.bg_panel),
                    );
                    ui.painter().text(
                        Pos2::new(rect.left() + TEXT_X, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        crate::i18n::t("nav.labels"),
                        crate::theme::medium(15.0),
                        rgb(t.text_primary),
                    );
                    ui.painter().text(
                        Pos2::new(rect.right() - 6.0, rect.center().y),
                        egui::Align2::RIGHT_CENTER,
                        self.business.labels.len().to_string(),
                        FontId::proportional(12.0),
                        rgb(t.text_faint),
                    );
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.screen = crate::state::Screen::Labels;
                    }
                }
                let mut photos = self.demo_photos(ui.ctx());
                if self.live.is_some() {
                    // Live rows wear their real profile
                    // pictures: the avatar pipeline is ensured
                    // for every roster chat (it dedups and
                    // caches), and whatever has landed overlays
                    // the demo map.
                    // Ids only (P2-68): cloning the whole
                    // roster every frame just to satisfy the
                    // borrow checker was steady allocation
                    // churn on the UI thread at large rosters.
                    // The live avatar path needs only the id.
                    let roster_ids: Vec<u64> = self.chats.iter().map(|c| c.id).collect();
                    for id in roster_ids {
                        let jid = self.live.as_ref().and_then(|s| s.jids.get(&id)).cloned();
                        if let Some(jid) = jid {
                            self.ensure_avatar(ui.ctx(), &jid);
                            if let Some(tex) = self.live_avatar(&jid) {
                                photos.insert(id, tex.id());
                            }
                        }
                    }
                }
                // Global message search (section 25): while a
                // query is typed, the panel also carries the
                // message matches across every chat, in a
                // Messages section below the chat rows. The
                // results refresh once per query change, and
                // the chat rows' scroll area shrinks to make
                // room: matched sets are bounded (the roster,
                // and the 100-result cap), so the section split
                // stays stable while searching. The unsearched
                // list keeps its full-height area untouched.
                // The refresh runs before the row filter: the
                // filter borrows the roster for its result list.
                let searching = !self.search.trim().is_empty();
                if searching {
                    self.refresh_global_search();
                }
                // Verified badges: settle the flags from the
                // session's book, and ask (once per identity)
                // for the DM roster's verified names.
                self.apply_verified_badges();
                if self.live.is_some() {
                    let ask: Vec<String> = self
                        .chats
                        .iter()
                        .filter(|c| c.kind == ChatKind::Dm)
                        .filter_map(|c| self.live.as_ref().and_then(|s| s.jids.get(&c.id)).cloned())
                        .collect();
                    for jid in &ask {
                        self.ensure_verified_name(jid);
                    }
                }
                // The locked folder's gate: until the code
                // passes, the panel shows the gate state and
                // none of the folder's rows (their titles and
                // previews are exactly what the code protects).
                if self.screen == crate::state::Screen::Locked && !self.folder_unlocked {
                    self.draw_locked_gate(ui, t);
                    return;
                }
                let visible = self.visible_chats();
                let mut clicked: Option<u64> = None;
                // A row context-menu pick (chat id, what it asks
                // for), applied after the list renders. Archiving
                // lives on the left, on the rows themselves (the
                // WhatsApp Web pattern): the conversation header
                // menu carries no archive item, and there is no
                // separate Archived entry row in this panel (the
                // rail and the filter chip are the entries).
                let row_action = std::cell::Cell::new(None::<(u64, RowPick)>);
                let mut scroll = egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .content_margin(egui::Margin {
                        bottom: self.nav_scroll_inset() as i8,
                        ..Default::default()
                    });
                if searching {
                    // Shrink to the matched rows (bounded by the
                    // space the Messages section needs), so both
                    // sections share the panel while searching.
                    let rows_h = visible.len() as f32 * 64.0 + 8.0;
                    let cap = (ui.available_height() - 190.0).max(70.0);
                    scroll = scroll.auto_shrink([false, true]).max_height(rows_h.min(cap));
                }
                scroll.show_rows(ui, 64.0, visible.len(), |ui, range| {
                    for idx in range {
                        let chat = visible[idx];
                        let state = self.row_state_for(chat);
                        let tags = self.chat_tags(chat.id);
                        let resp = chat_row(
                            ui,
                            chat,
                            chat.id == self.selected_chat,
                            chat.kind == ChatKind::Dm && self.blocked_chats.contains(&chat.id),
                            t,
                            state,
                            None,
                            photos.get(&chat.id).copied(),
                            &tags,
                        );
                        if resp.clicked() {
                            clicked = Some(chat.id);
                        }
                        resp.context_menu(|ui| {
                            ui.set_min_width(ROW_MENU_MIN_W);
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
                            let lock_label = if chat.locked {
                                crate::i18n::t("conversation.unlock_chat")
                            } else {
                                crate::i18n::t("conversation.lock_chat")
                            };
                            if crate::widgets::menu_item_icon(
                                ui,
                                t,
                                Some(crate::icons::Icon::Lock),
                                &lock_label,
                                None,
                            )
                            .clicked()
                            {
                                row_action.set(Some((chat.id, RowPick::SetLocked(!chat.locked))));
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
                            if crate::widgets::menu_item_icon(
                                ui,
                                t,
                                Some(crate::icons::Icon::Label),
                                &crate::i18n::t("labels.menu"),
                                None,
                            )
                            .clicked()
                            {
                                row_action.set(Some((chat.id, RowPick::LabelPicker)));
                                ui.close();
                            }
                            if chat.unread == 0
                                && crate::widgets::menu_item_icon(
                                    ui,
                                    t,
                                    Some(crate::icons::Icon::Chat),
                                    &crate::i18n::t("conversation.mark_unread"),
                                    None,
                                )
                                .clicked()
                            {
                                row_action.set(Some((chat.id, RowPick::MarkUnread)));
                                ui.close();
                            }
                            if crate::widgets::menu_item_danger(
                                ui,
                                t,
                                crate::icons::Icon::Delete,
                                &crate::i18n::t("chat.clear_title"),
                            )
                            .clicked()
                            {
                                row_action.set(Some((chat.id, RowPick::ClearChat)));
                                ui.close();
                            }
                            if crate::widgets::menu_item_danger(
                                ui,
                                t,
                                crate::icons::Icon::Delete,
                                &crate::i18n::t("chat.delete_title"),
                            )
                            .clicked()
                            {
                                row_action.set(Some((chat.id, RowPick::DeleteChat)));
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
                        RowPick::SetLocked(target_locked) => {
                            self.request_lock_chat(id, target_locked);
                        }
                        RowPick::SetFavorite(target) => {
                            self.apply_favorite(id, target);
                        }
                        RowPick::MarkUnread => {
                            self.mark_chat_unread(id);
                        }
                        RowPick::ClearChat => {
                            self.clear_keep_starred = true;
                            if let Some(c) = self.chats.iter().find(|c| c.id == id) {
                                self.ui_confirm = Some(crate::state::UiConfirm::ClearChat {
                                    chat_id: id,
                                    name: c.title.clone(),
                                });
                            }
                        }
                        RowPick::LabelPicker => {
                            self.business.label_picker =
                                Some(crate::state::LabelPickerTarget::Chat(id));
                        }
                        RowPick::DeleteChat => {
                            if let Some(c) = self.chats.iter().find(|c| c.id == id) {
                                self.ui_confirm = Some(crate::state::UiConfirm::DeleteChat {
                                    chat_id: id,
                                    name: c.title.clone(),
                                });
                            }
                        }
                    }
                }
                if no_visible {
                    // A fresh account with zero chats and no
                    // filter or search active gets its own
                    // honest state (P2-69): nothing was matched
                    // against anything, so "no match" would lie.
                    let roster_empty = self.chats.is_empty() && !searching;
                    ui.label(
                        RichText::new(crate::i18n::t(if roster_empty {
                            "chats.none_yet"
                        } else {
                            "chats.no_match"
                        }))
                        .color(rgb(t.text_secondary)),
                    );
                }
                if searching {
                    self.draw_global_search_results(ui, t);
                }
            });
        crate::widgets::soft_right_edge(root, panel.response.rect, t);
    }

    /// The locked folder's gate body: shown in place of the
    /// folder's rows until the code prompt passes. It names
    /// what the folder holds in count only and offers the one
    /// action that opens it.
    fn draw_locked_gate(&mut self, ui: &mut egui::Ui, t: Tokens) {
        let has_code = self.lock_code.is_some();
        ui.vertical_centered(|ui| {
            ui.add_space(72.0);
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(48.0), Sense::hover());
            crate::icons::draw(
                ui.painter(),
                crate::icons::Icon::Lock,
                rect.center(),
                40.0,
                rgb(t.text_secondary),
                rgb(t.bg_panel),
            );
            ui.add_space(16.0);
            ui.label(
                RichText::new(crate::i18n::t("nav.locked"))
                    .font(crate::theme::medium(16.0))
                    .color(rgb(t.text_primary)),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new(crate::i18n::t(if has_code {
                    "locked.gate_body"
                } else {
                    "locked.gate_create_body"
                }))
                .color(rgb(t.text_secondary)),
            );
            ui.add_space(16.0);
            if crate::widgets::button(
                ui,
                t,
                &crate::i18n::t(if has_code { "locked.enter_code" } else { "locked.create_code" }),
                crate::widgets::Btn::Primary,
            )
            .clicked()
            {
                self.open_code_prompt(if has_code {
                    crate::state::CodePrompt::OpenFolder
                } else {
                    crate::state::CodePrompt::CreateThenFolder
                });
            }
        });
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
        let mut new_chat = false;
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
                        .font(crate::theme::medium(15.0))
                        .color(rgb(t.text_primary)),
                );
                return;
            }
            // The app name is the title, like the phone client:
            // the brand is a name, not a localized string.
            ui.label(
                RichText::new("Zaprust")
                    .font(crate::theme::semibold(19.0))
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
                    .frame(crate::theme::glass_panel_frame(t, crate::theme::RADIUS_PANEL))
                    .width(220.0)
                    .show(|ui| {
                        if crate::widgets::menu_item_icon(
                            ui,
                            t,
                            Some(crate::icons::Icon::Chat),
                            &crate::i18n::t("chats.new_chat"),
                            None,
                        )
                        .clicked()
                        {
                            new_chat = true;
                            ui.close();
                        }
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
        if new_chat {
            self.new_chat.open = true;
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
    pub(crate) fn row_state_for(&self, chat: &Chat) -> Option<RowState> {
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
    tags: &[crate::labels::ChatTag],
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
    let time_font = FontId::proportional(12.0);
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
        if chat.unread > 0 { crate::theme::semibold(15.0) } else { crate::theme::medium(15.0) };
    let title_budget =
        (rect.right() - 6.0 - time_w - blocked_w - 8.0 - badges_w - text_left).max(0.0);
    let title = core::bidi::display(&crate::widgets::ellipsize(
        ui,
        &chat.title,
        title_font.clone(),
        title_budget,
    ));
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
    let snippet_font = FontId::proportional(12.0);
    let line_y = rect.top() + SNIPPET_DY;
    let mut right_limit = rect.right() - 6.0;
    if chat.mentions > 0 {
        right_limit = right_limit.min(rect.right() - 56.0);
    }
    if chat.unread > 0 {
        let count_label = core::filter::format_count(u64::from(chat.unread));
        let count_w = ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap(count_label, FontId::proportional(10.0), rgb(t.badge_text)).size().x
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
                f.layout_no_wrap(prefix.clone(), crate::theme::semibold(12.0), rgb(t.accent))
                    .size()
                    .x
            });
            painter.text(
                Pos2::new(text_left, line_y),
                egui::Align2::LEFT_CENTER,
                prefix,
                crate::theme::semibold(12.0),
                rgb(t.accent),
            );
            let body = core::bidi::display(&crate::widgets::ellipsize(
                ui,
                draft,
                snippet_font.clone(),
                right_limit - text_left - prefix_w - 6.0,
            ));
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
    // Label tags lead the snippet line (up to two, in name
    // order): small chips in the label's own color, ahead of
    // the preview text, which ellipsizes into what is left.
    if row_state.is_none() {
        for tag in tags.iter() {
            if x + 60.0 > right_limit {
                break;
            }
            x = crate::labels::paint_label_tag(painter, ui, x, line_y, tag, t.bg_panel) + 6.0;
        }
    }
    // The preview segment is composed whole (label included,
    // the media word exactly once) and painted in one run.
    let preview = chat_preview_display(chat);
    let preview = core::bidi::display(&crate::widgets::ellipsize(
        ui,
        &preview,
        snippet_font.clone(),
        right_limit - x,
    ));
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
        let font = FontId::proportional(10.0 * scale);
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
            FontId::proportional(10.0),
            rgb(t.badge_text),
        );
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The snippet a global search result shows: the matched text
/// windowed around the first match (about 24 characters of
/// lead-in and 56 of follow-through), ellipsized at the cuts,
/// on one line. Without a match (a caller error), the head of
/// the text stands in.
pub(crate) fn search_snippet(body: &str, query: &str) -> String {
    let flat: String = body.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    let needle = query.trim().to_lowercase();
    let lower = flat.to_lowercase();
    let Some(byte_pos) = lower.find(&needle) else {
        return flat.chars().take(90).collect();
    };
    let chars: Vec<char> = flat.chars().collect();
    let match_at = lower[..byte_pos].chars().count();
    let match_len = needle.chars().count();
    let start = match_at.saturating_sub(24);
    let end = (match_at + match_len + 56).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('\u{2026}');
    }
    out.extend(&chars[start..end]);
    if end < chars.len() {
        out.push('\u{2026}');
    }
    out
}

/// A global search hit's date line, in the chat list's date
/// language (the Starred panel's compact form): today's hits
/// show their clock, yesterday's its word, older ones a short
/// calendar date.
fn hit_date(timestamp: i64) -> String {
    match crate::live::fmt_day(timestamp).as_str() {
        "Today" => crate::live::fmt_time(timestamp),
        "Yesterday" => "Yesterday".to_owned(),
        label => {
            let mut parts = label.split_whitespace();
            match (parts.next(), parts.next(), parts.next()) {
                (Some(d), Some(m), Some(y)) => {
                    format!("{d} {} {y}", &m[..3.min(m.len())])
                }
                _ => label.to_owned(),
            }
        }
    }
}

impl App {
    /// Recomputes the global message search when the chat
    /// list's query changed since the last computation
    /// (section 25). Live mode asks the archive store (the one
    /// honest query path over every chat); demo mode scans the
    /// in-memory chats with the same visible-text matcher, the
    /// open chat read from its working copy.
    pub(crate) fn refresh_global_search(&mut self) {
        let query = self.search.trim().to_owned();
        if self.global_search.query == query {
            return;
        }
        self.global_search.query = query.clone();
        self.global_search.hits.clear();
        self.global_search.capped = false;
        if query.is_empty() {
            return;
        }
        const CAP: usize = 100;
        if self.live_mode {
            let Some(session) = &self.live else { return };
            let Some(store) = &session.store else { return };
            let Ok(found) = store.search_messages(&query, CAP + 1) else { return };
            self.global_search.capped = found.len() > CAP;
            for hit in found.into_iter().take(CAP) {
                let chat_id = crate::live::jid_id(&hit.chat_jid);
                let Some(chat) = self.chats.iter().find(|c| c.id == chat_id) else {
                    continue;
                };
                // Locked chats are invisible to global search:
                // their content sits behind the lock code, and
                // a hit here would quote it outside the gate.
                if chat.locked {
                    continue;
                }
                self.global_search.hits.push(crate::state::GlobalHit {
                    chat_id,
                    msg_id: crate::live::message_id(&hit.chat_jid, &hit.message_id),
                    chat_title: chat.title.clone(),
                    sender: if hit.from_me { None } else { Some(hit.sender.clone()) },
                    snippet: search_snippet(&hit.body, &query),
                    when: hit_date(hit.timestamp),
                });
            }
            return;
        }
        let mut hits: Vec<crate::state::GlobalHit> = Vec::new();
        for chat in &self.chats {
            // Locked chats are skipped here too (demo path):
            // the same gate as the live search above.
            if chat.locked {
                continue;
            }
            let messages: &[core::model::Message] =
                if chat.id == self.selected_chat { &self.conversation } else { &chat.messages };
            for msg in messages.iter().rev() {
                if core::filter::message_text_matches(msg, &query) {
                    hits.push(crate::state::GlobalHit {
                        chat_id: chat.id,
                        msg_id: msg.id,
                        chat_title: chat.title.clone(),
                        sender: msg.sender.clone(),
                        snippet: search_snippet(core::filter::visible_text(msg), &query),
                        when: crate::starred::compact_date(msg),
                    });
                    if hits.len() > CAP {
                        break;
                    }
                }
            }
            if hits.len() > CAP {
                break;
            }
        }
        self.global_search.capped = hits.len() > CAP;
        hits.truncate(CAP);
        self.global_search.hits = hits;
    }

    /// The Messages section under the chat rows while the list
    /// search carries a query (section 25): one row per match
    /// on the shared chat-row geometry (the Starred panel's
    /// anatomy), opening the hit's chat at the hit's message
    /// through the one stream locate path.
    pub(crate) fn draw_global_search_results(&mut self, ui: &mut egui::Ui, t: Tokens) {
        ui.add_space(SP_2_5);
        ui.label(
            RichText::new(crate::i18n::t("search.messages"))
                .font(crate::theme::semibold(12.0))
                .color(rgb(t.text_secondary)),
        );
        ui.add_space(SP_1);
        if self.global_search.hits.is_empty() {
            ui.label(
                RichText::new(crate::i18n::t("search.no_messages")).color(rgb(t.text_secondary)),
            );
            return;
        }
        let hits = std::mem::take(&mut self.global_search.hits);
        let mut open: Option<(u64, u64)> = None;
        egui::ScrollArea::vertical()
            .id_salt("global_search_results")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for hit in &hits {
                    let (rect, resp) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), ROW_H),
                        Sense::click(),
                    );
                    let hov =
                        crate::anim::hover_factor(ui, ("search_hit", hit.msg_id), resp.hovered());
                    if hov > 0.001 {
                        ui.painter().rect_filled(
                            rect,
                            CornerRadius::same(10),
                            crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
                        );
                    }
                    let painter = ui.painter();
                    if let Some(chat) = self.chats.iter().find(|c| c.id == hit.chat_id) {
                        crate::widgets::paint_chat_avatar(
                            painter,
                            Pos2::new(rect.left() + AVATAR_CX, rect.center().y),
                            AVATAR_R,
                            chat,
                        );
                    }
                    let text_left = rect.left() + TEXT_X;
                    let date_font = FontId::proportional(12.0);
                    let date_w = ui.ctx().fonts_mut(|f| {
                        f.layout_no_wrap(hit.when.clone(), date_font.clone(), rgb(t.text_faint))
                            .size()
                            .x
                    });
                    painter.text(
                        Pos2::new(rect.right() - 6.0, rect.top() + TITLE_DY),
                        egui::Align2::RIGHT_CENTER,
                        &hit.when,
                        date_font,
                        rgb(t.text_faint),
                    );
                    let title_budget = (rect.right() - 14.0 - date_w - text_left).max(0.0);
                    let title_text = core::bidi::display(&crate::widgets::ellipsize(
                        ui,
                        &hit.chat_title,
                        crate::theme::medium(15.0),
                        title_budget,
                    ));
                    painter.text(
                        Pos2::new(text_left, rect.top() + TITLE_DY),
                        egui::Align2::LEFT_CENTER,
                        title_text,
                        crate::theme::medium(15.0),
                        rgb(t.text_primary),
                    );
                    let line = match &hit.sender {
                        Some(sender) => format!("{sender}: {}", hit.snippet),
                        None => hit.snippet.clone(),
                    };
                    let budget = (rect.right() - 14.0 - text_left).max(0.0);
                    let line = core::bidi::display(&crate::widgets::ellipsize(
                        ui,
                        &line,
                        FontId::proportional(12.0),
                        budget,
                    ));
                    painter.text(
                        Pos2::new(text_left, rect.top() + SNIPPET_DY),
                        egui::Align2::LEFT_CENTER,
                        line,
                        FontId::proportional(12.0),
                        rgb(t.text_secondary),
                    );
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        open = Some((hit.chat_id, hit.msg_id));
                    }
                }
            });
        self.global_search.hits = hits;
        if self.global_search.capped {
            ui.add_space(SP_1);
            ui.label(
                RichText::new(crate::i18n::tf("search.more", "count", "100"))
                    .size(12.0)
                    .color(rgb(t.text_faint)),
            );
        }
        if let Some((chat_id, msg_id)) = open {
            // Opening a result follows the Starred panel's path:
            // switch chats when needed (the demo working copy
            // goes back first), then locate the message in the
            // stream with the scroll and the highlight wash.
            if chat_id != self.selected_chat {
                if !self.live_mode {
                    let previous = self.selected_chat;
                    self.write_back_conversation(previous);
                }
                self.open_community = None;
                self.community_home_from_overview = false;
                self.community_origin = None;
                self.select_chat_now(chat_id);
            }
            self.scroll_to_message = Some(msg_id);
            self.highlight_message = Some(msg_id);
            self.highlight_started = None;
        }
    }
}

#[cfg(test)]
mod search_tests {
    use super::{chat_preview_display, preview_adds_beyond_label, search_snippet};
    use core::model::{Chat, PreviewMedia};

    fn chat(preview: &str, sender: Option<&str>, media: Option<PreviewMedia>) -> Chat {
        let mut chat = core::model::demo_announcement_chat();
        chat.preview = preview.to_owned();
        chat.preview_sender = sender.map(ToOwned::to_owned);
        chat.preview_media = media;
        chat
    }

    /// The owner's live capture: an uncaptioned photo preview
    /// read "Photo Photo" because the body already was the
    /// media word and the row painted the label beside it. The
    /// composed line carries the word exactly once, in every
    /// shape: bare, group-prefixed, captioned, and voiced.
    #[test]
    fn preview_line_names_media_once() {
        let dm_photo = chat("Photo", None, Some(PreviewMedia::Photo));
        assert_eq!(chat_preview_display(&dm_photo), "Photo");
        let group_photo = chat("Photo", Some("Ana"), Some(PreviewMedia::Photo));
        assert_eq!(chat_preview_display(&group_photo), "Ana: Photo");
        let captioned = chat("sunset at the lake", Some("Ana"), Some(PreviewMedia::Photo));
        assert_eq!(chat_preview_display(&captioned), "Photo Ana: sunset at the lake");
        let voice = chat("Voice message", None, Some(PreviewMedia::Voice(9)));
        assert_eq!(chat_preview_display(&voice), "Voice message · 0:09");
        let text = chat("see you at 7", Some("Ana"), None);
        assert_eq!(chat_preview_display(&text), "Ana: see you at 7");
        assert!(!preview_adds_beyond_label("Photo", "Photo"));
        assert!(!preview_adds_beyond_label("Voice message", "Voice message · 0:09"));
        assert!(preview_adds_beyond_label("a caption", "Photo"));
    }

    #[test]
    fn snippet_windows_around_the_match() {
        let body = "the quick brown fox jumps over the lazy dog near the river bank at dawn";
        let out = search_snippet(body, "lazy");
        assert!(out.contains("lazy dog"), "{out}");
        assert!(out.starts_with('\u{2026}'), "long lead-in is ellipsized: {out}");
        // A short body passes through whole.
        assert_eq!(search_snippet("lazy day", "lazy"), "lazy day");
        // Newlines flatten: a snippet is one line.
        assert_eq!(search_snippet("one\nlazy\ntwo", "lazy"), "one lazy two");
        // No match: the head stands in.
        assert_eq!(search_snippet("nothing here", "zzz"), "nothing here");
    }
}
