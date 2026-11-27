//! Group extras over the library's group facade: sharing
//! recent history with new members (row 64), the account's
//! own member label and the admin report surface (row 65).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use whatsapp_rust::prelude::*;

use super::util::{error_words, parse_address};
use crate::{BackendEvent, EventSink, HistoryMessageRecord, HistoryShareReport};

/// Prepared history shares kept for a retry, keyed by group
/// JID. The library's retry token is opaque and cannot cross
/// the backend contract, so it lives here, in the session,
/// and a retry command consumes it in place. A reconnect
/// drops the map with the session, which the retry answer
/// says plainly.
pub(super) type HistoryRetries = Arc<Mutex<HashMap<String, whatsapp_rust::GroupHistoryRetryToken>>>;

/// A fresh, empty retry store for one session.
pub(super) fn new_history_retries() -> HistoryRetries {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Rebuilds one wire history record from the app's archived
/// plain-text message. The status states what the archive
/// knows: an outgoing message's persisted delivery state, and
/// read for an incoming message this device holds. The
/// library re-checks eligibility (acknowledged plain text,
/// inside the window) on its side; a record it drops is
/// simply not in the bundle.
fn history_record_to_proto(chat_jid: &str, record: &HistoryMessageRecord) -> wa::WebMessageInfo {
    let mut key = wa::MessageKey::default();
    key.remote_jid = Some(chat_jid.to_owned());
    key.id = Some(record.id.clone());
    key.from_me = Some(record.from_me);
    if !record.from_me {
        key.participant = record.author_jid.clone();
    }
    let mut message = wa::Message::default();
    if record.mentions.is_empty() {
        message.conversation = Some(record.body.clone());
    } else {
        let mut extended = wa::message::ExtendedTextMessage::default();
        extended.text = Some(record.body.clone());
        let mut context = wa::ContextInfo::default();
        context.mentioned_jid.clone_from(&record.mentions);
        extended.context_info = MessageField::some(context);
        message.extended_text_message = MessageField::some(extended);
    }
    let status = if record.from_me {
        match record.delivery {
            Some(crate::DeliveryState::Read) => wa::web_message_info::Status::READ,
            Some(crate::DeliveryState::Delivered) => wa::web_message_info::Status::DELIVERY_ACK,
            _ => wa::web_message_info::Status::SERVER_ACK,
        }
    } else {
        wa::web_message_info::Status::READ
    };
    let mut info = wa::WebMessageInfo::default();
    info.key = MessageField::some(key);
    info.message = MessageField::some(message);
    info.message_timestamp = Some(record.timestamp.max(0) as u64);
    info.status = Some(status);
    info
}

/// The skip reasons in words the app can show as they are.
fn skip_reason_words(reason: whatsapp_rust::GroupHistorySkipReason) -> String {
    use whatsapp_rust::GroupHistorySkipReason as Reason;
    match reason {
        Reason::NoOptedInSuccessfulRecipients => {
            "none of the added members could receive history".to_owned()
        }
        Reason::SenderNotAuthorized => {
            "you do not have permission to share history in this group".to_owned()
        }
        Reason::UnsupportedGroup => "this kind of group cannot receive history".to_owned(),
        Reason::AccountPropsUnavailable => {
            "history sharing is not ready on this connection yet".to_owned()
        }
        Reason::GroupMetadataUnavailable => "the group's details could not be verified".to_owned(),
        Reason::GroupPropsUnavailable => {
            "history sharing is not ready for this group yet".to_owned()
        }
        Reason::InvalidProperties => "the history sharing settings are not usable".to_owned(),
        Reason::SharingDisabled => "history sharing is turned off for this group".to_owned(),
        Reason::NoEligibleMessages => "there are no recent text messages to share".to_owned(),
        _ => "history sharing was skipped".to_owned(),
    }
}

/// Maps the library's share outcome onto the contract report.
/// A retryable failure's token moves into the session store
/// so the retry command can resume the prepared share; the
/// report itself carries only the fact and the words.
fn share_report(
    outcome: whatsapp_rust::GroupHistoryShareOutcome,
    retries: &HistoryRetries,
    chat_jid: &str,
) -> HistoryShareReport {
    use whatsapp_rust::GroupHistoryShareOutcome as Outcome;
    let keep = |token: whatsapp_rust::GroupHistoryRetryToken| {
        if let Ok(mut map) = retries.lock() {
            map.insert(chat_jid.to_owned(), token);
        }
    };
    let failed = |error: String, retryable: bool| HistoryShareReport::Failed { error, retryable };
    match outcome {
        Outcome::Shared { message_count, .. } => {
            HistoryShareReport::Shared { messages: message_count }
        }
        Outcome::NotRequested => HistoryShareReport::NotRequested,
        Outcome::Skipped(reason) => {
            HistoryShareReport::Skipped { reason: skip_reason_words(reason) }
        }
        Outcome::PreparationFailed { error } => failed(error, false),
        Outcome::UploadFailed { error, retry } => {
            keep(retry);
            failed(error, true)
        }
        Outcome::BundleNotSent { error, retry, .. } => {
            keep(retry);
            failed(error, true)
        }
        Outcome::BundleRejected { error, .. } => failed(
            error.unwrap_or_else(|| "the server rejected the history bundle".to_owned()),
            false,
        ),
        Outcome::BundleRetryableRejection { error, retry, .. } => {
            keep(retry);
            failed(
                error.unwrap_or_else(|| "the server rejected the history bundle".to_owned()),
                true,
            )
        }
        Outcome::BundleIndeterminate { retry, .. } => {
            keep(retry);
            failed("the server did not confirm the history bundle".to_owned(), true)
        }
        Outcome::BundlePartialFanout { encrypted_devices, addressed_devices, retry, .. } => {
            keep(retry);
            failed(
                format!(
                    "the history bundle reached {encrypted_devices} of {addressed_devices} devices"
                ),
                true,
            )
        }
        Outcome::NoticeNotSent { error, retry, .. } => {
            keep(retry);
            failed(format!("the history was shared, but its announcement failed: {error}"), true)
        }
        Outcome::NoticeRejected { error, .. } => failed(
            match error {
                Some(error) => {
                    format!("the history was shared, but its announcement was rejected: {error}")
                }
                None => "the history was shared, but its announcement was rejected".to_owned(),
            },
            false,
        ),
        Outcome::NoticeRetryableRejection { error, retry, .. } => {
            keep(retry);
            failed(
                match error {
                    Some(error) => {
                        format!(
                            "the history was shared, but its announcement was rejected: {error}"
                        )
                    }
                    None => "the history was shared, but its announcement was rejected".to_owned(),
                },
                true,
            )
        }
        Outcome::NoticeIndeterminate { retry, .. } => {
            keep(retry);
            failed(
                "the history was shared, but its announcement was not confirmed".to_owned(),
                true,
            )
        }
        _ => {
            failed("the history share ended in a state this build does not know".to_owned(), false)
        }
    }
}

/// The add-with-history command: add the members, then let
/// the library prepare and deliver the bundle built from the
/// app's archived records. The two outcomes are reported
/// separately, as the library returns them.
pub(super) async fn add_participants_with_history(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    retries: &HistoryRetries,
    chat_jid: &str,
    participants: &[String],
    history: &[HistoryMessageRecord],
) {
    let parsed = (
        chat_jid.parse::<Jid>(),
        participants.iter().map(|p| parse_address(p)).collect::<Result<Vec<Jid>, String>>(),
    );
    let (jid, parts) = match parsed {
        (Ok(jid), Ok(parts)) => (jid, parts),
        (Err(e), _) => {
            events.emit(BackendEvent::Error {
                message: format!("Invalid group address: {}", error_words(&e)),
            });
            return;
        }
        (_, Err(e)) => {
            events.emit(BackendEvent::Error { message: e });
            return;
        }
    };
    let records: Vec<wa::WebMessageInfo> =
        history.iter().map(|record| history_record_to_proto(chat_jid, record)).collect();
    match client.groups().add_participants_with_history(jid, &parts, &parts, &records).await {
        Ok(result) => {
            let mut added = Vec::new();
            let mut refused = Vec::new();
            for change in &result.participants {
                if change.is_ok() {
                    added.push(change.jid.to_string());
                } else {
                    refused.push((
                        change.jid.to_string(),
                        change
                            .error
                            .clone()
                            .or_else(|| change.status.clone())
                            .unwrap_or_else(|| "refused".to_owned()),
                    ));
                }
            }
            let report = share_report(result.history_share, retries, chat_jid);
            events.emit(BackendEvent::GroupHistoryReported {
                chat_jid: chat_jid.to_owned(),
                added,
                refused,
                report,
            });
        }
        Err(e) => events.emit(BackendEvent::Error {
            message: format!("That group change failed: {}", error_words(&e)),
        }),
    }
}

/// The retry command: resume the prepared share the session
/// kept. Without a kept share the answer says so instead of
/// pretending (a reconnect drops the prepared bundle with
/// the session that made it).
pub(super) async fn retry_history_share(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    retries: &HistoryRetries,
    chat_jid: &str,
) {
    let token = retries.lock().ok().and_then(|mut map| map.remove(chat_jid));
    match token {
        Some(token) => {
            let outcome = client.groups().retry_group_history(&token).await;
            let report = share_report(outcome, retries, chat_jid);
            events.emit(BackendEvent::GroupHistoryReported {
                chat_jid: chat_jid.to_owned(),
                added: Vec::new(),
                refused: Vec::new(),
                report,
            });
        }
        None => events.emit(BackendEvent::GroupHistoryReported {
            chat_jid: chat_jid.to_owned(),
            added: Vec::new(),
            refused: Vec::new(),
            report: HistoryShareReport::Failed {
                error: "the prepared share did not survive; add the members again to share history"
                    .to_owned(),
                retryable: false,
            },
        }),
    }
}

/// The own-member-label command (row 65).
pub(super) async fn set_own_member_label(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    chat_jid: &str,
    label: &str,
) {
    match chat_jid.parse::<Jid>() {
        Ok(jid) => match client.groups().update_member_label(jid, label).await {
            Ok(()) => events.emit(BackendEvent::MemberLabelSet {
                chat_jid: chat_jid.to_owned(),
                label: label.to_owned(),
            }),
            Err(e) => events.emit(BackendEvent::MemberLabelFailed {
                chat_jid: chat_jid.to_owned(),
                message: format!("Setting your label failed: {}", error_words(&e)),
            }),
        },
        Err(e) => events.emit(BackendEvent::MemberLabelFailed {
            chat_jid: chat_jid.to_owned(),
            message: format!("Invalid group address: {}", error_words(&e)),
        }),
    }
}

/// The report-to-admins command (row 65).
pub(super) async fn report_message_to_admins(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    chat_jid: &str,
    message_id: &str,
) {
    let fail = |message: String| {
        events.emit(BackendEvent::ReportToAdminsFailed {
            chat_jid: chat_jid.to_owned(),
            message_id: message_id.to_owned(),
            message,
        });
    };
    match chat_jid.parse::<Jid>() {
        Ok(jid) => {
            match client.groups().report_messages_to_admins(jid, &[message_id.to_owned()]).await {
                Ok(()) => events.emit(BackendEvent::MessageReportedToAdmins {
                    chat_jid: chat_jid.to_owned(),
                    message_id: message_id.to_owned(),
                }),
                Err(e) => fail(format!("Reporting to the admins failed: {}", error_words(&e))),
            }
        }
        Err(e) => fail(format!("Invalid group address: {}", error_words(&e))),
    }
}

/// The admin side of row 65: fetch the outstanding reports.
pub(super) async fn fetch_reported_messages(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    chat_jid: &str,
) {
    let fail = |message: String| {
        events
            .emit(BackendEvent::ReportedMessagesFailed { chat_jid: chat_jid.to_owned(), message });
    };
    match chat_jid.parse::<Jid>() {
        Ok(jid) => match client.groups().get_reported_messages(jid).await {
            Ok(reported) => {
                let reports = reported
                    .reports
                    .iter()
                    .map(|report| crate::ReportedMessageEntry {
                        message_id: report.message_id.clone(),
                        reporters: report.reporters.iter().map(|r| r.jid.to_string()).collect(),
                    })
                    .collect();
                events.emit(BackendEvent::ReportedMessages {
                    chat_jid: chat_jid.to_owned(),
                    reports,
                });
            }
            Err(e) => fail(format!("Fetching the reports failed: {}", error_words(&e))),
        },
        Err(e) => fail(format!("Invalid group address: {}", error_words(&e))),
    }
}
