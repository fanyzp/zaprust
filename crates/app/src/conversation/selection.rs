//! Cross-message text selection (row 129). Bubbles are
//! custom-painted, so egui's built-in label selection cannot
//! see their text: the paint pass registers every painted
//! body galley (message id, galley, screen origin, and the
//! exact string it was laid out from) into a per-frame
//! registry, and the stream hit-tests drags against it.
//!
//! Bounds, by construction: the registry holds the messages
//! the stream painted this frame (the window plus overscan),
//! so a selection spans painted messages; the copied text is
//! the painted text (mentions as displayed), and for
//! right-to-left bodies the character order is the visual
//! order the galley carries.

use std::collections::HashMap;
use std::sync::Arc;

/// One painted body: message id, its galley, the galley's
/// screen origin, and the string the galley was laid out
/// from (character indices count into this string).
pub(crate) type GalleyRegistry = Vec<(u64, Arc<egui::Galley>, egui::Pos2, String)>;

/// Per-message highlight ranges (character spans) for the
/// current selection, computed by the stream from the
/// previous frame's registry and read by the paint pass.
pub(crate) type HighlightMap = HashMap<u64, Vec<(usize, usize)>>;

pub(crate) fn registry_id() -> egui::Id {
    egui::Id::unique("selection_registry")
}

pub(crate) fn highlights_id() -> egui::Id {
    egui::Id::unique("selection_highlights")
}

/// Called by the bubble paint pass for every painted body.
pub(crate) fn register(
    ui: &egui::Ui,
    message_id: u64,
    galley: &Arc<egui::Galley>,
    origin: egui::Pos2,
    text: &str,
) {
    ui.ctx().data_mut(|data| {
        let registry = data.get_temp_mut_or_default::<GalleyRegistry>(registry_id());
        registry.push((message_id, galley.clone(), origin, text.to_owned()));
    });
}

/// The highlight ranges one message paints this frame.
pub(crate) fn highlights_for(ui: &egui::Ui, message_id: u64) -> Vec<(usize, usize)> {
    ui.ctx()
        .data(|data| data.get_temp::<HighlightMap>(highlights_id()))
        .and_then(|map| map.get(&message_id).cloned())
        .unwrap_or_default()
}

/// Paints the selection highlight behind one galley's text:
/// one filled rect per row segment the span touches.
pub(crate) fn paint_highlight(
    painter: &egui::Painter,
    galley: &egui::Galley,
    origin: egui::Pos2,
    ranges: &[(usize, usize)],
    color: egui::Color32,
) {
    for &(start, end) in ranges {
        let mut char_index = 0usize;
        for row in &galley.rows {
            let mut span: Option<(f32, f32)> = None;
            for glyph in &row.glyphs {
                let index = char_index;
                char_index += 1;
                if glyph.chr == '\n' {
                    continue;
                }
                if index >= start && index < end {
                    let x0 = glyph.pos.x;
                    let x1 = glyph.pos.x + glyph.size().x;
                    span = Some(match span {
                        None => (x0, x1),
                        Some((lo, hi)) => (lo.min(x0), hi.max(x1)),
                    });
                }
            }
            if let Some((x0, x1)) = span {
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(origin.x + x0, origin.y + row.rect().top()),
                        egui::pos2(origin.x + x1, origin.y + row.rect().bottom()),
                    ),
                    egui::CornerRadius::same(2),
                    color,
                );
            }
        }
    }
}

/// The (registry index, character index) a selection
/// endpoint names, when its message is in the registry.
fn locate(registry: &GalleyRegistry, endpoint: (u64, usize)) -> Option<(usize, usize)> {
    registry.iter().position(|(id, ..)| *id == endpoint.0).map(|index| (index, endpoint.1))
}

