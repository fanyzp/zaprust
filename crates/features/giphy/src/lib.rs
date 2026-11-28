//! GIF Search (GIPHY), a native Zaprust feature.
//!
//! Registers the **GIF** tab of the composer's media picker. The tab
//! shows trending GIFs on open and searches on demand, using the
//! GIPHY API with a key drawn from the host's central credential
//! pool (slot `giphy_api_key`), this feature deliberately has **no
//! key field of its own**; without a key the tab renders the
//! "needs a key" card that routes the user to Settings.
//!
//! Live GIPHY calls are **untested in CI** (no key is available to
//! the test suite); URL building, response parsing, and the key-state
//! decision are unit-tested, and request behavior is verified
//! against a local mock server.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use core::credentials::KeyPool;

use serde_json::Value;

/// The picker tab id this feature registers.
pub const TAB_ID: &str = "gif";

/// The credential slot this feature draws its key from.
pub const KEY_SLOT: &str = "giphy_api_key";

/// The credential slot this feature uses, declared with the
/// central credential store at startup.
#[must_use]
pub fn credential_slot() -> core::credentials::CredentialSlot {
    core::credentials::CredentialSlot {
        id: KEY_SLOT.to_owned(),
        label: "GIPHY API key".to_owned(),
        kind: core::credentials::CredentialKind::ApiKey,
    }
}

/// GIPHY API origin.
pub const API_BASE: &str = "https://api.giphy.com/v1/gifs";

/// One GIF result, reduced to what the picker renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GifItem {
    /// GIPHY id.
    pub id: String,
    /// Alt/title text.
    pub title: String,
    /// Static preview image URL (picker thumbnail).
    pub preview_url: String,
    /// The animated GIF URL (sent when the item is clicked).
    pub gif_url: String,
    /// The MP4 rendition of the same clip, when GIPHY offers one.
    /// Live sending needs it: WhatsApp GIF-style messages are MP4
    /// videos with the GIF playback flag, not GIF files.
    pub mp4_url: Option<String>,
}

/// What the GIF tab should render, decided from credential state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabState {
    /// No key configured: show the "needs a key" card.
    NeedsKey,
    /// The configured key was rejected (401/403): same card, with a
    /// "key rejected" note.
    KeyRejected,
    /// A key exists: show search + grid.
    Ready,
}

/// Decides the tab state from the pool contents. `rejected` is set
/// by the host after a failed request with HTTP 401/403.
#[must_use]
pub fn tab_state(pool: &KeyPool, rejected: bool) -> TabState {
    if pool.is_empty() {
        TabState::NeedsKey
    } else if rejected {
        TabState::KeyRejected
    } else {
        TabState::Ready
    }
}

/// Percent-encodes a query parameter value.
#[must_use]
pub(crate) fn encode_query(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Builds the trending endpoint URL.
#[must_use]
pub(crate) fn trending_url(api_key: &str, limit: u32, rating: &str) -> String {
    format!(
        "{API_BASE}/trending?api_key={}&limit={limit}&rating={}",
        encode_query(api_key),
        encode_query(rating)
    )
}

/// Builds the search endpoint URL.
#[must_use]
pub(crate) fn search_url(api_key: &str, query: &str, limit: u32, rating: &str) -> String {
    format!(
        "{API_BASE}/search?api_key={}&q={}&limit={limit}&rating={}",
        encode_query(api_key),
        encode_query(query),
        encode_query(rating)
    )
}

/// Parses a GIPHY list response into picker items. Items missing a
/// preview or GIF URL are dropped.
#[must_use]
pub(crate) fn parse_gifs(body: &Value) -> Vec<GifItem> {
    let Some(items) = body["data"].as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let images = &item["images"];
            let preview = images["fixed_width_small_still"]["url"].as_str()?;
            let gif = images["fixed_width"]["url"].as_str()?;
            let mp4 = images["fixed_width"]["mp4"].as_str().map(str::to_owned);
            Some(GifItem {
                id: item["id"].as_str().unwrap_or_default().to_string(),
                title: item["title"].as_str().unwrap_or_default().to_string(),
                preview_url: preview.to_string(),
                gif_url: gif.to_string(),
                mp4_url: mp4,
            })
        })
        .collect()
}

