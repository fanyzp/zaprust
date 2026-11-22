//! Headless pairing harness for real-device tests.
//! Takes the phone number (digits) as an argument or from stdin,
//! requests a pairing code through the real backend, prints the
//! code and the event summaries, and exits after Connected plus a
//! short linger. Message bodies are never printed.
//!
//! Run it with:
//! `cargo run --release -p backend --example pair_harness -- 12025550123`

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use backend::{
    spawn_backend, Backend, BackendCommand, BackendConfig, BackendEvent, WhatsappBackend,
};

fn summarize(event: &BackendEvent) -> String {
    match event {
        BackendEvent::QrCode { .. } => "QrCode".to_owned(),
        BackendEvent::QrExpired => "QrExpired".to_owned(),
        BackendEvent::PairCode { code } => format!("PairCode {code}"),
        BackendEvent::Connected { account } => format!("Connected account={account}"),
        BackendEvent::LoggedOut => "LoggedOut".to_owned(),
        BackendEvent::TemporaryBan { code } => format!("TemporaryBan code={code}"),
        BackendEvent::ChatUpsert(_) => "ChatUpsert".to_owned(),
        BackendEvent::MessageUpsert(_) => "MessageUpsert".to_owned(),
        BackendEvent::MessageSent { message_id, .. } => format!("MessageSent id={message_id}"),
        BackendEvent::Receipt { kind, .. } => format!("Receipt {kind:?}"),
        BackendEvent::ReactionUpdate { .. } => "ReactionUpdate".to_owned(),
        BackendEvent::ReactionFailed { message, .. } => {
            format!("ReactionFailed {message}")
        }
        BackendEvent::StarFailed { message, .. } => format!("StarFailed {message}"),
        BackendEvent::PinFailed { message, .. } => format!("PinFailed {message}"),
        BackendEvent::StatusPosted { .. } => "StatusPosted".to_owned(),
        BackendEvent::CommunityCreated { jid, name } => {
            format!("CommunityCreated {name} jid={jid}")
        }
        BackendEvent::StatusPostFailed { message, .. } => {
            format!("StatusPostFailed {message}")
        }
        BackendEvent::PollVote { selected, .. } => {
            format!("PollVote {} option(s)", selected.len())
        }
        BackendEvent::MessageEdited { .. } => "MessageEdited".to_owned(),
        BackendEvent::MessageRevoked { .. } => "MessageRevoked".to_owned(),
        BackendEvent::DisappearingChanged { .. } => "DisappearingChanged".to_owned(),
        BackendEvent::DefaultDisappearingTimerChanged { .. } => {
            "DefaultDisappearingTimerChanged".to_owned()
        }
        BackendEvent::MuteChanged { muted, .. } => format!("MuteChanged muted={muted}"),
        BackendEvent::PinChanged { pinned, .. } => format!("PinChanged pinned={pinned}"),
        BackendEvent::ArchiveChanged { archived, .. } => {
            format!("ArchiveChanged archived={archived}")
        }
        BackendEvent::StarChanged { starred, .. } => format!("StarChanged starred={starred}"),
        BackendEvent::ChatReadChanged { read, unread_count, .. } => {
            format!("ChatReadChanged read={read} unread={unread_count}")
        }
        BackendEvent::ChatDeleted { .. } => "ChatDeleted".to_owned(),
        BackendEvent::ChatCleared { delete_starred, .. } => {
            format!("ChatCleared delete_starred={delete_starred}")
        }
        BackendEvent::MessageDeletedForMe { .. } => "MessageDeletedForMe".to_owned(),
        BackendEvent::PrivacyState(_) => "PrivacyState".to_owned(),
        BackendEvent::Blocklist(jids) => format!("Blocklist {} contacts", jids.len()),
        BackendEvent::IncomingCall { .. } => "IncomingCall".to_owned(),
        BackendEvent::CallChanged { phase, .. } => format!("CallChanged {phase:?}"),
        BackendEvent::CallMissed { .. } => "CallMissed".to_owned(),
        BackendEvent::CallEndedElsewhere { answered, .. } => {
            format!("CallEndedElsewhere answered={answered}")
        }
        BackendEvent::MediaSaved { .. } => "MediaSaved".to_owned(),
        BackendEvent::PhotoBytes { bytes, .. } => format!("PhotoBytes {} bytes", bytes.len()),
        BackendEvent::PhotoFetchFailed { message, .. } => {
            format!("PhotoFetchFailed {message}")
        }
        BackendEvent::VoiceBytes { bytes, .. } => format!("VoiceBytes {} bytes", bytes.len()),
        BackendEvent::VoiceFetchFailed { message, .. } => {
            format!("VoiceFetchFailed {message}")
        }
        BackendEvent::VideoBytes { bytes, .. } => format!("VideoBytes {} bytes", bytes.len()),
        BackendEvent::VideoReady { path, .. } => format!("VideoReady {path}"),
        BackendEvent::VideoFetchFailed { message, .. } => {
            format!("VideoFetchFailed {message}")
        }
        BackendEvent::TypingReceived { .. } => "TypingReceived".to_owned(),
        BackendEvent::PresenceReceived { .. } => "PresenceReceived".to_owned(),
        BackendEvent::GroupMembers { .. } => "GroupMembers".to_owned(),
        BackendEvent::GroupInviteLink { .. } => "GroupInviteLink".to_owned(),
        BackendEvent::GroupApprovalState { .. } => "GroupApprovalState".to_owned(),
        BackendEvent::GroupModesState { .. } => "GroupModesState".to_owned(),
        BackendEvent::JoinRequests { .. } => "JoinRequests".to_owned(),
        BackendEvent::ConnectionLost { reason } => format!("ConnectionLost {reason}"),
        BackendEvent::Crashed { summary, restarted } => {
            format!("Crashed restarted={restarted} {summary}")
        }
        BackendEvent::Error { message } => format!("Error {message}"),
        BackendEvent::OwnProfile(profile) => {
            format!("OwnProfile name={:?} about={:?}", profile.name, profile.about)
        }
        BackendEvent::OwnProfileSaveFailed { message } => {
            format!("OwnProfileSaveFailed {message}")
        }
    }
}