/// Normalizes a selection into per-message character
/// ranges, in registry order. `None` when either endpoint's
/// message is not in the registry or the selection is empty.
pub(crate) fn normalized_ranges(
    registry: &GalleyRegistry,
    selection: crate::state::TextSelection,
) -> Option<Vec<(u64, usize, usize)>> {
    let anchor = locate(registry, selection.anchor)?;
    let focus = locate(registry, selection.focus)?;
    let (start, end) = if anchor <= focus { (anchor, focus) } else { (focus, anchor) };
    if start == end {
        return None;
    }
    let mut out = Vec::new();
    for (offset, (id, _, _, text)) in registry[start.0..=end.0].iter().enumerate() {
        let index = start.0 + offset;
        let len = text.chars().count();
        let from = if index == start.0 { start.1.min(len) } else { 0 };
        let to = if index == end.0 { end.1.min(len) } else { len };
        if from < to {
            out.push((*id, from, to));
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// The highlight map the paint pass reads, from a selection
/// and the registry it was made against.
pub(crate) fn highlight_map(
    registry: &GalleyRegistry,
    selection: Option<crate::state::TextSelection>,
) -> HighlightMap {
    let mut map = HighlightMap::new();
    if let Some(selection) = selection {
        if let Some(ranges) = normalized_ranges(registry, selection) {
            for (id, from, to) in ranges {
                map.entry(id).or_default().push((from, to));
            }
        }
    }
    map
}

/// The text a selection copies: the painted bodies' spans,
/// messages joined by a newline.
pub(crate) fn extract(
    registry: &GalleyRegistry,
    selection: crate::state::TextSelection,
) -> Option<String> {
    let ranges = normalized_ranges(registry, selection)?;
    let mut parts = Vec::new();
    for (id, from, to) in ranges {
        let (_, _, _, text) = registry.iter().find(|(rid, ..)| *rid == id)?;
        let slice: String = text.chars().skip(from).take(to - from).collect();
        parts.push(slice);
    }
    Some(parts.join("\n"))
}

/// The painted text one registered message carries, so a
/// gesture can resolve granularity against the exact string
/// its galley was laid out from.
pub(crate) fn message_text(registry: &GalleyRegistry, message_id: u64) -> Option<&str> {
    registry.iter().find(|(id, ..)| *id == message_id).map(|(_, _, _, text)| text.as_str())
}

/// The character class a word scan treats as one run: word
/// characters (letters, digits, marks and connector
/// punctuation in the Unicode sense, via `is_alphanumeric`
/// plus underscore), whitespace, and everything else
/// (punctuation and symbols).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Space,
    Other,
}

fn char_class(c: char) -> CharClass {
    if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else if c.is_whitespace() {
        CharClass::Space
    } else {
        CharClass::Other
    }
}

/// The word boundaries around a character index: the maximal
/// run of one character class containing the character, as a
/// (start, end) character span. A double-click on a word
/// selects the word; on punctuation or whitespace it selects
/// that run, so the gesture always lands on something.
pub(crate) fn word_bounds(text: &str, char_index: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return (0, 0);
    }
    let at = char_index.min(chars.len() - 1);
    let class = char_class(chars[at]);
    let mut start = at;
    while start > 0 && char_class(chars[start - 1]) == class {
        start -= 1;
    }
    let mut end = at + 1;
    while end < chars.len() && char_class(chars[end]) == class {
        end += 1;
    }
    (start, end)
}

/// Hit-tests a screen position against the registry:
/// the message and character index under the pointer.
pub(crate) fn hit_test(registry: &GalleyRegistry, pos: egui::Pos2) -> Option<(u64, usize)> {
    for (id, galley, origin, _) in registry {
        let local = pos - *origin;
        if local.x >= 0.0
            && local.y >= 0.0
            && local.x <= galley.size().x
            && local.y <= galley.size().y
        {
            let cursor = galley.cursor_from_pos(local);
            return Some((*id, cursor.index.into()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TextSelection;

    fn registry() -> GalleyRegistry {
        // Galleys are irrelevant to normalization and
        // extraction; one real (empty) galley stands in.
        let mut fonts = epaint::text::Fonts::new(
            epaint::text::TextOptions::default(),
            epaint::text::FontDefinitions::default(),
        );
        let galley =
            Arc::new(fonts.layout_uncached(1.0, Arc::new(egui::text::LayoutJob::default())));
        vec![
            (1, galley.clone(), egui::Pos2::ZERO, "hello world".to_owned()),
            (2, galley.clone(), egui::Pos2::ZERO, "second message".to_owned()),
            (3, galley, egui::Pos2::ZERO, "third".to_owned()),
        ]
    }

    #[test]
    fn extraction_spans_messages_in_order() {
        let reg = registry();
        let sel = TextSelection { anchor: (1, 6), focus: (2, 6) };
        assert_eq!(extract(&reg, sel).as_deref(), Some("world\nsecond"));
        // Direction does not matter.
        let sel = TextSelection { anchor: (2, 6), focus: (1, 6) };
        assert_eq!(extract(&reg, sel).as_deref(), Some("world\nsecond"));
    }

    #[test]
    fn extraction_within_one_message() {
        let reg = registry();
        let sel = TextSelection { anchor: (3, 1), focus: (3, 4) };
        assert_eq!(extract(&reg, sel).as_deref(), Some("hir"));
    }

    #[test]
    fn word_bounds_take_the_whole_word() {
        // "hello world": any index inside a word spans it.
        assert_eq!(word_bounds("hello world", 0), (0, 5));
        assert_eq!(word_bounds("hello world", 4), (0, 5));
        assert_eq!(word_bounds("hello world", 6), (6, 11));
        assert_eq!(word_bounds("hello world", 10), (6, 11));
        // An index past the end clamps into the last word.
        assert_eq!(word_bounds("hello world", 99), (6, 11));
        assert_eq!(word_bounds("", 0), (0, 0));
    }

    #[test]
    fn word_bounds_stop_at_punctuation_and_whitespace() {
        // The comma and the spaces are their own runs; the
        // words on either side stop at them.
        assert_eq!(word_bounds("hello, world", 4), (0, 5));
        assert_eq!(word_bounds("hello, world", 5), (5, 6));
        assert_eq!(word_bounds("hello, world", 6), (6, 7));
        assert_eq!(word_bounds("hello, world", 7), (7, 12));
        // A punctuation run selects as one span.
        assert_eq!(word_bounds("wait... what", 5), (4, 7));
        // Underscore joins a word; a digit run is a word too.
        assert_eq!(word_bounds("order_id 42", 3), (0, 8));
        assert_eq!(word_bounds("order_id 42", 10), (9, 11));
    }

    #[test]
    fn word_bounds_follow_unicode_letters() {
        // Accented and non-Latin letters are word characters:
        // the scan counts characters, not bytes.
        assert_eq!(word_bounds("café au lait", 2), (0, 4));
        assert_eq!(word_bounds("halo dunia", 6), (5, 10));
        assert_eq!(word_bounds("日本語 テスト", 1), (0, 3));
        assert_eq!(word_bounds("日本語 テスト", 5), (4, 7));
    }

    #[test]
    fn word_selection_extracts_from_the_hit_message_only() {
        // Multi-message bodies: the word scan runs against
        // the hit message's own text, and the resulting
        // selection extracts just that word.
        let reg = registry();
        let text = message_text(&reg, 2).expect("second message registered");
        let (start, end) = word_bounds(text, 8);
        let sel = TextSelection { anchor: (2, start), focus: (2, end) };
        assert_eq!(extract(&reg, sel).as_deref(), Some("message"));
        assert_eq!(message_text(&reg, 99), None);
    }

    #[test]
    fn empty_and_missing_selections_extract_nothing() {
        let reg = registry();
        let sel = TextSelection { anchor: (1, 3), focus: (1, 3) };
        assert_eq!(extract(&reg, sel), None);
        let sel = TextSelection { anchor: (9, 0), focus: (1, 3) };
        assert_eq!(extract(&reg, sel), None);
    }
}
