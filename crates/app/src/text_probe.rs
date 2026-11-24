//! P2-65 measurement probe (test-only): what the app's text
//! stack does with RTL and mixed-direction strings, measured
//! at the layout level with the app's real font installation.
//! Run with output shown:
//!   cargo test -p app bidi_probe -- --nocapture
//! The probe prints, per probe string: which font face (if
//! any) covers each character, and the laid-out glyph order
//! with x positions. It asserts nothing: it is the measuring
//! instrument whose readings the round's log records, before
//! and after the fix.

use eframe::egui;

/// (name, text) probe strings: pure RTL, RTL with embedded
/// Latin and digits, and a Latin control.
const PROBES: &[(&str, &str)] = &[
    ("latin", "Hello world"),
    ("arabic", "مرحبا بالعالم"),
    ("arabic_mixed", "سنلتقي غدا at 5"),
    ("arabic_digits", "المبلغ 1234 ريال"),
    ("hebrew", "שלום עולם"),
    ("hebrew_mixed", "פגישה tomorrow ב-9"),
];

fn report(ctx: &egui::Context, stack: &str) {
    // Fonts exist only after the first frame pass. The
    // frame's texture delta is cleared rather than applied:
    // nothing is painted in this probe.
    let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
    out.textures_delta.clear();
    ctx.fonts_mut(|fonts| {
        let chars = fonts.characters(&egui::FontFamily::Proportional).clone();
        for (name, text) in PROBES {
            println!("== [{stack}] probe {name}: {text}");
            for c in text.chars() {
                if c == ' ' {
                    continue;
                }
                let covering = chars.get(&c).cloned().unwrap_or_default();
                println!("   U+{:04X} {c} covered by {covering:?}", c as u32);
            }
            let job = egui::text::LayoutJob::single_section(
                (*text).to_owned(),
                egui::TextFormat {
                    font_id: egui::FontId::proportional(14.0),
                    ..Default::default()
                },
            );
            let galley = fonts.layout_job(job);
            for row in &galley.rows {
                let seq: Vec<String> =
                    row.glyphs.iter().map(|g| format!("{}@{:.0}", g.chr, g.pos.x)).collect();
                println!("   row: {}", seq.join(" "));
            }
        }
    });
}

/// The permanent half of the probe: the app's font stack
/// must cover the Arabic and Hebrew scripts (the P2-65
/// finding was that it covered neither, at all).
#[test]
fn app_stack_covers_arabic_and_hebrew() {
    let app = egui::Context::default();
    crate::theme::install_fonts(&app);
    let mut out = app.run_ui(egui::RawInput::default(), |_| {});
    out.textures_delta.clear();
    app.fonts_mut(|fonts| {
        let fid = egui::FontId::proportional(14.0);
        for c in "مرحبا שלום".chars().filter(|c| *c != ' ') {
            assert!(fonts.has_glyph(&fid, c), "U+{:04X} must be covered", c as u32);
        }
    });
}

#[test]
fn bidi_probe_report() {
    // The egui default stack (no app fonts): the baseline
    // the app's own installation is compared against.
    let plain = egui::Context::default();
    report(&plain, "egui defaults");
    // The app's real installation, exactly as startup runs it.
    let app = egui::Context::default();
    crate::theme::install_fonts(&app);
    report(&app, "app stack");
}
