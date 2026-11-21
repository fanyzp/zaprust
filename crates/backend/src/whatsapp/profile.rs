//! The signed-in account's own profile for the WhatsApp backend:
//! reading the push name, About and picture back from the server,
//! and applying editor changes through the library's profile API.
//!
//! The read-back is three separate sources, because that is how
//! the protocol exposes a linked device's own profile: the push
//! name is the library's local device-store value (the name the
//! phone set, kept in sync by the app-state protocol), the About
//! is the account's text status read with a USync status query
//! against the account's own JID, and the picture is the
//! protocol's picture lookup, whose result is a CDN URL the bytes
//! are then downloaded from. Each part degrades on its own: a
//! part that cannot be read comes back empty rather than failing
//! the whole profile, so the editor still opens with what the
//! server did answer.

use whatsapp_rust::prelude::*;
use whatsapp_rust::usync::{
    UsyncContext, UsyncMode, UsyncOutcome, UsyncProtocol, UsyncProtocolKind, UsyncProtocolResult,
    UsyncQuery, UsyncResponse, UsyncUser,
};
use whatsapp_rust::{ProfilePictureRequest, ProfilePictureTarget, ProfilePictureType, TextStatusExpiry};

use crate::OwnProfile;

/// Largest profile picture download accepted, in bytes. WhatsApp
/// serves profile pictures at most a few hundred kilobytes; the
/// cap only guards against a CDN answer that is not a picture.
const MAX_PICTURE_BYTES: u64 = 8 * 1024 * 1024;

/// Reads the account's own profile from the server (see the
/// module docs for the three sources). Never fails: parts that
/// cannot be read come back empty.
pub(super) async fn fetch_own_profile(client: &Client) -> OwnProfile {
    let name = client.push_name();
    let (about, photo) = match client.pn() {
        Some(jid) => {
            let about = fetch_own_about(client, &jid).await;
            let photo = fetch_own_picture(client, &jid).await;
            (about, photo)
        }
        None => (String::new(), None),
    };
    OwnProfile { name, about, photo }
}

/// The account's About (its text status), read with a USync
/// status query against its own JID. Empty when unset or when
/// the query fails.
async fn fetch_own_about(client: &Client, jid: &Jid) -> String {
    let query = match UsyncQuery::new(
        UsyncMode::Query,
        UsyncContext::Interactive,
        vec![UsyncProtocol::Status],
        vec![UsyncUser::from_jid(jid.clone())],
    ) {
        Ok(query) => query,
        Err(_) => return String::new(),
    };
    match client.query_usync(query).await {
        Ok(response) => about_from_usync(&response),
        Err(_) => String::new(),
    }
}

/// Extracts the About text from a USync status response: the
/// first user's status value, or empty when the response carries
/// none (unset About, an error outcome, or no user at all).
pub(super) fn about_from_usync(response: &UsyncResponse) -> String {
    let Some(user) = response.users.first() else {
        return String::new();
    };
    let Some(UsyncProtocolResult::Status(outcome)) = user.protocol(UsyncProtocolKind::Status)
    else {
        return String::new();
    };
    let UsyncOutcome::Value(result) = outcome else {
        return String::new();
    };
    result.status.as_ref().map(|s| s.to_string()).unwrap_or_default()
}

/// The account's profile picture bytes: the protocol picture
/// lookup (preview size) for the account's own JID, then a plain
/// download of the URL it returns. `None` when the account has
/// no picture or the bytes cannot be fetched.
async fn fetch_own_picture(client: &Client, jid: &Jid) -> Option<Vec<u8>> {
    let request = ProfilePictureRequest::new(
        ProfilePictureTarget::Contact(jid),
        ProfilePictureType::Preview,
    );
    let found = client.pictures().lookup(request).await.ok()?.into_found()?;
    let url = found.url;
    tokio::task::spawn_blocking(move || download_picture(&url))
        .await
        .ok()
        .flatten()
}

/// Downloads a profile picture from its CDN URL. The URL passes
/// the same public-HTTPS gate the app's own fetches use before
/// anything is requested, and the body read is size-capped.
/// Redirects are not followed: the gate covers the URL the
/// protocol returned, and a redirect answer would leave it, so
/// a redirect simply yields no picture. Blocking HTTP; call on
/// a blocking thread.
fn download_picture(url: &str) -> Option<Vec<u8>> {
    if !core::net::public_https_url_allowed(url) {
        return None;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(20)))
        .max_redirects(0)
        .build()
        .into();
    let mut response = agent.get(url).call().ok()?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_PICTURE_BYTES)
        .read_to_vec()
        .ok()
}

/// Applies one editor save to the account's profile. Fields left
/// `None` are untouched. The first field the server refuses
/// aborts the save with a user-words error; the caller re-reads
/// the profile afterwards either way, so the UI reconciles with
/// whatever state the server kept.
pub(super) async fn update_own_profile(
    client: &Client,
    name: Option<String>,
    about: Option<String>,
    photo: Option<Option<Vec<u8>>>,
) -> Result<(), String> {
    let profile = client.profile();
    if let Some(name) = name {
        let name = name.trim();
        if name.is_empty() {
            return Err("Profile change failed: the name cannot be empty.".to_owned());
        }
        // Both outcomes mean the change was accepted: `Synced`
        // when the app-state sync already carried it, and
        // `SyncPending` when the presence frame carried the name
        // and the sync follows. The re-read after the save shows
        // the name either way.
        let outcome = profile
            .set_push_name(name)
            .await
            .map_err(|e| format!("Profile change failed: {e}"))?;
        match outcome {
            whatsapp_rust::PushNameOutcome::Synced
            | whatsapp_rust::PushNameOutcome::SyncPending { .. } => {}
            // The outcome enum is non-exhaustive; every variant
            // reports how an accepted change completes its sync,
            // never a refusal (refusals are the error above).
            _ => {}
        }
    }
    if let Some(about) = about {
        let about = about.trim();
        if about.is_empty() {
            profile
                .clear_text_status()
                .await
                .map_err(|e| format!("Profile change failed: {e}"))?;
        } else {
            profile
                .set_text_status(about, None, TextStatusExpiry::Never)
                .await
                .map_err(|e| format!("Profile change failed: {e}"))?;
        }
    }
    if let Some(photo) = photo {
        match photo {
            Some(jpeg) => {
                profile
                    .set_profile_picture(jpeg)
                    .await
                    .map_err(|e| format!("Profile change failed: {e}"))?;
            }
            None => {
                profile
                    .remove_profile_picture()
                    .await
                    .map_err(|e| format!("Profile change failed: {e}"))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The USync response types are `#[non_exhaustive]` in the
    // library, so no response fixture can be constructed here;
    // `about_from_usync` is covered by compilation against the
    // pinned library types and by the live retest, not by a
    // fixture test. What can be proven offline is the download
    // gate: a URL the gate refuses must yield no picture and no
    // request.
    #[test]
    fn picture_download_refuses_urls_the_gate_rejects() {
        assert_eq!(download_picture("http://8.8.8.8/pic.jpg"), None);
        assert_eq!(download_picture("https://127.0.0.1/pic.jpg"), None);
        assert_eq!(download_picture("https://10.0.0.5/pic.jpg"), None);
        assert_eq!(download_picture("not a url"), None);
    }
}
