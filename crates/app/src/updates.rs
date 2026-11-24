//! Updates screen: the status surface. Its structure follows
//! the reference layout in every mode: the title, a My Status
//! row (my avatar with an add badge, the row title, and a
//! subtitle that reflects the real state), a "Recent updates"
//! section, and the feed of other people's statuses under it,
//! split into recent and viewed like the reference client.
//!
//! What is real here is dictated by how statuses move: the
//! pinned protocol library posts text, photo and video
//! statuses and revokes them (`BackendCommand::PostTextStatus`,
//! `PostMediaStatus`, `DeleteStatus`), and received statuses
//! arrive with the message flow (live, and redelivered by
//! history sync at link time) as `StatusReceived` events.
//! There is no fetch for a status and no viewer list in the
//! library, so the feed is exactly what arrived while linked
//! (plus what history sync still held), kept for this session
//! only, and a status expires out of it 24 hours after it was
//! posted. Nothing is fabricated: an account whose contacts
//! posted nothing sees the honest empty state.
//!
//! Layout: on desktop the screen is a left list panel in the
//! Starred / Calls family (see `App::list_panel`), with the
//! conversation placeholder in the right pane. In single-pane
//! modes (portrait included) the same content fills the screen.
//! The viewer is a full-screen overlay in the media viewer's
//! family, opened from a feed row or from My Status.

use crate::state::{App, Notice, PhotoFetchState, StagedStatusMedia, StatusEntry, StatusViewer};
use crate::theme::{rgb, SP_1, SP_2, SP_3, SP_4, SP_1_5};
use eframe::egui::{self, Align2, CornerRadius, FontId, Margin, Pos2, RichText, Sense, Vec2};

/// Height of the My Status row and of a feed row: avatar disc
/// plus two text lines.
const STATUS_ROW_H: f32 = 64.0;

/// A status's lifetime: it leaves the feed 24 hours after it
/// was posted.
const STATUS_LIFETIME_SECS: i64 = 24 * 3600;

/// The background a text status post carries, as an ARGB
/// word: the house green. The backend stamps the same value
/// on the wire (its own copy lives with the post path); this
/// one colors the locally applied demo post's stage.
const TEXT_STATUS_BACKGROUND_ARGB: u32 = 0xFF00_A884;

/// One feed row's display data, gathered before painting so
/// the row pass holds no borrow of the app while it clicks.
struct FeedRow {
    /// The author's JID.
    author_jid: String,
    /// The author's display name.
    name: String,
    /// The author's newest status's preview line.
    preview: String,
    /// The author's newest status's timestamp.
    latest: i64,
    /// How many statuses the author has in the feed.
    count: usize,
    /// How many of them are still unviewed.
    unviewed: usize,
    /// Index into the author's run (oldest first) the viewer
    /// opens at: the first unviewed status, or the first.
    open_index: usize,
}

