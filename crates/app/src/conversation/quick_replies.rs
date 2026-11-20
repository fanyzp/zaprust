//! Native quick replies: the user's saved list of canned
//! replies for the composer.
//!
//! There is exactly one list. It lives in its own file,
//! `<data dir>/quick-replies.json`, written through the same
//! atomic-write helper the rest of the app state uses, and it
//! holds the feature crate's [`QuickReply`] entries: the crate
//! owns the type, the expansion logic and the built-in set,
//! this module owns persistence and the composer panel state.
//! The list is loaded once and cached; every edit saves
//! immediately.
//!
//! On first run the crate's built-in set seeds the list. After
//! that the saved list is the only source, for the composer
//! panel (which inserts an entry's body into the draft) and
//! for trigger expansion on send alike. Files written by older
//! versions, a plain JSON array of strings, are migrated on
//! load: each string becomes a body-only entry, insertable
//! from the panel but with no trigger to expand.
//!
//! The composer panel's open state and the editor's draft also
//! live here (process-wide, like the list itself) because the
//! `App` struct is shared with other screens and this feature
//! is local to the composer.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use quick_replies::QuickReply;

fn store_path() -> PathBuf {
    backend::data_dir().join("quick-replies.json")
}

fn store() -> &'static Mutex<Vec<QuickReply>> {
    static STORE: OnceLock<Mutex<Vec<QuickReply>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(load()))
}

fn load() -> Vec<QuickReply> {
    let stored =
        crate::extras::read_json_file_pub(&store_path()).and_then(|text| decode_list(&text));
    seed(stored)
}

/// Decodes a saved list. Current files hold `QuickReply`
/// objects; older files hold plain strings, which migrate to
/// body-only entries. Returns `None` when the text is neither.
fn decode_list(text: &str) -> Option<Vec<QuickReply>> {
    if let Ok(list) = serde_json::from_str::<Vec<QuickReply>>(text) {
        return Some(list);
    }
    let strings = serde_json::from_str::<Vec<String>>(text).ok()?;
    Some(strings.into_iter().map(|body| QuickReply { trigger: String::new(), body }).collect())
}

/// The effective list: the saved one when there is one (an
/// explicitly emptied list stays empty), the feature crate's
/// built-in set as the first-run seed otherwise.
fn seed(stored: Option<Vec<QuickReply>>) -> Vec<QuickReply> {
    stored.unwrap_or_else(quick_replies::default_replies)
}

fn save(items: &[QuickReply]) {
    if let Ok(text) = serde_json::to_string(items) {
        crate::extras::write_json_file_pub(&store_path(), &text);
    }
}

/// A snapshot of the current quick replies, in display order.
/// This is the one list every consumer reads: the composer
/// panel renders it and trigger expansion runs over it.
pub(crate) fn items() -> Vec<QuickReply> {
    store().lock().map(|m| m.clone()).unwrap_or_default()
}

/// Parses editor text into an entry. A leading `/word` followed
/// by more text becomes the trigger, the rest the body; anything
/// else is a body-only entry for the panel.
fn parse_entry(text: &str) -> QuickReply {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('/') {
        if let Some((head, tail)) = rest.split_once(char::is_whitespace) {
            let body = tail.trim();
            if !head.is_empty() && !body.is_empty() {
                return QuickReply { trigger: format!("/{head}"), body: body.to_owned() };
            }
        }
    }
    QuickReply { trigger: String::new(), body: text.to_owned() }
}

/// Appends a reply parsed from the editor text (see
/// [`parse_entry`]); empty text and exact duplicates are
/// ignored. Saves the list.
pub(crate) fn add(text: &str) {
    let entry = parse_entry(text);
    if entry.body.is_empty() {
        return;
    }
    if let Ok(mut m) = store().lock() {
        if !m.contains(&entry) {
            m.push(entry);
            save(&m);
        }
    }
}

/// Removes the reply at `index` and saves the list.
pub(crate) fn remove(index: usize) {
    if let Ok(mut m) = store().lock() {
        if index < m.len() {
            m.remove(index);
            save(&m);
        }
    }
}

/// Composer UI state for the quick replies panel and its editor.
#[derive(Default)]
struct UiState {
    panel_open: bool,
    editor_open: bool,
    editor_draft: String,
}

fn ui_state() -> &'static Mutex<UiState> {
    static UI: OnceLock<Mutex<UiState>> = OnceLock::new();
    UI.get_or_init(|| Mutex::new(UiState::default()))
}

/// Whether the quick replies panel above the composer is open.
pub(crate) fn panel_open() -> bool {
    ui_state().lock().is_ok_and(|s| s.panel_open)
}

/// Opens or closes the quick replies panel.
pub(crate) fn set_panel_open(open: bool) {
    if let Ok(mut s) = ui_state().lock() {
        s.panel_open = open;
    }
}

/// Whether the quick replies editor popover is open.
pub(crate) fn editor_open() -> bool {
    ui_state().lock().is_ok_and(|s| s.editor_open)
}

/// Opens or closes the quick replies editor popover.
pub(crate) fn set_editor_open(open: bool) {
    if let Ok(mut s) = ui_state().lock() {
        s.editor_open = open;
    }
}

/// The editor's in-progress new-reply text.
pub(crate) fn editor_draft() -> String {
    ui_state().lock().map(|s| s.editor_draft.clone()).unwrap_or_default()
}

/// Replaces the editor's in-progress new-reply text.
pub(crate) fn set_editor_draft(text: String) {
    if let Ok(mut s) = ui_state().lock() {
        s.editor_draft = text;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_reads_trigger_entries() {
        let list = decode_list(r#"[{"trigger":"/brb","body":"Be right back"}]"#)
            .expect("current format decodes");
        assert_eq!(
            list,
            vec![QuickReply { trigger: "/brb".to_owned(), body: "Be right back".to_owned() }]
        );
    }

    #[test]
    fn decode_migrates_the_legacy_string_list() {
        let list = decode_list(r#"["On my way","Thanks!"]"#).expect("legacy format decodes");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].body, "On my way");
        assert!(list.iter().all(|qr| qr.trigger.is_empty()));
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode_list("not json").is_none());
        assert!(decode_list("[1,2]").is_none());
    }

    #[test]
    fn seed_only_fills_a_missing_list() {
        let user =
            vec![QuickReply { trigger: "/brb".to_owned(), body: "Be right back".to_owned() }];
        assert_eq!(seed(Some(user.clone())), user);
        assert_eq!(seed(Some(Vec::new())), Vec::new(), "an explicitly emptied list stays empty");
        assert_eq!(seed(None), quick_replies::default_replies());
    }

    #[test]
    fn parse_entry_splits_a_leading_trigger() {
        assert_eq!(
            parse_entry("/sbr Sebentar, lagi di jalan"),
            QuickReply { trigger: "/sbr".to_owned(), body: "Sebentar, lagi di jalan".to_owned() }
        );
        assert_eq!(
            parse_entry("Siap, meluncur"),
            QuickReply { trigger: String::new(), body: "Siap, meluncur".to_owned() }
        );
    }

    #[test]
    fn user_entries_expand_through_the_effective_list() {
        // The regression test for the split implementation: a
        // reply the user saved must drive expansion, and the
        // built-in set must not reappear alongside it.
        let stored = decode_list(r#"[{"trigger":"/brb","body":"Be right back"}]"#)
            .expect("stored list decodes");
        let effective = seed(Some(stored));
        assert_eq!(quick_replies::expand("/brb", &effective), "Be right back");
        assert_eq!(quick_replies::expand("/thanks", &effective), "/thanks");
    }
}
