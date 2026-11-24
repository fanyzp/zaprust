//! Voice call signalling: incoming-call offers, outgoing and
//! incoming call setup, and the per-call media pump task.

#[cfg(feature = "calls")]
use whatsapp_rust::prelude::*;
#[cfg(feature = "calls")]
use whatsapp_rust::wacore::types::events::Event;

#[cfg(feature = "calls")]
use crate::{BackendEvent, EventSink};

#[cfg(feature = "calls")]
use super::util::{error_words, lock_cache};

/// The ringing incoming call, kept as the whole protocol event
/// so accept/reject can borrow the offer out of it.
#[cfg(feature = "calls")]
pub(super) type CallIncoming = std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<Event>>>>;

/// The live call's shared state.
#[cfg(feature = "calls")]
pub(super) type CallActive = std::sync::Arc<std::sync::Mutex<Option<ActiveCall>>>;

/// A call-link join in flight: the flag the join task checks
/// when its long start finally returns, set by EndCall when the
/// user gives up waiting (the join's own Ended was already
/// emitted by the EndCall arm, so a cancelled join that still
/// connects is terminated silently instead of resurrecting a
/// call the user already closed).
#[cfg(feature = "calls")]
pub(super) type CallJoining =
    std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>>>;

/// One live call: its protocol handle plus its media sessions.
#[cfg(feature = "calls")]
pub(super) struct ActiveCall {
    pub(super) call_id: String,
    pub(super) handle: whatsapp_rust::voip::CallHandle,
    pub(super) audio: crate::audio::CallAudio,
    /// The video endpoints, when the call carries video (it
    /// started as a video call, or video was switched on
    /// mid-call). Dropping this stops the camera and the frame
    /// forwarder, so the slot owns it for exactly the call's
    /// lifetime.
    pub(super) video: Option<crate::callvideo::CallVideo>,
    /// The other side's video is on, as the pump last reported
    /// it. Shared with the session's control arms, which
    /// compose [`BackendEvent::CallVideoState`] from the slot
    /// and need the remote half they do not own.
    pub(super) remote_video: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// (call id, caller chat) of an incoming offer event.
#[cfg(feature = "calls")]
pub(super) fn offer_call_id(
    ic: &whatsapp_rust::wacore::types::call::IncomingCall,
) -> (String, String) {
    use whatsapp_rust::wacore::types::call::CallAction;
    let call_id = match &ic.action {
        CallAction::Offer { call_id, .. } => call_id.clone(),
        _ => String::new(),
    };
    (call_id, ic.from.to_string())
}

/// Translates call protocol events into backend events and keeps
/// the ringing-call slot current. Runs before the generic event
/// translation, which ignores call events.
#[cfg(feature = "calls")]
pub(super) fn handle_call_event(
    event: &std::sync::Arc<Event>,
    slot: &CallIncoming,
    sink: &EventSink,
) {
    use whatsapp_rust::wacore::types::call::CallAction;
    match &**event {
        Event::IncomingCall(ic) => match &ic.action {
            CallAction::Offer { call_id, is_video, group_jid, .. } => {
                // Only one call rings at a time: a second offer
                // while one is already stored must not replace
                // it. The app is showing the first caller, so
                // keep the first offer and ignore the second;
                // swapping it here would make Accept answer a
                // different caller than the one on screen. A
                // group invitation rings the same way, keyed to
                // the group: the call belongs to the group,
                // whoever sent the invite.
                let stored_first = {
                    let mut guard = lock_cache(slot);
                    if guard.is_some() {
                        true
                    } else {
                        *guard = Some(event.clone());
                        false
                    }
                };
                if !stored_first {
                    sink.emit(BackendEvent::IncomingCall {
                        call_id: call_id.clone(),
                        chat_jid: group_jid
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| ic.from.to_string()),
                        video: *is_video,
                        group_jid: group_jid.as_ref().map(ToString::to_string),
                    });
                }
            }
            CallAction::Terminate { call_id, .. } => {
                // Clear the ringing offer only if it is this
                // call's, decided and done under one lock hold
                // (release_slot_if). The old take, check, restore
                // read the slot as empty in between: a new offer
                // landing in that window was stored and announced,
                // then overwritten by the restore, and an Accept
                // peeking in the window saw no call at all.
                if release_slot_if(slot, |e| is_offer_for(e, call_id)).is_some() {
                    sink.emit(BackendEvent::CallChanged {
                        call_id: call_id.clone(),
                        chat_jid: ic.from.to_string(),
                        phase: crate::CallPhase::Ended,
                        seconds: 0,
                        reason: Some("Call ended".to_owned()),
                    });
                }
            }
            _ => {}
        },
        Event::MissedCall(missed) => {
            // Same conditional clear as Terminate above: one
            // lock hold, and another caller's offer is never
            // taken out of the slot, not even briefly.
            if release_slot_if(slot, |e| is_offer_for(e, &missed.call_id)).is_some() {
                sink.emit(BackendEvent::CallChanged {
                    call_id: missed.call_id.clone(),
                    chat_jid: missed.from.to_string(),
                    phase: crate::CallPhase::Ended,
                    seconds: 0,
                    reason: Some("Missed call".to_owned()),
                });
            } else {
                // The offer was never this slot's, so the app was
                // never told the call was ringing (another call
                // held the slot, or the library replayed the call
                // from the offline queue). The call still belongs
                // in the history: report it as missed, with the
                // protocol event's own ring timestamp.
                sink.emit(BackendEvent::CallMissed {
                    call_id: missed.call_id.clone(),
                    chat_jid: missed.from.to_string(),
                    at: missed.timestamp.timestamp(),
                });
            }
        }
        Event::CallEndedElsewhere(ended) => {
            use whatsapp_rust::wacore::types::call::ElsewhereOutcome;
            // The library dispatches this outcome before the
            // terminate stanza that follows it, so clearing the
            // ringing slot here (the same conditional clear as
            // Terminate and MissedCall) leaves that stanza
            // nothing to announce: the call ends exactly once,
            // through this event, carrying the outcome the other
            // device reached instead of a plain "Call ended"
            // the app would have to record as missed. Whether
            // the slot held the offer or not, the app hears the
            // same event: it ends the ringing prompt for a shown
            // call and records the history entry for an unshown
            // one.
            release_slot_if(slot, |e| is_offer_for(e, &ended.call_id));
            sink.emit(BackendEvent::CallEndedElsewhere {
                call_id: ended.call_id.clone(),
                chat_jid: ended.from.to_string(),
                answered: matches!(ended.outcome, ElsewhereOutcome::Accepted),
                at: ended.timestamp.timestamp(),
            });
        }
        _ => {}
    }
}

