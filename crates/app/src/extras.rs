//! Host-side glue for the native features and rich messaging:
//! persisted feature state, settings persistence, preview
//! workers, and the per-frame expiry tick.

use std::path::PathBuf;

use core::model::{Message, MessageKind};

use crate::state::{now_unix, App};

/// Messages arriving from GIPHY worker threads.
pub(crate) enum GiphyMsg {
    /// Trending/search results.
    Items(Result<Vec<giphy::GifItem>, String>),
    /// One decoded preview image: (preview URL, PNG/JPEG bytes).
    Preview(String, Vec<u8>),
}

/// One finished live-media download: the MP4 bytes for a GIF send,
/// targeted at the chat that was open when the user clicked.
pub(crate) struct MediaFetchResult {
    /// Live chat JID the media belongs to.
    pub(crate) jid: String,
    /// GIF title, used as the video caption.
    pub(crate) title: String,
    /// Downloaded bytes, or the fetch error.
    pub(crate) result: Result<Vec<u8>, String>,
}

/// Directory holding per-feature JSON state files. The folder
/// name predates the native overhaul (it was the plugin-state
/// directory); it is kept so existing state files stay in
/// place across the upgrade.
pub(crate) fn feature_state_dir() -> PathBuf {
    backend::data_dir().join("plugin-state")
}

/// Reads a state file's text, tolerating a missing file.
fn read_state_file(name: &str) -> Option<String> {
    read_json_file(&feature_state_dir().join(name))
}

fn read_json_file(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    // A file that exists but does not parse as JSON is corrupt
    // (torn write, disk trouble). Quarantine it aside and behave as
    // if it were absent, instead of silently defaulting over the
    // only copy of the user's data (logic review F12).
    if !text.trim().is_empty() && serde_json::from_str::<serde_json::Value>(&text).is_err() {
        let aside = path.with_extension(format!("corrupt-{}", now_unix()));
        let _ = std::fs::rename(path, &aside);
        tracing::warn!("quarantined corrupt state file: {}", path.display());
        return None;
    }
    Some(text)
}

fn write_json_file(path: &std::path::Path, text: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    write_file_atomic(path, text.as_bytes());
}

