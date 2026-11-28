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

/// How a call ended, from the answer flags on [`CallUi`].
/// `Connected` is the only outcome with a talk duration; the
/// others are the honest words the call-ended summary shows
/// for a call that never connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallOutcome {
    /// The call reached Active: it has a talk duration.
    Connected,
    /// An incoming call this device declined.
    Declined,
    /// Another of the account's devices answered or declined
    /// the call while it rang here.
    AnsweredElsewhere,
    /// An incoming call nobody answered.
    Missed,
    /// An outgoing call the peer never answered.
    NoAnswer,
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

    /// How this call ended, derived from the answer flags.
    /// The one derivation the Calls log record and the
    /// call-ended summary both read, so the two surfaces can
    /// never disagree about what happened.
    pub(crate) fn outcome(&self) -> CallOutcome {
        if self.connected {
            CallOutcome::Connected
        } else if self.declined {
            CallOutcome::Declined
        } else if self.ended_elsewhere {
            CallOutcome::AnsweredElsewhere
        } else if !self.outgoing {
            CallOutcome::Missed
        } else {
            CallOutcome::NoAnswer
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
            missed: self.outcome() == CallOutcome::Missed,
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
            // The backend's reason is not lost: the Ended
            // summary renders it as its closing line, under the
            // fact rows it explains.
            CallUiPhase::Ended => crate::i18n::t("call.ended"),
        }
    }
}

/// Clamps the floating card's center offset so the card can
/// never be dragged fully off-window: at least a 64 px slice
/// of it stays inside the window on both axes, whatever the
/// drag did and however the window was resized since.
fn clamp_card_offset(offset: Vec2, card: Vec2, window: egui::Rect) -> Vec2 {
    const KEEP: f32 = 64.0;
    let max_x = ((window.width() + card.x) / 2.0 - KEEP).max(0.0);
    let max_y = ((window.height() + card.y) / 2.0 - KEEP).max(0.0);
    Vec2::new(offset.x.clamp(-max_x, max_x), offset.y.clamp(-max_y, max_y))
}

/// The largest rect of aspect `frame_w:frame_h` that fits the
/// available box: the expanded stage shows the whole frame,
/// letterboxed by the card's own ground, never cropped or
/// stretched.
fn fit_stage(frame_w: f32, frame_h: f32, avail_w: f32, avail_h: f32) -> (f32, f32) {
    let scale = (avail_w / frame_w).min(avail_h / frame_h);
    (frame_w * scale, frame_h * scale)
}

/// The height the expanded card's non-stage sections take,
/// estimated from the same layout constants the draw below
/// uses (frame margins, the toggle row, the header strip,
/// roster rows at 30 px, the action rows at the disc column's
/// height). The stage gets whatever height this leaves in
/// the window; the estimate errs on the generous side so the
/// card's foot stays inside the window.
fn stage_chrome_height(call: &CallUi, sections_on: bool) -> f32 {
    // Frame margins, the toggle row and its gap, the stage's
    // own trailing gap.
    let mut h = 40.0 + 28.0 + SP_2 + SP_3;
    // Header strip: the avatar (only without sections) plus
    // the title and status lines.
    if !sections_on {
        h += 40.0 + SP_2;
    }
    h += 25.0 + 2.0 + 16.0;
    if call.video_note.is_some() {
        h += 18.0;
    }
    // Roster rows.
    if call.group
        && !call.participants.is_empty()
        && call.phase != CallUiPhase::Incoming
        && call.phase != CallUiPhase::Ended
    {
        let shown = call.participants.len().min(6);
        h += SP_SECTION + shown as f32 * 30.0;
        if call.participants.len() > shown {
            h += 16.0;
        }
    }
    // Waiting room: heading, one row per waiting user, and
    // the approval switch row.
    if let Some(waiting) = &call.waiting {
        if waiting.is_admin {
            if !waiting.users.is_empty() {
                h += SP_SECTION + 16.0 + waiting.users.len() as f32 * 36.0;
            }
            h += SP_3 + 28.0;
        }
    }
    // Reaction row.
    if call.phase == CallUiPhase::Active {
        h += SP_SECTION + 36.0;
    }
    // The gap above the actions and the action rows
    // themselves (the group row rides second, when it draws).
    h += SP_SECTION + 78.0;
    if call.group && (call.phase == CallUiPhase::Connecting || call.phase == CallUiPhase::Active) {
        h += SP_2 + 78.0;
    }
    h
}

