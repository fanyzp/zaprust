//! The signed-in account's own profile for the WhatsApp backend:
//! reading the push name, About and picture back from the server,
//! and applying editor changes through the library's profile API.
//! The same module carries the two identity operations that are
//! not about the account itself: setting or removing a group's
//! picture (the picture domain's other write) and resolving a
//! phone number to a person (the contacts facade's lookup).
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
use whatsapp_rust::{
    ProfilePictureRequest, ProfilePictureTarget, ProfilePictureType, TextStatusExpiry,
};

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

/// The outcome of one [`fetch_avatar`] lookup, in the terms
/// the app caches: bytes, a definitive negative (no picture, or
/// privacy refuses), or a transport failure that must stay
/// retryable and never be cached as a negative.
pub(super) enum AvatarOutcome {
    /// The picture's bytes and its server-side ID.
    Bytes {
        /// Server-side picture ID, for change detection.
        picture_id: Option<String>,
        /// The picture bytes (JPEG as served).
        bytes: Vec<u8>,
    },
    /// No picture exists for this identity, or its privacy
    /// settings refuse the lookup.
    Missing,
    /// The lookup or the download failed; nothing is known.
    Unavailable,
}

/// Fetches one identity's profile picture for display (a
/// contact, a group, or a community). The picture must belong
/// to the identity it labels, so the address form is resolved
/// before the lookup: a group is queried under its group JID,
/// and a contact known under a LID is queried under its
/// phone-number form (the same cache the title healer reads).
/// A LID whose phone form is not known yet is not queried at
/// all: the picture lookup is phone-addressed, so an answer
/// to a LID query says nothing about the picture, and treating
/// it as a definitive miss would cache a wrong negative for a
/// contact who may well have one. That case is Unavailable
/// (retryable, never cached), so a later paint retries once
/// the mapping is learned.
pub(super) async fn fetch_avatar(client: &Client, jid_str: &str) -> AvatarOutcome {
    use whatsapp_rust::ProfilePictureLookup;
    let Ok(jid) = jid_str.parse::<Jid>() else { return AvatarOutcome::Unavailable };
    let (target_jid, is_group) = if jid_str.ends_with("@g.us") {
        (jid.clone(), true)
    } else if jid_str.ends_with("@lid") {
        match client.get_lid_pn_entry(&jid).await {
            Ok(Some(entry)) => (
                format!("{}@s.whatsapp.net", entry.phone_number)
                    .parse::<Jid>()
                    .unwrap_or_else(|_| jid.clone()),
                false,
            ),
            _ => return AvatarOutcome::Unavailable,
        }
    } else {
        (jid.clone(), false)
    };
    let target = if is_group {
        ProfilePictureTarget::Group(&target_jid)
    } else {
        ProfilePictureTarget::Contact(&target_jid)
    };
    let request = ProfilePictureRequest::new(target, ProfilePictureType::Preview);
    match client.pictures().lookup(request).await {
        Ok(ProfilePictureLookup::Found(picture)) => {
            let picture_id = Some(picture.id.clone());
            let url = picture.url.clone();
            match tokio::task::spawn_blocking(move || download_picture(&url)).await {
                Ok(Some(bytes)) => AvatarOutcome::Bytes { picture_id, bytes },
                _ => AvatarOutcome::Unavailable,
            }
        }
        Ok(ProfilePictureLookup::NotFound | ProfilePictureLookup::NotAuthorized) => {
            AvatarOutcome::Missing
        }
        // Unchanged cannot occur (no conditional ID is sent),
        // and a lookup error says nothing about the picture:
        // both stay retryable. The lookup enum is non-exhaustive,
        // so any future outcome lands in the same retryable arm.
        _ => AvatarOutcome::Unavailable,
    }
}

