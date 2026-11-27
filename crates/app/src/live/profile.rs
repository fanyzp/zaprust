//! Own profile and privacy: the profile save path, privacy
//! state and cycling, member and request row resolution, and
//! read engagement.

use super::*;

impl crate::state::App {
    /// Adopts a privacy state fetched from the account. The map
    /// itself drives the account rows on the Privacy page, and
    /// one value in it is also a local setting: read receipts.
    /// That flag mirrors the account setting for the toggle on
    /// the Privacy page (receipt dispatch itself is not gated
    /// by it; the library downgrades what it sends when the
    /// account has receipts off), and it is loaded from the
    /// settings file at startup,
    /// before any fetch lands, so the server's value (the user
    /// may have changed it on their phone) is adopted into the
    /// flag and written through to the persisted settings. Left
    /// unreconciled the two silently disagree in the exposing
    /// direction: receipts the account has off keep going out
    /// until the toggle here happens to be flipped. Only the
    /// spellings the toggle itself writes are adopted; an
    /// unrecognized value leaves the flag alone. Demo mode never
    /// adopts: the fake backend's scripted state is not an
    /// account and must not rewrite real settings.
    pub(super) fn apply_privacy_state(&mut self, map: std::collections::BTreeMap<String, String>) {
        if self.live_mode {
            if let Some(on) = server_read_receipts(map.get("readreceipts")) {
                if on != self.read_receipts {
                    self.read_receipts = on;
                    self.persist_settings();
                }
            }
            // The status audience rides this map in its own two
            // keys (it is an app-state record, not a privacy-IQ
            // category): adopt it when present, and when a state
            // lands without it, ask for it once per session. The
            // ask is guarded by the synced flag, because a
            // backend with no audience observed yet answers
            // without the keys, and re-asking on every state
            // would loop commands forever.
            if let Some(mode) = map.get("statusprivacy") {
                self.updates.status_privacy_mode = mode.clone();
                let adopted: Vec<String> = map
                    .get("statusprivacy_list")
                    .map(|list| {
                        list.split(',').filter(|j| !j.is_empty()).map(str::to_owned).collect()
                    })
                    .unwrap_or_default();
                // An entry can arrive naming a form this
                // session's reports have already retired: the
                // contact changed number before this fetch
                // landed, so the backend's canonical rewrite
                // through the book returned the retired form
                // unchanged, the book holding no mapping for
                // it any more. Each adopted entry is carried
                // through the same heal the report migration
                // applies (see heal_stored_entry): an entry
                // naming a retired form follows the person, so
                // the mirror names who the record means from
                // the moment it lands; an entry whose person
                // left the saved set while their form passed
                // to a different identified person drops out
                // instead of retargeting to the form's new
                // holder; entries the current report knows
                // were canonicalized by the backend, and
                // entries no map knows pass through as the
                // record carries them. When the heal moves
                // the list, the healed list goes back so the
                // record heals too: a fetched record landing
                // is a data change, not a viewing, so the
                // dispatch keeps the editors' discipline
                // (nothing is sent for merely opening an
                // editor).
                let followed: Vec<String> = match &self.live {
                    Some(session) => {
                        adopted.iter().filter_map(|jid| session.heal_stored_entry(jid)).collect()
                    }
                    None => adopted.clone(),
                };
                let heal = if followed != adopted { Some(followed.clone()) } else { None };
                self.updates.status_privacy_list = followed;
                self.updates.status_privacy_synced = true;
                if let Some(jids) = heal {
                    self.send_backend(BackendCommand::SetStatusPrivacy {
                        mode: mode.clone(),
                        jids,
                    });
                }
            } else if !self.updates.status_privacy_synced {
                self.updates.status_privacy_synced = true;
                self.send_backend(BackendCommand::FetchStatusPrivacy);
            }
        }
        self.privacy_state = map;
    }

    /// Applies an exception-list edit from the Privacy page:
    /// the list is recorded as this session's known list for
    /// the category, and in live mode it goes to the account
    /// through the backend, which sets the category to
    /// `contact_blacklist` with exactly this list. The backend
    /// answers with a fresh PrivacyState, so a rejected edit
    /// reconciles the row's value with the server's. This is
    /// called only when the user actually toggles a contact in
    /// the editor: opening the editor records nothing and
    /// sends nothing, so the category keeps its current value
    /// until the user picks someone.
    pub(crate) fn set_privacy_exceptions(&mut self, category: &str, jids: Vec<String>) {
        self.privacy_exception_lists.insert(category.to_owned(), jids.clone());
        if self.live_mode {
            self.send_backend(BackendCommand::SetPrivacyExceptions {
                category: category.to_owned(),
                jids,
            });
        }
    }

