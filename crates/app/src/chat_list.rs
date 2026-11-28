//! Chat list panel: search, filter chips, rows, and the row
//! context menu (archiving lives here, on the left).

use crate::state::App;
use crate::theme::{rgb, SP_1, SP_2, SP_2_5};
use crate::widgets::chip;
use core::filter::{filter_chats, ChatFilter};
use core::model::{Chat, ChatKind};
use core::theme::Tokens;
use eframe::egui::{self, FontId, Margin, Pos2, RichText, Sense, Vec2};

/// Shared chat-row metrics: owned by the shared list-row
/// component (`crate::widgets::list_row`), re-exported here so
/// existing callers keep their `chat_list::ROW_H` style paths.
/// The component owns the values; this module must not restate
/// them, or the copies would drift apart again.
pub(crate) use crate::widgets::{AVATAR_CX, AVATAR_R, ROW_H, SNIPPET_DY, TEXT_X, TITLE_DY};

/// Whether the window is currently visible for rendering:
/// not minimized and not fully occluded, per the viewport
/// signals egui reports. An unknown answer (platforms that do
/// not report occlusion) counts as visible: the sync row's
/// periodic repaint wakes exist to keep timed state honest,
/// and only a positive not-visible report may stop them. When
/// the window comes back, the next frame (raised by the OS
/// focus/expose path or any backend event) resumes the ticks
/// where they left off; nothing about the row's state depends
/// on the ticks having run while hidden.
pub(crate) fn window_visible(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.viewport().visible()) != Some(false)
}

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
    /// Set the chat's pinned state.
    SetPinned(bool),
    /// Set the chat's muted state (an indefinite mute, the
    /// row menu's one-step form of the header menu's choice).
    SetMuted(bool),
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

/// Every roster count the list header paints, from ONE pass
/// over the chats (qc1644): the filter chips (unread, personal,
/// favorites), the archived chip's presence and the portrait
/// archived row (archived), and the locked folder's entry row
/// (locked). These were five separate full-roster passes per
/// frame, each re-walking the roster with its own filter
/// closure. The counted chip filters exclude archived and
/// locked chats, like the lists they label do; the archived
/// and locked counts cover every chat in their state, matching
/// the entry rows that carry them.
#[derive(Clone, Copy, Default)]
pub(crate) struct RosterCounts {
    unread: usize,
    personal: usize,
    favorites: usize,
    archived: usize,
    locked: usize,
}

/// The chat list's verified-badge memo (qc1644): the DM
/// roster's JIDs and the set of chats already badged, built
/// under one key of the live session. The work it caches ran
/// on every frame before: a cloned JID list for every DM, a
/// rebuilt hash set, and a full roster scan, all producing the
/// same answer until the roster actually changes. The memo is
/// kept in egui's temp data, the way the filter chip row keeps
/// its drag state, and is rebuilt only when its key moves.
#[derive(Clone, Default)]
struct VerifiedBadgeMemo {
    /// The session's roster generation, verified-name count,
    /// and asserted-tier fold when the memo was built. The
    /// name count rides along because a verified name can land
    /// for a roster that did not change, and its badge must
    /// not wait for the next roster change to appear. The
    /// tier fold rides along because an assertion can change
    /// for an identity the roster and the name book already
    /// hold, and a downgrade must retract its badge without
    /// waiting either (qc1655 L-1). `None` while there is no
    /// live session.
    key: Option<(u64, usize, u64)>,
    dm_jids: Vec<String>,
    applied: std::collections::HashSet<u64>,
}

pub(crate) fn roster_counts(chats: &[Chat]) -> RosterCounts {
    let mut counts = RosterCounts::default();
    for c in chats {
        if c.archived {
            counts.archived += 1;
        }
        if c.locked {
            counts.locked += 1;
        }
        if !c.archived && !c.locked {
            if c.unread > 0 {
                counts.unread += 1;
            }
            if c.kind == ChatKind::Dm {
                counts.personal += 1;
            }
            if c.favorite {
                counts.favorites += 1;
            }
        }
    }
    counts
}

/// The FNV-1a fold of the set of chat JIDs carrying
/// `label_id`: the member JIDs sorted, their bytes folded
/// with a 0xff separator byte between JIDs so a different
/// split of the same concatenated bytes cannot alias to
/// the same set. A membership fold, not a count, because a
/// tag/untag swap preserves the count while changing which
/// chats the label filter must show.
fn label_members_fold(
    chat_labels: &std::collections::HashMap<String, std::collections::HashSet<String>>,
    label_id: &str,
) -> u64 {
    let mut members: Vec<&str> = chat_labels
        .iter()
        .filter(|(_, set)| set.contains(label_id))
        .map(|(jid, _)| jid.as_str())
        .collect();
    members.sort_unstable();
    let mut fp: u64 = 0xcbf2_9ce4_8422_2325;
    for (index, jid) in members.iter().enumerate() {
        if index > 0 {
            fp ^= u64::from(0xff_u8);
            fp = fp.wrapping_mul(0x0100_0000_01b3);
        }
        for b in jid.as_bytes() {
            fp ^= u64::from(*b);
            fp = fp.wrapping_mul(0x0100_0000_01b3);
        }
    }
    fp
}