/// Builds the video plumbing for a call that carries video.
/// Runs the (potentially slow, camera-probing) construction on
/// a blocking thread so a wedged camera driver cannot stall
/// the session loop; the construction itself is bounded by its
/// own setup handshake.
#[cfg(feature = "calls")]
async fn build_video(events: &EventSink) -> crate::callvideo::CallVideo {
    let sink = events.clone();
    match tokio::task::spawn_blocking(move || crate::callvideo::CallVideo::real(sink)).await {
        Ok(video) => video,
        Err(_) => crate::callvideo::CallVideo::real(events.clone()),
    }
}

/// Emits the call's video state from a video session's camera
/// facts and the remote side's last known state.
#[cfg(feature = "calls")]
fn emit_video_state(
    events: &EventSink,
    video: Option<&crate::callvideo::CallVideo>,
    remote_on: bool,
) {
    let (local_on, local_note) = match video {
        Some(video) => (video.camera_on, video.camera_note.clone()),
        None => (false, None),
    };
    events.emit(BackendEvent::CallVideoState { local_on, remote_on, local_note });
}

/// Adopts a started call: announces its first phase, reports
/// its video state when it carries video, stores it in the
/// slot BEFORE spawning its pump, and spawns the pump. The
/// store-before-spawn order matters: the pump releases the
/// slot by call id when the call ends, and a call that ends
/// instantly (a buffered relay failure already waiting in its
/// event stream) must find its own entry there; spawning
/// first let the pump clean up an empty slot and left the
/// dead entry stored behind it, rejecting every later call
/// as "already in progress" until the app restarted.
#[cfg(feature = "calls")]
#[allow(clippy::too_many_arguments)]
fn adopt_call(
    events: &EventSink,
    active: &CallActive,
    handle: whatsapp_rust::voip::CallHandle,
    chat_jid: String,
    audio: crate::audio::CallAudio,
    video: Option<crate::callvideo::CallVideo>,
    first_phase: crate::CallPhase,
) {
    let call_id = handle.call_id().to_owned();
    events.emit(BackendEvent::CallChanged {
        call_id: call_id.clone(),
        chat_jid: chat_jid.clone(),
        phase: first_phase,
        seconds: 0,
        reason: None,
    });
    if video.is_some() {
        emit_video_state(events, video.as_ref(), false);
    }
    let pump_handle = handle.clone();
    *lock_cache(active) = Some(ActiveCall {
        call_id: call_id.clone(),
        handle,
        audio,
        video,
        remote_video: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    });
    spawn_call_pump(pump_handle, call_id, chat_jid, active, events);
}

/// Starts an outgoing 1:1 call: opens the audio devices first
/// (that is also the microphone permission request), then
/// places the call through the library facade. A video call
/// also builds the camera plumbing and attaches the video
/// endpoints; a machine without a camera still places the
/// call (receiving video, sending voice) and says so through
/// the video state event.
#[cfg(feature = "calls")]
pub(super) async fn start_outgoing_call(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    active: &CallActive,
    chat_jid: &str,
    video: bool,
) {
    if lock_cache(active).is_some() {
        events.emit(BackendEvent::Error { message: "A call is already in progress.".to_owned() });
        return;
    }
    let jid = match chat_jid.parse::<Jid>() {
        Ok(j) => j,
        Err(e) => {
            events.emit(BackendEvent::Error {
                message: format!("Invalid chat address: {}", error_words(&e)),
            });
            return;
        }
    };
    let audio = match crate::audio::CallAudio::real(false).await {
        Ok(a) => a,
        Err(e) => {
            events.emit(BackendEvent::Error { message: e });
            return;
        }
    };
    let video_plumbing = if video { Some(build_video(events).await) } else { None };
    let mut builder =
        client.voip().call(&jid).audio(audio.source.clone(), audio.sink.clone());
    if let Some(plumbing) = &video_plumbing {
        builder = builder.video(plumbing.source.clone(), plumbing.sink.clone());
    }
    match builder.start().await {
        Ok(handle) => {
            adopt_call(
                events,
                active,
                handle,
                chat_jid.to_owned(),
                audio,
                video_plumbing,
                crate::CallPhase::Ringing,
            );
        }
        Err(e) => {
            audio.shutdown();
            if let Some(plumbing) = &video_plumbing {
                plumbing.shutdown();
            }
            events.emit(BackendEvent::Error {
                message: format!("Starting the call failed: {}", error_words(&e)),
            });
        }
    }
}

/// Starts an outgoing group call bound to a group chat: the
/// library resolves the group's current roster itself and
/// rings its members. Phases and roster then flow like any
/// group call's.
#[cfg(feature = "calls")]
pub(super) async fn start_group_call(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    active: &CallActive,
    chat_jid: &str,
    video: bool,
) {
    if lock_cache(active).is_some() {
        events.emit(BackendEvent::Error { message: "A call is already in progress.".to_owned() });
        return;
    }
    let jid = match chat_jid.parse::<Jid>() {
        Ok(j) => j,
        Err(e) => {
            events.emit(BackendEvent::Error {
                message: format!("Invalid chat address: {}", error_words(&e)),
            });
            return;
        }
    };
    let audio = match crate::audio::CallAudio::real(false).await {
        Ok(a) => a,
        Err(e) => {
            events.emit(BackendEvent::Error { message: e });
            return;
        }
    };
    let video_plumbing = if video { Some(build_video(events).await) } else { None };
    let voip = client.voip();
    let mut builder =
        voip.group_call_by_id(&jid).audio(audio.source.clone(), audio.sink.clone());
    if let Some(plumbing) = &video_plumbing {
        builder = builder.video(plumbing.source.clone(), plumbing.sink.clone());
    }
    match builder.start().await {
        Ok(handle) => {
            adopt_call(
                events,
                active,
                handle,
                chat_jid.to_owned(),
                audio,
                video_plumbing,
                crate::CallPhase::Ringing,
            );
        }
        Err(e) => {
            audio.shutdown();
            if let Some(plumbing) = &video_plumbing {
                plumbing.shutdown();
            }
            events.emit(BackendEvent::Error {
                message: format!("Starting the group call failed: {}", error_words(&e)),
            });
        }
    }
}

