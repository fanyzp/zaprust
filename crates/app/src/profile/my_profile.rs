//! The signed-in user's own profile editor panel: its state, its
//! intents, and the draw routine the shell hosts. Kept apart from
//! the chat profile dock because it edits instead of presenting.
//!
//! In live mode the editor edits the account itself: the shell
//! seeds it from the backend's `OwnProfile` read-back, a save
//! issues `UpdateOwnProfile` with exactly the changed fields, and
//! the panel then shows the saving state until the backend's
//! re-read lands (adopted as the new truth) or its failure event
//! arrives (shown in the panel, buffers reverted to the account
//! values). The settings.json copies of the name and About are a
//! cache of the last adopted read-back only; they never stand in
//! for the account. In demo mode there is no account, and the
//! editor keeps its local behavior on the persisted values, as
//! the rest of demo mode does.

use crate::icons::Icon;
use crate::theme::rgb;
use crate::widgets::{button, button_enabled, text_field, Btn};
use core::theme::Tokens;
use eframe::egui::{self, RichText, Stroke, Vec2};

/// Which world the editor is running in, decided by the shell
/// from the session state on every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MyProfileMode {
    /// No live session: demo mode, the editor works on the
    /// locally persisted values.
    Demo,
    /// A linked live session: the editor works on the account's
    /// real profile through the backend.
    Live,
    /// A live session exists but is not linked yet: there is no
    /// account profile to edit, and the panel says so instead of
    /// offering a form that would save nowhere.
    NotLinked,
}

/// The account's own profile as last read back from the server:
/// the truth the editor's buffers are diffed and reconciled
/// against in live mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountProfile {
    /// The account's push name (display name).
    pub name: String,
    /// The account's About / text status (empty when unset).
    pub about: String,
    /// The account's current profile picture bytes, if any.
    pub photo: Option<Vec<u8>>,
}

/// Where a live-mode save stands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SavePhase {
    /// No save in flight.
    #[default]
    Idle,
    /// The save command was sent; the panel waits for the
    /// backend's re-read (success) or failure event.
    Saving,
    /// The last save failed on the server; the message is in
    /// user words and the buffers were reverted to the account
    /// values.
    Failed(String),
}

/// One pending profile change, built from the editor buffers by
/// [`MyProfileState::pending_update`]: exactly the fields that
/// differ from the account read-back, in the shape the backend's
/// `UpdateOwnProfile` command carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnProfileUpdate {
    /// New push name, when changed.
    pub name: Option<String>,
    /// New About, when changed (empty clears it).
    pub about: Option<String>,
    /// New picture, when changed: `Some(Some(jpeg))` sets,
    /// `Some(None)` removes.
    pub photo: Option<Option<Vec<u8>>>,
}

/// What the preview texture was decoded from, so it reloads only
/// when the source actually changes. Byte sources key on a
/// content fingerprint, never on identity: a re-read that returns
/// the same picture must not redecode it every frame.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PhotoKey {
    Path(String),
    Bytes(u64),
}

/// Editable state for the "My profile" screen.
///
/// `name` and `about` are edit buffers. In live mode they start
/// from the [`AccountProfile`] read-back and return to it after
/// every save outcome; in demo mode they are the persisted
/// values themselves (settings.json keys `my_name`, `my_about`,
/// `my_photo_path`), seeded by the shell at startup.
#[derive(Default)]
pub struct MyProfileState {
    /// Display name edit buffer.
    pub name: String,
    /// About / status line edit buffer.
    pub about: String,
    /// Filesystem path of the chosen profile photo in demo mode,
    /// persisted as `my_photo_path`. Live mode never reads it:
    /// the account's picture is bytes, not a local file.
    pub photo_path: Option<String>,
    /// Decoded preview of the current effective photo.
    pub photo_texture: Option<egui::TextureHandle>,
    /// The source `photo_texture` was decoded from.
    photo_key: Option<PhotoKey>,
    /// True when the current photo source failed to decode.
    pub photo_error: bool,
    /// Set once the shell has seeded the persisted values, so the
    /// screen never overwrites them with defaults.
    pub seeded: bool,
    /// The account's profile as last read back (live mode only).
    pub account: Option<AccountProfile>,
    /// Where the in-flight live save stands.
    pub save_phase: SavePhase,
    /// A photo picked in live mode, as the file's raw bytes,
    /// waiting for the next save (converted to a sized JPEG when
    /// the update is built).
    pub picked_photo: Option<Vec<u8>>,
    /// The user removed the account's photo in the editor; the
    /// removal is issued with the next save.
    pub photo_removed: bool,
}

