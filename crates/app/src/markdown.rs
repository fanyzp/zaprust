//! WhatsApp-style inline formatting for message text.
//!
//! WhatsApp uses `*bold*`, `_italic_`, `~strikethrough~`, and `` `mono` ``
//! (plus triple-backtick blocks). This module parses that syntax into an
//! egui [`LayoutJob`] so bubbles can render it for real instead of
//! showing the raw markers. Deliberately small: no links, no headings,
//! no nesting beyond one level, matching what WhatsApp itself does.

use std::sync::Arc;

use eframe::egui::{self, Color32, FontFamily, FontId, Galley, Stroke, TextFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Style {
    bold: bool,
    italic: bool,
    strike: bool,
    mono: bool,
    mention: bool,
}

fn parse(text: &str) -> Vec<(String, Style)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<(String, Style)> = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    let flush = |out: &mut Vec<(String, Style)>, cur: &mut String, style: Style| {
        if !cur.is_empty() {
            out.push((std::mem::take(cur), style));
        }
    };
    while i < chars.len() {
        let c = chars[i];
        // Mentions: `@word` at a word boundary (WhatsApp highlights these).
        if c == '@' && (i == 0 || chars[i - 1].is_whitespace()) {
            let mut j = i + 1;
            while j < chars.len()
                && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '.')
            {
                j += 1;
            }
            if j > i + 1 {
                flush(&mut out, &mut cur, Style::default());
                out.push((
                    chars[i..j].iter().collect(),
                    Style { mention: true, ..Style::default() },
                ));
                i = j;
                continue;
            }
        }
        let marker = match c {
            '*' => Some((Style { bold: true, ..Style::default() }, '*')),
            '_' => Some((Style { italic: true, ..Style::default() }, '_')),
            '~' => Some((Style { strike: true, ..Style::default() }, '~')),
            '`' => Some((Style { mono: true, ..Style::default() }, '`')),
            _ => None,
        };
        if let Some((style, m)) = marker {
            // Find the closing marker with non-empty content between.
            if let Some(rel) = chars[i + 1..].iter().position(|&x| x == m) {
                let end = i + 1 + rel;
                let inner: String = chars[i + 1..end].iter().collect();
                if !inner.is_empty() && !inner.starts_with(' ') && !inner.ends_with(' ') {
                    flush(&mut out, &mut cur, Style::default());
                    out.push((inner, style));
                    i = end + 1;
                    continue;
                }
            }
        }
        cur.push(c);
        i += 1;
    }
    flush(&mut out, &mut cur, Style::default());
    out
}

/// The block structure of one line: WhatsApp's block syntax is
/// line-based (a fenced code block opens and closes on its own
/// line, a list item starts its line), so the job builder
/// classifies each line before the inline parser sees it.
#[derive(Debug, PartialEq, Eq)]
enum BlockLine<'a> {
    /// A normal line: inline formatting applies.
    Text(&'a str),
    /// One line inside a fenced code block: mono, no inline
    /// parsing, no marker characters.
    Code(&'a str),
    /// A list item: the marker to paint (a bullet, or the item's
    /// own number) and the item text, which still parses inline.
    Item(String, &'a str),
}

/// Classifies one line, tracking the fence state across lines.
/// A fence delimiter line (three backticks at the line's start,
/// an optional language tag after them) toggles the state and
/// renders nothing itself.
fn block_line<'a>(line: &'a str, in_fence: &mut bool) -> Option<BlockLine<'a>> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        *in_fence = !*in_fence;
        return None;
    }
    if *in_fence {
        return Some(BlockLine::Code(line));
    }
    for marker in ["- ", "• "] {
        if let Some(item) = trimmed.strip_prefix(marker) {
            return Some(BlockLine::Item("•  ".to_owned(), item));
        }
    }
    // An ordered item: one to three digits, a dot, a space.
    let digits = trimmed.len() - trimmed.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if (1..=3).contains(&digits) {
        if let Some(item) = trimmed[digits..].strip_prefix(". ") {
            return Some(BlockLine::Item(format!("{}  ", &trimmed[..digits + 1]), item));
        }
    }
    Some(BlockLine::Text(line))
}

