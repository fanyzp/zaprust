//! The composer: the floating glass pill (Composer B: attach,
//! emoji and quick replies buttons plus the text input, with
//! the send/microphone morph in its own always-reserved slot
//! outside the pill), the accessory lines above it, the send
//! path, and the quick replies editor popover.

use super::*;

impl App {
    /// Closes every composer-attached floating surface: the media
    /// picker, the attach menu, the quick replies panel and its
    /// editor. These surfaces are mutually exclusive BY STATE:
    /// at most one of them is open at any moment, so two
    /// translucent panels can never stack over the stream (the
    /// overlap in the owner's report came from the quick replies
    /// bar rendering through the open emoji picker). Every open
    /// site below routes through here first.
    pub(crate) fn close_composer_surfaces(&mut self) {
        self.picker.open = false;
        self.composer.rich_menu_open = false;
        quick_replies::set_panel_open(false);
        quick_replies::set_editor_open(false);
    }

    /// Opens the quick replies editor, closing the panel it edits
    /// and every other composer surface: the editor carries the
    /// full list itself, so nothing is lost by the panel closing,
    /// and the two never overlap on narrow windows.
    fn open_quick_replies_editor(&mut self) {
        if !self.feature_active("quick-replies") {
            return;
        }
        self.close_composer_surfaces();
        quick_replies::set_editor_open(true);
    }