/// The memo key of [`App::visible_chats`] (qc1644): the
/// live session's roster generation, the active filter, the
/// search query and the label selection. While a live
/// session stands, its roster generation is the roster's
/// version: every mutation path bumps it when a field
/// `filter_chats` reads changes on any row (title, preview,
/// preview sender, unread, archived, locked, pinned,
/// favorite, kind) or when a chat's activity timestamp
/// moves the list order, so a matching key proves the
/// filtered ordering is still the answer and neither the
/// text fold the old memo keyed on nor the filter itself
/// runs again. The label selection rides in the key as the
/// label ID plus a fold of its member set, so any
/// membership change re-runs the filter even when no
/// roster generation bump accompanies it. In demo mode
/// there is no session and no generation, so the key falls
/// back to the roster fingerprint fold (`demo_fingerprint`),
/// which is sound there because demo rosters are small and
/// mutate outside any session bookkeeping.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct VisibleChatsKey {
    roster_generation: Option<u64>,
    demo_fingerprint: u64,
    filter: u8,
    search: String,
    label: Option<String>,
    label_members: u64,
    chats_len: usize,
}

/// The chat list's filter memo: the key the visible ordering
/// was computed under, and the ordering itself as indices
/// into `App::chats`. Indices, not references, so the memo
/// can live inside the app it indexes.
#[derive(Clone)]
pub(crate) struct FilterMemo {
    key: VisibleChatsKey,
    pub(crate) indices: Vec<usize>,
}

impl App {
    /// The visible chats for the current filter and query, as
    /// indices into `chats`, memoized on [`VisibleChatsKey`]
    /// (qc1644, replacing the P2-68 fingerprint memo): on a
    /// key hit the cached ordering is returned by reference,
    /// with no fold over the roster's text and no fresh Vec;
    /// the full filter (with its per-chat lowercase
    /// allocations) runs only when the key moves. The label
    /// selection rides in the key as the filtered label's ID
    /// plus a fold of its member set: any membership change
    /// re-runs the filter, definition-only changes do not need
    /// to.
    pub(crate) fn visible_chats(&self) -> std::cell::Ref<'_, Vec<usize>> {
        let key = self.visible_chats_key();
        if self.filter_memo.borrow().as_ref().is_some_and(|memo| memo.key == key) {
            return std::cell::Ref::map(self.filter_memo.borrow(), |memo| {
                &memo.as_ref().expect("the memo was just checked").indices
            });
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
        *self.filter_memo.borrow_mut() = Some(FilterMemo { key, indices });
        std::cell::Ref::map(self.filter_memo.borrow(), |memo| {
            &memo.as_ref().expect("the memo was just stored").indices
        })
    }

    /// The current [`VisibleChatsKey`]: the roster generation
    /// in live mode, the fingerprint fold in demo mode (see
    /// the key's doc comment for why the two modes differ).
    fn visible_chats_key(&self) -> VisibleChatsKey {
        let label_members = self
            .business
            .label_filter
            .as_ref()
            .map_or(0, |label_id| label_members_fold(&self.business.chat_labels, label_id));
        let (roster_generation, demo_fingerprint) = match &self.live {
            Some(session) => (Some(session.roster_generation), 0),
            None => (None, self.roster_fingerprint()),
        };
        VisibleChatsKey {
            roster_generation,
            demo_fingerprint,
            filter: self.filter as u8,
            search: self.search.clone(),
            label: self.business.label_filter.clone(),
            label_members,
            chats_len: self.chats.len(),
        }
    }

