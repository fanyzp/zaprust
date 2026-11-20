//! Debug mode support: redaction, masking, the structured
//! trace ring, and the debug-bundle builder.
//!
//! Hard rules (enforced by tests): nothing in this module ever
//! emits session keys, API keys, pairing codes, tokens, or full
//! phone numbers. Message bodies live only in the local trace
//! ring's `body` field (visible in the inspector while debug
//! mode is on) and reach an exported bundle ONLY when the user
//! explicitly opts in.

use std::collections::VecDeque;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

static DEBUG_ON: AtomicBool = AtomicBool::new(false);

/// Turns debug mode on/off for this process. The permission
/// gate and every capability check ignore this flag, a test in
/// the agent crate locks that parity in.
pub fn set_debug(on: bool) {
    DEBUG_ON.store(on, Ordering::Relaxed);
}

/// Masks a phone-like digit run, keeping only the last 2 digits:
/// `+1 555-013-4472` → `+**72`. Runs shorter than 7 digits are
/// returned unchanged (they are not phone numbers).
#[must_use]
pub(crate) fn mask_phones(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut digits = String::new();
    let mut run = String::new();
    let flush = |run: &str, digits: &str, out: &mut String| {
        if digits.len() >= 7 {
            out.push_str("***");
            out.push_str(&digits[digits.len() - 2..]);
        } else {
            out.push_str(run);
        }
    };
    for ch in text.chars() {
        if ch.is_ascii_digit() || matches!(ch, '+' | '-' | ' ') && !run.is_empty() {
            run.push(ch);
            if ch.is_ascii_digit() {
                digits.push(ch);
            }
        } else {
            if !run.is_empty() {
                flush(&run, &digits, &mut out);
                run.clear();
                digits.clear();
            }
            out.push(ch);
        }
    }
    if !run.is_empty() {
        flush(&run, &digits, &mut out);
    }
    out
}

/// Byte offset of a case-insensitive "bearer " prefix in `text`.
fn find_bearer(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    let needle = b"bearer ";
    if b.len() < needle.len() {
        return None;
    }
    (0..=b.len() - needle.len()).find(|&i| b[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

/// Redacts secrets from a log line or summary. Applied to every
/// line that reaches a log file, the trace ring, or a bundle.
#[must_use]
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    // sk-... API keys (OpenAI-style) and Bearer tokens.
    while let Some(idx) = rest.find("sk-") {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx..];
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(tail.len());
        if end >= 8 {
            out.push_str("sk-***");
            rest = &tail[end..];
        } else {
            out.push_str("sk-");
            rest = &tail[3..];
        }
    }
    out.push_str(rest);
    // "Bearer <token>": mask the token itself, not just the prefix.
    let mut masked_bearer = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(idx) = find_bearer(rest) {
        let token_start = idx + "bearer ".len();
        let tail = &rest[token_start..];
        let token_end = tail
            .find(|c: char| {
                !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '=' | '~'))
            })
            .unwrap_or(tail.len());
        masked_bearer.push_str(&rest[..token_start]);
        masked_bearer.push_str("***");
        rest = &tail[token_end..];
    }
    masked_bearer.push_str(rest);
    let out = masked_bearer;
    // GitHub / AWS style tokens.
    let mut out = out;
    for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_", "AKIA", "ASIA", "AIza"] {
        let mut rebuilt = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(idx) = rest.find(prefix) {
            let tail = &rest[idx..];
            let end =
                tail.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(tail.len());
            rebuilt.push_str(&rest[..idx]);
            rebuilt.push_str(prefix);
            rebuilt.push_str("***");
            rest = &tail[end..];
        }
        rebuilt.push_str(rest);
        out = rebuilt;
    }
    // pair_code=XXXX / api_key=... / token=... assignments,
    // wherever they appear (log lines, JSON members, prose).
    let mut masked = out.clone();
    for key in ["pair_code=", "api_key=", "apikey=", "token=", "password="] {
        let mut search_from = 0;
        while let Some(pos) = masked[search_from..].to_lowercase().find(key) {
            let value_start = search_from + pos + key.len();
            let value_end = masked[value_start..]
                .find(|c: char| {
                    !(c.is_ascii_alphanumeric()
                        || matches!(c, '-' | '_' | '.' | '+' | '/' | '=' | '~'))
                })
                .map_or(masked.len(), |e| value_start + e);
            masked.replace_range(value_start..value_end, "***");
            search_from = value_start + 3;
        }
    }
    mask_phones(&masked)
}

