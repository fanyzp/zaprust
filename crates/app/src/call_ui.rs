//! Call overlay: the incoming-call prompt and the in-call
//! controls, driven by backend call events. One call at a time,
//! matching the backend's contract.
//!
//! The card carries the whole call surface: 1:1 and group
//! calls, video (the remote stage, the local preview, the
//! camera toggle), call-link calls and their waiting room,
//! in-call reactions and hand raising. Every control on it
//! performs a real action against the call's current state;
//! a control the state cannot perform is not drawn (design
//! standard section 19).

use backend::BackendCommand;
use eframe::egui::{self, Align2, Area, Id, Margin, RichText, Sense, Vec2};

use crate::icons::{self, Icon};
use crate::state::{App, Notice};
use crate::theme::{rgb, SP_2, SP_3, SP_SECTION};

/// Phases the overlay renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallUiPhase {
    /// Ringing here, waiting for accept/reject.
    Incoming,
    /// Outgoing, ringing at the peer.
    Ringing,
    /// Accepted, media path being set up.
    Connecting,
    /// Media flowing.
    Active,
    /// Over; shows the reason and dismisses itself.
    Ended,
}

/// A call link's facts as its preview reported them, held by
/// the join dialog.
#[derive(Debug, Clone)]
pub(crate) struct LinkPreviewUi {
    /// The link's canonical URL.
    pub url: String,
    /// JID of the account that created the link.
    pub creator_jid: String,
    /// True for a video link.
    pub video: bool,
    /// Joining waits for the host to admit this device.
    pub waiting_room: bool,
}

/// The call's waiting room as the backend last reported it.
#[derive(Debug, Clone)]
pub(crate) struct WaitingRoomUi {
    /// This device hosts the call and may admit or deny.
    pub is_admin: bool,
    /// Joining currently requires the host's approval.
    pub approval_required: bool,
    /// The people waiting.
    pub users: Vec<backend::CallWaitingUser>,
}

/// UI state for the one live call.
#[derive(Debug, Clone)]
pub(crate) struct CallUi {
    pub call_id: String,
    pub chat_id: u64,
    pub title: String,
    pub video: bool,
    pub phase: CallUiPhase,
    pub seconds: u32,
    pub muted: bool,
    /// The (muted, speaker) pair from before the latest
    /// optimistic toggle flip, kept until the backend's
    /// CallAudioState confirmation lands; a failure in that
    /// window restores this pair (P2-33).
    pub audio_backup: Option<(bool, bool)>,
    pub speaker: bool,
    pub reason: Option<String>,
    pub ended_at: Option<std::time::Instant>,
    /// True when this device placed the call. Part of the
    /// history record the call leaves behind (see
    /// [`CallUi::history_record`]).
    pub outgoing: bool,
    /// Unix second the call started (the offer arrived, or the
    /// outgoing ring was confirmed): the history entry's
    /// timestamp.
    pub started_at: i64,
    /// The call reached Active at some point: it connected, so
    /// its history entry carries its talk duration and is never
    /// a missed or unanswered one.
    pub connected: bool,
    /// The user declined this incoming call. Declining is an
    /// answer, so the history entry is a normal incoming call
    /// with no duration, not a missed call.
    pub declined: bool,
    /// Another of the account's devices answered or declined
    /// this call while it rang here. That is an answer too, so
    /// like a local decline the history entry is a normal
    /// incoming call with no duration, not a missed call: the
    /// call did not go unanswered, it was picked up elsewhere.
    pub ended_elsewhere: bool,
    /// An accept/reject/end command was sent and not yet confirmed
    /// by a phase change (or an error). While set, the action
    /// buttons ignore further clicks: a double-click used to send a
    /// second command the backend correctly rejected, surfacing a
    /// scary error during a healthy call (logic review F18).
    pub action_pending: bool,
    /// A group or call-link call: the roster strip and the
    /// group controls (hand, add people) apply.
    pub group: bool,
    /// A call-link call: no chat stands behind it, so it
    /// leaves no chat-bound history record.
    pub link: bool,
    /// The roster, as the backend's snapshots report it.
    pub participants: Vec<backend::CallParticipant>,
    /// Current reactions by participant JID: transient display
    /// state the roster snapshots do not carry.
    pub reactions: std::collections::HashMap<String, String>,
    /// My own current reaction, so its button reads as chosen
    /// and clicking it again clears it.
    pub my_reaction: Option<String>,
    /// The waiting room, when the backend reports one for
    /// this call.
    pub waiting: Option<WaitingRoomUi>,
    /// The waiting user whose admit/deny is in flight.
    pub waiting_pending: Option<String>,
    /// My hand is raised.
    pub hand_raised: bool,
    /// The (video_local, hand_raised) pair from before the
    /// latest optimistic control flip, restored when the
    /// backend answers with CallControlFailed. The video half
    /// is settled by the next CallVideoState, the hand half by
    /// the next roster snapshot: both ride the call's ordered
    /// event stream behind the control they confirm.
    pub control_backup: Option<(bool, bool)>,
    /// My camera is sending, as CallVideoState last confirmed.
    pub video_local: bool,
    /// The other side's video is on, as CallVideoState last
    /// confirmed.
    pub video_remote: bool,
    /// Why local video cannot run, when the backend said so
    /// (no camera on this machine, the camera would not
    /// open): shown on the card under the status line.
    pub video_note: Option<String>,
    /// JID of the participant whose frames the stage is
    /// showing, in a group call; None on a 1:1 call.
    pub stage_sender: Option<String>,
}

/// How a call sits in the world: who started it, and
/// whether a group or a link stands behind it. Bundled so
/// [`CallUi::fresh`] stays readable.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CallShape {
    /// This device placed the call.
    pub outgoing: bool,
    /// A group or call-link call.
    pub group: bool,
    /// A call-link call (no chat behind it).
    pub link: bool,
}

impl CallUi {
    /// A call in its first phase, with every control at rest.
    fn fresh(
        call_id: &str,
        chat_id: u64,
        title: String,
        video: bool,
        phase: CallUiPhase,
        shape: CallShape,
    ) -> Self {
        Self {
            call_id: call_id.to_owned(),
            chat_id,
            title,
            video,
            phase,
            seconds: 0,
            muted: false,
            audio_backup: None,
            speaker: false,
            reason: None,
            ended_at: None,
            outgoing: shape.outgoing,
            started_at: crate::state::now_unix() as i64,
            connected: false,
            declined: false,
            ended_elsewhere: false,
            action_pending: false,
            group: shape.group,
            participants: Vec::new(),
            reactions: std::collections::HashMap::new(),
            my_reaction: None,
            waiting: None,
            waiting_pending: None,
            hand_raised: false,
            control_backup: None,
            video_local: false,
            video_remote: false,
            video_note: None,
            stage_sender: None,
            link: shape.link,
        }
    }

