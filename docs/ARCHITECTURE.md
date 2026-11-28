# Zaprust, Architecture

This document describes the system **as it actually exists**: a native
Rust WhatsApp client with a real backend (whatsapp-rust, pinned), an
app-owned archive, native features instead of a plugin system,
and an opt-in demo mode (live mode is the
default). Where something is not implemented, this document says so.
Per-feature status lives in `docs/FEATURE-MATRIX.md`.

## Workspace layout

```
zaprust/
├─ crates/
│  ├─ core/       Pure logic, no GUI: model, theme tokens,
│  │              credentials (key pools), crash reports,
│  │              rich-message reducers, update logic,
│  │              debug redaction.
│  ├─ backend/    The Backend trait + event/command protocol,
│  │              WhatsappBackend (whatsapp-rust, pinned), a
│  │              deterministic FakeBackend, QR matrix encoding,
│  │              audio, and the panic-containing supervisor.
│  ├─ store/      The app's own chat archive: rusqlite (bundled
│  │              SQLite), PRAGMA user_version migrations.
│  ├─ features/   One crate per native feature (export,
│  │              stickers, giphy).
│  │              Compiled in, switched
│  │              in Settings; there is no plugin system.
│  └─ app/        The egui binary (bin name: zaprust). Screens,
│                  live session wiring, demo data. Conversation,
│                  rich-message surfaces, the attach flow
│                  (attach.rs, including clipboard paste) and
│                  the voice recorder (recorder.rs) are split
│                  into modules; Settings is a root category
│                  list that opens one sub page per category
│                  (settings/, one module per category).
├─ themes/        Custom theme template (JSON, 18-token system)
└─ docs/          This file, the feature matrix, the i18n guide
```

Data flow (live mode):

```
phone ⇄ whatsapp-rust ⇄ WhatsappBackend ──events──► app ──► archive (store)
                              ▲                  commands│      ▲ write-through
                              └────── supervisor thread ◄┘      │
                                                  UI state ◄──────┘ load-on-start
```

## Layering rules

- **Core never depends on the app, egui, or the backend.** Pure
  logic, fully tested headlessly.
- **The backend speaks Zaprust types only.** No whatsapp-rust
  type crosses into the app; mapping lives in
  `backend::whatsapp`. The trait + FakeBackend let tests and
  screenshots exercise live mode without a phone.
- **Features are native crates, not plugins.** A feature's logic
  lives in its own crate under `crates/features/`; the app crate
  wires it into the UI and the live session. The features are
  built in and always available; Settings holds their
  configuration (GIF preferences, API keys), not switches.
  There is deliberately **no bulk-send capability** anywhere.
- **Themes are named, not picked by color.** The built-in Dark
  and Light schemes are fixed palettes generated from one
  canonical seed constant plus the mode; named presets pin
  their own token tables. There is no user-selectable seed or
  accent: a theme's accent is part of that theme's palette. A
  custom JSON theme can override any subset of the same 18
  tokens.

## Backend & supervisor

`spawn_backend(factory, config, wake)` runs the backend on its
own thread (Tokio runtime inside WhatsappBackend). The
supervisor contains worker panics: a panic becomes
`Event::Crashed`, the worker is restarted **once**; a second
failure surfaces a persistent error state. The app never dies
because the backend did.

Session state is the library's `SqliteStore` under
`<data dir>/accounts/<account>/`, protocol state only. The
**archive** (chats/messages/flags)
is the app's own SQLite (`store`), written through
on every upsert and loaded at start.

### Encryption at rest

Both databases are encrypted with SQLCipher under one random
32-byte key: the archive directly, and the library session store
through the connection init hook the library provides for
keying. The key is generated on first run and held primarily in
an owner-only `db.key` file in the data directory; the OS
keyring (Secret Service, Keychain, or Credential Manager) is a
mirror and a recovery source, never the only copy, so a linked
session survives any keyring failure mode. The file protects a
database copied on its own but not a copy of the whole
directory. Databases written
by older plaintext builds are converted in place on first open.
Permissions stay enforced as a second layer: on Unix the data
directory is set to 0700 and the database and secrets files to
0600 at startup. Credentials (API keys) are **not** in either
database; they live in the OS keyring / 0600 secrets file.
Moving to a new machine means moving the key too: export it from
the old machine's keyring, or the databases cannot be opened.

## Rich messages

Polls (create + vote own/demo; voting on others' polls works
when the poll was created or received this session, because
the creation secret travels with the creation message; older
polls answer with an honest error),
reactions, quotes, edits/revokes (local state; live send paths
limited by the library), link previews (rendered only from
the preview the sender's client attached; never fetched, so
a planted link cannot leak the recipient's address or
timing), location/contact cards, and
carousel **rendering** (receive-only; the pinned library has
no public send construction) are mapped in the backend and
rendered natively. The support table in
`docs/FEATURE-MATRIX.md` is the source of truth and matches
the code.

## Identity flags (verified, Meta AI)

The chat model carries `verified` and `meta_ai` flags. The pinned
protocol library does not surface a verified flag, so in live mode
`verified` is always false today; demo data carries examples
(channels) and the UI draws the badge wherever the flag is set.
No address is hardcoded for Meta AI: a live chat is treated as the
Meta AI contact when its title is exactly "Meta AI" (the library
exposes no dedicated AI-contact marker), and it then renders with the
AI avatar and an "AI" chip while behaving as a normal DM. That
heuristic is deliberate and documented here because it can, in
principle, misfire on a contact literally named "Meta AI".

