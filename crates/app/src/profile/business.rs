//! The business surfaces inside the contact profile dock
//! (parity P10, rows 130): the fetched business profile as a
//! dock section, and the catalog as a dock sub-view that
//! takes the body over the way the gallery does.
//!
//! Both are free functions over the business state: the dock
//! draws inside a scroll closure that cannot call `&mut App`
//! methods, so fetches leave as commands the dock sends when
//! the draw is done.

use super::dock::{dock_card_section, dock_prose, dock_row, dock_separator};
use crate::icons::Icon;
use crate::state::{BusinessState, ProductImageState};
use crate::theme::{rgb, SP_1, SP_2, SP_3};
use backend::{BackendCommand, LiveBusinessProfile, LiveProduct};
use core::theme::Tokens;
use eframe::egui::{self, RichText};

/// Starts a product image fetch when the URL has none yet:
/// the state machine makes repeat calls free, and a failed
/// or gated URL settles through the failure event.
fn ensure_image(business: &mut BusinessState, commands: &mut Vec<BackendCommand>, url: &str) {
    if url.is_empty() || business.product_images.contains_key(url) {
        return;
    }
    business.product_images.insert(url.to_owned(), ProductImageState::Loading);
    commands.push(BackendCommand::FetchProductImage { url: url.to_owned() });
}

/// Lays text out the way the bubble module does (its own
/// helper is private to it).
fn layout_text(
    ui: &egui::Ui,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    wrap: f32,
) -> std::sync::Arc<egui::Galley> {
    ui.ctx().fonts_mut(|f| f.layout(text.to_owned(), font, color, wrap))
}

/// The wire day name's locale key.
fn day_key(day: &str) -> &'static str {
    match day {
        "mon" => "business.day_mon",
        "tue" => "business.day_tue",
        "wed" => "business.day_wed",
        "thu" => "business.day_thu",
        "fri" => "business.day_fri",
        "sat" => "business.day_sat",
        _ => "business.day_sun",
    }
}

/// One day's hours as display text: the mode word, or the
/// open and close times for specific hours.
fn day_hours_text(day: &backend::LiveBusinessDayHours) -> String {
    match day.mode.as_str() {
        "open_24h" => crate::i18n::t("business.open_24h"),
        "appointment_only" => crate::i18n::t("business.appointment_only"),
        "specific_hours" => {
            let fmt = |mins: u32| format!("{:02}:{:02}", mins / 60, mins % 60);
            match (day.open_minutes, day.close_minutes) {
                (Some(o), Some(c)) => format!("{} - {}", fmt(o), fmt(c)),
                _ => crate::i18n::t("business.open_24h"),
            }
        }
        _ => crate::i18n::t("business.closed"),
    }
}

/// The business section of a contact profile: description,
/// contact lines, categories, address, hours, and the row
/// that opens the catalog. Drawn only when the fetch
/// answered with a profile; a contact with no business
/// profile gets no section at all.
pub(super) fn business_section(
    ui: &mut egui::Ui,
    t: Tokens,
    business: &mut BusinessState,
    jid: &str,
    commands: &mut Vec<BackendCommand>,
) {
    let Some(entry) = business.profiles.get(jid) else {
        return;
    };
    let Some(profile) = entry.clone() else {
        return;
    };
    let profile: LiveBusinessProfile = profile;
    dock_card_section(ui, t, &crate::i18n::t("business.title"), None, |ui| {
        if !profile.description.is_empty() {
            dock_prose(ui, |ui| {
                ui.label(RichText::new(&profile.description).size(13.0).color(rgb(t.text_primary)));
            });
            ui.add_space(SP_1);
        }
        if let Some(email) = &profile.email {
            info_line(ui, t, email);
        }
        for site in &profile.websites {
            info_line(ui, t, site);
        }
        if !profile.categories.is_empty() {
            let names: Vec<&str> = profile.categories.iter().map(|c| c.name.as_str()).collect();
            info_line(ui, t, &names.join(", "));
        }
        if let Some(address) = &profile.address {
            info_line(ui, t, address);
        }
        if !profile.hours.is_empty() {
            ui.add_space(SP_2);
            for day in &profile.hours {
                ui.horizontal(|ui| {
                    ui.add_space(super::dock::DOCK_LABEL_X);
                    ui.label(
                        RichText::new(crate::i18n::t(day_key(&day.day)))
                            .size(12.0)
                            .color(rgb(t.text_secondary)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(day_hours_text(day))
                                .size(12.0)
                                .color(rgb(t.text_primary)),
                        );
                    });
                });
            }
        }
        ui.add_space(SP_2);
        dock_separator(ui, t);
        if dock_row(
            ui,
            t,
            Some((Icon::Storefront, t.text_secondary)),
            &crate::i18n::t("business.view_catalog"),
            t.text_primary,
            true,
        )
        .clicked()
        {
            business.catalog_open_for = Some(jid.to_owned());
            if !business.catalogs.contains_key(jid) {
                commands.push(BackendCommand::FetchCollections { chat_jid: jid.to_owned() });
                commands
                    .push(BackendCommand::FetchCatalog { chat_jid: jid.to_owned(), after: None });
            }
        }
    });
}

/// A secondary info line inside the business section.
fn info_line(ui: &mut egui::Ui, t: Tokens, text: &str) {
    dock_prose(ui, |ui| {
        ui.label(RichText::new(text).size(12.5).color(rgb(t.text_secondary)));
    });
}