/// One structured trace entry. `body` is the optional message
/// body: local-only, exported only on explicit user opt-in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEntry {
    /// Monotonic sequence number.
    pub seq: u64,
    /// Milliseconds since the flow (app start in debug mode).
    pub at_ms: u64,
    /// Pipeline stage: backend/command/archive/ui/receipt/gate/
    /// scheduler/memory/feature.
    pub stage: String,
    /// Short redacted summary.
    pub summary: String,
    /// Stage duration when measured.
    pub duration_ms: Option<u64>,
    /// Message body (local inspector only; see module rules).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// Bounded in-memory ring of trace entries (the inspector and
/// the tracing file writer feed from the same events).
#[derive(Debug)]
pub struct TraceRing {
    entries: VecDeque<TraceEntry>,
    cap: usize,
    next_seq: u64,
}

impl Default for TraceRing {
    fn default() -> Self {
        Self::new(500)
    }
}

impl TraceRing {
    /// Creates a ring holding at most `cap` entries.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self { entries: VecDeque::new(), cap, next_seq: 1 }
    }

    /// Records an entry. The summary is redacted here, at the
    /// single choke point, so no caller can leak a secret.
    pub fn push(
        &mut self,
        at_ms: u64,
        stage: &str,
        summary: &str,
        duration_ms: Option<u64>,
        body: Option<String>,
    ) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push_back(TraceEntry {
            seq,
            at_ms,
            stage: stage.to_owned(),
            summary: redact(summary),
            duration_ms,
            body,
        });
        while self.entries.len() > self.cap {
            self.entries.pop_front();
        }
        seq
    }

    /// All entries, oldest first.
    #[must_use]
    pub fn entries(&self) -> Vec<TraceEntry> {
        self.entries.iter().cloned().collect()
    }

    /// JSON export of the timeline. Bodies are included only
    /// when `include_bodies` is explicitly true.
    #[must_use]
    pub fn export_json(&self, include_bodies: bool) -> String {
        let entries: Vec<TraceEntry> = self
            .entries
            .iter()
            .map(|e| {
                let mut e = e.clone();
                if !include_bodies {
                    e.body = None;
                } else if let Some(b) = &e.body {
                    e.body = Some(redact(b));
                }
                e
            })
            .collect();
        serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_owned())
    }
}

/// Inputs for a debug bundle.
pub struct BundleInputs {
    /// Timeline JSON (already body-filtered by the caller).
    pub timeline_json: String,
    /// (file name, contents) of debug logs, redacted again here.
    pub logs: Vec<(String, String)>,
    /// App/build info text (same source as the About page).
    pub build_info: String,
    /// Latest crash report (already redacted at write time).
    pub crash_report: Option<String>,
}