    /// The call-history record for this call, built when its
    /// Ended event lands. `chat_jid` is the peer's JID as the
    /// event carried it; `seconds` is the backend's final talk
    /// duration (kept only for a call that connected: a call that
    /// never connected has no talk time to record).
    pub(crate) fn history_record(&self, chat_jid: &str, seconds: u32) -> backend::CallRecord {
        backend::CallRecord {
            id: 0,
            call_id: self.call_id.clone(),
            chat_jid: chat_jid.to_owned(),
            video: self.video,
            outgoing: self.outgoing,
            missed: !self.outgoing && !self.connected && !self.declined && !self.ended_elsewhere,
            duration_secs: if self.connected { seconds } else { 0 },
            started_at: self.started_at,
        }
    }

    fn status_line(&self) -> String {
        match self.phase {
            CallUiPhase::Incoming => {
                if self.video {
                    crate::i18n::t("call.incoming_video")
                } else if self.group {
                    crate::i18n::t("call.incoming_group")
                } else {
                    crate::i18n::t("call.incoming_voice")
                }
            }
            CallUiPhase::Ringing => crate::i18n::t("call.ringing"),
            CallUiPhase::Connecting => {
                if self.link {
                    crate::i18n::t("call.waiting_for_host")
                } else {
                    crate::i18n::t("connect.status_connecting")
                }
            }
            CallUiPhase::Active => format_duration(self.seconds),
            CallUiPhase::Ended => {
                self.reason.clone().unwrap_or_else(|| crate::i18n::t("call.ended"))
            }
        }
    }
}

