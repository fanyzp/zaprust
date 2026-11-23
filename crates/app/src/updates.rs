//! Updates screen: the status surface. Its structure follows
//! the reference layout in every mode: the title, a My Status
//! row (my avatar with an add badge, the row title, and a
//! subtitle that reflects the real state), a "Recent updates"
//! section header, and the feed area under it.
//!
//! What is real here, and what is not, is dictated by the
//! pinned protocol library: it can POST a text status
//! (`BackendCommand::PostTextStatus`, answered by the
//! `StatusPosted` / `StatusPostFailed` events), but it has no
//! API to list or fetch other people's statuses. So the My
//! Status row carries a working post action, and the feed area
//! is an honest empty state, in live and demo mode alike: no
//! feed is fabricated, and no demo statuses are invented.
//!
//! Layout: on desktop the screen is a left list panel in the
//! Starred / Calls family (see `App::list_panel`), with the
//! conversation placeholder in the right pane. In single-pane
//! modes (portrait included) the same content fills the screen.

use crate::state::{App, Notice};
use crate::theme::rgb;
use eframe::egui::{self, Align2, CornerRadius, FontId, Margin, Pos2, RichText, Sense, Vec2};

/// Height of the My Status row: avatar disc plus two text lines.
const MY_STATUS_ROW_H: f32 = 64.0;

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
    /// full-screen page: title, My Status row, the Recent
    /// updates section header, and the feed's empty state.
    fn updates_content(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens();
        ui.label(
            RichText::new(crate::i18n::t("nav.updates"))
                .font(crate::theme::semibold(19.0))
                .color(rgb(t.text_primary)),
        );
        ui.add_space(12.0);
        // The My Status row: my avatar with the add badge, the
        // title, and the state subtitle. The whole row is the
        // composer's trigger.
        let mut open_composer = false;
        {
            let (rect, resp) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), MY_STATUS_ROW_H),
                Sense::click(),
            );
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
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                open_composer = true;
            }
        }
        if open_composer {
            self.updates.composer_open = true;
            self.updates.error = None;
        }
        ui.add_space(16.0);
        ui.label(
            RichText::new(crate::i18n::t("updates.recent"))
                .font(crate::theme::medium(12.0))
                .color(rgb(t.text_secondary)),
        );
        // The feed area: other people's statuses cannot be
        // fetched through the pinned library, so this is the
        // shared empty state, never a fabricated feed.
        crate::widgets::empty_state(ui, t, &crate::i18n::t("updates.empty"));
    }

    /// The My Status row's subtitle, the row's real state in
    /// one line: the add prompt before anything is posted, the
    /// posted text itself afterwards.
    pub(crate) fn my_status_subtitle(&self) -> String {
        match &self.updates.my_status {
            Some(text) => text.clone(),
            None => crate::i18n::t("updates.add_prompt"),
        }
    }

    /// The status composer's Post action. Returns nothing: the
    /// outcome lands in the state. In demo mode (no live
    /// session) the post is local state only, and the composer
    /// closes at once. With a session, the command goes to the
    /// backend with the account's direct-chat partners as the
    /// audience, the composer stays open in its posting state,
    /// and the result event settles it: `apply_status_posted`
    /// closes it, `apply_status_post_failed` keeps it open with
    /// the failure inside. Validation failures (empty text, no
    /// audience) refuse in the modal and send nothing.
    pub(crate) fn post_my_status(&mut self) {
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
            self.updates.posting = true;
            self.send_backend(backend::BackendCommand::PostTextStatus { text, recipients });
            return;
        }
        self.apply_status_posted(text);
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

    /// A status post landed (the backend's `StatusPosted`, or
    /// a demo-mode local post): the text becomes my status for
    /// the rest of the session, the composer closes, and the
    /// notice line confirms it, the app's existing feedback
    /// pattern for a completed action.
    pub(crate) fn apply_status_posted(&mut self, text: String) {
        self.updates.my_status = Some(text);
        self.updates.posting = false;
        self.updates.composer_open = false;
        self.updates.draft.clear();
        self.updates.error = None;
        self.notice = Some(Notice::info(crate::i18n::t("updates.posted")));
    }

    /// A live status post failed (the backend's
    /// `StatusPostFailed`): the previous status state is kept,
    /// and the failure is shown inside the still-open composer
    /// instead of being dressed up as a success. The message
    /// passes through the display scrubber first, like every
    /// backend failure the app renders.
    pub(crate) fn apply_status_post_failed(&mut self, message: String) {
        let message = core::crash::scrub_for_display(&message);
        tracing::warn!("status post failed: {message}");
        self.updates.posting = false;
        self.updates.error = Some(crate::live::human_error(&message));
    }

    /// The status composer modal: the house centered-modal
    /// pattern (the create-group dialog's shape), one text
    /// field and a Post action. Drawn from the shell so it
    /// floats over whichever layout the Updates screen took.
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
                        ui.add_space(6.0);
                        crate::widgets::text_field(
                            ui,
                            t,
                            &mut self.updates.draft,
                            &crate::i18n::t("updates.composer_hint"),
                            280.0,
                        );
                        if self.live.is_none() {
                            ui.label(
                                RichText::new(crate::i18n::t("updates.demo_note"))
                                    .size(10.0)
                                    .color(rgb(t.text_faint)),
                            );
                        }
                        if let Some(error) = &self.updates.error {
                            ui.add_space(4.0);
                            ui.label(RichText::new(error.clone()).size(12.0).color(rgb(t.danger)));
                        }
                        ui.add_space(8.0);
                        if self.updates.posting {
                            crate::widgets::loading_row(
                                ui,
                                &crate::i18n::t("updates.posting"),
                                14.0,
                                t.text_secondary,
                            );
                            ui.add_space(8.0);
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
        if post {
            self.post_my_status();
        }
        if close {
            self.updates.composer_open = false;
            self.updates.error = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Args, Screen};

    /// A demo-mode app with popup motion off, at the default
    /// window size the real binary opens with (1180x760, so the
    /// shell is in its two-pane arrangement with the rail). The
    /// shell test harness's fixture, restated here so this
    /// module's tests stand alone.
    fn updates_app(ctx: &egui::Context) -> App {
        crate::theme::install_fonts(ctx);
        let mut app = App::new(Args::parse(), ctx);
        // App::new may have attempted a real live start against
        // this machine's data dir; the test owns the state from
        // here and wants the plain demo shell.
        app.live = None;
        app.live_mode = false;
        app.screen = Screen::Updates;
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

    #[test]
    fn subtitle_follows_the_post_state() {
        let ctx = egui::Context::default();
        let mut app = updates_app(&ctx);
        assert_eq!(app.my_status_subtitle(), crate::i18n::t("updates.add_prompt"));
        app.updates.my_status = Some("Out for a walk".to_owned());
        assert_eq!(app.my_status_subtitle(), "Out for a walk");
    }

    #[test]
    fn demo_post_updates_my_status_and_survives_screen_switches() {
        let ctx = egui::Context::default();
        let mut app = updates_app(&ctx);
        let mut time = 0.0;
        // The composer modal opens over the Updates screen and
        // paints for real frames before the post goes through.
        app.updates.composer_open = true;
        app.updates.draft = "Out for a walk".to_owned();
        run_empty(&mut app, &ctx, 3, &mut time);
        assert!(app.updates.composer_open, "precondition: the composer is open");
        app.post_my_status();
        assert_eq!(app.updates.my_status.as_deref(), Some("Out for a walk"));
        assert!(!app.updates.composer_open, "a landed post closes the composer");
        assert!(app.updates.draft.is_empty(), "the draft is spent");
        assert_eq!(app.notice_str(), Some(crate::i18n::t("updates.posted").as_str()));
        // The posted state survives leaving the screen and
        // coming back: it is session state, not screen state.
        app.screen = Screen::Chats;
        run_empty(&mut app, &ctx, 2, &mut time);
        app.screen = Screen::Updates;
        run_empty(&mut app, &ctx, 2, &mut time);
        assert_eq!(app.updates.my_status.as_deref(), Some("Out for a walk"));
        assert_eq!(app.my_status_subtitle(), "Out for a walk");
    }

    #[test]
    fn empty_draft_is_refused_inside_the_composer() {
        let ctx = egui::Context::default();
        let mut app = updates_app(&ctx);
        app.updates.composer_open = true;
        app.updates.draft = "   ".to_owned();
        app.post_my_status();
        assert!(app.updates.my_status.is_none(), "nothing was posted");
        assert!(app.updates.composer_open, "the composer stays open");
        assert_eq!(
            app.updates.error.as_deref(),
            Some(crate::i18n::t("updates.empty_error").as_str())
        );
    }

    #[test]
    fn live_post_failure_lands_back_in_the_composer() {
        let ctx = egui::Context::default();
        let mut app = updates_app(&ctx);
        // The state a sent live post leaves behind: composer
        // open, posting in flight, draft still in the field.
        app.updates.composer_open = true;
        app.updates.draft = "Hello".to_owned();
        app.updates.posting = true;
        app.apply_status_post_failed("Posting the status failed: boom".to_owned());
        assert!(!app.updates.posting, "the flight is over");
        assert!(app.updates.composer_open, "the failure lands in the open composer");
        assert!(app.updates.my_status.is_none(), "a failed post changes no state");
        assert!(
            app.updates.error.as_deref().is_some_and(|e| e.contains("boom")),
            "the failure is shown, got {:?}",
            app.updates.error
        );
        // And the success event settles the same composer.
        app.updates.posting = true;
        app.apply_status_posted("Hello".to_owned());
        assert_eq!(app.updates.my_status.as_deref(), Some("Hello"));
        assert!(!app.updates.composer_open);
        assert!(!app.updates.posting);
    }
}
