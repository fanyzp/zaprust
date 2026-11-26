//! Media state: photo, sticker, video and voice fetch and
//! playback bookkeeping, avatars and verified badges.

use super::*;

impl crate::state::App {
    /// Whether bundled demo fixtures may stand in as content:
    /// demo mode, and the fake backend's test mode (whose data
    /// is scripted fixtures, labelled test data). In a real live
    /// session they may never: demo material presented as a real
    /// person's data is a fabrication, however small. Every
    /// fixture lookup (demo photo, demo avatars) gates on this
    /// one predicate.
    pub(crate) fn demo_fixtures_allowed(&self) -> bool {
        !self.live_mode || self.live.as_ref().is_some_and(|s| s.fake)
    }

    /// Whether the generated demo photo may stand in for a photo
    /// message's pixels (see [`App::demo_fixtures_allowed`]). In
    /// a real live session a received photo shows its own bytes
    /// or an honest placeholder, never a substitute image.
    pub(crate) fn photo_demo_allowed(&self) -> bool {
        self.demo_fixtures_allowed()
    }

    /// What a photo message's tile may honestly show right now
    /// (see [`crate::media::PhotoTile`]): the decoded real bytes
    /// when they are here, the demo fixture where that is the
    /// disclosed content, otherwise the fetch state.
    pub(crate) fn photo_tile(&self, msg_id: u64) -> crate::media::PhotoTile {
        if self.media.textures.contains_key(&msg_id) {
            return crate::media::PhotoTile::Real;
        }
        if self.photo_demo_allowed() {
            return crate::media::PhotoTile::Demo;
        }
        match self.media.photo_fetch.get(&msg_id) {
            Some(crate::state::PhotoFetchState::Loading) => crate::media::PhotoTile::Loading,
            Some(crate::state::PhotoFetchState::Failed) => crate::media::PhotoTile::Failed,
            None => crate::media::PhotoTile::Idle,
        }
    }

    /// What one sticker message's tile may honestly show,
    /// decided exactly like [`App::photo_tile`]: the sticker's
    /// own decoded bytes, a fetch in flight, a failed fetch, or
    /// not yet requested. There is no demo variant: a sticker is
    /// never substituted with another image.
    pub(crate) fn sticker_tile(&self, msg_id: u64) -> crate::media::PhotoTile {
        if self.media.textures.contains_key(&msg_id) {
            return crate::media::PhotoTile::Real;
        }
        match self.media.sticker_fetch.get(&msg_id) {
            Some(crate::state::PhotoFetchState::Loading) => crate::media::PhotoTile::Loading,
            Some(crate::state::PhotoFetchState::Failed) => crate::media::PhotoTile::Failed,
            None => crate::media::PhotoTile::Idle,
        }
    }

    /// Starts the display fetch for one photo message of the open
    /// chat, unless its bytes are already here or a fetch is
    /// already running. The fetch passes the message's archived
    /// re-download reference along, so a photo the backend no
    /// longer holds in its session cache is still fetchable.
    /// Only real live sessions fetch: demo and test mode paint
    /// their fixture photo instead.
    pub(crate) fn ensure_photo_fetch(&mut self, msg_id: u64) {
        if self.photo_demo_allowed() || self.media.textures.contains_key(&msg_id) {
            return;
        }
        if self.media.photo_fetch.contains_key(&msg_id) {
            return;
        }
        let (chat_jid, photo_ref) = {
            let Some(session) = &self.live else { return };
            let Some(chat_jid) = session.jids.get(&self.selected_chat).cloned() else {
                return;
            };
            (chat_jid, session.photo_refs.get(&msg_id).cloned())
        };
        let Some(protocol_id) = self.protocol_ids.get(&msg_id).cloned() else {
            // A live photo without a protocol id can never be
            // addressed; record the failure instead of spinning.
            self.media.photo_fetch.insert(msg_id, crate::state::PhotoFetchState::Failed);
            return;
        };
        self.media.photo_fetch.insert(msg_id, crate::state::PhotoFetchState::Loading);
        self.send_backend(BackendCommand::FetchPhoto {
            chat_jid,
            message_id: protocol_id,
            photo_ref,
        });
    }

