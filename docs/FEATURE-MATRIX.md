# WhatsApp feature matrix

Status of the core WhatsApp surface in Zaprust, per feature: what
the pinned `whatsapp-rust` library supports, what the app wires in
live mode, what demo mode does, and where the tests live. Cell
values: **Done** / **Demo-only** / **Live-only** / **Not supported**
/ **Phone-managed**.

This matrix is the contract the README feature section summarizes;
if they disagree, this file is the detailed truth and the README
needs fixing.

| Feature | Library at the pin | Live mode | Demo mode | Tests |
|---|---|---|---|---|
| Send text | Done | Done | Done | fake unit `send_text_echoes_and_replies_in_the_dm`, app state |
| Default message timer (account) | Done (`set_default_disappearing_mode`) | Done: Settings > Privacy picks Off / 24 hours / 7 days / 90 days, persisted locally and sent to WhatsApp while linked | Done: the same setting seeds the timer onto groups created in demo mode | contract `disappearing_timer_sets_state_and_stamps_sent_messages` (command log), app `default_timer_seeds_groups_created_in_demo` |
| Disappearing messages (per-chat timer) | Done (`set_chat_disappearing_timer`, `Groups::set_ephemeral`, per-message ephemeral expiration, `DisappearingModeChanged` event) | Done: the info dock sets Off / 24 hours / 7 days / 90 days, the command reaches the protocol for DMs and groups, incoming changes apply with a system notice, sent and received messages carry an absolute expiry in the archive, and a per-second pass deletes expired rows | Done: the same dock setting, sends stamp the expiry, the same pass removes expired messages from the demo state | contract `disappearing_timer_sets_state_and_stamps_sent_messages`, store `v5_ephemeral_state_round_trips_and_messages_delete`, core `expiry_uses_the_supplied_clock`, app `expiry_pass_removes_only_due_messages_in_demo` |
| Quote / reply (receive) | Done | Done | Done | demo invariants; quoted context fields are mapped but no scripted quote exists in the fake yet (gap noted in the audit) |
| Quote / reply (send) | Done (`text_with_context` + ContextInfo) | Done: the composer's reply bar resolves the quoted message's protocol id and author JID and the send carries extendedText context on the wire; archived messages without a stored author JID fall back to the chat address | Done (local echo) | backend unit tests `reply_text_carries_the_quote_context`, `reply_without_an_author_falls_back_to_the_chat_jid`, `plain_text_sends_without_context` |
| Reactions add / remove | Done | Done | Done | core reducer tests, contract |
| Edit own message | Done (`edit_message`) | Done | Done | contract `edit_and_revoke_commands_emit_their_events` |
| Delete for everyone | Done (`revoke_message`) | Done | Done | contract `edit_and_revoke_commands_emit_their_events` |
| Forward message | Done (`forward_message`) | Done, session-scoped (see note) | Done (local copy) | contract `forward_command_copies_the_message_into_the_target_chat` |
| Pin message | Done (`pin_message`) | Done (7-day pin) | Done | contract `star_and_pin_message_commands_update_state_and_log` |
| Star message | Done (`star_message`) | Done | Done | contract (same test) |
| Starred messages from before linking (import) | Done (app-state full sync dispatches `StarUpdate`; history sync messages carry `WebMessageInfo.starred`) | Done: both arrival paths are applied, the app-state actions and the per-message flag the history translation carries since v1.6.8, so the Starred panel reflects the phone's set after a fresh link | Done (scripted stars in the fake's roster) | backend unit `history_sync_message_carries_its_star` |
| Fetch older messages on scroll | Done (PDO `fetch_message_history_with_id`; the answer returns as an on-demand history sync) | Done: scrolling to the top of a conversation requests the next older page from the phone (page size 50), with a loading row while the request is outstanding, an end-of-history latch, and a 45 second watchdog that fails an unanswered request honestly | Done: the fake serves two scripted older pages for the DM, then ends the history | contract `older_pages_land_in_order_and_the_last_ends_history`, app `history_fetch_state_moves_with_the_outcome_events` |
| Message info (receipt timeline) | Done (Receipt events) | Done | Done (simulated progression) | mapping code-reviewed; receipt-event conversion has no dedicated unit test (audit exception) |
| Media receive (photo/video/voice/document/sticker) | Done | Done (typed Media kind) | Done (scripted) | contract `scripted_dm_includes_photo_and_view_once_video` |
| Media download | Done (`download`) | Done, session-scoped (success path needs a real linked session; not testable without a phone) | Not supported (test mode answers with an honest error) | contract `create_group_fetch_members_and_download_honesty` |
| Media send (photo/video/voice/document) | Done (`upload` + media builders) | Done: the attach menu opens the portal file dialog (multi-select for photos and videos), files stage above the composer and send one media message per file through the backend | Done (local; demo mode sends the picked file's own pixels) | contract `send_media_commands_emit_media_upserts_for_each_kind`, `send_media_with_empty_bytes_is_an_honest_error` |
| Voice note: record and send | Done (audio upload builder) | Done: the composer mic records through cpal (48 kHz mono), encodes Opus-in-Ogg on stop and sends push-to-talk audio | Done (local voice bubble with the recorded waveform; the whole demo is test mode) | app recorder tests (`opus_head_is_well_formed`, `opus_tags_are_well_formed`, level resample) |
| Voice note: playback speed | n/a (playback is app-side) | Simulated playback with the 1x / 1.5x / 2x pill (no audio output yet: the session clock drives the progress) | Same | app `voice_playback_*` state tests |
| Media send (stickers/GIFs from the picker) | Done for stickers (proto assembled from the upload response; the library ships no sticker builder) and videos | Done: stickers converted to 512x512 WebP; GIFs sent as MP4 video with GIF playback from the GIPHY MP4 rendition | Done (demo images) | contract (same tests), app `sticker_webp` tests; live runtime check pending the owner's pairing test |
| Album view (consecutive media grouped) | n/a (presentation layer; the protocol sends individual media messages) | Done: two or more consecutive photos/videos from one sender render as one grid card (max 4 tiles + "+N"), tap a tile to open the photo | Done (demo data ships a three-photo album; the fake scripts one too) | store round-trip covers the scripted run; stream grouping is app-side |
| View-once receive | Done | Done (wrapper unwrapped, flag carried) | Done | contract (scripted view-once video), store round-trip |
| View-once send | Done in the library | Done: a single staged photo or video with the "1" toggle sends wrapped in `viewOnceMessageV2` | Done (local echo of the view-once kind) | backend unit test `view_once_wraps_photos_and_videos_only` |
| Polls: create | Done | Done | Done | contract, core reducer |
| Polls: vote on own poll | Done | Done | Done | contract `create_poll_vote_and_react_commands_flow_through_the_fake`; live: the creation secret is cached for the session (vote_raw), so own polls stay votable until restart |
| Polls: vote on others' polls | Done (`vote_raw`; the creation secret travels inside the creation message's context info) | Done for polls received during the session: the backend caches the secret on receipt (see docs/research/poll-voting.md). Polls from before the session answer with an honest error | Done (local) | backend unit tests for secret extraction; runtime check pending the owner's pairing test |
| Carousel receive / render | Done (proto oneof exists) | Done (render-only; the renderer shows at most 2 cards with up to 2 static buttons each) | Done (scripted) | contract (scripted carousel) |
| Carousel send | Not supported through the library's public send path | Not supported (no menu entry) | Not supported | documented limit |
| Link previews | n/a (sender attaches) | Sender-attached previews render; never fetched | Scripted sample attached | backend translate + app live tests |
| Location / contact cards receive | Done | Done | Done (scripted) | contract `scripted_group_delivers_location_carousel_and_contact_kinds` |
| Location send | Done (proto `locationMessage`) | Done: the attach menu opens a name + coordinates form and sends a location message | Done (local card) | backend unit test `location_message_carries_degrees_and_optional_name` |
| Status: post text | Done (`Client::status().send_text`) | Done: the Updates screen's My Status row opens a composer and posts a text status with the account's direct-chat partners as the audience and the library's default contacts privacy; the result answers as a dedicated event pair, success adopts the text as the session's status, and a failure stays inside the composer instead of pretending success | Done: demo mode keeps the post as local session state (the composer labels it as test mode); a session on the fake backend records the post and answers success | backend fake `post_text_status_is_recorded_and_answers_posted`, `post_text_status_refuses_an_empty_text_or_audience`, app updates `demo_post_updates_my_status_and_survives_screen_switches`, `live_post_failure_lands_back_in_the_composer` |
| Status: view other people's updates | Not supported (the library's status API covers posting and revoking only; it has no list or fetch for other people's statuses) | Not supported: the feed area of the Updates screen renders an honest empty state | Not supported: the same empty state, and no demo statuses are invented | documented limit |
| Paste image from clipboard | n/a (app-side) | Done: Ctrl+V stages the clipboard picture through the attach flow when the clipboard holds no text | Done (same staged path) | app `a_real_png_stages_as_a_photo` |
| Media download button | Done (`download`) | Done: document cards and received voice notes carry a download chip wired to `DownloadMedia` | Honest error (test mode has no bytes) | contract `create_group_fetch_members_and_download_honesty` |
| Create group | Done | Done | Done (local entry) | contract `create_group_fetch_members_and_download_honesty` |
| New chat by phone number | Done (`Contacts::is_on_whatsapp`) | Done: the chat list's New chat dialog checks the number, opens the person's conversation (the roster chat when either address form is already known), answers plainly inline when the number is not on WhatsApp, and shows lookup failures as errors, never as not-found | Not applicable (needs a session; the dialog says so); a session on the fake backend resolves its scripted directory | contract `lookup_resolves_the_directory_and_refuses_the_rest`, backend `phone_digits` unit test, app `contact_resolved_opens_one_chat_per_person`, `lookup_outcomes_move_only_the_dialog_that_asked` |
| Add / remove participants | Done | Done | Not applicable (demo groups are data) | contract `group_participant_ops_mutate_the_member_list` |
| Promote / demote admins | Done | Done | Not applicable | contract `group_participant_ops_mutate_the_member_list` |
| Edit group name / description | Done | Done | Not applicable | contract `group_participant_ops_mutate_the_member_list` (fake state); invalid descriptions error instead of clearing |
| Group picture (set / remove) | Done (`Groups::set_profile_picture`, `remove_profile_picture`) | Done: the group settings card's Photo block sets (file pick, square 640 JPEG preparation) and removes (confirmation modal first); a landed write invalidates and refetches through the avatar pipeline; a server refusal (for example a non-admin attempt) surfaces as an error notice in the server's terms | Not applicable (demo groups are data); a session on the fake backend stores the change and serves it back through FetchAvatar | contract `group_picture_set_and_remove_show_in_fetch_avatar`, app `group_picture_events_invalidate_and_report` |
| Leave group | Done | Done | Not applicable | contract `group_participant_ops_mutate_the_member_list` (fake archives the chat) |
| Fetch group members | Done (`fetch_metadata`) | Done | Demo names from data | contract `group_participant_ops_mutate_the_member_list`; note: the real backend returns address user parts as names, the fake returns scripted names |
| Group invite link (show / copy / reset) | Done (`get_invite_link`) | Done: the group dock fetches the link on demand, copies it, and resets it (a reset mints a new link) | Not applicable (demo groups are data) | contract `invite_link_approval_and_join_requests_flow_through_the_fake` |
| Membership approval + join requests | Done (`set_membership_approval`, `get_membership_requests`, approve / reject) | Done: the group dock toggles approval, lists pending requests, and approves or rejects each; an approval lands the requester in the member list | Not applicable (demo groups are data) | contract `invite_link_approval_and_join_requests_flow_through_the_fake` |
| Join group by invite link | Done (`join_with_invite_code`) | Done: the New group dialog links to a Join group dialog; the joined group arrives as a chat, and a group that requires approval answers with a join-request notice instead of a phantom chat | Not applicable (needs a live session; the dialog says so) | contract `invite_link_approval_and_join_requests_flow_through_the_fake` |
| Group announce / locked modes | Done (`set_announce`, `set_locked`, both read back from group metadata) | Done: two switches in the group dock set who can send messages and who can edit the group info | Not applicable (demo groups are data) | contract `invite_link_approval_and_join_requests_flow_through_the_fake` |
| Read receipts: receive states | Done | Done | Done | mapping code-reviewed (no dedicated unit test; audit exception) |
| Read receipts: send (mark as read) | Done (`mark_as_read`) | Done, governed by the Privacy toggle | Same state machine | app privacy predicate tests, fake command log |
| Typing: send | Done (chatstate) | Done, governed by the Privacy toggle | n/a | app privacy predicate tests, fake command log |
| Typing: receive | Done (`ChatPresence`) | Done (header shows "typing...") | Scripted event in test mode | mapping code-reviewed (no dedicated unit test; audit exception) |
| Mute / unmute chat | Done (chat actions) | Done (app state + backend action, persisted in the archive so it survives restarts) | Done (local state) | contract `mute_and_unpin_chat_actions_round_trip`, store `v6_mute_state_round_trips_and_defaults_off` |
| Archive / unarchive chat | Done (chat actions) | Done | Done (Archived filter) | contract `chat_action_commands_update_the_chat_row`, core filter tests |
| Pin / unpin chat | Done (chat actions) | Done | Done (pinned-first ordering) | contract `mute_and_unpin_chat_actions_round_trip`, core filter tests |
| Block / unblock contact | Done (`blocking`) | Done | Local state with notice | contract `block_contact_is_logged_by_the_fake` (log-observable) |
| Report spam | Library exposes reporting | Not supported in the app UI yet | Not supported | not wired (block is the wired entry point) |
| Privacy settings (last seen, online, photo, about, group adds, read receipts) | Done (`set_privacy_setting`) | Done while linked | Phone-managed note | contract (privacy flow), fake command log |
| Desktop notifications | n/a (OS facility) | Done: OS popups via notify-rust, driven by Settings > Notifications (message/group toggles, per-chat mute, sound, preview toggle) and suppressed for the focused open chat | Test-backend sessions fire them too; pure demo data is static at startup and raises none | app notify unit tests (decision and compose) |
| Calls: voice 1:1 | Done (voip-mlow profile plus the native relay; PCM through cpal, see docs/design/calls.md) | Done: outgoing and incoming calls, accept/reject/end, mute, speaker routing, duration timer, call-log entry in the chat. Tested with fake audio devices and the fake backend; a real verified call awaits the owner's pairing test | Full simulation through the test backend (scripted incoming call, outgoing flow, call log) | backend audio unit tests, contract call tests, app duration-format test |
| Calls: video 1:1 | Done (pre-encoded H.264 transport; the library never touches pixels) | Not wired: the app must bring camera capture and an H.264 codec. Planned as the next sub-phase (docs/design/calls.md); video call attempts answer with an honest error | Not supported | documented plan |
| Calls: group calls and call links | Done in the library facade | Not supported; a group invitation is reported in the app, not joined | Not supported | documented limit |

