//! Own profile and privacy: the profile save path, privacy
//! state and cycling, member and request row resolution, and
//! read engagement.

use super::*;

impl crate::state::App {
    /// Adopts a privacy state fetched from the account. The map
    /// itself drives the account rows on the Privacy page, and
    /// one value in it is also a local setting: read receipts.
    /// That flag gates whether this client sends read receipts at
    /// all, and it is loaded from the settings file at startup,
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
        }
        self.privacy_state = map;
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
    /// page. The local flag flips at once (it gates receipt
    /// sending, so turning receipts off takes effect immediately),
    /// the choice is persisted, and in live mode the account
    /// setting follows via SetPrivacy. The backend answers a
    /// successful change with a fresh PrivacyState, which
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
    /// read. Two gates apply: the read-receipts toggle must be ON
    /// (the privacy contract), and the user must have performed a
    /// real read action in this chat since the pending messages
    /// arrived (the read-by-action rule). Without engagement the
    /// pending protocol ids stay queued and nothing is sent:
    /// selection changes, screen switches and passive arrivals all
    /// call this and all stop at the gate. Returns how many
    /// messages were marked.
    pub(crate) fn mark_selected_chat_read(&mut self) -> usize {
        let chat_id = self.selected_chat;
        let engaged = self.live.as_ref().is_some_and(|s| s.engaged_chats.contains(&chat_id));
        if !should_send_receipt(self.read_receipts, self.live_mode, engaged) {
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
        self.send_backend(BackendCommand::MarkAsRead {
            chat_jid: jid,
            sender_jid: None,
            message_ids: protocol_ids,
        });
        count
    }
}