    /// Whether the category exception editor's row for one
    /// person (a current canonical key from
    /// [`Self::privacy_contact_options`]) displays as
    /// excepted: any mirror entry that resolves to the
    /// person counts, not only a byte-identical one. The
    /// mirror names people in the forms their picks were
    /// recorded in, so after a mid-session re-point an
    /// excepted person's mirror entry names their retired
    /// number while their row carries the new one; comparing
    /// raw strings would show them unexcepted while the
    /// account still excepts them. The comparison goes
    /// through the session's stored-JID resolution, the same
    /// one the status surfaces use, so the display follows
    /// the person. Without a session the raw compare stands.
    pub(crate) fn privacy_exception_contains(&self, chosen: &[String], jid: &str) -> bool {
        match &self.live {
            Some(session) => chosen.iter().any(|entry| session.resolve_stored_jid(entry) == jid),
            None => chosen.iter().any(|entry| entry == jid),
        }
    }

    /// The exception list after toggling one person row in
    /// the category exception editor: turning a row off
    /// removes every mirror entry that resolves to the
    /// person (the entry may name them under a form a
    /// re-point has since retired, and leaving it in would
    /// keep the exception the row no longer shows, with no
    /// checked row left to remove it by); turning a row on
    /// appends the row's current canonical key.
    pub(crate) fn privacy_exception_toggled(
        &self,
        chosen: &[String],
        jid: &str,
        checked: bool,
    ) -> Vec<String> {
        if checked {
            match &self.live {
                Some(session) => chosen
                    .iter()
                    .filter(|entry| session.resolve_stored_jid(entry) != jid)
                    .cloned()
                    .collect(),
                None => chosen.iter().filter(|entry| entry.as_str() != jid).cloned().collect(),
            }
        } else {
            let mut next = chosen.to_vec();
            next.push(jid.to_owned());
            next
        }
    }

    /// The contacts the Privacy page's people pickers offer,
    /// as (JID, display name), one row per person under their
    /// canonical JID: the account's DM partners first, in chat
    /// order, resolved through the live session's chat map and
    /// its stored-JID resolution (the saved-contact form map,
    /// then the retired forms a re-point left behind: a DM
    /// partner whose chat is addressed under an alternate or
    /// retired form is listed once, under the canonical JID),
    /// then the
    /// account's saved contacts that are not already listed,
    /// under their saved names, sorted by name. The pickers
    /// offer DM partners plus the account's saved contacts;
    /// everyone offered is someone the user can explicitly
    /// pick, so the editor still fails safe. Empty without a
    /// session.
    pub(crate) fn privacy_contact_options(&self) -> Vec<(String, String)> {
        let Some(session) = &self.live else {
            return Vec::new();
        };
        let mut options: Vec<(String, String)> = Vec::new();
        for chat in self.chats.iter().filter(|chat| chat.kind == core::model::ChatKind::Dm) {
            let Some(jid) = session.jids.get(&chat.id) else {
                continue;
            };
            let canonical = session.resolve_stored_jid(jid);
            if options.iter().any(|(listed, _)| listed == &canonical) {
                continue;
            }
            options.push((canonical, chat.title.clone()));
        }
        let mut extra: Vec<(String, String)> = session
            .saved_contacts
            .iter()
            .filter(|(jid, _)| !options.iter().any(|(listed, _)| listed == *jid))
            .map(|(jid, name)| (jid.clone(), name.clone()))
            .collect();
        extra.sort_by(|a, b| a.1.cmp(&b.1));
        options.extend(extra);
        options
    }

