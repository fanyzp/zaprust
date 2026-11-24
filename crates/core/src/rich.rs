//! Rich-message logic shared by the demo driver and the live
//! driver, so both follow ONE state machine: poll voting, reaction
//! toggling, and link-preview parsing.
//!
//! Everything here is pure and deterministic, the app applies
//! these reducers to local state immediately (optimistic UI) and
//! the backend echoes authoritative state back as upserts.

/// Applies my vote for option `idx` to a poll.
///
/// Returns the new `(options, my_votes)`. Rules:
///
/// - single-choice: voting a different option moves my vote
///   (old option −1, new option +1); voting the same option again
///   retracts it;
/// - multiple-choice: voting toggles that option only.
///
/// Total vote counts change by at most one per call, and my votes
/// always point at options whose counts include me.
#[must_use]
pub fn apply_vote(
    options: &[(String, u32)],
    my_votes: &[usize],
    multiple: bool,
    idx: usize,
) -> (Vec<(String, u32)>, Vec<usize>) {
    let mut options = options.to_vec();
    let mut mine: Vec<usize> = my_votes.to_vec();
    if idx >= options.len() {
        return (options, mine);
    }
    if mine.contains(&idx) {
        // Retract.
        options[idx].1 = options[idx].1.saturating_sub(1);
        mine.retain(|&i| i != idx);
        return (options, mine);
    }
    if !multiple {
        for old in mine.drain(..) {
            if old < options.len() {
                options[old].1 = options[old].1.saturating_sub(1);
            }
        }
    }
    options[idx].1 += 1;
    mine.push(idx);
    mine.sort_unstable();
    (options, mine)
}

/// A poll's aggregated state: the per-option tallies, my own
/// current selection, and the latest ballot per voter the
/// tallies were derived from.
pub struct PollTally {
    /// (label, votes) pairs, in poll order.
    pub options: Vec<(String, u32)>,
    /// Option indices I currently vote for.
    pub my_votes: Vec<usize>,
    /// Latest ballot per voter, keyed by voter JID.
    pub ballots: Vec<(String, Vec<usize>)>,
}

/// Applies one voter's ballot to a poll's tallies, replacing that
/// voter's previous ballot (last-vote-wins, the protocol's rule:
/// a vote message carries the voter's whole current selection,
/// and an empty selection clears the vote).
///
/// `ballots` holds the latest known selection per voter, keyed by
/// voter JID; my own ballot lives in `my_votes` instead and is the
/// one replaced when `from_me` is set. Counts move by the
/// difference between the voter's old and new selections only, so
/// re-applying the same ballot is a no-op and a voter who changes
/// their mind never counts twice. Selections pointing outside
/// `options` are dropped. Returns the new tally state.
#[must_use]
pub fn apply_poll_ballot(
    options: &[(String, u32)],
    my_votes: &[usize],
    ballots: &[(String, Vec<usize>)],
    voter: &str,
    from_me: bool,
    selected: &[usize],
) -> PollTally {
    let mut options = options.to_vec();
    let mut mine: Vec<usize> = my_votes.to_vec();
    let mut ballots: Vec<(String, Vec<usize>)> = ballots.to_vec();
    let mut selected: Vec<usize> =
        selected.iter().copied().filter(|i| *i < options.len()).collect();
    selected.sort_unstable();
    selected.dedup();
    let old: Vec<usize> = if from_me {
        mine.clone()
    } else {
        ballots.iter().find(|(v, _)| v == voter).map(|(_, s)| s.clone()).unwrap_or_default()
    };
    for idx in &old {
        if let Some(entry) = options.get_mut(*idx) {
            entry.1 = entry.1.saturating_sub(1);
        }
    }
    for idx in &selected {
        options[*idx].1 = options[*idx].1.saturating_add(1);
    }
    if from_me {
        mine = selected;
    } else if selected.is_empty() {
        ballots.retain(|(v, _)| v != voter);
    } else if let Some(entry) = ballots.iter_mut().find(|(v, _)| v == voter) {
        entry.1 = selected;
    } else {
        ballots.push((voter.to_owned(), selected));
    }
    PollTally { options, my_votes: mine, ballots }
}

