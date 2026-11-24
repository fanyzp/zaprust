//! Business surfaces for the WhatsApp backend: reading a
//! business profile, its catalog and collections, and one
//! order's details through the library's business facade,
//! and applying the account's own business profile edits.
//! Every library type is mapped onto the backend's own
//! types here, so nothing of the library's business module
//! crosses the boundary (the layering rule in
//! `docs/ARCHITECTURE.md`).

use whatsapp_rust::prelude::*;
use whatsapp_rust::{
    BusinessHourMode, BusinessHoursConfig, BusinessHoursUpdate, BusinessProfile,
    BusinessProfileUpdate, CatalogOptions, CollectionOptions, DayOfWeek, Price, Product,
};

use crate::{
    LiveBusinessCategory, LiveBusinessDayHours, LiveBusinessHoursUpdate, LiveBusinessProfile,
    LiveBusinessProfileUpdate, LiveCollection, LiveOrder, LiveOrderItem, LiveProduct,
};

use super::util::error_words;

/// Products per catalog page. The official clients page at
/// a similar size; the cursor carries the rest.
const CATALOG_PAGE: u32 = 50;
/// Products fetched inline per collection, and collections
/// per page: the collections query returns a prefix, so the
/// limit is also the truncation signal (a collection that
/// comes back full may hold more).
const COLLECTION_ITEMS: u32 = 20;

/// Largest product image download accepted, in bytes.
const MAX_PRODUCT_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

/// Maps one library business profile onto the contract's.
fn map_profile(profile: BusinessProfile) -> LiveBusinessProfile {
    let hours = profile
        .business_hours
        .business_config
        .unwrap_or_default()
        .iter()
        .map(|config| LiveBusinessDayHours {
            day: config.day_of_week.as_str().to_owned(),
            mode: config.mode.as_str().to_owned(),
            open_minutes: config.open_time,
            close_minutes: config.close_time,
        })
        .collect();
    LiveBusinessProfile {
        description: profile.description,
        email: profile.email,
        websites: profile.website,
        categories: profile
            .categories
            .iter()
            .map(|c| LiveBusinessCategory { id: c.id.clone(), name: c.name.clone() })
            .collect(),
        address: profile.address,
        hours_timezone: profile.business_hours.timezone,
        hours,
    }
}

/// Maps one library product onto the contract's. The
/// display-dimension image URL wins over the stored-size
/// one: it is the size the surface asked for.
fn map_product(product: &Product) -> LiveProduct {
    let price = |p: &Price| (Some(p.amount_1000), p.currency.clone());
    let (price_amount, price_currency) = product.price.as_ref().map_or((None, None), price);
    LiveProduct {
        id: product.id.clone(),
        name: product.name.clone(),
        description: product.description.clone(),
        price_amount_1000: price_amount,
        sale_amount_1000: product.sale_price.as_ref().map(|s| s.price.amount_1000),
        price_currency,
        availability: product.availability.as_ref().map(|a| a.as_str().to_owned()),
        hidden: product.is_hidden.unwrap_or(false),
        image_url: product.images.first().and_then(|img| {
            img.request_image_url.clone().or_else(|| img.original_image_url.clone())
        }),
    }
}

/// Reads one business's profile. `Ok(None)` is the
/// protocol's own answer for an account with no business
/// profile, not a failure.
pub(super) async fn fetch_business_profile(
    client: &Client,
    jid: &Jid,
) -> Result<Option<LiveBusinessProfile>, String> {
    client
        .get_business_profile(jid)
        .await
        .map(|found| found.map(map_profile))
        .map_err(|e| format!("Could not load the business profile: {}", error_words(&e)))
}

/// Reads one page of a business's catalog.
pub(super) async fn fetch_catalog(
    client: &Client,
    jid: &Jid,
    after: Option<String>,
) -> Result<(Vec<LiveProduct>, Option<String>), String> {
    let mut options = CatalogOptions::default().with_limit(CATALOG_PAGE);
    if let Some(cursor) = after {
        options = options.with_after(cursor);
    }
    let page = client
        .business()
        .get_catalog(jid, &options)
        .await
        .map_err(|e| format!("Could not load the catalog: {}", error_words(&e)))?;
    Ok((page.products.iter().map(map_product).collect(), page.after_cursor))
}

