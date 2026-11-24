//! Debug mode UI: the always-visible banner, the inspector
//! screen, trace recording, and the bundle export dialog.
//!
//! Active only with `--debug`. Everything recorded passes
//! through `core::debug::redact` at the ring's choke point;
//! bodies exist only in the local ring and leave the machine
//! solely inside an explicitly opted-in bundle.

use crate::state::{App, Notice, Screen};
use crate::theme::{SP_1_5, SP_2, SP_2_5};
use backend::BackendEvent;
use core::debug;
use eframe::egui::{self, RichText};

impl App {
    /// Records one trace entry (no-op unless debug mode is on)
    /// and mirrors it to the tracing log file.
    pub(crate) fn trace_push(&mut self, stage: &str, summary: &str, body: Option<String>) {
        if !self.debug.mode {
            return;
        }
        let at_ms = self.debug.flow_start.elapsed().as_millis() as u64;
        let redacted = debug::redact(summary);
        tracing::info!(stage, at_ms, "{}", redacted);
        self.debug.trace.push(at_ms, stage, summary, None, body);
    }

    /// Records a backend event into the trace (stage + summary;
    /// message bodies attached for the local inspector only).
    /// Summaries identify chats and senders by JID only, never by
    /// display name or chat title: the ring is mirrored to the
    /// debug log file and exported in bundles, and
    /// `debug::redact` masks the phone digits inside JIDs but
    /// cannot mask a name (privacy audit C7).
    pub(crate) fn trace_event(&mut self, event: &BackendEvent) {
        if !self.debug.mode {
            return;
        }
        let (stage, summary, body): (&str, String, Option<String>) = match event {
            BackendEvent::QrCode { .. } => {
                ("backend", "QrCode received (payload redacted)".to_owned(), None)
            }
            BackendEvent::QrExpired => ("backend", "QrExpired".to_owned(), None),
            BackendEvent::PairCode { .. } => {
                ("backend", "PairCode ready (code redacted)".to_owned(), None)
            }
            BackendEvent::Connected { .. } => ("backend", "Connected".to_owned(), None),
            BackendEvent::SyncProgress { percent } => (
                "backend",
                match percent {
                    Some(p) => format!("SyncProgress {p}%"),
                    None => "SyncProgress".to_owned(),
                },
                None,
            ),
            BackendEvent::SyncCompleted => ("backend", "SyncCompleted".to_owned(), None),
            BackendEvent::ChatRenamed { .. } => ("backend", "ChatRenamed".to_owned(), None),
            BackendEvent::SenderNamed { .. } => ("backend", "SenderNamed".to_owned(), None),
            BackendEvent::OwnProfile(_) => {
                ("backend", "OwnProfile received (values redacted)".to_owned(), None)
            }
            BackendEvent::OwnProfileSaveFailed { .. } => {
                ("backend", "OwnProfileSaveFailed".to_owned(), None)
            }
            BackendEvent::LoggedOut => ("backend", "LoggedOut".to_owned(), None),
            BackendEvent::TemporaryBan { code } => {
                ("backend", format!("TemporaryBan {code}"), None)
            }
            BackendEvent::ConnectionLost { reason } => {
                ("backend", format!("ConnectionLost: {reason}"), None)
            }
            BackendEvent::ChatUpsert(chat) => {
                ("archive", format!("ChatUpsert {}", chat.jid), None)
            }
            BackendEvent::CommunityCreated { jid, .. } => {
                ("backend", format!("CommunityCreated {jid} (name redacted)"), None)
            }
            BackendEvent::MessageUpsert(message) => (
                "message",
                format!(
                    "MessageUpsert in {} from {}",
                    message.chat_jid,
                    message
                        .sender_jid
                        .as_deref()
                        .unwrap_or(if message.from_me { "me" } else { "unknown" })
                ),
                Some(message.body.clone()),
            ),
            BackendEvent::MessageSent { chat_jid, .. } => {
                ("backend", format!("MessageSent to {chat_jid}"), None)
            }
            BackendEvent::Receipt { chat_jid, kind, .. } => {
                ("receipt", format!("Receipt {kind:?} for {chat_jid}"), None)
            }
            BackendEvent::ReactionUpdate { emoji, .. } => {
                ("message", format!("ReactionUpdate {emoji}"), None)
            }
            BackendEvent::ReactionFailed { message, .. } => {
                ("message", format!("ReactionFailed {message}"), None)
            }
            BackendEvent::StarFailed { message, .. } => {
                ("message", format!("StarFailed {message}"), None)
            }
            BackendEvent::PinFailed { message, .. } => {
                ("message", format!("PinFailed {message}"), None)
            }
            BackendEvent::StatusPosted { .. } => {
                ("message", "StatusPosted (text redacted)".to_owned(), None)
            }
            BackendEvent::StatusPostFailed { message, .. } => {
                ("message", format!("StatusPostFailed {message}"), None)
            }
            BackendEvent::PollVote { chat_jid, selected, .. } => (
                "message",
                format!("PollVote in {chat_jid}: {} option(s) selected", selected.len()),
                None,
            ),
            BackendEvent::MessageEdited { .. } => ("message", "MessageEdited".to_owned(), None),
            BackendEvent::MessageRevoked { .. } => ("message", "MessageRevoked".to_owned(), None),
            BackendEvent::DisappearingChanged { chat_jid, duration_secs } => {
                ("message", format!("DisappearingChanged in {chat_jid}: {duration_secs}s"), None)
            }
            BackendEvent::DefaultDisappearingTimerChanged { duration_secs } => (
                "message",
                format!("DefaultDisappearingTimerChanged: {duration_secs}s"),
                None,
            ),
            BackendEvent::MuteChanged { chat_jid, muted, .. } => {
                ("message", format!("MuteChanged in {chat_jid}: muted={muted}"), None)
            }
            BackendEvent::PinChanged { chat_jid, pinned } => {
                ("message", format!("PinChanged in {chat_jid}: pinned={pinned}"), None)
            }
            BackendEvent::ArchiveChanged { chat_jid, archived } => {
                ("message", format!("ArchiveChanged in {chat_jid}: archived={archived}"), None)
            }
            BackendEvent::StarChanged { chat_jid, message_id, starred } => (
                "message",
                format!("StarChanged in {chat_jid}: {message_id} starred={starred}"),
                None,
            ),
            BackendEvent::ChatReadChanged { chat_jid, read, unread_count } => (
                "message",
                format!("ChatReadChanged in {chat_jid}: read={read} unread={unread_count}"),
                None,
            ),
            BackendEvent::ChatDeleted { chat_jid } => {
                ("message", format!("ChatDeleted {chat_jid}"), None)
            }
            BackendEvent::ChatCleared { chat_jid, delete_starred, last_message_timestamp } => (
                "message",
                format!(
                    "ChatCleared in {chat_jid}: delete_starred={delete_starred} bound={last_message_timestamp:?}"
                ),
                None,
            ),
            BackendEvent::MessageDeletedForMe { chat_jid, message_id } => (
                "message",
                format!("MessageDeletedForMe in {chat_jid}: {message_id}"),
                None,
            ),
            BackendEvent::PrivacyState(_) => ("backend", "PrivacyState".to_owned(), None),
            BackendEvent::Blocklist(jids) => {
                ("backend", format!("Blocklist: {} contacts", jids.len()), None)
            }
            BackendEvent::Crashed { summary, restarted } => {
                ("backend", format!("Crashed (restarted={restarted}): {summary}"), None)
            }
            BackendEvent::MediaSaved { chat_jid, path, .. } => {
                ("backend", format!("MediaSaved for {chat_jid} -> {path}"), None)
            }
            BackendEvent::PhotoBytes { chat_jid, bytes, .. } => {
                ("backend", format!("PhotoBytes for {chat_jid}: {} bytes", bytes.len()), None)
            }
            BackendEvent::PhotoFetchFailed { chat_jid, message, .. } => {
                ("backend", format!("PhotoFetchFailed for {chat_jid}: {message}"), None)
            }
            BackendEvent::VoiceBytes { chat_jid, bytes, .. } => {
                ("backend", format!("VoiceBytes for {chat_jid}: {} bytes", bytes.len()), None)
            }
            BackendEvent::VoiceFetchFailed { chat_jid, message, .. } => {
                ("backend", format!("VoiceFetchFailed for {chat_jid}: {message}"), None)
            }
            BackendEvent::VideoBytes { chat_jid, bytes, .. } => {
                ("backend", format!("VideoBytes for {chat_jid}: {} bytes", bytes.len()), None)
            }
            BackendEvent::VideoReady { chat_jid, path, .. } => {
                ("backend", format!("VideoReady for {chat_jid} -> {path}"), None)
            }
            BackendEvent::VideoFetchFailed { chat_jid, message, .. } => {
                ("backend", format!("VideoFetchFailed for {chat_jid}: {message}"), None)
            }
            BackendEvent::TypingReceived { chat_jid, typing } => {
                ("backend", format!("TypingReceived {chat_jid} typing={typing}"), None)
            }
            BackendEvent::PresenceReceived { chat_jid, online, last_seen } => {
                (
                    "backend",
                    format!("PresenceReceived {chat_jid} online={online} last_seen={last_seen:?}"),
                    None,
                )
            }
            BackendEvent::GroupMembers { chat_jid, members, .. } => {
                ("backend", format!("GroupMembers {chat_jid}: {} members", members.len()), None)
            }
            BackendEvent::GroupInviteLink { chat_jid, .. } => {
                ("backend", format!("GroupInviteLink {chat_jid} (link redacted)"), None)
            }
            BackendEvent::GroupApprovalState { chat_jid, enabled } => {
                ("backend", format!("GroupApprovalState {chat_jid}: {enabled}"), None)
            }
            BackendEvent::GroupModesState { chat_jid, announce, locked } => (
                "backend",
                format!("GroupModesState {chat_jid}: announce={announce} locked={locked}"),
                None,
            ),
            BackendEvent::JoinRequests { chat_jid, requests } => {
                ("backend", format!("JoinRequests {chat_jid}: {} pending", requests.len()), None)
            }
            BackendEvent::Error { message } => ("backend", format!("Error: {message}"), None),
            BackendEvent::IncomingCall { call_id, chat_jid, video } => (
                "backend",
                format!("IncomingCall {call_id} from {chat_jid} (video: {video})"),
                None,
            ),
            BackendEvent::CallChanged { call_id, phase, seconds, .. } => {
                ("backend", format!("CallChanged {call_id} {phase:?} at {seconds}s"), None)
            }
            BackendEvent::CallMissed { call_id, chat_jid, at } => {
                ("backend", format!("CallMissed {call_id} from {chat_jid} at {at}"), None)
            }
            BackendEvent::CallEndedElsewhere { call_id, chat_jid, answered, at } => (
                "backend",
                format!("CallEndedElsewhere {call_id} from {chat_jid} answered={answered} at {at}"),
                None,
            ),
            // Media, avatar, sync and call-history events: the
            // trace names them without payloads (bytes and JIDs
            // stay out of the exported log).
            other => (
                "backend",
                format!("Event {:?}", std::mem::discriminant(other)),
                None,
            ),
        };
        self.trace_push(stage, &summary, body);
    }