/// Builds the debug bundle zip in memory. Every text member is
/// passed through [`redact`] once more at this boundary,
/// defense in depth for anything a caller missed.
#[must_use]
pub fn build_bundle(inputs: &BundleInputs) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let mut add = |name: &str, text: &str| {
            let _ = zip.start_file(name, opts);
            let _ = zip.write_all(redact(text).as_bytes());
        };
        add("timeline.json", &inputs.timeline_json);
        add("build-info.txt", &inputs.build_info);
        for (name, text) in &inputs.logs {
            add(&format!("logs/{name}"), text);
        }
        if let Some(crash) = &inputs.crash_report {
            add("crash-report.txt", crash);
        }
        add(
            "README.txt",
            "Zaprust debug bundle. Message bodies are excluded unless the \
             exporter explicitly opted in. Attach this file manually to a \
             GitHub issue if you choose to report the problem.",
        );
        let _ = zip.finish();
    }
    buf.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_masks_base64_bearer_suffixes_and_newer_token_shapes() {
        let out = redact("Authorization: Bearer abc/def== end");
        assert!(!out.contains("abc/def"), "{out}");
        assert!(!out.contains("def=="), "{out}");
        let gh = redact("key github_pat_11ABCDEFG0xyz here");
        assert!(!gh.contains("11ABCDEFG0xyz"), "{gh}");
        let aws = redact("temp ASIAIOSFODNN7EXAMPLE end");
        assert!(!aws.contains("ASIAIOSFODNN7EXAMPLE"), "{aws}");
    }

    #[test]
    fn redact_masks_bearer_and_provider_tokens_fully() {
        let line = "Authorization: Bearer abcDEF123456789 tail ghp_ABCDEFGHIJ1234567890 AKIAIOSFODNN7EXAMPLE";
        let out = redact(line);
        assert!(!out.contains("abcDEF123456789"), "{out}");
        assert!(!out.contains("ghp_ABCDEFGHIJ"), "{out}");
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"), "{out}");
        assert!(out.contains("Bearer ***"), "{out}");
    }

    #[test]
    fn redact_removes_api_keys_pair_codes_and_phones() {
        let line = "auth sk-AbC123_xYz-987654 done pair_code=ABCD2345 token=zzz \
                    from +1 555-013-4472 ok";
        let out = redact(line);
        assert!(!out.contains("sk-AbC123"), "key leaked: {out}");
        assert!(!out.contains("ABCD2345"), "pair code leaked: {out}");
        assert!(!out.contains("555-013"), "phone leaked: {out}");
        assert!(out.contains("***72"), "mask keeps last 2 digits: {out}");
    }

    #[test]
    fn short_digit_runs_are_not_phones() {
        assert_eq!(mask_phones("code 1234 and 42"), "code 1234 and 42");
    }

    #[test]
    fn ring_is_ordered_and_bounded() {
        let mut ring = TraceRing::new(3);
        for i in 0..5u64 {
            ring.push(i * 10, "backend", &format!("event {i}"), None, None);
        }
        let entries = ring.entries();
        assert_eq!(entries.len(), 3);
        assert!(entries.windows(2).all(|w| w[0].seq < w[1].seq));
        assert!(entries.windows(2).all(|w| w[0].at_ms <= w[1].at_ms));
        assert_eq!(entries[0].summary, "event 2");
    }

    #[test]
    fn ring_summaries_are_redacted_at_push() {
        let mut ring = TraceRing::new(10);
        ring.push(0, "backend", "key sk-SuperSecretKey123 used", None, None);
        assert!(!ring.entries()[0].summary.contains("SuperSecret"));
    }

    #[test]
    fn export_excludes_bodies_by_default() {
        let mut ring = TraceRing::new(10);
        ring.push(0, "message", "incoming text", None, Some("hello world body".to_owned()));
        let without = ring.export_json(false);
        assert!(!without.contains("hello world body"));
        let with = ring.export_json(true);
        assert!(with.contains("hello world body"));
    }

    #[test]
    fn bundle_contains_redacted_members() {
        let inputs = BundleInputs {
            timeline_json: "[{\"summary\":\"pair_code=ABCD2345\"}]".to_owned(),
            logs: vec![(
                "zaprust.log".to_owned(),
                "login sk-MySecretKey999 from 15550134472".to_owned(),
            )],
            build_info: "zaprust 0.20.0".to_owned(),
            crash_report: Some("panic at src/lib.rs".to_owned()),
        };
        let bytes = build_bundle(&inputs);
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let names: Vec<String> = zip.file_names().map(ToOwned::to_owned).collect();
        assert!(names.iter().any(|n| n == "timeline.json"));
        assert!(names.iter().any(|n| n == "logs/zaprust.log"));
        assert!(names.iter().any(|n| n == "crash-report.txt"));
        for name in &names {
            let mut f = zip.by_name(name).unwrap();
            let mut text = String::new();
            std::io::Read::read_to_string(&mut f, &mut text).unwrap();
            assert!(!text.contains("MySecretKey999"), "{name} leaked a key");
            assert!(!text.contains("ABCD2345"), "{name} leaked a pair code");
            assert!(!text.contains("15550134472"), "{name} leaked a phone");
        }
    }
}
