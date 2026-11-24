//! Message-level mirror work: upserts, polls, reactions,
//! receipt pruning, message flags, view-once, revokes and
//! typing indicators.

use super::*;

impl crate::state::App {
    /// Copies a poll's aggregated state from the session mirror
    /// into the open conversation's copy of the message. The
    /// mirror is where apply_poll_vote aggregates; the
    /// conversation is a separate rendering copy, and a rollback
    /// must move both.
    pub(crate) fn sync_poll_from_mirror(&mut self, chat_id: u64, mid: u64) {
        let restored = self
            .live
            .as_ref()
            .and_then(|s| s.messages.get(&chat_id))
            .and_then(|msgs| msgs.iter().find(|(_, m)| m.id == mid))
            .and_then(|(_, m)| match &m.kind {
                MessageKind::Poll { options, my_votes, .. } => {
                    Some((options.clone(), my_votes.clone()))
                }
                _ => None,
            });
        if let Some((options, my_votes)) = restored {
            for m in self.conversation.iter_mut().filter(|m| m.id == mid) {
                if let MessageKind::Poll { options: o, my_votes: v, .. } = &mut m.kind {
                    *o = options.clone();
                    *v = my_votes.clone();
                }
            }
        }
    }

    /// Points the chat list row at the mirror's newest
    /// surviving message after messages were removed from a
    /// chat (the expiry pass's rule: the new newest message
    /// becomes the preview, or the preview empties when none
    /// survive), and writes the row through to the archive so
    /// its stored preview cannot resurrect a removed message's
    /// body on the next start. The row's timestamp is the
    /// session's recorded one, so the chat keeps its list
    /// position.
    pub(crate) fn refresh_preview_after_removal(&mut self, chat_id: u64) {
        let last = self.live.as_ref().and_then(|s| {
            s.messages.get(&chat_id).and_then(|v| v.last().map(|(ts, m)| (*ts, m.clone())))
        });
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            match &last {
                Some((ts, m)) => {
                    chat.preview = m.preview_text();
                    chat.time = fmt_time(*ts);
                }
                None => {
                    chat.preview = String::new();
                }
            }
        }
        self.persist_chat_row(chat_id);
    }

    /// Applies one decrypted poll vote: the voter's whole current
    /// selection replaces their previous ballot, so counts move by
    /// the difference only and re-applying the same ballot is a
    /// no-op. The mirror poll is updated first; a poll the mirror
    /// does not hold (its chat was never loaded this session) is
    /// updated straight in the archive. Either way the resulting
    /// tally snapshot is persisted, so the counts and the ballots
    /// they came from survive a restart.
    pub(crate) fn apply_poll_vote(
        &mut self,
        chat_jid: &str,
        poll_id: &str,
        voter: &str,
        from_me: bool,
        selected: &[usize],
    ) {
        let chat_id = jid_id(chat_jid);
        let mid = message_id(chat_jid, poll_id);
        let mut tally: Option<PollTallyWrite> = None;
        if let Some(session) = &mut self.live {
            let ballots = session.poll_ballots.get(&mid).cloned().unwrap_or_default();
            if let Some(entry) = session.messages.get_mut(&chat_id) {
                if let Some((_, msg)) = entry.iter_mut().find(|(_, m)| m.id == mid) {
                    if let MessageKind::Poll { options, my_votes, .. } = &mut msg.kind {
                        let core::rich::PollTally {
                            options: new_options,
                            my_votes: new_mine,
                            ballots: new_ballots,
                        } = core::rich::apply_poll_ballot(
                            options, my_votes, &ballots, voter, from_me, selected,
                        );
                        *options = new_options.clone();
                        *my_votes = new_mine.clone();
                        tally = Some(PollTallyWrite {
                            chat_jid: chat_jid.to_owned(),
                            message_id: poll_id.to_owned(),
                            counts: new_options.iter().map(|(_, c)| *c).collect(),
                            my_votes: new_mine,
                            ballots: new_ballots,
                        });
                    }
                }
            }
            if let Some(tally) = &tally {
                if tally.ballots.is_empty() {
                    session.poll_ballots.remove(&mid);
                } else {
                    session.poll_ballots.insert(mid, tally.ballots.clone());
                }
            }
        }
        if tally.is_none() {
            // The mirror never loaded this poll (its chat is not in
            // memory): aggregate against the archived copy and
            // write the tally straight back to the archive.
            if let Some(store) = self.live.as_ref().and_then(|s| s.store.as_ref()) {
                if let Ok(rows) = store.messages(chat_jid) {
                    if let Some((archived, _)) = rows.into_iter().find(|(m, _)| m.id == poll_id) {
                        if let backend::LiveMessageKind::Poll { options, my_votes, .. } =
                            &archived.kind
                        {
                            let core::rich::PollTally {
                                options: new_options,
                                my_votes: new_mine,
                                ballots: new_ballots,
                            } = core::rich::apply_poll_ballot(
                                options,
                                my_votes,
                                &archived.poll_ballots,
                                voter,
                                from_me,
                                selected,
                            );
                            let counts: Vec<u32> = new_options.iter().map(|(_, c)| *c).collect();
                            let _ = store.set_poll_tally(
                                chat_jid,
                                poll_id,
                                &counts,
                                &new_mine,
                                &new_ballots,
                            );
                        }
                    }
                }
            }
            return;
        }
        if let Some(tally) = tally {
            self.persist_poll_tally(tally);
        }
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Brings the open conversation's copy of one event back in
    /// line with the session mirror (the rollback path after a
    /// failed response re-applied the previous answer in the
    /// mirror). The event counterpart of
    /// [`App::sync_poll_from_mirror`].
    pub(crate) fn sync_event_from_mirror(&mut self, chat_id: u64, mid: u64) {
        let restored = self
            .live
            .as_ref()
            .and_then(|s| s.messages.get(&chat_id))
            .and_then(|msgs| msgs.iter().find(|(_, m)| m.id == mid))
            .and_then(|(_, m)| match &m.kind {
                MessageKind::Event { going, maybe, not_going, my_rsvp, .. } => {
                    Some((*going, *maybe, *not_going, *my_rsvp))
                }
                _ => None,
            });
        if let Some((going, maybe, not_going, my_rsvp)) = restored {
            for m in self.conversation.iter_mut().filter(|m| m.id == mid) {
                if let MessageKind::Event {
                    going: g,
                    maybe: mb,
                    not_going: ng,
                    my_rsvp: mine,
                    ..
                } = &mut m.kind
                {
                    *g = going;
                    *mb = maybe;
                    *ng = not_going;
                    *mine = my_rsvp;
                }
            }
        }
    }

    /// Applies one decrypted event response: the responder's
    /// whole current answer replaces their previous one, so
    /// re-applying the same answer is a no-op and a changed
    /// mind never counts twice. The mirror event is updated
    /// first; an event the mirror does not hold (its chat was
    /// never loaded this session) is updated straight in the
    /// archive. Either way the resulting tally snapshot is
    /// persisted, so the counts and the ballots they came from
    /// survive a restart. The event counterpart of
    /// [`App::apply_poll_vote`].
    pub(crate) fn apply_event_response(
        &mut self,
        chat_jid: &str,
        event_id: &str,
        responder: &str,
        from_me: bool,
        response: backend::EventRsvp,
    ) {
        let chat_id = jid_id(chat_jid);
        let mid = message_id(chat_jid, event_id);
        let mut tally: Option<EventTallyWrite> = None;
        if let Some(session) = &mut self.live {
            let ballots = session.event_ballots.get(&mid).cloned().unwrap_or_default();
            if let Some(entry) = session.messages.get_mut(&chat_id) {
                if let Some((_, msg)) = entry.iter_mut().find(|(_, m)| m.id == mid) {
                    if let MessageKind::Event { going, maybe, not_going, my_rsvp, .. } =
                        &mut msg.kind
                    {
                        let t = core::rich::apply_event_response(
                            *my_rsvp,
                            &ballots
                                .iter()
                                .map(|(j, r)| (j.clone(), crate::live::to_rsvp(*r)))
                                .collect::<Vec<_>>(),
                            responder,
                            from_me,
                            crate::live::to_rsvp(response),
                        );
                        *going = t.going;
                        *maybe = t.maybe;
                        *not_going = t.not_going;
                        *my_rsvp = t.my_rsvp;
                        tally = Some(EventTallyWrite {
                            chat_jid: chat_jid.to_owned(),
                            message_id: event_id.to_owned(),
                            going: t.going,
                            maybe: t.maybe,
                            not_going: t.not_going,
                            my_rsvp: t.my_rsvp.map(crate::live::rsvp_to_backend),
                            ballots: t
                                .ballots
                                .iter()
                                .map(|(j, r)| (j.clone(), crate::live::rsvp_to_backend(*r)))
                                .collect(),
                        });
                    }
                }
            }
            if let Some(tally) = &tally {
                if tally.ballots.is_empty() {
                    session.event_ballots.remove(&mid);
                } else {
                    session.event_ballots.insert(mid, tally.ballots.clone());
                }
            }
        }
        if tally.is_none() {
            // The mirror never loaded this event (its chat is not
            // in memory): aggregate against the archived copy and
            // write the tally straight back to the archive.
            if let Some(store) = self.live.as_ref().and_then(|s| s.store.as_ref()) {
                if let Ok(rows) = store.messages(chat_jid) {
                    if let Some((archived, _)) = rows.into_iter().find(|(m, _)| m.id == event_id) {
                        if let backend::LiveMessageKind::Event { my_rsvp: archived_mine, .. } =
                            &archived.kind
                        {
                            let t = core::rich::apply_event_response(
                                archived_mine.map(crate::live::to_rsvp),
                                &archived
                                    .event_ballots
                                    .iter()
                                    .map(|(j, r)| (j.clone(), crate::live::to_rsvp(*r)))
                                    .collect::<Vec<_>>(),
                                responder,
                                from_me,
                                crate::live::to_rsvp(response),
                            );
                            let ballots: Vec<(String, backend::EventRsvp)> = t
                                .ballots
                                .iter()
                                .map(|(j, r)| (j.clone(), crate::live::rsvp_to_backend(*r)))
                                .collect();
                            let _ = store.set_event_tally(
                                chat_jid,
                                event_id,
                                t.going,
                                t.maybe,
                                t.not_going,
                                t.my_rsvp.map(crate::live::rsvp_to_backend),
                                &ballots,
                            );
                        }
                    }
                }
            }
            return;
        }
        if let Some(tally) = tally {
            self.persist_event_tally(tally);
        }
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Retracts my own response on one event (the rollback for
    /// a failed first answer: there is no previous answer to
    /// re-apply, so mine is cleared and the tallies recomputed
    /// from the remaining ballots). Mirror first, archive
    /// fallback, persisted either way, exactly like
    /// [`App::apply_event_response`].
    pub(crate) fn apply_event_retraction(&mut self, chat_jid: &str, event_id: &str) {
        let chat_id = jid_id(chat_jid);
        let mid = message_id(chat_jid, event_id);
        let mut tally: Option<EventTallyWrite> = None;
        if let Some(session) = &mut self.live {
            let ballots = session.event_ballots.get(&mid).cloned().unwrap_or_default();
            if let Some(entry) = session.messages.get_mut(&chat_id) {
                if let Some((_, msg)) = entry.iter_mut().find(|(_, m)| m.id == mid) {
                    if let MessageKind::Event { going, maybe, not_going, my_rsvp, .. } =
                        &mut msg.kind
                    {
                        let t = core::rich::retract_event_response(
                            &ballots
                                .iter()
                                .map(|(j, r)| (j.clone(), crate::live::to_rsvp(*r)))
                                .collect::<Vec<_>>(),
                        );
                        *going = t.going;
                        *maybe = t.maybe;
                        *not_going = t.not_going;
                        *my_rsvp = None;
                        tally = Some(EventTallyWrite {
                            chat_jid: chat_jid.to_owned(),
                            message_id: event_id.to_owned(),
                            going: t.going,
                            maybe: t.maybe,
                            not_going: t.not_going,
                            my_rsvp: None,
                            ballots: t
                                .ballots
                                .iter()
                                .map(|(j, r)| (j.clone(), crate::live::rsvp_to_backend(*r)))
                                .collect(),
                        });
                    }
                }
            }
            if let Some(tally) = &tally {
                if tally.ballots.is_empty() {
                    session.event_ballots.remove(&mid);
                } else {
                    session.event_ballots.insert(mid, tally.ballots.clone());
                }
            }
        }
        if tally.is_none() {
            if let Some(store) = self.live.as_ref().and_then(|s| s.store.as_ref()) {
                if let Ok(rows) = store.messages(chat_jid) {
                    if let Some((archived, _)) = rows.into_iter().find(|(m, _)| m.id == event_id) {
                        if let backend::LiveMessageKind::Event { .. } = &archived.kind {
                            let t = core::rich::retract_event_response(
                                &archived
                                    .event_ballots
                                    .iter()
                                    .map(|(j, r)| (j.clone(), crate::live::to_rsvp(*r)))
                                    .collect::<Vec<_>>(),
                            );
                            let ballots: Vec<(String, backend::EventRsvp)> = t
                                .ballots
                                .iter()
                                .map(|(j, r)| (j.clone(), crate::live::rsvp_to_backend(*r)))
                                .collect();
                            let _ = store.set_event_tally(
                                chat_jid,
                                event_id,
                                t.going,
                                t.maybe,
                                t.not_going,
                                None,
                                &ballots,
                            );
                        }
                    }
                }
            }
            return;
        }
        if let Some(tally) = tally {
            self.persist_event_tally(tally);
        }
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Applies one event edit (mine confirmed, a peer's edit
    /// arriving from the wire, or the rollback restoring the
    /// previous fields): the descriptive fields of `new_kind`
    /// replace the event's, while its tallies and my response
    /// are preserved. The mirror, the open conversation copy
    /// and the archive row all move together.
    pub(crate) fn apply_event_edit(
        &mut self,
        chat_jid: &str,
        event_id: &str,
        chat_id: u64,
        mid: u64,
        new_kind: core::model::MessageKind,
    ) {
        let core::model::MessageKind::Event {
            name,
            description,
            start_time,
            end_time,
            location,
            join_link,
            scheduled_call,
            extra_guests,
            canceled,
            ..
        } = &new_kind
        else {
            return;
        };
        let (name, description, location, join_link) =
            (name.clone(), description.clone(), location.clone(), join_link.clone());
        let (start_time, end_time, scheduled_call, extra_guests, canceled) =
            (*start_time, *end_time, *scheduled_call, *extra_guests, *canceled);
        let merge = |kind: &mut core::model::MessageKind| {
            if let core::model::MessageKind::Event {
                name: n,
                description: d,
                start_time: s,
                end_time: e,
                location: l,
                join_link: j,
                scheduled_call: sc,
                extra_guests: eg,
                canceled: c,
                ..
            } = kind
            {
                *n = name.clone();
                *d = description.clone();
                *s = start_time;
                *e = end_time;
                *l = location.clone();
                *j = join_link.clone();
                *sc = scheduled_call;
                *eg = extra_guests;
                *c = canceled;
            }
        };
        if let Some(entry) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            for (_, m) in entry.iter_mut().filter(|(_, m)| m.id == mid) {
                merge(&mut m.kind);
            }
        }
        for m in self.conversation.iter_mut().filter(|m| m.id == mid) {
            merge(&mut m.kind);
        }
        if let Some(store) = self.live.as_ref().and_then(|s| s.store.as_ref()) {
            let _ = store.set_event_fields(
                chat_jid,
                event_id,
                &name,
                &description,
                start_time,
                end_time,
                &location,
                &join_link,
                scheduled_call,
                extra_guests,
                canceled,
            );
        }
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Routes one event tally snapshot to the archive, with the
    /// same batching contract as [`App::persist_poll_tally`].
    pub(super) fn persist_event_tally(&mut self, tally: EventTallyWrite) {
        if self.event_batch.active {
            self.event_batch.event_tally_writes.push(tally);
            return;
        }
        if let Some(store) = self.live.as_ref().and_then(|s| s.store.as_ref()) {
            let _ = store.set_event_tally(
                &tally.chat_jid,
                &tally.message_id,
                tally.going,
                tally.maybe,
                tally.not_going,
                tally.my_rsvp,
                &tally.ballots,
            );
        }
    }

    /// Routes one poll tally snapshot to the archive: queued into
    /// the running event batch when there is one (its flush writes
    /// tallies after the batch's upserts), written immediately
    /// otherwise.
    pub(super) fn persist_poll_tally(&mut self, tally: PollTallyWrite) {
        if self.event_batch.active {
            self.event_batch.poll_tally_writes.push(tally);
            return;
        }
        if let Some(store) = self.live.as_ref().and_then(|s| s.store.as_ref()) {
            let _ = store.set_poll_tally(
                &tally.chat_jid,
                &tally.message_id,
                &tally.counts,
                &tally.my_votes,
                &tally.ballots,
            );
        }
    }

    /// Applies one backend message upsert. Returns true only when
    /// the upsert pushed a genuinely new incoming message (not an
    /// echo replacement and not a re-upsert of a known id).
    pub(crate) fn apply_message_upsert(&mut self, live: &LiveMessage) -> bool {
        if live.chat_jid == "status@broadcast" {
            // A status post is not a message in a conversation
            // (the backend drops these at translation; this is
            // the same rule at the app boundary, so no source
            // can file one as a fake DM): no mirror row, no
            // archive write, no unread, no notification. The
            // Updates feed round owns received statuses.
            return false;
        }
        let chat_id = jid_id(&live.chat_jid);
        // Set inside the session block when this upsert pushes a
        // genuinely new incoming message (not an echo replacement,
        // not a re-upsert of a known id): the OS notification queue
        // below consumes it, so notifications can never double-fire
        // on edits, stars or receipt-driven re-upserts.
        let mut new_incoming = false;
        if let Some(session) = &mut self.live {
            let msg_id = message_id(&live.chat_jid, &live.id);
            if let Some(server_id) = live.server_id {
                session.server_ids.insert((chat_id, server_id), msg_id);
            }
            if let Some(reference) = &live.photo_ref {
                session.photo_refs.insert(msg_id, reference.clone());
            }
            if let Some(reference) = &live.voice_ref {
                session.voice_refs.insert(msg_id, reference.clone());
            }
            if let Some(reference) = &live.video_ref {
                session.video_refs.insert(msg_id, reference.clone());
            }
            if let Some(thumb) = &live.video_thumbnail {
                session.video_thumbs.insert(msg_id, thumb.clone());
            }
            if let Some(reference) = &live.poster_ref {
                session.poster_refs.insert(msg_id, reference.clone());
            }
            if let (Some(w), Some(h)) = (live.video_width, live.video_height) {
                session.video_dims.insert(msg_id, (w, h));
            }
            if let Some(reference) = &live.sticker_ref {
                session.sticker_refs.insert(msg_id, reference.clone());
            }
            if let Some(thumb) = &live.photo_thumbnail {
                session.photo_thumbs.insert(msg_id, thumb.clone());
            }
            if let Some(reference) = &live.document_ref {
                session.document_refs.insert(msg_id, reference.clone());
            }
            if let Some(thumb) = &live.document_thumbnail {
                session.document_thumbs.insert(msg_id, thumb.clone());
            }
            if live.gif_playback {
                session.gif_videos.insert(msg_id);
            }
            if live.sticker_animated || live.sticker_lottie {
                session.sticker_anim.insert(msg_id, (live.sticker_animated, live.sticker_lottie));
            }
            if let Some(thumb) = &live.sticker_thumbnail {
                session.sticker_thumbs.insert(msg_id, thumb.clone());
            }
            if let Some(thumb) = &live.location_thumbnail {
                session.location_thumbs.insert(msg_id, thumb.clone());
            }
            if let Some(thumb) = live.link_preview.as_ref().and_then(|p| p.thumbnail.as_ref()) {
                session.preview_thumbs.insert(msg_id, thumb.clone());
            }
            if live.view_once {
                session.view_once_ids.insert(msg_id);
            }
            if let (Some(jid), Some(name)) = (&live.sender_jid, &live.sender_verified_name) {
                session.verified_names.insert(jid.clone(), name.clone());
                session.verified_checked.insert(jid.clone());
            }
            if !live.mentions.is_empty() {
                session.message_mentions.insert(msg_id, live.mentions.clone());
            }
            // The sender's resolved name joins the name
            // directory under both of the sender's JID forms
            // when the session knows them, so mentions of this
            // sender resolve wherever they appear.
            if let (Some(jid), Some(name)) = (&live.sender_jid, &live.sender) {
                if !name.is_empty() && !is_raw_identifier(name) {
                    session.names.insert(jid.clone(), name.clone());
                }
            }
            let mut flags = session.flags.get(&msg_id).copied().unwrap_or(0);
            if live.starred {
                flags |= store::FLAG_STARRED;
            }
            if live.pinned {
                flags |= store::FLAG_PINNED_MESSAGE;
            }
            // A server copy already carrying the state a pending
            // star or pin change was aiming at settles that
            // change: the backend accepted it, so a failure
            // report arriving later for it must not roll anything
            // back. A copy showing neither state settles nothing
            // (it may predate the change). No event reports a
            // message pin adopted elsewhere, so this adoption is
            // the pin's only confirmation path.
            if live.starred && self.pending_stars.get(&msg_id) == Some(&false) {
                self.pending_stars.remove(&msg_id);
            }
            if live.pinned && self.pending_pins.get(&msg_id) == Some(&false) {
                self.pending_pins.remove(&msg_id);
            }
            if flags != session.flags.get(&msg_id).copied().unwrap_or(0) {
                if flags == 0 {
                    // Same rule as `set_live_flag`: the flag map
                    // holds only messages that carry flags.
                    session.flags.remove(&msg_id);
                } else {
                    session.flags.insert(msg_id, flags);
                }
            }
            if session.store.is_some() {
                if self.event_batch.active {
                    // Deferred to the batch flush, which writes
                    // every queued upsert in one transaction; the
                    // flags are resolved there, not captured here.
                    self.event_batch.store_writes.push(live.clone());
                } else if let Some(store) = &session.store {
                    let _ = store.upsert_message(live, flags);
                }
            }
            session.jids.entry(chat_id).or_insert_with(|| live.chat_jid.clone());
            session.times.insert(chat_id, live.timestamp);
            let entry = session.messages.entry(chat_id).or_default();
            let mut converted = to_message_with_flags(live, flags);
            // A poll creation re-delivered (history sync, a
            // receipt-driven re-upsert) carries zero counts and no
            // ballots: keep the tallies the mirror has already
            // aggregated, the same merge the archive write applies.
            if let MessageKind::Poll { options: new_options, my_votes: new_mine, .. } =
                &mut converted.kind
            {
                if let Some((_, old)) = entry.iter().find(|(_, m)| m.id == converted.id) {
                    if let MessageKind::Poll { options: old_options, my_votes: old_mine, .. } =
                        &old.kind
                    {
                        if old_options.len() == new_options.len() {
                            for (new, old_count) in new_options.iter_mut().zip(old_options.iter()) {
                                new.1 = old_count.1;
                            }
                            *new_mine = old_mine.clone();
                        }
                    }
                }
            }
            // Delivery merges monotonically across re-upserts,
            // the same discipline as the poll tallies above: a
            // re-delivered copy carries at best the state its
            // payload states, and a Read the receipts already
            // proved must never fall back to a lesser tick.
            if converted.delivery.is_some() {
                if let Some((_, old)) = entry.iter().find(|(_, m)| m.id == converted.id) {
                    let rank = |d: Delivery| match d {
                        Delivery::Failed | Delivery::Sending => 0,
                        Delivery::Sent => 1,
                        Delivery::Delivered => 2,
                        Delivery::Read => 3,
                    };
                    if let (Some(new_d), Some(old_d)) = (converted.delivery, old.delivery) {
                        if rank(old_d) > rank(new_d) {
                            converted.delivery = Some(old_d);
                        }
                    }
                }
            }
            // Replace a pending local echo of this outgoing message.
            // Text echoes match on body; media echoes match on kind,
            // because their display bodies legitimately differ
            // between the echo and the backend upsert (a sticker
            // echoes as "Sticker" but upserts as "[sticker]", so
            // body matching could never retire it, logic review F7).
            // Oldest-first, so quick repeats retire in send order.
            let echo_pos = if live.from_me {
                entry.iter().position(|(_, m)| {
                    m.delivery == Some(Delivery::Sending)
                        && (m.body == live.body
                            || (!matches!(converted.kind, MessageKind::Text)
                                && std::mem::discriminant(&m.kind)
                                    == std::mem::discriminant(&converted.kind)))
                })
            } else {
                None
            };
            let mut is_new_push = false;
            if let Some(pos) = echo_pos {
                entry[pos] = (live.timestamp, converted);
            } else if let Some(pos) = entry.iter().position(|(_, m)| m.id == converted.id) {
                entry[pos] = (live.timestamp, converted);
            } else {
                entry.push((live.timestamp, converted));
                is_new_push = true;
            }

            new_incoming = is_new_push && !live.from_me;
            if self.event_batch.active {
                // Sorted once when the batch finishes. In-batch
                // readers of this vec look messages up by ID or
                // by minimum timestamp, never by position, so the
                // deferral cannot change what they find.
                self.event_batch.unsorted_chats.insert(chat_id);
            } else {
                entry.sort_by_key(|(ts, _)| *ts);
            }
        }
        // "Keep chats archived" OFF: a new incoming message brings
        // an archived chat back to the main list (WhatsApp's rule).
        if !live.from_me && !self.keep_archived {
            let was_archived =
                self.chats.iter().find(|c| c.id == chat_id).is_some_and(|c| c.archived);
            if was_archived {
                if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                    c.archived = false;
                }
                let snapshot = self.live.as_ref().and_then(|session| {
                    let jid = session.jids.get(&chat_id)?.clone();
                    let ts = session.times.get(&chat_id).copied().unwrap_or(0);
                    let chat = self.chats.iter().find(|c| c.id == chat_id)?.clone();
                    Some((chat, jid, ts))
                });
                if let (Some((chat, jid, ts)), Some(session)) = (snapshot, &self.live) {
                    if let Some(store) = &session.store {
                        let _ = store.upsert_chat(&crate::live::chat_to_live(&chat, &jid, ts));
                    }
                }
            }
        }
        // Keep the chat list preview in step even without a ChatUpsert.
        if !self.chats.iter().any(|c| c.id == chat_id) {
            self.apply_chat_upsert(&LiveChat {
                jid: live.chat_jid.clone(),
                title: live.sender.clone().unwrap_or_else(|| "Live chat".to_owned()),
                kind: if live.chat_jid.ends_with("@g.us") {
                    LiveChatKind::Group
                } else if live.chat_jid.ends_with("@newsletter") {
                    LiveChatKind::Channel
                } else {
                    LiveChatKind::Dm
                },
                preview: glanceable_text(live),
                preview_sender: None,
                timestamp: live.timestamp,
                unread: 0,
                archived: false,
                locked: false,
                pinned: false,
                ephemeral_secs: 0,
                muted: false,
                mute_until: 0,
                parent_jid: None,
                is_parent: false,
            });
        }
        // Protocol-id bookkeeping (receipts, reactions, read marks).
        // Read-by-action: a new incoming message queues for a read
        // receipt even when its chat is the one on screen. Arrival
        // alone never sends the receipt; the queue drains only
        // after a real user action (see `note_read_engagement`).
        let app_msg_id = message_id(&live.chat_jid, &live.id);
        self.protocol_ids.insert(app_msg_id, live.id.clone());
        if let Some(jid) = &live.sender_jid {
            self.sender_jids.insert(app_msg_id, jid.clone());
        }
        if new_incoming {
            self.unread_live.entry(chat_id).or_default().push(live.id.clone());
        }
        // Queue the OS notification for a genuinely new incoming
        // message; the per-frame drain applies the user's settings
        // (toggles, mute, preview, focus) before firing. History
        // never queues: a message stamped before this connection
        // came up is first-sync or offline catch-up, and a popup
        // for it would be noise about the past. The fake backend
        // is exempt: its clock is scripted, so the stamp
        // comparison is meaningless there and the demo keeps its
        // previous behavior.
        let history = self
            .live
            .as_ref()
            .is_some_and(|s| !s.fake && crate::notify::is_history(live.timestamp, s.connected_at));
        if new_incoming && !history {
            if let Some(chat) = self.chats.iter().find(|c| c.id == chat_id) {
                // Blocked state gates the popup like mute does:
                // the chat itself for a direct chat with a
                // blocked contact, the message's sender inside a
                // group. The server stops delivering a blocked
                // contact's messages, but anything already in
                // flight or synced can still land here.
                let blocked = self.blocked_chats.contains(&chat_id)
                    || live
                        .sender_jid
                        .as_deref()
                        .is_some_and(|j| self.blocked_chats.contains(&jid_id(j)));
                self.pending_notifications.push(crate::notify::NotifyRequest {
                    chat_id,
                    title: chat.title.clone(),
                    kind: chat.kind,
                    muted: chat.muted,
                    blocked,
                    locked: chat.locked,
                    sender: live.sender.clone(),
                    // The queued body is the only text the popup
                    // planner can ever show, so a view-once
                    // message queues its placeholder here: its
                    // caption cannot reach a popup (or the lock
                    // screen) whatever the preview toggle says.
                    body: glanceable_text(live),
                });
            }
        }
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.preview = glanceable_text(live);
            chat.time = fmt_time(live.timestamp);
            // Snippet anatomy for the row: ticks when the latest
            // message is ours, a media label when it is not text.
            chat.preview_mine = live.from_me;
            chat.preview_delivery = if live.from_me {
                Some(match live.delivery {
                    Some(backend::DeliveryState::Read) => Delivery::Read,
                    Some(backend::DeliveryState::Delivered) => Delivery::Delivered,
                    Some(backend::DeliveryState::Sent) | None => Delivery::Sent,
                })
            } else {
                None
            };
            chat.preview_media = core::model::PreviewMedia::of(&to_kind(&live.kind));
            // Only a genuinely new message raises the unread badge;
            // re-upserts (edits, votes, stars) must not (F8). The
            // badge also rises in the currently open chat: under
            // read-by-action the message is not read yet just
            // because it arrived while the screen was open, and it
            // clears when the user acts (see the flush in
            // conversation.rs).
            if new_incoming {
                chat.unread += 1;
            }
        }
        // The archive row must carry the badge the mirror now
        // shows: the increment above is the only place the count
        // grows, and the arrival upsert's own row write carries
        // the pre-increment count (merged from the known value,
        // see apply_chat_upsert), so without this write a restart
        // would reload a smaller count than the list showed.
        // Inside a batch the write defers to the flush, like the
        // message rows.
        if new_incoming {
            if self.event_batch.active {
                self.event_batch.chat_rows_dirty.insert(chat_id);
            } else {
                self.persist_chat_row(chat_id);
            }
        }
        self.sort_live_chats();
        if chat_id == self.selected_chat {
            self.sync_conversation();
        }
        new_incoming
    }

    /// Sorts the chat list by latest activity. Inside an event
    /// batch the sort is deferred to the end of the batch (see
    /// [`EventBatch`]): a burst would otherwise re-sort the whole
    /// list per event. The list is only ever rendered between
    /// batches, so the deferral is invisible.
    pub(super) fn sort_live_chats(&mut self) {
        if self.event_batch.active {
            self.event_batch.chats_dirty = true;
            return;
        }
        self.sort_live_chats_now();
    }

    /// The immediate chat-list sort behind [`App::sort_live_chats`].
    pub(super) fn sort_live_chats_now(&mut self) {
        if let Some(session) = &self.live {
            let times = &session.times;
            self.chats.sort_by(|a, b| {
                times.get(&b.id).copied().unwrap_or(0).cmp(&times.get(&a.id).copied().unwrap_or(0))
            });
        }
    }

    /// Writes my own reaction into the actor map and derives
    /// the message's counts from it, the exact bookkeeping the
    /// ReactionUpdate handler runs when the server reports a
    /// reaction. The optimistic send path and the failure
    /// rollback both go through here so local state has a single
    /// representation: when the server's echo of my change
    /// arrives it re-applies the identical write and nothing
    /// moves, and a rollback is the same write with the older
    /// value. `mine` is my current emoji, or None for no
    /// reaction.
    pub(crate) fn apply_my_reaction_to_actors(
        &mut self,
        chat_id: u64,
        msg_id: u64,
        mine: Option<&str>,
    ) {
        let key = format!("{chat_id}:{msg_id}");
        let counts = {
            let actors = self.reaction_actors.entry(key.clone()).or_default();
            match mine {
                Some(emoji) => {
                    actors.insert("Me".to_owned(), emoji.to_owned());
                }
                None => {
                    actors.remove("Me");
                }
            }
            count_reactions(actors)
        };
        if let Some(entry) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            for (_, m) in entry.iter_mut() {
                if m.id == msg_id {
                    m.reactions = counts.clone();
                }
            }
        }
        if counts.is_empty() {
            self.reaction_actors.remove(&key);
        }
        if self.selected_chat == chat_id {
            if let Some(msg) = self.conversation.iter_mut().find(|m| m.id == msg_id) {
                msg.reactions = counts;
            }
        }
    }

    /// Sets a message's reaction counts outright, in the
    /// session mirror and the open conversation. Channel
    /// updates carry aggregate counts with no actor list
    /// behind them, so their optimistic writes and rollbacks
    /// set the counts directly instead of deriving them from
    /// the actor map the way chat reactions do.
    pub(crate) fn set_message_reactions(
        &mut self,
        chat_id: u64,
        msg_id: u64,
        counts: Vec<(String, u32)>,
    ) {
        if let Some(entry) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            for (_, m) in entry.iter_mut() {
                if m.id == msg_id {
                    m.reactions = counts.clone();
                }
            }
        }
        if self.selected_chat == chat_id {
            if let Some(msg) = self.conversation.iter_mut().find(|m| m.id == msg_id) {
                msg.reactions = counts;
            }
        }
    }

    /// The receipt log's size cap: at most 4,096 message
    /// histories are kept. Past the cap the OLDEST histories
    /// drop, in first-insertion order, and the recent ones
    /// stay: the log used to clear wholesale at the cap,
    /// silently emptying the Message info history of every
    /// message at once (VRF-05). Keys already forgotten leave
    /// a stale slot in the order queue; the prune skips them.
    pub(crate) fn prune_receipt_log(&mut self) {
        const RECEIPT_LOG_CAP: usize = 4096;
        while self.receipt_log.len() > RECEIPT_LOG_CAP {
            let Some(oldest) = self.receipt_log_order.pop_front() else { break };
            self.receipt_log.remove(&oldest);
        }
    }

    /// Drops every per-message helper entry kept for one live
    /// message: its protocol ID, sender JID, receipt log,
    /// reaction state, archive flags, and any pending read
    /// receipt. Called when a message leaves the session mirror
    /// (today: a disappearing message whose timer ran out), so
    /// the helper maps stay scoped to the messages the mirror
    /// actually holds instead of gaining an entry for every
    /// message ever seen. Nothing here is consulted for a
    /// message that is no longer rendered, so lookups for the
    /// messages that remain are unaffected.
    pub(crate) fn forget_live_message(
        &mut self,
        chat_id: u64,
        msg_id: u64,
        protocol_id: Option<&str>,
    ) {
        self.protocol_ids.remove(&msg_id);
        if let Some(session) = &mut self.live {
            session.photo_refs.remove(&msg_id);
            // Every per-message map goes (P2-40): media refs
            // and thumbnails of every kind, mentions, poll
            // ballots. A forgotten message that leaves refs
            // behind keeps dead bytes and stale state the next
            // message with a recycled id would inherit.
            session.voice_refs.remove(&msg_id);
            session.video_refs.remove(&msg_id);
            session.video_thumbs.remove(&msg_id);
            session.poster_refs.remove(&msg_id);
            session.video_dims.remove(&msg_id);
            session.sticker_refs.remove(&msg_id);
            session.sticker_anim.remove(&msg_id);
            session.sticker_thumbs.remove(&msg_id);
            session.document_refs.remove(&msg_id);
            session.document_thumbs.remove(&msg_id);
            session.location_thumbs.remove(&msg_id);
            session.preview_thumbs.remove(&msg_id);
            session.photo_thumbs.remove(&msg_id);
            session.message_mentions.remove(&msg_id);
            session.poll_ballots.remove(&msg_id);
        }
        self.media.photo_fetch.remove(&msg_id);
        self.media.voice_fetch.remove(&msg_id);
        self.media.video_fetch.remove(&msg_id);
        self.media.poster_fetch.remove(&msg_id);
        self.media.sticker_fetch.remove(&msg_id);
        self.media.video_files.remove(&msg_id);
        self.media.video_bytes.remove(&msg_id);
        self.media.textures.remove(&msg_id);
        self.sender_jids.remove(&msg_id);
        self.receipt_log.remove(&msg_id);
        self.receipt_log_order.retain(|id| *id != msg_id);
        self.my_reactions.remove(&msg_id);
        self.pending_reactions.remove(&msg_id);
        self.pending_channel_reactions.remove(&msg_id);
        // Pending action state for the forgotten message:
        // a star/pin/keep/vote outcome arriving later must
        // not resurrect bookkeeping for a message that is gone.
        self.pending_stars.remove(&msg_id);
        self.pending_pins.remove(&msg_id);
        self.pending_keeps.remove(&msg_id);
        self.pending_poll_votes.remove(&msg_id);
        self.pending_edits.remove(&msg_id);
        self.pending_revokes.remove(&msg_id);
        self.reaction_actors.remove(&format!("{chat_id}:{msg_id}"));
        if let Some(pid) = protocol_id {
            let mut drained = false;
            if let Some(pending) = self.unread_live.get_mut(&chat_id) {
                pending.retain(|p| p != pid);
                drained = pending.is_empty();
            }
            if drained {
                self.unread_live.remove(&chat_id);
            }
        }
        if let Some(session) = &mut self.live {
            session.flags.remove(&msg_id);
        }
    }

    /// Sets (or clears with `flags = 0`) archive flags for one
    /// message and persists them when a store row exists.
    pub(crate) fn set_live_flag(&mut self, chat_id: u64, msg_id: u64, bit: i64, on: bool) {
        let protocol_id = self.protocol_ids.get(&msg_id).cloned();
        if let Some(session) = &mut self.live {
            let flags = {
                let entry = session.flags.entry(msg_id).or_insert(0);
                if on {
                    *entry |= bit;
                } else {
                    *entry &= !bit;
                }
                *entry
            };
            if flags == 0 {
                // Absence and zero mean the same to every reader
                // (all read with `unwrap_or(0)`); keep the map
                // scoped to messages that actually carry flags.
                // The store write below still persists the zero.
                session.flags.remove(&msg_id);
            }
            // Persist through a targeted flags-only UPDATE. The old
            // path rebuilt the whole row from the UI model, which
            // zeroed the timestamp and erased quote, preview and
            // view-once data on every star/pin/revoke (logic review
            // F1, fixed in the store in v0.25).
            if let (Some(store), Some(jid), Some(pid)) =
                (&session.store, session.jids.get(&chat_id), protocol_id)
            {
                let _ = store.set_message_flags(jid, &pid, flags);
            }
        }
    }

    /// Marks a view-once message opened, in memory and in the
    /// archive. The in-memory set drives the bubble's opened
    /// presentation for this session; the stored flag
    /// ([`store::FLAG_VIEW_ONCE_OPENED`]) is what start_live
    /// seeds that set from after a restart, so an opened
    /// view-once message never presents as unopened again:
    /// once opened, it stays opened (privacy audit D13).
    pub(crate) fn mark_view_once_opened(&mut self, msg_id: u64) {
        if !self.media.opened_view_once.insert(msg_id) {
            return;
        }
        let chat_id = self.live.as_ref().and_then(|session| {
            session
                .messages
                .iter()
                .find(|(_, msgs)| msgs.iter().any(|(_, m)| m.id == msg_id))
                .map(|(chat_id, _)| *chat_id)
        });
        if let Some(chat_id) = chat_id {
            self.set_live_flag(chat_id, msg_id, store::FLAG_VIEW_ONCE_OPENED, true);
        }
    }

    /// Handles a delete-for-everyone for one message: the native
    /// behaviour, the message is replaced by the placeholder.
    pub(crate) fn apply_revoke(&mut self, chat_id: u64, msg_id: u64) {
        if let Some(entry) = self.live.as_mut().and_then(|s| s.messages.get_mut(&chat_id)) {
            for (_, m) in entry.iter_mut() {
                if m.id == msg_id {
                    m.deleted = true;
                    m.body = "This message was deleted".to_owned();
                    m.reactions.clear();
                }
            }
        }
        self.set_live_flag(chat_id, msg_id, store::FLAG_DELETED, true);
        if self.selected_chat == chat_id {
            self.sync_conversation();
        }
    }

    /// Sends a composing/paused chat-state for the selected live
    /// chat, only when the typing-indicators toggle is ON and the
    /// state actually changed (no per-keystroke spam). Tracked per
    /// chat: switching chats mid-composing pauses the old chat
    /// instead of orphaning its indicator (logic review F16).
    pub(crate) fn set_typing(&mut self, typing: bool) {
        if !should_send_typing(self.typing_indicators, self.live_mode) {
            return;
        }
        let chat_id = self.selected_chat;
        if typing {
            if self.typing_sent_for == Some(chat_id) {
                return;
            }
            if let Some(old) = self.typing_sent_for.take() {
                self.send_typing_to(old, false);
            }
            self.typing_sent_for = Some(chat_id);
            self.send_typing_to(chat_id, true);
        } else if let Some(old) = self.typing_sent_for.take() {
            self.send_typing_to(old, false);
        }
    }

    pub(super) fn send_typing_to(&mut self, chat_id: u64, typing: bool) {
        if let Some(jid) = self.live.as_ref().and_then(|s| s.jids.get(&chat_id).cloned()) {
            self.send_backend(BackendCommand::SetTyping { chat_jid: jid, typing });
        }
    }

    /// Applies a typing-indicators toggle change from the
    /// Privacy page. Turning the toggle off must also retract a
    /// composing state already sent: the peer would otherwise
    /// keep seeing "typing..." until it times out server-side,
    /// from a client whose user just said indicators are off.
    /// The retraction goes through [`App::send_typing_to`], which
    /// does not consult the toggle: the gate in
    /// [`App::set_typing`] would swallow it.
    pub(crate) fn set_typing_indicators(&mut self, on: bool) {
        self.typing_indicators = on;
        if !on {
            if let Some(chat_id) = self.typing_sent_for.take() {
                self.send_typing_to(chat_id, false);
            }
        }
        self.persist_settings();
    }
}