impl App {
    /// The Updates screen: a left list panel with the
    /// conversation placeholder beside it in two-pane mode, the
    /// same content full-screen in single-pane modes (the
    /// portrait bottom bar's second tab lands here too).
    pub(crate) fn draw_updates(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        if self.single_pane {
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(rgb(t.bg_panel))
                        .inner_margin(Margin::symmetric(16, 14)),
                )
                .show(root, |ui| self.updates_content(ui));
            return;
        }
        let panel = self.list_panel(root);
        let panel = panel
            .frame(
                egui::Frame::new()
                    .fill(rgb(t.bg_panel))
                    .inner_margin(Margin::symmetric(crate::theme::LIST_PANEL_INSET_X, 8)),
            )
            .show(root, |ui| self.updates_content(ui));
        crate::widgets::soft_right_edge(root, panel.response.rect, t);
        self.draw_conversation_placeholder(root);
    }

    /// The Updates content, shared by the panel and the
    /// full-screen page: title, My Status row, then the feed
    /// grouped by author under Recent updates / Viewed
    /// headers, or the feed's empty state.
    fn updates_content(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens();
        let now = self.status_now();
        self.prune_statuses();
        ui.label(
            RichText::new(crate::i18n::t("nav.updates"))
                .font(crate::theme::semibold(19.0))
                .color(rgb(t.text_primary)),
        );
        ui.add_space(SP_3);
        // The My Status row: my avatar with the add badge, the
        // title, and the state subtitle. The badge zone opens
        // the composer; the rest of the row opens my statuses
        // in the viewer when I have any, and the composer when
        // I have none (the reference client's split).
        let mut open_composer = false;
        let mut open_mine = false;
        {
            let (rect, resp) = ui
                .allocate_exact_size(Vec2::new(ui.available_width(), STATUS_ROW_H), Sense::click());
            let hov = crate::anim::hover_factor(ui, "my_status_row", resp.hovered());
            if hov > 0.001 {
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::same(10),
                    crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
                );
            }
            let painter = ui.painter();
            let avatar_center = Pos2::new(rect.left() + 32.0, rect.center().y);
            crate::profile::paint_my_avatar_disc(painter, avatar_center, 24.0, &self.my_profile, t);
            // The ring: my newest status is unexpired, so the
            // avatar wears the status ring like the reference
            // client's.
            if !self.updates.mine.is_empty() {
                painter.circle_stroke(avatar_center, 26.5, egui::Stroke::new(2.0, rgb(t.accent)));
            }
            // The add badge: an accent disc on the avatar's
            // lower right, ringed in the panel color so it reads
            // as sitting on the avatar, not beside it.
            let badge_center = avatar_center + Vec2::new(17.0, 17.0);
            painter.circle_filled(badge_center, 10.0, rgb(t.bg_panel));
            painter.circle_filled(badge_center, 8.5, rgb(t.accent));
            crate::icons::draw(
                painter,
                crate::icons::Icon::Plus,
                badge_center,
                crate::icons::SIZE_BADGE,
                rgb(t.on_accent()),
                rgb(t.accent),
            );
            let text_left = rect.left() + 68.0;
            painter.text(
                Pos2::new(text_left, rect.top() + 22.0),
                Align2::LEFT_CENTER,
                crate::i18n::t("updates.my_status"),
                crate::theme::medium(15.0),
                rgb(t.text_primary),
            );
            let subtitle = crate::widgets::ellipsize(
                ui,
                &self.my_status_subtitle(),
                FontId::proportional(12.0),
                (rect.right() - 8.0 - text_left).max(0.0),
            );
            painter.text(
                Pos2::new(text_left, rect.top() + 44.0),
                Align2::LEFT_CENTER,
                subtitle,
                FontId::proportional(12.0),
                rgb(t.text_secondary),
            );
            let pointer_pos = resp.interact_pointer_pos();
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                if let Some(pos) = pointer_pos {
                    // The badge zone always opens the composer;
                    // the rest of the row opens the viewer, or
                    // the composer when there is nothing of mine
                    // to view yet.
                    if pos.x < rect.left() + 64.0 || self.updates.mine.is_empty() {
                        open_composer = true;
                    } else {
                        open_mine = true;
                    }
                }
            }
        }
        if open_composer {
            self.updates.composer_open = true;
            self.updates.error = None;
        }
        if open_mine {
            self.open_status_viewer(None, true, 0);
        }
        ui.add_space(SP_4);
        // The feed, grouped by author. Recent holds the
        // authors with anything unviewed; Viewed the rest.
        let rows = self.feed_rows();
        if rows.is_empty() {
            ui.label(
                RichText::new(crate::i18n::t("updates.recent"))
                    .font(crate::theme::medium(12.0))
                    .color(rgb(t.text_secondary)),
            );
            crate::widgets::empty_state(ui, t, &crate::i18n::t("updates.empty"));
            return;
        }
        let mut header_shown = false;
        let mut open: Option<(String, usize)> = None;
        for row in rows.iter().filter(|r| r.unviewed > 0) {
            if !header_shown {
                ui.label(
                    RichText::new(crate::i18n::t("updates.recent"))
                        .font(crate::theme::medium(12.0))
                        .color(rgb(t.text_secondary)),
                );
                header_shown = true;
            }
            if self.feed_row(ui, row, now) {
                open = Some((row.author_jid.clone(), row.open_index));
            }
        }
        let mut viewed_header_shown = false;
        for row in rows.iter().filter(|r| r.unviewed == 0) {
            if !viewed_header_shown {
                ui.add_space(SP_3);
                ui.label(
                    RichText::new(crate::i18n::t("updates.viewed"))
                        .font(crate::theme::medium(12.0))
                        .color(rgb(t.text_secondary)),
                );
                viewed_header_shown = true;
            }
            if self.feed_row(ui, row, now) {
                open = Some((row.author_jid.clone(), row.open_index));
            }
        }
        if let Some((author, index)) = open {
            self.open_status_viewer(Some(author), false, index);
        }
    }

    /// One feed row: the author's avatar (ringed while
    /// anything of theirs is unviewed), name, and the newest
    /// status's preview with its age. Returns true on click.
    fn feed_row(&mut self, ui: &mut egui::Ui, row: &FeedRow, now: i64) -> bool {
        let t = self.tokens();
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), STATUS_ROW_H), Sense::click());
        let hov =
            crate::anim::hover_factor(ui, format!("status_row_{}", row.author_jid), resp.hovered());
        if hov > 0.001 {
            ui.painter().rect_filled(
                rect,
                CornerRadius::same(10),
                crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
            );
        }
        let painter = ui.painter();
        let avatar_center = Pos2::new(rect.left() + 32.0, rect.center().y);
        if row.unviewed > 0 {
            painter.circle_stroke(avatar_center, 26.5, egui::Stroke::new(2.0, rgb(t.accent)));
        }
        crate::widgets::paint_initials_avatar(
            painter,
            avatar_center,
            24.0,
            &row.name,
            (crate::live::jid_id(&row.author_jid) & 0xff) as u8,
        );
        let text_left = rect.left() + 68.0;
        painter.text(
            Pos2::new(text_left, rect.top() + 22.0),
            Align2::LEFT_CENTER,
            crate::widgets::ellipsize(
                ui,
                &row.name,
                FontId::proportional(15.0),
                (rect.right() - 8.0 - text_left).max(0.0),
            ),
            crate::theme::medium(15.0),
            rgb(t.text_primary),
        );
        let age = status_age_label(now, row.latest);
        let subtitle = if row.count > 1 {
            format!("{} · {age}", crate::i18n::tf("updates.count", "count", &row.count.to_string()))
        } else {
            age
        };
        let preview = crate::widgets::ellipsize(
            ui,
            &row.preview,
            FontId::proportional(12.0),
            (rect.right() - 8.0 - text_left).max(0.0),
        );
        painter.text(
            Pos2::new(text_left, rect.top() + 44.0),
            Align2::LEFT_CENTER,
            if preview.is_empty() { subtitle.clone() } else { format!("{preview} · {subtitle}") },
            FontId::proportional(12.0),
            rgb(t.text_secondary),
        );
        resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
    }

    /// The feed grouped for display: one row per author,
    /// authors ordered by their newest status, newest first.
    fn feed_rows(&self) -> Vec<FeedRow> {
        let mut authors: Vec<FeedRow> = Vec::new();
        for entry in &self.updates.feed {
            let status = &entry.status;
            let pos = authors.iter().position(|r| r.author_jid == status.author_jid);
            let idx = match pos {
                Some(i) => i,
                None => {
                    authors.push(FeedRow {
                        author_jid: status.author_jid.clone(),
                        name: status.author_name.clone().unwrap_or_else(|| {
                            status
                                .author_jid
                                .split('@')
                                .next()
                                .unwrap_or(&status.author_jid)
                                .to_owned()
                        }),
                        preview: String::new(),
                        latest: 0,
                        count: 0,
                        unviewed: 0,
                        open_index: 0,
                    });
                    authors.len() - 1
                }
            };
            let row = &mut authors[idx];
            row.count += 1;
            if !self.updates.viewed.contains(&status.id) {
                row.unviewed += 1;
            }
            if status.timestamp >= row.latest {
                row.latest = status.timestamp;
                row.preview = status_preview(status);
            }
        }
        // The viewer's open index is into the author's run
        // oldest first: the first unviewed position.
        for row in &mut authors {
            let run: Vec<&StatusEntry> = self
                .updates
                .feed
                .iter()
                .filter(|e| e.status.author_jid == row.author_jid)
                .collect();
            let mut sorted: Vec<&StatusEntry> = run;
            sorted.sort_by_key(|e| e.status.timestamp);
            row.open_index = sorted
                .iter()
                .position(|e| !self.updates.viewed.contains(&e.status.id))
                .unwrap_or(0);
        }
        authors.sort_by_key(|r| std::cmp::Reverse(r.latest));
        authors
    }

    /// The My Status row's subtitle, the row's real state in
    /// one line: the add prompt before anything is posted, the
    /// newest post afterwards (its text, or its caption, or
    /// the media kind's word).
    pub(crate) fn my_status_subtitle(&self) -> String {
        match self.updates.mine.last() {
            Some(entry) => {
                let status = &entry.status;
                if !status.body.trim().is_empty() {
                    status.body.clone()
                } else {
                    status_preview(status)
                }
            }
            None => crate::i18n::t("updates.add_prompt"),
        }
    }

    /// The status timeline's "now": the newest status
    /// timestamp the session has seen, plus the real time
    /// elapsed since it was seen (the clock anchor in
    /// `UpdatesState`). In a live session the anchor tracks
    /// the wall clock through arrivals; in test mode the
    /// scripted timeline ages in its own era.
    pub(crate) fn status_now(&mut self) -> i64 {
        let newest = self
            .updates
            .feed
            .iter()
            .chain(self.updates.mine.iter())
            .map(|e| e.status.timestamp)
            .max()
            .unwrap_or(0);
        let instant = std::time::Instant::now();
        match self.updates.clock_anchor {
            Some((anchor_ts, anchor_instant)) if newest <= anchor_ts => {
                anchor_ts + anchor_instant.elapsed().as_secs() as i64
            }
            _ => {
                if newest > 0 {
                    self.updates.clock_anchor = Some((newest, instant));
                }
                newest
            }
        }
    }

    /// Drops statuses past their 24-hour lifetime, read on
    /// the status timeline's own clock.
    pub(crate) fn prune_statuses(&mut self) {
        let horizon = self.status_now() - STATUS_LIFETIME_SECS;
        if horizon <= 0 {
            return;
        }
        self.updates.feed.retain(|e| e.status.timestamp > horizon);
        self.updates.mine.retain(|e| e.status.timestamp > horizon);
    }

    /// Opens the viewer on one author's run. `author` is the
    /// feed author's JID; `mine` opens my own run instead.
    pub(crate) fn open_status_viewer(&mut self, author: Option<String>, mine: bool, index: usize) {
        let author_jid = if mine {
            self.updates.mine.first().map(|e| e.status.author_jid.clone()).unwrap_or_default()
        } else {
            author.unwrap_or_default()
        };
        self.updates.viewer = Some(StatusViewer {
            author_jid,
            mine,
            index,
            reply: String::new(),
            error: None,
            deleting: false,
            dispatched: None,
        });
    }

    /// The author's run the viewer walks, oldest first.
    pub(crate) fn status_run(&self, viewer: &StatusViewer) -> Vec<StatusEntry> {
        let source: &Vec<StatusEntry> =
            if viewer.mine { &self.updates.mine } else { &self.updates.feed };
        let mut run: Vec<StatusEntry> = source
            .iter()
            .filter(|e| viewer.mine || e.status.author_jid == viewer.author_jid)
            .cloned()
            .collect();
        run.sort_by_key(|e| e.status.timestamp);
        run
    }
}

/// One status's one-line preview: its text, its caption, or
/// the typed kind label it carries (a media post without a
/// caption previews as its kind's word).
fn status_preview(status: &backend::LiveStatus) -> String {
    let body = status.body.trim();
    if !body.is_empty() {
        return body.to_owned();
    }
    match &status.kind {
        backend::LiveMessageKind::Media { media, .. } => match media {
            backend::MediaKind::Photo => crate::i18n::t("updates.photo"),
            backend::MediaKind::Video => crate::i18n::t("updates.video"),
            backend::MediaKind::Voice | backend::MediaKind::Audio => {
                crate::i18n::t("updates.voice")
            }
            _ => crate::i18n::t("updates.media"),
        },
        backend::LiveMessageKind::Other(label) => label.clone(),
        _ => String::new(),
    }
}

/// A status's age in the compact form the reference client
/// uses: minutes, then hours, then the clock time for
/// anything older (a status never lives past 24 hours, so
/// the clock time is always today's or yesterday's).
fn status_age_label(now: i64, ts: i64) -> String {
    let age = now.saturating_sub(ts);
    if age < 60 {
        crate::i18n::t("updates.age_now")
    } else if age < 3600 {
        crate::i18n::tf("updates.age_minutes", "count", &(age / 60).to_string())
    } else if age < 6 * 3600 {
        crate::i18n::tf("updates.age_hours", "count", &(age / 3600).to_string())
    } else {
        crate::live::fmt_time(ts)
    }
}

/// The egui color of a status's wire background (an ARGB
/// word), for the text stage.
fn argb_color(argb: u32) -> egui::Color32 {
    egui::Color32::from_rgb(
        ((argb >> 16) & 0xff) as u8,
        ((argb >> 8) & 0xff) as u8,
        (argb & 0xff) as u8,
    )
}

impl App {
    /// A status arrived (`StatusReceived`): someone in the
    /// audience posted, my own post was delivered back, or
    /// history sync redelivered one. Re-delivery of a status
    /// already held replaces it (keeping any local bytes of an
    /// own post); a new one joins its list, and expired ones
    /// are pruned on the way.
    pub(crate) fn apply_status_received(&mut self, status: backend::LiveStatus) {
        let entry = StatusEntry { status, local_bytes: None };
        if entry.status.from_me {
            upsert_status(&mut self.updates.mine, entry);
        } else {
            upsert_status(&mut self.updates.feed, entry);
        }
        self.prune_statuses();
    }

