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

/// One sticker preview decoded by a worker thread: the file it
/// belongs to, the file's stamp when the worker read it, and the
/// tile-sized RGBA pixels (None when the file could not be read
/// or decoded). The draw path never decodes; it only queues
/// requests and binds the textures this message produces.
pub(crate) struct StickerPreviewMsg {
    /// Sticker file the preview belongs to.
    pub(crate) path: PathBuf,
    /// The file's stamp at read time; a failure is remembered
    /// against it, so a replaced file is retried and an unchanged
    /// one is not.
    pub(crate) stamp: Option<StickerFileStamp>,
    /// Decoded RGBA pixels and their [width, height], or None
    /// when the read or decode failed.
    pub(crate) pixels: Option<(Vec<u8>, [usize; 2])>,
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
        // These files carry user content (settings, quick
        // replies, plugin state): owner-only on Unix, asserted
        // on the temp before any bytes land and inherited by the
        // target through the rename (privacy audit A5).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
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
                (had_features, had_giphy) = self.apply_settings_document(&v);
            }
        }
        // The my-profile seed ran above (or the settings file does
        // not exist yet, in which case the defaults ARE the seed):
        // either way the editor must never overwrite the state
        // with defaults after this point.
        self.my_profile.seeded = true;
        // The settings load parks every composer draft by chat
        // id, the open chat's included; the composer holds the
        // open one directly, so move it over before any chat
        // switch can park state again.
        self.restore_open_draft();
        self.migrate_legacy_plugin_state(had_features, had_giphy);
    }

    /// Applies one parsed settings document to the app state:
    /// the load half of the settings round trip, exactly what
    /// [`App::load_persisted_state`] runs over settings.json.
    /// Split out so tests can run a document produced by
    /// [`App::settings_json`] through the real load path without
    /// touching the real data dir. Returns whether the document
    /// carried feature flags and GIF preferences (the legacy
    /// migration only seeds what the document does not carry).
    pub(crate) fn apply_settings_document(&mut self, v: &serde_json::Value) -> (bool, bool) {
        let mut had_features = false;
        let mut had_giphy = false;
        self.read_receipts = v["read_receipts"].as_bool().unwrap_or(true);
        self.enter_sends = v["enter_sends"].as_bool().unwrap_or(true);
        self.keep_archived = v["keep_archived"].as_bool().unwrap_or(true);
        self.default_ephemeral_secs = v["default_ephemeral_secs"].as_u64().unwrap_or(0) as u32;
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
        if let Some(z) = v["zoom_percent"].as_u64() {
            // The same clamp the Appearance control
            // applies, so a hand-edited file cannot push
            // the render scale outside the offered range.
            self.zoom_percent = (z as u32).clamp(80, 120);
        }
        // The theme choice: ThemeId's own deserialization
        // maps retired palette names and anything unrecognized
        // onto Dark instead of failing, so any string the file
        // carries loads; a non-string value keeps the current
        // choice.
        if let Some(theme) = v
            .get("theme")
            .and_then(|raw| serde_json::from_value::<core::theme::ThemeId>(raw.clone()).ok())
        {
            self.theme_choice = theme;
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
        // The constructor default is false (message text
        // in OS notifications is the privacy-sensitive
        // behavior), so the loader fallback must agree: a
        // settings file written without this key must not
        // silently turn previews on (audit finding 11).
        self.notify_preview = v["notify_preview"].as_bool().unwrap_or(false);
        // Legacy blocked-contact copy. Builds before
        // the A6 fix persisted the blocked set here, as
        // chat ids; the set's home is now the encrypted
        // archive (see `App::start_live`, which migrates
        // this copy into the store on the first live
        // start and never writes it back: settings_json
        // no longer emits the field, so the next settings
        // save scrubs it from this file). Reading it here
        // keeps the panel's pre-connect display working
        // until that migration runs. Chat ids are stored,
        // not JIDs: the panel reasons in ids.
        if let Some(blocked) = v["blocked_chats"].as_array() {
            self.blocked_chats = blocked.iter().filter_map(serde_json::Value::as_u64).collect();
        }
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
        self.update.auto_check = v["update_auto_check"].as_bool().unwrap_or(true);
        // "My profile" identity: seed the editor state
        // once from the persisted keys. In live mode these
        // are only a cache of the last account read-back
        // (the editor shows its loading state until the
        // fresh read-back lands, so the cache is never
        // presented as the account's current values); in
        // demo mode they are the values themselves. Absent
        // keys leave the defaults (empty name/about, no
        // photo).
        if let Some(n) = v["my_name"].as_str() {
            self.my_profile.name = n.to_owned();
        }
        if let Some(a) = v["my_about"].as_str() {
            self.my_profile.about = a.to_owned();
        }
        if let Some(p) = v["my_photo_path"].as_str() {
            if !p.is_empty() {
                self.my_profile.photo_path = Some(p.to_owned());
            }
        }
        // Composer drafts, parked per chat id: the document
        // the writer produces composes the parked map plus the
        // open chat's draft under its own id, and the load puts
        // them all back in the parked map (the open chat's is
        // moved into the composer by restore_open_draft, at the
        // end of the settings load). Whitespace-only entries
        // carry no draft and are skipped, the parking rule in
        // switch_composer_chat applied on the way in.
        if let Some(drafts) = v["drafts"].as_object() {
            for (id, text) in drafts {
                if let (Ok(id), Some(text)) = (id.parse::<u64>(), text.as_str()) {
                    if !text.trim().is_empty() {
                        self.composer.chat_drafts.insert(id, text.to_owned());
                    }
                }
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
        (had_features, had_giphy)
    }

    /// Moves the open chat's draft out of the parked map into
    /// the composer. The settings load parks every draft by
    /// chat id, the open chat's included, but the composer
    /// holds the open one directly (and the chat list reads it
    /// from there for the selected row). Runs at the end of the
    /// settings load, before any chat switch can park state
    /// again. A draft the constructor already placed (the
    /// --demo-draft QA hook) wins; the parked copy then stays
    /// parked and resurfaces on the next switch back.
    pub(crate) fn restore_open_draft(&mut self) {
        if self.composer.draft.is_empty() {
            if let Some(draft) = self.composer.chat_drafts.remove(&self.selected_chat) {
                self.composer.draft = draft;
            }
        }
    }

    /// The drafts value of the settings document: every parked
    /// per-chat draft plus the open chat's draft under the
    /// selected chat's id, keyed by chat id as a string (JSON
    /// object keys are strings). Whitespace-only drafts carry
    /// nothing worth restoring and are left out, the parking
    /// rule in switch_composer_chat applied on the way out.
    fn drafts_document(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for (id, draft) in &self.composer.chat_drafts {
            if !draft.trim().is_empty() {
                map.insert(id.to_string(), serde_json::Value::String(draft.clone()));
            }
        }
        if !self.composer.draft.trim().is_empty() {
            map.insert(
                self.selected_chat.to_string(),
                serde_json::Value::String(self.composer.draft.clone()),
            );
        }
        serde_json::Value::Object(map)
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
        write_json_file(
            &backend::data_dir().join("settings.json"),
            &self.settings_json().to_string(),
        );
    }

    /// The settings document exactly as [`App::persist_settings`]
    /// writes it. Split out so tests can verify what a state
    /// change puts on disk without touching the real data dir.
    pub(crate) fn settings_json(&self) -> serde_json::Value {
        serde_json::json!({
            "read_receipts": self.read_receipts,
            "enter_sends": self.enter_sends,
            "keep_archived": self.keep_archived,
            "default_ephemeral_secs": self.default_ephemeral_secs,
            "typing_indicators": self.typing_indicators,
            "link_previews": self.link_previews,
            "saturation": self.saturation,
            "contrast": self.contrast,
            "glow": self.glow,
            "zoom_percent": self.zoom_percent,
            "theme": self.theme_choice,
            "drafts": self.drafts_document(),
            "seed": [self.seed.0, self.seed.1, self.seed.2],
            "notify_messages": self.notify_messages,
            "notify_groups": self.notify_groups,
            "notify_sound": self.notify_sound,
            "notify_preview": self.notify_preview,
            // No "blocked_chats" here: the set persists in the
            // encrypted archive store, not in this plaintext file
            // (privacy audit A6). Omitting the field also scrubs
            // the legacy copy older builds wrote, on the next
            // save.
            "motion": self.motion.as_str(),
            "language": self.language,
            "update_last_check": self.update.last_check,
            "update_auto_check": self.update.auto_check,
            "features": self.feature_flags,
            "giphy_prefs": self.giphy_prefs,
            "my_name": self.my_profile.name,
            "my_about": self.my_profile.about,
            "my_photo_path": self.my_profile.photo_path,
        })
    }

    /// Polls all worker channels (GIPHY, staged sends).
    /// Called every frame from the shell.
    pub(crate) fn poll_workers(&mut self, ctx: &egui::Context) {
        // Chat export: the worker wrote the file (or failed);
        // the outcome is reported via the notice banner, the
        // same place the synchronous export used to report it.
        if let Some(rx) = &self.export_rx {
            match rx.try_recv() {
                Ok(Ok(path)) => {
                    self.notice = Some(format!("Chat exported to {}", path.display()));
                    self.export_rx = None;
                }
                Ok(Err(e)) => {
                    self.notice = Some(format!("Export failed: {e}"));
                    self.export_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.export_rx = None;
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
        // Staged album sends: files the send worker has read.
        self.poll_staged_send();
        // Sticker previews: workers read and decode off the UI
        // thread; here the finished pixels are only uploaded,
        // and failures go into the negative cache so the picker
        // does not queue them again.
        if self.picker.sticker_rx.is_some() {
            let mut drained: Vec<StickerPreviewMsg> = Vec::new();
            if let Some(rx) = &self.picker.sticker_rx {
                loop {
                    match rx.try_recv() {
                        Ok(msg) => drained.push(msg),
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                    }
                }
            }
            for msg in drained {
                self.picker.sticker_pending.remove(&msg.path);
                match msg.pixels {
                    Some((rgba, size)) => {
                        let name = msg
                            .path
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let color = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                        let handle = ctx.load_texture(
                            format!("sticker-{name}"),
                            color,
                            egui::TextureOptions::LINEAR,
                        );
                        self.picker.sticker_textures.insert(msg.path.clone(), handle);
                        self.picker.sticker_failures.forget(&msg.path);
                    }
                    None => self.picker.sticker_failures.record(&msg.path, msg.stamp),
                }
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

    /// Queues sticker files for preview decoding on a worker
    /// thread (the GIPHY preview pattern): the worker reads and
    /// decodes each file at tile size and reports back through
    /// the shared channel drained in `poll_workers`. Called from
    /// the sticker tab's draw with the stickers that have no
    /// texture, no remembered failure and no decode in flight;
    /// the draw path itself never reads or decodes.
    pub(crate) fn request_sticker_previews(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        if self.picker.sticker_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel::<StickerPreviewMsg>();
            self.picker.sticker_tx = Some(tx);
            self.picker.sticker_rx = Some(rx);
        }
        let Some(tx) = self.picker.sticker_tx.clone() else {
            return;
        };
        for path in &paths {
            self.picker.sticker_pending.insert(path.clone());
        }
        let queued = paths.clone();
        let spawned = std::thread::Builder::new()
            .name("zaprust-sticker-previews".to_owned())
            .spawn(move || {
                for path in paths {
                    let stamp = StickerFileStamp::of(&path);
                    let pixels =
                        std::fs::read(&path).ok().and_then(|bytes| decode_sticker_preview(&bytes));
                    let _ = tx.send(StickerPreviewMsg { path, stamp, pixels });
                }
            });
        if spawned.is_err() {
            // No worker will report back; unmark the batch so a
            // later draw can queue it again instead of waiting on
            // results that will never arrive.
            for path in &queued {
                self.picker.sticker_pending.remove(path);
            }
        }
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
                    // One batch for the whole pass: the store
                    // deletes in a single transaction and
                    // reclaims the freed pages once, instead of
                    // paying a commit (and WAL truncation) per
                    // expired message.
                    let keys: Vec<(String, String)> = work
                        .iter()
                        .filter_map(|(chat_id, _, pid)| {
                            Some((session.jids.get(chat_id)?.clone(), pid.clone()?))
                        })
                        .collect();
                    let _ = store.delete_messages(&keys);
                }
            }
            // The expired messages have left the mirror; their
            // per-message helper entries go with them, so the
            // helper maps stay scoped to the messages the mirror
            // holds instead of growing by one entry per message
            // ever seen. A saved download of an expired message
            // goes too (privacy audit A3): expiry that left the
            // plaintext file behind would not be expiry.
            let data_dir = backend::data_dir();
            for (chat_id, mid, pid) in &work {
                self.forget_live_message(*chat_id, *mid, pid.as_deref());
                if let (Some(pid), Some(session)) = (pid, &self.live) {
                    if let Some(jid) = session.jids.get(chat_id) {
                        backend::remove_download_for_message(&data_dir, jid, pid);
                    }
                }
            }
            // A chat whose newest message expired shows the new
            // newest message as its preview, in the list and in
            // the archive: the shared removal rule recomputes
            // the preview and writes the chat row through, so a
            // restart cannot resurrect an expired message's
            // body as the stored preview. This pass runs from
            // the per-frame tick, after poll_backend has flushed
            // and closed its event batch, so the helper's direct
            // row write has no pending batch flush to fight.
            let mut refreshed: Vec<u64> = Vec::new();
            for (chat_id, _, _) in &work {
                if !refreshed.contains(chat_id) {
                    refreshed.push(*chat_id);
                    self.refresh_preview_after_removal(*chat_id);
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

/// Edge bound for sticker previews, in pixels: twice the 52 px
/// tile the picker renders, so high-DPI displays stay sharp
/// without paying for the source resolution.
pub(crate) const STICKER_PREVIEW_PX: u32 = 104;

/// Decodes sticker bytes into a tile-sized preview: the same
/// limited decode as [`decode_image_limited`], then a downscale
/// so neither edge exceeds [`STICKER_PREVIEW_PX`]. Runs on the
/// preview worker thread, never on the draw path. Returns the
/// RGBA pixels and their [width, height].
pub(crate) fn decode_sticker_preview(bytes: &[u8]) -> Option<(Vec<u8>, [usize; 2])> {
    let img = decode_image_limited(bytes)?;
    let img =
        img.resize(STICKER_PREVIEW_PX, STICKER_PREVIEW_PX, image::imageops::FilterType::Triangle);
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some((rgba.into_raw(), size))
}

/// A file's identity for failure caching: length plus modified
/// time. Two stamps are equal only while the file is untouched,
/// so a sticker that is replaced or repaired gets a new stamp
/// and is decoded again, while an unchanged bad file is not
/// retried.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct StickerFileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl StickerFileStamp {
    /// The stamp of the file at `path` right now, or None when
    /// its metadata cannot be read (missing or unstat-able).
    pub(crate) fn of(path: &std::path::Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self { len: meta.len(), modified: meta.modified().ok() })
    }
}

/// Negative cache for sticker previews: the paths whose decode
/// failed, each remembered against the file's stamp at attempt
/// time. The picker asks [`StickerFailureCache::still_failed`]
/// before queueing a decode; a true answer means "do not retry",
/// so one corrupt file costs one decode, not one per repaint.
#[derive(Default)]
pub(crate) struct StickerFailureCache {
    entries: std::collections::HashMap<PathBuf, Option<StickerFileStamp>>,
}

impl StickerFailureCache {
    /// Remembers that decoding `path` failed while the file had
    /// the given stamp.
    pub(crate) fn record(&mut self, path: &std::path::Path, stamp: Option<StickerFileStamp>) {
        self.entries.insert(path.to_path_buf(), stamp);
    }

    /// Drops any remembered failure for `path` (its decode has
    /// since succeeded).
    pub(crate) fn forget(&mut self, path: &std::path::Path) {
        self.entries.remove(path);
    }

    /// True while `path` has a remembered failure and the file
    /// still carries the stamp it failed with. When the file has
    /// changed since (new length or mtime, or it vanished and
    /// came back), the entry is dropped and the answer is false,
    /// so the next draw queues a fresh decode.
    pub(crate) fn still_failed(&mut self, path: &std::path::Path) -> bool {
        let Some(&stamp) = self.entries.get(path) else {
            return false;
        };
        if StickerFileStamp::of(path) == stamp {
            true
        } else {
            self.entries.remove(path);
            false
        }
    }
}

/// Caps for the GPU texture caches, in entries. Photo textures
/// are full-size images, so the media cache stays small; avatars
/// and stickers are tiny tiles and previews are downscaled, so
/// they get more room. At these sizes a linear lookup is cheap.
pub(crate) const MAX_MEDIA_TEXTURES: usize = 128;
pub(crate) const MAX_AVATAR_TEXTURES: usize = 256;
pub(crate) const MAX_GIPHY_TEXTURES: usize = 128;
pub(crate) const MAX_STICKER_TEXTURES: usize = 256;

/// A bounded GPU texture cache: insertion order is kept in a
/// plain Vec and the oldest entry is evicted first once the
/// cache grows past its cap. Without a bound these caches kept
/// every texture decoded in the session alive until exit
/// (audit finding 10).
pub(crate) struct TextureCache<K> {
    entries: Vec<(K, egui::TextureHandle)>,
    cap: usize,
}

impl<K: PartialEq> TextureCache<K> {
    pub(crate) fn new(cap: usize) -> Self {
        Self { entries: Vec::new(), cap }
    }

    pub(crate) fn get<Q>(&self, key: &Q) -> Option<&egui::TextureHandle>
    where
        K: std::borrow::Borrow<Q>,
        Q: PartialEq + ?Sized,
    {
        self.entries.iter().find(|(k, _)| k.borrow() == key).map(|(_, handle)| handle)
    }

    pub(crate) fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: std::borrow::Borrow<Q>,
        Q: PartialEq + ?Sized,
    {
        self.entries.iter().any(|(k, _)| k.borrow() == key)
    }

    pub(crate) fn insert(&mut self, key: K, handle: egui::TextureHandle) {
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = handle;
            return;
        }
        self.entries.push((key, handle));
        while self.entries.len() > self.cap {
            // Dropping the evicted handle frees its texture.
            let _ = self.entries.remove(0);
        }
    }

    /// Removes one entry, freeing its texture. Returns whether an
    /// entry was present.
    pub(crate) fn remove<Q>(&mut self, key: &Q) -> bool
    where
        K: std::borrow::Borrow<Q>,
        Q: PartialEq + ?Sized,
    {
        let before = self.entries.len();
        self.entries.retain(|(k, _)| k.borrow() != key);
        self.entries.len() != before
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_texture(ctx: &egui::Context, name: &str) -> egui::TextureHandle {
        ctx.load_texture(
            name,
            egui::ColorImage::new([1, 1], vec![egui::Color32::RED]),
            egui::TextureOptions::NEAREST,
        )
    }

    #[test]
    fn texture_cache_evicts_the_oldest_entries_past_the_cap() {
        // Audit finding 10: the texture caches must stay
        // bounded, with the earliest inserts evicted first.
        let ctx = egui::Context::default();
        let mut cache = TextureCache::new(2);
        cache.insert(1u64, test_texture(&ctx, "a"));
        cache.insert(2u64, test_texture(&ctx, "b"));
        cache.insert(3u64, test_texture(&ctx, "c"));
        assert!(cache.get(&1).is_none(), "the oldest entry is evicted");
        assert!(cache.get(&2).is_some());
        assert!(cache.get(&3).is_some());
    }

    #[test]
    fn texture_cache_reinsert_replaces_and_clear_empties() {
        let ctx = egui::Context::default();
        let mut cache = TextureCache::new(2);
        cache.insert("key".to_owned(), test_texture(&ctx, "a"));
        let second = test_texture(&ctx, "b");
        let second_id = second.id();
        cache.insert("key".to_owned(), second);
        assert_eq!(cache.get("key").map(|h| h.id()), Some(second_id));
        assert!(cache.contains_key("key"));
        cache.clear();
        assert!(cache.get("key").is_none());
        assert!(!cache.contains_key("key"));
    }

    #[test]
    fn sticker_preview_decode_downscales_to_the_tile_bound() {
        // A source far larger than the 52 px tile: the preview
        // request must come back bounded, not at source size.
        let img = image::RgbaImage::from_pixel(512, 300, image::Rgba([30, 200, 30, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("png encode");
        let (rgba, size) = decode_sticker_preview(&png.into_inner()).expect("preview decodes");
        assert_eq!(size[0], STICKER_PREVIEW_PX as usize);
        assert!(size[1] <= STICKER_PREVIEW_PX as usize, "height {} over the bound", size[1]);
        assert!(size[1] < size[0], "aspect ratio preserved");
        assert_eq!(rgba.len(), size[0] * size[1] * 4);
    }

    #[test]
    fn sticker_preview_decode_rejects_non_image_bytes() {
        assert!(decode_sticker_preview(b"not an image").is_none());
    }

    /// A fresh temp path for one failure-cache test (parallel
    /// tests use distinct names).
    fn sticker_test_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("zaprust-sticker-failure-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn sticker_failure_cache_does_not_retry_an_unchanged_file() {
        let path = sticker_test_path("unchanged.png");
        std::fs::write(&path, b"corrupt").expect("write bad sticker");
        let mut cache = StickerFailureCache::default();
        assert!(!cache.still_failed(&path), "nothing recorded yet");
        cache.record(&path, StickerFileStamp::of(&path));
        // The picker queues a decode only when this is false, so
        // staying true across draws is what stops the per-frame
        // retry.
        assert!(cache.still_failed(&path));
        assert!(cache.still_failed(&path));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sticker_failure_cache_retries_once_the_file_changes() {
        let path = sticker_test_path("replaced.png");
        std::fs::write(&path, b"corrupt").expect("write bad sticker");
        let mut cache = StickerFailureCache::default();
        cache.record(&path, StickerFileStamp::of(&path));
        assert!(cache.still_failed(&path));
        // Replacing the file (different length, new mtime) moves
        // its stamp: the failure no longer applies and a fresh
        // decode is due without an app restart.
        std::fs::write(&path, b"a longer, still not valid, sticker body").expect("replace sticker");
        assert!(!cache.still_failed(&path));
        assert!(!cache.still_failed(&path), "the stale entry stays dropped");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sticker_failure_cache_forget_clears_the_entry() {
        let path = sticker_test_path("forgotten.png");
        std::fs::write(&path, b"corrupt").expect("write bad sticker");
        let mut cache = StickerFailureCache::default();
        cache.record(&path, StickerFileStamp::of(&path));
        assert!(cache.still_failed(&path));
        cache.forget(&path);
        assert!(!cache.still_failed(&path));
        let _ = std::fs::remove_file(&path);
    }
}
