//! Chat-list filtering and searching, the logic behind the filter chips
//! and the navigation rail views (Chats / Channels / Groups are presets
//! over the same list, not separate data).

use crate::model::{Chat, ChatKind};

/// A chat-list filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ChatFilter {
    /// Everything except archived chats.
    #[default]
    All,
    /// Only chats with unread messages.
    Unread,
    /// Only one-to-one chats.
    Private,
    /// Only favorites.
    Favorites,
    /// Only groups.
    Groups,
    /// Only channels.
    Channels,
    /// Only archived chats (the main list hides them everywhere else).
    Archived,
}

impl ChatFilter {
    /// Whether a chat passes this filter.
    #[must_use]
    pub fn matches(self, chat: &Chat) -> bool {
        match self {
            Self::Archived => chat.archived,
            // Archived chats leave every other list until unarchived.
            _ if chat.archived => false,
            Self::All => true,
            Self::Unread => chat.unread > 0,
            Self::Private => chat.kind == ChatKind::Dm,
            Self::Favorites => chat.favorite,
            Self::Groups => chat.kind == ChatKind::Group,
            Self::Channels => chat.kind == ChatKind::Channel,
        }
    }
}

/// Filters and searches chats, keeping pinned chats first and otherwise
/// preserving the input order (which the provider keeps recency-sorted).
///
/// The search matches the chat title, the preview text, and the preview
/// sender, case-insensitively.
#[must_use]
pub fn filter_chats<'a>(chats: &'a [Chat], filter: ChatFilter, query: &str) -> Vec<&'a Chat> {
    let query = query.trim().to_lowercase();
    let mut matched: Vec<&Chat> = chats
        .iter()
        .filter(|c| filter.matches(c))
        .filter(|c| {
            query.is_empty()
                || c.title.to_lowercase().contains(&query)
                || c.preview.to_lowercase().contains(&query)
                || c.preview_sender.as_deref().is_some_and(|s| s.to_lowercase().contains(&query))
        })
        .collect();
    // Stable partition: pinned first, original order kept inside each part.
    matched.sort_by_key(|c| !c.pinned);
    matched
}

/// Total unread across chats (for the tray / rail badge).
#[must_use]
pub fn total_unread(chats: &[Chat]) -> u32 {
    chats.iter().map(|c| c.unread).sum()
}

/// How many archived chats still have unread messages (the
/// Archived rail badge count).
#[must_use]
pub fn archived_unread_count(chats: &[Chat]) -> u32 {
    chats.iter().filter(|c| c.archived && c.unread > 0).count() as u32
}

/// The one display format for a count on a badge or a count chip
/// (design standard: count formatting). Exact below 1000; above
/// that, compact with a floor and a plus: thousands as `NK+`
/// (2075 reads "2K+"), millions as `NM+`. A badge never renders
/// the raw integer at 1000 or beyond: four digits overflow the
/// pill, and the exact figure carries no information a badge
/// reader can act on. Every badge and count chip in the app
/// formats through here, so the rule cannot drift per surface.
#[must_use]
pub fn format_count(count: u64) -> String {
    match count {
        0..=999 => count.to_string(),
        1_000..=999_999 => format!("{}K+", count / 1_000),
        _ => format!("{}M+", count / 1_000_000),
    }
}

/// The text of a message its reader can see, for search
/// (design standard section 25): a text message's body, a
/// photo or video's caption, a document's file name. Every
/// other kind has no searchable text of its own (a poll's
/// question, a location's name and a kind label are not
/// message text), and a view-once message's caption is part
/// of its sealed payload, so it reads as empty here.
#[must_use]
pub fn visible_text(message: &crate::model::Message) -> &str {
    match &message.kind {
        crate::model::MessageKind::Text => &message.body,
        crate::model::MessageKind::Image { caption, view_once }
        | crate::model::MessageKind::Video { caption, view_once, .. } => {
            if *view_once {
                ""
            } else {
                caption
            }
        }
        crate::model::MessageKind::Document { name, .. } => name,
        _ => "",
    }
}

/// Whether a message matches a search query: its visible text
/// contains the query as a case-insensitive substring. The
/// query is trimmed first; an empty query matches nothing.
/// Deleted messages never match (their media and text are
/// gone; the placeholder is not content). This is the one
/// matcher every search surface uses, so the in-chat search,
/// the demo global search and the archive search can never
/// disagree about what a match is.
#[must_use]
pub fn message_text_matches(message: &crate::model::Message, query: &str) -> bool {
    let needle = query.trim().to_lowercase();
    !needle.is_empty() && !message.deleted && visible_text(message).to_lowercase().contains(&needle)
}

