//! The composer: the floating glass pill (Composer B: attach
//! and emoji buttons plus the text input, with the
//! send/microphone morph in its own always-reserved slot
//! outside the pill), the accessory lines above it, and the
//! send path.

use super::*;

impl App {
    /// Closes every composer-attached floating surface: the media
    /// picker and the attach menu. These surfaces are mutually
    /// exclusive BY STATE:
    /// at most one of them is open at any moment, so two
    /// translucent panels can never stack over the stream (the
    /// overlap in the owner's report came from a floating
    /// bar rendering through the open emoji picker). Every open
    /// site below routes through here first.
    pub(crate) fn close_composer_surfaces(&mut self) {
        self.picker.open = false;
        self.composer.rich_menu_open = false;
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
        let fmargin = side_margin(full_rect.width());
        // Fake-frost underlay for the composer zone: a
        // soft chat-background rect under the whole
        // floating stack deepens the frosted read where
        // messages scroll beneath it. Sized from last
        // frame's measured stack height, like the
        // clearance the stream reserves.
        let underlay = egui::Rect::from_min_max(
            Pos2::new(
                full_rect.left() + fmargin - 6.0,
                full_rect.bottom() - (COMPOSER_LIFT + 6.0) - self.composer.stack_h,
            ),
            Pos2::new(full_rect.right() - fmargin + 6.0, full_rect.bottom() - 8.0),
        );
        ui.painter().rect_filled(underlay, CornerRadius::same(30), crate::theme::glass_underlay(t));
        let composer_area = egui::Area::new(egui::Id::unique("composer_float"))
            .fixed_pos(Pos2::new(full_rect.left() + fmargin, full_rect.bottom() - COMPOSER_LIFT))
            .pivot(egui::Align2::LEFT_BOTTOM)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                ui.set_width(full_rect.width() - fmargin * 2.0);
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
                    // buttons and
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
                            .shadow(crate::theme::composer_shadow(t))
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
                                        let mut edit: Option<usize> = None;
                                        for (i, item) in self.composer.staged.iter().enumerate() {
                                            // A staged photo's tile is
                                            // the photo editor's entry
                                            // (section 25): the tile
                                            // itself clicks open on it.
                                            let editable =
                                                crate::attach::staged_photo_editable(item);
                                            let (tile, tile_resp) = ui.allocate_exact_size(
                                                Vec2::splat(44.0),
                                                if editable {
                                                    Sense::click()
                                                } else {
                                                    Sense::hover()
                                                },
                                            );
                                            let tile_rect = tile.intersect(ui.clip_rect());
                                            // The remove badge sits on the
                                            // tile's top-right corner: a
                                            // click there is the badge's,
                                            // never the editor's.
                                            let badge_c = Pos2::new(
                                                tile_rect.right() - 2.0,
                                                tile_rect.top() + 2.0,
                                            );
                                            let tile_resp = tile_resp
                                                .on_hover_text(crate::i18n::t(
                                                    "composer.edit_photo",
                                                ))
                                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                                            if editable
                                                && tile_resp.clicked()
                                                && tile_resp
                                                    .interact_pointer_pos()
                                                    .is_some_and(|p| p.distance(badge_c) > 10.0)
                                            {
                                                edit = Some(i);
                                            }
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
                                                    CornerRadius::same(crate::theme::RADIUS_MICRO),
                                                    rgb(t.bg_panel_alt),
                                                );
                                                crate::icons::draw(
                                                    ui.painter(),
                                                    crate::icons::Icon::Document,
                                                    tile_rect.center(),
                                                    crate::icons::SIZE_CONTROL,
                                                    rgb(t.text_secondary),
                                                    rgb(t.bg_panel_alt),
                                                );
                                            }
                                            ui.painter().rect_stroke(
                                                tile_rect,
                                                CornerRadius::same(crate::theme::RADIUS_MICRO),
                                                Stroke::new(1.0_f32, rgb(t.border)),
                                                egui::StrokeKind::Inside,
                                            );
                                            // Remove badge: a small disc
                                            // at the tile's top-right (its
                                            // centre was computed with the
                                            // tile above).
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
                                                CornerRadius::same(crate::theme::RADIUS_MICRO),
                                                Stroke::new(1.0_f32, rgb(t.border)),
                                                egui::StrokeKind::Inside,
                                            );
                                            crate::icons::draw(
                                                ui.painter(),
                                                crate::icons::Icon::X,
                                                badge_c,
                                                crate::icons::SIZE_BADGE,
                                                rgb(t.text_secondary),
                                                rgb(t.bg_panel),
                                            );
                                            if badge_resp.clicked() {
                                                remove = Some(i);
                                            }
                                        }
                                        if let Some(i) = remove {
                                            let item = self.composer.staged.remove(i);
                                            // Unstaging ends the item's life: a
                                            // pasted temp file backing it is
                                            // deleted (picked files are not).
                                            crate::attach::discard_pasted_temp(&item.path);
                                        }
                                        if let Some(i) = edit {
                                            self.open_photo_editor(i);
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
                                                        crate::icons::SIZE_MENU,
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
                                    ui.add_space(SP_HALF);
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
                                    ui.add_space(SP_HALF);
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
                                            .size(12.0)
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
                                                    self.persist_settings();
                                                }
                                            },
                                        );
                                    });
                                }
                                // The input row is allocated at an
                                // exact height computed from the
                                // CURRENT draft (see
                                // composer_row_height). A plain
                                // with_layout row is offered the
                                // floating Area's height, which the
                                // Area carries over from the last
                                // frame, and the row reports that
                                // offered height back as its own:
                                // once a multiline draft had grown
                                // the pill, the row (and so the
                                // pill) stayed that tall after the
                                // draft was cleared or sent, with
                                // the icons floating in the middle
                                // of the empty box. An exact
                                // allocation makes the height a
                                // pure function of the draft, so
                                // the pill shrinks back the frame
                                // the draft does.
                                let staged_active = !self.composer.staged.is_empty();
                                let rec_secs = (self.motion.duration(120.0) / 1000.0) as f32;
                                let rec_p = crate::anim::scalar(
                                    ui,
                                    "rec_bar_p",
                                    if self.recorder.is_some() { 1.0 } else { 0.0 },
                                    rec_secs,
                                );
                                let line_height = composer_line_height(ui);
                                let (field_rows, field_h) = if rec_p > 0.001 {
                                    (1, composer_field_height(1, line_height))
                                } else {
                                    let active_text: &str = if staged_active {
                                        &self.composer.staged_caption
                                    } else {
                                        &self.composer.draft
                                    };
                                    // The width the text field
                                    // will get inside the row:
                                    // the row's width minus the
                                    // icon buttons ahead of it
                                    // (36 px each plus one item
                                    // spacing each) and the
                                    // field's own 8 px reserve,
                                    // mirroring the desired_width
                                    // call below.
                                    let icons = 2.0;
                                    let consumed = icons * (36.0 + ui.spacing().item_spacing.x);
                                    let field_w = (ui.available_width() - consumed - 8.0).max(80.0);
                                    let rows = composer_visual_rows(
                                        ui,
                                        active_text,
                                        (field_w - 8.0).max(1.0),
                                        line_height,
                                    );
                                    (rows, composer_field_height(rows, line_height))
                                };
                                let row_h = composer_row_height(field_rows, line_height);
                                ui.allocate_ui_with_layout(
                                    Vec2::new(ui.available_width(), row_h),
                                    Layout::left_to_right(Align::Center),
                                    |ui| {
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
                                            // clears. (staged_active is
                                            // computed above the row, with
                                            // the row height.)
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
                                                .desired_rows(clamped_composer_rows(field_rows))
                                                .desired_width(
                                                    (ui.available_width() - 8.0).max(80.0),
                                                )
                                                .frame(
                                                    egui::Frame::NONE
                                                        .inner_margin(Margin::symmetric(4, 8)),
                                                );
                                            // Past the row cap the field
                                            // scrolls internally instead
                                            // of growing the pill: the
                                            // scroll area is exactly the
                                            // field height the row was
                                            // sized for, and the text
                                            // edit's scroll-to-cursor
                                            // keeps the caret visible
                                            // while typing. The
                                            // min-scrolled height must be
                                            // zeroed: egui's 64 px default
                                            // would floor a one-line
                                            // field at 64 px tall.
                                            let response = egui::ScrollArea::vertical()
                                                .id_salt("composer_field")
                                                .max_height(field_h)
                                                .min_scrolled_height(0.0)
                                                .auto_shrink([true, true])
                                                .show(ui, |ui| ui.add(edit))
                                                .inner;
                                            input_focused = response.has_focus();
                                            // Read-by-action: focusing or
                                            // typing in the composer is a
                                            // real read action (see live.rs).
                                            if response.gained_focus() || response.changed() {
                                                *engaged = true;
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
                                            let pasted_images: Vec<egui::ColorImage> =
                                                ui.input(|i| {
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
                                    },
                                );
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
                                    crate::icons::SIZE_CONTROL,
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
                                    // The mic is a real button, not a
                                    // bare glyph on the chat
                                    // background: it rests on the same
                                    // filled disc surface as the
                                    // app's other round icon buttons
                                    // (a panel-alt fill with a
                                    // hairline edge), stepping to the
                                    // hover and pressed fills under
                                    // the pointer. The whole surface
                                    // fades with the send morph.
                                    let mic_fill = if sresp.is_pointer_button_down_on() {
                                        core::theme::pressed_step(t.bg_panel_alt)
                                    } else if sresp.hovered() {
                                        core::theme::hover_step(t.bg_panel_alt)
                                    } else {
                                        t.bg_panel_alt
                                    };
                                    ui.painter().circle_filled(
                                        sc,
                                        19.0,
                                        fade(rgb(mic_fill), mic_a),
                                    );
                                    ui.painter().circle_stroke(
                                        sc,
                                        19.0,
                                        Stroke::new(1.0_f32, fade(rgb(t.border), mic_a)),
                                    );
                                    let mic_col = rgb(t.text_secondary);
                                    crate::icons::draw(
                                        ui.painter(),
                                        crate::icons::Icon::Mic,
                                        sc,
                                        crate::icons::SIZE_CONTROL * (1.0 - 0.25 * swap),
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
                                    // The paper plane is the icon
                                    // font's send glyph, the same
                                    // family as the mic it morphs
                                    // with; it scales up from
                                    // three-quarter size as it
                                    // fades in.
                                    let gs = 0.7 + 0.3 * swap;
                                    let plane = Color32::from_rgba_unmultiplied(
                                        255,
                                        255,
                                        255,
                                        (255.0 * swap) as u8,
                                    );
                                    crate::icons::draw(
                                        ui.painter(),
                                        crate::icons::Icon::Send,
                                        sc,
                                        21.0 * gs,
                                        plane,
                                        rgb(send_fill),
                                    );
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
                                // Quick replies expand in
                                // the composer popup while
                                // typing (below), never
                                // invisibly at send time: what
                                // sends is the visible text.
                                let body = raw;
                                // The reply target is honored only
                                // while its message is in the open
                                // conversation: the per-chat parking
                                // keeps it local, and this check is
                                // the boundary behind the parking, so
                                // a foreign quote can neither ride on
                                // the wire nor decorate the local echo.
                                let reply_in_chat = self
                                    .composer
                                    .reply_to
                                    .as_ref()
                                    .is_some_and(|(id, _, _)| self.in_open_conversation(*id));
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
                                    let quoted = self.live_quote_for_reply();
                                    self.send_live_text(&body, quoted);
                                    // Attach the quote
                                    // context to the local
                                    // echo; the backend's
                                    // own upsert replaces it.
                                    if let Some((_, qs, qb)) = self.composer.reply_to.take() {
                                        if reply_in_chat {
                                            if let Some(echo) = self.conversation.last_mut() {
                                                echo.quoted_sender = Some(qs);
                                                echo.quoted_body = Some(qb);
                                            }
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
                                        if reply_in_chat {
                                            msg.quoted_sender = Some(qs);
                                            msg.quoted_body = Some(qb);
                                        }
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
                            // The sent draft leaves the
                            // persisted set with the send, or a
                            // restart would resurrect it.
                            self.persist_settings();
                        }
                    }
                    // Typing presence: composing
                    // while the draft is non-empty
                    // (live mode, toggle-gated).
                    if input_changed && self.live_mode {
                        self.set_typing(!self.composer.draft.trim().is_empty());
                    }
                    // Draft persistence: the settings document
                    // carries the parked drafts plus the open
                    // one, so a draft typed here survives a
                    // restart. While media is staged the input
                    // edits the staged caption instead, which is
                    // not a draft and changes nothing here.
                    if input_changed && self.composer.staged.is_empty() {
                        self.persist_settings();
                    }
                    // A recording that ran into the cap auto-sends:
                    // the capture stopped growing, so the honest end
                    // of the note is sending it, not dropping audio.
                    if self.recorder.as_ref().is_some_and(crate::recorder::Recorder::capped) {
                        self.finish_recording(engaged);
                    }
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

        // The quick-reply popup (parity P10, row 132): while
        // the draft is a slash token and the account has
        // synced replies, the matches float above the
        // composer. Picking one replaces the draft with the
        // reply's text; nothing expands invisibly at send
        // time.
        if self.live.is_some() && !self.business.quick_replies.is_empty() {
            let matches = crate::live::quick_reply_matches(
                &self.business.quick_replies,
                &self.composer.draft,
            );
            if !matches.is_empty() {
                let mut picked: Option<String> = None;
                egui::Area::new(egui::Id::unique("qr_popup"))
                    .fixed_pos(Pos2::new(
                        full_rect.left() + fmargin,
                        full_rect.bottom() - COMPOSER_LIFT - self.composer.stack_h - 8.0,
                    ))
                    .pivot(egui::Align2::LEFT_BOTTOM)
                    .order(egui::Order::Foreground)
                    .show(ui.ctx(), |ui| {
                        egui::Frame::new()
                            .fill(rgb(t.bg_panel))
                            .corner_radius(CornerRadius::same(14))
                            .stroke(Stroke::new(1.0_f32, rgb(t.border)))
                            .shadow(crate::theme::composer_shadow(t))
                            .inner_margin(Margin::symmetric(8, 6))
                            .show(ui, |ui| {
                                ui.set_width((full_rect.width() - fmargin * 2.0 - 90.0).max(200.0));
                                for reply in &matches {
                                    let preview = if reply.message.chars().count() <= 48 {
                                        reply.message.clone()
                                    } else {
                                        crate::widgets::ellipsize_fit(&reply.message, |p| {
                                            p.chars().count() <= 48
                                        })
                                    };
                                    let label = format!("/{}  {}", reply.shortcut, preview);
                                    let row = ui.horizontal(|ui| {
                                        let (slot, _) = ui.allocate_exact_size(
                                            egui::vec2(16.0, 16.0),
                                            egui::Sense::hover(),
                                        );
                                        crate::icons::draw(
                                            ui.painter(),
                                            crate::icons::Icon::Bolt,
                                            slot.center(),
                                            13.0,
                                            rgb(t.accent),
                                            rgb(t.bg_panel),
                                        );
                                        ui.add(
                                            egui::Label::new(RichText::new(label).size(13.0))
                                                .sense(egui::Sense::click()),
                                        )
                                        .clicked()
                                    });
                                    if row.inner {
                                        picked = Some(reply.message.clone());
                                    }
                                }
                            });
                    });
                if let Some(text) = picked {
                    self.composer.draft = text;
                }
            }
        }
    }
}

/// The tallest the composer text field may grow, in visual
/// rows: past this the field scrolls internally instead of
/// growing the pill.
pub(crate) const COMPOSER_MAX_ROWS: usize = 5;

/// Vertical space the text field's own frame adds around the
/// text: the `Margin::symmetric(4, 8)` inner margin on the
/// composer's text edit, top plus bottom.
const COMPOSER_FIELD_MARGIN_Y: f32 = 16.0;

/// The height the composer input row is floored at: the
/// 36 px icon buttons it always contains.
const COMPOSER_ROW_MIN_H: f32 = 36.0;

fn clamped_composer_rows(visual_rows: usize) -> usize {
    visual_rows.clamp(1, COMPOSER_MAX_ROWS)
}

/// The text field's height for a draft occupying
/// `visual_rows` wrapped lines at `line_height`: the clamped
/// row count times the line height, plus the field frame's
/// vertical margins. A pure function of the current draft:
/// an empty draft is one row, and a draft that shrinks back
/// to one row returns this to the one-line height on the
/// same frame, whatever it grew to before.
fn composer_field_height(visual_rows: usize, line_height: f32) -> f32 {
    clamped_composer_rows(visual_rows) as f32 * line_height + COMPOSER_FIELD_MARGIN_Y
}

/// The input row's height: the field height, floored at the
/// icon buttons' height, so a one-line draft yields exactly
/// the shipped one-line pill (a 36 px row inside the pill's
/// own margins).
fn composer_row_height(visual_rows: usize, line_height: f32) -> f32 {
    composer_field_height(visual_rows, line_height).max(COMPOSER_ROW_MIN_H)
}

/// The composer's line height, computed the way egui's text
/// edit computes it: the Body font's row height plus the
/// style's extra line spacing.
fn composer_line_height(ui: &egui::Ui) -> f32 {
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    ui.fonts_mut(|f| f.row_height(&font_id)) + ui.spacing().extra_text_line_spacing
}

/// How many visual (wrapped) rows `text` occupies at
/// `wrap_width`, laid out with the same job the text edit
/// builds for itself: the Body font, the same line height,
/// trailing whitespace kept. An empty draft is one row.
fn composer_visual_rows(ui: &egui::Ui, text: &str, wrap_width: f32, line_height: f32) -> usize {
    if text.is_empty() {
        return 1;
    }
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let mut job = egui::text::LayoutJob::simple(
        text.to_owned(),
        font_id,
        ui.style().visuals.text_color(),
        wrap_width,
    );
    job.halign = Align::LEFT;
    job.keep_trailing_whitespace = true;
    for section in &mut job.sections {
        section.format.line_height = Some(line_height);
    }
    ui.fonts_mut(|f| f.layout_job(job)).rows.len().max(1)
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
        let pulse = 0.55 + 0.45 * f32::sin((now * crate::motion::RECORDING_PULSE_RATE) as f32);
        let red = rgb(t.danger);
        ui.painter().circle_filled(dot.center(), 5.5, fade(red, pulse));
        let label = format!("{:02}:{:02}", secs / 60, secs % 60);
        ui.label(
            RichText::new(label).font(crate::theme::semibold(12.0)).color(rgb(t.text_primary)),
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
                crate::icons::SIZE_MENU,
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

#[cfg(test)]
mod tests {
    use super::*;

    // The line height the shipped theme produces for the
    // Body font (14 px Inter): the value the layout probe
    // measured in the running app, where a one-line field is
    // 33 px tall including its 16 px of frame margins.
    const LINE_H: f32 = 17.0;

    #[test]
    fn empty_draft_is_the_one_line_pill() {
        assert_eq!(composer_field_height(1, LINE_H), 33.0);
        // The row is floored at the 36 px icon buttons, so a
        // one-line (or degenerate zero-row) draft yields
        // exactly the one-line row the shipped pill has.
        assert_eq!(composer_row_height(1, LINE_H), 36.0);
        assert_eq!(composer_row_height(0, LINE_H), 36.0);
    }

    #[test]
    fn multiline_draft_grows_one_line_at_a_time() {
        assert_eq!(composer_field_height(2, LINE_H), 50.0);
        assert_eq!(composer_row_height(2, LINE_H), 50.0);
        assert_eq!(composer_row_height(3, LINE_H), 67.0);
    }

    #[test]
    fn growth_is_capped_at_the_max_rows() {
        assert_eq!(composer_row_height(12, LINE_H), composer_row_height(COMPOSER_MAX_ROWS, LINE_H));
        assert_eq!(composer_field_height(12, LINE_H), 101.0);
    }

    #[test]
    fn cleared_draft_returns_to_one_line() {
        // The height functions are stateless: whatever the
        // draft grew to while typing, one visual row always
        // maps back to the one-line height, so clearing the
        // draft (or sending it) shrinks the pill on the same
        // frame.
        assert_eq!(composer_row_height(5, LINE_H), 101.0);
        assert_eq!(composer_row_height(1, LINE_H), 36.0);
    }
}