/// m:ss, or h:mm:ss past an hour.
fn format_duration(seconds: u32) -> String {
    let (h, m, s) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// A raw Annex-B H.264 decoder for the call stage: the chat
/// video player's decode session is shaped around demuxed
/// files, so the stage keeps this small one of its own over
/// the same vendored OpenH264.
pub(crate) struct CallDecoder {
    decoder: openh264::decoder::Decoder,
}

impl CallDecoder {
    /// Opens a decoder, when the codec starts on this device.
    pub(crate) fn new() -> Option<Self> {
        openh264::decoder::Decoder::with_api_config(
            openh264::OpenH264API::from_source(),
            openh264::decoder::DecoderConfig::new(),
        )
        .ok()
        .map(|decoder| Self { decoder })
    }

    /// Feeds one access unit; returns the frame it produced as
    /// (RGBA pixels, width, height), when it produced one. A
    /// unit the decoder cannot use yet (a stream's first
    /// non-keyframe units, a damaged unit) produces nothing:
    /// the stage keeps its last good frame and the next
    /// keyframe resyncs.
    pub(crate) fn decode(&mut self, au: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        use openh264::formats::YUVSource as _;
        match self.decoder.decode(au) {
            Ok(Some(yuv)) => {
                let (width, height) = yuv.dimensions();
                let mut rgba = vec![0u8; width * height * 4];
                yuv.write_rgba8(&mut rgba);
                Some((rgba, width as u32, height as u32))
            }
            _ => None,
        }
    }
}

/// Decoded call video held outside the (cloned-per-draw)
/// [`CallUi`]: the decoder session and the uploaded textures
/// for the stage and the local preview.
#[derive(Default)]
pub(crate) struct CallMediaState {
    /// The stage's decoder session.
    pub decoder: Option<CallDecoder>,
    /// The participant the current decoder session belongs
    /// to: a sender change restarts the session, because the
    /// new sender's stream starts from its own config.
    pub decoder_sender: Option<String>,
    /// The stage's current frame texture and its pixel size.
    pub stage_texture: Option<egui::TextureHandle>,
    /// The stage frame's pixel size.
    pub stage_size: (u32, u32),
    /// The local preview's current texture and pixel size.
    pub preview_texture: Option<egui::TextureHandle>,
    /// The preview frame's pixel size.
    pub preview_size: (u32, u32),
}

impl App {
    /// The header phone button: starts a call to the open
    /// chat (live and test mode only). A group chat's button
    /// starts a group call: the call belongs to the group.
    pub(crate) fn start_call_from_header(&mut self) {
        let group = self
            .chats
            .iter()
            .find(|c| c.id == self.selected_chat)
            .is_some_and(|c| c.kind == core::model::ChatKind::Group);
        self.start_call_inner(false, group);
    }

    /// Starts a 1:1 call of the given kind to the open chat.
    /// The one start path every 1:1 entry point shares: the
    /// conversation header, the call log's call-back action and
    /// the new call picker's per-row voice and video actions
    /// all land here, so the kind the user picked is the kind
    /// the backend is asked for.
    pub(crate) fn start_call_kind(&mut self, video: bool) {
        self.start_call_inner(video, false);
    }

    /// Starts a group call of the given kind to the open
    /// group chat.
    pub(crate) fn start_group_call_kind(&mut self, video: bool) {
        self.start_call_inner(video, true);
    }

    /// The shared start path (live and test mode only).
    fn start_call_inner(&mut self, video: bool, group: bool) {
        if !self.live_mode {
            self.notice = Some(Notice::info(
                "Calls work in live mode and test mode, not in the offline demo.".to_owned(),
            ));
            return;
        }
        if self.calls.active.is_some() {
            self.notice = Some(Notice::info("A call is already in progress.".to_owned()));
            return;
        }
        match self.live_jid(self.selected_chat) {
            Some(jid) => {
                self.calls.outgoing_video = video;
                self.send_backend(if group {
                    BackendCommand::StartGroupCall { chat_jid: jid, video }
                } else {
                    BackendCommand::StartCall { chat_jid: jid, video }
                });
            }
            None => {
                self.notice = Some(Notice::info("Open a real chat to start a call.".to_owned()));
            }
        }
    }

    /// Joins a call through a call link: the overlay opens in
    /// Connecting from the backend's first phase event, which
    /// the outgoing-link flag lets adoption recognize.
    pub(crate) fn join_call_link(&mut self, url: &str, video: bool) {
        if !self.live_mode {
            self.notice = Some(Notice::info(
                "Calls work in live mode and test mode, not in the offline demo.".to_owned(),
            ));
            return;
        }
        if self.calls.active.is_some() {
            self.notice = Some(Notice::info("A call is already in progress.".to_owned()));
            return;
        }
        self.calls.outgoing_link = true;
        self.calls.outgoing_video = video;
        self.send_backend(BackendCommand::JoinCallLink { link: url.to_owned(), video });
    }

    /// Backend reports a ringing incoming call.
    pub(crate) fn on_incoming_call(
        &mut self,
        call_id: &str,
        chat_id: u64,
        video: bool,
        group: bool,
    ) {
        if self.calls.active.is_some() {
            return;
        }
        let title = self
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .map_or_else(|| crate::i18n::t("call.unknown_caller"), |c| c.title.clone());
        self.calls.media = CallMediaState::default();
        self.calls.active = Some(CallUi::fresh(
            call_id,
            chat_id,
            title,
            video,
            CallUiPhase::Incoming,
            CallShape { outgoing: false, group, link: false },
        ));
    }

    /// The overlay's Accept action: answers the ringing call.
    /// Latched by `action_pending`, so a double click sends one
    /// command (see the field's note).
    pub(crate) fn accept_active_call(&mut self) {
        if let Some(call) = &mut self.calls.active {
            if !call.action_pending {
                call.action_pending = true;
                self.send_backend(BackendCommand::AcceptCall);
            }
        }
    }

    /// The overlay's Decline action: rejects the ringing call
    /// and marks it declined, so the history entry it leaves is
    /// a declined incoming call rather than a missed one.
    pub(crate) fn reject_active_call(&mut self) {
        if let Some(call) = &mut self.calls.active {
            if !call.action_pending {
                call.action_pending = true;
                call.declined = true;
                self.send_backend(BackendCommand::RejectCall);
            }
        }
    }

    /// The overlay's End call action: ends the current call,
    /// ringing or active.
    pub(crate) fn end_active_call(&mut self) {
        if let Some(call) = &mut self.calls.active {
            if !call.action_pending {
                call.action_pending = true;
                self.send_backend(BackendCommand::EndCall);
            }
        }
    }

    /// Backend reports a call phase change.
    pub(crate) fn on_call_changed(
        &mut self,
        call_id: &str,
        chat_id: u64,
        phase: backend::CallPhase,
        seconds: u32,
        reason: Option<String>,
    ) {
        use backend::CallPhase;
        // Adopt an outgoing call the backend just confirmed: the
        // UI state is created from its first Ringing event.
        if self.calls.active.is_none() && phase == CallPhase::Ringing {
            let chat = self.chats.iter().find(|c| c.id == chat_id).cloned();
            let title =
                chat.as_ref().map_or_else(|| crate::i18n::t("call.unknown"), |c| c.title.clone());
            let group = chat.is_some_and(|c| c.kind == core::model::ChatKind::Group);
            self.calls.media = CallMediaState::default();
            self.calls.active = Some(CallUi::fresh(
                call_id,
                chat_id,
                title,
                self.calls.outgoing_video,
                CallUiPhase::Ringing,
                CallShape { outgoing: true, group, link: false },
            ));
            // The remembered kind has been adopted; the next
            // outgoing call sets it again when it starts.
            self.calls.outgoing_video = false;
            return;
        }
        // Adopt an outgoing call-link join: its first phase
        // event is Connecting with no chat behind it, and the
        // join action's flag is what identifies it.
        if self.calls.active.is_none() && phase == CallPhase::Connecting && self.calls.outgoing_link
        {
            self.calls.outgoing_link = false;
            self.calls.media = CallMediaState::default();
            self.calls.active = Some(CallUi::fresh(
                call_id,
                0,
                crate::i18n::t("call.link_title"),
                self.calls.outgoing_video,
                CallUiPhase::Connecting,
                CallShape { outgoing: true, group: true, link: true },
            ));
            self.calls.outgoing_video = false;
            return;
        }
        let Some(call) = &mut self.calls.active else {
            return;
        };
        // Any phase report confirms the pending action (F18).
        call.action_pending = false;
        if !call.call_id.is_empty() && call.call_id != call_id {
            return;
        }
        if call.chat_id != chat_id && call.phase != CallUiPhase::Incoming && !call.link {
            // An event for a different chat than the live call's:
            // not ours to render.
            return;
        }
        call.call_id = call_id.to_owned();
        call.seconds = seconds;
        match phase {
            CallPhase::Ringing => {
                if call.phase == CallUiPhase::Incoming {
                    // Our own accept is not a phase change.
                } else {
                    call.phase = CallUiPhase::Ringing;
                }
            }
            CallPhase::Connecting => call.phase = CallUiPhase::Connecting,
            CallPhase::Active => {
                call.phase = CallUiPhase::Active;
                call.connected = true;
            }
            CallPhase::Ended => {
                call.phase = CallUiPhase::Ended;
                call.reason = reason;
                if call.ended_at.is_none() {
                    call.ended_at = Some(std::time::Instant::now());
                }
            }
        }
    }

    /// Backend reports the call's roster snapshot: replace,
    /// never merge. A snapshot also settles an optimistic hand
    /// flip (see [`CallUi::control_backup`]).
    pub(crate) fn on_call_participants(&mut self, participants: Vec<backend::CallParticipant>) {
        if let Some(call) = &mut self.calls.active {
            call.participants = participants;
            if let Some((video_local, _)) = call.control_backup {
                call.control_backup = Some((video_local, call.hand_raised));
            }
        }
    }

    /// Backend reports a participant's reaction change.
    pub(crate) fn on_call_reaction(&mut self, jid: &str, emoji: Option<String>) {
        if let Some(call) = &mut self.calls.active {
            match emoji {
                Some(emoji) => {
                    call.reactions.insert(jid.to_owned(), emoji);
                }
                None => {
                    call.reactions.remove(jid);
                }
            }
        }
    }

    /// Backend reports the call's waiting room.
    pub(crate) fn on_call_waiting_room(&mut self, waiting: WaitingRoomUi) {
        if let Some(call) = &mut self.calls.active {
            call.waiting = Some(waiting);
            call.waiting_pending = None;
        }
    }

    /// Backend confirms the call's video state.
    pub(crate) fn on_call_video_state(
        &mut self,
        local_on: bool,
        remote_on: bool,
        local_note: Option<String>,
    ) {
        if let Some(call) = &mut self.calls.active {
            call.video_local = local_on;
            call.video_remote = remote_on;
            call.video_note = local_note;
            // The confirmation settles an optimistic video flip.
            if let Some((_, hand_raised)) = call.control_backup {
                call.control_backup = Some((local_on, hand_raised));
            }
            if local_on || remote_on {
                call.video = true;
            }
        }
    }

    /// A control failed: the optimistic flips go back to their
    /// backup pair, the same discipline as the audio toggles
    /// (P2-33). The notice itself is the caller's job.
    pub(crate) fn on_call_control_failed(&mut self) {
        if let Some(call) = &mut self.calls.active {
            if let Some((video_local, hand_raised)) = call.control_backup.take() {
                call.video_local = video_local;
                call.hand_raised = hand_raised;
            }
        }
    }

    /// Backend delivers one remote video frame (an encoded
    /// access unit): decode it and upload the picture as the
    /// stage's texture. Frames from a sender other than the
    /// current one restart the decoder session.
    pub(crate) fn on_call_video_au(
        &mut self,
        ctx: &egui::Context,
        sender: Option<String>,
        data: &[u8],
        keyframe: bool,
    ) {
        if self.calls.active.is_none() {
            return;
        }
        if self.calls.media.decoder_sender != sender {
            self.calls.media.decoder = None;
            self.calls.media.decoder_sender = sender.clone();
        }
        if self.calls.media.decoder.is_none() {
            if !keyframe {
                // A fresh session can only start from a
                // keyframe; earlier units are undecodable.
                return;
            }
            self.calls.media.decoder = CallDecoder::new();
        }
        let Some(decoder) = &mut self.calls.media.decoder else {
            return;
        };
        if let Some((rgba, width, height)) = decoder.decode(data) {
            let image =
                egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba);
            self.calls.media.stage_texture =
                Some(ctx.load_texture("call_stage", image, egui::TextureOptions::LINEAR));
            self.calls.media.stage_size = (width, height);
            if let Some(call) = &mut self.calls.active {
                call.stage_sender = sender;
            }
        }
    }

    /// Backend delivers the local camera's preview frame:
    /// upload it as the preview tile's texture.
    pub(crate) fn on_call_video_frame(
        &mut self,
        ctx: &egui::Context,
        rgba: Vec<u8>,
        width: u32,
        height: u32,
    ) {
        if self.calls.active.is_none() || width == 0 || height == 0 {
            return;
        }
        let image =
            egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba);
        self.calls.media.preview_texture =
            Some(ctx.load_texture("call_preview", image, egui::TextureOptions::LINEAR));
        self.calls.media.preview_size = (width, height);
    }

    /// Another of the account's devices answered or declined
    /// the call that is ringing here: the overlay ends with
    /// that reason, and the call is flagged
    /// [`CallUi::ended_elsewhere`] so the history entry it
    /// leaves is not a missed call. Returns true when the call
    /// on screen is this event's call and still ringing, so
    /// the caller records it from the tracked state; false
    /// when the app is not showing this call ringing (it never
    /// rang here, or it already left the ringing phase).
    pub(crate) fn on_call_ended_elsewhere(&mut self, call_id: &str, answered: bool) -> bool {
        let Some(call) = &mut self.calls.active else {
            return false;
        };
        if call.call_id != call_id || call.outgoing || call.phase != CallUiPhase::Incoming {
            return false;
        }
        // The ringing is over either way; a pending accept or
        // decline of ours lost the race to the other device.
        call.action_pending = false;
        call.ended_elsewhere = true;
        call.phase = CallUiPhase::Ended;
        call.reason = Some(if answered {
            crate::i18n::t("call.answered_elsewhere")
        } else {
            crate::i18n::t("call.declined_elsewhere")
        });
        if call.ended_at.is_none() {
            call.ended_at = Some(std::time::Instant::now());
        }
        true
    }

    /// Resolves a call participant's display name: the call
    /// chat's member list first (it carries the group's own
    /// labels), then a DM chat with that JID, then the JID's
    /// user part.
    pub(crate) fn call_participant_name(&self, call: &CallUi, jid: &str) -> String {
        if let Some(members) = self.groups.live_members.get(&call.chat_id) {
            if let Some(member) = members.iter().find(|m| m.address == jid) {
                if !member.label.is_empty() {
                    return member.label.clone();
                }
            }
        }
        if let Some(chat) = self.chats.iter().find(|c| self.live_jid(c.id).as_deref() == Some(jid))
        {
            if !chat.title.is_empty() {
                return chat.title.clone();
            }
        }
        jid.split('@').next().unwrap_or(jid).to_owned()
    }
}

