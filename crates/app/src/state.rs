//! Application state: CLI arguments, screens, and the root app struct.
//! No drawing happens here beyond construction.

use crate::theme::apply_visuals;
use core::filter::ChatFilter;
use core::model::{Chat, Message};
use core::theme::{Rgb, ThemeId, Tokens};
use eframe::egui::{self};

/// not a CLI framework).
/// Resolves a `--seed` value: a preset name (case-insensitive) or a
/// hex color (`RRGGBB` or `#RRGGBB`).
fn parse_seed(v: &str) -> Option<Rgb> {
    if let Some((_, rgb)) =
        core::theme::SEED_PRESETS.iter().find(|(name, _)| name.eq_ignore_ascii_case(v))
    {
        return Some(*rgb);
    }
    let hex = if v.starts_with('#') { v.to_owned() } else { format!("#{v}") };
    Rgb::from_hex(&hex).ok()
}

#[derive(Debug, Clone)]
pub(crate) struct Args {
    theme: ThemeId,
    screen: Screen,
    sidebar_collapsed: bool,
    open_viewonce: bool,
    demo_draft: bool,
    settings_search: Option<String>,
    scroll_top: bool,
    /// QA hook (--community-scroll <px>): vertical scroll offset
    /// for the community home (screenshots of its lower sections).
    community_scroll: f32,
    pair_code: bool,
    /// QA hook (--country-picker): open the connect form's
    /// country picker (screenshots).
    country_picker: bool,
    /// QA hook (--pair-form): open the connect screen on the
    /// phone-number form, before any code is requested
    /// (screenshots).
    pair_form: bool,
    account_menu: bool,
    /// QA hook (--search <text>): seed the chat list's search
    /// field (screenshots of the global search results).
    search: Option<String>,
    /// QA hook (--chat-search <text>): open the in-chat search
    /// strip with this query (screenshots).
    chat_search: Option<String>,
    /// QA hook (--gallery media|links|docs): open the profile
    /// dock on a gallery section (screenshots).
    gallery: Option<String>,
    /// QA hook (--stage <path>): stage this file in the
    /// selected chat's composer (screenshots).
    stage: Option<String>,
    /// QA hook (--editor): open the photo editor on the staged
    /// photo (screenshots; pairs with --stage).
    editor: bool,
    live: bool,
    screen_explicit: bool,
    fake_backend: bool,
    fake_linked: bool,
    saturation: f32,
    contrast: f32,
    seed: Rgb,
    debug: bool,
    select_chat: Option<u64>,
    scroll_to: Option<usize>,
    picker: Option<String>,
    profile: bool,
    header_menu: bool,
    /// QA hook (--community <id>): open this community's home, so
    /// a screenshot can show it without clicking through.
    community: Option<u64>,
    /// QA hook (--calls-filter missed): open the Calls panel
    /// with its Missed chip active (screenshots).
    calls_missed: bool,
    /// QA hook (--calls-picker): open the Calls panel's new
    /// call contact picker (screenshots).
    calls_picker: bool,
}

impl Args {
    /// Test constructor for code outside this module: the
    /// plain demo dataset (no live session, no backend), the
    /// same state this module's own tests build by setting
    /// `live = false` directly.
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let mut args = Self::parse();
        args.live = false;
        args
    }

    pub(crate) fn parse() -> Self {
        let argv: Vec<String> = std::env::args().collect();
        let mut args = Self {
            theme: ThemeId::Dark,
            screen: Screen::Chats,
            sidebar_collapsed: false,
            open_viewonce: false,
            demo_draft: false,
            settings_search: None,
            scroll_top: false,
            community_scroll: 0.0,
            pair_code: false,
            country_picker: false,
            pair_form: false,
            account_menu: false,
            search: None,
            chat_search: None,
            gallery: None,
            stage: None,
            editor: false,
            // Live is the default: an installed copy opens as the
            // real client (link screen or the stored session).
            // Demo data is opt-in via --demo only.
            live: true,
            screen_explicit: false,
            fake_backend: false,
            fake_linked: false,
            debug: false,
            select_chat: None,
            scroll_to: None,
            picker: None,
            profile: false,
            header_menu: false,
            community: None,
            calls_missed: false,
            calls_picker: false,
            saturation: 1.0,
            contrast: 1.0,
            seed: core::theme::DEFAULT_SEED,
        };
        let mut i = 0;
        while i < argv.len() {
            match argv[i].as_str() {
                "--theme" => {
                    if let Some(v) = argv.get(i + 1) {
                        args.theme = ThemeId::parse(v).unwrap_or(args.theme);
                        i += 1;
                    }
                }
                "--seed" => {
                    if let Some(v) = argv.get(i + 1) {
                        args.seed = parse_seed(v).unwrap_or(args.seed);
                        i += 1;
                    }
                }
                "--screen" => {
                    if let Some(v) = argv.get(i + 1) {
                        args.screen = Screen::parse(v).unwrap_or(args.screen);
                        args.screen_explicit = true;
                        i += 1;
                    }
                }
                "--calls-filter" => {
                    if let Some(v) = argv.get(i + 1) {
                        args.calls_missed = v == "missed";
                        i += 1;
                    }
                }
                "--calls-picker" => {
                    args.calls_picker = true;
                }
                "--demo" => {
                    // Offline demo dataset, no network, no backend.
                    // The only way into demo mode; a plain launch is
                    // always the real client.
                    args.live = false;
                }
                "--demo-draft" => {
                    args.demo_draft = true;
                }
                "--scroll-top" => {
                    // Open the conversation at the top instead
                    // of sticking to the newest message
                    // (screenshots of a long demo conversation).
                    args.scroll_top = true;
                }
                "--community-scroll" => {
                    // QA hook: open the community home scrolled
                    // (screenshots of its lower sections).
                    if let Some(v) = argv.get(i + 1) {
                        args.community_scroll = v.parse().unwrap_or(0.0);
                        i += 1;
                    }
                }
                "--settings-search" => {
                    // Seed the Settings search field (screenshots
                    // of the search index and its results).
                    if let Some(v) = argv.get(i + 1) {
                        args.settings_search = Some(v.clone());
                        i += 1;
                    }
                }
                "--search" => {
                    // Seed the chat list's search field
                    // (screenshots of the global search results).
                    if let Some(v) = argv.get(i + 1) {
                        args.search = Some(v.clone());
                        i += 1;
                    }
                }
                "--chat-search" => {
                    // Open the in-chat search strip with a query
                    // (screenshots).
                    if let Some(v) = argv.get(i + 1) {
                        args.chat_search = Some(v.clone());
                        i += 1;
                    }
                }
                "--gallery" => {
                    // Open the profile dock on a gallery section
                    // (screenshots).
                    if let Some(v) = argv.get(i + 1) {
                        args.gallery = Some(v.clone());
                        i += 1;
                    }
                }
                "--stage" => {
                    // Stage a file in the selected chat's
                    // composer (screenshots).
                    if let Some(v) = argv.get(i + 1) {
                        args.stage = Some(v.clone());
                        i += 1;
                    }
                }
                "--editor" => {
                    args.editor = true;
                }
                "--open-viewonce" => {
                    args.open_viewonce = true;
                }
                "--pair-code" => {
                    args.pair_code = true;
                }
                "--country-picker" => {
                    args.country_picker = true;
                }
                "--pair-form" => {
                    args.pair_form = true;
                }
                "--account-menu" => {
                    args.account_menu = true;
                }
                "--live" => {
                    args.live = true;
                }
                "--fake-backend" => {
                    // Live mode against the deterministic fake backend
                    // (implies --live). Add --fake-linked to complete
                    // the pairing script automatically.
                    args.live = true;
                    args.fake_backend = true;
                }
                "--fake-linked" => {
                    args.fake_linked = true;
                }
                "--select-chat" => {
                    if let Some(v) = argv.get(i + 1) {
                        args.select_chat = v.parse().ok();
                        i += 1;
                    }
                }
                "--scroll-to" => {
                    // QA hook: open the selected conversation
                    // located at the Nth message (1-based), the
                    // same locate the search jump uses
                    // (screenshots of a windowed stream's middle).
                    if let Some(v) = argv.get(i + 1) {
                        args.scroll_to = v.parse().ok();
                        i += 1;
                    }
                }
                "--community" => {
                    // QA hook: open a community home (screenshots).
                    if let Some(v) = argv.get(i + 1) {
                        args.community = v.parse().ok();
                        i += 1;
                    }
                }
                "--profile" => {
                    args.profile = true;
                }
                "--header-menu" => {
                    args.header_menu = true;
                }
                "--picker" => {
                    // Open the media picker on a tab (screenshots).
                    if let Some(v) = argv.get(i + 1) {
                        args.picker = Some(v.clone());
                        i += 1;
                    }
                }
                "--debug" => {
                    // Opt-in debug mode: banner + inspector +
                    // structured tracing (docs/REAL-TEST-FLOW.md).
                    args.debug = true;
                }
                "--saturation" => {
                    if let Some(v) = argv.get(i + 1) {
                        args.saturation = v.parse().unwrap_or(1.0);
                        i += 1;
                    }
                }
                "--contrast" => {
                    if let Some(v) = argv.get(i + 1) {
                        args.contrast = v.parse().unwrap_or(1.0);
                        i += 1;
                    }
                }
                "--collapsed" => {
                    args.sidebar_collapsed = true;
                }
                _ => {}
            }
            i += 1;
        }
        args
    }
}

/// Top-level screens (the rail switches between them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Screen {
    Chats,
    Calls,
    Channels,
    Groups,
    Communities,
    Starred,
    Archived,
    Updates,
    Settings,
    Connect,
    Debug,
}

impl Screen {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "chats" | "chat" => Some(Self::Chats),
            "calls" => Some(Self::Calls),
            "channels" => Some(Self::Channels),
            "groups" => Some(Self::Groups),
            "communities" => Some(Self::Communities),
            "starred" => Some(Self::Starred),
            "archived" => Some(Self::Archived),
            "updates" | "status" => Some(Self::Updates),
            "settings" => Some(Self::Settings),
            "connect" | "link" => Some(Self::Connect),
            "debug" => Some(Self::Debug),
            _ => None,
        }
    }

    pub(crate) fn filter(self) -> ChatFilter {
        match self {
            Self::Channels => ChatFilter::Channels,
            Self::Groups => ChatFilter::Groups,
            Self::Archived => ChatFilter::Archived,
            _ => ChatFilter::All,
        }
    }

    /// The rail destinations that are modes of the chat list
    /// rather than screens of their own: the plain list narrowed
    /// to one kind of chat. A filtered destination carries its
    /// own header identity (back chevron plus its own title, no
    /// generic chip row) instead of rendering as the Chats list.
    pub(crate) fn is_filtered_list(self) -> bool {
        matches!(self, Self::Channels | Self::Groups | Self::Archived)
    }

    /// The locale key for a filtered destination's own header
    /// title. Only meaningful when [`Self::is_filtered_list`].
    pub(crate) fn filtered_title_key(self) -> &'static str {
        match self {
            Self::Channels => "nav.channels",
            Self::Groups => "nav.groups",
            Self::Archived => "nav.archived",
            _ => "nav.chats",
        }
    }

    pub(crate) fn is_list_screen(self) -> bool {
        // Starred, Calls and Communities are list screens too:
        // their panels replace the chat list on the left while
        // the conversation stays on the right, like every other
        // entry here (the owner's left-panel rule: secondary
        // lists never take the full screen).
        matches!(
            self,
            Self::Chats
                | Self::Channels
                | Self::Groups
                | Self::Archived
                | Self::Starred
                | Self::Calls
                | Self::Communities
        )
    }
}

/// How the shell arranges its panes at a given window size.
/// Two-pane is the desktop arrangement: the rail, the list
/// panel and the conversation side by side. Below the
/// breakpoint there is no honest room for both content panes,
/// so the shell goes single-pane: the list and the conversation
/// take turns full-width, like a phone app. A single-pane
/// window that is also portrait-shaped (taller than wide) goes
/// one step further, to the native phone arrangement: no rail,
/// a bottom navigation bar, and full-screen pages (see
/// portrait.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutMode {
    TwoPane,
    SinglePane,
    Portrait,
}

/// The window width where the two-pane shell stops working,
/// built from the layout's own constants: the rail (60) plus
/// the list panel's default width (380) plus the narrowest
/// conversation column that still holds the composer pill and
/// readable bubbles (550). Below it, keeping both panes only
/// crushes them (the graded small-width failure), so the shell
/// switches to single-pane instead.
pub(crate) const SINGLE_PANE_BREAKPOINT: f32 = 60.0 + 380.0 + 550.0;

/// The layout mode for a window of `width` by `height`: the
/// pure decision the shell re-evaluates every frame. Portrait
/// is strictly taller-than-wide; a square window is not
/// portrait and keeps the single-pane shell.
pub(crate) fn layout_mode_for_size(width: f32, height: f32) -> LayoutMode {
    if width >= SINGLE_PANE_BREAKPOINT {
        LayoutMode::TwoPane
    } else if height > width {
        LayoutMode::Portrait
    } else {
        LayoutMode::SinglePane
    }
}

/// The layout mode for the shell's content rect, which egui
/// reports in points at the pixels-per-point currently in
/// effect. Scaling the points back by that factor recovers the
/// window's physical pixel size, and the mode is decided on
/// the physical size: deciding on points would let the density
/// setting move the breakpoint (a scaled-down desktop window
/// would read as a phone, a scaled-up phone as a desktop).
pub(crate) fn layout_mode_for_content(content: egui::Vec2, pixels_per_point: f32) -> LayoutMode {
    layout_mode_for_size(content.x * pixels_per_point, content.y * pixels_per_point)
}

impl LayoutMode {
    /// The mode's automatic density factor, applied on top of
    /// the user's font-size preference. The desktop shell was
    /// designed at 1.0 and stays there; the small modes step
    /// down because the same chrome at full size reads
    /// oversized on a phone-shaped window (the round-3 grading
    /// verdict): single-pane takes a mild step, portrait the
    /// full one, matching how much narrower than the desktop
    /// design each mode runs.
    pub(crate) fn density_factor(self) -> f32 {
        match self {
            LayoutMode::TwoPane => 1.0,
            LayoutMode::SinglePane => 0.95,
            LayoutMode::Portrait => 0.90,
        }
    }
}

/// The pixels-per-point the shell renders at: the user's
/// font-size preference (a percent, 100 is the designed size)
/// composed with the layout mode's automatic density factor.
/// One composition point, so the setting and the mode can
/// never disagree about the final scale.
pub(crate) fn effective_pixels_per_point(zoom_percent: u32, mode: LayoutMode) -> f32 {
    zoom_percent as f32 / 100.0 * mode.density_factor()
}

/// How the connect screen links: QR scan or 8-character pairing code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairMode {
    Qr,
    Code,
}

/// A destructive action parked for the house confirmation
/// modal. The surface that asked (a dock row, a kebab menu)
/// stores one of these in `App::ui_confirm`; the modal names
/// the target and only its confirm button performs the action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UiConfirm {
    /// Remove one member from a group (live participant op).
    RemoveMember { chat_id: u64, address: String, name: String },
    /// Remove a group's profile picture (design standard
    /// section 23): destructive in effect (the picture is gone
    /// for every member), so it confirms like the other
    /// destructive actions before the command is sent.
    RemoveGroupPhoto { chat_id: u64, name: String },
    /// Leave a group; confirming closes the info dock, as the
    /// chat is archived by the leave itself.
    LeaveGroup { chat_id: u64, name: String },
    /// Cycle a privacy row away from `contact_blacklist`:
    /// the value names an exception list configured on the
    /// phone, and replacing it discards the association with
    /// that list, which this client cannot restore. Confirming
    /// performs the cycle the row click asked for.
    ReplaceExceptionList { category: String, label: String },
    /// Clear a chat's messages (design standard section 24):
    /// every message in the chat goes, on every device. The
    /// modal carries the keep-starred choice the wire supports;
    /// the checkbox state lives in
    /// [`ZapApp::clear_keep_starred`].
    ClearChat { chat_id: u64, name: String },
    /// Delete a chat (design standard section 24): the chat and
    /// its messages go, on every device. Nothing about it is
    /// recoverable from this client.
    DeleteChat { chat_id: u64, name: String },
    /// Unfollow a channel (design standard section 24): the
    /// channel leaves the roster and its updates stop arriving;
    /// following again needs the invite link, so it confirms
    /// like the other destructive actions.
    UnfollowChannel { chat_id: u64, name: String },
    /// Cancel one of my events (P7 row 50): every member sees
    /// the event as canceled, and this client cannot un-cancel
    /// it, so it confirms like the other destructive actions.
    /// Carries the event's chat, message and name.
    CancelEvent { chat_id: u64, msg_id: u64, name: String },
    /// Unlink a group from its community: the group survives as
    /// a standalone group, but the community tie is cut for
    /// every member, so it confirms like the other destructive
    /// actions. Carries the community's announcement chat id
    /// (the parent) and the group's chat id.
    UnlinkCommunityGroup { parent_id: u64, group_id: u64, name: String },
    /// Remove a member from a community entirely: the parent
    /// group and every linked subgroup lose them at once, so it
    /// confirms like the other destructive actions.
    RemoveCommunityMember { parent_id: u64, address: String, name: String },
    /// Deactivate a community: the community and its
    /// announcement group go away for every member; subgroups
    /// survive as ordinary groups. Nothing about it is
    /// recoverable from this client.
    DeactivateCommunity { parent_id: u64, name: String },
    /// Report a message as spam (design standard section 24):
    /// the report goes to WhatsApp with the message's identity;
    /// confirming sends it. The chat and message are named so
    /// the outcome notice can say what was reported.
    ReportSpam { chat_id: u64, msg_id: u64, name: String },
    /// Delete a local sticker pack (design standard
    /// section 24): the pack's folder is removed from disk,
    /// which no undo in this client can reverse, so it
    /// confirms like the other destructive actions; the
    /// deletion's result is reported, never swallowed.
    DeleteStickerPack { dir: std::path::PathBuf, name: String },
    /// Delete one of my own status updates (design standard
    /// section 24): the revoke fans out to the status's whole
    /// audience and cannot be undone, so it confirms like the
    /// other destructive actions. The status is named by its
    /// protocol ID; the modal's body is generic because a
    /// status has no title.
    DeleteStatus { message_id: String },
    /// Log out: unlink this device from the account. Confirming
    /// sends the backend's disconnect command; the local chats
    /// stay in the archive on this computer.
    LogOut,
    /// Start fresh after an unlock failure: the databases and
    /// the key file are set aside into a backup folder and a
    /// normal live start runs from zero. Destructive in effect
    /// (the set-aside data cannot be opened without its missing
    /// key), so it is confirmed like the other destructive
    /// actions, never run from the panel button directly.
    StartFresh,
}

/// Debug mode state (see debug_ui.rs): the trace ring, its clock and the inspector toggles.
pub(crate) struct DebugState {
    /// Debug mode (--debug): banner, inspector, tracing.
    pub(crate) mode: bool,
    /// Structured trace ring feeding the debug inspector.
    pub(crate) trace: core::debug::TraceRing,
    /// Flow start for trace timestamps.
    pub(crate) flow_start: std::time::Instant,
    /// Export-bundle dialog open.
    pub(crate) export_open: bool,
    /// Export dialog opt-in: include message bodies (sensitive).
    pub(crate) include_bodies: bool,
    /// Inspector toggle: reveal message bodies locally.
    pub(crate) reveal_bodies: bool,
}

/// Runtime animation state: in-flight runs and their edge-detection flags. The motion setting itself stays on App.
pub(crate) struct AnimState {
    /// Running picker open animation.
    pub(crate) picker: Option<crate::motion::Anim>,
    /// Running conversation-switch fade.
    pub(crate) chat_fade: Option<crate::motion::Anim>,
    /// Running send-button pulse.
    pub(crate) send_pulse: Option<crate::motion::Anim>,
    /// Whether the picker was open last frame (open-edge detect).
    pub(crate) picker_was_open: bool,
    /// The picker is playing its exit run: still drawn, fading
    /// and sinking, until the run completes.
    pub(crate) picker_closing: bool,
    /// A conversation-switch fade was requested.
    pub(crate) chat_fade_pending: bool,
    /// How many animations were started this session (debug).
    pub(crate) started: u32,
    /// The photo viewer's reveal run: spinner while loading, then
    /// the photo crossfades in. Restarted on every open.
    pub(crate) viewer_reveal: Option<crate::motion::Anim>,
    /// Running shell screen-switch fade (rail and bottom-tab
    /// switches), on the motion system's SWITCH duration.
    pub(crate) screen_fade: Option<crate::motion::Anim>,
    /// The shell key the last frame drew, as (screen, the
    /// single-pane content pane is showing): a change between
    /// frames is what arms [`Self::screen_fade`]. `None` until
    /// the first frame, which never fades.
    pub(crate) screen_key: Option<(Screen, bool)>,
}

