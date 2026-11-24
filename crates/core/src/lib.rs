//! Zaprust core, pure logic for the Zaprust desktop client.
//!
//! This crate intentionally knows nothing about windows or drawing.
//! It owns:
//!
//! - [`model`], chats, messages, contacts and the demo dataset.
//! - [`theme`], semantic theme tokens generated from one seed color.
//! - [`filter`], chat-list filtering and searching.
//! - [`credentials`], [`update`], [`crash`], [`debug`], [`rich`],
//!   credential storage, self-update checks, crash reports, debug
//!   bundles and rich-text parsing.
//! - [`net`], the shared public-address validation every remote
//!   fetch (GIF media) runs before connecting.
//!
//! Everything here is unit-tested without a GUI. The application crate
//! (the `app` crate) is a thin shell that draws what this crate describes.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bidi;
pub mod crash;
pub mod credentials;
pub mod debug;
pub mod filter;
pub mod lockcode;
pub mod model;
pub mod net;
pub mod rich;
pub mod theme;
pub mod update;

#[cfg(test)]
mod props_tests;