fn main() {
    // Route the protocol library's `log` records to stderr so a failed
    // connection attempt is diagnosable instead of silent. The writer
    // is the app's scrubbing writer (core::debug::ScrubWriter, the
    // same one the app's --debug log file uses): sharing terminal
    // output is the natural way to debug a pairing failure, so
    // library lines pass through the crash-report redaction before
    // they reach the terminal instead of bypassing it (privacy
    // audit C5). The harness's own PAIR_CODE print further down
    // stays plaintext on purpose: the operator pairs with it on
    // their own terminal.
    let _ = tracing_log::LogTracer::init();
    let filter = tracing_subscriber::EnvFilter::try_from_env("HARNESS_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug"));
    let log_writer = core::debug::ScrubWriter::new(std::io::stderr());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(move || log_writer.clone())
        .with_ansi(false)
        .try_init();

    let started = Instant::now();
    // Phone number: first CLI argument, else one line from stdin.
    // Digits only, international format (e.g. 12025550123).
    let mut input = std::env::args().nth(1).unwrap_or_default();
    if input.chars().filter(char::is_ascii_digit).count() < 9 {
        eprint!("Phone number (digits, international format): ");
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            eprintln!("HARNESS no stdin");
            std::process::exit(9);
        }
        input = line;
    }
    let phone: String = input.chars().filter(char::is_ascii_digit).collect();
    if phone.len() < 9 {
        eprintln!("HARNESS bad phone input");
        std::process::exit(9);
    }

    // Same platform data dir as the app, separate account slot, so a
    // test session never touches the real account's data.
    let data_dir = backend::data_dir();
    let db_key = store::load_or_create_key(&data_dir).expect("database key");
    let config = BackendConfig { data_dir, account_id: "pairtest".to_owned(), db_key };
    let wake: backend::WakeFn = Arc::new(|| {});
    let channels = spawn_backend(|| Box::new(WhatsappBackend) as Box<dyn Backend>, config, wake);

    if channels.commands.send(BackendCommand::ConnectPairCode { phone: phone.clone() }).is_err() {
        eprintln!("HARNESS command channel closed");
        std::process::exit(8);
    }
    println!("HARNESS pair code requested t={:.1}s", started.elapsed().as_secs_f32());
    let mut retries = 0u32;
    let mut last_retry = Instant::now();

    let overall = Duration::from_secs(300);
    let mut connected_at: Option<Instant> = None;
    let mut saw_code = false;
    loop {
        let now = started.elapsed();
        if now > overall {
            println!("HARNESS timeout overall");
            std::process::exit(3);
        }
        if let Some(t) = connected_at {
            if t.elapsed() > Duration::from_secs(90) {
                println!("HARNESS done, linger over");
                std::process::exit(0);
            }
        }
        match channels.events.recv_timeout(Duration::from_secs(2)) {
            Ok(event) => {
                let line = summarize(&event);
                println!("EV t={:.1}s {line}", started.elapsed().as_secs_f32());
                match &event {
                    BackendEvent::PairCode { code } => {
                        saw_code = true;
                        println!("PAIR_CODE {code}");
                    }
                    BackendEvent::Connected { .. } => {
                        if connected_at.is_none() {
                            connected_at = Some(Instant::now());
                            println!("HARNESS connected, lingering 90s");
                        }
                    }
                    BackendEvent::TemporaryBan { .. } => {
                        println!("HARNESS banned, stopping");
                        std::process::exit(5);
                    }
                    BackendEvent::LoggedOut => {
                        println!("HARNESS logged out, stopping");
                        std::process::exit(6);
                    }
                    BackendEvent::Error { message } => {
                        // The client socket needs a few seconds to come
                        // up; an early request fails with "not connected".
                        // Re-issue the request until the code arrives.
                        if message.contains("not connected")
                            && !saw_code
                            && retries < 20
                            && last_retry.elapsed() > Duration::from_secs(2)
                        {
                            retries += 1;
                            last_retry = Instant::now();
                            let _ = channels
                                .commands
                                .send(BackendCommand::ConnectPairCode { phone: phone.clone() });
                            println!("HARNESS pair code re-requested (retry {retries})");
                        }
                    }
                    _ => {}
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // Time-based retry: the error event only fires in
                // response to a request, so re-issue on a timer until
                // the client socket is up and the code arrives.
                if !saw_code && retries < 25 && last_retry.elapsed() > Duration::from_secs(4) {
                    retries += 1;
                    last_retry = Instant::now();
                    let _ = channels
                        .commands
                        .send(BackendCommand::ConnectPairCode { phone: phone.clone() });
                    println!(
                        "HARNESS pair code re-requested (timer retry {retries}) t={:.1}s",
                        started.elapsed().as_secs_f32()
                    );
                }
                if !saw_code && started.elapsed() > Duration::from_secs(240) {
                    println!("HARNESS timeout waiting for pair code");
                    std::process::exit(4);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                println!("HARNESS event channel closed");
                std::process::exit(7);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::{Arc, Mutex};

    /// In-memory sink standing in for stderr, whose contents a
    /// test can inspect after writing through a writer clone.
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Privacy audit C5: the writer the harness puts under its
    /// stderr subscriber is the app's ScrubWriter, so a library
    /// log line carrying a phone number or a secret shape is
    /// scrubbed before it can be shared as terminal output.
    #[test]
    fn harness_log_writer_scrubs_lines() {
        let sink = SharedBuf::default();
        let mut writer = core::debug::ScrubWriter::new(sink.clone());
        writer.write_all(b"connect failed for +62 812-3456-7890 token=zzz999888\n").unwrap();
        writer.flush().unwrap();
        let out = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
        assert!(!out.contains("812-3456"), "phone leaked through the harness writer: {out}");
        assert!(!out.contains("zzz999888"), "secret leaked through the harness writer: {out}");
    }
}
