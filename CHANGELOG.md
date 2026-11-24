# Changelog

All notable changes to Zaprust are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com), versions
follow [SemVer](https://semver.org).

## [1.6.16] - 2026-11-24

### feat:

- Status is complete. The Updates screen now carries a real feed: other people's text, photo and video statuses arrive and read in a viewer, grouped into Recent and Viewed, and expire after 24 hours like they should. Posting grows past text: the composer also sends photo and video statuses (a video shows its poster frame before it goes out). In the viewer you can reply to a status (the reply lands in the author's DM as a quoted message) and react to it, and your own statuses can be deleted from the same viewer, behind a confirmation.
- Demo mode rehearses media end to end: the fake backend now serves real video bytes and answers downloads with fixture files, so playback and saving work in test mode instead of stopping at an honest error.

### fix:

- Message text wraps at word boundaries again. A long word no longer splits across lines in the middle ("inst / all"); only a token longer than the whole bubble (a link, a pasted hash) still breaks, and it breaks inside the bubble instead of overflowing it.
- Settings > Appearance: the Dynamic Color card shows one color name at a time. Hovering the dots previews the hovered name in the card's own label; the old hover pill that duplicated the name and spilled past the card's bottom edge is gone.
- Backend errors reach the screen as plain category sentences (a timeout says it timed out, a refused sign-in says so) instead of raw library wording, which could change under you and leak internals. The full error still goes to the log.
- Call mute and speaker toggles only flip back when the call-audio change itself fails; an unrelated backend error can no longer flap a toggle.
- Message info history no longer empties itself: the receipt log prunes its oldest entries past its cap instead of clearing everything.
- Horizontally scrolling chip rows (chat filters, call filters) show a chevron at an edge that has more chips scrolled out of view, instead of a chip simply cut off at the panel edge.
- Spacing inside screens sits on one shared scale kept in the theme, so the same gap is the same number everywhere it is used.

## [1.6.15] - 2026-11-24

### fix:

- Video calls are refused honestly. Starting one fails at once with a clear notice instead of ringing into nothing; accepting an incoming video call connects as a voice call and says so; the call picker offers voice only.
- In-call mute and speaker toggles now confirm against the backend, and a failed toggle flips back instead of lying about the call state.
- Community pictures load through the community lookup, so a community's own photo shows instead of a letter tile.
- Broadcast lists are their own kind of chat: they no longer wear a raw identifier as their name or borrow another chat's identity.
- Contact cards read real-world vCards: tel: links and parameter-heavy phone lines parse to the number, and a card without a display name falls back to the name inside the vCard.
- Group creation, poll creation, contact sending and forwarding show a working state until the server answers, and creating a group reports its success when the group actually arrives.
- Clearing a chat with "keep starred" now also keeps messages starred during this session; before, the archive check could miss them.
- The chat archive can no longer rewind: an older sync cannot overwrite a newer preview, timestamp or unread count, and stale bookkeeping (aged missing-photo records, unbounded call history, per-message leftovers) is pruned.
- Expiring messages follow a guarded clock: a system clock jump during a session can no longer make messages vanish early or linger far past their time.
- The updater verifies the downloaded binary by size and SHA-256 against the signed SHA256SUMS before swapping it in, and on Linux the file an update may overwrite must prove it is this app: an APPIMAGE target is accepted only when it is an ELF carrying the AppImage type-2 marker or the running executable itself. Wrapped signatures verify, a failed daily check no longer suppresses the next one, and the session key file's permissions are re-asserted on every load.
- Payment requests, invoices, scheduled calls, sticker packs, music shares and other rare message kinds now show a truthful label instead of a generic placeholder, and protocol bookkeeping payloads no longer surface as messages.
- Long names and captions truncate on whole characters in every script, including emoji and combining marks; right-to-left messages (Hebrew, Arabic) align to the right.
- A fresh account sees "No chats yet" instead of "No chats match"; the member list caps at 200 with an honest "and N more"; a group member-add box clears only when the add actually lands.
- Favorites in Settings say plainly that they are stored on this device only, the account menu names the live connection honestly, the profile panel's Search opens the in-chat search, and two dozen leftover hard-coded labels (message info, poll creation, profile fields, sticker packs, day separators and more) now follow the chosen language.

### feat:

- The design system is finished: every panel, card and frame sits on the canonical corner radii, every text size sits on the canonical scale, and icons sit on the canonical grid. The conversation header menu uses the same shared menu rows as every other menu.

## [1.6.14] - 2026-11-24

### fix:

- Read ticks now tell the truth. A message's delivery state (sent, delivered, read) is real data end to end: it arrives from history sync and live receipts, is saved in the archive, and never moves backwards when a message is delivered again by a later sync. One group member reading a message no longer turns its ticks blue for the whole group. A chat you have read stays read after a restart.
- The photo viewer shows photos at their real shape instead of stretching them into a fixed frame.
- Communities are detected from the real group structure: a community and its groups now appear because the server says they belong together, not because of a name guess.
- The Starred panel shows every starred message from every chat, read from the archive, not only the stars of the chat you happen to have open.
- The syncing indicator is honest: an interrupted sync no longer reports itself as finished, and the indicator no longer disappears during the long quiet gaps a big account's sync legitimately has.
- Quick Replies is removed, at the owner's request: the chip strip above the composer, its editor, and its settings are gone.

### feat:

- The interface adopts the owner's design system: no borders on cards, menus or panels, depth from surface tones alone, one thin shadow reserved for floating surfaces, panel corners at the canonical radius, text on the canonical size scale, and theme changes crossfade instead of snapping. The Liquid Glass (Glow) appearance option is retired; floating surfaces are solid now.

## [1.6.13] - 2026-11-24

