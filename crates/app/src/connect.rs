//! Connect flow (layout "C, centered minimal", chosen 2026-11-15)
//! and the account switcher (layout "B, command modal", chosen
//! 2026-11-13). The QR area is gated by
//! [`crate::live::ConnectQr`]: only a current server-issued
//! payload is ever painted as a code; every other state (demo,
//! connecting, failed start, down session) renders words. The
//! demo pairing code is a deterministic placeholder.

use crate::icons::{draw as draw_icon, Icon};
use crate::state::{App, PairMode, Screen};
use crate::theme::rgb;
use crate::widgets::paint_avatar;
use core::theme::Tokens;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Margin, Pos2, RichText, Sense, Stroke, Vec2,
};

/// Connect layout constants (layout C, centered minimal): the
/// screen is one centered column, and its widths come from this
/// table, not from per-call literals, so the card, the stepper,
/// the status slot, and the pairing fields can never drift out
/// of alignment with each other.
/// The centered column's width; the card, stepper, and status
/// slot all span exactly it.
const COL_W: f32 = 360.0;
/// The QR slot's edge: a square that holds the plate, its 10pt
/// margin, the quiet zone, and the code (see `qr_layout`).
const QR_SLOT: f32 = 248.0;

impl App {
    /// Full-window connect screen (layout "C, centered minimal",
    /// chosen 2026-11-15): a slim topbar with Cancel, then ONE
    /// centered column: the brand mark, the glass card holding
    /// the QR (or the pairing entry), a horizontal 3-step stepper
    /// that doubles as the reserved state slot, the status line,
    /// and the pairing-mode switch as a real secondary button.
    /// Every link state renders inside that same column, so the
    /// layout never shifts while pairing progresses.
    pub(crate) fn draw_connect(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(rgb(t.bg_app))).show(
            root,
            |ui| {
                let area = ui.max_rect();
                let painter = ui.painter().clone();
                let bar = egui::Rect::from_min_size(area.min, Vec2::new(area.width(), 46.0));
                painter.line_segment(
                    [Pos2::new(bar.left(), bar.bottom()), Pos2::new(bar.right(), bar.bottom())],
                    Stroke::new(1.0_f32, rgb(t.border)),
                );
                painter.circle_filled(
                    Pos2::new(bar.left() + 30.0, bar.center().y),
                    11.0,
                    rgb(t.accent),
                );
                draw_icon(
                    &painter,
                    Icon::Chat,
                    Pos2::new(bar.left() + 30.0, bar.center().y),
                    13.0,
                    rgb(t.bg_app),
                    rgb(t.accent),
                );
                painter.text(
                    Pos2::new(bar.left() + 50.0, bar.center().y),
                    Align2::LEFT_CENTER,
                    "Zaprust",
                    FontId::proportional(14.5),
                    rgb(t.text_primary),
                );
                let cancel_rect = egui::Rect::from_min_size(
                    Pos2::new(bar.right() - 96.0, bar.top()),
                    Vec2::new(96.0, 46.0),
                );
                let cresp =
                    ui.interact(cancel_rect, egui::Id::unique("connect_cancel"), Sense::click());
                let cresp = cresp.on_hover_cursor(egui::CursorIcon::PointingHand);
                painter.text(
                    Pos2::new(bar.right() - 18.0, bar.center().y),
                    Align2::RIGHT_CENTER,
                    "Cancel",
                    FontId::proportional(13.0),
                    if cresp.hovered() { rgb(t.accent) } else { rgb(t.text_secondary) },
                );
                if cresp.clicked() {
                    self.screen = Screen::Chats;
                }
                let footer = egui::Rect::from_min_size(
                    Pos2::new(area.left(), area.bottom() - 38.0),
                    Vec2::new(area.width(), 38.0),
                );
                painter.line_segment(
                    [footer.left_top(), footer.right_top()],
                    Stroke::new(1.0_f32, rgb(t.border)),
                );
                painter.text(
                    Pos2::new(footer.left() + 20.0, footer.center().y),
                    Align2::LEFT_CENTER,
                    "Unofficial client. Using it may be against WhatsApp's terms of service.",
                    FontId::proportional(11.5),
                    rgb(t.text_faint),
                );
                // The centered column. Its height is estimated
                // per mode only to center it vertically in the
                // body; the content itself flows.
                let body = egui::Rect::from_min_max(
                    Pos2::new(area.left(), bar.bottom()),
                    Pos2::new(area.right(), footer.top()),
                );
                let content_h = match self.pair_mode {
                    PairMode::Qr => 560.0,
                    PairMode::Code => 540.0,
                } + if self.crash_report.is_some() { 128.0 } else { 0.0 };
                let top_pad = ((body.height() - content_h) * 0.5).clamp(0.0, 200.0);
                let col_w = COL_W;
                let col_rect = egui::Rect::from_min_size(
                    Pos2::new(area.center().x - col_w / 2.0, body.top() + top_pad),
                    Vec2::new(col_w, body.height() - top_pad),
                );
                let mut col = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(col_rect)
                        .layout(egui::Layout::top_down(egui::Align::Center)),
                );
                // Brand mark.
                col.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 15.0, rgb(t.accent));
                    draw_icon(
                        ui.painter(),
                        Icon::Chat,
                        rect.center(),
                        17.0,
                        rgb(t.bg_app),
                        rgb(t.accent),
                    );
                    ui.add_space(4.0);
                    ui.label(RichText::new("Zaprust").font(crate::theme::semibold(20.0)));
                });
                col.add_space(20.0);
                // The crash banner renders IN FLOW at the top of
                // the column on this screen: as a fixed overlay it
                // painted over the connect header and collided
                // with the unlock notice (the owner's v1.1.0
                // report). On the main screens it stays an
                // overlay; the banner content is shared
                // (crash_ui::draw_crash_banner).
                if self.crash_report.is_some() {
                    self.draw_crash_banner(&mut col, col_w);
                    col.add_space(16.0);
                }
                // The glass card: the one elevated surface on
                // this screen, holding the QR or the pairing
                // entry, so the code is what the eye lands on.
                let card = crate::theme::glass_text_frame(t, 16)
                    .inner_margin(Margin::symmetric(20, 18))
                    .show(&mut col, |ui| {
                        ui.set_width(col_w - 40.0);
                        ui.with_layout(
                            egui::Layout::top_down(egui::Align::Center),
                            |ui| match self.pair_mode {
                                PairMode::Qr => {
                                    let (rect, _) =
                                        ui.allocate_exact_size(Vec2::splat(QR_SLOT), Sense::hover());
                                    // The pairing gate (live.rs): a
                                    // matrix is painted only for the
                                    // Live state, which requires a
                                    // pairable session holding a
                                    // current server-issued payload.
                                    // Every other state renders
                                    // words in this slot, never a
                                    // code-shaped graphic.
                                    match self.connect_qr() {
                                        crate::live::ConnectQr::Live(matrix) => {
                                            draw_qr_matrix(ui.painter(), rect, matrix);
                                            ui.add_space(12.0);
                                            let fake = self.live.as_ref().is_some_and(|s| s.fake);
                                            ui.label(
                                                RichText::new(if fake {
                                                    "Test mode code. WhatsApp cannot scan it."
                                                } else {
                                                    "Live QR. Scan it with your phone."
                                                })
                                                .size(11.0)
                                                .color(rgb(t.text_faint)),
                                            );
                                        }
                                        _ => self.connect_qr_placeholder(ui, rect, t),
                                    }
                                }
                                PairMode::Code => self.connect_code_panel(ui, t),
                            },
                        );
                    });
                crate::theme::paint_glass_edge(col.painter(), card.response.rect, 16.0, t);
                col.add_space(18.0);
                // The stepper doubles as the state slot: steps
                // complete as pairing progresses, and the status
                // line under it names the current state, all in
                // reserved space.
                let steps: &[&str] = match self.pair_mode {
                    PairMode::Qr => &["Open WhatsApp", "Linked devices", "Scan the code"],
                    PairMode::Code => &["Open WhatsApp", "Linked devices", "Enter the code"],
                };
                steps_stepper(&mut col, t, steps, self.stepper_progress());
                col.add_space(8.0);
                self.connect_status_line(&mut col, t);
                col.add_space(14.0);
                // The other pairing path, as a real secondary
                // button under the status line.
                let (label, target) = match self.pair_mode {
                    PairMode::Qr => ("Link with phone number", PairMode::Code),
                    PairMode::Code => ("Link with QR code", PairMode::Qr),
                };
                if crate::widgets::button(&mut col, t, label, crate::widgets::Btn::Secondary)
                    .clicked()
                {
                    self.pair_mode = target;
                }
            },
        );
    }

    /// How far the connect stepper has progressed: the first
    /// two steps are the phone-side preparation (done by the
    /// time the user reads this screen), the third completes
    /// when the session links.
    fn stepper_progress(&self) -> usize {
        if let Some(session) = &self.live {
            if matches!(session.status, crate::live::LinkStatus::Linked) {
                return 3;
            }
        }
        2
    }

    /// The status line under the stepper: a fixed-height slot,
    /// so waiting, confirming, linked and error states swap in
    /// place without moving anything around them.
    fn connect_status_line(&mut self, ui: &mut egui::Ui, t: Tokens) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(COL_W, 24.0), Sense::hover());
        let mut slot = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(
            egui::Layout::left_to_right(egui::Align::Center).with_main_align(egui::Align::Center),
        ));
        // A failed live start owns the line in both pair modes:
        // nothing is waiting to be scanned or entered.
        if self.live_start_error.is_some() {
            slot.label(RichText::new("Could not connect.").size(12.5).color(rgb(t.danger)));
            return;
        }
        if self.live_mode {
            if let Some(session) = &self.live {
                let failed = matches!(
                    session.status,
                    crate::live::LinkStatus::Failed(_)
                        | crate::live::LinkStatus::Banned(_)
                        | crate::live::LinkStatus::Lost(_)
                        | crate::live::LinkStatus::Crashed(_)
                );
                // In code mode there is no QR on screen to scan:
                // while the backend still waits, it is waiting for
                // WhatsApp to send the pairing code, so the status
                // line names that instead of the scan.
                let status_label = if matches!(self.pair_mode, PairMode::Code)
                    && matches!(session.status, crate::live::LinkStatus::WaitingQr)
                {
                    "Waiting for WhatsApp to send a code.".to_owned()
                } else if matches!(self.pair_mode, PairMode::Qr)
                    && matches!(session.status, crate::live::LinkStatus::WaitingQr)
                    && session.qr.is_none()
                {
                    // No payload is painted (the gate shows a
                    // connecting state), so the line must not claim
                    // a scan is being awaited.
                    "Waiting for WhatsApp to send a QR code.".to_owned()
                } else {
                    session.status.label()
                };
                let (status_text, color) = if session.fake {
                    (format!("Test mode (not WhatsApp): {status_label}"), t.text_faint)
                } else if failed {
                    (status_label, t.danger)
                } else {
                    (status_label, t.accent)
                };
                let waiting = matches!(
                    session.status,
                    crate::live::LinkStatus::Starting | crate::live::LinkStatus::WaitingQr
                );
                if waiting {
                    crate::widgets::spinner(&mut slot, 14.0);
                    slot.add_space(6.0);
                }
                slot.label(RichText::new(status_text).size(12.5).color(rgb(color)));
            } else if self.logged_out {
                slot.label(
                    RichText::new("Logged out. Link again to continue.")
                        .size(12.5)
                        .color(rgb(t.accent)),
                );
            }
            return;
        }
        match self.pair_mode {
            PairMode::Qr => {
                slot.label(
                    RichText::new("Demo mode. There is no code to scan here.")
                        .size(12.5)
                        .color(rgb(t.text_secondary)),
                );
            }
            PairMode::Code if self.pair_code_shown => {
                crate::widgets::spinner(&mut slot, 14.0);
                slot.add_space(6.0);
                slot.label(
                    RichText::new("Waiting for the code to be entered on your phone\u{2026}")
                        .size(12.5)
                        .color(rgb(t.text_secondary)),
                );
            }
            PairMode::Code => {
                slot.label(
                    RichText::new("Enter your phone number to get a pairing code.")
                        .size(12.5)
                        .color(rgb(t.text_secondary)),
                );
            }
        }
    }

    /// The QR slot's non-code states (see
    /// [`crate::live::ConnectQr`]): each is plain words centered
    /// in the slot, so the eye lands on the state, not on a
    /// graphic that looks scannable but is not.
    fn connect_qr_placeholder(&mut self, ui: &mut egui::Ui, rect: egui::Rect, t: Tokens) {
        // Classify into owned data first: the gate value borrows
        // the session, and the failed-start arm below needs
        // `&mut self` for its retry button.
        enum Slot {
            StartFailed(String),
            Connecting,
            Unavailable(String),
            Demo,
            LoggedOut,
        }
        let state = match self.connect_qr() {
            crate::live::ConnectQr::Live(_) => return,
            crate::live::ConnectQr::StartFailed(message) => Slot::StartFailed(message.to_owned()),
            crate::live::ConnectQr::Connecting => Slot::Connecting,
            crate::live::ConnectQr::Unavailable(status) => Slot::Unavailable(status.label()),
            crate::live::ConnectQr::Demo => Slot::Demo,
            crate::live::ConnectQr::LoggedOut => Slot::LoggedOut,
        };
        let mut slot = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::Center)),
        );
        match state {
            Slot::StartFailed(message) => {
                slot.add_space(28.0);
                self.connect_start_failed_panel(&mut slot, t, &message);
            }
            Slot::Connecting => {
                slot.add_space(96.0);
                crate::widgets::loading_row(
                    &mut slot,
                    "Waiting for WhatsApp to send a QR code\u{2026}",
                    12.0,
                    t.text_secondary,
                );
            }
            Slot::Unavailable(label) => {
                slot.add_space(96.0);
                slot.label(RichText::new(label).size(12.5).color(rgb(t.text_secondary)));
            }
            Slot::Demo => {
                slot.add_space(84.0);
                slot.label(RichText::new("Demo mode").size(14.0).strong());
                slot.add_space(8.0);
                slot.label(
                    RichText::new(
                        "Pairing is not real in the demo, and there is no code \
                         to scan. Run Zaprust in live mode to link an account.",
                    )
                    .size(12.0)
                    .color(rgb(t.text_secondary)),
                );
            }
            Slot::LoggedOut => {
                slot.add_space(28.0);
                self.connect_logged_out_panel(&mut slot, t);
            }
        }
    }

    /// The logged-out panel shared by the QR slot and the
    /// pairing-code panel: the state in plain words and the way
    /// back in (a fresh link, which starts a new session).
    fn connect_logged_out_panel(&mut self, ui: &mut egui::Ui, t: Tokens) {
        ui.label(RichText::new("You are logged out").size(14.0).strong());
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "This computer is unlinked. The chats saved on this \
                 computer stay in the local archive.",
            )
            .size(12.0)
            .color(rgb(t.text_secondary)),
        );
        ui.add_space(12.0);
        if crate::widgets::button(ui, t, "Link again", crate::widgets::Btn::Primary).clicked() {
            let ctx = ui.ctx().clone();
            self.start_live(&ctx, false, false);
        }
    }

    /// The failed-live-start panel shared by the QR slot and the
    /// pairing-code panel: the plain-language reason, wrapped in
    /// the card (never a clipped strip), and the two ways out:
    /// retry the start, or start fresh (the stranded data is set
    /// aside; that path is destructive, so its button only parks
    /// the house confirmation). This panel is what stays on the
    /// connect screen itself for as long as no session exists;
    /// the same text is not repeated as a banner over it.
    fn connect_start_failed_panel(&mut self, ui: &mut egui::Ui, t: Tokens, message: &str) {
        ui.label(RichText::new("Could not connect").size(14.0).strong());
        ui.add_space(8.0);
        ui.label(RichText::new(message).size(12.0).color(rgb(t.text_secondary)));
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if crate::widgets::button(ui, t, "Try again", crate::widgets::Btn::Primary).clicked() {
                let ctx = ui.ctx().clone();
                self.start_live(&ctx, false, false);
            }
            if crate::widgets::button(
                ui,
                t,
                &crate::i18n::t("connect.start_fresh"),
                crate::widgets::Btn::Secondary,
            )
            .clicked()
            {
                self.ui_confirm = Some(crate::state::UiConfirm::StartFresh);
            }
        });
    }

    /// The pairing entry inside the connect card: first the
    /// phone field, then the eight code boxes, centered in the
    /// column like everything else on this screen.
    fn connect_code_panel(&mut self, ui: &mut egui::Ui, t: Tokens) {
        // A failed live start owns the whole card: the demo code
        // path below must never present itself as the real flow
        // to a user whose live start just failed.
        if let Some(message) = self.live_start_error.clone() {
            self.connect_start_failed_panel(ui, t, &message);
            return;
        }
        // A logged-out device owns the card the same way: there
        // is no session to pair against until a fresh link starts.
        if self.logged_out && self.live.is_none() {
            self.connect_logged_out_panel(ui, t);
            return;
        }
        ui.add_space(6.0);
        if self.pair_code_shown {
            ui.label(RichText::new("Enter this code on your phone").size(14.0).strong());
            ui.add_space(12.0);
            let live_code = self.live.as_ref().and_then(|s| s.pair_code.clone());
            let code = if self.live_mode {
                live_code.unwrap_or_else(|| "········".to_owned())
            } else {
                pairing_code(&self.pair_phone)
            };
            ui.vertical_centered(|ui| {
                for row in 0..2 {
                    ui.horizontal(|ui| {
                        for i in 0..4 {
                            let ch = code.chars().nth(row * 4 + i).unwrap_or('·');
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::splat(52.0), Sense::hover());
                            ui.painter().rect_filled(
                                rect,
                                CornerRadius::same(11),
                                rgb(t.bg_panel_alt),
                            );
                            ui.painter().rect_stroke(
                                rect,
                                CornerRadius::same(11),
                                Stroke::new(1.0_f32, rgb(t.border)),
                                egui::StrokeKind::Inside,
                            );
                            ui.painter().text(
                                rect.center(),
                                Align2::CENTER_CENTER,
                                ch.to_string(),
                                FontId::proportional(21.0),
                                rgb(t.text_primary),
                            );
                            // The gap lives between tiles only, so
                            // the row's outer edges stay flush.
                            if i < 3 {
                                ui.add_space(2.0);
                            }
                        }
                    });
                    ui.add_space(7.0);
                }
            });
            let caption = if self.live_mode {
                match self.live.as_ref().and_then(|s| s.pair_code_at) {
                    Some(at) => {
                        let left = crate::live::PAIR_CODE_TTL.saturating_sub(at.elapsed());
                        format!(
                            "Live code. It expires in {}:{:02}.",
                            left.as_secs() / 60,
                            left.as_secs() % 60
                        )
                    }
                    None => "Waiting for WhatsApp to send a code…".to_owned(),
                }
            } else {
                "Demo code. The real one comes from WhatsApp when you link.".to_owned()
            };
            let waiting_code =
                self.live_mode && self.live.as_ref().and_then(|s| s.pair_code_at).is_none();
            if waiting_code {
                crate::widgets::loading_row(ui, &caption, 11.0, t.text_faint);
            } else {
                ui.label(RichText::new(caption).size(11.0).color(rgb(t.text_faint)));
            }
        } else {
            ui.label(RichText::new("Enter your phone number").size(14.0).strong());
            ui.add_space(10.0);
            // The shared text field: 238 inner width plus its frame
            // margins gives the same 258 total the old hand-wrapped
            // field occupied.
            crate::widgets::text_field(ui, t, &mut self.pair_phone, "+1 …", 238.0);
            ui.add_space(12.0);
            // The shared primary button, stretched across the
            // column width (258, the same width as the field above)
            // by a cross-justified layout instead of the old
            // hand-painted pill.
            let bresp = ui
                .allocate_ui_with_layout(
                    Vec2::new(258.0, 32.0),
                    egui::Layout::top_down(egui::Align::Min).with_cross_justify(true),
                    |ui| {
                        crate::widgets::button(
                            ui,
                            t,
                            "Get pairing code",
                            crate::widgets::Btn::Primary,
                        )
                    },
                )
                .inner;
            if bresp.clicked() {
                self.pair_code_shown = true;
                if self.live_mode {
                    self.request_pair_code();
                }
            }
        }
    }

    /// Account switcher as a centred command-style modal.
    pub(crate) fn draw_account_menu(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        let Some(menu_p) =
            crate::anim::popup_phase(&ctx, "account_menu", self.account_menu_open, self.motion)
        else {
            return;
        };
        let exiting = !self.account_menu_open;
        let t = self.tokens();
        if !exiting && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.account_menu_open = false;
            return;
        }
        // The dim and the modal fade together while the modal's
        // layer scales from 0.95 around its center; the exit is
        // the same motion, shorter and reversed. The dim is the
        // shared modal dim, the same depth every modal uses.
        let area = root.max_rect();
        root.painter().rect_filled(area, CornerRadius::ZERO, crate::theme::modal_dim(menu_p));
        // The modal grows by one row while a linked session can
        // be logged out.
        let size = Vec2::new(430.0, if self.can_log_out() { 352.0 } else { 302.0 });
        let rect = egui::Rect::from_min_size(
            Pos2::new(area.center().x - size.x / 2.0, area.top() + 96.0),
            size,
        );
        if !exiting
            && ctx.input(|i| i.pointer.any_click())
            && !rect.contains(ctx.input(|i| i.pointer.interact_pos().unwrap_or(Pos2::ZERO)))
        {
            self.account_menu_open = false;
            return;
        }
        egui::Area::new(egui::Id::unique("account_modal"))
            .fixed_pos(rect.min)
            .order(egui::Order::Foreground)
            .show(&ctx, |ui| {
                if menu_p < 0.999 {
                    ui.set_opacity(menu_p);
                }
                // The shared glass frame at the modal radius:
                // the same fill, hairline and shadow as every
                // other glass surface (this frame used to carry
                // a one-off deeper shadow of its own).
                let modal = crate::theme::glass_frame(t, 16)
                    .inner_margin(Margin::symmetric(14, 12))
                    .show(ui, |ui| {
                        ui.set_width(size.x - 28.0);
                        egui::Frame::new()
                            .fill(rgb(t.bg_panel_alt))
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(Margin::symmetric(12, 10))
                            .show(ui, |ui| {
                                ui.set_width(size.x - 52.0);
                                ui.label(
                                    RichText::new("Switch account…")
                                        .size(13.5)
                                        .color(rgb(t.text_faint)),
                                );
                            });
                        ui.add_space(10.0);
                        egui::Frame::new()
                            .fill(rgb(t.bg_panel_alt))
                            .corner_radius(CornerRadius::same(12))
                            .inner_margin(Margin::symmetric(10, 8))
                            .show(ui, |ui| {
                                ui.set_width(size.x - 48.0);
                                ui.horizontal(|ui| {
                                    // The account's own photo when
                                    // one is loaded (the same state
                                    // the profile editor edits),
                                    // the generic avatar otherwise.
                                    if self.my_profile.photo_texture.is_some() {
                                        crate::profile::paint_my_avatar_disc(
                                            ui.painter(),
                                            ui.cursor().min + Vec2::new(17.0, 17.0),
                                            17.0,
                                            &self.my_profile,
                                            t,
                                        );
                                    } else {
                                        paint_avatar(
                                            ui.painter(),
                                            ui.cursor().min + Vec2::new(17.0, 17.0),
                                            17.0,
                                            false,
                                            0,
                                        );
                                    }
                                    ui.add_space(42.0);
                                    ui.vertical(|ui| {
                                        let (name, sub) = if self.live_mode {
                                            let session = self.live.as_ref();
                                            // The header names the
                                            // account by its real
                                            // push name once the
                                            // profile read-back has
                                            // landed; the session's
                                            // generic label is only
                                            // the pre-read-back
                                            // placeholder.
                                            let profile_name = self
                                                .my_profile
                                                .account
                                                .as_ref()
                                                .map(|a| a.name.clone())
                                                .filter(|n| !n.trim().is_empty());
                                            let name = profile_name.unwrap_or_else(|| {
                                                session.map_or_else(
                                                    || "Live account".to_owned(),
                                                    |s| {
                                                        if s.account.is_empty() {
                                                            "Live account".to_owned()
                                                        } else {
                                                            s.account.clone()
                                                        }
                                                    },
                                                )
                                            });
                                            let sub = session
                                                .map(|s| {
                                                    if s.fake {
                                                        format!("Test mode, {}", s.status.label())
                                                    } else {
                                                        s.status.label()
                                                    }
                                                })
                                                .unwrap_or_default();
                                            (name, sub)
                                        } else {
                                            (
                                                "fanyzp".to_owned(),
                                                "Demo account, not linked".to_owned(),
                                            )
                                        };
                                        ui.label(RichText::new(name).size(13.5).strong());
                                        ui.label(
                                            RichText::new(sub)
                                                .size(11.5)
                                                .color(rgb(t.text_secondary)),
                                        );
                                    });
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            let (crect, _) = ui.allocate_exact_size(
                                                Vec2::splat(20.0),
                                                Sense::hover(),
                                            );
                                            draw_icon(
                                                ui.painter(),
                                                Icon::Check,
                                                crect.center(),
                                                18.0,
                                                rgb(t.accent),
                                                rgb(t.bg_panel_alt),
                                            );
                                        },
                                    );
                                });
                            });
                        ui.add_space(6.0);
                        // My profile: the signed-in user's own
                        // profile editor (name, about, photo).
                        let (mrect, mresp) =
                            ui.allocate_exact_size(Vec2::new(size.x - 28.0, 44.0), Sense::click());
                        if mresp.hovered() {
                            ui.painter().rect_filled(
                                mrect,
                                CornerRadius::same(12),
                                rgb(t.bg_panel_alt),
                            );
                        }
                        paint_avatar(
                            ui.painter(),
                            Pos2::new(mrect.left() + 26.0, mrect.center().y),
                            13.0,
                            false,
                            0,
                        );
                        ui.painter().text(
                            Pos2::new(mrect.left() + 52.0, mrect.center().y),
                            Align2::LEFT_CENTER,
                            crate::i18n::t("profile.my_title"),
                            FontId::proportional(13.5),
                            rgb(t.text_primary),
                        );
                        if mresp.clicked() && !exiting {
                            self.account_menu_open = false;
                            self.open_my_profile();
                        }
                        ui.add_space(6.0);
                        let (rrect, rresp) =
                            ui.allocate_exact_size(Vec2::new(size.x - 28.0, 44.0), Sense::click());
                        if rresp.hovered() {
                            ui.painter().rect_filled(
                                rrect,
                                CornerRadius::same(12),
                                rgb(t.bg_panel_alt),
                            );
                        }
                        draw_icon(
                            ui.painter(),
                            Icon::Plus,
                            Pos2::new(rrect.left() + 26.0, rrect.center().y),
                            18.0,
                            rgb(t.accent),
                            rgb(t.bg_panel),
                        );
                        ui.painter().text(
                            Pos2::new(rrect.left() + 52.0, rrect.center().y),
                            Align2::LEFT_CENTER,
                            "Add account",
                            FontId::proportional(13.5),
                            rgb(t.text_primary),
                        );
                        if rresp.clicked() && !exiting {
                            self.account_menu_open = false;
                            if !self.live_mode {
                                // The in-app path into live mode: start
                                // the real backend, then show the link
                                // flow it drives.
                                if let Some(ctx) = self.egui_ctx.clone() {
                                    self.start_live(&ctx, false, false);
                                }
                            }
                            self.screen = Screen::Connect;
                        }
                        if self.can_log_out() {
                            ui.add_space(6.0);
                            // Log out: unlinks this device. A
                            // destructive action, so the row only
                            // parks the house confirmation; the
                            // modal's confirm button performs it.
                            let (lrect, lresp) = ui.allocate_exact_size(
                                Vec2::new(size.x - 28.0, 44.0),
                                Sense::click(),
                            );
                            if lresp.hovered() {
                                ui.painter().rect_filled(
                                    lrect,
                                    CornerRadius::same(12),
                                    rgb(t.bg_panel_alt),
                                );
                            }
                            draw_icon(
                                ui.painter(),
                                Icon::LogOut,
                                Pos2::new(lrect.left() + 26.0, lrect.center().y),
                                18.0,
                                rgb(t.danger),
                                rgb(t.bg_panel),
                            );
                            ui.painter().text(
                                Pos2::new(lrect.left() + 52.0, lrect.center().y),
                                Align2::LEFT_CENTER,
                                crate::i18n::t("account.log_out"),
                                FontId::proportional(13.5),
                                rgb(t.danger),
                            );
                            if lresp.clicked() && !exiting {
                                self.account_menu_open = false;
                                self.ui_confirm = Some(crate::state::UiConfirm::LogOut);
                            }
                        }
                        ui.add_space(8.0);
                        let (drect, _) =
                            ui.allocate_exact_size(Vec2::new(size.x - 28.0, 1.0), Sense::hover());
                        ui.painter().rect_filled(drect, CornerRadius::ZERO, rgb(t.border));
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("Enter select  ·  Esc close")
                                .size(11.5)
                                .color(rgb(t.text_faint)),
                        );
                    });
                crate::theme::paint_glass_edge(ui.painter(), modal.response.rect, 16.0, t);
            });
        crate::anim::popup_transform(
            &ctx,
            egui::Id::unique("account_modal"),
            rect.center(),
            menu_p,
        );
    }
}