/// The reaction set the card offers, WhatsApp's in-call set.
const REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🙏"];

/// Diameter of one call action disc.
const CALL_DISC: f32 = 56.0;

/// One labeled call action: a disc with its caption under it,
/// centered in the cell it is given (one column of the action
/// row). The whole column is the click target, and the caption
/// is always visible: a call action never hides its meaning in
/// a hover tooltip. While an action is latched (the `pending`
/// argument, fed from the call's action_pending flag) the disc,
/// its glyph and its caption paint dimmed, so the latch the
/// click handling enforces is visible instead of silently
/// swallowing further clicks.
fn call_action(
    ui: &mut egui::Ui,
    icon: Icon,
    bg: egui::Color32,
    glyph: egui::Color32,
    label: &str,
    t: core::theme::Tokens,
    pending: bool,
) -> bool {
    let mut clicked = false;
    ui.vertical_centered(|ui| {
        // Disc, a 6 px gap, and room for the 12 px caption.
        let (rect, response) = ui.allocate_exact_size(
            Vec2::new(CALL_DISC + 16.0, CALL_DISC + 6.0 + 16.0),
            Sense::click(),
        );
        let dim = |c: egui::Color32| {
            egui::Color32::from_rgba_unmultiplied(
                c.r(),
                c.g(),
                c.b(),
                (f32::from(c.a()) * 0.55) as u8,
            )
        };
        let (bg, glyph, label_color) = if pending {
            (dim(bg), dim(glyph), dim(rgb(t.text_secondary)))
        } else {
            (bg, glyph, rgb(t.text_secondary))
        };
        let disc_center = egui::Pos2::new(rect.center().x, rect.top() + CALL_DISC / 2.0);
        ui.painter().circle_filled(disc_center, CALL_DISC / 2.0, bg);
        icons::draw(ui.painter(), icon, disc_center, crate::icons::SIZE_TAB, glyph, bg);
        ui.painter().text(
            egui::Pos2::new(rect.center().x, rect.top() + CALL_DISC + 6.0),
            Align2::CENTER_TOP,
            label,
            egui::FontId::proportional(12.0),
            label_color,
        );
        clicked = response.on_hover_text(label).clicked();
    });
    clicked
}