/// Atomic file replacement: write a sibling temp file, flush it to
/// disk, then rename over the target. A crash or power loss mid-save
/// can no longer leave a torn file behind (logic review F12); the
/// rename is atomic on both Unix and Windows for same-directory
/// paths.
pub(crate) fn write_file_atomic(path: &std::path::Path, bytes: &[u8]) {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let written = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Shared with quick_replies.rs (same persistence helpers).
pub(crate) fn read_json_file_pub(path: &std::path::Path) -> Option<String> {
    read_json_file(path)
}

/// Shared with quick_replies.rs (same persistence helpers).
pub(crate) fn write_json_file_pub(path: &std::path::Path, text: &str) {
    write_json_file(path, text);
}

/// Whether a live message body is media-only, as a pure
/// predicate so it is testable without a window or a backend.
///
/// The old check called any body wrapped in brackets media, so
/// a plain "[lol]" was skipped. The backends only ever produce
/// a small set of bracketed labels for a message with no text
/// of its own: the fake backend labels sent media "[photo]",
/// "[video]", "[voice note]", "[document]" and "[sticker]",
/// and the real backend falls back to "[non-text message]" for
/// a non-text payload with no caption. Both producers pair the
/// label with a non-Text kind, so a genuine text message is
/// never media-only, even when its text matches a label. Media
/// from the fake scripted set carries an empty body instead of
/// a label, so that case counts too; media with a caption
/// carries the caption as its body and is answered on that text.
#[must_use]
/// The localized label for one disappearing-messages duration
/// (`0` reads "Off"). Shared by the info-dock options and the
/// system notice, so the two always name a duration the same way.
pub(crate) fn ephemeral_duration_label(duration_secs: u32) -> String {
    match duration_secs {
        0 => crate::i18n::t("profile.disappearing_off"),
        86_400 => crate::i18n::t("profile.duration_24h"),
        604_800 => crate::i18n::t("profile.duration_7d"),
        7_776_000 => crate::i18n::t("profile.duration_90d"),
        _ => crate::i18n::tf("profile.duration_seconds", "count", &duration_secs.to_string()),
    }
}

/// The system-notice text announcing a disappearing-timer change.
pub(crate) fn disappearing_notice_text(duration_secs: u32) -> String {
    if duration_secs == 0 {
        crate::i18n::t("conversation.disappearing_off_notice")
    } else {
        crate::i18n::tf(
            "conversation.disappearing_set",
            "duration",
            &ephemeral_duration_label(duration_secs),
        )
    }
}

/// One media-picker tab contributed by a native feature, as the
/// picker strip sees it.
pub(crate) struct PickerTab {
    /// Owning feature id ("giphy", "stickers").
    pub(crate) owner: String,
    /// Tab id ("gif", "stickers").
    pub(crate) tab_id: String,
    /// Tab label shown in the picker strip.
    pub(crate) label: String,
}

impl App {
    /// Whether a native feature is switched on. Accepts the short
    /// feature id ("giphy") and, for call sites not yet migrated,
    /// the legacy plugin id spelling ("community.giphy").
    pub(crate) fn feature_active(&self, id: &str) -> bool {
        let short = id.strip_prefix("community.").unwrap_or(id);
        self.feature_flags.get(short).copied().unwrap_or(false)
    }

    /// The composer's live quick replies: the user's saved list
    /// while the Quick Replies feature is on, nothing while it
    /// is off. The persisted list (seeded with the built-in set
    /// on first run) is owned by the conversation quick replies
    /// module; the panel and this expansion path read the same
    /// snapshot from it.
    pub(crate) fn quick_replies(&self) -> Vec<quick_replies::QuickReply> {
        if self.feature_active("quick-replies") {
            crate::conversation::quick_replies::items()
        } else {
            Vec::new()
        }
    }

    /// Expands a leading quick-reply trigger in `text` (see the
    /// quick-replies feature crate), unchanged when the feature is
    /// off or nothing matches.
    pub(crate) fn expand_quick_reply(&self, text: &str) -> String {
        quick_replies::expand(text, &self.quick_replies())
    }

    /// The media picker's feature tabs (GIF search, Stickers).
    /// The host renders the strip from this list and each tab's
    /// content by tab id; a tab whose feature is off is skipped
    /// by the caller via `feature_active`.
    pub(crate) fn picker_tabs() -> Vec<PickerTab> {
        vec![
            PickerTab {
                owner: "giphy".to_owned(),
                tab_id: giphy::TAB_ID.to_owned(),
                label: "GIF".to_owned(),
            },
            PickerTab {
                owner: "stickers".to_owned(),
                tab_id: stickers::TAB_ID.to_owned(),
                label: "Stickers".to_owned(),
            },
        ]
    }

    /// Loads persisted settings + feature state from the data dir.
    pub(crate) fn load_persisted_state(&mut self) {
        let mut had_features = false;
        let mut had_giphy = false;
        let dir = backend::data_dir();
        if let Some(text) = read_json_file(&dir.join("settings.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                self.read_receipts = v["read_receipts"].as_bool().unwrap_or(true);
                self.default_ephemeral_secs =
                    v["default_ephemeral_secs"].as_u64().unwrap_or(0) as u32;
                self.typing_indicators = v["typing_indicators"].as_bool().unwrap_or(true);
                self.link_previews = v["link_previews"].as_bool().unwrap_or(true);
                if let Some(s) = v["saturation"].as_f64() {
                    self.saturation = s as f32;
                }
                if let Some(c) = v["contrast"].as_f64() {
                    self.contrast = c as f32;
                }
                if let Some(g) = v["glow"].as_bool() {
                    self.glow = g;
                }
                if let Some(a) = v["seed"].as_array() {
                    if a.len() == 3 {
                        self.seed = core::theme::Rgb(
                            a[0].as_u64().unwrap_or(0) as u8,
                            a[1].as_u64().unwrap_or(0) as u8,
                            a[2].as_u64().unwrap_or(0) as u8,
                        );
                    }
                }
                self.notify_messages = v["notify_messages"].as_bool().unwrap_or(true);
                self.notify_groups = v["notify_groups"].as_bool().unwrap_or(true);
                self.notify_sound = v["notify_sound"].as_bool().unwrap_or(true);
                self.notify_preview = v["notify_preview"].as_bool().unwrap_or(true);
                self.composer.hint_dismissed =
                    v["composer_hint_dismissed"].as_bool().unwrap_or(false);
                if let Some(m) = v["motion"].as_str() {
                    self.motion = crate::motion::MotionSetting::from_str_or_full(m);
                }
                if let Some(l) = v["language"].as_str() {
                    self.language = l.to_owned();
                    // Under test, App::new runs in many suites that
                    // do not care about the developer's real
                    // settings.json; applying it here flips the
                    // global locale under the i18n tests' feet.
                    // Real launches always apply it.
                    if !cfg!(test) {
                        crate::i18n::set_locale(l);
                    }
                }
                if let Some(u) = v["update_last_check"].as_u64() {
                    self.update.last_check = u;
                }
                // "My profile" identity: seed the editor state
                // once from the persisted keys. Absent keys leave
                // the defaults (empty name/about, no photo).
                if let Some(n) = v["my_name"].as_str() {
                    self.my_profile.name = n.to_owned();
                }
                if let Some(a) = v["my_about"].as_str() {
                    self.my_profile.about = a.to_owned();
                }
                if let Some(p) = v["my_photo_path"].as_str() {
                    if !p.is_empty() {
                        self.my_profile.photo_path = Some(p.to_owned());
                        self.my_profile.photo_input = p.to_owned();
                    }
                }
                if let Some(f) = v["features"].as_object() {
                    had_features = true;
                    for (id, on) in f {
                        if let Some(b) = on.as_bool() {
                            self.feature_flags.insert(id.clone(), b);
                        }
                    }
                }
                if let Some(g) = v["giphy_prefs"].as_object() {
                    had_giphy = true;
                    self.giphy_prefs = g.clone();
                }
            }
        }
        // The my-profile seed ran above (or the settings file does
        // not exist yet, in which case the defaults ARE the seed):
        // either way the editor must never overwrite the state
        // with defaults after this point.
        self.my_profile.seeded = true;
        self.migrate_legacy_plugin_state(had_features, had_giphy);
    }

    /// One-time migration from the pre-native plugin bookkeeping:
    /// the install book (which features were installed + enabled)
    /// seeds the native feature switches, and the per-plugin
    /// settings file seeds the GIF preferences. Runs only when
    /// the native settings carry no value yet, so it never
    /// overrides a choice made in the native Settings.
    fn migrate_legacy_plugin_state(&mut self, had_features: bool, had_giphy: bool) {
        if !had_features {
            if let Some(text) = read_state_file("catalog.json") {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(installed) = v["installed"].as_object() {
                        for id in crate::state::FEATURE_IDS {
                            self.feature_flags.insert((*id).to_owned(), false);
                        }
                        for (id, on) in installed {
                            let short = id.strip_prefix("community.").unwrap_or(id).to_owned();
                            self.feature_flags.insert(short, on.as_bool().unwrap_or(false));
                        }
                    }
                }
            }
        }
        if let Some(text) = read_state_file("plugin-settings.json") {
            if let Ok(all) =
                serde_json::from_str::<std::collections::BTreeMap<String, serde_json::Value>>(&text)
            {
                if !had_giphy {
                    if let Some(g) = all.get("giphy").and_then(|v| v.as_object()) {
                        self.giphy_prefs = g.clone();
                    }
                }
            }
        }
    }

    /// Persists the settings the user can change (privacy toggles,
    /// appearance fine-tuning, feature configuration).
    pub(crate) fn persist_settings(&self) {
        let v = serde_json::json!({
            "read_receipts": self.read_receipts,
            "default_ephemeral_secs": self.default_ephemeral_secs,
            "typing_indicators": self.typing_indicators,
            "link_previews": self.link_previews,
            "saturation": self.saturation,
            "contrast": self.contrast,
            "glow": self.glow,
            "seed": [self.seed.0, self.seed.1, self.seed.2],
            "notify_messages": self.notify_messages,
            "notify_groups": self.notify_groups,
            "notify_sound": self.notify_sound,
            "notify_preview": self.notify_preview,
            "composer_hint_dismissed": self.composer.hint_dismissed,
            "motion": self.motion.as_str(),
            "language": self.language,
            "update_last_check": self.update.last_check,
            "features": self.feature_flags,
            "giphy_prefs": self.giphy_prefs,
            "my_name": self.my_profile.name,
            "my_about": self.my_profile.about,
            "my_photo_path": self.my_profile.photo_path,
        });
        write_json_file(&backend::data_dir().join("settings.json"), &v.to_string());
    }

    /// Starts a link-preview fetch for a live message (worker
    /// thread, 5s timeout, result cached by URL).
    pub(crate) fn maybe_fetch_preview(&mut self, msg_id: u64, url: String) {
        if let Some(cached) = self.og_cache.get(&url).cloned() {
            self.apply_preview(msg_id, cached);
            return;
        }
        if self.og_rx.is_some() {
            return; // one fetch at a time; later messages retry next frame
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let fetch_url = url.clone();
        std::thread::Builder::new()
            .name("zaprust-og-fetch".to_owned())
            .spawn(move || {
                let data = fetch_open_graph(&fetch_url);
                let _ = tx.send((msg_id, fetch_url, data));
            })
            .ok();
        self.og_rx = Some(rx);
    }

    fn apply_preview(&mut self, msg_id: u64, data: Option<core::rich::OgData>) {
        let Some(data) = data else {
            return;
        };
        let preview = core::model::LinkPreview {
            title: data.title,
            description: data.description,
            url: String::new(),
            // Live fetches carry no image; the text card renders alone.
            image: core::model::PreviewImage::None,
        };
        // Apply to the live session copy and the visible conversation.
        if let Some(session) = &mut self.live {
            for msgs in session.messages.values_mut() {
                for (_, m) in msgs.iter_mut() {
                    if m.id == msg_id {
                        let mut p = preview.clone();
                        if p.url.is_empty() {
                            p.url = core::rich::first_url(&m.body).unwrap_or_default();
                        }
                        m.link_preview = Some(p);
                    }
                }
            }
        }
        for m in &mut self.conversation {
            if m.id == msg_id && m.link_preview.is_none() {
                let mut p = preview.clone();
                if p.url.is_empty() {
                    p.url = core::rich::first_url(&m.body).unwrap_or_default();
                }
                m.link_preview = Some(p);
            }
        }
    }

    /// Polls all worker channels (GIPHY, previews).
    /// Called every frame from the shell.
    pub(crate) fn poll_workers(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.og_rx {
            match rx.try_recv() {
                Ok((msg_id, url, data)) => {
                    self.og_cache.insert(url, data.clone());
                    self.apply_preview(msg_id, data);
                    self.og_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.og_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        // GIPHY results: drain the channel first (ending the
        // borrow), then apply.
        if self.picker.giphy_rx.is_some() {
            let mut drained: Vec<GiphyMsg> = Vec::new();
            let mut disconnected = false;
            if let Some(rx) = &self.picker.giphy_rx {
                loop {
                    match rx.try_recv() {
                        Ok(msg) => drained.push(msg),
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
            }
            let mut failed = false;
            let mut replaced = false;
            for msg in drained {
                match msg {
                    GiphyMsg::Items(Ok(items)) => {
                        self.picker.giphy_items = items;
                        self.picker.giphy_rejected = false;
                        replaced = self.load_visible_giphy_previews(ctx);
                    }
                    GiphyMsg::Items(Err(e)) => {
                        if e.contains("401") || e.contains("403") {
                            self.picker.giphy_rejected = true;
                        }
                        self.notice = Some(format!("GIF: {e}"));
                        failed = true;
                    }
                    GiphyMsg::Preview(url, bytes) => {
                        if let Some(img) = decode_image_limited(&bytes) {
                            let rgba = img.to_rgba8();
                            let size = [rgba.width() as usize, rgba.height() as usize];
                            let color =
                                egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                            let handle = ctx.load_texture(
                                format!("giphy-{url}"),
                                color,
                                egui::TextureOptions::LINEAR,
                            );
                            self.picker.giphy_textures.insert(url, handle);
                        }
                    }
                }
            }
            if failed || (disconnected && !replaced) {
                self.picker.giphy_rx = None;
            }
        }
        // Live GIF send: the MP4 download finished (or failed).
        if self.picker.media_fetch_rx.is_some() {
            let mut results: Vec<MediaFetchResult> = Vec::new();
            let mut disconnected = false;
            if let Some(rx) = &self.picker.media_fetch_rx {
                loop {
                    match rx.try_recv() {
                        Ok(msg) => results.push(msg),
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
            }
            for done in results {
                match done.result {
                    Ok(bytes) => self.complete_live_gif_send(&done.jid, &done.title, bytes),
                    Err(e) => self.notice = Some(format!("GIF: {e}")),
                }
            }
            if disconnected {
                self.picker.media_fetch_rx = None;
            }
        }
    }

    /// Starts a GIPHY trending fetch (worker thread).
    pub(crate) fn start_giphy_trending(&mut self) {
        self.start_giphy_fetch(None);
    }

    /// Starts a GIPHY search fetch (worker thread).
    pub(crate) fn start_giphy_search(&mut self) {
        let q = self.picker.giphy_query.trim().to_owned();
        if !q.is_empty() {
            self.start_giphy_fetch(Some(q));
        }
    }

    fn start_giphy_fetch(&mut self, query: Option<String>) {
        if self.picker.giphy_rx.is_some() {
            return;
        }
        let mut pool = self.credentials.pool_for(giphy::KEY_SLOT);
        let stored = self.giphy_prefs.clone();
        let (limit, rating) = giphy::fetch_params(&stored);
        let (tx, rx) = std::sync::mpsc::channel::<GiphyMsg>();
        std::thread::Builder::new()
            .name("zaprust-giphy".to_owned())
            .spawn(move || {
                let result = match &query {
                    Some(q) => giphy::search(&mut pool, q, limit, &rating, now_unix()),
                    None => giphy::trending(&mut pool, limit, &rating, now_unix()),
                };
                let _ = tx.send(GiphyMsg::Items(result.map_err(|e| e.to_string())));
            })
            .ok();
        self.picker.giphy_rx = Some(rx);
    }

    /// Fetches preview images for the current GIF items that have
    /// no texture yet (worker thread per batch). Returns true when
    /// a new worker channel replaced `giphy_rx`.
    pub(crate) fn load_visible_giphy_previews(&mut self, ctx: &egui::Context) -> bool {
        let _ = ctx;
        let missing: Vec<String> = self
            .picker
            .giphy_items
            .iter()
            .map(|i| i.preview_url.clone())
            .filter(|u| !self.picker.giphy_textures.contains_key(u))
            .collect();
        if missing.is_empty() {
            return false;
        }
        let (tx, rx) = std::sync::mpsc::channel::<GiphyMsg>();
        std::thread::Builder::new()
            .name("zaprust-giphy-previews".to_owned())
            .spawn(move || {
                for url in missing {
                    if let Ok(bytes) = giphy::fetch_preview(&url) {
                        let _ = tx.send(GiphyMsg::Preview(url, bytes));
                    }
                }
            })
            .ok();
        self.picker.giphy_rx = Some(rx);
        true
    }

    /// Per-frame expiry tick: disappearing messages expire on
    /// their own schedule; once per second is plenty for a
    /// per-message timer. Cheap when nothing is due.
    pub(crate) fn tick_expiry(&mut self) {
        let now = now_unix();
        if now != self.last_expiry_scan {
            self.last_expiry_scan = now;
            self.expire_disappearing_messages(now);
        }
    }

    /// Display stamp and expiry second for a demo-mode send
    /// into the selected chat. A chat with a disappearing timer
    /// runs on the real clock (real time stamp, a real expiry),
    /// so the timer's effect is genuine in demo state; timer-off
    /// chats keep the dataset's fixed "19:36" stamp and no
    /// expiry.
    pub(crate) fn demo_send_stamp(&self) -> (String, u64) {
        let timer = self.ephemeral_secs_of(self.selected_chat);
        if timer > 0 {
            let now = now_unix();
            (crate::live::fmt_time(now as i64), now + u64::from(timer))
        } else {
            ("19:36".to_owned(), 0)
        }
    }

    /// The disappearing-messages timer of the chat behind a
    /// live JID (`0` when unknown or off).
    pub(crate) fn ephemeral_secs_for_jid(&self, jid: &str) -> u32 {
        self.live
            .as_ref()
            .and_then(|s| s.jids.iter().find(|(_, j)| *j == jid).map(|(id, _)| *id))
            .map_or(0, |id| self.ephemeral_secs_of(id))
    }

    /// The disappearing-messages timer of one chat (`0` when the
    /// chat is unknown or its timer is off).
    pub(crate) fn ephemeral_secs_of(&self, chat_id: u64) -> u32 {
        self.chats
            .iter()
            .find(|c| c.id == chat_id)
            .or_else(|| self.detached_chat.as_ref().filter(|c| c.id == chat_id))
            .map_or(0, |c| c.ephemeral_secs)
    }

    /// Pushes a centred system notice into one chat's stream
    /// (the disappearing-timer announcements use this). Demo mode
    /// mirrors the composer pattern: the chat's stored messages
    /// and, when the chat is open, the rendered conversation both
    /// get the row. Live mode appends to the session mirror; like
    /// every system row it is session-local, the archive cannot
    /// represent system messages.
    pub(crate) fn push_system_notice(&mut self, chat_id: u64, text: &str) {
        self.sent_counter += 1;
        let mut system_msg = Message::mine(9_400_000 + self.sent_counter, text.to_owned(), "");
        system_msg.kind = MessageKind::System;
        system_msg.delivery = None;
        // A notice belongs to the day it appears on; without a
        // day label the stream would file it under the previous
        // separator.
        system_msg.time = crate::live::fmt_time(now_unix() as i64);
        system_msg.day = crate::live::fmt_day(now_unix() as i64);
        if self.live_mode {
            if let Some(session) = &mut self.live {
                session.messages.entry(chat_id).or_default().push((now_unix() as i64, system_msg));
            }
            if self.selected_chat == chat_id {
                self.sync_conversation();
            }
        } else {
            if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
                chat.messages.push(system_msg.clone());
            } else if let Some(chat) = self.detached_chat.as_mut().filter(|c| c.id == chat_id) {
                chat.messages.push(system_msg.clone());
            }
            if self.selected_chat == chat_id {
                self.conversation.push(system_msg);
            }
        }
    }

    /// The disappearing-messages expiry pass: removes every
    /// message whose timer has run out, from the rendered state
    /// and, in live mode, from the archive as well. Runs at most
    /// once per second from the tick; the per-message decision is
    /// the pure `Message::is_expired` against the supplied clock.
    pub(crate) fn expire_disappearing_messages(&mut self, now: u64) {
        if self.live_mode {
            // Collect first (chat, app message id, protocol id):
            // the removals below borrow the session mutably.
            let mut work: Vec<(u64, u64, Option<String>)> = Vec::new();
            if let Some(session) = &self.live {
                for (chat_id, msgs) in &session.messages {
                    for (_, m) in msgs {
                        if m.is_expired(now) {
                            work.push((*chat_id, m.id, self.protocol_ids.get(&m.id).cloned()));
                        }
                    }
                }
            }
            if work.is_empty() {
                return;
            }
            let mut touched_selected = false;
            if let Some(session) = &mut self.live {
                for (chat_id, msgs) in &mut session.messages {
                    let before = msgs.len();
                    msgs.retain(|(_, m)| !m.is_expired(now));
                    if msgs.len() != before && *chat_id == self.selected_chat {
                        touched_selected = true;
                    }
                }
                if let Some(store) = &session.store {
                    for (chat_id, _, pid) in &work {
                        if let (Some(jid), Some(pid)) = (session.jids.get(chat_id), pid) {
                            let _ = store.delete_message(jid, pid);
                        }
                    }
                }
            }
            // A chat whose newest message expired shows the new
            // newest message as its preview.
            for (chat_id, _, _) in &work {
                let last = self.live.as_ref().and_then(|s| {
                    s.messages.get(chat_id).and_then(|v| v.last().map(|(ts, m)| (*ts, m.clone())))
                });
                if let Some(chat) = self.chats.iter_mut().find(|c| c.id == *chat_id) {
                    match last {
                        Some((ts, m)) => {
                            chat.preview = m.body.clone();
                            chat.time = crate::live::fmt_time(ts);
                        }
                        None => {
                            chat.preview = String::new();
                        }
                    }
                }
            }
            if touched_selected {
                self.sync_conversation();
            }
        } else {
            let mut touched_selected = false;
            for chat in &mut self.chats {
                let expired = core::model::expired_message_ids(&chat.messages, now);
                if expired.is_empty() {
                    continue;
                }
                chat.messages.retain(|m| !expired.contains(&m.id));
                match chat.messages.last() {
                    Some(last) => {
                        chat.preview = last.body.clone();
                        chat.time = last.time.clone();
                        chat.sync_preview_from_last();
                    }
                    None => {
                        chat.preview = String::new();
                        chat.time = String::new();
                    }
                }
                if chat.id == self.selected_chat {
                    touched_selected = true;
                }
            }
            if let Some(chat) = &mut self.detached_chat {
                let expired = core::model::expired_message_ids(&chat.messages, now);
                if !expired.is_empty() {
                    chat.messages.retain(|m| !expired.contains(&m.id));
                    if chat.id == self.selected_chat {
                        touched_selected = true;
                    }
                }
            }
            if touched_selected {
                self.conversation.retain(|m| !m.is_expired(now));
            }
        }
    }
}

/// Decodes image bytes with explicit dimension and allocation
/// limits (security audit SEC-F03). The bytes come from a remote
/// server, so a small compressed file must not be able to expand
/// into an unbounded bitmap on the UI thread; over-limit or
/// undecodable input simply yields no image. The public-address
/// validation that used to live beside this helper moved to
/// `core::net`, shared with the GIPHY media fetch.
pub(crate) fn decode_image_limited(bytes: &[u8]) -> Option<image::DynamicImage> {
    let mut reader =
        image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().ok()
}

/// Cap on cached link previews. Past it the oldest entries are
/// evicted; without a bound the cache grew for the whole session
/// (SEC-F06).
const MAX_OG_CACHE_ENTRIES: usize = 256;

/// Bounded link-preview cache by URL. Insertion order is kept in
/// a plain Vec and the oldest entry is evicted first once the
/// cache grows past [`MAX_OG_CACHE_ENTRIES`]; at this size a
/// linear lookup is cheap and no extra dependency is needed.
/// A cached `None` means the URL was fetched and had no preview;
/// it still counts as cached so it is not fetched again.
#[derive(Default)]
pub(crate) struct OgCache {
    entries: Vec<(String, Option<core::rich::OgData>)>,
}

impl OgCache {
    pub(crate) fn get(&self, url: &str) -> Option<&Option<core::rich::OgData>> {
        self.entries.iter().find(|(key, _)| key == url).map(|(_, data)| data)
    }

    pub(crate) fn insert(&mut self, url: String, data: Option<core::rich::OgData>) {
        if let Some(slot) = self.entries.iter_mut().find(|(key, _)| *key == url) {
            slot.1 = data;
            return;
        }
        self.entries.push((url, data));
        while self.entries.len() > MAX_OG_CACHE_ENTRIES {
            self.entries.remove(0);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Builds the link-preview client for one fetch hop: the hop's
/// URL is validated and resolved once through `core::net`, and
/// the client is pinned to exactly those addresses, so the name
/// is never resolved a second time at connect and a rebinding
/// answer cannot slip a private address past the check (SEC-F05).
/// IP-literal hosts need no pin; there is no name to rebind.
/// Returns `None` when the URL fails validation.
fn pinned_preview_client(url: &str) -> Option<reqwest::blocking::Client> {
    let target = core::net::validated_target(url)?;
    let host = reqwest::Url::parse(url).ok()?.host_str()?.to_owned();
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none());
    if host.parse::<std::net::IpAddr>().is_err() {
        builder = builder.resolve_to_addrs(&host, &target.addrs);
    }
    builder.build().ok()
}

/// Fetches a page and parses its OpenGraph metadata (blocking,
/// 5-second timeout). Never called in demo mode. Hardened per
/// security audit SEC-01/SEC-13: https only, public hosts only,
/// redirects followed manually so every hop is re-validated and
/// pinned to its validated addresses (SEC-F05), and the body read
/// is capped instead of buffered whole.
fn fetch_open_graph(url: &str) -> Option<core::rich::OgData> {
    use std::io::Read;
    let mut current = url.to_owned();
    for _ in 0..=3 {
        let client = pinned_preview_client(&current)?;
        let mut resp = client.get(&current).send().ok()?;
        if resp.status().is_redirection() {
            let location = resp.headers().get("location")?.to_str().ok()?.to_owned();
            let next = reqwest::Url::parse(&current).ok()?.join(&location).ok()?;
            current = next.to_string();
            continue;
        }
        if !resp.status().is_success() {
            return None;
        }
        let mut buf = Vec::new();
        resp.by_ref().take(512 * 1024).read_to_end(&mut buf).ok()?;
        let html = String::from_utf8_lossy(&buf);
        return core::rich::parse_open_graph(&html);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_client_builds_for_a_public_literal() {
        // SEC-F05: client construction itself needs no network;
        // a public literal passes validation and builds.
        assert!(pinned_preview_client("https://8.8.8.8/page").is_some());
    }

    #[test]
    fn og_cache_evicts_the_oldest_entries_past_the_cap() {
        // SEC-F06 regression: the cache must stay bounded, with
        // the earliest inserts evicted first.
        let mut cache = OgCache::default();
        for i in 0..MAX_OG_CACHE_ENTRIES + 2 {
            cache.insert(format!("https://example.com/{i}"), None);
        }
        assert!(cache.get("https://example.com/0").is_none());
        assert!(cache.get("https://example.com/1").is_none());
        assert!(cache.get("https://example.com/2").is_some());
        let last = format!("https://example.com/{}", MAX_OG_CACHE_ENTRIES + 1);
        assert!(cache.get(&last).is_some());
    }

    #[test]
    fn og_cache_reinsert_replaces_in_place() {
        let mut cache = OgCache::default();
        cache.insert("https://example.com/a".to_owned(), None);
        let data = core::rich::OgData { title: "Title".to_owned(), description: "Desc".to_owned() };
        cache.insert("https://example.com/a".to_owned(), Some(data));
        let cached = cache.get("https://example.com/a").expect("entry stays cached");
        assert_eq!(cached.as_ref().map(|d| d.title.as_str()), Some("Title"));
        cache.clear();
        assert!(cache.get("https://example.com/a").is_none());
    }

    #[test]
    fn pinned_client_rejects_non_public_targets() {
        for bad in [
            "http://8.8.8.8/page",
            "https://127.0.0.1/admin",
            "https://192.168.1.1/page",
            "https://example.com@10.0.0.5/page",
            "not a url",
        ] {
            assert!(pinned_preview_client(bad).is_none(), "{bad} must be rejected");
        }
    }
}
