//! Shared drawing widgets: rail buttons, chips, avatars, name colors.
//!
//! The component library, grouped by family into submodules:
//! buttons, menus, inputs, avatars, indicators and brand. Everything is
//! re-exported here, so callers keep using `crate::widgets::button`
//! style paths.

mod avatars;
mod brand;
mod buttons;
mod indicators;
mod inputs;
mod list_row;
mod menus;
mod scroll;

pub(crate) use avatars::*;
pub(crate) use brand::*;
pub(crate) use buttons::*;
pub(crate) use indicators::*;
pub(crate) use inputs::*;
pub(crate) use list_row::*;
pub(crate) use menus::*;
pub(crate) use scroll::*;
