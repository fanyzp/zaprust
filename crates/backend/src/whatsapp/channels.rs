//! Channel (newsletter) commands for the live session: the
//! directory and follow graph, history catch-up, reactions,
//! polls, admin edits, and the info payload. Every command
//! answers with its typed success/failure event pair (design
//! standard section 27); the raw library error is classified
//! at this boundary by `error_words`, like every other
//! backend surface.

use whatsapp_rust::prelude::*;

use crate::{
    BackendEvent, EventSink, LiveChannel, LiveChannelInfo, LiveChannelRole, LiveChat, LiveChatKind,
};

use super::translate::translate_newsletter_message;
use super::util::error_words;

/// Converts the library's channel metadata into the backend's
/// channel type. A metadata with no role is one the account
/// does not follow (a lookup result), which is the Guest role.
pub(super) fn channel_from_metadata(meta: &whatsapp_rust::NewsletterMetadata) -> LiveChannel {
    let role = match meta.role {
        Some(whatsapp_rust::NewsletterRole::Owner) => LiveChannelRole::Owner,
        Some(whatsapp_rust::NewsletterRole::Admin) => LiveChannelRole::Admin,
        Some(whatsapp_rust::NewsletterRole::Subscriber) => LiveChannelRole::Follower,
        Some(whatsapp_rust::NewsletterRole::Guest) | None => LiveChannelRole::Guest,
        // The library's role enum is non-exhaustive; a role
        // this client does not model is not a follow it can
        // act on, so it reads as Guest.
        Some(_) => LiveChannelRole::Guest,
    };
    LiveChannel {
        jid: meta.jid.to_string(),
        name: meta.name.clone(),
        description: meta.description.clone().unwrap_or_default(),
        followers: meta.subscriber_count,
        verified: matches!(meta.verification, whatsapp_rust::NewsletterVerification::Verified),
        role,
        muted: meta.muted.unwrap_or(false),
        invite_code: meta.invite_code.clone(),
        created_at: meta.creation_time.and_then(|t| i64::try_from(t).ok()).unwrap_or(0),
    }
}

/// Extracts the invite code from what the user pasted: a full
/// `whatsapp.com/channel/<code>` link, the bare code, or a
/// link with tracking parameters. Returns None when nothing
/// code-shaped remains.
fn invite_code_of(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let tail = trimmed.rsplit("channel/").next().unwrap_or(trimmed);
    let code: String =
        tail.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    if code.len() >= 8 {
        Some(code)
    } else {
        None
    }
}

/// Answers [`BackendCommand::FetchChannels`].
pub(super) async fn fetch_channels(client: &whatsapp_rust::Client, events: &EventSink) {
    match client.newsletter().list_subscribed().await {
        Ok(list) => {
            let channels = list.iter().map(channel_from_metadata).collect();
            events.emit(BackendEvent::ChannelsListed { channels });
        }
        Err(e) => events.emit(BackendEvent::ChannelsListFailed {
            message: format!("Loading the channel list failed: {}", error_words(&e)),
        }),
    }
}

/// Answers [`BackendCommand::LookupChannel`].
pub(super) async fn lookup_channel(client: &whatsapp_rust::Client, events: &EventSink, code: &str) {
    let Some(code) = invite_code_of(code) else {
        events.emit(BackendEvent::ChannelLookupFailed {
            message: "That does not look like a channel invite link.".to_owned(),
        });
        return;
    };
    match client.newsletter().get_metadata_by_invite(&code).await {
        Ok(meta) => {
            events.emit(BackendEvent::ChannelFound { channel: channel_from_metadata(&meta) });
        }
        Err(e) => events.emit(BackendEvent::ChannelLookupFailed {
            message: format!("No channel was found for that link: {}", error_words(&e)),
        }),
    }
}

/// The roster row for a channel this session just brought
/// into the account (a follow or a creation): the channel's
/// updates need a chat to land in, and a fresh follow has no
/// history yet, so the row starts empty at the current time.
fn fresh_channel_chat(channel: &LiveChannel) -> LiveChat {
    LiveChat {
        jid: channel.jid.clone(),
        title: channel.name.clone(),
        kind: LiveChatKind::Channel,
        preview: String::new(),
        preview_sender: None,
        timestamp: super::util::unix_now(),
        unread: 0,
        archived: false,
        pinned: false,
        ephemeral_secs: 0,
        muted: channel.muted,
        mute_until: 0,
        parent_jid: None,
        is_parent: false,
    }
}

