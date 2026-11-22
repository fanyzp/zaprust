//! App shell: the root frame dispatch, the navigation rail, and the
//! collapsed-sidebar strip.

use crate::state::{effective_pixels_per_point, layout_mode_for_content, App, LayoutMode, Screen};
use crate::theme::{apply_visuals, rgb};
use crate::widgets::rail_button;
use core::filter::total_unread;
use eframe::egui::{self, Align, Layout, Margin, RichText, Sense, Vec2};

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::B)) {
            self.sidebar_collapsed = !self.sidebar_collapsed;
        }
        apply_visuals(&ctx, self.tokens);
        // The layout mode follows the window's PHYSICAL size:
        // below the breakpoint the panes take turns
        // (single-pane), above it they share the width
        // (two-pane), and a portrait single-pane window takes
        // the native phone arrangement (portrait). The content
        // rect is in points at the pixels-per-point currently
        // in effect, so the mode helper scales it back to
        // physical pixels first: the density applied below can
        // then never move the breakpoint. Refreshed every frame
        // so a resize switches behavior immediately.
        // `single_pane` stays true in portrait mode: every "not
        // two-pane" behavior (pane swapping, the back chevron,
        // the full-width profile dock) applies there too.
        let mode = layout_mode_for_content(ctx.content_rect().size(), ctx.pixels_per_point());
        self.single_pane = mode != LayoutMode::TwoPane;
        self.portrait = mode == LayoutMode::Portrait;
        // The render scale: the user's font-size preference
        // composed with the mode's automatic density factor
        // (the small modes step down a touch; see state.rs).
        ctx.set_pixels_per_point(effective_pixels_per_point(self.zoom_percent, mode));
        self.poll_backend(&ctx);
        self.poll_workers(&ctx);
        self.poll_update(&ctx);
        self.drain_os_notifications(&ctx);
        self.tick_expiry();

        if self.debug.mode {
            self.draw_debug_banner(ui);
        }
        if self.screen == Screen::Connect {
            self.draw_connect(ui);
            self.draw_banners_on_connect(ui);
            // The house confirmation modal must be reachable
            // here too: the connect screen's start-fresh action
            // parks its confirmation in it.
            self.draw_ui_confirm(&ctx);
            return;
        }
        if self.portrait {
            // No rail in portrait mode: the bottom navigation
            // bar takes its place. It is drawn before the
            // screens so its panel reserves the bottom strip;
            // while full-screen content or a non-tab page is up
            // it is not drawn at all and the screen takes the
            // whole height.
            if self.bottom_nav_visible() {
                self.draw_bottom_nav(ui);
            }
        } else {
            self.draw_rail(ui);
            if self.sidebar_collapsed && self.screen.is_list_screen() {
                self.draw_collapsed_strip(ui);
                self.draw_content(ui);
                return;
            }
        }
        if self.screen.is_list_screen() {
            // The secondary screens swap the chat list for their
            // own left panel (Starred, Calls, the Communities
            // overview); the conversation on the right stays in
            // place either way (the owner's left-panel rule).
            // In single-pane mode only one of the two is drawn:
            // the list, or the open conversation (or community
            // home) over it, never both squeezed side by side.
            let show_list = !self.single_pane || !self.content_visible();
            let show_content = !self.single_pane || self.content_visible();
            if show_list {
                match self.screen {
                    Screen::Starred => self.draw_starred_panel(ui),
                    Screen::Calls => self.draw_calls_panel(ui),
                    Screen::Communities => self.draw_communities_panel(ui),
                    _ => self.draw_chat_list(ui),
                }
            }
            if show_content {
                self.draw_content(ui);
            }
            if self.portrait_fab_visible() {
                self.draw_new_chat_fab(ui);
            }
            if self.portrait_calls_fab_visible() {
                self.draw_calls_fab(ui);
            }
        } else if self.screen == Screen::Updates {
            self.draw_updates(ui);
        } else if self.screen == Screen::Settings {
            // Settings fills the content area: nav and page side
            // by side in two-pane mode, one at a time in
            // single-pane mode (see draw_settings).
            self.draw_settings(ui);
        } else if self.screen == Screen::Debug {
            self.draw_simple_list_panel(ui, "Debug");
            self.draw_debug_screen(ui);
        }
        // The account surfaces (menu header, rail button) paint
        // the signed-in user's photo from the same state the
        // profile editor maintains; keep its texture current
        // even while the editor is closed.
        let profile_mode = self.my_profile_mode();
        crate::profile::my_profile_sync_photo(&ctx, &mut self.my_profile, profile_mode);
        self.draw_account_menu(ui);
        self.draw_my_profile(&ctx);
        self.draw_photo_viewer(ui);
        self.draw_call_overlay(&ctx);
        self.draw_banners(ui);
    }
}

impl App {
    /// Whether the content pane (the conversation, or the open
    /// community's home) is the pane on screen in single-pane
    /// mode: a chat was opened, or a community home is up.
    pub(crate) fn content_visible(&self) -> bool {
        self.conversation_open || self.open_community.is_some()
    }

