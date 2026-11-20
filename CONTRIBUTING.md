# Contributing to Zaprust

Thank you for helping build Zaprust. This document is the contract every
contribution follows. It exists so the project stays readable, testable,
and welcoming as more people join.

Zaprust is an original project by fanyzp, built from scratch.
By contributing you agree your work is licensed under the project's MIT
license.

---

## 1. Module map

The workspace splits the client side from the frontend. The rule that
keeps the split clean: the UI crate never talks to the protocol library,
and the client crates never draw anything.

### Client side

| Crate | Role |
|---|---|
| `crates/core` | Pure logic, no GUI: data model, theme tokens, credentials, crash reports, rich-message reducers, update logic, debug redaction. |
| `crates/backend` | The `Backend` trait plus the event/command protocol, the real WhatsApp backend, a deterministic fake backend for tests and screenshots, QR encoding, audio, and the panic-containing supervisor. |
| `crates/store` | The app's own chat archive on SQLite, with versioned migrations. |
| `crates/features/*` | One crate per native feature: `quick-replies`, `export-chat`, `stickers`, `giphy`. Every feature is compiled in and switched in Settings; there is no plugin system. |

### Frontend

| Path | Role |
|---|---|
| `crates/app` | The egui application; ships the `zaprust` binary. A thin shell over `core`: screens, live session wiring, demo data. |
| `crates/app/src/conversation/` | The open conversation: `composer.rs` (input row, send path) and `stream.rs` (day chips, unread divider, scrolling). |
| `crates/app/src/rich_ui/` | Rich message surfaces: `pickers.rs`, `menus.rs`, `poll.rs`. |
| `crates/app/src/settings/` | One module per Settings category, plus the settings registry that powers Settings search. |

Feature status and honest limits live in
[`docs/FEATURE-MATRIX.md`](docs/FEATURE-MATRIX.md); the system design
lives in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## 2. Ground rules

- Be kind and constructive, see [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).
- One pull request = one intention. Unrelated changes go in separate PRs.
- Every PR description states: **what** changed, **why**, **how it was
  tested**, and a **screenshot** for any UI change.
- `main` and `nightly` must always build. No direct pushes to `main`;
  `main` only receives changes promoted from `nightly` by pull request.

## 3. Development setup

```bash
git clone https://github.com/fanyzp/zaprust
cd zaprust
cargo run -p app                   # the real client (live; links or reconnects an account)
cargo run -p app -- --demo         # offline demo mode with sample data
cargo run -p app -- --live --fake-backend --fake-linked   # scripted live mode
```

Rust: stable via rustup. The workspace MSRV is 1.85 (declared in the
workspace `Cargo.toml`); development happens on the current stable.
System packages per OS:

### Linux (Debian/Ubuntu)

```bash
sudo apt-get install -y build-essential cmake perl clang \
  libclang-dev libasound2-dev libpipewire-0.3-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libgl1-mesa-dev libgtk-3-dev libopus-dev pkg-config
```

(The `libclang`/`cmake`/`perl` set is for the protocol library's
native dependencies; the rest is the usual egui/winit set, and
`libopus` encodes voice notes.)

### Windows

- Visual Studio Build Tools (Desktop development with C++).
- CMake and Perl (e.g. via `winget install Kitware.CMake` and
  Strawberry Perl) on PATH, the protocol library's native deps need
  them.
- Then `cargo run -p app` from a developer prompt.

### macOS

```bash
xcode-select --install
brew install cmake perl
cargo run -p app
```

Crate names in this repo are short on purpose:
`core`, `backend`, `store`, `app` (the binary is `zaprust`).

## 4. The checks (CI runs exactly these)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

A PR with a red check is not reviewed until it is green. Run them locally
before pushing, it takes a minute and saves a round-trip.

All workflows are manual-only: nothing builds on push. CI and releases
are started from the Actions tab (the Release workflow takes the
existing version tag as its input).

## 5. Code standards

### Rust

- Edition 2021. The toolchain is not pinned to a version: it floats
  on stable (`rust-toolchain.toml` tracks the stable channel, CI
  installs stable the same way). The floor is MSRV 1.85, declared in
  the workspace `Cargo.toml` and inherited by every crate; do not
  rely on language or std features newer than the MSRV.
- `rustfmt` output is the only acceptable formatting. No manual exceptions.
- Clippy must be warning-free. Any `#[allow(...)]` needs a one-line reason
  comment next to it.