    /// Drops every cached form of one identity's picture
    /// (decoded texture, staged bytes, the store row, the
    /// negative and the in-flight marks), so the next display
    /// refetches through `ensure_avatar`. The one invalidation
    /// path, shared by the wire's picture-change notification
    /// and by our own confirmed picture writes.
    pub(crate) fn drop_avatar(&mut self, jid: &str) {
        self.media.live_avatar_textures.remove(jid);
        if let Some(session) = &mut self.live {
            if let Some(store) = &session.store {
                let _ = store.delete_avatar(jid);
            }
            session.avatar_bytes.remove(jid);
            session.avatar_missing.remove(jid);
            session.avatar_requested.remove(jid);
        }
    }

    /// Ensures one identity's profile picture is on its way or
    /// already here, called by every surface that paints an
    /// avatar. Resolution order: the decoded texture cache, the
    /// store-staged bytes (decoded here, where a graphics
    /// context exists), a fresh-enough negative, and finally a
    /// backend fetch, deduplicated per JID. In demo mode the
    /// demo portraits answer instead and no fetch is sent.
    /// Asks the backend for one identity's verified business
    /// name, once per identity per session: the answer (or a
    /// history payload) is what badges the chat. Groups,
    /// channels and system addresses are never businesses.
    pub(crate) fn ensure_verified_name(&mut self, jid: &str) {
        let Some(session) = &mut self.live else { return };
        if session.fake
            || jid.ends_with("@g.us")
            || jid.ends_with("@newsletter")
            || jid == "status@broadcast"
            || session.verified_names.contains_key(jid)
            || session.verified_checked.contains(jid)
        {
            return;
        }
        session.verified_checked.insert(jid.to_owned());
        let _ = session
            .channels
            .commands
            .send(backend::BackendCommand::FetchVerifiedName { jid: jid.to_owned() });
    }

    /// Brings the materialized chat list's verified flags in
    /// line with the session's verified-name book, so a chat
    /// badges as soon as any source (a history payload, a
    /// usync answer) proves the identity is a verified
    /// business.
    pub(crate) fn apply_verified_badges(&mut self) {
        let Some(session) = &self.live else { return };
        let verified_ids: std::collections::HashSet<u64> = session
            .jids
            .iter()
            .filter(|(_, jid)| session.verified_names.contains_key(*jid))
            .map(|(id, _)| *id)
            .collect();
        if verified_ids.is_empty() {
            return;
        }
        for chat in &mut self.chats {
            if verified_ids.contains(&chat.id) {
                chat.verified = true;
            }
        }
    }

    pub(crate) fn ensure_avatar(&mut self, ctx: &egui::Context, jid: &str) {
        if self.live.is_none() {
            return;
        }
        if self.media.live_avatar_textures.contains_key(jid) {
            return;
        }
        let staged = self.live.as_ref().and_then(|s| s.avatar_bytes.get(jid).cloned());
        if let Some(bytes) = staged {
            if let Some(tex) = crate::media::decode_photo(ctx, &bytes) {
                self.media.live_avatar_textures.insert(jid.to_owned(), tex);
                return;
            }
        }
        let Some(session) = &mut self.live else { return };
        if session.avatar_requested.contains(jid) {
            return;
        }
        let now = i64::try_from(crate::state::now_unix()).unwrap_or(0);
        if let Some(answered_at) = session.avatar_missing.get(jid) {
            // A negative younger than a day stands; an older
            // one is re-asked, because people set pictures.
            if now - answered_at < 86_400 {
                return;
            }
        }
        session.avatar_requested.insert(jid.to_owned());
        // A community parent (it is its own parent in the map)
        // is looked up under the Community target (P2-05).
        let id = jid_id(jid);
        let community = self.chat_parents.get(&id) == Some(&id);
        let _ = session
            .channels
            .commands
            .send(BackendCommand::FetchAvatar { jid: jid.to_owned(), community });
    }

    /// The decoded live profile picture for one identity, when
    /// the avatar pipeline has produced one. Surfaces call
    /// `ensure_avatar` first and paint this through the shared
    /// avatar dispatch; `None` paints the initial disc.
    pub(crate) fn live_avatar(&self, jid: &str) -> Option<&egui::TextureHandle> {
        self.media.live_avatar_textures.get(jid)
    }