    /// Draws the floating composer stack and measures it: the
    /// measured height becomes the stream's bottom clearance
    /// on the next frame (see draw_conversation).
    pub(crate) fn draw_composer(
        &mut self,
        ui: &mut egui::Ui,
        full_rect: egui::Rect,
        t: Tokens,
        now: f64,
        engaged: &mut bool,
    ) {
        // Composer: a FLOATING pill (the 2026 "liquid glass" chat
        // bar), detached from every edge, soft shadow, messages
        // scroll underneath it. egui has no backdrop blur, so the
        // glass feel is a slightly translucent elevated fill.
        let quick_replies_active = self.feature_active("quick-replies");
        if !quick_replies_active {
            // The feature switch owns the panel and its editor:
            // switching it off while either surface is open must
            // not leave a stale surface behind, so both close
            // here, at render time, before anything can draw.
            quick_replies::set_panel_open(false);
            quick_replies::set_editor_open(false);
        }
        let fmargin = side_margin(full_rect.width());
        // The shortcut hint bar lives in its OWN area under the
        // pill. It must never become a trailing sibling of the
        // multiline input inside the composer area: trailing
        // content in that vertically-unbounded layout re-measures
        // one row taller every frame, an endless growth loop.
        let hint_visible = !self.composer.hint_dismissed && self.recorder.is_none();
        let hint_total = if hint_visible { self.composer.hint_bar_h + 6.0 } else { 0.0 };
        // Fake-frost underlay for the composer zone: a
        // soft chat-background rect under the whole
        // floating stack deepens the frosted read where
        // messages scroll beneath it. Sized from last
        // frame's measured stack height, like the
        // clearance the stream reserves.
        let underlay = egui::Rect::from_min_max(
            Pos2::new(
                full_rect.left() + fmargin - 6.0,
                full_rect.bottom() - 20.0 - self.composer.stack_h - hint_total,
            ),
            Pos2::new(full_rect.right() - fmargin + 6.0, full_rect.bottom() - 8.0),
        );
        ui.painter().rect_filled(underlay, CornerRadius::same(30), crate::theme::glass_underlay(t));
        let composer_area = egui::Area::new(egui::Id::unique("composer_float"))
            .fixed_pos(Pos2::new(
                full_rect.left() + fmargin,
                full_rect.bottom() - 14.0 - hint_total,
            ))
            .pivot(egui::Align2::LEFT_BOTTOM)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                ui.set_width(full_rect.width() - fmargin * 2.0);
                // No quick replies hint line: the triggers
                // are not advertised as a permanent pill
                // above the composer. The feature is found
                // through use (typing "/" opens the panel,
                // the composer button toggles it) and the
                // send path still expands a typed trigger.
                // Native quick replies panel: a row of canned
                // messages above the pill. Tapping one inserts
                // it into the draft; it never sends directly.
                let qr_open = quick_replies_active && quick_replies::panel_open();
                // The phase machine still runs while the feature
                // is off so its exit state drains, but its result
                // is discarded: with the switch off the panel
                // never renders at all.
                let qr_phase = crate::anim::popup_phase(ui.ctx(), "qr_panel", qr_open, self.motion);
                let qr_phase = if quick_replies_active { qr_phase } else { None };
                let qr_exiting = !qr_open;
                if let Some(qr_p) = qr_phase {
                    let items = quick_replies::items();
                    let qr_frame = crate::theme::glass_panel_frame(t, 12)
                        .inner_margin(Margin::symmetric(8, 6))
                        .show(ui, |ui| {
                            if qr_p < 0.999 {
                                ui.set_opacity(qr_p);
                            }
                            ui.horizontal(|ui| {
                                if items.is_empty() {
                                    ui.label(
                                        RichText::new(crate::i18n::t(
                                            "conversation.quick_replies_empty",
                                        ))
                                        .size(11.5)
                                        .color(rgb(t.text_secondary)),
                                    );
                                } else {
                                    let chips_w = (ui.available_width() - 84.0).max(80.0);
                                    ui.allocate_ui_with_layout(
                                        Vec2::new(chips_w, 36.0),
                                        Layout::left_to_right(Align::Center),
                                        |ui| {
                                            egui::ScrollArea::horizontal()
                                                .id_salt("native_qr_chips")
                                                .show(ui, |ui| {
                                                    ui.horizontal(|ui| {
                                                        for item in &items {
                                                            let label: String = item
                                                                .body
                                                                .chars()
                                                                .take(42)
                                                                .collect();
                                                            if crate::widgets::button(
                                                                ui,
                                                                t,
                                                                &label,
                                                                crate::widgets::Btn::Secondary,
                                                            )
                                                            .clicked()
                                                                && !qr_exiting
                                                            {
                                                                if self
                                                                    .composer
                                                                    .draft
                                                                    .trim_start()
                                                                    .starts_with('/')
                                                                    || self
                                                                        .composer
                                                                        .draft
                                                                        .trim()
                                                                        .is_empty()
                                                                {
                                                                    self.composer.draft =
                                                                        item.body.clone();
                                                                } else {
                                                                    self.composer.draft.push(' ');
                                                                    self.composer
                                                                        .draft
                                                                        .push_str(&item.body);
                                                                }
                                                                quick_replies::set_panel_open(
                                                                    false,
                                                                );
                                                            }
                                                        }
                                                    });
                                                });
                                        },
                                    );
                                }
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if crate::widgets::icon_button(
                                        ui,
                                        t,
                                        crate::icons::Icon::X,
                                        28.0,
                                        &crate::i18n::t("conversation.quick_replies"),
                                        None,
                                    )
                                    .clicked()
                                    {
                                        quick_replies::set_panel_open(false);
                                    }
                                    if crate::widgets::button(
                                        ui,
                                        t,
                                        &crate::i18n::t("conversation.quick_replies_edit"),
                                        crate::widgets::Btn::Secondary,
                                    )
                                    .clicked()
                                        && !qr_exiting
                                    {
                                        self.open_quick_replies_editor();
                                    }
                                });
                            });
                        });
                    crate::theme::paint_glass_edge(ui.painter(), qr_frame.response.rect, 12.0, t);
                    // Entrance settle: the gap under the
                    // panel starts 8 px wider and eases
                    // shut, so the panel reads as rising
                    // out of the composer it belongs to.
                    ui.add_space(4.0 + (1.0 - qr_p) * 8.0);
                }
                {
                    // Set inside the row from the text
                    // input's state; the focus ring is
                    // painted after the row, below, and the
                    // send runs there too, because the
                    // morph button lives outside the pill.
                    let mut input_focused = false;
                    let mut enter_send = false;
                    let mut input_changed = false;
                    let mut has_text = false;
                    let mut send_clicked = false;
                    // Composer B: one horizontal row. The
                    // glass pill (the attach and emoji
                    // buttons, the quick replies toggle and
                    // the text input, with the reply and
                    // editing bars above the input) takes
                    // the width left after the external
                    // button's slot. The slot is ALWAYS
                    // reserved at 38 px plus its gap, so
                    // the pill's width never changes when
                    // the button morphs between microphone
                    // and send: zero reflow.
                    let pill_w = (ui.available_width() - 46.0).max(160.0);
                    let pill_rect = {
                        // The pill is the flagship glass
                        // surface: the panel-alt base
                        // tinted toward the seed at the
                        // text-bearing glass alpha, so the
                        // messages scrolling underneath
                        // tint it faintly without staying
                        // legible through it.
                        let pill = egui::Frame::new()
                            .fill(crate::theme::glass_text_tint(t, t.bg_panel_alt))
                            .corner_radius(CornerRadius::same(28))
                            .stroke(Stroke::new(1.0_f32, rgb(t.border)))
                            .shadow(crate::theme::glass_shadow(t))
                            .inner_margin(Margin::symmetric(12, 8))
                            .show(ui, |ui| {
                                ui.set_width(pill_w - 24.0);
                                // Staged media bar: the attach flow's
                                // waiting files. Thumbnails for
                                // photos, an icon tile for documents,
                                // a per-file remove badge, and the
                                // view-once toggle for a single
                                // photo or video.
                                if !self.composer.staged.is_empty() {
                                    ui.horizontal(|ui| {
                                        let mut remove: Option<usize> = None;
                                        for (i, item) in self.composer.staged.iter().enumerate() {
                                            let (tile, _) = ui.allocate_exact_size(
                                                Vec2::splat(44.0),
                                                Sense::hover(),
                                            );
                                            let tile_rect = tile.intersect(ui.clip_rect());
                                            if let Some(tex) = &item.thumb {
                                                ui.painter().image(
                                                    tex.id(),
                                                    tile_rect,
                                                    egui::Rect::from_min_max(
                                                        Pos2::ZERO,
                                                        Pos2::new(1.0, 1.0),
                                                    ),
                                                    Color32::WHITE,
                                                );
                                            } else {
                                                ui.painter().rect_filled(
                                                    tile_rect,
                                                    CornerRadius::same(8),
                                                    rgb(t.bg_panel_alt),
                                                );
                                                crate::icons::draw(
                                                    ui.painter(),
                                                    crate::icons::Icon::Document,
                                                    tile_rect.center(),
                                                    20.0,
                                                    rgb(t.text_secondary),
                                                    rgb(t.bg_panel_alt),
                                                );
                                            }
                                            ui.painter().rect_stroke(
                                                tile_rect,
                                                CornerRadius::same(8),
                                                Stroke::new(1.0_f32, rgb(t.border)),
                                                egui::StrokeKind::Inside,
                                            );
                                            // Remove badge: a small disc
                                            // at the tile's top-right.
                                            let badge_c = Pos2::new(
                                                tile_rect.right() - 2.0,
                                                tile_rect.top() + 2.0,
                                            );
                                            let badge_resp = ui
                                                .allocate_rect(
                                                    egui::Rect::from_center_size(
                                                        badge_c,
                                                        Vec2::splat(16.0),
                                                    ),
                                                    Sense::click(),
                                                )
                                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                                            ui.painter().circle_filled(
                                                badge_c,
                                                8.0,
                                                rgb(t.bg_panel),
                                            );
                                            ui.painter().rect_stroke(
                                                egui::Rect::from_center_size(
                                                    badge_c,
                                                    Vec2::splat(16.0),
                                                ),
                                                CornerRadius::same(8),
                                                Stroke::new(1.0_f32, rgb(t.border)),
                                                egui::StrokeKind::Inside,
                                            );
                                            crate::icons::draw(
                                                ui.painter(),
                                                crate::icons::Icon::X,
                                                badge_c,
                                                10.0,
                                                rgb(t.text_secondary),
                                                rgb(t.bg_panel),
                                            );
                                            if badge_resp.clicked() {
                                                remove = Some(i);
                                            }
                                        }
                                        if let Some(i) = remove {
                                            self.composer.staged.remove(i);
                                        }
                                        if self.composer.staged.len() == 1
                                            && self.composer.staged[0].kind
                                                != crate::state::StagedKind::Document
                                        {
                                            ui.with_layout(
                                                Layout::right_to_left(Align::Center),
                                                |ui| {
                                                    let on = self.composer.staged[0].view_once;
                                                    let (tog, tog_resp) = ui.allocate_exact_size(
                                                        Vec2::splat(30.0),
                                                        Sense::click(),
                                                    );
                                                    let tog_resp = tog_resp
                                                        .on_hover_cursor(
                                                            egui::CursorIcon::PointingHand,
                                                        )
                                                        .on_hover_text(crate::i18n::t(
                                                            "attach.view_once",
                                                        ));
                                                    let shown = crate::anim::swap_factor(
                                                        ui,
                                                        "staged_view_once",
                                                        on,
                                                        self.motion,
                                                    );
                                                    if shown > 0.001 {
                                                        ui.painter().circle_filled(
                                                            tog.center(),
                                                            14.0,
                                                            fade(rgb(t.accent), shown),
                                                        );
                                                    }
                                                    let col = if on {
                                                        Color32::WHITE
                                                    } else {
                                                        rgb(t.text_secondary)
                                                    };
                                                    crate::icons::draw(
                                                        ui.painter(),
                                                        crate::icons::Icon::LooksOne,
                                                        tog.center(),
                                                        18.0,
                                                        col,
                                                        rgb(t.bg_panel_alt),
                                                    );
                                                    if tog_resp.clicked() {
                                                        self.composer.staged[0].view_once = !on;
                                                    }
                                                },
                                            );
                                        }
                                    });
                                    ui.add_space(2.0);
                                }
                                if let Some((_, qsender, qbody)) = self.composer.reply_to.clone() {
                                    // The reply bar speaks the
                                    // quote block's grammar (see
                                    // bubble.rs): a 3 px bar and
                                    // the sender's name in the
                                    // sender's color, the quoted
                                    // text under it.
                                    let qs_color = crate::widgets::sender_color(&qsender, t);
                                    ui.horizontal(|ui| {
                                        let (bar, _) = ui.allocate_exact_size(
                                            Vec2::new(3.0, 30.0),
                                            Sense::hover(),
                                        );
                                        ui.painter().rect_filled(
                                            bar,
                                            CornerRadius::same(1),
                                            qs_color,
                                        );
                                        ui.vertical(|ui| {
                                            ui.label(
                                                RichText::new(&qsender)
                                                    .font(crate::theme::semibold(
                                                        crate::bubble::INNER_TITLE_SIZE,
                                                    ))
                                                    .color(qs_color),
                                            );
                                            ui.label(
                                                RichText::new(
                                                    qbody.chars().take(64).collect::<String>(),
                                                )
                                                .size(crate::bubble::INNER_BODY_SIZE)
                                                .color(rgb(t.text_secondary)),
                                            );
                                        });
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                if crate::widgets::icon_button(
                                                    ui,
                                                    t,
                                                    crate::icons::Icon::X,
                                                    24.0,
                                                    &crate::i18n::t("common.cancel"),
                                                    None,
                                                )
                                                .clicked()
                                                {
                                                    self.composer.reply_to = None;
                                                }
                                            },
                                        );
                                    });
                                    ui.add_space(2.0);
                                }
                                if self.composer.editing_id.is_some() {
                                    // A slim state bar, not an
                                    // instruction sentence: the
                                    // label says what is happening
                                    // and the icon button cancels
                                    // the edit for real.
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(crate::i18n::t(
                                                "conversation.editing_message",
                                            ))
                                            .size(11.5)
                                            .color(rgb(t.text_secondary)),
                                        );
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                if crate::widgets::icon_button(
                                                    ui,
                                                    t,
                                                    crate::icons::Icon::X,
                                                    24.0,
                                                    &crate::i18n::t("common.cancel"),
                                                    None,
                                                )
                                                .clicked()
                                                {
                                                    self.composer.editing_id = None;
                                                    self.composer.draft.clear();
                                                }
                                            },
                                        );
                                    });
                                }
                                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                    if composer_icon(
                                        ui,
                                        crate::icons::Icon::Plus,
                                        t,
                                        &crate::i18n::t("attach.menu"),
                                    ) {
                                        if self.composer.rich_menu_open {
                                            self.composer.rich_menu_open = false;
                                        } else {
                                            self.close_composer_surfaces();
                                            self.composer.rich_menu_open = true;
                                        }
                                    }
                                    if composer_icon(
                                        ui,
                                        crate::icons::Icon::Smile,
                                        t,
                                        &crate::i18n::t("picker.title"),
                                    ) {
                                        if self.picker.open {
                                            self.picker.open = false;
                                        } else {
                                            self.close_composer_surfaces();
                                            self.picker.open = true;
                                        }
                                    }
                                    if quick_replies_active
                                        && composer_icon(
                                            ui,
                                            crate::icons::Icon::Chat,
                                            t,
                                            &crate::i18n::t("conversation.quick_replies"),
                                        )
                                    {
                                        if quick_replies::panel_open() {
                                            quick_replies::set_panel_open(false);
                                        } else {
                                            self.close_composer_surfaces();
                                            quick_replies::set_panel_open(true);
                                        }
                                    }
                                    let rec_secs = (self.motion.duration(120.0) / 1000.0) as f32;
                                    let rec_p = crate::anim::scalar(
                                        ui,
                                        "rec_bar_p",
                                        if self.recorder.is_some() { 1.0 } else { 0.0 },
                                        rec_secs,
                                    );
                                    if rec_p > 0.001 {
                                        // The recording bar takes the
                                        // input row: pulsing dot, the
                                        // running timer, the live level
                                        // waveform, cancel and stop. It
                                        // crossfades in over the input.
                                        ui.scope(|ui| {
                                            if rec_p < 0.999 {
                                                ui.set_opacity(rec_p);
                                            }
                                            draw_recording_bar(self, ui, t, now, engaged);
                                        });
                                        has_text = false;
                                    } else {
                                        // While media is staged the
                                        // input row IS the caption
                                        // field, WhatsApp-style; the
                                        // draft returns once the stage
                                        // clears.
                                        let staged_active = !self.composer.staged.is_empty();
                                        let text = if staged_active {
                                            &mut self.composer.staged_caption
                                        } else {
                                            &mut self.composer.draft
                                        };
                                        // No local style override: the
                                        // composer text inherits the global
                                        // text style and field look from the
                                        // finishing layer in theme.rs. The
                                        // TextEdit paints no background of
                                        // its own (custom Frame::NONE), so
                                        // the pill fill below is the only
                                        // surface and stays as designed.
                                        let edit = egui::TextEdit::multiline(text)
                                            .hint_text(if staged_active {
                                                crate::i18n::t("attach.caption_hint")
                                            } else {
                                                crate::i18n::t("conversation.type_message")
                                            })
                                            .desired_rows(1)
                                            .desired_width((ui.available_width() - 8.0).max(80.0))
                                            .frame(
                                                egui::Frame::NONE
                                                    .inner_margin(Margin::symmetric(4, 8)),
                                            );
                                        let response = ui.add(edit);
                                        input_focused = response.has_focus();
                                        // Read-by-action: focusing or
                                        // typing in the composer is a
                                        // real read action (see live.rs).
                                        if response.gained_focus() || response.changed() {
                                            *engaged = true;
                                        }
                                        // Typing "/" at the start of the
                                        // draft opens the native quick
                                        // replies panel, closing the
                                        // picker or attach menu if one
                                        // is up (one surface at a time).
                                        if quick_replies_active
                                            && !staged_active
                                            && self.composer.draft.starts_with('/')
                                            && !quick_replies::panel_open()
                                        {
                                            self.close_composer_surfaces();
                                            quick_replies::set_panel_open(true);
                                        }
                                        enter_send = self.enter_sends
                                            && response.has_focus()
                                            && ui.input(|i| {
                                                i.key_pressed(egui::Key::Enter)
                                                    && !i.modifiers.shift
                                            });
                                        // Ctrl+V with a picture on the
                                        // clipboard: egui-winit delivers it
                                        // as PasteImage (text pastes stay
                                        // native Paste events).
                                        let pasted_images: Vec<egui::ColorImage> = ui.input(|i| {
                                            i.events
                                                .iter()
                                                .filter_map(|e| match e {
                                                    egui::Event::PasteImage(img) => {
                                                        Some((**img).clone())
                                                    }
                                                    _ => None,
                                                })
                                                .collect()
                                        });
                                        for img in pasted_images {
                                            self.stage_pasted_image(ui.ctx(), img);
                                        }
                                        has_text = if staged_active {
                                            // Staged files send even
                                            // without a caption.
                                            true
                                        } else {
                                            !self.composer.draft.trim().is_empty()
                                        };
                                        input_changed = response.changed();
                                    }
                                });
                                // The shortcut hint as the pill's
                                // footer line, the reference
                                // wireframe's one-line keyboard help.
                            });
                        pill.response.rect
                    };
                    // The morph button gets its own small Area in
                    // the reserved slot: bottom-aligned with the
                    // pill however tall the pill grows (reply
                    // bar, multi-line draft), and the pill never
                    // moves because its width already excludes
                    // the slot.
                    let recording = self.recorder.is_some();
                    egui::Area::new(egui::Id::unique("composer_action"))
                        .fixed_pos(Pos2::new(pill_rect.right() + 8.0, pill_rect.bottom() - 46.0))
                        .order(egui::Order::Foreground)
                        .show(ui.ctx(), |ui| {
                            // Send: a proper round accent button with a
                            // paper-plane drawn as vectors (the font's
                            // arrow glyphs look cheap). Empty draft:
                            // a plain microphone in a hover circle.
                            // While a recording runs the slot shows
                            // the red stop disc instead.
                            let (srect, sresp) =
                                ui.allocate_exact_size(Vec2::splat(38.0), Sense::click());
                            let sresp = sresp.on_hover_cursor(egui::CursorIcon::PointingHand);
                            let sc = srect.center();
                            // Motion: the send button pulses once per send.
                            let pulse_p = crate::motion::Anim::drive(
                                &mut self.anim.send_pulse,
                                ui.ctx(),
                                now,
                            );
                            // sin(pi * p) is 0 at rest (p = 1),
                            // so the button is steady when idle.
                            let pulse_r = 19.0 - 3.0 * (pulse_p * std::f32::consts::PI).sin();
                            if recording {
                                ui.painter().circle_filled(
                                    sc,
                                    19.0,
                                    rgb(if sresp.is_pointer_button_down_on() {
                                        core::theme::pressed_step(t.danger)
                                    } else {
                                        t.danger
                                    }),
                                );
                                crate::icons::draw(
                                    ui.painter(),
                                    crate::icons::Icon::Stop,
                                    sc,
                                    22.0,
                                    Color32::WHITE,
                                    rgb(t.danger),
                                );
                            } else {
                                // The microphone and the send
                                // button morph into each other
                                // instead of swapping in one
                                // frame: as the draft gains or
                                // loses its text, the accent
                                // disc grows out of (or shrinks
                                // back into) the mic's hover
                                // circle while the two glyphs
                                // crossfade and counter-scale.
                                let swap = crate::anim::swap_factor(
                                    ui,
                                    "composer_send_swap",
                                    has_text,
                                    self.motion,
                                );
                                if swap < 0.999 {
                                    let mic_a = 1.0 - swap;
                                    if sresp.is_pointer_button_down_on() {
                                        ui.painter().circle_filled(
                                            sc,
                                            19.0,
                                            fade(rgb(core::theme::pressed_step(t.border)), mic_a),
                                        );
                                    } else if sresp.hovered() {
                                        ui.painter().circle_filled(
                                            sc,
                                            19.0,
                                            fade(rgb(t.border), mic_a),
                                        );
                                    }
                                    let mic_col = rgb(t.text_secondary);
                                    crate::icons::draw(
                                        ui.painter(),
                                        crate::icons::Icon::Mic,
                                        sc,
                                        21.0 * (1.0 - 0.25 * swap),
                                        Color32::from_rgba_unmultiplied(
                                            mic_col.r(),
                                            mic_col.g(),
                                            mic_col.b(),
                                            (255.0 * mic_a) as u8,
                                        ),
                                        rgb(t.bg_panel_alt),
                                    );
                                }
                                if swap > 0.001 {
                                    let send_fill = if sresp.is_pointer_button_down_on() {
                                        core::theme::pressed_step(t.accent)
                                    } else {
                                        t.accent
                                    };
                                    ui.painter().circle_filled(sc, pulse_r * swap, rgb(send_fill));
                                    // The paper plane (drawn as
                                    // vectors; the font's arrow
                                    // glyphs look cheap) scales
                                    // up from three-quarter size
                                    // as it fades in.
                                    let gs = 0.7 + 0.3 * swap;
                                    let plane = Color32::from_rgba_unmultiplied(
                                        255,
                                        255,
                                        255,
                                        (255.0 * swap) as u8,
                                    );
                                    let nose = sc + Vec2::new(10.0 * gs, 0.0);
                                    let tail_top = sc + Vec2::new(-8.0 * gs, -8.0 * gs);
                                    let tail_bot = sc + Vec2::new(-8.0 * gs, 8.0 * gs);
                                    let inner = sc + Vec2::new(-2.5 * gs, 0.0);
                                    ui.painter().add(egui::Shape::convex_polygon(
                                        vec![nose, tail_top, inner],
                                        plane,
                                        Stroke::NONE,
                                    ));
                                    ui.painter().add(egui::Shape::convex_polygon(
                                        vec![nose, inner, tail_bot],
                                        plane,
                                        Stroke::NONE,
                                    ));
                                }
                            }
                            send_clicked = sresp.clicked();
                        });
                    if send_clicked && recording {
                        // The stop disc ends and sends the note.
                        self.finish_recording(engaged);
                    } else if send_clicked && !has_text && !recording {
                        // The idle mic starts a voice recording.
                        self.start_recording();
                    }
                    if has_text && (send_clicked || enter_send) && !recording {
                        self.anim.send_pulse =
                            Some(self.fresh_anim(crate::motion::dur::PULSE, ui.ctx()));
                    }
                    if !recording && (send_clicked || (enter_send && has_text)) {
                        if !self.composer.staged.is_empty() {
                            self.send_staged(engaged);
                        } else {
                            let raw = self.composer.draft.trim().trim_end_matches('\n').to_owned();
                            if !raw.is_empty() {
                                // Quick Replies expansion
                                // runs through the feature
                                // guard (see state.rs).
                                let body = if quick_replies_active {
                                    self.guard_feature("quick-replies", |app| {
                                        app.expand_quick_reply(&raw)
                                    })
                                    .unwrap_or(raw)
                                } else {
                                    raw
                                };
                                if let Some(edit_id) = self.composer.editing_id.take() {
                                    // Editing one of my
                                    // messages replaces
                                    // the send entirely.
                                    self.apply_edit(edit_id, &body);
                                } else if self.live_mode {
                                    // Resolve the reply target's protocol
                                    // id and author JID so the quote rides
                                    // on the wire (extendedText context),
                                    // not just on the local echo.
                                    let quoted =
                                        self.composer.reply_to.as_ref().and_then(|(id, _, qb)| {
                                            let message_id = self.protocol_ids.get(id)?.clone();
                                            let sender_jid = self.sender_jids.get(id).cloned();
                                            Some(backend::QuotedRef {
                                                message_id,
                                                sender_jid,
                                                excerpt: qb.chars().take(200).collect(),
                                            })
                                        });
                                    self.send_live_text(&body, quoted);
                                    // Attach the quote
                                    // context to the local
                                    // echo; the backend's
                                    // own upsert replaces it.
                                    if let Some((_, qs, qb)) = self.composer.reply_to.take() {
                                        if let Some(echo) = self.conversation.last_mut() {
                                            echo.quoted_sender = Some(qs);
                                            echo.quoted_body = Some(qb);
                                        }
                                    }
                                    self.set_typing(false);
                                } else {
                                    self.sent_counter += 1;
                                    // In a disappearing chat the
                                    // demo send runs on the real
                                    // clock (see demo_send_stamp).
                                    let (stamp, expires_at) = self.demo_send_stamp();
                                    let mut msg = Message {
                                        delivery: Some(Delivery::Delivered),
                                        expires_at,
                                        ..Message::mine(
                                            900 + self.sent_counter,
                                            body.clone(),
                                            stamp.clone(),
                                        )
                                    };
                                    if let Some((_, qs, qb)) = self.composer.reply_to.take() {
                                        msg.quoted_sender = Some(qs);
                                        msg.quoted_body = Some(qb);
                                    }
                                    self.conversation.push(msg.clone());
                                    let chat_id = self.selected_chat;
                                    if let Some(chat) =
                                        self.chats.iter_mut().find(|c| c.id == chat_id)
                                    {
                                        chat.messages.push(msg);
                                        chat.preview = body;
                                        chat.time = stamp;
                                        chat.sync_preview_from_last();
                                    } else if let Some(chat) =
                                        self.detached_chat.as_mut().filter(|c| c.id == chat_id)
                                    {
                                        // Sending inside the
                                        // announcement view
                                        // updates the detached
                                        // copy the same way.
                                        chat.messages.push(msg);
                                        chat.preview = body;
                                        chat.time = stamp;
                                        chat.sync_preview_from_last();
                                    }
                                }
                                // Read-by-action: sending
                                // a reply reads the chat.
                                *engaged = true;
                            }
                            self.composer.draft.clear();
                        }
                    }
                    // Typing presence: composing
                    // while the draft is non-empty
                    // (live mode, toggle-gated).
                    if input_changed && self.live_mode {
                        self.set_typing(!self.composer.draft.trim().is_empty());
                    }
                    // A recording that ran into the cap auto-sends:
                    // the capture stopped growing, so the honest end
                    // of the note is sending it, not dropping audio.
                    if self.recorder.as_ref().is_some_and(crate::recorder::Recorder::capped) {
                        self.finish_recording(engaged);
                    }
                    crate::theme::paint_glass_edge(ui.painter(), pill_rect, 28.0, t);
                    // Focus affordance: while the input
                    // holds the keyboard, the accent ring
                    // traces the pill, exactly the focus
                    // language the shared text field and
                    // the chat search pill already speak
                    // (1.5 px accent, inside stroke), so
                    // every text entry in the app shows
                    // focus the same way.
                    if input_focused {
                        ui.painter().rect_stroke(
                            pill_rect,
                            CornerRadius::same(28),
                            Stroke::new(1.5_f32, rgb(t.accent)),
                            egui::StrokeKind::Inside,
                        );
                    }
                }
            });
        // The measured stack height becomes the scroll
        // area's bottom clearance on the next frame (see
        // the top of this function). The stored value
        // eases toward the measured one over a short run:
        // when the pill grows a line (or a panel opens
        // above it), the clearance, the frost underlay and
        // the scroll-to-bottom button glide to the new
        // height instead of jumping a whole line in one
        // frame. Motion Off stores the measurement as-is.
        let measured_stack_h = composer_area.response.rect.height();
        if self.motion == crate::motion::MotionSetting::Off {
            self.composer.stack_h = measured_stack_h;
        } else {
            let secs = (self.motion.duration(120.0) / 1000.0) as f32;
            self.composer.stack_h =
                crate::anim::scalar(ui, "composer_stack_h", measured_stack_h, secs);
        }
        // The hint bar's own area: a single natural-height row (no
        // multiline input inside, so its measurement is stable).
        // Its measured height feeds the composer anchor, the
        // underlay and the stream clearance next frame.
        if hint_visible {
            let hint_area = egui::Area::new(egui::Id::unique("composer_hint"))
                .fixed_pos(Pos2::new(full_rect.left() + fmargin, full_rect.bottom() - 10.0))
                .pivot(egui::Align2::LEFT_BOTTOM)
                .order(egui::Order::Foreground)
                .show(ui.ctx(), |ui| {
                    // Same width as the pill above (the pill reserves
                    // 46 px for the morph slot), one quiet row: no
                    // border, faint text, keyboard glyph far right.
                    ui.set_width(full_rect.width() - fmargin * 2.0 - 46.0);
                    egui::Frame::new()
                        .fill(crate::theme::glass_fill(t))
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(Margin::symmetric(10, 3))
                        .show(ui, |ui| {
                            ui.with_layout(
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    if crate::widgets::icon_button(
                                        ui,
                                        t,
                                        crate::icons::Icon::X,
                                        18.0,
                                        &crate::i18n::t("composer.hint_dismiss"),
                                        None,
                                    )
                                    .clicked()
                                    {
                                        self.composer.hint_dismissed = true;
                                        self.persist_settings();
                                    }
                                    ui.add_space(2.0);
                                    ui.label(
                                        RichText::new(crate::i18n::t("composer.hint"))
                                            .size(10.5)
                                            .color(rgb(t.text_faint)),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            let (krect, _) = ui.allocate_exact_size(
                                                Vec2::splat(18.0),
                                                Sense::hover(),
                                            );
                                            crate::icons::draw(
                                                ui.painter(),
                                                crate::icons::Icon::Keyboard,
                                                krect.center(),
                                                15.0,
                                                rgb(t.text_faint),
                                                crate::theme::glass_fill(t),
                                            );
                                        },
                                    );
                                },
                            );
                        });
                });
            let measured_hint_h = hint_area.response.rect.height();
            if self.motion == crate::motion::MotionSetting::Off {
                self.composer.hint_bar_h = measured_hint_h;
            } else {
                let secs = (self.motion.duration(120.0) / 1000.0) as f32;
                self.composer.hint_bar_h =
                    crate::anim::scalar(ui, "hint_bar_h", measured_hint_h, secs);
            }
        } else {
            self.composer.hint_bar_h = 0.0;
        }
    }

    /// The quick replies editor popover: the list with a remove
    /// button per entry and an add row, anchored above the
    /// composer so it reads as part of the panel it edits.
    pub(crate) fn draw_quick_replies_editor(&mut self, ui: &mut egui::Ui, t: Tokens) {
        // Quick replies editor popover: the list with a
        // remove button per entry and an add row. Anchored
        // above the composer so it reads as part of the
        // panel it edits.
        if !self.feature_active("quick-replies") {
            // Same switch as the panel: with the feature off the
            // editor cannot open, and an editor left open when
            // the switch flips closes here. The phase machine
            // still ticks once with closed state so its exit
            // state drains instead of replaying later.
            quick_replies::set_editor_open(false);
            quick_replies::set_panel_open(false);
            let _ = crate::anim::popup_phase(ui.ctx(), "qr_editor", false, self.motion);
            return;
        }
        let qr_editor_open = quick_replies::editor_open();
        let ed_phase = crate::anim::popup_phase(ui.ctx(), "qr_editor", qr_editor_open, self.motion);
        if let Some(ed_p) = ed_phase {
            let ed_exiting = !qr_editor_open;
            // The window's own open flag stays up while
            // the exit runs; its close button feeds the
            // module state, which starts the exit.
            let mut win_open = true;
            let qr_win = egui::Window::new(crate::i18n::t("conversation.quick_replies"))
                .open(&mut win_open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_BOTTOM, Vec2::new(0.0, -108.0))
                .default_width(330.0)
                .frame(
                    crate::theme::glass_panel_frame(t, 12).inner_margin(Margin::symmetric(12, 10)),
                )
                .show(ui.ctx(), |ui| {
                    if ed_p < 0.999 {
                        ui.set_opacity(ed_p);
                    }
                    let items = quick_replies::items();
                    if items.is_empty() {
                        ui.label(
                            RichText::new(crate::i18n::t("conversation.quick_replies_empty"))
                                .size(12.0)
                                .color(rgb(t.text_secondary)),
                        );
                    }
                    for (i, item) in items.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if crate::widgets::icon_button(
                                ui,
                                t,
                                crate::icons::Icon::X,
                                24.0,
                                &crate::i18n::t("conversation.quick_replies"),
                                None,
                            )
                            .clicked()
                                && !ed_exiting
                            {
                                quick_replies::remove(i);
                            }
                            let label = if item.trigger.is_empty() {
                                item.body.clone()
                            } else {
                                format!("{}  {}", item.trigger, item.body)
                            };
                            ui.label(RichText::new(label).size(13.0));
                        });
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let mut draft = quick_replies::editor_draft();
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut draft)
                                .hint_text(crate::i18n::t("conversation.quick_replies_new"))
                                .desired_width(190.0),
                        );
                        quick_replies::set_editor_draft(draft.clone());
                        let add_clicked = crate::widgets::button(
                            ui,
                            t,
                            &crate::i18n::t("conversation.quick_replies_add"),
                            crate::widgets::Btn::Primary,
                        )
                        .clicked();
                        let enter =
                            resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if (add_clicked || enter) && !ed_exiting && !draft.trim().is_empty() {
                            quick_replies::add(&draft);
                            quick_replies::set_editor_draft(String::new());
                        }
                    });
                });
            if let Some(win) = &qr_win {
                let painter = ui.ctx().layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::unique("zaprust_glass_edge"),
                ));
                crate::theme::paint_glass_edge(&painter, win.response.rect, 12.0, t);
            }
            if !win_open {
                quick_replies::set_editor_open(false);
            }
        }
    }
}