    /// Applies a status privacy change from the Privacy page:
    /// the local mirror moves at once (the Updates composer
    /// computes its audience from it), and in live mode the
    /// account record follows through the backend. The
    /// backend's answer re-asserts the audience it holds, so a
    /// rejected change reconciles the mirror with reality.
    /// The incoming JIDs are resolved through the session's
    /// stored-JID resolution (the saved-contact form map,
    /// then the retired forms a re-point left behind) before
    /// they are stored or sent, so the mirror and the editor's
    /// checkbox state are canonical immediately, and a list
    /// the editor carries forward from an earlier pick names
    /// each person under their current key, not the form the
    /// pick was recorded in; an entry no resolution knows
    /// passes through unchanged, and the backend additionally
    /// canonicalizes what it writes to the account.
    pub(crate) fn set_status_privacy(&mut self, mode: &str, jids: Vec<String>) {
        let jids: Vec<String> = match &mut self.live {
            Some(session) => {
                let resolved: Vec<String> =
                    jids.into_iter().map(|jid| session.resolve_stored_jid(&jid)).collect();
                // A list the editor writes names the people
                // the picker showed, under their current
                // keys: a form it names is spoken for by a
                // current pick, so an orphan mark an earlier
                // report left on that form (it once changed
                // hands from an owner who left the saved set)
                // no longer describes it, and the heals stop
                // dropping it.
                session.orphaned_contact_forms.retain(|form| !resolved.contains(form));
                resolved
            }
            None => jids,
        };
        self.updates.status_privacy_mode = mode.to_owned();
        self.updates.status_privacy_list = jids.clone();
        if self.live_mode {
            self.send_backend(BackendCommand::SetStatusPrivacy { mode: mode.to_owned(), jids });
        }
    }

    /// Which world the My Profile editor runs in, from the
    /// session state: no session is demo mode; a linked session
    /// (real or test mode, both honor the profile commands) edits
    /// the account; a session that is not linked yet has no
    /// account profile to edit.
    pub(crate) fn my_profile_mode(&self) -> crate::profile::MyProfileMode {
        match &self.live {
            // Live mode without a session (a start that failed,
            // or a session ended by logging out) is not demo
            // mode: there is no account to edit until a device
            // links again.
            None if self.live_mode => crate::profile::MyProfileMode::NotLinked,
            None => crate::profile::MyProfileMode::Demo,
            Some(session) if session.status.is_linked() => crate::profile::MyProfileMode::Live,
            Some(_) => crate::profile::MyProfileMode::NotLinked,
        }
    }

    /// Opens the My Profile editor. In live mode it also asks the
    /// backend for a fresh read-back, so the editor opens on the
    /// account's current values rather than the connect-time
    /// ones. A failure left over from a previous save is cleared:
    /// it described that attempt, not this one.
    pub(crate) fn open_my_profile(&mut self) {
        self.my_profile_open = true;
        if self.my_profile.save_phase != crate::profile::SavePhase::Saving {
            self.my_profile.save_phase = crate::profile::SavePhase::Idle;
        }
        if self.my_profile_mode() == crate::profile::MyProfileMode::Live {
            self.send_backend(BackendCommand::FetchOwnProfile);
        }
    }

    /// Applies a Save click in the My Profile editor. Demo mode
    /// persists the buffers locally, as it always has. Live mode
    /// builds the pending update (exactly the fields that differ
    /// from the account read-back), marks the save in flight, and
    /// sends it; the outcome arrives as the backend's re-read or
    /// its failure event, applied by [`App::apply_own_profile`]
    /// and [`App::apply_own_profile_save_failed`]. A save with no
    /// changes just closes the panel.
    pub(crate) fn save_my_profile(&mut self) {
        match self.my_profile_mode() {
            crate::profile::MyProfileMode::Demo => {
                self.persist_settings();
                self.notice = Some(Notice::info(crate::i18n::t("profile.my_saved")));
                self.my_profile_open = false;
            }
            crate::profile::MyProfileMode::Live => match self.my_profile.pending_update() {
                Ok(Some(update)) => {
                    self.my_profile.save_phase = crate::profile::SavePhase::Saving;
                    self.send_backend(BackendCommand::UpdateOwnProfile {
                        name: update.name,
                        about: update.about,
                        photo: update.photo,
                    });
                }
                Ok(None) => {
                    self.my_profile_open = false;
                }
                Err(message) => {
                    self.notice = Some(Notice::error(message));
                }
            },
            crate::profile::MyProfileMode::NotLinked => {}
        }
    }