fn job(
    text: &str,
    size: f32,
    color: Color32,
    code_bg: Color32,
    mention_color: Color32,
    leading: Option<f32>,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let plain = FontId::proportional(size);
    let mut in_fence = false;
    // Rows are separated by appending the newline before each
    // rendered line after the first, so a fence delimiter (which
    // renders nothing) never leaves a blank row behind, wherever
    // it sits.
    let mut started = false;
    for raw_line in text.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let Some(block) = block_line(line, &mut in_fence) else { continue };
        if started {
            job.append(
                "\n",
                0.0,
                TextFormat {
                    font_id: plain.clone(),
                    color,
                    line_height: leading,
                    ..Default::default()
                },
            );
        }
        started = true;
        match block {
            BlockLine::Code(code) => {
                job.append(
                    code,
                    0.0,
                    TextFormat {
                        font_id: FontId::monospace(size - 1.0),
                        color,
                        background: code_bg,
                        line_height: leading,
                        ..Default::default()
                    },
                );
            }
            BlockLine::Item(marker, item) => {
                job.append(
                    &marker,
                    0.0,
                    TextFormat {
                        font_id: plain.clone(),
                        color,
                        line_height: leading,
                        ..Default::default()
                    },
                );
                append_inline(&mut job, item, size, color, code_bg, mention_color, leading, &plain);
            }
            BlockLine::Text(line) => {
                append_inline(&mut job, line, size, color, code_bg, mention_color, leading, &plain);
            }
        }
    }
    // Direction arrives handled: callers pass the display
    // form of their text (core::bidi: Arabic reshaped into
    // contextual forms, paragraphs reordered into visual
    // order), so this job lays out strictly left to right
    // like every other. An earlier right-alignment shortcut
    // for RTL text lived here; it double-applied direction
    // once the real transform existed and broke the bubble
    // geometry, so it is gone.
    job
}

/// Appends one line's inline-parsed segments to the job.
#[allow(clippy::too_many_arguments)]
fn append_inline(
    job: &mut egui::text::LayoutJob,
    text: &str,
    size: f32,
    color: Color32,
    code_bg: Color32,
    mention_color: Color32,
    leading: Option<f32>,
    plain: &FontId,
) {
    for (segment, style) in parse(text) {
        let format = if style.mention {
            TextFormat {
                font_id: FontId::new(size, FontFamily::Name("bold".into())),
                color: mention_color,
                line_height: leading,
                ..Default::default()
            }
        } else if style.mono {
            TextFormat {
                font_id: FontId::monospace(size - 1.0),
                color,
                background: code_bg,
                line_height: leading,
                ..Default::default()
            }
        } else if style.bold {
            TextFormat {
                font_id: FontId::new(size, FontFamily::Name("bold".into())),
                color,
                line_height: leading,
                ..Default::default()
            }
        } else if style.italic {
            TextFormat {
                font_id: FontId::new(size, FontFamily::Name("oblique".into())),
                color,
                line_height: leading,
                ..Default::default()
            }
        } else if style.strike {
            TextFormat {
                font_id: plain.clone(),
                color,
                strikethrough: Stroke::new(1.0_f32, color),
                line_height: leading,
                ..Default::default()
            }
        } else {
            TextFormat { font_id: plain.clone(), color, line_height: leading, ..Default::default() }
        };
        job.append(&segment, 0.0, format);
    }
}

/// Lays out `text` with WhatsApp formatting, wrapped at `wrap` pixels.
pub(crate) fn galley(
    ui: &egui::Ui,
    text: &str,
    size: f32,
    color: Color32,
    code_bg: Color32,
    mention_color: Color32,
    wrap: f32,
) -> Arc<Galley> {
    galley_leading(ui, text, size, color, code_bg, mention_color, wrap, None)
}

