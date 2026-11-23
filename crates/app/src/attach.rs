//! The attach flow: picking files from disk through the portal file
//! dialog, staging them above the composer, and sending them as
//! media messages (photos and videos for the album-style flow,
//! documents as files). Also the location form and the voice
//! recorder wiring live here, since both are composer attachments.

use super::*;
use crate::theme::rgb;
use core::model::{Message, MessageKind};
use core::theme::Tokens;
use eframe::egui::{Align, CornerRadius, Layout, Margin, Pos2, RichText, Sense, Stroke, Vec2};
use state::{now_unix, Notice, StagedKind, StagedMedia, StagedSendState};
use std::io::Read as _;

/// Photo extensions the picker accepts and the stager recognizes.
const PHOTO_EXTS: [&str; 5] = ["jpg", "jpeg", "png", "webp", "gif"];
/// Video extensions recognized as staged videos.
const VIDEO_EXTS: [&str; 5] = ["mp4", "mkv", "webm", "mov", "avi"];

/// Hard cap on one staged file send, in bytes. A send
/// materializes the file in memory (the backend encrypts from
/// the byte buffer), so without a cap one accidental multi-GB
/// pick would be read whole and could take the app down with
/// it. 512 MiB matches the backend's download cap and sits far
/// above anything WhatsApp itself produces in practice (its
/// videos are recompressed to tens of megabytes). Oversized
/// files are refused with a notice, never read.
pub(crate) const MAX_SEND_BYTES: u64 = 512 * 1024 * 1024;

/// Why a staged file of `size` bytes cannot be sent, in user
/// words, or `None` when it can. Pure policy, no IO: staging
/// checks it from metadata so an unsendable file is refused
/// with feedback right away, and the send worker checks it
/// again before and after reading (the file can change, or
/// grow, between staging and sending).
fn staged_size_problem(size: u64) -> Option<String> {
    if size == 0 {
        Some("the file is empty".to_owned())
    } else if size > MAX_SEND_BYTES {
        Some(format!("the file is larger than the {} MB send limit", MAX_SEND_BYTES / 1024 / 1024))
    } else {
        None
    }
}

/// The directory holding pasted-image temp files. A paste is
/// the one staged file the app creates itself (picked files
/// stay where the user keeps them), so pastes get their own
/// directory under the data dir, beside `exports` and
/// `crashes`: everything in it is app-owned scratch with a
/// lifecycle (see [`discard_pasted_temp`]).
fn pasted_dir() -> std::path::PathBuf {
    backend::data_dir().join("pasted")
}

/// A fresh, collision-free path for one pasted image inside
/// `dir`. The old name was `pasted-{now_unix()}.png`, at
/// one-second resolution: two pastes within the same second
/// produced the same path, and the second paste overwrote the
/// file the first staged item pointed at, so the first
/// attachment silently became the second image. The counter
/// follows the temp-name pattern core uses for its atomic
/// writes (a process-wide atomic counter plus the pid);
/// together with the timestamp it makes every name unique,
/// for same-second pastes, for identical pixels pasted twice,
/// and across two running instances.
fn pasted_image_path_in(dir: &std::path::Path) -> std::path::PathBuf {
    static PASTE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = PASTE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    dir.join(format!("pasted-{}-{}-{counter}.png", now_unix(), std::process::id()))
}

/// Whether `path` is a pasted-image temp file this app owns:
/// anything inside the pasted dir, plus the legacy location
/// (builds before the dedicated dir wrote pastes into the
/// data dir root, beside the databases, under the same
/// `pasted-` name pattern). Picked files never match: they
/// live outside the data dir and keep their own names.
fn is_pasted_temp(path: &std::path::Path, data_dir: &std::path::Path) -> bool {
    if path.parent() == Some(data_dir.join("pasted").as_path()) {
        return true;
    }
    path.parent() == Some(data_dir)
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("pasted-") && n.ends_with(".png"))
}

/// Deletes `path` when it is a pasted-image temp file, and
/// does nothing otherwise, so it is safe to call for every
/// staged item at the end of its life: unstaged in the
/// composer, sent, or dropped unsent. It must never be
/// called for an item that is still staged or whose send
/// read is still in flight; the staged-send poll calls it
/// only once the worker's result for that file has arrived
/// (the bytes are what gets sent, not the file).
pub(crate) fn discard_pasted_temp(path: &std::path::Path) {
    discard_pasted_temp_in(path, &backend::data_dir());
}

fn discard_pasted_temp_in(path: &std::path::Path, data_dir: &std::path::Path) {
    if is_pasted_temp(path, data_dir) {
        let _ = std::fs::remove_file(path);
    }
}

/// Startup backstop for pasted-image temp files: removes the
/// ones earlier sessions left behind (staged but never sent
/// when the app closed, or a send still in flight at exit).
/// Staged state is in-memory only, so at startup nothing in
/// the pasted dir can still be staged or mid-send for this
/// process and every file there is an orphan. The data dir
/// root is swept for the legacy name pattern only; the
/// databases and settings beside them are never touched.
pub(crate) fn sweep_stale_pasted_images() {
    sweep_stale_pasted_images_in(&backend::data_dir());
}