/// Answers [`BackendCommand::FollowChannel`].
pub(super) async fn follow_channel(client: &whatsapp_rust::Client, events: &EventSink, jid: &str) {
    let fail = |message: String| {
        events.emit(BackendEvent::ChannelFollowFailed { jid: jid.to_owned(), message });
    };
    match jid.parse::<Jid>() {
        Ok(parsed) => match client.newsletter().join(&parsed).await {
            Ok(meta) => {
                let channel = channel_from_metadata(&meta);
                events.emit(BackendEvent::ChatUpsert(fresh_channel_chat(&channel)));
                events.emit(BackendEvent::ChannelFollowed { channel });
            }
            Err(e) => fail(format!("Following the channel failed: {}", error_words(&e))),
        },
        Err(e) => fail(format!("That channel address is not usable: {}", error_words(&e))),
    }
}

/// Answers [`BackendCommand::UnfollowChannel`].
pub(super) async fn unfollow_channel(client: &whatsapp_rust::Client, events: &EventSink, jid: &str) {
    let fail = |message: String| {
        events.emit(BackendEvent::ChannelUnfollowFailed { jid: jid.to_owned(), message });
    };
    match jid.parse::<Jid>() {
        Ok(parsed) => match client.newsletter().leave(&parsed).await {
            Ok(()) => events.emit(BackendEvent::ChannelUnfollowed { jid: jid.to_owned() }),
            Err(e) => fail(format!("Unfollowing the channel failed: {}", error_words(&e))),
        },
        Err(e) => fail(format!("That channel address is not usable: {}", error_words(&e))),
    }
}

/// Answers [`BackendCommand::CreateChannel`].
pub(super) async fn create_channel(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    name: &str,
    description: &str,
) {
    let description = if description.trim().is_empty() { None } else { Some(description) };
    match client.newsletter().create(name, description).await {
        Ok(meta) => {
            let channel = channel_from_metadata(&meta);
            events.emit(BackendEvent::ChatUpsert(fresh_channel_chat(&channel)));
            events.emit(BackendEvent::ChannelCreated { channel });
        }
        Err(e) => events.emit(BackendEvent::ChannelCreateFailed {
            message: format!("Creating the channel failed: {}", error_words(&e)),
        }),
    }
}

/// Answers [`BackendCommand::FetchChannelHistory`]: the
/// page's messages go out as ordinary upserts first, then the
/// completion event, the FetchOlderMessages pattern. The
/// account's own reactions and poll votes (the library's
/// add-ons read) are folded into the page's messages before
/// they are emitted; an add-ons failure never fails the page.
pub(super) async fn fetch_channel_history(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    jid: &str,
    count: u32,
    before: Option<u64>,
) {
    let fail = |message: String| {
        events.emit(BackendEvent::ChannelHistoryFailed { jid: jid.to_owned(), message });
    };
    let parsed = match jid.parse::<Jid>() {
        Ok(parsed) => parsed,
        Err(e) => {
            fail(format!("That channel address is not usable: {}", error_words(&e)));
            return;
        }
    };
    let entries = match client.newsletter().get_messages(parsed.clone(), count, before).await {
        Ok(entries) => entries,
        Err(e) => {
            fail(format!("Loading the channel's updates failed: {}", error_words(&e)));
            return;
        }
    };
    let addons = client.newsletter().get_my_addons(&parsed, count).await.unwrap_or_default();
    let added = u32::try_from(entries.len()).unwrap_or(u32::MAX);
    let end_of_history = entries.len() < usize::try_from(count).unwrap_or(usize::MAX);
    for entry in &entries {
        let mut msg = translate_newsletter_message(jid, entry);
        if let Some(addon) = addons.iter().find(|a| a.server_id == entry.server_id) {
            if let Some(reaction) = &addon.reaction {
                msg.my_reaction = Some(reaction.code.clone());
            }
            if let Some(vote) = &addon.poll_vote {
                if let crate::LiveMessageKind::Poll { options, my_votes, .. } = &mut msg.kind {
                    *my_votes = options
                        .iter()
                        .enumerate()
                        .filter(|(_, (name, _))| {
                            vote.option_hashes
                                .contains(&whatsapp_rust::wacore::poll::compute_option_hash(name))
                        })
                        .map(|(index, _)| index)
                        .collect();
                }
            }
        }
        events.emit(BackendEvent::MessageUpsert(msg));
    }
    events.emit(BackendEvent::ChannelHistoryFetched { jid: jid.to_owned(), added, end_of_history });
}