/// Calls state: the call log behind the Calls panel, its search and filters, the new-call picker, and the one live call's UI state.
pub(crate) struct CallsState {
    /// The demo mode call log, scripted from the demo contacts.
    /// Demo mode only: live mode renders [`Self::live_entries`]
    /// instead, and the two lists never mix.
    pub(crate) entries: Vec<core::model::CallEntry>,
    /// The live call history: the archive's persisted records,
    /// loaded when a live session starts and extended as calls
    /// complete, newest first (the order the archive reads back).
    pub(crate) live_entries: Vec<core::model::CallEntry>,
    /// Protocol call IDs already written into the history this
    /// run. A call can be reported twice (its end event and a
    /// missed-call event), and the archive's own uniqueness is
    /// the second line of defense, not the first.
    pub(crate) recorded_ids: std::collections::HashSet<String>,
    /// The Calls panel's own search text.
    pub(crate) search: String,
    /// The Calls panel's Missed chip state.
    pub(crate) missed_only: bool,
    /// The Calls favorites filter: only entries whose chat is
    /// marked favorite. Toggled by the shortcut row's favorites
    /// circle, which both the panel and portrait carry.
    pub(crate) favorites_only: bool,
    /// The portrait Calls screen's search field is expanded
    /// (its header carries a search icon, the phone pattern,
    /// instead of the always-open pill the panel shows in the
    /// other modes).
    pub(crate) search_open: bool,
    /// The Calls panel's new call contact picker is open.
    pub(crate) picker_open: bool,
    /// The kind of the outgoing call most recently started from
    /// the app, remembered until the backend's first Ringing
    /// event adopts it: that event carries no kind of its own,
    /// so without this the adopted call UI would always claim
    /// the call is a voice call.
    pub(crate) outgoing_video: bool,
    /// The new call picker's own search text.
    pub(crate) picker_search: String,
    /// The one live call's UI state, if any (see call_ui.rs).
    pub(crate) active: Option<crate::call_ui::CallUi>,
}

/// Media state: decoded photo textures, the photo viewer and view-once tracking, and voice-note playback.
/// One live photo fetch's progress, tracked per message in
/// [`MediaState::photo_fetch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhotoFetchState {
    /// The fetch command was sent; the bytes have not landed.
    Loading,
    /// The fetch answered with a failure (or with bytes that did
    /// not decode); the photo is unavailable until retried.
    Failed,
}

/// One voice fetch's progress, tracked per message in
/// [`MediaState::voice_fetch`]: the same two states as a photo
/// fetch, with the same meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VoiceFetchState {
    /// The fetch command was sent; the bytes have not landed.
    Loading,
    /// The fetch failed, the bytes did not decode, or no output
    /// device would open; the note is unavailable until retried.
    Failed,
}

/// Updates (status) state: the received feed, my own posts,
/// and the composer / viewer state. Session-scoped on purpose:
/// statuses are receive-driven (there is no fetch for them)
/// and expire 24 hours after posting, so nothing here is
/// persisted; a restart's feed is whatever history sync
/// redelivers at link time (see updates.rs).
pub(crate) struct UpdatesState {
    /// Other people's statuses received this session, in
    /// arrival order; the screen groups and sorts them for
    /// display. Entries older than 24 hours are pruned as
    /// events land and frames draw.
    pub(crate) feed: Vec<StatusEntry>,
    /// My own statuses this session: posted from this client,
    /// or delivered back under the broadcast (including posts
    /// made on the phone and redelivered by history sync).
    pub(crate) mine: Vec<StatusEntry>,
    /// Protocol IDs of feed statuses already opened in the
    /// viewer this session: the feed's viewed split.
    pub(crate) viewed: std::collections::HashSet<String>,
    /// The status viewer overlay's state, while it is open.
    pub(crate) viewer: Option<StatusViewer>,
    /// The status composer modal is open.
    pub(crate) composer_open: bool,
    /// The status composer modal's text field: the status
    /// text, or the caption when a media file is staged.
    pub(crate) draft: String,
    /// A media file staged in the composer (picked, not yet
    /// posted). While one is staged, posting sends it instead
    /// of a text status.
    pub(crate) staged: Option<StagedStatusMedia>,
    /// The staged media of a post in flight, kept so the
    /// landed post carries its bytes for the session view
    /// (the backend keeps no reference for a just-posted
    /// status). Taken when the post settles.
    pub(crate) pending_media: Option<StagedStatusMedia>,
    /// The status composer modal's inline error: a validation
    /// refusal, or the backend's answer after a failed live post.
    pub(crate) error: Option<String>,
    /// A live post is in flight: the command was sent and its
    /// result event has not landed yet. The composer stays open
    /// and inert meanwhile, so a failure lands inside it instead
    /// of behind a closed dialog.
    pub(crate) posting: bool,
    /// The audience the most recent post used, kept so a
    /// delete of that post can fan out to the same recipient
    /// list (a status revoke is addressed like the post was).
    pub(crate) last_audience: Vec<String>,
    /// Counter for the locally minted IDs of demo-mode posts
    /// (no session, so no protocol ID exists).
    pub(crate) local_seq: u64,
    /// Per-status reaction actors (protocol ID → actor →
    /// emoji), the accounting behind each status's reaction
    /// summary, kept like the chat path's actor map.
    pub(crate) reaction_actors:
        std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    /// My optimistic reactions awaiting the server's word
    /// (protocol ID → the reaction it replaced), for the
    /// failure rollback.
    pub(crate) reaction_pending: std::collections::HashMap<String, Option<String>>,
    /// The status timeline's clock anchor: the newest status
    /// timestamp seen, with the instant it was seen. Ages and
    /// the 24-hour prune read time as the anchor plus elapsed
    /// real time, so a scripted timeline in test mode ages in
    /// its own era instead of the wall clock emptying the
    /// feed at once (the guarded-clock pattern, P1-53).
    pub(crate) clock_anchor: Option<(i64, std::time::Instant)>,
}

/// One status in the feed or in My Status: the backend's
/// value, plus the bytes of a media post made from this
/// client this session (the only copy the viewer can play
/// until the post is redelivered with references).
#[derive(Clone)]
pub(crate) struct StatusEntry {
    /// The status itself.
    pub(crate) status: backend::LiveStatus,
    /// Media bytes of an own post, kept for the session view.
    pub(crate) local_bytes: Option<Vec<u8>>,
}

/// The status viewer overlay's state: which list it walks,
/// where in the author's run it stands, and the reply form.
#[derive(Clone)]
pub(crate) struct StatusViewer {
    /// The author whose statuses are being viewed (my own JID
    /// for the My Status run).
    pub(crate) author_jid: String,
    /// True when the run is my own statuses (the delete action
    /// replaces the reply form).
    pub(crate) mine: bool,
    /// Index into the author's run, oldest first.
    pub(crate) index: usize,
    /// The reply draft (feed runs only).
    pub(crate) reply: String,
    /// An inline error inside the viewer: a failed delete or
    /// a reaction the backend refused.
    pub(crate) error: Option<String>,
    /// A delete of the shown status is in flight.
    pub(crate) deleting: bool,
    /// The protocol ID of the item whose photo fetch was
    /// dispatched for this viewing, so the fetch is sent once
    /// per item instead of once per frame (a landed fetch
    /// clears its in-flight mark, which must not re-arm the
    /// dispatch). Reset when the item changes or a retry is
    /// asked for.
    pub(crate) dispatched: Option<String>,
}

/// A media file staged in the status composer: the picked
/// file's bytes and the derived posting material (thumbnail,
/// probed duration), ready for `PostMediaStatus`.
pub(crate) struct StagedStatusMedia {
    /// Photo or video.
    pub(crate) kind: backend::MediaKind,
    /// The picked file's name, for the staged chip.
    pub(crate) name: String,
    /// The file's bytes.
    pub(crate) data: Vec<u8>,
    /// The file's MIME type.
    pub(crate) mime: String,
    /// JPEG thumbnail bytes (the photo at preview fidelity,
    /// or a real first frame of the video); empty when no
    /// thumbnail could be made.
    pub(crate) thumbnail: Vec<u8>,
    /// Duration in seconds for a video (probed); 0 for a photo.
    pub(crate) seconds: u32,
}

pub(crate) struct MediaState {
    /// Lazily-created demo photo texture.
    pub(crate) photo_texture: Option<egui::TextureHandle>,
    /// Decoded photo textures per app message ID: picked files and
    /// fetched received photos render their own pixels. In a real
    /// live session nothing is substituted for a photo whose bytes
    /// are not here (see `photo_fetch`); the demo photo texture
    /// stands in only in demo and test mode. Bounded (see
    /// `extras::TextureCache`): the oldest texture is evicted once
    /// the cap is passed.
    pub(crate) textures: crate::extras::TextureCache<u64>,
    /// Decoded demo persona portraits, keyed by display name (see
    /// demo_avatars.rs). Bounded like `textures`.
    pub(crate) avatar_textures: crate::extras::TextureCache<String>,
    /// Decoded live profile pictures, keyed by JID (see
    /// `live::LiveSession`'s avatar bookkeeping: the store's
    /// avatar rows and the backend's picture lookups feed this
    /// cache, and only this cache paints a real photo). Bounded
    /// like `textures`.
    pub(crate) live_avatar_textures: crate::extras::TextureCache<String>,
    /// Live sticker fetches in flight or failed, per app
    /// message ID, mirroring `photo_fetch`. A successful fetch
    /// removes the entry and lands the decoded texture in
    /// `textures` instead.
    pub(crate) sticker_fetch: std::collections::HashMap<u64, PhotoFetchState>,
    /// Decoded photo wire thumbnails per app message ID: the
    /// photo itself at preview fidelity, painted by its tile
    /// before (and while) the full bytes are fetched. Bounded
    /// like `textures`.
    pub(crate) photo_thumb_textures: crate::extras::TextureCache<u64>,
    /// View-once photos the user has already opened.
    pub(crate) opened_view_once: std::collections::HashSet<u64>,
    /// The view-once photo currently open in the viewer, if any.
    pub(crate) viewing_photo: Option<u64>,
    /// Live photo fetches in flight or failed, per app message ID.
    /// Absence means no fetch has been requested yet. A successful
    /// fetch removes the entry and lands the decoded texture in
    /// `textures` instead.
    pub(crate) photo_fetch: std::collections::HashMap<u64, PhotoFetchState>,
    /// Media downloads (save-to-disk) in flight, per app message
    /// ID: the message's download chip spins while its ID is in
    /// here, and the entry leaves when the backend answers with
    /// MediaSaved or an error.
    pub(crate) downloads: std::collections::HashSet<u64>,
    /// The view-once photo whose real pixels were actually
    /// painted in the viewer during the current viewing. Closing
    /// the viewer spends the single view only when this names the
    /// viewed message: a view-once photo whose bytes never
    /// arrived (or were never painted) has not been viewed, and
    /// its allowance survives.
    pub(crate) view_once_shown: Option<u64>,
    /// The voice-note playback session, when one note is loaded
    /// in the playback engine (playing or paused). The engine
    /// owns the position: it is the output's own consumption
    /// count, never a timed estimate.
    pub(crate) voice_player: Option<crate::player::VoicePlayer>,
    /// Voice fetches in flight or failed, per app message ID.
    /// Absence means no fetch has been requested yet; a landed
    /// fetch removes the entry and starts the player instead.
    pub(crate) voice_fetch: std::collections::HashMap<u64, VoiceFetchState>,
    /// The note the user most recently asked to hear. Fetched
    /// bytes start a player only for this note: bytes that land
    /// late for a note the user has already moved past are
    /// dropped, not played over the current one.
    pub(crate) voice_wanted: Option<u64>,
    /// The most recently decoded voice note's PCM, kept so
    /// replaying that note does not fetch and decode it again.
    /// One note only: decoded PCM is large (96 KB per second).
    pub(crate) voice_pcm: Option<(u64, std::sync::Arc<[i16]>)>,
    /// Seeks requested for a note whose bytes have not landed
    /// yet, applied to its player the moment it is created.
    pub(crate) voice_pending_seek: std::collections::HashMap<u64, f32>,
    /// Output sink for the voice player: the machine's device in
    /// normal runs. Tests and speaker-less machines substitute
    /// the null sink (see `player::VoiceSink`).
    pub(crate) voice_sink: crate::player::VoiceSink,
    /// Per-message voice playback rates chosen on the speed pill,
    /// owned here with the rest of the playback state: a fresh
    /// session starts at the stored rate, and cycling the pill
    /// mid-play updates both this map and the live session.
    pub(crate) voice_rates: std::collections::HashMap<u64, f32>,
    /// Decoded video thumbnail textures per app message ID: the
    /// real JPEG frame the sender's client embedded in the
    /// video's payload, decoded once and painted by the bubble
    /// and the viewer. A video without thumbnail bytes has no
    /// entry and paints its neutral tile; nothing generated is
    /// ever substituted. Bounded like `textures`.
    pub(crate) video_textures: crate::extras::TextureCache<u64>,
    /// Decoded embedded-thumbnail textures per app message ID
    /// for the kinds whose picture rides inside the payload: a
    /// location's map snapshot, a document's preview image, a
    /// link preview's picture. One message has one kind, so one
    /// cache serves all three. Bounded like `video_textures`.
    pub(crate) embedded_textures: crate::extras::TextureCache<u64>,
    /// Video byte fetches in flight or failed, per app message
    /// ID. Absence means no fetch has been requested yet. A
    /// landed fetch removes the entry and records the source in
    /// `video_files` (regular videos) or `video_bytes`
    /// (view-once videos) instead.
    pub(crate) video_fetch: std::collections::HashMap<u64, PhotoFetchState>,
    /// Video poster fetches in flight or failed, per app
    /// message ID. Absence means no poster fetch has been
    /// requested yet. A landed fetch removes the entry and the
    /// bytes join the session's video thumbnails (and the
    /// archive's thumbnail slot), so the poster is fetched at
    /// most once per message.
    pub(crate) poster_fetch: std::collections::HashMap<u64, PhotoFetchState>,
    /// Fetched cache files for regular videos, per app message
    /// ID, ready to hand to the player.
    pub(crate) video_files: std::collections::HashMap<u64, String>,
    /// Fetched bytes for view-once videos, per app message ID.
    /// Memory only: view-once bytes are never written to disk.
    pub(crate) video_bytes: std::collections::HashMap<u64, Vec<u8>>,
    /// The video player session for the video open in the
    /// viewer, when one is loaded. The player owns the position:
    /// it is the presentation timestamp of the frame on screen
    /// or the playback clock's target, never a simulated
    /// estimate.
    pub(crate) video_player: Option<crate::video::VideoPlayer>,
    /// The view-once video whose real content (its own
    /// thumbnail or a decoded frame) was actually presented
    /// during the current viewing. Closing the viewer spends
    /// the single view only when this names the viewed message:
    /// a view-once video that never presented anything real has
    /// not been viewed, and its allowance survives.
    pub(crate) video_presented: Option<u64>,
    /// The texture holding the video player's currently
    /// presented frame, updated in place as frames advance.
    pub(crate) video_frame: Option<egui::TextureHandle>,
    /// Which frame `video_frame` currently holds, as (message
    /// ID, presentation timestamp bits): a frame is uploaded
    /// only when this changes.
    pub(crate) video_frame_key: Option<(u64, u64)>,
}

/// One DM contact's presence, as last reported by the backend
/// for the chat the presence watch is subscribed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContactPresence {
    /// The contact is online now.
    pub(crate) online: bool,
    /// When the contact was last seen (unix seconds), when the
    /// report carried it. `None` while online, and when the
    /// contact's privacy settings hide their last seen.
    pub(crate) last_seen: Option<i64>,
}

/// One resolved group-member row for the profile panel. The
/// address is the participant JID, used only as the target of
/// backend commands; it is never rendered. The label is the
/// person's resolved display name, or a masked identifier when
/// no name is known. The detail is the secondary line: the phone
/// number, present only under a resolved name for a
/// number-addressed participant (a LID-addressed row shows no
/// number, matching how WhatsApp hides it).
#[derive(Clone)]
pub(crate) struct MemberRow {
    pub(crate) address: String,
    pub(crate) label: String,
    pub(crate) detail: Option<String>,
    pub(crate) admin: bool,
}

/// Group state: member lists, modes and invite data reported by the backend, plus the join-by-link and create-group dialog state.
pub(crate) struct GroupsState {
    /// Live mode: real group member lists fetched on demand for
    /// the profile panel, labels resolved on arrival (see
    /// `MemberRow`).
    pub(crate) live_members: std::collections::HashMap<u64, Vec<MemberRow>>,
    /// Live mode: whether my own account is an admin of the
    /// group, as the backend computed it from the same group
    /// metadata that produced `live_members`. Gates the
    /// admin-only message actions; absent means unknown, and
    /// unknown never shows an admin action.
    pub(crate) own_admin: std::collections::HashMap<u64, bool>,
    /// Chats whose member list was already requested this panel
    /// session (avoids re-requesting every frame).
    pub(crate) members_requested: std::collections::HashSet<u64>,
    /// The invite link fetched for the group whose dock is open:
    /// (chat id, link). Cleared when the dock switches chats.
    pub(crate) invite_link: Option<(u64, String)>,
    /// The add-participant address awaiting its outcome
    /// (chat id, address as sent): the input keeps the text
    /// until the refreshed member list lands (P2-37), so a
    /// failed add never eats what the user typed.
    pub(crate) participant_add_awaiting: Option<(u64, String)>,
    /// Per-group membership approval state as last reported by
    /// the backend (chat id -> approval required).
    pub(crate) approval: std::collections::HashMap<u64, bool>,
    /// Per-group (announce, locked) modes as last reported by
    /// the backend.
    pub(crate) modes: std::collections::HashMap<u64, (bool, bool)>,
    /// Pending membership requests for the group whose dock is
    /// open: (chat id, (jid, resolved display label) rows). The
    /// JID is the review command's target and is never rendered.
    pub(crate) join_requests: Option<(u64, Vec<(String, String)>)>,
    /// The join-group-by-link dialog state.
    pub(crate) join_link_open: bool,
    pub(crate) join_link_input: String,
    pub(crate) join_link_error: Option<String>,
    /// Create-group dialog state: open flag + form buffers.
    pub(crate) create_open: bool,
    /// A live CreateGroup command is in flight: set at send,
    /// cleared by the answering group upsert (which speaks the
    /// Info outcome) or by the failure that replaces the Busy
    /// notice.
    pub(crate) create_pending: bool,
    pub(crate) create_error: Option<String>,
    /// Create-group form: the group name.
    pub(crate) create_name: String,
    /// Create-group form: comma-separated participant addresses.
    pub(crate) create_members: String,
}

/// New-chat-by-number dialog state (design standard section
/// 23): a chat-list dialog in the create/join family. The three
/// lookup outcomes stay distinct in the state itself: a check
/// in flight, a definitive not-on-WhatsApp answer, and a
/// failed lookup (an error, never reported as not found).
pub(crate) struct NewChatState {
    /// The dialog is open.
    pub(crate) open: bool,
    /// The phone number as typed (any formatting; the backend
    /// boundary normalizes it).
    pub(crate) phone: String,
    /// The number whose lookup is in flight, as typed. The
    /// outcome events carry the number back, and only an
    /// outcome for this exact input moves the dialog.
    pub(crate) checking: Option<String>,
    /// The number the server answered is not on WhatsApp.
    pub(crate) not_found: Option<String>,
    /// The lookup failed, in user words, shown inline.
    pub(crate) error: Option<String>,
}

/// Self-update state (see update.rs): the phase, the release on offer, worker results and check bookkeeping.
pub(crate) struct UpdateState {
    /// Self-update: what the updater is doing (see update.rs).
    pub(crate) phase: crate::update::UpdatePhase,
    /// Self-update: the newer release on offer, if any.
    pub(crate) release: Option<core::update::ReleaseInfo>,
    /// Self-update: in-flight worker results.
    pub(crate) rx: Option<std::sync::mpsc::Receiver<crate::update::UpdateMsg>>,
    /// Self-update: last check time (Unix seconds), persisted; the
    /// silent startup check runs at most once a day.
    pub(crate) last_check: u64,
    /// Self-update: whether the silent once-a-day check runs at
    /// all, persisted; the user's opt-out from Settings > About.
    /// Manual checks are unaffected. Default on.
    pub(crate) auto_check: bool,
    /// Self-update: whether the running check was user-triggered
    /// (manual checks surface their failures, silent ones do not).
    pub(crate) manual: bool,
    /// Self-update: the once-per-startup silent check has run.
    pub(crate) startup_done: bool,
    /// Self-update: path to restart into after a successful swap.
    pub(crate) target: Option<std::path::PathBuf>,
}

