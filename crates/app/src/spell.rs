//! Composer spell checking (row 128): a pure-Rust Hunspell
//! engine (spellbook) over two bundled dictionaries, English
//! (en_US) and Indonesian (id_ID), whose sources and licenses
//! live beside them in `assets/dicts/`. A word passes when
//! either dictionary accepts it: the app's users write both
//! languages, often in one message, and flagging the other
//! language's words would make the feature noise.
//!
//! The checker never flags: tokens touching digits, URLs,
//! emails or handles (a token adjacent to `.`, `@`, `/` or
//! `:`), all-caps acronyms, the user's personal words, names
//! the session knows (contacts and chat titles), and the word
//! currently being typed (the caret's word is unfinished by
//! definition).

use std::collections::HashSet;
use std::ops::Range;

const EN_AFF: &str = include_str!("../assets/dicts/en_US.aff");
const EN_DIC: &str = include_str!("../assets/dicts/en_US.dic");
const ID_AFF: &str = include_str!("../assets/dicts/id_ID.aff");
const ID_DIC: &str = include_str!("../assets/dicts/id_ID.dic");

/// The spell checker: both dictionaries plus the user's own
/// word sets. Built once per app run; the dictionaries are
/// large but parse in well under a frame's budget at startup.
pub(crate) struct SpellChecker {
    en: spellbook::Dictionary,
    id: spellbook::Dictionary,
    personal: HashSet<String>,
    known_names: HashSet<String>,
}

impl SpellChecker {
    /// Builds the checker from the bundled dictionaries and
    /// the user's personal words. A dictionary that fails to
    /// parse is a build defect, so this returns `None` and
    /// the composer simply shows no underlines (the settings
    /// toggle stays honest: no engine, no marks, and the
    /// failure is logged).
    pub(crate) fn new(personal_words: &[String]) -> Option<Self> {
        let en = match spellbook::Dictionary::new(EN_AFF, EN_DIC) {
            Ok(dict) => dict,
            Err(error) => {
                tracing::warn!("en_US dictionary failed to parse: {error}");
                return None;
            }
        };
        let id = match spellbook::Dictionary::new(ID_AFF, ID_DIC) {
            Ok(dict) => dict,
            Err(error) => {
                tracing::warn!("id_ID dictionary failed to parse: {error}");
                return None;
            }
        };
        Some(Self {
            en,
            id,
            personal: personal_words.iter().map(|w| w.to_lowercase()).collect(),
            known_names: HashSet::new(),
        })
    }

    /// Replaces the personal word set (the Settings editor
    /// saves through here).
    pub(crate) fn set_personal(&mut self, words: &[String]) {
        self.personal = words.iter().map(|w| w.to_lowercase()).collect();
    }

    /// Adds session-known names (contact names, chat titles,
    /// group member names) as known words: a name the user
    /// talks to is never a misspelling.
    pub(crate) fn add_names<I: IntoIterator<Item = String>>(&mut self, names: I) {
        for name in names {
            for token in tokenize(&name) {
                let word = &name[token];
                if word.chars().count() > 1 {
                    self.known_names.insert(word.to_lowercase());
                }
            }
        }
    }

    /// Whether one word is spelled correctly: either
    /// dictionary, the personal set, or the known names.
    /// Capitalization is tried as written, lowercased, and
    /// sentence-cased, because Hunspell's case handling for
    /// proper nouns differs between the two dictionaries.
    pub(crate) fn accepts(&self, word: &str) -> bool {
        if word.chars().count() < 2 {
            return true;
        }
        let lower = word.to_lowercase();
        if self.personal.contains(&lower) || self.known_names.contains(&lower) {
            return true;
        }
        if self.en.check(word) || self.id.check(word) {
            return true;
        }
        if self.en.check(&lower) || self.id.check(&lower) {
            return true;
        }
        let mut chars = lower.chars();
        if let Some(first) = chars.next() {
            let titled: String = first.to_uppercase().chain(chars).collect();
            if self.en.check(&titled) || self.id.check(&titled) {
                return true;
            }
        }
        false
    }