/// One product row: the thumbnail when its bytes are here
/// (a neutral tile while they load or when they failed),
/// the name, and the price line with the sale price struck
/// through beside it when one is set.
fn product_row(
    ui: &mut egui::Ui,
    t: Tokens,
    business: &mut BusinessState,
    commands: &mut Vec<BackendCommand>,
    product: &LiveProduct,
) {
    let width = ui.available_width();
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(width, 56.0), egui::Sense::hover());
    let painter = ui.painter().clone();
    // The thumbnail slot.
    let tile = egui::Rect::from_min_size(
        egui::pos2(rect.left() + super::dock::DOCK_LABEL_X, rect.top() + 4.0),
        egui::vec2(48.0, 48.0),
    );
    painter.rect_filled(tile, egui::CornerRadius::same(8), rgb(t.bg_panel_alt));
    let image_url: &str = product.image_url.as_deref().unwrap_or("");
    if !image_url.is_empty() {
        ensure_image(business, commands, image_url);
    }
    match business.product_images.get(image_url) {
        Some(ProductImageState::Ready(handle)) => {
            painter.image(
                handle.id(),
                tile,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        _ => {
            crate::icons::draw(
                &painter,
                Icon::Storefront,
                tile.center(),
                20.0,
                rgb(t.text_faint),
                rgb(t.bg_panel_alt),
            );
        }
    }
    let text_x = tile.right() + 10.0;
    let name = product.name.as_deref().unwrap_or("");
    let name_g = layout_text(
        ui,
        name,
        crate::theme::semibold(13.0),
        rgb(t.text_primary),
        rect.right() - text_x - 8.0,
    );
    painter.galley(egui::pos2(text_x, rect.top() + 8.0), name_g, rgb(t.text_primary));
    // The price line: sale price first (it is the price that
    // applies), the regular price struck through after it.
    let mut price_line = String::new();
    if let Some(sale) = product.sale_amount_1000 {
        price_line.push_str(&crate::live::format_money(
            sale,
            product.price_currency.as_deref().unwrap_or(""),
        ));
        if let Some(price) = product.price_amount_1000 {
            price_line.push_str("  ");
            price_line.push_str(&crate::live::format_money(
                price,
                product.price_currency.as_deref().unwrap_or(""),
            ));
        }
    } else if let Some(price) = product.price_amount_1000 {
        price_line.push_str(&crate::live::format_money(
            price,
            product.price_currency.as_deref().unwrap_or(""),
        ));
    }
    if product.availability.as_deref() == Some("OUT_OF_STOCK") {
        if !price_line.is_empty() {
            price_line.push_str("  ·  ");
        }
        price_line.push_str(&crate::i18n::t("business.out_of_stock"));
    }
    let price_g = layout_text(
        ui,
        &price_line,
        egui::FontId::proportional(12.0),
        rgb(t.text_secondary),
        rect.right() - text_x - 8.0,
    );
    painter.galley(egui::pos2(text_x, rect.top() + 30.0), price_g, rgb(t.text_secondary));
}

/// The catalog sub-view: collections as sections when the
/// business has them, otherwise the flat paged product list,
/// with the load-more row while the server offers a cursor.
pub(super) fn catalog_body(
    ui: &mut egui::Ui,
    t: Tokens,
    business: &mut BusinessState,
    jid: &str,
    commands: &mut Vec<BackendCommand>,
) {
    // Back row.
    if dock_row(
        ui,
        t,
        Some((Icon::ChevronLeft, t.text_secondary)),
        &crate::i18n::t("business.catalog"),
        t.text_primary,
        false,
    )
    .clicked()
    {
        business.catalog_open_for = None;
        return;
    }
    ui.add_space(SP_3);
    let Some(state) = business.catalogs.get(jid) else {
        ui.label(
            RichText::new(crate::i18n::t("business.catalog_loading"))
                .size(12.5)
                .color(rgb(t.text_faint)),
        );
        return;
    };
    // The draw needs owned copies: painting a row can queue
    // an image fetch against the same state.
    let products = state.products.clone();
    let collections = state.collections.clone();
    let after = state.after.clone();
    let loading = state.loading;
    let failed = state.failed.is_some();
    if failed {
        ui.label(
            RichText::new(crate::i18n::t("business.catalog_failed"))
                .size(12.5)
                .color(rgb(t.text_secondary)),
        );
        return;
    }
    let collections: Vec<backend::LiveCollection> = collections.unwrap_or_default();
    if products.is_empty() && collections.is_empty() {
        ui.label(
            RichText::new(crate::i18n::t("business.catalog_empty"))
                .size(12.5)
                .color(rgb(t.text_faint)),
        );
        return;
    }
    if !collections.is_empty() {
        for collection in &collections {
            ui.label(
                RichText::new(collection.name.as_deref().unwrap_or(""))
                    .font(crate::theme::semibold(13.0))
                    .color(rgb(t.text_primary)),
            );
            ui.add_space(SP_1);
            for product in collection.products.iter().filter(|p| !p.hidden) {
                product_row(ui, t, business, commands, product);
            }
            ui.add_space(SP_3);
        }
    } else {
        for product in &products {
            product_row(ui, t, business, commands, product);
        }
    }
    if loading {
        ui.label(
            RichText::new(crate::i18n::t("business.catalog_loading"))
                .size(12.0)
                .color(rgb(t.text_faint)),
        );
    } else if let Some(after) = &after {
        let after = after.clone();
        if crate::widgets::button(
            ui,
            t,
            &crate::i18n::t("business.load_more"),
            crate::widgets::Btn::Secondary,
        )
        .clicked()
        {
            if let Some(state) = business.catalogs.get_mut(jid) {
                state.loading = true;
            }
            commands.push(BackendCommand::FetchCatalog {
                chat_jid: jid.to_owned(),
                after: Some(after),
            });
        }
    }
}