/// Media picker state: the emoji, GIF and sticker panel, its drafts and results, decoded textures and in-flight workers.
pub(crate) struct PickerState {
    /// Media picker panel open (Emoji/GIF/Stickers).
    pub(crate) open: bool,
    /// Active picker tab id ("emoji" or a native feature tab id).
    pub(crate) tab: String,
    /// The emoji picker's search text.
    pub(crate) emoji_search: String,
    /// Recently used emoji, newest first, persisted; drives the
    /// picker's Recent strip.
    pub(crate) emoji_recents: Vec<String>,
    /// Sticker packs on disk (refreshed when the picker opens).
    pub(crate) sticker_packs: Vec<stickers::Pack>,
    /// New-pack name draft (Stickers tab).
    pub(crate) sticker_new_pack: String,
    /// New text-sticker draft (Stickers tab).
    pub(crate) sticker_new_text: String,
    /// GIF search text (GIPHY tab).
    pub(crate) giphy_query: String,
    /// Current GIF results (GIPHY tab).
    pub(crate) giphy_items: Vec<giphy::GifItem>,
    /// Whether any GIPHY result has arrived for the current
    /// fetch: an empty result set is a real answer ("no GIFs"),
    /// not a wait, so the tab only reads as loading while this
    /// is false. Set on every result message, including an empty
    /// one; cleared when a new fetch starts.
    pub(crate) giphy_responded: bool,
    /// Whether the GIPHY key was rejected (bad-key state).
    pub(crate) giphy_rejected: bool,
    /// Decoded GIF preview textures by preview URL. Bounded (see
    /// `extras::TextureCache`).
    pub(crate) giphy_textures: crate::extras::TextureCache<String>,
    /// Decoded sticker PNG textures, keyed by file path. Bounded
    /// (see `extras::TextureCache`).
    pub(crate) sticker_textures: crate::extras::TextureCache<std::path::PathBuf>,
    /// Sticker files whose preview decode failed, remembered
    /// against the file's stamp so a bad file is attempted once,
    /// not once per repaint, and a replaced file is retried.
    pub(crate) sticker_failures: crate::extras::StickerFailureCache,
    /// Sticker paths a preview worker is currently decoding.
    pub(crate) sticker_pending: std::collections::HashSet<std::path::PathBuf>,
    /// Shared sender handed to sticker preview workers.
    pub(crate) sticker_tx: Option<std::sync::mpsc::Sender<crate::extras::StickerPreviewMsg>>,
    /// In-flight sticker preview worker results.
    pub(crate) sticker_rx: Option<std::sync::mpsc::Receiver<crate::extras::StickerPreviewMsg>>,
    /// In-flight GIPHY worker results.
    pub(crate) giphy_rx: Option<std::sync::mpsc::Receiver<crate::extras::GiphyMsg>>,
    /// In-flight live GIF download (MP4 bytes for one send).
    pub(crate) media_fetch_rx: Option<std::sync::mpsc::Receiver<crate::extras::MediaFetchResult>>,
}

/// The send-relevant composer attachments of one chat, parked
/// while another chat is open: staged files and their caption,
/// the quote and edit targets, and the poll and location drafts.
/// Every one of these is consumed against the open chat at send
/// time, so each belongs to exactly one chat and travels with
/// it, exactly like the parked draft text in
/// [`ComposerState::chat_drafts`].
pub(crate) struct ParkedComposer {
    pub(crate) staged: Vec<StagedMedia>,
    pub(crate) staged_caption: String,
    pub(crate) reply_to: Option<(u64, String, String)>,
    pub(crate) editing_id: Option<u64>,
    pub(crate) poll_draft: Option<PollDraft>,
    pub(crate) location_draft: Option<LocationDraft>,
    pub(crate) event_draft: Option<EventDraft>,
}

impl ParkedComposer {
    fn is_empty(&self) -> bool {
        self.staged.is_empty()
            && self.staged_caption.is_empty()
            && self.reply_to.is_none()
            && self.editing_id.is_none()
            && self.poll_draft.is_none()
            && self.location_draft.is_none()
            && self.event_draft.is_none()
    }
}

/// Composer state: the draft and parked per-chat drafts, staged media and captions, the poll and location drafts, quote and edit targets, and the composer's measured stack heights.
pub(crate) struct ComposerState {
    pub(crate) draft: String,
    /// Composer drafts parked per chat: switching chats stores
    /// the current draft here and restores the target chat's, so
    /// a half-typed message survives a detour. The open chat's
    /// draft lives in `draft`; this map holds the parked ones,
    /// and the chat list reads it for its Draft row state.
    pub(crate) chat_drafts: std::collections::HashMap<u64, String>,
    /// Composer attachments parked per chat while another chat
    /// is open (see [`ParkedComposer`]). Switching chats parks
    /// the live attachments here and restores the target chat's,
    /// so staged files, a quote, or an edit target created in
    /// one chat can never be sent into another.
    pub(crate) chat_parked: std::collections::HashMap<u64, ParkedComposer>,
    /// The chat the currently staged files were attached in.
    /// Recorded at stage time and checked again in
    /// `send_staged`: the hard backstop that refuses the send
    /// if the stage ever outlives a switch that did not park it.
    pub(crate) staged_chat: Option<u64>,
    /// Measured height of the floating composer stack (accessory
    /// lines and pill) from the last frame
    /// it was drawn. The conversation reserves exactly this much
    /// bottom clearance, so the clearance cannot desync when the
    /// stack changes shape.
    pub(crate) stack_h: f32,
    /// The composer's "+" rich-message menu is open.
    pub(crate) rich_menu_open: bool,
    /// The poll builder draft, when open.
    pub(crate) poll_draft: Option<PollDraft>,
    /// The event builder draft, when open (attach menu).
    pub(crate) event_draft: Option<EventDraft>,
    /// Files staged in the composer (attach menu), sent together.
    pub(crate) staged: Vec<StagedMedia>,
    /// Caption typed for the staged media (applies to the first
    /// photo of the set, or the document).
    pub(crate) staged_caption: String,
    /// The location form, when open (attach menu).
    pub(crate) location_draft: Option<LocationDraft>,
    /// Quote context for the next sent message: (id, sender, excerpt).
    pub(crate) reply_to: Option<(u64, String, String)>,
    /// The message currently being edited, if any.
    pub(crate) editing_id: Option<u64>,
}

/// What a transient notice is doing, so its banner speaks in
/// the right voice (design standard section 14): an ordinary
/// result in the plain voice, work still in flight with the
/// shared spinner riding in front of its text, and a failure
/// in the danger token like every other failure in the app.
/// A notice is replaced wholesale by the next one, so an
/// in-flight notice never outlives the result that replaces
/// or clears it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoticeKind {
    Info,
    Busy,
    Error,
}

/// A transient user-facing notice: its text and its kind.
/// Built through the named constructors so every assignment
/// site declares which voice it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Notice {
    pub text: String,
    pub kind: NoticeKind,
}

impl Notice {
    pub(crate) fn info(text: impl Into<String>) -> Self {
        Self { text: text.into(), kind: NoticeKind::Info }
    }

    pub(crate) fn busy(text: impl Into<String>) -> Self {
        Self { text: text.into(), kind: NoticeKind::Busy }
    }

    pub(crate) fn error(text: impl Into<String>) -> Self {
        Self { text: text.into(), kind: NoticeKind::Error }
    }
}

impl App {
    /// The standing notice's text, if any: the read the banner's
    /// duplicate suppression and the tests need, without
    /// threading the kind through call sites that only care
    /// what the notice says.
    pub(crate) fn notice_str(&self) -> Option<&str> {
        self.notice.as_ref().map(|n| n.text.as_str())
    }

    /// Clears the standing notice when it is a Busy one: the
    /// in-flight marker an async start left behind, whose
    /// completion has now landed without a result notice of
    /// its own. A standing Info or Error notice is a result
    /// and is never cleared by this.
    pub(crate) fn clear_busy_notice(&mut self) {
        if self.notice.as_ref().is_some_and(|n| n.kind == NoticeKind::Busy) {
            self.notice = None;
        }
    }
}

/// One global message-search result (design standard section
/// 25): a message in some chat whose visible text matched the
/// chat list's search query.
pub(crate) struct GlobalHit {
    /// The chat the message belongs to (app chat id).
    pub(crate) chat_id: u64,
    /// The message's app id, for the stream locate.
    pub(crate) msg_id: u64,
    /// The chat's display title.
    pub(crate) chat_title: String,
    /// The sender's display name (`None` for own messages).
    pub(crate) sender: Option<String>,
    /// The matched text, windowed around the match.
    pub(crate) snippet: String,
    /// The result's date line (chat-list date language).
    pub(crate) when: String,
}

/// The chat list's global message search: results are cached
/// against the exact query string and recomputed only when it
/// changes, never per frame.
#[derive(Default)]
pub(crate) struct GlobalSearch {
    /// The query the cached hits belong to.
    pub(crate) query: String,
    /// The cached hits, newest first.
    pub(crate) hits: Vec<GlobalHit>,
    /// Whether more hits exist beyond the shown cap.
    pub(crate) capped: bool,
}

/// The in-chat search session (section 25): one strip under
/// the conversation header, stepping through the open chat's
/// matches.
#[derive(Default)]
pub(crate) struct ChatSearch {
    /// Whether the strip is open.
    pub(crate) open: bool,
    /// The query as typed.
    pub(crate) query: String,
    /// The current match's message id (the steady wash target).
    pub(crate) current: Option<u64>,
    /// The date-jump popup is open inside the strip.
    pub(crate) date_open: bool,
    /// The date popup's text field (YYYY-MM-DD).
    pub(crate) date_text: String,
    /// The date popup shows its inline error line.
    pub(crate) date_error: bool,
    /// The cache key the match list was computed for: (query,
    /// conversation length, first id, last id). A changed key
    /// recomputes the matches; an unchanged one reuses them.
    pub(crate) cache_key: (String, usize, u64, u64),
    /// Matching message ids in conversation order.
    pub(crate) matches: Vec<u64>,
    /// The query field takes focus on the next draw (armed
    /// wherever the strip opens).
    pub(crate) focus_armed: bool,
    /// The chat the session belongs to: the strip resets
    /// itself when the open chat changes (a search is a
    /// per-chat session, never carried across chats).
    pub(crate) chat_id: u64,
}

/// Which gallery section the profile dock is showing
/// (section 25).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GallerySection {
    /// Photos and videos grid.
    Media,
    /// Links list.
    Links,
    /// Documents list.
    Docs,
}

/// The photo editor's aspect presets (section 25). `Free`
/// leaves the crop rectangle's corners independent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CropAspect {
    /// No constraint.
    #[default]
    Free,
    /// The frame's own aspect.
    Original,
    /// 1:1.
    Square,
    /// 4:3 landscape.
    FourThree,
    /// 3:4 portrait.
    ThreeFour,
    /// 16:9 landscape.
    SixteenNine,
    /// 9:16 portrait.
    NineSixteen,
}

impl CropAspect {
    /// The preset's width-over-height ratio, or `None` for
    /// Free. Original resolves against the working image.
    pub(crate) fn ratio(self, image_aspect: f32) -> Option<f32> {
        match self {
            Self::Free => None,
            Self::Original => Some(image_aspect),
            Self::Square => Some(1.0),
            Self::FourThree => Some(4.0 / 3.0),
            Self::ThreeFour => Some(3.0 / 4.0),
            Self::SixteenNine => Some(16.0 / 9.0),
            Self::NineSixteen => Some(9.0 / 16.0),
        }
    }
}

/// The photo editor session (section 25): one staged photo
/// being cropped and rotated before it is sent. The working
/// image is the decoded source with rotations already
/// applied; the crop rectangle lives in normalized
/// coordinates over it.
pub(crate) struct PhotoEditor {
    /// Index of the staged item being edited (composer staged
    /// list of the chat the editor was opened from).
    pub(crate) staged_index: usize,
    /// The chat the staged item belongs to.
    pub(crate) chat_id: u64,
    /// The working image (rotations applied).
    pub(crate) image: image::DynamicImage,
    /// The working image's preview texture, rebuilt whenever
    /// the image changes (a rotation).
    pub(crate) texture: Option<egui::TextureHandle>,
    /// The crop rectangle: normalized (x0, y0, x1, y1) over
    /// the working image.
    pub(crate) crop: [f32; 4],
    /// The active aspect preset.
    pub(crate) aspect: CropAspect,
}