    /// The byte ranges of `text` that are misspelled. The
    /// token containing `caret` (a byte offset, when the
    /// caller knows it) is never flagged: it is the word
    /// being typed.
    pub(crate) fn misspellings(&self, text: &str, caret: Option<usize>) -> Vec<Range<usize>> {
        let bytes = text.as_bytes();
        let mut out = Vec::new();
        for range in tokenize(text) {
            let word = &text[range.clone()];
            if let Some(caret) = caret {
                if range.contains(&caret) || caret == range.end {
                    continue;
                }
            }
            // A token fused to a digit (abc123, H2O split as
            // H and O) is a code or a formula, not prose.
            let before = range.start.checked_sub(1).map(|i| bytes[i]);
            let after = bytes.get(range.end).copied();
            if before.is_some_and(|b| b.is_ascii_digit())
                || after.is_some_and(|b| b.is_ascii_digit())
            {
                continue;
            }
            // URLs, emails, handles and paths: a token
            // adjacent to `.`, `@`, `/` or `:` belongs to an
            // address, not a sentence.
            if before.is_some_and(|b| matches!(b, b'.' | b'@' | b'/' | b':'))
                || after.is_some_and(|b| matches!(b, b'.' | b'@' | b'/' | b':'))
            {
                continue;
            }
            // All-caps runs are acronyms and initialisms.
            if word.chars().count() > 1 && word.chars().all(|c| !c.is_lowercase()) {
                continue;
            }
            if !self.accepts(word) {
                out.push(range);
            }
        }
        out
    }
}

