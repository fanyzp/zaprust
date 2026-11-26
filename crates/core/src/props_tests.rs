//! Property-based tests for core invariants: theme adjustment,
//! poll vote conservation, URL extraction, and the crash report
//! redaction + round-trip. Proptest shrinks failures to a
//! minimal case.

use std::path::Path;

use crate::crash::CrashReport;
use crate::rich::apply_vote;
use crate::theme::ThemeId as BuiltinTheme;
use proptest::prelude::*;

proptest! {
    /// Adjusting with identity parameters returns every built-in
    /// theme unchanged.
    #[test]
    fn adjust_identity_is_exact(_seed in 0u8..8) {
        for theme in BuiltinTheme::ALL {
            let tokens = theme.tokens();
            prop_assert_eq!(tokens.adjust(1.0, 1.0), tokens);
        }
    }

    /// Out-of-range adjust parameters clamp to the same result
    /// as the range endpoints (no wraparound, no panic).
    #[test]
    fn adjust_clamps_out_of_range(
        sat in 1.5f32..100.0, con in 1.2f32..100.0,
    ) {
        let tokens = BuiltinTheme::Dark.tokens();
        prop_assert_eq!(tokens.adjust(sat, con), tokens.adjust(1.5, 1.2));
        prop_assert_eq!(
            tokens.adjust(-sat, -con),
            tokens.adjust(0.0, 0.8)
        );
    }

    /// Poll votes: any sequence of single-choice votes keeps the
    /// running total exactly accounted (switch = move one vote,
    /// retract = remove one, fresh = add one), and my selection
    /// never holds more than one option.
    #[test]
    fn single_choice_votes_conserve_totals(
        votes in prop::collection::vec(0usize..4, 1..24),
    ) {
        let mut options: Vec<(String, u32)> =
            ["A", "B", "C", "D"].iter().map(|s| (s.to_string(), 3)).collect();
        let mut expected: u32 = options.iter().map(|(_, v)| v).sum();
        let mut mine: Vec<usize> = Vec::new();
        for idx in votes {
            let was_mine = mine.contains(&idx);
            let had_any = !mine.is_empty();
            let (o, m) = apply_vote(&options, &mine, false, idx);
            options = o;
            mine = m;
            if was_mine {
                expected -= 1;
            } else if !had_any {
                expected += 1;
            }
            let total: u32 = options.iter().map(|(_, v)| v).sum();
            prop_assert_eq!(total, expected);
            prop_assert!(mine.len() <= 1);
        }
    }

    /// Crash reports: after redaction the home path is gone, and
    /// the report survives a JSON round-trip unchanged.
    #[test]
    fn crash_report_redacts_and_round_trips(
        message in ".{0,120}",
        user in "[a-z]{3,10}",
    ) {
        let home = format!("/home/{user}");
        let raw = format!("boom in {home}/secret/file.rs while {message}");
        let mut report = CrashReport::capture(env!("CARGO_PKG_VERSION"), raw, Some("src/lib.rs:1:1".to_owned()), "bt".to_owned());
        report.redact(Path::new(&home));
        prop_assert!(!report.panic_message.contains(&home));
        let json = serde_json::to_string(&report).expect("serializes");
        let back: CrashReport = serde_json::from_str(&json).expect("round-trip parses");
        prop_assert_eq!(back, report);
    }

    /// The first URL found in arbitrary text is a substring of
    /// that text and looks like a URL.
    #[test]
    fn first_url_is_a_substring(text in ".{0,200}") {
        if let Some(url) = crate::rich::first_url(&text) {
            prop_assert!(text.contains(&url));
            prop_assert!(url.starts_with("http"));
        }
    }
}
