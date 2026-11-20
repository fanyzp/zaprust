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
