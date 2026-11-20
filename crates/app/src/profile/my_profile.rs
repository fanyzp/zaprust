//! The signed-in user's own profile editor panel: its state, its
//! intents, and the draw routine the shell hosts. Kept apart from
//! the chat profile dock because it edits instead of presenting.

use crate::icons::Icon;
use crate::theme::rgb;
use crate::widgets::{button, text_field, Btn};
use core::theme::Tokens;
use eframe::egui::{self, RichText, Stroke, Vec2};

/// Editable state for the "My profile" screen: the signed-in
/// user's own display name, about line and profile photo.
///
/// The shell owns one of these (it is deliberately NOT an `App`
/// field in this module: persistence wiring lives outside this
/// file). Seed it from the persisted settings at startup, pass it
/// to [`draw_my_profile_panel`] while the screen is open, and on a
/// save intent write `name`, `about` and `photo_path` back to the
/// settings store (suggested settings.json keys: `my_name`,
/// `my_about`, `my_photo_path`). The photo texture is derived
/// state only and is never persisted; it is rebuilt from
/// `photo_path` on demand.
#[derive(Default)]
pub struct MyProfileState {
    /// Display name shown to other people.
    pub name: String,
    /// About / status line shown under the name.
    pub about: String,
    /// Filesystem path of the chosen profile photo, if any. There
    /// is no file-dialog dependency in the workspace, so the path
    /// is entered (or handed in by the shell) rather than picked
    /// through a native dialog; see [`MyProfileIntents::pick_photo`].
    pub photo_path: Option<String>,
    /// Path text currently in the photo field (the edit buffer;
    /// committed to `photo_path` on Load/Save).
    pub photo_input: String,
    /// Decoded preview of the photo at `photo_path`.
    pub photo_texture: Option<egui::TextureHandle>,
    /// The path `photo_texture` was decoded from, so the preview
    /// reloads only when the path actually changes.
    pub photo_loaded_for: Option<String>,
    /// True when the current photo path failed to decode.
    pub photo_error: bool,
    /// Set once the shell has seeded the persisted values, so the
    /// screen never overwrites them with defaults.
    pub seeded: bool,
}

/// Intents from the "My profile" screen, applied by the shell
/// after the draw (same pattern as [`ProfileIntents`]).
#[derive(Debug, Default)]
pub struct MyProfileIntents {
    /// Close the screen without saving.
    pub close: bool,
    /// Save was clicked: persist `name`, `about` and `photo_path`
    /// from the state, then typically close.
    pub save: bool,
    /// "Change photo" was clicked. No file-dialog crate exists in
    /// the workspace yet, so the screen answers by focusing the
    /// photo-path field; if the shell later adds a dialog (e.g.
    /// `rfd`) it should intercept this intent, open the dialog,
    /// and write the result into `photo_input` + `photo_path`.
    pub pick_photo: bool,
    /// "Remove photo" was clicked: `photo_path` was already
    /// cleared in the state; persist on the next save.
    pub remove_photo: bool,
}

