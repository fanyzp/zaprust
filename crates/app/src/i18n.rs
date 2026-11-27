//! Interface language: user-facing strings live in locale files
//! (`locales/<lang>.toml`), never hardcoded in draw code.
//!
//! English (`en`) is the source locale. Lookup falls back to
//! English for any key a locale has not translated yet, so a
//! partial locale is safe to ship; Indonesian currently covers
//! every key. See docs/I18N.md for how to add or extend a
//! language.
//!
//! Placeholders use `{name}` form and are filled by [`tf`].

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

const EN_TOML: &str = include_str!("../locales/en.toml");
const ID_TOML: &str = include_str!("../locales/id.toml");

/// The languages the picker offers: (locale id, display name,
/// completeness note). The names are the languages' own names for
/// themselves where sensible.
pub(crate) const LOCALES: &[(&str, &str)] = &[("en", "English"), ("id", "Indonesian")];

fn parse(source: &str) -> HashMap<String, String> {
    // A malformed bundled locale is a build-time bug; the tests
    // parse both files, and this fallback keeps the app alive with
    // an empty map (which falls back to keys/English) regardless.
    let Ok(value) = source.parse::<toml::Table>() else {
        return HashMap::new();
    };
    value.into_iter().filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_owned()))).collect()
}

fn en_map() -> &'static HashMap<String, String> {
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    MAP.get_or_init(|| parse(EN_TOML))
}

fn id_map() -> &'static HashMap<String, String> {
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    MAP.get_or_init(|| parse(ID_TOML))
}

fn current() -> &'static RwLock<&'static str> {
    static CURRENT: OnceLock<RwLock<&'static str>> = OnceLock::new();
    CURRENT.get_or_init(|| RwLock::new("en"))
}

/// Sets the active locale (unknown ids fall back to English).
pub(crate) fn set_locale(id: &str) {
    let id = if LOCALES.iter().any(|(l, _)| *l == id) { id } else { "en" };
    // The static strs are the canonical ids from LOCALES.
    let canonical: &'static str = LOCALES.iter().find(|(l, _)| *l == id).map_or("en", |(l, _)| l);
    if let Ok(mut guard) = current().write() {
        *guard = canonical;
    }
}

/// The active locale id ("en" or "id").
#[must_use]
pub(crate) fn locale() -> &'static str {
    current().read().map_or("en", |g| *g)
}

fn map_for(id: &str) -> &'static HashMap<String, String> {
    match id {
        "id" => id_map(),
        _ => en_map(),
    }
}

/// Looks up a UI string: active locale first, English fallback,
/// and the key itself as the last resort (visible in development,
/// never a panic).
#[must_use]
pub(crate) fn t(key: &str) -> String {
    let active = locale();
    if active != "en" {
        if let Some(v) = map_for(active).get(key) {
            return v.clone();
        }
    }
    en_map().get(key).cloned().unwrap_or_else(|| key.to_owned())
}

/// [`t`] with one named placeholder filled (`{name}` in the
/// string). Additional placeholders can be filled by chaining
/// `replace` on the result.
#[must_use]
pub(crate) fn tf(key: &str, name: &str, value: &str) -> String {
    t(key).replace(&format!("{{{name}}}"), value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_locale_files_parse_and_english_is_substantial() {
        assert!(en_map().len() > 60, "en is the complete source locale: {}", en_map().len());
        assert!(!id_map().is_empty(), "the Indonesian starter has content");
    }

    #[test]
    fn indonesian_keys_are_a_subset_of_english_keys() {
        for key in id_map().keys() {
            assert!(en_map().contains_key(key), "id key {key} has no en source string");
        }
    }

    #[test]
    fn placeholders_match_between_locales() {
        fn placeholders(s: &str) -> Vec<String> {
            let mut out = Vec::new();
            let mut rest = s;
            while let Some(open) = rest.find('{') {
                if let Some(close) = rest[open..].find('}') {
                    out.push(rest[open..=open + close].to_owned());
                    rest = &rest[open + close + 1..];
                } else {
                    break;
                }
            }
            out.sort();
            out
        }
        for (key, id_value) in id_map() {
            let en_value = &en_map()[key];
            assert_eq!(
                placeholders(id_value),
                placeholders(en_value),
                "placeholder mismatch for {key}"
            );
        }
    }

    #[test]
    fn nav_title_keys_resolve_to_real_strings() {
        // The rail labels and panel headers look these keys up; a
        // missing entry would render the raw key as the title
        // (this is how "nav.calls.entries" once shipped).
        const NAV_TITLE_KEYS: &[&str] = &[
            "nav.chats",
            "nav.calls",
            "nav.channels",
            "nav.groups",
            "nav.communities",
            "nav.starred",
            "nav.archived",
            "nav.settings",
            "nav.updates",
        ];
        for key in NAV_TITLE_KEYS {
            let value = en_map().get(*key).unwrap_or_else(|| panic!("{key} missing from en"));
            assert_ne!(*value, *key, "{key} resolves to itself");
            assert!(!value.is_empty(), "{key} is empty");
        }
    }

    #[test]
    fn locale_switching_resolves_translations_and_unknown_keys() {
        // Resolve everything first and restore the locale
        // before asserting, so a failure here can never leak
        // the Indonesian locale into other tests sharing this
        // process (the locale is one global).
        set_locale("id");
        let shared = id_map().keys().next().unwrap().clone();
        let id_value = t(&shared);
        let unknown = t("no.such.key");
        set_locale("en");
        let en_value = t(&shared);
        assert_eq!(id_value, id_map()[&shared], "a translated key resolves to Indonesian");
        assert_eq!(unknown, "no.such.key", "unknown keys surface as themselves, never panic");
        assert_eq!(en_value, en_map()[&shared], "English resolves the source string");
        // The English fallback for a key the active locale has
        // not translated cannot be triggered from the bundled
        // files now that the Indonesian starter is complete;
        // it stays covered by the t() lookup order itself.
    }
}
