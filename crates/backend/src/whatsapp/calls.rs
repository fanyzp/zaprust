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
        // Video calling is not built yet; the user hears the
        // plain fact and the working alternative.
        events.emit(BackendEvent::Error {
            message: "Video calls are not available yet. Voice calls work.".to_owned(),
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
            events.emit(BackendEvent::Error { message: format!("Invalid chat address: {}", error_words(&e)) });
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
            events.emit(BackendEvent::Error { message: format!("Starting the call failed: {}", error_words(&e)) });
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
            events.emit(BackendEvent::Error { message: format!("Accepting the call failed: {}", error_words(&e)) });
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