/// A fetch failure: HTTP status when the provider answered.
#[derive(Debug)]
pub enum GiphyError {
    /// No key in the pool.
    NoKey,
    /// Provider answered with this HTTP status.
    Http(u16),
    /// Transport/parse failure. The string is built from a
    /// reqwest error stripped of its URL (see [`transport_error`]),
    /// because the request URL carries the API key as a query
    /// parameter and must never reach an error message.
    Transport(String),
    /// A media URL taken from a provider response failed the
    /// shared public-address validation (see [`fetch_preview`]),
    /// so nothing was fetched.
    UrlRejected,
    /// A media response exceeded the download size cap.
    TooLarge,
}

/// Converts a reqwest error into a transport error with the
/// request URL stripped (SEC-09). reqwest errors otherwise embed
/// the full URL ("error sending request for url (...)"), which for
/// this feature contains `api_key=<the user's GIPHY key>`; that
/// string is shown to the user and pasted into bug reports.
/// `Error::without_url` exists in reqwest precisely for
/// key-in-query APIs like this one. EVERY reqwest error in this
/// feature goes through here before it is stringified.
fn transport_error(e: reqwest::Error) -> GiphyError {
    GiphyError::Transport(e.without_url().to_string())
}

impl std::fmt::Display for GiphyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoKey => write!(f, "no GIPHY API key configured"),
            Self::Http(s) => write!(f, "GIPHY returned HTTP {s}"),
            Self::Transport(m) => write!(f, "GIPHY request failed: {m}"),
            Self::UrlRejected => write!(f, "GIPHY media URL failed safety validation"),
            Self::TooLarge => write!(f, "GIPHY media exceeds the download size limit"),
        }
    }
}

impl std::error::Error for GiphyError {}

fn fetch(
    url_for: impl Fn(&str) -> String,
    pool: &mut KeyPool,
    now: u64,
) -> Result<Vec<GifItem>, GiphyError> {
    let Some(key) = pool.acquire(now).map(ToString::to_string) else {
        return Err(GiphyError::NoKey);
    };
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(transport_error)?;
    let resp = client.get(url_for(&key)).send().map_err(transport_error)?;
    let status = resp.status().as_u16();
    pool.report_http_status(&key, status, now);
    if status / 100 != 2 {
        return Err(GiphyError::Http(status));
    }
    let body: Value = resp.json().map_err(transport_error)?;
    Ok(parse_gifs(&body))
}

/// Fetches trending GIFs with a pooled key.
///
/// # Errors
/// [`GiphyError`] on missing key, HTTP errors, or transport failure.
pub fn trending(
    pool: &mut KeyPool,
    limit: u32,
    rating: &str,
    now: u64,
) -> Result<Vec<GifItem>, GiphyError> {
    fetch(|key| trending_url(key, limit, rating), pool, now)
}

/// Searches GIFs with a pooled key.
///
/// # Errors
/// [`GiphyError`] on missing key, HTTP errors, or transport failure.
pub fn search(
    pool: &mut KeyPool,
    query: &str,
    limit: u32,
    rating: &str,
    now: u64,
) -> Result<Vec<GifItem>, GiphyError> {
    fetch(|key| search_url(key, query, limit, rating), pool, now)
}

/// Resolves the fetch parameters from stored values (validated
/// against the same ranges the old manifest schema declared):
/// `(result_limit, rating)`.
#[must_use]
pub fn fetch_params(values: &serde_json::Map<String, serde_json::Value>) -> (u32, String) {
    let limit = values
        .get("result_limit")
        .and_then(serde_json::Value::as_u64)
        .filter(|n| (1..=50).contains(n))
        .unwrap_or(24) as u32;
    let rating = match values.get("rating").and_then(|v| v.as_str()) {
        Some(r @ ("g" | "pg" | "pg-13" | "r")) => r.to_owned(),
        _ => "g".to_owned(),
    };
    (limit, rating)
}

/// Download cap for one preview still.
const MAX_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;

/// Download cap for one MP4 rendition (larger than a still, still
/// a hard bound instead of an unbounded in-memory buffer).
const MAX_MP4_BYTES: u64 = 16 * 1024 * 1024;

/// Downloads one preview image (bytes; the host decodes/uploads it
/// as a texture).
///
/// # Errors
/// [`GiphyError::UrlRejected`] when the URL fails safety
/// validation, [`GiphyError::TooLarge`] past the size cap, or a
/// transport failure or non-2xx status.
pub fn fetch_preview(url: &str) -> Result<Vec<u8>, GiphyError> {
    fetch_media(url, MAX_PREVIEW_BYTES)
}

