//! Chat export as ZIP: the gather state machine that
//! collects message media into one archive file.

use super::*;

impl ZipKind {
    /// The kind word the manifest and the media file names use.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Photo => "photo",
            Self::Video => "video",
            Self::Voice => "voice",
            Self::Document => "document",
            Self::Sticker => "sticker",
        }
    }
}

/// One attachment's gathered payload: bytes carried in memory,
/// or a file already on disk (a video cache file, a saved
/// download) whose bytes the ZIP writer streams in.
pub(crate) enum ZipPayload {
    /// Bytes in memory.
    Bytes(Vec<u8>),
    /// A file on disk.
    File(std::path::PathBuf),
}

/// Where one ZIP attachment stands in the collection.
pub(crate) enum ZipItemState {
    /// Not fetched yet.
    Pending,
    /// Gathered, ready for the writer.
    Included(ZipPayload),
    /// Not included; the manifest carries the reason.
    Missing(String),
}

/// One attachment of a ZIP export: a media message of the
/// exported chat, in conversation order.
pub(crate) struct ZipItem {
    /// The message's app id.
    pub msg_id: u64,
    /// Its fetch rail.
    pub kind: ZipKind,
    /// Its name inside the ZIP's media folder.
    pub file_name: String,
    /// Its manifest line context: "22 November 2026 · 14:03".
    pub stamp: String,
    /// The sender's display name ("You" for own messages).
    pub sender: String,
    /// The caption or file name, when the message carries one.
    pub caption: String,
    /// The collection state.
    pub state: ZipItemState,
}

/// A ZIP export's attachment collection in flight
/// (section 25): the plan is captured whole at start (chat,
/// transcript, items), then fetches are issued one at a time
/// and the answering events settle their item, so an export
/// belongs to the chat it started from even if the user moves
/// on while it gathers.
pub(crate) struct ZipGather {
    /// The exported chat's app id.
    pub chat_id: u64,
    /// The exported chat's title (the manifest's heading and
    /// the completion notice's subject).
    pub chat_title: String,
    /// The export file's base name, captured at start (the
    /// gather belongs to its chat even if the user moves on).
    pub file_base: String,
    /// The rendered Markdown transcript (the ZIP's chat.md).
    pub transcript: String,
    /// How many messages the transcript covers.
    pub message_count: usize,
    /// The attachments, in conversation order.
    pub items: Vec<ZipItem>,
    /// The message whose fetch is currently in flight.
    pub waiting: Option<u64>,
    /// egui time the current wait began (the watchdog's base).
    pub wait_started: f64,
}

impl crate::state::App {
    /// Settles the ZIP gather's waiting item with gathered
    /// bytes, when the event that landed is the answer the
    /// gather is waiting for. Called from the media byte
    /// events before their display handling, so bytes for a
    /// note the user is not playing (which the playback path
    /// would drop) are still the export's.
    pub(crate) fn zip_collect_bytes(&mut self, msg_id: u64, kind: ZipKind, bytes: Vec<u8>) {
        self.zip_settle(msg_id, Some(kind), ZipItemState::Included(ZipPayload::Bytes(bytes)));
    }

    /// Settles the waiting item with a file on disk (a video
    /// cache file or a saved download).
    pub(crate) fn zip_collect_file(
        &mut self,
        msg_id: u64,
        kind: ZipKind,
        path: std::path::PathBuf,
    ) {
        self.zip_settle(msg_id, Some(kind), ZipItemState::Included(ZipPayload::File(path)));
    }

    /// Settles the waiting item as not included, with the
    /// reason the manifest will carry. `kind` `None` settles
    /// whatever kind is waiting (the generic Error event
    /// carries no kind; only a document wait may be settled
    /// by it, which the caller enforces).
    pub(crate) fn zip_collect_missing(
        &mut self,
        msg_id: u64,
        kind: Option<ZipKind>,
        reason: String,
    ) {
        self.zip_settle(msg_id, kind, ZipItemState::Missing(reason));
    }

    /// The one settle path: an event settles the gather only
    /// when it answers the item actually being waited on, of
    /// the kind expected. Late or foreign answers change
    /// nothing.
    pub(super) fn zip_settle(&mut self, msg_id: u64, kind: Option<ZipKind>, state: ZipItemState) {
        let Some(gather) = &mut self.zip_gather else { return };
        if gather.waiting != Some(msg_id) {
            return;
        }
        let Some(item) = gather.items.iter_mut().find(|i| i.msg_id == msg_id) else {
            return;
        };
        if let Some(expected) = kind {
            if item.kind != expected {
                return;
            }
        }
        item.state = state;
        gather.waiting = None;
    }

