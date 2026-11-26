# Changelog

All notable changes to Zaprust are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com), versions
follow [SemVer](https://semver.org).

## [1.0.0-beta] - 2026-11-27

First public beta. Zaprust is a native WhatsApp client written in Rust:
one binary, no browser, no Electron, linked to your phone as a companion
device.

### Sync and reliability

- Sync is dramatically lighter at first link and on reconnect: chat,
  message, and delivery writes are batched into one archive transaction
  per burst instead of one per event. In a synthetic first-link replay
  (1,000 chats, 20,000 messages), archive write commits drop from 4,253
  to 285 and total sync time from 5.1s to 4.5s.
- Read state now syncs both ways: reading a chat on Zaprust marks it
  read on your phone and other linked devices too, and unread badges no
  longer double-count messages that arrived in the initial history sync.
- Contact names keep healing during a session: when a name or number is
  learned for a contact mid-session, their chats, message labels, and
  previews update right away. A contact whose number is genuinely
  unknown now shows a masked identifier instead of the raw ID.
- The chat list sync row now shows real progress: the percentage
  reported by the sync itself while history downloads, an indeterminate
  state for phases that report no number, and a brief "Sync complete
  100%" only when the sync truly finishes.
- Sending a message can no longer duplicate it: the local completeness
  index stays truthful when a sent message is bound to its server ID,
  and every path that appends a message now records it in the index.
- If the startup archive load fails or times out, the archive store is
  reopened for writes instead of silently skipping them for the rest of
  the session.

### Interface

- App icon: Zaprust now ships the dark plate icon family, a dark
  rounded plate carrying the white mark, at every size for the window,
  taskbar and desktop installs, together with a white twin of the mark
  for dark surfaces inside the app. The About dialog and other in-app
  brand spots draw the mark through one shared adaptive brand texture.
- Added named theme presets (Nord, Monokai, Gray, WhatsApp) alongside
  the generated schemes, each pinned to the same WCAG AA contrast
  floors. Picking a preset activates its token table; picking a seed
  returns to the seed system, and the active preset persists.
- Pill family unified: count badges share one component (19 px), filter
  chips and label tags share one chip size (24 px), selected filter
  chips rest on a solid accent fill, and label tag text clears the
  WCAG AA floor on its tag fill for every label color.
- Layered bubbles: every message bubble now carries exactly one inner
  card level, with carousel cards, interactive chips, list rows and
  download chips deriving their fill from the bubble itself and no
  border strokes on inner cards.
- Voice notes: the waveform now renders as 4 px block bars and the play
  button is a neutral inner-card disc with an accent glyph.
- View-once messages: photo and video view-once rows now show a 52 px
  blurred thumbnail beside the label, with a same-size placeholder
  block when no thumbnail is available yet.
- Documents: document messages show a 96 px first-page preview banner
  above the file row whenever the message carries a preview.
- Link previews: link embeds now follow the two-form rule, a full-width
  image band for wide images and a big left thumbnail with the text at
  its right for small or square images, both inside the single inner
  card.
- Shared list rows: the chat list, search results, starred, calls,
  updates and community rows now paint through one list-row component
  with one opaque step wash, and the status rows sit on the shared row
  grid. Pin and Mute entries now appear in the chat-row context menu.
- The media viewer now uses the app's shared buttons for Close,
  Previous, Next and Download, wraps long captions through the same
  markdown rendering as message captions, and adds a clickable
  filmstrip, arrow-key navigation, and a sender, date and size row.
- The emoji, sticker and GIF picker now keeps one uniform row of
  shared chip tabs along the bottom for every tab, and the emoji
  search is the shared search pill at the top of the picker.
- The composer's reply and edit bars now sit on one inner card in the
  composer's own inner-card fill, each with a sender color bar on its
  edge in the quoted or edited sender's color.
- The chat info dock now uses the standard 15px row grammar throughout
  and gains an encryption row stating that messages are end-to-end
  encrypted.
- Dialogs and menus across the app now share one popup frame with an
  icon close button, the notice banner is a full-width strip on the
  window's top edge, and result notices appear as a toast at the
  bottom-right.
- The message context menu, conversation header tooltips, storage
  page, My Profile dialog and dock media counts no longer bypass the
  locale system; every user-visible string ships in English and
  Indonesian.

### Startup and performance

- Restoring a stored session now opens on a plain spinner instead of
  the pairing screen, yields the moment a pairing QR or code appears
  or the connection drops, and falls back to the connect flow if no
  outcome arrives within 30 seconds.
- Startup no longer waits for the full chat archive to load: the first
  paint and the link flow start immediately while the archive loads in
  the background.
- The system color emoji font now loads lazily after the first frames
  instead of blocking startup.
- The sync row's once-a-second repaint wake now stops while the window
  is minimized or fully covered, and resumes when it is visible again.
- Scrollbars now firm up while a list is scrolling and for a moment
  after, per list, instead of only when the pointer hovers them.
- The empty conversation pane now shows the Zaprust mark above its
  title, in black on light themes and white on dark themes.
- The composer pill no longer shifts left for a few seconds after the
  window is shrunk while the message list is scrolled up; the floating
  composer area takes its width from the current panel rect on every
  frame.
- The connect screen and its pair-code flow were audited end to end:
  the wide connect card now engages only when the card plus its page
  margins fit the window, so the QR plate can no longer be pushed off
  screen at widths just above the old threshold.
