//! Crash-safety UI: the "Zaprust crashed last time" banner, the
//! full-report viewer, and the shared open-a-URL helper (cross-platform
//! via the `open` crate; falls back to copying the URL + a note).

use crate::state::App;
use crate::theme::rgb;
use eframe::egui::{self, Align2, CornerRadius, Margin, Pos2, RichText, Stroke, Vec2};

/// Opens `url` in the system browser. On failure, copies the URL to
/// the clipboard and sets a notice telling the user what happened.
pub(crate) fn open_or_copy(app: &mut App, ui: &egui::Ui, url: &str) {
    if let Ok(()) = open::that(url) {
    } else {
        ui.ctx().copy_text(url.to_owned());
        app.notice =
            Some("Could not open a browser, the link was copied to your clipboard.".to_owned());
    }
}

impl App {
    /// Top-centre banners: crash report first, then the generic
    /// notice. Drawn as overlays above every screen.
    pub(crate) fn draw_banners(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        let area = root.max_rect();
        let mut y = area.top() + 12.0;

        if let Some(report) = self.crash_report.clone() {
            let size = Vec2::new(560.0, 86.0);
            let rect =
                egui::Rect::from_min_size(Pos2::new(area.center().x - size.x / 2.0, y), size);
            let mut action: Option<&str> = None;
            egui::Area::new(egui::Id::unique("crash_banner"))
                .fixed_pos(rect.min)
                .order(egui::Order::Foreground)
                .show(root.ctx(), |ui| {
                    // Text-bearing glass with the danger stroke:
                    // the banner floats over the app and its text
                    // must stay crisp.
                    let banner = egui::Frame::new()
                        .fill(crate::theme::glass_text_tint(t, t.bg_panel))
                        .corner_radius(CornerRadius::same(12))
                        .stroke(Stroke::new(1.0_f32, rgb(t.danger)))
                        .inner_margin(Margin::same(12))
                        .show(ui, |ui| {
                            ui.set_width(size.x - 24.0);
                            ui.label(RichText::new(crate::i18n::t("crash.title")).strong());
                            ui.label(
                                RichText::new(report.summary())
                                    .size(12.0)
                                    .color(rgb(t.text_secondary)),
                            );
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                if crate::widgets::button(
                                    ui,
                                    t,
                                    &crate::i18n::t("crash.report"),
                                    crate::widgets::Btn::Primary,
                                )
                                .clicked()
                                {
                                    action = Some("report");
                                }
                                if crate::widgets::button(
                                    ui,
                                    t,
                                    &crate::i18n::t("crash.details"),
                                    crate::widgets::Btn::Secondary,
                                )
                                .clicked()
                                {
                                    action = Some("details");
                                }
                                if crate::widgets::button(
                                    ui,
                                    t,
                                    &crate::i18n::t("crash.dismiss"),
                                    crate::widgets::Btn::Secondary,
                                )
                                .clicked()
                                {
                                    action = Some("dismiss");
                                }
                            });
                        });
                    crate::theme::paint_glass_edge(ui.painter(), banner.response.rect, 12.0, t);
                });
            match action {
                Some("report") => {
                    let url = core::crash::crash_issue_url(&report);
                    open_or_copy(self, root, &url);
                    self.dismiss_crash();
                }
                Some("details") => self.crash_details_open = true,
                Some("dismiss") => self.dismiss_crash(),
                _ => {}
            }
            y += size.y + 8.0;
        }

        if let Some(notice) = self.notice.clone() {
            // The banner fades/slides in and out like every popup
            // (popup_phase); while an AI worker runs, a spinner
            // rides in front of the text so a busy action reads as
            // busy.
            let phase = crate::anim::popup_phase(root.ctx(), "notice_banner", true, self.motion);
            if let Some(banner_p) = phase {
                let size = Vec2::new(560.0, 40.0);
                let slide = if self.motion.slides() { (1.0 - banner_p) * 10.0 } else { 0.0 };
                let rect = egui::Rect::from_min_size(
                    Pos2::new(area.center().x - size.x / 2.0, y + slide),
                    size,
                );
                let mut dismiss = false;
                egui::Area::new(egui::Id::unique("notice_banner"))
                    .fixed_pos(rect.min)
                    .order(egui::Order::Foreground)
                    .show(root.ctx(), |ui| {
                        if banner_p < 0.999 {
                            ui.set_opacity(banner_p);
                        }
                        let banner = egui::Frame::new()
                            .fill(crate::theme::glass_text_tint(t, t.bg_panel))
                            .corner_radius(CornerRadius::same(12))
                            .stroke(Stroke::new(1.0_f32, rgb(t.border)))
                            .inner_margin(Margin::symmetric(12, 8))
                            .show(ui, |ui| {
                                ui.set_width(size.x - 24.0);
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&notice).size(12.5));
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if crate::widgets::button(
                                                ui,
                                                t,
                                                &crate::i18n::t("crash.dismiss"),
                                                crate::widgets::Btn::Secondary,
                                            )
                                            .clicked()
                                            {
                                                dismiss = true;
                                            }
                                        },
                                    );
                                });
                            });
                        crate::theme::paint_glass_edge(ui.painter(), banner.response.rect, 12.0, t);
                    });
                if dismiss {
                    self.notice = None;
                }
            }
        }

        // Called unconditionally (while a report exists): the
        // modal's exit animation runs after its open flag drops,
        // and the phase machine inside decides whether anything
        // is drawn at all.
        if self.crash_report.is_some() {
            self.draw_crash_details(root);
        }
    }

    fn dismiss_crash(&mut self) {
        if let Some(report) = &self.crash_report {
            let dir = backend::data_dir().join("crashes");
            let _ = core::crash::mark_seen(&dir, report.timestamp_unix);
        }
        self.crash_report = None;
        self.crash_details_open = false;
    }

    /// Modal with the full report text and a Copy button.
    fn draw_crash_details(&mut self, root: &mut egui::Ui) {
        let t = self.tokens();
        let Some(report) = self.crash_report.clone() else {
            self.crash_details_open = false;
            return;
        };
        // Visibility runs through the popup phase machine, like
        // every other modal: fade and scale in, the same motion
        // shorter on the way out. The report outlives the open
        // flag, so the exit run still has its content.
        let Some(det_p) = crate::anim::popup_phase(
            root.ctx(),
            "crash_details",
            self.crash_details_open,
            self.motion,
        ) else {
            return;
        };
        let exiting = !self.crash_details_open;
        let area = root.max_rect();
        root.painter().rect_filled(area, CornerRadius::ZERO, crate::theme::modal_dim(det_p));
        let size = Vec2::new(620.0, 430.0);
        let rect = egui::Rect::from_center_size(area.center(), size);
        let mut close = false;
        let mut copy = false;
        egui::Area::new(egui::Id::unique("crash_details"))
            .fixed_pos(rect.min)
            .order(egui::Order::Foreground)
            .show(root.ctx(), |ui| {
                if det_p < 0.999 {
                    ui.set_opacity(det_p);
                }
                let modal = egui::Frame::new()
                    .fill(crate::theme::glass_text_tint(t, t.bg_panel))
                    .corner_radius(CornerRadius::same(16))
                    .stroke(Stroke::new(1.0_f32, rgb(t.border)))
                    .inner_margin(Margin::same(14))
                    .show(ui, |ui| {
                        ui.set_width(size.x - 28.0);
                        ui.label(RichText::new("Crash report").size(16.0).strong());
                        ui.add_space(6.0);
                        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                            ui.label(
                                RichText::new(report.full_text())
                                    .size(11.5)
                                    .family(egui::FontFamily::Monospace)
                                    .color(rgb(t.text_secondary)),
                            );
                        });
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if crate::widgets::button(ui, t, "Copy", crate::widgets::Btn::Secondary)
                                .clicked()
                            {
                                copy = true;
                            }
                            if crate::widgets::button(
                                ui,
                                t,
                                "Close",
                                crate::widgets::Btn::Secondary,
                            )
                            .clicked()
                            {
                                close = true;
                            }
                        });
                    });
                crate::theme::paint_glass_edge(ui.painter(), modal.response.rect, 16.0, t);
            });
        crate::anim::popup_transform(
            root.ctx(),
            egui::Id::unique("crash_details"),
            area.center(),
            det_p,
        );
        if copy && !exiting {
            root.ctx().copy_text(report.full_text());
            self.notice = Some("Crash report copied to the clipboard.".to_owned());
        }
        if close {
            self.crash_details_open = false;
        }
        let _ = Align2::CENTER_CENTER;
    }
}
