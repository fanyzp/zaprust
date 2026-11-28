//! [`WhatsappBackend`], the real protocol implementation, built on
//! the whatsapp-rust library (pinned by git revision; see this
//! crate's `Cargo.toml` for why).
//!
//! Shape: a Tokio runtime on the backend worker thread; the library's
//! `Bot` runs as a task; UI commands arrive over a std channel and are
//! bridged in. All library event types are translated into
//! [`BackendEvent`]s here, nothing from the library crosses this
//! module boundary.
//!
//! The library's `SqliteStore` holds protocol/session state only
//! (Signal keys, device state). Chats and messages are archived by
//! the app's own `store` crate from the events emitted here.

use std::sync::mpsc::Receiver;

use crate::{Backend, BackendCommand, BackendConfig, EventSink};

use session::run_async;

mod business;
mod calls;
mod channels;
mod groups;
mod profile;
mod send;
mod session;
mod translate;
mod util;

/// Budget for the runtime's own teardown once the session has
/// ended. Dropping a Tokio runtime waits indefinitely for its
/// spawned tasks, so a leftover task (a call pump, a library
/// background task) could hold the backend worker thread forever
/// after an otherwise clean session end. Past this budget the
/// runtime aborts what remains, which is the same state a
/// process exit would leave.
const RUNTIME_TEARDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The real WhatsApp backend. See the module docs.
#[derive(Debug, Default)]
pub struct WhatsappBackend;

