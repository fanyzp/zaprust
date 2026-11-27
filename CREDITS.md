# Credits & third-party works

Zaprust is an original, independently developed project
(MIT License) built on the work of many open-source maintainers.
Thank you, to everyone named here, and to the hundreds of
dependency maintainers not named individually: this project
exists because you shared yours.

## Creator

- **Created by fanyzp**, [github.com/fanyzp](https://github.com/fanyzp)

## Icons

The interface icons are Material Symbols Rounded, Google's icon
typeface (Apache License 2.0), subset to the glyphs Zaprust
uses and drawn as text from the bundled font. See
`packaging/material-icons.py` for how the subset is built.

## Fonts

- **Inter** 4.1 (subsets: Regular, Medium, SemiBold, Bold, plus
  Italic for markdown), SIL Open Font License 1.1. See
  `crates/app/assets/fonts/`.

## Libraries (selection)

- [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust),
  the WhatsApp protocol implementation Zaprust's live mode is
  built on (see its repository for its license).
- SQLite, public domain; via `rusqlite` (bundled).
- egui / eframe, MIT OR Apache-2.0.
- Tokio, MIT.
- reqwest / rustls, MIT OR Apache-2.0.
- The `keyring`, `directories`, `open`, `tracing`, `zip`,
  `image`, `qrcode`, `serde`, and `proptest` crates, their
  respective MIT/Apache-2.0 licenses.

## Theming

Zaprust bundles no third-party palettes. Its themes are dynamic
color: every surface and accent is generated in the app from a
single seed color, in dark, light, and system-following
variants.

A generated THIRD-PARTY-LICENSES file (e.g. via `cargo about`)
is not yet produced; this hand-maintained list is the current
record. If you believe any credit here is incomplete or
incorrect, please open an issue.
