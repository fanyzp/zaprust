//! Feedback and text helpers: spinners, loading rows, empty states,
//! text truncation and the panel edge seam.

use crate::theme::{rgb, SP_6};
use core::theme::{Rgb, Tokens};
use eframe::egui::{self, Color32, FontId, Pos2, RichText, Stroke};

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
    format!("{}…", &text[..floor_grapheme_boundary(text, bounds[lo])])
}

/// Lowers a char-boundary byte offset so the cut never splits
/// a grapheme cluster (P2-64): not a base letter from its
/// combining marks, not a flag's regional-indicator pair, not
/// a ZWJ emoji sequence. The width search above works on char
/// boundaries; this walks the chosen boundary back over any
/// continuation characters that belong to the previous
/// cluster, and over a ZWJ joiner itself. No segmentation
/// crate: the continuation classes are small and stable.
pub(crate) fn floor_grapheme_boundary(text: &str, mut end: usize) -> usize {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let pos = chars.iter().position(|(i, _)| *i == end).unwrap_or(chars.len());
    let mut cut = pos;
    while cut > 0 {
        let next = chars.get(cut).map(|(_, c)| *c);
        let prev = chars[cut - 1].1;
        let splits = match next {
            // Combining marks, variation selectors and skin
            // tones belong to the previous cluster.
            Some(c) if is_combining(c) => true,
            // A ZWJ joins the next cluster to the previous
            // one: a cut before it strands the joiner, a cut
            // after it splits the sequence.
            Some('\u{200d}') => true,
            _ => prev == '\u{200d}',
        };
        // A regional-indicator pair (a flag) splits when the
        // run of indicators before the cut is odd.
        let flag_split = next.is_some_and(is_regional_indicator)
            && chars[..cut].iter().rev().take_while(|(_, c)| is_regional_indicator(*c)).count() % 2
                == 1;
        if splits || flag_split {
            cut -= 1;
            end = chars[cut].0;
        } else {
            break;
        }
    }
    end
}

fn is_combining(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{036f}'
        | '\u{0483}'..='\u{0489}'
        | '\u{0591}'..='\u{05bd}'
        | '\u{0610}'..='\u{061a}'
        | '\u{064b}'..='\u{065f}'
        | '\u{0670}'..='\u{0670}'
        | '\u{06d6}'..='\u{06dc}'
        | '\u{0e31}'..='\u{0e3a}'
        | '\u{0e47}'..='\u{0e4e}'
        | '\u{1ab0}'..='\u{1aff}'
        | '\u{1dc0}'..='\u{1dff}'
        | '\u{20d0}'..='\u{20f0}'
        | '\u{fe00}'..='\u{fe0f}'
        | '\u{fe20}'..='\u{fe2f}'
        | '\u{1f3fb}'..='\u{1f3ff}')
}

fn is_regional_indicator(c: char) -> bool {
    matches!(c, '\u{1f1e6}'..='\u{1f1ff}')
}

/// The one empty state for list panels: a quiet secondary label
/// set off from the panel header. Panels differ only in the text.
pub(crate) fn empty_state(ui: &mut egui::Ui, t: Tokens, text: &str) {
    ui.add_space(SP_6);
    ui.label(RichText::new(text).size(15.0).color(rgb(t.text_secondary)));
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

/// A softened, inset horizontal separator for the rare split
/// that survives inside one surface (a menu's action groups, a
/// list against its dismiss action): the border token at the
/// soft_right_edge alpha, inset by the shared list inset on
/// both sides, with breathing space above and below. Flat
/// same-surface content is separated by spacing alone (design
/// standard, hairline discipline); this helper exists so the
/// surviving splits all take this one quiet form instead of
/// egui's full-contrast, full-width separator.
pub(crate) fn soft_separator(ui: &mut egui::Ui, t: Tokens) {
    let c = rgb(t.border);
    let inset = crate::theme::LIST_PANEL_INSET_X as f32;
    ui.add_space(6.0);
    let rect = ui.available_rect_before_wrap();
    ui.painter().line_segment(
        [Pos2::new(rect.left() + inset, rect.top()), Pos2::new(rect.right() - inset, rect.top())],
        Stroke::new(1.0_f32, egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 100)),
    );
    ui.add_space(6.0);
}