/// The account's profile picture bytes: the protocol picture
/// lookup (preview size) for the account's own JID, then a plain
/// download of the URL it returns. `None` when the account has
/// no picture or the bytes cannot be fetched.
async fn fetch_own_picture(client: &Client, jid: &Jid) -> Option<Vec<u8>> {
    let request =
        ProfilePictureRequest::new(ProfilePictureTarget::Contact(jid), ProfilePictureType::Preview);
    let found = client.pictures().lookup(request).await.ok()?.into_found()?;
    let url = found.url;
    tokio::task::spawn_blocking(move || download_picture(&url)).await.ok().flatten()
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
    response.body_mut().with_config().limit(MAX_PICTURE_BYTES).read_to_vec().ok()
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
        let outcome =
            profile.set_push_name(name).await.map_err(|e| format!("Profile change failed: {e}"))?;
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
            profile.clear_text_status().await.map_err(|e| format!("Profile change failed: {e}"))?;
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

/// Sets or removes one group's profile picture through the
/// library's groups facade. The bytes are carried as given (the
/// caller cropped and encoded them, exactly like the account's
/// own picture). Errors are worded for the user from the
/// error's structure: the protocol has no typed not-admin
/// error, so a server rejection (an active refusal, read
/// through the library's error-chain helper) is worded as a
/// refusal with admin rights named as the ordinary cause, and
/// a transport failure as a reachability failure. Neither is
/// ever collapsed into the other, and neither is silent.
pub(super) async fn set_group_picture(
    client: &Client,
    group_jid: &str,
    image: Option<Vec<u8>>,
) -> Result<(), String> {
    use whatsapp_rust::ErrorChainExt;
    let Ok(jid) = group_jid.parse::<Jid>() else {
        return Err("That group address is not usable.".to_owned());
    };
    if image.as_ref().is_some_and(|bytes| bytes.is_empty()) {
        return Err("That picture file is empty, nothing was sent.".to_owned());
    }
    let groups = client.groups();
    let result = match image {
        Some(bytes) => groups.set_profile_picture(jid, bytes).await.map(|_| ()),
        None => groups.remove_profile_picture(jid).await.map(|_| ()),
    };
    result.map_err(|e| {
        if let Some(rejection) = e.server_rejection() {
            let detail = if rejection.text.is_empty() {
                String::new()
            } else {
                format!(": {}", rejection.text)
            };
            format!(
                "The server refused the picture change{detail}. \
                 Only a group admin can change a group's picture."
            )
        } else if e.is_timeout() || e.is_transport_unavailable() {
            "The picture change could not reach the server. Check the connection and try again."
                .to_owned()
        } else {
            format!("The picture change failed: {e}")
        }
    })
}

/// The outcome of one [`lookup_contact`] call, in the three
/// shapes the protocol boundary keeps distinct: a resolved
/// person, a definitive not-on-WhatsApp answer, and a failed
/// lookup (which must never be reported as the second).
pub(super) enum ContactOutcome {
    /// The person is on WhatsApp; the chat is ready to open,
    /// with the person's other address form when disclosed.
    Resolved {
        /// The resolved person's chat.
        chat: crate::LiveChat,
        /// The person's LID or phone-number counterpart JID.
        alt_jid: Option<String>,
    },
    /// The number is not on WhatsApp.
    NotFound,
    /// The lookup itself failed, in user words.
    Failed(String),
}

/// Resolves one phone number (digits only, international form;
/// the caller normalized it) to the person behind it through
/// the contacts facade. The chat's title is the first name
/// source that knows the person: the business verified name
/// the lookup itself returns, then the formatted number (the
/// identity standard's fallback; a personal account's push
/// name is not something this query returns, and a name is
/// never invented). The chat is addressed by the phone-number
/// JID the user typed; the LID counterpart rides along so the
/// app can find a roster chat that already exists under it.
pub(super) async fn lookup_contact(client: &Client, digits: &str) -> ContactOutcome {
    let Ok(jid) = format!("{digits}@s.whatsapp.net").parse::<Jid>() else {
        return ContactOutcome::Failed(
            "That does not look like a phone number: use the full international format, digits only."
                .to_owned(),
        );
    };
    let results = match client.contacts().is_on_whatsapp(std::slice::from_ref(&jid)).await {
        Ok(results) => results,
        Err(e) => {
            return ContactOutcome::Failed(format!("The lookup did not reach the server: {e}"));
        }
    };
    let Some(result) = results.into_iter().next() else {
        return ContactOutcome::Failed("The server did not answer the lookup.".to_owned());
    };
    if let Some(error) = &result.contact_error {
        let detail = error.text.as_deref().unwrap_or("the server could not check it");
        return ContactOutcome::Failed(format!("The lookup failed for that number: {detail}"));
    }
    if !result.is_registered {
        return ContactOutcome::NotFound;
    }
    let pn_jid = result.pn_jid.clone().unwrap_or_else(|| jid.clone());
    let alt_jid = result.lid.as_ref().map(ToString::to_string);
    let title = result
        .verified_name
        .as_ref()
        .and_then(|verified| verified.name.clone())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| format!("+{digits}"));
    ContactOutcome::Resolved {
        chat: crate::LiveChat {
            jid: pn_jid.to_string(),
            title,
            kind: crate::LiveChatKind::Dm,
            preview: String::new(),
            preview_sender: None,
            timestamp: 0,
            unread: 0,
            archived: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
        },
        alt_jid,
    }
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