/// Loads (or reloads) the preview texture for the state's current
/// `photo_path`, decoding with the `image` crate the app already
/// depends on. A path that fails to decode sets `photo_error` and
/// drops the preview instead of panicking.
fn my_profile_sync_photo(ctx: &egui::Context, state: &mut MyProfileState) {
    if state.photo_loaded_for == state.photo_path && !state.photo_error {
        return;
    }
    state.photo_loaded_for.clone_from(&state.photo_path);
    state.photo_texture = None;
    state.photo_error = false;
    let Some(path) = state.photo_path.clone() else {
        return;
    };
    match image::open(&path) {
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

/// The "My profile" screen: the self variant of the profile
/// family, laid out like WhatsApp's own-profile editor. Large
/// avatar with a change-photo affordance, then the editable
/// display name and about line as form rows (shared `form_field` +
/// `text_field`), and an explicit Save button (nothing persists
/// until it is clicked).
///
/// The shell calls this while its my-profile flag is set, passes
/// its persisted [`MyProfileState`], and applies the returned
/// [`MyProfileIntents`]: on `save`, write `name` / `about` /
/// `photo_path` to settings.json and show the `profile.my_saved`
/// notice. Free function (not an `App` method) because the state
/// lives with the shell; see the integration notes in this file's
/// report.
pub fn draw_my_profile_panel(
    ctx: &egui::Context,
    t: Tokens,
    state: &mut MyProfileState,
    open: bool,
    motion: crate::motion::MotionSetting,
) -> MyProfileIntents {
    let mut intents = MyProfileIntents::default();
    // Visibility runs through the popup phase machine, like the
    // other panels: closed and fully exited draws nothing.
    let Some(fade) = crate::anim::popup_phase(ctx, "my_profile_panel", open, motion) else {
        return intents;
    };
    let exiting = !open;
    my_profile_sync_photo(ctx, state);
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
    let win = egui::Window::new("my_profile")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .frame(crate::theme::glass_frame(t, 16).inner_margin(egui::Margin::same(20)))
        .show(ctx, |ui| {
            if fade < 0.999 {
                ui.set_opacity(fade);
            }
            ui.set_width(380.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(crate::i18n::t("profile.my_title"))
                        .font(crate::theme::semibold(16.0))
                        .color(rgb(t.text_primary)),
                );
            });
            ui.add_space(12.0);
            // Avatar with the change-photo affordance: the whole
            // disc is the button, with a small accent badge on its
            // edge (camera swap affordance, WhatsApp style).
            ui.vertical_centered(|ui| {
                let (rect, resp) =
                    ui.allocate_exact_size(egui::vec2(112.0, 112.0), egui::Sense::click());
                paint_my_avatar(ui.painter(), rect.center(), 56.0, state, t);
                let badge_c = rect.center() + Vec2::new(38.0, 38.0);
                ui.painter().circle_filled(badge_c, 13.0, rgb(t.accent));
                crate::icons::draw(
                    ui.painter(),
                    Icon::Plus,
                    badge_c,
                    15.0,
                    rgb(t.on_accent()),
                    rgb(t.accent),
                );
                if resp
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
                    .font(crate::theme::semibold(17.0))
                    .color(rgb(t.text_primary)),
                );
                if !state.about.trim().is_empty() {
                    ui.label(
                        RichText::new(state.about.clone()).size(12.5).color(rgb(t.text_secondary)),
                    );
                }
            });
            ui.add_space(16.0);
            // Editable fields: the shared form builder, so labels
            // and controls align with Settings.
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
            ui.add_space(10.0);
            crate::widgets::form_field(
                ui,
                t,
                &crate::i18n::t("profile.my_photo_path"),
                Some(&crate::i18n::t("profile.my_photo_path_hint")),
                |ui| {
                    ui.vertical(|ui| {
                        let resp = text_field(
                            ui,
                            t,
                            &mut state.photo_input,
                            &crate::i18n::t("profile.my_photo_placeholder"),
                            crate::widgets::FORM_CONTROL_W,
                        );
                        if resp.changed() {
                            let p = state.photo_input.trim().to_owned();
                            state.photo_path = if p.is_empty() { None } else { Some(p) };
                        }
                        if state.photo_error {
                            ui.label(
                                RichText::new(crate::i18n::t("profile.my_photo_error"))
                                    .size(11.0)
                                    .color(rgb(t.danger)),
                            );
                        }
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if button(
                                ui,
                                t,
                                &crate::i18n::t("profile.my_change_photo"),
                                Btn::Secondary,
                            )
                            .clicked()
                            {
                                intents.pick_photo = true;
                            }
                            if state.photo_path.is_some()
                                && button(
                                    ui,
                                    t,
                                    &crate::i18n::t("profile.my_remove_photo"),
                                    Btn::Danger,
                                )
                                .clicked()
                            {
                                state.photo_path = None;
                                state.photo_input.clear();
                                intents.remove_photo = true;
                            }
                        });
                    })
                },
            );
            ui.add_space(16.0);
            // Dialog convention (the same one the agent confirm
            // spells out): the safe action first, the primary
            // action last.
            ui.horizontal(|ui| {
                if button(ui, t, "Close", Btn::Secondary).clicked() {
                    intents.close = true;
                }
                if button(ui, t, &crate::i18n::t("profile.my_save"), Btn::Primary).clicked() {
                    intents.save = true;
                }
            });
        });
    if let Some(win) = &win {
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::unique("zaprust_glass_edge"),
        ));
        crate::theme::paint_glass_edge(&painter, win.response.rect, 16.0, t);
    }
    if exiting {
        // The exit run only replays the panel; its intents are
        // discarded.
        return MyProfileIntents::default();
    }
    intents
}