    /// A text post landed (the backend's `StatusPosted`, or a
    /// demo-mode local post): the post joins My Status under
    /// its protocol ID, the composer closes, and the notice
    /// line confirms it, the app's existing feedback pattern
    /// for a completed action. The matching `StatusReceived`
    /// echo, when it lands, replaces this entry by ID.
    pub(crate) fn apply_status_posted(&mut self, text: String, message_id: String) {
        let status = backend::LiveStatus {
            id: message_id,
            author_jid: String::new(),
            author_name: None,
            body: text,
            timestamp: self.status_now(),
            from_me: true,
            kind: backend::LiveMessageKind::Text,
            background_argb: Some(TEXT_STATUS_BACKGROUND_ARGB),
            reactions: Vec::new(),
            my_reaction: None,
            photo_ref: None,
            photo_thumbnail: None,
            video_ref: None,
            video_thumbnail: None,
            poster_ref: None,
            video_width: None,
            video_height: None,
        };
        upsert_status(&mut self.updates.mine, StatusEntry { status, local_bytes: None });
        self.updates.posting = false;
        self.updates.composer_open = false;
        self.updates.draft.clear();
        self.updates.staged = None;
        self.updates.error = None;
        self.notice = Some(Notice::info(crate::i18n::t("updates.posted")));
    }

    /// A media post landed (the backend's
    /// `StatusMediaPosted`): the post joins My Status carrying
    /// the bytes that were just posted (the only copy the
    /// session view can show until the post is redelivered
    /// with references), and the composer settles like a text
    /// post's.
    pub(crate) fn apply_status_media_posted(&mut self, status: backend::LiveStatus) {
        let local_bytes = self.updates.pending_media.take().map(|staged| staged.data);
        upsert_status(&mut self.updates.mine, StatusEntry { status, local_bytes });
        self.updates.posting = false;
        self.updates.composer_open = false;
        self.updates.draft.clear();
        self.updates.staged = None;
        self.updates.error = None;
        self.notice = Some(Notice::info(crate::i18n::t("updates.posted")));
    }

    /// A live status post failed (the backend's
    /// `StatusPostFailed`): the previous status state is kept,
    /// and the failure is shown inside the still-open composer
    /// instead of being dressed up as a success. A staged
    /// media file goes back to the composer so the post can be
    /// retried. The message passes through the display
    /// scrubber first, like every backend failure the app
    /// renders.
    pub(crate) fn apply_status_post_failed(&mut self, message: String) {
        let message = core::crash::scrub_for_display(&message);
        tracing::warn!("status post failed: {message}");
        self.updates.posting = false;
        if let Some(staged) = self.updates.pending_media.take() {
            if self.updates.staged.is_none() {
                self.updates.staged = Some(staged);
            }
        }
        self.updates.error = Some(crate::live::human_error(&message));
    }

    /// A delete of one of my statuses landed (the backend's
    /// `StatusDeleted`): the post leaves My Status, the
    /// viewer's run follows, and the notice confirms it.
    pub(crate) fn apply_status_deleted(&mut self, message_id: &str) {
        self.updates.mine.retain(|e| e.status.id != message_id);
        if let Some(viewer) = &mut self.updates.viewer {
            viewer.deleting = false;
            viewer.error = None;
        }
        self.notice = Some(Notice::info(crate::i18n::t("updates.deleted")));
    }

    /// A delete failed (the backend's `StatusDeleteFailed`):
    /// the status stays, and the failure shows inside the
    /// viewer where the delete was attempted.
    pub(crate) fn apply_status_delete_failed(&mut self, message_id: &str, message: String) {
        let message = core::crash::scrub_for_display(&message);
        tracing::warn!("status delete failed for {message_id}: {message}");
        if let Some(viewer) = &mut self.updates.viewer {
            viewer.deleting = false;
            viewer.error = Some(crate::live::human_error(&message));
        } else {
            self.notice = Some(Notice::error(crate::live::human_error(&message)));
        }
    }

    /// A status was revoked (a `MessageRevoked` addressed to
    /// the broadcast): the author's delete, or my own delete
    /// landing from another device. The status leaves
    /// whichever list held it.
    pub(crate) fn apply_status_revoked(&mut self, message_id: &str) {
        self.updates.feed.retain(|e| e.status.id != message_id);
        self.updates.mine.retain(|e| e.status.id != message_id);
        self.updates.viewed.remove(message_id);
    }

    /// A reaction update addressed to the broadcast: it
    /// belongs to a status. My own reaction settles from it
    /// (the server has spoken); anyone else's updates the
    /// status's reaction summary through the per-actor map,
    /// the same accounting the chat path keeps.
    pub(crate) fn apply_status_reaction(&mut self, message_id: &str, actor: &str, emoji: &str) {
        if actor == "Me" {
            self.updates.reaction_pending.remove(message_id);
            if let Some(entry) = self.find_status_mut(message_id) {
                entry.status.my_reaction =
                    if emoji.is_empty() { None } else { Some(emoji.to_owned()) };
            }
            return;
        }
        let actors = self.updates.reaction_actors.entry(message_id.to_owned()).or_default();
        if emoji.is_empty() {
            actors.remove(actor);
        } else {
            actors.insert(actor.to_owned(), emoji.to_owned());
        }
        let counts = count_status_reactions(actors);
        if counts.is_empty() {
            self.updates.reaction_actors.remove(message_id);
        }
        if let Some(entry) = self.find_status_mut(message_id) {
            entry.status.reactions = counts;
        }
    }

    /// A reaction to a status failed (a `ReactionFailed`
    /// addressed to the broadcast): my optimistic reaction
    /// rolls back to what it was, and the failure shows inside
    /// the viewer.
    pub(crate) fn apply_status_reaction_failed(&mut self, message_id: &str, message: &str) {
        let message = core::crash::scrub_for_display(message);
        tracing::warn!("status reaction failed: {message}");
        if let Some(previous) = self.updates.reaction_pending.remove(message_id) {
            if let Some(entry) = self.find_status_mut(message_id) {
                entry.status.my_reaction = previous;
            }
        }
        if let Some(viewer) = &mut self.updates.viewer {
            viewer.error = Some(crate::live::human_error(&message));
        }
    }

    /// Finds a status entry by protocol ID, in the feed first,
    /// then in My Status.
    fn find_status_mut(&mut self, message_id: &str) -> Option<&mut StatusEntry> {
        if let Some(entry) = self.updates.feed.iter_mut().find(|e| e.status.id == message_id) {
            return Some(entry);
        }
        self.updates.mine.iter_mut().find(|e| e.status.id == message_id)
    }

    /// The status composer's Post action. A staged media file
    /// posts as a media status; otherwise the draft posts as a
    /// text status. Returns nothing: the outcome lands in the
    /// state. In demo mode (no live session) the post is local
    /// state only, and the composer closes at once. With a
    /// session, the command goes to the backend with the
    /// account's direct-chat partners as the audience, the
    /// composer stays open in its posting state, and the
    /// result event settles it: the posted events close it, a
    /// failure keeps it open with the failure inside.
    /// Validation failures refuse in the modal and send
    /// nothing.
    pub(crate) fn post_my_status(&mut self) {
        if self.updates.staged.is_some() {
            self.post_my_media_status();
            return;
        }
        let text = self.updates.draft.trim().to_owned();
        if text.is_empty() {
            self.updates.error = Some(crate::i18n::t("updates.empty_error"));
            return;
        }
        self.updates.error = None;
        if self.live.is_some() {
            let recipients = self.status_recipients();
            if recipients.is_empty() {
                self.updates.error = Some(crate::i18n::t("updates.no_recipients"));
                return;
            }
            self.updates.last_audience = recipients.clone();
            self.updates.posting = true;
            self.send_backend(backend::BackendCommand::PostTextStatus { text, recipients });
            return;
        }
        self.updates.local_seq += 1;
        let id = format!("local-status-{}", self.updates.local_seq);
        self.apply_status_posted(text, id);
    }

