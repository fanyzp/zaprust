//! The lock surfaces: the code prompt modal (the locked
//! folder's gate and the create-code flow) and the full-window
//! app lock screen.
//!
//! Both surfaces speak plainly about what the code is: a local
//! window gate. It stops casual access to the open window; it
//! does not encrypt the archive, and the chat-lock state that
//! syncs to the account carries no code with it (the code
//! never leaves this machine). See core::lockcode for the
//! stored form and the full honesty bounds.

use crate::state::{App, CodeError, CodePrompt};
use crate::theme::{rgb, SP_1, SP_2, SP_2_5};
use eframe::egui::{self, Align2, Margin, RichText, Vec2};

impl App {
    /// The code prompt modal: a centered glass card over the
    /// unified dim, in the house confirmation modal's shape.
    /// Escape or a click on the dim cancels; only the primary
    /// button (or Enter in a field) submits, so the click that
    /// asked can never be the click that opens the gate.
    pub(crate) fn draw_code_prompt(&mut self, ctx: &egui::Context) {
        let t = self.tokens;
        if let Some(prompt) = &self.code_prompt {
            ctx.data_mut(|d| d.insert_temp(egui::Id::unique("code_prompt_snap"), prompt.clone()));
        }
        let Some(p) =
            crate::anim::popup_phase(ctx, "code_prompt", self.code_prompt.is_some(), self.motion)
        else {
            return;
        };
        let exiting = self.code_prompt.is_none();
        let Some(prompt) =
            ctx.data(|d| d.get_temp::<CodePrompt>(egui::Id::unique("code_prompt_snap")))
        else {
            return;
        };
        let create = !matches!(prompt, CodePrompt::OpenFolder);
        let mut cancel = false;
        let mut submit = false;
        if !exiting && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        egui::Area::new(egui::Id::unique("code_prompt_dim"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::Pos2::ZERO)
            .show(ctx, |ui| {
                let screen = ui.ctx().content_rect();
                let resp = ui.allocate_rect(screen, egui::Sense::click());
                ui.painter().rect_filled(screen, 0.0, crate::theme::modal_dim(p));
                if resp.clicked() && !exiting {
                    cancel = true;
                }
            });
        egui::Area::new(egui::Id::unique("code_prompt"))
            .pivot(Align2::CENTER_CENTER)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if p < 0.999 {
                    ui.set_opacity(p);
                }
                let frame = crate::theme::glass_panel_frame(t, crate::theme::RADIUS_PANEL)
                    .inner_margin(Margin::symmetric(12, 10))
                    .fill(rgb(core::theme::elevated_step(t.bg_panel)));
                frame.show(ui, |ui| {
                    ui.set_width(320.0);
                    ui.add_enabled_ui(!exiting, |ui| {
                        let (title, body) = if create {
                            (
                                crate::i18n::t("locked.create_title"),
                                crate::i18n::t("locked.create_body"),
                            )
                        } else {
                            (crate::i18n::t("nav.locked"), crate::i18n::t("locked.prompt_body"))
                        };
                        ui.label(RichText::new(title).size(15.0).strong());
                        ui.add_space(SP_1);
                        ui.label(RichText::new(body).size(12.0).color(rgb(t.text_secondary)));
                        ui.add_space(SP_2);
                        let field = crate::widgets::text_field_password(
                            ui,
                            t,
                            &mut self.code_entry,
                            &crate::i18n::t(if create {
                                "locked.new_code_hint"
                            } else {
                                "locked.code_hint"
                            }),
                            280.0,
                        );
                        if !field.has_focus() && self.code_error.is_none() {
                            // Focus the code field on open so
                            // the code can be typed at once.
                            field.request_focus();
                        }
                        if field.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                            submit = true;
                        }
                        if create {
                            ui.add_space(SP_1);
                            let repeat = crate::widgets::text_field_password(
                                ui,
                                t,
                                &mut self.code_entry_repeat,
                                &crate::i18n::t("locked.repeat_hint"),
                                280.0,
                            );
                            if repeat.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter))
                            {
                                submit = true;
                            }
                        }
                        if let Some(error) = self.code_error {
                            ui.add_space(SP_1);
                            ui.label(
                                RichText::new(crate::i18n::t(match error {
                                    CodeError::Wrong => "locked.wrong_code",
                                    CodeError::Mismatch => "locked.mismatch",
                                    CodeError::TooShort => "locked.too_short",
                                }))
                                .size(12.0)
                                .color(rgb(t.danger)),
                            );
                        }
                        ui.add_space(SP_2_5);
                        ui.horizontal(|ui| {
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
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if crate::widgets::button(
                                        ui,
                                        t,
                                        &crate::i18n::t(if create {
                                            "locked.create_code"
                                        } else {
                                            "locked.unlock"
                                        }),
                                        crate::widgets::Btn::Primary,
                                    )
                                    .clicked()
                                    {
                                        submit = true;
                                    }
                                },
                            );
                        });
                    });
                });
            });
        crate::anim::popup_transform(
            ctx,
            egui::Id::unique("code_prompt"),
            ctx.content_rect().center(),
            p,
        );
        if cancel && !exiting {
            self.close_code_prompt();
        }
        if submit && !exiting {
            self.submit_code_prompt();
        }
    }

    /// The app lock screen: the whole window while the app is
    /// locked. Backend polling continues behind it (the shell
    /// polls before drawing this), but nothing of the account
    /// shows: no chats, no previews, no names. The recovery for
    /// a forgotten code is stated on the surface: log out to
    /// unlink this device (the chats on the phone are not
    /// affected); the log-out arms on the first click and
    /// fires on the second, the house confirmation discipline
    /// inside the locked surface.
    pub(crate) fn draw_lock_screen(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens();
        let mut submit = false;
        egui::CentralPanel::default().frame(egui::Frame::new().fill(rgb(t.bg_app))).show(
            ui,
            |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space((ui.available_height() * 0.24).max(24.0));
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(56.0), egui::Sense::hover());
                    crate::icons::draw(
                        ui.painter(),
                        crate::icons::Icon::Lock,
                        rect.center(),
                        44.0,
                        rgb(t.text_secondary),
                        rgb(t.bg_app),
                    );
                    ui.add_space(SP_2);
                    ui.label(
                        RichText::new("Zaprust")
                            .font(crate::theme::semibold(19.0))
                            .color(rgb(t.text_primary)),
                    );
                    ui.add_space(SP_1);
                    ui.label(
                        RichText::new(crate::i18n::t("lockscreen.body"))
                            .color(rgb(t.text_secondary)),
                    );
                    ui.add_space(SP_2_5);
                    // The field's frame fills its allocation,
                    // so the allocation is what sets the width:
                    // a fixed 240 pt slot, centered by the
                    // layout, never the whole window.
                    let field = ui
                        .allocate_ui(Vec2::new(240.0, 38.0), |ui| {
                            crate::widgets::text_field_password(
                                ui,
                                t,
                                &mut self.code_entry,
                                &crate::i18n::t("locked.code_hint"),
                                240.0,
                            )
                        })
                        .inner;
                    if !field.has_focus() && self.code_error.is_none() {
                        field.request_focus();
                    }
                    if field.lost_focus() && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter)) {
                        submit = true;
                    }
                    if let Some(error) = self.code_error {
                        ui.add_space(SP_1);
                        ui.label(
                            RichText::new(crate::i18n::t(match error {
                                CodeError::Wrong => "locked.wrong_code",
                                _ => "locked.wrong_code",
                            }))
                            .size(12.0)
                            .color(rgb(t.danger)),
                        );
                    }
                    ui.add_space(SP_2);
                    if crate::widgets::button(
                        ui,
                        t,
                        &crate::i18n::t("locked.unlock"),
                        crate::widgets::Btn::Primary,
                    )
                    .clicked()
                    {
                        submit = true;
                    }
                    if self.live.is_some() {
                        ui.add_space(SP_2_5);
                        ui.label(
                            RichText::new(crate::i18n::t("lockscreen.forgot"))
                                .size(12.0)
                                .color(rgb(t.text_secondary)),
                        );
                        ui.add_space(SP_1);
                        let label = if self.lock_logout_armed {
                            crate::i18n::t("lockscreen.logout_confirm")
                        } else {
                            crate::i18n::t("account.log_out")
                        };
                        if crate::widgets::button(
                            ui,
                            t,
                            &label,
                            if self.lock_logout_armed {
                                crate::widgets::Btn::Danger
                            } else {
                                crate::widgets::Btn::Secondary
                            },
                        )
                        .clicked()
                        {
                            if self.lock_logout_armed {
                                self.log_out();
                            } else {
                                self.lock_logout_armed = true;
                            }
                        }
                    }
                });
            },
        );
        if submit {
            if self.lock_code.as_ref().is_some_and(|c| c.verify(&self.code_entry)) {
                self.app_locked_now = false;
                self.lock_logout_armed = false;
                self.code_entry.clear();
                self.code_error = None;
            } else {
                self.code_error = Some(CodeError::Wrong);
            }
        }
    }
}