pub(crate) struct App {
    pub(crate) screen: Screen,
    pub(crate) theme_choice: ThemeId,
    pub(crate) tokens: Tokens,
    /// Theme crossfade state: the token set the current one is
    /// blending from, the target it is blending toward, and
    /// when the blend started. All None at rest.
    pub(crate) theme_fade_from: Option<Tokens>,
    pub(crate) theme_fade_target: Option<Tokens>,
    pub(crate) theme_fade_start: Option<std::time::Instant>,
    pub(crate) chats: Vec<Chat>,
    /// Placeholder returned by `selected()` when the chat list is
    /// empty (live mode before the first chat arrives).
    pub(crate) fallback_chat: Chat,
    pub(crate) conversation: Vec<Message>,
    pub(crate) selected_chat: u64,
    /// Deferred `--select-chat` for live mode: backend chats arrive
    /// via events after startup, so the selection is applied by
    /// `poll_backend` once the chat exists.
    pub(crate) pending_select_chat: Option<u64>,
    pub(crate) filter: ChatFilter,
    pub(crate) search: String,
    /// The Starred panel's own search text (the chat list's
    /// `search` keeps belonging to the chat list alone).
    pub(crate) starred_search: String,
    /// Calls state: the call log behind the Calls panel, its search and filters, the new-call picker, and the one live call's UI state.
    pub(crate) calls: CallsState,
    /// Updates state: my posted status for the session and the status composer modal's form state.
    pub(crate) updates: UpdatesState,
    /// Composer state: the draft and parked per-chat drafts, staged media and captions, the poll and location drafts, quote and edit targets, and the composer's measured stack heights.
    pub(crate) composer: ComposerState,
    /// The Settings search text (the registry and results live
    /// in settings.rs).
    pub(crate) settings_search: String,
    /// QA hook (--scroll-top): open conversations at the top
    /// instead of sticking to the newest message.
    pub(crate) qa_scroll_top: bool,
    /// QA hook (--scroll-to N): locate the Nth message of the
    /// first conversation opened, once.
    pub(crate) qa_scroll_to: Option<usize>,
    /// QA hook (--community-scroll <px>): the community home's
    /// initial vertical scroll offset.
    pub(crate) qa_community_scroll: f32,
    /// The settings row a search activation just jumped to: it
    /// highlights briefly and the highlight fades (settings.rs).
    pub(crate) settings_highlight: Option<crate::settings::SettingHighlight>,
    /// Whether the Appearance page's Advanced disclosure (the
    /// saturation and contrast calibration sliders) is open.
    /// Session state only, collapsed by default; the sliders'
    /// values persist as before, this flag never does.
    pub(crate) appearance_advanced_open: bool,
    /// The user's font-size preference (Settings, Appearance):
    /// a percent of the designed UI size, persisted in
    /// settings.json. The shell composes it with the layout
    /// mode's automatic density factor (see
    /// [`effective_pixels_per_point`]).
    pub(crate) zoom_percent: u32,
    pub(crate) enter_sends: bool,
    pub(crate) keep_archived: bool,
    pub(crate) sidebar_collapsed: bool,
    /// The current frame's layout mode, refreshed by the shell
    /// from the window width on every frame: true below
    /// [`SINGLE_PANE_BREAKPOINT`], where the list and the
    /// conversation take turns instead of sharing the width.
    pub(crate) single_pane: bool,
    /// The current frame's layout mode is the portrait one
    /// (single-pane territory and taller than wide): the rail
    /// gives way to the bottom navigation bar and the screens
    /// follow the native phone arrangement (see portrait.rs).
    /// Refreshed by the shell alongside `single_pane`, which
    /// stays true in portrait mode: every "not two-pane"
    /// behavior (pane swapping, the back chevron, the full-width
    /// profile dock) applies there too.
    pub(crate) portrait: bool,
    /// Single-pane navigation: a conversation is open over the
    /// list. Set whenever a chat is selected, cleared by the
    /// conversation's back affordance and by rail navigation.
    /// Ignored by the two-pane layout, where the conversation
    /// is always on screen.
    pub(crate) conversation_open: bool,
    /// Single-pane Settings navigation: the selected category's
    /// page is showing instead of the category list. Ignored by
    /// the two-pane layout, where nav and page sit side by side.
    pub(crate) settings_page_open: bool,
    pub(crate) settings_tab: &'static str,
    pub(crate) pair_mode: PairMode,
    /// The full international number the pairing request
    /// consumes, composed from the selected country and the
    /// national number on every edit (see `countries`).
    pub(crate) pair_phone: String,
    /// The pairing form's selected country (ISO alpha-2),
    /// preselected from the device locale; its dial code is
    /// what `pair_phone` is composed with.
    pub(crate) pair_country_iso: String,
    /// The pairing form's national number as typed, without a
    /// country code.
    pub(crate) pair_national: String,
    /// Whether the country picker popup is open.
    pub(crate) country_picker_open: bool,
    /// The country picker's search text.
    pub(crate) country_search: String,
    /// The country selector's rect from the last drawn frame.
    /// The picker anchors to it (dropping from the selector at
    /// the connect card's width) instead of floating over the
    /// window, so it needs the rect one frame after it is drawn.
    pub(crate) country_anchor: Option<egui::Rect>,
    /// The connect card's rect from the last drawn frame; the
    /// country picker takes its width from it.
    pub(crate) connect_card_rect: Option<egui::Rect>,
    pub(crate) pair_code_shown: bool,
    pub(crate) account_menu_open: bool,
    /// The egui frame on which the rail avatar toggle last
    /// opened the account menu, if it was opened that way.
    /// The menu's outside-click check runs later in that same
    /// frame and would otherwise read the opening click itself
    /// as a dismissal (the avatar sits outside the modal), so
    /// the check skips the frame recorded here.
    pub(crate) account_menu_open_frame: Option<u64>,
    pub(crate) profile_anchor: Option<egui::Pos2>,
    pub(crate) notify_messages: bool,
    pub(crate) notify_groups: bool,
    pub(crate) notify_sound: bool,
    pub(crate) notify_preview: bool,
    pub(crate) read_receipts: bool,
    /// The account default disappearing-messages timer in
    /// seconds (0 is off), set in Settings > Privacy. Live mode
    /// sends it to WhatsApp; demo mode seeds it onto groups
    /// created here.
    pub(crate) default_ephemeral_secs: u32,
    pub(crate) cache_cleared: bool,
    /// Media state: decoded photo textures, the photo viewer and view-once tracking, and voice-note playback.
    pub(crate) media: MediaState,
    /// The unread divider for the open conversation, snapshotted
    /// when the chat was opened. The read-by-action flush clears
    /// `chat.unread` as the user engages, but the divider stays
    /// where it was placed until the chat is switched.
    pub(crate) unread_divider: Option<UnreadDivider>,
    /// One-shot request from the scroll-to-bottom button: the
    /// message stream consumes it on the next frame and runs its
    /// animated scroll to the newest message.
    pub(crate) scroll_bottom_requested: bool,
    /// Native feature on/off state, keyed by short feature id
    /// ("giphy", "stickers", ...). Every feature ships inside the
    /// app; this map is the user's switches, persisted in
    /// settings.json. Seeded from the pre-native install book on
    /// first run so an upgrade keeps the user's choices.
    pub(crate) feature_flags: std::collections::BTreeMap<String, bool>,
    /// GIF search preferences (rating, result limit), kept as a
    /// raw map because `giphy::fetch_params` validates it.
    pub(crate) giphy_prefs: serde_json::Map<String, serde_json::Value>,
    pub(crate) sent_counter: u64,
    /// The last second the disappearing-messages expiry pass ran
    /// (the pass is throttled to once per second).
    pub(crate) last_expiry_scan: u64,
    /// The expiry clock's anchor (P1-53): the wall second the
    /// monotonic anchor was taken at, with the instant it was
    /// taken. Expiry and mute lifting read time through this
    /// anchor, never from the raw wall clock, so a forward
    /// clock jump cannot mass-delete messages.
    pub(crate) expiry_anchor: Option<(u64, std::time::Instant)>,
    /// True when the shell renders live data instead of the demo set.
    pub(crate) live_mode: bool,
    /// The running live session, when live mode is on.
    pub(crate) live: Option<crate::live::LiveSession>,
    /// Why the most recent live start failed before a session
    /// existed (for example the database key could not be read),
    /// when it did. The connect screen renders this state instead
    /// of any pairing surface: a start that never produced a
    /// session must not present a scannable-looking code or a
    /// waiting-for-scan status. Cleared by the next start attempt.
    pub(crate) live_start_error: Option<String>,
    /// A live session ended by logging out and no new session
    /// has started since. The connect screen renders its
    /// logged-out state (not the demo panel) while this is set;
    /// cleared when a fresh session starts.
    pub(crate) logged_out: bool,
    /// Transient user-facing notice (backend errors, feature
    /// crashes, export results). Shown as a dismissible banner.
    pub(crate) notice: Option<Notice>,
    /// A crash report from a previous run that the user has not
    /// dismissed yet (crash-safety flow).
    pub(crate) crash_report: Option<core::crash::CrashReport>,
    /// Whether the full crash report text is being viewed.
    pub(crate) crash_details_open: bool,
    /// Whether the open crash dialog's Copy action has run: the
    /// button confirms in place ("Copied") instead of raising a
    /// notice over the dialog.
    pub(crate) crash_copied: bool,
    /// The egui context, kept so the account modal can start live
    /// mode (the backend wake hook needs it) after startup.
    pub(crate) egui_ctx: Option<egui::Context>,
    /// Appearance fine-tuning (Settings > Appearance sliders),
    /// applied over the active theme's tokens via
    /// `core::theme::Tokens::adjust`. Defaults are 1.0 / 1.0.
    pub(crate) saturation: f32,
    /// See `saturation`.
    pub(crate) contrast: f32,
    /// Seed color the whole dynamic palette is generated from.
    pub(crate) seed: Rgb,
    /// The central credential store (keyring or file backend,
    /// plus `.env.local` overrides loaded at startup).
    pub(crate) credentials: core::credentials::CredentialStore,
    /// Draft text for the "add a key" field in the credentials UI,
    /// keyed by slot id. Values are secrets, never logged.
    pub(crate) credential_drafts: std::collections::HashMap<String, String>,
    /// Send typing indicators (Settings > Privacy). When OFF the
    /// app never emits composing/pause chat-states.
    pub(crate) typing_indicators: bool,
    /// Legacy link-previews preference. Received links are
    /// never fetched (a fetch would hand the sender the user's
    /// address and online timing), and previews render only
    /// when the sender's client attached one, so this value no
    /// longer gates anything. It is still loaded and persisted
    /// so existing settings files keep round-tripping.
    pub(crate) link_previews: bool,
    /// Privacy values reported by the account (live mode), spelling
    /// per `backend::BackendCommand::SetPrivacy`.
    pub(crate) privacy_state: std::collections::BTreeMap<String, String>,
    /// App message ID → protocol message ID (live mode). Holds
    /// an entry per message in the session mirror (plus, briefly,
    /// a send whose binding landed before its upsert); when a
    /// message leaves the mirror its entry is dropped with it
    /// (see `App::forget_live_message`), so the map tracks the
    /// mirror instead of the whole session history.
    pub(crate) protocol_ids: std::collections::HashMap<u64, String>,
    /// App message ID → author JID (live mode): the participant a
    /// reply quotes. Same lifecycle as `protocol_ids`.
    pub(crate) sender_jids: std::collections::HashMap<u64, String>,
    /// Chat ID → protocol IDs of unread incoming messages (live).
    pub(crate) unread_live: std::collections::HashMap<u64, Vec<String>>,
    /// Receipt history per app message ID: (kind, "HH:MM").
    pub(crate) receipt_log: std::collections::HashMap<u64, Vec<(backend::ReceiptKind, String)>>,
    /// First-insertion order of `receipt_log`'s keys, so the
    /// log's size cap can drop the oldest histories instead of
    /// clearing the whole map (VRF-05).
    pub(crate) receipt_log_order: std::collections::VecDeque<u64>,
    /// Live mode: chats whose peer is currently typing, mapped to
    /// the local time the last typing event arrived (the header
    /// shows "typing..." for a few seconds after it).
    pub(crate) live_typing: std::collections::HashMap<u64, std::time::Instant>,
    /// Live mode: the watched DM contact's presence, as last
    /// reported by the backend. The presence watch subscribes to
    /// exactly one contact (the open DM conversation), and an
    /// entry is dropped the moment its feed stops: the chat is
    /// left, the connection drops, or the session ends. The
    /// header therefore never presents presence it is no longer
    /// receiving as if it were current.
    pub(crate) live_presence: std::collections::HashMap<u64, ContactPresence>,
    /// The DM chat the backend is currently asked to watch
    /// presence for: (chat ID, JID). Kept next to the state it
    /// feeds so `sync_presence_watch` can move the subscription
    /// as the open conversation changes.
    pub(crate) presence_watch: Option<(u64, String)>,
    /// Who is typing in a chat, by name, for the chat list's
    /// typing row state. Demo mode seeds it; the live layer
    /// reports typing through `live_typing` instants instead,
    /// which carry no name.
    pub(crate) typing_names: std::collections::HashMap<u64, String>,
    /// Group state: member lists, modes and invite data reported by the backend, plus the join-by-link and create-group dialog state.
    pub(crate) groups: GroupsState,
    /// New-chat-by-number dialog state (see NewChatState).
    pub(crate) new_chat: NewChatState,
    /// Contacts blocked from this client (live: also sent to the
    /// backend; demo: local state with an honest caption).
    /// Persisted in the encrypted archive store (privacy audit
    /// A6; older builds kept it in the settings file, whose copy
    /// is migrated on the first live start), and replaced by the
    /// backend's connect-time Blocklist event when a live session
    /// reports the server's list: the persisted copy only covers
    /// the time before that fetch lands.
    pub(crate) blocked_chats: std::collections::HashSet<u64>,
    /// Profile panel group-edit form state.
    pub(crate) profile_form_for: Option<u64>,
    /// A destructive action waiting on the house confirmation
    /// modal (drawn by `rich_ui::menus::draw_ui_confirm`).
    /// Destructive actions never fire from the surface that asked
    /// for them: the row or menu records the intent here, and
    /// the centered modal, away from the click that opened it,
    /// is what confirms.
    pub(crate) ui_confirm: Option<UiConfirm>,
    /// The clear-chat modal's keep-starred checkbox state
    /// (design standard section 24). Defaults to keeping starred
    /// messages, the safer reading of a clear; the modal resets
    /// it every time it opens.
    pub(crate) clear_keep_starred: bool,
    /// The attach menu's contact picker dialog is open (design
    /// standard section 24): it lists the contacts the roster
    /// knows by phone number and sends the picked one as a
    /// contact card into the open chat.
    pub(crate) contact_picker_open: bool,
    /// Profile panel: the add-participant input.
    pub(crate) add_participant_input: String,
    /// Profile panel: the group subject edit buffer.
    pub(crate) group_subject_edit: String,
    /// Profile panel: the group description edit buffer.
    pub(crate) group_desc_edit: String,
    /// Motion preference (Settings > Appearance).
    pub(crate) motion: crate::motion::MotionSetting,
    /// Interface language id ("en" or "id"), persisted; the
    /// active locale itself lives in the i18n module.
    pub(crate) language: String,
    /// Runtime animation state: in-flight runs and their edge-detection flags. The motion setting itself stays on App.
    pub(crate) anim: AnimState,
    /// `"<chat>:<msg>"` → actor → current reaction emoji.
    pub(crate) reaction_actors:
        std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    /// My current reaction per app message ID.
    pub(crate) my_reactions: std::collections::HashMap<u64, String>,
    /// Live mode: my reaction per app message ID as it was
    /// before a change the backend has not yet settled (None =
    /// I had none). The entry is taken when the change is sent;
    /// the server's echo of my reaction drops it (confirmed),
    /// and a ReactionFailed restores it verbatim (rolled back).
    pub(crate) pending_reactions: std::collections::HashMap<u64, Option<String>>,
    /// Live mode, channels only: a channel update's aggregate
    /// reaction counts before an optimistic reaction change.
    /// Channel counts have no actor list to derive them from
    /// (unlike chat reactions), so the rollback restores this
    /// snapshot verbatim; ChannelReactionSet drops it.
    pub(crate) pending_channel_reactions: std::collections::HashMap<u64, Vec<(String, u32)>>,
    /// Live mode: the starred value of a message before a
    /// star change the backend has not yet settled. The entry
    /// is taken when the change is sent; a server report of
    /// the message's star state (StarChanged, or an upsert
    /// already carrying the new state) drops it (confirmed),
    /// and a StarFailed restores it verbatim (rolled back).
    pub(crate) pending_stars: std::collections::HashMap<u64, bool>,
    /// Live mode: the pinned value of a message before a pin
    /// change the backend has not yet settled; the pin
    /// counterpart of `pending_stars`, settled by an upsert
    /// carrying the new state and restored by a PinFailed.
    pub(crate) pending_pins: std::collections::HashMap<u64, bool>,
    /// Live mode: the kept value of a message before a keep
    /// change the backend has not yet settled; the keep
    /// counterpart of `pending_stars`, settled by a MessageKept
    /// report and restored by a KeepFailed.
    pub(crate) pending_keeps: std::collections::HashMap<u64, bool>,
    /// Live mode: my poll selection before a vote the backend
    /// has not yet settled, per poll message ID. A PollVote
    /// echo carrying my selection drops the entry (confirmed);
    /// a PollVoteFailed re-applies the stored selection as my
    /// ballot, which recomputes the tally back (rolled back).
    pub(crate) pending_poll_votes: std::collections::HashMap<u64, Vec<usize>>,
    /// Live mode: my event response before an answer the
    /// backend has not yet settled, per event message ID (None
    /// when I had not answered). An EventResponse echo carrying
    /// my answer drops the entry (confirmed); an
    /// EventRespondFailed re-applies the stored answer (or
    /// retracts mine when there was none), which recomputes
    /// the tally back (rolled back). The event counterpart of
    /// `pending_poll_votes`.
    pub(crate) pending_event_rsvps: std::collections::HashMap<u64, Option<backend::EventRsvp>>,
    /// Live mode: an event's full kind before an edit the
    /// backend has not yet settled, per event message ID. An
    /// EventEdited report drops the entry (confirmed); an
    /// EventEditFailed re-applies the stored kind (rolled
    /// back).
    pub(crate) pending_event_edits: std::collections::HashMap<u64, core::model::MessageKind>,
    /// The event edit form, when open: the chat and message it
    /// targets plus the draft pre-filled from the event. Bound
    /// to the message, not the open chat, so a chat switch
    /// mid-edit can never retarget the submit.
    pub(crate) event_edit_draft: Option<(u64, u64, EventDraft)>,
    /// Extra-guest counts chosen on event cards, per event
    /// message ID: the stepper's value rides with my Going
    /// answer when it changes.
    pub(crate) event_guests: std::collections::HashMap<u64, i32>,
    /// Live mode: a chat's (archived, pinned, muted, mute_until)
    /// before a chat action the backend has not yet settled,
    /// keyed by chat ID and the action's field group ("archive",
    /// "pin", "mute"). A matching Changed report drops the
    /// entry; a ChatActionFailed restores only the fields its
    /// action owns.
    pub(crate) pending_chat_actions:
        std::collections::HashMap<(u64, &'static str), (bool, bool, bool, i64)>,
    /// Live mode: a chat's unread count before a mark-unread
    /// the backend has not yet settled. A ChatReadChanged drops
    /// the entry; a MarkUnreadFailed restores the count.
    pub(crate) pending_unread: std::collections::HashMap<u64, u32>,
    /// Live mode: a chat's blocked state before a block toggle
    /// the backend has not yet settled. A Blocklist report that
    /// matches the intended state drops the entry and speaks
    /// the success notice; a BlockFailed restores the state.
    pub(crate) pending_blocks: std::collections::HashMap<u64, bool>,
    /// Live mode: a group's title before a group-info save,
    /// with the subject the save carries. A GroupInfoSaved
    /// applies the subject and speaks the saved notice; a
    /// GroupInfoFailed drops the entry (the title was never
    /// applied) and speaks the failure.
    pub(crate) pending_group_info: std::collections::HashMap<u64, (String, Option<String>)>,
    /// Live mode: a group's archived flag before a leave the
    /// backend has not yet settled. A LeaveGroupFailed restores
    /// it (mirror and archive).
    pub(crate) pending_leave: std::collections::HashMap<u64, bool>,
    /// Live mode: a chat's disappearing-timer seconds before a
    /// change the backend has not yet settled. A
    /// DisappearingChanged drops the entry; a
    /// DisappearingTimerFailed restores the seconds quietly
    /// (no new in-stream announcement).
    pub(crate) pending_timers: std::collections::HashMap<u64, u32>,
    /// Live mode: a message's (body, edited flag) before an
    /// edit the backend has not yet settled. A MessageEdited
    /// drops the entry; an EditMessageFailed restores both.
    pub(crate) pending_edits: std::collections::HashMap<u64, (String, bool)>,
    /// Live mode: a message's deleted flag before a revoke the
    /// backend has not yet settled. A MessageRevoked drops the
    /// entry; a RevokeMessageFailed restores the flag.
    pub(crate) pending_revokes: std::collections::HashMap<u64, bool>,
    /// Live mode: the archived row a delete-for-me removed
    /// optimistically (chat ID, the row, its flags), per
    /// message ID. A MessageDeletedForMe report drops the
    /// entry (the removal stands); a DeleteForMeFailed writes
    /// the row back to the archive and re-adopts it into the
    /// mirror and the open conversation.
    pub(crate) pending_delete_for_me:
        std::collections::HashMap<u64, (u64, backend::LiveMessage, i64)>,
    /// Live mode: chats whose clear this client sent and whose
    /// confirming ChatCleared report has not landed yet, with
    /// the modal's keep-starred choice. Nothing is deleted
    /// locally while an entry stands: the local apply is the
    /// confirmation's, not the click's. A ClearChatFailed drops
    /// the entry and speaks the failure.
    pub(crate) pending_clears: std::collections::HashMap<u64, bool>,
    /// Live mode: chats whose delete this client sent and whose
    /// confirming ChatDeleted report has not landed yet. The
    /// delete counterpart of `pending_clears`.
    pub(crate) pending_chat_deletes: std::collections::HashSet<u64>,
    /// Media picker state: the emoji, GIF and sticker panel, its drafts and results, decoded textures and in-flight workers.
    pub(crate) picker: PickerState,
    /// The live voice recorder, present while a recording runs.
    pub(crate) recorder: Option<crate::recorder::Recorder>,
    /// The profile panel's add-member form is open (live groups).
    pub(crate) add_member_open: bool,
    /// Message whose info popover is open, if any.
    pub(crate) message_info_id: Option<u64>,
    /// Message whose reaction picker is open, if any.
    pub(crate) reaction_picker_id: Option<u64>,
    /// Profile panel (contact/group info) open for the selected chat.
    pub(crate) profile_open: bool,
    /// "My profile" editor state: the signed-in user's own name,
    /// about line and photo, seeded from settings.json at startup
    /// (extras.rs) and written back on the panel's save intent.
    pub(crate) my_profile: crate::profile::MyProfileState,
    /// "My profile" editor open (account menu entry).
    pub(crate) my_profile_open: bool,
    /// Community groups joined this session from a community
    /// home's Other groups section (demo data; live mode has no
    /// other groups).
    pub(crate) joined_community_groups: std::collections::HashSet<u64>,
    /// Sticker filehashes the account's other devices marked
    /// favorite (app-state sync, P1-01), and the recent-sticker
    /// removals seen this session. Bookkeeping for the synced
    /// sticker sets: the picker surfaces local packs today, so
    /// these sets are the consumed sync state a synced-favorites
    /// surface will read; nothing is displayed from them yet.
    pub(crate) synced_sticker_favorites: std::collections::HashSet<String>,
    pub(crate) synced_recent_sticker_removals: Vec<String>,
    /// The community whose description is expanded on its home,
    /// if any.
    pub(crate) community_desc_expanded: Option<u64>,
    /// Community home open in the content area: the community id
    /// (see community.rs for how ids resolve in demo/live mode).
    pub(crate) open_community: Option<u64>,
    /// Whether the open community home was entered from the
    /// Communities overview page (rather than the chat-list
    /// community row): its back affordance returns to the
    /// overview instead of just closing the home.
    pub(crate) community_home_from_overview: bool,
    /// One-shot request from the Starred page: the message stream
    /// scrolls this message into view on the next draw, then
    /// clears the request.
    pub(crate) scroll_to_message: Option<u64>,
    /// Message currently wearing the locate highlight (set
    /// together with `scroll_to_message` by the Starred page);
    /// the stream fades it out and clears it.
    pub(crate) highlight_message: Option<u64>,
    /// egui time the locate highlight started, stamped by the
    /// stream on its first highlighted frame.
    pub(crate) highlight_started: Option<f64>,
    /// P2-67 windowing: settled row heights of the open
    /// conversation, keyed by row (see
    /// `conversation::anchor`). Measured rows replace their
    /// estimates as they render; the map is dropped when the
    /// open chat changes.
    pub(crate) stream_heights: std::collections::HashMap<crate::conversation::anchor::RowKey, f32>,
    /// P2-67 windowing: the scroll anchor captured at the end
    /// of the last stream draw (row key + intra-row offset),
    /// the scroll offset it was captured at, and the chat it
    /// belongs to. A frame whose model moved rows above the
    /// anchor while the offset stood still (a history prepend,
    /// a measurement correction) restores the anchor's exact
    /// viewport position.
    pub(crate) stream_anchor: Option<(crate::conversation::anchor::RowKey, f32)>,
    /// The scroll offset the anchor was captured at.
    pub(crate) stream_anchor_offset: f32,
    /// The chat the anchor and heights belong to.
    pub(crate) stream_anchor_chat: u64,
    /// Last frame's scroll offset and viewport height, the
    /// window computation's inputs for this frame (one frame
    /// of lag, covered by the overscan).
    pub(crate) stream_last_offset: f32,
    /// Last frame's viewport height.
    pub(crate) stream_last_viewport: f32,
    /// Whether the last stream draw rested at the bottom:
    /// stick-to-bottom territory, where the anchor fix never
    /// applies (the stick owns the position there).
    pub(crate) stream_was_bottom: bool,
    /// The chat list's global message search (section 25):
    /// cached results for the list search's current query.
    pub(crate) global_search: GlobalSearch,
    /// The in-chat search session (section 25).
    pub(crate) chat_search: ChatSearch,
    /// The profile dock's gallery section, when the dock is
    /// showing the gallery instead of the profile (section 25).
    pub(crate) gallery_section: Option<GallerySection>,
    /// The photo editor session (section 25), when open.
    pub(crate) photo_editor: Option<PhotoEditor>,
    /// The ZIP export's attachment collection in flight
    /// (section 25), when one is gathering.
    pub(crate) zip_gather: Option<crate::live::ZipGather>,
    /// The ZIP export worker's result channel.
    pub(crate) zip_export_rx: Option<std::sync::mpsc::Receiver<Result<std::path::PathBuf, String>>>,
    /// A chat that can be open in the conversation view without
    /// belonging to the chat list: the community announcement
    /// chat. It lives in this slot instead of `chats` precisely so
    /// the list never renders it as a regular row; selection, the
    /// header and the profile resolve it through here.
    pub(crate) detached_chat: Option<Chat>,
    /// The community the open chat was entered from (through its
    /// home), when it was: the conversation header offers a way
    /// back to that home while it is set and the open chat still
    /// belongs to the community.
    pub(crate) community_origin: Option<u64>,
    /// The Channels panel's find-by-invite form is open.
    pub(crate) channel_find_open: bool,
    /// The find-by-invite form's input (a link or a bare code).
    pub(crate) channel_find_text: String,
    /// The channel the last invite lookup found, shown as a
    /// preview card in the Channels panel until it is followed
    /// or the form closes.
    pub(crate) channel_found: Option<backend::LiveChannel>,
    /// The Channels panel's own inline message (a lookup or
    /// follow outcome the panel states in place, where the
    /// action was attempted, instead of a global banner).
    pub(crate) channel_panel_notice: Option<String>,
    /// The "New channel" form is open.
    pub(crate) channel_create_open: bool,
    /// The "New channel" form's name field.
    pub(crate) channel_create_name: String,
    /// The "New channel" form's description field.
    pub(crate) channel_create_desc: String,
    /// The "New channel" form's inline validation error.
    pub(crate) channel_create_error: Option<String>,
    /// The channel info panel is open for this chat id.
    pub(crate) channel_info_for: Option<u64>,
    /// The channel poll composer's target chat id, while the
    /// composer is open (admins post polls from the channel
    /// conversation's attach menu).
    pub(crate) channel_poll_for: Option<u64>,
    /// The channel poll composer's question field.
    pub(crate) channel_poll_question: String,
    /// The channel poll composer's option fields.
    pub(crate) channel_poll_options: Vec<String>,
    /// The channel poll composer's multiple-answers toggle.
    pub(crate) channel_poll_multiple: bool,
    /// The "New community" form is open (the Communities overview
    /// panel's lead row opens it).
    pub(crate) community_create_open: bool,
    /// The "New community" form's name field.
    pub(crate) community_create_name: String,
    /// The "New community" form's inline validation error.
    pub(crate) community_create_error: Option<String>,
    /// Community home management state: whether the link-a-group
    /// picker is expanded on the open community's home.
    pub(crate) community_link_picker_open: bool,
    /// Community home management state: the create-subgroup
    /// form's name field on the open community's home.
    pub(crate) community_subgroup_name: String,
    /// Communities created during this session, through the
    /// "New community" form: demo mode creates them locally, live
    /// mode appends them when the backend's CommunityCreated
    /// event lands. They merge into the overview list and their
    /// homes resolve like any other community's.
    pub(crate) session_communities: Vec<core::model::Community>,
    /// Live community structure by chat id: a subgroup maps to
    /// its parent community's chat id, a parent maps to itself.
    /// Learned from group metadata (BackendEvent::ChatCommunity
    /// and LiveChat parent fields); the Communities panel
    /// derives from it, never from name heuristics.
    pub(crate) chat_parents: std::collections::HashMap<u64, u64>,
    /// Memoized chat-list filter result (P2-68): the roster
    /// fingerprint it was computed for, and the visible chats'
    /// indices into `chats`. Recomputed only when the
    /// fingerprint moves, so a steady frame does no filtering
    /// (and none of its per-chat lowercase allocations).
    pub(crate) filter_memo: std::cell::RefCell<Option<(u64, Vec<usize>)>>,
    /// The announcement chats of the session-created communities,
    /// kept outside `chats` under the same rule as the demo
    /// announcement chat: the chat list never renders them as
    /// regular rows, and opening one loads it into the detached
    /// slot.
    pub(crate) session_announcements: Vec<Chat>,
    pub(crate) header_menu_open: bool,
    /// OS notification requests queued by the upsert path for new
    /// incoming messages; drained per frame by the notify module.
    pub(crate) pending_notifications: Vec<crate::notify::NotifyRequest>,
    /// Backend events received but not yet applied. `poll_backend`
    /// applies them in per-frame budgeted batches, so a history
    /// sync burst cannot freeze the UI; the rest wait here, in
    /// order, for the following frames (see live.rs).
    pub(crate) event_queue: std::collections::VecDeque<backend::BackendEvent>,
    /// Side effects deferred while one event batch is being
    /// applied: per-chat sorts, the chat-list sort, the
    /// conversation rebuild and the archive writes are coalesced
    /// to once per batch instead of once per event (see live.rs).
    pub(crate) event_batch: crate::live::EventBatch,
    /// In-flight chat export: a worker renders and writes the
    /// file off the UI thread; the result is the written path or
    /// the error text (see conversation/mod.rs).
    pub(crate) export_rx: Option<std::sync::mpsc::Receiver<Result<std::path::PathBuf, String>>>,
    /// Data & Storage page sizes (archive bytes, account bytes,
    /// total bytes), measured once when the page opens and
    /// dropped when another settings page is shown: walking the
    /// whole data dir is far too expensive to repeat per repaint
    /// (audit finding 13).
    pub(crate) storage_sizes: Option<(u64, u64, u64)>,
    /// In-flight staged album send: a worker thread reads the
    /// staged files off the UI thread and `poll_staged_send`
    /// applies the results in staging order (see attach.rs).
    pub(crate) staged_send: Option<StagedSendState>,
    /// Message whose context menu is open (secondary click), if any.
    pub(crate) bubble_menu: Option<u64>,
    /// Message id waiting for a forward target (picker popup).
    pub(crate) forward_from: Option<u64>,
    /// The chat a composing chat-state was sent to and not yet
    /// paused, if any (typing-indicator de-duplication). Per-chat:
    /// a single global flag orphaned the composing state on the old
    /// chat and suppressed it on the new one (logic review F16).
    pub(crate) typing_sent_for: Option<u64>,
    /// Debug mode state (see debug_ui.rs): the trace ring, its clock and the inspector toggles.
    pub(crate) debug: DebugState,
    /// Last persisted notification-toggle tuple (persist on change).
    pub(crate) notify_snapshot: Option<(bool, bool, bool, bool)>,
    /// Self-update state (see update.rs): the phase, the release on offer, worker results and check bookkeeping.
    pub(crate) update: UpdateState,
}

/// A poll under construction in the composer menu.
#[derive(Debug, Clone)]
pub(crate) struct PollDraft {
    /// Poll question.
    pub(crate) question: String,
    /// Option drafts (2–12 used).
    pub(crate) options: Vec<String>,
    /// Multiple-choice toggle.
    pub(crate) multiple: bool,
}

/// The event builder's draft: the create form's fields, and the
/// edit form's (the same shape; an edit pre-fills it from the
/// event). Date and times are strict text (`YYYY-MM-DD`,
/// `HH:MM`, on the app's UTC display grid) because the app has
/// no date-picker widget; they are parsed and validated whole
/// at send, with the error kept in the form.
#[derive(Debug, Clone, Default)]
pub(crate) struct EventDraft {
    /// Event name (required).
    pub(crate) name: String,
    /// Description (optional).
    pub(crate) description: String,
    /// Start date, `YYYY-MM-DD`.
    pub(crate) date: String,
    /// Start time, `HH:MM`.
    pub(crate) start_time: String,
    /// End time, `HH:MM` (optional; same day as the start).
    pub(crate) end_time: String,
    /// Location label (optional).
    pub(crate) location: String,
    /// Join link (optional).
    pub(crate) link: String,
    /// The event is a scheduled call.
    pub(crate) scheduled_call: bool,
    /// Guests may bring extra guests.
    pub(crate) extra_guests: bool,
    /// The last validation error, shown inside the form.
    pub(crate) error: Option<String>,
}

/// Which attach-menu flow staged a file: the kind decides the
/// backend media kind, the mime sniffing and the preview shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StagedKind {
    Photo,
    Video,
    Document,
}