    /// The media half of the composer's Post action: the
    /// staged file goes out as a photo or video status with
    /// the draft as its caption.
    fn post_my_media_status(&mut self) {
        let Some(staged) = self.updates.staged.take() else {
            return;
        };
        let caption = self.updates.draft.trim().to_owned();
        self.updates.error = None;
        if self.live.is_some() {
            let recipients = self.status_recipients();
            if recipients.is_empty() {
                self.updates.error = Some(crate::i18n::t("updates.no_recipients"));
                self.updates.staged = Some(staged);
                return;
            }
            self.updates.last_audience = recipients.clone();
            self.updates.posting = true;
            let command = backend::BackendCommand::PostMediaStatus {
                kind: staged.kind,
                data: staged.data.clone(),
                mime: staged.mime.clone(),
                caption,
                thumbnail: staged.thumbnail.clone(),
                seconds: staged.seconds,
                recipients,
            };
            self.updates.pending_media = Some(staged);
            self.send_backend(command);
            return;
        }
        // Demo mode: the post is local state, carrying its
        // bytes for the session view.
        self.updates.local_seq += 1;
        let status = backend::LiveStatus {
            id: format!("local-status-{}", self.updates.local_seq),
            author_jid: String::new(),
            author_name: None,
            body: caption.clone(),
            timestamp: self.status_now(),
            from_me: true,
            kind: backend::LiveMessageKind::Media {
                media: staged.kind,
                caption,
                seconds: staged.seconds,
                size_bytes: staged.data.len() as u64,
            },
            background_argb: None,
            reactions: Vec::new(),
            my_reaction: None,
            photo_ref: None,
            photo_thumbnail: if matches!(staged.kind, backend::MediaKind::Photo) {
                Some(staged.thumbnail.clone())
            } else {
                None
            },
            video_ref: None,
            video_thumbnail: if matches!(staged.kind, backend::MediaKind::Video) {
                Some(staged.thumbnail.clone())
            } else {
                None
            },
            poster_ref: None,
            video_width: None,
            video_height: None,
        };
        let local_bytes = staged.data;
        self.updates.pending_media = None;
        upsert_status(
            &mut self.updates.mine,
            StatusEntry { status, local_bytes: Some(local_bytes) },
        );
        self.updates.posting = false;
        self.updates.composer_open = false;
        self.updates.draft.clear();
        self.notice = Some(Notice::info(crate::i18n::t("updates.posted")));
    }

    /// The audience of a status post: the JIDs of the account's
    /// direct chats, resolved through the live session's chat
    /// map. Groups and channels are not status audiences. Empty
    /// without a session, or when the account has no direct
    /// chats yet.
    fn status_recipients(&self) -> Vec<String> {
        let Some(session) = &self.live else {
            return Vec::new();
        };
        self.chats
            .iter()
            .filter(|chat| chat.kind == core::model::ChatKind::Dm)
            .filter_map(|chat| session.jids.get(&chat.id).cloned())
            .collect()
    }

    /// Deletes one of my own statuses (the confirm modal's
    /// decision). With a session the revoke fans out to the
    /// audience the posts used (or the current direct-chat
    /// partners when that list is gone); the result event
    /// settles it. In demo mode the post is local state and
    /// leaves at once.
    pub(crate) fn delete_my_status(&mut self, message_id: &str) {
        if self.live.is_none() {
            self.apply_status_deleted(message_id);
            return;
        }
        let recipients = if self.updates.last_audience.is_empty() {
            self.status_recipients()
        } else {
            self.updates.last_audience.clone()
        };
        if recipients.is_empty() {
            self.apply_status_delete_failed(message_id, crate::i18n::t("updates.no_recipients"));
            return;
        }
        if let Some(viewer) = &mut self.updates.viewer {
            viewer.deleting = true;
            viewer.error = None;
        }
        self.send_backend(backend::BackendCommand::DeleteStatus {
            message_id: message_id.to_owned(),
            recipients,
        });
    }

    /// Sends the viewer's reply to the shown status: a text
    /// message to the author's direct chat, quoting the status
    /// (the quote's home is the broadcast, so the quote
    /// carries it as its remote chat). The local echo lands in
    /// the author's conversation like a composer's send, so
    /// the reply is visible there afterwards.
    pub(crate) fn send_status_reply(&mut self) {
        let Some(viewer) = &self.updates.viewer else {
            return;
        };
        let run = self.status_run(viewer);
        let Some(entry) = run.get(viewer.index.min(run.len().saturating_sub(1))) else {
            return;
        };
        let status = entry.status.clone();
        let text = viewer.reply.trim().to_owned();
        if text.is_empty() || status.from_me || self.live.is_none() {
            return;
        }
        let quoted = backend::QuotedRef {
            message_id: status.id.clone(),
            sender_jid: Some(status.author_jid.clone()),
            excerpt: status.body.chars().take(200).collect(),
            remote_jid: Some(crate::live::STATUS_BROADCAST_JID.to_owned()),
        };
        self.send_backend(backend::BackendCommand::SendText {
            chat_jid: status.author_jid.clone(),
            body: text.clone(),
            quoted: Some(quoted),
            ephemeral_secs: 0,
        });
        // The local echo, the composer's own pattern: a
        // Sending message in the author's conversation, bound
        // to the protocol ID when MessageSent lands.
        let chat_id = crate::live::jid_id(&status.author_jid);
        let now = crate::live::unix_now_secs() as i64;
        self.sent_counter += 1;
        let mut echo = core::model::Message::mine(9_000_000 + self.sent_counter, text, "\u{2026}");
        echo.delivery = Some(core::model::Delivery::Sending);
        echo.quoted_sender = status.author_name.clone();
        echo.quoted_body = Some(status.body.chars().take(200).collect());
        if let Some(session) = &mut self.live {
            session.messages.entry(chat_id).or_default().push((now, echo));
        }
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
        if let Some(viewer) = &mut self.updates.viewer {
            viewer.reply.clear();
            viewer.error = None;
        }
        self.notice = Some(Notice::info(crate::i18n::t("updates.reply_sent")));
    }

    /// Reacts to the shown status (or removes my reaction
    /// when the same emoji is tapped again): optimistic, with
    /// the previous reaction parked for the failure rollback.
    /// The reaction targets the status under the broadcast,
    /// with the author as its sender.
    pub(crate) fn react_to_status(&mut self, status: &backend::LiveStatus, emoji: &str) {
        if status.from_me || self.live.is_none() {
            return;
        }
        let previous = status.my_reaction.clone();
        let next = if previous.as_deref() == Some(emoji) { "" } else { emoji };
        self.updates.reaction_pending.insert(status.id.clone(), previous);
        if let Some(entry) = self.find_status_mut(status.id.as_str()) {
            entry.status.my_reaction = if next.is_empty() { None } else { Some(next.to_owned()) };
        }
        self.send_backend(backend::BackendCommand::React {
            chat_jid: crate::live::STATUS_BROADCAST_JID.to_owned(),
            message_id: status.id.clone(),
            sender_jid: Some(status.author_jid.clone()),
            from_me: false,
            emoji: next.to_owned(),
        });
    }

    /// Starts the byte fetch for a status photo, unless its
    /// bytes are already here or a fetch is already running:
    /// the ordinary fetch command addressed to the broadcast,
    /// whose protocol cache holds the status payload like any
    /// received message's.
    pub(crate) fn ensure_status_photo_fetch(&mut self, status: &backend::LiveStatus) {
        let id = crate::live::message_id(crate::live::STATUS_BROADCAST_JID, &status.id);
        if self.media.photo_fetch.contains_key(&id) || self.live.is_none() {
            return;
        }
        self.media.photo_fetch.insert(id, PhotoFetchState::Loading);
        self.send_backend(backend::BackendCommand::FetchPhoto {
            chat_jid: crate::live::STATUS_BROADCAST_JID.to_owned(),
            message_id: status.id.clone(),
            photo_ref: status.photo_ref.clone(),
        });
    }

    /// Starts the byte fetch for a status video, mirroring
    /// `ensure_video_fetch`'s bookkeeping with the broadcast
    /// addressing. A post made from this client has no
    /// reference to fetch by: its local bytes are seeded into
    /// the player's map instead.
    pub(crate) fn ensure_status_video_fetch(&mut self, entry: &StatusEntry) {
        let status = &entry.status;
        let id = crate::live::message_id(crate::live::STATUS_BROADCAST_JID, &status.id);
        if self.media.video_files.contains_key(&id)
            || self.media.video_bytes.contains_key(&id)
            || self.media.video_fetch.contains_key(&id)
            || self.live.is_none()
        {
            return;
        }
        if let Some(bytes) = &entry.local_bytes {
            self.media.video_bytes.insert(id, bytes.clone());
            return;
        }
        self.media.video_fetch.insert(id, PhotoFetchState::Loading);
        self.send_backend(backend::BackendCommand::FetchVideo {
            chat_jid: crate::live::STATUS_BROADCAST_JID.to_owned(),
            message_id: status.id.clone(),
            video_ref: status.video_ref.clone(),
            view_once: false,
        });
    }

    /// Retries a failed status video fetch (the video stage's
    /// retry signal, dispatched against the status source
    /// instead of the open chat).
    pub(crate) fn retry_status_video_fetch(&mut self, entry: &StatusEntry) {
        let id = crate::live::message_id(crate::live::STATUS_BROADCAST_JID, &entry.status.id);
        if self.media.video_fetch.get(&id) == Some(&PhotoFetchState::Failed) {
            self.media.video_fetch.remove(&id);
        }
        self.ensure_status_video_fetch(entry);
    }
}

