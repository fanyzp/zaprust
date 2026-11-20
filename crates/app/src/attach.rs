//! The attach flow: picking files from disk through the portal file
//! dialog, staging them above the composer, and sending them as
//! media messages (photos and videos for the album-style flow,
//! documents as files). Also the location form and the voice
//! recorder wiring live here, since both are composer attachments.

use super::*;
use core::model::{Message, MessageKind};
use state::{now_unix, StagedKind, StagedMedia, StagedSendState};
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
            self.notice = Some(format!("Not attached: {}.", refused.join(", ")));
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
            self.composer.staged.clear();
            self.composer.staged_caption.clear();
            self.composer.staged_chat = None;
            self.notice =
                Some("Those files were attached in a different chat and were not sent.".to_owned());
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
                StagedKind::Video => {
                    MessageKind::Video { caption: item_caption.clone(), view_once, secs: 0 }
                }
                StagedKind::Document => {
                    MessageKind::Document { name: item.name.clone(), size_kib: 0 }
                }
            };
            let size_kib =
                u32::try_from(std::fs::metadata(&item.path).map(|m| m.len()).unwrap_or(0) / 1024)
                    .unwrap_or(0);
            self.sent_counter += 1;
            let msg_id = 900 + self.sent_counter;
            let kind = match item.kind {
                StagedKind::Document => MessageKind::Document { name: item.name.clone(), size_kib },
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
            self.notice =
                Some("Still sending the previous files; wait for those to finish.".to_owned());
            return;
        }
        let Some(jid) = self.live_jid(self.selected_chat) else {
            // Nothing is taken: the stage stays put for a retry
            // once a live chat is open.
            self.notice = Some("Open a live chat first.".to_owned());
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
        }
        if disconnected {
            if let Some(mut send) = self.staged_send.take() {
                while let Some(entry) = send.pending.pop_front() {
                    send.failed.push((entry.item.name.clone(), "could not be read".to_owned()));
                }
                if let Some(notice) = staged_send_summary(send.sent, &send.failed) {
                    self.notice = Some(notice);
                }
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
        let msg_kind = match item.kind {
            StagedKind::Photo => MessageKind::Image { caption: caption.clone(), view_once },
            StagedKind::Video => {
                MessageKind::Video { caption: caption.clone(), view_once, secs: 0 }
            }
            StagedKind::Document => MessageKind::Document { name: item.name.clone(), size_kib: 0 },
        };
        self.send_backend(backend::BackendCommand::SendMedia {
            chat_jid: jid,
            kind: backend_kind,
            data: bytes,
            mime: item.mime.clone(),
            caption,
            file_name: Some(item.name.clone()),
            gif_playback: false,
            seconds: 0,
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
            self.notice = Some(crate::i18n::t("attach.location_needs_numbers"));
            return;
        };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            self.composer.location_draft = Some(draft);
            self.notice = Some(crate::i18n::t("attach.location_needs_numbers"));
            return;
        }
        let name = draft.name.trim().to_owned();
        if self.live_mode {
            let Some(jid) =
                self.live.as_ref().and_then(|s| s.jids.get(&self.selected_chat).cloned())
            else {
                self.notice = Some("Open a live chat first.".to_owned());
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
    /// field); the pixels land in a timestamped PNG under the data
    /// dir so the staged-file flow sends them like any picked
    /// photo.
    pub(crate) fn stage_pasted_image(&mut self, ctx: &egui::Context, image: egui::ColorImage) {
        let [w, h] = image.size;
        let rgba = image::RgbaImage::from_fn(w as u32, h as u32, |x, y| {
            let c = image.pixels[y as usize * w + x as usize];
            image::Rgba([c.r(), c.g(), c.b(), c.a()])
        });
        let path = backend::data_dir().join(format!("pasted-{}.png", now_unix()));
        if let Err(e) = rgba.save(&path) {
            self.notice = Some(format!("Could not save the pasted image: {e}"));
            return;
        }
        self.stage_files(ctx, vec![path]);
    }

    /// Starts a voice recording (the composer's mic button). An
    /// unavailable microphone surfaces an honest notice.
    pub(crate) fn start_recording(&mut self) {
        if self.recorder.is_some() {
            return;
        }
        match crate::recorder::Recorder::start() {
            Ok(rec) => self.recorder = Some(rec),
            Err(e) => self.notice = Some(e),
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
                        self.notice = Some("Open a live chat first.".to_owned());
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
            Err(e) => self.notice = Some(e),
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

#[cfg(test)]
mod tests {
    use super::{
        ext_png_jpeg, read_staged_file, run_staged_reads, staged_from_path, staged_send_summary,
        staged_size_problem, StagedReadJob, MAX_SEND_BYTES,
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
}