### feat:

- Videos show their real poster frame and keep their recorded aspect ratio. The poster is fetched from the sender's hosted copy, like a photo.
- Audio files arrive as audio files, with their format and length, not as voice notes. Only recordings made with the microphone button are voice notes.
- Documents carry their real file name, size and page count. Tiny files report their size in bytes instead of rounding up to a kilobyte, and a document saved before this version can still be downloaded because its download reference is kept in the archive.
- GIFs play as GIFs and carry a GIF badge, animated stickers say so on their tile, and a sticker that fails to load offers a retry like photos and videos do.
- Verified business names badge the chats they belong to, in the chat list and the conversation header.
- Location messages show their map thumbnail and open the coordinates in your maps app when clicked. Link previews and documents show their thumbnails too.
- Indonesian is now a complete translation, and pairing failures explain themselves in plain words: a refused code, a wait imposed by the server, or a feature the server does not offer.

### fix:

- View-once voice notes, stickers and documents are spent when you actually view them, using the flags each kind carries on the wire.
- Mentions are recognised in every message kind that can carry them, and an unresolved mention no longer prints a raw internal identifier.
- Group sender colours and avatar photos follow the sender's identity, not their display name, so two people who share a name no longer share a colour.
- The video cache is capped and is cleaned up together with a chat's other media when messages or chats are deleted.
- A chat with a damaged saved message loads everything else and tells you which message could not be read, instead of silently coming back short. A damaged message payload now fails loudly instead of being overwritten with an empty one.
- Poll results, added poll options, scheduled calls, sticker packs and music shares show their contents instead of a bare type label.

## [1.6.12] - 2026-11-24

### fix:

- Incoming direct messages from history sync are no longer shown as your own outgoing messages. A 1:1 history payload names no participant, and the author now defaults to the conversation's other party.
- A saved contact's name now replaces the phone number as the chat title. A formatted number no longer counts as a resolved title, so the name healing pass keeps working until a real name arrives, and group sender names and quote authors resolve through the same session identity book.
- Profile photos for contacts whose identity is still in the hidden LID form are retried instead of being cached as missing for a day, and a chat that only has a number for a title shows the neutral person silhouette instead of a "+" initial.
- Message lists and fenced code blocks render as lists and code. List markers become bullets or numbers and fences are consumed, with inline formatting intact.
- A group join that waits for an admin's approval is reported as a pending request, not as an error.
- An unrelated backend error no longer marks an in-flight send as failed; only a send failure does.
- A GIF search failure and an export worker failure are reported as errors, and a chat archive that fails to open is surfaced as an error state instead of a footnote.

## [1.6.11] - 2026-11-23

### fix:

- Actions that change state on the server now tell the truth. A failed poll vote, edit, revoke, block, mute, pin, archive, group info save, group leave, or disappearing timer change restores the previous state and reports the failure; clearing or deleting a chat applies only once the server confirms it.
- The contact info panel's Report button opens the real spam report flow; it previously blocked the contact instead.
- Status posts no longer appear in the chat list as a fake conversation.
- A timed mute keeps its end time when chats sync; it could silently become a permanent mute before.
- Voice notes render the waveform the sender's client recorded; a note whose payload carries none shows a neutral flat form.
- Deleting a sticker pack asks for confirmation first and reports the result.
- Clear chat and Delete chat wear the danger colour in every menu.

## [1.6.10] - 2026-11-23

### feat:

- Search messages across all chats, search within a chat, and jump to a date.
- A per-chat gallery collects the chat's media, links, and documents.
- Crop and rotate a photo before sending it.
- Export a chat as a ZIP with its media and a manifest of what could not be included.

### fix:

- Video tiles take the poster's real shape instead of a stretched smear.
- Chat list previews flatten to a single honest line, with the media label shown once.
- Channels and groups resolve their real titles instead of showing raw identifiers.
- Many more message kinds (video notes, albums, events, invites, structured bot messages) render as themselves instead of a generic bubble.

## [1.6.9] - 2026-11-23

### feat:

- Delete a message only for yourself, clear a chat (optionally keeping its starred messages), or delete a chat; each asks for confirmation first.
- Mark a chat as unread, and keep a message in a disappearing chat.
- Pin a chat for 24 hours, 7 days, or 30 days; mute for 8 hours, one week, or always.
- Group admins can delete other members' messages.
- Send a contact card, and report a message as spam from its menu.

## [1.6.8] - 2026-11-23

### feat:

- Older messages load on demand beyond the first sync.
- Set or remove a group's picture.
- Start a chat by phone number, checked against the directory first.

### fix:

- Starred messages import correctly on a fresh link.

## [1.6.7] - 2026-11-23

### feat:

- Profile photos for contacts, groups, and channels, kept current.
- Mentions in group messages render as names.
- The phone's call history appears in Calls.

### fix:

- Group and chat titles heal instead of showing raw numbers after linking.
- Photo thumbnails keep their real shape, and stickers stay compact in the stream.

## [1.0.0] - 2026-11-15

| Version | Date | Platforms |
| --- | --- | --- |
| 1.0.0 | 2026-11-15 | Linux (x86_64, ARM64), Windows (x86_64, ARM64), macOS (Apple Silicon) |

First release under the Zaprust name.

### feat:

- Chats, groups, and communities, with voice notes (playback speed and seek), polls, reactions, replies, and forwards.
- Native extras built in: quick replies and chat export.
- Themes generated from a single seed color, in dark and light, with a Liquid Glass floating layer.
- Voice calls (1:1), desktop notifications, and a built-in updater that checks GitHub Releases.

### chore:

- First public source release: modular workspace (client crates plus a separate UI crate), manual-only CI, and a fresh history.