    /// The profile photo for one identity addressed by JID (a
    /// group member, a message sender): the real picture through
    /// the avatar pipeline in a live session, the bundled
    /// portrait for the name in demo mode.
    pub(crate) fn member_avatar_photo(
        &mut self,
        ctx: &egui::Context,
        jid: &str,
        label: &str,
    ) -> Option<egui::TextureId> {
        if self.live.is_some() {
            self.ensure_avatar(ctx, jid);
            self.live_avatar(jid).map(|tex| tex.id())
        } else {
            self.demo_avatar(ctx, label)
        }
    }

    /// The avatar photo for one chat, for every surface that
    /// paints a chat's avatar (list row, conversation header,
    /// call card): in a live session, the chat JID's real
    /// profile picture through the avatar pipeline; otherwise
    /// the demo portrait for a DM persona. `None` paints the
    /// initial disc or group glyph.
    pub(crate) fn chat_avatar_photo(
        &mut self,
        ctx: &egui::Context,
        chat: &core::model::Chat,
    ) -> Option<egui::TextureId> {
        if self.live.is_some() {
            let jid = self.live.as_ref().and_then(|s| s.jids.get(&chat.id)).cloned();
            if let Some(jid) = jid {
                self.ensure_avatar(ctx, &jid);
                return self.live_avatar(&jid).map(|tex| tex.id());
            }
            return None;
        }
        if chat.kind == core::model::ChatKind::Dm {
            return self.demo_avatar(ctx, &chat.title);
        }
        None
    }

    /// Ensures one sticker's bytes are on their way, mirroring
    /// [`App::ensure_photo_fetch`] through the sticker fetch
    /// path. A Lottie sticker is never fetched: its bytes are
    /// animation JSON, not an image, and the tile presents the
    /// payload's embedded static frame instead (the stream
    /// decodes it from the session's sticker thumbnails).
    pub(crate) fn ensure_sticker_fetch(&mut self, msg_id: u64) {
        if self.media.textures.contains_key(&msg_id) {
            return;
        }
        if self.media.sticker_fetch.contains_key(&msg_id) {
            return;
        }
        if self
            .live
            .as_ref()
            .and_then(|s| s.sticker_anim.get(&msg_id).copied())
            .is_some_and(|(_, lottie)| lottie)
        {
            return;
        }
        let (chat_jid, sticker_ref) = {
            let Some(session) = &self.live else { return };
            let Some(chat_jid) = session.jids.get(&self.selected_chat).cloned() else {
                return;
            };
            (chat_jid, session.sticker_refs.get(&msg_id).cloned())
        };
        let Some(protocol_id) = self.protocol_ids.get(&msg_id).cloned() else {
            self.media.sticker_fetch.insert(msg_id, crate::state::PhotoFetchState::Failed);
            return;
        };
        self.media.sticker_fetch.insert(msg_id, crate::state::PhotoFetchState::Loading);
        self.send_backend(BackendCommand::FetchSticker {
            chat_jid,
            message_id: protocol_id,
            sticker_ref,
        });
    }

    /// Re-runs a failed display fetch (opening the viewer on a
    /// failed photo is the retry gesture).
    pub(crate) fn retry_photo_fetch(&mut self, msg_id: u64) {
        if self.media.photo_fetch.get(&msg_id) == Some(&crate::state::PhotoFetchState::Failed) {
            self.media.photo_fetch.remove(&msg_id);
        }
        self.ensure_photo_fetch(msg_id);
    }

    /// Re-runs a failed sticker fetch (clicking a failed
    /// sticker tile is the retry gesture, the sticker analogue
    /// of opening the viewer on a failed photo).
    pub(crate) fn retry_sticker_fetch(&mut self, msg_id: u64) {
        if self.media.sticker_fetch.get(&msg_id) == Some(&crate::state::PhotoFetchState::Failed) {
            self.media.sticker_fetch.remove(&msg_id);
        }
        self.ensure_sticker_fetch(msg_id);
    }