impl MyProfileState {
    /// Adopts a fresh account read-back as the editor's truth:
    /// the buffers take its values and any pending photo pick or
    /// removal is dropped, because the read-back already reflects
    /// what the server holds.
    pub fn adopt_account(&mut self, profile: AccountProfile) {
        self.name = profile.name.clone();
        self.about = profile.about.clone();
        self.account = Some(profile);
        self.picked_photo = None;
        self.photo_removed = false;
        self.photo_error = false;
    }

    /// Drops everything that came from the linked account when
    /// its session ends (log out): the read-back, the decoded
    /// photo and any unsaved photo change stop being presented
    /// as the user's profile. The demo-mode persisted values
    /// (name, about, photo path) are untouched: they were never
    /// the account's.
    pub fn forget_account(&mut self) {
        self.account = None;
        self.photo_texture = None;
        self.photo_key = None;
        self.picked_photo = None;
        self.photo_removed = false;
        self.save_phase = SavePhase::Idle;
    }

    /// True when the buffers differ from the account read-back
    /// (live mode). Always false before the first read-back.
    pub fn live_dirty(&self) -> bool {
        let Some(account) = &self.account else {
            return false;
        };
        self.name.trim() != account.name
            || self.about.trim() != account.about
            || self.picked_photo.is_some()
            || self.photo_removed
    }

    /// True when the editor shows a photo right now: a pending
    /// pick, or the account's (demo: the picked file's) photo
    /// that was not removed.
    pub fn has_effective_photo(&self, mode: MyProfileMode) -> bool {
        match mode {
            MyProfileMode::Demo => self.photo_path.is_some(),
            MyProfileMode::Live | MyProfileMode::NotLinked => {
                self.picked_photo.is_some()
                    || (!self.photo_removed
                        && self.account.as_ref().is_some_and(|a| a.photo.is_some()))
            }
        }
    }

    /// Builds the pending live-mode change from the buffers, or
    /// `None` when nothing differs from the account read-back.
    /// A picked photo is converted to the sized square JPEG the
    /// protocol carries here, so a conversion failure surfaces
    /// before any command is sent.
    pub fn pending_update(&self) -> Result<Option<OwnProfileUpdate>, String> {
        let Some(account) = &self.account else {
            return Ok(None);
        };
        let name = (self.name.trim() != account.name).then(|| self.name.trim().to_owned());
        let about = (self.about.trim() != account.about).then(|| self.about.trim().to_owned());
        let photo = if let Some(picked) = &self.picked_photo {
            Some(Some(prepare_profile_jpeg(picked)?))
        } else if self.photo_removed && account.photo.is_some() {
            Some(None)
        } else {
            None
        };
        if name.is_none() && about.is_none() && photo.is_none() {
            return Ok(None);
        }
        Ok(Some(OwnProfileUpdate { name, about, photo }))
    }
}

/// Prepares picked image bytes for the profile picture the
/// protocol carries: decoded, center-cropped to a square, sized
/// down to at most 640 by 640, and encoded as JPEG. WhatsApp
/// stores profile pictures at that size; sending the camera
/// original would upload megabytes the server discards.
pub fn prepare_profile_jpeg(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let err = || crate::i18n::t("profile.my_photo_error");
    let img = image::load_from_memory(bytes).map_err(|_| err())?;
    let (w, h) = (img.width(), img.height());
    let side = w.min(h);
    let cropped = img.crop_imm((w - side) / 2, (h - side) / 2, side, side);
    let sized = if side > 640 {
        cropped.resize(640, 640, image::imageops::FilterType::Lanczos3)
    } else {
        cropped
    };
    let rgb = sized.to_rgb8();
    let mut out = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
    image::ImageEncoder::write_image(
        encoder,
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        image::ExtendedColorType::Rgb8,
    )
    .map_err(|_| err())?;
    Ok(out)
}