/// Like [`galley`], with an explicit line height (in points) for
/// every row: the bubble body text passes about 1.5x its font
/// size for the airier rounded-modern reading rhythm.
#[allow(clippy::too_many_arguments)]
pub(crate) fn galley_leading(
    ui: &egui::Ui,
    text: &str,
    size: f32,
    color: Color32,
    code_bg: Color32,
    mention_color: Color32,
    wrap: f32,
    leading: Option<f32>,
) -> Arc<Galley> {
    let mut layout = job(text, size, color, code_bg, mention_color, leading);
    layout.wrap.max_width = wrap;
    // Rows break at word boundaries (then dashes, then
    // punctuation), never inside a word that fits whole on a
    // line of its own: a mid-word split with no hyphen reads
    // as corruption. A single unbroken token longer than the
    // wrap (a long link, a pasted hash) still breaks, through
    // the break candidates' last-resort fallback, so it wraps
    // inside the bubble instead of overflowing it.
    layout.wrap.break_anywhere = false;
    ui.ctx().fonts_mut(|f| f.layout_job(layout))
}

/// The single-line (unwrapped) width of the formatted text.
pub(crate) fn measure(
    ui: &egui::Ui,
    text: &str,
    size: f32,
    color: Color32,
    code_bg: Color32,
    mention_color: Color32,
) -> f32 {
    galley(ui, text, size, color, code_bg, mention_color, f32::INFINITY).size().x
}

#[cfg(test)]
mod tests {
    use super::block_line;
    use super::job;
    use super::parse;
    use super::BlockLine;
    use super::Style;
    use eframe::egui::{Color32, FontFamily};

    /// The body of the demo showcase message (GTA VI chat): one
    /// bubble mixing every WhatsApp span, `*bold*`, `` `mono` ``
    /// and `~strike~`.
    fn showcase_body() -> String {
        for chat in core::model::demo_chats() {
            if let Some(msg) = core::model::demo_messages_for(chat.id)
                .into_iter()
                .find(|m| m.body.contains("probably"))
            {
                return msg.body;
            }
        }
        panic!("the demo showcase message exists");
    }

    #[test]
    fn showcase_message_parses_to_the_intended_spans() {
        let body = showcase_body();
        let parts = parse(&body);
        let styled: Vec<(&str, Style)> = parts
            .iter()
            .map(|(t, s)| (t.as_str(), *s))
            .filter(|(_, s)| *s != Style::default())
            .collect();
        assert!(styled.contains(&("preload", Style { bold: true, ..Style::default() })));
        assert!(styled.contains(&("95 GB", Style { mono: true, ..Style::default() })));
        assert!(styled.contains(&("probably", Style { strike: true, ..Style::default() })));
        // The spans tile the plain text exactly: no marker
        // characters survive, and no character is dropped,
        // doubled or reordered.
        let plain: String = parts.iter().map(|(t, _)| t.as_str()).collect();
        let expected: String = body.chars().filter(|c| !matches!(c, '*' | '`' | '~')).collect();
        assert_eq!(plain, expected);
    }

    /// The union of the solid triangles of one colour in a row
    /// mesh. Backgrounds and strike/underline lines tessellate
    /// as solid (single-uv) triangles, unlike textured glyph
    /// quads, so this isolates exactly what is painted for them.
    fn solid_union(
        mesh: &eframe::egui::Mesh,
        color: eframe::egui::Color32,
    ) -> Option<eframe::egui::Rect> {
        let mut union: Option<eframe::egui::Rect> = None;
        for tri in mesh.indices.chunks_exact(3) {
            let vs = [
                mesh.vertices[tri[0] as usize],
                mesh.vertices[tri[1] as usize],
                mesh.vertices[tri[2] as usize],
            ];
            if vs.iter().all(|v| v.color == color && v.uv == vs[0].uv) {
                for v in vs {
                    match &mut union {
                        None => union = Some(eframe::egui::Rect::from_min_max(v.pos, v.pos)),
                        Some(r) => r.extend_with(v.pos),
                    }
                }
            }
        }
        union
    }