- **`unsafe` is forbidden** in the whole workspace
  (`#![forbid(unsafe_code)]`). If you believe you truly need it, open an
  issue with the design first, instead of a PR.
- No `unwrap()` / `expect()` on anything that can fail because of user
  input, files, or the network. Use `?` with context. `unwrap()` in tests
  and on proven invariants (with a comment) is fine.
- Libraries return typed errors (hand-rolled enums with human `Display`
  text); free-form strings stay at the display boundary only.
- Logging goes through `tracing` and must never include message contents
  or phone numbers, at any level.
- No global mutable state. State is owned by an explicit struct.

### egui / UI

- `update()` functions only: read state, draw, emit an intent
  (command/event). Business logic lives in `core`, never in a
  draw function.
- Colors, fonts, and spacing come **only** from theme tokens
  (`core::theme`). A literal `Color32::from_rgb(...)` outside the
  theme module fails review.
- Durations and easings come only from `app::motion`; no ad-hoc
  animation constants.
- Repeated widgets (chat row, message bubble) are pure functions of
  `(model, theme)`.
- egui widget IDs must be derived from stable data IDs, not loop indices.
- Every new screen must be reachable in demo mode and selectable via
  `--screen <name>` so screenshots stay reproducible.

### Tests

- Pure logic lives in `core` precisely so it can be unit-tested
  without a window. New logic ships with tests in the same PR.
- A bug fix ships with a test that fails before the fix, when the bug is
  testable at all.

## 6. Design notes for surface changes

Before changing any of the surfaces below, open an issue (or a draft PR)
with a short design note: the problem, the proposal, alternatives
considered, the compatibility impact, and the migration path.

- the `Backend` trait or its event/command surface,
- a theme token (add or remove),
- the persisted data model (archive schema, settings format),
- the settings registry (a new Settings row must register its title
  and keywords in both locales with the form definition).

Everything else is a normal PR.

## 7. Commits, branches, releases

### Commit messages

Every commit uses Conventional Commits, no exceptions:

```
type(scope): subject
```

- Types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`,
  `build`, `ci`, `chore`.
- Scope: the crate or area (`core`, `app`, `backend`, `store`,
  `features`, `ci`, `docs`). Optional for small changes.
- Subject: imperative, lowercase, no trailing period, about 72
  characters at most. Add a body when the reason is not obvious from
  the subject.
- Examples: `feat(backend): wire the chat-actions, group and media surface`,
  `fix(backend): queue supervisor commands instead of dropping them`,
  `feat(app): motion settings, micro-animations and localization`,
  `docs: fix contributor guide references and changelog structure`.
- Check your subjects before pushing with
  `scripts/check-commits.sh origin/nightly..HEAD`. All workflows are
  manual-only, so no CI job runs this check; the maintainer verifies
  commit subjects again at review.

### Branches and releases

- `nightly` is the beta branch. All new features and fixes land here
  first. Branch short-lived feature branches off `nightly`
  (`your-name/short-feature`) and open a pull request back to
  `nightly`.
- `main` is the stable branch. It receives no direct feature work and
  no direct pushes. When `nightly` is ready to promote, open a pull
  request from `nightly` to `main`; it merges only after review and
  approval by the maintainer.
- Stable releases are cut from tags (`v1.0.0`) on `main`: create the
  tag, then run the Release workflow manually from the Actions tab
  with that tag as input. It builds all three platforms and publishes
  checksums.
- Nightly builds are produced by a manually run workflow from the
  `nightly` branch as a rolling prerelease.
- SemVer. Release notes come from the matching CHANGELOG.md section:
  short, a version table, changes grouped as feat/fix/chore in user
  language.

## 8. Running the app and test tools

```sh
zaprust                 # the real client: link an account (QR or pairing code)
zaprust --demo          # offline demo mode with sample data
zaprust --debug         # debug banner, inspector, structured logs
zaprust --seed blue     # theme seed preset (or a hex color)
```

Demo mode never touches the network. Live mode stores its session
and archive under the OS data directory (the exact paths are listed
in Settings > Data & Storage).

For device tests there is a headless pairing harness in the backend
crate. It requests a pairing code for a phone number and prints the
library's own log lines, which makes connection problems readable
without the GUI:

```sh
cargo run --release -p backend --example pair_harness -- <phone, international format, digits only>
```

Enter the printed code on the phone under WhatsApp > Settings >
Linked devices within about three minutes; the harness reports the
connection events as they arrive.

## Translations

User-facing strings live in locale files; see `docs/I18N.md` for how to add or extend a language.