/// A cheap content fingerprint for picture bytes (FNV-1a over
/// the bytes, seeded with the length), used only to notice when
/// the preview's source changed.
fn photo_fingerprint(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ (bytes.len() as u64);
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The photo the preview should show, as a decode key plus the
/// bytes or path to decode. Live mode: the pending pick wins,
/// then the account's picture unless removed. Demo mode: the
/// picked file's path.
fn photo_source(state: &MyProfileState, mode: MyProfileMode) -> Option<(PhotoKey, PhotoData)> {
    match mode {
        MyProfileMode::Demo => {
            state.photo_path.clone().map(|p| (PhotoKey::Path(p.clone()), PhotoData::Path(p)))
        }
        MyProfileMode::Live | MyProfileMode::NotLinked => {
            if let Some(picked) = &state.picked_photo {
                return Some((
                    PhotoKey::Bytes(photo_fingerprint(picked)),
                    PhotoData::Bytes(picked.clone()),
                ));
            }
            if state.photo_removed {
                return None;
            }
            state.account.as_ref().and_then(|a| a.photo.as_ref()).map(|bytes| {
                (PhotoKey::Bytes(photo_fingerprint(bytes)), PhotoData::Bytes(bytes.clone()))
            })
        }
    }
}

// The decode half of `photo_source`'s return, kept at module
// level so the sync below reads linearly.
enum PhotoData {
    Path(String),
    Bytes(Vec<u8>),
}

/// Loads (or reloads) the preview texture for the state's
/// current effective photo. A source that fails to decode sets
/// `photo_error` and drops the preview instead of panicking.
/// Called by the panel draw and by the shell for the account
/// surfaces, so the account menu and the rail show the same
/// photo as the editor.
pub fn my_profile_sync_photo(ctx: &egui::Context, state: &mut MyProfileState, mode: MyProfileMode) {
    let source = photo_source(state, mode);
    let key = source.as_ref().map(|(k, _)| k.clone());
    if state.photo_key == key && !state.photo_error {
        return;
    }
    state.photo_key = key;
    state.photo_texture = None;
    state.photo_error = false;
    let Some((_, data)) = source else {
        return;
    };
    let decoded = match &data {
        PhotoData::Path(path) => image::open(path),
        PhotoData::Bytes(bytes) => image::load_from_memory(bytes),
    };
    match decoded {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            let color =
                egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
            state.photo_texture =
                Some(ctx.load_texture("my-profile-photo", color, egui::TextureOptions::LINEAR));
        }
        Err(_) => {
            state.photo_error = true;
        }
    }
}

