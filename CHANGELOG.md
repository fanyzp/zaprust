# Changelog

All notable changes to Zaprust are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com), versions
follow [SemVer](https://semver.org).

## [1.6.25] - 2026-11-25

### fix:

- The spell checker no longer underlines a correctly spelled word just because it is wrapped in single quotes. The tokenizer kept the opening quote as part of the word, so a quoted word matched neither dictionary; quoted words are now checked as themselves, in both languages, and a misspelled quoted word is still underlined. Words with apostrophes inside them (don't, users') behave exactly as before.

## [1.6.24] - 2026-11-25

### fix:

- The spell checker now catches the last word of a sentence. Its address rule used to exempt any word touching a full stop, so a misspelling at the end of a sentence was never underlined. The rule now exempts only words fused into an address-like string (a domain, a file name, a path, an email, a handle); links, addresses, codes and acronyms are still never flagged, and correctly spelled sentence-final words still pass in both languages.
- The chat list filter chips no longer rest with the last chip clipped at the panel edge. The chip row now flows whole chips onto a second row when the panel is too narrow for one row, so every filter (All, Unread, Personal, Favorites, Groups, and Archived when it applies) is fully visible at any panel width, in either language, whatever the counts grow to.

## [1.6.23] - 2026-11-25

### feat:

- Interactive messages can be sent and answered. The attach menu's Interactive message builder composes three shapes: a buttons message (text, an optional footer, up to three buttons), a single-select list (title, description, sections of rows), and a carousel of up to two cards with quick-reply buttons. Received buttons and lists now render as themselves instead of flattened text: buttons are tappable chips, list rows are tappable rows, and tapping one sends the protocol's answer quoting the original. One honest limit: these payloads are assembled as raw protocol messages, and whether the service delivers them from an ordinary account, and how they render on the other side's phone, can only be proven by using them on a real account.
- Groups can share their history with new members. The group panel's Add flow gains a "Share recent messages" switch: when it is on, adding a member also sends up to 100 recent text messages from this device's archive, converted into the protocol's history bundle. The result is reported per person (added, refused and why) and for the share itself (shared, skipped with the reason, or failed); a failed share that the protocol allows retrying keeps a Retry button in the panel. Only messages this device holds, that the protocol acknowledges, can be shared.
- Member labels and reporting to admins arrive. A member's self-chosen label shows beside their name in the member list and updates live when they change it; your own label for the group is editable in the group panel. A message's menu in a group offers Report to admins (it asks first), and group admins get a Reported messages section in the panel listing what was reported, by whom, with the message text when this device holds it.
- The composer checks spelling. Misspelled words get a wavy underline as you type, checked against bundled English and Indonesian dictionaries; a word passes when either language accepts it, the names of your chats and contacts count as known words, and links, addresses, codes and acronyms are never flagged. Settings > Chats > Writing holds the switch and a personal dictionary for words of your own.
- Text can be selected across messages. Dragging over message text selects across bubbles; the selection highlights, Ctrl+C copies it (messages separated by newlines) when no input is focused, and Escape or a fresh click clears it. The selection covers message body text only, not quotes, names or card contents.

## [1.6.22] - 2026-11-25

### feat:

- Video calls work. A video call shows the other side's picture on a stage in the call card, your own camera's picture in a small tile in its corner, and a camera toggle among the call controls; a voice call can turn its camera on mid-call, and a video call can turn it off. Your camera is captured on this machine and encoded here; nothing is sent until you turn video on. If the machine has no camera, or the camera will not open, the call card says so under the status line and the call continues as a voice call.
- Group calls work. A group chat's call button starts a call for the whole group, a group call invitation rings here like a 1:1 call, and the call card lists who is in the call as they join. During a group call you can raise your hand and invite more of the group's members from the card.
- Call links and the waiting room work. The Calls panel can create a call link (voice or video) to copy and share, and can join with a link: the link is previewed first, showing its kind and who created it. When a link requires approval, guests wait in a waiting room; the host sees them on the call card and admits or denies each one, and can turn the approval requirement on or off.
- In-call reactions and screen sharing arrive. While a call is live, the card offers a reaction row; reactions from everyone show next to their name. When someone shares their screen, their picture takes the stage with a caption naming them. Two honest limits: sharing your own screen is not offered, because capturing the desktop is not built; and a participant whose client sends standard Opus audio (some desktop clients do) cannot be heard in a group call, because this build carries no Opus decoder by design.

## [1.6.21] - 2026-11-24

### feat:

- Business profiles and catalogs are viewable. A business contact's info panel gains a Business section (description, email, websites, categories, address, and opening hours per day) and a catalog view: collections as sections when the business uses them, otherwise the product list with Load more, product photos included. An order message renders as an order card (items, total, status) that opens the order's line items.
- Labels arrive, synced with your other devices. The Labels panel (from the chat list) creates, renames, and recolors labels and deletes them behind a confirmation. Chats and individual messages get labels from their menus; labeled chats show tag chips in the list, labeled messages show a tag line under the bubble, and the chat list can be filtered to one label. Changes made on your phone show up here, and changes made here show up there. Labels persist in the local archive.
- Business quick replies sync too. Settings > Business lists and edits them (deleting asks first), and typing a slash in the composer, like /hours, offers the matching replies; picking one fills the composer with its text.
- Your own business profile is editable in Settings > Business: description, email, address, websites, timezone, and opening hours. Two honest limits: categories can be kept or removed but not added, because the library this client is built on offers no list of categories to pick from; and there is no cover photo control, because the upload it needs is not implemented in that library.

## [1.6.20] - 2026-11-24

### feat:

- Chats can be locked. Locking a chat (from its row menu or the conversation header) moves it out of the chat list, the unread total, search results and notification previews, into a Locked chats folder at the top of the list. The lock state syncs with your other devices like archive and pin do. The folder itself opens with a lock code you create in this app: the code is yours alone, stored only as a salted digest on this machine, and it gates this window only (it does not encrypt the archive, which has its own key, and it never syncs anywhere).
- The whole app can lock too. Settings > Privacy & Security manages the same code: create it, change it, or remove it there, turn on App lock so the app starts behind the code, or lock on demand from the settings page or with Ctrl+Alt+L. While the app is locked nothing shows and notifications carry no names or text. Forgotten the code? The lock screen offers log out, which disconnects and clears the code with the session.
- Blocked contacts get their own page. Settings > Privacy & Security shows how many contacts are blocked and opens a list where each one can be unblocked, with a picker to block a contact from your chats. Changes confirm against the server list before they are announced, and a failure puts the row back.
- The app can live in the system tray (Linux). Turn it on in Settings > Notifications and closing the window tucks the app into the tray instead of quitting, with the unread count on the tray entry and Open and Quit in its menu. If no tray is running, closing works exactly as before.
- Keyboard shortcuts grow up, and gain a reference page. Settings > Shortcuts lists the set: Ctrl+Alt+N for a new chat, Ctrl+Shift+] and Ctrl+Shift+[ to move between chats, Ctrl+E to archive the open chat, Ctrl+Shift+M to mute it, Ctrl+Shift+U to mark it unread, Ctrl+Alt+L to lock the app, alongside the existing in-chat search and sidebar chords.
- Arabic and Hebrew text renders properly. Messages, chat names, previews and search results in those scripts now use bundled Noto fallback fonts (before, the app had no font that covered them at all, so the text painted as placeholder boxes), Arabic letters take their joined contextual forms, and right-to-left text reads in the correct order in bubbles, lists and headers. Text you are still typing in the composer keeps the editor's own layout, and stored text is unchanged: the fix is entirely in how text is painted.

## [1.6.19] - 2026-11-24

### feat:

- Events are a real surface now. An event someone shares renders as its own card in the conversation: name, date and time, place, description, a join or call link when it has one, and how many people answered Going, Maybe, or Not going. You can answer from the card itself, and bring extra guests when the organizer allows them. Creating an event lives in the attach menu (direct and group chats): name, date, start and end, place, description, link, scheduled call, and extra guests, with the form checking the essentials before anything sends and keeping your draft if the send fails. Your own events can be edited or cancelled from the message menu (cancelling asks first), and an event can be pinned like any other message. Answers and edits made on another device show up live. One honest limit, the same one polls have: an event from before this version, whose creation secret this device never captured, can be read but not answered or edited here, and the app says so instead of pretending.

### fix:

- Long conversations now render through a window: only the messages near the screen are drawn, with the rest held as measured space, so a chat with thousands of messages scrolls at full frame rate instead of slowing down as it grows. Scroll position is anchored exactly: loading older messages or a row changing height no longer makes the view jump. Chats under the window threshold render exactly as before.

## [1.6.18] - 2026-11-24

### feat:

- Communities grow their management surface. A community home now lists the community's members for everyone in it. Admins can promote or demote other admins from a member's menu, and remove a member from the whole community at once (the parent group and every linked group), behind a confirmation. The home also carries the community's invite link with copy and reset, a Manage groups section that links your other admin groups into the community and creates new subgroups, per-group unlinking behind a kebab menu and a confirmation (the group keeps its members and messages and carries on as an ordinary group), and a Deactivate community action, also confirmed, that removes the community while its groups survive as standalone groups. Transferring community ownership is not offered: the library this client is built on exposes no such call, and the docs say so rather than faking it.

### fix:

- Under the hood, the live session code moved out of a single 16,000-line file into focused modules (session lifecycle, event application, message and media handling, profile and privacy, archive extraction), and two helpers that existed in duplicate (the chat-row text shortener and the changed-chat store write) now exist once each. No behavior changes: the full gate suite passes and before/after screenshots of the chat list, a conversation, calls, and settings show no visual difference.

## [1.6.17] - 2026-11-24

### feat:

- Channels are a real surface now. The Channels entry opens its own panel: the channels you follow with their follower counts and verified badges, a find-by-invite-link lookup with a preview card and a Follow button, and a form to create a channel of your own. Opening a channel loads its history (older updates page in as you scroll up) and keeps reaction counts and poll tallies live while you watch.
- Following is one tap from the preview card; unfollowing asks first, both in the panel's row menu and in the conversation header. A channel's info (description, follower count, admin count, some follower names, the invite link, the creation date) is one click from the header or the row menu.
- Channels stay one-way, like WhatsApp: followers and visitors get a short explainer bar instead of a composer (with a Follow button when you have not joined yet), while admins and owners post text, media and polls from the same conversation. Admins can edit and delete their channel's updates from the message menu; everyone can react to updates and vote in channel polls, and a failed reaction or vote puts things back the way they were and says why.
- Channel comments and AI responses read as what they are: a comment on a channel post shows its own words (or its photo), and an AI response shows its text, instead of a generic placeholder.

### fix:

- Spacing inside screens completes its move onto the shared scale: the tight half steps between the main rungs are named steps of their own, so every gap in the app is one named number from the theme.

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