impl App {
    /// The camera toggle: flips optimistically with the
    /// control backup taken, the same discipline as the audio
    /// toggles (P2-33).
    pub(crate) fn toggle_call_video(&mut self) {
        if let Some(call) = &mut self.calls.active {
            if call.control_backup.is_none() {
                call.control_backup = Some((call.video_local, call.hand_raised));
            }
            call.video_local = !call.video_local;
            let on = call.video_local;
            self.send_backend(BackendCommand::SetCallVideo { on });
        }
    }

    /// The hand-raise toggle, same optimistic discipline.
    pub(crate) fn toggle_call_hand(&mut self) {
        if let Some(call) = &mut self.calls.active {
            if call.control_backup.is_none() {
                call.control_backup = Some((call.video_local, call.hand_raised));
            }
            call.hand_raised = !call.hand_raised;
            let raised = call.hand_raised;
            self.send_backend(BackendCommand::SetCallHandRaised { raised });
        }
    }

    /// Sends (or clears) my reaction.
    pub(crate) fn send_call_reaction(&mut self, emoji: &str) {
        if let Some(call) = &mut self.calls.active {
            if call.my_reaction.as_deref() == Some(emoji) {
                call.my_reaction = None;
                self.send_backend(BackendCommand::SendCallReaction { emoji: String::new() });
            } else {
                call.my_reaction = Some(emoji.to_owned());
                self.send_backend(BackendCommand::SendCallReaction { emoji: emoji.to_owned() });
            }
        }
    }

    /// Admits one waiting user (the pending latch clears when
    /// the next waiting-room snapshot lands).
    pub(crate) fn admit_waiting_user(&mut self, jid: &str) {
        if let Some(call) = &mut self.calls.active {
            call.waiting_pending = Some(jid.to_owned());
        }
        self.send_backend(BackendCommand::AdmitWaitingUser { jid: jid.to_owned() });
    }

    /// Denies one waiting user.
    pub(crate) fn deny_waiting_user(&mut self, jid: &str) {
        if let Some(call) = &mut self.calls.active {
            call.waiting_pending = Some(jid.to_owned());
        }
        self.send_backend(BackendCommand::DenyWaitingUser { jid: jid.to_owned() });
    }

    /// Flips the call's approval requirement.
    pub(crate) fn toggle_call_approval(&mut self) {
        let enabled = self
            .calls
            .active
            .as_ref()
            .and_then(|c| c.waiting.as_ref())
            .is_some_and(|w| !w.approval_required);
        self.send_backend(BackendCommand::SetCallApproval { enabled });
    }

    /// Invites one group member into the call and closes the
    /// picker.
    pub(crate) fn invite_call_participant(&mut self, jid: &str) {
        self.calls.invite_open = false;
        self.send_backend(BackendCommand::InviteCallParticipant { jid: jid.to_owned() });
    }

