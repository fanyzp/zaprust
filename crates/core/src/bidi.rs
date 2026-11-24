//! Paint-side bidirectional text support.
//!
//! Stored and searched text is always logical-order Unicode,
//! exactly as it arrived. This module produces the DISPLAY
//! string for paint surfaces, in two steps per paragraph:
//!
//! 1. Arabic letters are reshaped into their presentation
//!    forms (contextual joining, plus the lam-alef ligatures)
//!    in logical order. egui has no text shaper, so without
//!    this step Arabic letters could only ever paint in their
//!    isolated forms. The joining data below is generated
//!    from the Unicode Character Database 16.0
//!    (ArabicShaping.txt joining types and the presentation
//!    form decompositions in UnicodeData.txt).
//! 2. The paragraph is reordered into visual order with the
//!    Unicode Bidirectional Algorithm (the `unicode-bidi`
//!    crate). egui 0.37's text layout contains no bidi
//!    handling at all (measured: no direction support in
//!    epaint's text module), so it paints a string strictly
//!    left to right in storage order; feeding it the visual
//!    order string is what makes RTL text read correctly.
//!
//! Bounds, stated plainly: the reorder is per paragraph.
//! When a paragraph wraps, the painter chooses wrap points
//! on the visual string; for word-wrapped text the same
//! words group per line, but strict UAX #9 line-level
//! reordering inside egui's own wrapper remains egui's.
//! Combining marks pass through unshaped and are positioned
//! by advance width only (egui has no mark positioning).
//! The text editor (the composer) does not use this module:
//! egui's TextEdit owns its layout and cursor model, and a
//! display transform would break cursor mapping, so text
//! being edited still lays out in logical order.

/// True when the text contains a character from the Hebrew
/// or Arabic scripts (the ranges this module handles).
pub fn contains_rtl(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c as u32,
            0x0590..=0x05FF
            | 0x0600..=0x06FF
            | 0x0750..=0x077F
            | 0x08A0..=0x08FF
            | 0xFB1D..=0xFB4F
            | 0xFB50..=0xFDFF
            | 0xFE70..=0xFEFF)
    })
}

/// The display form of a text for paint surfaces: Arabic
/// reshaped, every paragraph reordered into visual order.
/// Text without RTL characters returns unchanged, byte for
/// byte, so Latin surfaces are untouched.
pub fn display(text: &str) -> String {
    if !contains_rtl(text) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    for (i, para) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if para.is_empty() {
            continue;
        }
        let shaped = reshape_arabic(para);
        out.push_str(&visual_order(&shaped));
    }
    out
}

/// One paragraph into visual order via UAX #9, with the
/// base direction taken from the paragraph's first strong
/// character (rule P2/P3).
fn visual_order(text: &str) -> String {
    let info = unicode_bidi::BidiInfo::new(text, None);
    let Some(para) = info.paragraphs.first() else {
        return text.to_owned();
    };
    info.reorder_line(para, para.range.clone()).into_owned()
}

/// Joining class of an Arabic-script character, from
/// ArabicShaping.txt: right-joining, dual-joining,
/// join-causing, transparent (combining marks), or
/// non-joining.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Join {
    R,
    D,
    C,
    T,
    U,
}

fn joining(cp: u32) -> Join {
    match cp {
        0x200D => return Join::C, // zero width joiner
        0x200C => return Join::U, // zero width non-joiner
        _ => {}
    }
    if let Ok(i) = LETTER_FORMS.binary_search_by_key(&cp, |e| e.0) {
        return LETTER_FORMS[i].1;
    }
    if let Ok(i) = FORMLESS_JOINING.binary_search_by_key(&cp, |e| e.0) {
        return FORMLESS_JOINING[i].1;
    }
    // Combining marks of the Arabic blocks are transparent:
    // they neither join nor break a join.
    if matches!(cp,
        0x0610..=0x061A
        | 0x064B..=0x065F
        | 0x0670
        | 0x06D6..=0x06DC
        | 0x06DF..=0x06E4
        | 0x06E7..=0x06E8
        | 0x06EA..=0x06ED
        | 0x08D3..=0x08E1
        | 0x08E3..=0x08FF)
    {
        return Join::T;
    }
    Join::U
}

