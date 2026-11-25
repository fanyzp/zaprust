//! Labels panel and label widgets (parity P10, row 131):
//! the synced label set as a LEFT panel (the owner's
//! left-panel rule), the label editor, the checklist popup
//! the chat and message menus open, and the tag chips the
//! chat rows and message bubbles wear.
//!
//! Labels are account data synced through the backend; this
//! module only draws state and records intents. Every action
//! lands in `live::business` (optimistic apply with rollback
//! for saves and toggles, ack-gated deletes).

use crate::state::{App, LabelPickerTarget, Screen};
use crate::theme::{rgb, SP_1, SP_2, SP_2_5, SP_3};
use core::theme::Tokens;
use eframe::egui::{self, CornerRadius, FontId, Margin, Pos2, RichText, Sense, Vec2};

/// One label tag as the chat row paints it: the name and its
/// palette color.
#[derive(Clone)]
pub(crate) struct ChatTag {
    /// Label name.
    pub(crate) name: String,
    /// Palette color for the label's color index.
    pub(crate) color: core::theme::Rgb,
}

impl App {
    /// The tags for one chat, resolved through its JID: up to
    /// two, in label-name order, for the chat row's snippet
    /// line.
    pub(crate) fn chat_tags(&self, chat_id: u64) -> Vec<ChatTag> {
        let Some(jid) = self.live_jid(chat_id) else { return Vec::new() };
        let Some(ids) = self.business.chat_labels.get(&jid) else { return Vec::new() };
        let mut tags: Vec<ChatTag> = self
            .business
            .labels
            .iter()
            .filter(|l| ids.contains(&l.id))
            .map(|l| ChatTag { name: l.name.clone(), color: crate::live::label_color(l.color) })
            .collect();
        tags.sort_by(|a, b| a.name.cmp(&b.name));
        tags.truncate(2);
        tags
    }

    /// The tags for one message, resolved through its chat
    /// JID and protocol id, in label-name order, for the
    /// bubble's strip line.
    pub(crate) fn message_tags(&self, chat_id: u64, msg_id: u64) -> Vec<ChatTag> {
        let (Some(jid), Some(proto)) = (self.live_jid(chat_id), self.protocol_ids.get(&msg_id))
        else {
            return Vec::new();
        };
        let Some(ids) = self.business.message_labels.get(&(jid, proto.clone())) else {
            return Vec::new();
        };
        let mut tags: Vec<ChatTag> = self
            .business
            .labels
            .iter()
            .filter(|l| ids.contains(&l.id))
            .map(|l| ChatTag { name: l.name.clone(), color: crate::live::label_color(l.color) })
            .collect();
        tags.sort_by(|a, b| a.name.cmp(&b.name));
        tags
    }