    /// Removes one message's voice re-download reference from
    /// the session map and from the archive (the view-once
    /// voice spend).
    pub(super) fn drop_voice_ref(&mut self, msg_id: u64) {
        let protocol_id = self.protocol_ids.get(&msg_id).cloned();
        let Some(session) = &mut self.live else { return };
        session.voice_refs.remove(&msg_id);
        let chat_jid = session
            .messages
            .iter()
            .find(|(_, msgs)| msgs.iter().any(|(_, m)| m.id == msg_id))
            .and_then(|(chat_id, _)| session.jids.get(chat_id).cloned());
        if let (Some(store), Some(chat_jid), Some(protocol_id)) =
            (&session.store, chat_jid, protocol_id)
        {
            let _ = store.clear_voice_ref(&chat_jid, &protocol_id);
        }
    }

    /// Removes one message's sticker re-download reference from
    /// the session map and from the archive (the view-once
    /// sticker spend).
    pub(super) fn drop_sticker_ref(&mut self, msg_id: u64) {
        let protocol_id = self.protocol_ids.get(&msg_id).cloned();
        let Some(session) = &mut self.live else { return };
        session.sticker_refs.remove(&msg_id);
        let chat_jid = session
            .messages
            .iter()
            .find(|(_, msgs)| msgs.iter().any(|(_, m)| m.id == msg_id))
            .and_then(|(chat_id, _)| session.jids.get(chat_id).cloned());
        if let (Some(store), Some(chat_jid), Some(protocol_id)) =
            (&session.store, chat_jid, protocol_id)
        {
            let _ = store.clear_sticker_ref(&chat_jid, &protocol_id);
        }
    }

    /// The play/pause gesture on one voice message. A note already
    /// loaded in the engine pauses or resumes (a finished note
    /// restarts from the top); any other note ends the current
    /// session and begins through [`App::begin_voice`], so what
    /// the user hears is always real decoded audio or a stated
    /// failure, never a moving playhead over silence.
    pub(crate) fn toggle_voice(&mut self, msg_id: u64) {
        let current = self.media.voice_player.as_ref().is_some_and(|p| p.id == msg_id);
        if current {
            let player = self.media.voice_player.as_ref().expect("checked above");
            if player.is_ended() {
                player.seek_fraction(0.0);
                player.resume();
            } else {
                player.toggle_pause();
            }
            return;
        }
        self.media.voice_player = None;
        self.begin_voice(msg_id, None);
    }

    /// The seek gesture on one voice message's waveform. A loaded
    /// note seeks in the engine; any other note begins at the
    /// sought position (the seek is parked and applied when its
    /// player is created, if its bytes still have to be fetched).
    pub(crate) fn seek_voice(&mut self, msg_id: u64, fraction: f32) {
        let current = self.media.voice_player.as_ref().is_some_and(|p| p.id == msg_id);
        if current {
            self.media
                .voice_player
                .as_ref()
                .expect("checked above")
                .seek_fraction(f64::from(fraction));
            return;
        }
        self.media.voice_player = None;
        self.begin_voice(msg_id, Some(fraction));
    }

    /// Begins hearing one voice note: from the decoded PCM still
    /// held for it when there is some, otherwise by fetching its
    /// bytes. Demo mode has no session and its notes are scripted
    /// fixtures with no audio behind them; that is said plainly
    /// instead of animating a playhead over nothing.
    pub(super) fn begin_voice(&mut self, msg_id: u64, seek: Option<f32>) {
        self.media.voice_wanted = Some(msg_id);
        if let Some((held, pcm)) = &self.media.voice_pcm {
            if *held == msg_id {
                let pcm = std::sync::Arc::clone(pcm);
                self.start_voice_player(msg_id, pcm, seek);
                return;
            }
        }
        if self.live.is_none() {
            self.media.voice_fetch.insert(msg_id, crate::state::VoiceFetchState::Failed);
            self.notice = Some(Notice::info(crate::i18n::t("voice.demo_unavailable")));
            return;
        }
        if self.media.voice_fetch.get(&msg_id) == Some(&crate::state::VoiceFetchState::Loading) {
            // A fetch for this note is already running; only the
            // parked seek can still change.
            if let Some(fraction) = seek {
                self.media.voice_pending_seek.insert(msg_id, fraction);
            }
            return;
        }
        let (chat_jid, voice_ref) = {
            let Some(session) = &self.live else { return };
            let Some(chat_jid) = session.jids.get(&self.selected_chat).cloned() else {
                return;
            };
            (chat_jid, session.voice_refs.get(&msg_id).cloned())
        };
        let Some(protocol_id) = self.protocol_ids.get(&msg_id).cloned() else {
            // A live voice note without a protocol id can never be
            // addressed; record the failure instead of spinning.
            self.media.voice_fetch.insert(msg_id, crate::state::VoiceFetchState::Failed);
            return;
        };
        self.media.voice_fetch.insert(msg_id, crate::state::VoiceFetchState::Loading);
        if let Some(fraction) = seek {
            self.media.voice_pending_seek.insert(msg_id, fraction);
        }
        self.send_backend(BackendCommand::FetchVoice {
            chat_jid,
            message_id: protocol_id,
            voice_ref,
        });
    }