    /// The left list panel in the shape the layout mode needs:
    /// the resizable 350-wide panel of the two-pane shell, or,
    /// in single-pane mode, a panel spanning the full available
    /// width (everything right of the rail, or the whole window
    /// in portrait mode), since the list is then the only pane.
    pub(crate) fn list_panel(&self, root: &egui::Ui) -> egui::Panel {
        if self.single_pane {
            egui::Panel::left("chat_list").exact_size(root.available_width())
        } else {
            egui::Panel::left("chat_list")
                .resizable(true)
                .default_size(350.0)
                .size_range(280.0..=460.0)
        }
    }

    /// The content area next to the rail and chat list: the open
    /// community's home while one is open, otherwise the
    /// conversation. On a filtered list destination (Channels,
    /// Groups, Archived) whose filter does not contain the
    /// selected chat, the empty conversation placeholder takes
    /// the conversation's place, so the two panes never disagree
    /// about what is selected.
    fn draw_content(&mut self, root: &mut egui::Ui) {
        if self.open_community.is_some() {
            self.draw_community_home(root);
        } else if !self.selection_in_current_filter() {
            self.draw_conversation_placeholder(root);
        } else {
            self.draw_conversation(root);
        }
    }

    /// The empty conversation placeholder: the chat background
    /// and one quiet centred line, the same presentation the
    /// pane takes when no chat is selected at all. Shown on the
    /// filtered list destinations while the selection sits
    /// outside the filter; the selection is left untouched, so
    /// it never opens, marks read, or disturbs the foreign chat.
    /// In single-pane mode a back chevron at the top returns to
    /// the filtered list, mirroring the conversation header.
    fn draw_conversation_placeholder(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(rgb(t.bg_chat))).show(
            root,
            |ui| {
                if self.single_pane {
                    ui.horizontal(|ui| {
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
                            self.conversation_open = false;
                        }
                    });
                }
                ui.vertical_centered(|ui| {
                    ui.add_space((ui.available_height() * 0.36).max(0.0));
                    let (icon_rect, _) = ui.allocate_exact_size(Vec2::splat(48.0), Sense::hover());
                    crate::icons::draw(
                        ui.painter(),
                        crate::icons::Icon::Chat,
                        icon_rect.center(),
                        40.0,
                        rgb(t.text_faint),
                        rgb(t.bg_chat),
                    );
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(crate::i18n::t("conversation.select_chat"))
                            .size(14.5)
                            .color(rgb(t.text_secondary)),
                    );
                });
            },
        );
    }

    /// The "My profile" editor (account menu entry): drawn while
    /// open, its intents applied here. What Save and the photo
    /// picker do depends on the editor's mode (demo, live,
    /// not-linked); see `App::save_my_profile` and
    /// `App::pick_my_profile_photo` in live.rs. `remove_photo`
    /// needs nothing here: the state already reflects the
    /// removal, and it takes effect with the next save.
    fn draw_my_profile(&mut self, ctx: &egui::Context) {
        let mode = self.my_profile_mode();
        let intents = crate::profile::draw_my_profile_panel(
            ctx,
            self.tokens(),
            &mut self.my_profile,
            self.my_profile_open,
            self.motion,
            mode,
        );
        if intents.pick_photo {
            self.pick_my_profile_photo();
        }
        if intents.save {
            self.save_my_profile();
        }
        if intents.close {
            self.my_profile_open = false;
        }
    }
}