/// The presentation forms of one letter, if it has any:
/// (isolated, final, initial, medial), zero where a form
/// does not exist.
fn letter_forms(cp: u32) -> Option<(u32, u32, u32, u32)> {
    LETTER_FORMS.binary_search_by_key(&cp, |e| e.0).ok().map(|i| {
        let e = &LETTER_FORMS[i];
        (e.2, e.3, e.4, e.5)
    })
}

/// Reshapes the Arabic letters of one paragraph (logical
/// order in, logical order out): every letter that has
/// presentation forms takes the form its neighbours demand,
/// and lam followed by an alef variant becomes the
/// ligature. Non-Arabic characters pass through untouched.
fn reshape_arabic(text: &str) -> String {
    if !text.chars().any(|c| (0x0600..=0x08FF).contains(&(c as u32))) {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    // The joining class of the previous / next non-transparent
    // character, or Non-joining at the paragraph edges.
    let prev_join = |from: usize| -> Join {
        let mut j = from;
        loop {
            if j == 0 {
                return Join::U;
            }
            j -= 1;
            let t = joining(chars[j] as u32);
            if t != Join::T {
                return t;
            }
        }
    };
    let next_join = |from: usize| -> Join {
        let mut j = from;
        while j < chars.len() {
            let t = joining(chars[j] as u32);
            if t != Join::T {
                return t;
            }
            j += 1;
        }
        Join::U
    };
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let cp = c as u32;
        let mine = joining(cp);
        // Lam-alef: lam directly followed by an alef variant
        // is one ligature glyph, final when the lam connects
        // backwards, isolated otherwise.
        if cp == 0x0644 && i + 1 < chars.len() {
            let alef = chars[i + 1] as u32;
            if let Some(lig) = LAM_ALEF.iter().find(|e| e.0 == alef) {
                let back = matches!(prev_join(i), Join::D | Join::C);
                let form = if back { lig.2 } else { lig.1 };
                push_cp(&mut out, form);
                i += 2;
                continue;
            }
        }
        let Some((isolated, fin, initial, medial)) = letter_forms(cp) else {
            out.push(c);
            i += 1;
            continue;
        };
        let back = matches!(prev_join(i), Join::D | Join::C) && matches!(mine, Join::D | Join::R);
        let fwd =
            matches!(mine, Join::D | Join::C) && matches!(next_join(i + 1), Join::D | Join::R);
        let form = match (back, fwd) {
            (true, true) if medial != 0 => medial,
            (true, _) => fin,
            (false, true) if initial != 0 => initial,
            _ => isolated,
        };
        push_cp(&mut out, if form == 0 { cp } else { form });
        i += 1;
    }
    out
}