/// Word tokens in `text`: maximal runs of letters with
/// internal apostrophes kept (don't, l'amour), returned as
/// byte ranges. Anything else separates.
fn tokenize(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (index, ch) in text.char_indices() {
        let part = ch.is_alphabetic() || ch == '\'' || ch == '\u{2019}';
        match (part, start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                out.push(begin..index);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        out.push(begin..text.len());
    }
    // A trailing apostrophe is punctuation, not part of the
    // word (users' -> users): trim it from every token.
    for range in &mut out {
        while range.end > range.start {
            let tail = &text[range.start..range.end];
            let Some(last) = tail.chars().next_back() else { break };
            if last == '\'' || last == '\u{2019}' {
                range.end -= last.len_utf8();
            } else {
                break;
            }
        }
    }
    out.retain(|range| !range.is_empty());
    out
}

impl crate::state::App {
    /// Paints the spell-check underlines for the composer
    /// draft (row 128): the checker is built lazily on first
    /// use and seeded with the names this session knows (chat
    /// titles and the live name directory, refreshed when the
    /// open chat changes). Underlines come from the edit's
    /// own galley, so they sit exactly under the painted
    /// words through wrapping and scrolling.
    pub(crate) fn paint_spell_underlines(
        &mut self,
        ui: &mut egui::Ui,
        output: &egui::text_edit::TextEditOutput,
        t: core::theme::Tokens,
    ) {
        if self.spell_checker.is_none() {
            self.spell_checker = SpellChecker::new(&self.spell_personal);
        }
        if self.spell_names_chat != Some(self.selected_chat) {
            self.spell_names_chat = Some(self.selected_chat);
            let mut names: Vec<String> = self.chats.iter().map(|c| c.title.clone()).collect();
            if let Some(session) = &self.live {
                names.extend(session.names.values().cloned());
            }
            if let Some(checker) = &mut self.spell_checker {
                checker.add_names(names);
            }
        }
        let Some(checker) = &self.spell_checker else {
            return;
        };
        let draft = self.composer.draft.clone();
        let caret = output.cursor_range.and_then(|range| {
            let char_index: usize = range.primary.index.into();
            draft.char_indices().nth(char_index).map(|(byte, _)| byte).or(Some(draft.len()))
        });
        let ranges = checker.misspellings(&draft, caret);
        if ranges.is_empty() {
            return;
        }
        let color = crate::theme::rgb(t.danger).gamma_multiply(0.85);
        let painter =
            ui.painter().clone().with_clip_rect(output.text_clip_rect.intersect(ui.clip_rect()));
        let origin = output.galley_pos;
        for range in ranges {
            let start_char = draft[..range.start].chars().count();
            let end_char = start_char + draft[range].chars().count();
            for (x0, x1, y) in underline_segments(&output.galley, start_char, end_char, origin) {
                let mut points = Vec::new();
                let mut x = x0;
                let mut up = false;
                while x < x1 {
                    points.push(egui::pos2(x, if up { y - 1.3 } else { y }));
                    x += 2.0;
                    up = !up;
                }
                points.push(egui::pos2(x1, y));
                painter.add(egui::Shape::line(points, egui::Stroke::new(1.0, color)));
            }
        }
    }
}

/// The underline segments for one character span in a
/// galley: one (x0, x1, y) per row the span touches, in
/// screen coordinates. Glyphs are walked in order, which is
/// the text's character order for the left-to-right
/// languages the bundled dictionaries cover.
fn underline_segments(
    galley: &egui::Galley,
    start_char: usize,
    end_char: usize,
    origin: egui::Pos2,
) -> Vec<(f32, f32, f32)> {
    let mut out = Vec::new();
    let mut char_index = 0usize;
    for row in &galley.rows {
        let mut span: Option<(f32, f32)> = None;
        for glyph in &row.glyphs {
            let index = char_index;
            char_index += 1;
            if glyph.chr == '\n' {
                continue;
            }
            if index >= start_char && index < end_char {
                let x0 = glyph.pos.x;
                let x1 = glyph.pos.x + glyph.size().x;
                span = Some(match span {
                    None => (x0, x1),
                    Some((lo, hi)) => (lo.min(x0), hi.max(x1)),
                });
            }
        }
        if let Some((x0, x1)) = span {
            out.push((origin.x + x0, origin.x + x1, origin.y + row.rect().bottom() - 1.0));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker() -> SpellChecker {
        SpellChecker::new(&[]).expect("the bundled dictionaries parse")
    }

    #[test]
    fn english_words_pass_and_typos_flag() {
        let checker = checker();
        assert!(checker.accepts("hello"));
        assert!(checker.accepts("message"));
        assert!(checker.accepts("don't"));
        assert!(!checker.accepts("helo"));
        assert!(!checker.accepts("mesage"));
    }

    #[test]
    fn indonesian_affixed_words_pass() {
        // The affix rules are the whole point of bundling the
        // real dictionary: prefixes and suffixes must resolve.
        let checker = checker();
        for word in [
            "menggunakan",
            "diketahui",
            "pembelajaran",
            "memperbaiki",
            "kebersamaan",
            "terima",
            "kasih",
            "besok",
            "rapat",
        ] {
            assert!(checker.accepts(word), "Indonesian word rejected: {word}");
        }
        assert!(!checker.accepts("menggunakann"));
        assert!(!checker.accepts("pembelajran"));
    }

    #[test]
    fn both_languages_pass_in_one_text() {
        let checker = checker();
        let text = "Besok pagi kita meeting di kantor, jangan lupa membawa laptop";
        assert!(
            checker.misspellings(text, None).is_empty(),
            "mixed text flagged: {:?}",
            checker.misspellings(text, None)
        );
    }

    #[test]
    fn addresses_and_codes_are_never_flagged() {
        let checker = checker();
        for text in [
            "lihat https://example.com/path ya",
            "kirim ke rifan@example.id besok",
            "kode OTP1234 dan H2O",
            "tanya @fanyzp nanti",
        ] {
            assert!(
                checker.misspellings(text, None).is_empty(),
                "address-like text flagged: {text}"
            );
        }
    }

    #[test]
    fn the_caret_word_is_never_flagged() {
        let checker = checker();
        let text = "saya sedang mengetik mesag";
        let caret = text.len();
        assert!(checker.misspellings(text, Some(caret)).is_empty());
        assert_eq!(checker.misspellings(text, None).len(), 1);
    }

    #[test]
    fn personal_words_and_names_pass() {
        let mut checker = SpellChecker::new(&["zaprust".to_owned()]).unwrap();
        checker.add_names(["Rifan Saputra".to_owned()]);
        assert!(checker.accepts("zaprust"));
        assert!(checker.accepts("Rifan"));
        assert!(checker.accepts("Saputra"));
    }

    #[test]
    fn acronyms_are_never_flagged() {
        let checker = checker();
        assert!(checker.misspellings("API dan USB serta HDMI", None).is_empty());
    }
}
