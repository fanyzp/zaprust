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
/// The wide card's width at desktop widths: instructions on the
/// left, the QR or pairing code on the right, in one card.
const WIDE_W: f32 = 720.0;
/// The instruction pane's width inside the wide card; the code
/// pane takes what remains after the card margins and the gap.
const WIDE_STEPS_W: f32 = 296.0;

/// Whether a window of this width gets the wide two-part
/// connect card. Below it the screen keeps the single centered
/// column, which is the only arrangement a portrait or narrow
/// window can hold without crushing either part.
fn connect_wide(width: f32) -> bool {
    width >= 700.0
}
/// The QR slot's edge: a square that holds the plate, its 10pt
/// margin, the quiet zone, and the code (see `qr_layout`).
const QR_SLOT: f32 = 248.0;
/// The pairing form's width: the country selector, the number
/// field, and the primary button all span exactly it.
const FIELD_W: f32 = 258.0;

/// The official brand mark (the bolt bubble of
/// `packaging/icon.svg`, via its bundled PNG) as a GPU
/// texture, decoded once and kept for the life of the app.
/// A decode failure yields no mark rather than a substitute
/// glyph: the brand is never improvised from the icon font.
fn brand_texture(ctx: &egui::Context) -> Option<egui::TextureId> {
    thread_local! {
        static BRAND: std::cell::RefCell<Option<Option<egui::TextureHandle>>> =
            const { std::cell::RefCell::new(None) };
    }
    BRAND.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            const BYTES: &[u8] = include_bytes!("../../../packaging/icons/zaprust-256.png");
            *slot = Some(image::load_from_memory(BYTES).ok().map(|img| {
                let rgba = img.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                ctx.load_texture("zaprust-brand", color, egui::TextureOptions::LINEAR)
            }));
        }
        slot.as_ref().and_then(|inner| inner.as_ref().map(|h| h.id()))
    })
}

