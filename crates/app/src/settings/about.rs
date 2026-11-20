//! The About settings page: version, updates, credits.

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
                UpdatePhase::Idle => {
                    if self.update.auto_check {
                        "Zaprust checks for updates once a day, and only ever \
                         installs one when you say so."
                            .to_owned()
                    } else {
                        "Automatic checks are off, and Zaprust only ever \
                         installs an update when you say so."
                            .to_owned()
                    }
                }
                UpdatePhase::Checking => "Checking for updates...".to_owned(),
                UpdatePhase::UpToDate => "You are on the latest version.".to_owned(),
                UpdatePhase::Available => match &self.update.release {
                    Some(r) => format!("Version {} is available.", r.version),
                    None => "A new version is available.".to_owned(),
                },
                UpdatePhase::Downloading => "Downloading the update and checking it against the \
                     published checksum..."
                    .to_owned(),
                UpdatePhase::Ready => {
                    "The update is installed. Restart Zaprust to finish.".to_owned()
                }
                UpdatePhase::Failed(e) => e.user_message(),
            };
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "about.status",
                t,
                "Status",
                "",
                |ui| {
                    ui.label(RichText::new(status).color(rgb(t.text_secondary)));
                },
            );
            if let (UpdatePhase::Available, Some(r)) = (&self.update.phase, &self.update.release) {
                let notes = r.notes.trim();
                if !notes.is_empty() {
                    divider(ui, t);
                    let summary: String = notes.chars().take(400).collect();
                    row_with(ui, t, "What is new", "", |ui| {
                        ui.label(RichText::new(summary).size(12.5).color(rgb(t.text_secondary)));
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
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "about.auto_check",
                t,
                "Automatic checks",
                "Once a day, ask GitHub whether a newer version exists",
                |ui| switch(ui, &mut auto, t),
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
                    row_with(
                        ui,
                        t,
                        "Update",
                        "Downloads, verifies, and swaps in the new version.",
                        |ui| {
                            if crate::widgets::button(
                                ui,
                                t,
                                "Update now",
                                crate::widgets::Btn::Primary,
                            )
                            .clicked()
                            {
                                update_now = true;
                            }
                        },
                    );
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
        page_header(ui, "About", "Version information and credits.");
        let repo = core::crash::GITHUB_REPO;
        let repo_url = format!("https://github.com/{repo}");
        let mut report_bug = false;
        group_label(ui, t, "App & build");
        settings_card(ui, t, |ui| {
            row_with(
                ui,
                t,
                "Zaprust",
                "A native WhatsApp client in Rust, every feature built in",
                |ui| {
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
                },
            );
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
                "about.build",
                t,
                "Build",
                "Git commit · build channel · target platform",
                |ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} · local · {}",
                            env!("ZAPRUST_GIT_HASH"),
                            env!("ZAPRUST_TARGET")
                        ))
                        .size(12.0)
                        .color(rgb(t.text_secondary)),
                    );
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
                &format!("github.com/{repo}"),
                |ui| {
                    ui.hyperlink_to("github.com/…", &repo_url);
                },
            );
        });
        ui.add_space(16.0);
        self.updates_section(ui, t);
        ui.add_space(16.0);
        group_label(ui, t, "People");
        settings_card(ui, t, |ui| {
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "about.created_by",
                t,
                "Created by",
                "fanyzp",
                |ui| {
                    ui.hyperlink_to("@fanyzp", "https://github.com/fanyzp");
                },
            );
            divider(ui, t);
            row_hl(
                ui,
                &mut self.settings_highlight,
                self.motion,
                "about.contributors",
                t,
                "Contributors",
                "A solo project for now, contributions welcome",
                |ui| {
                    ui.hyperlink_to("Contributors", format!("{repo_url}/graphs/contributors"));
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
                "Opens a prefilled GitHub issue",
                |ui| {
                    if crate::widgets::button(ui, t, "Report a bug", crate::widgets::Btn::Primary)
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
        ui.add_space(16.0);
        group_label(ui, t, "Third-party credits");
        settings_card(ui, t, |ui| {
            for line in [
                "whatsapp-rust: the WhatsApp protocol library Zaprust links against.",
                "SQLite: the archive database engine (public domain).",
                "egui / eframe: immediate-mode GUI (MIT OR Apache-2.0).",
                "Tokio: the async runtime (MIT).",
                "Inter: the UI typeface (SIL Open Font License 1.1).",
                "Icons: Material Symbols Rounded, Google's icon typeface (Apache-2.0).",
                "Theming: dynamic color, generated in the app from a single seed color.",
                "And heartfelt thanks to every other open-source maintainer whose \
                 work Zaprust builds on, the full list lives in CREDITS.md in \
                 the repository.",
            ] {
                ui.label(RichText::new(line).size(12.5).color(rgb(t.text_secondary)));
                ui.add_space(4.0);
            }
        });
    }
}
