//! The About settings page: version, updates and people.

use super::*;

impl App {
    /// The Updates group on the About page: current status, a
    /// manual check button, and the one-click update offer. System
    /// package installs get a link to the release page instead of a
    /// swap button, because the app cannot (and must not) overwrite
    /// files owned by the system package manager.
    fn updates_section(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        use crate::update::UpdatePhase;
        let ctx = ui.ctx().clone();
        let (_, install_kind) = crate::update::install_location();
        let busy = matches!(self.update.phase, UpdatePhase::Checking | UpdatePhase::Downloading);
        let mut check_now = false;
        let mut update_now = false;
        let mut restart_now = false;
        group_label(ui, t, "Updates");
        settings_card(ui, t, |ui| {
            let status: String = match &self.update.phase {
                // The status row states the phase, nothing
                // more: the reassurance copy it used to carry
                // (daily checks, installs only on request)
                // explained the app to its user and repeated
                // what the Automatic checks switch below
                // already says.
                UpdatePhase::Idle => "Not checked yet.".to_owned(),
                UpdatePhase::Checking => "Checking for updates...".to_owned(),
                UpdatePhase::UpToDate => "You are on the latest version.".to_owned(),
                UpdatePhase::Available => match &self.update.release {
                    Some(r) => format!("Version {} is available.", r.version),
                    None => "A new version is available.".to_owned(),
                },
                UpdatePhase::Downloading => "Downloading the update...".to_owned(),
                UpdatePhase::Ready => {
                    "The update is installed. Restart Zaprust to finish.".to_owned()
                }
                UpdatePhase::Failed(e) => e.user_message(),
            };
            // The busy phases render through the shared loading
            // row, so a check or download in flight reads as
            // waiting like every other wait in the app instead of
            // as static status text.
            let waiting =
                matches!(self.update.phase, UpdatePhase::Checking | UpdatePhase::Downloading);
            // A failure speaks in the danger token like every
            // other failure in the app, not in the secondary voice
            // of an ordinary status.
            let failed = matches!(self.update.phase, UpdatePhase::Failed(_));
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "about.status",
                t,
                "Status",
                "",
                |ui| {
                    if waiting {
                        crate::widgets::loading_row(ui, &status, 14.0, t.text_secondary);
                    } else if failed {
                        ui.label(RichText::new(status).color(rgb(t.danger)));
                    } else {
                        ui.label(RichText::new(status).color(rgb(t.text_secondary)));
                    }
                },
            );
            if let (UpdatePhase::Available, Some(r)) = (&self.update.phase, &self.update.release) {
                let notes = r.notes.trim();
                if !notes.is_empty() {
                    divider(ui, t);
                    let summary: String = notes.chars().take(400).collect();
                    row_with_text_control(ui, t, "What is new", "", |ui| {
                        ui.label(RichText::new(summary).size(12.0).color(rgb(t.text_secondary)));
                    });
                }
            }
            divider(ui, t);
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "about.check_updates",
                t,
                "Version check",
                "",
                |ui| {
                    if crate::widgets::button_enabled(
                        ui,
                        t,
                        "Check for updates",
                        crate::widgets::Btn::Secondary,
                        !busy,
                    )
                    .clicked()
                    {
                        check_now = true;
                    }
                },
            );
            divider(ui, t);
            let mut auto = self.update.auto_check;
            switch_row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "about.auto_check",
                t,
                "Automatic checks",
                "",
                &mut auto,
            );
            if auto != self.update.auto_check {
                self.update.auto_check = auto;
                self.persist_settings();
            }
            if self.update.phase == UpdatePhase::Available {
                divider(ui, t);
                if install_kind == core::update::InstallKind::SystemPackage {
                    row_with(
                        ui,
                        t,
                        "Installed by your system",
                        "This copy came from a system package, so Zaprust \
                         cannot replace it by itself. Get the new version \
                         from the release page instead.",
                        |ui| {
                            ui.hyperlink_to(
                                "Open the release page",
                                crate::update::release_page_url(),
                            );
                        },
                    );
                } else {
                    row_with(ui, t, "Update", "", |ui| {
                        if crate::widgets::button(ui, t, "Update now", crate::widgets::Btn::Primary)
                            .clicked()
                        {
                            update_now = true;
                        }
                    });
                }
            }
            if self.update.phase == UpdatePhase::Ready {
                divider(ui, t);
                row_with(ui, t, "Finish", "", |ui| {
                    if crate::widgets::button(ui, t, "Restart now", crate::widgets::Btn::Primary)
                        .clicked()
                    {
                        restart_now = true;
                    }
                });
            }
        });
        if check_now {
            self.start_update_check(&ctx, true);
        }
        if update_now {
            self.start_update_download(&ctx);
        }
        if restart_now {
            self.restart_into_update();
        }
    }

    pub(crate) fn page_about(&mut self, ui: &mut egui::Ui, t: core::theme::Tokens) {
        page_header(ui, "About");
        let repo = core::crash::GITHUB_REPO;
        let repo_url = format!("https://github.com/{repo}");
        let mut report_bug = false;
        group_label(ui, t, "App");
        settings_card(ui, t, |ui| {
            row_with_text_control(ui, t, "Zaprust", "", |ui| {
                if let Some(tex) = crate::widgets::brand_texture(ui.ctx(), t) {
                    ui.image((tex, egui::Vec2::splat(crate::icons::SIZE_ROW)));
                }
                ui.label(
                    RichText::new(if self.live_mode {
                        if self.live.as_ref().is_some_and(|s| s.fake) {
                            "Live mode (test mode)"
                        } else {
                            "Live mode"
                        }
                    } else {
                        "Demo mode"
                    })
                    .color(rgb(t.text_secondary)),
                );
            });
            divider(ui, t);
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "about.version",
                t,
                "Version",
                "",
                |ui| {
                    ui.label(RichText::new(version_label()).color(rgb(t.text_secondary)));
                },
            );
            divider(ui, t);
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "about.license",
                t,
                "License",
                "",
                |ui| {
                    ui.label(RichText::new("MIT").color(rgb(t.text_secondary)));
                },
            );
            divider(ui, t);
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "about.repository",
                t,
                "Repository",
                "",
                |ui| {
                    ui.hyperlink_to(format!("github.com/{repo}"), &repo_url);
                },
            );
        });
        ui.add_space(SP_4);
        self.updates_section(ui, t);
        ui.add_space(SP_4);
        group_label(ui, t, "People");
        settings_card(ui, t, |ui| {
            // The creator is a fact, not a link farm: the
            // name stands as plain text (the Repository row
            // above is the page's one GitHub link), the
            // Contributors row that repeated the same repo
            // is gone, and the bug report keeps its function
            // at secondary weight instead of a primary call
            // to action.
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "about.created_by",
                t,
                "Created by",
                "",
                |ui| {
                    ui.label(RichText::new("fanyzp").color(rgb(t.text_secondary)));
                },
            );
            divider(ui, t);
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "about.report_bug",
                t,
                "Report a bug",
                "",
                |ui| {
                    if crate::widgets::button(ui, t, "Report a bug", crate::widgets::Btn::Secondary)
                        .clicked()
                    {
                        report_bug = true;
                    }
                },
            );
        });
        if report_bug {
            let url = core::crash::bug_issue_url();
            crate::crash_ui::open_or_copy(self, ui, &url);
        }
    }
}