    /// Creates the playback engine session for one note whose PCM
    /// is here. A player that will not start (no output device,
    /// a refused stream) fails the note honestly and says why.
    pub(super) fn start_voice_player(
        &mut self,
        msg_id: u64,
        pcm: std::sync::Arc<[i16]>,
        seek: Option<f32>,
    ) {
        let rate = f64::from(self.voice_speed(msg_id));
        match crate::player::VoicePlayer::start(msg_id, pcm, rate, self.media.voice_sink) {
            Ok(player) => {
                if let Some(fraction) = seek {
                    player.seek_fraction(f64::from(fraction));
                }
                self.media.voice_fetch.remove(&msg_id);
                self.media.voice_player = Some(player);
                // A view-once voice note's single play is spent
                // by playback actually starting on its real
                // decoded audio: the opened state is recorded
                // and the archived reference dropped, so the
                // note cannot be fetched again after a restart.
                // The loaded player remains usable until another
                // note replaces it; that session is the viewing.
                if self.live.as_ref().is_some_and(|s| s.view_once_ids.contains(&msg_id)) {
                    self.mark_view_once_opened(msg_id);
                    self.drop_voice_ref(msg_id);
                }
            }
            Err(message) => {
                self.media.voice_fetch.insert(msg_id, crate::state::VoiceFetchState::Failed);
                self.notice = Some(Notice::error(message));
            }
        }
    }

    /// Closes the media viewer on one message (the viewer is
    /// shared by photos and videos). A view-once photo consumes
    /// its single view here, and only under the honesty rule: in
    /// a real live session the view is spent solely when the
    /// photo's real pixels were actually painted during this
    /// viewing (tracked in `MediaState::view_once_shown`). When
    /// they never arrived, the allowance survives, the failure is
    /// said out loud, and the photo can be opened again later.
    /// Demo and test mode keep the standing behaviour (their
    /// fixture photo is the content, so closing spends the view).
    /// A spent live view also drops the decoded pixels and the
    /// archived re-download reference, so nothing can bring the
    /// photo back afterwards. A view-once video follows the same
    /// rule against its own presentation record
    /// (`MediaState::video_presented`).
    pub(crate) fn close_photo_viewer(&mut self, id: u64, view_once: bool) {
        // The video player belongs to this viewing; closing the
        // viewer stops it (dropping the player stops its worker
        // thread).
        if self.media.video_player.as_ref().is_some_and(|p| p.key == id) {
            self.media.video_player = None;
        }
        if view_once {
            // A view-once video follows the same honesty rule as
            // a view-once photo, gated on its own presentation
            // record: the view is spent only when the video's
            // real content (its thumbnail or a decoded frame)
            // was actually presented during this viewing.
            let is_video = self
                .conversation
                .iter()
                .any(|m| m.id == id && matches!(m.kind, core::model::MessageKind::Video { .. }));
            if is_video {
                let spent = self.photo_demo_allowed() || self.media.video_presented == Some(id);
                if spent {
                    self.mark_view_once_opened(id);
                    if !self.photo_demo_allowed() {
                        self.media.video_bytes.remove(&id);
                        self.media.video_textures.remove(&id);
                        self.drop_video_ref(id);
                    }
                } else {
                    self.notice = Some(Notice::info(crate::i18n::t("video.view_once_not_loaded")));
                }
            } else {
                let spent = self.photo_demo_allowed() || self.media.view_once_shown == Some(id);
                if spent {
                    self.mark_view_once_opened(id);
                    if !self.photo_demo_allowed() {
                        self.media.textures.remove(&id);
                        self.drop_photo_ref(id);
                    }
                } else {
                    self.notice = Some(Notice::info(crate::i18n::t("photo.view_once_not_loaded")));
                }
            }
        }
        if self.media.view_once_shown == Some(id) {
            self.media.view_once_shown = None;
        }
        if self.media.video_presented == Some(id) {
            self.media.video_presented = None;
        }
        self.media.viewing_photo = None;
    }