/// Accepts the ringing incoming call, with the same audio-first
/// flow as outgoing calls.
#[cfg(feature = "calls")]
pub(super) async fn accept_incoming_call(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    active: &CallActive,
    incoming: &CallIncoming,
) {
    if lock_cache(active).is_some() {
        events.emit(BackendEvent::Error { message: "A call is already in progress.".to_owned() });
        return;
    }
    // Peek at the ringing offer instead of taking it: the slot is
    // only cleared once the call is actually accepted below. If
    // audio setup or the voip accept fails, the caller is still
    // ringing and the user must be able to retry Accept, or use
    // Reject/EndCall, all of which read this same slot.
    let stored = lock_cache(incoming).clone();
    let Some(event) = stored else {
        events.emit(BackendEvent::Error {
            message: "There is no incoming call to accept.".to_owned(),
        });
        return;
    };
    let Event::IncomingCall(ic) = &*event else {
        return;
    };
    let (_, chat_jid) = offer_call_id(ic);
    // The offer's own facts decide the accept's shape: a video
    // offer is accepted with video endpoints (the facade builds
    // the group media spec itself when the offer is a group
    // invitation, so one accept path serves both).
    let offer_video = matches!(
        &ic.action,
        whatsapp_rust::wacore::types::call::CallAction::Offer { is_video: true, .. }
    );
    let offer_group = matches!(
        &ic.action,
        whatsapp_rust::wacore::types::call::CallAction::Offer { group_jid: Some(_), .. }
    );
    let audio = match crate::audio::CallAudio::real(false).await {
        Ok(a) => a,
        Err(e) => {
            events.emit(BackendEvent::Error { message: e });
            return;
        }
    };
    let video_plumbing = if offer_video { Some(build_video(events).await) } else { None };
    let mut builder =
        client.voip().accept(ic).audio(audio.source.clone(), audio.sink.clone());
    if let Some(plumbing) = &video_plumbing {
        builder = builder.video(plumbing.source.clone(), plumbing.sink.clone());
    }
    match builder.start().await {
        Ok(handle) => {
            // Accepted: now the slot can go. Clear it only if it
            // still holds this offer (it cannot have been replaced,
            // handle_call_event keeps the first ringing offer, but
            // a Terminate/MissedCall may have taken it meanwhile).
            let mut guard = lock_cache(incoming);
            if guard.as_ref().is_some_and(|e| std::sync::Arc::ptr_eq(e, &event)) {
                *guard = None;
            }
            drop(guard);
            // A group call's history and overlay belong to the
            // group chat, not to whoever sent the invite.
            let owner_jid = if offer_group {
                match &ic.action {
                    whatsapp_rust::wacore::types::call::CallAction::Offer {
                        group_jid: Some(group),
                        ..
                    } => group.to_string(),
                    _ => chat_jid.clone(),
                }
            } else {
                chat_jid.clone()
            };
            adopt_call(
                events,
                active,
                handle,
                owner_jid,
                audio,
                video_plumbing,
                crate::CallPhase::Connecting,
            );
        }
        Err(e) => {
            audio.shutdown();
            if let Some(plumbing) = &video_plumbing {
                plumbing.shutdown();
            }
            events.emit(BackendEvent::Error {
                message: format!("Accepting the call failed: {}", error_words(&e)),
            });
        }
    }
}

/// Creates a call link of the asked kind and answers with its
/// URL.
#[cfg(feature = "calls")]
pub(super) async fn create_call_link(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    video: bool,
) {
    use whatsapp_rust::wacore::types::group_call::CallLinkMedia;
    let media = if video { CallLinkMedia::Video } else { CallLinkMedia::Audio };
    match client.voip().create_call_link(media).await {
        Ok(link) => {
            events.emit(BackendEvent::CallLinkCreated { url: link.url(), video });
        }
        Err(e) => {
            events.emit(BackendEvent::CallLinkFailed {
                message: format!("Creating the call link failed: {}", error_words(&e)),
            });
        }
    }
}

/// The media kind a call link string names, from its URL path
/// when it carries one. A bare token names no kind.
#[cfg(feature = "calls")]
fn link_media_from_url(link: &str) -> Option<whatsapp_rust::wacore::types::group_call::CallLinkMedia> {
    use whatsapp_rust::wacore::types::group_call::CallLinkMedia;
    if link.contains("/video/") {
        Some(CallLinkMedia::Video)
    } else if link.contains("/audio/") {
        Some(CallLinkMedia::Audio)
    } else {
        None
    }
}

/// Reads a call link's facts before joining. The preview call
/// needs the link's kind; a full URL names it, and for a bare
/// token the audio kind is tried first, then video (the
/// preview answers only for the kind the link was created
/// with, so the one that answers is the link's kind).
#[cfg(feature = "calls")]
pub(super) async fn preview_call_link(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    link: &str,
) {
    use whatsapp_rust::wacore::types::group_call::CallLinkMedia;
    let kinds: Vec<CallLinkMedia> = match link_media_from_url(link) {
        Some(media) => vec![media],
        None => vec![CallLinkMedia::Audio, CallLinkMedia::Video],
    };
    let mut last_error: Option<String> = None;
    for media in kinds {
        match client.voip().preview_call_link(link, media).await {
            Ok(preview) => {
                events.emit(BackendEvent::CallLinkPreview {
                    url: format!(
                        "https://call.whatsapp.com/{}/{}",
                        media.as_str(),
                        preview.token
                    ),
                    creator_jid: preview.creator.to_string(),
                    video: media == CallLinkMedia::Video,
                    waiting_room: preview.waiting_room_enabled,
                });
                return;
            }
            Err(e) => {
                last_error = Some(error_words(&e).to_owned());
            }
        }
    }
    events.emit(BackendEvent::CallLinkFailed {
        message: format!(
            "That call link could not be read: {}",
            last_error.unwrap_or_else(|| "unknown error".to_owned())
        ),
    });
}