    /// Opens the photo picker for the My Profile editor and loads
    /// the chosen file. Live mode keeps the file's bytes as the
    /// pending picture, validated as a decodable image here so a
    /// bad pick is refused at pick time rather than at save time;
    /// demo mode keeps the file's path, as it always has.
    pub(crate) fn pick_my_profile_photo(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"])
            .set_title(crate::i18n::t("profile.my_change_photo"))
            .pick_file()
        else {
            return;
        };
        match self.my_profile_mode() {
            crate::profile::MyProfileMode::Live => match std::fs::read(&path) {
                Ok(bytes) if image::load_from_memory(&bytes).is_ok() => {
                    self.my_profile.picked_photo = Some(bytes);
                    self.my_profile.photo_removed = false;
                    self.my_profile.photo_error = false;
                }
                _ => {
                    self.my_profile.photo_error = true;
                }
            },
            _ => {
                self.my_profile.photo_path = Some(path.to_string_lossy().into_owned());
            }
        }
    }

    /// Adopts the account's own profile as read back from the
    /// server: the editor's account truth, and the values the
    /// account surfaces show. A save in flight is resolved by the
    /// read-back (the backend re-reads after every save), so it
    /// completes with the saved notice. A read-back landing while
    /// the user is mid-edit updates the account truth but never
    /// the buffers: what they typed stays theirs until they save
    /// or the panel closes. The settings cache follows the
    /// adopted values only for a real session: a test-mode
    /// session's scripted profile is not the user's account and
    /// must not rewrite it (the same rule as
    /// [`App::apply_privacy_state`]).
    pub(super) fn apply_own_profile(&mut self, profile: backend::OwnProfile) {
        let account = crate::profile::AccountProfile {
            name: profile.name,
            jid: profile.jid,
            lid: profile.lid,
            about: profile.about,
            photo: profile.photo,
        };
        let was_saving = self.my_profile.save_phase == crate::profile::SavePhase::Saving;
        let dirty = self.my_profile.live_dirty();
        let changed = self.my_profile.account.as_ref() != Some(&account);
        if was_saving {
            self.my_profile.save_phase = crate::profile::SavePhase::Idle;
            self.my_profile.adopt_account(account);
            self.notice = Some(Notice::info(crate::i18n::t("profile.my_saved")));
        } else if !dirty {
            self.my_profile.adopt_account(account);
        } else {
            self.my_profile.account = Some(account);
        }
        if changed && !self.my_profile.live_dirty() && self.live.as_ref().is_some_and(|s| !s.fake) {
            self.persist_settings();
        }
    }

    /// A profile save failed on the server: the panel shows the
    /// reason and the buffers revert to the account values the
    /// server kept. The reconciling read-back that follows lands
    /// on a clean editor and changes nothing.
    pub(super) fn apply_own_profile_save_failed(&mut self, message: String) {
        if self.my_profile.save_phase != crate::profile::SavePhase::Saving {
            return;
        }
        self.my_profile.save_phase = crate::profile::SavePhase::Failed(message);
        if let Some(account) = self.my_profile.account.clone() {
            self.my_profile.adopt_account(account);
        }
    }

    /// Adopts the account-wide default disappearing timer the
    /// server pushed (a change made on another device, the phone
    /// included). The control is a local value loaded from the
    /// settings file at startup; without this adoption a default
    /// changed on the phone and the value this client applies to
    /// new chats silently disagree, in the exposing direction
    /// both ways. There is no fetch for the account default (the
    /// library offers only the set), so this push is the only
    /// reconciliation channel, the same server-wins rule as
    /// [`App::apply_privacy_state`]. Demo mode never adopts.
    pub(super) fn apply_default_disappearing_timer(&mut self, duration_secs: u32) {
        if self.live_mode && duration_secs != self.default_ephemeral_secs {
            self.default_ephemeral_secs = duration_secs;
            self.persist_settings();
        }
    }

    /// Persists the blocked-chat set to its home, the encrypted
    /// archive store (privacy audit A6: the set used to ride in
    /// the plaintext settings file as FNV-1a hashes of
    /// phone-number JIDs, reversible by dictionary over the
    /// phone-number space). No live session (demo mode, or the
    /// archive failed to open) means no persisted copy: the set
    /// then lives in memory for the session, and in live mode
    /// the server's connect-time blocklist re-seeds it anyway.
    pub(crate) fn persist_blocked_chats(&self) {
        if let Some(store) = self.live.as_ref().and_then(|s| s.store.as_ref()) {
            let mut ids: Vec<u64> = self.blocked_chats.iter().copied().collect();
            ids.sort_unstable();
            let _ = store.set_blocked_chats(&ids);
        }
    }