fn push_cp(out: &mut String, cp: u32) {
    if let Some(c) = char::from_u32(cp) {
        out.push(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_text_is_byte_identical() {
        for s in [
            "Hello world",
            "Meeting at 5, room 12",
            "https://example.com/path?q=1",
            "multi\nline\ntext",
            "",
        ] {
            assert_eq!(display(s), s);
        }
        assert!(!contains_rtl("Hello world"));
    }

    #[test]
    fn pure_hebrew_reverses_into_visual_order() {
        // Logical: shin lamed vav final-mem. Visual order is
        // the exact reverse; Hebrew letters need no shaping.
        assert_eq!(display("שלום"), "םולש");
        assert!(contains_rtl("שלום"));
    }

    #[test]
    fn arabic_letters_take_contextual_forms() {
        // beh-alef-beh: initial beh, final alef, isolated
        // beh (alef does not join forwards), then the whole
        // word reverses for display.
        assert_eq!(display("باب"), "\u{FE8F}\u{FE8E}\u{FE91}");
    }

    #[test]
    fn lam_alef_becomes_the_ligature() {
        assert_eq!(display("لا"), "\u{FEFB}");
        // In السلام: isolated alef, initial lam, medial seen,
        // the lam-alef ligature in final form (the seen joins
        // into it), isolated meem; reversed for display.
        assert_eq!(display("السلام"), "\u{FEE1}\u{FEFC}\u{FEB4}\u{FEDF}\u{FE8D}");
    }

    #[test]
    fn mixed_paragraph_keeps_latin_runs_in_order() {
        // An LTR paragraph (first strong is Latin) keeps its
        // Latin order; the Arabic run inside is shaped and
        // reversed as one unit.
        assert_eq!(display("abc مرحبا def"), "abc \u{FE8E}\u{FE92}\u{FEA3}\u{FEAE}\u{FEE3} def");
    }

    #[test]
    fn digits_inside_an_rtl_paragraph_stay_readable() {
        // "The amount 1234": the digits form one LTR run at
        // the visual end of the RTL paragraph, digits in
        // their normal order.
        let out = display("المبلغ 1234");
        assert!(out.starts_with("1234 "), "digits lead visually: {out:?}");
        assert!(!out.contains('4') || out.matches('4').count() == 1);
    }

    #[test]
    fn reshape_leaves_hebrew_and_latin_alone() {
        assert_eq!(reshape_arabic("שלום abc"), "שלום abc");
    }
}
/// (letter, joining type, isolated, final, initial, medial);
/// a zero form means the letter has no such form.
static LETTER_FORMS: &[(u32, Join, u32, u32, u32, u32)] = &[
    (0x0621, Join::U, 0xFE80, 0x0000, 0x0000, 0x0000),
    (0x0622, Join::R, 0xFE81, 0xFE82, 0x0000, 0x0000),
    (0x0623, Join::R, 0xFE83, 0xFE84, 0x0000, 0x0000),
    (0x0624, Join::R, 0xFE85, 0xFE86, 0x0000, 0x0000),
    (0x0625, Join::R, 0xFE87, 0xFE88, 0x0000, 0x0000),
    (0x0626, Join::D, 0xFE89, 0xFE8A, 0xFE8B, 0xFE8C),
    (0x0627, Join::R, 0xFE8D, 0xFE8E, 0x0000, 0x0000),
    (0x0628, Join::D, 0xFE8F, 0xFE90, 0xFE91, 0xFE92),
    (0x0629, Join::R, 0xFE93, 0xFE94, 0x0000, 0x0000),
    (0x062A, Join::D, 0xFE95, 0xFE96, 0xFE97, 0xFE98),
    (0x062B, Join::D, 0xFE99, 0xFE9A, 0xFE9B, 0xFE9C),
    (0x062C, Join::D, 0xFE9D, 0xFE9E, 0xFE9F, 0xFEA0),
    (0x062D, Join::D, 0xFEA1, 0xFEA2, 0xFEA3, 0xFEA4),
    (0x062E, Join::D, 0xFEA5, 0xFEA6, 0xFEA7, 0xFEA8),
    (0x062F, Join::R, 0xFEA9, 0xFEAA, 0x0000, 0x0000),
    (0x0630, Join::R, 0xFEAB, 0xFEAC, 0x0000, 0x0000),
    (0x0631, Join::R, 0xFEAD, 0xFEAE, 0x0000, 0x0000),
    (0x0632, Join::R, 0xFEAF, 0xFEB0, 0x0000, 0x0000),
    (0x0633, Join::D, 0xFEB1, 0xFEB2, 0xFEB3, 0xFEB4),
    (0x0634, Join::D, 0xFEB5, 0xFEB6, 0xFEB7, 0xFEB8),
    (0x0635, Join::D, 0xFEB9, 0xFEBA, 0xFEBB, 0xFEBC),
    (0x0636, Join::D, 0xFEBD, 0xFEBE, 0xFEBF, 0xFEC0),
    (0x0637, Join::D, 0xFEC1, 0xFEC2, 0xFEC3, 0xFEC4),
    (0x0638, Join::D, 0xFEC5, 0xFEC6, 0xFEC7, 0xFEC8),
    (0x0639, Join::D, 0xFEC9, 0xFECA, 0xFECB, 0xFECC),
    (0x063A, Join::D, 0xFECD, 0xFECE, 0xFECF, 0xFED0),
    (0x0641, Join::D, 0xFED1, 0xFED2, 0xFED3, 0xFED4),
    (0x0642, Join::D, 0xFED5, 0xFED6, 0xFED7, 0xFED8),
    (0x0643, Join::D, 0xFED9, 0xFEDA, 0xFEDB, 0xFEDC),
    (0x0644, Join::D, 0xFEDD, 0xFEDE, 0xFEDF, 0xFEE0),
    (0x0645, Join::D, 0xFEE1, 0xFEE2, 0xFEE3, 0xFEE4),
    (0x0646, Join::D, 0xFEE5, 0xFEE6, 0xFEE7, 0xFEE8),
    (0x0647, Join::D, 0xFEE9, 0xFEEA, 0xFEEB, 0xFEEC),
    (0x0648, Join::R, 0xFEED, 0xFEEE, 0x0000, 0x0000),
    (0x0649, Join::D, 0xFEEF, 0xFEF0, 0xFBE8, 0xFBE9),
    (0x064A, Join::D, 0xFEF1, 0xFEF2, 0xFEF3, 0xFEF4),
    (0x0671, Join::R, 0xFB50, 0xFB51, 0x0000, 0x0000),
    (0x0677, Join::R, 0xFBDD, 0x0000, 0x0000, 0x0000),
    (0x0679, Join::D, 0xFB66, 0xFB67, 0xFB68, 0xFB69),
    (0x067A, Join::D, 0xFB5E, 0xFB5F, 0xFB60, 0xFB61),
    (0x067B, Join::D, 0xFB52, 0xFB53, 0xFB54, 0xFB55),
    (0x067E, Join::D, 0xFB56, 0xFB57, 0xFB58, 0xFB59),
    (0x067F, Join::D, 0xFB62, 0xFB63, 0xFB64, 0xFB65),
    (0x0680, Join::D, 0xFB5A, 0xFB5B, 0xFB5C, 0xFB5D),
    (0x0683, Join::D, 0xFB76, 0xFB77, 0xFB78, 0xFB79),
    (0x0684, Join::D, 0xFB72, 0xFB73, 0xFB74, 0xFB75),
    (0x0686, Join::D, 0xFB7A, 0xFB7B, 0xFB7C, 0xFB7D),
    (0x0687, Join::D, 0xFB7E, 0xFB7F, 0xFB80, 0xFB81),
    (0x0688, Join::R, 0xFB88, 0xFB89, 0x0000, 0x0000),
    (0x068C, Join::R, 0xFB84, 0xFB85, 0x0000, 0x0000),
    (0x068D, Join::R, 0xFB82, 0xFB83, 0x0000, 0x0000),
    (0x068E, Join::R, 0xFB86, 0xFB87, 0x0000, 0x0000),
    (0x0691, Join::R, 0xFB8C, 0xFB8D, 0x0000, 0x0000),
    (0x0698, Join::R, 0xFB8A, 0xFB8B, 0x0000, 0x0000),
    (0x06A4, Join::D, 0xFB6A, 0xFB6B, 0xFB6C, 0xFB6D),
    (0x06A6, Join::D, 0xFB6E, 0xFB6F, 0xFB70, 0xFB71),
    (0x06A9, Join::D, 0xFB8E, 0xFB8F, 0xFB90, 0xFB91),
    (0x06AD, Join::D, 0xFBD3, 0xFBD4, 0xFBD5, 0xFBD6),
    (0x06AF, Join::D, 0xFB92, 0xFB93, 0xFB94, 0xFB95),
    (0x06B1, Join::D, 0xFB9A, 0xFB9B, 0xFB9C, 0xFB9D),
    (0x06B3, Join::D, 0xFB96, 0xFB97, 0xFB98, 0xFB99),
    (0x06BA, Join::D, 0xFB9E, 0xFB9F, 0x0000, 0x0000),
    (0x06BB, Join::D, 0xFBA0, 0xFBA1, 0xFBA2, 0xFBA3),
    (0x06BE, Join::D, 0xFBAA, 0xFBAB, 0xFBAC, 0xFBAD),
    (0x06C0, Join::R, 0xFBA4, 0xFBA5, 0x0000, 0x0000),
    (0x06C1, Join::D, 0xFBA6, 0xFBA7, 0xFBA8, 0xFBA9),
    (0x06C5, Join::R, 0xFBE0, 0xFBE1, 0x0000, 0x0000),
    (0x06C6, Join::R, 0xFBD9, 0xFBDA, 0x0000, 0x0000),
    (0x06C7, Join::R, 0xFBD7, 0xFBD8, 0x0000, 0x0000),
    (0x06C8, Join::R, 0xFBDB, 0xFBDC, 0x0000, 0x0000),
    (0x06C9, Join::R, 0xFBE2, 0xFBE3, 0x0000, 0x0000),
    (0x06CB, Join::R, 0xFBDE, 0xFBDF, 0x0000, 0x0000),
    (0x06CC, Join::D, 0xFBFC, 0xFBFD, 0xFBFE, 0xFBFF),
    (0x06D0, Join::D, 0xFBE4, 0xFBE5, 0xFBE6, 0xFBE7),
    (0x06D2, Join::R, 0xFBAE, 0xFBAF, 0x0000, 0x0000),
    (0x06D3, Join::R, 0xFBB0, 0xFBB1, 0x0000, 0x0000),
];

/// Joining types for in-scope letters that carry no
/// presentation forms of their own (they still connect
/// their neighbours).
static FORMLESS_JOINING: &[(u32, Join)] = &[
    (0x0620, Join::D),
    (0x063B, Join::D),
    (0x063C, Join::D),
    (0x063D, Join::D),
    (0x063E, Join::D),
    (0x063F, Join::D),
    (0x0640, Join::C),
    (0x066E, Join::D),
    (0x066F, Join::D),
    (0x0672, Join::R),
    (0x0673, Join::R),
    (0x0675, Join::R),
    (0x0676, Join::R),
    (0x0678, Join::D),
    (0x067C, Join::D),
    (0x067D, Join::D),
    (0x0681, Join::D),
    (0x0682, Join::D),
    (0x0685, Join::D),
    (0x0689, Join::R),
    (0x068A, Join::R),
    (0x068B, Join::R),
    (0x068F, Join::R),
    (0x0690, Join::R),
    (0x0692, Join::R),
    (0x0693, Join::R),
    (0x0694, Join::R),
    (0x0695, Join::R),
    (0x0696, Join::R),
    (0x0697, Join::R),
    (0x0699, Join::R),
    (0x069A, Join::D),
    (0x069B, Join::D),
    (0x069C, Join::D),
    (0x069D, Join::D),
    (0x069E, Join::D),
    (0x069F, Join::D),
    (0x06A0, Join::D),
    (0x06A1, Join::D),
    (0x06A2, Join::D),
    (0x06A3, Join::D),
    (0x06A5, Join::D),
    (0x06A7, Join::D),
    (0x06A8, Join::D),
    (0x06AA, Join::D),
    (0x06AB, Join::D),
    (0x06AC, Join::D),
    (0x06AE, Join::D),
    (0x06B0, Join::D),
    (0x06B2, Join::D),
    (0x06B4, Join::D),
    (0x06B5, Join::D),
    (0x06B6, Join::D),
    (0x06B7, Join::D),
    (0x06B8, Join::D),
    (0x06B9, Join::D),
    (0x06BC, Join::D),
    (0x06BD, Join::D),
    (0x06BF, Join::D),
    (0x06C2, Join::D),
    (0x06C3, Join::R),
    (0x06C4, Join::R),
    (0x06CA, Join::R),
    (0x06CD, Join::R),
    (0x06CE, Join::D),
    (0x06CF, Join::R),
    (0x06D1, Join::D),
    (0x06D5, Join::R),
    (0x06EE, Join::R),
    (0x06EF, Join::R),
    (0x06FA, Join::D),
    (0x06FB, Join::D),
    (0x06FC, Join::D),
    (0x06FF, Join::D),
    (0x0750, Join::D),
    (0x0751, Join::D),
    (0x0752, Join::D),
    (0x0753, Join::D),
    (0x0754, Join::D),
    (0x0755, Join::D),
    (0x0756, Join::D),
    (0x0757, Join::D),
    (0x0758, Join::D),
    (0x0759, Join::R),
    (0x075A, Join::R),
    (0x075B, Join::R),
    (0x075C, Join::D),
    (0x075D, Join::D),
    (0x075E, Join::D),
    (0x075F, Join::D),
    (0x0760, Join::D),
    (0x0761, Join::D),
    (0x0762, Join::D),
    (0x0763, Join::D),
    (0x0764, Join::D),
    (0x0765, Join::D),
    (0x0766, Join::D),
    (0x0767, Join::D),
    (0x0768, Join::D),
    (0x0769, Join::D),
    (0x076A, Join::D),
    (0x076B, Join::R),
    (0x076C, Join::R),
    (0x076D, Join::D),
    (0x076E, Join::D),
    (0x076F, Join::D),
    (0x0770, Join::D),
    (0x0771, Join::R),
    (0x0772, Join::D),
    (0x0773, Join::R),
    (0x0774, Join::R),
    (0x0775, Join::D),
    (0x0776, Join::D),
    (0x0777, Join::D),
    (0x0778, Join::R),
    (0x0779, Join::R),
    (0x077A, Join::D),
    (0x077B, Join::D),
    (0x077C, Join::D),
    (0x077D, Join::D),
    (0x077E, Join::D),
    (0x077F, Join::D),
    (0x08A0, Join::D),
    (0x08A1, Join::D),
    (0x08A2, Join::D),
    (0x08A3, Join::D),
    (0x08A4, Join::D),
    (0x08A5, Join::D),
    (0x08A6, Join::D),
    (0x08A7, Join::D),
    (0x08A8, Join::D),
    (0x08A9, Join::D),
    (0x08AA, Join::R),
    (0x08AB, Join::R),
    (0x08AC, Join::R),
    (0x08AE, Join::R),
    (0x08AF, Join::D),
    (0x08B0, Join::D),
    (0x08B1, Join::R),
    (0x08B2, Join::R),
    (0x08B3, Join::D),
    (0x08B4, Join::D),
    (0x08B5, Join::D),
    (0x08B6, Join::D),
    (0x08B7, Join::D),
    (0x08B8, Join::D),
    (0x08B9, Join::R),
    (0x08BA, Join::D),
    (0x08BB, Join::D),
    (0x08BC, Join::D),
    (0x08BD, Join::D),
    (0x08BE, Join::D),
    (0x08BF, Join::D),
    (0x08C0, Join::D),
    (0x08C1, Join::D),
    (0x08C2, Join::D),
    (0x08C3, Join::D),
    (0x08C4, Join::D),
    (0x08C5, Join::D),
    (0x08C6, Join::D),
    (0x08C7, Join::D),
    (0x08C8, Join::D),
];

/// Lam-alef ligatures: (alef variant, isolated, final).
static LAM_ALEF: &[(u32, u32, u32)] = &[
    (0x0622, 0xFEF5, 0xFEF6),
    (0x0623, 0xFEF7, 0xFEF8),
    (0x0625, 0xFEF9, 0xFEFA),
    (0x0627, 0xFEFB, 0xFEFC),
];