/// An event's aggregated state: the response tallies, my own
/// current response, and the latest response per responder the
/// tallies were derived from.
#[derive(Debug, Clone, PartialEq)]
pub struct EventTally {
    /// Responses counted Going.
    pub going: u32,
    /// Responses counted Maybe.
    pub maybe: u32,
    /// Responses counted Not going.
    pub not_going: u32,
    /// My current response, when I have answered.
    pub my_rsvp: Option<crate::model::EventRsvp>,
    /// Latest response per responder (excluding me), keyed by
    /// responder JID.
    pub ballots: Vec<(String, crate::model::EventRsvp)>,
}

/// Applies one responder's answer to an event's tallies,
/// replacing that responder's previous answer (last-response-
/// wins, the protocol's rule: a response message carries the
/// responder's whole current answer). The tallies are
/// recomputed from the ballots plus my own answer, so
/// re-applying the same answer is a no-op and a responder who
/// changes their mind never counts twice. My own ballot lives
/// in `my_rsvp`, never in `ballots` (the same split
/// [`apply_poll_ballot`] keeps for polls).
#[must_use]
pub fn apply_event_response(
    my_rsvp: Option<crate::model::EventRsvp>,
    ballots: &[(String, crate::model::EventRsvp)],
    responder: &str,
    from_me: bool,
    response: crate::model::EventRsvp,
) -> EventTally {
    let mut mine = my_rsvp;
    let mut ballots: Vec<(String, crate::model::EventRsvp)> = ballots.to_vec();
    if from_me {
        mine = Some(response);
    } else if let Some(entry) = ballots.iter_mut().find(|(v, _)| v == responder) {
        entry.1 = response;
    } else {
        ballots.push((responder.to_owned(), response));
    }
    let (mut going, mut maybe, mut not_going) = (0_u32, 0_u32, 0_u32);
    let mut count = |r: crate::model::EventRsvp| match r {
        crate::model::EventRsvp::Going => going += 1,
        crate::model::EventRsvp::Maybe => maybe += 1,
        crate::model::EventRsvp::NotGoing => not_going += 1,
    };
    for (_, r) in &ballots {
        count(*r);
    }
    if let Some(r) = mine {
        count(r);
    }
    EventTally { going, maybe, not_going, my_rsvp: mine, ballots }
}

/// Retracts my own event response (the rollback twin of
/// [`apply_event_response`]): my answer is cleared and the
/// tallies are recomputed from the remaining ballots.
#[must_use]
pub fn retract_event_response(ballots: &[(String, crate::model::EventRsvp)]) -> EventTally {
    let (mut going, mut maybe, mut not_going) = (0_u32, 0_u32, 0_u32);
    for (_, r) in ballots {
        match r {
            crate::model::EventRsvp::Going => going += 1,
            crate::model::EventRsvp::Maybe => maybe += 1,
            crate::model::EventRsvp::NotGoing => not_going += 1,
        }
    }
    EventTally { going, maybe, not_going, my_rsvp: None, ballots: ballots.to_vec() }
}

/// Days since the unix epoch for a civil date (Howard
/// Hinnant's days-from-civil, the inverse of the civil-from-
/// days conversion the app displays with).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + (d - 1) as i64;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parses the event form's strict date and time text
/// (`YYYY-MM-DD`, `HH:MM`) into unix seconds on the UTC grid
/// the whole app displays on. Anything malformed, out of
/// range, or not a real calendar day (31 February, month 13)
/// is None: the form keeps the draft and shows the error
/// instead of sending a guessed instant.
#[must_use]
pub fn parse_event_datetime(date: &str, time: &str) -> Option<i64> {
    let mut dparts = date.split('-');
    let year: i64 = dparts.next()?.parse().ok()?;
    let month: u32 = dparts.next()?.parse().ok()?;
    let day: u32 = dparts.next()?.parse().ok()?;
    if dparts.next().is_some() || !(1..=12).contains(&month) || day == 0 {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_len = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        [(month - 1) as usize];
    if day > month_len {
        return None;
    }
    let mut tparts = time.split(':');
    let hour: i64 = tparts.next()?.parse().ok()?;
    let minute: i64 = tparts.next()?.parse().ok()?;
    if tparts.next().is_some() || !(0..24).contains(&hour) || !(0..60).contains(&minute) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60)
}

