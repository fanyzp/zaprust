//! The attach flow: picking files from disk through the portal file
//! dialog, staging them above the composer, and sending them as
//! media messages (photos and videos for the album-style flow,
//! documents as files). Also the location form and the voice
//! recorder wiring live here, since both are composer attachments.

use super::*;
use core::model::{Message, MessageKind};
use state::{now_unix, StagedKind, StagedMedia};

/// Photo extensions the picker accepts and the stager recognizes.
const PHOTO_EXTS: [&str; 5] = ["jpg", "jpeg", "png", "webp", "gif"];
/// Video extensions recognized as staged videos.
const VIDEO_EXTS: [&str; 5] = ["mp4", "mkv", "webm", "mov", "avi"];

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
        for path in paths {
            self.composer.staged.push(staged_from_path(ctx, &path));
        }
        // The stage belongs to the chat it was attached in;
        // `send_staged` refuses to send it anywhere else.
        self.composer.staged_chat = Some(self.selected_chat);
    }

    /// Sends everything staged, as one media message per file
    /// (consecutive sends read as an album in the stream), then
    /// clears the stage. Live reads the bytes now; demo sends the
    /// canned equivalents.
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
            let backend_kind = match item.kind {
                StagedKind::Photo => backend::MediaKind::Photo,
                StagedKind::Video => backend::MediaKind::Video,
                StagedKind::Document => backend::MediaKind::Document,
            };
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
            if self.live_mode {
                let Some(jid) =
                    self.live.as_ref().and_then(|s| s.jids.get(&self.selected_chat).cloned())
                else {
                    self.notice = Some("Open a live chat first.".to_owned());
                    return;
                };
                let bytes = match std::fs::read(&item.path) {
                    Ok(b) => b,
                    Err(e) => {
                        self.notice = Some(format!("Could not read {}: {e}", item.name));
                        return;
                    }
                };
                self.send_backend(backend::BackendCommand::SendMedia {
                    chat_jid: jid,
                    kind: backend_kind,
                    data: bytes,
                    mime: item.mime.clone(),
                    caption: item_caption,
                    file_name: Some(item.name.clone()),
                    gif_playback: false,
                    seconds: 0,
                    view_once,
                    ephemeral_secs: self.ephemeral_secs_of(self.selected_chat),
                });
                // The echo's id follows echo_live_media's counter, so
                // the photo's pixels bind to the same bubble the
                // backend upsert will replace.
                let echo_id = 9_000_000 + self.sent_counter + 1;
                self.echo_live_media(msg_kind, &item.name);
                if let Some(tex) = item.thumb {
                    self.media.textures.insert(echo_id, tex);
                }
            } else {
                let size_kib = u32::try_from(
                    std::fs::metadata(&item.path).map(|m| m.len()).unwrap_or(0) / 1024,
                )
                .unwrap_or(0);
                self.sent_counter += 1;
                let msg_id = 900 + self.sent_counter;
                let kind = match item.kind {
                    StagedKind::Document => {
                        MessageKind::Document { name: item.name.clone(), size_kib }
                    }
                    _ => msg_kind,
                };
                let (stamp, expires_at) = self.demo_send_stamp();
                let msg =
                    Message { kind, expires_at, ..Message::mine(msg_id, String::new(), stamp) };
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
        }
        *engaged = true;
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
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
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
    use super::{ext_png_jpeg, staged_from_path};

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