#[cfg(test)]
mod mirror_pruning_tests {
    use super::*;
    use crate::state::{App, Args};

    const CHAT_JID: &str = "6281234567890@s.whatsapp.net";

    /// The same live-mode harness as the burst tests: an app
    /// whose backend channels the test feeds and reads by hand,
    /// with an in-memory archive attached.
    pub(super) fn live_app() -> (
        App,
        std::sync::mpsc::Sender<BackendEvent>,
        egui::Context,
        std::sync::mpsc::Receiver<BackendCommand>,
    ) {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = App::new(Args::parse(), &ctx);
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<BackendCommand>();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel::<BackendEvent>();
        app.live = Some(LiveSession {
            channels: BackendChannels { commands: cmd_tx, events: evt_rx },
            status: LinkStatus::Linked,
            qr: None,
            qr_expires_at: None,
            pair_code: None,
            pair_code_at: None,
            account: "Test account".to_owned(),
            connected_at: None,
            syncing: false,
            sync_last_activity: None,
            synced_once: false,
            store: Some(store::Store::open_in_memory().unwrap()),
            jids: HashMap::new(),
            messages: HashMap::new(),
            times: HashMap::new(),
            fake: true,
            logout_requested: false,
            flags: HashMap::new(),
            engaged_chats: std::collections::HashSet::new(),
            photo_refs: HashMap::new(),
            voice_refs: HashMap::new(),
            video_refs: HashMap::new(),
            video_thumbs: HashMap::new(),
            poster_refs: HashMap::new(),
            video_dims: HashMap::new(),
            document_refs: HashMap::new(),
            gif_videos: std::collections::HashSet::new(),
            sticker_anim: HashMap::new(),
            sticker_thumbs: HashMap::new(),
            document_thumbs: HashMap::new(),
            location_thumbs: HashMap::new(),
            preview_thumbs: HashMap::new(),
            view_once_ids: std::collections::HashSet::new(),
            verified_names: HashMap::new(),
            verified_checked: std::collections::HashSet::new(),
            sticker_refs: HashMap::new(),
            photo_thumbs: HashMap::new(),
            message_mentions: HashMap::new(),
            names: HashMap::new(),
            avatar_bytes: HashMap::new(),
            avatar_missing: HashMap::new(),
            avatar_requested: std::collections::HashSet::new(),
            history: HashMap::new(),
            channel_meta: HashMap::new(),
            channel_infos: HashMap::new(),
            server_ids: HashMap::new(),
            channel_history: HashMap::new(),
            pending_channel_votes: HashMap::new(),
            channels_list_requested: false,
            history_armed: std::collections::HashSet::new(),
            poll_ballots: HashMap::new(),
            event_ballots: HashMap::new(),
        });
        app.live_mode = true;
        (app, evt_tx, ctx, cmd_rx)
    }

