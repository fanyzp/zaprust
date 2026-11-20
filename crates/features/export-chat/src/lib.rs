//! Export Chat, a native Zaprust feature.
//!
//! Renders a conversation to Markdown. The host owns the destination
//! (it writes the file and tells the user where), and calls
//! [`export_markdown`] when the user runs the feature's registered
//! `command:export-chat`; the feature itself touches no files and no
//! network.
//!
//! Capabilities used: `ui.command` for the export action, `chats.read`
//! and `messages.read` because the host hands it chat + message data.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use core::model::{Chat, Message, MessageKind};

/// Renders a conversation as a Markdown document.
///
/// The header carries export metadata (chat name, export date,
/// message count). System notices are rendered as blockquote lines;
/// media messages are represented by a bracketed placeholder
/// describing the content (the export carries text, not media
/// bytes). Edited messages gain an `(edited)` marker; deleted ones
/// export as the standard placeholder, and messages whose archived
/// body survived a sender delete (older archives) are marked
/// `(deleted by sender, retained locally)`.
#[must_use]
pub fn export_markdown(chat: &Chat, messages: &[Message], exported_at: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", chat.title));
    if !chat.subtitle.is_empty() {
        out.push_str(&format!("_{}_\n\n", chat.subtitle));
    }
    out.push_str(&format!(
        "Exported by Zaprust · {exported_at} · {} messages\n\n---\n\n",
        messages.len()
    ));
    for msg in messages {
        if msg.kind == MessageKind::System {
            out.push_str(&format!("> _{}_\n\n", msg.body));
        } else {
            let who = msg.sender.as_deref().unwrap_or("Me");
            out.push_str(&format!("**{who}** ({}):\n\n", msg.time));
            if let (Some(qs), Some(qb)) = (&msg.quoted_sender, &msg.quoted_body) {
                out.push_str(&format!("> **{qs}:** {qb}\n\n"));
            }
            out.push_str(&body_text(msg));
            out.push_str("\n\n");
            if !msg.reactions.is_empty() {
                let rs: Vec<String> =
                    msg.reactions.iter().map(|(e, n)| format!("{e} ×{n}")).collect();
                out.push_str(&format!("Reactions: {}\n\n", rs.join(" · ")));
            }
        }
    }
    out
}

/// Renders a conversation as plain text (same content rules as
/// [`export_markdown`], no Markdown formatting).
#[must_use]
pub fn export_text(chat: &Chat, messages: &[Message], exported_at: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{}, exported by Zaprust · {exported_at} · {} messages\n\n",
        chat.title,
        messages.len()
    ));
    for msg in messages {
        if msg.kind == MessageKind::System {
            out.push_str(&format!("--- {} ---\n", msg.body));
        } else {
            let who = msg.sender.as_deref().unwrap_or("Me");
            out.push_str(&format!("[{}] {who}: {}\n", msg.time, body_text(msg)));
        }
    }
    out
}

fn body_text(msg: &Message) -> String {
    if msg.deleted {
        return "[this message was deleted]".to_string();
    }
    let mut text = match &msg.kind {
        MessageKind::Text | MessageKind::System => msg.body.clone(),
        MessageKind::Image { caption, view_once } => {
            if *view_once {
                format!("[view-once photo: {caption}]")
            } else {
                format!("[photo: {caption}]")
            }
        }
        MessageKind::Video { caption, secs, .. } => {
            if caption.is_empty() {
                format!("[video, {secs}s]")
            } else {
                format!("[video: {caption}, {secs}s]")
            }
        }
        MessageKind::Voice { secs, .. } => format!("[voice message, {secs}s]"),
        MessageKind::Document { name, size_kib } => format!("[document: {name}, {size_kib} KiB]"),
        MessageKind::Poll { question, options, .. } => {
            let opts: Vec<String> =
                options.iter().map(|(label, votes)| format!("{label}, {votes}")).collect();
            format!("[poll] {question} ({})", opts.join(", "))
        }
        MessageKind::Location { name, .. } => format!("[location: {name}]"),
        MessageKind::Contact { name, .. } => format!("[contact: {name}]"),
        MessageKind::Carousel { cards, .. } => format!("[carousel: {} cards]", cards.len()),
    };
    if msg.edited {
        text.push_str(" (edited)");
    }
    if msg.deleted_by_sender {
        text.push_str(" (deleted by sender, retained locally)");
    }
    text
}

/// A filesystem-safe `.md` file name for a chat export.
#[must_use]
pub fn export_filename(chat: &Chat) -> String {
    let slug: String = chat
        .title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    format!("zaprust-chat-{slug}.md")
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::model::{demo_chats, demo_conversation};

    #[test]
    fn export_contains_titles_and_messages() {
        let chats = demo_chats();
        let md = export_markdown(&chats[0], &demo_conversation(), "2026-11-14");
        assert!(md.starts_with("# GTA VI Launch Crew"));
        assert!(md.contains("**Sarah**"));
        assert!(md.contains("[poll]"));
        assert!(md.contains("[voice message, 42s]"));
    }

    #[test]
    fn export_fixture_has_metadata_order_and_markers() {
        use core::model::Message;
        let chats = demo_chats();
        let mut edited = Message::mine(1, "schedule moved to Thursday", "10:00");
        edited.edited = true;
        let mut deleted = Message::theirs(2, "Sarah", "a secret", "10:01");
        deleted.deleted = true;
        deleted.body = "This message was deleted".to_owned();
        let mut retained = Message::theirs(3, "Mike", "do not use the old code", "10:02");
        retained.deleted_by_sender = true;
        let msgs = vec![edited, deleted, retained];
        let md = export_markdown(&chats[0], &msgs, "2026-11-14");
        // Metadata header: chat name, export date, message count.
        assert!(md.contains("Exported by Zaprust · 2026-11-14 · 3 messages"), "{md}");
        // Order is preserved.
        let a = md.find("schedule moved").expect("first");
        let b = md.find("this message was deleted").expect("second");
        let c = md.find("do not use the old code").expect("third");
        assert!(a < b && b < c, "ordering: {a} {b} {c}");
        // Markers.
        assert!(md.contains("(edited)"), "{md}");
        assert!(md.contains("(deleted by sender, retained locally)"), "{md}");
        // The deleted body never leaks into the export.
        assert!(!md.contains("a secret"), "deleted body leaked: {md}");
        // Plain-text variant carries the same facts.
        let txt = export_text(&chats[0], &msgs, "2026-11-14");
        assert!(txt.contains("[10:00] Me: schedule moved to Thursday (edited)"), "{txt}");
        assert!(txt.contains("[this message was deleted]"), "{txt}");
    }

    #[test]
    fn filename_is_filesystem_safe() {
        let chats = demo_chats();
        assert_eq!(export_filename(&chats[0]), "zaprust-chat-gta-vi-launch-crew.md");
    }
}