    /// Removes one message's photo re-download reference from the
    /// session map and from the archive.
    pub(super) fn drop_photo_ref(&mut self, msg_id: u64) {
        let protocol_id = self.protocol_ids.get(&msg_id).cloned();
        let Some(session) = &mut self.live else { return };
        session.photo_refs.remove(&msg_id);
        let chat_jid = session
            .messages
            .iter()
            .find(|(_, msgs)| msgs.iter().any(|(_, m)| m.id == msg_id))
            .and_then(|(chat_id, _)| session.jids.get(chat_id).cloned());
        if let (Some(store), Some(chat_jid), Some(protocol_id)) =
            (&session.store, chat_jid, protocol_id)
        {
            let _ = store.clear_photo_ref(&chat_jid, &protocol_id);
        }
    }

    /// What a video message's tile may honestly show right now
    /// (see [`crate::media::VideoTile`]): the video's own
    /// embedded thumbnail when those bytes are here, otherwise
    /// the neutral tile. Unlike photos there is no demo variant:
    /// a video never paints a generated picture, in any mode.
    pub(crate) fn video_tile(&self, msg_id: u64) -> crate::media::VideoTile {
        if self.media.video_textures.contains_key(&msg_id)
            || self.live.as_ref().is_some_and(|s| s.video_thumbs.contains_key(&msg_id))
        {
            crate::media::VideoTile::Thumb
        } else {
            crate::media::VideoTile::Neutral
        }
    }

    /// The decoded texture for one video's thumbnail, with its
    /// aspect ratio (width over height), decoding and caching
    /// the wire bytes on first use. The poster's own dimensions
    /// are the frame's, so the aspect sizes the tile exactly
    /// like a photo's. `None` when the video carries no
    /// thumbnail bytes (or they do not decode): the neutral
    /// tile paints instead. An associated function over the two
    /// state fields it touches, so paint code that already
    /// borrows other parts of the app can still call it.
    pub(crate) fn video_thumb_texture(
        media: &mut crate::state::MediaState,
        live: Option<&LiveSession>,
        ctx: &egui::Context,
        msg_id: u64,
    ) -> Option<(egui::TextureId, f32)> {
        if let Some(handle) = media.video_textures.get(&msg_id) {
            let size = handle.size_vec2();
            return Some((handle.id(), size.x / size.y.max(1.0)));
        }
        let bytes = live?.video_thumbs.get(&msg_id)?.clone();
        let handle = crate::media::decode_photo(ctx, &bytes)?;
        let size = handle.size_vec2();
        let id = handle.id();
        media.video_textures.insert(msg_id, handle);
        Some((id, size.x / size.y.max(1.0)))
    }

    /// The decoded texture of one photo's wire thumbnail, with
    /// its aspect ratio (width over height): the photo itself
    /// at preview fidelity, decoded once into the thumbnail
    /// cache. Mirrors [`App::video_thumb_texture`].
    pub(crate) fn photo_thumb_texture(
        media: &mut crate::state::MediaState,
        live: Option<&LiveSession>,
        ctx: &egui::Context,
        msg_id: u64,
    ) -> Option<(egui::TextureId, f32)> {
        if let Some(handle) = media.photo_thumb_textures.get(&msg_id) {
            let size = handle.size_vec2();
            return Some((handle.id(), size.x / size.y.max(1.0)));
        }
        let bytes = live?.photo_thumbs.get(&msg_id)?.clone();
        let handle = crate::media::decode_photo(ctx, &bytes)?;
        let size = handle.size_vec2();
        let id = handle.id();
        media.photo_thumb_textures.insert(msg_id, handle);
        Some((id, size.x / size.y.max(1.0)))
    }

