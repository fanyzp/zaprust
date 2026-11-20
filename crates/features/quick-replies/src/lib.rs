//! Quick Replies, a native Zaprust feature.
//!
//! Canned replies for the composer: while the feature is enabled,
//! sending a message that starts with one of the triggers expands
//! it into the full text. Expansion happens in the app, on text the
//! user typed and chose to send; this module is the pure data and
//! expansion logic. The app owns the user's saved list and passes
//! it to [`expand`]; the defaults here only seed that list on
//! first run.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

/// The canned replies this feature ships: `(trigger, expansion)`.
pub const DEFAULT_REPLIES: [(&str, &str); 3] = [
    ("/thanks", "Thank you so much, really appreciate it!"),
    ("/omw", "On my way, give me about 10 minutes."),
    ("/docs", "The documents are in the shared folder; the link is pinned in this chat."),
];

/// One live quick reply, as the composer sees it. Serialized as
/// part of the user's saved list, which the app persists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickReply {
    /// The typed shortcut, e.g. `"/thanks"`. May be empty for a
    /// reply that only exists for one-tap insertion from the
    /// composer panel; such entries never expand.
    pub trigger: String,
    /// The full text the trigger expands into.
    pub body: String,
}

/// The default reply set as owned values.
#[must_use]
pub fn default_replies() -> Vec<QuickReply> {
    DEFAULT_REPLIES
        .iter()
        .map(|(trigger, body)| QuickReply {
            trigger: (*trigger).to_owned(),
            body: (*body).to_owned(),
        })
        .collect()
}

/// Expands a leading quick-reply trigger in `text`, if the text
/// starts with one of `replies`' triggers followed by a space or
/// end-of-text. Returns the text unchanged when nothing matches.
/// Only the given list is consulted; entries with an empty
/// trigger are skipped.
#[must_use]
pub fn expand(text: &str, replies: &[QuickReply]) -> String {
    for qr in replies {
        if qr.trigger.is_empty() {
            continue;
        }
        if let Some(rest) = text.strip_prefix(qr.trigger.as_str()) {
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                let rest = rest.trim_start();
                return if rest.is_empty() {
                    qr.body.clone()
                } else {
                    format!("{} {rest}", qr.body)
                };
            }
        }
    }
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_matches_trigger_boundaries() {
        let replies = default_replies();
        assert_eq!(expand("/omw", &replies), "On my way, give me about 10 minutes.");
        assert_eq!(expand("/omw all", &replies), "On my way, give me about 10 minutes. all");
        assert_eq!(expand("/omway", &replies), "/omway", "prefix alone must not expand");
        assert_eq!(expand("hello", &replies), "hello");
        assert_eq!(expand("/hi", &[]), "/hi", "no replies, no expansion");
    }

    #[test]
    fn expand_uses_the_callers_list_not_the_defaults() {
        // A list the user built themselves: their own entry
        // expands, and a built-in trigger they removed from
        // their list does not come back.
        let user_list = vec![QuickReply {
            trigger: "/brb".to_owned(),
            body: "Be right back, in a meeting".to_owned(),
        }];
        assert_eq!(expand("/brb", &user_list), "Be right back, in a meeting");
        assert_eq!(expand("/brb now", &user_list), "Be right back, in a meeting now");
        assert_eq!(
            expand("/thanks", &user_list),
            "/thanks",
            "defaults must not leak into a caller-provided list"
        );
    }

    #[test]
    fn expand_skips_entries_without_a_trigger() {
        let replies = vec![QuickReply { trigger: String::new(), body: "Panel only".to_owned() }];
        assert_eq!(expand("Panel only", &replies), "Panel only");
        assert_eq!(expand("anything", &replies), "anything");
    }
}