/// Joins a call through a call link. The join can sit in the
/// link's waiting room for as long as the host takes, so it
/// runs as its own task: the overlay opens in Connecting at
/// once (with an empty call id, adopted by the real one when
/// the join lands), and the EndCall arm can cancel through
/// the joining slot while the wait runs.
#[cfg(feature = "calls")]
pub(super) async fn join_call_link(
    client: std::sync::Arc<whatsapp_rust::Client>,
    events: &EventSink,
    active: &CallActive,
    joining: &CallJoining,
    link: String,
    video: bool,
) {
    use whatsapp_rust::wacore::types::group_call::CallLinkMedia;
    if lock_cache(active).is_some() || lock_cache(joining).is_some() {
        events.emit(BackendEvent::Error { message: "A call is already in progress.".to_owned() });
        return;
    }
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    *lock_cache(joining) = Some(cancelled.clone());
    events.emit(BackendEvent::CallChanged {
        call_id: String::new(),
        chat_jid: String::new(),
        phase: crate::CallPhase::Connecting,
        seconds: 0,
        reason: None,
    });
    let media = if video { CallLinkMedia::Video } else { CallLinkMedia::Audio };
    let audio = match crate::audio::CallAudio::real(false).await {
        Ok(a) => a,
        Err(e) => {
            *lock_cache(joining) = None;
            events.emit(BackendEvent::CallChanged {
                call_id: String::new(),
                chat_jid: String::new(),
                phase: crate::CallPhase::Ended,
                seconds: 0,
                reason: Some(e),
            });
            return;
        }
    };
    let video_plumbing = if video { Some(build_video(events).await) } else { None };
    let active = active.clone();
    let joining = joining.clone();
    let events = events.clone();
    tokio::spawn(async move {
        // The builder borrows the client and the link string,
        // so it is built here, inside the task that owns both.
        let voip = client.voip();
        let mut builder = voip
            .call_link(&link, media)
            .audio(audio.source.clone(), audio.sink.clone());
        if let Some(plumbing) = &video_plumbing {
            builder = builder.video(plumbing.source.clone(), plumbing.sink.clone());
        }
        let started = builder.start().await;
        *lock_cache(&joining) = None;
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            // The user already closed this call: end whatever
            // arrived and stay silent.
            if let Ok(handle) = started {
                let _ = handle.terminate().await;
            }
            audio.shutdown();
            if let Some(plumbing) = &video_plumbing {
                plumbing.shutdown();
            }
            return;
        }
        match started {
            Ok(handle) => {
                adopt_call(
                    &events,
                    &active,
                    handle,
                    String::new(),
                    audio,
                    video_plumbing,
                    crate::CallPhase::Connecting,
                );
            }
            Err(e) => {
                audio.shutdown();
                if let Some(plumbing) = &video_plumbing {
                    plumbing.shutdown();
                }
                events.emit(BackendEvent::CallChanged {
                    call_id: String::new(),
                    chat_jid: String::new(),
                    phase: crate::CallPhase::Ended,
                    seconds: 0,
                    reason: Some(format!(
                        "Joining the call failed: {}",
                        error_words(&e)
                    )),
                });
            }
        }
    });
}

/// Turns the current call's local video on or off. Turning it
/// on builds the camera plumbing and starts the video session
/// (on a voice call this is the upgrade to a video call);
/// turning it off stops the session and releases the camera.
/// The confirmed state answers on
/// [`BackendEvent::CallVideoState`], a refusal on
/// [`BackendEvent::CallControlFailed`].
#[cfg(feature = "calls")]
pub(super) async fn set_call_video(events: &EventSink, active: &CallActive, on: bool) {
    let handle = lock_cache(active).as_ref().map(|a| a.handle.clone());
    let Some(handle) = handle else {
        events.emit(BackendEvent::CallControlFailed {
            message: "No call is in progress.".to_owned(),
        });
        return;
    };
    if !on {
        // Detach the plumbing first: if stop_video then fails,
        // the camera is still released and the state event
        // below reports the local side off, which is the
        // user's intent either way.
        let taken = lock_cache(active).as_mut().and_then(|a| a.video.take());
        match handle.stop_video().await {
            Ok(()) => {
                if let Some(plumbing) = &taken {
                    plumbing.shutdown();
                }
                let remote = lock_cache(active)
                    .as_ref()
                    .is_some_and(|a| a.remote_video.load(std::sync::atomic::Ordering::Relaxed));
                emit_video_state(events, None, remote);
            }
            Err(e) => {
                // Restore the plumbing: the session is still
                // live, so the camera must stay owned by it.
                if let Some(plumbing) = taken {
                    if let Some(slot) = &mut *lock_cache(active) {
                        slot.video = Some(plumbing);
                    }
                }
                events.emit(BackendEvent::CallControlFailed {
                    message: format!("Stopping video failed: {}", error_words(&e)),
                });
            }
        }
        return;
    }
    if lock_cache(active).as_ref().is_some_and(|a| a.video.is_some()) {
        // Already on: confirm the standing state.
        let mut guard = lock_cache(active);
        if let Some(slot) = &mut *guard {
            let remote = slot.remote_video.load(std::sync::atomic::Ordering::Relaxed);
            emit_video_state(events, slot.video.as_ref(), remote);
        }
        return;
    }
    let plumbing = build_video(events).await;
    match handle.start_video(plumbing.source.clone(), plumbing.sink.clone()).await {
        Ok(()) => {
            let mut guard = lock_cache(active);
            match &mut *guard {
                Some(slot) if slot.handle.call_id() == handle.call_id() => {
                    slot.video = Some(plumbing);
                    let remote = slot.remote_video.load(std::sync::atomic::Ordering::Relaxed);
                    emit_video_state(events, slot.video.as_ref(), remote);
                }
                _ => {
                    // The call ended while the camera warmed
                    // up: release it and say the control found
                    // no call to act on.
                    plumbing.shutdown();
                    events.emit(BackendEvent::CallControlFailed {
                        message: "No call is in progress.".to_owned(),
                    });
                }
            }
        }
        Err(e) => {
            plumbing.shutdown();
            events.emit(BackendEvent::CallControlFailed {
                message: format!("Starting video failed: {}", error_words(&e)),
            });
        }
    }
}