/// Draws the self avatar for the "My profile" screen: the photo
/// clipped to a disc when one is loaded, otherwise the standard
/// accent disc with the white silhouette (the same avatar the rail
/// paints for the signed-in user).
fn paint_my_avatar(
    painter: &egui::Painter,
    center: egui::Pos2,
    radius: f32,
    state: &MyProfileState,
    t: Tokens,
) {
    if let Some(tex) = &state.photo_texture {
        let rect = egui::Rect::from_center_size(center, Vec2::splat(radius * 2.0));
        // egui shapes have no circular clip; a rounded rect with
        // the radius at half the size reads as a disc.
        painter.image(
            tex.id(),
            rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        painter.circle_stroke(center, radius, Stroke::new(1.0_f32, rgb(t.border)));
    } else {
        crate::widgets::paint_avatar_disc(painter, center, radius, false, rgb(t.accent));
    }
}

/// Paints the signed-in user's avatar (photo when one is loaded,
/// the accent disc otherwise) for the account surfaces outside
/// the editor: the account menu header and the rail's account
/// button. `radius` is the disc radius.
pub fn paint_my_avatar_disc(
    painter: &egui::Painter,
    center: egui::Pos2,
    radius: f32,
    state: &MyProfileState,
    t: Tokens,
) {
    paint_my_avatar(painter, center, radius, state, t);
}

/// Intents from the "My profile" screen, applied by the shell
/// after the draw (same pattern as [`ProfileIntents`]).
#[derive(Debug, Default)]
pub struct MyProfileIntents {
    /// Close the screen.
    pub close: bool,
    /// Save was clicked. In demo mode the shell persists the
    /// buffers; in live mode it issues the pending update.
    pub save: bool,
    /// "Change photo" was clicked: the shell opens the file
    /// picker and hands the chosen file's bytes (live) or path
    /// (demo) back into the state.
    pub pick_photo: bool,
    /// "Remove photo" was clicked: the state already reflects
    /// the removal; it takes effect with the next save.
    pub remove_photo: bool,
}

/// The "My profile" screen: the self variant of the profile
/// family, laid out like WhatsApp's own-profile editor. Large
/// avatar with a change-photo affordance, then the editable
/// display name and about line as form rows, and an explicit
/// Save button (nothing is sent or persisted until it is
/// clicked).
///
/// What the screen offers depends on `mode`: demo edits the
/// local values; live edits the account (and shows a loading
/// note until the first read-back lands, a saving note while a
/// save is in flight, and the server's refusal after a failed
/// save); not-linked explains there is no account to edit yet.
pub fn draw_my_profile_panel(
    ctx: &egui::Context,
    t: Tokens,
    state: &mut MyProfileState,
    open: bool,
    motion: crate::motion::MotionSetting,
    mode: MyProfileMode,
) -> MyProfileIntents {
    let mut intents = MyProfileIntents::default();
    // Visibility runs through the popup phase machine, like the
    // other panels: closed and fully exited draws nothing.
    let Some(fade) = crate::anim::popup_phase(ctx, "my_profile_panel", open, motion) else {
        return intents;
    };
    let exiting = !open;
    my_profile_sync_photo(ctx, state, mode);
    egui::Area::new(egui::Id::unique("my_profile_dim"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::Pos2::ZERO)
        .show(ctx, |ui| {
            let screen = ui.ctx().content_rect();
            let resp = ui.allocate_rect(screen, egui::Sense::click());
            ui.painter().rect_filled(screen, 0.0, crate::theme::modal_dim(fade));
            if resp.clicked() {
                intents.close = true;
            }
        });
    let _win = egui::Window::new("my_profile")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .frame(crate::theme::glass_frame(t, crate::theme::RADIUS_PANEL).inner_margin(egui::Margin::same(20)))
        .show(ctx, |ui| {
            if fade < 0.999 {
                ui.set_opacity(fade);
            }
            ui.set_width(380.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(crate::i18n::t("profile.my_title"))
                        .font(crate::theme::semibold(15.0))
                        .color(rgb(t.text_primary)),
                );
            });
            ui.add_space(12.0);
            match mode {
                MyProfileMode::NotLinked => {
                    ui.label(
                        RichText::new(crate::i18n::t("profile.my_not_linked"))
                            .size(12.0)
                            .color(rgb(t.text_secondary)),
                    );
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        if button(ui, t, "Close", Btn::Secondary).clicked() {
                            intents.close = true;
                        }
                    });
                }
                MyProfileMode::Live if state.account.is_none() => {
                    crate::widgets::loading_row(
                        ui,
                        &crate::i18n::t("profile.my_loading"),
                        14.0,
                        t.text_secondary,
                    );
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        if button(ui, t, "Close", Btn::Secondary).clicked() {
                            intents.close = true;
                        }
                    });
                }
                _ => draw_editor_form(ui, t, state, mode, &mut intents),
            }
        });
    if exiting {
        // The exit run only replays the panel; its intents are
        // discarded.
        return MyProfileIntents::default();
    }
    intents
}