/// Inserts or replaces a status in one list, by protocol ID:
/// a re-delivered status replaces the held one, keeping the
/// local bytes of an own post when the replacement has none.
fn upsert_status(list: &mut Vec<StatusEntry>, entry: StatusEntry) {
    if let Some(existing) = list.iter_mut().find(|e| e.status.id == entry.status.id) {
        let local = existing.local_bytes.take();
        *existing = entry;
        if existing.local_bytes.is_none() {
            existing.local_bytes = local;
        }
        return;
    }
    list.push(entry);
}

/// Counts a per-actor reaction map into the (emoji, count)
/// summary a status carries, in first-seen emoji order: the
/// chat path's accounting, for one status's actor map.
fn count_status_reactions(
    actors: &std::collections::HashMap<String, String>,
) -> Vec<(String, u32)> {
    let mut counts: Vec<(String, u32)> = Vec::new();
    for emoji in actors.values() {
        if let Some((_, count)) = counts.iter_mut().find(|(e, _)| e == emoji) {
            *count += 1;
        } else {
            counts.push((emoji.clone(), 1));
        }
    }
    counts
}

impl App {
    /// Opens the file picker for a status media post and
    /// stages the picked file in the composer: kind and MIME
    /// from the extension (the attach picker's rules), the
    /// JPEG thumbnail made here (the photo at preview
    /// fidelity, or the video's own first frame), and the
    /// video's duration probed with the app's demuxer. A file
    /// that is neither photo nor video, or cannot be read, is
    /// refused in the composer, named and with the reason.
    pub(crate) fn pick_status_media(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter(
                crate::i18n::t("attach.filter_media").as_str(),
                &["jpg", "jpeg", "png", "webp", "gif", "mp4", "mkv", "webm", "mov", "avi"],
            )
            .pick_file()
        else {
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_owned());
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let kind = if ["jpg", "jpeg", "png", "webp", "gif"].contains(&ext.as_str()) {
            Some(backend::MediaKind::Photo)
        } else if ["mp4", "mkv", "webm", "mov", "avi"].contains(&ext.as_str()) {
            Some(backend::MediaKind::Video)
        } else {
            None
        };
        let Some(kind) = kind else {
            self.updates.error = Some(crate::i18n::t("updates.media_kind_error"));
            return;
        };
        let data = match std::fs::read(&path) {
            Ok(data) if !data.is_empty() => data,
            _ => {
                self.updates.error = Some(crate::i18n::t("updates.media_read_error"));
                return;
            }
        };
        let mime = match kind {
            backend::MediaKind::Photo => {
                format!("image/{}", if ext == "jpg" { "jpeg".to_owned() } else { ext.clone() })
            }
            _ => format!("video/{ext}"),
        };
        let (thumbnail, seconds) = match kind {
            backend::MediaKind::Photo => (crate::media::jpeg_thumbnail(&data), 0),
            _ => {
                let seconds = crate::video::demux(&data)
                    .map(|d| d.info.duration_secs.round() as u32)
                    .unwrap_or(0);
                (crate::media::video_poster_jpeg(&data).unwrap_or_default(), seconds)
            }
        };
        self.updates.error = None;
        self.updates.staged =
            Some(StagedStatusMedia { kind, name, data, mime, thumbnail, seconds });
    }

    /// The status composer modal: the house centered-modal
    /// pattern (the create-group dialog's shape), one text
    /// field, an optional staged media file, and a Post
    /// action. Drawn from the shell so it floats over
    /// whichever layout the Updates screen took.
    pub(crate) fn draw_status_composer(&mut self, ctx: &egui::Context) {
        let Some(p) = crate::anim::popup_phase(
            ctx,
            "status_composer",
            self.updates.composer_open,
            self.motion,
        ) else {
            return;
        };
        let exiting = !self.updates.composer_open;
        let t = self.tokens();
        let mut post = false;
        let mut close = false;
        let mut pick = false;
        let mut unstage = false;
        egui::Area::new(egui::Id::unique("status_composer"))
            .pivot(Align2::CENTER_CENTER)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if p < 0.999 {
                    ui.set_opacity(p);
                }
                let frame = crate::theme::glass_panel_frame(t, crate::theme::RADIUS_PANEL)
                    .inner_margin(Margin::symmetric(12, 10));
                let _inner = frame.show(ui, |ui| {
                    ui.set_width(300.0);
                    ui.add_enabled_ui(!exiting && !self.updates.posting, |ui| {
                        ui.label(
                            RichText::new(crate::i18n::t("updates.composer_title"))
                                .size(15.0)
                                .strong(),
                        );
                        ui.add_space(SP_1_5);
                        if let Some(staged) = &self.updates.staged {
                            // The staged file's chip: kind icon,
                            // name, size, and the remove action.
                            ui.horizontal(|ui| {
                                let (icon_rect, _) =
                                    ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
                                crate::icons::draw(
                                    ui.painter(),
                                    if matches!(staged.kind, backend::MediaKind::Video) {
                                        crate::icons::Icon::Video
                                    } else {
                                        crate::icons::Icon::Image
                                    },
                                    icon_rect.center(),
                                    crate::icons::SIZE_ROW,
                                    rgb(t.text_secondary),
                                    rgb(t.bg_panel),
                                );
                                ui.label(
                                    RichText::new(crate::widgets::ellipsize(
                                        ui,
                                        &staged.name,
                                        FontId::proportional(12.0),
                                        180.0,
                                    ))
                                    .size(12.0),
                                );
                                ui.label(
                                    RichText::new(crate::shell::video_size(
                                        staged.data.len() as u64
                                    ))
                                    .size(12.0)
                                    .color(rgb(t.text_faint)),
                                );
                                if crate::widgets::icon_button(
                                    ui,
                                    t,
                                    crate::icons::Icon::X,
                                    22.0,
                                    &crate::i18n::t("common.remove"),
                                    None,
                                )
                                .clicked()
                                {
                                    unstage = true;
                                }
                            });
                            ui.add_space(SP_1_5);
                        }
                        crate::widgets::text_field(
                            ui,
                            t,
                            &mut self.updates.draft,
                            &crate::i18n::t(if self.updates.staged.is_some() {
                                "updates.caption_hint"
                            } else {
                                "updates.composer_hint"
                            }),
                            280.0,
                        );
                        if self.updates.staged.is_none() {
                            ui.add_space(SP_1_5);
                            if crate::widgets::button(
                                ui,
                                t,
                                &crate::i18n::t("updates.attach_media"),
                                crate::widgets::Btn::Secondary,
                            )
                            .clicked()
                            {
                                pick = true;
                            }
                        }
                        if self.live.is_none() {
                            ui.label(
                                RichText::new(crate::i18n::t("updates.demo_note"))
                                    .size(10.0)
                                    .color(rgb(t.text_faint)),
                            );
                        }
                        if let Some(error) = &self.updates.error {
                            ui.add_space(SP_1);
                            ui.label(RichText::new(error.clone()).size(12.0).color(rgb(t.danger)));
                        }
                        ui.add_space(SP_2);
                        if self.updates.posting {
                            crate::widgets::loading_row(
                                ui,
                                &crate::i18n::t("updates.posting"),
                                14.0,
                                t.text_secondary,
                            );
                            ui.add_space(SP_2);
                        }
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
                                &crate::i18n::t("updates.post"),
                                crate::widgets::Btn::Primary,
                            )
                            .clicked()
                            {
                                post = true;
                            }
                        });
                    });
                });
            });
        crate::anim::popup_transform(
            ctx,
            egui::Id::unique("status_composer"),
            ctx.content_rect().center(),
            p,
        );
        if pick {
            self.pick_status_media();
        }
        if unstage {
            self.updates.staged = None;
        }
        if post {
            self.post_my_status();
        }
        if close {
            self.updates.composer_open = false;
            self.updates.error = None;
            self.updates.staged = None;
        }
    }
}