/// Percentage (0–100, rounded) of the total vote for each option.
#[must_use]
pub fn poll_percents(options: &[(String, u32)]) -> Vec<u32> {
    let total: u32 = options.iter().map(|(_, v)| v).sum();
    if total == 0 {
        return vec![0; options.len()];
    }
    options
        .iter()
        .map(|(_, v)| ((u64::from(*v) * 100 + u64::from(total) / 2) / u64::from(total)) as u32)
        .collect()
}

/// Applies tapping `tapped` in the reaction picker, given my
/// current reaction (`None` = none). Returns the new aggregate
/// counts and my new reaction. Tapping my current reaction removes
/// it; tapping another moves mine.
#[must_use]
pub fn apply_my_reaction(
    reactions: &[(String, u32)],
    current_mine: Option<&str>,
    tapped: &str,
) -> (Vec<(String, u32)>, Option<String>) {
    let mut out: Vec<(String, u32)> = reactions.to_vec();
    let adjust = |emoji: &str, delta: i64, out: &mut Vec<(String, u32)>| {
        if let Some(entry) = out.iter_mut().find(|(e, _)| e == emoji) {
            entry.1 = (i64::from(entry.1) + delta).max(0) as u32;
        } else if delta > 0 {
            out.push((emoji.to_string(), delta as u32));
        }
    };
    if current_mine == Some(tapped) {
        adjust(tapped, -1, &mut out);
        out.retain(|(_, n)| *n > 0);
        return (out, None);
    }
    if let Some(old) = current_mine {
        adjust(old, -1, &mut out);
    }
    adjust(tapped, 1, &mut out);
    out.retain(|(_, n)| *n > 0);
    (out, Some(tapped.to_string()))
}

/// The first `http(s)` URL in a text, if any (used for link
/// previews). Trailing punctuation is trimmed.
#[must_use]
pub fn first_url(text: &str) -> Option<String> {
    for word in text.split_whitespace() {
        if word.starts_with("https://") || word.starts_with("http://") {
            let trimmed = word.trim_end_matches(['.', ',', ')', ']', '!', '?']);
            return Some(trimmed.to_string());
        }
    }
    None
}

/// A parsed OpenGraph preview (title + optional description).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OgData {
    /// `og:title` (or `<title>` fallback).
    pub title: String,
    /// `og:description` / meta description, if present.
    pub description: String,
}

/// Parses OpenGraph metadata out of an HTML document. Deliberately
/// small: string search over `<meta>` tags, `property` or `name`
/// attributes in either order, single or double quotes. Returns
/// `None` when no title can be found.
#[must_use]
pub fn parse_open_graph(html: &str) -> Option<OgData> {
    let title = meta_content(html, "og:title").or_else(|| html_title(html))?;
    let description = meta_content(html, "og:description")
        .or_else(|| meta_content(html, "description"))
        .unwrap_or_default();
    Some(OgData { title: decode_entities(&title), description: decode_entities(&description) })
}

/// Byte offset of `needle` in `hay` at or after `from`, comparing ASCII
/// case-insensitively. All needles used here are ASCII, so byte offsets
/// stay valid in the original string (no length-changing lowercase pass).
/// Shared with the crash scrubbers, which need the same guarantee.
pub(crate) fn find_ci(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let hay_b = hay.as_bytes();
    let needle_b = needle.as_bytes();
    if needle_b.is_empty() || hay_b.len() < needle_b.len() || from > hay_b.len() - needle_b.len() {
        return None;
    }
    (from..=hay_b.len() - needle_b.len())
        .find(|&i| hay_b[i..i + needle_b.len()].eq_ignore_ascii_case(needle_b))
}