    /// The decoded texture of one message's embedded thumbnail
    /// (a location's map snapshot, a document's preview image,
    /// a link preview's picture), with its aspect ratio,
    /// decoding and caching the wire bytes on first use into
    /// the shared embedded cache. `bytes` is the payload's own
    /// image as the caller read it from the session's maps;
    /// `None` (or undecodable bytes) paints the kind's neutral
    /// card instead. Mirrors [`App::video_thumb_texture`].
    pub(crate) fn embedded_thumb_texture(
        media: &mut crate::state::MediaState,
        ctx: &egui::Context,
        msg_id: u64,
        bytes: Option<&[u8]>,
    ) -> Option<(egui::TextureId, f32)> {
        if let Some(handle) = media.embedded_textures.get(&msg_id) {
            let size = handle.size_vec2();
            return Some((handle.id(), size.x / size.y.max(1.0)));
        }
        let handle = crate::media::decode_photo(ctx, bytes?)?;
        let size = handle.size_vec2();
        let id = handle.id();
        media.embedded_textures.insert(msg_id, handle);
        Some((id, size.x / size.y.max(1.0)))
    }

    /// Where pressing play on one video message stands (see
    /// [`crate::media::VideoPlayRoute`]). One decision function
    /// for the bubble and the viewer, so the play gesture can
    /// never drift from the fetch state it depends on.
    pub(crate) fn video_play_route(&self, msg_id: u64) -> crate::media::VideoPlayRoute {
        if !self.live_mode {
            return crate::media::VideoPlayRoute::DemoUnavailable;
        }
        if self.media.video_files.contains_key(&msg_id)
            || self.media.video_bytes.contains_key(&msg_id)
        {
            return crate::media::VideoPlayRoute::Ready;
        }
        match self.media.video_fetch.get(&msg_id) {
            Some(crate::state::PhotoFetchState::Loading) => crate::media::VideoPlayRoute::Loading,
            Some(crate::state::PhotoFetchState::Failed) => crate::media::VideoPlayRoute::Failed,
            None => crate::media::VideoPlayRoute::Idle,
        }
    }

    /// Starts the byte fetch for one video message of the open
    /// chat, unless its bytes are already here or a fetch is
    /// already running. The fetch passes the message's archived
    /// re-download reference along, so a video the backend no
    /// longer holds in its session cache is still fetchable.
    /// Only live sessions fetch; demo mode's scripted videos
    /// have no bytes to fetch (see `video_play_route`).
    pub(crate) fn ensure_video_fetch(&mut self, msg_id: u64, view_once: bool) {
        if !self.live_mode
            || self.media.video_files.contains_key(&msg_id)
            || self.media.video_bytes.contains_key(&msg_id)
            || self.media.video_fetch.contains_key(&msg_id)
        {
            return;
        }
        let (chat_jid, video_ref) = {
            let Some(session) = &self.live else { return };
            let Some(chat_jid) = session.jids.get(&self.selected_chat).cloned() else {
                return;
            };
            (chat_jid, session.video_refs.get(&msg_id).cloned())
        };
        let Some(protocol_id) = self.protocol_ids.get(&msg_id).cloned() else {
            // A live video without a protocol id can never be
            // addressed; record the failure instead of spinning.
            self.media.video_fetch.insert(msg_id, crate::state::PhotoFetchState::Failed);
            return;
        };
        self.media.video_fetch.insert(msg_id, crate::state::PhotoFetchState::Loading);
        self.send_backend(BackendCommand::FetchVideo {
            chat_jid,
            message_id: protocol_id,
            video_ref,
            view_once,
        });
    }

