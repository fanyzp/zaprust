# Changelog

All notable changes to Zaprust are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com), versions
follow [SemVer](https://semver.org).

## [1.0.2] - 2026-11-28

Maturation release. The window, the composer, and the message menus
grow up: a custom titlebar, working clipboard and text selection,
grouped context menus with a confirmation step for deletion, and a
long list of correctness fixes in privacy, media handling, and the
archive, each found by adversarial review and pinned by tests.

### Window and navigation

- Custom titlebar on every screen, with drag, double-click to
  maximize, minimize, maximize, and close controls, and resize
  zones on the frameless window.
- Narrow windows now behave like a phone: below the split threshold
  the app shows one pane, the chat list or the conversation with a
  way back, instead of a conversation hidden in a pane that is not
  shown. At widths with room, the list and the conversation always
  divide the screen.
- Channels no longer appear in the regular chat list; they live
  under the Channels view only, and channel unread badges no longer
  inflate the app total. Opening a channel fetches its real
  follower count instead of asserting zero.
- Community avatars load in the Communities list and on the
  community home cover.
- The Starred panel is memoized and virtualized; scrolling it no
  longer lags behind the main chat list.
- Long names in the info dock scroll inside their margins instead
  of colliding with the panel edges, and the self chat shows your
  own name with the (You) suffix in the dock as it does elsewhere.

### Composer and messages

- Image paste into the composer works, and text copy and paste
  work across applications (the OS clipboard is now connected).
- Message text is selectable: double-click selects a word,
  triple-click selects the whole message. Keyboard shortcuts
  Ctrl+N (new chat), Ctrl+K (chat search), and Ctrl+, (Settings)
  join the existing set.
- The microphone button works in the empty composer state, which
  is the state where recording starts.
- Context menus group their variants: Pin offers 24 hours, 7
  days, or 30 days in a submenu, and Delete offers for everyone
  and for me in a submenu. Both delete forms now end in a
  confirmation dialog instead of executing immediately. Escape
  closes one menu level at a time.
- Quick replies follow WhatsApp Business behavior: typing "/" in
  the composer opens the quick-reply chooser at the caret,
  choosing inserts the reply in place of the fragment, and the
  surrounding draft and caret survive.
- Template messages render with their body, footer, and buttons;
  carousel templates render as carousels. A message this client
  cannot render appears as an explicit "message not supported"
  bubble in the conversation, never as an empty message.
- Stickers you send are filed as stickers, not as photos. Your
  favorite and recent stickers sync from the account's app state
  and appear in the picker.
- Mentions in groups render the person's name instead of a masked
  number fragment.
- The pending placeholder no longer shows beside the delivery
  ticks of a settled message, and the ticks themselves are larger.
- Conversation scrolling no longer jumps upward mid-gesture.
- A chat presented at its newest messages now counts as read on
  every entry path, so read chats stop keeping their unread
  badges, including after reopening the app.

### Calls

- The call window floats over the main window and can be dragged,
  so the chat stays usable during a call. It expands to an in-app
  stage at the true aspect and shrinks back; call controls are
  keyboard reachable with visible focus and accessibility labels.
- The call-ended card is a real summary: duration, start time,
  call type, and outcome, with Call back and Open chat actions.

### Appearance and settings

- The accent picker is gone. Appearance belongs to themes: every
  theme carries its own complete palette, generated from one
  canonical seed, and the raised surfaces carry a light emboss
  treatment with a wider surface ladder (the owner's pick from
  five measured candidates).
- The built-in dark theme is re-based on a soft dark gray floor
  instead of near black, with text steps raised so every generated
  theme still meets WCAG AA contrast.
- Settings no longer carries switches for built-in features (chat
  export, stickers, GIF search are simply available). The Features
  page keeps genuine configuration: GIF content rating and API
  keys.
- "Keep Zaprust in the system tray" is on by default, as a feature
  people rely on should be. A settings file written by an older
  version that never recorded a real choice is migrated once, so
  the new default reaches upgraders too.
- The verified badge follows the server-asserted verification
  tier only. A WhatsApp Business account is not by itself
  verified, and a later lower assertion retracts the badge.

### Archive and privacy correctness

- Disappearing messages that expire outside the visible window
  are now deleted from the archive itself on schedule, not left
  readable in Starred and search. Kept messages are exempt.
- Deleting a message now removes its downloaded and cached media
  files with it on every path: expiry, delete for me, clear chat,
  and revoke (delete for everyone), including messages outside
  the visible window and messages revoked after a restart. A
  revoke the server rejects keeps its files, matching the
  restored message.
- A message's star, pin, and keep state can no longer be lost
  when history arrives in large bursts, when a synced change
  lands on a message outside the visible window, or when a pin
  is reported for a message the session has not loaded yet.

## [1.0.1] - 2026-11-27

Performance and privacy release. The first-link sync is an order of
magnitude lighter on large accounts, sending can no longer duplicate
or silently lose a message, and read receipts, status privacy, and
the other privacy settings now follow WhatsApp's own rules.

### Sync and performance

- First-link sync at owner scale (1,200 chats, 60,000 messages,
  5,000 receipts, 500 contact namings, replayed through the in-tree
  harness): total apply time drops from 78.7 seconds to about 6
  seconds, and the receipts and namings phase from 73.3 seconds to
  under a second. Contact naming is now indexed and batched instead
  of one unindexed archive scan per naming, receipt bursts are
  processed in chunks, and the per-frame bookkeeping runs inside
  the frame budget. The worst single frame drops from 1,074 ms to
  under 100 ms, so the window no longer goes unresponsive while
  history downloads.
- When the phone's reported progress reaches 99 percent, the sync
  row now says it is finishing while the remaining phases apply,
  instead of sitting on a bare 99 percent.
- Startup applies the stored archive in budgeted slices, chat list
  first, instead of one wholesale replacement of the in-memory
  mirror.
- The app stays light after sync: avatar images decode only for
  the rows on screen and are kept in a least-recently-used texture
  cache, roster counts and badges are computed in one pass, and
  the thumbnail and avatar caches are byte-budgeted. The in-memory
  mirror keeps the newest 1,500 messages per chat; older history
  pages in from the archive on demand, and export, search, and
  profile media counts still cover the full archived history.

### Sending

- Every message now gets its final ID at compose time. A send that
  times out is attributed to the exact message, and retrying reuses
  the same ID, so a retry can never deliver a duplicate. Sending
  and Failed states survive restarts.
- Sends run in per-chat ordered queues, so one chat's slow send no
  longer blocks the others. A failed send shows a localized notice
  and restores the text to the composer as a draft, and failed
  text, location, and contact messages offer Resend.
- Outgoing messages now generate their link previews on send,
  fetched with every hop pinned to validated addresses.

### Privacy and status

- Read receipts follow WhatsApp's rules: turning them off
  downgrades direct-chat receipts to read-self, so your own other
  devices still sync read state, instead of suppressing receipts
  entirely; group receipts are always sent, with the group sender;
  voice notes send played receipts.
- Status audiences are drawn from your saved contacts. The My
  contacts except and Only share with lists keep following a
  contact when they change number, whichever form the list was
  recorded in. Status view receipts are sent to authors and
  respect the read-receipts setting, and Updates gains a Muted
  section for muted authors.
- The exception lists for last seen, profile photo, and group
  adds are editable. A contact's About is fetched and shown as
  its own profile row. The link-previews setting now controls
  outgoing preview generation.

### Interface

- The composer takes focus when a chat opens, ready for typing.
- Your chat with yourself is shown as your own name with a (You)
  suffix in the chat list, the conversation header, search, and
  the forward picker.
- Chats titled with a formatted phone number now pick up the
  contact's name once it is learned, on every surface, instead of
  keeping the number for the rest of the session.

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