## Privacy wiring

Read receipts are **sent only after real engagement** (scrolling
to the bottom, focusing or typing in the composer, or sending),
and only when the Privacy toggle allows them; typing
indicators likewise are only sent when enabled, and turning
them off retracts a composing state already sent. Privacy values
the library exposes (last seen, online, profile, status,
group-add, read receipts) are fetched on connect and set via
`set_privacy_setting`. The fetch also carries categories the
Privacy page has no row for (who can add the account to calls,
who can message it, defense mode); those are kept in the app's
state map rather than discarded, but they are edited on the
phone, and the UI shows no fake control for them. The
account-wide default disappearing timer has no fetch at all:
the server pushes changes made on other devices as a sync
notification, and the app adopts those (server wins), the same
rule as the read-receipts toggle.

Presence announcements are the library's session behavior, not
an app setting: the client announces `available` automatically
on connect and reconnect (the library's `PresencePolicy`
default), the same posture as WhatsApp Web. Who can *see* that
presence is the account's Online privacy setting above. The
library also offers a manual policy under which the host app
owns every presence transition; Zaprust does not take that
over, because going manual without owning the transitions
would silently change when contacts see the account online.

## Network egress and proxies

Besides the WhatsApp connection itself, the app makes two
kinds of HTTP requests: update checks and downloads (ureq),
and GIPHY API and media requests (reqwest, only with a
user-supplied API key). Their proxy behavior differs, and the
difference is stated plainly rather than smoothed over: the
update traffic honors the standard proxy environment
variables (`ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY`,
`NO_PROXY`, ureq's default), while GIPHY traffic and the
WhatsApp connection always connect directly. There is no
in-app proxy or Tor setting, so an environment proxy does
not cover the WhatsApp connection.

## Crash safety

- A global panic hook (installed first in `main`) writes a
  structured, redacted crash report (JSON) to
  `<data dir>/crashes/`, never message bodies, credentials, or
  phone numbers; home paths are reduced to `~`.
- On the next start, an unseen report produces the "crashed
  last time" banner with [Report on GitHub] [View details]
  [Dismiss]. The issue flow uses one repo constant
  (`core::crash::GITHUB_REPO` = `fanyzp/zaprust`, the official
  public repo) and the `open` crate with a clipboard fallback.
- The workspace keeps `panic = "unwind"` (never `abort`) so
  all of the above can work.

## Self-update integrity

The updater (`core::update` for the logic, `app::update` for
the worker) downloads only over HTTPS, and only from
github.com, api.github.com, or a githubusercontent.com host;
any other host in a release payload is rejected. Downloads are
streamed with a size cap (the release's declared asset size,
with a fixed fallback) and the checksum file is size-capped
too. The SHA-256 check against the release's SHA256SUMS is
mandatory and runs before anything is swapped; a mismatch
aborts with the old binary intact. The checksum file itself is
authenticated separately: every release carries an Ed25519
signature over SHA256SUMS (SHA256SUMS.sig), and the app pins
the release signing public key and refuses a release whose
signature does not verify, so a substituted checksum file
cannot bless a substituted binary.
On Linux, an AppImage path from the environment is
only treated as the install target when it exists, is a
file, and proves it is this app's own AppImage: an ELF
carrying the AppImage type-2 marker, or the running
executable itself. Anything else falls back to the
executable path, so a stale or hostile variable cannot
decide which file an update overwrites. The downloaded
binary itself is verified by size and SHA-256 against
the signed SHA256SUMS (above), never by an identity
check.

## Errors

`core` and `store` errors are hand-rolled enums with human
Display text (no error-framework dependency). The updater's
failures are a typed kind plus a human message
end to end, and at the app's
display boundary, backend error strings are mapped to short
human sentences (the raw text goes to the log, never verbatim
into a banner). Free-form strings remain only where they cross
the backend event boundary, whose ~70-variant protocol event
surface is the next typing target.

## Local persistence

JSON state files (settings, feature settings, credentials)
are written
atomically: temp file in the same directory, sync, rename. A
state file that exists but does not parse is quarantined to a
`.corrupt-<timestamp>` sibling with a log line, instead of
being silently replaced by defaults. Flag changes on
stored messages (star, pin, revoke) use a targeted store
update that touches only the flags column; rebuilding a stored
row from the UI model is banned (it corrupted rows in the
past).

## Rename adoption (1.0.0)

The app shipped as Zaplite before 1.0.0. Three one-time
adoption shims keep an existing install whole across the
rename, each a copy or read-through, never a move:

- `backend::data_dir` copies the legacy data directory into
  the Zaprust one on first run when the new one does not
  exist yet.
- The keyring backend adopts entries saved under the legacy
  service name on first read and writes them through under
  the current service.
- Credential environment overrides accept the pre-rename
  `ZAPLITE_CRED_<SLOT>` names as a fallback.

## Debug mode

`--debug` (off by default) adds the pinned DEBUG banner, the
inspector screen, and `tracing` spans to rotating logs under
`<data dir>/debug/`. Redaction (`core::debug`) runs at the
trace choke point and again at bundle export; gate decisions
are provably identical with debug on or off.

## Verification

Gates: `cargo fmt --all` clean, `cargo clippy --workspace
--all-targets -- -D warnings`, `cargo test --workspace`, and a
release build, on Linux locally; Windows/macOS are covered by the
CI matrix (build + test). See CONTRIBUTING.md.

Feature support status: see `docs/FEATURE-MATRIX.md`.