/// A file staged in the composer, waiting to be sent: picked from
/// disk through the attach menu. Bytes are read at send time so a
/// staged file never goes stale in memory.
pub(crate) struct StagedMedia {
    pub(crate) kind: StagedKind,
    /// File name as picked (display and document file name).
    pub(crate) name: String,
    /// Absolute path on disk.
    pub(crate) path: std::path::PathBuf,
    /// MIME type derived from the extension.
    pub(crate) mime: String,
    /// View-once toggle (single photo or video only).
    pub(crate) view_once: bool,
    /// Decoded preview texture (photos only, lazy on first paint).
    pub(crate) thumb: Option<egui::TextureHandle>,
}

/// One staged album in flight (see attach.rs): the worker
/// thread streams one read result per staged file through
/// `rx`, and `pending` holds the matching send descriptors in
/// the same order, so each result pairs with the file it
/// belongs to. `sent` and `failed` accumulate the outcome for
/// the summary notice shown when the worker finishes; a file
/// whose result never arrives is counted as failed, never
/// silently dropped.
pub(crate) struct StagedSendState {
    pub(crate) rx: std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
    pub(crate) pending: std::collections::VecDeque<crate::attach::PendingStagedSend>,
    pub(crate) sent: usize,
    pub(crate) failed: Vec<(String, String)>,
}

/// The attach-menu location form: a place name plus coordinates,
/// prefilled with a sample point the user can edit.
#[derive(Debug, Clone)]
pub(crate) struct LocationDraft {
    pub(crate) name: String,
    pub(crate) latitude: String,
    pub(crate) longitude: String,
}

