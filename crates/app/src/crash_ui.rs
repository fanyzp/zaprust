//! Crash-safety UI: the "Zaprust crashed last time" banner, the
//! full-report viewer, and the shared open-a-URL helper (cross-platform
//! via the `open` crate; falls back to copying the URL + a note).

use crate::state::{App, Notice, NoticeKind};
use crate::theme::rgb;
use eframe::egui::{self, CornerRadius, Margin, Pos2, RichText, Stroke, Vec2};

/// Opens `url` in the system browser. On failure, copies the URL to
/// the clipboard and sets a notice telling the user what happened.
pub(crate) fn open_or_copy(app: &mut App, ui: &egui::Ui, url: &str) {
    open_or_copy_with(app, ui, url, |url: &str| open::that(url));
}

/// [`open_or_copy`] with the system opener injected, so the
/// fallback is exercisable in tests that have no browser.
fn open_or_copy_with(
    app: &mut App,
    ui: &egui::Ui,
    url: &str,
    opener: impl Fn(&str) -> std::io::Result<()>,
) {
    if opener(url).is_err() {
        ui.ctx().copy_text(url.to_owned());
        app.notice = Some(Notice::info(
            "Could not open a browser, the link was copied to your clipboard.".to_owned(),
        ));
    }
}

impl App {
    /// Top-centre banners on the main screens: crash report first,
    /// then the generic notice. Drawn as overlays above the screen.
    pub(crate) fn draw_banners(&mut self, root: &mut egui::Ui) {
        self.draw_banners_impl(root, false);
    }

    /// The connect screen's banner pass. Two differences from the
    /// main screens, both from the owner's v1.1.0 report: the crash
    /// banner is NOT an overlay here (the connect column renders it
    /// in flow, so it can never paint over the screen's header),
    /// and a notice that only repeats the failed start's reason is
    /// not painted either, because the failed-start panel inside
    /// the card already shows that text in flow. The crash details
    /// modal still draws: it is a real modal, dimmed and centred,
    /// on every screen.
    pub(crate) fn draw_banners_on_connect(&mut self, root: &mut egui::Ui) {
        self.draw_banners_impl(root, true);
    }

