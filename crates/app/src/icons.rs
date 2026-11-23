//! Google Material icons, drawn as text from the bundled subset
//! icon font (family `zaprust-material`, registered in theme.rs).
//! The font is built by `packaging/material-icons.py` from Material
//! Symbols Rounded, pinned to the outline instance (FILL 0, GRAD 0,
//! opsz 24, wght 500) and subset to the glyphs below. One consistent,
//! friendly icon set everywhere instead of hand drawn strokes.
//!
//! The codepoints are private use area values taken from the source
//! font's own codepoints file; only this font can render them, and
//! the build script verifies every one survives subsetting.

use eframe::egui::{self, Align2, Color32, FontFamily, FontId, Pos2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Icon {
    Chat,
    Megaphone,
    Users,
    Blocks,
    Sliders,
    Search,
    Plus,
    Smile,
    Mic,
    MicOff,
    Phone,
    Video,
    X,
    Contrast,
    Bell,
    Shield,
    Database,
    Info,
    Check,
    ChevronLeft,
    ChevronRight,
    Sparkles,
    Group,
    Play,
    Pause,
    ChevronDown,
    Reply,
    Copy,
    Edit,
    Delete,
    Forward,
    Star,
    Pin,
    Download,
    Poll,
    React,
    Person,
    Archive,
    MoreVert,
    Menu,
    Image,
    Document,
    LocationOn,
    Stop,
    LooksOne,
    BellOff,
    Status,
    AddCall,
    CallMade,
    CallReceived,
    CallMissed,
    CallEnd,
    VolumeUp,
    LogOut,
    Schedule,
    Send,
    Lock,
    Done,
    DoneAll,
}

/// The icon size grid (design standard, icon discipline):
/// every glyph drawn in the app takes one of these sizes, so
/// the family reads at one scale language everywhere. Values
/// are pre-optical; [`draw`] applies the per-glyph correction.
/// Sizes derived from a control's hit box (icon buttons) or an
/// avatar's radius are documented derived scales, not grid
/// values, and say so at their call sites.
pub(crate) const SIZE_BADGE: f32 = 12.0;
/// Row and menu-item glyphs.
pub(crate) const SIZE_ROW: f32 = 17.0;
/// Menu and list-control glyphs.
pub(crate) const SIZE_MENU: f32 = 18.0;
/// Composer and bubble controls.
pub(crate) const SIZE_CONTROL: f32 = 22.0;
/// Rail, tab bar and floating action buttons.
pub(crate) const SIZE_TAB: f32 = 24.0;
/// Large placeholders on media.
pub(crate) const SIZE_LARGE: f32 = 32.0;
/// Empty-state and placeholder display glyphs.
pub(crate) const SIZE_DISPLAY: f32 = 40.0;

/// Snaps an arbitrary size to the nearest grid value. Icon
/// buttons derive their glyph size from the hit box; snapping
/// the result keeps those glyphs on the same grid as every
/// directly drawn one.
pub(crate) fn snap_size(v: f32) -> f32 {
    const GRID: [f32; 7] =
        [SIZE_BADGE, SIZE_ROW, SIZE_MENU, SIZE_CONTROL, SIZE_TAB, SIZE_LARGE, SIZE_DISPLAY];
    GRID.iter().copied().min_by(|a, b| (a - v).abs().total_cmp(&(b - v).abs())).unwrap_or(SIZE_ROW)
}

/// The Material Symbols codepoint for `icon` in the bundled font.
pub(crate) fn glyph(icon: Icon) -> char {
    match icon {
        Icon::Chat => '\u{e0cb}',         // chat_bubble
        Icon::Megaphone => '\u{ef49}',    // campaign
        Icon::Users => '\u{ea21}',        // group
        Icon::Blocks => '\u{e87b}',       // extension
        Icon::Sliders => '\u{e429}',      // tune
        Icon::Search => '\u{ef7a}',       // search
        Icon::Plus => '\u{e145}',         // add
        Icon::Smile => '\u{e813}',        // sentiment_satisfied
        Icon::Mic => '\u{e31d}',          // mic
        Icon::MicOff => '\u{e02b}',       // mic_off
        Icon::Phone => '\u{f0d4}',        // call
        Icon::Video => '\u{e04b}',        // videocam
        Icon::X => '\u{e5cd}',            // close
        Icon::Contrast => '\u{eb37}',     // contrast
        Icon::Bell => '\u{e7f5}',         // notifications
        Icon::Shield => '\u{e9e0}',       // shield
        Icon::Database => '\u{f20e}',     // database
        Icon::Info => '\u{e88e}',         // info
        Icon::Check => '\u{e668}',        // check
        Icon::ChevronLeft => '\u{e5cb}',  // chevron_left
        Icon::ChevronRight => '\u{e5cc}', // chevron_right
        Icon::Sparkles => '\u{e65f}',     // auto_awesome
        Icon::Group => '\u{f233}',        // groups
        Icon::Play => '\u{e037}',         // play_arrow
        Icon::Pause => '\u{e034}',        // pause
        Icon::ChevronDown => '\u{e5cf}',  // expand_more
        Icon::Reply => '\u{e15e}',        // reply
        Icon::Copy => '\u{e14d}',         // content_copy
        Icon::Edit => '\u{f097}',         // edit
        Icon::Delete => '\u{e92e}',       // delete
        Icon::Forward => '\u{f57a}',      // forward
        Icon::Star => '\u{f09a}',         // star
        Icon::Pin => '\u{f10d}',          // push_pin
        Icon::Download => '\u{f090}',     // download
        Icon::Poll => '\u{f0cc}',         // poll
        Icon::React => '\u{e1d3}',        // add_reaction
        Icon::Person => '\u{f0d3}',       // person
        Icon::Archive => '\u{e149}',      // archive
        Icon::MoreVert => '\u{e5d4}',     // more_vert
        Icon::Menu => '\u{e5d2}',         // menu
        Icon::Image => '\u{e3f4}',        // image
        Icon::Document => '\u{e873}',     // description
        Icon::LocationOn => '\u{f1db}',   // location_on
        Icon::Stop => '\u{e047}',         // stop
        Icon::LooksOne => '\u{e400}',     // looks_one
        Icon::BellOff => '\u{e7f6}',      // notifications_off
        Icon::Status => '\u{e917}',       // donut_large
        Icon::AddCall => '\u{f0b7}',      // add_call
        Icon::CallMade => '\u{e0b2}',     // call_made
        Icon::CallReceived => '\u{e0b5}', // call_received
        Icon::CallMissed => '\u{e0b4}',   // call_missed
        Icon::CallEnd => '\u{f0bc}',      // call_end
        Icon::VolumeUp => '\u{e050}',     // volume_up
        Icon::LogOut => '\u{e9ba}',       // logout
        Icon::Schedule => '\u{efd6}',     // schedule
        Icon::Send => '\u{e163}',         // send
        Icon::Lock => '\u{e899}',         // lock
        Icon::Done => '\u{e876}',         // done
        Icon::DoneAll => '\u{e877}',      // done_all
    }
}

/// Draws `icon` centred in a `size` by `size` box at `center` by
/// painting its glyph from the bundled icon font. `_bg` is kept so
/// call sites written for the old stroke renderer (which knocked
/// hollow details out against the background) compile unchanged;
/// font glyphs are solid shapes and need no knockout.
pub(crate) fn draw(
    painter: &egui::Painter,
    icon: Icon,
    center: Pos2,
    size: f32,
    color: Color32,
    _bg: Color32,
) {
    painter.text(
        center,
        Align2::CENTER_CENTER,
        glyph(icon),
        FontId::new(size * optical(icon), FontFamily::Name("zaprust-material".into())),
        color,
    );
}

/// Optical size corrections, applied in [`draw`] rather than at
/// every call site: Material glyphs differ in apparent weight at
/// one pixel size (an outlined speech bubble reads smaller than
/// the smiley next to it, three dots read smaller than a filled
/// disc), so the glyphs that run light get a small bump here.
/// Measured against side-by-side pills in the composer and the
/// menus; values stay within 10 percent so stroke weights match.
fn optical(icon: Icon) -> f32 {
    match icon {
        Icon::Chat | Icon::MoreVert => 1.10,
        Icon::Search | Icon::Mic | Icon::Download | Icon::BellOff => 1.06,
        Icon::Phone | Icon::Document | Icon::Edit => 1.04,
        _ => 1.0,
    }
}