    pub(super) fn drain_all(app: &mut App, ctx: &egui::Context) {
        for _ in 0..64 {
            app.poll_backend(ctx);
            if app.event_queue.is_empty() {
                return;
            }
        }
        panic!("the event queue did not drain");
    }

    /// The one path that removes messages from the live mirror
    /// (a disappearing message expiring) must take the message's
    /// helper entries with it, and only that message's: every
    /// per-message map is seeded for an expiring and a surviving
    /// message, and after the pass the survivor's entries must
    /// still resolve while the expired one's are all gone.
    #[test]
    pub(super) fn expiry_prunes_helper_entries_of_the_expired_message_only() {
        let (mut app, _tx, _ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        let old_mid = message_id(CHAT_JID, "old-pid");
        let new_mid = message_id(CHAT_JID, "new-pid");
        let old = Message { expires_at: 100, ..Message::mine(old_mid, "about to vanish", "09:00") };
        let new = Message { expires_at: 0, ..Message::mine(new_mid, "stays", "09:01") };
        {
            let session = app.live.as_mut().unwrap();
            session.jids.insert(chat_id, CHAT_JID.to_owned());
            session.messages.insert(chat_id, vec![(1_000, old), (1_060, new)]);
            session.flags.insert(old_mid, store::FLAG_STARRED);
            session.flags.insert(new_mid, store::FLAG_STARRED);
        }
        for mid in [old_mid, new_mid] {
            let pid = if mid == old_mid { "old-pid" } else { "new-pid" };
            app.protocol_ids.insert(mid, pid.to_owned());
            app.sender_jids.insert(mid, "6281234567890@s.whatsapp.net".to_owned());
            app.receipt_log.insert(mid, vec![(ReceiptKind::Read, "09:05".to_owned())]);
            app.my_reactions.insert(mid, "thumbs-up".to_owned());
            app.reaction_actors.insert(
                format!("{chat_id}:{mid}"),
                HashMap::from([("Alice".to_owned(), "thumbs-up".to_owned())]),
            );
        }
        app.unread_live.insert(chat_id, vec!["old-pid".to_owned()]);

        app.expire_disappearing_messages(200);

        let session = app.live.as_ref().unwrap();
        let mirror = session.messages.get(&chat_id).unwrap();
        assert_eq!(mirror.len(), 1, "only the expired message leaves the mirror");
        assert_eq!(mirror[0].1.id, new_mid);
        assert!(!session.flags.contains_key(&old_mid), "expired flags are pruned");
        assert!(session.flags.contains_key(&new_mid), "surviving flags stay");
        for (map, mid) in
            [("protocol", old_mid), ("sender", old_mid), ("receipt", old_mid), ("mine", old_mid)]
        {
            let present = match map {
                "protocol" => app.protocol_ids.contains_key(&mid),
                "sender" => app.sender_jids.contains_key(&mid),
                "receipt" => app.receipt_log.contains_key(&mid),
                _ => app.my_reactions.contains_key(&mid),
            };
            assert!(!present, "the expired message's {map} entry is pruned");
        }
        assert!(
            !app.reaction_actors.contains_key(&format!("{chat_id}:{old_mid}")),
            "the expired message's reaction entry is pruned"
        );
        assert!(
            !app.unread_live.contains_key(&chat_id),
            "the expired message's pending read receipt is pruned with it"
        );

        // Lookups for the retained message still resolve, through
        // the same paths the UI uses: the protocol map itself,
        // the flag path that reads it to persist, and the quote
        // resolution for a reply aimed at the survivor.
        assert_eq!(app.protocol_ids.get(&new_mid).map(String::as_str), Some("new-pid"));
        assert_eq!(
            app.sender_jids.get(&new_mid).map(String::as_str),
            Some("6281234567890@s.whatsapp.net")
        );
        assert!(app.receipt_log.contains_key(&new_mid));
        assert!(app.my_reactions.contains_key(&new_mid));
        assert!(app.reaction_actors.contains_key(&format!("{chat_id}:{new_mid}")));
        app.set_live_flag(chat_id, new_mid, store::FLAG_PINNED_MESSAGE, true);
        assert_eq!(
            app.live.as_ref().unwrap().flags.get(&new_mid).copied().unwrap_or(0),
            store::FLAG_STARRED | store::FLAG_PINNED_MESSAGE,
            "the flag path still finds the survivor's protocol id"
        );
        app.selected_chat = chat_id;
        app.sync_conversation();
        app.composer.reply_to = Some((new_mid, "Sender".to_owned(), "stays".to_owned()));
        let quote = app.live_quote_for_reply().expect("a reply to the survivor still resolves");
        assert_eq!(quote.message_id, "new-pid");
        app.composer.reply_to = Some((old_mid, "Sender".to_owned(), "gone".to_owned()));
        assert!(
            app.live_quote_for_reply().is_none(),
            "a reply to the expired message resolves to nothing, as before"
        );
    }

    /// The expiry pass writes the refreshed preview through to
    /// the archive chat row, not just the in-memory list: after
    /// the newest message expires, the stored preview is the
    /// newest survivor's body (a restart must not resurrect the
    /// expired body), and when the last message expires the
    /// stored preview empties with the mirror's.
    #[test]
    pub(super) fn expiry_writes_the_refreshed_preview_through_to_the_archive() {
        let (mut app, _tx, _ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        let older_mid = message_id(CHAT_JID, "older-pid");
        let newer_mid = message_id(CHAT_JID, "newer-pid");
        let older = Message {
            expires_at: 300,
            ..Message::mine(older_mid, "older, expires later", "09:00")
        };
        let newer = Message {
            expires_at: 100,
            ..Message::mine(newer_mid, "newest, expires first", "09:01")
        };
        // The list row and the archive row both still preview
        // the newest message, as they did before the pass runs.
        let live_row = LiveChat {
            jid: CHAT_JID.to_owned(),
            title: "Sender".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "newest, expires first".to_owned(),
            preview_sender: None,
            timestamp: 1_060,
            unread: 0,
            archived: false,
            locked: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
        };
        app.chats.push(to_chat(&live_row));
        {
            let session = app.live.as_mut().unwrap();
            session.jids.insert(chat_id, CHAT_JID.to_owned());
            session.times.insert(chat_id, 1_060);
            session.messages.insert(chat_id, vec![(1_000, older), (1_060, newer)]);
            session.store.as_ref().unwrap().upsert_chat(&live_row).unwrap();
        }
        app.protocol_ids.insert(older_mid, "older-pid".to_owned());
        app.protocol_ids.insert(newer_mid, "newer-pid".to_owned());

        let mirror_preview =
            |app: &App| app.chats.iter().find(|c| c.id == chat_id).unwrap().preview.clone();
        let stored_preview = |app: &App| {
            app.live
                .as_ref()
                .unwrap()
                .store
                .as_ref()
                .unwrap()
                .chats()
                .unwrap()
                .into_iter()
                .find(|c| c.jid == CHAT_JID)
                .expect("the archive chat row stays")
                .preview
        };

        app.expire_disappearing_messages(200);
        assert_eq!(mirror_preview(&app), "older, expires later");
        assert_eq!(
            stored_preview(&app),
            "older, expires later",
            "the stored preview follows the newest survivor, not the expired message"
        );

        app.expire_disappearing_messages(400);
        assert_eq!(mirror_preview(&app), "");
        assert_eq!(stored_preview(&app), "", "with no survivors the stored preview empties too");
    }

    /// When the last actor removes their reaction, the
    /// per-message actor entry itself is dropped, so the map
    /// holds only messages with live reactions.
    #[test]
    pub(super) fn removing_the_last_reaction_drops_the_actor_entry() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "r1");
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "r1",
            CHAT_JID,
            Some("Alice".to_owned()),
            "react to me",
            1_700_000_000,
            false,
        )))
        .unwrap();
        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "r1".to_owned(),
            actor: "Alice".to_owned(),
            emoji: "thumbs-up".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        let key = format!("{chat_id}:{mid}");
        assert!(app.reaction_actors.contains_key(&key), "the reaction registers its entry");
        let mirror = app.live.as_ref().unwrap().messages.get(&chat_id).unwrap();
        assert_eq!(mirror[0].1.reactions, vec![("thumbs-up".to_owned(), 1)]);

        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: CHAT_JID.to_owned(),
            message_id: "r1".to_owned(),
            actor: "Alice".to_owned(),
            emoji: String::new(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(!app.reaction_actors.contains_key(&key), "the emptied entry is dropped");
        let mirror = app.live.as_ref().unwrap().messages.get(&chat_id).unwrap();
        assert!(mirror[0].1.reactions.is_empty(), "the mirror count still clears");
    }

    /// Toggling a flag off removes the in-memory entry (absence
    /// and zero are the same to every reader) while the zero
    /// itself is still persisted to the archive row.
    #[test]
    pub(super) fn clearing_the_last_flag_removes_the_entry_but_persists_zero() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(CHAT_JID);
        let mid = message_id(CHAT_JID, "f1");
        tx.send(BackendEvent::MessageUpsert(LiveMessage::text(
            "f1",
            CHAT_JID,
            Some("Sender".to_owned()),
            "star me",
            1_700_000_000,
            false,
        )))
        .unwrap();
        drain_all(&mut app, &ctx);

        app.set_live_flag(chat_id, mid, store::FLAG_STARRED, true);
        assert_eq!(
            app.live.as_ref().unwrap().flags.get(&mid).copied().unwrap_or(0),
            store::FLAG_STARRED
        );
        app.set_live_flag(chat_id, mid, store::FLAG_STARRED, false);
        assert!(
            !app.live.as_ref().unwrap().flags.contains_key(&mid),
            "a zeroed flag entry leaves the map"
        );
        let archived =
            app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let row = archived.iter().find(|(m, _)| m.id == "f1").expect("the row is archived");
        assert_eq!(row.1, 0, "the archive row still records the cleared flag");
    }

    /// A poll creation as the backend delivers it in this
    /// harness: three options, zero counts, and the creation
    /// secret plus creator the translation layer extracted.
    pub(super) fn poll_upsert(id: &str) -> LiveMessage {
        let mut msg = LiveMessage::text(
            id,
            CHAT_JID,
            Some("Alice".to_owned()),
            "Lunch?",
            1_700_000_000,
            false,
        );
        msg.kind = backend::LiveMessageKind::Poll {
            question: "Lunch?".to_owned(),
            options: vec![
                ("Deli".to_owned(), 0),
                ("Sushi".to_owned(), 0),
                ("Delivery".to_owned(), 0),
            ],
            multiple: true,
            my_votes: Vec::new(),
            correct: None,
        };
        msg.poll_secret = Some(vec![7u8; 32]);
        msg.poll_creator = Some(CHAT_JID.to_owned());
        msg
    }

    pub(super) fn poll_vote(
        id: &str,
        voter: &str,
        from_me: bool,
        selected: &[usize],
    ) -> BackendEvent {
        BackendEvent::PollVote {
            chat_jid: CHAT_JID.to_owned(),
            message_id: id.to_owned(),
            voter_jid: voter.to_owned(),
            from_me,
            selected: selected.to_vec(),
        }
    }

    /// (counts, my_votes) of the poll in the mirror.
    pub(super) fn mirror_poll(app: &App) -> (Vec<u32>, Vec<usize>) {
        let msg = app
            .live
            .as_ref()
            .unwrap()
            .messages
            .get(&jid_id(CHAT_JID))
            .expect("the chat has messages")
            .iter()
            .map(|(_, m)| m)
            .find(|m| matches!(m.kind, MessageKind::Poll { .. }))
            .expect("the poll is in the mirror");
        match &msg.kind {
            MessageKind::Poll { options, my_votes, .. } => {
                (options.iter().map(|(_, v)| *v).collect(), my_votes.clone())
            }
            _ => unreachable!(),
        }
    }

    /// (counts, my_votes, ballots) of one poll, read from the archive.
    type ArchivedPoll = (Vec<u32>, Vec<usize>, Vec<(String, Vec<usize>)>);

    pub(super) fn archived_poll(app: &App) -> ArchivedPoll {
        let rows = app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let (msg, _) = rows
            .iter()
            .find(|(m, _)| matches!(m.kind, backend::LiveMessageKind::Poll { .. }))
            .expect("the poll is in the archive");
        match &msg.kind {
            backend::LiveMessageKind::Poll { options, my_votes, .. } => (
                options.iter().map(|(_, v)| *v).collect(),
                my_votes.clone(),
                msg.poll_ballots.clone(),
            ),
            _ => unreachable!(),
        }
    }

    /// Item 123: incoming votes aggregate onto the poll, in the
    /// mirror and in the archive. A voter changing their
    /// selection replaces their ballot (counts move, they do not
    /// stack), and an empty selection retracts it.
    #[test]
    pub(super) fn poll_votes_aggregate_with_replacement_and_clear() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(poll_upsert("poll-1"))).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_poll(&app).0, vec![0, 0, 0]);

        tx.send(poll_vote("poll-1", "10000000007@s.whatsapp.net", false, &[0])).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_poll(&app).0, vec![1, 0, 0]);

        tx.send(poll_vote("poll-1", "10000000007@s.whatsapp.net", false, &[0, 2])).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_poll(&app).0, vec![1, 0, 1], "a changed vote replaces, not stacks");

        tx.send(poll_vote("poll-1", "10000000008@s.whatsapp.net", false, &[0])).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_poll(&app).0, vec![2, 0, 1]);

        tx.send(poll_vote("poll-1", "10000000007@s.whatsapp.net", false, &[])).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_poll(&app).0, vec![1, 0, 0], "an empty selection retracts the ballot");

        let (counts, _mine, ballots) = archived_poll(&app);
        assert_eq!(counts, vec![1, 0, 0], "the archive carries the same tally");
        assert_eq!(
            ballots,
            vec![("10000000008@s.whatsapp.net".to_owned(), vec![0])],
            "and the one surviving ballot it was derived from"
        );
    }

    /// Item 123: my vote is applied optimistically when I cast it
    /// (what `vote_poll` does after sending the command); the
    /// server's echo of the same ballot must not count it twice.
    #[test]
    pub(super) fn my_poll_vote_applies_once_when_the_echo_repeats_it() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(poll_upsert("poll-1"))).unwrap();
        drain_all(&mut app, &ctx);

        app.apply_poll_vote(CHAT_JID, "poll-1", "", true, &[1]);
        assert_eq!(mirror_poll(&app), (vec![0, 1, 0], vec![1]));

        tx.send(poll_vote("poll-1", "10000000000@s.whatsapp.net", true, &[1])).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_poll(&app), (vec![0, 1, 0], vec![1]), "the echo changes nothing");
        let (counts, mine, _) = archived_poll(&app);
        assert_eq!((counts, mine), (vec![0, 1, 0], vec![1]), "and the archive agrees");
    }

    /// Item 123: a vote for a poll whose chat is not loaded in
    /// the mirror still lands, straight on the archived copy.
    #[test]
    pub(super) fn poll_vote_for_an_unloaded_chat_updates_the_archive() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(poll_upsert("poll-1"))).unwrap();
        drain_all(&mut app, &ctx);
        app.live.as_mut().unwrap().messages.remove(&jid_id(CHAT_JID));

        tx.send(poll_vote("poll-1", "10000000007@s.whatsapp.net", false, &[2])).unwrap();
        drain_all(&mut app, &ctx);
        let (counts, _mine, ballots) = archived_poll(&app);
        assert_eq!(counts, vec![0, 0, 1]);
        assert_eq!(ballots, vec![("10000000007@s.whatsapp.net".to_owned(), vec![2])]);
    }

    /// Item 123: the ballots seeded from the archive at session
    /// start (the load start_live performs) make the next vote
    /// from the same voter replace their pre-restart ballot
    /// instead of stacking on top of it.
    #[test]
    pub(super) fn seeded_ballots_make_the_next_vote_replace() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(poll_upsert("poll-1"))).unwrap();
        tx.send(poll_vote("poll-1", "10000000007@s.whatsapp.net", false, &[0])).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(archived_poll(&app).0, vec![1, 0, 0]);

        // The restart load, performed the way start_live performs
        // it: mirror rebuilt from the archive rows, ballot map
        // seeded from the archived ballots.
        {
            let session = app.live.as_mut().unwrap();
            let rows = session.store.as_ref().unwrap().messages(CHAT_JID).unwrap();
            session.poll_ballots.clear();
            for (m, _) in &rows {
                if !m.poll_ballots.is_empty() {
                    session
                        .poll_ballots
                        .insert(message_id(&m.chat_jid, &m.id), m.poll_ballots.clone());
                }
            }
            session.messages.insert(
                jid_id(CHAT_JID),
                rows.iter()
                    .map(|(m, flags)| (m.timestamp, to_message_with_flags(m, *flags)))
                    .collect(),
            );
        }

        tx.send(poll_vote("poll-1", "10000000007@s.whatsapp.net", false, &[2])).unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            mirror_poll(&app).0,
            vec![0, 0, 1],
            "the pre-restart ballot is replaced, not stacked"
        );
        assert_eq!(archived_poll(&app).0, vec![0, 0, 1]);
    }

    /// Item 123: an archived poll that kept its creation secret
    /// builds the registration command the next session sends to
    /// the backend; anything without a kept secret builds none.
    #[test]
    pub(super) fn archived_poll_secret_builds_a_registration_command() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        tx.send(BackendEvent::MessageUpsert(poll_upsert("poll-1"))).unwrap();
        drain_all(&mut app, &ctx);

        let rows = app.live.as_ref().unwrap().store.as_ref().unwrap().messages(CHAT_JID).unwrap();
        let (msg, _) = rows.iter().find(|(m, _)| m.id == "poll-1").unwrap();
        let cmd = poll_secret_registration(msg).expect("the archived poll carries its secret");
        let BackendCommand::RegisterPollSecret {
            chat_jid,
            message_id,
            creator_jid,
            secret,
            options,
        } = cmd
        else {
            panic!("the registration is a RegisterPollSecret command");
        };
        assert_eq!(chat_jid, CHAT_JID);
        assert_eq!(message_id, "poll-1");
        assert_eq!(creator_jid, CHAT_JID);
        assert_eq!(secret, vec![7u8; 32]);
        assert_eq!(options, vec!["Deli".to_owned(), "Sushi".to_owned(), "Delivery".to_owned()]);

        let plain = LiveMessage::text("t-1", CHAT_JID, None, "hi", 1_700_000_000, false);
        assert!(poll_secret_registration(&plain).is_none(), "no secret, no registration");
    }
}