/// Horizontal stepper for the centered connect layout: one
/// cell per step (disc over a short label), hairline connectors
/// at disc height. Steps before `done` wear a check, step
/// `done` is the current one, the rest wait in the border tone.
fn steps_stepper(ui: &mut egui::Ui, t: Tokens, items: &[&str], done: usize) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(COL_W, 46.0), Sense::hover());
    let painter = ui.painter();
    let cell_w = rect.width() / items.len() as f32;
    let cy = rect.top() + 11.0;
    for (i, _) in items.iter().enumerate() {
        if i > 0 {
            let from = Pos2::new(rect.left() + cell_w * (i as f32 - 0.5) + 14.0, cy);
            let to = Pos2::new(rect.left() + cell_w * (i as f32 + 0.5) - 14.0, cy);
            let col = if i < done { rgb(t.accent) } else { rgb(t.border) };
            painter.line_segment([from, to], Stroke::new(1.5_f32, col));
        }
    }
    for (i, text) in items.iter().enumerate() {
        let center = Pos2::new(rect.left() + cell_w * (i as f32 + 0.5), cy);
        if i < done {
            painter.circle_filled(center, 10.5, rgb(t.accent));
            draw_icon(painter, Icon::Check, center, 12.0, rgb(t.bg_app), rgb(t.accent));
        } else if i == done {
            painter.circle_filled(center, 10.5, rgb(t.accent));
            painter.text(
                center,
                Align2::CENTER_CENTER,
                format!("{}", i + 1),
                FontId::proportional(11.5),
                rgb(t.bg_app),
            );
        } else {
            painter.circle_filled(center, 10.5, rgb(t.bg_panel_alt));
            painter.circle_stroke(center, 10.5, Stroke::new(1.0_f32, rgb(t.border)));
            painter.text(
                center,
                Align2::CENTER_CENTER,
                format!("{}", i + 1),
                FontId::proportional(11.5),
                rgb(t.text_faint),
            );
        }
        painter.text(
            Pos2::new(center.x, rect.top() + 36.0),
            Align2::CENTER_CENTER,
            *text,
            FontId::proportional(11.0),
            if i <= done { rgb(t.text_secondary) } else { rgb(t.text_faint) },
        );
    }
}