impl App {
    // ------------------------------------------------------------- rail
    pub(crate) fn draw_rail(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        let unread = total_unread(&self.chats);
        let archived_unread = core::filter::archived_unread_count(&self.chats);
        egui::Panel::left("rail")
            .exact_size(60.0)
            .frame(egui::Frame::new().fill(rgb(t.bg_panel)))
            .show(root, |ui| {
                ui.vertical_centered(|ui| {
                    // No app logo, the rail starts straight with navigation.
                    ui.add_space(14.0);
                    let rail_labels = [
                        (Screen::Chats, crate::icons::Icon::Chat, crate::i18n::t("nav.chats")),
                        (Screen::Calls, crate::icons::Icon::Phone, crate::i18n::t("nav.calls")),
                        (
                            Screen::Channels,
                            crate::icons::Icon::Megaphone,
                            crate::i18n::t("nav.channels"),
                        ),
                        (Screen::Groups, crate::icons::Icon::Users, crate::i18n::t("nav.groups")),
                        (
                            Screen::Communities,
                            crate::icons::Icon::Group,
                            crate::i18n::t("nav.communities"),
                        ),
                        (Screen::Starred, crate::icons::Icon::Star, crate::i18n::t("nav.starred")),
                        (
                            Screen::Archived,
                            crate::icons::Icon::Archive,
                            crate::i18n::t("nav.archived"),
                        ),
                    ];
                    for (screen, icon, label) in &rail_labels {
                        let screen = *screen;
                        // One badge language: a numeric pill
                        // wherever a count is known (Chats carries
                        // the total unread, Archived its own unread
                        // count), no marker anywhere else.
                        let count = match screen {
                            Screen::Chats => unread,
                            Screen::Archived => archived_unread,
                            _ => 0,
                        };
                        if rail_button(ui, *icon, label, self.screen == screen, count, t) {
                            self.screen = screen;
                            // Rail navigation lands on the new
                            // screen's list: in single-pane mode
                            // an open conversation closes back to
                            // it (the two-pane layout ignores this).
                            self.conversation_open = false;
                            self.filter = screen.filter();
                            self.open_community = None;
                            self.community_home_from_overview = false;
                            self.community_origin = None;
                            // Entering the Archived view only
                            // switches the list filter above: the
                            // open conversation is left exactly as
                            // it was (owner's ruling; the selection
                            // sync an earlier version ran here was
                            // removed on purpose).
                        }
                        ui.add_space(6.0);
                    }
                    ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
                        ui.add_space(12.0);
                        // Profile lives at the very bottom, like a desktop client should.
                        {
                            let (rect, resp) =
                                ui.allocate_exact_size(Vec2::splat(38.0), Sense::click());
                            self.profile_anchor = Some(rect.center());
                            crate::profile::paint_my_avatar_disc(
                                ui.painter(),
                                rect.center(),
                                19.0,
                                &self.my_profile,
                                t,
                            );
                            if resp.clicked() {
                                self.account_menu_open = !self.account_menu_open;
                                if self.account_menu_open {
                                    // Record the opening frame:
                                    // the account menu is drawn
                                    // later in this same frame and
                                    // its outside-click check must
                                    // not read this click as a
                                    // dismissal.
                                    self.account_menu_open_frame =
                                        Some(ui.ctx().cumulative_frame_nr());
                                }
                            }
                            resp.on_hover_text("Accounts");
                        }
                        ui.add_space(6.0);
                        let settings_label = crate::i18n::t("nav.settings");
                        if rail_button(
                            ui,
                            crate::icons::Icon::Sliders,
                            &settings_label,
                            self.screen == Screen::Settings,
                            0,
                            t,
                        ) {
                            self.screen = Screen::Settings;
                        }
                        ui.add_space(6.0);
                    });
                });
            });
    }

    pub(crate) fn draw_collapsed_strip(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        let panel = egui::Panel::left("collapsed_strip")
            .exact_size(42.0)
            .frame(egui::Frame::new().fill(rgb(t.bg_panel)).inner_margin(Margin::symmetric(4, 8)))
            .show(root, |ui| {
                ui.vertical_centered(|ui| {
                    if crate::widgets::icon_button(
                        ui,
                        t,
                        crate::icons::Icon::ChevronRight,
                        30.0,
                        "Expand chat list (Ctrl+B)",
                        None,
                    )
                    .clicked()
                    {
                        self.sidebar_collapsed = false;
                    }
                    ui.add_space(10.0);
                    // The list marker under the expand button:
                    // the bundled menu glyph, not a text
                    // hamburger character.
                    let (mrect, _) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
                    crate::icons::draw(
                        ui.painter(),
                        crate::icons::Icon::Menu,
                        mrect.center(),
                        16.0,
                        rgb(t.text_secondary),
                        rgb(t.bg_panel),
                    );
                });
            });
        crate::widgets::soft_right_edge(root, panel.response.rect, t);
    }
}