#[cfg(test)]
mod reaction_tests {
    use super::*;
    use crate::state::{App, Args};

    const GROUP_JID: &str = "120363001234567890@g.us";
    const DM_JID: &str = "6281234567890@s.whatsapp.net";
    const ALICE_JID: &str = "6281111111111@s.whatsapp.net";

    /// An app in live mode on a hand-fed backend channel, the
    /// same harness shape as the other live test modules.
    pub(super) fn live_app() -> (
        App,
        std::sync::mpsc::Sender<BackendEvent>,
        egui::Context,
        std::sync::mpsc::Receiver<BackendCommand>,
    ) {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = App::new(Args::parse(), &ctx);
        app.live_start_error = None;
        app.logged_out = false;
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<BackendCommand>();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel::<BackendEvent>();
        app.live = Some(LiveSession {
            channels: BackendChannels { commands: cmd_tx, events: evt_rx },
            status: LinkStatus::Linked,
            qr: None,
            qr_expires_at: None,
            pair_code: None,
            pair_code_at: None,
            account: "Test account".to_owned(),
            connected_at: None,
            syncing: false,
            sync_last_activity: None,
            synced_once: false,
            store: Some(store::Store::open_in_memory().unwrap()),
            jids: HashMap::new(),
            messages: HashMap::new(),
            times: HashMap::new(),
            fake: true,
            logout_requested: false,
            flags: HashMap::new(),
            engaged_chats: std::collections::HashSet::new(),
            poll_ballots: HashMap::new(),
            event_ballots: HashMap::new(),
            photo_refs: HashMap::new(),
            voice_refs: HashMap::new(),
            video_refs: HashMap::new(),
            video_thumbs: HashMap::new(),
            poster_refs: HashMap::new(),
            video_dims: HashMap::new(),
            document_refs: HashMap::new(),
            gif_videos: std::collections::HashSet::new(),
            sticker_anim: HashMap::new(),
            sticker_thumbs: HashMap::new(),
            document_thumbs: HashMap::new(),
            location_thumbs: HashMap::new(),
            preview_thumbs: HashMap::new(),
            view_once_ids: std::collections::HashSet::new(),
            verified_names: HashMap::new(),
            verified_checked: std::collections::HashSet::new(),
            sticker_refs: HashMap::new(),
            photo_thumbs: HashMap::new(),
            message_mentions: HashMap::new(),
            names: HashMap::new(),
            avatar_bytes: HashMap::new(),
            avatar_missing: HashMap::new(),
            avatar_requested: std::collections::HashSet::new(),
            history: HashMap::new(),
            channel_meta: HashMap::new(),
            channel_infos: HashMap::new(),
            server_ids: HashMap::new(),
            channel_history: HashMap::new(),
            pending_channel_votes: HashMap::new(),
            channels_list_requested: false,
            history_armed: std::collections::HashSet::new(),
        });
        app.live_mode = true;
        (app, evt_tx, ctx, cmd_rx)
    }