    /// Names this session already knows for people, keyed by
    /// JID: the titles of 1:1 chats (the push name the chat list
    /// resolved for that partner) and the sender names carried
    /// on mirrored messages. Built once per member/request
    /// fetch so resolving a whole list stays a single pass over
    /// the mirror. A chat title that is just the partner's own
    /// number (the fallback when no push name was ever seen) is
    /// not a name and is skipped.
    pub(super) fn known_person_names(&self) -> HashMap<String, String> {
        let mut names: HashMap<String, String> = HashMap::new();
        let Some(session) = &self.live else {
            return names;
        };
        for (chat_id, jid) in &session.jids {
            let Some(chat) = self.chats.iter().find(|c| c.id == *chat_id) else {
                continue;
            };
            if chat.kind != ChatKind::Dm {
                continue;
            }
            let title = chat.title.trim();
            if !title.is_empty() && title != jid_user_part(jid) {
                names.insert(jid.clone(), chat.title.clone());
            }
        }
        for messages in session.messages.values() {
            for (_, msg) in messages {
                let Some(name) = &msg.sender else { continue };
                if name.trim().is_empty() {
                    continue;
                }
                if let Some(jid) = self.sender_jids.get(&msg.id) {
                    names.entry(jid.clone()).or_insert_with(|| name.clone());
                }
            }
        }
        names
    }

    /// Resolves backend member rows into renderable rows (see
    /// [`crate::state::MemberRow`]): the label is the best name
    /// available for the participant and the detail the number
    /// line. Backend addresses pass through untouched; they are
    /// command targets, never labels.
    pub(super) fn resolve_member_rows(
        &self,
        members: Vec<backend::GroupMemberRow>,
    ) -> Vec<crate::state::MemberRow> {
        let known = self.known_person_names();
        members
            .into_iter()
            .map(|(address, name, admin, member_label)| {
                let (label, named) = resolve_person_label(&name, &address, &known);
                crate::state::MemberRow {
                    detail: member_detail(&address, named),
                    address,
                    label,
                    admin,
                    member_label,
                }
            })
            .collect()
    }

    /// Resolves backend join-request rows into (jid, label)
    /// rows, with the same label rule as member rows.
    pub(super) fn resolve_request_rows(
        &self,
        requests: Vec<(String, String)>,
    ) -> Vec<(String, String)> {
        let known = self.known_person_names();
        requests
            .into_iter()
            .map(|(jid, name)| {
                let (label, _) = resolve_person_label(&name, &jid, &known);
                (jid, label)
            })
            .collect()
    }

    /// Applies a read-receipts toggle change from the Privacy
    /// page. The local flag flips at once so the toggle shows
    /// the choice, the choice is persisted, and in live mode
    /// the account setting follows via SetPrivacy: the library
    /// reads that account setting when it builds receipts, so
    /// turning receipts off takes effect there, in the
    /// downgrade to read-self receipts for direct chats. The
    /// backend answers a successful change with a fresh PrivacyState, which
    /// [`App::apply_privacy_state`] adopts as confirmation; a
    /// failure surfaces as a notice and the same fetch re-adopts
    /// the value the server kept, so the toggle never keeps
    /// claiming a state the server rejected.
    pub(crate) fn set_read_receipts(&mut self, on: bool) {
        self.read_receipts = on;
        self.persist_settings();
        if self.live_mode {
            self.send_backend(BackendCommand::SetPrivacy {
                category: "readreceipts".to_owned(),
                value: if on { "all" } else { "none" }.to_owned(),
            });
        }
    }

    /// Cycles one account-privacy row on the Privacy page to its
    /// next audience and sends it to the account. The displayed
    /// value is not touched here: it comes from `privacy_state`,
    /// which only a fresh fetch updates, on success and on failure
    /// alike (the backend re-asserts the fetched state after a
    /// rejected change), so the page never claims a value the
    /// server did not take.
    pub(crate) fn cycle_privacy(&mut self, category: &str) {
        let current = self.privacy_state.get(category).cloned().unwrap_or_default();
        let Some(next) = next_privacy_audience(category, &current) else {
            return;
        };
        if self.live_mode {
            self.send_backend(BackendCommand::SetPrivacy {
                category: category.to_owned(),
                value: next.to_owned(),
            });
        }
    }

