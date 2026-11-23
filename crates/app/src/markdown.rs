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
    job
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
    // A single unbroken token (a long link, a pasted hash, a
    // run of digits) must wrap inside the bubble instead of
    // overflowing it: WhatsApp breaks such runs, and a bubble
    // that clips its own text reads as a bug.
    layout.wrap.break_anywhere = true;
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
    use super::parse;
    use super::Style;

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
                        14.5,
                        text_color,
                        code_bg,
                        eframe::egui::Color32::BLUE,
                        wrap,
                        Some(14.5 * 1.5),
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
}