/// Downloads one MP4 rendition for a live GIF send (bytes; the
/// host forwards them to the backend as the message video).
///
/// # Errors
/// Same as [`fetch_preview`], with the larger MP4 size cap.
pub fn fetch_mp4(url: &str) -> Result<Vec<u8>, GiphyError> {
    fetch_media(url, MAX_MP4_BYTES)
}

/// Builds the media client for one fetch hop: the hop's URL is
/// validated and resolved once through `core::net`, and the
/// client is pinned to exactly those addresses, so the name is
/// never resolved a second time at connect and a rebinding answer
/// cannot slip a private address past the check (SEC-F05).
/// IP-literal hosts need no pin; there is no name to rebind.
fn pinned_media_client(url: &str) -> Result<reqwest::blocking::Client, GiphyError> {
    let target = core::net::validated_target(url).ok_or(GiphyError::UrlRejected)?;
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .ok_or(GiphyError::UrlRejected)?;
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none());
    if host.parse::<std::net::IpAddr>().is_err() {
        builder = builder.resolve_to_addrs(&host, &target.addrs);
    }
    builder.build().map_err(transport_error)
}

/// Fetches media bytes from a URL that came out of a provider
/// response, hardened per security audit SEC-F02/SEC-F03 with the
/// same pattern as the link-preview fetch: the URL must pass the
/// shared public-address validation (`core::net`), redirects are
/// followed manually so every hop is re-validated and pinned to
/// its validated addresses (SEC-F05), and the body read stops at
/// `max_bytes` instead of buffering without a cap. Without this, a
/// crafted provider response could make the app request internal
/// network addresses or download without bound.
fn fetch_media(url: &str, max_bytes: u64) -> Result<Vec<u8>, GiphyError> {
    use std::io::Read;
    let mut current = url.to_owned();
    for _ in 0..=3 {
        let client = pinned_media_client(&current)?;
        let mut resp = client.get(&current).send().map_err(transport_error)?;
        if resp.status().is_redirection() {
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
                .ok_or(GiphyError::Http(resp.status().as_u16()))?;
            let next = reqwest::Url::parse(&current)
                .and_then(|base| base.join(&location))
                .map_err(|e| GiphyError::Transport(e.to_string()))?;
            current = next.to_string();
            continue;
        }
        let status = resp.status().as_u16();
        if status / 100 != 2 {
            return Err(GiphyError::Http(status));
        }
        // Read one byte past the cap so an oversized body is
        // detected and rejected, never silently truncated.
        let mut buf = Vec::new();
        resp.by_ref()
            .take(max_bytes + 1)
            .read_to_end(&mut buf)
            .map_err(|e| GiphyError::Transport(e.to_string()))?;
        if buf.len() as u64 > max_bytes {
            return Err(GiphyError::TooLarge);
        }
        return Ok(buf);
    }
    Err(GiphyError::Transport("too many redirects".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    const FIXTURE: &str = r#"{
        "data": [
            {"id": "abc", "title": "Cat wave",
             "images": {"fixed_width_small_still": {"url": "https://media.example/abc_s.gif"},
                        "fixed_width": {"url": "https://media.example/abc.gif",
                                        "mp4": "https://media.example/abc.mp4"}}},
            {"id": "broken", "title": "No images", "images": {}},
            {"id": "def", "title": "Dog nod",
             "images": {"fixed_width_small_still": {"url": "https://media.example/def_s.gif"},
                        "fixed_width": {"url": "https://media.example/def.gif"}}}
        ]
    }"#;

    #[test]
    fn urls_are_built_with_encoding_and_rating() {
        let url = search_url("KEY 123", "cat & dog", 8, "pg");
        assert!(url.starts_with("https://api.giphy.com/v1/gifs/search?"));
        assert!(url.contains("api_key=KEY%20123"), "{url}");
        assert!(url.contains("q=cat%20%26%20dog"), "{url}");
        assert!(url.contains("limit=8"));
        assert!(url.contains("rating=pg"));
        let t = trending_url("k", 12, "g");
        assert!(t.contains("/trending?api_key=k&limit=12&rating=g"), "{t}");
    }

    #[test]
    fn settings_reach_the_request_url() {
        let mut stored = serde_json::Map::new();
        stored.insert("result_limit".to_owned(), serde_json::json!(7));
        stored.insert("rating".to_owned(), serde_json::json!("pg-13"));
        let (limit, rating) = fetch_params(&stored);
        assert_eq!(limit, 7);
        let url = search_url("k", "cats", limit, &rating);
        assert!(url.contains("limit=7"), "{url}");
        assert!(url.contains("rating=pg-13"), "{url}");
        // Defaults come from the manifest when nothing is stored.
        let (limit, rating) = fetch_params(&serde_json::Map::new());
        assert_eq!((limit, rating.as_str()), (24, "g"));
    }

    #[test]
    fn parse_drops_items_without_images() {
        let body: Value = serde_json::from_str(FIXTURE).expect("fixture");
        let items = parse_gifs(&body);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "abc");
        assert_eq!(items[0].preview_url, "https://media.example/abc_s.gif");
        assert_eq!(items[0].mp4_url.as_deref(), Some("https://media.example/abc.mp4"));
        assert_eq!(items[1].gif_url, "https://media.example/def.gif");
        assert_eq!(items[1].mp4_url, None, "a missing MP4 rendition stays None");
    }

    #[test]
    fn tab_state_follows_key_presence() {
        let empty = KeyPool::new(vec![]);
        assert_eq!(tab_state(&empty, false), TabState::NeedsKey);
        let with_key = KeyPool::new(vec!["k".to_string()]);
        assert_eq!(tab_state(&with_key, false), TabState::Ready);
        assert_eq!(tab_state(&with_key, true), TabState::KeyRejected);
    }

    #[test]
    fn transport_errors_never_contain_the_key_or_url() {
        // SEC-09 regression: force a transport failure against a
        // URL whose query carries a (fake) key, via a port that is
        // closed by construction (bound, then released). The error
        // shown to the user must contain neither the key nor the
        // request URL.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let url = format!("http://{addr}/v1/gifs/trending?api_key=SECRETKEY12345&limit=8");
        let err = fetch_preview(&url).expect_err("closed port must fail");
        let shown = err.to_string();
        assert!(!shown.contains("SECRETKEY12345"), "key leaked into error: {shown}");
        assert!(!shown.contains("api_key"), "request URL leaked into error: {shown}");
        assert!(!shown.contains("/v1/gifs/trending"), "request URL leaked: {shown}");
    }

    #[test]
    fn media_fetch_rejects_unsafe_urls_before_connecting() {
        // SEC-F02 regression: URLs out of a provider response must
        // be validated before any connection is attempted, so these
        // fail with UrlRejected deterministically, without network.
        for bad in [
            "http://media.example/x.gif",
            "https://127.0.0.1/x.gif",
            "https://169.254.169.254/latest/meta-data",
            "https://10.0.0.5/x.gif",
            "https://[::1]/x.gif",
            "not a url",
        ] {
            let err = fetch_preview(bad).expect_err("unsafe URL must be rejected");
            assert!(
                matches!(err, GiphyError::UrlRejected),
                "{bad}: expected UrlRejected, got {err:?}"
            );
            let err = fetch_mp4(bad).expect_err("unsafe URL must be rejected");
            assert!(matches!(err, GiphyError::UrlRejected), "{bad}: {err:?}");
        }
    }

    #[test]
    fn pinned_client_builds_for_a_public_literal() {
        // SEC-F05: client construction itself needs no network;
        // a public literal passes validation and builds, while
        // non-public targets fail as UrlRejected before any
        // connection could be attempted.
        assert!(pinned_media_client("https://8.8.8.8/x.gif").is_ok());
        for bad in [
            "http://8.8.8.8/x.gif",
            "https://127.0.0.1/x.gif",
            "https://10.0.0.5/x.gif",
            "not a url",
        ] {
            let err = pinned_media_client(bad).expect_err("unsafe URL must be rejected");
            assert!(matches!(err, GiphyError::UrlRejected), "{bad}: {err:?}");
        }
    }

    #[test]
    fn trending_against_mock_server_parses_and_reports() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let body = FIXTURE;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        // Point the fetch at the mock by rebuilding the URL closure:
        // `fetch` takes a URL builder, so exercise it via trending_url
        // shape against the mock origin.
        let mut pool = KeyPool::new(vec!["mock-key".to_string()]);
        let url = format!("http://{addr}/trending?api_key=mock-key&limit=8&rating=g");
        let items = fetch(|_| url.clone(), &mut pool, 0).expect("items");
        assert_eq!(items.len(), 2);
        server.join().expect("server thread");
    }
}