impl App {
    /// The status viewer overlay: a full-screen dark stage in
    /// the media viewer's family. The header names the author
    /// over progress segments (one per status in the run), the
    /// stage presents the post (text on its own background, a
    /// photo fitted, a video through the shared video stage),
    /// and the bottom bar carries the run's actions: reply and
    /// quick reactions for someone else's status, delete for
    /// my own. Drawn from the shell.
    pub(crate) fn draw_status_viewer(&mut self, ctx: &egui::Context) {
        // Snapshot the viewer state while open, like the other
        // popups: the exit animation outlives the state.
        if let Some(viewer) = &self.updates.viewer {
            ctx.data_mut(|d| {
                d.insert_temp(egui::Id::unique("status_viewer_snap"), viewer.clone());
            });
        }
        let Some(p) = crate::anim::popup_phase(
            ctx,
            "status_viewer",
            self.updates.viewer.is_some(),
            self.motion,
        ) else {
            return;
        };
        let exiting = self.updates.viewer.is_none();
        let Some(viewer) =
            ctx.data(|d| d.get_temp::<StatusViewer>(egui::Id::unique("status_viewer_snap")))
        else {
            return;
        };
        let run = self.status_run(&viewer);
        if run.is_empty() {
            if !exiting {
                self.updates.viewer = None;
            }
            return;
        }
        let index = viewer.index.min(run.len() - 1);
        let entry = run[index].clone();
        let status = entry.status.clone();
        let vid = crate::live::message_id(crate::live::STATUS_BROADCAST_JID, &status.id);
        let media = match &status.kind {
            backend::LiveMessageKind::Media { media, seconds, .. } => Some((*media, *seconds)),
            _ => None,
        };
        // Opening an item is what starts its media fetch, once
        // per item: a photo's bytes (or its local copy for an
        // own post), a video's route dispatch.
        if !exiting {
            if !viewer.mine {
                self.updates.viewed.insert(status.id.clone());
            }
            match media {
                Some((backend::MediaKind::Photo, _)) => {
                    if viewer.dispatched.as_deref() != Some(status.id.as_str()) {
                        if let Some(bytes) = &entry.local_bytes {
                            if !self.media.textures.contains_key(&vid) {
                                if let Some(tex) = crate::media::decode_photo(ctx, bytes) {
                                    self.media.textures.insert(vid, tex);
                                }
                            }
                        } else {
                            if !self.media.textures.contains_key(&vid) {
                                if let Some(thumb) = &status.photo_thumbnail {
                                    if let Some(tex) = crate::media::decode_photo(ctx, thumb) {
                                        self.media.textures.insert(vid, tex);
                                    }
                                }
                            }
                            self.ensure_status_photo_fetch(&status);
                        }
                        if let Some(live_viewer) = &mut self.updates.viewer {
                            live_viewer.dispatched = Some(status.id.clone());
                        }
                    }
                }
                Some((backend::MediaKind::Video, seconds)) => match self.video_play_route(vid) {
                    crate::media::VideoPlayRoute::Idle => {
                        self.ensure_status_video_fetch(&entry);
                    }
                    crate::media::VideoPlayRoute::Ready => {
                        self.start_video_player(vid, seconds);
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        let screen = ctx.content_rect();
        let stage_w = (screen.width() - 32.0).clamp(200.0, 430.0);
        let stage_x = screen.center().x - stage_w / 2.0;
        let bottom_h = if viewer.mine { 76.0 } else { 132.0 };
        let header_rect = egui::Rect::from_min_size(
            Pos2::new(stage_x, screen.top() + 10.0),
            Vec2::new(stage_w, 62.0),
        );
        let stage_rect = egui::Rect::from_min_max(
            Pos2::new(stage_x, header_rect.bottom() + 8.0),
            Pos2::new(stage_x + stage_w, screen.bottom() - bottom_h - 10.0),
        );

        let mut close = false;
        let mut step: i32 = 0;
        let mut retry_photo = false;
        if !exiting && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }
        egui::Area::new(egui::Id::unique("status_viewer"))
            .order(egui::Order::Foreground)
            .fixed_pos(Pos2::ZERO)
            .show(ctx, |ui| {
                if p < 0.999 {
                    ui.set_opacity(p);
                }
                let painter = ui.painter().clone();
                painter.rect_filled(
                    screen,
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(10, 10, 10, (235.0 * p) as u8),
                );
                // Progress segments: one per status in the run,
                // filled up to and including the current one.
                let seg_gap = 4.0;
                let seg_w = (stage_w - seg_gap * (run.len() as f32 - 1.0).max(0.0))
                    / run.len().max(1) as f32;
                for i in 0..run.len() {
                    let seg = egui::Rect::from_min_size(
                        Pos2::new(stage_x + i as f32 * (seg_w + seg_gap), screen.top() + 10.0),
                        Vec2::new(seg_w, 3.0),
                    );
                    let fill = if i <= index {
                        crate::shell::VIEWER_TEXT
                    } else {
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 60)
                    };
                    painter.rect_filled(seg, CornerRadius::same(2), fill);
                }
                // Header: the author's avatar and name, the
                // post's age, and the close control.
                let avatar_center =
                    Pos2::new(header_rect.left() + 20.0, header_rect.center().y + 4.0);
                if viewer.mine {
                    crate::profile::paint_my_avatar_disc(
                        &painter,
                        avatar_center,
                        18.0,
                        &self.my_profile,
                        self.tokens(),
                    );
                } else {
                    crate::widgets::paint_initials_avatar(
                        &painter,
                        avatar_center,
                        18.0,
                        status.author_name.as_deref().unwrap_or(""),
                        (crate::live::jid_id(&status.author_jid) & 0xff) as u8,
                    );
                }
                let name = if viewer.mine {
                    crate::i18n::t("updates.my_status")
                } else {
                    status.author_name.clone().unwrap_or_else(|| {
                        status.author_jid.split('@').next().unwrap_or("").to_owned()
                    })
                };
                painter.text(
                    Pos2::new(header_rect.left() + 48.0, header_rect.center().y - 4.0),
                    Align2::LEFT_CENTER,
                    name,
                    crate::theme::medium(15.0),
                    crate::shell::VIEWER_TEXT,
                );
                painter.text(
                    Pos2::new(header_rect.left() + 48.0, header_rect.center().y + 14.0),
                    Align2::LEFT_CENTER,
                    status_age_label(self.status_now(), status.timestamp),
                    FontId::proportional(12.0),
                    crate::shell::VIEWER_TEXT_SECONDARY,
                );
                let close_rect = egui::Rect::from_center_size(
                    Pos2::new(header_rect.right() - 18.0, header_rect.center().y + 4.0),
                    Vec2::splat(32.0),
                );
                crate::icons::draw(
                    &painter,
                    crate::icons::Icon::X,
                    close_rect.center(),
                    crate::icons::SIZE_TAB,
                    crate::shell::VIEWER_TEXT,
                    egui::Color32::TRANSPARENT,
                );
                if ui
                    .interact(close_rect, egui::Id::unique("status_viewer_close"), Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                    && !exiting
                {
                    close = true;
                }

                // The stage itself, per kind.
                match &status.kind {
                    backend::LiveMessageKind::Text => {
                        let fill = status
                            .background_argb
                            .map(argb_color)
                            .unwrap_or_else(|| rgb(self.tokens().accent));
                        painter.rect_filled(stage_rect, CornerRadius::same(12), fill);
                        let mut slot = ui.new_child(
                            egui::UiBuilder::new().max_rect(stage_rect.shrink(24.0)).layout(
                                egui::Layout::centered_and_justified(egui::Direction::TopDown),
                            ),
                        );
                        slot.label(
                            RichText::new(&status.body)
                                .font(crate::theme::semibold(23.0))
                                .color(egui::Color32::WHITE),
                        );
                    }
                    backend::LiveMessageKind::Media { media: kind, caption, seconds, .. } => {
                        match kind {
                            backend::MediaKind::Photo => {
                                self.paint_status_photo(ui, stage_rect, vid);
                                if self
                                    .media
                                    .photo_fetch
                                    .get(&vid)
                                    .is_some_and(|s| *s == PhotoFetchState::Failed)
                                {
                                    let resp = ui.interact(
                                        stage_rect,
                                        egui::Id::unique("status_photo_retry"),
                                        Sense::click(),
                                    );
                                    if resp.clicked() && !exiting {
                                        retry_photo = true;
                                    }
                                }
                            }
                            backend::MediaKind::Video => {
                                let route = self.video_play_route(vid);
                                let (_, retry) = self.paint_video_stage(
                                    ui, stage_rect, vid, false, *seconds, exiting, route, false,
                                );
                                if retry && !exiting {
                                    self.retry_status_video_fetch(&entry);
                                }
                            }
                            _ => {
                                self.paint_status_label(ui, stage_rect, &status);
                            }
                        }
                        if !caption.trim().is_empty() {
                            painter.text(
                                Pos2::new(stage_rect.center().x, stage_rect.bottom() - 10.0),
                                Align2::CENTER_BOTTOM,
                                crate::widgets::ellipsize(
                                    ui,
                                    caption,
                                    FontId::proportional(12.0),
                                    stage_w - 32.0,
                                ),
                                FontId::proportional(12.0),
                                crate::shell::VIEWER_TEXT,
                            );
                        }
                    }
                    _ => {
                        self.paint_status_label(ui, stage_rect, &status);
                    }
                }

                // Previous / next controls at the stage's
                // edges, when the run has somewhere to go.
                if run.len() > 1 {
                    for (dx, icon, delta) in [
                        (-1.0f32, crate::icons::Icon::ChevronLeft, -1i32),
                        (1.0f32, crate::icons::Icon::ChevronRight, 1i32),
                    ] {
                        let center = Pos2::new(
                            if dx < 0.0 {
                                stage_rect.left() + 24.0
                            } else {
                                stage_rect.right() - 24.0
                            },
                            stage_rect.center().y,
                        );
                        let zone = egui::Rect::from_center_size(center, Vec2::splat(40.0));
                        painter.circle_filled(
                            center,
                            18.0,
                            egui::Color32::from_rgba_unmultiplied(0, 0, 0, 90),
                        );
                        crate::icons::draw(
                            &painter,
                            icon,
                            center,
                            crate::icons::SIZE_TAB,
                            crate::shell::VIEWER_TEXT,
                            egui::Color32::TRANSPARENT,
                        );
                        if ui
                            .interact(
                                zone,
                                egui::Id::unique(format!("status_step_{delta}")),
                                Sense::click(),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                            && !exiting
                        {
                            step = delta;
                        }
                    }
                }
            });

        // The bottom bar: the run's actions, as real widgets
        // in their own area over the stage.
        let mut send_reply = false;
        let mut react: Option<String> = None;
        let mut ask_delete = false;
        egui::Area::new(egui::Id::unique("status_viewer_bar"))
            .order(egui::Order::Foreground)
            .fixed_pos(Pos2::new(stage_x, screen.bottom() - bottom_h - 4.0))
            .show(ctx, |ui| {
                if p < 0.999 {
                    ui.set_opacity(p);
                }
                let frame = egui::Frame::new()
                    .fill(egui::Color32::from_rgb(26, 26, 26))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::symmetric(12, 8));
                let _ = frame.show(ui, |ui| {
                    ui.set_width(stage_w - 24.0);
                    ui.add_enabled_ui(!exiting, |ui| {
                        if !status.reactions.is_empty() {
                            let summary = status
                                .reactions
                                .iter()
                                .map(|(emoji, count)| format!("{emoji} {count}"))
                                .collect::<Vec<_>>()
                                .join("   ");
                            ui.label(
                                RichText::new(summary)
                                    .size(12.0)
                                    .color(crate::shell::VIEWER_TEXT_SECONDARY),
                            );
                            ui.add_space(SP_1);
                        }
                        if let Some(error) = &viewer.error {
                            ui.label(
                                RichText::new(error.clone())
                                    .size(12.0)
                                    .color(rgb(self.tokens().danger)),
                            );
                            ui.add_space(SP_1);
                        }
                        if viewer.mine {
                            if viewer.deleting {
                                crate::widgets::loading_row(
                                    ui,
                                    &crate::i18n::t("updates.deleting"),
                                    14.0,
                                    self.tokens().text_secondary,
                                );
                            } else if crate::widgets::button(
                                ui,
                                self.tokens(),
                                &crate::i18n::t("updates.delete"),
                                crate::widgets::Btn::Danger,
                            )
                            .clicked()
                            {
                                ask_delete = true;
                            }
                        } else {
                            ui.horizontal(|ui| {
                                for emoji in crate::rich_ui::QUICK_REACTIONS {
                                    let active = status.my_reaction.as_deref() == Some(emoji);
                                    let button = if active {
                                        egui::Button::new(RichText::new(emoji).size(19.0))
                                            .fill(rgb(self.tokens().accent))
                                    } else {
                                        egui::Button::new(RichText::new(emoji).size(19.0))
                                            .frame(false)
                                    };
                                    if ui.add(button).clicked() {
                                        react = Some(emoji.to_owned());
                                    }
                                }
                            });
                            ui.add_space(SP_1_5);
                            ui.horizontal(|ui| {
                                let field_w = (stage_w - 24.0 - 52.0).max(80.0);
                                let t = self.tokens();
                                if let Some(live_viewer) = &mut self.updates.viewer {
                                    crate::widgets::text_field(
                                        ui,
                                        t,
                                        &mut live_viewer.reply,
                                        &crate::i18n::t("updates.reply_hint"),
                                        field_w,
                                    );
                                }
                                if crate::widgets::icon_button(
                                    ui,
                                    t,
                                    crate::icons::Icon::Send,
                                    30.0,
                                    &crate::i18n::t("updates.reply_send"),
                                    None,
                                )
                                .clicked()
                                {
                                    send_reply = true;
                                }
                            });
                        }
                    });
                });
            });

        // Settle the frame's actions against the live state.
        if retry_photo {
            self.media.photo_fetch.remove(&vid);
            if let Some(live_viewer) = &mut self.updates.viewer {
                live_viewer.dispatched = None;
            }
        }
        if send_reply {
            self.send_status_reply();
        }
        if let Some(emoji) = react {
            self.react_to_status(&status, &emoji);
        }
        if ask_delete {
            self.ui_confirm =
                Some(crate::state::UiConfirm::DeleteStatus { message_id: status.id.clone() });
        }
        if step != 0 {
            if let Some(live_viewer) = &mut self.updates.viewer {
                let next = (index as i32 + step).clamp(0, run.len() as i32 - 1) as usize;
                live_viewer.index = next;
                live_viewer.dispatched = None;
                live_viewer.error = None;
            }
        }
        if close {
            // A status video's player goes with the viewer,
            // like the photo viewer's close stops its player.
            if self.media.video_player.as_ref().is_some_and(|pl| pl.key == vid) {
                self.media.video_player = None;
            }
            self.updates.viewer = None;
        }
    }

    /// Paints a photo status's stage: the photo fitted to the
    /// stage once its bytes are here, the spinner while they
    /// travel, and the honest failure line when they could not
    /// be fetched (clicking the stage retries).
    fn paint_status_photo(&mut self, ui: &mut egui::Ui, stage_rect: egui::Rect, vid: u64) {
        let painter = ui.painter().clone();
        if let Some(tex) = self.media.textures.get(&vid) {
            let aspect = tex.aspect_ratio();
            let size = if aspect.is_finite() && aspect > 0.05 {
                let w = stage_rect.width().min(stage_rect.height() * aspect);
                Vec2::new(w, w / aspect)
            } else {
                stage_rect.size()
            };
            let rect = egui::Rect::from_center_size(stage_rect.center(), size);
            painter.image(
                tex.id(),
                rect,
                egui::Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            return;
        }
        painter.rect_filled(
            stage_rect,
            CornerRadius::same(12),
            egui::Color32::from_rgb(32, 32, 32),
        );
        match self.media.photo_fetch.get(&vid) {
            Some(PhotoFetchState::Failed) => {
                painter.text(
                    stage_rect.center(),
                    Align2::CENTER_CENTER,
                    crate::i18n::t("updates.photo_failed"),
                    FontId::proportional(12.0),
                    crate::shell::VIEWER_TEXT_SECONDARY,
                );
            }
            _ => {
                let spinner_rect =
                    egui::Rect::from_center_size(stage_rect.center(), Vec2::splat(36.0));
                let mut slot = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(spinner_rect)
                        .layout(egui::Layout::top_down(egui::Align::Center)),
                );
                crate::widgets::spinner(&mut slot, 28.0);
            }
        }
    }

    /// Paints a status kind the viewer cannot present as
    /// media (a voice status, an unsupported payload): the
    /// typed kind label it carries, centered, never a
    /// fabricated player.
    fn paint_status_label(
        &mut self,
        ui: &mut egui::Ui,
        stage_rect: egui::Rect,
        status: &backend::LiveStatus,
    ) {
        let painter = ui.painter().clone();
        painter.rect_filled(
            stage_rect,
            CornerRadius::same(12),
            egui::Color32::from_rgb(32, 32, 32),
        );
        painter.text(
            stage_rect.center(),
            Align2::CENTER_CENTER,
            status_preview(status),
            crate::theme::medium(15.0),
            crate::shell::VIEWER_TEXT,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{status_age_label, status_preview};
    use crate::state::{App, Args, Screen};
    use eframe::egui::{Pos2, Vec2};

    /// A demo-mode app (no live session) on the Updates
    /// screen, the harness the status tests drive state and
    /// frames through.
    fn updates_app(ctx: &egui::Context) -> App {
        crate::theme::install_fonts(ctx);
        // App::new may have attempted a real live start against
        // this machine's data dir; the test owns the state from
        // here (the calls tests' pattern).
        let mut app = App::new(Args::parse(), ctx);
        app.live = None;
        app.screen = Screen::Updates;
        app
    }

    fn status(
        id: &str,
        author_jid: &str,
        name: &str,
        body: &str,
        timestamp: i64,
        from_me: bool,
    ) -> backend::LiveStatus {
        backend::LiveStatus {
            id: id.to_owned(),
            author_jid: author_jid.to_owned(),
            author_name: if from_me { None } else { Some(name.to_owned()) },
            body: body.to_owned(),
            timestamp,
            from_me,
            kind: backend::LiveMessageKind::Text,
            background_argb: Some(0xFF00_A884),
            reactions: Vec::new(),
            my_reaction: None,
            photo_ref: None,
            photo_thumbnail: None,
            video_ref: None,
            video_thumbnail: None,
            poster_ref: None,
            video_width: None,
            video_height: None,
        }
    }

    const SARAH: &str = "628111@s.whatsapp.net";
    const EMMA: &str = "628222@s.whatsapp.net";

    #[test]
    fn feed_groups_by_author_newest_first() {
        let ctx = egui::Context::default();
        let mut app = updates_app(&ctx);
        app.apply_status_received(status("s1", SARAH, "Sarah", "morning", 1_000, false));
        app.apply_status_received(status("e1", EMMA, "Emma", "hi", 2_000, false));
        app.apply_status_received(status("s2", SARAH, "Sarah", "later", 3_000, false));
        let rows = app.feed_rows();
        assert_eq!(rows.len(), 2);
        // Sarah's newest (3000) outranks Emma's (2000).
        assert_eq!(rows[0].author_jid, SARAH);
        assert_eq!(rows[0].count, 2);
        assert_eq!(rows[0].unviewed, 2);
        assert_eq!(rows[0].preview, "later");
        assert_eq!(rows[1].author_jid, EMMA);
        assert_eq!(rows[1].count, 1);
    }

    #[test]
    fn viewed_marks_advance_the_open_index() {
        let ctx = egui::Context::default();
        let mut app = updates_app(&ctx);
        app.apply_status_received(status("s1", SARAH, "Sarah", "one", 1_000, false));
        app.apply_status_received(status("s2", SARAH, "Sarah", "two", 2_000, false));
        app.updates.viewed.insert("s1".to_owned());
        let rows = app.feed_rows();
        assert_eq!(rows[0].unviewed, 1);
        // The run is oldest first: s1 viewed, so the viewer
        // opens at s2 (index 1).
        assert_eq!(rows[0].open_index, 1);
        app.updates.viewed.insert("s2".to_owned());
        let rows = app.feed_rows();
        assert_eq!(rows[0].unviewed, 0);
        assert_eq!(rows[0].open_index, 0);
    }

    #[test]
    fn posted_then_echo_keeps_one_entry() {
        let ctx = egui::Context::default();
        let mut app = updates_app(&ctx);
        app.apply_status_posted("Shipping day".to_owned(), "m1".to_owned());
        assert_eq!(app.updates.mine.len(), 1);
        // The same post redelivered by the broadcast replaces
        // the local entry instead of doubling it.
        app.apply_status_received(status("m1", "", "", "Shipping day", 5_000, true));
        assert_eq!(app.updates.mine.len(), 1);
        assert_eq!(app.my_status_subtitle(), "Shipping day");
    }

    #[test]
    fn revoke_removes_from_feed_and_mine() {
        let ctx = egui::Context::default();
        let mut app = updates_app(&ctx);
        app.apply_status_received(status("s1", SARAH, "Sarah", "gone soon", 1_000, false));
        app.apply_status_received(status("m1", "", "", "mine", 2_000, true));
        app.updates.viewed.insert("s1".to_owned());
        app.apply_status_revoked("s1");
        assert!(app.updates.feed.is_empty());
        assert!(!app.updates.viewed.contains("s1"));
        app.apply_status_revoked("m1");
        assert!(app.updates.mine.is_empty());
    }

    #[test]
    fn reactions_count_by_actor_and_mine_settles() {
        let ctx = egui::Context::default();
        let mut app = updates_app(&ctx);
        app.apply_status_received(status("s1", SARAH, "Sarah", "look", 1_000, false));
        app.apply_status_reaction("s1", "628333@s.whatsapp.net", "❤️");
        app.apply_status_reaction("s1", "628444@s.whatsapp.net", "❤️");
        app.apply_status_reaction("s1", "628555@s.whatsapp.net", "👍");
        let entry = &app.updates.feed[0];
        let mut counts = entry.status.reactions.clone();
        counts.sort();
        assert_eq!(counts, vec![("❤️".to_owned(), 2), ("👍".to_owned(), 1)]);
        // An actor changing their reaction moves the count.
        app.apply_status_reaction("s1", "628555@s.whatsapp.net", "❤️");
        let entry = &app.updates.feed[0];
        assert_eq!(entry.status.reactions, vec![("❤️".to_owned(), 3)]);
        // My own reaction settles from the server's word.
        app.apply_status_reaction("s1", "Me", "😂");
        assert_eq!(app.updates.feed[0].status.my_reaction.as_deref(), Some("😂"));
        app.apply_status_reaction("s1", "Me", "");
        assert_eq!(app.updates.feed[0].status.my_reaction, None);
    }

    #[test]
    fn reaction_failure_rolls_back_to_the_previous() {
        let ctx = egui::Context::default();
        let mut app = updates_app(&ctx);
        app.apply_status_received(status("s1", SARAH, "Sarah", "look", 1_000, false));
        app.updates.feed[0].status.my_reaction = Some("❤️".to_owned());
        app.updates.reaction_pending.insert("s1".to_owned(), Some("❤️".to_owned()));
        app.updates.feed[0].status.my_reaction = Some("👍".to_owned());
        app.apply_status_reaction_failed("s1", "boom");
        assert_eq!(app.updates.feed[0].status.my_reaction.as_deref(), Some("❤️"));
        assert!(!app.updates.reaction_pending.contains_key("s1"));
    }

    #[test]
    fn delete_landing_removes_and_settles_the_viewer() {
        let ctx = egui::Context::default();
        let mut app = updates_app(&ctx);
        app.apply_status_received(status("m1", "", "", "mine", 1_000, true));
        app.open_status_viewer(None, true, 0);
        app.updates.viewer.as_mut().unwrap().deleting = true;
        app.apply_status_deleted("m1");
        assert!(app.updates.mine.is_empty());
        assert!(!app.updates.viewer.as_ref().unwrap().deleting);
    }

    #[test]
    fn prune_uses_the_status_timeline_clock() {
        let ctx = egui::Context::default();
        let mut app = updates_app(&ctx);
        // A scripted-era timeline: the newest status anchors
        // "now", so the one 25 hours older is past its life
        // while both are far from the wall clock.
        app.apply_status_received(status("old", SARAH, "Sarah", "old", 1_000, false));
        app.apply_status_received(status("new", SARAH, "Sarah", "new", 1_000 + 25 * 3600, false));
        assert_eq!(app.updates.feed.len(), 1);
        assert_eq!(app.updates.feed[0].status.id, "new");
    }

    #[test]
    fn demo_text_post_is_local_and_closes_the_composer() {
        let ctx = egui::Context::default();
        let mut app = updates_app(&ctx);
        app.updates.composer_open = true;
        app.updates.draft = "  Shipping day  ".to_owned();
        app.post_my_status();
        assert_eq!(app.updates.mine.len(), 1);
        assert_eq!(app.updates.mine[0].status.id, "local-status-1");
        assert_eq!(app.updates.mine[0].status.body, "Shipping day");
        assert!(!app.updates.composer_open);
        // An empty draft refuses inside the composer.
        app.updates.composer_open = true;
        app.updates.draft = "   ".to_owned();
        app.post_my_status();
        assert_eq!(app.updates.mine.len(), 1);
        assert!(app.updates.error.is_some());
        assert!(app.updates.composer_open);
    }

    #[test]
    fn previews_and_age_labels_read_like_the_reference() {
        let photo = status("p1", SARAH, "Sarah", "", 1_000, false);
        let mut photo = photo;
        photo.kind = backend::LiveMessageKind::Media {
            media: backend::MediaKind::Photo,
            caption: String::new(),
            seconds: 0,
            size_bytes: 0,
        };
        assert_eq!(status_preview(&photo), "Photo");
        photo.body = "at the beach".to_owned();
        assert_eq!(status_preview(&photo), "at the beach");
        assert_eq!(status_age_label(1_030, 1_000), "Just now");
        assert_eq!(status_age_label(1_000 + 5 * 60, 1_000), "5m ago");
        assert_eq!(status_age_label(1_000 + 2 * 3600, 1_000), "2h ago");
    }

    /// The Updates screen, the viewer over a text status, and
    /// the composer all draw real frames without panicking:
    /// draw-time side effects (the viewed mark, the fetch
    /// dispatch) are exercised, not just state assignments.
    #[test]
    fn updates_surfaces_draw_frames() {
        let ctx = egui::Context::default();
        let mut app = updates_app(&ctx);
        app.apply_status_received(status(
            "s1",
            SARAH,
            "Sarah",
            "hello status",
            1_700_000_000,
            false,
        ));
        app.apply_status_received(status("m1", "", "", "my own", 1_700_000_100, true));
        let mut time = 0.0f64;
        let drive = |app: &mut App, time: &mut f64| {
            *time += 0.05;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 760.0))),
                time: Some(*time),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ctx| {
                let mut root = egui::Ui::new(
                    ctx.clone(),
                    egui::Id::unique("updates_test_root"),
                    egui::UiBuilder::new()
                        .max_rect(egui::Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 760.0))),
                );
                app.draw_updates(&mut root);
                app.draw_status_composer(ctx);
                app.draw_status_viewer(ctx);
            });
            out.textures_delta.clear();
        };
        drive(&mut app, &mut time);
        app.open_status_viewer(Some(SARAH.to_owned()), false, 0);
        drive(&mut app, &mut time);
        drive(&mut app, &mut time);
        assert!(app.updates.viewed.contains("s1"), "viewing marks the status viewed");
        app.updates.viewer = None;
        app.updates.composer_open = true;
        app.updates.draft = "draft".to_owned();
        drive(&mut app, &mut time);
    }
}
