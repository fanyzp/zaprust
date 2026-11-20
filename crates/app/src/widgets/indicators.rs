//! Feedback and text helpers: spinners, loading rows, empty states,
//! text truncation and the panel edge seam.

use crate::theme::rgb;
use core::theme::{Rgb, Tokens};
use eframe::egui::{self, Color32, FontId, RichText, Stroke};

/// Shortens `text` to fit `max_w` at `font`, cutting on a char
/// boundary and appending an ellipsis. The fit is measured with
/// the real font metrics, never by character count: callers hand
/// this the width actually left after their fixed elements
/// (timestamps, badges) have been measured, so text uses all of
/// the space it has and never runs underneath a fixed element.
pub(crate) fn ellipsize(ui: &egui::Ui, text: &str, font: FontId, max_w: f32) -> String {
    let width = |s: &str| {
        ui.ctx()
            .fonts_mut(|f| f.layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE).size().x)
    };
    if max_w <= 0.0 || width(text) <= max_w {
        return if max_w <= 0.0 { String::new() } else { text.to_owned() };
    }
    let ellipsis_w = width("…");
    if max_w < ellipsis_w {
        return String::new();
    }
    // Char-boundary byte offsets, binary-searched for the
    // longest prefix that still leaves room for the ellipsis.
    let bounds: Vec<usize> = text.char_indices().map(|(i, _)| i).chain([text.len()]).collect();
    let (mut lo, mut hi) = (0usize, bounds.len() - 1);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if width(&text[..bounds[mid]]) + ellipsis_w <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    format!("{}…", &text[..bounds[lo]])
}

/// The one empty state for list panels: a quiet secondary label
/// set off from the panel header. Panels differ only in the text.
pub(crate) fn empty_state(ui: &mut egui::Ui, t: Tokens, text: &str) {
    ui.add_space(24.0);
    ui.label(RichText::new(text).size(13.5).color(rgb(t.text_secondary)));
}

/// The one spinner, sized by the caller. Use [`loading_row`]
/// when a label travels with it.
pub(crate) fn spinner(ui: &mut egui::Ui, size: f32) -> egui::Response {
    ui.add(egui::Spinner::new().size(size))
}

/// A loading row: the shared 14 pt spinner beside a label, so
/// waiting reads as waiting everywhere it appears. The label's
/// size and colour stay the caller's, matching the surface the
/// row sits on.
pub(crate) fn loading_row(ui: &mut egui::Ui, label: &str, size: f32, color: Rgb) {
    ui.horizontal(|ui| {
        spinner(ui, 14.0);
        ui.label(RichText::new(label).size(size).color(rgb(color)));
    });
}

/// A single soft hairline on a panel's right edge, the smooth seam
/// between regions. (A full frame outline around every panel stacks
/// into a hard black bar at the seams, especially at 125–150% UI
/// scaling; one translucent line reads as a shadow gap instead.)
pub(crate) fn soft_right_edge(ui: &egui::Ui, rect: egui::Rect, t: Tokens) {
    let c = rgb(t.border);
    ui.painter().line_segment(
        [
            egui::Pos2::new(rect.right() - 0.5, rect.top()),
            egui::Pos2::new(rect.right() - 0.5, rect.bottom()),
        ],
        Stroke::new(1.0_f32, egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 100)),
    );
}