impl App {
    /// Full-screen photo viewer. Regular photos open freely; a
    /// view-once photo consumes its allowance on close, and only
    /// when its real pixels were actually shown during the
    /// viewing: the bubble then collapses to the "Opened" receipt
    /// and can never be viewed again, like the real thing. A
    /// view-once photo whose bytes never arrived keeps its
    /// allowance (see `App::close_photo_viewer`).
    pub(crate) fn draw_photo_viewer(&mut self, root: &mut egui::Ui) {
        // Snapshot the photo id while open: the exit animation
        // outlives the state, like the other popups.
        if let Some(id) = self.media.viewing_photo {
            root.ctx().data_mut(|d| d.insert_temp(egui::Id::unique("photo_snap"), id));
        }
        let Some(view_p) = crate::anim::popup_phase(
            root.ctx(),
            "photo_viewer",
            self.media.viewing_photo.is_some(),
            self.motion,
        ) else {
            return;
        };
        let exiting = self.media.viewing_photo.is_none();
        let Some(id) = root.ctx().data(|d| d.get_temp::<u64>(egui::Id::unique("photo_snap")))
        else {
            return;
        };
        let t = self.tokens();
        // The reveal run restarts on every open: a short loading
        // phase, then the photo crossfades in. Closing stays manual
        // (Close button or Escape); nothing here auto-closes.
        let open_now = !exiting;
        let was_open: bool =
            root.ctx().data(|d| d.get_temp(egui::Id::unique("viewer_was_open"))).unwrap_or(false);
        if open_now && !was_open {
            self.anim.viewer_reveal =
                Some(self.fresh_anim(self.motion.duration(700.0), root.ctx()));
        }
        root.ctx().data_mut(|d| d.insert_temp(egui::Id::unique("viewer_was_open"), open_now));
        let now = root.ctx().input(|i| i.time);
        let reveal_p = crate::motion::Anim::drive(&mut self.anim.viewer_reveal, root.ctx(), now);
        let (caption, view_once, is_photo, video_secs) = self
            .conversation
            .iter()
            .find(|m| m.id == id)
            .map(|m| match &m.kind {
                core::model::MessageKind::Image { caption, view_once } => {
                    (caption.clone(), *view_once, true, 0)
                }
                core::model::MessageKind::Video { caption, view_once, secs } => {
                    (caption.clone(), *view_once, false, *secs)
                }
                _ => (String::new(), false, false, 0),
            })
            .unwrap_or_default();
        // Opening the viewer on a photo is what starts (or
        // retries) its display fetch: bubbles fetch regular
        // photos on sight, but a view-once photo is fetched only
        // once the user actually opens it.
        if open_now && !was_open && is_photo {
            match self.photo_tile(id) {
                crate::media::PhotoTile::Idle => self.ensure_photo_fetch(id),
                crate::media::PhotoTile::Failed => self.retry_photo_fetch(id),
                _ => {}
            }
        }
        let tile = self.photo_tile(id);
        let paintable =
            matches!(tile, crate::media::PhotoTile::Real | crate::media::PhotoTile::Demo);
        // 0.0-0.55: loading only. 0.55-1.0: the photo crossfades in
        // with a slight scale-up. The reveal completes only when
        // there is something true to reveal: while a live photo is
        // still being fetched the loading phase holds, and a
        // failed fetch never reveals a substitute image.
        let photo_a =
            if paintable || exiting { ((reveal_p - 0.55) / 0.45).clamp(0.0, 1.0) } else { 0.0 };
        let spinner_a = if exiting || tile == crate::media::PhotoTile::Failed {
            0.0
        } else if paintable {
            (1.0 - photo_a * 1.6).clamp(0.0, 1.0)
        } else {
            1.0
        };
        if tile == crate::media::PhotoTile::Demo && self.media.photo_texture.is_none() {
            self.media.photo_texture = Some(crate::media::demo_photo(root.ctx()));
        }
        let header = if is_photo {
            if view_once {
                crate::i18n::t("viewer.view_once")
            } else {
                crate::i18n::t("viewer.photo")
            }
        } else if view_once {
            crate::i18n::t("viewer.view_once_video")
        } else {
            crate::i18n::t("viewer.video")
        };
        let screen_rect = root.ctx().content_rect();
        let mut close = !exiting && root.ctx().input(|i| i.key_pressed(egui::Key::Escape));
        egui::Area::new(egui::Id::unique("photo_viewer"))
            .fixed_pos(screen_rect.min)
            .order(egui::Order::Foreground)
            .show(root.ctx(), |ui| {
                if view_p < 0.999 {
                    ui.set_opacity(view_p);
                }
                let (rect, _) = ui.allocate_exact_size(screen_rect.size(), egui::Sense::hover());
                ui.painter().rect_filled(
                    rect,
                    egui::CornerRadius::ZERO,
                    egui::Color32::from_rgba_unmultiplied(8, 10, 12, 242),
                );
                // Videos paint through their own body (the real
                // thumbnail, real decoded frames, honest states);
                // photos keep the reveal sequence below.
                let mut video_a = 0.0;
                if !is_photo {
                    video_a = self.draw_video_body(ui, rect, id, view_once, video_secs, exiting);
                }
                // The loading phase: a small spinner where the
                // photo will sit, fading out as the photo reveals.
                if is_photo && spinner_a > 0.001 {
                    ui.set_opacity(spinner_a * view_p);
                    let spinner_rect = egui::Rect::from_center_size(
                        egui::Pos2::new(rect.center().x, rect.center().y - 24.0),
                        egui::Vec2::splat(40.0),
                    );
                    ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(spinner_rect)
                            .layout(egui::Layout::top_down(egui::Align::Center)),
                    )
                    .add(egui::Spinner::new().size(30.0));
                    ui.set_opacity(view_p);
                }
                // The photo's own pixels: the decoded real bytes,
                // or the demo fixture where that is the disclosed
                // content. A live photo whose bytes are not here
                // paints nothing in this block; the loading phase
                // or the failure note speaks for it instead.
                if is_photo && photo_a > 0.001 {
                    ui.set_opacity(photo_a);
                    let tex = self.media.textures.get(&id).cloned().or_else(|| {
                        if tile == crate::media::PhotoTile::Demo {
                            self.media.photo_texture.clone()
                        } else {
                            None
                        }
                    });
                    if let Some(tex) = &tex {
                        if view_once && tile == crate::media::PhotoTile::Real {
                            // The real pixels are on screen: this
                            // viewing is what spends the single
                            // view, recorded for the close path.
                            self.media.view_once_shown = Some(id);
                        }
                        let max_w = (rect.width() - 120.0).min(620.0);
                        let scale = 0.96 + 0.04 * photo_a;
                        let size = egui::Vec2::new(max_w * scale, max_w * scale * 400.0 / 600.0);
                        let img_rect = egui::Rect::from_center_size(
                            egui::Pos2::new(rect.center().x, rect.center().y - 24.0),
                            size,
                        );
                        ui.painter().image(
                            tex.id(),
                            img_rect,
                            egui::Rect::from_min_max(
                                egui::Pos2::new(0.0, 0.0),
                                egui::Pos2::new(1.0, 1.0),
                            ),
                            egui::Color32::WHITE,
                        );
                    }
                    ui.set_opacity(view_p);
                }
                // A failed fetch says so, in place of any image.
                if is_photo && !exiting && tile == crate::media::PhotoTile::Failed {
                    ui.painter().text(
                        egui::Pos2::new(rect.center().x, rect.center().y - 24.0),
                        egui::Align2::CENTER_CENTER,
                        crate::i18n::t("viewer.photo_failed"),
                        egui::FontId::proportional(14.0),
                        egui::Color32::from_rgb(200, 208, 214),
                    );
                }
                // The caption reveals with the content, not
                // before it.
                let content_a = if is_photo { photo_a } else { video_a };
                if content_a > 0.001 {
                    let cap_y =
                        rect.center().y + (rect.width().min(740.0) * 400.0 / 600.0) / 2.0 + 2.0;
                    ui.painter().text(
                        egui::Pos2::new(rect.center().x, cap_y),
                        egui::Align2::CENTER_TOP,
                        caption,
                        egui::FontId::proportional(14.0),
                        egui::Color32::from_rgba_unmultiplied(
                            230,
                            235,
                            240,
                            (255.0 * content_a) as u8,
                        ),
                    );
                }
                ui.painter().text(
                    egui::Pos2::new(rect.center().x, rect.top() + 18.0),
                    egui::Align2::CENTER_TOP,
                    header,
                    egui::FontId::proportional(12.0),
                    egui::Color32::from_rgb(160, 170, 178),
                );
                // Close button (bottom centre). The overlay is
                // painted in absolute coordinates, so this stays a
                // hand-placed pill, but it now steps its fill on
                // hover and press and takes the pointing hand like
                // the shared controls do.
                let btn_rect = egui::Rect::from_center_size(
                    egui::Pos2::new(rect.center().x, rect.bottom() - 44.0),
                    egui::Vec2::new(96.0, 34.0),
                );
                let resp =
                    ui.interact(btn_rect, egui::Id::unique("viewer_close"), egui::Sense::click());
                let fill = if resp.is_pointer_button_down_on() {
                    rgb(core::theme::pressed_step(t.accent))
                } else if resp.hovered() {
                    rgb(core::theme::hover_step(t.accent))
                } else {
                    rgb(t.accent)
                };
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                ui.painter().rect_filled(btn_rect, egui::CornerRadius::same(17), fill);
                crate::icons::draw(
                    ui.painter(),
                    crate::icons::Icon::X,
                    btn_rect.center() + egui::Vec2::new(-24.0, 0.0),
                    16.0,
                    egui::Color32::WHITE,
                    fill,
                );
                ui.painter().text(
                    btn_rect.center() + egui::Vec2::new(8.0, 0.0),
                    egui::Align2::CENTER_CENTER,
                    "Close",
                    egui::FontId::proportional(14.0),
                    egui::Color32::from_rgb(234, 238, 241),
                );
                if resp.clicked() {
                    close = true;
                }
            });
        if close && !exiting {
            // Only a view-once photo consumes its allowance, and
            // only when its real pixels were actually shown;
            // regular photos reopen freely. The decision and the
            // cleanup live in close_photo_viewer.
            self.close_photo_viewer(id, view_once);
        }
    }

    /// The video half of the media viewer. The video's own
    /// thumbnail is its poster; once its bytes are here the real
    /// player presents decoded frames with a working play/pause
    /// and seek bar; while the bytes are missing the states are
    /// honest (loading, failed with a retry, the test-mode note
    /// in demo mode). No substitute image is ever painted.
    /// Returns 1.0 when real content (the thumbnail or a decoded
    /// frame) is on screen, else 0.0; presenting real content of
    /// a view-once video is recorded here for the close path's
    /// spend decision.
    fn draw_video_body(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        id: u64,
        view_once: bool,
        secs: u32,
        exiting: bool,
    ) -> f32 {
        let t = self.tokens();
        let route = self.video_play_route(id);
        // Opening the viewer is what starts the byte fetch, and
        // landed bytes start the player (paused on its first
        // frame).
        if !exiting {
            match route {
                crate::media::VideoPlayRoute::Idle => self.ensure_video_fetch(id, view_once),
                crate::media::VideoPlayRoute::Ready => self.start_video_player(id, secs),
                _ => {}
            }
        }
        // The poster: the video's own thumbnail.
        let thumb = Self::video_thumb_texture(&mut self.media, self.live.as_ref(), ui.ctx(), id);
        let thumb_aspect = self
            .media
            .video_textures
            .get(&id)
            .map(|h| h.aspect_ratio())
            .filter(|a| a.is_finite() && *a > 0.05);
        // Pump the player and upload its presented frame.
        let mut playing = false;
        let mut position = 0.0;
        let mut duration = f64::from(secs);
        let mut player_failed: Option<String> = None;
        let mut audio_note: Option<String> = None;
        let mut has_player = false;
        if let Some(player) = self.media.video_player.as_mut().filter(|p| p.key == id) {
            has_player = true;
            player.pump();
            if let Some(frame) = &player.current {
                let key = (id, frame.pts.to_bits());
                if self.media.video_frame_key != Some(key) {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [frame.width as usize, frame.height as usize],
                        &frame.rgba,
                    );
                    match &mut self.media.video_frame {
                        Some(handle) => handle.set(image, egui::TextureOptions::LINEAR),
                        None => {
                            self.media.video_frame = Some(ui.ctx().load_texture(
                                "video-frame",
                                image,
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                    }
                    self.media.video_frame_key = Some(key);
                }
            }
            playing = player.is_playing();
            position = player.position();
            duration = player.info.duration_secs;
            player_failed = player.failed.clone();
            audio_note = player.audio_note.clone();
            if player.current.is_some() && view_once {
                self.media.video_presented = Some(id);
            }
            if playing {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(16));
            } else if player.current.is_none() && player_failed.is_none() {
                // The first frame is still decoding.
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(30));
            }
        }
        let frame_aspect = self
            .media
            .video_player
            .as_ref()
            .filter(|p| p.key == id)
            .and_then(|p| p.current.as_ref())
            .map(|f| f.width as f32 / f.height.max(1) as f32)
            .filter(|a| a.is_finite() && *a > 0.05);
        let aspect = frame_aspect.or(thumb_aspect).unwrap_or(600.0 / 400.0);
        let max_w = (rect.width() - 120.0).min(620.0);
        let frame_rect = egui::Rect::from_center_size(
            egui::Pos2::new(rect.center().x, rect.center().y - 24.0),
            egui::Vec2::new(max_w, max_w / aspect),
        );
        let painter = ui.painter().clone();
        painter.rect_filled(frame_rect, egui::CornerRadius::same(6), rgb(t.bg_panel_alt));
        let mut content_a = 0.0;
        // A decoded frame wins; the thumbnail is the poster
        // until (and unless) the player presents.
        let frame_tex = if self.media.video_frame_key.is_some_and(|(mid, _)| mid == id) {
            self.media.video_frame.as_ref().map(egui::TextureHandle::id)
        } else {
            None
        };
        if let Some(tex) = frame_tex {
            painter.image(
                tex,
                frame_rect,
                egui::Rect::from_min_max(egui::Pos2::new(0.0, 0.0), egui::Pos2::new(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            content_a = 1.0;
        } else if let Some(tex) = thumb {
            painter.image(
                tex,
                frame_rect,
                egui::Rect::from_min_max(egui::Pos2::new(0.0, 0.0), egui::Pos2::new(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            content_a = 1.0;
            if view_once {
                self.media.video_presented = Some(id);
            }
        } else {
            // The neutral tile: a video glyph on the panel, the
            // same presentation as the bubble without a
            // thumbnail.
            crate::icons::draw(
                &painter,
                crate::icons::Icon::Video,
                frame_rect.center(),
                40.0,
                rgb(t.text_faint),
                rgb(t.bg_panel_alt),
            );
        }
        let note_color = egui::Color32::from_rgb(200, 208, 214);
        if let Some(message) = &player_failed {
            // A decode or read failure, in the player's own
            // user-facing words.
            painter.text(
                frame_rect.center() + egui::Vec2::new(0.0, 44.0),
                egui::Align2::CENTER_CENTER,
                message,
                egui::FontId::proportional(13.0),
                note_color,
            );
        } else if has_player {
            // Transport: a play/pause disc over the frame, and a
            // seek bar with real positions under it.
            let disc = egui::Rect::from_center_size(frame_rect.center(), egui::Vec2::splat(56.0));
            let resp = ui
                .interact(disc, egui::Id::unique("video_toggle"), egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            painter.circle_filled(frame_rect.center(), 28.0, rgb(t.accent));
            crate::icons::draw(
                &painter,
                if playing { crate::icons::Icon::Pause } else { crate::icons::Icon::Play },
                frame_rect.center(),
                26.0,
                egui::Color32::WHITE,
                rgb(t.accent),
            );
            if resp.clicked() && !exiting {
                if let Some(player) = self.media.video_player.as_mut().filter(|p| p.key == id) {
                    player.toggle();
                }
            }
            // Seek bar: the filled part is the presented
            // position over the real duration; clicking or
            // dragging seeks there.
            let bar = egui::Rect::from_min_size(
                egui::Pos2::new(frame_rect.left(), frame_rect.bottom() + 14.0),
                egui::Vec2::new(frame_rect.width(), 4.0),
            );
            painter.rect_filled(
                bar,
                egui::CornerRadius::same(2),
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 60),
            );
            if duration > 0.0 {
                let frac = (position / duration).clamp(0.0, 1.0) as f32;
                let filled =
                    egui::Rect::from_min_size(bar.min, egui::Vec2::new(bar.width() * frac, 4.0));
                painter.rect_filled(filled, egui::CornerRadius::same(2), rgb(t.accent));
            }
            let bar_resp = ui
                .interact(
                    bar.expand2(egui::Vec2::new(0.0, 6.0)),
                    egui::Id::unique("video_seek"),
                    egui::Sense::click_and_drag(),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if !exiting && (bar_resp.clicked() || bar_resp.dragged()) {
                if let Some(pos) = bar_resp.interact_pointer_pos() {
                    let frac = ((pos.x - bar.left()) / bar.width()).clamp(0.0, 1.0);
                    if let Some(player) = self.media.video_player.as_mut().filter(|p| p.key == id) {
                        player.seek(f64::from(frac) * duration);
                    }
                }
            }
            painter.text(
                egui::Pos2::new(bar.left(), bar.bottom() + 8.0),
                egui::Align2::LEFT_TOP,
                format!("{} / {}", video_clock(position), video_clock(duration)),
                egui::FontId::proportional(11.5),
                note_color,
            );
            if let Some(note) = &audio_note {
                // The sound situation, stated plainly under the
                // transport: a sound track that exists but is
                // not playing is never left as unexplained
                // silence.
                painter.text(
                    egui::Pos2::new(bar.left(), bar.bottom() + 26.0),
                    egui::Align2::LEFT_TOP,
                    note,
                    egui::FontId::proportional(11.5),
                    note_color,
                );
            }
        } else {
            match route {
                crate::media::VideoPlayRoute::DemoUnavailable => {
                    painter.text(
                        frame_rect.center() + egui::Vec2::new(0.0, 44.0),
                        egui::Align2::CENTER_CENTER,
                        crate::i18n::t("video.demo_unavailable"),
                        egui::FontId::proportional(13.0),
                        note_color,
                    );
                }
                crate::media::VideoPlayRoute::Failed => {
                    painter.text(
                        frame_rect.center() + egui::Vec2::new(0.0, 44.0),
                        egui::Align2::CENTER_CENTER,
                        crate::i18n::t("viewer.video_failed"),
                        egui::FontId::proportional(13.0),
                        note_color,
                    );
                    // Clicking the tile retries the fetch.
                    let resp = ui
                        .interact(frame_rect, egui::Id::unique("video_retry"), egui::Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.clicked() && !exiting {
                        self.retry_video_fetch(id, view_once);
                    }
                }
                crate::media::VideoPlayRoute::Loading | crate::media::VideoPlayRoute::Idle => {
                    // The bytes are on their way: a spinner over
                    // the poster, and a repaint tick so it lands
                    // without waiting for input.
                    let spinner_rect =
                        egui::Rect::from_center_size(frame_rect.center(), egui::Vec2::splat(36.0));
                    ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(spinner_rect)
                            .layout(egui::Layout::top_down(egui::Align::Center)),
                    )
                    .add(egui::Spinner::new().size(28.0));
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(60));
                }
                crate::media::VideoPlayRoute::Ready => {}
            }
            // The video's facts, under the poster: duration and,
            // once the bytes are here, their size.
            let size_bytes =
                self.media.video_bytes.get(&id).map(|b| b.len() as u64).or_else(|| {
                    self.media
                        .video_files
                        .get(&id)
                        .and_then(|p| std::fs::metadata(p).ok())
                        .map(|m| m.len())
                });
            let facts = match size_bytes {
                Some(bytes) => {
                    format!("{}  ·  {}", video_clock(f64::from(secs)), video_size(bytes))
                }
                None => video_clock(f64::from(secs)),
            };
            painter.text(
                egui::Pos2::new(frame_rect.center().x, frame_rect.bottom() + 12.0),
                egui::Align2::CENTER_TOP,
                facts,
                egui::FontId::proportional(11.5),
                note_color,
            );
        }
        content_a
    }
}

/// A playback clock reading: `m:ss` under an hour, `h:mm:ss`
/// above it. Pure formatting for the viewer's transport line.
fn video_clock(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    if total >= 3600 {
        format!("{}:{:02}:{:02}", total / 3600, (total % 3600) / 60, total % 60)
    } else {
        format!("{}:{:02}", total / 60, total % 60)
    }
}

/// A byte count in the viewer's facts line.
fn video_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Args;

    /// A demo-mode app with popup motion off, at the default
    /// window size the real binary opens with (1180x760, so the
    /// shell is in its two-pane arrangement with the rail).
    fn shell_app(ctx: &egui::Context) -> App {
        crate::theme::install_fonts(ctx);
        let mut app = App::new(Args::parse(), ctx);
        // App::new may have attempted a real live start against
        // this machine's data dir; the test owns the state from
        // here and wants the plain demo shell.
        app.live = None;
        app.live_mode = false;
        app.screen = crate::state::Screen::Chats;
        app.motion = crate::motion::MotionSetting::Off;
        app.account_menu_open = false;
        app
    }

    /// One real frame through the shell's root draw entry
    /// (`eframe::App::ui`), with synthetic input.
    fn run_frame(app: &mut App, ctx: &egui::Context, events: Vec<egui::Event>, time: f64) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1180.0, 760.0),
            )),
            time: Some(time),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| {
            let mut frame = eframe::Frame::_new_kittest();
            <App as eframe::App>::ui(app, ui, &mut frame);
        });
        out.textures_delta.clear();
    }

    fn run_empty(app: &mut App, ctx: &egui::Context, frames: usize, time: &mut f64) {
        for _ in 0..frames {
            *time += 0.05;
            run_frame(app, ctx, Vec::new(), *time);
        }
    }

    /// A full click (move, press in one frame, release in the
    /// next) at `at`, the same split the context-menu tests use:
    /// egui reports `clicked()` on the release frame.
    fn click_at(app: &mut App, ctx: &egui::Context, at: egui::Pos2, time: &mut f64) {
        *time += 0.05;
        run_frame(
            app,
            ctx,
            vec![
                egui::Event::PointerMoved(at),
                egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            *time,
        );
        *time += 0.05;
        run_frame(
            app,
            ctx,
            vec![egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            *time,
        );
    }

    /// Opens the account menu by clicking the rail avatar for
    /// real and returns the avatar position the rail recorded.
    fn open_via_avatar(app: &mut App, ctx: &egui::Context, time: &mut f64) -> egui::Pos2 {
        run_empty(app, ctx, 3, time);
        let avatar = app.profile_anchor.expect("the rail records the avatar centre");
        click_at(app, ctx, avatar, time);
        avatar
    }

    /// Board item 132: clicking the rail avatar opened the
    /// account menu and closed it again in the same frame,
    /// because the rail toggle runs before `draw_account_menu`
    /// in the frame and the menu's outside-click check read
    /// that same opening click (the avatar sits outside the
    /// centred modal) as a dismissal.
    #[test]
    fn avatar_click_opens_account_menu_and_it_stays_open() {
        let ctx = egui::Context::default();
        let mut app = shell_app(&ctx);
        let mut time = 0.0;
        open_via_avatar(&mut app, &ctx, &mut time);
        assert!(app.account_menu_open, "the avatar click opens the account menu");
        run_empty(&mut app, &ctx, 3, &mut time);
        assert!(app.account_menu_open, "the account menu stays open on later frames");
    }

    /// A genuine outside click (well away from both the avatar
    /// and the centred modal) still dismisses the menu.
    #[test]
    fn outside_click_closes_the_account_menu() {
        let ctx = egui::Context::default();
        let mut app = shell_app(&ctx);
        let mut time = 0.0;
        open_via_avatar(&mut app, &ctx, &mut time);
        assert!(app.account_menu_open, "precondition: the menu is open");
        click_at(&mut app, &ctx, egui::pos2(1000.0, 650.0), &mut time);
        assert!(!app.account_menu_open, "an outside click closes the account menu");
    }

    /// Escape still dismisses the menu.
    #[test]
    fn escape_closes_the_account_menu() {
        let ctx = egui::Context::default();
        let mut app = shell_app(&ctx);
        let mut time = 0.0;
        open_via_avatar(&mut app, &ctx, &mut time);
        assert!(app.account_menu_open, "precondition: the menu is open");
        time += 0.05;
        run_frame(
            &mut app,
            &ctx,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: Some(egui::Key::Escape),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            time,
        );
        assert!(!app.account_menu_open, "Escape closes the account menu");
    }

    /// On a filtered destination the conversation pane shows
    /// the placeholder while the selection is outside the
    /// filter: frames run, the selection is never moved, and a
    /// member selection takes the conversation back.
    #[test]
    fn filtered_screen_keeps_a_foreign_selection_untouched() {
        let ctx = egui::Context::default();
        let mut app = shell_app(&ctx);
        // The membership assertions below are about the demo
        // dataset (chat 1 a group, chat 5 a channel); the shell
        // fixture's own chat list depends on the machine's data
        // dir, so load the dataset explicitly.
        app.chats = core::model::demo_chats();
        app.screen = Screen::Channels;
        app.filter = core::filter::ChatFilter::Channels;
        // The shell app starts on chat 1, a group: foreign here.
        assert_eq!(app.selected_chat, 1);
        assert!(!app.selection_in_current_filter());
        let mut time = 0.0;
        run_empty(&mut app, &ctx, 3, &mut time);
        assert_eq!(app.selected_chat, 1, "the placeholder never moves the selection");
        app.selected_chat = 5;
        assert!(app.selection_in_current_filter(), "chat 5 is a channel");
        run_empty(&mut app, &ctx, 3, &mut time);
        assert_eq!(app.selected_chat, 5);
    }

    /// Clicking the avatar again toggles the menu closed.
    #[test]
    fn avatar_click_again_toggles_the_account_menu_closed() {
        let ctx = egui::Context::default();
        let mut app = shell_app(&ctx);
        let mut time = 0.0;
        let avatar = open_via_avatar(&mut app, &ctx, &mut time);
        assert!(app.account_menu_open, "precondition: the menu is open");
        click_at(&mut app, &ctx, avatar, &mut time);
        assert!(!app.account_menu_open, "a second avatar click toggles the menu closed");
    }
}