/// Whether the stored ringing event is the offer for `call_id`.
#[cfg(feature = "calls")]
fn is_offer_for(event: &Event, call_id: &str) -> bool {
    use whatsapp_rust::wacore::types::call::CallAction;
    matches!(event, Event::IncomingCall(prev)
        if matches!(&prev.action, CallAction::Offer { call_id: id, .. } if id == call_id))
}

/// Takes a one-at-a-time slot's entry out only if it is still
/// the entry `is_owner` accepts. This is the call pump's exit
/// path and the ringing slot's conditional clear, and the check
/// is what makes the release safe: the entry is matched by call
/// id under the lock, so a pump finishing late can never evict a
/// newer call that has taken the slot since, a pump whose call
/// was already released finds nothing to do, and a ringing offer
/// for a different call is never taken out, not even briefly.
#[cfg(feature = "calls")]
fn release_slot_if<T>(
    slot: &std::sync::Mutex<Option<T>>,
    is_owner: impl FnOnce(&T) -> bool,
) -> Option<T> {
    let mut guard = lock_cache(slot);
    if guard.as_ref().is_some_and(is_owner) {
        guard.take()
    } else {
        None
    }
}

/// Spawns the per-call pump task (see [`pump_call`]).
#[cfg(feature = "calls")]
pub(super) fn spawn_call_pump(
    handle: whatsapp_rust::voip::CallHandle,
    call_id: String,
    chat_jid: String,
    active: &CallActive,
    events: &EventSink,
) {
    let active = active.clone();
    let events = events.clone();
    tokio::spawn(async move {
        pump_call(handle, call_id, chat_jid, active, events).await;
    });
}