    pub(super) fn drain_all(app: &mut App, ctx: &egui::Context) {
        for _ in 0..64 {
            app.poll_backend(ctx);
            if app.event_queue.is_empty() {
                return;
            }
        }
        panic!("the event queue did not drain");
    }

    pub(super) fn sent_commands(
        cmd_rx: &std::sync::mpsc::Receiver<BackendCommand>,
    ) -> Vec<BackendCommand> {
        let mut out = Vec::new();
        while let Ok(cmd) = cmd_rx.try_recv() {
            out.push(cmd);
        }
        out
    }

    /// Feeds one message into the live mirror and opens its
    /// chat, returning the app message ID.
    pub(super) fn open_message(
        app: &mut App,
        tx: &std::sync::mpsc::Sender<BackendEvent>,
        ctx: &egui::Context,
        msg: LiveMessage,
    ) -> u64 {
        let chat_jid = msg.chat_jid.clone();
        let proto = msg.id.clone();
        tx.send(BackendEvent::MessageUpsert(msg)).unwrap();
        drain_all(app, ctx);
        app.selected_chat = jid_id(&chat_jid);
        app.sync_conversation();
        message_id(&chat_jid, &proto)
    }

    pub(super) fn alice_group_message() -> LiveMessage {
        let mut msg = LiveMessage::text(
            "g1",
            GROUP_JID,
            Some("Alice".to_owned()),
            "hello group",
            1_700_000_000,
            false,
        );
        msg.sender_jid = Some(ALICE_JID.to_owned());
        msg
    }