fn sweep_stale_pasted_images_in(data_dir: &std::path::Path) {
    if let Ok(entries) = std::fs::read_dir(data_dir.join("pasted")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(data_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_pasted_temp(&path, data_dir) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// One file the staged-send worker has to read. Only the path
/// crosses to the worker; the name, texture and other UI-side
/// state stay on the UI thread in the matching
/// [`PendingStagedSend`], paired with this job's result by
/// position.
pub(crate) struct StagedReadJob {
    path: std::path::PathBuf,
}

/// Reads one staged file for sending, on the worker thread.
/// Metadata comes first, so a vanished or oversized file fails
/// before a single byte is read; the read itself is bounded,
/// so a file that grows between the check and the read still
/// cannot exceed the cap. The error is the user-facing reason
/// (without the file name, which the matching send descriptor
/// carries).
fn read_staged_file(job: &StagedReadJob) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(&job.path).map_err(|e| format!("could not be read: {e}"))?;
    if !meta.is_file() {
        return Err("not a file".to_owned());
    }
    if let Some(problem) = staged_size_problem(meta.len()) {
        return Err(problem);
    }
    let file = std::fs::File::open(&job.path).map_err(|e| format!("could not be read: {e}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_SEND_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("could not be read: {e}"))?;
    if let Some(problem) = staged_size_problem(bytes.len() as u64) {
        return Err(problem);
    }
    Ok(bytes)
}

/// The staged-send worker body: reads the jobs in order and
/// streams one result per file, so the UI sends the album in
/// the order it was staged. Stops early when the receiver is
/// gone (the app shut down mid-send).
fn run_staged_reads(
    jobs: Vec<StagedReadJob>,
    tx: std::sync::mpsc::Sender<Result<Vec<u8>, String>>,
) {
    for job in &jobs {
        if tx.send(read_staged_file(job)).is_err() {
            return;
        }
    }
}

/// Everything needed to finish one staged file's send once
/// its bytes arrive from the worker: the staged item itself
/// (its texture binds to the echo) plus the target and the
/// options captured at send time, so a chat switch while the
/// read is in flight cannot reroute the file.
pub(crate) struct PendingStagedSend {
    pub(crate) item: StagedMedia,
    pub(crate) chat_id: u64,
    pub(crate) jid: String,
    pub(crate) caption: String,
    pub(crate) view_once: bool,
    pub(crate) ephemeral_secs: u32,
}

/// The notice for a finished staged send: `None` when every
/// file went out, otherwise a summary that names each file
/// that did not, with its reason. A partial album is always
/// reported as partial.
fn staged_send_summary(sent: usize, failed: &[(String, String)]) -> Option<String> {
    if failed.is_empty() {
        return None;
    }
    let list = failed
        .iter()
        .map(|(name, reason)| format!("{name} ({reason})"))
        .collect::<Vec<_>>()
        .join(", ");
    if sent == 0 {
        Some(format!("Nothing was sent. Could not send: {list}."))
    } else {
        Some(format!("Sent {sent} of {} files. Could not send: {list}.", sent + failed.len()))
    }
}

/// The display name for a picked path: its file name, or a
/// generic label for a path that has none.
fn file_display_name(path: &std::path::Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".to_owned())
}

impl App {
    /// Opens the portal file dialog for photos and videos (multi
    /// select) and stages everything picked. The blocking dialog is
    /// the standard rfd pattern: the UI thread parks until the
    /// dialog closes, exactly like any native modal.
    pub(crate) fn pick_photos(&mut self, ctx: &egui::Context) {
        self.composer.rich_menu_open = false;
        let Some(paths) = rfd::FileDialog::new()
            .add_filter(
                crate::i18n::t("attach.filter_media").as_str(),
                &["jpg", "jpeg", "png", "webp", "gif", "mp4", "mkv", "webm", "mov", "avi"],
            )
            .pick_files()
        else {
            return;
        };
        self.stage_files(ctx, paths);
    }

    /// Opens the portal file dialog for one document of any type
    /// and stages it.
    pub(crate) fn pick_document(&mut self, ctx: &egui::Context) {
        self.composer.rich_menu_open = false;
        let Some(path) = rfd::FileDialog::new().pick_file() else {
            return;
        };
        self.stage_files(ctx, vec![path]);
    }

    /// Stages picked paths: photos and videos keep their kind,
    /// everything else stages as a document. One file per pick for
    /// documents; photos and videos may arrive as a set (the album
    /// send path).
    pub(crate) fn stage_files(&mut self, ctx: &egui::Context, paths: Vec<std::path::PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let mut refused: Vec<String> = Vec::new();
        for path in paths {
            // A file that can never be sent is refused here,
            // named and with the reason, instead of staging it
            // and failing at send time. A metadata error does
            // not refuse: the send worker reports it honestly
            // when it tries to read the file.
            if let Ok(meta) = std::fs::metadata(&path) {
                if meta.is_file() {
                    if let Some(problem) = staged_size_problem(meta.len()) {
                        refused.push(format!("{} ({problem})", file_display_name(&path)));
                        continue;
                    }
                }
            }
            self.composer.staged.push(staged_from_path(ctx, &path));
        }
        if !refused.is_empty() {
            self.notice = Some(Notice::error(format!("Not attached: {}.", refused.join(", "))));
        }
        // The stage belongs to the chat it was attached in;
        // `send_staged` refuses to send it anywhere else.
        self.composer.staged_chat = Some(self.selected_chat);
    }

    /// Sends everything staged, as one media message per file
    /// (consecutive sends read as an album in the stream), then
    /// clears the stage. Live hands the files to a read worker
    /// (see [`App::dispatch_staged_send`]); demo sends the
    /// canned equivalents right away.
    pub(crate) fn send_staged(&mut self, engaged: &mut bool) {
        if self.composer.staged.is_empty() {
            return;
        }
        // Hard backstop behind the per-chat parking: a stage
        // that belongs to another chat (a switch path that did
        // not park, restored state from an older build) is
        // refused and dropped, never sent into the conversation
        // that happens to be open.
        if self.composer.staged_chat != Some(self.selected_chat) {
            // The dropped stage ends here: any pasted temp
            // files backing it are deleted with it.
            let dropped = std::mem::take(&mut self.composer.staged);
            for item in &dropped {
                discard_pasted_temp(&item.path);
            }
            self.composer.staged_caption.clear();
            self.composer.staged_chat = None;
            self.notice = Some(Notice::info(
                "Those files were attached in a different chat and were not sent.".to_owned(),
            ));
            return;
        }
        if self.live_mode {
            self.dispatch_staged_send(engaged);
            return;
        }
        let staged = std::mem::take(&mut self.composer.staged);
        let caption = std::mem::take(&mut self.composer.staged_caption);
        self.composer.staged_chat = None;
        // View-once only makes sense for one photo or video: an
        // album opens one photo at a time by its own rules.
        let single = staged.len() == 1;
        let mut first_caption_used = false;
        for item in staged {
            let view_once = single && item.kind != StagedKind::Document && item.view_once;
            let item_caption = if first_caption_used { String::new() } else { caption.clone() };
            first_caption_used = true;
            let msg_kind = match item.kind {
                StagedKind::Photo => {
                    MessageKind::Image { caption: item_caption.clone(), view_once }
                }
                StagedKind::Video => MessageKind::Video {
                    caption: item_caption.clone(),
                    view_once,
                    // The staged file's own duration, probed
                    // from its bytes: a sent video states the
                    // length it actually has.
                    secs: std::fs::read(&item.path)
                        .map(|bytes| probe_video_seconds(&bytes))
                        .unwrap_or(0),
                },
                StagedKind::Document => MessageKind::Document {
                    name: item.name.clone(),
                    size_bytes: 0,
                    page_count: None,
                },
            };
            let size_bytes = std::fs::metadata(&item.path).map(|m| m.len()).unwrap_or(0);
            self.sent_counter += 1;
            let msg_id = 900 + self.sent_counter;
            let kind = match item.kind {
                StagedKind::Document => {
                    MessageKind::Document { name: item.name.clone(), size_bytes, page_count: None }
                }
                _ => msg_kind,
            };
            let (stamp, expires_at) = self.demo_send_stamp();
            let msg = Message { kind, expires_at, ..Message::mine(msg_id, String::new(), stamp) };
            self.conversation.push(msg.clone());
            let chat_id = self.selected_chat;
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                chat.messages.push(msg);
                chat.sync_preview_from_last();
            }
            if let Some(tex) = item.thumb {
                self.media.textures.insert(msg_id, tex);
            }
            // The send consumed the item: a pasted temp file
            // backing it has no life past this point.
            discard_pasted_temp(&item.path);
        }
        *engaged = true;
    }

    /// Live staged send: captures the target and options, then
    /// hands the files to a worker thread that reads them off
    /// the UI thread (see [`read_staged_file`]). Each file is
    /// sent as its bytes arrive, in staging order, by
    /// [`App::poll_staged_send`]; per-file failures are
    /// collected there and reported by name when the worker
    /// finishes, never silently dropped.
    fn dispatch_staged_send(&mut self, engaged: &mut bool) {
        if self.staged_send.is_some() {
            self.notice = Some(Notice::info(
                crate::i18n::t("attach.still_sending"),
            ));
            return;
        }
        let Some(jid) = self.live_jid(self.selected_chat) else {
            // Nothing is taken: the stage stays put for a retry
            // once a live chat is open.
            self.notice = Some(Notice::info(crate::i18n::t("attach.open_live_first")));
            return;
        };
        let staged = std::mem::take(&mut self.composer.staged);
        let caption = std::mem::take(&mut self.composer.staged_caption);
        self.composer.staged_chat = None;
        let chat_id = self.selected_chat;
        let ephemeral_secs = self.ephemeral_secs_of(chat_id);
        // View-once only makes sense for one photo or video: an
        // album opens one photo at a time by its own rules.
        let single = staged.len() == 1;
        let mut first_caption_used = false;
        let mut pending = std::collections::VecDeque::new();
        let mut jobs = Vec::new();
        for item in staged {
            let view_once = single && item.kind != StagedKind::Document && item.view_once;
            let item_caption = if first_caption_used { String::new() } else { caption.clone() };
            first_caption_used = true;
            jobs.push(StagedReadJob { path: item.path.clone() });
            pending.push_back(PendingStagedSend {
                item,
                chat_id,
                jid: jid.clone(),
                caption: item_caption,
                view_once,
                ephemeral_secs,
            });
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.staged_send = Some(StagedSendState { rx, pending, sent: 0, failed: Vec::new() });
        // The reads run on the worker and can take seconds for
        // a large file; until the first message lands (or the
        // send completes), the send reads as in flight instead
        // of as silence (audit S18). The completion in
        // poll_staged_send clears or replaces this.
        self.notice = Some(Notice::busy(crate::i18n::t("attach.sending")));
        // A failed spawn leaves the receiver disconnected, so
        // the poll reports every file as unreadable instead of
        // hanging or dropping the album silently.
        std::thread::Builder::new()
            .name("zaprust-staged-send".to_owned())
            .spawn(move || run_staged_reads(jobs, tx))
            .ok();
        *engaged = true;
    }

    /// Applies staged-send worker results, in staging order:
    /// each finished read is sent (and echoed, when its chat is
    /// still open) and each failure is recorded by name. When
    /// the worker's channel closes the send is over: any file
    /// that never produced a result counts as failed, and the
    /// summary notice names every file that did not go out.
    pub(crate) fn poll_staged_send(&mut self) {
        if self.staged_send.is_none() {
            return;
        }
        // Drain the channel first (ending the borrow), then
        // pair each result with its descriptor in order.
        let mut results: Vec<Result<Vec<u8>, String>> = Vec::new();
        let mut disconnected = false;
        if let Some(send) = &self.staged_send {
            loop {
                match send.rx.try_recv() {
                    Ok(result) => results.push(result),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        let mut completed: Vec<(PendingStagedSend, Result<Vec<u8>, String>)> = Vec::new();
        if let Some(send) = &mut self.staged_send {
            for result in results {
                let Some(entry) = send.pending.pop_front() else { break };
                completed.push((entry, result));
            }
        }
        for (entry, result) in completed {
            // This file's read is finished (its result is
            // here), so nothing is in flight for it anymore:
            // once the send below is done, a pasted temp file
            // backing it can be deleted. The bytes are what
            // gets sent, never the file itself.
            let temp_path = entry.item.path.clone();
            match result {
                Ok(bytes) if self.live.is_some() => {
                    if let Some(send) = &mut self.staged_send {
                        send.sent += 1;
                    }
                    self.send_staged_item(entry, bytes);
                }
                Ok(_) => {
                    // The session ended while the file was
                    // being read; the send has nowhere to go.
                    if let Some(send) = &mut self.staged_send {
                        send.failed.push((
                            entry.item.name.clone(),
                            "the connection closed before it could be sent".to_owned(),
                        ));
                    }
                }
                Err(reason) => {
                    if let Some(send) = &mut self.staged_send {
                        send.failed.push((entry.item.name.clone(), reason));
                    }
                }
            }
            discard_pasted_temp(&temp_path);
        }
        if disconnected {
            if let Some(mut send) = self.staged_send.take() {
                while let Some(entry) = send.pending.pop_front() {
                    send.failed.push((entry.item.name.clone(), "could not be read".to_owned()));
                    // The channel is disconnected, so the
                    // worker is gone: no read of this file is
                    // in flight, and its temp file can go.
                    discard_pasted_temp(&entry.item.path);
                }
                if let Some(notice) = staged_send_summary(send.sent, &send.failed) {
                    self.notice = Some(Notice::error(notice));
                }
                // A clean finish has no summary; the Busy
                // marker from the send's start goes with it.
                self.clear_busy_notice();
            }
        }
    }

    /// Sends one staged file whose bytes the worker read, and
    /// echoes it locally when its chat is the open one (the
    /// backend upsert replaces the echo, as with every live
    /// send). When another chat is open the message still goes
    /// to its own chat; only the local echo is skipped, the
    /// same rule the GIF send follows.
    fn send_staged_item(&mut self, entry: PendingStagedSend, bytes: Vec<u8>) {
        let PendingStagedSend { item, chat_id, jid, caption, view_once, ephemeral_secs } = entry;
        let backend_kind = match item.kind {
            StagedKind::Photo => backend::MediaKind::Photo,
            StagedKind::Video => backend::MediaKind::Video,
            StagedKind::Document => backend::MediaKind::Document,
        };
        // A sent video states its real duration on the wire:
        // probed from the staged bytes with the app's own
        // demuxer (an unprobeable file honestly states 0). A
        // sent document's size is its byte count, never a
        // rounded-down figure.
        let seconds = match item.kind {
            StagedKind::Video => probe_video_seconds(&bytes),
            _ => 0,
        };
        let msg_kind = match item.kind {
            StagedKind::Photo => MessageKind::Image { caption: caption.clone(), view_once },
            StagedKind::Video => {
                MessageKind::Video { caption: caption.clone(), view_once, secs: seconds }
            }
            StagedKind::Document => MessageKind::Document {
                name: item.name.clone(),
                size_bytes: bytes.len() as u64,
                page_count: None,
            },
        };
        self.send_backend(backend::BackendCommand::SendMedia {
            chat_jid: jid,
            kind: backend_kind,
            data: bytes,
            mime: item.mime.clone(),
            caption,
            file_name: Some(item.name.clone()),
            gif_playback: false,
            seconds,
            view_once,
            ephemeral_secs,
        });
        if self.selected_chat == chat_id {
            self.echo_live_media(msg_kind, &item.name);
            if let Some(tex) = item.thumb {
                // The echo's id follows echo_live_media's
                // counter, so the photo's pixels bind to the
                // same bubble the backend upsert will replace.
                self.media.textures.insert(9_000_000 + self.sent_counter, tex);
            }
        }
    }

    /// Sends the location form's values as a location message.
    /// Demo echoes the canned card; live sends through the backend.
    pub(crate) fn send_location_draft(&mut self) {
        let Some(draft) = self.composer.location_draft.take() else { return };
        let lat = draft.latitude.trim().parse::<f64>().ok();
        let lon = draft.longitude.trim().parse::<f64>().ok();
        let (Some(lat), Some(lon)) = (lat, lon) else {
            self.composer.location_draft = Some(draft);
            self.notice = Some(Notice::info(crate::i18n::t("attach.location_needs_numbers")));
            return;
        };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            self.composer.location_draft = Some(draft);
            self.notice = Some(Notice::info(crate::i18n::t("attach.location_needs_numbers")));
            return;
        }
        let name = draft.name.trim().to_owned();
        if self.live_mode {
            let Some(jid) =
                self.live.as_ref().and_then(|s| s.jids.get(&self.selected_chat).cloned())
            else {
                self.notice = Some(Notice::info(crate::i18n::t("attach.open_live_first")));
                return;
            };
            self.send_backend(backend::BackendCommand::SendLocation {
                chat_jid: jid,
                name: name.clone(),
                latitude: lat,
                longitude: lon,
                ephemeral_secs: self.ephemeral_secs_of(self.selected_chat),
            });
            self.echo_live_media(
                MessageKind::Location { name, latitude: lat, longitude: lon },
                "Location",
            );
        } else {
            self.sent_counter += 1;
            let (stamp, expires_at) = self.demo_send_stamp();
            let msg = Message {
                kind: MessageKind::Location { name, latitude: lat, longitude: lon },
                expires_at,
                ..Message::mine(900 + self.sent_counter, String::new(), stamp)
            };
            self.conversation.push(msg.clone());
            let chat_id = self.selected_chat;
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                chat.messages.push(msg);
                chat.sync_preview_from_last();
            }
        }
    }

    /// Stages an image pasted from the clipboard. egui-winit turns
    /// a Ctrl+V over an image clipboard into `Event::PasteImage`
    /// (text pastes stay native `Paste` events for the input
    /// field); the pixels land in a uniquely named PNG in the
    /// pasted dir (see [`pasted_image_path_in`]) so the
    /// staged-file flow sends them like any picked photo. The
    /// file is a temp: it is deleted when the staged item is
    /// unstaged, sent or dropped, and any file a previous
    /// session left behind is swept at startup.
    pub(crate) fn stage_pasted_image(&mut self, ctx: &egui::Context, image: egui::ColorImage) {
        let [w, h] = image.size;
        let rgba = image::RgbaImage::from_fn(w as u32, h as u32, |x, y| {
            let c = image.pixels[y as usize * w + x as usize];
            image::Rgba([c.r(), c.g(), c.b(), c.a()])
        });
        let dir = pasted_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = pasted_image_path_in(&dir);
        if let Err(e) = rgba.save(&path) {
            self.notice = Some(Notice::error(format!("Could not save the pasted image: {e}")));
            return;
        }
        self.stage_files(ctx, vec![path.clone()]);
        // Staging can refuse a file (empty, past the send
        // cap): a refused paste was never staged, so its temp
        // file has no owner left and is deleted right away.
        if !self.composer.staged.iter().any(|s| s.path == path) {
            discard_pasted_temp(&path);
        }
    }

    /// Starts a voice recording (the composer's mic button). An
    /// unavailable microphone surfaces an honest notice.
    pub(crate) fn start_recording(&mut self) {
        if self.recorder.is_some() {
            return;
        }
        match crate::recorder::Recorder::start() {
            Ok(rec) => self.recorder = Some(rec),
            Err(e) => self.notice = Some(Notice::error(e)),
        }
    }

    /// Cancels the recording: the recorder's Drop stops the thread
    /// and discards the audio.
    pub(crate) fn cancel_recording(&mut self) {
        self.recorder = None;
    }

    /// Stops and encodes the recording, then sends it as a voice
    /// note (live: Ogg/Opus through the backend; demo: the local
    /// voice bubble with the recorded waveform).
    pub(crate) fn finish_recording(&mut self, engaged: &mut bool) {
        let Some(rec) = self.recorder.take() else { return };
        let levels = rec.levels(28);
        match rec.finish() {
            Ok((bytes, secs)) => {
                let waveform = waveform_from_levels(&levels);
                if self.live_mode {
                    let Some(jid) =
                        self.live.as_ref().and_then(|s| s.jids.get(&self.selected_chat).cloned())
                    else {
                        self.notice = Some(Notice::info(crate::i18n::t("attach.open_live_first")));
                        return;
                    };
                    self.send_backend(backend::BackendCommand::SendMedia {
                        chat_jid: jid,
                        kind: backend::MediaKind::Voice,
                        data: bytes,
                        mime: "audio/ogg; codecs=opus".to_owned(),
                        caption: String::new(),
                        file_name: None,
                        gif_playback: false,
                        seconds: secs,
                        view_once: false,
                        ephemeral_secs: self.ephemeral_secs_of(self.selected_chat),
                    });
                    self.echo_live_media(MessageKind::Voice { secs, waveform }, "Voice note");
                } else {
                    self.sent_counter += 1;
                    let (stamp, expires_at) = self.demo_send_stamp();
                    let msg = Message {
                        kind: MessageKind::Voice { secs, waveform },
                        expires_at,
                        ..Message::mine(900 + self.sent_counter, String::new(), stamp)
                    };
                    self.conversation.push(msg.clone());
                    let chat_id = self.selected_chat;
                    if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                        chat.messages.push(msg);
                        chat.sync_preview_from_last();
                    }
                }
                *engaged = true;
            }
            Err(e) => self.notice = Some(Notice::error(e)),
        }
    }
}

/// Maps "jpeg"/"jpg" to jpeg, everything else passes through, so
/// the mime matches what the protocol side expects.
fn ext_png_jpeg(ext: &str) -> &str {
    if ext == "jpg" || ext == "jpeg" {
        "jpeg"
    } else {
        ext
    }
}

/// Classifies one picked path into a staged file: kind and mime
/// come from the extension, photos decode a small preview right
/// away so the staged bar shows real pixels, not a placeholder.
fn staged_from_path(ctx: &egui::Context, path: &std::path::Path) -> StagedMedia {
    let name = file_display_name(path);
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let kind = if PHOTO_EXTS.contains(&ext.as_str()) {
        StagedKind::Photo
    } else if VIDEO_EXTS.contains(&ext.as_str()) {
        StagedKind::Video
    } else {
        StagedKind::Document
    };
    let mime = match kind {
        StagedKind::Photo => format!("image/{}", ext_png_jpeg(&ext)),
        StagedKind::Video => format!("video/{ext}"),
        StagedKind::Document if ext == "pdf" => "application/pdf".to_owned(),
        StagedKind::Document => "application/octet-stream".to_owned(),
    };
    let thumb = if kind == StagedKind::Photo { crate::media::load_thumb(ctx, path) } else { None };
    StagedMedia { kind, name, path: path.to_path_buf(), mime, view_once: false, thumb }
}

/// Compresses the recording's level taps into the 28-bar waveform
/// shape the voice bubble renders.
/// A staged video's duration in whole seconds, probed from its
/// bytes with the app's own demuxer. A file the demuxer cannot
/// read (not an MP4, or damaged) honestly probes as 0: the send
/// then states no duration rather than an invented one.
fn probe_video_seconds(bytes: &[u8]) -> u32 {
    crate::video::demux(bytes).map(|demuxed| demuxed.info.duration_secs.round() as u32).unwrap_or(0)
}

fn waveform_from_levels(levels: &[f32]) -> Vec<f32> {
    let mut bars: Vec<f32> = levels.to_vec();
    bars.resize(28, 0.0);
    let peak = bars.iter().cloned().fold(0.0_f32, f32::max);
    if peak > 0.0 {
        for b in &mut bars {
            *b = (*b / peak).clamp(0.06, 1.0);
        }
    }
    bars
}

/// Whether a staged item offers the photo editor (section
/// 25): a staged photo that is not a GIF. A GIF's animation
/// would be flattened by any edit (the image crate decodes
/// its first frame), so it is never offered; a file that
/// fails to decode is refused at open with an honest notice.
pub(crate) fn staged_photo_editable(item: &StagedMedia) -> bool {
    item.kind == StagedKind::Photo && item.mime != "image/gif"
}

/// Reframes a crop rectangle (normalized over an image of
/// the given pixel size) to the largest centred rectangle of
/// the target pixel aspect. Pure geometry, shared by the
/// aspect presets.
fn reframed_crop(image_w: u32, image_h: u32, aspect: f32) -> [f32; 4] {
    let (w, h) = (image_w as f32, image_h as f32);
    let (mut cw, mut ch) = (w, h);
    if w / h > aspect {
        cw = h * aspect;
    } else {
        ch = w / aspect;
    }
    let x0 = (w - cw) / 2.0 / w;
    let y0 = (h - ch) / 2.0 / h;
    [x0, y0, x0 + cw / w, y0 + ch / h]
}

/// Clamps a crop rectangle into the unit square with a sane
/// minimum size on each axis.
fn clamped_crop(mut crop: [f32; 4]) -> [f32; 4] {
    const MIN: f32 = 0.05;
    crop[0] = crop[0].clamp(0.0, 1.0 - MIN);
    crop[1] = crop[1].clamp(0.0, 1.0 - MIN);
    crop[2] = crop[2].clamp(crop[0] + MIN, 1.0);
    crop[3] = crop[3].clamp(crop[1] + MIN, 1.0);
    crop
}

impl App {
    /// Opens the photo editor on the staged photo at `index`
    /// (section 25). The decoded image becomes the working
    /// image; nothing about the staged item changes until
    /// Apply.
    pub(crate) fn open_photo_editor(&mut self, index: usize) {
        let Some(item) = self.composer.staged.get(index) else { return };
        if !staged_photo_editable(item) {
            return;
        }
        let path = item.path.clone();
        let decoded = image::ImageReader::open(&path)
            .map_err(|e| e.to_string())
            .and_then(|r| r.with_guessed_format().map_err(|e| e.to_string()))
            .and_then(|r| r.decode().map_err(|e| e.to_string()));
        match decoded {
            Ok(image) => {
                self.photo_editor = Some(state::PhotoEditor {
                    staged_index: index,
                    chat_id: self.composer.staged_chat.unwrap_or(self.selected_chat),
                    image,
                    texture: None,
                    crop: [0.0, 0.0, 1.0, 1.0],
                    aspect: state::CropAspect::Free,
                });
            }
            Err(_) => {
                self.notice =
                    Some(Notice::error("That photo could not be opened for editing.".to_owned()));
            }
        }
    }

    /// Applies the editor's working state to the staged item:
    /// crop in pixel space, encode as JPEG, write into the
    /// pasted scratch dir (so the temp lifecycle disposes of
    /// it like any app-made file), and point the staged item
    /// at the new file. The send reads the staged path, so
    /// the edited bytes are by construction what gets sent.
    fn apply_photo_editor(&mut self, ctx: &egui::Context) {
        let Some(editor) = self.photo_editor.take() else { return };
        let valid = self.composer.staged_chat == Some(editor.chat_id)
            && self.composer.staged.get(editor.staged_index).is_some();
        if !valid {
            self.notice = Some(Notice::error(
                "That photo is no longer staged, so the edit was dropped.".to_owned(),
            ));
            return;
        }
        let (w, h) = (editor.image.width(), editor.image.height());
        let crop = clamped_crop(editor.crop);
        let x = (crop[0] * w as f32).round() as u32;
        let y = (crop[1] * h as f32).round() as u32;
        let cw = (((crop[2] - crop[0]) * w as f32).round() as u32).max(1).min(w - x);
        let ch = (((crop[3] - crop[1]) * h as f32).round() as u32).max(1).min(h - y);
        let cropped = editor.image.crop_imm(x, y, cw, ch);
        let mut bytes: Vec<u8> = Vec::new();
        let encoded =
            cropped.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Jpeg);
        if encoded.is_err() {
            self.notice = Some(Notice::error("The edited photo could not be written.".to_owned()));
            return;
        }
        let dir = pasted_dir();
        if std::fs::create_dir_all(&dir).is_err() {
            self.notice = Some(Notice::error("The edited photo could not be written.".to_owned()));
            return;
        }
        let mut path = pasted_image_path_in(&dir);
        path.set_extension("jpg");
        if std::fs::write(&path, &bytes).is_err() {
            self.notice = Some(Notice::error("The edited photo could not be written.".to_owned()));
            return;
        }
        let item = &mut self.composer.staged[editor.staged_index];
        let old_path = std::mem::replace(&mut item.path, path.clone());
        item.mime = "image/jpeg".to_owned();
        item.thumb = crate::media::load_thumb(ctx, &path);
        // The replaced file's life ends here when it was one
        // of ours (a pasted photo or an earlier edit); a picked
        // file on the user's disk is never touched.
        discard_pasted_temp(&old_path);
    }

    /// The photo editor modal (section 25): the working image
    /// with its crop rectangle, rotate and aspect controls,
    /// and Cancel / Apply. One modal over the conversation,
    /// drawn from the shell's overlay pass.
    pub(crate) fn draw_photo_editor(&mut self, ctx: &egui::Context) {
        if self.photo_editor.is_none() {
            return;
        }
        let t = self.tokens();
        let mut cancel = false;
        let mut apply = false;
        let mut rotate = false;
        let screen = ctx.content_rect();
        // Scrim: the modal owns the pointer while it is up.
        egui::Area::new(egui::Id::unique("photo_editor_scrim"))
            .order(egui::Order::Foreground)
            .fixed_pos(Pos2::ZERO)
            .show(ctx, |ui| {
                let (rect, _) = ui.allocate_exact_size(screen.size(), Sense::click());
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::ZERO,
                    egui::Color32::from_rgba_unmultiplied(0, 0, 0, 150),
                );
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        let panel_w = (screen.width() - 32.0).min(560.0);
        egui::Area::new(egui::Id::unique("photo_editor"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                crate::theme::glass_frame(t, crate::theme::RADIUS_PANEL).inner_margin(Margin::same(16)).show(ui, |ui| {
                    ui.set_width(panel_w - 32.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(crate::i18n::t("editor.title"))
                                .font(crate::theme::semibold(15.0))
                                .color(rgb(t.text_primary)),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if crate::widgets::icon_button(
                                ui,
                                t,
                                crate::icons::Icon::X,
                                26.0,
                                &crate::i18n::t("common.close"),
                                None,
                            )
                            .clicked()
                            {
                                cancel = true;
                            }
                        });
                    });
                    ui.add_space(10.0);
                    self.editor_preview(ui, t);
                    ui.add_space(10.0);
                    // Controls: rotate, then the aspect
                    // presets as chips.
                    ui.horizontal(|ui| {
                        if crate::widgets::button(
                            ui,
                            t,
                            &crate::i18n::t("editor.rotate"),
                            crate::widgets::Btn::Secondary,
                        )
                        .clicked()
                        {
                            rotate = true;
                        }
                        ui.add_space(8.0);
                        let aspects = [
                            (crate::i18n::t("editor.aspect_free"), state::CropAspect::Free),
                            (crate::i18n::t("editor.aspect_original"), state::CropAspect::Original),
                            ("1:1".to_owned(), state::CropAspect::Square),
                            ("4:3".to_owned(), state::CropAspect::FourThree),
                            ("3:4".to_owned(), state::CropAspect::ThreeFour),
                            ("16:9".to_owned(), state::CropAspect::SixteenNine),
                            ("9:16".to_owned(), state::CropAspect::NineSixteen),
                        ];
                        let current =
                            self.photo_editor.as_ref().map(|e| e.aspect).unwrap_or_default();
                        for (label, aspect) in aspects {
                            if crate::widgets::chip(ui, &label, current == aspect, t).clicked() {
                                if let Some(editor) = &mut self.photo_editor {
                                    editor.aspect = aspect;
                                    if let Some(ratio) = aspect.ratio(
                                        editor.image.width() as f32
                                            / editor.image.height().max(1) as f32,
                                    ) {
                                        editor.crop = reframed_crop(
                                            editor.image.width(),
                                            editor.image.height(),
                                            ratio,
                                        );
                                    }
                                }
                            }
                        }
                    });
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if crate::widgets::button(
                            ui,
                            t,
                            &crate::i18n::t("common.cancel"),
                            crate::widgets::Btn::Secondary,
                        )
                        .clicked()
                        {
                            cancel = true;
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if crate::widgets::button(
                                ui,
                                t,
                                &crate::i18n::t("editor.apply"),
                                crate::widgets::Btn::Primary,
                            )
                            .clicked()
                            {
                                apply = true;
                            }
                        });
                    });
                });
            });
        if rotate {
            if let Some(editor) = &mut self.photo_editor {
                editor.image = editor.image.rotate90();
                editor.texture = None;
                // The crop restarts from the full frame in the
                // rotated space: carrying a rectangle across a
                // rotation invites off-by-transpose mistakes,
                // and the full frame is the honest default.
                editor.crop = [0.0, 0.0, 1.0, 1.0];
            }
        }
        if cancel {
            self.photo_editor = None;
        } else if apply {
            self.apply_photo_editor(ctx);
        }
    }

    /// The editor's preview: the working image fitted into
    /// the panel, the crop rectangle over it (dimmed outside,
    /// accent border), dragged to move, corner-dragged to
    /// resize, and a press outside starts a new rectangle.
    fn editor_preview(&mut self, ui: &mut egui::Ui, t: Tokens) {
        let Some(editor) = &mut self.photo_editor else { return };
        if editor.texture.is_none() {
            let preview = editor.image.thumbnail(1024, 1024);
            let rgba = preview.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            editor.texture = Some(ui.ctx().load_texture(
                "photo_editor_preview",
                color,
                egui::TextureOptions::LINEAR,
            ));
        }
        let Some(tex) = &editor.texture else { return };
        let tex_size = tex.size_vec2();
        let avail_w = ui.available_width();
        let max_h = 360.0_f32;
        let scale = (avail_w / tex_size.x).min(max_h / tex_size.y).min(1.0);
        let disp = tex_size * scale;
        let (rect, resp) = ui.allocate_exact_size(disp, Sense::drag());
        let painter = ui.painter();
        painter.image(
            tex.id(),
            rect,
            egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        // The crop rectangle in display space.
        let crop = editor.crop;
        let crect = egui::Rect::from_min_max(
            Pos2::new(rect.left() + crop[0] * rect.width(), rect.top() + crop[1] * rect.height()),
            Pos2::new(rect.left() + crop[2] * rect.width(), rect.top() + crop[3] * rect.height()),
        );
        // Dim everything outside the crop.
        let dim = egui::Color32::from_rgba_unmultiplied(0, 0, 0, 110);
        painter.rect_filled(
            egui::Rect::from_min_max(rect.min, Pos2::new(rect.right(), crect.top())),
            CornerRadius::ZERO,
            dim,
        );
        painter.rect_filled(
            egui::Rect::from_min_max(Pos2::new(rect.left(), crect.bottom()), rect.max),
            CornerRadius::ZERO,
            dim,
        );
        painter.rect_filled(
            egui::Rect::from_min_max(
                Pos2::new(rect.left(), crect.top()),
                Pos2::new(crect.left(), crect.bottom()),
            ),
            CornerRadius::ZERO,
            dim,
        );
        painter.rect_filled(
            egui::Rect::from_min_max(
                Pos2::new(crect.right(), crect.top()),
                Pos2::new(rect.right(), crect.bottom()),
            ),
            CornerRadius::ZERO,
            dim,
        );
        painter.rect_stroke(
            crect,
            CornerRadius::ZERO,
            Stroke::new(1.5_f32, rgb(t.accent)),
            egui::StrokeKind::Outside,
        );
        // Corner handles.
        for corner in
            [crect.left_top(), crect.right_top(), crect.left_bottom(), crect.right_bottom()]
        {
            painter.rect_filled(
                egui::Rect::from_center_size(corner, Vec2::splat(11.0)),
                CornerRadius::same(2),
                rgb(t.accent),
            );
        }
        // Dragging: the mode is decided at press by where the
        // pointer landed (a corner resizes from the opposite
        // corner, inside moves, outside starts a new rect),
        // then the delta drives the rectangle in normalized
        // space, with the aspect preset locking the shape.
        let drag_id = egui::Id::unique("photo_editor_drag");
        if resp.drag_started() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let n = Pos2::new(
                    (pos.x - rect.left()) / rect.width(),
                    (pos.y - rect.top()) / rect.height(),
                );
                let near = 0.07_f32;
                let mode = if (n.x - crop[0]).abs() < near && (n.y - crop[1]).abs() < near {
                    "br" // fix bottom-right, move top-left: anchor is crop[2],crop[3]
                } else if (n.x - crop[2]).abs() < near && (n.y - crop[1]).abs() < near {
                    "bl"
                } else if (n.x - crop[0]).abs() < near && (n.y - crop[3]).abs() < near {
                    "tr"
                } else if (n.x - crop[2]).abs() < near && (n.y - crop[3]).abs() < near {
                    "tl"
                } else if crect.contains(pos) {
                    "move"
                } else {
                    "new"
                };
                ui.data_mut(|d| {
                    d.insert_temp(drag_id, mode);
                    d.insert_temp(drag_id.with("anchor"), n);
                    d.insert_temp(drag_id.with("start"), crop);
                });
            }
        }
        if resp.dragged() {
            let mode: &str = ui.data(|d| d.get_temp(drag_id)).unwrap_or("move");
            let start: [f32; 4] = ui.data(|d| d.get_temp(drag_id.with("start"))).unwrap_or(crop);
            let anchor: Pos2 = ui.data(|d| d.get_temp(drag_id.with("anchor"))).unwrap_or_default();
            let delta = resp.drag_delta();
            let dn = Vec2::new(delta.x / rect.width(), delta.y / rect.height());
            let mut next = start;
            match mode {
                "move" => {
                    let w = start[2] - start[0];
                    let h = start[3] - start[1];
                    next[0] = (start[0] + dn.x).clamp(0.0, 1.0 - w);
                    next[1] = (start[1] + dn.y).clamp(0.0, 1.0 - h);
                    next[2] = next[0] + w;
                    next[3] = next[1] + h;
                }
                "new" => {
                    next = [
                        anchor.x.min(anchor.x + dn.x),
                        anchor.y.min(anchor.y + dn.y),
                        anchor.x.max(anchor.x + dn.x),
                        anchor.y.max(anchor.y + dn.y),
                    ];
                }
                // Corner modes name the FIXED corner.
                "tl" => {
                    next[2] = start[2] + dn.x;
                    next[3] = start[3] + dn.y;
                }
                "tr" => {
                    next[0] = start[0] + dn.x;
                    next[3] = start[3] + dn.y;
                }
                "bl" => {
                    next[2] = start[2] + dn.x;
                    next[1] = start[1] + dn.y;
                }
                _ => {
                    // "br": the top-left corner moves.
                    next[0] = start[0] + dn.x;
                    next[1] = start[1] + dn.y;
                }
            }
            // Aspect lock: reshape around the fixed corner (or
            // the centre for a move, which keeps its size and
            // is therefore already locked).
            if mode != "move" {
                if let Some(ratio) = editor
                    .aspect
                    .ratio(editor.image.width() as f32 / editor.image.height().max(1) as f32)
                {
                    let (iw, ih) = (editor.image.width() as f32, editor.image.height() as f32);
                    let w_px = (next[2] - next[0]) * iw;
                    let h_px = w_px / ratio;
                    let hn = h_px / ih;
                    match mode {
                        "tl" => next[1] = next[3] - hn,
                        "tr" => next[3] = next[1] + hn,
                        "bl" => next[1] = next[3] - hn,
                        "new" => {
                            // Grow from the anchor point.
                            if dn.y < 0.0 {
                                next[1] = next[3] - hn;
                            } else {
                                next[3] = next[1] + hn;
                            }
                        }
                        _ => next[3] = next[1] + hn,
                    }
                }
            }
            editor.crop = clamped_crop(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        clamped_crop, discard_pasted_temp_in, ext_png_jpeg, pasted_image_path_in, read_staged_file,
        reframed_crop, run_staged_reads, staged_from_path, staged_photo_editable,
        staged_send_summary, staged_size_problem, sweep_stale_pasted_images_in, StagedReadJob,
        MAX_SEND_BYTES,
    };

    /// Writes a small temp file for the read tests and returns
    /// its path.
    fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("zaprust-attach-read-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("test file writes");
        path
    }

    fn read_job(path: &std::path::Path) -> StagedReadJob {
        StagedReadJob { path: path.to_path_buf() }
    }

    #[test]
    fn send_size_limits_are_checked_at_the_boundaries() {
        assert_eq!(staged_size_problem(1), None);
        assert_eq!(staged_size_problem(MAX_SEND_BYTES), None);
        assert!(staged_size_problem(0).unwrap().contains("empty"));
        assert!(staged_size_problem(MAX_SEND_BYTES + 1).unwrap().contains("send limit"));
    }

    #[test]
    fn a_small_file_reads_back_verbatim() {
        let path = temp_file("small.bin", b"hello staged world");
        let bytes = read_staged_file(&read_job(&path)).expect("a small file reads");
        assert_eq!(bytes, b"hello staged world");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_reports_a_read_failure() {
        let path = std::env::temp_dir().join("zaprust-attach-read-test").join("gone.bin");
        let _ = std::fs::remove_file(&path);
        let err = read_staged_file(&read_job(&path)).unwrap_err();
        assert!(err.contains("could not be read"), "unexpected reason: {err}");
    }

    #[test]
    fn an_empty_file_is_refused_as_empty() {
        let path = temp_file("empty.bin", b"");
        let err = read_staged_file(&read_job(&path)).unwrap_err();
        assert!(err.contains("empty"), "unexpected reason: {err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_oversized_file_is_refused_from_metadata_alone() {
        // A sparse file past the cap: the metadata check must
        // refuse it without a single byte being read.
        let path = temp_file("huge.bin", b"");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_SEND_BYTES + 1)
            .expect("the sparse file extends");
        let err = read_staged_file(&read_job(&path)).unwrap_err();
        assert!(err.contains("send limit"), "unexpected reason: {err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_directory_is_not_a_sendable_file() {
        let dir = std::env::temp_dir().join("zaprust-attach-read-test");
        let _ = std::fs::create_dir_all(&dir);
        let err = read_staged_file(&read_job(&dir)).unwrap_err();
        assert!(err.contains("not a file"), "unexpected reason: {err}");
    }

    #[test]
    fn the_read_worker_reports_each_file_in_staging_order() {
        let first = temp_file("one.bin", b"one");
        let gone = std::env::temp_dir().join("zaprust-attach-read-test").join("vanished.bin");
        let _ = std::fs::remove_file(&gone);
        let second = temp_file("two.bin", b"two!");
        let jobs = vec![read_job(&first), read_job(&gone), read_job(&second)];
        let (tx, rx) = std::sync::mpsc::channel();
        run_staged_reads(jobs, tx);
        let results: Vec<_> = rx.iter().collect();
        assert_eq!(results.len(), 3, "one result per file, none dropped");
        assert_eq!(results[0].as_ref().map(Vec::as_slice), Ok(&b"one"[..]));
        assert!(results[1].is_err(), "the vanished file fails in its own slot");
        assert_eq!(results[2].as_ref().map(Vec::as_slice), Ok(&b"two!"[..]));
        let _ = std::fs::remove_file(&first);
        let _ = std::fs::remove_file(&second);
    }

    #[test]
    fn a_clean_send_has_no_summary_notice() {
        assert_eq!(staged_send_summary(3, &[]), None);
    }

    #[test]
    fn a_partial_send_is_reported_as_partial_with_names() {
        let failed = vec![("b.png".to_owned(), "the file is empty".to_owned())];
        let notice = staged_send_summary(2, &failed).expect("a partial send reports");
        assert!(notice.starts_with("Sent 2 of 3 files."), "unexpected notice: {notice}");
        assert!(notice.contains("b.png (the file is empty)"), "unexpected notice: {notice}");
    }

    #[test]
    fn a_fully_failed_send_says_nothing_was_sent() {
        let failed = vec![("a.png".to_owned(), "could not be read: gone".to_owned())];
        let notice = staged_send_summary(0, &failed).expect("a failed send reports");
        assert!(notice.starts_with("Nothing was sent."), "unexpected notice: {notice}");
        assert!(notice.contains("a.png"), "unexpected notice: {notice}");
    }

    /// A fresh fake data dir for the paste-temp tests, in the
    /// layout the ownership rules expect (a `pasted` subdir
    /// beside the root files).
    fn fake_data_dir(name: &str) -> std::path::PathBuf {
        let data = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(data.join("pasted")).expect("fake data dir creates");
        data
    }

    #[test]
    fn two_pastes_in_the_same_second_get_distinct_files() {
        let dir = std::env::temp_dir().join("zaprust-paste-name-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test dir creates");
        // Both names are minted back to back, so they share a
        // timestamp second: the old scheme returned one path
        // for both and the second write clobbered the first.
        let first = pasted_image_path_in(&dir);
        let second = pasted_image_path_in(&dir);
        assert_ne!(first, second, "same-second pastes must not share a path");
        // Identical pixels pasted twice survive as two files.
        std::fs::write(&first, b"same pixels").expect("first paste writes");
        std::fs::write(&second, b"same pixels").expect("second paste writes");
        assert_eq!(std::fs::read(&first).expect("first paste reads"), b"same pixels");
        assert_eq!(std::fs::read(&second).expect("second paste reads"), b"same pixels");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discard_removes_paste_temps_but_never_other_files() {
        let data = fake_data_dir("zaprust-paste-discard-test");
        // A paste in the dedicated dir and a legacy paste in
        // the root are app-owned: both are deleted at the end
        // of their staged life.
        let current = data.join("pasted").join("pasted-1-2-3.png");
        std::fs::write(&current, b"png").expect("paste writes");
        discard_pasted_temp_in(&current, &data);
        assert!(!current.exists(), "a finished paste temp is deleted");
        let legacy = data.join("pasted-123.png");
        std::fs::write(&legacy, b"png").expect("legacy paste writes");
        discard_pasted_temp_in(&legacy, &data);
        assert!(!legacy.exists(), "a legacy paste temp is deleted");
        // Everything else survives: the app's own data files,
        // a root file that merely starts with the prefix, and
        // a user-picked photo elsewhere on disk that happens
        // to carry a paste-like name.
        let settings = data.join("settings.json");
        std::fs::write(&settings, b"{}").expect("settings write");
        discard_pasted_temp_in(&settings, &data);
        assert!(settings.exists(), "data files are never paste temps");
        let notes = data.join("pasted-notes.txt");
        std::fs::write(&notes, b"hi").expect("notes write");
        discard_pasted_temp_in(&notes, &data);
        assert!(notes.exists(), "the legacy pattern only covers PNG pastes");
        let picked_dir = std::env::temp_dir().join("zaprust-paste-picked-test");
        std::fs::create_dir_all(&picked_dir).expect("picked dir creates");
        let picked = picked_dir.join("pasted-123.png");
        std::fs::write(&picked, b"png").expect("picked photo writes");
        discard_pasted_temp_in(&picked, &data);
        assert!(picked.exists(), "a picked file is never deleted");
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_dir_all(&picked_dir);
    }

    #[test]
    fn the_startup_sweep_removes_only_stale_pastes() {
        let data = fake_data_dir("zaprust-paste-sweep-test");
        let stale_a = data.join("pasted").join("pasted-1-2-3.png");
        let stale_b = data.join("pasted").join("pasted-1-2-4.png");
        let legacy = data.join("pasted-999.png");
        for p in [&stale_a, &stale_b, &legacy] {
            std::fs::write(p, b"png").expect("stale paste writes");
        }
        let settings = data.join("settings.json");
        let archive = data.join("archive.db");
        std::fs::write(&settings, b"{}").expect("settings write");
        std::fs::write(&archive, b"db").expect("archive write");
        let exports = data.join("exports");
        std::fs::create_dir_all(&exports).expect("exports dir creates");
        let export = exports.join("chat.txt");
        std::fs::write(&export, b"log").expect("export writes");
        sweep_stale_pasted_images_in(&data);
        assert!(!stale_a.exists() && !stale_b.exists(), "orphaned pastes are swept");
        assert!(!legacy.exists(), "legacy root pastes are swept");
        assert!(settings.exists() && archive.exists(), "data files survive the sweep");
        assert!(export.exists(), "other subdirectories survive the sweep");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn jpeg_extensions_map_to_the_jpeg_mime() {
        assert_eq!(ext_png_jpeg("jpg"), "jpeg");
        assert_eq!(ext_png_jpeg("jpeg"), "jpeg");
        assert_eq!(ext_png_jpeg("png"), "png");
        assert_eq!(ext_png_jpeg("webp"), "webp");
    }

    #[test]
    fn a_real_png_stages_as_a_photo() {
        let ctx = egui::Context::default();
        let dir = std::env::temp_dir().join("zaprust-attach-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("sample.png");
        image::RgbaImage::from_pixel(8, 8, image::Rgba([10u8, 20, 30, 255]))
            .save(&path)
            .expect("test png writes");
        let staged = staged_from_path(&ctx, &path);
        assert_eq!(staged.kind, crate::state::StagedKind::Photo);
        assert_eq!(staged.mime, "image/png");
        assert_eq!(staged.name, "sample.png");
        assert!(!staged.view_once);
        assert!(path.exists(), "staging must not consume the file");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unknown_extension_stages_as_a_document() {
        let ctx = egui::Context::default();
        let staged = staged_from_path(&ctx, std::path::Path::new("/tmp/report.xyz"));
        assert_eq!(staged.kind, crate::state::StagedKind::Document);
        assert_eq!(staged.mime, "application/octet-stream");
        assert!(staged.thumb.is_none(), "documents have no preview");
    }

    /// Section 25: the editor is offered for staged photos
    /// only, and never for a GIF (an edit would flatten its
    /// animation) or a document.
    #[test]
    fn editor_is_offered_for_photos_but_never_gifs() {
        let ctx = egui::Context::default();
        let photo = staged_from_path(&ctx, std::path::Path::new("/tmp/shot.png"));
        assert!(staged_photo_editable(&photo));
        let gif = staged_from_path(&ctx, std::path::Path::new("/tmp/dance.gif"));
        assert_eq!(gif.mime, "image/gif");
        assert!(!staged_photo_editable(&gif), "a GIF is never offered the editor");
        let doc = staged_from_path(&ctx, std::path::Path::new("/tmp/report.pdf"));
        assert!(!staged_photo_editable(&doc));
    }

    /// Section 25: aspect reframes are the largest centred
    /// rectangle of the target aspect, and crop clamping keeps
    /// a rectangle inside the frame with a minimum size.
    #[test]
    fn crop_geometry_reframes_and_clamps() {
        // A 2000x1000 frame squared: full height, centred width.
        let crop = reframed_crop(2000, 1000, 1.0);
        assert_eq!(crop, [0.25, 0.0, 0.75, 1.0]);
        // A portrait frame at 16:9: full width, centred height.
        let crop = reframed_crop(1000, 2000, 16.0 / 9.0);
        assert!((crop[0]).abs() < 1e-6 && (crop[2] - 1.0).abs() < 1e-6);
        assert!((crop[3] - crop[1] - 0.28125).abs() < 1e-4, "{crop:?}");
        // Clamping pulls an out-of-frame rectangle back in.
        let crop = clamped_crop([-0.5, 0.2, 1.4, 0.9]);
        assert_eq!(crop[0], 0.0);
        assert_eq!(crop[2], 1.0);
        // And enforces the minimum size.
        let crop = clamped_crop([0.5, 0.5, 0.51, 0.9]);
        assert!(crop[2] - crop[0] >= 0.05 - 1e-6, "{crop:?}");
    }
}