/// The editable form itself, shared by demo and live mode: the
/// two modes differ only in what a save does and in the status
/// line (demo saves locally at once; live reports the save's
/// progress and outcome).
fn draw_editor_form(
    ui: &mut egui::Ui,
    t: Tokens,
    state: &mut MyProfileState,
    mode: MyProfileMode,
    intents: &mut MyProfileIntents,
) {
    let saving = state.save_phase == SavePhase::Saving;
    // Avatar with the change-photo affordance: the whole
    // disc is the button, with a small accent badge on its
    // edge (camera swap affordance, WhatsApp style).
    ui.vertical_centered(|ui| {
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(112.0, 112.0), egui::Sense::click());
        paint_my_avatar(ui.painter(), rect.center(), 56.0, state, t);
        let badge_c = rect.center() + Vec2::new(38.0, 38.0);
        ui.painter().circle_filled(badge_c, 13.0, rgb(t.accent));
        crate::icons::draw(
            ui.painter(),
            Icon::Plus,
            badge_c,
            crate::icons::SIZE_ROW,
            rgb(t.on_accent()),
            rgb(t.accent),
        );
        if !saving
            && resp
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(crate::i18n::t("profile.my_change_photo"))
                .clicked()
        {
            intents.pick_photo = true;
        }
        ui.label(
            RichText::new(if state.name.trim().is_empty() {
                crate::i18n::t("profile.my_default_name")
            } else {
                state.name.clone()
            })
            .font(crate::theme::semibold(19.0))
            .color(rgb(t.text_primary)),
        );
        if !state.about.trim().is_empty() {
            ui.label(RichText::new(state.about.clone()).size(12.0).color(rgb(t.text_secondary)));
        }
    });
    ui.add_space(16.0);
    // Editable fields: the shared form builder, so labels
    // and controls align with Settings. While a live save is
    // in flight the fields lock: the read-back that resolves
    // it adopts the account values, and typing into that
    // moment would be silently overwritten.
    ui.add_enabled_ui(!saving, |ui| {
        crate::widgets::form_field(
            ui,
            t,
            &crate::i18n::t("profile.my_name"),
            Some(&crate::i18n::t("profile.my_name_hint")),
            |ui| {
                text_field(
                    ui,
                    t,
                    &mut state.name,
                    &crate::i18n::t("profile.my_name_placeholder"),
                    crate::widgets::FORM_CONTROL_W,
                )
            },
        );
        ui.add_space(10.0);
        crate::widgets::form_field(
            ui,
            t,
            &crate::i18n::t("profile.my_about"),
            Some(&crate::i18n::t("profile.my_about_hint")),
            |ui| {
                text_field(
                    ui,
                    t,
                    &mut state.about,
                    &crate::i18n::t("profile.my_about_placeholder"),
                    crate::widgets::FORM_CONTROL_W,
                )
            },
        );
    });
    ui.add_space(10.0);
    crate::widgets::form_field(ui, t, &crate::i18n::t("profile.my_photo"), None, |ui| {
        ui.horizontal(|ui| {
            if button_enabled(
                ui,
                t,
                &crate::i18n::t("profile.my_change_photo"),
                Btn::Secondary,
                !saving,
            )
            .clicked()
            {
                intents.pick_photo = true;
            }
            if state.has_effective_photo(mode)
                && button_enabled(
                    ui,
                    t,
                    &crate::i18n::t("profile.my_remove_photo"),
                    Btn::Danger,
                    !saving,
                )
                .clicked()
            {
                match mode {
                    MyProfileMode::Demo => state.photo_path = None,
                    MyProfileMode::Live | MyProfileMode::NotLinked => {
                        if state.picked_photo.is_some() {
                            state.picked_photo = None;
                        } else {
                            state.photo_removed = true;
                        }
                    }
                }
                intents.remove_photo = true;
            }
        });
    });
    if state.photo_error {
        ui.add_space(6.0);
        ui.label(
            RichText::new(crate::i18n::t("profile.my_photo_error")).size(10.0).color(rgb(t.danger)),
        );
    }
    if mode == MyProfileMode::Live {
        match &state.save_phase {
            SavePhase::Saving => {
                ui.add_space(10.0);
                crate::widgets::loading_row(
                    ui,
                    &crate::i18n::t("profile.my_saving"),
                    14.0,
                    t.text_secondary,
                );
            }
            SavePhase::Failed(message) => {
                ui.add_space(10.0);
                ui.label(RichText::new(message.clone()).size(12.0).color(rgb(t.danger)));
            }
            SavePhase::Idle => {}
        }
    }
    ui.add_space(16.0);
    // Dialog convention (the same one the agent confirm
    // spells out): the safe action first, the primary
    // action last. In live mode Save is offered only when
    // there is something to send and no save is in flight.
    let save_enabled = match mode {
        MyProfileMode::Demo => true,
        MyProfileMode::Live => !saving && state.live_dirty() && !state.name.trim().is_empty(),
        MyProfileMode::NotLinked => false,
    };
    ui.horizontal(|ui| {
        if button(ui, t, "Close", Btn::Secondary).clicked() {
            intents.close = true;
        }
        if button_enabled(ui, t, &crate::i18n::t("profile.my_save"), Btn::Primary, save_enabled)
            .clicked()
        {
            intents.save = true;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> AccountProfile {
        AccountProfile {
            name: "Account Name".to_owned(),
            about: "Account about".to_owned(),
            photo: None,
        }
    }

    fn state_with_account() -> MyProfileState {
        let mut state = MyProfileState::default();
        state.adopt_account(account());
        state
    }

    /// A tiny valid PNG, produced by the image crate itself so
    /// the fixture cannot drift from what the decoder accepts.
    fn tiny_png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(8, 4, image::Rgba([10, 20, 30, 255]));
        let mut out = Vec::new();
        image::ImageEncoder::write_image(
            image::codecs::png::PngEncoder::new(&mut out),
            img.as_raw(),
            8,
            4,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
        out
    }

    #[test]
    fn adopt_account_seeds_the_buffers_and_clears_pending_photo_changes() {
        let mut state = MyProfileState {
            picked_photo: Some(vec![1, 2, 3]),
            photo_removed: true,
            ..Default::default()
        };
        state.adopt_account(account());
        assert_eq!(state.name, "Account Name");
        assert_eq!(state.about, "Account about");
        assert!(state.picked_photo.is_none());
        assert!(!state.photo_removed);
        assert!(!state.live_dirty(), "a fresh read-back is clean by definition");
    }

    #[test]
    fn pending_update_carries_only_the_changed_fields() {
        let mut state = state_with_account();
        assert_eq!(state.pending_update().unwrap(), None, "no edits, no update");

        state.about = "  New about  ".to_owned();
        let update = state.pending_update().unwrap().expect("an update");
        assert_eq!(update.name, None);
        assert_eq!(update.about.as_deref(), Some("New about"), "values are trimmed");
        assert_eq!(update.photo, None);

        state.name = "Renamed".to_owned();
        state.about = "Account about".to_owned();
        let update = state.pending_update().unwrap().expect("an update");
        assert_eq!(update.name.as_deref(), Some("Renamed"));
        assert_eq!(update.about, None, "reverting a field drops it from the update");
    }

    #[test]
    fn pending_update_clears_the_about_with_an_empty_string() {
        let mut state = state_with_account();
        state.about = String::new();
        let update = state.pending_update().unwrap().expect("an update");
        assert_eq!(update.about.as_deref(), Some(""));
    }

    #[test]
    fn pending_update_removes_and_sets_the_photo() {
        let mut state = state_with_account();
        state.account.as_mut().unwrap().photo = Some(vec![9, 9, 9]);
        state.photo_removed = true;
        let update = state.pending_update().unwrap().expect("an update");
        assert_eq!(update.photo, Some(None), "removal is Some(None)");

        let mut state = state_with_account();
        state.picked_photo = Some(tiny_png());
        let update = state.pending_update().unwrap().expect("an update");
        let jpeg = update.photo.expect("a photo change").expect("a set, not a removal");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "the pick is converted to JPEG");
    }

    #[test]
    fn prepare_profile_jpeg_crops_to_a_square_and_caps_the_size() {
        let big = image::RgbaImage::from_pixel(900, 300, image::Rgba([1, 2, 3, 255]));
        let mut png = Vec::new();
        image::ImageEncoder::write_image(
            image::codecs::png::PngEncoder::new(&mut png),
            big.as_raw(),
            900,
            300,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
        let jpeg = prepare_profile_jpeg(&png).unwrap();
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (300, 300), "center square crop");

        let huge = image::RgbaImage::from_pixel(900, 900, image::Rgba([1, 2, 3, 255]));
        let mut png = Vec::new();
        image::ImageEncoder::write_image(
            image::codecs::png::PngEncoder::new(&mut png),
            huge.as_raw(),
            900,
            900,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
        let jpeg = prepare_profile_jpeg(&png).unwrap();
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (640, 640), "capped at 640");
    }

    #[test]
    fn prepare_profile_jpeg_rejects_undecodable_bytes() {
        assert!(prepare_profile_jpeg(b"not an image").is_err());
        assert!(prepare_profile_jpeg(&[]).is_err());
    }

    #[test]
    fn live_dirty_tracks_each_field() {
        let mut state = state_with_account();
        assert!(!state.live_dirty());
        state.name = "Other".to_owned();
        assert!(state.live_dirty());
        state.name = "Account Name".to_owned();
        assert!(!state.live_dirty());
        state.photo_removed = true;
        assert!(state.live_dirty());
    }
}