/// A deterministic demo pairing code derived from the typed number,
/// stable for screenshots, obviously not a real credential.
fn pairing_code(phone: &str) -> String {
    const CHARS: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut h: u32 = 0x811C_9DC5;
    for b in phone.bytes() {
        h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    (0..8)
        .map(|i| {
            h = h.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            CHARS[(((h >> (i % 16)) & 31) as usize) % CHARS.len()] as char
        })
        .collect()
}

/// Draws a real QR payload matrix (from the `backend` crate, which
/// owns QR encoding) with a 4-module quiet zone.
/// Layout of a painted code inside its slot: the white plate
/// fills the slot, and modules begin after a 10pt plate margin
/// plus the standard four-module quiet zone on every side. One
/// cell size spans the whole code; it is fractional at the
/// connect slot's size, which is safe because the painter draws
/// vector rectangles (nothing is rasterized or resampled).
fn qr_layout(rect: egui::Rect, modules: usize) -> (egui::Rect, f32) {
    let inner = rect.shrink(10.0);
    (inner, inner.width() / (modules + 8) as f32)
}

fn draw_qr_matrix(painter: &egui::Painter, rect: egui::Rect, matrix: &backend::QrMatrix) {
    painter.rect_filled(rect, CornerRadius::same(10), Color32::WHITE);
    let (inner, cell) = qr_layout(rect, matrix.size);
    let black = Color32::from_rgb(0x11, 0x1B, 0x21);
    for y in 0..matrix.size {
        for x in 0..matrix.size {
            if matrix.dark(x, y) {
                let min = Pos2::new(
                    inner.left() + (x + 4) as f32 * cell,
                    inner.top() + (y + 4) as f32 * cell,
                );
                painter.rect_filled(
                    egui::Rect::from_min_size(min, Vec2::splat(cell + 0.5)),
                    CornerRadius::ZERO,
                    black,
                );
            }
        }
    }
}

#[cfg(test)]
mod qr_layout_tests {
    use super::qr_layout;
    use egui::{Pos2, Rect, Vec2};

    #[test]
    fn plate_margin_and_quiet_zone_are_reserved() {
        // The connect slot is 248pt; payloads run from version 2
        // (25 modules) up past version 13 (61) for pairing data.
        for modules in [25usize, 41, 61, 97] {
            let rect = Rect::from_min_size(Pos2::ZERO, Vec2::splat(248.0));
            let (inner, cell) = qr_layout(rect, modules);
            // The plate margin is exactly 10pt on every side.
            assert_eq!(inner.left(), 10.0);
            assert_eq!(inner.top(), 10.0);
            assert_eq!(inner.width(), 228.0);
            // The cell spans code plus both four-module quiet
            // zones, no more and no less: the first dark module
            // lands four cells in from the inner edge.
            let spanned = cell * (modules + 8) as f32;
            assert!((spanned - inner.width()).abs() < 0.05, "modules {modules}");
            assert!(cell > 0.0);
        }
    }

    #[test]
    fn cell_at_the_connect_slot_stays_legible() {
        // A pairing payload encodes to roughly 61 modules; at
        // the 248pt slot the module size must stay comfortably
        // above the size phone cameras resolve at arm's length.
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::splat(248.0));
        let (_, cell) = qr_layout(rect, 61);
        assert!(cell > 3.0, "cell was {cell}");
    }
}