    /// The P2-68 roster fingerprint: an FNV fold over
    /// everything the filter reads (ids, titles, previews,
    /// senders, unread, kind, the pin/archive/favorite/lock
    /// state `ChatFilter::matches` decides on, timestamps).
    /// Byte folding is O(text) with no allocation. Demo mode
    /// only: live mode keys on the roster generation instead.
    fn roster_fingerprint(&self) -> u64 {
        let mut fp: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |bytes: &[u8]| {
            for b in bytes {
                fp ^= u64::from(*b);
                fp = fp.wrapping_mul(0x0100_0000_01b3);
            }
        };
        mix(self.search.as_bytes());
        mix(&[self.filter as u8]);
        if let Some(label_id) = &self.business.label_filter {
            mix(label_id.as_bytes());
            mix(&label_members_fold(&self.business.chat_labels, label_id).to_le_bytes());
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
            mix(&[
                c.kind as u8,
                u8::from(c.pinned),
                u8::from(c.archived),
                u8::from(c.favorite),
                u8::from(c.locked),
            ]);
        }
        fp
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
                        // The percentage is honest or absent: it
                        // shows only a number the library actually
                        // reported for this episode (max-clamped in
                        // the session, never extrapolated), and
                        // phases that report no number stay
                        // indeterminate. The repaint tick keeps the
                        // quiet-window fallback in poll_backend
                        // timely while the row is up. While the
                        // window is not visible the tick stops
                        // (qc1636 Step 7): nobody can see the row,
                        // and an occluded window must not keep
                        // waking the frame loop for it.
                        if window_visible(ui.ctx()) {
                            ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
                        }
                        let label = if session.sync_finishing {
                            // The finishing phase reports no
                            // number of its own: the last history
                            // percent would sit frozen on the row
                            // and read as a stall, so the row
                            // names the phase instead (qc1644).
                            crate::i18n::t("sync.finishing")
                        } else {
                            match session.sync_percent {
                                Some(percent) => crate::i18n::tf(
                                    "chats.syncing_history",
                                    "percent",
                                    &percent.to_string(),
                                ),
                                None => crate::i18n::t("chats.syncing"),
                            }
                        };
                        crate::widgets::loading_row(ui, &label, 14.0, t.text_secondary);
                        ui.add_space(SP_2);
                    } else if session
                        .sync_completed_at
                        .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(1500))
                    {
                        // A real completion just landed: the row
                        // holds its completed state (the only place
                        // 100 ever shows) briefly, then clears. The
                        // repaint tick drives the clear, under the
                        // same visibility gate as the syncing tick
                        // above: hidden, the row simply clears on
                        // the next visible frame instead.
                        if window_visible(ui.ctx()) {
                            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
                        }
                        ui.label(
                            RichText::new(crate::i18n::tf("chats.sync_complete", "percent", "100"))
                                .size(12.0)
                                .color(rgb(t.text_secondary)),
                        );
                        ui.add_space(SP_2);
                    }
                }
                // One pass over the roster for every count
                // the header, the entry rows and the chips
                // below paint (see `roster_counts`).
                let counts = roster_counts(&self.chats);
                // The filter chips live on ONE row that scrolls
                // horizontally when the panel is too narrow for
                // them (the owner's ruling: the row must never
                // wrap; the v1.6.24 wrapping flow row and the
                // clipped strip before it are both superseded).
                // A chip resting half-visible at the row's edge
                // is the scroll affordance; every chip is
                // reachable by scrolling. Only chat-intrinsic
                // filters are chips: Groups and Channels are
                // separate destinations now (their rail views
                // preset the same filter), so they no longer
                // appear here. Filtered destinations show no
                // chip row at all: the destination itself is
                // the filter, and its header above already
                // says which one.
                if !filtered {
                    // A selection pointing at a removed chip
                    // (a Groups or Channels preset left in
                    // memory) falls back to All instead of
                    // stranding the list on a filter no chip
                    // shows as selected.
                    if matches!(self.filter, ChatFilter::Groups | ChatFilter::Channels) {
                        self.filter = ChatFilter::All;
                    }
                    // Drag scrolling is hand-driven: egui's own
                    // drag-to-scroll never engages here because
                    // a press on a chip is claimed by the chip,
                    // so the row tracks the gesture itself. A
                    // press that starts on the row and travels
                    // more than 6 px sideways scrolls instead of
                    // selecting, and the release that ends it
                    // does not fire the chip under it. The offset
                    // round-trips through the area's state so
                    // the wheel path below keeps working on the
                    // same value.
                    let drag_id = ui.scope_id().with("chat_filter_chips_drag");
                    let (mut offset_x, mut drag_moved, mut drag_origin) = ui.ctx().data_mut(|d| {
                        (
                            d.get_temp::<f32>(drag_id).unwrap_or(0.0),
                            d.get_temp::<bool>(drag_id.with("moved")).unwrap_or(false),
                            d.get_temp::<Option<egui::Pos2>>(drag_id.with("origin"))
                                .unwrap_or(None),
                        )
                    });
                    let row_rect = egui::Rect::from_min_size(
                        ui.cursor().min,
                        egui::Vec2::new(ui.available_width(), 28.0),
                    );
                    let (primary_down, pointer_delta, pointer_pos) = ui.ctx().input(|i| {
                        (i.pointer.primary_down(), i.pointer.delta(), i.pointer.latest_pos())
                    });
                    // The gesture's origin is tracked here, not
                    // read from the pointer state: on some
                    // backends the recorded press origin is the
                    // window corner, which silently killed the
                    // first version of this tracking.
                    if primary_down {
                        if drag_origin.is_none() {
                            drag_origin = pointer_pos;
                        }
                        if let (Some(origin), Some(pos)) = (drag_origin, pointer_pos) {
                            if row_rect.contains(origin) {
                                if (pos.x - origin.x).abs() > 6.0 {
                                    drag_moved = true;
                                }
                                if drag_moved {
                                    offset_x = (offset_x - pointer_delta.x).max(0.0);
                                }
                            }
                        }
                    } else {
                        drag_origin = None;
                    }
                    let suppress_click = drag_moved;
                    let output = egui::ScrollArea::horizontal()
                        .id_salt("chat_filter_chips")
                        .scroll_source(egui::scroll_area::ScrollSource {
                            drag: egui::scroll_area::DragScroll::Never,
                            ..Default::default()
                        })
                        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                        .max_height(28.0)
                        .scroll_offset(egui::Vec2::new(offset_x, 0.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                for f in [
                                    ChatFilter::All,
                                    ChatFilter::Unread,
                                    ChatFilter::Private,
                                    ChatFilter::Favorites,
                                ]
                                .into_iter()
                                .chain((counts.archived > 0).then_some(ChatFilter::Archived))
                                {
                                    let selected = self.filter == f;
                                    // Filter chips carry their counts, the
                                    // way mainstream chat clients show them
                                    // (Unread 6, Personal 4, and so on).
                                    let count = match f {
                                        // Locked, Groups and Channels are
                                        // never chips (the locked folder's
                                        // entry row is its way in; Groups
                                        // and Channels are rail
                                        // destinations); the arm exists
                                        // for exhaustiveness. The counted
                                        // filters exclude locked chats
                                        // like the lists they label do
                                        // (that rule lives in
                                        // `roster_counts`).
                                        ChatFilter::All
                                        | ChatFilter::Archived
                                        | ChatFilter::Locked
                                        | ChatFilter::Groups
                                        | ChatFilter::Channels => 0,
                                        ChatFilter::Unread => counts.unread,
                                        ChatFilter::Private => counts.personal,
                                        ChatFilter::Favorites => counts.favorites,
                                    };
                                    let name = crate::i18n::t(filter_label_key(f));
                                    let label = if count > 0 {
                                        format!(
                                            "{name} {}",
                                            core::filter::format_count(count as u64)
                                        )
                                    } else {
                                        name
                                    };
                                    let resp = chip(ui, &label, selected, t);
                                    if resp.clicked() && !suppress_click {
                                        self.filter = f;
                                    }
                                }
                            });
                        });
                    ui.ctx().data_mut(|d| {
                        d.insert_temp(drag_id, output.state.clamped_offset().x);
                        d.insert_temp(drag_id.with("origin"), drag_origin);
                        if !primary_down {
                            d.insert_temp(drag_id.with("moved"), false);
                        } else {
                            d.insert_temp(drag_id.with("moved"), drag_moved);
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
                                    .size(12.0)
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
                    let archived_count = counts.archived;
                    if archived_count > 0 {
                        ui.add_space(SP_1);
                        let resp = crate::widgets::entry_row(
                            ui,
                            t,
                            "portrait_archived_row",
                            crate::icons::Icon::Archive,
                            &crate::i18n::t("nav.archived"),
                            &archived_count.to_string(),
                        );
                        if resp.clicked() {
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
                    let locked_count = counts.locked;
                    if locked_count > 0 || self.lock_code.is_some() {
                        ui.add_space(SP_1);
                        let resp = crate::widgets::entry_row(
                            ui,
                            t,
                            "locked_entry_row",
                            crate::icons::Icon::Lock,
                            &crate::i18n::t("nav.locked"),
                            &locked_count.to_string(),
                        );
                        if resp.clicked() {
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
                    let resp = crate::widgets::entry_row(
                        ui,
                        t,
                        "labels_entry_row",
                        crate::icons::Icon::Label,
                        &crate::i18n::t("nav.labels"),
                        &self.business.labels.len().to_string(),
                    );
                    if resp.clicked() {
                        self.screen = crate::state::Screen::Labels;
                    }
                }
                // Demo portraits are computed only where demo
                // fixtures may stand in as content (qc1644): in
                // a real live session the map is empty by
                // construction, and building it cloned every DM
                // title on every frame for nothing. Live rows
                // wear their real profile pictures instead,
                // ensured per painted row in the row closure
                // below, where the landed textures overlay this
                // map.
                let mut photos = if self.demo_fixtures_allowed() {
                    self.demo_photos(ui.ctx())
                } else {
                    std::collections::HashMap::new()
                };
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
                // for the DM roster's verified names. The work
                // is generation-driven (qc1644): while the
                // session's roster generation stands, the memo
                // above is still the answer and both the JID
                // rebuild and the badge scan are skipped.
                let badge_id = ui.scope_id().with("verified_badge_memo");
                let badge_key = self.live.as_ref().map(|s| {
                    (s.roster_generation, s.verified_names.len(), s.verified_levels_fold())
                });
                let mut memo = ui
                    .ctx()
                    .data_mut(|d| d.get_temp::<VerifiedBadgeMemo>(badge_id).unwrap_or_default());
                if memo.key != badge_key {
                    memo.key = badge_key;
                    memo.applied.clear();
                    memo.dm_jids = if self.live.is_some() {
                        self.chats
                            .iter()
                            .filter(|c| c.kind == ChatKind::Dm)
                            .filter_map(|c| {
                                self.live.as_ref().and_then(|s| s.jids.get(&c.id)).cloned()
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    self.apply_verified_badges_into(&mut memo.applied);
                    for jid in &memo.dm_jids {
                        self.ensure_verified_name(jid);
                    }
                }
                ui.ctx().data_mut(|d| d.insert_temp(badge_id, memo));
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
                } else if visible.is_empty() {
                    // No rows: collapse the area to nothing, so
                    // the empty label drawn after it lands at the
                    // top of the panel. At full height the label
                    // was pushed under the empty area and only
                    // its top sliver stayed visible at the
                    // window's bottom edge (archived and filtered
                    // lists with no matches).
                    scroll = scroll.auto_shrink([false, true]).max_height(0.0);
                }
                let no_visible = visible.is_empty();
                // Row indices into the roster, copied out
                // before the row closure so the closure can
                // hold the app mutably (its avatar pass ensures
                // pictures) while it paints: the memo's own
                // indices, cloned once, instead of a vector of
                // chat references that would keep the roster
                // borrowed for the whole call.
                let visible_idx: Vec<usize> = visible.iter().copied().collect();
                drop(visible);
                crate::widgets::show_scroll_rows(
                    ui,
                    "chat_rows",
                    scroll,
                    64.0,
                    visible_idx.len(),
                    |ui, range| {
                        // The avatar pass runs for exactly the
                        // rows about to be painted (qc1644):
                        // ensuring the whole roster decoded
                        // pictures for chats far off screen on
                        // every frame, and a texture miss
                        // decodes on this thread. Painted rows
                        // still resolve their picture before
                        // they draw, so a row scrolled into view
                        // wears its avatar on the same frame as
                        // before; the pipeline dedups and caches,
                        // so a row already resolved costs a map
                        // lookup.
                        if self.live.is_some() {
                            for &row in &visible_idx[range.clone()] {
                                let Some(chat) = self.chats.get(row) else { continue };
                                let chat_id = chat.id;
                                let jid =
                                    self.live.as_ref().and_then(|s| s.jids.get(&chat_id)).cloned();
                                if let Some(jid) = jid {
                                    self.ensure_avatar(ui.ctx(), &jid);
                                    if let Some(tex) = self.live_avatar(&jid) {
                                        photos.insert(chat_id, tex.id());
                                    }
                                }
                            }
                        }
                        for idx in range {
                            let Some(chat) = self.chats.get(visible_idx[idx]) else {
                                continue;
                            };
                            let state = self.row_state_for(chat);
                            let tags = self.chat_tags(chat.id);
                            let row_title = self.display_chat_title(chat);
                            let resp = chat_row(
                                ui,
                                chat,
                                &row_title,
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
                                // Pin leads the menu, where the
                                // reference client places it: the row
                                // state existed (the pin indicator,
                                // the pinned-first ordering) but this
                                // menu, the row's own action surface,
                                // could not change it.
                                let pin_label = if chat.pinned {
                                    crate::i18n::t("conversation.unpin")
                                } else {
                                    crate::i18n::t("conversation.pin")
                                };
                                if crate::widgets::menu_item_icon(
                                    ui,
                                    t,
                                    Some(crate::icons::Icon::Pin),
                                    &pin_label,
                                    None,
                                )
                                .clicked()
                                {
                                    row_action
                                        .set(Some((chat.id, RowPick::SetPinned(!chat.pinned))));
                                    ui.close();
                                }
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
                                // Mute sits with the menu's state
                                // group, right after Archive: like
                                // pin, the muted state was visible on
                                // the row (the bell, the grey badge)
                                // but unreachable from its own menu.
                                // The toggle is the indefinite form;
                                // timed durations stay in the header
                                // menu, where they are chosen.
                                let mute_label = if chat.muted {
                                    crate::i18n::t("conversation.unmute_notifications")
                                } else {
                                    crate::i18n::t("conversation.mute_notifications")
                                };
                                let mute_icon = if chat.muted {
                                    crate::icons::Icon::Bell
                                } else {
                                    crate::icons::Icon::BellOff
                                };
                                if crate::widgets::menu_item_icon(
                                    ui,
                                    t,
                                    Some(mute_icon),
                                    &mute_label,
                                    None,
                                )
                                .clicked()
                                {
                                    row_action.set(Some((chat.id, RowPick::SetMuted(!chat.muted))));
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
                                    row_action
                                        .set(Some((chat.id, RowPick::SetLocked(!chat.locked))));
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
                    },
                );
                if let Some(id) = clicked {
                    // Picking a chat leaves the community home: the
                    // conversation takes the content area back, and
                    // the chat was not entered through a community,
                    // so the header's way back to one goes away.
                    self.open_community = None;
                    self.community_home_from_overview = false;
                    self.community_origin = None;
                    // In single-pane mode the click must present
                    // the conversation (qc1654 Claim J): this
                    // application keeps its own switch sequence
                    // instead of routing through select_chat_now
                    // because two of its effects differ from that
                    // path (the demo branch below clears the
                    // listed chat's unread badge, which
                    // select_chat_now's demo branch does not, and
                    // its unread-at-open capture predates the
                    // detached-slot fallback), so the pane flag is
                    // set here directly.
                    self.conversation_open = true;
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
                        RowPick::SetPinned(target_pinned) => {
                            if let Some(c) = self.chats.iter().find(|c| c.id == id) {
                                let (archived, muted) = (c.archived, c.muted);
                                self.apply_chat_action(id, archived, target_pinned, muted);
                            }
                        }
                        RowPick::SetMuted(target_muted) => {
                            if let Some(c) = self.chats.iter().find(|c| c.id == id) {
                                let (archived, pinned) = (c.archived, c.pinned);
                                self.apply_chat_action(id, archived, pinned, target_muted);
                            }
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
                    .font(crate::theme::semibold(19.0))
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
                crate::anim::stock_popup_enter(
                    ui.ctx(),
                    egui::Popup::default_response_id(&kebab),
                    kebab.rect.center(),
                );
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

/// The account address inside a JID, without any device
/// part: `user:device@server` becomes `user@server`, and a
/// JID carrying no device part is returned unchanged. The
/// device part names one linked device of an account, not
/// the account itself, so two forms differing only there
/// address the same account.
fn strip_device_part(jid: &str) -> std::borrow::Cow<'_, str> {
    let Some((user, server)) = jid.split_once('@') else {
        return std::borrow::Cow::Borrowed(jid);
    };
    let Some((account, _device)) = user.split_once(':') else {
        return std::borrow::Cow::Borrowed(jid);
    };
    std::borrow::Cow::Owned(format!("{account}@{server}"))
}

/// Whether `chat` is the account's own self chat (qc1644
/// item 19): the direct chat addressed to the account itself.
/// Identification is by account address wherever the session
/// knows the account's own addresses: the backend reports
/// the own phone-number JID and LID on its own-profile
/// read-back, so a chat addressed under either form matches,
/// and a contact saved under the account's own name never
/// does. Matching is by account address and tolerates a
/// device part on either side, because the own profile
/// reports the LID without one while a roster self chat can
/// be keyed with one. Only when no own address is known at
/// all (a read-back has not landed yet, or it carried none)
/// does the match fall back to the one signal the roster
/// itself carries: the self chat is the direct chat whose
/// resolved title is the account's own name (the name the
/// backend resolved for that chat is the account's push
/// name, exactly as it is for the account surfaces). With
/// no own address and no own name known, nothing matches:
/// the rule never guesses.
pub(crate) fn is_self_chat(
    chat: &Chat,
    own_jids: &[&str],
    chat_jid: Option<&str>,
    own_name: &str,
) -> bool {
    if chat.kind != ChatKind::Dm {
        return false;
    }
    if !own_jids.is_empty() {
        return chat_jid.is_some_and(|jid| {
            own_jids.contains(&jid)
                || own_jids.iter().any(|own| strip_device_part(own) == strip_device_part(jid))
        });
    }
    let own_name = own_name.trim();
    !own_name.is_empty() && chat.title.trim() == own_name
}

/// The self chat's display title: the account's own name plus
/// the localized suffix, or, when no own name is known, the
/// chat's current title (the own number) plus the suffix.
/// A pure formatting rule over the stored title: the chat's
/// own data is never renamed by it.
pub(crate) fn self_chat_title(title: &str, own_name: &str, suffix: &str) -> String {
    let own_name = own_name.trim();
    let base = if own_name.is_empty() { title } else { own_name };
    format!("{base} {suffix}")
}

impl App {
    /// The account's own display name, from the profile the
    /// app already holds: the live account read-back first
    /// (the truth the account menu and the profile editor
    /// show), then the editor's persisted value, which is the
    /// settings cache of that same read-back in live mode and
    /// the demo account's name in demo mode. No fetch is made
    /// for display purposes. Empty when nothing is known.
    pub(crate) fn own_display_name(&self) -> String {
        if let Some(account) = &self.my_profile.account {
            if !account.name.trim().is_empty() {
                return account.name.trim().to_owned();
            }
        }
        self.my_profile.name.trim().to_owned()
    }

    /// Every address form the account itself can be addressed
    /// under: the phone-number JID first, then the LID, both
    /// from the adopted own-profile read-back. A self chat can
    /// sit under either form, so [`is_self_chat`] matches
    /// against the whole set. Before any read-back lands, a
    /// test-mode session uses the fake backend's published
    /// constants; a real session knows nothing yet and the set
    /// is empty.
    pub(crate) fn own_jids(&self) -> Vec<String> {
        let mut jids = Vec::new();
        if let Some(account) = &self.my_profile.account {
            for candidate in [&account.jid, &account.lid] {
                let candidate = candidate.trim();
                if !candidate.is_empty() {
                    jids.push(candidate.to_owned());
                }
            }
        }
        if jids.is_empty() {
            if let Some(session) = &self.live {
                if session.fake {
                    jids.push(backend::fake::FAKE_OWN_JID.to_owned());
                    jids.push(backend::fake::FAKE_OWN_LID.to_owned());
                }
            }
        }
        jids
    }

    /// The title a surface should show for `chat`: the self
    /// chat's "<own name> (You)" form for the account's own
    /// chat, the stored title for every other chat. This is
    /// the one resolution point the chat list row, the
    /// conversation header, the global search results and the
    /// forward picker all share, so the self chat reads the
    /// same everywhere and no surface renames the stored data.
    pub(crate) fn display_chat_title(&self, chat: &Chat) -> String {
        let own_name = self.own_display_name();
        let own_jids = self.own_jids();
        let own_jids: Vec<&str> = own_jids.iter().map(String::as_str).collect();
        let chat_jid = self.live.as_ref().and_then(|s| s.jids.get(&chat.id)).map(String::as_str);
        if is_self_chat(chat, &own_jids, chat_jid, &own_name) {
            self_chat_title(&chat.title, &own_name, &crate::i18n::t("chat.you_suffix"))
        } else {
            chat.title.clone()
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn chat_row(
    ui: &mut egui::Ui,
    chat: &Chat,
    title: &str,
    selected: bool,
    blocked: bool,
    t: Tokens,
    row_state: Option<RowState>,
    avatar_icon: Option<crate::icons::Icon>,
    photo: Option<egui::TextureId>,
    tags: &[crate::labels::ChatTag],
) -> egui::Response {
    // The row itself is the shared list-row component: it
    // owns the allocation and the one opaque step wash
    // (hover the hover step, selected the pressed step), so
    // this row can no longer drift from the other panels'
    // rows. The content below paints into its slots.
    let row = crate::widgets::list_row_std(ui, t, ("chat_row", chat.id), selected, Sense::click());
    let rect = row.rect;
    let response = &row.response;
    let painter = ui.painter();
    let center = row.avatar_center();
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
    // The title text is the caller's resolved display title
    // (the self chat's "(You)" form in the chat list); the
    // avatar above still paints from the stored chat, so a
    // display rule never changes a row's avatar.
    let title = core::bidi::display(&crate::widgets::ellipsize(
        ui,
        title,
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
        let count_w = crate::widgets::badge_size(ui, &count_label, 1.0).x;
        right_limit = right_limit.min(rect.right() - 6.0 - count_w - 6.0);
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
                    // The advance is the bubble's own tick span
                    // plus the trailing gap, so the preview text
                    // starts clear of the glyphs the ticks paint.
                    x += crate::bubble::tick_span(ticks) + 5.0;
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
            x = crate::labels::paint_label_tag(painter, ui, x, line_y, tag, t) + 6.0;
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
    // The unread badge's width, kept for the pin indicator
    // below, which steps left past it.
    let mut unread_w = 0.0;
    if chat.unread > 0 {
        // The unread badge, painted by the shared badge
        // component, with a short pop (scale to about 1.3 and
        // back over 300 ms) on the frame the count changes, so
        // a new message arriving in a closed chat visibly
        // lands. Muted chats wear the component's grey variant:
        // the count is still there, it just does not shout in
        // the accent.
        let scale = crate::anim::pop_scale(ui, ("unread_badge", chat.id), chat.unread);
        let label = core::filter::format_count(u64::from(chat.unread));
        let size = crate::widgets::badge_size(ui, &label, scale);
        unread_w = size.x;
        let center = Pos2::new(rect.right() - 6.0 - size.x / 2.0, line_y);
        crate::widgets::paint_badge(painter, center, &label, size, t, chat.muted, scale);
    }
    if chat.mentions > 0 {
        // The mention badge beside it, the badge component's
        // circle form at the same diameter.
        let mcenter = Pos2::new(rect.right() - 40.0, line_y);
        crate::widgets::paint_mention_badge(painter, mcenter, t);
    }
    if chat.pinned {
        // The pin indicator: a small pin in the faint token at
        // the snippet line's right end, stepped left past
        // whichever badges the row carries. The pinned state
        // had no visible sign in the list before this: the row
        // moved to the top, but nothing on it said why.
        let mut pin_right = rect.right() - 6.0;
        if chat.unread > 0 {
            pin_right -= unread_w + 6.0;
        }
        if chat.mentions > 0 {
            pin_right -= 50.0;
        }
        crate::icons::draw(
            painter,
            crate::icons::Icon::Pin,
            Pos2::new(pin_right - 6.0, line_y),
            crate::icons::SIZE_BADGE,
            rgb(t.text_faint),
            rgb(t.bg_panel),
        );
    }
    row.response.on_hover_cursor(egui::CursorIcon::PointingHand)
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
                let chat_title = self.display_chat_title(chat);
                self.global_search.hits.push(crate::state::GlobalHit {
                    chat_id,
                    msg_id: crate::live::message_id(&hit.chat_jid, &hit.message_id),
                    chat_title,
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
                        chat_title: self.display_chat_title(chat),
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
        crate::widgets::show_scroll_area(
            ui,
            "global_search_results",
            egui::ScrollArea::vertical().auto_shrink([false, false]),
            |ui| {
                for hit in &hits {
                    // A search hit is a shared list row like
                    // every other panel row: same slots, same
                    // opaque step wash.
                    let row = crate::widgets::list_row_std(
                        ui,
                        t,
                        ("search_hit", hit.msg_id),
                        false,
                        Sense::click(),
                    );
                    let rect = row.rect;
                    let painter = ui.painter();
                    if let Some(chat) = self.chats.iter().find(|c| c.id == hit.chat_id) {
                        crate::widgets::paint_chat_avatar(
                            painter,
                            row.avatar_center(),
                            AVATAR_R,
                            chat,
                        );
                    }
                    let text_left = row.text_left();
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
                    crate::widgets::paint_row_title(
                        ui,
                        &row,
                        &hit.chat_title,
                        crate::theme::medium(15.0),
                        rgb(t.text_primary),
                        title_budget,
                    );
                    let line = match &hit.sender {
                        Some(sender) => format!("{sender}: {}", hit.snippet),
                        None => hit.snippet.clone(),
                    };
                    crate::widgets::paint_row_snippet(
                        ui,
                        &row,
                        &line,
                        FontId::proportional(12.0),
                        rgb(t.text_secondary),
                        rect.right() - 14.0,
                    );
                    if row.response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        open = Some((hit.chat_id, hit.msg_id));
                    }
                }
            },
        );
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
            } else {
                // Already the open chat: selecting its result
                // still presents the conversation in
                // single-pane mode (qc1654 Claim J).
                self.conversation_open = true;
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

#[cfg(test)]
mod counts_tests {
    use super::roster_counts;
    use core::model::{Chat, ChatKind};

    fn chat(kind: ChatKind) -> Chat {
        let mut chat = core::model::demo_announcement_chat();
        chat.kind = kind;
        chat.unread = 0;
        chat.archived = false;
        chat.locked = false;
        chat.favorite = false;
        chat
    }

    #[test]
    fn roster_counts_fold_every_count_in_one_pass() {
        // The counted chip filters exclude archived and locked
        // chats; the archived and locked counts cover every
        // chat in their state, including one that is both.
        let mut dm_unread = chat(ChatKind::Dm);
        dm_unread.unread = 2;
        let mut dm_archived = chat(ChatKind::Dm);
        dm_archived.archived = true;
        dm_archived.unread = 3;
        let mut dm_locked = chat(ChatKind::Dm);
        dm_locked.locked = true;
        dm_locked.unread = 1;
        dm_locked.favorite = true;
        let mut group_favorite = chat(ChatKind::Group);
        group_favorite.favorite = true;
        let mut group_both = chat(ChatKind::Group);
        group_both.archived = true;
        group_both.locked = true;
        let chats = vec![dm_unread, dm_archived, dm_locked, group_favorite, group_both];
        let counts = roster_counts(&chats);
        assert_eq!(counts.unread, 1, "only the plain DM counts as unread");
        assert_eq!(counts.personal, 1, "only the plain DM counts as personal");
        assert_eq!(counts.favorites, 1, "the locked favorite does not count");
        assert_eq!(counts.archived, 2);
        assert_eq!(counts.locked, 2);
        let empty = roster_counts(&[]);
        assert_eq!(
            empty.unread + empty.personal + empty.favorites + empty.archived + empty.locked,
            0
        );
    }
}

#[cfg(test)]
mod self_chat_tests {
    use super::{is_self_chat, self_chat_title};
    use core::model::{Chat, ChatKind};

    fn chat(kind: ChatKind, title: &str) -> Chat {
        let mut chat = core::model::demo_announcement_chat();
        chat.kind = kind;
        chat.title = title.to_owned();
        chat
    }

    #[test]
    fn self_chat_matches_by_jid_when_the_session_knows_it() {
        let mine = chat(ChatKind::Dm, "+10000000000");
        assert!(is_self_chat(
            &mine,
            &["10000000000@s.whatsapp.net"],
            Some("10000000000@s.whatsapp.net"),
            "",
        ));
        // A different direct chat is not the self chat, even
        // when its title happens to be the own name: with a
        // JID known, the JID alone decides.
        let other = chat(ChatKind::Dm, "Rifan");
        assert!(!is_self_chat(
            &other,
            &["10000000000@s.whatsapp.net"],
            Some("10000000007@s.whatsapp.net"),
            "Rifan",
        ));
    }

    #[test]
    fn self_chat_jid_match_wins_over_a_same_name_contact() {
        // The false positive the name fallback could not rule
        // out: two direct chats, one addressed to the account
        // itself under a title that is not the own name, and
        // one addressed to a contact saved under the exact own
        // name. With the own JID known, only the first is the
        // self chat.
        let own = &["10000000000@s.whatsapp.net"];
        let mine = chat(ChatKind::Dm, "+10000000000");
        assert!(is_self_chat(&mine, own, Some("10000000000@s.whatsapp.net"), "Rifan"));
        let namesake = chat(ChatKind::Dm, "Rifan");
        assert!(!is_self_chat(&namesake, own, Some("10000000007@s.whatsapp.net"), "Rifan"));
    }

    #[test]
    fn self_chat_matches_under_the_own_lid() {
        // A self chat addressed under the account's LID
        // instead of its phone-number JID is still the self
        // chat when the backend reported the LID.
        let own = &["10000000000@s.whatsapp.net", "10000000000@lid"];
        let mine = chat(ChatKind::Dm, "+10000000000");
        assert!(is_self_chat(&mine, own, Some("10000000000@lid"), "Rifan"));
        // Another person's LID-addressed chat is not.
        let other = chat(ChatKind::Dm, "Rifan");
        assert!(!is_self_chat(&other, own, Some("10000000007@lid"), "Rifan"));
    }

    #[test]
    fn self_chat_matches_a_device_suffixed_jid() {
        // The own profile reports the LID without a device
        // part, while the roster can key the self chat with
        // one: the account address is the same, so it matches.
        let lid_own = &["10000000000@lid"];
        let mine = chat(ChatKind::Dm, "+10000000000");
        assert!(is_self_chat(&mine, lid_own, Some("10000000000:2@lid"), "Rifan"));
        // The device-suffixed phone-number form matches too.
        let pn_own = &["10000000000@s.whatsapp.net"];
        assert!(is_self_chat(&mine, pn_own, Some("10000000000:2@s.whatsapp.net"), "Rifan"));
        // A device part on the reported own address is
        // tolerated the same way.
        let device_own = &["10000000000:2@lid"];
        assert!(is_self_chat(&mine, device_own, Some("10000000000@lid"), "Rifan"));
        // A different account stays different once the device
        // part is stripped: its user part never equals the own
        // one, suffixed or not.
        let other = chat(ChatKind::Dm, "Rifan");
        assert!(!is_self_chat(&other, lid_own, Some("10000000007:2@lid"), "Rifan"));
        assert!(!is_self_chat(&other, pn_own, Some("10000000007:2@s.whatsapp.net"), "Rifan"));
    }

    #[test]
    fn self_chat_matches_by_own_name_without_a_known_jid() {
        let mine = chat(ChatKind::Dm, "Rifan");
        assert!(is_self_chat(&mine, &[], Some("10000000000@s.whatsapp.net"), "Rifan"));
        // No own name known: the rule never fires.
        assert!(!is_self_chat(&mine, &[], None, ""));
        assert!(!is_self_chat(&mine, &[], None, "  "));
        // Another contact's chat never matches.
        let other = chat(ChatKind::Dm, "Sarah Miller");
        assert!(!is_self_chat(&other, &[], None, "Rifan"));
        // Groups and channels are never the self chat, even
        // under the own name or the own JID.
        let group = chat(ChatKind::Group, "Rifan");
        assert!(!is_self_chat(&group, &[], None, "Rifan"));
        assert!(!is_self_chat(
            &group,
            &["10000000000@s.whatsapp.net"],
            Some("10000000000@s.whatsapp.net"),
            "Rifan",
        ));
    }

    #[test]
    fn self_chat_title_prefers_the_own_name_and_falls_back_to_the_title() {
        assert_eq!(self_chat_title("+6281234567890", "Rifan", "(You)"), "Rifan (You)");
        // No own name: the current title (the own number)
        // carries the suffix instead of a raw identifier
        // standing alone.
        assert_eq!(self_chat_title("+6281234567890", "", "(You)"), "+6281234567890 (You)");
        assert_eq!(self_chat_title("+6281234567890", "  ", "(Anda)"), "+6281234567890 (Anda)");
    }
}

#[cfg(test)]
mod fingerprint_tests {
    use crate::state::{App, Args};

    /// qc1658 L-2: the demo roster fingerprint must move on
    /// every field `ChatFilter::matches` reads. The lock state
    /// was the one omission: a demo chat whose lock flipped
    /// with nothing else changing left the filter memo serving
    /// the old ordering, even though the Locked preset and the
    /// guard that hides locked chats elsewhere both read it.
    #[test]
    fn roster_fingerprint_moves_when_a_chat_locks() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        // Args parses as live by default and its fields are
        // private outside state.rs, so the demo roster is
        // seeded directly: the fingerprint reads the roster
        // and the filter state, nothing else.
        let mut app = App::new(Args::parse(), &ctx);
        app.live = None;
        app.live_mode = false;
        app.chats = core::model::demo_chats();
        assert!(!app.chats.is_empty(), "the demo roster is seeded");
        let before = app.roster_fingerprint();
        app.chats[0].locked = !app.chats[0].locked;
        assert_ne!(
            app.roster_fingerprint(),
            before,
            "flipping one chat's lock state moves the fingerprint"
        );
    }
}