/// Reads a business's collections with their inline
/// products.
pub(super) async fn fetch_collections(
    client: &Client,
    jid: &Jid,
) -> Result<Vec<LiveCollection>, String> {
    let options = CollectionOptions::default()
        .with_collection_limit(CATALOG_PAGE)
        .with_item_limit(COLLECTION_ITEMS);
    let page = client
        .business()
        .get_collections(jid, &options)
        .await
        .map_err(|e| format!("Could not load the collections: {}", error_words(&e)))?;
    Ok(page
        .collections
        .iter()
        .map(|collection| LiveCollection {
            id: collection.id.clone(),
            name: collection.name.clone(),
            products: collection.products.iter().map(map_product).collect(),
        })
        .collect())
}

/// Reads one order's line items and totals.
pub(super) async fn fetch_order(
    client: &Client,
    jid: &Jid,
    order_id: &str,
    token: &str,
) -> Result<LiveOrder, String> {
    let order = client
        .business()
        .get_order(jid, order_id, token)
        .await
        .map_err(|e| format!("Could not load the order: {}", error_words(&e)))?;
    let items = order
        .products
        .iter()
        .map(|item| LiveOrderItem {
            name: item.name.clone(),
            price_amount_1000: item.price.as_ref().map(|p| p.amount_1000),
            price_currency: item.price.as_ref().and_then(|p| p.currency.clone()),
            quantity: item.quantity,
            variant: item
                .variant_properties
                .iter()
                .filter_map(|v| Some((v.name.clone()?, v.value.clone()?)))
                .collect(),
        })
        .collect();
    let details = order.price_details.as_ref();
    Ok(LiveOrder {
        items,
        subtotal_amount_1000: details.and_then(|d| d.subtotal.as_ref().map(|p| p.amount_1000)),
        total_amount_1000: details.and_then(|d| d.total.as_ref().map(|p| p.amount_1000)),
        currency: details.and_then(|d| d.currency.clone()),
    })
}

/// Downloads one catalog product image from its hosted
/// URL. The URL passes the same public-HTTPS gate the
/// profile picture download uses, and the read is
/// size-capped; redirects are not followed, for the same
/// reason as there. Blocking HTTP; call on a blocking
/// thread.
pub(super) fn download_product_image(url: &str) -> Option<Vec<u8>> {
    if !core::net::public_https_url_allowed(url) {
        return None;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(20)))
        .max_redirects(0)
        .build()
        .into();
    let mut response = agent.get(url).call().ok()?;
    response.body_mut().with_config().limit(MAX_PRODUCT_IMAGE_BYTES).read_to_vec().ok()
}

/// Applies one delta to the account's own business
/// profile, mapping the contract's tri-state fields onto
/// the library's delta type field by field.
pub(super) async fn update_own_business_profile(
    client: &Client,
    update: &LiveBusinessProfileUpdate,
) -> Result<(), String> {
    let mut delta = BusinessProfileUpdate::default();
    // Text fields: Some("") is the wire's clear, exactly
    // what the contract's Some(None) means.
    let text =
        |field: &Option<Option<String>>| field.as_ref().map(|v| v.clone().unwrap_or_default());
    delta.description = text(&update.description);
    delta.email = text(&update.email);
    delta.address = text(&update.address);
    delta.websites = update.websites.clone();
    delta.categories = update.categories.clone();
    if let Some(hours) = &update.hours {
        delta.business_hours = Some(map_hours_update(hours));
    }
    client
        .business()
        .update_profile(&delta)
        .await
        .map_err(|e| format!("Business profile change failed: {}", error_words(&e)))
}

/// Maps the contract's hours update onto the library's.
fn map_hours_update(hours: &LiveBusinessHoursUpdate) -> BusinessHoursUpdate {
    BusinessHoursUpdate {
        timezone: hours.timezone.clone(),
        note: hours.note.clone(),
        config: hours
            .days
            .iter()
            .map(|day| {
                let mode = BusinessHourMode::from(day.mode.as_str());
                match (day.open_minutes, day.close_minutes) {
                    (Some(open), Some(close)) => BusinessHoursConfig::with_hours(
                        DayOfWeek::from(day.day.as_str()),
                        mode,
                        open,
                        close,
                    ),
                    _ => BusinessHoursConfig::new(DayOfWeek::from(day.day.as_str()), mode),
                }
            })
            .collect(),
    }
}