/// A round hoverable icon button used inside the composer pill.
/// Returns true when clicked. Delegates to the shared
/// [`crate::widgets::icon_button`] so the composer gets the same
/// hover wash, pressed step, hand cursor and tooltip treatment as
/// every other icon button in the app (at 36 px the shared icon
/// scale works out to the same 21 px glyph this drew by hand).
fn composer_icon(ui: &mut egui::Ui, icon: crate::icons::Icon, t: Tokens, tooltip: &str) -> bool {
    crate::widgets::icon_button(ui, t, icon, 36.0, tooltip, None).clicked()
}

/// `color` at `f` of its own alpha, for crossfades between two
/// states of one control (the composer's microphone/send morph).
/// Callers pass opaque colors, so scaling the alpha channel is
/// the whole fade.
fn fade(color: Color32, f: f32) -> Color32 {
    let a = (f32::from(color.a()) * f.clamp(0.0, 1.0)) as u8;
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a)
}

/// The live recording bar: a pulsing red dot, the running timer,
/// the level waveform as it fills, and cancel/stop controls. It
/// draws in place of the composer input while a voice note records.
fn draw_recording_bar(app: &mut App, ui: &mut egui::Ui, t: Tokens, now: f64, engaged: &mut bool) {
    let secs = app.recorder.as_ref().map_or(0, |r| r.elapsed_secs());
    let levels = app.recorder.as_ref().map_or(Vec::new(), |r| r.levels(24));
    let mut cancel = false;
    let mut stop = false;
    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
        // The pulsing red dot: a soft two-beat breathing alpha so
        // the bar reads as live without any hard blink.
        let (dot, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
        let pulse = 0.55 + 0.45 * f32::sin((now * 2.4) as f32);
        let red = rgb(t.danger);
        ui.painter().circle_filled(dot.center(), 5.5, fade(red, pulse));
        let label = format!("{:02}:{:02}", secs / 60, secs % 60);
        ui.label(
            RichText::new(label).font(crate::theme::semibold(13.0)).color(rgb(t.text_primary)),
        );
        // The level waveform grows across whatever width the row
        // still has, leaving room for the two controls.
        let wave_w = (ui.available_width() - 84.0).max(60.0);
        let (wave, _) = ui.allocate_exact_size(Vec2::new(wave_w, 22.0), Sense::hover());
        let bars = levels.len().max(1);
        let bw = wave.width() / bars as f32;
        for (i, level) in levels.iter().enumerate() {
            let h = (wave.height() * level.clamp(0.04, 1.0)).max(2.0);
            let x = wave.left() + i as f32 * bw;
            let rect = egui::Rect::from_min_size(
                Pos2::new(x + 0.5, wave.center().y - h / 2.0),
                Vec2::new((bw - 1.5).max(1.0), h),
            );
            ui.painter().rect_filled(rect, CornerRadius::same(1), rgb(t.accent));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if crate::widgets::icon_button(
                ui,
                t,
                crate::icons::Icon::X,
                30.0,
                &crate::i18n::t("attach.recording_cancel"),
                None,
            )
            .clicked()
            {
                cancel = true;
            }
            // Stop: the red disc that ends and sends the note,
            // mirroring the morph slot's recording state.
            let (srect, sresp) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
            let sresp = sresp.on_hover_cursor(egui::CursorIcon::PointingHand);
            ui.painter().circle_filled(
                srect.center(),
                14.0,
                rgb(if sresp.is_pointer_button_down_on() {
                    core::theme::pressed_step(t.danger)
                } else {
                    t.danger
                }),
            );
            crate::icons::draw(
                ui.painter(),
                crate::icons::Icon::Stop,
                srect.center(),
                18.0,
                Color32::WHITE,
                rgb(t.danger),
            );
            if sresp.clicked() {
                stop = true;
            }
        });
    });
    if cancel {
        app.cancel_recording();
    } else if stop {
        app.finish_recording(engaged);
    }
}
