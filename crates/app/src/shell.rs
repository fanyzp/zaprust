//! App shell: the root frame dispatch, the navigation rail, and the
//! collapsed-sidebar strip.

use crate::state::{layout_mode_for_width, App, LayoutMode, Screen};
use crate::theme::{apply_visuals, rgb};
use crate::widgets::rail_button;
use core::filter::total_unread;
use eframe::egui::{self, Align, Layout, Margin, Sense, Vec2};

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::B)) {
            self.sidebar_collapsed = !self.sidebar_collapsed;
        }
        apply_visuals(&ctx, self.tokens);
        ctx.set_pixels_per_point(self.zoom_percent as f32 / 100.0);
        self.poll_backend(&ctx);
        self.poll_workers(&ctx);
        self.poll_update(&ctx);
        self.drain_os_notifications(&ctx);
        self.tick_expiry();

        // The layout mode follows the window width: below the
        // breakpoint the panes take turns (single-pane), above
        // it they share the width (two-pane). Refreshed every
        // frame so a resize switches behavior immediately.
        self.single_pane =
            layout_mode_for_width(ctx.content_rect().width()) == LayoutMode::SinglePane;
        if self.debug.mode {
            self.draw_debug_banner(ui);
        }
        if self.screen == Screen::Connect {
            self.draw_connect(ui);
            self.draw_banners(ui);
            return;
        }
        self.draw_rail(ui);
        if self.sidebar_collapsed && self.screen.is_list_screen() {
            self.draw_collapsed_strip(ui);
            self.draw_content(ui);
            return;
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
        } else if self.screen == Screen::Settings {
            // Settings fills the content area: nav and page side
            // by side in two-pane mode, one at a time in
            // single-pane mode (see draw_settings).
            self.draw_settings(ui);
        } else if self.screen == Screen::Debug {
            self.draw_simple_list_panel(ui, "Debug");
            self.draw_debug_screen(ui);
        }
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
    /// in single-pane mode, a panel spanning everything right
    /// of the rail, since the list is then the only pane.
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
    /// conversation.
    fn draw_content(&mut self, root: &mut egui::Ui) {
        if self.open_community.is_some() {
            self.draw_community_home(root);
        } else {
            self.draw_conversation(root);
        }
    }

    /// The "My profile" editor (account menu entry): drawn while
    /// open, its intents applied here. Save persists the identity
    /// keys through the settings write path (extras.rs) and
    /// confirms with the saved notice. `pick_photo` is a no-op
    /// for now: there is no file-dialog dependency in the
    /// workspace, so the panel's photo-path field is the picker.
    /// `remove_photo` needs nothing here either: the state is
    /// already cleared and is written on the next save.
    fn draw_my_profile(&mut self, ctx: &egui::Context) {
        let intents = crate::profile::draw_my_profile_panel(
            ctx,
            self.tokens(),
            &mut self.my_profile,
            self.my_profile_open,
            self.motion,
        );
        if intents.save {
            self.persist_settings();
            self.notice = Some(crate::i18n::t("profile.my_saved"));
            self.my_profile_open = false;
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
                        let badge =
                            screen == Screen::Chats && unread > 0 && self.screen != Screen::Chats;
                        let count = if screen == Screen::Archived { archived_unread } else { 0 };
                        if rail_button(ui, *icon, label, self.screen == screen, badge, count, t) {
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
                            crate::widgets::paint_avatar_disc(
                                ui.painter(),
                                rect.center(),
                                19.0,
                                false,
                                rgb(t.accent),
                            );
                            if resp.clicked() {
                                self.account_menu_open = !self.account_menu_open;
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
                            false,
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
    /// view-once photo consumes its allowance on close: the bubble
    /// collapses to the "Opened" receipt and can never be viewed
    /// again, like the real thing.
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
        // 0.0-0.55: loading only. 0.55-1.0: the photo crossfades in
        // with a slight scale-up.
        let photo_a = ((reveal_p - 0.55) / 0.45).clamp(0.0, 1.0);
        let spinner_a = (1.0 - photo_a * 1.6).clamp(0.0, 1.0);
        if self.media.photo_texture.is_none() {
            self.media.photo_texture = Some(crate::media::demo_photo(root.ctx()));
        }
        let (caption, view_once) = self
            .conversation
            .iter()
            .find(|m| m.id == id)
            .map(|m| match &m.kind {
                core::model::MessageKind::Image { caption, view_once } => {
                    (caption.clone(), *view_once)
                }
                core::model::MessageKind::Video { caption, view_once, .. } => {
                    (caption.clone(), *view_once)
                }
                _ => (String::new(), false),
            })
            .unwrap_or_default();
        let header = if view_once {
            crate::i18n::t("viewer.view_once")
        } else {
            crate::i18n::t("viewer.photo")
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
                // The loading phase: a small spinner where the
                // photo will sit, fading out as the photo reveals.
                if spinner_a > 0.001 {
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
                // The message's own pixels when they exist (a photo
                // sent from disk), the demo photo otherwise. Hidden
                // while the loading phase runs, then scaled in.
                if photo_a > 0.001 {
                    ui.set_opacity(photo_a);
                    let tex = self
                        .media
                        .textures
                        .get(&id)
                        .cloned()
                        .or_else(|| self.media.photo_texture.clone());
                    if let Some(tex) = &tex {
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
                // The caption reveals with the photo, not before it.
                if photo_a > 0.001 {
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
                            (255.0 * photo_a) as u8,
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
            // Only a view-once photo consumes its allowance; regular
            // photos reopen freely. The opened state is persisted
            // with the message, so it survives a restart too.
            if view_once {
                self.mark_view_once_opened(id);
            }
            self.media.viewing_photo = None;
        }
    }
}
