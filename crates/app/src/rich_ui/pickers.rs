//! Pickers and media sending: the emoji/GIF/sticker picker, the forward picker, and the media send paths behind them.

use super::*;

impl App {
    pub(crate) fn draw_picker(&mut self, ctx: &egui::Context) {
        // Motion: the picker fades/rises in when it opens and
        // plays the same run back, shorter, when it closes.
        let now = ctx.input(|i| i.time);
        if self.picker.open && !self.anim.picker_was_open {
            self.anim.picker = Some(self.fresh_anim(crate::motion::dur::SLIDE, ctx));
            self.anim.picker_closing = false;
        }
        if !self.picker.open && self.anim.picker_was_open {
            self.anim.picker = Some(self.fresh_anim(crate::motion::dur::EXIT, ctx));
            self.anim.picker_closing = true;
        }
        self.anim.picker_was_open = self.picker.open;
        if !self.picker.open && !self.anim.picker_closing {
            return;
        }
        let run_p = crate::motion::Anim::drive(&mut self.anim.picker, ctx, now);
        if self.anim.picker_closing && self.anim.picker.is_none() {
            self.anim.picker_closing = false;
            return;
        }
        // Visual openness: the entrance run drives it 0 to 1,
        // the exit run drives it 1 back to 0.
        let picker_p = if self.anim.picker_closing { 1.0 - run_p } else { run_p };
        let picker_slide = if self.motion.slides() { (1.0 - picker_p) * 12.0 } else { 0.0 };
        // Refresh sticker packs each time the picker is drawn (cheap:
        // a directory listing of small PNG packs).
        let root = crate::extras::feature_state_dir().join("stickers");
        if self.feature_active("stickers") {
            let _ = stickers::ensure_default_pack(&root);
            if let Ok(packs) = stickers::list_packs(&root) {
                self.picker.sticker_packs = packs;
            }
        }
        let screen = ctx.content_rect();
        let t = self.tokens;
        // Fake-frost underlay behind the panel: the panel's outer
        // size is its 340 px content plus the popup frame's
        // margins, and the underlay rect pads it a little on every
        // side. Painted on the background layer, it deepens the
        // frost under the near-opaque panel fill so stream text
        // stops bleeding through in two layers; it slides with
        // the panel so the two never separate mid-animation.
        let underlay = egui::Rect::from_min_size(
            egui::Pos2::new(
                screen.left() + 430.0 - 10.0,
                screen.bottom() - 470.0 + picker_slide - 10.0,
            ),
            Vec2::new(364.0 + 20.0, 360.0 + 20.0),
        );
        ctx.layer_painter(egui::LayerId::background()).rect_filled(
            underlay,
            CornerRadius::same(24),
            crate::theme::glass_underlay(t),
        );
        egui::Area::new(egui::Id::unique("media_picker"))
            .fixed_pos(egui::Pos2::new(
                screen.left() + 430.0,
                screen.bottom() - 470.0 + picker_slide,
            ))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                show_popup(self.popup_frame(), self.tokens, ui, |ui| {
                    if picker_p < 0.999 {
                        ui.set_opacity(picker_p);
                    }
                    ui.set_width(340.0);
                    ui.set_height(340.0);
                    match self.picker.tab.as_str() {
                        "gif" => {
                            self.picker_tab_row(ui, t, "gif");
                            ui.add_space(6.0);
                            self.picker_gif_tab(ui);
                        }
                        "stickers" => {
                            self.picker_tab_row(ui, t, "stickers");
                            ui.add_space(6.0);
                            self.picker_stickers_tab(ui);
                        }
                        _ => self.picker_emoji_tab(ui, t),
                    }
                });
            });
    }

    /// The shared tab row for the GIF and sticker panes: Emoji
    /// (always present) plus the feature-owned tabs, with the close
    /// button pinned right. The emoji pane carries its own row at
    /// the bottom instead, matching the WhatsApp layout.
    fn picker_tab_row(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens, active: &str) {
        ui.horizontal(|ui| {
            if ui
                .selectable_label(active == "emoji", crate::i18n::t("emoji.tab").as_str())
                .clicked()
            {
                self.picker.tab = "emoji".to_owned();
            }
            for tab in Self::picker_tabs() {
                if self.feature_active(&tab.owner)
                    && ui.selectable_label(active == tab.tab_id, &tab.label).clicked()
                {
                    self.picker.tab = tab.tab_id.clone();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if crate::widgets::icon_button(
                    ui,
                    t,
                    crate::icons::Icon::X,
                    26.0,
                    &crate::i18n::t("common.close"),
                    None,
                )
                .clicked()
                {
                    self.picker.open = false;
                }
            });
        });
    }

    /// The emoji tab: search on top, Recent strip, then the
    /// categorized grid, and the tab row (Emoji | GIF | Stickers)
    /// along the bottom, WhatsApp-style. GIF and Stickers stay
    /// feature-gated exactly as before.
    fn picker_emoji_tab(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        // Search field.
        let search = ui.add(
            egui::TextEdit::singleline(&mut self.picker.emoji_search)
                .hint_text(crate::i18n::t("emoji.search_hint"))
                .desired_width(ui.available_width()),
        );
        if search.changed() {
            self.picker.emoji_search = self.picker.emoji_search.to_lowercase();
        }
        ui.add_space(4.0);
        let query = self.picker.emoji_search.trim().to_owned();
        let recents = self.picker.emoji_recents.clone();
        egui::ScrollArea::vertical().id_salt("emoji_grid").show(ui, |ui| {
            let mut used: Option<String> = None;
            if query.is_empty() && !recents.is_empty() {
                section_label(ui, t, &crate::i18n::t("emoji.recent"));
                ui.horizontal_wrapped(|ui| {
                    for emoji in recents.iter().take(crate::rich_ui::emoji::RECENT_CAP) {
                        if emoji_button(ui, emoji) {
                            used = Some(emoji.clone());
                        }
                    }
                });
                ui.add_space(6.0);
            }
            for group in crate::rich_ui::emoji::GROUPS {
                let hits: Vec<&(&str, &str)> =
                    group.emoji.iter().filter(|e| emoji::matches(e, &query)).collect();
                if hits.is_empty() {
                    continue;
                }
                if query.is_empty() {
                    section_label(ui, t, &crate::i18n::t(group.label_key));
                }
                ui.horizontal_wrapped(|ui| {
                    for emoji in hits {
                        if emoji_button(ui, emoji.0) {
                            used = Some((*emoji.0).to_owned());
                        }
                    }
                });
                ui.add_space(6.0);
            }
            if let Some(emoji) = used {
                emoji::note_used(&mut self.picker.emoji_recents, &emoji);
                self.composer.draft.push_str(&emoji);
            }
        });
        crate::widgets::soft_separator(ui, t);
        // Bottom tab row: Emoji, then the feature-owned tabs, and
        // the close button pinned right.
        ui.horizontal(|ui| {
            if ui.selectable_label(true, crate::i18n::t("emoji.tab").as_str()).clicked() {
                // Already the emoji tab: nothing to switch.
            }
            for tab in Self::picker_tabs() {
                if self.feature_active(&tab.owner)
                    && ui.selectable_label(false, &tab.label).clicked()
                {
                    self.picker.tab = tab.tab_id.clone();
                    if tab.tab_id == "gif" && self.picker.giphy_items.is_empty() {
                        self.start_giphy_trending();
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if crate::widgets::icon_button(
                    ui,
                    t,
                    crate::icons::Icon::X,
                    26.0,
                    &crate::i18n::t("common.close"),
                    None,
                )
                .clicked()
                {
                    self.picker.open = false;
                }
            });
        });
    }

    fn picker_gif_tab(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        let pool = self.credentials.pool_for(giphy::KEY_SLOT);
        match giphy::tab_state(&pool, self.picker.giphy_rejected) {
            giphy::TabState::NeedsKey | giphy::TabState::KeyRejected => {
                // The no-key state (matches the reference design):
                // explain, link the developer portal, and route to
                // the CENTRAL credentials UI, never a per-feature
                // key field.
                egui::Frame::new()
                    .fill(rgb(t.bg_panel_alt))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::symmetric(14, 12))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(crate::i18n::t("pickers.gif_needs_key")).size(15.0).strong(),
                        );
                        if self.picker.giphy_rejected {
                            ui.label(
                                RichText::new(crate::i18n::t("pickers.gif_key_rejected"))
                                    .size(12.0)
                                    .color(rgb(t.danger)),
                            );
                        }
                        ui.label(
                            RichText::new(
                                "Zaprust uses your own GIPHY key, stored once in the \
                                 central credential store; features never keep their \
                                 own keys.",
                            )
                            .size(12.0)
                            .color(rgb(t.text_secondary)),
                        );
                        ui.hyperlink_to(
                            "Get a key at developers.giphy.com",
                            GIPHY_DEVELOPERS_URL,
                        );
                        if crate::widgets::button(
                            ui,
                            t,
                            "Open Settings > AI",
                            crate::widgets::Btn::Primary,
                        )
                        .clicked()
                        {
                            self.screen = crate::state::Screen::Settings;
                            self.picker.open = false;
                        }
                    });
            }
            giphy::TabState::Ready => {
                ui.horizontal(|ui| {
                    crate::widgets::text_field(
                        ui,
                        t,
                        &mut self.picker.giphy_query,
                        "Search GIFs",
                        130.0,
                    );
                    if crate::widgets::button(ui, t, "Search", crate::widgets::Btn::Secondary)
                        .clicked()
                    {
                        self.start_giphy_search();
                    }
                    if crate::widgets::button(ui, t, "Trending", crate::widgets::Btn::Secondary)
                        .clicked()
                    {
                        self.start_giphy_trending();
                    }
                });
                if self.picker.giphy_items.is_empty() {
                    if self.picker.giphy_responded {
                        // A result arrived and it was empty: that
                        // is an answer, not a wait, so it gets the
                        // plain empty text instead of a spinner
                        // that would promise more is coming.
                        ui.label(
                            RichText::new(crate::i18n::t("picker.empty"))
                                .size(12.0)
                                .color(rgb(t.text_secondary)),
                        );
                    } else {
                        // The fetch-in-flight row: the shared
                        // loading row, so waiting reads as waiting.
                        crate::widgets::loading_row(
                            ui,
                            &crate::i18n::t("picker.loading"),
                            14.0,
                            t.text_secondary,
                        );
                    }
                }
                let items = self.picker.giphy_items.clone();
                let now = ui.ctx().input(|i| i.time);
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for item in &items {
                            let loaded = self.picker.giphy_textures.contains_key(&item.preview_url);
                            if loaded {
                                // Loaded tiles keep the GIF label.
                                let btn = egui::Button::new(RichText::new("GIF"))
                                    .min_size(Vec2::new(76.0, 56.0));
                                if tile_add(ui, t, |ui| ui.add(btn))
                                    .on_hover_text(&item.title)
                                    .clicked()
                                {
                                    self.send_gif(item);
                                }
                            } else {
                                // Pending preview: a pulsing skeleton
                                // tile (the pulse breathes on a slow
                                // sine so the grid reads as loading,
                                // not as broken buttons).
                                let pulse = 0.55
                                    + 0.3
                                        * f32::sin(
                                            (now * crate::motion::SKELETON_PULSE_RATE) as f32,
                                        );
                                let (rect, _) = ui.allocate_exact_size(
                                    Vec2::new(76.0, 56.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect_filled(
                                    rect,
                                    CornerRadius::same(8),
                                    egui::Color32::from_rgba_unmultiplied(
                                        rgb(t.bg_panel_alt).r(),
                                        rgb(t.bg_panel_alt).g(),
                                        rgb(t.bg_panel_alt).b(),
                                        (255.0 * pulse) as u8,
                                    ),
                                );
                                ui.painter().rect_stroke(
                                    rect,
                                    CornerRadius::same(8),
                                    egui::Stroke::new(1.0_f32, rgb(t.border)),
                                    egui::StrokeKind::Inside,
                                );
                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    "GIF",
                                    egui::FontId::proportional(10.0),
                                    rgb(t.text_faint),
                                );
                            }
                        }
                    });
                });
            }
        }
    }

    fn picker_stickers_tab(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        let root = crate::extras::feature_state_dir().join("stickers");
        let packs = self.picker.sticker_packs.clone();
        if packs.is_empty() {
            ui.label(crate::i18n::t("pickers.no_packs_yet"));
        }
        // Stickers with no texture yet, queued for the preview
        // worker after the draw. The draw path only binds
        // already-decoded textures; reading and decoding happen
        // on the worker (see extras::request_sticker_previews).
        let mut missing: Vec<std::path::PathBuf> = Vec::new();
        for pack in &packs {
            ui.label(RichText::new(&pack.name).size(12.0).strong());
            ui.horizontal_wrapped(|ui| {
                for sticker in &pack.stickers {
                    let name = sticker
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let texture_id = self.picker.sticker_textures.get(sticker).map(|h| h.id());
                    if texture_id.is_none()
                        && !self.picker.sticker_pending.contains(sticker)
                        && !self.picker.sticker_failures.still_failed(sticker)
                    {
                        missing.push(sticker.clone());
                    }
                    let clicked = match texture_id {
                        Some(id) => tile_add(ui, t, |ui| {
                            ui.add(egui::Button::image(egui::load::SizedTexture::new(
                                id,
                                Vec2::new(52.0, 52.0),
                            )))
                            .on_hover_text(&name)
                            .clicked()
                        }),
                        None => tile_add(ui, t, |ui| {
                            // No art yet: the tile speaks the
                            // picker's skeleton language instead of
                            // wearing a stock button (a pending
                            // preview pulses like the GIF tiles, a
                            // failed one rests static), and it
                            // still sends on click, as before.
                            let failed = self.picker.sticker_failures.still_failed(sticker);
                            let (rect, resp) =
                                ui.allocate_exact_size(Vec2::new(52.0, 52.0), egui::Sense::click());
                            // The same pulse the GIF skeleton
                            // tiles breathe on (a failed preview
                            // rests at full strength instead).
                            let now = ui.ctx().input(|i| i.time);
                            let pulse = if failed {
                                1.0
                            } else {
                                0.55 + 0.3
                                    * f32::sin((now * crate::motion::SKELETON_PULSE_RATE) as f32)
                            };
                            let base = rgb(t.bg_panel_alt);
                            ui.painter().rect_filled(
                                rect,
                                CornerRadius::same(8),
                                egui::Color32::from_rgba_unmultiplied(
                                    base.r(),
                                    base.g(),
                                    base.b(),
                                    (255.0 * pulse) as u8,
                                ),
                            );
                            ui.painter().rect_stroke(
                                rect,
                                CornerRadius::same(8),
                                egui::Stroke::new(1.0_f32, rgb(t.border)),
                                egui::StrokeKind::Inside,
                            );
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                "Sticker",
                                egui::FontId::proportional(10.0),
                                rgb(t.text_faint),
                            );
                            resp.on_hover_text(&name).clicked()
                        }),
                    };
                    if clicked {
                        self.send_sticker(sticker, &name);
                    }
                }
            });
            ui.horizontal(|ui| {
                if crate::widgets::button(ui, t, "Delete pack", crate::widgets::Btn::Danger)
                    .clicked()
                {
                    // Deleting a pack removes files with no
                    // undo: it confirms through the shared modal
                    // (section 24), which also speaks the result.
                    self.ui_confirm = Some(crate::state::UiConfirm::DeleteStickerPack {
                        dir: pack.dir.clone(),
                        name: pack.name.clone(),
                    });
                }
            });
        }
        self.request_sticker_previews(missing);
        ui.add_space(10.0);
        ui.label(RichText::new(crate::i18n::t("pickers.new_pack_name")).size(12.0).color(rgb(t.text_secondary)));
        ui.horizontal(|ui| {
            crate::widgets::text_field(ui, t, &mut self.picker.sticker_new_pack, "", 220.0);
            if crate::widgets::button(ui, t, "New pack", crate::widgets::Btn::Secondary).clicked()
                && !self.picker.sticker_new_pack.trim().is_empty()
            {
                let name = self.picker.sticker_new_pack.clone();
                match stickers::create_pack(&root, &name) {
                    Ok(_) => self.picker.sticker_new_pack.clear(),
                    Err(e) => self.notice = Some(Notice::error(e)),
                }
            }
        });
        ui.label(RichText::new(crate::i18n::t("pickers.new_sticker_text")).size(12.0).color(rgb(t.text_secondary)));
        ui.horizontal(|ui| {
            crate::widgets::text_field(ui, t, &mut self.picker.sticker_new_text, "", 220.0);
            if crate::widgets::button(ui, t, "Add text sticker", crate::widgets::Btn::Secondary)
                .clicked()
            {
                if let Some(first) = packs.first() {
                    let text = self.picker.sticker_new_text.clone();
                    match stickers::add_text_sticker(&first.dir, &text) {
                        Ok(_) => self.picker.sticker_new_text.clear(),
                        Err(e) => self.notice = Some(Notice::error(e)),
                    }
                } else {
                    self.notice = Some(Notice::info("Create a pack first.".to_owned()));
                }
            }
        });
        ui.label(
            RichText::new(crate::i18n::t("pickers.text_stickers_first_pack"))
                .size(10.0)
                .color(rgb(self.tokens.text_faint)),
        );
    }

    /// The forward target picker: a small popup listing the other
    /// chats; one tap forwards the message exactly once (there is
    /// no multi-target fan-out, by design).
    pub(crate) fn draw_forward_picker(&mut self, ctx: &egui::Context) {
        let t = self.tokens;
        if let Some(id) = self.forward_from {
            ctx.data_mut(|d| d.insert_temp(egui::Id::unique("forward_snap"), id));
        }
        let Some(fwd_p) = crate::anim::popup_phase(
            ctx,
            "forward_picker",
            self.forward_from.is_some(),
            self.motion,
        ) else {
            return;
        };
        let exiting = self.forward_from.is_none();
        let Some(msg_id) = ctx.data(|d| d.get_temp::<u64>(egui::Id::unique("forward_snap"))) else {
            return;
        };
        let current = self.selected_chat;
        let targets: Vec<(u64, String, u8)> = self
            .chats
            .iter()
            .filter(|c| c.id != current)
            .map(|c| (c.id, c.title.clone(), c.avatar_seed))
            .collect();
        let mut chosen: Option<u64> = None;
        let mut close = false;
        egui::Area::new(egui::Id::unique("forward_picker"))
            .pivot(Align2::CENTER_CENTER)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if fwd_p < 0.999 {
                    ui.set_opacity(fwd_p);
                }
                show_popup(self.popup_frame(), self.tokens, ui, |ui| {
                    ui.set_width(260.0);
                    ui.add_enabled_ui(!exiting, |ui| {
                        ui.label(
                            RichText::new(crate::i18n::t("forward.title")).size(12.0).strong(),
                        );
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .max_height(320.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                for (id, title, seed) in &targets {
                                    ui.horizontal(|ui| {
                                        let (rect, _) = ui.allocate_exact_size(
                                            Vec2::splat(26.0),
                                            egui::Sense::hover(),
                                        );
                                        crate::widgets::paint_avatar_disc(
                                            ui.painter(),
                                            rect.center(),
                                            14.0,
                                            false,
                                            crate::widgets::avatar_color(*seed),
                                        );
                                        if crate::widgets::menu_item(ui, t, title).clicked() {
                                            chosen = Some(*id);
                                        }
                                    });
                                }
                            });
                        crate::widgets::soft_separator(ui, t);
                        if crate::widgets::menu_item(ui, t, &crate::i18n::t("common.cancel"))
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });
            });
        crate::anim::popup_transform(
            ctx,
            egui::Id::unique("forward_picker"),
            ctx.content_rect().center(),
            fwd_p,
        );
        if let Some(target) = chosen {
            self.do_forward(msg_id, target);
            close = true;
        }
        if close {
            self.forward_from = None;
        }
    }

    /// Sends a sticker/GIF as an image message (demo path, shared
    /// by the live entry points below for their demo branch).
    pub(crate) fn send_media_message(&mut self, caption: &str) {
        self.sent_counter += 1;
        let msg = Message {
            kind: MessageKind::Image { caption: caption.to_owned(), view_once: false },
            ..Message::mine(900 + self.sent_counter, String::new(), "19:36")
        };
        self.conversation.push(msg.clone());
        let chat_id = self.selected_chat;
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.messages.push(msg);
            chat.preview = caption.to_owned();
        }
        self.picker.open = false;
    }

    /// Sends one sticker from the picker. Demo: the canned image
    /// bubble. Live: the pack file is converted to a 512x512 WebP
    /// and sent through the backend; the echo bubble is replaced by
    /// the backend's own upsert when it lands.
    pub(crate) fn send_sticker(&mut self, path: &std::path::Path, name: &str) {
        if !self.live_mode {
            self.send_media_message(&format!("Sticker: {name}"));
            return;
        }
        let Some(jid) = self.live_jid(self.selected_chat) else {
            self.notice = Some(Notice::info(crate::i18n::t("attach.open_live_first")));
            return;
        };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.notice = Some(Notice::error(format!("Could not read the sticker file: {e}")));
                return;
            }
        };
        let webp = match sticker_webp(&bytes) {
            Ok(w) => w,
            Err(e) => {
                self.notice = Some(Notice::error(format!("Sticker: {e}")));
                return;
            }
        };
        self.send_backend(backend::BackendCommand::SendMedia {
            chat_jid: jid,
            kind: backend::MediaKind::Sticker,
            data: webp,
            mime: "image/webp".to_owned(),
            caption: String::new(),
            file_name: None,
            gif_playback: false,
            seconds: 0,
            view_once: false,
            ephemeral_secs: self.ephemeral_secs_of(self.selected_chat),
        });
        self.echo_live_media(
            MessageKind::Image { caption: "Sticker".to_owned(), view_once: false },
            "Sticker",
        );
        self.picker.open = false;
    }

    /// Sends one GIF from the picker. Demo: the canned image
    /// bubble. Live: WhatsApp GIF-style messages are MP4 videos
    /// with the GIF playback flag, so the item's MP4 rendition is
    /// downloaded on a worker thread and sent when it lands; an
    /// item without an MP4 rendition is refused honestly.
    pub(crate) fn send_gif(&mut self, item: &giphy::GifItem) {
        if !self.live_mode {
            self.send_media_message(&format!("GIF: {}", item.gif_url));
            return;
        }
        let Some(jid) = self.live_jid(self.selected_chat) else {
            self.notice = Some(Notice::info(crate::i18n::t("attach.open_live_first")));
            return;
        };
        // One media download at a time: overwriting the slot would
        // silently drop the first GIF's result (logic review F20).
        if self.picker.media_fetch_rx.is_some() {
            self.notice = Some(Notice::info("Still fetching the previous GIF.".to_owned()));
            return;
        }
        let Some(mp4_url) = item.mp4_url.clone() else {
            self.notice = Some(Notice::info(
                "This GIF has no video version, so it cannot be sent in live mode.".to_owned(),
            ));
            return;
        };
        let title = item.title.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.picker.media_fetch_rx = Some(rx);
        self.picker.open = false;
        // The MP4 fetch runs on its thread and can take
        // seconds; until it lands in the extras poll, the send
        // reads as in flight instead of as silence. The poll
        // clears or replaces this.
        self.notice = Some(Notice::busy("Sending GIF…".to_owned()));
        let spawned = std::thread::Builder::new()
            .name("zaprust-gif-send".to_owned())
            .spawn(move || {
                let result = giphy::fetch_mp4(&mp4_url).map_err(|e| e.to_string());
                let _ = tx.send(crate::extras::MediaFetchResult { jid, title, result });
            });
        if spawned.is_err() {
            // No worker, no result: the send did not happen
            // (P2-36). Say so as an error and drop the channel,
            // instead of leaving the Busy notice standing over
            // a send that will never complete.
            self.picker.media_fetch_rx = None;
            self.notice =
                Some(Notice::error("GIF: the fetch worker could not start.".to_owned()));
        }
    }

    /// Finishes a live GIF send once its MP4 bytes are in: sends
    /// the video (GIF playback on) and echoes a local bubble.
    pub(crate) fn complete_live_gif_send(&mut self, jid: &str, title: &str, bytes: Vec<u8>) {
        self.send_backend(backend::BackendCommand::SendMedia {
            chat_jid: jid.to_owned(),
            kind: backend::MediaKind::Video,
            data: bytes,
            mime: "video/mp4".to_owned(),
            caption: title.to_owned(),
            file_name: None,
            gif_playback: true,
            seconds: 0,
            view_once: false,
            ephemeral_secs: self.ephemeral_secs_for_jid(jid),
        });
        if self.live_jid(self.selected_chat).as_deref() == Some(jid) {
            self.echo_live_media(
                MessageKind::Video { caption: title.to_owned(), view_once: false, secs: 0 },
                title,
            );
        }
    }

    /// Pushes the local echo for a live media send into the live
    /// mirror (the backend upsert for the same message replaces it
    /// when it arrives, the same pattern `send_live_text` uses).
    pub(crate) fn echo_live_media(&mut self, kind: MessageKind, body: &str) {
        self.sent_counter += 1;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        // The echo expires on the chat's timer just like the
        // backend upsert that replaces it.
        let timer = self.ephemeral_secs_of(self.selected_chat);
        let echo = Message {
            kind,
            delivery: Some(core::model::Delivery::Sending),
            expires_at: if timer > 0 {
                u64::try_from(now + i64::from(timer)).unwrap_or(0)
            } else {
                0
            },
            ..Message::mine(9_000_000 + self.sent_counter, body.to_owned(), "…")
        };
        if let Some(session) = &mut self.live {
            session.messages.entry(self.selected_chat).or_default().push((now, echo));
        }
        self.sync_conversation();
    }
}

