//! Call overlay: the incoming-call prompt and the in-call
//! controls, driven by backend call events. One call at a time,
//! matching the backend's contract.

use backend::BackendCommand;
use eframe::egui::{self, Align2, Area, Id, Margin, RichText, Sense, Vec2};

use crate::icons::{self, Icon};
use crate::state::{App, Notice};
use crate::theme::rgb;

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
}

impl CallUi {
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
                } else {
                    crate::i18n::t("call.incoming_voice")
                }
            }
            CallUiPhase::Ringing => crate::i18n::t("call.ringing"),
            CallUiPhase::Connecting => crate::i18n::t("connect.status_connecting"),
            CallUiPhase::Active => format_duration(self.seconds),
            CallUiPhase::Ended => self.reason.clone().unwrap_or_else(|| crate::i18n::t("call.ended")),
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

impl App {
    /// The header phone button: starts a voice call to the open
    /// chat (live and test mode only).
    pub(crate) fn start_call_from_header(&mut self) {
        self.start_call_kind(false);
    }

    /// Starts a call of the given kind to the open chat (live
    /// and test mode only). The one start path every call entry
    /// point shares: the conversation header, the call log's
    /// call-back action and the new call picker's per-row voice
    /// and video actions all land here, so the kind the user
    /// picked is the kind the backend is asked for.
    pub(crate) fn start_call_kind(&mut self, video: bool) {
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
                self.send_backend(BackendCommand::StartCall { chat_jid: jid, video });
            }
            None => {
                self.notice = Some(Notice::info("Open a real chat to start a call.".to_owned()));
            }
        }
    }

    /// Backend reports a ringing incoming call.
    pub(crate) fn on_incoming_call(&mut self, call_id: &str, chat_id: u64, video: bool) {
        if self.calls.active.is_some() {
            return;
        }
        let title = self
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .map_or_else(|| crate::i18n::t("call.unknown_caller"), |c| c.title.clone());
        self.calls.active = Some(CallUi {
            call_id: call_id.to_owned(),
            chat_id,
            title,
            video,
            phase: CallUiPhase::Incoming,
            seconds: 0,
            muted: false,
            speaker: false,
            reason: None,
            ended_at: None,
            outgoing: false,
            started_at: crate::state::now_unix() as i64,
            connected: false,
            declined: false,
            ended_elsewhere: false,
            action_pending: false,
        });
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
            let title = self
                .chats
                .iter()
                .find(|c| c.id == chat_id)
                .map_or_else(|| crate::i18n::t("call.unknown"), |c| c.title.clone());
            self.calls.active = Some(CallUi {
                call_id: call_id.to_owned(),
                chat_id,
                title,
                video: self.calls.outgoing_video,
                phase: CallUiPhase::Ringing,
                seconds: 0,
                muted: false,
                speaker: false,
                reason: None,
                ended_at: None,
                outgoing: true,
                started_at: crate::state::now_unix() as i64,
                connected: false,
                declined: false,
                ended_elsewhere: false,
                action_pending: false,
            });
            // The remembered kind has been adopted; the next
            // outgoing call sets it again when it starts.
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
        if call.chat_id != chat_id && call.phase != CallUiPhase::Incoming {
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

    /// Draws the call card over the conversation, if a call is in
    /// any phase. Ended cards dismiss themselves after a few
    /// seconds (or on tap).
    pub(crate) fn draw_call_overlay(&mut self, ctx: &egui::Context) {
        let Some(call) = self.calls.active.clone() else {
            return;
        };
        if call.phase == CallUiPhase::Ended
            && call.ended_at.is_some_and(|at| at.elapsed().as_secs() >= 4)
        {
            self.calls.active = None;
            return;
        }
        let t = self.tokens;
        let mut action: Option<&'static str> = None;
        Area::new(Id::unique("call_overlay")).anchor(Align2::CENTER_CENTER, Vec2::ZERO).show(
            ctx,
            |ui| {
                // The card floats over the conversation, so it
                // carries the frame treatment every other floating
                // surface carries (the shared glass frame's stroke
                // and shadow), keeping its own panel fill.
                let card = crate::theme::glass_frame(t, 16)
                    .fill(rgb(t.bg_panel))
                    .inner_margin(Margin::symmetric(24, 20))
                    .show(ui, |ui| {
                        // One fixed content width, so the identity
                        // block and the action row compose on a
                        // single axis (design standard section 19)
                        // whatever the caller's name or the phase.
                        ui.set_min_width(CALL_CARD_W);
                        ui.vertical_centered(|ui| {
                            // The caller's identity exactly as the
                            // chat list and the conversation header
                            // paint it: the shared avatar dispatch
                            // for the call's chat, so the card and
                            // the list behind it show one person.
                            // A chat the roster does not know falls
                            // back to the shared silhouette avatar.
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
                            match self.chats.iter().find(|c| c.id == call.chat_id).cloned() {
                                Some(chat) => {
                                    let photo = self.chat_avatar_photo(ui.ctx(), &chat);
                                    crate::widgets::paint_chat_avatar_photo(
                                        ui.painter(),
                                        rect.center(),
                                        32.0,
                                        &chat,
                                        photo,
                                    );
                                }
                                None => crate::widgets::paint_avatar(
                                    ui.painter(),
                                    rect.center(),
                                    32.0,
                                    false,
                                    (call.chat_id % 251) as u8,
                                ),
                            }
                            ui.add_space(12.0);
                            ui.label(
                                RichText::new(&call.title)
                                    .font(crate::theme::semibold(17.0))
                                    .color(rgb(t.text_primary)),
                            );
                            ui.label(
                                RichText::new(call.status_line())
                                    .size(13.0)
                                    .color(rgb(t.text_secondary)),
                            );
                            ui.add_space(20.0);
                            match call.phase {
                                CallUiPhase::Incoming => {
                                    // Decline and Accept as a
                                    // symmetric pair of labeled
                                    // actions in equal cells.
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
                                            action = Some("reject");
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
                                            action = Some("accept");
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
                                        action = Some("close");
                                    }
                                }
                                _ => {
                                    // The in-call controls as an
                                    // even triple of labeled
                                    // actions in equal cells.
                                    ui.columns(3, |cols| {
                                        let mute_icon =
                                            if call.muted { Icon::MicOff } else { Icon::Mic };
                                        if call_action(
                                            &mut cols[0],
                                            mute_icon,
                                            rgb(t.bg_panel_alt),
                                            rgb(t.text_primary),
                                            &if call.muted { crate::i18n::t("common.unmute") } else { crate::i18n::t("common.mute") },
                                            t,
                                            call.action_pending,
                                        ) {
                                            action = Some("mute");
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
                                            action = Some("speaker");
                                        }
                                        if call_action(
                                            &mut cols[2],
                                            Icon::CallEnd,
                                            rgb(t.danger),
                                            rgb(t.on_danger()),
                                            &crate::i18n::t("call.end"),
                                            t,
                                            call.action_pending,
                                        ) {
                                            action = Some("end");
                                        }
                                    });
                                }
                            }
                        });
                    });
                crate::theme::paint_glass_edge(ui.painter(), card.response.rect, 16.0, t);
            },
        );
        match action {
            Some("accept") => self.accept_active_call(),
            Some("reject") => self.reject_active_call(),
            Some("end") => self.end_active_call(),
            Some("mute") => {
                if let Some(call) = &mut self.calls.active {
                    call.muted = !call.muted;
                    let muted = call.muted;
                    self.send_backend(BackendCommand::SetCallMuted { muted });
                }
            }
            Some("speaker") => {
                if let Some(call) = &mut self.calls.active {
                    call.speaker = !call.speaker;
                    let speaker = call.speaker;
                    self.send_backend(BackendCommand::SetCallSpeaker { speaker });
                }
            }
            Some("close") => self.calls.active = None,
            _ => {}
        }
    }
}

/// The call card's content width: the identity block and the
/// action row share this one axis (design standard section 19),
/// so the card's composition never shifts with the caller's name
/// or the call's phase.
const CALL_CARD_W: f32 = 264.0;

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