    /// Drives the collection (section 25): issues the next
    /// pending item's fetch, settles items whose fetch cannot
    /// be issued, applies the wait watchdog, and finishes the
    /// export when every item has settled. Called every frame
    /// from the worker poll, so event handlers only settle and
    /// this one place advances.
    pub(crate) fn zip_gather_advance(&mut self, now: f64) {
        // The watchdog: a fetch that never answers must not
        // stall the export forever. Twenty seconds is far
        // beyond any of these rails' honest latency.
        if let Some(gather) = &mut self.zip_gather {
            if gather.waiting.is_some() && now - gather.wait_started > 20.0 {
                let stuck = gather.waiting.take().unwrap_or(0);
                if let Some(item) = gather.items.iter_mut().find(|i| i.msg_id == stuck) {
                    item.state = ZipItemState::Missing("the phone did not answer".to_owned());
                }
            }
        }
        loop {
            let Some(gather) = &self.zip_gather else { return };
            if gather.waiting.is_some() {
                return;
            }
            let Some(item) = gather.items.iter().find(|i| matches!(i.state, ZipItemState::Pending))
            else {
                // Every item has settled: the collection is
                // complete and the writer takes over.
                let gather = self.zip_gather.take().unwrap();
                self.finish_zip_export(gather);
                return;
            };
            let (chat_id, msg_id, kind) = (gather.chat_id, item.msg_id, item.kind);
            if self.zip_issue_fetch(chat_id, msg_id, kind) {
                if let Some(gather) = &mut self.zip_gather {
                    gather.waiting = Some(msg_id);
                    gather.wait_started = now;
                }
                return;
            }
            // The fetch cannot even be addressed (no session,
            // no protocol id): the item settles as missing and
            // the loop moves to the next one.
            if let Some(gather) = &mut self.zip_gather {
                if let Some(item) = gather.items.iter_mut().find(|i| i.msg_id == msg_id) {
                    item.state = ZipItemState::Missing("not available in this session".to_owned());
                }
            }
        }
    }

    /// Issues one item's fetch on its rail, mirroring the
    /// display paths' commands exactly (archived references
    /// included, so media from before this session is as
    /// fetchable for the export as it is for the screen).
    /// Returns whether the command was sent.
    pub(super) fn zip_issue_fetch(&mut self, chat_id: u64, msg_id: u64, kind: ZipKind) -> bool {
        let Some(protocol_id) = self.protocol_ids.get(&msg_id).cloned() else {
            return false;
        };
        let Some(session) = &self.live else { return false };
        let Some(chat_jid) = session.jids.get(&chat_id).cloned() else {
            return false;
        };
        let command = match kind {
            ZipKind::Photo => BackendCommand::FetchPhoto {
                chat_jid,
                message_id: protocol_id,
                photo_ref: session.photo_refs.get(&msg_id).cloned(),
            },
            ZipKind::Voice => BackendCommand::FetchVoice {
                chat_jid,
                message_id: protocol_id,
                voice_ref: session.voice_refs.get(&msg_id).cloned(),
            },
            ZipKind::Video => BackendCommand::FetchVideo {
                chat_jid,
                message_id: protocol_id,
                video_ref: session.video_refs.get(&msg_id).cloned(),
                view_once: false,
            },
            ZipKind::Sticker => BackendCommand::FetchSticker {
                chat_jid,
                message_id: protocol_id,
                sticker_ref: session.sticker_refs.get(&msg_id).cloned(),
            },
            ZipKind::Document => BackendCommand::DownloadMedia {
                chat_jid,
                message_id: protocol_id,
                document_ref: session.document_refs.get(&msg_id).cloned(),
            },
        };
        self.send_backend(command);
        true
    }

    /// Hands a completed collection to the ZIP writer worker
    /// (the same thread, channel and Busy discipline as the
    /// Markdown export) and arms its result channel.
    pub(crate) fn finish_zip_export(&mut self, gather: ZipGather) {
        let dir = backend::data_dir().join("exports");
        let base = gather.file_base.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned =
            std::thread::Builder::new().name("zaprust-export-zip".to_owned()).spawn(move || {
                let result = crate::conversation::write_zip_export(&dir, &base, &gather);
                let _ = tx.send(result);
            });
        match spawned {
            Ok(_) => {
                self.zip_export_rx = Some(rx);
            }
            Err(e) => {
                self.notice = Some(crate::state::Notice::error(format!("Export failed: {e}")));
            }
        }
    }
}