/// Current Unix time in seconds.
pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `YYYY-MM-DD` for today (UTC), computed without a date library.
pub(crate) fn today_string() -> String {
    let days = (now_unix() / 86_400) as i64;
    // Howard Hinnant's civil-from-days algorithm.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Every native feature id, in Settings display order. The
/// features themselves live in `crates/features/*`; this list is
/// only the switchboard the Settings page and `feature_active`
/// share.
pub(crate) const FEATURE_IDS: [&str; 3] = ["export-chat", "stickers", "giphy"];

/// First-run feature switches: everything off until the user
/// changes a switch in Settings > Features.
pub(crate) fn default_feature_flags() -> std::collections::BTreeMap<String, bool> {
    FEATURE_IDS.iter().map(|id| ((*id).to_owned(), false)).collect()
}

impl App {
    pub(crate) fn new(args: Args, egui_ctx: &egui::Context) -> Self {
        // In live mode the demo dataset is discarded the moment
        // start_live runs, so building it was pure startup waste
        // (v0.25 perf review).
        let (chats, conversation) = if args.live {
            (Vec::new(), Vec::new())
        } else {
            let mut chats = core::model::demo_chats();
            // Every demo chat carries its own consistent
            // conversation (see core::model::demo_messages_for and
            // its invariants).
            for chat in &mut chats {
                chat.messages = core::model::demo_messages_for(chat.id);
                chat.sync_preview_from_last();
            }
            // Demo presentation seeds: a few archived chats (so
            // the Archived screen and its rail badge have real
            // content) and a few starred messages (so the Starred
            // screen is not empty). Chat 4 keeps unread 2 and
            // chat 12 unread 1, so the archived unread badge
            // counts 2; chat 11 is archived with no unread.
            // Channels stay unarchived so that screen stays
            // populated.
            for chat in &mut chats {
                if matches!(chat.id, 4 | 11 | 12) {
                    chat.archived = true;
                }
                for msg in &mut chat.messages {
                    if matches!(msg.id, 102 | 2003 | 3003) {
                        msg.starred = true;
                    }
                }
            }
            // The open conversation is chat 1's messages after
            // seeding, so its starred copy matches the store.
            let conversation = chats
                .iter()
                .find(|c| c.id == 1)
                .map(|c| c.messages.clone())
                .unwrap_or_else(core::model::demo_conversation);
            (chats, conversation)
        };

        // Credential store: OS keyring when it answers, otherwise a
        // 0600 secrets file under the data dir. `.env.local` in the
        // working directory provides development overrides.
        let credentials = {
            let backend_box: Box<dyn core::credentials::CredentialBackend + Send> =
                if core::credentials::KeyringBackend::available() {
                    Box::new(core::credentials::KeyringBackend)
                } else {
                    Box::new(core::credentials::FileBackend::new(
                        backend::data_dir().join("secrets.json"),
                    ))
                };
            let mut store =
                core::credentials::CredentialStore::open(backend_box).unwrap_or_else(|_| {
                    core::credentials::CredentialStore::open(Box::new(
                        core::credentials::MemoryBackend::default(),
                    ))
                    .expect("memory credential backend cannot fail")
                });
            // A `.env.local` in the working directory can override
            // stored keys, which is a development convenience only:
            // release builds ignore it, so a file planted in the
            // launch directory cannot silently replace the user's
            // keys. Process environment variables still apply.
            if cfg!(debug_assertions) {
                store.set_overrides(core::credentials::load_dotenv_local(
                    &std::path::PathBuf::from(".env.local"),
                ));
            }
            store
        };

        // The initially open chat (id 1) gets its unread divider
        // snapshot right away, like any chat opened later.
        let initial_divider = unread_divider_for(
            chats.iter().find(|c| c.id == 1).map_or(0, |c| c.unread),
            &conversation,
        );
        let mut app = Self {
            screen: args.screen,
            theme_choice: args.theme,
            theme_fade_from: None,
            theme_fade_target: None,
            theme_fade_start: None,
            tokens: Tokens::dynamic(args.seed, args.theme.resolve(true) == ThemeId::Dark)
                .adjust(args.saturation, args.contrast)
                .with_glow(false),
            fallback_chat: core::model::Chat {
                id: 0,
                kind: core::model::ChatKind::Dm,
                title: "No chats yet".to_owned(),
                subtitle: String::new(),
                preview: String::new(),
                preview_sender: None,
                preview_mine: false,
                preview_delivery: None,
                preview_media: None,
                time: String::new(),
                unread: 0,
                mentions: 0,
                pinned: false,
                muted: false,
                mute_until: 0,
                favorite: false,
                avatar_seed: 0,
                verified: false,
                meta_ai: false,
                community: None,
                archived: false,
                ephemeral_secs: 0,
                messages: Vec::new(),
            },
            chats,
            conversation,
            selected_chat: 1,
            pending_select_chat: None,
            filter: args.screen.filter(),
            search: String::new(),
            starred_search: String::new(),
            calls: CallsState {
                entries: if args.live {
                    Vec::new()
                } else {
                    crate::calls::demo_call_log(now_unix() as i64)
                },
                live_entries: Vec::new(),
                recorded_ids: std::collections::HashSet::new(),
                search: String::new(),
                missed_only: args.calls_missed,
                favorites_only: false,
                search_open: false,
                picker_open: args.calls_picker,
                outgoing_video: false,
                picker_search: String::new(),
                active: None,
            },
            updates: UpdatesState {
                feed: Vec::new(),
                mine: Vec::new(),
                viewed: std::collections::HashSet::new(),
                viewer: None,
                composer_open: false,
                draft: String::new(),
                staged: None,
                pending_media: None,
                error: None,
                posting: false,
                last_audience: Vec::new(),
                local_seq: 0,
                reaction_actors: std::collections::HashMap::new(),
                reaction_pending: std::collections::HashMap::new(),
                clock_anchor: None,
            },
            composer: ComposerState {
                draft: if args.demo_draft {
                    "Sure, I will send the file tomorrow".to_owned()
                } else {
                    String::new()
                },
                chat_drafts: std::collections::HashMap::new(),
                chat_parked: std::collections::HashMap::new(),
                staged_chat: None,
                stack_h: 84.0,
                rich_menu_open: false,
                poll_draft: None,
                event_draft: None,
                staged: Vec::new(),
                staged_caption: String::new(),
                location_draft: None,
                reply_to: None,
                editing_id: None,
            },
            settings_search: args.settings_search.clone().unwrap_or_default(),
            settings_highlight: None,
            appearance_advanced_open: false,
            qa_scroll_top: args.scroll_top,
            qa_scroll_to: args.scroll_to,
            qa_community_scroll: args.community_scroll,
            zoom_percent: 100,
            enter_sends: true,
            keep_archived: true,
            sidebar_collapsed: args.sidebar_collapsed,
            single_pane: false,
            portrait: false,
            conversation_open: false,
            settings_page_open: false,
            settings_tab: match std::env::args()
                .collect::<Vec<_>>()
                .windows(2)
                .find(|w| w[0] == "--tab")
                .map(|w| w[1].as_str())
            {
                Some("chats") => "Chats",
                Some("notifications") => "Notifications",
                Some("privacy") => "Privacy & Security",
                Some("features") => "Features",
                Some("storage") => "Data & Storage",
                Some("about") => "About",
                _ => "Appearance",
            },
            pair_mode: if args.pair_code || args.pair_form { PairMode::Code } else { PairMode::Qr },
            pair_phone: crate::countries::compose_number(
                crate::countries::default_country().dial,
                "",
            ),
            pair_country_iso: crate::countries::default_country().iso.to_owned(),
            pair_national: String::new(),
            country_picker_open: args.country_picker,
            country_search: String::new(),
            country_anchor: None,
            connect_card_rect: None,
            pair_code_shown: args.pair_code,
            account_menu_open: args.account_menu,
            account_menu_open_frame: None,
            profile_anchor: None,
            notify_messages: true,
            notify_groups: true,
            notify_sound: true,
            notify_preview: false,
            read_receipts: true,
            default_ephemeral_secs: 0,
            cache_cleared: false,
            media: MediaState {
                photo_texture: None,
                textures: crate::extras::TextureCache::new(crate::extras::MAX_MEDIA_TEXTURES),
                avatar_textures: crate::extras::TextureCache::new(
                    crate::extras::MAX_AVATAR_TEXTURES,
                ),
                live_avatar_textures: crate::extras::TextureCache::new(
                    crate::extras::MAX_AVATAR_TEXTURES,
                ),
                sticker_fetch: std::collections::HashMap::new(),
                photo_thumb_textures: crate::extras::TextureCache::new(
                    crate::extras::MAX_MEDIA_TEXTURES,
                ),
                opened_view_once: std::collections::HashSet::new(),
                viewing_photo: if args.open_viewonce { Some(112) } else { None },
                photo_fetch: std::collections::HashMap::new(),
                downloads: std::collections::HashSet::new(),
                view_once_shown: None,
                voice_player: None,
                voice_fetch: std::collections::HashMap::new(),
                voice_wanted: None,
                voice_pcm: None,
                voice_pending_seek: std::collections::HashMap::new(),
                voice_sink: crate::player::VoiceSink::Device,
                voice_rates: std::collections::HashMap::new(),
                video_textures: crate::extras::TextureCache::new(crate::extras::MAX_MEDIA_TEXTURES),
                embedded_textures: crate::extras::TextureCache::new(
                    crate::extras::MAX_MEDIA_TEXTURES,
                ),
                video_fetch: std::collections::HashMap::new(),
                poster_fetch: std::collections::HashMap::new(),
                video_files: std::collections::HashMap::new(),
                video_bytes: std::collections::HashMap::new(),
                video_player: None,
                video_presented: None,
                video_frame: None,
                video_frame_key: None,
            },
            unread_divider: initial_divider,
            scroll_bottom_requested: false,
            feature_flags: default_feature_flags(),
            giphy_prefs: serde_json::Map::new(),
            sent_counter: 0,
            last_expiry_scan: 0,
            expiry_anchor: None,
            live_mode: false,
            live: None,
            live_start_error: None,
            logged_out: false,
            notice: None,
            crash_report: core::crash::unseen_report(&backend::data_dir().join("crashes")),
            crash_details_open: false,
            crash_copied: false,
            egui_ctx: Some(egui_ctx.clone()),
            saturation: args.saturation,
            contrast: args.contrast,
            seed: args.seed,
            credentials,
            credential_drafts: std::collections::HashMap::new(),
            typing_indicators: true,
            link_previews: true,
            privacy_state: std::collections::BTreeMap::new(),
            protocol_ids: std::collections::HashMap::new(),
            sender_jids: std::collections::HashMap::new(),
            unread_live: std::collections::HashMap::new(),
            receipt_log: std::collections::HashMap::new(),
            receipt_log_order: std::collections::VecDeque::new(),
            live_typing: std::collections::HashMap::new(),
            live_presence: std::collections::HashMap::new(),
            presence_watch: None,
            typing_names: std::collections::HashMap::new(),
            groups: GroupsState {
                live_members: std::collections::HashMap::new(),
                own_admin: std::collections::HashMap::new(),
                members_requested: std::collections::HashSet::new(),
                invite_link: None,
                approval: std::collections::HashMap::new(),
                modes: std::collections::HashMap::new(),
                join_requests: None,
                join_link_open: false,
                join_link_input: String::new(),
                join_link_error: None,
                create_open: false,
                create_error: None,
                create_pending: false,
                participant_add_awaiting: None,
                create_name: String::new(),
                create_members: String::new(),
            },
            new_chat: NewChatState {
                open: false,
                phone: String::new(),
                checking: None,
                not_found: None,
                error: None,
            },
            blocked_chats: std::collections::HashSet::new(),
            profile_form_for: None,
            ui_confirm: None,
            clear_keep_starred: true,
            contact_picker_open: false,
            add_participant_input: String::new(),
            group_subject_edit: String::new(),
            group_desc_edit: String::new(),
            motion: crate::motion::MotionSetting::Full,
            language: "en".to_owned(),
            anim: AnimState {
                picker: None,
                chat_fade: None,
                send_pulse: None,
                picker_was_open: false,
                picker_closing: false,
                chat_fade_pending: false,
                started: 0,
                viewer_reveal: None,
                screen_fade: None,
                screen_key: None,
            },
            reaction_actors: std::collections::HashMap::new(),
            my_reactions: std::collections::HashMap::new(),
            pending_reactions: std::collections::HashMap::new(),
            pending_channel_reactions: std::collections::HashMap::new(),
            pending_stars: std::collections::HashMap::new(),
            pending_pins: std::collections::HashMap::new(),
            pending_keeps: std::collections::HashMap::new(),
            pending_poll_votes: std::collections::HashMap::new(),
            pending_event_rsvps: std::collections::HashMap::new(),
            pending_event_edits: std::collections::HashMap::new(),
            event_edit_draft: None,
            event_guests: std::collections::HashMap::new(),
            pending_chat_actions: std::collections::HashMap::new(),
            pending_unread: std::collections::HashMap::new(),
            pending_blocks: std::collections::HashMap::new(),
            pending_group_info: std::collections::HashMap::new(),
            pending_leave: std::collections::HashMap::new(),
            pending_timers: std::collections::HashMap::new(),
            pending_edits: std::collections::HashMap::new(),
            pending_revokes: std::collections::HashMap::new(),
            pending_delete_for_me: std::collections::HashMap::new(),
            pending_clears: std::collections::HashMap::new(),
            pending_chat_deletes: std::collections::HashSet::new(),
            picker: PickerState {
                open: args.picker.is_some(),
                tab: args.picker.clone().unwrap_or_else(|| "emoji".to_owned()),
                emoji_search: String::new(),
                emoji_recents: crate::rich_ui::emoji::load_recents(),
                sticker_packs: Vec::new(),
                sticker_new_pack: String::new(),
                sticker_new_text: String::new(),
                giphy_query: String::new(),
                giphy_items: Vec::new(),
                giphy_responded: false,
                giphy_rejected: false,
                giphy_textures: crate::extras::TextureCache::new(crate::extras::MAX_GIPHY_TEXTURES),
                sticker_textures: crate::extras::TextureCache::new(
                    crate::extras::MAX_STICKER_TEXTURES,
                ),
                sticker_failures: crate::extras::StickerFailureCache::default(),
                sticker_pending: std::collections::HashSet::new(),
                sticker_tx: None,
                sticker_rx: None,
                giphy_rx: None,
                media_fetch_rx: None,
            },
            recorder: None,
            add_member_open: false,
            message_info_id: None,
            reaction_picker_id: None,
            profile_open: args.profile,
            my_profile: crate::profile::MyProfileState::default(),
            my_profile_open: false,
            joined_community_groups: std::collections::HashSet::new(),
            synced_sticker_favorites: std::collections::HashSet::new(),
            synced_recent_sticker_removals: Vec::new(),
            community_desc_expanded: None,
            open_community: None,
            community_home_from_overview: false,
            scroll_to_message: None,
            stream_heights: std::collections::HashMap::new(),
            stream_anchor: None,
            stream_anchor_offset: 0.0,
            stream_anchor_chat: 0,
            stream_last_offset: 0.0,
            stream_last_viewport: 0.0,
            stream_was_bottom: true,
            highlight_message: None,
            highlight_started: None,
            global_search: GlobalSearch::default(),
            chat_search: ChatSearch::default(),
            gallery_section: None,
            photo_editor: None,
            zip_gather: None,
            zip_export_rx: None,
            detached_chat: None,
            community_origin: None,
            channel_find_open: false,
            channel_find_text: String::new(),
            channel_found: None,
            channel_panel_notice: None,
            channel_create_open: false,
            channel_create_name: String::new(),
            channel_create_desc: String::new(),
            channel_create_error: None,
            channel_info_for: None,
            channel_poll_for: None,
            channel_poll_question: String::new(),
            channel_poll_options: vec![String::new(), String::new()],
            channel_poll_multiple: false,
            community_create_open: false,
            community_create_name: String::new(),
            community_create_error: None,
            community_link_picker_open: false,
            community_subgroup_name: String::new(),
            session_communities: Vec::new(),
            chat_parents: std::collections::HashMap::new(),
            filter_memo: std::cell::RefCell::new(None),
            session_announcements: Vec::new(),
            header_menu_open: args.header_menu,
            pending_notifications: Vec::new(),
            event_queue: std::collections::VecDeque::new(),
            event_batch: crate::live::EventBatch::default(),
            export_rx: None,
            storage_sizes: None,
            staged_send: None,
            bubble_menu: None,
            forward_from: None,
            typing_sent_for: None,
            debug: DebugState {
                mode: args.debug,
                trace: core::debug::TraceRing::default(),
                flow_start: std::time::Instant::now(),
                export_open: false,
                include_bodies: false,
                reveal_bodies: false,
            },
            notify_snapshot: None,
            update: UpdateState {
                phase: crate::update::UpdatePhase::Idle,
                release: None,
                rx: None,
                last_check: 0,
                auto_check: true,
                manual: false,
                startup_done: false,
                target: None,
            },
        };
        app.load_persisted_state();
        app.tokens = Tokens::dynamic(app.seed, app.theme_choice.resolve(true) == ThemeId::Dark)
            .adjust(app.saturation, app.contrast)
            .with_glow(false);
        crate::update::cleanup_stale_backup();
        // Declare the native features' credential slots so the
        // Settings credentials area can manage their keys.
        app.credentials.declare("GIF search", &[giphy::credential_slot()]);
        if !args.live {
            // Demo data exercises the chat list's two transient
            // row states: Alex is typing in the Sunrise Yoga Club
            // group, and a draft is parked in the Maple Street
            // Neighbors group.
            app.typing_names.insert(3, "Alex".to_owned());
            app.composer
                .chat_drafts
                .insert(6, "Can we move the block cleanup to Saturday?".to_owned());
        }
        if app.debug.mode {
            core::debug::set_debug(true);
            app.trace_push("ui", "debug mode on (banner + inspector + tracing)", None);
        }
        if args.live {
            app.start_live(egui_ctx, args.fake_backend, args.fake_linked);
            if !args.screen_explicit {
                // A plain launch lands on the link screen until
                // the backend reports a linked session; the
                // Connected event then moves to Chats on its own
                // (see live.rs). An explicit --screen always wins.
                app.screen = Screen::Connect;
            }
        }
        if let Some(id) = args.select_chat {
            // The demo announcement chat can be selected directly
            // (screenshots, debugging): it takes the detached slot,
            // never a chat-list place.
            if !app.live_mode && id == core::model::demo_announcement_chat().id {
                let mut announcement = core::model::demo_announcement_chat();
                announcement.messages = core::model::demo_announcement_messages();
                app.detached_chat = Some(announcement);
            }
            if app.chats.iter().any(|c| c.id == id)
                || app.detached_chat.as_ref().is_some_and(|c| c.id == id)
            {
                app.select_chat_now(id);
            } else if app.live_mode {
                // Live chats arrive via backend events; defer.
                app.pending_select_chat = Some(id);
            }
        }
        if let Some(id) = args.community {
            // QA hook: open the community home when it resolves,
            // so screenshots can reach it without a click.
            if app.communities().iter().any(|c| c.id == id) {
                app.open_community = Some(id);
            }
        }
        if let Some(query) = &args.search {
            // QA hook: seed the chat list's search field.
            app.search = query.clone();
        }
        if let Some(query) = &args.chat_search {
            // QA hook: open the in-chat search strip with a
            // query already typed. The session is bound to the
            // selected chat here, or the strip's own per-chat
            // reset would clear the seeded query on first draw.
            app.chat_search.open = true;
            app.chat_search.chat_id = app.selected_chat;
            app.chat_search.query = query.clone();
        }
        if let Some(section) = &args.gallery {
            // QA hook: open the profile dock on a gallery
            // section.
            app.gallery_section = Some(match section.as_str() {
                "links" => GallerySection::Links,
                "docs" => GallerySection::Docs,
                _ => GallerySection::Media,
            });
            app.profile_open = true;
        }
        if let Some(path) = &args.stage {
            // QA hook: stage a file in the selected chat's
            // composer.
            app.stage_files(egui_ctx, vec![std::path::PathBuf::from(path)]);
        }
        if args.editor {
            // QA hook: open the photo editor on the first
            // staged photo.
            app.open_photo_editor(0);
        }
        app
    }

    /// Joins a community group offered in a community home's
    /// Other groups section. A no-op when the group was already
    /// joined or is already in the chat list. Otherwise the group
    /// is recorded as joined and, when it is not listed yet, its
    /// demo chat value is pushed with its demo messages so it
    /// behaves like any other listed group. Live mode has no
    /// other groups, so nothing resolves there.
    pub(crate) fn join_community_group(&mut self, chat_id: u64) {
        if self.live_mode {
            return;
        }
        // A known other-group id is recorded as joined BEFORE the
        // already-listed early return below, so home resolution
        // can never show a listed group under Other groups.
        let was_joined = self.joined_community_groups.contains(&chat_id);
        let is_other_group =
            core::model::demo_communities().iter().any(|c| c.other_group_ids.contains(&chat_id));
        if is_other_group {
            self.joined_community_groups.insert(chat_id);
        }
        if was_joined {
            return;
        }
        if self.chats.iter().any(|c| c.id == chat_id) {
            return;
        }
        let mut found: Option<Chat> = None;
        for community in core::model::demo_communities() {
            if let Some(c) = core::model::demo_community_other_groups(community.id)
                .into_iter()
                .find(|c| c.id == chat_id)
            {
                found = Some(c);
                break;
            }
        }
        let Some(mut chat) = found else {
            return;
        };
        chat.messages = core::model::demo_messages_for(chat_id);
        chat.sync_preview_from_last();
        self.chats.push(chat);
    }

    /// Demo mode: writes the open conversation (the working copy)
    /// back into the store copy of the chat being left: the listed
    /// chat, or the detached announcement chat when the listed one
    /// does not hold it. Every chat-switch path in demo mode goes
    /// through this one helper so the rule cannot drift. Live mode
    /// keeps its conversation in the session and archive, so this
    /// is a no-op there.
    pub(crate) fn write_back_conversation(&mut self, leaving: u64) {
        if self.live_mode {
            return;
        }
        if let Some(old) = self.chats.iter_mut().find(|c| c.id == leaving) {
            old.messages = self.conversation.clone();
        } else if let Some(old) = self.detached_chat.as_mut().filter(|c| c.id == leaving) {
            old.messages = self.conversation.clone();
        }
    }

    /// Whether a message id belongs to the OPEN conversation.
    /// Message ids are unique per chat in both modes (demo ids
    /// are namespaced per chat, live ids are derived from the
    /// chat JID and the protocol id), so membership is an
    /// identity check: an id minted in another chat is never
    /// found here. Send paths that resolve a composer target
    /// against global maps (the edit command, the wire quote)
    /// gate on this, so a target can never be paired with the
    /// wrong chat even if the per-chat parking is bypassed.
    pub(crate) fn in_open_conversation(&self, msg_id: u64) -> bool {
        self.conversation.iter().any(|m| m.id == msg_id)
    }

    /// Moves the composer state between chats: everything the
    /// composer holds for the chat being left (the draft text,
    /// staged files and their caption, the quote and edit
    /// targets, the poll and location drafts) is parked under
    /// that chat, and whatever was parked for the chat being
    /// entered is restored. Every chat switch runs this (the
    /// chat list directly, every other path through
    /// `select_chat_now`), so composer state follows its chat
    /// on every route and can never leak into the conversation
    /// that happens to be open next. A voice recording is the
    /// one composer-adjacent state that does not park: it
    /// belongs to the chat it was started in, so a switch
    /// cancels it instead (see below).
    pub(crate) fn switch_composer_chat(&mut self, previous: u64, next: u64) {
        if previous == next {
            return;
        }
        // A recording in progress is cancelled by the switch,
        // through the same path as the composer's cancel
        // button: the recorder is dropped, its capture thread
        // stops and the partial take is discarded. Parking the
        // take instead would let it be finished into the chat
        // that happens to be open next, a wrong-recipient
        // hazard; cancelling matches WhatsApp's own behavior.
        self.cancel_recording();
        // The draft text keeps its existing parking rule (an
        // empty draft leaves no entry; the chat list's Draft row
        // state reads the same map).
        let leaving = std::mem::take(&mut self.composer.draft);
        if leaving.trim().is_empty() {
            self.composer.chat_drafts.remove(&previous);
        } else {
            self.composer.chat_drafts.insert(previous, leaving);
        }
        self.composer.draft = self.composer.chat_drafts.remove(&next).unwrap_or_default();
        // The attachments park as one bundle; a chat with
        // nothing attached leaves no entry behind.
        let parked = ParkedComposer {
            staged: std::mem::take(&mut self.composer.staged),
            staged_caption: std::mem::take(&mut self.composer.staged_caption),
            reply_to: self.composer.reply_to.take(),
            editing_id: self.composer.editing_id.take(),
            poll_draft: self.composer.poll_draft.take(),
            location_draft: self.composer.location_draft.take(),
            event_draft: self.composer.event_draft.take(),
        };
        self.composer.staged_chat = None;
        if parked.is_empty() {
            self.composer.chat_parked.remove(&previous);
        } else {
            self.composer.chat_parked.insert(previous, parked);
        }
        if let Some(parked) = self.composer.chat_parked.remove(&next) {
            self.composer.staged = parked.staged;
            self.composer.staged_caption = parked.staged_caption;
            self.composer.reply_to = parked.reply_to;
            self.composer.editing_id = parked.editing_id;
            self.composer.poll_draft = parked.poll_draft;
            self.composer.location_draft = parked.location_draft;
            self.composer.event_draft = parked.event_draft;
            if !self.composer.staged.is_empty() {
                self.composer.staged_chat = Some(next);
            }
        }
    }

    /// Selects a chat and loads its conversation (live: from the
    /// archive; demo: the chat's messages). Selection alone never
    /// marks anything read: in live mode the unread badge and the
    /// protocol receipts both wait for a real read action in the
    /// conversation (scroll to the newest messages, use the
    /// composer, send a reply), which flushes them together via
    /// `note_read_engagement`. The `mark_selected_chat_read` call
    /// below is gate-safe: without engagement it sends nothing.
    pub(crate) fn select_chat_now(&mut self, id: u64) {
        // The switch fade is armed here; the conversation draw
        // starts it on the next frame (it owns the egui clock).
        self.anim.chat_fade_pending = true;
        // Selecting a chat opens it: in the single-pane layout
        // this is what brings the conversation on screen over
        // the list (the two-pane layout ignores the flag).
        self.conversation_open = true;
        // The composer state belongs to the chat being left:
        // park it and restore the target chat's before anything
        // else observes the new selection.
        let previous = self.selected_chat;
        self.switch_composer_chat(previous, id);
        self.selected_chat = id;
        // The unread count at open time, from the chat store or
        // the detached slot: captured before the demo branch
        // below clears the detached badge, so the divider
        // snapshot still sees it.
        let unread = self
            .chats
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.unread)
            .or_else(|| self.detached_chat.as_ref().filter(|c| c.id == id).map(|c| c.unread))
            .unwrap_or(0);
        if self.live_mode {
            self.sync_conversation();
            self.mark_selected_chat_read();
            // Opening a chat arms its older-history trigger
            // (design standard section 22): a conversation that
            // opens at its top may ask the phone for the page
            // before it.
            if let Some(session) = &mut self.live {
                session.history_armed.insert(id);
            }
            // A channel opens on its backend record: live
            // updates armed and the newest history page
            // fetched (the roster alone only carries the
            // latest post).
            let is_channel = self
                .chats
                .iter()
                .find(|c| c.id == id)
                .is_some_and(|c| c.kind == core::model::ChatKind::Channel);
            if is_channel {
                self.open_channel(id);
            }
        } else {
            // The detached slot (the community announcement chat)
            // loads exactly like a listed chat; it is only the
            // chat list that never sees it.
            self.conversation = self
                .chats
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.messages.clone())
                .or_else(|| {
                    self.detached_chat.as_ref().filter(|c| c.id == id).map(|c| c.messages.clone())
                })
                .unwrap_or_default();
            // Opening the detached chat clears its badge, like
            // opening a listed chat does on the chat-list path;
            // without this the announcement badge could never
            // clear, since the list never renders the slot.
            if let Some(detached) = self.detached_chat.as_mut().filter(|c| c.id == id) {
                detached.unread = 0;
            }
        }
        // Snapshot the unread divider for the newly opened chat:
        // the count and position are frozen now, so the
        // read-by-action flush clearing the badge as the user
        // engages does not move or remove the divider mid-view.
        self.unread_divider = unread_divider_for(unread, &self.conversation);
    }

    /// Runs feature code with panic containment: if the feature
    /// panics, it is switched off and a notice is shown, instead of
    /// the panic taking the whole app down.
    ///
    /// `AssertUnwindSafe` is sound here because the closure only reads
    /// app state to compute a value; after a caught panic the feature
    /// is off and no value crosses the boundary, so no inconsistent
    /// state is observed afterwards.
    pub(crate) fn guard_feature<T>(
        &mut self,
        feature_id: &str,
        f: impl FnOnce(&Self) -> T,
    ) -> Option<T> {
        let this: &Self = self;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(this)));
        if let Ok(value) = result {
            Some(value)
        } else {
            self.feature_flags.insert(feature_id.to_owned(), false);
            self.notice = Some(Notice::error(format!(
                "The {feature_id} feature crashed and was switched off. \
                 The rest of Zaprust is unaffected."
            )));
            None
        }
    }

    pub(crate) fn tokens(&self) -> Tokens {
        self.tokens
    }

    pub(crate) fn apply_theme_choice(&mut self, ctx: &egui::Context) {
        let dark = self.theme_choice.resolve(true) == ThemeId::Dark;
        let target = Tokens::dynamic(self.seed, dark)
            .adjust(self.saturation, self.contrast)
            .with_glow(false);
        // The change crossfades over the canonical theme span
        // (design standard): the shell blends tokens from the
        // current set to the target across frames. Motion Off
        // snaps, as it does for every animation.
        if self.motion == crate::motion::MotionSetting::Off {
            self.tokens = target;
            self.theme_fade_from = None;
            self.theme_fade_target = None;
            self.theme_fade_start = None;
        } else {
            self.theme_fade_from = Some(self.tokens);
            self.theme_fade_target = Some(target);
            self.theme_fade_start = Some(std::time::Instant::now());
            ctx.request_repaint();
        }
        apply_visuals(ctx, self.tokens);
    }

    /// Advances the theme crossfade by one frame, called from
    /// the shell before anything paints. Returns nothing; at
    /// rest it is a no-op.
    pub(crate) fn step_theme_fade(&mut self, ctx: &egui::Context) {
        let (Some(from), Some(target), Some(start)) =
            (self.theme_fade_from, self.theme_fade_target, self.theme_fade_start)
        else {
            return;
        };
        let span = crate::motion::dur::THEME / 1000.0;
        let p = (start.elapsed().as_secs_f64() / span).clamp(0.0, 1.0) as f32;
        if p >= 1.0 {
            self.tokens = target;
            self.theme_fade_from = None;
            self.theme_fade_target = None;
            self.theme_fade_start = None;
        } else {
            self.tokens = from.blend(&target, crate::motion::ease_out_cubic(f64::from(p)) as f32);
            ctx.request_repaint();
        }
    }

    pub(crate) fn selected(&self) -> &Chat {
        self.chats
            .iter()
            .find(|c| c.id == self.selected_chat)
            .or_else(|| self.detached_chat.as_ref().filter(|c| c.id == self.selected_chat))
            .or_else(|| self.chats.first())
            .unwrap_or(&self.fallback_chat)
    }

    /// Whether the selected chat is a member of the list the
    /// current screen shows. Always true outside the filtered
    /// list destinations; on Channels, Groups and Archived the
    /// selected chat must pass that destination's filter. The
    /// shell uses this to keep the two panes in agreement: a
    /// selection outside the filter shows the empty conversation
    /// placeholder instead of a foreign chat. The selection
    /// itself is never changed by this check, so returning to
    /// the plain Chats list reopens the same chat.
    pub(crate) fn selection_in_current_filter(&self) -> bool {
        if !self.screen.is_filtered_list() {
            return true;
        }
        let filter = self.screen.filter();
        self.chats.iter().find(|c| c.id == self.selected_chat).is_some_and(|c| filter.matches(c))
            || self
                .detached_chat
                .as_ref()
                .is_some_and(|c| c.id == self.selected_chat && filter.matches(c))
    }
}