    fn draw_banners_impl(&mut self, root: &mut egui::Ui, on_connect: bool) {
        let t = self.tokens();
        let area = root.max_rect();
        let mut y = area.top() + 12.0;

        if !on_connect && self.crash_report.is_some() {
            // The banner sizes to the window: the fixed cap on
            // wide windows, and never wider than the window minus
            // the same 12.0 side margins its top offset uses, so
            // a portrait window never clips it.
            let size = Vec2::new((area.width() - 24.0).min(560.0), 86.0);
            let rect =
                egui::Rect::from_min_size(Pos2::new(area.center().x - size.x / 2.0, y), size);
            let mut height = size.y;
            egui::Area::new(egui::Id::unique("crash_banner"))
                .fixed_pos(rect.min)
                .order(egui::Order::Foreground)
                .show(root.ctx(), |ui| {
                    height = self.draw_crash_banner(ui, size.x);
                });
            y += height + 8.0;
        }

        // A notice never paints while the crash details modal is
        // open: a toast over the dialog was the second half of the
        // owner's crash report screenshot.
        let notice = self.notice.clone().filter(|_| {
            !self.crash_details_open
                && !(on_connect && self.live_start_error.as_deref() == self.notice_str())
        });
        if let Some(notice) = notice {
            // The banner fades/slides in and out like every popup
            // (popup_phase). The notice's kind picks its voice: a
            // Busy notice carries the shared spinner in front of
            // its text so work in flight reads as busy, and an
            // Error notice speaks in the danger token like every
            // other failure in the app.
            let phase = crate::anim::popup_phase(root.ctx(), "notice_banner", true, self.motion);
            if let Some(banner_p) = phase {
                // Same window-sized width as the crash banner
                // above: cap 560.0, window width minus margins
                // below that.
                let size = Vec2::new((area.width() - 24.0).min(560.0), 40.0);
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
                        let _banner = egui::Frame::new()
                            .fill(crate::theme::glass_text_tint(t, t.bg_panel))
                            .corner_radius(CornerRadius::same(12))
                            .stroke(Stroke::new(1.0_f32, rgb(t.border)))
                            .inner_margin(Margin::symmetric(12, 8))
                            .show(ui, |ui| {
                                ui.set_width(size.x - 24.0);
                                ui.horizontal(|ui| {
                                    if notice.kind == NoticeKind::Busy {
                                        crate::widgets::spinner(ui, 14.0);
                                        ui.add_space(2.0);
                                    }
                                    let mut label = RichText::new(&notice.text).size(12.0);
                                    if notice.kind == NoticeKind::Error {
                                        label = label.color(rgb(t.danger));
                                    }
                                    ui.label(label);
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

    /// The crash banner's content, shared by the overlay on the
    /// main screens and the in-flow slot in the connect column:
    /// a title, ONE human sentence, and the three actions. The
    /// raw panic text is never the banner's body; it stays inside
    /// the report, behind View details, Report, and Copy.
    /// Returns the height the banner painted at.
    pub(crate) fn draw_crash_banner(&mut self, ui: &mut egui::Ui, width: f32) -> f32 {
        let t = self.tokens();
        let mut action: Option<&str> = None;
        // Text-bearing glass with the danger stroke: the banner
        // carries text that must stay crisp on any surface.
        let banner = egui::Frame::new()
            .fill(crate::theme::glass_text_tint(t, t.bg_panel))
            .corner_radius(CornerRadius::same(12))
            .stroke(Stroke::new(1.0_f32, rgb(t.danger)))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(width - 24.0);
                ui.label(RichText::new(crate::i18n::t("crash.title")).strong());
                ui.label(
                    RichText::new(crate::i18n::t("crash.body"))
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
        match action {
            Some("report") => {
                if let Some(report) = self.crash_report.clone() {
                    let url = core::crash::crash_issue_url(&report);
                    open_or_copy(self, ui, &url);
                }
                self.dismiss_crash();
            }
            Some("details") => {
                self.crash_copied = false;
                self.crash_details_open = true;
            }
            Some("dismiss") => self.dismiss_crash(),
            _ => {}
        }
        banner.response.rect.height()
    }

    fn dismiss_crash(&mut self) {
        if let Some(report) = &self.crash_report {
            let dir = backend::data_dir().join("crashes");
            let _ = core::crash::mark_seen(&dir, report.timestamp_unix);
        }
        self.crash_report = None;
        self.crash_details_open = false;
        self.crash_copied = false;
    }

    /// The crash details modal: compact and human. A title, one
    /// sentence naming the version that crashed, and the actions.
    /// The full report text (error, location, backtrace) is never
    /// the dialog body; it leaves only through Copy (clipboard)
    /// or Report on GitHub (the prefilled issue), by the user's
    /// choice. Copy confirms inside the dialog ("Copied"), never
    /// as a toast painted over it.
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
        // The modal sizes to the window like the banners do:
        // the fixed width on wide windows, and never wider than
        // the window minus the same 12.0 side margins, so a
        // portrait window never clips it.
        let size = Vec2::new((area.width() - 24.0).min(460.0), 208.0);
        let rect = egui::Rect::from_center_size(area.center(), size);
        let mut close = false;
        let mut copy = false;
        let mut report_it = false;
        egui::Area::new(egui::Id::unique("crash_details"))
            .fixed_pos(rect.min)
            .order(egui::Order::Foreground)
            .show(root.ctx(), |ui| {
                if det_p < 0.999 {
                    ui.set_opacity(det_p);
                }
                let _modal = egui::Frame::new()
                    .fill(crate::theme::glass_text_tint(t, t.bg_panel))
                    .corner_radius(CornerRadius::same(crate::theme::RADIUS_SMALL))
                    .stroke(Stroke::new(1.0_f32, rgb(t.border)))
                    .inner_margin(Margin::same(14))
                    .show(ui, |ui| {
                        ui.set_width(size.x - 28.0);
                        ui.label(RichText::new("Crash report").size(15.0).strong());
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(format!(
                                "Zaprust {} ran into a problem and closed. \
                                 The report holds the error and the technical \
                                 trace; nothing from your chats is in it.",
                                report.app_version
                            ))
                            .size(12.0)
                            .color(rgb(t.text_secondary)),
                        );
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if crate::widgets::button(
                                ui,
                                t,
                                &crate::i18n::t("crash.report"),
                                crate::widgets::Btn::Primary,
                            )
                            .clicked()
                            {
                                report_it = true;
                            }
                            let copy_label = if self.crash_copied { "Copied" } else { "Copy" };
                            if crate::widgets::button(
                                ui,
                                t,
                                copy_label,
                                crate::widgets::Btn::Secondary,
                            )
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
            });
        crate::anim::popup_transform(
            root.ctx(),
            egui::Id::unique("crash_details"),
            area.center(),
            det_p,
        );
        if copy && !exiting {
            root.ctx().copy_text(report.full_text());
            self.crash_copied = true;
        }
        if report_it && !exiting {
            let url = core::crash::crash_issue_url(&report);
            open_or_copy(self, root, &url);
            self.dismiss_crash();
        }
        if close {
            self.crash_details_open = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Args;

    const URL: &str = "https://example.invalid/zaprust-test-link";

    fn demo_app(ctx: &egui::Context) -> App {
        let mut app = App::new(Args::parse(), ctx);
        // App::new may have attempted a real live start against
        // this machine's data dir (and left its failure notice
        // behind when that start refused); the test owns the
        // state from here and wants a clean demo shell.
        app.live = None;
        app.live_mode = false;
        app.notice = None;
        app
    }

    fn no_browser(_: &str) -> std::io::Result<()> {
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no opener in the test rig"))
    }

    fn browser(_: &str) -> std::io::Result<()> {
        Ok(())
    }

    fn run(
        app: &mut App,
        ctx: &egui::Context,
        opener: fn(&str) -> std::io::Result<()>,
    ) -> egui::FullOutput {
        ctx.begin_pass(egui::RawInput::default());
        egui::Area::new(egui::Id::unique("open-or-copy-test")).show(ctx, |ui| {
            open_or_copy_with(app, ui, URL, opener);
        });
        let mut out = ctx.end_pass();
        // No renderer will ever apply this frame's texture
        // deltas; clear them so dropping the output is quiet.
        out.textures_delta.clear();
        out
    }

    fn copied_texts(out: &egui::FullOutput) -> Vec<&str> {
        out.platform_output
            .commands
            .iter()
            .filter_map(|c| match c {
                egui::OutputCommand::CopyText(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The About page's Report a bug action (and the crash
    /// banner's report link) reach the browser through
    /// open_or_copy: when the opener fails, the exact URL
    /// must land on the clipboard and the user must be told.
    #[test]
    fn a_failed_open_copies_the_url_and_says_so() {
        let ctx = egui::Context::default();
        let mut app = demo_app(&ctx);
        let out = run(&mut app, &ctx, no_browser);
        assert_eq!(copied_texts(&out), [URL], "the link lands on the clipboard instead");
        assert_eq!(
            app.notice_str(),
            Some("Could not open a browser, the link was copied to your clipboard.")
        );
    }

    /// When the opener succeeds, nothing is copied and no
    /// notice appears: the browser has the link.
    #[test]
    fn a_successful_open_leaves_clipboard_and_notice_alone() {
        let ctx = egui::Context::default();
        let mut app = demo_app(&ctx);
        let out = run(&mut app, &ctx, browser);
        assert!(copied_texts(&out).is_empty());
        assert_eq!(app.notice, None);
    }
}