impl App {
    /// Full-window connect screen (layout "C, centered minimal",
    /// chosen 2026-11-15): a slim topbar carrying the one brand
    /// mark and a real Cancel button, then the link flow. At
    /// desktop widths the flow is one wide card, instructions on
    /// the left and the QR (or pairing entry) on the right; at
    /// narrower widths it is ONE centered column: the glass card,
    /// a horizontal 3-step stepper that doubles as the reserved
    /// state slot, the status line, and the pairing-mode switch
    /// as a real secondary button. Every link state renders in
    /// the same arrangement, so the layout never shifts while
    /// pairing progresses.
    pub(crate) fn draw_connect(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(rgb(t.bg_app))).show(
            root,
            |ui| {
                let area = ui.max_rect();
                let painter = ui.painter().clone();
                let bar = egui::Rect::from_min_size(area.min, Vec2::new(area.width(), 46.0));
                // No divider under the bar: the bar and the body
                // share the app ground, and a flat surface is
                // separated by geometry and spacing, not a line
                // (design standard, hairline discipline).
                // The brand mark is the official bolt bubble
                // artwork, the same mark as the window and
                // desktop icons: it is the one brand glyph the
                // app shows anywhere (design standard, icon
                // discipline).
                if let Some(tex) = brand_texture(ui.ctx()) {
                    let center = Pos2::new(bar.left() + 30.0, bar.center().y);
                    painter.image(
                        tex,
                        egui::Rect::from_center_size(center, Vec2::splat(crate::icons::SIZE_TAB)),
                        egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }
                painter.text(
                    Pos2::new(bar.left() + 50.0, bar.center().y),
                    Align2::LEFT_CENTER,
                    "Zaprust",
                    FontId::proportional(14.5),
                    rgb(t.text_primary),
                );
                // Cancel is the house button, not bare text, so
                // the way out of the flow reads as an action with
                // the same affordance as every other button.
                let mut bar_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(bar)
                        .layout(egui::Layout::right_to_left(egui::Align::Center)),
                );
                bar_ui.add_space(12.0);
                if crate::widgets::button(
                    &mut bar_ui,
                    t,
                    &crate::i18n::t("connect.cancel"),
                    crate::widgets::Btn::Secondary,
                )
                .clicked()
                {
                    self.screen = Screen::Chats;
                }
                let footer = egui::Rect::from_min_size(
                    Pos2::new(area.left(), area.bottom() - 38.0),
                    Vec2::new(area.width(), 38.0),
                );
                // No line above the footer either, for the
                // same reason: the strip's geometry and the
                // faint disclaimer text delimit it.
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
                let wide = connect_wide(area.width());
                let content_h = if wide {
                    360.0
                } else {
                    match self.pair_mode {
                        PairMode::Qr => 560.0,
                        PairMode::Code => 540.0,
                    }
                } + if self.crash_report.is_some() { 128.0 } else { 0.0 }
                    + if self.live_start_error.is_some() { 96.0 } else { 0.0 };
                let top_pad = ((body.height() - content_h) * 0.5).clamp(0.0, 200.0);
                let col_w = if wide { WIDE_W } else { COL_W };
                let col_rect = egui::Rect::from_min_size(
                    Pos2::new(area.center().x - col_w / 2.0, body.top() + top_pad),
                    Vec2::new(col_w, body.height() - top_pad),
                );
                let mut col = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(col_rect)
                        .layout(egui::Layout::top_down(egui::Align::Center)),
                );
                // No brand mark in the column: the top bar
                // already carries it, and one brand per screen is
                // enough (a second one here only duplicated it).
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
                // The steps both arrangements show, named by the
                // pairing path the user is on.
                let steps: &[&str] = match self.pair_mode {
                    PairMode::Qr => &["Open WhatsApp", "Linked devices", "Scan the code"],
                    PairMode::Code => &["Open WhatsApp", "Linked devices", "Enter the code"],
                };
                let complete = self.stepper_complete();
                // The glass card: the one elevated surface on
                // this screen, holding the QR or the pairing
                // entry, so the code is what the eye lands on.
                // Wide, it also holds the instructions on its
                // left; narrow, they sit under it as the stepper
                // and status line.
                let card = crate::theme::glass_text_frame(t, 16)
                    .inner_margin(Margin::symmetric(20, 18))
                    .show(&mut col, |ui| {
                        ui.set_width(col_w - 40.0);
                        if wide {
                            ui.horizontal_top(|ui| {
                                ui.vertical(|ui| {
                                    ui.set_width(WIDE_STEPS_W);
                                    ui.label(
                                        RichText::new(crate::i18n::t("connect.title"))
                                            .font(crate::theme::semibold(15.0)),
                                    );
                                    ui.add_space(14.0);
                                    steps_column(ui, t, steps, complete);
                                    ui.add_space(14.0);
                                    self.connect_status_line(ui, t, WIDE_STEPS_W);
                                });
                                ui.add_space(24.0);
                                ui.allocate_ui_with_layout(
                                    Vec2::new(col_w - 40.0 - WIDE_STEPS_W - 24.0, 300.0),
                                    egui::Layout::top_down(egui::Align::Center),
                                    |ui| self.connect_card_main(ui, t),
                                );
                            });
                        } else {
                            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                                self.connect_card_main(ui, t)
                            });
                        }
                    });
                // The country picker takes its width from this
                // card, so it remembers the rect for next frame.
                self.connect_card_rect = Some(card.response.rect);
                crate::theme::paint_glass_edge(col.painter(), card.response.rect, 16.0, t);
                col.add_space(18.0);
                if !wide {
                    // The stepper doubles as the state slot: the
                    // status line under it names the current
                    // state, all in reserved space.
                    steps_stepper(&mut col, t, steps, complete);
                    col.add_space(8.0);
                    self.connect_status_line(&mut col, t, COL_W);
                    col.add_space(14.0);
                }
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
                    self.country_picker_open = false;
                }
            },
        );
    }

    /// Whether the connect flow has actually finished: the
    /// steps are actions the user takes on their phone, which
    /// this screen cannot observe one by one, so no step is
    /// marked done while pairing is still in progress. A linked
    /// session is the one event that proves every step happened,
    /// and only then do the steps wear checks (see
    /// [`step_marks`]).
    fn stepper_complete(&self) -> bool {
        self.live.as_ref().is_some_and(|s| s.status.is_linked())
    }

    /// The card's main content: the QR side of the flow, or the
    /// pairing entry in code mode. Shared by the narrow card and
    /// the wide card's right pane, so both arrangements present
    /// exactly the same states.
    fn connect_card_main(&mut self, ui: &mut egui::Ui, t: Tokens) {
        match self.pair_mode {
            PairMode::Qr => {
                // The failed-start panel sizes by its content
                // (the wrapped reason runs long), so it flows in
                // the card instead of the fixed square slot:
                // inside the slot its buttons spilled over the
                // stepper below.
                if let crate::live::ConnectQr::StartFailed(message) = self.connect_qr() {
                    let message = message.to_owned();
                    self.connect_start_failed_panel(ui, t, &message);
                } else {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(QR_SLOT), Sense::hover());
                    // The pairing gate (live.rs): a matrix is
                    // painted only for the Live state, which
                    // requires a pairable session holding a
                    // current server-issued payload. Every other
                    // state renders words in this slot, never a
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
            }
            PairMode::Code => self.connect_code_panel(ui, t),
        }
    }

    /// The status line under the stepper: a fixed-height slot,
    /// so waiting, confirming, linked and error states swap in
    /// place without moving anything around them.
    fn connect_status_line(&mut self, ui: &mut egui::Ui, t: Tokens, width: f32) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 24.0), Sense::hover());
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
                // While the flow is still preparing, the line
                // guides the user in one human sentence, the
                // same one wherever the wait shows: preparing
                // the QR code, or, in code mode, the pairing
                // code. It never narrates the machinery behind
                // the wait, and it never claims a scan is being
                // awaited before a payload exists to scan.
                let status_label =
                    match waiting_line_key(self.pair_mode, &session.status, session.qr.is_some()) {
                        Some(key) => crate::i18n::t(key),
                        None => session.status.label(),
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
        // The StartFailed state never reaches this slot: the
        // caller renders its panel in flow (see draw_connect).
        enum Slot {
            Connecting,
            Unavailable(String),
            Demo,
            LoggedOut,
        }
        let state = match self.connect_qr() {
            crate::live::ConnectQr::Live(_) | crate::live::ConnectQr::StartFailed(_) => return,
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
            Slot::Connecting => {
                slot.add_space(96.0);
                crate::widgets::loading_row(
                    &mut slot,
                    &crate::i18n::t("connect.waiting_qr"),
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

    /// The failed-live-start panel shared by both pair modes:
    /// the plain-language reason, wrapped in the card (never a
    /// clipped strip), and the two ways out:
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
            // The tiles show a code only when a real one
            // exists: while the flow is still waiting for one,
            // the tiles stay empty, because a dotted tile reads
            // as a masked code that has already arrived.
            let live_code = self.live.as_ref().and_then(|s| s.pair_code.clone());
            let code: Option<String> =
                if self.live_mode { live_code } else { Some(pairing_code(&self.pair_phone)) };
            let tiles = pair_code_tiles(code.as_deref());
            ui.vertical_centered(|ui| {
                for row in 0..2 {
                    ui.horizontal(|ui| {
                        for i in 0..4 {
                            let ch = tiles[row * 4 + i];
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
                            if let Some(ch) = ch {
                                ui.painter().text(
                                    rect.center(),
                                    Align2::CENTER_CENTER,
                                    ch.to_string(),
                                    FontId::proportional(21.0),
                                    rgb(t.text_primary),
                                );
                            }
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
                    None => crate::i18n::t("connect.waiting_code"),
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
            let country = crate::countries::by_iso(&self.pair_country_iso)
                .unwrap_or_else(|| crate::countries::default_country());
            // The country selector: the selected country's flag,
            // name, and dial code over a chevron. Opens the
            // searchable country picker.
            if self.country_selector(ui, t, country) {
                self.country_picker_open = !self.country_picker_open;
                if self.country_picker_open {
                    self.country_search.clear();
                }
            }
            ui.add_space(8.0);
            // The number field: the selected dial code as a fixed
            // prefix slot inside the field, the national number
            // after it. Every edit recomposes `pair_phone`, the
            // full international number the request consumes.
            let changed = self.phone_number_field(ui, t, country);
            if changed {
                self.pair_phone =
                    crate::countries::compose_number(country.dial, &self.pair_national);
            }
            ui.add_space(12.0);
            // The shared primary button, stretched across the
            // form width (the same width as the fields above) by
            // a cross-justified layout.
            let bresp = ui
                .allocate_ui_with_layout(
                    Vec2::new(FIELD_W, 32.0),
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
                if self.pair_national.chars().any(|c| c.is_ascii_digit()) {
                    self.pair_code_shown = true;
                    if self.live_mode {
                        self.request_pair_code();
                    }
                } else {
                    self.notice = Some("Enter your phone number first.".to_owned());
                }
            }
        }
        self.draw_country_picker(ui.ctx());
    }

    /// The country selector row over the number field: flag,
    /// country name, dial code, chevron, at the form width and
    /// the shared field height. Returns whether it was clicked.
    fn country_selector(
        &mut self,
        ui: &mut egui::Ui,
        t: Tokens,
        country: &'static crate::countries::Country,
    ) -> bool {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(FIELD_W, 40.0), Sense::click());
        // The picker anchors to this rect (see
        // draw_country_picker), so it is remembered each frame.
        self.country_anchor = Some(rect);
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(8), crate::widgets::field_fill(t));
        if resp.hovered() {
            painter.rect_stroke(
                rect,
                CornerRadius::same(8),
                Stroke::new(1.0_f32, rgb(t.border)),
                egui::StrokeKind::Inside,
            );
        }
        painter.text(
            Pos2::new(rect.left() + 10.0, rect.center().y),
            Align2::LEFT_CENTER,
            crate::countries::flag_emoji(country.iso),
            FontId::proportional(20.0),
            rgb(t.text_primary),
        );
        painter.text(
            Pos2::new(rect.left() + 46.0, rect.center().y),
            Align2::LEFT_CENTER,
            country.name,
            FontId::proportional(13.5),
            rgb(t.text_primary),
        );
        painter.text(
            Pos2::new(rect.right() - 34.0, rect.center().y),
            Align2::RIGHT_CENTER,
            format!("+{}", country.dial),
            FontId::proportional(13.0),
            rgb(t.text_secondary),
        );
        draw_icon(
            painter,
            Icon::ChevronDown,
            Pos2::new(rect.right() - 18.0, rect.center().y),
            crate::icons::SIZE_MENU,
            rgb(t.text_secondary),
            rgb(t.bg_panel_alt),
        );
        resp.clicked()
    }

    /// The number field under the selector: a fixed prefix slot
    /// showing the selected dial code, then the national number
    /// editor. Painted on an exact rect (like the stepper and
    /// the code tiles) so the field is the form width at every
    /// window size and for every dial code length. Returns
    /// whether the national number changed.
    fn phone_number_field(
        &mut self,
        ui: &mut egui::Ui,
        t: Tokens,
        country: &'static crate::countries::Country,
    ) -> bool {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(FIELD_W, 40.0), Sense::hover());
        ui.painter().rect_filled(rect, CornerRadius::same(8), crate::widgets::field_fill(t));
        ui.painter().text(
            Pos2::new(rect.left() + 10.0, rect.center().y),
            Align2::LEFT_CENTER,
            format!("+{}", country.dial),
            FontId::proportional(13.5),
            rgb(t.text_secondary),
        );
        let edit_rect = egui::Rect::from_min_max(
            Pos2::new(rect.left() + 56.0, rect.top()),
            Pos2::new(rect.right() - 6.0, rect.bottom()),
        );
        let mut slot = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(edit_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let resp = slot.add(
            egui::TextEdit::singleline(&mut self.pair_national)
                .frame(egui::Frame::NONE)
                .hint_text("812 3456 7890")
                .desired_width(edit_rect.width()),
        );
        if resp.has_focus() {
            ui.painter().rect_stroke(
                rect,
                CornerRadius::same(8),
                Stroke::new(1.5_f32, rgb(t.accent)),
                egui::StrokeKind::Inside,
            );
        }
        resp.changed()
    }

    /// The country picker popup: a search field over the full
    /// country list (flag, name, dial code per row), anchored to
    /// the country selector: it drops from the selector's bottom
    /// edge at the connect card's width, so it opens where the
    /// control that summoned it sits instead of floating over
    /// the card header. Closes on a selection, on Escape, or on
    /// a click outside it (a click that opened it this frame
    /// does not count).
    fn draw_country_picker(&mut self, ctx: &egui::Context) {
        // Whether the picker was already open when this frame
        // began: the click that opens it lands outside the
        // popup by construction and must not close it again in
        // the same frame.
        let was_open = ctx
            .data(|d| d.get_temp::<bool>(egui::Id::unique("country_picker_seen")))
            .unwrap_or(false);
        ctx.data_mut(|d| {
            d.insert_temp(egui::Id::unique("country_picker_seen"), self.country_picker_open);
        });
        // The entrance runs on the shared popup phase (FADE in,
        // EXIT out) like every other menu and dialog; this popup
        // used to appear and vanish instantly. While the exit
        // runs the picker still draws, but its dismissals and
        // selections are ignored, the same contract the other
        // popup_phase surfaces follow.
        let Some(pick_p) =
            crate::anim::popup_phase(ctx, "country_picker", self.country_picker_open, self.motion)
        else {
            return;
        };
        let exiting = !self.country_picker_open;
        let t = self.tokens();
        let area = ctx.content_rect();
        let rect = country_picker_rect(area, self.connect_card_rect, self.country_anchor);
        let size = rect.size();
        if !exiting && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.country_picker_open = false;
            return;
        }
        if !exiting && was_open && ctx.input(|i| i.pointer.any_click()) {
            if let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) {
                if !rect.contains(pos) {
                    self.country_picker_open = false;
                    return;
                }
            }
        }
        let mut chosen: Option<&'static crate::countries::Country> = None;
        egui::Area::new(egui::Id::unique("country_picker"))
            .fixed_pos(rect.min)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if pick_p < 0.999 {
                    ui.set_opacity(pick_p);
                }
                let frame = crate::theme::glass_frame(t, 16)
                    .inner_margin(Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(size.x - 24.0);
                        crate::widgets::text_field(
                            ui,
                            t,
                            &mut self.country_search,
                            "Search country",
                            size.x - 48.0,
                        );
                        ui.add_space(8.0);
                        // Only the visible rows are built
                        // (show_rows): building all ~225 rows
                        // every frame would lay out every flag
                        // glyph in the colour emoji font at once
                        // and overflow the font atlas, which
                        // stalls the frame entirely.
                        let filtered: Vec<&crate::countries::Country> = crate::countries::COUNTRIES
                            .iter()
                            .filter(|c| crate::countries::matches(c, &self.country_search))
                            .collect();
                        egui::ScrollArea::vertical().max_height(262.0).show_rows(
                            ui,
                            38.0,
                            filtered.len(),
                            |ui, range| {
                                for &country in &filtered[range] {
                                    let (row, resp) = ui.allocate_exact_size(
                                        Vec2::new(size.x - 24.0, 38.0),
                                        Sense::click(),
                                    );
                                    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                                    if resp.hovered() {
                                        ui.painter().rect_filled(
                                            row,
                                            CornerRadius::same(8),
                                            rgb(t.bg_panel_alt),
                                        );
                                    }
                                    ui.painter().text(
                                        Pos2::new(row.left() + 8.0, row.center().y),
                                        Align2::LEFT_CENTER,
                                        crate::countries::flag_emoji(country.iso),
                                        FontId::proportional(20.0),
                                        rgb(t.text_primary),
                                    );
                                    ui.painter().text(
                                        Pos2::new(row.left() + 48.0, row.center().y),
                                        Align2::LEFT_CENTER,
                                        country.name,
                                        FontId::proportional(13.5),
                                        rgb(t.text_primary),
                                    );
                                    ui.painter().text(
                                        Pos2::new(row.right() - 8.0, row.center().y),
                                        Align2::RIGHT_CENTER,
                                        format!("+{}", country.dial),
                                        FontId::proportional(13.0),
                                        rgb(t.text_faint),
                                    );
                                    if resp.clicked() {
                                        chosen = Some(country);
                                    }
                                }
                            },
                        );
                    });
                crate::theme::paint_glass_edge(ui.painter(), frame.response.rect, 16.0, t);
            });
        if !exiting {
            if let Some(country) = chosen {
                self.pair_country_iso = country.iso.to_owned();
                self.pair_phone =
                    crate::countries::compose_number(country.dial, &self.pair_national);
                self.country_picker_open = false;
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
        let size = Vec2::new(430.0, if self.can_log_out() { 271.0 } else { 221.0 });
        let rect = egui::Rect::from_min_size(
            Pos2::new(area.center().x - size.x / 2.0, area.top() + 96.0),
            size,
        );
        // The opening frame is exempt: the rail avatar toggle
        // runs earlier in that frame, so the click visible here
        // is the one that opened the menu, not a dismissal.
        if !exiting
            && self.account_menu_open_frame != Some(ctx.cumulative_frame_nr())
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
                                                crate::icons::SIZE_MENU,
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
                            crate::icons::SIZE_MENU,
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
                                crate::icons::SIZE_MENU,
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

/// What one step's disc shows: its number, or a check once the
/// flow has actually passed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepMark {
    Number,
    Check,
}

/// The marks for the connect steps. The steps are actions on
/// the user's phone, which this screen cannot observe one by
/// one, so while pairing is in progress every step is numbered
/// and none is checked: a check would claim progress the flow
/// has not seen. Only a linked session, which proves the whole
/// sequence happened, turns all three into checks.
fn step_marks(complete: bool) -> [StepMark; 3] {
    if complete {
        [StepMark::Check; 3]
    } else {
        [StepMark::Number; 3]
    }
}

/// Paints one step disc: a check on the accent when the step is
/// done, its number on a quiet disc while it is not.
fn paint_step_disc(painter: &egui::Painter, t: Tokens, center: Pos2, index: usize, mark: StepMark) {
    match mark {
        StepMark::Check => {
            painter.circle_filled(center, 10.5, rgb(t.accent));
            draw_icon(
                painter,
                Icon::Check,
                center,
                crate::icons::SIZE_BADGE,
                rgb(t.bg_app),
                rgb(t.accent),
            );
        }
        StepMark::Number => {
            painter.circle_filled(center, 10.5, rgb(t.bg_panel_alt));
            painter.circle_stroke(center, 10.5, Stroke::new(1.0_f32, rgb(t.border)));
            painter.text(
                center,
                Align2::CENTER_CENTER,
                format!("{}", index + 1),
                FontId::proportional(11.5),
                rgb(t.text_secondary),
            );
        }
    }
}

/// Horizontal stepper for the centered connect layout: one
/// cell per step (disc over a short label), hairline connectors
/// at disc height.
fn steps_stepper(ui: &mut egui::Ui, t: Tokens, items: &[&str], complete: bool) {
    let marks = step_marks(complete);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(COL_W, 46.0), Sense::hover());
    let painter = ui.painter();
    let cell_w = rect.width() / items.len() as f32;
    let cy = rect.top() + 11.0;
    for (i, _) in items.iter().enumerate() {
        if i > 0 {
            let from = Pos2::new(rect.left() + cell_w * (i as f32 - 0.5) + 14.0, cy);
            let to = Pos2::new(rect.left() + cell_w * (i as f32 + 0.5) - 14.0, cy);
            let col = if complete { rgb(t.accent) } else { rgb(t.border) };
            painter.line_segment([from, to], Stroke::new(1.0_f32, col));
        }
    }
    for (i, text) in items.iter().enumerate() {
        let center = Pos2::new(rect.left() + cell_w * (i as f32 + 0.5), cy);
        paint_step_disc(painter, t, center, i, marks[i]);
        painter.text(
            Pos2::new(center.x, rect.top() + 36.0),
            Align2::CENTER_CENTER,
            *text,
            FontId::proportional(11.0),
            rgb(t.text_secondary),
        );
    }
}

/// Vertical step list for the wide card's instruction pane: the
/// same discs and marks as the horizontal stepper, one row per
/// step, hairline connectors at disc height between rows.
fn steps_column(ui: &mut egui::Ui, t: Tokens, items: &[&str], complete: bool) {
    let marks = step_marks(complete);
    let row_h = 34.0;
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(WIDE_STEPS_W, row_h * items.len() as f32), Sense::hover());
    let painter = ui.painter();
    let cx = rect.left() + 11.0;
    for (i, _) in items.iter().enumerate() {
        if i > 0 {
            let from = Pos2::new(cx, rect.top() + row_h * (i as f32 - 0.5) + 14.0);
            let to = Pos2::new(cx, rect.top() + row_h * (i as f32 + 0.5) - 14.0);
            let col = if complete { rgb(t.accent) } else { rgb(t.border) };
            painter.line_segment([from, to], Stroke::new(1.0_f32, col));
        }
    }
    for (i, text) in items.iter().enumerate() {
        let center = Pos2::new(cx, rect.top() + row_h * (i as f32 + 0.5));
        paint_step_disc(painter, t, center, i, marks[i]);
        painter.text(
            Pos2::new(rect.left() + 30.0, center.y),
            Align2::LEFT_CENTER,
            *text,
            FontId::proportional(12.0),
            rgb(t.text_secondary),
        );
    }
}

/// The locale key for the connect screen's waiting line, when
/// the flow is in a waiting state: one human sentence that
/// guides the user while the code they need is being prepared.
/// `None` for states the status enum already names in plain
/// language (a code ready to scan or enter, a failure, a link).
fn waiting_line_key(
    mode: PairMode,
    status: &crate::live::LinkStatus,
    has_qr: bool,
) -> Option<&'static str> {
    use crate::live::LinkStatus;
    match status {
        LinkStatus::Starting => Some(match mode {
            PairMode::Qr => "connect.waiting_qr",
            PairMode::Code => "connect.waiting_code",
        }),
        // In code mode there is no QR on screen to scan: while
        // the session waits, it is waiting for the pairing code.
        LinkStatus::WaitingQr => match mode {
            PairMode::Code => Some("connect.waiting_code"),
            // No payload is painted yet, so the line must not
            // claim a scan is being awaited.
            PairMode::Qr if !has_qr => Some("connect.waiting_qr"),
            PairMode::Qr => None,
        },
        _ => None,
    }
}