/// Call history for the Calls panel: newest first, optionally
/// only the missed calls. The input order does not matter; the
/// sort is stable, so entries with equal timestamps keep their
/// input order.
#[must_use]
pub fn filter_calls(
    entries: &[crate::model::CallEntry],
    missed_only: bool,
) -> Vec<&crate::model::CallEntry> {
    let mut out: Vec<&crate::model::CallEntry> =
        entries.iter().filter(|e| !missed_only || e.missed).collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.timestamp));
    out
}

/// Starred messages across `chats`, as (chat id, chat title,
/// message) triples in one flat list, newest first: the Starred
/// panel is a single chronological list, not grouped by chat.
///
/// The ordering key is built from the message's own display
/// labels, which are all the model carries: the day label
/// resolves to a day ordinal ("Today" and "Yesterday" rank above
/// every full date, because the label producer only ever writes
/// a full date for days before yesterday) and the "HH:MM" clock
/// to a minute of day. The sort is stable, so messages that tie
/// keep their collection order (chat list order, then stored
/// order within the chat).
#[must_use]
pub fn starred_messages(chats: &[Chat]) -> Vec<(u64, &str, &crate::model::Message)> {
    let mut out = Vec::new();
    for chat in chats {
        for msg in &chat.messages {
            if msg.starred {
                out.push((chat.id, chat.title.as_str(), msg));
            }
        }
    }
    out.sort_by_key(|entry| std::cmp::Reverse(starred_sort_key(entry.2)));
    out
}

/// The chronological key for one starred message: (day ordinal,
/// minute of day). Relative day labels get ordinals far above
/// any real civil date, keeping "Today" first and "Yesterday"
/// right behind it; a full-date label ("14 November 2026", the
/// shape the app's day formatter writes) converts to its true
/// days-from-civil ordinal, and anything unparseable sorts as
/// the oldest day with minute 0.
fn starred_sort_key(msg: &crate::model::Message) -> (i64, i64) {
    const RELATIVE_TODAY: i64 = 1_000_000_000;
    let day = match msg.day_label() {
        "Today" => RELATIVE_TODAY,
        "Yesterday" => RELATIVE_TODAY - 1,
        label => parse_full_date(label).map_or(0, |(y, m, d)| days_from_civil(y, m, d)),
    };
    let minute = parse_clock(&msg.time).map_or(0, |(h, m)| i64::from(h) * 60 + i64::from(m));
    (day, minute)
}

/// The days-from-civil ordinal of a calendar date, the same
/// scale [`starred_sort_key`] orders by, exposed so a date
/// jump can express its target on that scale.
#[must_use]
pub fn civil_day_ordinal(y: i64, m: u32, d: u32) -> i64 {
    days_from_civil(y, m, d)
}

/// The day ordinal of one message's day label, on the
/// days-from-civil scale, with the relative labels resolved
/// against the caller's today: "Today" is `today_ordinal`,
/// "Yesterday" one below it, a full-date label its civil
/// ordinal. `None` when the label names no day at all.
#[must_use]
pub fn message_day_ordinal(message: &crate::model::Message, today_ordinal: i64) -> Option<i64> {
    match message.day_label() {
        "Today" => Some(today_ordinal),
        "Yesterday" => Some(today_ordinal - 1),
        label => parse_full_date(label).map(|(y, m, d)| days_from_civil(y, m, d)),
    }
}

/// The index a jump to a day lands on (design standard
/// section 25): the first message whose day is at or after
/// the target day. A target past the newest message lands on
/// the last message; a target before the archive lands on
/// the first message at or after it, which is the oldest
/// message of the earliest reachable day. Messages are
/// assumed in conversation (chronological) order. `None`
/// for an empty conversation.
#[must_use]
pub fn jump_to_date_index(
    messages: &[crate::model::Message],
    target_ordinal: i64,
    today_ordinal: i64,
) -> Option<usize> {
    if messages.is_empty() {
        return None;
    }
    for (i, message) in messages.iter().enumerate() {
        if let Some(day) = message_day_ordinal(message, today_ordinal) {
            if day >= target_ordinal {
                return Some(i);
            }
        }
    }
    Some(messages.len() - 1)
}

/// Parses a "14 November 2026" day label into (year, month, day),
/// with the full English month names the day formatter writes.
fn parse_full_date(label: &str) -> Option<(i64, u32, u32)> {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let mut parts = label.split_whitespace();
    let d: u32 = parts.next()?.parse().ok()?;
    let month = parts.next()?;
    let m = MONTHS.iter().position(|name| *name == month)? as u32 + 1;
    let y: i64 = parts.next()?.parse().ok()?;
    Some((y, m, d))
}

/// Parses an "HH:MM" display clock into (hour, minute).
fn parse_clock(time: &str) -> Option<(u32, u32)> {
    let (h, m) = time.split_once(':')?;
    Some((h.parse().ok()?, m.parse().ok()?))
}

