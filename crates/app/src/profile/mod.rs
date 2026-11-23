//! Chat profile screens: the standard WhatsApp info layout for
//! contacts, groups and channels, plus an AI profile in the style
//! of Meta AI's. Opened from the conversation header (avatar or
//! title) or the header menu; the surface is a panel docked to
//! the right of the conversation (Telegram-style), and
//! [`crate::state::App::draw_profile_dock`] picks a [`ProfileKind`] and renders
//! the matching body into it.
//!
//! Layout language: a large centered avatar, the name with its
//! badges, a status line, then flat sections of full-width rows
//! separated by hairlines. The AI variant swaps the rows for a capability
//! description and suggestion chips, the way Meta AI presents
//! itself. Only actions that do something real in the app are
//! wired; the audio/video rows render because the layout calls for
//! them and answer with an honest notice, because Zaprust has no
//! calls surface yet.

use core::model::{Chat, ChatKind};

mod dock;
mod gallery;
mod my_profile;
mod panel;

pub use my_profile::{
    draw_my_profile_panel, my_profile_sync_photo, paint_my_avatar_disc, AccountProfile,
    MyProfileMode, MyProfileState, SavePhase,
};

/// Which profile layout a chat gets. The signed-in user's own
/// profile is not a variant here: it has no chat to classify, and
/// its editor is a separate panel with its own state and intents
/// ([`draw_my_profile_panel`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProfileKind {
    /// A one-to-one contact.
    User,
    /// A group chat.
    Group,
    /// A channel (newsletter).
    Channel,
    /// An AI contact: Meta AI.
    Ai,
}

/// Classifies a chat for the profile screen. A chat flagged
/// `meta_ai` gets the AI layout regardless of its stored kind.
#[must_use]
pub(crate) fn profile_kind(chat: &Chat) -> ProfileKind {
    if chat.meta_ai {
        ProfileKind::Ai
    } else {
        match chat.kind {
            ChatKind::Dm => ProfileKind::User,
            ChatKind::Group => ProfileKind::Group,
            ChatKind::Channel => ProfileKind::Channel,
        }
    }
}

/// Clicks collected while a profile body renders. Bodies are free
/// functions without `App` access; the shell applies the intents
/// after the panel draws, the same flag pattern the rest of the
/// panel uses.
#[derive(Debug, Default)]
pub(crate) struct ProfileIntents {
    /// Close the panel.
    pub close: bool,
    /// Toggle block on the contact (user variant danger rows).
    pub toggle_block: bool,
    /// Report the contact (user variant danger rows): opens the
    /// spam-report confirmation for the chat's latest incoming
    /// message, the same flow the message menu's Report uses.
    pub report: bool,
    /// Focus the chat-list search (user variant Search action).
    pub focus_search: bool,
    /// An AI suggestion chip was clicked: put this text in the
    /// composer draft.
    pub suggestion: Option<String>,
    /// An audio/video action was clicked: Zaprust has no calls
    /// surface yet, so the shell answers with a notice.
    pub start_call: bool,
    /// The export action cell was clicked (user variant, when the
    /// export feature is on).
    pub export: bool,
    /// A "Media, links and docs" row was clicked: open the
    /// gallery on this section (section 25).
    pub gallery: Option<crate::state::GallerySection>,
    /// A disappearing-messages duration was picked (user and
    /// group variants): seconds, `0` for off.
    pub ephemeral_pick: Option<u32>,
}

/// What one member kebab menu asked for. The menu only records
/// the pick; the dock applies it after the menu has closed, so
/// no action ever runs inside the popup that requested it.
enum MemberAction {
    /// Open the member's DM chat (only offered when one exists).
    Message(u64),
    /// Promote or demote the member at this address.
    Op(String, backend::ParticipantOp),
    /// Remove the member at this address (parks a confirmation).
    Remove(String, String),
}