/// The card's expand/shrink control: a 28 px square carrying
/// a corner-bracket glyph painted directly, because the
/// bundled icon font carries no expand glyph and that font is
/// regenerated only by its packaging script. Expand brackets
/// hug the square's corners; shrink brackets sit inset with
/// their arms reaching back out, the reverse gesture.
/// Keyboard reachable in the same grammar as the call
/// actions: the click sense makes it focusable, egui turns
/// Space/Enter into a click, the focus ring is the house
/// 1.5 px accent stroke, and the accessibility label is the
/// control's own caption.
fn expand_toggle(ui: &mut egui::Ui, expanded: bool, t: core::theme::Tokens) -> bool {
    let label = crate::i18n::t(if expanded { "call.shrink" } else { "call.expand" });
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
    let hov = crate::anim::hover_factor(ui, ("call_expand_toggle", expanded), response.hovered());
    if hov > 0.001 {
        ui.painter().rect_filled(
            rect,
            8.0,
            crate::theme::glass_wash(t, t.bg_panel_alt, (hov * 115.0) as u8),
        );
    }
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect,
            8.0,
            egui::Stroke::new(1.5, rgb(t.accent)),
            egui::StrokeKind::Inside,
        );
    }
    let stroke = egui::Stroke::new(1.8, rgb(t.text_secondary));
    let (inset, arm) = if expanded { (11.0, -5.0) } else { (6.0, 5.0) };
    for (cx, cy, dx, dy) in [
        (rect.min.x, rect.min.y, 1.0, 1.0),
        (rect.max.x, rect.min.y, -1.0, 1.0),
        (rect.min.x, rect.max.y, 1.0, -1.0),
        (rect.max.x, rect.max.y, -1.0, -1.0),
    ] {
        let anchor = egui::pos2(cx + dx * inset, cy + dy * inset);
        ui.painter().line_segment([anchor, egui::pos2(anchor.x + dx * arm, anchor.y)], stroke);
        ui.painter().line_segment([anchor, egui::pos2(anchor.x, anchor.y + dy * arm)], stroke);
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label.clone()));
    response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(&label).clicked()
}

/// One fact row of the call-ended summary: the label at the
/// left in the faint step, the value at the right in the
/// primary step, on the card's full width.
fn summary_row(ui: &mut egui::Ui, label: &str, value: &str, t: core::theme::Tokens) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(12.0).color(rgb(t.text_faint)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(value).size(12.0).strong().color(rgb(t.text_primary)));
        });
    });
    ui.add_space(2.0);
}

/// The call-ended summary's fact rows. Duration appears only
/// for a call that connected (a call that never connected has
/// no talk time to state); every other call gets its outcome
/// word from the same derivation the history record uses.
fn ended_summary(ui: &mut egui::Ui, call: &CallUi, t: core::theme::Tokens) {
    let outcome = call.outcome();
    if outcome == CallOutcome::Connected {
        summary_row(
            ui,
            &crate::i18n::t("call.summary_duration"),
            &format_duration(call.seconds),
            t,
        );
    } else {
        let word = match outcome {
            CallOutcome::Connected => unreachable!(),
            CallOutcome::Missed => crate::i18n::t("calls.missed"),
            CallOutcome::Declined => crate::i18n::t("call.declined"),
            CallOutcome::AnsweredElsewhere => crate::i18n::t("call.answered_elsewhere"),
            CallOutcome::NoAnswer => crate::i18n::t("call.no_answer"),
        };
        summary_row(ui, &crate::i18n::t("call.summary_outcome"), &word, t);
    }
    summary_row(
        ui,
        &crate::i18n::t("call.summary_started"),
        &crate::live::fmt_time(call.started_at),
        t,
    );
    let kind = match (call.group, call.video) {
        (true, true) => crate::i18n::t("call.type_group_video"),
        (true, false) => crate::i18n::t("call.type_group_voice"),
        (false, true) => crate::i18n::t("calls.video"),
        (false, false) => crate::i18n::t("calls.voice"),
    };
    summary_row(ui, &crate::i18n::t("call.summary_type"), &kind, t);
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

/// The call card's presentation state: where the floating
/// card has been dragged to, and whether it is expanded to
/// the in-app stage. Held outside the (cloned-per-draw)
/// [`CallUi`] alongside the media state, and reset with it at
/// every call adoption and dismissal, so a new call always
/// starts centered and floating (the screenshot rig stages
/// calls through the same adoption path and inherits the
/// reset).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CallViewState {
    /// Offset from the window center the user dragged the
    /// floating card to; clamped every frame so the card can
    /// never be dragged fully off-window.
    pub offset: Vec2,
    /// The card fills the window's content area and the video
    /// stage takes the available height. In-app only, never
    /// OS fullscreen.
    pub expanded: bool,
}