## Native features (formerly plugins)

Zaprust has no plugin system. Everything that used to be a
plugin is a native feature of the app: compiled in, always
present, switched on or off in Settings. Each feature's logic
lives in its own crate under `crates/features/`, unit-tested
there; the app crate wires the features into the UI and the
live session.

| Feature | Crate | Settings home | Notes |
|---|---|---|---|
| Quick Replies | `crates/features/quick-replies` | Features (on by default) | Trigger expansion (`/thanks`, `/omw`, `/docs`) on send |
| Chat Export | `crates/features/export-chat` | Features | Export the open chat as Markdown |
| Stickers | `crates/features/stickers` | Features | Sticker packs in the media picker |
| GIF Search | `crates/features/giphy` | Features | GIF tab in the media picker (GIPHY, own API key) |

Feature switches persist in `settings.json` (`features` map).
API keys live
in the central credential store (Settings > Features), never in
per-feature files. Upgrading from a plugin-era install
migrates the old install book and per-plugin settings into the
native stores on first run.

## Notes

- **Session-scoped forward and download.** Forwarding and media
  download need the original protocol payload (media encryption
  keys and CDN references live only there). The archive
  deliberately stores no protocol payloads, so the backend keeps a
  session-scoped payload cache (bounded, cleared on restart) and
  both actions answer with a clear error for messages from before
  this session. This is a design decision, not an oversight.
- **Quote on send.** The library's plain-text send path has no
  quote field, so an outgoing quote is attached to the local echo
  only. Receiving quotes works fully.
- **Group star/pin of other people's messages** passes no sender
  (the app does not retain sender addresses for protocol actions);
  the server may reject those for group messages it cannot
  attribute. Own messages and DM messages work.
- **Demo parity.** Demo mode and the FakeBackend share the app's
  single state machine; the fake's scripted flow covers polls,
  reactions, media, view-once, location, contact cards and a
  carousel so the live code paths are exercised without a phone.
