//! Voice call signalling: incoming-call offers, outgoing and
//! incoming call setup, and the per-call media pump task.

#[cfg(feature = "calls")]
use whatsapp_rust::prelude::*;
#[cfg(feature = "calls")]
use whatsapp_rust::wacore::types::events::Event;

#[cfg(feature = "calls")]
use crate::{BackendEvent, EventSink};

#[cfg(feature = "calls")]
use super::util::lock_cache;

/// The ringing incoming call, kept as the whole protocol event
/// so accept/reject can borrow the offer out of it.
#[cfg(feature = "calls")]
pub(super) type CallIncoming = std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<Event>>>>;

/// The live call's shared state.
#[cfg(feature = "calls")]
pub(super) type CallActive = std::sync::Arc<std::sync::Mutex<Option<ActiveCall>>>;

/// One live call: its protocol handle plus its audio session.
#[cfg(feature = "calls")]
pub(super) struct ActiveCall {
    pub(super) call_id: String,
    pub(super) handle: whatsapp_rust::voip::CallHandle,
    pub(super) audio: crate::audio::CallAudio,
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
                if group_jid.is_some() {
                    sink.emit(BackendEvent::Error {
                        message: "A group call invitation arrived. Group calls are not \
                                  supported in Zaprust yet."
                            .to_owned(),
                    });
                } else {
                    // Only one call rings at a time: a second offer
                    // while one is already stored must not replace
                    // it. The app is showing the first caller, so
                    // keep the first offer and ignore the second;
                    // swapping it here would make Accept answer a
                    // different caller than the one on screen.
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
                            chat_jid: ic.from.to_string(),
                            video: *is_video,
                        });
                    }
                }
            }
            CallAction::Terminate { call_id, .. } => {
                let stored = lock_cache(slot).take();
                let matches = stored.as_ref().is_some_and(|e| {
                    matches!(&**e, Event::IncomingCall(prev)
                        if matches!(&prev.action, CallAction::Offer { call_id: id, .. } if id == call_id))
                });
                if matches {
                    sink.emit(BackendEvent::CallChanged {
                        call_id: call_id.clone(),
                        chat_jid: ic.from.to_string(),
                        phase: crate::CallPhase::Ended,
                        seconds: 0,
                        reason: Some("Call ended".to_owned()),
                    });
                } else if let Some(prev) = stored {
                    *lock_cache(slot) = Some(prev);
                }
            }
            _ => {}
        },
        Event::MissedCall(missed) => {
            let stored = lock_cache(slot).take();
            let matches = stored.as_ref().is_some_and(|e| {
                matches!(&**e, Event::IncomingCall(prev)
                    if matches!(&prev.action, CallAction::Offer { call_id, .. } if *call_id == missed.call_id))
            });
            if matches {
                sink.emit(BackendEvent::CallChanged {
                    call_id: missed.call_id.clone(),
                    chat_jid: missed.from.to_string(),
                    phase: crate::CallPhase::Ended,
                    seconds: 0,
                    reason: Some("Missed call".to_owned()),
                });
            } else if let Some(prev) = stored {
                *lock_cache(slot) = Some(prev);
            }
        }
        _ => {}
    }
}

/// Starts an outgoing voice call: opens the audio devices first
/// (that is also the microphone permission request), then places
/// the call through the library facade.
#[cfg(feature = "calls")]
pub(super) async fn start_outgoing_call(
    client: &whatsapp_rust::Client,
    events: &EventSink,
    active: &CallActive,
    chat_jid: &str,
    video: bool,
) {
    if video {
        events.emit(BackendEvent::Error {
            message: "Video calls are not wired yet. Voice calls work; video lands in a \
                      later sub-phase (see docs/design/calls.md)."
                .to_owned(),
        });
        return;
    }
    if lock_cache(active).is_some() {
        events.emit(BackendEvent::Error { message: "A call is already in progress.".to_owned() });
        return;
    }
    let jid = match chat_jid.parse::<Jid>() {
        Ok(j) => j,
        Err(e) => {
            events.emit(BackendEvent::Error { message: format!("Invalid chat address: {e}") });
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
    match client.voip().call(&jid).audio(audio.source.clone(), audio.sink.clone()).start().await {
        Ok(handle) => {
            let call_id = handle.call_id().to_owned();
            events.emit(BackendEvent::CallChanged {
                call_id: call_id.clone(),
                chat_jid: chat_jid.to_owned(),
                phase: crate::CallPhase::Ringing,
                seconds: 0,
                reason: None,
            });
            // Store the call BEFORE spawning its pump. The pump
            // releases the slot by call id when the call ends, and
            // a call that ends instantly (a buffered relay failure
            // already waiting in its event stream) must find its
            // own entry there: spawning first let the pump clean
            // up an empty slot and left this dead entry stored
            // behind it, rejecting every later call as "already
            // in progress" until the app restarted.
            let pump_handle = handle.clone();
            *lock_cache(active) = Some(ActiveCall { call_id: call_id.clone(), handle, audio });
            spawn_call_pump(pump_handle, call_id, chat_jid.to_owned(), active, events);
        }
        Err(e) => {
            audio.shutdown();
            events.emit(BackendEvent::Error { message: format!("Starting the call failed: {e}") });
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
    let (call_id, chat_jid) = offer_call_id(ic);
    let audio = match crate::audio::CallAudio::real(false).await {
        Ok(a) => a,
        Err(e) => {
            events.emit(BackendEvent::Error { message: e });
            return;
        }
    };
    match client.voip().accept(ic).audio(audio.source.clone(), audio.sink.clone()).start().await {
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
            events.emit(BackendEvent::CallChanged {
                call_id: call_id.clone(),
                chat_jid: chat_jid.clone(),
                phase: crate::CallPhase::Connecting,
                seconds: 0,
                reason: None,
            });
            // Store the call BEFORE spawning its pump, for the
            // same reason as in start_outgoing_call: the pump
            // releases the slot by call id when the call ends,
            // and an instantly ending call must find its own
            // entry there, not be stored behind the cleanup.
            let pump_handle = handle.clone();
            *lock_cache(active) = Some(ActiveCall { call_id: call_id.clone(), handle, audio });
            spawn_call_pump(pump_handle, call_id, chat_jid, active, events);
        }
        Err(e) => {
            audio.shutdown();
            events.emit(BackendEvent::Error { message: format!("Accepting the call failed: {e}") });
        }
    }
}

/// Takes a one-at-a-time slot's entry out only if it is still
/// the entry `is_owner` accepts. This is the call pump's exit
/// path, and the check is what makes the release safe: the
/// entry is matched by call id under the lock, so a pump
/// finishing late can never evict a newer call that has taken
/// the slot since, and a pump whose call was already released
/// finds nothing to do.
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

/// Pumps one call's event stream into `CallChanged` events: media
/// connected flips the call Active and starts the duration clock;
/// terminal library events end it with their reason; a half-second
/// phase poll catches ends that arrive without an event. On the
/// way out it releases the call slot and the audio devices.
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
                    Ok(_) => {}
                    Err(()) => break 'pump,
                }
            }
            () = tick => {
                if matches!(handle.phase(), Some(whatsapp_rust::wacore::voip_control::CallPhase::Ended)) {
                    break 'pump;
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
    }
    events.emit(BackendEvent::CallChanged {
        call_id,
        chat_jid,
        phase: crate::CallPhase::Ended,
        seconds,
        reason: Some(end_reason.unwrap_or_else(|| "Call ended".to_owned())),
    });
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
}