    /// The call overlay: drawn above everything while a call
    /// is ringing or live. The card follows the call-surface
    /// standard (section 19): one center axis, every action a
    /// captioned disc, no control without a real action.
    pub(crate) fn draw_call_overlay(&mut self, ctx: &egui::Context) {
        // The ended card dismisses itself shortly after the
        // call ends.
        let dismiss = self.calls.active.as_ref().is_some_and(|call| {
            call.phase == CallUiPhase::Ended
                && call.ended_at.is_some_and(|at| at.elapsed().as_secs() >= 4)
        });
        if dismiss {
            self.calls.active = None;
            self.calls.media = CallMediaState::default();
            self.calls.invite_open = false;
            return;
        }
        let Some(call) = self.calls.active.clone() else {
            return;
        };
        let t = self.tokens;
        let mut action: Option<String> = None;
        let media_stage = self.calls.media.stage_texture.clone();
        let media_preview = self.calls.media.preview_texture.clone();
        let sharing_jid = call.participants.iter().find(|p| p.sharing).map(|p| p.jid.clone());
        Area::new(Id::unique("call_overlay")).anchor(Align2::CENTER_CENTER, Vec2::ZERO).show(
            ctx,
            |ui| {
                crate::theme::glass_frame(t, crate::theme::RADIUS_PANEL)
                    .fill(rgb(t.bg_panel))
                    .inner_margin(Margin::same(20))
                    .show(ui, |ui| {
                        ui.set_width(336.0);
                        ui.vertical_centered(|ui| {
                            // The video stage: the remote
                            // picture, with the local preview
                            // in its corner and a caption chip
                            // naming who is on it.
                            let stage_on = call.phase != CallUiPhase::Incoming
                                && call.phase != CallUiPhase::Ended
                                && (call.video
                                    || call.video_remote
                                    || media_stage.is_some()
                                    || media_preview.is_some());
                            if stage_on {
                                let width = ui.available_width();
                                let height = match media_stage {
                                    Some(ref tex) => {
                                        let (sw, sh) = self.calls.media.stage_size;
                                        if sw > 0 {
                                            (width * sh as f32 / sw as f32).clamp(120.0, 224.0)
                                        } else {
                                            let _ = tex;
                                            168.0
                                        }
                                    }
                                    None => 168.0,
                                };
                                let (rect, _) = ui
                                    .allocate_exact_size(Vec2::new(width, height), Sense::hover());
                                ui.painter().rect_filled(rect, 12.0, rgb(t.bg_panel_alt));
                                if let Some(ref tex) = media_stage {
                                    ui.painter().image(
                                        tex.id(),
                                        rect,
                                        egui::Rect::from_min_max(
                                            egui::pos2(0.0, 0.0),
                                            egui::pos2(1.0, 1.0),
                                        ),
                                        egui::Color32::WHITE,
                                    );
                                } else {
                                    icons::draw(
                                        ui.painter(),
                                        Icon::Videocam,
                                        rect.center(),
                                        icons::SIZE_DISPLAY,
                                        rgb(t.text_faint),
                                        rgb(t.bg_panel_alt),
                                    );
                                }
                                // The local preview tile.
                                if let Some(ref tex) = media_preview {
                                    let tile = egui::Rect::from_min_size(
                                        egui::pos2(rect.max.x - 96.0, rect.max.y - 70.0),
                                        Vec2::new(88.0, 62.0),
                                    );
                                    ui.painter().image(
                                        tex.id(),
                                        tile,
                                        egui::Rect::from_min_max(
                                            egui::pos2(0.0, 0.0),
                                            egui::pos2(1.0, 1.0),
                                        ),
                                        egui::Color32::WHITE,
                                    );
                                    ui.painter().rect_stroke(
                                        tile,
                                        8.0,
                                        egui::Stroke::new(1.0, egui::Color32::from_white_alpha(70)),
                                        egui::StrokeKind::Inside,
                                    );
                                }
                                // The stage caption: who is
                                // sharing, or whose picture
                                // this is in a group call.
                                let caption = if let Some(jid) = &sharing_jid {
                                    Some(format!(
                                        "{} {}",
                                        self.call_participant_name(&call, jid),
                                        crate::i18n::t("call.is_sharing_screen"),
                                    ))
                                } else if call.group {
                                    call.stage_sender
                                        .as_ref()
                                        .map(|jid| self.call_participant_name(&call, jid))
                                } else {
                                    None
                                };
                                if let Some(caption) = caption {
                                    let galley = ui.painter().layout_no_wrap(
                                        caption,
                                        egui::FontId::proportional(11.0),
                                        egui::Color32::WHITE,
                                    );
                                    let chip = egui::Rect::from_min_size(
                                        egui::pos2(
                                            rect.min.x + 8.0,
                                            rect.max.y - galley.size().y - 14.0,
                                        ),
                                        galley.size() + Vec2::new(14.0, 6.0),
                                    );
                                    ui.painter().rect_filled(
                                        chip,
                                        8.0,
                                        egui::Color32::from_black_alpha(140),
                                    );
                                    ui.painter().galley(
                                        chip.min + Vec2::new(7.0, 3.0),
                                        galley,
                                        egui::Color32::WHITE,
                                    );
                                }
                                ui.add_space(SP_3);
                            }
                            // The peer identity, on the shared
                            // chat avatar dispatch (section
                            // 20): a 1:1 call shows its chat, a
                            // group call its group, a link call
                            // the silhouette.
                            let radius = if stage_on { 20.0 } else { 32.0 };
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::splat(radius * 2.0), Sense::hover());
                            let center = rect.center();
                            match self.chats.iter().find(|c| c.id == call.chat_id).cloned() {
                                Some(chat) => {
                                    let photo = self.chat_avatar_photo(ui.ctx(), &chat);
                                    crate::widgets::paint_chat_avatar_photo(
                                        ui.painter(),
                                        center,
                                        radius,
                                        &chat,
                                        photo,
                                    );
                                }
                                None => crate::widgets::paint_avatar(
                                    ui.painter(),
                                    center,
                                    radius,
                                    call.group,
                                    (call.chat_id % 251) as u8,
                                ),
                            }
                            ui.add_space(SP_2);
                            ui.label(
                                RichText::new(&call.title)
                                    .font(crate::theme::semibold(19.0))
                                    .color(rgb(t.text_primary)),
                            );
                            ui.add_space(2.0);
                            ui.label(
                                RichText::new(call.status_line())
                                    .size(12.0)
                                    .color(rgb(t.text_secondary)),
                            );
                            if let Some(note) = &call.video_note {
                                ui.add_space(2.0);
                                ui.label(
                                    RichText::new(note).size(12.0).color(rgb(t.text_secondary)),
                                );
                            }
                            // The roster strip: group and
                            // link calls past the ringing
                            // phase.
                            if call.group
                                && !call.participants.is_empty()
                                && call.phase != CallUiPhase::Incoming
                                && call.phase != CallUiPhase::Ended
                            {
                                ui.add_space(SP_SECTION);
                                let shown = call.participants.len().min(6);
                                for participant in call.participants.iter().take(shown) {
                                    self.draw_call_participant_row(ui, &call, participant, t);
                                }
                                if call.participants.len() > shown {
                                    ui.label(
                                        RichText::new(format!(
                                            "+{} {}",
                                            call.participants.len() - shown,
                                            crate::i18n::t("call.more_people"),
                                        ))
                                        .size(12.0)
                                        .color(rgb(t.text_faint)),
                                    );
                                }
                            }
                            // The waiting room, for the host.
                            if let Some(waiting) = &call.waiting {
                                if waiting.is_admin {
                                    if !waiting.users.is_empty() {
                                        ui.add_space(SP_SECTION);
                                        ui.label(
                                            RichText::new(crate::i18n::t("call.waiting_room"))
                                                .size(12.0)
                                                .strong()
                                                .color(rgb(t.text_secondary)),
                                        );
                                        for user in &waiting.users {
                                            let name = user.phone.clone().unwrap_or_else(|| {
                                                self.call_participant_name(&call, &user.jid)
                                            });
                                            ui.add_space(SP_2);
                                            ui.horizontal(|ui| {
                                                ui.label(
                                                    RichText::new(name)
                                                        .size(12.0)
                                                        .color(rgb(t.text_primary)),
                                                );
                                                let pending = call.waiting_pending.as_deref()
                                                    == Some(user.jid.as_str());
                                                if !pending {
                                                    if crate::widgets::button(
                                                        ui,
                                                        t,
                                                        &crate::i18n::t("call.admit"),
                                                        crate::widgets::Btn::Primary,
                                                    )
                                                    .clicked()
                                                    {
                                                        action =
                                                            Some(format!("admit:{}", user.jid));
                                                    }
                                                    if crate::widgets::button(
                                                        ui,
                                                        t,
                                                        &crate::i18n::t("call.deny"),
                                                        crate::widgets::Btn::Secondary,
                                                    )
                                                    .clicked()
                                                    {
                                                        action = Some(format!("deny:{}", user.jid));
                                                    }
                                                }
                                            });
                                        }
                                    }
                                    // The approval switch: hosts
                                    // decide whether joining
                                    // needs them at all.
                                    ui.add_space(SP_3);
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(crate::i18n::t("call.require_approval"))
                                                .size(12.0)
                                                .color(rgb(t.text_primary)),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                let mut approval = waiting.approval_required;
                                                crate::widgets::switch(ui, &mut approval, t);
                                                if approval != waiting.approval_required {
                                                    action = Some("approval".to_owned());
                                                }
                                            },
                                        );
                                    });
                                }
                            }
                            // The reaction row, while the call
                            // is live.
                            if call.phase == CallUiPhase::Active {
                                ui.add_space(SP_SECTION);
                                ui.horizontal(|ui| {
                                    ui.add_space(18.0);
                                    for emoji in REACTIONS {
                                        let mine = call.my_reaction.as_deref() == Some(emoji);
                                        let mut button =
                                            egui::Button::new(RichText::new(emoji).size(17.0))
                                                .corner_radius(18.0);
                                        button = if mine {
                                            button.fill(rgb(t.bg_panel_alt))
                                        } else {
                                            button.frame(false)
                                        };
                                        if ui.add(button).clicked() {
                                            action = Some(format!("react:{emoji}"));
                                        }
                                    }
                                });
                            }
                            if call.phase != CallUiPhase::Ended {
                                ui.add_space(SP_SECTION);
                            }
                            // The actions, on the one center
                            // axis: even columns of labeled
                            // discs (section 19). Decline and
                            // End wear the danger fill, Accept
                            // the accent, and a toggle that is
                            // on wears the accent too.
                            match call.phase {
                                CallUiPhase::Incoming => {
                                    ui.columns(2, |cols| {
                                        if call_action(
                                            &mut cols[0],
                                            Icon::CallEnd,
                                            rgb(t.danger),
                                            rgb(t.on_danger()),
                                            &crate::i18n::t("call.decline"),
                                            t,
                                            call.action_pending,
                                        ) {
                                            action = Some("reject".to_owned());
                                        }
                                        if call_action(
                                            &mut cols[1],
                                            Icon::Phone,
                                            rgb(t.accent),
                                            rgb(t.on_accent()),
                                            &crate::i18n::t("call.accept"),
                                            t,
                                            call.action_pending,
                                        ) {
                                            action = Some("accept".to_owned());
                                        }
                                    });
                                }
                                CallUiPhase::Ended => {
                                    if crate::widgets::button(
                                        ui,
                                        t,
                                        &crate::i18n::t("common.close"),
                                        crate::widgets::Btn::Secondary,
                                    )
                                    .clicked()
                                    {
                                        action = Some("close".to_owned());
                                    }
                                }
                                _ => {
                                    ui.columns(4, |cols| {
                                        let mute_icon =
                                            if call.muted { Icon::MicOff } else { Icon::Mic };
                                        if call_action(
                                            &mut cols[0],
                                            mute_icon,
                                            rgb(t.bg_panel_alt),
                                            rgb(t.text_primary),
                                            &crate::i18n::t(if call.muted {
                                                "common.unmute"
                                            } else {
                                                "common.mute"
                                            }),
                                            t,
                                            call.action_pending,
                                        ) {
                                            action = Some("mute".to_owned());
                                        }
                                        // The speaker toggle wears
                                        // the accent while it is on,
                                        // so its state reads at a
                                        // glance, not only from its
                                        // glyph.
                                        let (speaker_bg, speaker_glyph) = if call.speaker {
                                            (rgb(t.accent), rgb(t.on_accent()))
                                        } else {
                                            (rgb(t.bg_panel_alt), rgb(t.text_primary))
                                        };
                                        if call_action(
                                            &mut cols[1],
                                            Icon::VolumeUp,
                                            speaker_bg,
                                            speaker_glyph,
                                            &crate::i18n::t("call.speaker"),
                                            t,
                                            call.action_pending,
                                        ) {
                                            action = Some("speaker".to_owned());
                                        }
                                        let (video_bg, video_glyph) = if call.video_local {
                                            (rgb(t.accent), rgb(t.on_accent()))
                                        } else {
                                            (rgb(t.bg_panel_alt), rgb(t.text_primary))
                                        };
                                        if call_action(
                                            &mut cols[2],
                                            if call.video_local {
                                                Icon::Videocam
                                            } else {
                                                Icon::VideocamOff
                                            },
                                            video_bg,
                                            video_glyph,
                                            &crate::i18n::t("call.video"),
                                            t,
                                            call.action_pending,
                                        ) {
                                            action = Some("video".to_owned());
                                        }
                                        if call_action(
                                            &mut cols[3],
                                            Icon::CallEnd,
                                            rgb(t.danger),
                                            rgb(t.on_danger()),
                                            &crate::i18n::t("call.end"),
                                            t,
                                            call.action_pending,
                                        ) {
                                            action = Some("end".to_owned());
                                        }
                                    });
                                    if call.group
                                        && (call.phase == CallUiPhase::Connecting
                                            || call.phase == CallUiPhase::Active)
                                    {
                                        ui.add_space(SP_3);
                                        ui.columns(2, |cols| {
                                            let (hand_bg, hand_glyph) = if call.hand_raised {
                                                (rgb(t.accent), rgb(t.on_accent()))
                                            } else {
                                                (rgb(t.bg_panel_alt), rgb(t.text_primary))
                                            };
                                            if call_action(
                                                &mut cols[0],
                                                Icon::BackHand,
                                                hand_bg,
                                                hand_glyph,
                                                &crate::i18n::t(if call.hand_raised {
                                                    "call.lower_hand"
                                                } else {
                                                    "call.raise_hand"
                                                }),
                                                t,
                                                call.action_pending,
                                            ) {
                                                action = Some("hand".to_owned());
                                            }
                                            if call_action(
                                                &mut cols[1],
                                                Icon::GroupAdd,
                                                rgb(t.bg_panel_alt),
                                                rgb(t.text_primary),
                                                &crate::i18n::t("call.add_people"),
                                                t,
                                                call.action_pending,
                                            ) {
                                                action = Some("invite".to_owned());
                                            }
                                        });
                                    }
                                }
                            }
                        });
                    });
            },
        );
        match action.as_deref() {
            Some("accept") => self.accept_active_call(),
            Some("reject") => self.reject_active_call(),
            Some("end") => self.end_active_call(),
            Some("mute") => {
                if let Some(call) = &mut self.calls.active {
                    // Optimistic flip with a backup: the
                    // backend's CallAudioState confirms it, an
                    // error reverts it (P2-33).
                    call.audio_backup = Some((call.muted, call.speaker));
                    call.muted = !call.muted;
                    let muted = call.muted;
                    self.send_backend(BackendCommand::SetCallMuted { muted });
                }
            }
            Some("speaker") => {
                if let Some(call) = &mut self.calls.active {
                    call.audio_backup = Some((call.muted, call.speaker));
                    call.speaker = !call.speaker;
                    let speaker = call.speaker;
                    self.send_backend(BackendCommand::SetCallSpeaker { speaker });
                }
            }
            Some("close") => {
                self.calls.active = None;
                self.calls.media = CallMediaState::default();
                self.calls.invite_open = false;
            }
            Some("video") => self.toggle_call_video(),
            Some("hand") => self.toggle_call_hand(),
            Some("invite") => {
                self.calls.invite_open = true;
                self.ensure_call_members();
            }
            Some("approval") => self.toggle_call_approval(),
            Some(other) => {
                if let Some(jid) = other.strip_prefix("admit:") {
                    self.admit_waiting_user(jid);
                } else if let Some(jid) = other.strip_prefix("deny:") {
                    self.deny_waiting_user(jid);
                } else if let Some(emoji) = other.strip_prefix("react:") {
                    let emoji = emoji.to_owned();
                    self.send_call_reaction(&emoji);
                }
            }
            None => {}
        }
    }

    /// One roster row: avatar, name, and the participant's
    /// live marks (reaction, raised hand, screen share, and a
    /// state word while they are not connected yet).
    fn draw_call_participant_row(
        &mut self,
        ui: &mut egui::Ui,
        call: &CallUi,
        participant: &backend::CallParticipant,
        t: core::theme::Tokens,
    ) {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
            let center = rect.center();
            match self
                .chats
                .iter()
                .find(|c| self.live_jid(c.id).as_deref() == Some(participant.jid.as_str()))
                .cloned()
            {
                Some(chat) => {
                    let photo = self.chat_avatar_photo(ui.ctx(), &chat);
                    crate::widgets::paint_chat_avatar_photo(
                        ui.painter(),
                        center,
                        14.0,
                        &chat,
                        photo,
                    );
                }
                None => crate::widgets::paint_avatar(
                    ui.painter(),
                    center,
                    14.0,
                    false,
                    (crate::live::jid_id(&participant.jid) % 251) as u8,
                ),
            }
            ui.add_space(SP_2);
            ui.label(
                RichText::new(self.call_participant_name(call, &participant.jid))
                    .size(13.0)
                    .color(rgb(t.text_primary)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if participant.state != "connected" {
                    ui.label(RichText::new(&participant.state).size(11.0).color(rgb(t.text_faint)));
                }
                if participant.sharing {
                    icons::draw(
                        ui.painter(),
                        Icon::ScreenShare,
                        ui.cursor().min + Vec2::new(8.0, 8.0),
                        icons::SIZE_BADGE,
                        rgb(t.accent),
                        rgb(t.bg_panel),
                    );
                    ui.add_space(16.0);
                }
                if participant.hand_raised {
                    icons::draw(
                        ui.painter(),
                        Icon::BackHand,
                        ui.cursor().min + Vec2::new(8.0, 8.0),
                        icons::SIZE_BADGE,
                        rgb(t.accent),
                        rgb(t.bg_panel),
                    );
                    ui.add_space(16.0);
                }
                if let Some(emoji) = call.reactions.get(&participant.jid) {
                    ui.label(RichText::new(emoji).size(14.0));
                }
            });
        });
        ui.add_space(2.0);
    }

    /// Fetches the call chat's member list when the invite
    /// picker needs it and it is not on hand, the same
    /// fetch-once discipline as the profile panel.
    fn ensure_call_members(&mut self) {
        let Some(call) = &self.calls.active else {
            return;
        };
        let chat_id = call.chat_id;
        if self.groups.live_members.contains_key(&chat_id)
            || self.groups.members_requested.contains(&chat_id)
        {
            return;
        }
        if let Some(jid) = self.live_jid(chat_id) {
            self.groups.members_requested.insert(chat_id);
            self.send_backend(BackendCommand::FetchGroupMembers { chat_jid: jid });
        }
    }

    /// The Add people picker: the call group's members who
    /// are not in the call yet. Picking one invites them.
    pub(crate) fn draw_invite_picker(&mut self, ctx: &egui::Context) {
        if !self.calls.invite_open {
            return;
        }
        let Some(call) = self.calls.active.clone() else {
            self.calls.invite_open = false;
            return;
        };
        let t = self.tokens;
        let mut close = false;
        let mut invite: Option<String> = None;
        egui::Window::new(crate::i18n::t("call.add_people"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .fixed_size(Vec2::new(320.0, 380.0))
            .frame(
                egui::Frame::new()
                    .fill(rgb(t.bg_panel))
                    .corner_radius(egui::CornerRadius::same(20))
                    .inner_margin(Margin::same(16)),
            )
            .show(ctx, |ui| {
                match self.groups.live_members.get(&call.chat_id) {
                    Some(members) => {
                        let in_call: std::collections::HashSet<&str> =
                            call.participants.iter().map(|p| p.jid.as_str()).collect();
                        let rows: Vec<&crate::state::MemberRow> = members
                            .iter()
                            .filter(|m| !in_call.contains(m.address.as_str()))
                            .collect();
                        if rows.is_empty() {
                            ui.label(
                                RichText::new(crate::i18n::t("call.everyone_here"))
                                    .size(12.0)
                                    .color(rgb(t.text_secondary)),
                            );
                        }
                        egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                            for member in rows {
                                let (rect, response) = ui.allocate_exact_size(
                                    Vec2::new(ui.available_width(), 44.0),
                                    Sense::click(),
                                );
                                if response.hovered() {
                                    ui.painter().rect_filled(rect, 8.0, rgb(t.bg_panel_alt));
                                }
                                let center = egui::pos2(rect.min.x + 22.0, rect.center().y);
                                crate::widgets::paint_avatar(
                                    ui.painter(),
                                    center,
                                    14.0,
                                    false,
                                    (crate::live::jid_id(&member.address) % 251) as u8,
                                );
                                ui.painter().text(
                                    egui::pos2(rect.min.x + 44.0, rect.center().y),
                                    Align2::LEFT_CENTER,
                                    &member.label,
                                    egui::FontId::proportional(13.0),
                                    rgb(t.text_primary),
                                );
                                if response.clicked() {
                                    invite = Some(member.address.clone());
                                }
                            }
                        });
                    }
                    None => {
                        ui.label(
                            RichText::new(crate::i18n::t("call.loading_members"))
                                .size(12.0)
                                .color(rgb(t.text_secondary)),
                        );
                    }
                }
                ui.add_space(SP_3);
                if crate::widgets::button(
                    ui,
                    t,
                    &crate::i18n::t("common.close"),
                    crate::widgets::Btn::Secondary,
                )
                .clicked()
                {
                    close = true;
                }
            });
        if let Some(jid) = invite {
            self.invite_call_participant(&jid);
        }
        if close {
            self.calls.invite_open = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::format_duration;

    #[test]
    fn durations_format_like_a_call_timer() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(65), "1:05");
        assert_eq!(format_duration(3599), "59:59");
        assert_eq!(format_duration(3661), "1:01:01");
    }
}