/// Answers [`BackendCommand::ReactToChannelUpdate`].
pub(super) async fn react_to_channel_update(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    jid: &str,
    server_id: u64,
    emoji: &str,
) {
    let fail = |message: String| {
        events.emit(BackendEvent::ChannelReactionFailed { jid: jid.to_owned(), server_id, message });
    };
    match jid.parse::<Jid>() {
        Ok(parsed) => match client.newsletter().send_reaction_raw(&parsed, server_id, emoji).await {
            Ok(_) => events.emit(BackendEvent::ChannelReactionSet {
                jid: jid.to_owned(),
                server_id,
                emoji: emoji.to_owned(),
            }),
            Err(e) => fail(format!("That reaction could not be sent: {}", error_words(&e))),
        },
        Err(e) => fail(format!("That channel address is not usable: {}", error_words(&e))),
    }
}

/// Answers [`BackendCommand::CreateChannelPoll`].
pub(super) async fn create_channel_poll(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    jid: &str,
    question: &str,
    options: &[String],
    multiple: bool,
) {
    let fail = |message: String| {
        events.emit(BackendEvent::ChannelPollFailed { jid: jid.to_owned(), message });
    };
    match jid.parse::<Jid>() {
        Ok(parsed) => {
            let selectable = if multiple { 0 } else { 1 };
            match client.newsletter().send_poll(&parsed, question, options, selectable).await {
                Ok(_) => events.emit(BackendEvent::ChannelPollCreated { jid: jid.to_owned() }),
                Err(e) => fail(format!("Posting the poll failed: {}", error_words(&e))),
            }
        }
        Err(e) => fail(format!("That channel address is not usable: {}", error_words(&e))),
    }
}

/// Answers [`BackendCommand::VoteChannelPoll`].
pub(super) async fn vote_channel_poll(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    jid: &str,
    server_id: u64,
    options: &[String],
) {
    let fail = |message: String| {
        events.emit(BackendEvent::ChannelPollVoteFailed { jid: jid.to_owned(), server_id, message });
    };
    match jid.parse::<Jid>() {
        Ok(parsed) => {
            let hashes: Vec<[u8; 32]> = options
                .iter()
                .map(|name| whatsapp_rust::wacore::poll::compute_option_hash(name))
                .collect();
            match client.newsletter().send_poll_vote_raw(&parsed, server_id, &hashes).await {
                Ok(_) => {
                    events.emit(BackendEvent::ChannelPollVoted { jid: jid.to_owned(), server_id });
                }
                Err(e) => fail(format!("That vote could not be sent: {}", error_words(&e))),
            }
        }
        Err(e) => fail(format!("That channel address is not usable: {}", error_words(&e))),
    }
}

/// Answers [`BackendCommand::EditChannelUpdate`].
pub(super) async fn edit_channel_update(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    jid: &str,
    message_id: &str,
    text: &str,
) {
    let fail = |message: String| {
        events.emit(BackendEvent::ChannelUpdateEditFailed {
            jid: jid.to_owned(),
            message_id: message_id.to_owned(),
            message,
        });
    };
    match jid.parse::<Jid>() {
        Ok(parsed) => {
            let mut content = wa::Message::default();
            content.conversation = Some(text.to_owned());
            match client.newsletter().edit_message_raw(&parsed, message_id, content).await {
                Ok(()) => events.emit(BackendEvent::ChannelUpdateEdited {
                    jid: jid.to_owned(),
                    message_id: message_id.to_owned(),
                }),
                Err(e) => fail(format!("Editing the update failed: {}", error_words(&e))),
            }
        }
        Err(e) => fail(format!("That channel address is not usable: {}", error_words(&e))),
    }
}

/// Answers [`BackendCommand::RevokeChannelUpdate`].
pub(super) async fn revoke_channel_update(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    jid: &str,
    message_id: &str,
) {
    let fail = |message: String| {
        events.emit(BackendEvent::ChannelUpdateRevokeFailed {
            jid: jid.to_owned(),
            message_id: message_id.to_owned(),
            message,
        });
    };
    match jid.parse::<Jid>() {
        Ok(parsed) => match client.newsletter().revoke_message_raw(&parsed, message_id).await {
            Ok(()) => events.emit(BackendEvent::ChannelUpdateRevoked {
                jid: jid.to_owned(),
                message_id: message_id.to_owned(),
            }),
            Err(e) => fail(format!("Deleting the update failed: {}", error_words(&e))),
        },
        Err(e) => fail(format!("That channel address is not usable: {}", error_words(&e))),
    }
}