    /// Board item 98: the struck word in the showcase message
    /// was reported as rendering overlapped. The layout contract
    /// that keeps it clean is pinned here at the two widths the
    /// bubble paints this body at on a 1440x900 desktop (the
    /// measure-pass cap and the final inner width once the
    /// bubble hugs its rows): glyphs never advance over a
    /// neighbour, the strike is a single thin band spanning
    /// exactly the struck word and sitting in the middle of its
    /// line box, and the mono background covers exactly its own
    /// span.
    #[test]
    fn showcase_strike_band_covers_exactly_the_struck_word() {
        let body = showcase_body();
        let ctx = eframe::egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let input = eframe::egui::RawInput {
            screen_rect: Some(eframe::egui::Rect::from_min_size(
                eframe::egui::Pos2::ZERO,
                eframe::egui::vec2(1440.0, 900.0),
            )),
            ..Default::default()
        };
        let text_color = eframe::egui::Color32::WHITE;
        let code_bg = eframe::egui::Color32::from_gray(60);
        let mut checked = 0;
        let mut out = ctx.run_pass(input, |ctx| {
            eframe::egui::Area::new(eframe::egui::Id::unique("markdown-layout-test")).show(ctx, |ui| {
                for wrap in [602.0, 585.0] {
                    let galley = super::galley_leading(
                        ui,
                        &body,
                        crate::theme::TEXT_BODY,
                        text_color,
                        code_bg,
                        eframe::egui::Color32::BLUE,
                        wrap,
                        Some(crate::theme::TEXT_BODY * 1.5),
                    );
                    assert_eq!(galley.rows.len(), 2, "showcase wraps to two rows at {wrap}");
                    for row in &galley.rows {
                        for pair in row.glyphs.windows(2) {
                            let end = pair[0].pos.x + pair[0].advance_width;
                            assert!(
                                pair[1].pos.x >= end - 1.01,
                                "glyph {:?} starts at {:.2}, over the advance of {:?} ending at {:.2}",
                                pair[1].chr,
                                pair[1].pos.x,
                                pair[0].chr,
                                end
                            );
                        }
                    }
                    let row =
                        galley.rows.iter().find(|r| r.text().contains("probably")).expect("row with the struck word");
                    // The whole message is ASCII, so the byte
                    // offset of the word is its glyph offset.
                    let text: String = row.glyphs.iter().map(|g| g.chr).collect();
                    let start = text.find("probably").expect("word in row");
                    let word = &row.glyphs[start..start + "probably".len()];
                    let span = (word[0].pos.x, word[word.len() - 1].max_x());
                    let band = solid_union(&row.visuals.mesh, text_color).expect("strike band tessellated");
                    assert!(
                        (band.left() - span.0).abs() <= 2.0,
                        "band starts at {:.1}, word at {:.1}",
                        band.left(),
                        span.0
                    );
                    assert!(
                        (band.right() - span.1).abs() <= 2.0,
                        "band ends at {:.1}, word at {:.1}",
                        band.right(),
                        span.1
                    );
                    assert!(band.height() <= 2.0, "strike band is one thin line, got {:.1} tall", band.height());
                    let (top, bottom) = word.iter().fold((f32::MAX, f32::MIN), |(t, b), g| {
                        let r = g.logical_rect();
                        (t.min(r.top()), b.max(r.bottom()))
                    });
                    let mid = band.center().y;
                    assert!(
                        mid >= top + (bottom - top) * 0.25 && mid <= top + (bottom - top) * 0.75,
                        "strike at {mid:.1} sits in the middle of the word's line box {top:.1}..{bottom:.1}"
                    );
                    let first = &galley.rows[0];
                    let text0: String = first.glyphs.iter().map(|g| g.chr).collect();
                    let mstart = text0.find("95 GB").expect("mono span in first row");
                    let mono = &first.glyphs[mstart..mstart + "95 GB".len()];
                    let mspan = (mono[0].pos.x, mono[mono.len() - 1].max_x());
                    let bg = solid_union(&first.visuals.mesh, code_bg).expect("mono background tessellated");
                    assert!(
                        (bg.left() - mspan.0).abs() <= 2.5 && (bg.right() - mspan.1).abs() <= 2.5,
                        "mono background {:.1}..{:.1} covers its span {:.1}..{:.1}",
                        bg.left(),
                        bg.right(),
                        mspan.0,
                        mspan.1
                    );
                    checked += 1;
                }
            });
        });
        out.textures_delta.clear();
        assert_eq!(checked, 2);
    }

