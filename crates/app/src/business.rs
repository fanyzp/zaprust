//! Business UI surfaces that are not part of a bigger
//! screen (parity P10): the order details dialog. The dock's
//! business section and catalog live in `profile`, the quick
//! replies and own profile editor in `settings`, and the
//! labels surfaces in `labels`.

use crate::state::{App, OrderDialogState};
use crate::theme::{rgb, SP_1, SP_3};
use eframe::egui::{self, Margin, Pos2, RichText, Sense, Vec2};

/// The order status's display word for a wire spelling.
pub(crate) fn order_status_word(status: &str) -> Option<String> {
    match status {
        "INQUIRY" => Some(crate::i18n::t("business.order_status_inquiry")),
        "ACCEPTED" => Some(crate::i18n::t("business.order_status_accepted")),
        "DECLINED" => Some(crate::i18n::t("business.order_status_declined")),
        _ => None,
    }
}

/// The items line for an order summary: "1 item" or
/// "{count} items".
pub(crate) fn order_items_text(item_count: u32) -> String {
    if item_count == 1 {
        crate::i18n::t("business.order_one_item")
    } else {
        crate::i18n::tf("business.order_items", "count", &item_count.to_string())
    }
}

impl App {
    /// The order details dialog: the message's own summary
    /// first (items, total, status), then the fetched line
    /// items when they land. Opened from the order card in
    /// the conversation; the fetch runs through
    /// `live::business`.
    pub(crate) fn draw_order_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &self.business.order_dialog else { return };
        let t = self.tokens();
        let summary_items = order_items_text(dialog.item_count);
        let summary_total = crate::live::format_money(dialog.total_amount_1000, &dialog.currency);
        let status_word = order_status_word(&dialog.status);
        let state = dialog.state.clone();
        let mut close = false;
        egui::Area::new(egui::Id::unique("order_dialog_dim"))
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
        egui::Area::new(egui::Id::unique("order_dialog"))
            .pivot(egui::Align2::CENTER_CENTER)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                crate::theme::glass_panel_frame(t, crate::theme::RADIUS_PANEL)
                    .inner_margin(Margin::symmetric(16, 14))
                    .show(ui, |ui| {
                        ui.set_width(300.0);
                        ui.label(
                            RichText::new(crate::i18n::t("business.order_details"))
                                .font(crate::theme::semibold(15.0))
                                .color(rgb(t.text_primary)),
                        );
                        ui.add_space(SP_1);
                        let mut summary = format!("{summary_items} · {summary_total}");
                        if let Some(word) = &status_word {
                            summary.push_str(" · ");
                            summary.push_str(word);
                        }
                        ui.label(RichText::new(summary).size(12.0).color(rgb(t.text_secondary)));
                        ui.add_space(SP_3);
                        match &state {
                            OrderDialogState::Loading => {
                                ui.label(
                                    RichText::new(crate::i18n::t("business.order_loading"))
                                        .size(12.0)
                                        .color(rgb(t.text_secondary)),
                                );
                            }
                            OrderDialogState::Failed(message) => {
                                ui.label(RichText::new(message).size(12.0).color(rgb(t.danger)));
                            }
                            OrderDialogState::Loaded(order) => {
                                for item in &order.items {
                                    ui.horizontal(|ui| {
                                        let name = item.name.clone().unwrap_or_default();
                                        let mut line = name;
                                        if let Some(qty) = item.quantity {
                                            if qty != 1 {
                                                line = format!("{line} × {qty}");
                                            }
                                        }
                                        ui.label(
                                            RichText::new(line)
                                                .size(15.0)
                                                .color(rgb(t.text_primary)),
                                        );
                                        if let Some(price) = item.price_amount_1000 {
                                            let currency =
                                                item.price_currency.clone().unwrap_or_default();
                                            let qty = item.quantity.unwrap_or(1).max(1);
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.label(
                                                        RichText::new(crate::live::format_money(
                                                            price * qty,
                                                            &currency,
                                                        ))
                                                        .size(15.0)
                                                        .color(rgb(t.text_secondary)),
                                                    );
                                                },
                                            );
                                        }
                                    });
                                    for (vname, vvalue) in &item.variant {
                                        ui.label(
                                            RichText::new(format!("{vname}: {vvalue}"))
                                                .size(12.0)
                                                .color(rgb(t.text_faint)),
                                        );
                                    }
                                    ui.add_space(SP_1);
                                }
                                if let Some(subtotal) = order.subtotal_amount_1000 {
                                    total_row(
                                        ui,
                                        t,
                                        &crate::i18n::t("business.order_subtotal"),
                                        crate::live::format_money(
                                            subtotal,
                                            order.currency.as_deref().unwrap_or_default(),
                                        ),
                                        false,
                                    );
                                }
                                if let Some(total) = order.total_amount_1000 {
                                    total_row(
                                        ui,
                                        t,
                                        &crate::i18n::t("business.order_total"),
                                        crate::live::format_money(
                                            total,
                                            order.currency.as_deref().unwrap_or_default(),
                                        ),
                                        true,
                                    );
                                }
                            }
                        }
                        ui.add_space(SP_3);
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
        if close {
            self.business.order_dialog = None;
        }
    }
}

/// One money row of the order dialog: label left, amount
/// right, the total in the stronger voice.
fn total_row(ui: &mut egui::Ui, t: core::theme::Tokens, label: &str, amount: String, strong: bool) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(15.0).color(if strong {
            rgb(t.text_primary)
        } else {
            rgb(t.text_secondary)
        }));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let text = RichText::new(amount).size(15.0);
            ui.label(if strong {
                text.font(crate::theme::semibold(15.0)).color(rgb(t.text_primary))
            } else {
                text.color(rgb(t.text_secondary))
            });
        });
    });
}