/// Decoded call video held outside the (cloned-per-draw)
/// [`CallUi`]: the decoder session and the uploaded textures
/// for the stage and the local preview. The card's
/// presentation state rides here too (see [`CallViewState`]):
/// this is the call-scoped state that outlives the per-draw
/// clone and resets with each call.
#[derive(Default)]
pub(crate) struct CallMediaState {
    /// The card's presentation state (drag offset, expanded).
    pub view: CallViewState,
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
        // Hover and press step the disc fill through the shared
        // factors, the same endpoints the dock rows use, so a call
        // action answers the pointer like every other control
        // instead of sitting flat until it is clicked.
        let hov = crate::anim::hover_factor(ui, ("call_action", label), response.hovered());
        let press = crate::anim::press_factor(
            ui,
            ("call_action", label),
            response.is_pointer_button_down_on(),
        );
        let base = core::theme::Rgb(bg.r(), bg.g(), bg.b());
        let stepped = if press > 0.001 {
            core::theme::mix(base, core::theme::pressed_step(base), press)
        } else {
            core::theme::mix(base, core::theme::hover_step(base), hov)
        };
        let bg = egui::Color32::from_rgba_unmultiplied(stepped.0, stepped.1, stepped.2, bg.a());
        let (bg, glyph, label_color) = if pending {
            (dim(bg), dim(glyph), dim(rgb(t.text_secondary)))
        } else {
            (bg, glyph, rgb(t.text_secondary))
        };
        let disc_center = egui::Pos2::new(rect.center().x, rect.top() + CALL_DISC / 2.0);
        ui.painter().circle_filled(disc_center, CALL_DISC / 2.0, bg);
        // The keyboard focus ring, in the house widget idiom
        // (1.5 px accent stroke): the click sense makes the
        // disc tab-focusable and egui turns Space/Enter into a
        // click for the focused widget, so the ring is the one
        // piece the discs had to add to be keyboard reachable.
        // It rings outside the disc, on the card's own ground,
        // so it reads on every disc fill.
        if response.has_focus() {
            ui.painter().circle_stroke(
                disc_center,
                CALL_DISC / 2.0 + 3.0,
                egui::Stroke::new(1.5, rgb(t.accent)),
            );
        }
        icons::draw(ui.painter(), icon, disc_center, crate::icons::SIZE_TAB, glyph, bg);
        ui.painter().text(
            egui::Pos2::new(rect.center().x, rect.top() + CALL_DISC + 6.0),
            Align2::CENTER_TOP,
            label,
            egui::FontId::proportional(12.0),
            label_color,
        );
        // The accessibility label: the caption's own text, so
        // a screen reader names the control exactly as the
        // card prints it.
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, !pending, label));
        clicked =
            response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(label).clicked();
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

    /// The call overlay: the call card, floating over the
    /// conversation while a call is ringing or live. The card
    /// follows the call-surface standard (section 19): one
    /// center axis, every action a captioned disc, no control
    /// without a real action.
    ///
    /// Layering, chosen deliberately: the card's Area sits at
    /// `Order::Middle`. The conversation and panels paint at
    /// Background/Middle earlier in the frame, so the card
    /// rides above them; every popup that must outrank a call
    /// (the context menus, the confirm modals, the invite
    /// picker window) paints at `Order::Foreground`, so a menu
    /// or a confirmation always opens over the card, never
    /// under it.
    pub(crate) fn draw_call_overlay(&mut self, ctx: &egui::Context) {
        // The ended card carries an explicit Close control;
        // this auto-dismiss is only the backstop that keeps an
        // unread summary from stranding on screen, set long
        // enough to read the facts and use the actions.
        let dismiss = self.calls.active.as_ref().is_some_and(|call| {
            call.phase == CallUiPhase::Ended
                && call.ended_at.is_some_and(|at| at.elapsed().as_secs() >= 20)
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
        let window = ctx.content_rect();
        // The ended summary always floats: the stage exists to
        // watch a live call, and a summary stretched across the
        // window would only scatter its few facts.
        let mut expanded = self.calls.media.view.expanded && call.phase != CallUiPhase::Ended;
        // Escape leaves the stage for the floating card. In
        // floating mode Escape does nothing to the call: ending
        // a call from a stray Escape is a footgun, so the End
        // disc stays the only terminator besides the peer.
        if expanded && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            expanded = false;
            self.calls.media.view.expanded = false;
        }
        let card_offset = if expanded { Vec2::ZERO } else { self.calls.media.view.offset };
        // Drag deltas the header strip reports this frame,
        // applied to the stored offset once the Area closes.
        let mut drag_delta = Vec2::ZERO;
        let area = Area::new(Id::unique("call_overlay"))
            .order(egui::Order::Middle)
            .anchor(Align2::CENTER_CENTER, card_offset)
            .show(ctx, |ui| {
                crate::theme::glass_frame(t, crate::theme::RADIUS_PANEL)
                    .fill(rgb(t.bg_panel))
                    .inner_margin(Margin::same(20))
                    .show(ui, |ui| {
                        if expanded {
                            ui.set_width((window.width() - 88.0).max(336.0));
                            ui.set_min_height((window.height() - 88.0).max(0.0));
                        } else {
                            ui.set_width(336.0);
                        }
                        ui.vertical_centered(|ui| {
                            // The expand/shrink control, at
                            // the card's top-right corner: it
                            // toggles between the floating card
                            // and the in-app stage. The ended
                            // summary has no stage to offer, so
                            // it is not drawn there.
                            if call.phase != CallUiPhase::Ended {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if expand_toggle(ui, expanded, t) {
                                            action = Some("expand_toggle".to_owned());
                                        }
                                    },
                                );
                                ui.add_space(SP_2);
                            }
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
                            // A group card also carries the
                            // roster and the waiting room; the
                            // stage shrinks so the whole card
                            // still fits the window.
                            let sections_on = call.group
                                && (!call.participants.is_empty() || call.waiting.is_some());
                            if stage_on {
                                let (sw, sh) = self.calls.media.stage_size;
                                let (width, height) = if expanded {
                                    // The stage takes the height
                                    // the rest of the card leaves
                                    // in the window, at the
                                    // frame's true aspect: fit the
                                    // frame inside the available
                                    // box, never crop or stretch
                                    // it. Before the first frame
                                    // lands there is no aspect to
                                    // honor, so the box itself is
                                    // the placeholder's shape.
                                    let avail_w = ui.available_width();
                                    let avail_h = (window.height()
                                        - stage_chrome_height(&call, sections_on))
                                    .max(96.0);
                                    if sw > 0 && sh > 0 {
                                        fit_stage(sw as f32, sh as f32, avail_w, avail_h)
                                    } else {
                                        (avail_w, avail_h.min(avail_w))
                                    }
                                } else {
                                    let width = ui.available_width();
                                    let height = match media_stage {
                                        Some(ref _tex) => {
                                            if sw > 0 {
                                                let (floor, cap) = if sections_on {
                                                    (80.0, 96.0)
                                                } else {
                                                    (110.0, 224.0)
                                                };
                                                (width * sh as f32 / sw as f32).clamp(floor, cap)
                                            } else {
                                                168.0
                                            }
                                        }
                                        None => {
                                            if sections_on {
                                                96.0
                                            } else {
                                                168.0
                                            }
                                        }
                                    };
                                    (width, height)
                                };
                                // The stage's clicks are call
                                // interactions, never a drag: a
                                // double-click toggles the stage
                                // presentation, and the drag
                                // sense lives only on the header
                                // strip below.
                                let (rect, stage_resp) = ui
                                    .allocate_exact_size(Vec2::new(width, height), Sense::click());
                                if stage_resp.double_clicked() {
                                    action = Some("expand_toggle".to_owned());
                                }
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
                                        egui::FontId::proportional(12.0),
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
                            // the silhouette. With the roster
                            // sections on, the stage already
                            // carries the picture and the avatar
                            // gives its height back so the card
                            // fits the window.
                            // The header strip (avatar, title,
                            // status) is the card's drag handle:
                            // dragging any of its elements moves
                            // the floating card, so a live call
                            // can be parked in a corner while the
                            // conversation stays usable. In stage
                            // mode the card is pinned to the
                            // window and the strip drags nothing.
                            let radius = if stage_on { 20.0 } else { 32.0 };
                            if !sections_on {
                                let (rect, resp) = ui
                                    .allocate_exact_size(Vec2::splat(radius * 2.0), Sense::drag());
                                if !expanded && resp.dragged() {
                                    drag_delta += resp.drag_delta();
                                }
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
                            }
                            ui.add_space(SP_2);
                            let title_resp = ui.add(
                                egui::Label::new(
                                    RichText::new(&call.title)
                                        .font(crate::theme::semibold(19.0))
                                        .color(rgb(t.text_primary)),
                                )
                                .sense(Sense::drag()),
                            );
                            if !expanded && title_resp.dragged() {
                                drag_delta += title_resp.drag_delta();
                            }
                            ui.add_space(2.0);
                            let status_resp = ui.add(
                                egui::Label::new(
                                    RichText::new(call.status_line())
                                        .size(12.0)
                                        .color(rgb(t.text_secondary)),
                                )
                                .sense(Sense::drag()),
                            );
                            if !expanded && status_resp.dragged() {
                                drag_delta += status_resp.drag_delta();
                            }
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
                                        let mut button = egui::Button::new(
                                            RichText::new(emoji).size(crate::theme::TEXT_HEADING),
                                        )
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
                                    // The call-ended summary: the
                                    // facts the call leaves behind,
                                    // read from the same flags the
                                    // history record is built from,
                                    // so the card and the Calls log
                                    // always tell the same story.
                                    ui.add_space(SP_SECTION);
                                    ended_summary(ui, &call, t);
                                    // The backend's reason, when
                                    // it gave one, closes the
                                    // summary: it explains the
                                    // facts above it.
                                    if let Some(reason) = &call.reason {
                                        ui.add_space(SP_3);
                                        ui.label(
                                            RichText::new(reason)
                                                .size(12.0)
                                                .color(rgb(t.text_secondary)),
                                        );
                                    }
                                    ui.add_space(SP_SECTION);
                                    // Call back and Open chat act
                                    // on the call's own chat, so a
                                    // call-link call (no chat
                                    // behind it) offers neither.
                                    if !call.link && call.chat_id != 0 {
                                        if crate::widgets::button(
                                            ui,
                                            t,
                                            &crate::i18n::t("calls.call_back"),
                                            crate::widgets::Btn::Primary,
                                        )
                                        .clicked()
                                        {
                                            action = Some("callback".to_owned());
                                        }
                                        ui.add_space(SP_2);
                                        if crate::widgets::button(
                                            ui,
                                            t,
                                            &crate::i18n::t("call.open_chat"),
                                            crate::widgets::Btn::Secondary,
                                        )
                                        .clicked()
                                        {
                                            action = Some("open_chat".to_owned());
                                        }
                                        ui.add_space(SP_2);
                                    }
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
                                    // Windows has no camera capture
                                    // (the camera crate is a no-op
                                    // there), so the grid is 3 columns
                                    // and the video toggle is not
                                    // offered; other platforms keep 4.
                                    let ncols = if cfg!(target_os = "windows") { 3 } else { 4 };
                                    let end_col = if cfg!(target_os = "windows") { 2 } else { 3 };
                                    ui.columns(ncols, |cols| {
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
                                        // Windows has no camera
                                        // capture, so no video toggle
                                        // is offered: the toggle would
                                        // promise a camera that can
                                        // never open. A video call on
                                        // Windows stays receive-only
                                        // (see the camera note under
                                        // the status line).
                                        #[cfg(not(target_os = "windows"))]
                                        {
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
                                        }
                                        if call_action(
                                            &mut cols[end_col],
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
                                        ui.add_space(SP_2);
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
            });
        // Settle the card's presentation state for the next
        // frame: fold in this frame's header drag, then clamp
        // the offset against the card's real size so no drag
        // (and no window shrink) can park the card fully
        // off-window.
        if !expanded {
            let view = &mut self.calls.media.view;
            view.offset =
                clamp_card_offset(view.offset + drag_delta, area.response.rect.size(), window);
        }
        match action.as_deref() {
            Some("expand_toggle") => {
                self.calls.media.view.expanded = !self.calls.media.view.expanded;
            }
            Some("callback") => {
                // Acting dismisses the summary: clear the ended
                // card first, then start the new call on the
                // chat this call belonged to, with its kind.
                let (chat_id, video, group) = (call.chat_id, call.video, call.group);
                self.calls.active = None;
                self.calls.media = CallMediaState::default();
                self.calls.invite_open = false;
                if chat_id != 0 {
                    self.select_chat_now(chat_id);
                    if group {
                        self.start_group_call_kind(video);
                    } else {
                        self.start_call_kind(video);
                    }
                }
            }
            Some("open_chat") => {
                let chat_id = call.chat_id;
                self.calls.active = None;
                self.calls.media = CallMediaState::default();
                self.calls.invite_open = false;
                if chat_id != 0 {
                    self.select_chat_now(chat_id);
                }
            }
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
                        12.0,
                        &chat,
                        photo,
                    );
                }
                None => crate::widgets::paint_avatar(
                    ui.painter(),
                    center,
                    12.0,
                    false,
                    (crate::live::jid_id(&participant.jid) % 251) as u8,
                ),
            }
            ui.add_space(SP_2);
            ui.label(
                RichText::new(self.call_participant_name(call, &participant.jid))
                    .size(15.0)
                    .color(rgb(t.text_primary)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if participant.state != "connected" {
                    ui.label(RichText::new(&participant.state).size(12.0).color(rgb(t.text_faint)));
                }
                if participant.sharing {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), egui::Sense::hover());
                    icons::draw(
                        ui.painter(),
                        Icon::ScreenShare,
                        rect.center(),
                        icons::SIZE_BADGE,
                        rgb(t.accent),
                        rgb(t.bg_panel),
                    );
                }
                if participant.hand_raised {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), egui::Sense::hover());
                    icons::draw(
                        ui.painter(),
                        Icon::BackHand,
                        rect.center(),
                        icons::SIZE_BADGE,
                        rgb(t.accent),
                        rgb(t.bg_panel),
                    );
                }
                if let Some(emoji) = call.reactions.get(&participant.jid) {
                    ui.label(RichText::new(emoji).size(crate::theme::TEXT_BODY));
                }
            });
        });
        ui.add_space(2.0);
    }

    /// Fetches the call chat's member list when the invite
    /// picker needs it and it is not on hand, the same
    /// fetch-once discipline as the profile panel.
    pub(crate) fn ensure_call_members(&mut self) {
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
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .fixed_size(Vec2::new(320.0, 380.0))
            .frame(crate::theme::popup_frame(t))
            .show(ctx, |ui| {
                // Title row in the converged modal anatomy:
                // title at the body step, shared icon close at
                // the right edge, replacing the old text Close
                // button at the dialog's foot.
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(crate::i18n::t("call.add_people"))
                            .font(crate::theme::semibold(15.0))
                            .color(rgb(t.text_primary)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if crate::widgets::icon_button(
                            ui,
                            t,
                            crate::icons::Icon::X,
                            28.0,
                            &crate::i18n::t("common.close"),
                            None,
                        )
                        .clicked()
                        {
                            close = true;
                        }
                    });
                });
                ui.add_space(SP_2);
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
                        crate::widgets::show_scroll_area(
                            ui,
                            "call_members",
                            egui::ScrollArea::vertical().max_height(280.0),
                            |ui| {
                                for member in rows {
                                    let (rect, response) = ui.allocate_exact_size(
                                        Vec2::new(ui.available_width(), 44.0),
                                        Sense::click(),
                                    );
                                    let hov = crate::anim::hover_factor(
                                        ui,
                                        ("call_invite_row", member.address.as_str()),
                                        response.hovered(),
                                    );
                                    let press = crate::anim::press_factor(
                                        ui,
                                        ("call_invite_row", member.address.as_str()),
                                        response.is_pointer_button_down_on(),
                                    );
                                    if hov > 0.001 {
                                        ui.painter().rect_filled(
                                            rect,
                                            crate::theme::RADIUS_MICRO,
                                            crate::theme::glass_wash(
                                                t,
                                                t.bg_panel_alt,
                                                (hov * 115.0) as u8,
                                            ),
                                        );
                                    }
                                    if press > 0.001 {
                                        ui.painter().rect_filled(
                                            rect,
                                            crate::theme::RADIUS_MICRO,
                                            crate::theme::glass_wash(
                                                t,
                                                t.bg_panel_alt,
                                                (press * 170.0) as u8,
                                            ),
                                        );
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
                                        egui::FontId::proportional(15.0),
                                        rgb(t.text_primary),
                                    );
                                    if response.clicked() {
                                        invite = Some(member.address.clone());
                                    }
                                }
                            },
                        );
                    }
                    None => {
                        ui.label(
                            RichText::new(crate::i18n::t("call.loading_members"))
                                .size(12.0)
                                .color(rgb(t.text_secondary)),
                        );
                    }
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
    use super::{
        clamp_card_offset, fit_stage, format_duration, CallOutcome, CallShape, CallUi, CallUiPhase,
    };
    use eframe::egui::{Pos2, Rect, Vec2};

    #[test]
    fn durations_format_like_a_call_timer() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(65), "1:05");
        assert_eq!(format_duration(3599), "59:59");
        assert_eq!(format_duration(3661), "1:01:01");
    }

    /// A call in its ended state with the given answer flags,
    /// for the outcome-derivation pins.
    fn ended_call(
        outgoing: bool,
        connected: bool,
        declined: bool,
        ended_elsewhere: bool,
    ) -> CallUi {
        let mut call = CallUi::fresh(
            "call-1",
            7,
            "Peer".to_owned(),
            false,
            CallUiPhase::Ended,
            CallShape { outgoing, group: false, link: false },
        );
        call.connected = connected;
        call.declined = declined;
        call.ended_elsewhere = ended_elsewhere;
        call
    }

    #[test]
    fn outcome_connected_whenever_the_call_reached_active() {
        // Connected outranks every other flag: a call that was
        // answered elsewhere after connecting here still has a
        // duration, and a declined flag set after an accept
        // (the F18 latch race) cannot rewrite it.
        assert_eq!(ended_call(false, true, false, false).outcome(), CallOutcome::Connected);
        assert_eq!(ended_call(true, true, false, false).outcome(), CallOutcome::Connected);
        assert_eq!(ended_call(false, true, true, true).outcome(), CallOutcome::Connected);
    }

    #[test]
    fn outcome_declined_and_answered_elsewhere_are_answers() {
        assert_eq!(ended_call(false, false, true, false).outcome(), CallOutcome::Declined);
        assert_eq!(ended_call(false, false, false, true).outcome(), CallOutcome::AnsweredElsewhere);
        // A local decline and an elsewhere answer are both
        // answers, never missed calls.
        assert_eq!(ended_call(false, false, true, true).outcome(), CallOutcome::Declined);
    }

    #[test]
    fn outcome_missed_is_incoming_and_unanswered_only() {
        assert_eq!(ended_call(false, false, false, false).outcome(), CallOutcome::Missed);
        assert_eq!(ended_call(true, false, false, false).outcome(), CallOutcome::NoAnswer);
    }

    #[test]
    fn history_record_missed_flag_matches_the_outcome() {
        // The summary and the Calls log read one derivation:
        // the record's missed flag is set exactly for the
        // Missed outcome, across the flag combinations.
        for outgoing in [false, true] {
            for connected in [false, true] {
                for declined in [false, true] {
                    for elsewhere in [false, true] {
                        let call = ended_call(outgoing, connected, declined, elsewhere);
                        let record = call.history_record("peer@s.whatsapp.net", 42);
                        assert_eq!(record.missed, call.outcome() == CallOutcome::Missed);
                        // Duration is kept only for a connected
                        // call, whichever other flags are set.
                        assert_eq!(record.duration_secs, if connected { 42 } else { 0 });
                    }
                }
            }
        }
    }

    #[test]
    fn card_offset_clamp_keeps_a_grabbable_slice_on_screen() {
        let window = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0));
        let card = Vec2::new(376.0, 500.0);
        // A modest drag passes through untouched.
        assert_eq!(
            clamp_card_offset(Vec2::new(100.0, -50.0), card, window),
            Vec2::new(100.0, -50.0)
        );
        // A wild drag stops with 64 px of card still inside on
        // the axis it was dragged along.
        let clamped = clamp_card_offset(Vec2::new(5000.0, -5000.0), card, window);
        assert_eq!(clamped, Vec2::new(724.0, -586.0));
    }

    #[test]
    fn stage_fit_keeps_the_frame_whole_at_its_aspect() {
        // A landscape frame in a tall box is width-bound.
        assert_eq!(fit_stage(1280.0, 720.0, 800.0, 600.0), (800.0, 450.0));
        // A portrait frame in a wide box is height-bound.
        let (w, h) = fit_stage(720.0, 1280.0, 1200.0, 500.0);
        assert!((h - 500.0).abs() < 0.01 && (w - 281.25).abs() < 0.01);
    }
}