/// Converts a sticker image (the packs render PNG) into the WebP
/// form WhatsApp stickers use: square 512x512, transparent
/// background preserved, image fitted inside without distortion.
/// Where a Giphy API key is obtained (the picker links here).
const GIPHY_DEVELOPERS_URL: &str = "https://developers.giphy.com";

pub(crate) fn sticker_webp(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("not a readable image: {e}"))?;
    let fitted = img.resize(512, 512, image::imageops::FilterType::Lanczos3);
    let mut canvas = image::RgbaImage::new(512, 512);
    let x = (512u32.saturating_sub(fitted.width())) / 2;
    let y = (512u32.saturating_sub(fitted.height())) / 2;
    image::imageops::overlay(&mut canvas, &fitted.to_rgba8(), i64::from(x), i64::from(y));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(canvas)
        .write_to(&mut out, image::ImageFormat::WebP)
        .map_err(|e| format!("WebP encode failed: {e}"))?;
    Ok(out.into_inner())
}

/// A small caps section label inside the emoji grid (Recent,
/// Smileys & People, ...).
fn section_label(ui: &mut egui::Ui, t: core::theme::Tokens, label: &str) {
    ui.label(RichText::new(label).size(10.0).strong().color(rgb(t.text_secondary)));
}

/// One emoji cell of the grid: a frameless button that inserts into
/// the draft on click (the caller records the recent use). The cell
/// is a fixed square so the wrapped rows lay out as an even grid
/// regardless of each glyph's own advance width (color glyphs from
/// the device emoji font are a full em wide, the monochrome
/// fallback's are not).
fn emoji_button(ui: &mut egui::Ui, emoji: &str) -> bool {
    ui.add(
        egui::Button::new(RichText::new(emoji).size(19.0)).frame(false).min_size(Vec2::splat(33.0)),
    )
    .clicked()
}

#[cfg(test)]
mod tests {
    use super::sticker_webp;

    #[test]
    fn sticker_webp_produces_a_512_square_webp() {
        // A small non-square PNG, like the packs render.
        let img = image::RgbaImage::from_pixel(100, 50, image::Rgba([200, 30, 30, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("png encode");
        let webp = sticker_webp(&png.into_inner()).expect("conversion");
        let decoded = image::load_from_memory(&webp).expect("webp decodes");
        assert_eq!((decoded.width(), decoded.height()), (512, 512));
    }

    #[test]
    fn sticker_webp_rejects_non_image_bytes() {
        assert!(sticker_webp(b"not an image").is_err());
    }
}