    /// Deterministic extra entries for the FakeBackend debug
    /// screenshot: gate decisions and one memory retrieval,
    /// clearly labeled as scripted.
    pub(crate) fn trace_fake_script(&mut self) {
        if !self.debug.mode {
            return;
        }
        self.trace_push(
            "gate",
            "(fake script) send_message -> NeedsConfirmation -> confirmed by user",
            None,
        );
        self.trace_push(
            "gate",
            "(fake script) summarize_chat -> Allow (capability read-chat)",
            None,
        );
        self.trace_push(
            "memory",
            "(fake script) retrieval: 2 hits, m1717 score 0.61, m1718 score 0.44",
            None,
        );
    }

    /// The DEBUG banner strip (top of the window, always visible
    /// while debug mode is on) with inspector + export actions.
    pub(crate) fn draw_debug_banner(&mut self, root: &mut egui::Ui) {
        let t = self.tokens;
        let mut open_inspector = false;
        let mut open_export = false;
        egui::Panel::top("debug_banner")
            .exact_size(30.0)
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    // A painted dot, not a text bullet glyph.
                    let (dot, _) = ui.allocate_exact_size(
                        egui::Vec2::splat(8.0),
                        egui::Sense::hover(),
                    );
                    ui.painter().circle_filled(
                        dot.center(),
                        4.0,
                        egui::Color32::from_rgb(0xE0, 0x6C, 0x50),
                    );
                    ui.label(
                        RichText::new("DEBUG MODE")
                            .size(12.0)
                            .strong()
                            .color(egui::Color32::from_rgb(0xE0, 0x6C, 0x50)),
                    );
                    ui.label(
                        RichText::new(
                            "tracing to data dir debug/ · bodies stay local unless exported with opt-in",
                        )
                        .size(10.0),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if crate::widgets::button(ui, t, "Export bundle…", crate::widgets::Btn::Primary).clicked() {
                            open_export = true;
                        }
                        if crate::widgets::button(ui, t, "Inspector", crate::widgets::Btn::Secondary).clicked() {
                            open_inspector = true;
                        }
                    });
                });
            });
        if open_inspector {
            self.screen = Screen::Debug;
        }
        if open_export {
            self.debug.export_open = true;
        }
        if self.debug.export_open {
            self.draw_debug_export(root);
        }
    }

    /// The inspector screen: connection, event stream, message
    /// timeline, gate decisions, scheduler, memory hits, each
    /// with ms-since-start timing from the shared trace ring.
    pub(crate) fn draw_debug_screen(&mut self, root: &mut egui::Ui) {
        egui::CentralPanel::default().show(root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Debug inspector");
                let status = if self.live_mode {
                    match &self.live {
                        Some(s) => format!("live · {:?}", s.status),
                        None => "live · starting".to_owned(),
                    }
                } else {
                    "demo mode".to_owned()
                };
                ui.label(RichText::new(format!("Connection: {status}")).size(12.0));
                let animating = usize::from(self.anim.picker.is_some())
                    + usize::from(self.anim.chat_fade.is_some())
                    + usize::from(self.anim.send_pulse.is_some());
                ui.label(
                    RichText::new(format!(
                        "Motion: {} animations started this session, {animating} running now. \
                         Idle frames request no repaints.",
                        self.anim.started
                    ))
                    .size(12.0),
                );
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Reveal message bodies (local only)").size(12.0));
                    let mut v = self.debug.reveal_bodies;
                    if ui.checkbox(&mut v, "").changed() {
                        self.debug.reveal_bodies = v;
                    }
                });
                ui.add_space(SP_2_5);
                self.trace_section(ui, "Event stream", &["backend", "archive", "ui"]);
                self.trace_section(ui, "Messages & receipts", &["message", "receipt"]);
                self.trace_section(ui, "Gate & tool calls", &["gate"]);
                self.trace_section(ui, "Scheduler", &["scheduler"]);
                self.trace_section(ui, "Memory retrieval", &["memory"]);
            });
        });
    }

    fn trace_section(&self, ui: &mut egui::Ui, title: &str, stages: &[&str]) {
        ui.label(RichText::new(title).size(12.0).strong());
        let mut shown = 0;
        for entry in self.debug.trace.entries().iter().rev() {
            if stages.contains(&entry.stage.as_str()) {
                shown += 1;
                if shown > 30 {
                    break;
                }
                let mut line = format!(
                    "#{} +{}ms [{}] {}",
                    entry.seq, entry.at_ms, entry.stage, entry.summary
                );
                if let Some(d) = entry.duration_ms {
                    line.push_str(&format!(" ({d}ms)"));
                }
                ui.label(RichText::new(line).size(10.0));
                if self.debug.reveal_bodies {
                    if let Some(body) = &entry.body {
                        ui.label(RichText::new(format!("    body: {body}")).size(10.0));
                    }
                }
            }
        }
        if shown == 0 {
            ui.label(RichText::new("(no entries yet)").size(10.0));
        }
        ui.add_space(SP_2);
    }

    /// The export-bundle dialog: states exactly what is included,
    /// bodies opt-in via a separate checkbox with a warning, and
    /// never uploads anything anywhere.
    fn draw_debug_export(&mut self, root: &mut egui::Ui) {
        let t = self.tokens;
        let mut close = false;
        let mut do_export = false;
        egui::Window::new("Export debug bundle")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(root.ctx(), |ui| {
                ui.set_width(430.0);
                ui.label(
                    "The bundle contains: the redacted debug logs, the event \
                     timeline (JSON), app/build info, and the latest crash \
                     report if one exists. It is written to the app data \
                     dir under debug/, nothing is uploaded anywhere; attach \
                     it to a GitHub issue yourself if you want to share it.",
                );
                ui.add_space(SP_1_5);
                let mut include = self.debug.include_bodies;
                if ui.checkbox(&mut include, "Include message bodies in the timeline").changed() {
                    self.debug.include_bodies = include;
                }
                if include {
                    ui.label(
                        RichText::new(
                            "Warning: bodies are your private conversations. \
                             Only include them for your own debugging.",
                        )
                        .size(12.0)
                        .color(egui::Color32::from_rgb(0xE0, 0x6C, 0x50)),
                    );
                }
                ui.horizontal(|ui| {
                    if crate::widgets::button(ui, t, "Export bundle", crate::widgets::Btn::Primary)
                        .clicked()
                    {
                        do_export = true;
                    }
                    if crate::widgets::button(ui, t, "Cancel", crate::widgets::Btn::Secondary)
                        .clicked()
                    {
                        close = true;
                    }
                });
            });
        if do_export {
            self.export_debug_bundle();
            close = true;
        }
        if close {
            self.debug.export_open = false;
            self.debug.include_bodies = false;
        }
    }

    fn export_debug_bundle(&mut self) {
        let dir = backend::data_dir().join("debug");
        let _ = std::fs::create_dir_all(&dir);
        // Current debug logs (contents redacted again in builder).
        let mut logs: Vec<(String, String)> = Vec::new();
        if let Ok(read) = std::fs::read_dir(&dir) {
            for entry in read.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".log") {
                    if let Ok(text) = std::fs::read_to_string(entry.path()) {
                        logs.push((name, text));
                    }
                }
            }
        }
        // Latest crash report, if any.
        let mut crash: Option<String> = None;
        let crashes = backend::data_dir().join("crashes");
        if let Ok(read) = std::fs::read_dir(&crashes) {
            let mut files: Vec<std::path::PathBuf> = read.flatten().map(|e| e.path()).collect();
            files.sort();
            if let Some(last) = files.last() {
                crash = std::fs::read_to_string(last).ok();
            }
        }
        let build_info = format!(
            "zaprust {}\ngit: {}\ntarget: {}\ndebug mode bundle",
            env!("CARGO_PKG_VERSION"),
            env!("ZAPRUST_GIT_HASH"),
            env!("ZAPRUST_TARGET"),
        );
        let inputs = debug::BundleInputs {
            timeline_json: self.debug.trace.export_json(self.debug.include_bodies),
            logs,
            build_info,
            crash_report: crash,
        };
        let bytes = debug::build_bundle(&inputs);
        let path = dir.join("zaprust-debug-bundle.zip");
        match std::fs::write(&path, bytes) {
            Ok(()) => {
                // The bundle can hold redacted message bodies
                // (only with the user's opt-in above) and sits at
                // this fixed path until the next export: make it
                // owner-only on Unix instead of leaving it at
                // default permissions (privacy audit A7).
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
                self.notice =
                    Some(Notice::info(format!("Debug bundle written to {}", path.display())));
                self.trace_push("ui", "debug bundle exported", None);
            }
            Err(e) => {
                self.notice = Some(Notice::error(format!("Bundle export failed: {e}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Args;
    use backend::{LiveChat, LiveChatKind, LiveMessage};

    fn debug_app() -> App {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = App::new(Args::parse(), &ctx);
        app.debug.mode = true;
        app
    }

    /// Privacy finding C7: trace summaries are mirrored to the
    /// debug log file and exported in bundles, so they must not
    /// carry contact names or group titles. Chats and senders
    /// are identified by JID only, whose phone digits the ring's
    /// redaction masks.
    #[test]
    fn trace_summaries_carry_no_names() {
        let mut app = debug_app();
        app.trace_event(&BackendEvent::ChatUpsert(LiveChat {
            jid: "6281234567890@s.whatsapp.net".to_owned(),
            title: "Alice Johnson".to_owned(),
            kind: LiveChatKind::Dm,
            preview: "hi".to_owned(),
            preview_sender: None,
            timestamp: 1_700_000_000,
            unread: 0,
            archived: false,
            pinned: false,
            ephemeral_secs: 0,
            muted: false,
            mute_until: 0,
            parent_jid: None,
            is_parent: false,
        }));
        let mut msg = LiveMessage::text(
            "m1",
            "6281234567890@s.whatsapp.net",
            Some("Alice Johnson".to_owned()),
            "hello".to_owned(),
            1_700_000_000,
            false,
        );
        msg.sender_jid = Some("6281234567890@s.whatsapp.net".to_owned());
        app.trace_event(&BackendEvent::MessageUpsert(msg));

        let entries = app.debug.trace.entries();
        assert_eq!(entries.len(), 2);
        for entry in &entries {
            assert!(
                !entry.summary.contains("Alice Johnson"),
                "a display name reached the trace: {}",
                entry.summary
            );
            assert!(
                !entry.summary.contains("6281234567890"),
                "an unmasked phone number reached the trace: {}",
                entry.summary
            );
        }
        assert!(entries[0].summary.starts_with("ChatUpsert "), "{}", entries[0].summary);
        assert!(entries[1].summary.contains("from ***90@s.whatsapp.net"), "{}", entries[1].summary);
    }
}