/// Where the conversation's unread divider sits: a snapshot
/// taken when a chat is opened. `count` is the unread count at
/// open time and `first_unread_id` the earliest message of the
/// trailing unread run. Snapshotting keeps the divider fixed in
/// place while the read-by-action flush clears the live badge
/// underneath it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UnreadDivider {
    /// Unread count at the moment the chat was opened.
    pub(crate) count: u32,
    /// The message the divider is painted above.
    pub(crate) first_unread_id: u64,
}

/// Computes the unread divider for a conversation being opened:
/// walking from the newest message backwards over incoming
/// messages, the divider belongs above the earliest of the last
/// `unread` of them. `None` when nothing is unread, or when the
/// loaded history holds no incoming message to anchor to.
pub(crate) fn unread_divider_for(unread: u32, messages: &[Message]) -> Option<UnreadDivider> {
    if unread == 0 {
        return None;
    }
    let mut remaining = unread;
    let mut first: Option<u64> = None;
    for m in messages.iter().rev() {
        if remaining == 0 {
            break;
        }
        if !m.is_mine() && m.kind != core::model::MessageKind::System {
            first = Some(m.id);
            remaining -= 1;
        }
    }
    first.map(|first_unread_id| UnreadDivider { count: unread, first_unread_id })
}

impl App {
    /// The stored playback rate for a voice message (1.0 when
    /// the user never touched its speed pill).
    pub(crate) fn voice_speed(&self, id: u64) -> f32 {
        self.media.voice_rates.get(&id).copied().unwrap_or(1.0)
    }

