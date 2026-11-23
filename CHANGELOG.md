# Changelog

All notable changes to Zaprust are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com), versions
follow [SemVer](https://semver.org).

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