    #[test]
    fn parses_mentions() {
        let parts = parse("@everyone meeting on *Thursday* ok");
        assert_eq!(parts[0].0, "@everyone");
        assert!(parts[0].1.mention);
        assert!(parts.iter().any(|p| p.1.bold && p.0 == "Thursday"));
        // An @ inside a word (email-like) is not a mention.
        let parts = parse("send it to a@b.id please");
        assert!(parts.iter().all(|p| !p.1.mention));
    }

    #[test]
    fn parses_whatsapp_markers() {
        let parts = parse("hello *world* and _italic_ ~strike~ `code` plain");
        let texts: Vec<&str> = parts.iter().map(|p| p.0.as_str()).collect();
        assert_eq!(
            texts,
            vec!["hello ", "world", " and ", "italic", " ", "strike", " ", "code", " plain"]
        );
        assert!(parts[1].1.bold);
        assert!(parts[3].1.italic);
        assert!(parts[5].1.strike);
        assert!(parts[7].1.mono);
    }

    #[test]
    fn unclosed_or_empty_markers_stay_literal() {
        let parts = parse("2 * 3 = 6 and *not closed");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].1, Style::default());
    }

    fn section_text(
        job: &eframe::egui::text::LayoutJob,
        section: &eframe::egui::text::LayoutSection,
    ) -> String {
        job.text[section.byte_range.start.0..section.byte_range.end.0].to_string()
    }

    fn job_text(job: &eframe::egui::text::LayoutJob) -> String {
        job.sections.iter().map(|section| section_text(job, section)).collect()
    }

    #[test]
    fn block_lines_classify_lists_and_fences() {
        let mut in_fence = false;
        assert_eq!(
            block_line("- milk", &mut in_fence),
            Some(BlockLine::Item("•  ".to_owned(), "milk"))
        );
        assert_eq!(
            block_line("• bread", &mut in_fence),
            Some(BlockLine::Item("•  ".to_owned(), "bread"))
        );
        assert_eq!(
            block_line("1. first", &mut in_fence),
            Some(BlockLine::Item("1.  ".to_owned(), "first"))
        );
        assert_eq!(
            block_line("12. twelfth", &mut in_fence),
            Some(BlockLine::Item("12.  ".to_owned(), "twelfth"))
        );
        assert_eq!(
            block_line("1.5 not a list", &mut in_fence),
            Some(BlockLine::Text("1.5 not a list"))
        );
        assert_eq!(block_line("-5 degrees", &mut in_fence), Some(BlockLine::Text("-5 degrees")));
        assert_eq!(block_line("plain", &mut in_fence), Some(BlockLine::Text("plain")));
        assert_eq!(block_line("```", &mut in_fence), None);
        assert!(in_fence);
        // Inside a fence, list markers are literal code.
        assert_eq!(
            block_line("- not a list", &mut in_fence),
            Some(BlockLine::Code("- not a list"))
        );
        assert_eq!(block_line("```rust", &mut in_fence), None);
        assert!(!in_fence);
    }

    #[test]
    fn job_renders_lists_and_code_blocks() {
        let text = "groceries:\n- milk\n- *fresh* bread\n2. eggs\n```\nlet x = -1;\n```\ndone";
        let job =
            job(text, 14.0, Color32::WHITE, Color32::from_gray(40), Color32::LIGHT_BLUE, None);
        assert_eq!(
            job_text(&job),
            "groceries:\n•  milk\n•  fresh bread\n2.  eggs\nlet x = -1;\ndone"
        );
        // The code line is one monospace section on the code
        // background; the fence delimiters never appear.
        let code = job
            .sections
            .iter()
            .find(|s| section_text(&job, s) == "let x = -1;")
            .expect("code section");
        assert_eq!(code.format.font_id.family, FontFamily::Monospace);
        assert_eq!(code.format.background, Color32::from_gray(40));
        // Inline formatting still applies inside list items.
        let fresh =
            job.sections.iter().find(|s| section_text(&job, s) == "fresh").expect("bold section");
        assert_eq!(fresh.format.font_id.family, FontFamily::Name("bold".into()));
    }

    #[test]
    fn job_fence_at_the_edges_leaves_no_blank_rows() {
        let fenced = job(
            "```\ncode\n```",
            14.0,
            Color32::WHITE,
            Color32::from_gray(40),
            Color32::LIGHT_BLUE,
            None,
        );
        assert_eq!(job_text(&fenced), "code");
        let plain = job(
            "plain\ntext",
            14.0,
            Color32::WHITE,
            Color32::from_gray(40),
            Color32::LIGHT_BLUE,
            None,
        );
        assert_eq!(job_text(&plain), "plain\ntext");
    }

    /// Lays out `body` the way a bubble paints it (same size,
    /// same leading, same wrap) inside a throwaway pass.
    fn bubble_galley(body: &str, wrap: f32) -> std::sync::Arc<eframe::egui::Galley> {
        let ctx = eframe::egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let input = eframe::egui::RawInput {
            screen_rect: Some(eframe::egui::Rect::from_min_size(
                eframe::egui::Pos2::ZERO,
                eframe::egui::vec2(1440.0, 900.0),
            )),
            ..Default::default()
        };
        let mut galley = None;
        let mut out = ctx.run_pass(input, |ctx| {
            eframe::egui::Area::new(eframe::egui::Id::unique("wrap-test")).show(ctx, |ui| {
                galley = Some(super::galley_leading(
                    ui,
                    body,
                    crate::theme::TEXT_BODY,
                    eframe::egui::Color32::WHITE,
                    eframe::egui::Color32::from_gray(60),
                    eframe::egui::Color32::BLUE,
                    wrap,
                    Some(crate::theme::TEXT_BODY * 1.5),
                ));
            });
        });
        out.textures_delta.clear();
        galley.expect("galley laid out")
    }

    /// VRF-01: the shipped bubble split "install" across rows
    /// ("inst" / "all") because the wrap allowed a break at any
    /// character. Rows must break at word boundaries: joining
    /// the trailing fragment of one row with the leading
    /// fragment of the next must never rebuild a word that
    /// would have fitted whole inside the wrap.
    #[test]
    fn wrapping_never_splits_a_word_that_fits() {
        let body = showcase_body();
        for wrap in [602.0, 585.0, 300.0, 190.0] {
            let galley = bubble_galley(&body, wrap);
            let rows: Vec<String> =
                galley.rows.iter().map(|r| r.glyphs.iter().map(|g| g.chr).collect()).collect();
            assert!(rows.len() >= 2, "showcase wraps at {wrap}");
            for pair in rows.windows(2) {
                let tail: String = pair[0]
                    .chars()
                    .rev()
                    .take_while(|c| c.is_alphanumeric())
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                let head: String = pair[1].chars().take_while(|c| c.is_alphanumeric()).collect();
                if tail.is_empty() || head.is_empty() {
                    continue;
                }
                let rejoined = format!("{tail}{head}");
                let width = bubble_galley(&rejoined, f32::INFINITY).size().x;
                assert!(
                    width > wrap,
                    "rows split the word {rejoined:?} at wrap {wrap}: {:?} / {:?}",
                    pair[0],
                    pair[1]
                );
            }
        }
    }

    /// The other half of the wrap contract: a token longer
    /// than the bubble (a link, a hash) must still break
    /// across rows instead of overflowing, and every row it
    /// produces stays inside the wrap.
    #[test]
    fn overlong_token_still_breaks_inside_the_wrap() {
        let token = "a".repeat(90);
        let body = format!("see {token} end");
        let galley = bubble_galley(&body, 200.0);
        assert!(galley.rows.len() >= 4, "the token breaks across rows: {}", galley.rows.len());
        for row in &galley.rows {
            let width = row.glyphs.iter().map(|g| g.max_x()).fold(0.0_f32, f32::max);
            assert!(width <= 200.5, "row stays inside the wrap, got {width}");
        }
    }

    /// VRF-01 paint layer: the row mesh is what the tessellator
    /// copies verbatim into the frame's triangles, so a glyph
    /// that paints twice (the reported overlap) would show up
    /// here as extra vertices. Every glyph of the showcase
    /// body, styled or plain, owns exactly one textured quad
    /// (four vertices) whose corners are its atlas cell rect, and
    /// spaces own none. Solid geometry (the mono background,
    /// the strike band) tessellates as single-uv triangles and
    /// is not counted as glyph paint.
    #[test]
    fn every_glyph_paints_as_exactly_one_quad() {
        let body = showcase_body();
        let galley = bubble_galley(&body, 585.0);
        for row in &galley.rows {
            let mesh = &row.visuals.mesh;
            let textured_glyphs = row.glyphs.iter().filter(|g| g.chr != ' ').count();
            let mut textured_tris = 0;
            for tri in mesh.indices.chunks_exact(3) {
                let vs: Vec<_> = tri.iter().map(|&i| mesh.vertices[i as usize]).collect();
                if !(vs[0].uv == vs[1].uv && vs[1].uv == vs[2].uv) {
                    textured_tris += 1;
                }
            }
            assert_eq!(
                textured_tris,
                textured_glyphs * 2,
                "one textured quad (two triangles) per glyph in row {:?}",
                row.text()
            );
            for (i, glyph) in row.glyphs.iter().enumerate() {
                let start = glyph.first_vertex as usize;
                // The next glyph's first vertex bounds this
                // glyph's range; the last glyph is followed
                // only by solid geometry, so its own quad is
                // the first four vertices of its range.
                let end =
                    row.glyphs.get(i + 1).map(|n| n.first_vertex as usize).unwrap_or(usize::MAX);
                if glyph.chr == ' ' {
                    // A trailing space has no next glyph to
                    // bound it; the triangle count above already
                    // proves it painted nothing.
                    if i + 1 < row.glyphs.len() {
                        assert_eq!(end, start, "a space paints no quad");
                    }
                    continue;
                }
                let verts = &mesh.vertices[start..(start + 4).min(mesh.vertices.len())];
                assert_eq!(verts.len(), 4, "glyph {:?} has a quad", glyph.chr);
                if i + 1 < row.glyphs.len() {
                    assert_eq!(end, start + 4, "glyph {:?} owns exactly four vertices", glyph.chr);
                }
                // The quad a glyph paints is its atlas cell
                // placed at the glyph's position plus the
                // cell's bearing offset.
                let rect = eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::new(
                        glyph.pos.x + glyph.uv_rect.offset[0],
                        glyph.pos.y + glyph.uv_rect.offset[1],
                    ),
                    eframe::egui::Vec2::new(glyph.uv_rect.size[0], glyph.uv_rect.size[1]),
                );
                let (mut min_x, mut min_y) = (f32::MAX, f32::MAX);
                let (mut max_x, mut max_y) = (f32::MIN, f32::MIN);
                for v in verts {
                    min_x = min_x.min(v.pos.x);
                    min_y = min_y.min(v.pos.y);
                    max_x = max_x.max(v.pos.x);
                    max_y = max_y.max(v.pos.y);
                }
                assert!(
                    (min_x - rect.min.x).abs() <= 0.01
                        && (min_y - rect.min.y).abs() <= 0.01
                        && (max_x - rect.max.x).abs() <= 0.01
                        && (max_y - rect.max.y).abs() <= 0.01,
                    "glyph {:?} quad covers its atlas cell rect",
                    glyph.chr
                );
            }
        }
    }
}