    /// Advances a voice message's stored rate to the next pill
    /// step (1x to 1.5x to 2x, then back to 1x) and returns the
    /// new rate.
    pub(crate) fn cycle_voice_speed(&mut self, id: u64) -> f32 {
        let next = crate::bubble::next_voice_speed(self.voice_speed(id));
        self.media.voice_rates.insert(id, next);
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_divider_sits_above_the_first_unread_incoming() {
        // History: mine, theirs, mine, theirs, theirs. With two
        // unread, the divider belongs above the earlier of the
        // last two incoming messages (id 4); my own messages in
        // between do not consume the count.
        let msgs = vec![
            Message::mine(1, "a", "10:00"),
            Message::theirs(2, "Ana", "b", "10:01"),
            Message::mine(3, "c", "10:02"),
            Message::theirs(4, "Ana", "d", "10:03"),
            Message::theirs(5, "Ana", "e", "10:04"),
        ];
        let d = unread_divider_for(2, &msgs).expect("two unread anchor a divider");
        assert_eq!(d.count, 2);
        assert_eq!(d.first_unread_id, 4);
        let d = unread_divider_for(3, &msgs).expect("three unread reach further back");
        assert_eq!(d.first_unread_id, 2);
        // Nothing unread, no divider; no incoming history, no
        // divider either.
        assert!(unread_divider_for(0, &msgs).is_none());
        assert!(unread_divider_for(1, &msgs[..1]).is_none());
    }

    #[test]
    fn voice_rates_are_stored_per_message_and_cycle() {
        let ctx = egui::Context::default();
        let mut args = Args::parse();
        args.live = false;
        let mut app = App::new(args, &ctx);
        // Unset messages run at 1x; cycling walks the pill steps
        // and wraps, per message independently.
        assert_eq!(app.voice_speed(42), 1.0);
        assert_eq!(app.cycle_voice_speed(42), 1.5);
        assert_eq!(app.cycle_voice_speed(42), 2.0);
        assert_eq!(app.cycle_voice_speed(42), 1.0);
        assert_eq!(app.voice_speed(42), 1.0);
        assert_eq!(app.voice_speed(43), 1.0);
    }

    #[test]
    fn joining_a_community_group_lists_it_once() {
        let ctx = egui::Context::default();
        let mut args = Args::parse();
        args.live = false;
        let mut app = App::new(args, &ctx);
        assert!(!app.chats.iter().any(|c| c.id == 15));
        app.join_community_group(15);
        assert!(app.joined_community_groups.contains(&15));
        let chat = app.chats.iter().find(|c| c.id == 15).expect("joined group is listed");
        assert_eq!(chat.title, "Weekend Football League");
        assert!(!chat.messages.is_empty());
        assert_eq!(chat.messages.last().map(|m| m.body.as_str()), Some(chat.preview.as_str()));
        let count = app.chats.iter().filter(|c| c.id == 15).count();
        app.join_community_group(15);
        assert_eq!(
            app.chats.iter().filter(|c| c.id == 15).count(),
            count,
            "second join is a no-op"
        );
        // Unknown ids and already listed chats are no-ops too.
        app.join_community_group(999);
        assert!(!app.chats.iter().any(|c| c.id == 999));
        app.join_community_group(1);
        assert_eq!(app.chats.iter().filter(|c| c.id == 1).count(), 1);
    }

    #[test]
    fn demo_seeds_archive_and_star() {
        let ctx = egui::Context::default();
        let mut args = Args::parse();
        args.live = false;
        let app = App::new(args, &ctx);
        for id in [4_u64, 11, 12] {
            assert!(app.chats.iter().find(|c| c.id == id).is_some_and(|c| c.archived));
        }
        assert_eq!(core::filter::archived_unread_count(&app.chats), 2);
        let starred = core::filter::starred_messages(&app.chats);
        let ids: Vec<u64> = starred.iter().map(|(_, _, m)| m.id).collect();
        assert!(ids.contains(&102) && ids.contains(&2003) && ids.contains(&3003));
        // The open conversation (chat 1) carries the star too.
        assert!(app.conversation.iter().find(|m| m.id == 102).is_some_and(|m| m.starred));
    }

    #[test]
    fn meta_ai_detection_is_the_exact_title_only() {
        // The live heuristic (documented in ARCHITECTURE.md): a
        // chat whose title is exactly "Meta AI" renders as the AI
        // contact; lookalikes must not.
        let is_meta = |title: &str| title.trim() == "Meta AI";
        assert!(is_meta("Meta AI"));
        assert!(is_meta("  Meta AI "));
        assert!(!is_meta("Meta AI Fans"));
        assert!(!is_meta("meta ai"));
    }

    /// Crash safety at app level: a panicking feature callback is
    /// contained, the feature is switched off, a notice is set, and
    /// the app keeps running.
    #[test]
    fn panicking_feature_callback_is_switched_off_not_fatal() {
        let ctx = egui::Context::default();
        let mut args = Args::parse();
        // Tests run offline against the demo dataset; a plain
        // launch defaults to live mode (see Args::parse).
        args.live = false;
        let mut app = App::new(args, &ctx);
        // export-chat is a native feature; switch it on to
        // exercise the guard.
        let id = "export-chat";
        app.feature_flags.insert(id.to_owned(), true);
        assert!(app.feature_flags.get(id).copied().unwrap_or(false));
        let result: Option<()> = app.guard_feature(id, |_| panic!("feature blew up"));
        assert!(result.is_none(), "no value crosses a caught panic");
        assert!(!app.feature_flags.get(id).copied().unwrap_or(true));
        let notice = app.notice_str().unwrap_or_default().to_owned();
        assert!(notice.contains("switched off"), "notice explains: {notice}");
        // An unrelated switch is untouched by the guard.
        assert!(!app.feature_flags.get("stickers").copied().unwrap_or(true));
    }

    /// Perfection round: notices are typed, and the Busy marker
    /// an async start leaves behind is cleared by its completion
    /// only while it is still the Busy marker: a result notice
    /// (Info or Error) standing in its place is never cleared.
    #[test]
    fn clear_busy_notice_only_clears_busy() {
        let ctx = egui::Context::default();
        let mut args = Args::parse();
        args.live = false;
        let mut app = App::new(args, &ctx);
        app.notice = Some(Notice::busy("Sending…"));
        app.clear_busy_notice();
        assert!(app.notice.is_none(), "the Busy marker goes with its work");
        app.notice = Some(Notice::error("Export failed: disk full"));
        app.clear_busy_notice();
        assert_eq!(app.notice_str(), Some("Export failed: disk full"));
        app.notice = Some(Notice::info("Saved to /tmp/x.pdf"));
        app.clear_busy_notice();
        assert_eq!(app.notice_str(), Some("Saved to /tmp/x.pdf"));
    }

    /// Drives real frames through the same draw dispatch the
    /// shell uses (chat list plus community home or conversation
    /// in the content area), headlessly, so draw-time side
    /// effects are exercised, not just field assignments.
    fn drive_frames(app: &mut App, ctx: &egui::Context, frames: usize, time: &mut f64) {
        for _ in 0..frames {
            *time += 0.05;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::Vec2::new(1180.0, 760.0),
                )),
                time: Some(*time),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ctx| {
                let mut root = egui::Ui::new(
                    ctx.clone(),
                    egui::Id::unique("detour_test_root"),
                    egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::Vec2::new(1180.0, 760.0),
                    )),
                );
                app.draw_chat_list(&mut root);
                if app.open_community.is_some() {
                    app.draw_community_home(&mut root);
                } else {
                    app.draw_conversation(&mut root);
                }
            });
            // No GPU in the harness: discard the texture deltas
            // explicitly instead of dropping them unhandled.
            out.textures_delta.clear();
        }
    }

    /// The conversation state the community detour must never
    /// disturb: which chat is open, what it contains, the live
    /// draft and the parked ones, every chat's unread count, and
    /// the detached announcement slot.
    #[derive(Debug, PartialEq)]
    struct ConversationSnapshot {
        selected: u64,
        message_ids: Vec<u64>,
        draft: String,
        parked_drafts: Vec<(u64, String)>,
        unread: Vec<(u64, u32)>,
        detached: Option<u64>,
    }

    fn conversation_snapshot(app: &App) -> ConversationSnapshot {
        let mut drafts: Vec<(u64, String)> =
            app.composer.chat_drafts.iter().map(|(id, d)| (*id, d.clone())).collect();
        drafts.sort_unstable();
        ConversationSnapshot {
            selected: app.selected_chat,
            message_ids: app.conversation.iter().map(|m| m.id).collect(),
            draft: app.composer.draft.clone(),
            parked_drafts: drafts,
            unread: app.chats.iter().map(|c| (c.id, c.unread)).collect(),
            detached: app.detached_chat.as_ref().map(|c| c.id),
        }
    }

    /// The owner's separation guarantee: opening a community
    /// home and going back restores the previously open
    /// conversation exactly. The open and back steps below copy
    /// the exact statements the chat-list row, the overview row,
    /// and the home's back intent run; the frames in between are
    /// the real draw code.
    #[test]
    fn community_home_detour_preserves_the_open_conversation() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut args = Args::parse();
        args.live = false;
        let mut app = App::new(args, &ctx);
        app.select_chat_now(1);
        app.composer.draft = "a draft in progress".to_owned();
        app.composer.chat_drafts.insert(2, "parked for another chat".to_owned());
        let mut time = 0.0;
        // Settle the conversation first: the snapshot is taken
        // after normal conversation frames, so anything the
        // detour changes afterwards is the detour's doing.
        drive_frames(&mut app, &ctx, 3, &mut time);
        let before = conversation_snapshot(&app);

        // The direct entry path (home opened without the
        // overview, as the QA hook does) and its back.
        app.community_home_from_overview = false;
        app.open_community = Some(1);
        drive_frames(&mut app, &ctx, 5, &mut time);
        assert_eq!(conversation_snapshot(&app), before, "rendering the home disturbed state");
        app.open_community = None;
        drive_frames(&mut app, &ctx, 3, &mut time);
        assert_eq!(conversation_snapshot(&app), before, "returning from the home disturbed state");

        // The overview entry path and its back statements (the
        // screen returns to the Communities panel; the content
        // area returns to the conversation).
        app.community_home_from_overview = true;
        app.open_community = Some(1);
        drive_frames(&mut app, &ctx, 5, &mut time);
        app.community_home_from_overview = false;
        app.open_community = None;
        app.screen = Screen::Communities;
        drive_frames(&mut app, &ctx, 3, &mut time);
        assert_eq!(conversation_snapshot(&app), before, "the overview detour disturbed state");
    }

    /// The filtered destinations gate the conversation pane by
    /// membership: a selected chat outside the destination's
    /// filter is not a member, one inside it is, and the plain
    /// Chats screen (and the other list screens) never gate.
    /// Demo data: chat 1 is a group, 2 an unarchived DM, 5 a
    /// channel, 11 an archived DM.
    #[test]
    fn selection_membership_follows_the_filtered_screen() {
        let mut app = demo_app();
        app.screen = Screen::Chats;
        app.selected_chat = 1;
        assert!(app.selection_in_current_filter());
        app.screen = Screen::Channels;
        app.filter = ChatFilter::Channels;
        assert!(!app.selection_in_current_filter(), "a group is not a channel");
        app.selected_chat = 5;
        assert!(app.selection_in_current_filter());
        app.screen = Screen::Groups;
        app.filter = ChatFilter::Groups;
        assert!(!app.selection_in_current_filter(), "a channel is not a group");
        app.selected_chat = 1;
        assert!(app.selection_in_current_filter());
        app.screen = Screen::Archived;
        app.filter = ChatFilter::Archived;
        app.selected_chat = 2;
        assert!(!app.selection_in_current_filter(), "an unarchived chat is not archived");
        app.selected_chat = 11;
        assert!(app.selection_in_current_filter());
        app.screen = Screen::Starred;
        app.selected_chat = 2;
        assert!(app.selection_in_current_filter());
    }

    /// Builds a demo-mode App on a default egui context.
    fn demo_app() -> App {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut args = Args::parse();
        args.live = false;
        App::new(args, &ctx)
    }

    /// Runs a settings document (produced by one app's
    /// settings_json, exactly what persist_settings writes)
    /// through the real load path on a fresh app and returns
    /// that app: the settings round trip, without touching the
    /// real data dir.
    fn reload_from_document(doc: &serde_json::Value) -> App {
        let mut fresh = demo_app();
        fresh.apply_settings_document(doc);
        fresh
    }

    /// Board item 121: the theme choice persisted nowhere, so
    /// a restart reopened on the constructor's theme. The
    /// Appearance radio's own statements (set, apply, persist)
    /// must put the choice in the document, and the load path
    /// must bring it back.
    #[test]
    fn theme_choice_round_trips_through_the_settings_document() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut args = Args::parse();
        args.live = false;
        let mut app = App::new(args, &ctx);
        // The statements the Appearance radio row runs.
        app.theme_choice = ThemeId::Light;
        app.apply_theme_choice(&ctx);
        let doc = app.settings_json();
        assert_eq!(doc["theme"], serde_json::json!("Light"));

        let mut fresh = reload_from_document(&doc);
        // Isolate from whatever the real settings file seeded:
        // start from a different choice, then load.
        fresh.theme_choice = ThemeId::Dark;
        fresh.apply_settings_document(&doc);
        assert_eq!(fresh.theme_choice, ThemeId::Light);
        // App::new recomputes the tokens from the loaded choice
        // right after the settings load; run that same apply.
        fresh.apply_theme_choice(&ctx);
        assert_eq!(
            fresh.tokens,
            Tokens::dynamic(fresh.seed, false)
                .adjust(fresh.saturation, fresh.contrast)
                .with_glow(false),
            "the loaded choice resolves to the light token set at startup"
        );
    }

    /// Board item 121: saturation and contrast were loaded and
    /// written by other paths but never saved at change time,
    /// so slider adjustments reset on restart. The fine-tuning
    /// block's own statements (mutate, apply, persist) must
    /// round-trip both values.
    #[test]
    fn saturation_and_contrast_round_trip_through_the_settings_document() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut args = Args::parse();
        args.live = false;
        let mut app = App::new(args, &ctx);
        // The statements the fine-tuning sliders run on change.
        app.saturation = 0.5;
        app.contrast = 1.1;
        app.apply_theme_choice(&ctx);
        let doc = app.settings_json();
        assert_eq!(doc["saturation"].as_f64(), Some(0.5));
        assert_eq!(doc["contrast"].as_f64(), Some(f64::from(1.1_f32)));

        let mut fresh = reload_from_document(&doc);
        fresh.saturation = 1.0;
        fresh.contrast = 1.0;
        fresh.apply_settings_document(&doc);
        assert_eq!(fresh.saturation, 0.5);
        assert_eq!(fresh.contrast, 1.1_f32);
    }

    /// Board item 121: the Chats page toggles (Enter sends,
    /// Keep chats archived) flipped state that no document
    /// carried, so both reset on restart. The page's own
    /// compare-set-persist statements must round-trip both.
    #[test]
    fn chats_page_toggles_round_trip_through_the_settings_document() {
        let mut app = demo_app();
        // The statements page_chats runs when a switch flips.
        app.enter_sends = false;
        app.keep_archived = false;
        let doc = app.settings_json();
        assert_eq!(doc["enter_sends"], serde_json::json!(false));
        assert_eq!(doc["keep_archived"], serde_json::json!(false));

        let mut fresh = reload_from_document(&doc);
        fresh.enter_sends = true;
        fresh.keep_archived = true;
        fresh.apply_settings_document(&doc);
        assert!(!fresh.enter_sends, "Enter sends survives the restart");
        assert!(!fresh.keep_archived, "Keep chats archived survives the restart");
    }

    /// Board item 121: composer drafts were session-only. A
    /// draft parked by a chat switch must reach the settings
    /// document under its chat id and come back parked on a
    /// fresh load.
    #[test]
    fn parked_composer_drafts_round_trip_through_the_settings_document() {
        let mut app = demo_app();
        app.select_chat_now(2);
        app.composer.draft = "a draft for the second chat".to_owned();
        app.select_chat_now(3);
        assert_eq!(
            app.composer.chat_drafts.get(&2).map(String::as_str),
            Some("a draft for the second chat"),
            "the switch parks the draft under its chat"
        );
        let doc = app.settings_json();
        assert_eq!(
            doc["drafts"]["2"],
            serde_json::json!("a draft for the second chat"),
            "the parked draft reaches the settings document"
        );

        let fresh = reload_from_document(&doc);
        assert_eq!(
            fresh.composer.chat_drafts.get(&2).map(String::as_str),
            Some("a draft for the second chat"),
            "a fresh load parks the draft again"
        );
    }

    /// Board item 121: the open chat's draft is part of the
    /// persisted set (under the selected chat's id), and the
    /// settings load moves it back into the composer, so
    /// typing, closing the app and reopening restores the text
    /// in place. A sent (cleared) draft leaves the document,
    /// so it cannot resurrect.
    #[test]
    fn open_composer_draft_round_trips_and_a_cleared_one_stays_gone() {
        let mut app = demo_app();
        app.select_chat_now(2);
        app.composer.draft = "still typing this".to_owned();
        let doc = app.settings_json();
        assert_eq!(
            doc["drafts"]["2"],
            serde_json::json!("still typing this"),
            "the open draft is persisted under the selected chat"
        );

        let mut fresh = reload_from_document(&doc);
        fresh.selected_chat = 2;
        fresh.composer.draft = String::new();
        fresh.restore_open_draft();
        assert_eq!(fresh.composer.draft, "still typing this");
        assert!(
            !fresh.composer.chat_drafts.contains_key(&2),
            "the restored draft leaves the parked map"
        );

        // The send path clears the draft and persists: the
        // document no longer carries the chat's entry.
        app.composer.draft.clear();
        let doc = app.settings_json();
        assert!(
            doc["drafts"].get("2").is_none(),
            "a cleared draft leaves the persisted set: {doc}"
        );
        let fresh = reload_from_document(&doc);
        assert!(!fresh.composer.chat_drafts.contains_key(&2));
    }

    fn media_msg(id: u64, view_once: bool) -> Message {
        let mut msg = Message::theirs(id, "Sarah", "", "10:00");
        msg.kind = core::model::MessageKind::Image { caption: String::new(), view_once };
        msg
    }

    #[test]
    fn forward_refuses_view_once_and_still_forwards_plain_media() {
        let mut app = demo_app();
        let source = app.chats[0].id;
        let target = app.chats[1].id;
        app.selected_chat = source;
        app.conversation = vec![media_msg(9001, true), media_msg(9002, false)];
        let count = |app: &App| app.chats.iter().find(|c| c.id == target).unwrap().messages.len();
        let before = count(&app);

        app.do_forward(9001, target);
        assert_eq!(app.notice_str(), Some("View-once media cannot be forwarded."));
        assert_eq!(count(&app), before, "view-once media must not land in the target chat");

        app.do_forward(9002, target);
        assert_eq!(app.notice_str(), Some("Message forwarded."));
        assert_eq!(count(&app), before + 1, "ordinary media still forwards");
    }

    #[test]
    fn save_refuses_view_once_and_passes_plain_media_through() {
        let mut app = demo_app();
        app.conversation = vec![media_msg(9001, true), media_msg(9002, false)];

        app.download_media(9001);
        assert_eq!(app.notice_str(), Some("View-once media cannot be saved."));

        // Demo mode has no live session, so ordinary media reaches
        // the usual path and earns the session notice there: the
        // view-once guard must not be what stops it.
        app.download_media(9002);
        assert_eq!(app.notice_str(), Some("That media is not available in this session."));
    }

    #[test]
    fn default_args_are_the_plain_live_user_run() {
        // The test process passes no mode flags, which is the
        // installed user's launch: live flow, real backend, no
        // demo data. Demo exists only behind --demo.
        let args = Args::parse();
        assert!(args.live, "a plain launch must default to the live flow");
        assert!(!args.fake_backend, "a plain launch must not use the fake backend");
        assert!(!args.fake_linked, "a plain launch must not auto-link a fake session");
    }

    #[test]
    fn expiry_pass_removes_only_due_messages_in_demo() {
        let mut app = demo_app();
        let chat_id = 2;
        {
            let chat = app.chats.iter_mut().find(|c| c.id == chat_id).unwrap();
            let mut old = core::model::Message::theirs(990_001, "Sarah", "already due", "10:00");
            old.expires_at = 500;
            let mut future = core::model::Message::theirs(990_002, "Sarah", "not yet due", "10:01");
            future.expires_at = 2_000_000;
            chat.messages.push(old);
            chat.messages.push(future);
        }
        app.select_chat_now(chat_id);
        assert!(app.conversation.iter().any(|m| m.id == 990_001));
        app.expire_disappearing_messages(1_000_000);
        let chat = app.chats.iter().find(|c| c.id == chat_id).unwrap();
        assert!(
            !chat.messages.iter().any(|m| m.id == 990_001),
            "the due message must leave the stored demo state"
        );
        assert!(
            chat.messages.iter().any(|m| m.id == 990_002),
            "a message whose timer has not run out must stay"
        );
        assert!(
            !app.conversation.iter().any(|m| m.id == 990_001),
            "the due message must leave the open conversation"
        );
        assert_eq!(chat.preview, "not yet due", "the preview falls back to the newest survivor");
    }

    #[test]
    fn default_timer_seeds_groups_created_in_demo() {
        let mut app = demo_app();
        app.default_ephemeral_secs = 86_400;
        app.groups.create_name = "Timer Test Group".to_owned();
        app.groups.create_members = String::new();
        assert!(app.do_create_group());
        let created = app.chats.iter().find(|c| c.title == "Timer Test Group").unwrap();
        assert_eq!(created.ephemeral_secs, 86_400, "the new group starts with the default timer");
        // With the default off, a group starts with no timer.
        app.default_ephemeral_secs = 0;
        app.groups.create_name = "Plain Group".to_owned();
        assert!(app.do_create_group());
        let created = app.chats.iter().find(|c| c.title == "Plain Group").unwrap();
        assert_eq!(created.ephemeral_secs, 0);
    }

    #[test]
    fn create_community_in_demo_registers_a_resolvable_community() {
        let mut app = demo_app();
        // An empty name is refused and the form keeps its error.
        app.community_create_name = "   ".to_owned();
        assert!(!app.do_create_community());
        assert!(app.community_create_error.is_some());
        // A named community is created locally: it appears in the
        // overview list, and its announcement chat resolves from
        // the session store without joining the chat list.
        app.community_create_name = "Harbor Community".to_owned();
        assert!(app.do_create_community());
        let created = app
            .communities()
            .into_iter()
            .find(|c| c.name == "Harbor Community")
            .expect("the created community is listed");
        assert!(created.group_chat_ids.is_empty());
        let announcement = app
            .session_announcements
            .iter()
            .find(|c| c.id == created.announcement_chat_id)
            .expect("the announcement chat is stored");
        assert_eq!(announcement.community.as_deref(), Some("Harbor Community"));
        assert!(
            !app.chats.iter().any(|c| c.id == created.announcement_chat_id),
            "the announcement chat must not join the chat list"
        );
    }

    #[test]
    fn write_back_conversation_never_touches_live_state() {
        let mut app = demo_app();
        app.select_chat_now(1);
        let stored_before: Vec<u64> =
            app.chats.iter().find(|c| c.id == 1).unwrap().messages.iter().map(|m| m.id).collect();
        app.conversation.push(core::model::Message::mine(990_003, "unsaved edit", "10:02"));
        // In live mode the helper is a hard no-op: the working
        // copy can never be written into stored chats.
        app.live_mode = true;
        app.write_back_conversation(1);
        let stored: Vec<u64> =
            app.chats.iter().find(|c| c.id == 1).unwrap().messages.iter().map(|m| m.id).collect();
        assert_eq!(stored, stored_before, "live mode must not accept demo write-backs");
        // In demo mode the same call is the normal switch path.
        app.live_mode = false;
        app.write_back_conversation(1);
        let stored: Vec<u64> =
            app.chats.iter().find(|c| c.id == 1).unwrap().messages.iter().map(|m| m.id).collect();
        assert!(stored.contains(&990_003), "demo mode writes the working copy back on switch");
    }

    fn staged_photo(name: &str) -> StagedMedia {
        StagedMedia {
            kind: StagedKind::Photo,
            name: name.to_owned(),
            path: std::path::PathBuf::from(format!("/tmp/{name}")),
            mime: "image/png".to_owned(),
            view_once: false,
            thumb: None,
        }
    }

    /// The composer scoping guarantee (audit finding: composer
    /// state leaked across chats): everything composed in one
    /// chat parks on switch and restores on return, on the
    /// centralized selection path every panel switch uses.
    #[test]
    fn composer_state_parks_and_restores_per_chat() {
        let mut app = demo_app();
        app.select_chat_now(2);
        app.composer.draft = "draft for two".to_owned();
        app.composer.staged.push(staged_photo("two.png"));
        app.composer.staged_caption = "caption for two".to_owned();
        app.composer.staged_chat = Some(2);
        app.composer.reply_to = Some((2001, "Sarah Miller".to_owned(), "quoted".to_owned()));
        app.composer.editing_id = Some(2001);
        app.composer.poll_draft = Some(PollDraft {
            question: "Lunch?".to_owned(),
            options: vec!["Yes".to_owned(), "No".to_owned()],
            multiple: false,
        });
        app.composer.location_draft = Some(LocationDraft {
            name: "Office".to_owned(),
            latitude: "0".to_owned(),
            longitude: "0".to_owned(),
        });

        // Switching away leaves a clean composer behind: nothing
        // composed for chat 2 is visible, let alone sendable,
        // in chat 3.
        app.select_chat_now(3);
        assert!(app.composer.draft.is_empty(), "the draft must park with its chat");
        assert!(app.composer.staged.is_empty(), "staged files must park with their chat");
        assert!(app.composer.staged_caption.is_empty());
        assert_eq!(app.composer.staged_chat, None);
        assert!(app.composer.reply_to.is_none(), "the quote target must park with its chat");
        assert!(app.composer.editing_id.is_none(), "the edit target must park with its chat");
        assert!(app.composer.poll_draft.is_none(), "the poll draft must park with its chat");
        assert!(app.composer.location_draft.is_none());
        assert_eq!(app.composer.chat_drafts.get(&2).map(String::as_str), Some("draft for two"));
        assert!(app.composer.chat_parked.contains_key(&2));

        // A draft typed in chat 3 parks under 3, and returning
        // to chat 2 restores its full composer state.
        app.composer.draft = "draft for three".to_owned();
        app.select_chat_now(2);
        assert_eq!(app.composer.draft, "draft for two");
        assert_eq!(app.composer.staged.len(), 1);
        assert_eq!(app.composer.staged[0].name, "two.png");
        assert_eq!(app.composer.staged_caption, "caption for two");
        assert_eq!(app.composer.staged_chat, Some(2));
        assert_eq!(app.composer.reply_to.as_ref().map(|(id, _, _)| *id), Some(2001));
        assert_eq!(app.composer.editing_id, Some(2001));
        assert_eq!(app.composer.poll_draft.as_ref().map(|d| d.question.as_str()), Some("Lunch?"));
        assert_eq!(app.composer.location_draft.as_ref().map(|d| d.name.as_str()), Some("Office"));
        assert_eq!(app.composer.chat_drafts.get(&3).map(String::as_str), Some("draft for three"));

        // And chat 3's draft comes back on the next switch.
        app.select_chat_now(3);
        assert_eq!(app.composer.draft, "draft for three");
        assert!(app.composer.staged.is_empty());
    }

    /// The edit boundary guard: an edit target minted in one
    /// chat must not edit anything while another chat is open,
    /// and must still work in its own chat.
    #[test]
    fn apply_edit_ignores_a_target_from_another_chat() {
        let mut app = demo_app();
        app.select_chat_now(2);
        app.start_edit(2001);
        assert_eq!(app.composer.editing_id, Some(2001));

        // In chat 3 the same call is a no-op everywhere: not in
        // the open conversation, not in chat 3's store copy, and
        // not in chat 2's store copy either.
        app.select_chat_now(3);
        let before_3: Vec<String> = app.conversation.iter().map(|m| m.body.clone()).collect();
        app.apply_edit(2001, "edited from afar");
        let after_3: Vec<String> = app.conversation.iter().map(|m| m.body.clone()).collect();
        assert_eq!(before_3, after_3, "a foreign edit target must not touch the open chat");
        let stored_2 = app.chats.iter().find(|c| c.id == 2).unwrap();
        assert!(
            !stored_2.messages.iter().any(|m| m.body == "edited from afar"),
            "a foreign edit target must not reach its own chat from afar either"
        );

        // Back in its own chat the edit applies normally.
        app.select_chat_now(2);
        app.apply_edit(2001, "edited properly");
        let msg = app.conversation.iter().find(|m| m.id == 2001).unwrap();
        assert_eq!(msg.body, "edited properly");
        assert!(msg.edited);
    }

    /// The wire-quote boundary guard: the protocol id map is
    /// global, so a reply target from another chat must resolve
    /// to no quote while this chat is open.
    #[test]
    fn live_quote_for_reply_rejects_a_foreign_target() {
        let mut app = demo_app();
        app.select_chat_now(2);
        app.protocol_ids.insert(2001, "proto-2001".to_owned());
        app.protocol_ids.insert(3001, "proto-3001".to_owned());
        app.sender_jids.insert(2001, "sarah@s.whatsapp.net".to_owned());

        // A target from the open conversation resolves.
        app.composer.reply_to = Some((2001, "Sarah Miller".to_owned(), "hi".to_owned()));
        let quoted = app.live_quote_for_reply().expect("an in-chat target must resolve");
        assert_eq!(quoted.message_id, "proto-2001");
        assert_eq!(quoted.sender_jid.as_deref(), Some("sarah@s.whatsapp.net"));

        // A target minted in chat 3 resolves to nothing here,
        // even though its protocol id is known globally.
        app.composer.reply_to = Some((3001, "Alex".to_owned(), "hi".to_owned()));
        assert!(app.live_quote_for_reply().is_none(), "a foreign quote must not resolve");
    }

    /// The staged-send backstop: a stage whose owning chat is
    /// not the open one (a switch that never parked) is refused
    /// and dropped, never delivered into the open chat.
    #[test]
    fn send_staged_refuses_a_stage_from_another_chat() {
        let mut app = demo_app();
        app.select_chat_now(2);
        app.composer.staged.push(staged_photo("for-two.png"));
        app.composer.staged_chat = Some(2);

        // Simulate a switch that bypassed the parking entirely:
        // selection and conversation move, the stage stays live.
        app.selected_chat = 3;
        app.conversation = app.chats.iter().find(|c| c.id == 3).unwrap().messages.clone();
        let stored_before = app.chats.iter().find(|c| c.id == 3).unwrap().messages.len();
        let mut engaged = false;
        app.send_staged(&mut engaged);
        assert!(!engaged, "a refused send is not an engagement");
        assert!(app.composer.staged.is_empty(), "the foreign stage is dropped");
        assert_eq!(app.composer.staged_chat, None);
        assert!(app.notice.is_some(), "the refusal is surfaced, not silent");
        assert_eq!(
            app.chats.iter().find(|c| c.id == 3).unwrap().messages.len(),
            stored_before,
            "nothing may be delivered into the open chat"
        );

        // A stage owned by the open chat sends normally.
        app.composer.staged.push(staged_photo("for-three.png"));
        app.composer.staged_chat = Some(3);
        app.send_staged(&mut engaged);
        assert!(engaged);
        assert_eq!(app.chats.iter().find(|c| c.id == 3).unwrap().messages.len(), stored_before + 1);
    }

    #[test]
    fn switching_chats_cancels_an_active_recording() {
        let mut app = demo_app();
        app.select_chat_now(1);
        // One second of captured audio stands in for a take in
        // progress (tests have no microphone to record from).
        app.recorder = Some(crate::recorder::Recorder::for_test(&vec![0; 48_000]));
        app.select_chat_now(2);
        assert!(app.recorder.is_none(), "the switch must cancel the recording");
        // Nothing of the take can be sent afterwards: finishing
        // is a no-op, no voice note lands in the newly opened
        // chat, and no send is counted.
        let conversation_before = app.conversation.len();
        let sent_before = app.sent_counter;
        let mut engaged = false;
        app.finish_recording(&mut engaged);
        assert!(!engaged);
        assert_eq!(app.conversation.len(), conversation_before);
        assert_eq!(app.sent_counter, sent_before);
    }

    #[test]
    fn a_recording_finished_in_its_own_chat_still_sends() {
        let mut app = demo_app();
        app.select_chat_now(1);
        app.recorder = Some(crate::recorder::Recorder::for_test(&vec![0; 48_000]));
        // Re-selecting the chat already open is not a switch:
        // the take survives it.
        app.select_chat_now(1);
        assert!(app.recorder.is_some(), "staying in the chat must keep the recording");
        let before = app.conversation.len();
        let mut engaged = false;
        app.finish_recording(&mut engaged);
        assert!(app.recorder.is_none(), "finishing consumes the recorder");
        assert!(engaged);
        assert_eq!(app.conversation.len(), before + 1, "the voice note lands in its own chat");
        let last = app.conversation.last().expect("the sent note is last");
        assert!(
            matches!(last.kind, core::model::MessageKind::Voice { secs: 1, .. }),
            "the sent note is the recorded second"
        );
    }

    /// The layout-mode decision over the owner's grading set:
    /// the portrait phone sizes land in the native portrait
    /// mode, the square and landscape small sizes keep the
    /// single-pane shell, the passing desktop sizes stay
    /// two-pane, and the two-pane switch happens exactly at the
    /// breakpoint, which is the layout's own constants added up
    /// (rail + list panel + the narrowest honest conversation
    /// column). Portrait is strictly taller-than-wide: a square
    /// window is not portrait.
    #[test]
    fn layout_mode_switches_at_the_breakpoint() {
        use super::{layout_mode_for_size, LayoutMode, SINGLE_PANE_BREAKPOINT};
        assert_eq!(SINGLE_PANE_BREAKPOINT, 60.0 + 380.0 + 550.0);
        // The graded set, as (width, height) pairs.
        for (w, h) in [(360.0, 800.0), (390.0, 844.0)] {
            assert_eq!(layout_mode_for_size(w, h), LayoutMode::Portrait, "{w}x{h}");
        }
        for (w, h) in [(600.0, 600.0), (800.0, 800.0), (844.0, 390.0)] {
            assert_eq!(layout_mode_for_size(w, h), LayoutMode::SinglePane, "{w}x{h}");
        }
        for (w, h) in [(1180.0, 820.0), (1440.0, 900.0)] {
            assert_eq!(layout_mode_for_size(w, h), LayoutMode::TwoPane, "{w}x{h}");
        }
        // The portrait boundary: equal sides are not portrait,
        // one point taller is.
        assert_eq!(layout_mode_for_size(500.0, 500.0), LayoutMode::SinglePane);
        assert_eq!(layout_mode_for_size(500.0, 501.0), LayoutMode::Portrait);
        // The two-pane switch happens exactly at the breakpoint
        // (a landscape height, so only the width decides).
        assert_eq!(
            layout_mode_for_size(SINGLE_PANE_BREAKPOINT - 1.0, 600.0),
            LayoutMode::SinglePane
        );
        assert_eq!(layout_mode_for_size(SINGLE_PANE_BREAKPOINT, 600.0), LayoutMode::TwoPane);
    }

    /// The mode is decided on the window's PHYSICAL size: the
    /// content rect arrives in points at the pixels-per-point
    /// in effect, so a scaled portrait window (fewer physical
    /// pixels than points) still reads as portrait, and a
    /// scaled desktop window still reads as two-pane. The
    /// graded sizes are checked at the portrait density, where
    /// points run about 11% larger than pixels.
    #[test]
    fn layout_mode_decides_on_physical_size() {
        use super::layout_mode_for_content;
        use egui::Vec2;
        // 390x844 physical at the portrait density (0.9):
        // the content rect is 433x938 points, still portrait.
        assert_eq!(
            layout_mode_for_content(Vec2::new(390.0 / 0.9, 844.0 / 0.9), 0.9),
            LayoutMode::Portrait
        );
        // 1180x820 physical at a user scale of 1.15: the
        // content rect is 1026x713 points, under the breakpoint
        // in points, but the physical window is a desktop one.
        assert_eq!(
            layout_mode_for_content(Vec2::new(1180.0 / 1.15, 820.0 / 1.15), 1.15),
            LayoutMode::TwoPane
        );
        // At 1.0 the content size is the physical size.
        assert_eq!(layout_mode_for_content(Vec2::new(844.0, 390.0), 1.0), LayoutMode::SinglePane);
    }

    /// The density composition: the user's font-size percent
    /// multiplies the mode's automatic factor, the desktop
    /// mode never scales on its own, and 100% in two-pane is
    /// exactly the designed size.
    #[test]
    fn effective_scale_composes_user_preference_and_mode() {
        use super::{effective_pixels_per_point, LayoutMode};
        assert_eq!(effective_pixels_per_point(100, LayoutMode::TwoPane), 1.0);
        assert_eq!(effective_pixels_per_point(100, LayoutMode::SinglePane), 0.95);
        assert_eq!(effective_pixels_per_point(100, LayoutMode::Portrait), 0.90);
        // The font-size steps (Small / Default / Large) in
        // portrait: Large lands near the old unscaled density,
        // Small is visibly tighter.
        assert!((effective_pixels_per_point(85, LayoutMode::Portrait) - 0.765).abs() < 1e-6);
        assert!((effective_pixels_per_point(115, LayoutMode::Portrait) - 1.035).abs() < 1e-6);
        assert!((effective_pixels_per_point(115, LayoutMode::TwoPane) - 1.15).abs() < 1e-6);
    }

    /// Portrait chrome: the bottom navigation bar shows on the
    /// tab screens and hides under full-screen content and on
    /// the non-tab pages (Settings here); the FAB floats only
    /// over the chat list. The Updates screen parses from both
    /// of its QA flag names.
    #[test]
    fn portrait_chrome_follows_the_screen() {
        let mut app = demo_app();
        app.portrait = true;
        app.screen = Screen::Chats;
        assert!(app.bottom_nav_visible());
        assert!(app.portrait_fab_visible());
        app.screen = Screen::Updates;
        assert!(app.bottom_nav_visible());
        assert!(!app.portrait_fab_visible());
        app.screen = Screen::Settings;
        assert!(!app.bottom_nav_visible());
        app.screen = Screen::Chats;
        app.conversation_open = true;
        assert!(!app.bottom_nav_visible(), "an open conversation hides the bar");
        assert!(!app.portrait_fab_visible());
        assert_eq!(Screen::parse("updates"), Some(Screen::Updates));
        assert_eq!(Screen::parse("status"), Some(Screen::Updates));
    }

    /// Single-pane pane selection: a fresh app shows the list
    /// (nothing is open over it); selecting a chat is what opens
    /// the conversation pane, which is also how the --select-chat
    /// QA flag lands on the conversation at narrow widths.
    #[test]
    fn selecting_a_chat_opens_the_conversation_pane() {
        let mut app = demo_app();
        assert!(!app.conversation_open, "a fresh app shows the list pane");
        assert!(!app.content_visible());
        app.select_chat_now(2);
        assert!(app.conversation_open, "selecting a chat opens its conversation");
        assert!(app.content_visible());
        app.conversation_open = false;
        assert!(!app.content_visible(), "the back affordance returns to the list");
    }
}