    /// The Privacy page's entry point for cycling a row. Leaving
    /// `contact_blacklist` is the one cycle step that destroys
    /// data this client cannot restore: the value names an
    /// exception list configured on the phone, and replacing it
    /// with a plain audience discards the association with that
    /// list (the backend refuses to set the list back without
    /// its contacts). That step asks first, through the same
    /// confirmation the other destructive actions use; every
    /// other step is a reversible value-to-value change and
    /// cycles at once.
    pub(crate) fn request_cycle_privacy(&mut self, category: &str, label: &str) {
        let current = self.privacy_state.get(category).cloned().unwrap_or_default();
        if current == "contact_blacklist" {
            self.ui_confirm = Some(crate::state::UiConfirm::ReplaceExceptionList {
                category: category.to_owned(),
                label: label.to_owned(),
            });
            return;
        }
        self.cycle_privacy(category);
    }

    /// Records a real read action in the selected chat (the
    /// conversation screen calls this when the user scrolls to the
    /// newest messages, focuses or types in the composer, or sends
    /// a reply) and immediately flushes any pending read receipts
    /// for it. Returns how many messages were marked read.
    pub(crate) fn note_read_engagement(&mut self) -> usize {
        let chat_id = self.selected_chat;
        if let Some(session) = &mut self.live {
            session.engaged_chats.insert(chat_id);
        }
        self.mark_selected_chat_read()
    }

    /// Marks the selected live chat's unread incoming messages as
    /// read. The gate is the read-by-action rule: the user must
    /// have performed a real read action in this chat since the
    /// pending messages arrived. Without engagement the pending
    /// protocol ids stay queued and nothing is sent: selection
    /// changes, screen switches and passive arrivals all call
    /// this and all stop at the gate. Returns how many messages
    /// were marked.
    ///
    /// Two things go out on a mark. The app-state patch
    /// (MarkChatRead) always goes: it syncs the read state to
    /// the account's own other devices, so the phone stops
    /// showing the chat as unread; the read-receipts privacy
    /// toggle does not gate it, because the toggle governs ticks
    /// shown to other people, not state shared between the
    /// user's own devices. The per-message receipts (MarkAsRead)
    /// always go too: suppressing them here when the toggle is
    /// off would also suppress the read-self sync the library
    /// sends in their place for direct chats, and group
    /// receipts are never downgraded at all. The library holds
    /// the account's receipts setting and performs the
    /// read-self downgrade for direct chats itself, so the
    /// toggle's only job here is keeping the account setting
    /// current (see [`App::set_read_receipts`]).
    pub(crate) fn mark_selected_chat_read(&mut self) -> usize {
        let chat_id = self.selected_chat;
        let engaged = self.live.as_ref().is_some_and(|s| s.engaged_chats.contains(&chat_id));
        if !self.live_mode || !engaged {
            return 0;
        }
        let Some(session) = &self.live else {
            return 0;
        };
        let Some(jid) = session.jids.get(&chat_id).cloned() else {
            return 0;
        };
        let protocol_ids: Vec<String> = self.unread_live.remove(&chat_id).unwrap_or_default();
        // The engagement is spent by this mark, even when the
        // queue was empty: messages that arrive later need a fresh
        // action before their receipts go out.
        if let Some(session) = &mut self.live {
            session.engaged_chats.remove(&chat_id);
        }
        if protocol_ids.is_empty() {
            return 0;
        }
        let count = protocol_ids.len();
        self.send_backend(BackendCommand::MarkChatRead { chat_jid: jid.clone() });
        if jid.ends_with("@g.us") {
            // A group receipt names the message sender as its
            // participant, and one receipt stanza carries one
            // participant, so the queue goes out grouped by
            // sender: one MarkAsRead per author, in first-seen
            // order. A message whose sender was never recorded
            // groups under no sender, the shape the receipt had
            // before senders were tracked.
            let mut by_sender: Vec<(Option<String>, Vec<String>)> = Vec::new();
            for pid in protocol_ids {
                let sender = self.sender_jids.get(&message_id(&jid, &pid)).cloned();
                match by_sender.iter_mut().find(|(s, _)| *s == sender) {
                    Some((_, ids)) => ids.push(pid),
                    None => by_sender.push((sender, vec![pid])),
                }
            }
            for (sender_jid, message_ids) in by_sender {
                self.send_backend(BackendCommand::MarkAsRead {
                    chat_jid: jid.clone(),
                    sender_jid,
                    message_ids,
                });
            }
        } else {
            self.send_backend(BackendCommand::MarkAsRead {
                chat_jid: jid,
                sender_jid: None,
                message_ids: protocol_ids,
            });
        }
        count
    }
}
