//! Shared drawing widgets: rail buttons, chips, avatars, name colors.
//!
//! The component library, grouped by family into submodules:
//! buttons, menus, inputs, avatars and indicators. Everything is
//! re-exported here, so callers keep using `crate::widgets::button`
//! style paths.

mod avatars;
mod buttons;
mod indicators;
mod inputs;
mod menus;

pub(crate) use avatars::*;
pub(crate) use buttons::*;
pub(crate) use indicators::*;
pub(crate) use inputs::*;
pub(crate) use menus::*;