    /// The Labels panel: header, the editor when open, and
    /// one row per label (color dot, name, tagged-chat count,
    /// kebab with Edit and Delete). A row click filters the
    /// chat list by that label.
    pub(crate) fn draw_labels_panel(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        let mut back = false;
        let mut new_label = false;
        let mut filter_by: Option<String> = None;
        let mut edit: Option<String> = None;
        let mut delete: Option<String> = None;
        let labels: Vec<backend::LiveLabel> = {
            let mut sorted = self.business.labels.clone();
            sorted.sort_by(|a, b| a.name.cmp(&b.name));
            sorted
        };
        let counts: std::collections::HashMap<String, usize> = {
            let mut map: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for set in self.business.chat_labels.values() {
                for id in set {
                    *map.entry(id.clone()).or_default() += 1;
                }
            }
            map
        };
        self.list_panel(root)
            .frame(
                egui::Frame::new()
                    .fill(rgb(t.bg_panel))
                    .inner_margin(Margin::symmetric(crate::theme::LIST_PANEL_INSET_X, 8)),
            )
            .show(root, |ui| {
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
                        RichText::new(crate::i18n::t("labels.title"))
                            .font(crate::theme::medium(15.0))
                            .color(rgb(t.text_primary)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if crate::widgets::icon_button(
                            ui,
                            t,
                            crate::icons::Icon::Plus,
                            28.0,
                            &crate::i18n::t("labels.new"),
                            None,
                        )
                        .clicked()
                        {
                            new_label = true;
                        }
                    });
                });
                ui.add_space(SP_2_5);
                if labels.is_empty() && self.business.label_editor.is_none() {
                    crate::widgets::empty_state(ui, t, &crate::i18n::t("labels.empty"));
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    // The editor leads the scroll content, not
                    // the panel flow above the area: laid out
                    // above the area, its controls below the
                    // name field took clicks one row higher
                    // than their paint (measured 26 px in the
                    // qc1629 rig runs), while rows inside the
                    // area hit exactly. Inside the area the
                    // editor shares the rows' layout pass.
                    if self.business.label_editor.is_some() {
                        self.draw_label_editor(ui, t);
                        ui.add_space(SP_3);
                    }
                    for label in &labels {
                        let (rect, resp) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 52.0),
                            Sense::click(),
                        );
                        let hov = crate::anim::hover_factor(
                            ui,
                            ("label_row", label.id.as_str()),
                            resp.hovered(),
                        );
                        if hov > 0.001 {
                            ui.painter().rect_filled(
                                rect,
                                CornerRadius::same(crate::theme::RADIUS_SMALL),
                                crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
                            );
                        }
                        let press = crate::anim::press_factor(
                            ui,
                            ("label_row", label.id.as_str()),
                            resp.is_pointer_button_down_on(),
                        );
                        if press > 0.001 {
                            ui.painter().rect_filled(
                                rect,
                                CornerRadius::same(crate::theme::RADIUS_SMALL),
                                crate::theme::glass_wash(t, t.bg_panel_alt, (press * 170.0) as u8),
                            );
                        }
                        let color = crate::live::label_color(label.color);
                        ui.painter().circle_filled(
                            Pos2::new(rect.left() + 14.0, rect.center().y),
                            7.0,
                            rgb(color),
                        );
                        ui.painter().text(
                            Pos2::new(rect.left() + 32.0, rect.center().y - 9.0),
                            egui::Align2::LEFT_CENTER,
                            &label.name,
                            crate::theme::medium(15.0),
                            rgb(t.text_primary),
                        );
                        let count = counts.get(&label.id).copied().unwrap_or(0);
                        let count_text =
                            if count == 1 { "1 chat".to_owned() } else { format!("{count} chats") };
                        ui.painter().text(
                            Pos2::new(rect.left() + 32.0, rect.center().y + 10.0),
                            egui::Align2::LEFT_CENTER,
                            count_text,
                            FontId::proportional(12.0),
                            rgb(t.text_faint),
                        );
                        // The row's kebab glyph: the menu
                        // itself hangs on the row (the chat
                        // list's pattern), carrying Edit and
                        // Delete behind it (progressive
                        // disclosure), Delete in danger color.
                        crate::icons::draw(
                            ui.painter(),
                            crate::icons::Icon::MoreVert,
                            Pos2::new(rect.right() - 20.0, rect.center().y),
                            crate::icons::SIZE_ROW,
                            rgb(t.text_secondary),
                            rgb(t.bg_panel),
                        );
                        let id_for_menu = label.id.clone();
                        resp.context_menu(|ui| {
                            ui.set_min_width(150.0);
                            if crate::widgets::menu_item_icon(
                                ui,
                                t,
                                Some(crate::icons::Icon::Edit),
                                &crate::i18n::t("labels.edit"),
                                None,
                            )
                            .clicked()
                            {
                                edit = Some(id_for_menu.clone());
                                ui.close();
                            }
                            if crate::widgets::menu_item_danger(
                                ui,
                                t,
                                crate::icons::Icon::Delete,
                                &crate::i18n::t("common.delete"),
                            )
                            .clicked()
                            {
                                delete = Some(id_for_menu.clone());
                                ui.close();
                            }
                        });
                        if resp.clicked() {
                            filter_by = Some(label.id.clone());
                        }
                    }
                });
            });
        if back {
            self.screen = Screen::Chats;
        }
        if new_label {
            self.business.label_editor =
                Some(crate::state::LabelForm { id: None, name: String::new(), color: 5 });
            self.business.label_editor_error = None;
        }
        if let Some(id) = edit {
            if let Some(label) = self.business.labels.iter().find(|l| l.id == id) {
                self.business.label_editor = Some(crate::state::LabelForm {
                    id: Some(label.id.clone()),
                    name: label.name.clone(),
                    color: label.color,
                });
                self.business.label_editor_error = None;
            }
        }
        if let Some(id) = delete {
            self.request_delete_label(&id);
        }
        if let Some(id) = filter_by {
            self.business.label_filter = Some(id);
            self.screen = Screen::Chats;
            self.filter = core::filter::ChatFilter::All;
        }
    }

    /// The label editor: name field, the twenty palette
    /// swatches, the inline failure, and Save/Cancel. One
    /// primary action (Save), per the design standard.
    fn draw_label_editor(&mut self, ui: &mut egui::Ui, t: Tokens) {
        let mut save = false;
        let mut cancel = false;
        egui::Frame::new()
            .fill(rgb(t.bg_panel_alt))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                let title = if self.business.label_editor.as_ref().is_some_and(|f| f.id.is_some()) {
                    crate::i18n::t("labels.edit")
                } else {
                    crate::i18n::t("labels.new")
                };
                ui.label(
                    RichText::new(title)
                        .font(crate::theme::medium(15.0))
                        .color(rgb(t.text_primary)),
                );
                ui.add_space(SP_2);
                if let Some(form) = &mut self.business.label_editor {
                    crate::widgets::text_field(
                        ui,
                        t,
                        &mut form.name,
                        &crate::i18n::t("labels.name"),
                        ui.available_width() - 24.0,
                    );
                }
                ui.add_space(SP_2);
                ui.label(
                    RichText::new(crate::i18n::t("labels.color"))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                );
                ui.add_space(SP_1);
                // The palette: twenty swatches in two rows;
                // the chosen one wears a ring.
                let mut picked: Option<i32> = None;
                for row in 0..2 {
                    ui.horizontal(|ui| {
                        for col in 0..10 {
                            let index = row * 10 + col;
                            let color = crate::live::label_color(index);
                            let (rect, resp) =
                                ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
                            ui.painter().circle_filled(rect.center(), 8.0, rgb(color));
                            let chosen = self
                                .business
                                .label_editor
                                .as_ref()
                                .is_some_and(|f| f.color == index);
                            if chosen {
                                ui.painter().circle_stroke(
                                    rect.center(),
                                    10.5,
                                    egui::Stroke::new(2.0, rgb(t.text_primary)),
                                );
                            }
                            if resp.clicked() {
                                picked = Some(index);
                            }
                        }
                    });
                    ui.add_space(4.0);
                }
                if let (Some(index), Some(form)) = (picked, &mut self.business.label_editor) {
                    form.color = index;
                }
                if let Some(error) = &self.business.label_editor_error {
                    ui.add_space(SP_1);
                    ui.label(RichText::new(error).size(12.0).color(rgb(t.danger)));
                }
                ui.add_space(SP_2);
                ui.horizontal(|ui| {
                    if crate::widgets::button(
                        ui,
                        t,
                        &crate::i18n::t("common.save"),
                        crate::widgets::Btn::Primary,
                    )
                    .clicked()
                    {
                        save = true;
                    }
                    if crate::widgets::button(
                        ui,
                        t,
                        &crate::i18n::t("common.cancel"),
                        crate::widgets::Btn::Secondary,
                    )
                    .clicked()
                    {
                        cancel = true;
                    }
                });
            });
        if save {
            self.submit_label_editor();
        }
        if cancel {
            self.business.label_editor = None;
            self.business.label_editor_error = None;
        }
    }

    /// The label checklist popup: one checkable row per
    /// label for the picker's target (a chat or a message).
    /// Toggles apply at once through the optimistic path;
    /// the popup stays open until closed.
    pub(crate) fn draw_label_picker(&mut self, ctx: &egui::Context) {
        let Some(target) = self.business.label_picker else { return };
        let t = self.tokens();
        let (title_key, active_ids): (&str, std::collections::HashSet<String>) = match target {
            LabelPickerTarget::Chat(chat_id) => (
                "labels.on_chat",
                self.live_jid(chat_id)
                    .and_then(|jid| self.business.chat_labels.get(&jid).cloned())
                    .unwrap_or_default(),
            ),
            LabelPickerTarget::Message(chat_id, msg_id) => (
                "labels.on_message",
                (|| {
                    let jid = self.live_jid(chat_id)?;
                    let proto = self.protocol_ids.get(&msg_id)?;
                    self.business.message_labels.get(&(jid, proto.clone())).cloned()
                })()
                .unwrap_or_default(),
            ),
        };
        let labels: Vec<backend::LiveLabel> = {
            let mut sorted = self.business.labels.clone();
            sorted.sort_by(|a, b| a.name.cmp(&b.name));
            sorted
        };
        let mut close = false;
        let mut toggles: Vec<(String, bool)> = Vec::new();
        egui::Area::new(egui::Id::unique("label_picker_dim"))
            .order(egui::Order::Foreground)
            .fixed_pos(Pos2::ZERO)
            .show(ctx, |ui| {
                let screen = ui.ctx().content_rect();
                let resp = ui.allocate_rect(screen, Sense::click());
                ui.painter().rect_filled(screen, 0.0, crate::theme::modal_dim(1.0));
                if resp.clicked() {
                    close = true;
                }
            });
        egui::Area::new(egui::Id::unique("label_picker"))
            .pivot(egui::Align2::CENTER_CENTER)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                crate::theme::glass_panel_frame(t, crate::theme::RADIUS_PANEL)
                    .inner_margin(Margin::symmetric(14, 12))
                    .show(ui, |ui| {
                        ui.set_width(260.0);
                        ui.label(
                            RichText::new(crate::i18n::t(title_key))
                                .font(crate::theme::medium(15.0))
                                .color(rgb(t.text_primary)),
                        );
                        ui.add_space(SP_2);
                        if labels.is_empty() {
                            ui.label(
                                RichText::new(crate::i18n::t("labels.empty"))
                                    .size(12.0)
                                    .color(rgb(t.text_secondary)),
                            );
                        }
                        for label in &labels {
                            let on = active_ids.contains(&label.id);
                            let (rect, resp) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), 34.0),
                                Sense::click(),
                            );
                            let hov = crate::anim::hover_factor(
                                ui,
                                ("label_picker_row", label.id.as_str()),
                                resp.hovered(),
                            );
                            if hov > 0.001 {
                                ui.painter().rect_filled(
                                    rect,
                                    CornerRadius::same(crate::theme::RADIUS_MICRO),
                                    crate::theme::glass_wash(
                                        t,
                                        t.bg_panel_alt,
                                        (hov * 115.0) as u8,
                                    ),
                                );
                            }
                            let press = crate::anim::press_factor(
                                ui,
                                ("label_picker_row", label.id.as_str()),
                                resp.is_pointer_button_down_on(),
                            );
                            if press > 0.001 {
                                ui.painter().rect_filled(
                                    rect,
                                    CornerRadius::same(crate::theme::RADIUS_MICRO),
                                    crate::theme::glass_wash(
                                        t,
                                        t.bg_panel_alt,
                                        (press * 170.0) as u8,
                                    ),
                                );
                            }
                            let color = crate::live::label_color(label.color);
                            ui.painter().circle_filled(
                                Pos2::new(rect.left() + 10.0, rect.center().y),
                                6.0,
                                rgb(color),
                            );
                            ui.painter().text(
                                Pos2::new(rect.left() + 26.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                &label.name,
                                crate::theme::medium(15.0),
                                rgb(t.text_primary),
                            );
                            if on {
                                crate::icons::draw(
                                    ui.painter(),
                                    crate::icons::Icon::Check,
                                    Pos2::new(rect.right() - 12.0, rect.center().y),
                                    crate::icons::SIZE_ROW,
                                    rgb(t.accent),
                                    rgb(t.bg_panel),
                                );
                            }
                            if resp.clicked() {
                                toggles.push((label.id.clone(), !on));
                            }
                        }
                        ui.add_space(SP_2);
                        if crate::widgets::button(
                            ui,
                            t,
                            &crate::i18n::t("common.close"),
                            crate::widgets::Btn::Secondary,
                        )
                        .clicked()
                        {
                            close = true;
                        }
                    });
            });
        for (label_id, labeled) in toggles {
            match target {
                LabelPickerTarget::Chat(chat_id) => {
                    self.set_chat_label(&label_id, chat_id, labeled);
                }
                LabelPickerTarget::Message(chat_id, msg_id) => {
                    self.set_message_label(&label_id, chat_id, msg_id, labeled);
                }
            }
        }
        if close {
            self.business.label_picker = None;
        }
    }
}