/// Pumps one call's event stream into backend events: media
/// connected flips the call Active and starts the duration
/// clock; terminal library events end it with their reason; a
/// half-second phase poll catches ends that arrive without an
/// event. Group and link calls additionally translate the
/// roster snapshots, hand raises, screen-share changes,
/// reactions and waiting-room updates; video state changes
/// are answered here too (a peer's upgrade request is
/// accepted with the call's own endpoints, or starts video
/// on a voice call). On the way out it releases the call slot
/// and the media devices.
#[cfg(feature = "calls")]
pub(super) async fn pump_call(
    handle: whatsapp_rust::voip::CallHandle,
    call_id: String,
    chat_jid: String,
    active: CallActive,
    events: EventSink,
) {
    use whatsapp_rust::voip::CallEvent;
    let mut stream = handle.take_events();
    let mut active_since: Option<std::time::Instant> = None;
    let mut last_seconds: u32 = u32::MAX;
    let mut end_reason: Option<String> = None;
    // Group roster bookkeeping: the engine's snapshots are
    // authoritative for membership; the hand and share flags
    // arrive on their own events and are folded in by JID.
    let mut roster: Vec<crate::CallParticipant> = Vec::new();
    let mut hand_raised: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    let mut sharing: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    let mut last_dropped: u64 = 0;
    let emit_roster = |events: &EventSink,
                       roster: &Vec<crate::CallParticipant>,
                       hand_raised: &std::collections::HashMap<String, bool>,
                       sharing: &std::collections::HashMap<String, bool>| {
        let participants = roster
            .iter()
            .map(|p| crate::CallParticipant {
                jid: p.jid.clone(),
                state: p.state.clone(),
                hand_raised: hand_raised.get(&p.jid).copied().unwrap_or(p.hand_raised),
                sharing: sharing.get(&p.jid).copied().unwrap_or(p.sharing),
            })
            .collect();
        events.emit(BackendEvent::CallParticipants { participants });
    };
    'pump: loop {
        let tick = tokio::time::sleep(std::time::Duration::from_millis(500));
        tokio::select! {
            ev = async {
                match &mut stream {
                    Some(s) => s.recv().await.map_err(|_| ()),
                    None => std::future::pending().await,
                }
            } => {
                match ev {
                    Ok(CallEvent::MediaConnected) => {
                        if active_since.is_none() {
                            active_since = Some(std::time::Instant::now());
                            last_seconds = 0;
                            events.emit(BackendEvent::CallChanged {
                                call_id: call_id.clone(),
                                chat_jid: chat_jid.clone(),
                                phase: crate::CallPhase::Active,
                                seconds: 0,
                                reason: None,
                            });
                        }
                    }
                    Ok(CallEvent::RelayAllocateFailed(code)) => {
                        end_reason = Some(format!("The call could not connect (relay error {code})."));
                        break 'pump;
                    }
                    Ok(CallEvent::RelayAllocateTimedOut) => {
                        end_reason = Some("The call could not connect (the relay did not answer).".to_owned());
                        break 'pump;
                    }
                    Ok(CallEvent::MediaSetupFailed(msg)) => {
                        end_reason = Some(format!("The call could not start: {msg}"));
                        break 'pump;
                    }
                    Ok(CallEvent::RelayReconnectTimedOut) => {
                        end_reason = Some("The call dropped (reconnect timed out).".to_owned());
                        break 'pump;
                    }
                    Ok(CallEvent::AudioFormatMismatch { .. }) => {
                        end_reason = Some("The other side's audio format is not supported.".to_owned());
                        break 'pump;
                    }
                    Ok(CallEvent::GroupUpdated(update)) => {
                        roster = update
                            .participants
                            .iter()
                            .map(|p| crate::CallParticipant {
                                jid: p.jid.to_string(),
                                state: p.state.clone().unwrap_or_default(),
                                hand_raised: false,
                                sharing: false,
                            })
                            .collect();
                        emit_roster(&events, &roster, &hand_raised, &sharing);
                    }
                    Ok(CallEvent::HandRaised { participant, raised }) => {
                        hand_raised.insert(participant.to_string(), raised);
                        emit_roster(&events, &roster, &hand_raised, &sharing);
                    }
                    Ok(CallEvent::ScreenShareChanged { participant, screen_share }) => {
                        use whatsapp_rust::wacore::types::group_call::ScreenShareState;
                        let on = screen_share.state == ScreenShareState::Started;
                        sharing.insert(participant.to_string(), on);
                        emit_roster(&events, &roster, &hand_raised, &sharing);
                        if on {
                            // A share is remote video: the
                            // stage opens for the sharer's
                            // frames.
                            set_remote_video(&active, &call_id, true);
                            emit_slot_video_state(&events, &active, &call_id);
                        }
                    }
                    Ok(CallEvent::Reaction { participant, emoji, removed, .. }) => {
                        events.emit(BackendEvent::CallReaction {
                            jid: participant.to_string(),
                            emoji: if removed { None } else { emoji },
                        });
                    }
                    Ok(CallEvent::WaitingRoomUpdated(room)) => {
                        events.emit(BackendEvent::CallWaitingRoom {
                            is_admin: room.is_admin,
                            approval_required: room.enabled,
                            users: room
                                .users
                                .iter()
                                .map(|u| crate::CallWaitingUser {
                                    jid: u.jid.to_string(),
                                    phone: u.pn.as_ref().map(ToString::to_string),
                                })
                                .collect(),
                        });
                    }
                    Ok(CallEvent::PeerVideoStateChanged { state, upgrade_token, .. }) => {
                        use whatsapp_rust::wacore::types::call::VideoState;
                        let remote_on = matches!(
                            state,
                            VideoState::Enabled
                                | VideoState::Paused
                                | VideoState::UpgradeAccept
                        );
                        set_remote_video(&active, &call_id, remote_on);
                        if state == VideoState::UpgradeRequest {
                            accept_peer_video_upgrade(
                                &handle, &active, &call_id, &events, upgrade_token,
                            )
                            .await;
                        }
                        emit_slot_video_state(&events, &active, &call_id);
                    }
                    Ok(CallEvent::VideoKeyframeNeeded) => {
                        if let Some(slot) = &*lock_cache(&active) {
                            if slot.call_id == call_id {
                                if let Some(video) = &slot.video {
                                    video
                                        .force_keyframe
                                        .store(true, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(()) => break 'pump,
                }
            }
            () = tick => {
                if matches!(handle.phase(), Some(whatsapp_rust::wacore::voip_control::CallPhase::Ended)) {
                    break 'pump;
                }
                // Keyframe repair: when the video forwarder
                // had to shed frames, ask the peer for a fresh
                // keyframe so the app's decoder resyncs (the
                // handle method is the sink consumer's documented
                // recovery; it is throttled inside the engine).
                {
                    let dropped_now = lock_cache(&active).as_ref().and_then(|slot| {
                        (slot.call_id == call_id)
                            .then(|| slot.video.as_ref().map(|v| {
                                v.dropped_frames.load(std::sync::atomic::Ordering::Relaxed)
                            }))
                            .flatten()
                    });
                    if let Some(dropped_now) = dropped_now {
                        if dropped_now > last_dropped {
                            last_dropped = dropped_now;
                            handle.request_peer_keyframe(
                                whatsapp_rust::voip::KeyframeUrgency::Immediate,
                            );
                        }
                    }
                }
                if let Some(since) = active_since {
                    let secs = u32::try_from(since.elapsed().as_secs()).unwrap_or(u32::MAX);
                    if secs != last_seconds {
                        last_seconds = secs;
                        events.emit(BackendEvent::CallChanged {
                            call_id: call_id.clone(),
                            chat_jid: chat_jid.clone(),
                            phase: crate::CallPhase::Active,
                            seconds: secs,
                            reason: None,
                        });
                    }
                }
            }
        }
    }
    let seconds = active_since.map_or(0, |s| u32::try_from(s.elapsed().as_secs()).unwrap_or(0));
    // Release the slot and the devices if this pump still owns
    // them (a replacement call may already be live).
    if let Some(finished) = release_slot_if(&active, |a| a.call_id == call_id) {
        finished.audio.shutdown();
        if let Some(video) = &finished.video {
            video.shutdown();
        }
    }
    events.emit(BackendEvent::CallChanged {
        call_id,
        chat_jid,
        phase: crate::CallPhase::Ended,
        seconds,
        reason: Some(end_reason.unwrap_or_else(|| "Call ended".to_owned())),
    });
}

/// Records the remote side's video state on the call slot (the
/// session's control arms compose their video state events
/// from it).
#[cfg(feature = "calls")]
fn set_remote_video(active: &CallActive, call_id: &str, on: bool) {
    if let Some(slot) = &*lock_cache(active) {
        if slot.call_id == call_id {
            slot.remote_video.store(on, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Emits the video state composed from the call slot: the
/// local half from its video plumbing, the remote half from
/// the flag the pump maintains.
#[cfg(feature = "calls")]
fn emit_slot_video_state(events: &EventSink, active: &CallActive, call_id: &str) {
    let guard = lock_cache(active);
    if let Some(slot) = &*guard {
        if slot.call_id == call_id {
            let remote = slot.remote_video.load(std::sync::atomic::Ordering::Relaxed);
            emit_video_state(events, slot.video.as_ref(), remote);
        }
    }
}

/// Answers a peer's video upgrade request: with the call's
/// own video endpoints when it already carries video (the
/// user consented to a video call when they started or
/// accepted it as one), or by starting video on a voice call,
/// which builds the camera plumbing and stores it in the
/// slot like the SetCallVideo arm does.
#[cfg(feature = "calls")]
async fn accept_peer_video_upgrade(
    handle: &whatsapp_rust::voip::CallHandle,
    active: &CallActive,
    call_id: &str,
    events: &EventSink,
    upgrade_token: Option<whatsapp_rust::wacore::voip_control::MediaVideoUpgradeToken>,
) {
    let existing = {
        let guard = lock_cache(active);
        guard.as_ref().and_then(|slot| {
            (slot.call_id == call_id).then(|| {
                slot.video
                    .as_ref()
                    .map(|v| (v.source.clone(), v.sink.clone()))
            })
        })
    };
    match existing {
        Some(Some((source, sink))) => {
            if let Some(token) = upgrade_token {
                let _ = handle.accept_video(token, source, sink).await;
            }
        }
        Some(None) => {
            let plumbing = build_video(events).await;
            if handle
                .start_video(plumbing.source.clone(), plumbing.sink.clone())
                .await
                .is_ok()
            {
                let mut guard = lock_cache(active);
                if let Some(slot) = &mut *guard {
                    if slot.call_id == call_id && slot.video.is_none() {
                        slot.video = Some(plumbing);
                        return;
                    }
                }
                plumbing.shutdown();
            } else {
                plumbing.shutdown();
            }
        }
        _ => {}
    }
}

#[cfg(all(test, feature = "calls"))]
mod tests {
    use super::*;

    /// Stand-in for `ActiveCall`: a real one needs a live
    /// library call handle, and the slot logic only ever looks
    /// at the call id.
    struct FakeCall {
        call_id: String,
    }

    fn slot_with(call_id: &str) -> std::sync::Mutex<Option<FakeCall>> {
        std::sync::Mutex::new(Some(FakeCall { call_id: call_id.to_owned() }))
    }

    /// The start paths store the call before spawning its pump,
    /// so a call that ends instantly still finds its own entry
    /// when the pump runs its exit path: the release takes it,
    /// and the slot is free for the next call to claim. (With
    /// the old spawn-first order the pump cleaned up an empty
    /// slot and the dead entry was stored behind it, wedging
    /// the slot for the rest of the session.)
    #[test]
    fn instantly_finished_call_leaves_the_slot_usable() {
        let slot = slot_with("call-a");
        let released = release_slot_if(&slot, |c| c.call_id == "call-a");
        assert!(released.is_some_and(|c| c.call_id == "call-a"));
        assert!(lock_cache(&slot).is_none(), "the slot is free for the next call");
    }

    /// A pump finishing late must not evict the newer call that
    /// took the slot after its own call ended.
    #[test]
    fn stale_cleanup_cannot_evict_a_newer_call() {
        let slot = slot_with("call-b");
        let released = release_slot_if(&slot, |c| c.call_id == "call-a");
        assert!(released.is_none());
        let guard = lock_cache(&slot);
        assert!(guard.as_ref().is_some_and(|c| c.call_id == "call-b"));
    }

    /// Cleanup against an empty slot is a no-op: this is the
    /// pump of a call the command loop already released.
    #[test]
    fn cleanup_on_an_empty_slot_is_a_no_op() {
        let slot: std::sync::Mutex<Option<FakeCall>> = std::sync::Mutex::new(None);
        assert!(release_slot_if(&slot, |c| c.call_id == "call-a").is_none());
        assert!(lock_cache(&slot).is_none());
    }

    use whatsapp_rust::wacore::types::call::{CallAction, IncomingCall, MissedCall, MissedReason};

    fn caller() -> Jid {
        "12025550111@s.whatsapp.net".parse().expect("caller jid parses")
    }

    /// Builds a protocol call event carrying `action` from the
    /// test caller, the shape `handle_call_event` consumes.
    fn call_event(action: CallAction) -> std::sync::Arc<Event> {
        let ic = IncomingCall::builder()
            .from(caller())
            .stanza_id("stanza-1".to_owned())
            .offline(false)
            .action(action)
            .build();
        std::sync::Arc::new(Event::IncomingCall(Box::new(ic)))
    }

    fn offer_action(call_id: &str) -> CallAction {
        CallAction::Offer {
            call_id: call_id.to_owned(),
            call_creator: caller(),
            caller_pn: None,
            caller_country_code: None,
            device_class: None,
            joinable: false,
            is_video: false,
            audio: Vec::new(),
            group_jid: None,
        }
    }

    fn terminate_action(call_id: &str) -> CallAction {
        CallAction::Terminate {
            call_id: call_id.to_owned(),
            call_creator: caller(),
            reason: None,
            duration: None,
            audio_duration: None,
        }
    }

    /// An event sink over a small bounded channel, plus the
    /// receiving end for assertions.
    fn test_sink() -> (EventSink, std::sync::mpsc::Receiver<crate::BackendEvent>) {
        let (tx, rx) = std::sync::mpsc::sync_channel::<crate::BackendEvent>(16);
        (EventSink { tx, wake: std::sync::Arc::new(|| {}) }, rx)
    }

    fn ringing_slot(call_id: &str) -> CallIncoming {
        std::sync::Arc::new(std::sync::Mutex::new(Some(call_event(offer_action(call_id)))))
    }

    /// A Terminate for a different call must leave the ringing
    /// offer in the slot, untouched and peekable, and announce
    /// nothing. The old take, check, restore emptied the slot
    /// for the duration of the check, so an Accept peeking in
    /// that window reported "no incoming call" for a call that
    /// was still ringing, and a new offer stored in the window
    /// was overwritten by the restore.
    #[test]
    fn terminate_for_another_call_keeps_the_ringing_offer() {
        let slot = ringing_slot("call-a");
        let (sink, rx) = test_sink();
        handle_call_event(&call_event(terminate_action("call-b")), &slot, &sink);
        let peeked = lock_cache(&slot).clone();
        assert!(
            peeked.is_some_and(|e| is_offer_for(&e, "call-a")),
            "the ringing offer survives a Terminate for another call"
        );
        assert!(rx.try_recv().is_err(), "nothing is announced for another call's Terminate");
    }

    /// A Terminate for the ringing call itself clears the slot
    /// and announces the end.
    #[test]
    fn terminate_for_the_ringing_call_clears_and_announces() {
        let slot = ringing_slot("call-a");
        let (sink, rx) = test_sink();
        handle_call_event(&call_event(terminate_action("call-a")), &slot, &sink);
        assert!(lock_cache(&slot).is_none(), "the slot is cleared");
        let announced = rx.try_recv().expect("the end is announced");
        assert!(matches!(
            announced,
            crate::BackendEvent::CallChanged { ref call_id, phase: crate::CallPhase::Ended, .. }
            if call_id == "call-a"
        ));
    }

    /// A MissedCall follows the same slot rule: only the ringing
    /// call's own missed event clears its offer. A missed event
    /// for a call whose offer is not the slot's (it rang while
    /// another call held the slot) leaves the offer untouched and
    /// is reported as a history-only CallMissed instead of a
    /// CallChanged: the app never showed that call ringing, so it
    /// must not see a phase change for it, but the call still
    /// belongs in the history.
    #[test]
    fn missed_call_clears_only_its_own_offer() {
        use whatsapp_rust::wacore::chrono::Utc;
        let slot = ringing_slot("call-a");
        let (sink, rx) = test_sink();
        let other = std::sync::Arc::new(Event::MissedCall(MissedCall::new(
            caller(),
            "call-b".to_owned(),
            Utc::now(),
            MissedReason::Remote,
        )));
        handle_call_event(&other, &slot, &sink);
        assert!(lock_cache(&slot).is_some(), "another call's missed event keeps the offer");
        let announced = rx.try_recv().expect("the unshown missed call is reported");
        assert!(
            matches!(
                announced,
                crate::BackendEvent::CallMissed { ref call_id, .. } if call_id == "call-b"
            ),
            "an unshown missed call is a CallMissed, not a phase change: {announced:?}"
        );

        let own = std::sync::Arc::new(Event::MissedCall(MissedCall::new(
            caller(),
            "call-a".to_owned(),
            Utc::now(),
            MissedReason::Remote,
        )));
        handle_call_event(&own, &slot, &sink);
        assert!(lock_cache(&slot).is_none(), "the missed call's own offer is cleared");
        let announced = rx.try_recv().expect("the missed call is announced");
        assert!(matches!(
            announced,
            crate::BackendEvent::CallChanged { ref call_id, phase: crate::CallPhase::Ended, .. }
            if call_id == "call-a"
        ));
        assert!(rx.try_recv().is_err(), "a shown call's missed event is not reported twice");
    }

    /// A missed call replayed from the offline queue never had an
    /// offer in the slot at all: it is reported as a CallMissed
    /// carrying the protocol event's own ring timestamp, so the
    /// history entry is stamped when the call actually rang, not
    /// when the device reconnected.
    #[test]
    fn missed_call_without_an_offer_reports_its_ring_time() {
        use whatsapp_rust::wacore::chrono::DateTime;
        let slot: CallIncoming = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (sink, rx) = test_sink();
        let at = DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp");
        let missed = std::sync::Arc::new(Event::MissedCall(MissedCall::new(
            caller(),
            "call-offline".to_owned(),
            at,
            MissedReason::Offline,
        )));
        handle_call_event(&missed, &slot, &sink);
        let announced = rx.try_recv().expect("the replayed missed call is reported");
        assert!(matches!(
            announced,
            crate::BackendEvent::CallMissed { ref call_id, ref chat_jid, at: 1_700_000_000 }
            if call_id == "call-offline" && chat_jid == "12025550111@s.whatsapp.net"
        ));
    }

    use whatsapp_rust::wacore::types::call::{CallEndedElsewhere, ElsewhereOutcome};

    /// Builds a protocol ended-elsewhere event from the test
    /// caller, stamped `at_secs`.
    fn elsewhere_event(
        call_id: &str,
        outcome: ElsewhereOutcome,
        at_secs: i64,
    ) -> std::sync::Arc<Event> {
        use whatsapp_rust::wacore::chrono::DateTime;
        let at = DateTime::from_timestamp(at_secs, 0).expect("valid timestamp");
        std::sync::Arc::new(Event::CallEndedElsewhere(CallEndedElsewhere::new(
            caller(),
            call_id.to_owned(),
            at,
            outcome,
        )))
    }

    /// The ringing call was answered on the phone: the outcome
    /// clears the slot and is reported as an answered
    /// CallEndedElsewhere. The terminate stanza the library
    /// dispatches right after the outcome then finds the slot
    /// empty and announces nothing, so the call ends exactly
    /// once, with the elsewhere outcome, instead of a plain
    /// "Call ended" the app would record as missed.
    #[test]
    fn answered_elsewhere_clears_the_ringing_call_and_reports_once() {
        let slot = ringing_slot("call-a");
        let (sink, rx) = test_sink();
        handle_call_event(
            &elsewhere_event("call-a", ElsewhereOutcome::Accepted, 1_700_000_100),
            &slot,
            &sink,
        );
        assert!(lock_cache(&slot).is_none(), "the slot is cleared");
        let announced = rx.try_recv().expect("the elsewhere outcome is reported");
        assert!(matches!(
            announced,
            crate::BackendEvent::CallEndedElsewhere {
                ref call_id, ref chat_jid, answered: true, at: 1_700_000_100
            } if call_id == "call-a" && chat_jid == "12025550111@s.whatsapp.net"
        ));
        handle_call_event(&call_event(terminate_action("call-a")), &slot, &sink);
        assert!(rx.try_recv().is_err(), "the trailing Terminate announces nothing more");
    }

    /// An ended-elsewhere report for a call whose offer is not
    /// the slot's (it rang while another call held the line)
    /// leaves the ringing offer untouched and is still reported,
    /// with the declined outcome: the app never showed that
    /// call, but it belongs in the history as answered
    /// elsewhere, not missed.
    #[test]
    fn rejected_elsewhere_for_an_unshown_call_keeps_the_ringing_offer() {
        let slot = ringing_slot("call-a");
        let (sink, rx) = test_sink();
        handle_call_event(
            &elsewhere_event("call-b", ElsewhereOutcome::Rejected, 1_700_000_200),
            &slot,
            &sink,
        );
        let peeked = lock_cache(&slot).clone();
        assert!(
            peeked.is_some_and(|e| is_offer_for(&e, "call-a")),
            "the ringing offer survives another call's elsewhere outcome"
        );
        let announced = rx.try_recv().expect("the unshown call's outcome is reported");
        assert!(matches!(
            announced,
            crate::BackendEvent::CallEndedElsewhere {
                ref call_id, answered: false, at: 1_700_000_200, ..
            } if call_id == "call-b"
        ));
    }

    /// A call that rang and ended elsewhere while this device
    /// was away never had an offer in the slot: the outcome is
    /// reported as-is, stamped with the protocol event's own
    /// timestamp.
    #[test]
    fn ended_elsewhere_without_an_offer_reports_the_outcome() {
        let slot: CallIncoming = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (sink, rx) = test_sink();
        handle_call_event(
            &elsewhere_event("call-offline", ElsewhereOutcome::Accepted, 1_700_000_300),
            &slot,
            &sink,
        );
        let announced = rx.try_recv().expect("the replayed outcome is reported");
        assert!(matches!(
            announced,
            crate::BackendEvent::CallEndedElsewhere {
                ref call_id, answered: true, at: 1_700_000_300, ..
            } if call_id == "call-offline"
        ));
    }
}