/// The eight pairing tiles' contents: the code's characters
/// when a real code exists, and empty tiles while the flow is
/// still waiting for one, so nothing on screen pretends a code
/// has arrived before it has.
fn pair_code_tiles(code: Option<&str>) -> [Option<char>; 8] {
    let mut tiles = [None; 8];
    if let Some(code) = code {
        for (tile, ch) in tiles.iter_mut().zip(code.chars()) {
            *tile = Some(ch);
        }
    }
    tiles
}

/// The country picker's rect: the connect card's width, its
/// left edge aligned with the card, dropping 6pt below the
/// country selector. The anchor rects are the previous frame's
/// (the picker is drawn from inside the card, before this
/// frame's card rect exists); before any frame has supplied
/// them it falls back to the column width centred in the
/// window, under the top bar. The rect is clamped on screen.
fn country_picker_rect(
    area: egui::Rect,
    card: Option<egui::Rect>,
    selector: Option<egui::Rect>,
) -> egui::Rect {
    let size = Vec2::new(card.map_or(COL_W, |r| r.width()), 352.0);
    let (left, top) = match (card, selector) {
        (Some(card), Some(selector)) => (card.left(), selector.bottom() + 6.0),
        _ => (area.center().x - size.x / 2.0, area.top() + 110.0),
    };
    let top = top.min(area.bottom() - size.y - 8.0).max(area.top() + 8.0);
    let left = left.min(area.right() - size.x - 8.0).max(area.left() + 8.0);
    egui::Rect::from_min_size(Pos2::new(left, top), size)
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

#[cfg(test)]
mod connect_flow_tests {
    use super::{
        connect_wide, country_picker_rect, pair_code_tiles, step_marks, waiting_line_key, StepMark,
    };
    use crate::live::LinkStatus;
    use crate::state::PairMode;
    use egui::{Pos2, Rect, Vec2};

    #[test]
    fn steps_are_numbered_until_the_flow_actually_links() {
        // In progress, no step may wear a check: the steps happen
        // on the phone, where this screen cannot see them.
        assert_eq!(step_marks(false), [StepMark::Number; 3]);
        // A linked session proves the whole sequence happened.
        assert_eq!(step_marks(true), [StepMark::Check; 3]);
    }

    #[test]
    fn waiting_line_is_one_human_sentence_per_mode() {
        // Starting and waiting-for-payload both read as the
        // preparation line for the mode on screen, never as
        // machinery narration.
        assert_eq!(
            waiting_line_key(PairMode::Qr, &LinkStatus::Starting, false),
            Some("connect.waiting_qr")
        );
        assert_eq!(
            waiting_line_key(PairMode::Qr, &LinkStatus::WaitingQr, false),
            Some("connect.waiting_qr")
        );
        assert_eq!(
            waiting_line_key(PairMode::Code, &LinkStatus::Starting, false),
            Some("connect.waiting_code")
        );
        assert_eq!(
            waiting_line_key(PairMode::Code, &LinkStatus::WaitingQr, false),
            Some("connect.waiting_code")
        );
        // A payload on screen is named by the status itself (a
        // scan is genuinely being awaited now), as are the
        // terminal states.
        assert_eq!(waiting_line_key(PairMode::Qr, &LinkStatus::WaitingQr, true), None);
        assert_eq!(waiting_line_key(PairMode::Qr, &LinkStatus::CodeReady, false), None);
        assert_eq!(waiting_line_key(PairMode::Code, &LinkStatus::Linked, false), None);
    }

    #[test]
    fn pair_tiles_are_empty_while_waiting_and_filled_by_a_real_code() {
        assert_eq!(pair_code_tiles(None), [None; 8]);
        assert_eq!(
            pair_code_tiles(Some("ABCD2345")),
            [
                Some('A'),
                Some('B'),
                Some('C'),
                Some('D'),
                Some('2'),
                Some('3'),
                Some('4'),
                Some('5'),
            ]
        );
    }

    #[test]
    fn picker_drops_from_the_selector_at_the_card_width() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1440.0, 900.0));
        let card = Rect::from_min_size(Pos2::new(360.0, 200.0), Vec2::new(720.0, 400.0));
        let selector = Rect::from_min_size(Pos2::new(400.0, 300.0), Vec2::new(258.0, 40.0));
        let rect = country_picker_rect(area, Some(card), Some(selector));
        assert_eq!(rect.left(), card.left());
        assert_eq!(rect.width(), card.width());
        assert_eq!(rect.top(), selector.bottom() + 6.0);
        // Without anchor rects it still lands on screen at the
        // column width, centred under the top bar.
        let fallback = country_picker_rect(area, None, None);
        assert_eq!(fallback.width(), super::COL_W);
        assert!(area.contains_rect(fallback));
    }

    #[test]
    fn wide_card_only_at_desktop_widths() {
        // The graded portrait and narrow sizes keep the column.
        assert!(!connect_wide(360.0));
        assert!(!connect_wide(390.0));
        assert!(!connect_wide(600.0));
        // Desktop sizes take the two-part card.
        assert!(connect_wide(1180.0));
        assert!(connect_wide(1440.0));
    }
}
