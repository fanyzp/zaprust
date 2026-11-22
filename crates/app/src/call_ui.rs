//! Call overlay: the incoming-call prompt and the in-call
//! controls, driven by backend call events. One call at a time,
//! matching the backend's contract.

use backend::BackendCommand;
use eframe::egui::{self, Align2, Area, Id, Margin, RichText, Sense, Vec2};

use crate::icons::{self, Icon};
use crate::state::App;
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
                    "Incoming video call".to_owned()
                } else {
                    "Incoming voice call".to_owned()
                }
            }
            CallUiPhase::Ringing => "Ringing…".to_owned(),
            CallUiPhase::Connecting => "Connecting…".to_owned(),
            CallUiPhase::Active => format_duration(self.seconds),
            CallUiPhase::Ended => self.reason.clone().unwrap_or_else(|| "Call ended".to_owned()),
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
            self.notice =
                Some("Calls work in live mode and test mode, not in the offline demo.".to_owned());
            return;
        }
        if self.calls.active.is_some() {
            self.notice = Some("A call is already in progress.".to_owned());
            return;
        }
        match self.live_jid(self.selected_chat) {
            Some(jid) => {
                self.calls.outgoing_video = video;
                self.send_backend(BackendCommand::StartCall { chat_jid: jid, video });
            }
            None => {
                self.notice = Some("Open a real chat to start a call.".to_owned());
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
            .map_or_else(|| "Unknown caller".to_owned(), |c| c.title.clone());
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
                .map_or_else(|| "Unknown".to_owned(), |c| c.title.clone());
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
            "Answered on another device".to_owned()
        } else {
            "Declined on another device".to_owned()
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
                    .inner_margin(Margin::symmetric(28, 22))
                    .show(ui, |ui| {
                        ui.set_min_width(280.0);
                        ui.vertical_centered(|ui| {
                            // Avatar disc with the title initial.
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
                            ui.painter().circle_filled(rect.center(), 32.0, rgb(t.accent));
                            let initial =
                                call.title.chars().next().unwrap_or('?').to_uppercase().to_string();
                            ui.painter().text(
                                rect.center(),
                                Align2::CENTER_CENTER,
                                initial,
                                egui::FontId::proportional(26.0),
                                egui::Color32::WHITE,
                            );
                            ui.add_space(10.0);
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
                            ui.add_space(16.0);
                            ui.horizontal(|ui| {
                                match call.phase {
                                    CallUiPhase::Incoming => {
                                        if call_button(
                                            ui,
                                            Icon::Phone,
                                            rgb(t.danger),
                                            "Decline",
                                            call.action_pending,
                                        ) {
                                            action = Some("reject");
                                        }
                                        ui.add_space(18.0);
                                        if call_button(
                                            ui,
                                            Icon::Phone,
                                            rgb(t.accent),
                                            "Accept",
                                            call.action_pending,
                                        ) {
                                            action = Some("accept");
                                        }
                                    }
                                    CallUiPhase::Ended => {
                                        if crate::widgets::button(
                                            ui,
                                            t,
                                            "Close",
                                            crate::widgets::Btn::Secondary,
                                        )
                                        .clicked()
                                        {
                                            action = Some("close");
                                        }
                                    }
                                    _ => {
                                        let mute_icon =
                                            if call.muted { Icon::MicOff } else { Icon::Mic };
                                        if call_button(
                                            ui,
                                            mute_icon,
                                            rgb(t.bg_panel_alt),
                                            if call.muted { "Unmute" } else { "Mute" },
                                            call.action_pending,
                                        ) {
                                            action = Some("mute");
                                        }
                                        ui.add_space(10.0);
                                        if call_button(
                                            ui,
                                            Icon::Megaphone,
                                            rgb(t.bg_panel_alt),
                                            if call.speaker { "Speaker on" } else { "Speaker off" },
                                            call.action_pending,
                                        ) {
                                            action = Some("speaker");
                                        }
                                        ui.add_space(10.0);
                                        // Video is a later sub-phase;
                                        // the button is honest about
                                        // it instead of pretending.
                                        call_button_disabled(
                                            ui,
                                            t,
                                            Icon::Video,
                                            "Video calls are not available yet",
                                        );
                                        ui.add_space(10.0);
                                        if call_button(
                                            ui,
                                            Icon::Phone,
                                            rgb(t.danger),
                                            "End call",
                                            call.action_pending,
                                        ) {
                                            action = Some("end");
                                        }
                                    }
                                }
                            });
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

/// One round call-control button; returns true on click. While
/// an action is latched (the `pending` argument, fed from the
/// call's action_pending flag) the disc and its glyph paint
/// dimmed, so the latch the click handling enforces is visible
/// instead of silently swallowing further clicks.
fn call_button(
    ui: &mut egui::Ui,
    icon: Icon,
    bg: egui::Color32,
    tooltip: &str,
    pending: bool,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(52.0), Sense::click());
    let dim = |c: egui::Color32| {
        egui::Color32::from_rgba_unmultiplied(
            c.r(),
            c.g(),
            c.b(),
            (f32::from(c.a()) * 0.55) as u8,
        )
    };
    let (bg, glyph) =
        if pending { (dim(bg), dim(egui::Color32::WHITE)) } else { (bg, egui::Color32::WHITE) };
    ui.painter().circle_filled(rect.center(), 26.0, bg);
    icons::draw(ui.painter(), icon, rect.center(), crate::icons::SIZE_TAB, glyph, bg);
    response.on_hover_text(tooltip).clicked()
}

/// The disabled variant (greyed, hover-only, explains itself).
fn call_button_disabled(ui: &mut egui::Ui, t: core::theme::Tokens, icon: Icon, tooltip: &str) {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(52.0), Sense::hover());
    let bg = rgb(t.bg_panel_alt);
    ui.painter().circle_filled(rect.center(), 26.0, bg);
    icons::draw(
        ui.painter(),
        icon,
        rect.center(),
        crate::icons::SIZE_TAB,
        rgb(t.text_secondary),
        bg,
    );
    response.on_hover_text(tooltip);
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