fn meta_content(html: &str, key: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(start) = find_ci(html, "<meta", search_from) {
        let end = html[start..].find('>').map_or(html.len(), |e| start + e + 1);
        let tag = &html[start..end];
        if find_ci(tag, &format!("\"{key}\""), 0).is_some()
            || find_ci(tag, &format!("'{key}'"), 0).is_some()
        {
            if let Some(content) = attr_value(tag, "content") {
                return Some(content);
            }
        }
        search_from = end;
    }
    None
}

fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=");
    let pos = find_ci(tag, &needle, 0)? + needle.len();
    let rest = &tag[pos..];
    let first = rest.chars().next()?;
    if first == '"' || first == '\'' {
        rest[1..].find(first).map(|e| rest[1..=e].to_string())
    } else {
        let end = rest.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

fn html_title(html: &str) -> Option<String> {
    let start = find_ci(html, "<title>", 0)? + "<title>".len();
    let end = find_ci(html, "</title>", start)?;
    let raw = html[start..end].trim();
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Vec<(String, u32)> {
        vec![("A".to_string(), 3), ("B".to_string(), 5), ("C".to_string(), 0)]
    }

    #[test]
    fn single_choice_vote_moves_exactly_one_vote() {
        let (opts, mine) = apply_vote(&options(), &[], false, 1);
        assert_eq!(opts[1].1, 6);
        assert_eq!(mine, vec![1]);
        // Switch: B loses one, C gains one, total conserved.
        let (opts, mine) = apply_vote(&opts, &mine, false, 2);
        assert_eq!(opts[1].1, 5);
        assert_eq!(opts[2].1, 1);
        assert_eq!(opts.iter().map(|(_, v)| v).sum::<u32>(), 9);
        assert_eq!(mine, vec![2]);
        // Retract by voting the same option again.
        let (opts, mine) = apply_vote(&opts, &mine, false, 2);
        assert_eq!(opts[2].1, 0);
        assert!(mine.is_empty());
    }

    #[test]
    fn multiple_choice_toggles_independently() {
        let (opts, mine) = apply_vote(&options(), &[], true, 0);
        let (opts, mine) = apply_vote(&opts, &mine, true, 2);
        assert_eq!(mine, vec![0, 2]);
        assert_eq!(opts[0].1, 4);
        assert_eq!(opts[2].1, 1);
        let (opts, mine) = apply_vote(&opts, &mine, true, 0);
        assert_eq!(mine, vec![2]);
        assert_eq!(opts[0].1, 3);
    }

    #[test]
    fn poll_ballot_replaces_the_voters_previous_selection() {
        let ballots: Vec<(String, Vec<usize>)> = Vec::new();
        // A voter's first ballot adds to the baseline counts.
        let PollTally { options: opts, my_votes: mine, ballots } =
            apply_poll_ballot(&options(), &[], &ballots, "voter@s.whatsapp.net", false, &[1]);
        assert_eq!(opts[1].1, 6);
        assert!(mine.is_empty());
        // The same voter changing their mind moves the vote, it
        // never counts twice.
        let PollTally { options: opts, ballots, .. } =
            apply_poll_ballot(&opts, &mine, &ballots, "voter@s.whatsapp.net", false, &[2]);
        assert_eq!(opts[1].1, 5);
        assert_eq!(opts[2].1, 1);
        assert_eq!(opts.iter().map(|(_, v)| v).sum::<u32>(), 9);
        // Re-applying the identical ballot is a no-op.
        let PollTally { options: opts, ballots, .. } =
            apply_poll_ballot(&opts, &[], &ballots, "voter@s.whatsapp.net", false, &[2]);
        assert_eq!(opts.iter().map(|(_, v)| v).sum::<u32>(), 9);
        // An empty selection clears the ballot and its count.
        let PollTally { options: opts, ballots, .. } =
            apply_poll_ballot(&opts, &[], &ballots, "voter@s.whatsapp.net", false, &[]);
        assert_eq!(opts[2].1, 0);
        assert!(ballots.is_empty());
    }

    #[test]
    fn poll_ballot_from_me_replaces_my_votes_not_a_ballot() {
        let PollTally { options: opts, my_votes: mine, ballots } =
            apply_poll_ballot(&options(), &[0], &[], "10000000001@s.whatsapp.net", true, &[2]);
        assert_eq!(opts[0].1, 2);
        assert_eq!(opts[2].1, 1);
        assert_eq!(mine, vec![2]);
        assert!(ballots.is_empty());
        // Out-of-range selections are dropped, never counted; the
        // ballot they replace is still retracted.
        let PollTally { options: opts, my_votes: mine, .. } =
            apply_poll_ballot(&opts, &mine, &ballots, "x", true, &[9]);
        assert!(mine.is_empty());
        assert_eq!(opts.iter().map(|(_, v)| v).sum::<u32>(), 7);
    }

    #[test]
    fn poll_percents_round_and_handle_zero() {
        assert_eq!(poll_percents(&options()), vec![38, 63, 0]);
        assert_eq!(poll_percents(&[("X".into(), 0)]), vec![0]);
    }

    #[test]
    fn reaction_toggle_adds_moves_and_removes() {
        let base = vec![("👍".to_string(), 2u32)];
        let (out, mine) = apply_my_reaction(&base, None, "❤️");
        assert_eq!(mine.as_deref(), Some("❤️"));
        assert!(out.contains(&("❤️".to_string(), 1)));
        // Move mine from ❤️ to 👍.
        let (out, mine) = apply_my_reaction(&out, mine.as_deref(), "👍");
        assert_eq!(mine.as_deref(), Some("👍"));
        assert!(!out.iter().any(|(e, _)| e == "❤️"), "old reaction removed at zero");
        assert!(out.contains(&("👍".to_string(), 3)));
        // Tap again to remove.
        let (out, mine) = apply_my_reaction(&out, mine.as_deref(), "👍");
        assert_eq!(mine, None);
        assert!(out.contains(&("👍".to_string(), 2)));
    }

    #[test]
    fn first_url_trims_trailing_punctuation() {
        assert_eq!(
            first_url("cek https://example.com/a?b=1, bagus").as_deref(),
            Some("https://example.com/a?b=1")
        );
        assert_eq!(first_url("no link in this text"), None);
    }

    #[test]
    fn open_graph_survives_unicode_before_the_meta_tags() {
        // U+0130 lowercases to two chars; a lowercase-then-slice parser
        // panics or misreads here. The parser must keep original offsets.
        let html = "<html><head><title>\u{130}stanbul Guide</title><meta property=\"og:description\" content=\"A City Guide\"></head></html>";
        let og = parse_open_graph(html).expect("og data");
        assert_eq!(og.description, "A City Guide");
        assert!(og.title.contains("stanbul Guide"));
    }

    #[test]
    fn open_graph_parses_fixture_variants() {
        let html = r#"<html><head>
            <meta property="og:title" content="Judul &amp; Berita">
            <meta name="description" content="Ringkasan singkat">
            <meta property="og:description" content="Deskripsi OG">
        </head></html>"#;
        let og = parse_open_graph(html).expect("og");
        assert_eq!(og.title, "Judul & Berita");
        assert_eq!(og.description, "Deskripsi OG");
        // Attribute order flipped + single quotes.
        let html2 = "<meta content='Judul Lain' property='og:title'>";
        assert_eq!(parse_open_graph(html2).expect("og2").title, "Judul Lain");
        // Title-tag fallback.
        let html3 = "<html><head><title>  Hanya Title  </title></head></html>";
        assert_eq!(parse_open_graph(html3).expect("og3").title, "Hanya Title");
        assert_eq!(parse_open_graph("<p>kosong</p>"), None);
    }
}