    pub(super) fn mirror_reactions(app: &App, chat_id: u64, mid: u64) -> Vec<(String, u32)> {
        let mirror = app.live.as_ref().unwrap().messages.get(&chat_id).unwrap();
        mirror.iter().find(|(_, m)| m.id == mid).unwrap().1.reactions.clone()
    }

    pub(super) fn react_command(
        cmd_rx: &std::sync::mpsc::Receiver<BackendCommand>,
    ) -> (String, String, Option<String>, bool, String) {
        let sent = sent_commands(cmd_rx);
        let found = sent.iter().find_map(|c| match c {
            BackendCommand::React { chat_jid, message_id, sender_jid, from_me, emoji } => Some((
                chat_jid.clone(),
                message_id.clone(),
                sender_jid.clone(),
                *from_me,
                emoji.clone(),
            )),
            _ => None,
        });
        found.expect("a reaction command is sent")
    }

    /// A group reaction names the participant who authored the
    /// target message: the JID recorded for the message when it
    /// arrived is what the command carries, not None.
    #[test]
    pub(super) fn group_reaction_names_the_message_author() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.apply_reaction(mid, "👍");
        let (chat_jid, message_id, sender_jid, from_me, emoji) = react_command(&cmd_rx);
        assert_eq!(chat_jid, GROUP_JID);
        assert_eq!(message_id, "g1");
        assert_eq!(sender_jid.as_deref(), Some(ALICE_JID), "the author is named");
        assert!(!from_me);
        assert_eq!(emoji, "👍");
    }

    /// Reacting to my own group message needs no sender: the
    /// library resolves my group identity itself, and a sender
    /// in my name could even be the wrong one (PN vs LID
    /// addressing is the library's call).
    #[test]
    pub(super) fn own_group_reaction_omits_the_sender() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let msg = LiveMessage::text("g2", GROUP_JID, None, "my own", 1_700_000_000, true);
        let mid = open_message(&mut app, &tx, &ctx, msg);
        app.apply_reaction(mid, "👍");
        let (_, _, sender_jid, from_me, _) = react_command(&cmd_rx);
        assert_eq!(sender_jid, None);
        assert!(from_me);
    }

    /// Direct-chat reactions are unchanged: no sender is
    /// attached even though the partner's JID is on record,
    /// because a DM key carries no participant.
    #[test]
    pub(super) fn dm_reaction_carries_no_sender() {
        let (mut app, tx, ctx, cmd_rx) = live_app();
        let mut msg =
            LiveMessage::text("d1", DM_JID, Some("Partner".to_owned()), "hi", 1_700_000_000, false);
        msg.sender_jid = Some(DM_JID.to_owned());
        let mid = open_message(&mut app, &tx, &ctx, msg);
        app.apply_reaction(mid, "👍");
        let (chat_jid, _, sender_jid, from_me, _) = react_command(&cmd_rx);
        assert_eq!(chat_jid, DM_JID);
        assert_eq!(sender_jid, None);
        assert!(!from_me);
    }

    /// The optimistic write and the server's echo of my own
    /// reaction are the same write into the actor map: the echo
    /// confirms the pending change and the count does not move.
    #[test]
    pub(super) fn my_reaction_echo_confirms_without_double_counting() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            actor: "Alice".to_owned(),
            emoji: "👍".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_reactions(&app, chat_id, mid), vec![("👍".to_owned(), 1)]);

        app.apply_reaction(mid, "👍");
        assert_eq!(mirror_reactions(&app, chat_id, mid), vec![("👍".to_owned(), 2)]);
        assert_eq!(app.my_reactions.get(&mid).map(String::as_str), Some("👍"));
        assert!(app.pending_reactions.contains_key(&mid), "the change awaits settlement");

        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            actor: "Me".to_owned(),
            emoji: "👍".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            mirror_reactions(&app, chat_id, mid),
            vec![("👍".to_owned(), 2)],
            "the echo does not count me twice"
        );
        assert!(!app.pending_reactions.contains_key(&mid), "the echo confirms the change");
        assert_eq!(app.my_reactions.get(&mid).map(String::as_str), Some("👍"));
    }

    /// My reaction disappearing server-side (removed from
    /// another device) converges the local state to the
    /// server's set: my picker state clears and my count goes.
    #[test]
    pub(super) fn server_removal_of_my_reaction_converges() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.apply_reaction(mid, "❤️");
        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            actor: "Me".to_owned(),
            emoji: "❤️".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_reactions(&app, chat_id, mid), vec![("❤️".to_owned(), 1)]);

        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            actor: "Me".to_owned(),
            emoji: String::new(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert!(mirror_reactions(&app, chat_id, mid).is_empty(), "my count is gone");
        assert!(!app.my_reactions.contains_key(&mid), "my picker state clears");
        assert!(
            !app.reaction_actors.contains_key(&format!("{chat_id}:{mid}")),
            "the emptied actor entry is dropped"
        );
    }

    /// A reaction the backend reports as failed rolls the
    /// optimistic state back to what the server last confirmed,
    /// and the failure reaches the notice banner.
    #[test]
    pub(super) fn failed_group_reaction_rolls_back() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            actor: "Alice".to_owned(),
            emoji: "👍".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);

        app.apply_reaction(mid, "❤️");
        let mut optimistic = mirror_reactions(&app, chat_id, mid);
        optimistic.sort();
        assert_eq!(optimistic, vec![("❤️".to_owned(), 1), ("👍".to_owned(), 1)]);
        assert_eq!(app.my_reactions.get(&mid).map(String::as_str), Some("❤️"));

        tx.send(BackendEvent::ReactionFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Reaction failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(
            mirror_reactions(&app, chat_id, mid),
            vec![("👍".to_owned(), 1)],
            "only Alice's confirmed reaction remains"
        );
        assert!(!app.my_reactions.contains_key(&mid), "my optimistic reaction is undone");
        assert!(!app.pending_reactions.contains_key(&mid), "nothing stays pending");
        let view = app.conversation.iter().find(|m| m.id == mid).unwrap();
        assert_eq!(view.reactions, vec![("👍".to_owned(), 1)], "the open view rolls back too");
        let notice = app.notice_str().unwrap_or_default().to_owned();
        assert!(notice.contains("Reaction failed"), "the failure is surfaced: {notice}");
    }

    /// Rolling back a *change* of reaction restores the emoji
    /// the server had confirmed before, not an empty state.
    #[test]
    pub(super) fn failed_reaction_change_restores_the_previous_emoji() {
        let (mut app, tx, ctx, _cmd_rx) = live_app();
        let chat_id = jid_id(GROUP_JID);
        let mid = open_message(&mut app, &tx, &ctx, alice_group_message());
        app.apply_reaction(mid, "👍");
        tx.send(BackendEvent::ReactionUpdate {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            actor: "Me".to_owned(),
            emoji: "👍".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_reactions(&app, chat_id, mid), vec![("👍".to_owned(), 1)]);

        app.apply_reaction(mid, "❤️");
        assert_eq!(mirror_reactions(&app, chat_id, mid), vec![("❤️".to_owned(), 1)]);

        tx.send(BackendEvent::ReactionFailed {
            chat_jid: GROUP_JID.to_owned(),
            message_id: "g1".to_owned(),
            message: "Reaction failed: connection lost".to_owned(),
        })
        .unwrap();
        drain_all(&mut app, &ctx);
        assert_eq!(mirror_reactions(&app, chat_id, mid), vec![("👍".to_owned(), 1)]);
        assert_eq!(app.my_reactions.get(&mid).map(String::as_str), Some("👍"));
    }
}