/// Paints one label tag chip at `x` on the line centred at
/// `line_y` and returns the x just past it: a soft fill
/// stepped from the label color over the row's panel, the
/// color dot, and the name. Shared by the chat row and the
/// bubble strip so tags read identically everywhere.
pub(crate) fn paint_label_tag(
    painter: &egui::Painter,
    ui: &egui::Ui,
    x: f32,
    line_y: f32,
    tag: &ChatTag,
    panel: core::theme::Rgb,
) -> f32 {
    let font = FontId::proportional(10.0);
    let text_w = ui
        .ctx()
        .fonts_mut(|f| f.layout_no_wrap(tag.name.clone(), font.clone(), rgb(tag.color)).size().x);
    let w = text_w + 22.0;
    let rect = egui::Rect::from_min_size(Pos2::new(x, line_y - 7.0), Vec2::new(w, 14.0));
    let fill = core::theme::mix(panel, tag.color, 0.22);
    painter.rect_filled(rect, CornerRadius::same(7), rgb(fill));
    painter.circle_filled(Pos2::new(x + 8.0, line_y), 3.0, rgb(tag.color));
    painter.text(
        Pos2::new(x + 15.0, line_y),
        egui::Align2::LEFT_CENTER,
        &tag.name,
        font,
        rgb(tag.color),
    );
    x + w
}