impl Backend for WhatsappBackend {
    fn run(
        self: Box<Self>,
        config: &BackendConfig,
        commands: Receiver<BackendCommand>,
        events: EventSink,
    ) -> Result<(), String> {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("zaprust-whatsapp")
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                return Err(format!("Could not start the async runtime: {e}"));
            }
        };
        // Set by the session when it ends in a logout (the
        // Disconnect command, or the library reporting the
        // device was unlinked): the account's session directory
        // must not outlive the link (privacy audit A8).
        let logged_out = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result = runtime.block_on(run_async(config, commands, events, logged_out.clone()));
        // The session is over; bound the runtime teardown itself
        // (see RUNTIME_TEARDOWN_TIMEOUT) instead of letting the
        // runtime's drop wait on leftover tasks without a limit.
        runtime.shutdown_timeout(RUNTIME_TEARDOWN_TIMEOUT);
        if logged_out.load(std::sync::atomic::Ordering::SeqCst) {
            // Only now, with the runtime down and every session
            // connection closed, is the removal safe from being
            // recreated behind our back.
            session::remove_account_dir_after_logout(&config.account_dir());
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::send::{
        apply_view_once, outgoing_interactive_message, outgoing_interactive_response,
        outgoing_location_message, outgoing_text_message,
    };
    use super::util::received_poll_secret;
    use crate::MediaKind;
    use whatsapp_rust::prelude::*;

    fn poll_message(secret: Option<Vec<u8>>) -> wa::Message {
        let mut msg = wa::Message::default();
        msg.poll_creation_message = MessageField::some({
            let mut poll = wa::message::PollCreationMessage::default();
            poll.name = Some("Lunch?".to_owned());
            poll.options = vec![
                {
                    let mut o = wa::message::poll_creation_message::Option::default();
                    o.option_name = Some("Nasi".to_owned());
                    o
                },
                {
                    let mut o = wa::message::poll_creation_message::Option::default();
                    o.option_name = Some("Mie".to_owned());
                    o
                },
            ];
            poll
        });
        if let Some(secret) = secret {
            msg.message_context_info = MessageField::some({
                let mut ctx = wa::MessageContextInfo::default();
                ctx.message_secret = Some(secret);
                ctx
            });
        }
        msg
    }

    #[test]
    fn received_poll_secret_comes_from_the_context_info() {
        let msg = poll_message(Some(vec![7u8; 32]));
        let (secret, names) = received_poll_secret(&msg).expect("secret extracted");
        assert_eq!(secret, vec![7u8; 32]);
        assert_eq!(names, ["Nasi", "Mie"]);
    }

    #[test]
    fn received_poll_secret_is_none_without_a_secret_or_poll() {
        assert!(received_poll_secret(&poll_message(None)).is_none());
        assert!(received_poll_secret(&poll_message(Some(Vec::new()))).is_none());
        assert!(received_poll_secret(&wa::Message::default()).is_none());
    }

    fn quoted(message_id: &str, sender: Option<&str>, excerpt: &str) -> crate::QuotedRef {
        crate::QuotedRef {
            message_id: message_id.to_owned(),
            sender_jid: sender.map(str::to_owned),
            excerpt: excerpt.to_owned(),
            remote_jid: None,
        }
    }

    #[test]
    fn reply_text_carries_the_quote_context() {
        let jid = "1203@g.us".parse().unwrap();
        let msg = outgoing_text_message(
            "my answer",
            Some(&quoted("wamid.1", Some("12025550111@s.whatsapp.net"), "the question")),
            &jid,
        );
        let text = msg.extended_text_message.as_option().expect("extended text");
        let ctx = text.context_info.as_option().expect("context");
        assert_eq!(ctx.stanza_id.as_deref(), Some("wamid.1"));
        assert_eq!(ctx.participant.as_deref(), Some("12025550111@s.whatsapp.net"));
        let quoted_msg = ctx.quoted_message.as_option().expect("quoted message");
        assert_eq!(quoted_msg.conversation.as_deref(), Some("the question"));
    }

    #[test]
    fn reply_without_an_author_falls_back_to_the_chat_jid() {
        let jid = "12025550112@s.whatsapp.net".parse().unwrap();
        let msg = outgoing_text_message("hi", Some(&quoted("wamid.2", None, "earlier")), &jid);
        let ctx = msg
            .extended_text_message
            .as_option()
            .and_then(|t| t.context_info.as_option())
            .expect("context");
        assert_eq!(ctx.participant.as_deref(), Some("12025550112@s.whatsapp.net"));
    }

    #[test]
    fn plain_text_sends_without_context() {
        let jid = "12025550112@s.whatsapp.net".parse().unwrap();
        let msg = outgoing_text_message("hello", None, &jid);
        assert_eq!(msg.conversation.as_deref(), Some("hello"));
        assert!(msg.extended_text_message.as_option().is_none());
    }

    #[test]
    fn view_once_wraps_photos_and_videos_only() {
        let photo = wa::Message::default();
        let wrapped = apply_view_once(photo, MediaKind::Photo, true);
        assert!(wrapped.view_once_message_v2.as_option().is_some());

        let video = wa::Message::default();
        let wrapped = apply_view_once(video, MediaKind::Video, true);
        assert!(wrapped.view_once_message_v2.as_option().is_some());

        let doc = wa::Message::default();
        let kept = apply_view_once(doc, MediaKind::Document, true);
        assert!(kept.view_once_message_v2.as_option().is_none());
        assert!(kept.document_message.as_option().is_none());

        let photo = wa::Message::default();
        let unwrapped = apply_view_once(photo, MediaKind::Photo, false);
        assert!(unwrapped.view_once_message_v2.as_option().is_none());
    }

    #[test]
    fn location_message_carries_degrees_and_optional_name() {
        let msg = outgoing_location_message("Rumah", -6.2, 106.8);
        let loc = msg.location_message.as_option().expect("location");
        assert_eq!(loc.degrees_latitude, Some(-6.2));
        assert_eq!(loc.degrees_longitude, Some(106.8));
        assert_eq!(loc.name.as_deref(), Some("Rumah"));

        let msg = outgoing_location_message("", 0.0, 0.0);
        let loc = msg.location_message.as_option().expect("location");
        assert_eq!(loc.name.as_deref(), None);
    }

    #[test]
    fn interactive_builders_carry_their_shapes() {
        use crate::{InteractiveSpec, LiveCard};
        // Buttons: positional ids, response type, footer rides.
        let msg = outgoing_interactive_message(&InteractiveSpec::Buttons {
            text: "Lunch?".to_owned(),
            footer: "Tap".to_owned(),
            buttons: vec!["Yes".to_owned(), "No".to_owned()],
        });
        let buttons = msg.buttons_message.as_option().expect("buttons message");
        assert_eq!(buttons.content_text.as_deref(), Some("Lunch?"));
        assert_eq!(buttons.footer_text.as_deref(), Some("Tap"));
        assert_eq!(buttons.buttons.len(), 2);
        assert_eq!(buttons.buttons[0].button_id.as_deref(), Some("1"));
        assert_eq!(buttons.buttons[1].button_id.as_deref(), Some("2"));

        // List: rows number across sections in order.
        let msg = outgoing_interactive_message(&InteractiveSpec::List {
            title: "Slots".to_owned(),
            description: String::new(),
            button_label: "See slots".to_owned(),
            sections: vec![
                ("Morning".to_owned(), vec![("09:00".to_owned(), String::new())]),
                ("Afternoon".to_owned(), vec![("13:00".to_owned(), String::new())]),
            ],
        });
        let list = msg.list_message.as_option().expect("list message");
        assert_eq!(list.title.as_deref(), Some("Slots"));
        assert_eq!(list.sections.len(), 2);
        assert_eq!(list.sections[0].rows[0].row_id.as_deref(), Some("1"));
        assert_eq!(list.sections[1].rows[0].row_id.as_deref(), Some("2"));

        // Carousel: one card, quick-reply params carry the label.
        let msg = outgoing_interactive_message(&InteractiveSpec::Carousel {
            body: "Pick one".to_owned(),
            cards: vec![LiveCard {
                title: "Card A".to_owned(),
                body: String::new(),
                image: None,
                buttons: vec!["Open".to_owned()],
            }],
        });
        let interactive = msg.interactive_message.as_option().expect("interactive message");
        let carousel = match interactive.interactive_message.as_ref() {
            Some(wa::message::interactive_message::InteractiveMessage::CarouselMessage(c)) => c,
            other => panic!("expected a carousel, got {other:?}"),
        };
        assert_eq!(carousel.cards.len(), 1);

        // Responses quote the original through the context info.
        let jid = super::util::parse_address("10000000002@g.us").expect("jid");
        let msg =
            outgoing_interactive_response(&jid, "m1", Some("100@s.whatsapp.net"), false, "2", "No");
        let response = msg.buttons_response_message.as_option().expect("buttons response");
        assert_eq!(response.selected_button_id.as_deref(), Some("2"));
        let context = response.context_info.as_option().expect("context");
        assert_eq!(context.stanza_id.as_deref(), Some("m1"));
        assert_eq!(context.participant.as_deref(), Some("100@s.whatsapp.net"));

        let msg = outgoing_interactive_response(&jid, "m2", None, true, "1", "09:00");
        let response = msg.list_response_message.as_option().expect("list response");
        let reply = response.single_select_reply.as_option().expect("single select reply");
        assert_eq!(reply.selected_row_id.as_deref(), Some("1"));
    }
}