/// Answers [`BackendCommand::FetchChannelInfo`]. The admin
/// facts ride along only when the server answers them: a
/// follower's info fetch is metadata-only and says so by
/// leaving the admin fields empty, never by failing.
pub(super) async fn fetch_channel_info(client: &whatsapp_rust::Client, events: &EventSink, jid: &str) {
    let fail = |message: String| {
        events.emit(BackendEvent::ChannelInfoFailed { jid: jid.to_owned(), message });
    };
    let parsed = match jid.parse::<Jid>() {
        Ok(parsed) => parsed,
        Err(e) => {
            fail(format!("That channel address is not usable: {}", error_words(&e)));
            return;
        }
    };
    let meta = match client.newsletter().get_metadata(&parsed).await {
        Ok(meta) => meta,
        Err(e) => {
            fail(format!("Loading the channel info failed: {}", error_words(&e)));
            return;
        }
    };
    let channel = channel_from_metadata(&meta);
    let (admin_count, follower_names) = if channel.role.can_post() {
        let admin_count =
            client.newsletter().get_admin_info(&parsed).await.ok().and_then(|info| info.admin_count);
        let follower_names = client
            .newsletter()
            .get_followers(&parsed, 50)
            .await
            .map(|followers| {
                followers
                    .iter()
                    .filter_map(|f| f.display_name.clone().or_else(|| f.username.clone()))
                    .collect()
            })
            .unwrap_or_default();
        (admin_count, follower_names)
    } else {
        (None, Vec::new())
    };
    events.emit(BackendEvent::ChannelInfoFetched {
        info: LiveChannelInfo { channel, admin_count, follower_names },
    });
}

/// Answers [`BackendCommand::SetChannelMuted`]. The
/// library's follower mute is the admin-activity setting
/// (the channel's own updates), which is the mute a
/// follower toggles.
pub(super) async fn set_channel_muted(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    jid: &str,
    muted: bool,
) {
    let fail = |message: String| {
        events.emit(BackendEvent::ChannelMuteFailed { jid: jid.to_owned(), message });
    };
    match jid.parse::<Jid>() {
        Ok(parsed) => match client.newsletter().set_admin_mute(&parsed, muted).await {
            Ok(()) => events.emit(BackendEvent::ChannelMuteSet { jid: jid.to_owned(), muted }),
            Err(e) => fail(format!("Changing the mute setting failed: {}", error_words(&e))),
        },
        Err(e) => fail(format!("That channel address is not usable: {}", error_words(&e))),
    }
}

/// Arms live updates for one channel and keeps the
/// subscription renewed for the rest of the session: the
/// server grants a duration (about 90 seconds in practice)
/// and the renewal lands ahead of it. A refused renewal
/// ends the loop quietly: history fetch stays the
/// guaranteed path, per the WatchChannel contract.
pub(super) async fn watch_channel(client: std::sync::Arc<whatsapp_rust::Client>, jid: &str) {
    let Ok(parsed) = jid.parse::<Jid>() else {
        return;
    };
    tokio::spawn(async move {
        loop {
            match client.newsletter().subscribe_live_updates(parsed.clone()).await {
                Ok(duration) => {
                    let renew_after = duration.saturating_mul(3) / 4;
                    tokio::time::sleep(std::time::Duration::from_secs(renew_after.max(15))).await;
                }
                Err(_) => return,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::invite_code_of;

    #[test]
    fn invite_code_comes_out_of_links_and_bare_codes() {
        assert_eq!(
            invite_code_of("https://whatsapp.com/channel/AbCdEfGh1234").as_deref(),
            Some("AbCdEfGh1234")
        );
        assert_eq!(invite_code_of("AbCdEfGh1234").as_deref(), Some("AbCdEfGh1234"));
        assert_eq!(
            invite_code_of("  whatsapp.com/channel/AbCdEfGh1234?x=1 ").as_deref(),
            Some("AbCdEfGh1234")
        );
        assert_eq!(invite_code_of("hello"), None);
        assert_eq!(invite_code_of(""), None);
    }
}