    /// Starts the poster fetch for one video message of the
    /// open chat whose payload carried no embedded thumbnail,
    /// unless the poster bytes are already here (as the
    /// message's thumbnail) or a fetch is already running or
    /// has failed. The fetch passes the message's archived
    /// poster reference along, so a video the backend no longer
    /// holds in its session cache still gets its frame. Only
    /// real live sessions fetch: demo and test mode have no
    /// poster bytes to fetch, the same rule the photo fetch
    /// follows.
    pub(crate) fn ensure_poster_fetch(&mut self, msg_id: u64) {
        if self.photo_demo_allowed()
            || self.media.video_textures.contains_key(&msg_id)
            || self.media.poster_fetch.contains_key(&msg_id)
        {
            return;
        }
        let (chat_jid, poster_ref) = {
            let Some(session) = &self.live else { return };
            if session.video_thumbs.contains_key(&msg_id) {
                return;
            }
            let Some(chat_jid) = session.jids.get(&self.selected_chat).cloned() else {
                return;
            };
            (chat_jid, session.poster_refs.get(&msg_id).cloned())
        };
        let Some(protocol_id) = self.protocol_ids.get(&msg_id).cloned() else {
            // A live video without a protocol id can never be
            // addressed; record the failure instead of spinning.
            self.media.poster_fetch.insert(msg_id, crate::state::PhotoFetchState::Failed);
            return;
        };
        self.media.poster_fetch.insert(msg_id, crate::state::PhotoFetchState::Loading);
        self.send_backend(BackendCommand::FetchVideoPoster {
            chat_jid,
            message_id: protocol_id,
            poster_ref,
        });
    }

    /// Re-runs a failed video byte fetch (the viewer's retry
    /// gesture).
    pub(crate) fn retry_video_fetch(&mut self, msg_id: u64, view_once: bool) {
        if self.media.video_fetch.get(&msg_id) == Some(&crate::state::PhotoFetchState::Failed) {
            self.media.video_fetch.remove(&msg_id);
        }
        self.ensure_video_fetch(msg_id, view_once);
    }

    /// Creates the video player for one message whose bytes are
    /// here: a regular video plays from its cache file, a
    /// view-once video from memory. A player for a different
    /// message is replaced (dropping it stops its worker).
    /// `seconds` is the message's stated duration; the player
    /// corrects it from the stream itself once demuxed.
    pub(crate) fn start_video_player(&mut self, msg_id: u64, seconds: u32) {
        if self.media.video_player.as_ref().is_some_and(|p| p.key == msg_id) {
            return;
        }
        let source = if let Some(path) = self.media.video_files.get(&msg_id) {
            crate::video::VideoSource::File(std::path::PathBuf::from(path))
        } else if let Some(bytes) = self.media.video_bytes.get(&msg_id) {
            crate::video::VideoSource::Bytes(bytes.clone())
        } else {
            return;
        };
        let info =
            crate::video::VideoInfo { width: 0, height: 0, duration_secs: f64::from(seconds) };
        let mut player = crate::video::VideoPlayer::start(msg_id, source, info);
        // A GIF plays on sight and loops (the viewer's pump
        // restarts it at the end): it is not a video the user
        // transports, it is a moving picture.
        if self.live.as_ref().is_some_and(|s| s.gif_videos.contains(&msg_id)) {
            player.play();
        }
        self.media.video_player = Some(player);
    }

    /// Removes one message's video re-download reference,
    /// thumbnail and poster reference from the session maps and
    /// from the archive.
    pub(super) fn drop_video_ref(&mut self, msg_id: u64) {
        let protocol_id = self.protocol_ids.get(&msg_id).cloned();
        let Some(session) = &mut self.live else { return };
        session.video_refs.remove(&msg_id);
        session.video_thumbs.remove(&msg_id);
        session.poster_refs.remove(&msg_id);
        let chat_jid = session
            .messages
            .iter()
            .find(|(_, msgs)| msgs.iter().any(|(_, m)| m.id == msg_id))
            .and_then(|(chat_id, _)| session.jids.get(chat_id).cloned());
        if let (Some(store), Some(chat_jid), Some(protocol_id)) =
            (&session.store, chat_jid, protocol_id)
        {
            let _ = store.clear_video_ref(&chat_jid, &protocol_id);
        }
    }
}