/// Days since the unix epoch for a civil date (Howard Hinnant's
/// days-from-civil, the inverse of the conversion the app uses
/// to compose day labels).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::demo_chats;

    #[test]
    fn search_matches_visible_text_only() {
        use crate::model::{Message, MessageKind};
        let mut text = Message::theirs(1, "Ana", "The report is ready", "10:00");
        assert!(message_text_matches(&text, "REPORT"), "case-insensitive substring");
        assert!(!message_text_matches(&text, "invoice"));
        assert!(!message_text_matches(&text, "  "), "an empty query matches nothing");
        // A photo's caption is its searchable text.
        text.kind = MessageKind::Image { caption: "report cover".to_owned(), view_once: false };
        text.body = "Photo".to_owned();
        assert!(message_text_matches(&text, "report"));
        assert!(!message_text_matches(&text, "photo"), "the kind label is not content");
        // A view-once caption is sealed: never a match.
        text.kind = MessageKind::Image { caption: "report cover".to_owned(), view_once: true };
        assert!(!message_text_matches(&text, "report"));
        // A document matches by file name.
        text.kind = MessageKind::Document { name: "report.pdf".to_owned(), size_bytes: 12 * 1024, page_count: None };
        assert!(message_text_matches(&text, "report"));
        // A deleted message never matches.
        text.deleted = true;
        assert!(!message_text_matches(&text, "report"));
    }

    #[test]
    fn date_jump_lands_on_the_first_message_of_the_day_or_after() {
        use crate::model::Message;
        let today = civil_day_ordinal(2026, 11, 23);
        let mut msgs: Vec<Message> = Vec::new();
        for (i, day) in
            ["20 November 2026", "20 November 2026", "22 November 2026", "Today"].iter().enumerate()
        {
            let mut m = Message::theirs(i as u64 + 1, "Ana", "x", "10:00");
            m.day = (*day).to_owned();
            msgs.push(m);
        }
        // Exact day: its first message.
        assert_eq!(jump_to_date_index(&msgs, civil_day_ordinal(2026, 11, 20), today), Some(0));
        // A day with no messages: the next day that has some.
        assert_eq!(jump_to_date_index(&msgs, civil_day_ordinal(2026, 11, 21), today), Some(2));
        // Before the archive: the oldest message.
        assert_eq!(jump_to_date_index(&msgs, civil_day_ordinal(2026, 11, 1), today), Some(0));
        // After the archive: the newest message.
        assert_eq!(jump_to_date_index(&msgs, civil_day_ordinal(2027, 1, 1), today), Some(3));
        // Today resolves through the relative label.
        assert_eq!(jump_to_date_index(&msgs, today, today), Some(3));
        // Empty conversation: no landing.
        assert_eq!(jump_to_date_index(&[], today, today), None);
    }

    #[test]
    fn counts_format_exact_below_a_thousand_and_compact_above() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(7), "7");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1K+");
        assert_eq!(format_count(2_075), "2K+");
        assert_eq!(format_count(999_999), "999K+");
        assert_eq!(format_count(1_000_000), "1M+");
        assert_eq!(format_count(2_500_000), "2M+");
    }

    #[test]
    fn pinned_comes_first() {
        let chats = demo_chats();
        let out = filter_chats(&chats, ChatFilter::All, "");
        assert!(out[0].pinned);
        assert_eq!(out.len(), chats.len());
    }

    #[test]
    fn unread_filter_only_keeps_unread() {
        let chats = demo_chats();
        let out = filter_chats(&chats, ChatFilter::Unread, "");
        assert!(!out.is_empty());
        assert!(out.iter().all(|c| c.unread > 0));
    }

    #[test]
    fn rail_views_are_presets() {
        let chats = demo_chats();
        let groups = filter_chats(&chats, ChatFilter::Groups, "");
        assert!(groups.iter().all(|c| c.kind == ChatKind::Group));
        let channels = filter_chats(&chats, ChatFilter::Channels, "");
        assert!(channels.iter().all(|c| c.kind == ChatKind::Channel));
        let private = filter_chats(&chats, ChatFilter::Private, "");
        assert!(private.iter().all(|c| c.kind == ChatKind::Dm));
    }

    #[test]
    fn search_matches_title_preview_and_sender() {
        let chats = demo_chats();
        assert_eq!(filter_chats(&chats, ChatFilter::All, "launch").len(), 1);
        assert_eq!(filter_chats(&chats, ChatFilter::All, "THURSDAY").len(), 1);
        assert_eq!(filter_chats(&chats, ChatFilter::All, "yoga").len(), 1);
        assert!(filter_chats(&chats, ChatFilter::All, "no-such-chat").is_empty());
    }

    #[test]
    fn total_unread_sums() {
        let chats = demo_chats();
        let manual: u32 = chats.iter().map(|c| c.unread).sum();
        assert_eq!(total_unread(&chats), manual);
        assert!(manual > 0);
    }

    #[test]
    fn archived_unread_count_only_counts_archived_with_unread() {
        let mut chats = demo_chats();
        assert_eq!(archived_unread_count(&chats), 0);
        // Archive a chat with unread and one without.
        for c in &mut chats {
            if c.id == 4 {
                c.archived = true; // unread 2
            }
            if c.id == 11 {
                c.archived = true; // unread 0
            }
        }
        assert_eq!(archived_unread_count(&chats), 1);
    }

    #[test]
    fn starred_messages_flat_newest_first_across_chats() {
        let mut chats = demo_chats();
        for c in &mut chats {
            c.messages = crate::model::demo_messages_for(c.id);
        }
        // Star one message in each of chats 1, 2 and 3: 102 is
        // Yesterday 17:35, 3003 is Today 17:05, 2003 is Today
        // 17:48 (the demo sets chat 1's early days to Yesterday
        // and leaves the other chats' days empty, which the day
        // label resolves to Today).
        for c in &mut chats {
            for m in &mut c.messages {
                if matches!(m.id, 102 | 2003 | 3003) {
                    m.starred = true;
                }
            }
        }
        let out = starred_messages(&chats);
        assert_eq!(out.len(), 3);
        // One flat chronological list, newest first, not grouped
        // by chat: today's later clock beats today's earlier one,
        // and both beat yesterday's.
        assert_eq!(out[0].2.id, 2003);
        assert_eq!(out[1].2.id, 3003);
        assert_eq!(out[2].2.id, 102);
        // The triples still carry their source chat.
        assert_eq!(out[0].0, 2);
        assert_eq!(out[1].0, 3);
        assert_eq!(out[2].0, 1);
        // Unstarred messages are skipped.
        assert!(out.iter().all(|(_, _, m)| m.starred));
    }

    #[test]
    fn starred_messages_orders_full_date_labels_by_calendar() {
        let mut chats = demo_chats();
        for c in &mut chats {
            c.messages = crate::model::demo_messages_for(c.id);
        }
        // Give the starred messages full-date day labels: the
        // calendar date decides first, the clock breaks the tie
        // between the two messages sharing a date.
        for c in &mut chats {
            for m in &mut c.messages {
                match m.id {
                    102 => {
                        m.starred = true;
                        m.day = "10 November 2026".to_owned();
                        m.time = "23:59".to_owned();
                    }
                    2003 => {
                        m.starred = true;
                        m.day = "14 November 2026".to_owned();
                        m.time = "09:00".to_owned();
                    }
                    3003 => {
                        m.starred = true;
                        m.day = "14 November 2026".to_owned();
                        m.time = "18:00".to_owned();
                    }
                    _ => {}
                }
            }
        }
        let out = starred_messages(&chats);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].2.id, 3003);
        assert_eq!(out[1].2.id, 2003);
        assert_eq!(out[2].2.id, 102);
    }

    #[test]
    fn calls_sort_newest_first_and_filter_missed() {
        use crate::model::{CallDirection, CallEntry, CallKind};
        let entry = |id: u64, missed: bool, timestamp: i64| CallEntry {
            id,
            chat_id: 2,
            kind: CallKind::Voice,
            direction: CallDirection::Incoming,
            missed,
            duration_secs: 0,
            timestamp,
        };
        // Deliberately unordered input.
        let entries = vec![
            entry(1, false, 100),
            entry(2, true, 300),
            entry(3, false, 200),
            entry(4, true, 50),
        ];
        let all = filter_calls(&entries, false);
        assert_eq!(
            all.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![2, 3, 1, 4],
            "newest first regardless of input order"
        );
        let missed = filter_calls(&entries, true);
        assert_eq!(missed.iter().map(|e| e.id).collect::<Vec<_>>(), vec![2, 4]);
        assert!(missed.iter().all(|e| e.missed));
    }

    #[test]
    fn starred_messages_empty_when_nothing_starred() {
        let chats = demo_chats();
        assert!(starred_messages(&chats).is_empty());
    }
}

#[cfg(test)]
mod archived_tests {
    use super::*;
    use crate::model::demo_chats;

    #[test]
    fn archived_chats_only_show_under_the_archived_filter() {
        let mut chats = demo_chats();
        chats[1].archived = true;
        let all = filter_chats(&chats, ChatFilter::All, "");
        assert!(!all.iter().any(|c| c.id == chats[1].id));
        let unread = filter_chats(&chats, ChatFilter::Unread, "");
        assert!(!unread.iter().any(|c| c.id == chats[1].id));
        let archived = filter_chats(&chats, ChatFilter::Archived, "");
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].id, chats[1].id);
    }
}
