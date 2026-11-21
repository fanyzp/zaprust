//! Voice-note playback: Ogg/Opus decoding and the output engine
//! that plays the decoded PCM.
//!
//! The playhead is the output's own consumption. A note's decoded
//! samples sit in a shared state, and the single [`pull`] function
//! is the only place the position advances: both output sinks
//! (the cpal device and the null test sink) advance it by
//! consuming samples through `pull`. The UI reads position,
//! duration and end-of-stream from the engine and never derives
//! progress from a clock, so a paused, stalled or absent output
//! cannot produce a moving playhead over silence.

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Sample rate of a decoded voice note. WhatsApp voice notes are
/// Opus; the decoder runs at 48 kHz, and a stereo file is
/// downmixed to mono while decoding.
pub(crate) const PCM_RATE: u32 = 48_000;

/// Cap on decoded samples: the recorder caps a note at five
/// minutes, and this leaves headroom above that while keeping a
/// hostile or corrupt file (silence compresses extremely well)
/// from allocating PCM without bound.
const MAX_PCM_SAMPLES: usize = 360 * PCM_RATE as usize;

/// Decodes an Ogg/Opus voice note's bytes to mono PCM at 48 kHz.
///
/// Anything that is not a well-formed Opus stream (a different
/// codec, a truncated file, unrelated bytes) is an error in user
/// words: the caller marks the note unavailable instead of
/// presenting silent playback. The stream's pre-skip and final
/// granule position are honored, so the PCM length is the note's
/// true length, not the container's padded one.
pub(crate) fn decode_voice(bytes: &[u8]) -> Result<Vec<i16>, String> {
    let mut reader = ogg::PacketReader::new(Cursor::new(bytes));
    let head = reader.read_packet().map_err(|_| not_playable())?.ok_or_else(not_playable)?;
    if head.data.len() < 19 || !head.data.starts_with(b"OpusHead") {
        return Err(not_playable());
    }
    let channels = match head.data[9] {
        1 => opus::Channels::Mono,
        2 => opus::Channels::Stereo,
        _ => return Err(not_playable()),
    };
    let stereo = channels == opus::Channels::Stereo;
    let pre_skip = usize::from(u16::from_le_bytes([head.data[10], head.data[11]]));
    let tags = reader.read_packet().map_err(|_| not_playable())?.ok_or_else(not_playable)?;
    if !tags.data.starts_with(b"OpusTags") {
        return Err(not_playable());
    }
    let mut decoder = opus::Decoder::new(PCM_RATE, channels).map_err(|_| not_playable())?;
    // One scratch buffer, reused per packet: 120 ms is the largest
    // Opus frame, 5760 samples per channel at 48 kHz.
    let mut frame = vec![0i16; 5760 * if stereo { 2 } else { 1 }];
    let mut pcm: Vec<i16> = Vec::new();
    let mut final_granule: u64 = 0;
    loop {
        let packet = match reader.read_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(_) => return Err("That voice note could not be decoded.".to_owned()),
        };
        // Pages that complete no packet carry the all-ones granule
        // sentinel; it is not a position and must not win the max.
        let absgp = packet.absgp_page();
        if absgp != u64::MAX && absgp > final_granule {
            final_granule = absgp;
        }
        if packet.data.is_empty() {
            continue;
        }
        let decoded = decoder
            .decode(&packet.data, &mut frame, false)
            .map_err(|_| "That voice note could not be decoded.".to_owned())?;
        if stereo {
            pcm.extend(
                frame[..decoded * 2]
                    .chunks_exact(2)
                    .map(|pair| ((i32::from(pair[0]) + i32::from(pair[1])) / 2) as i16),
            );
        } else {
            pcm.extend_from_slice(&frame[..decoded]);
        }
        if pcm.len() > MAX_PCM_SAMPLES {
            return Err("That voice note is too long to play.".to_owned());
        }
    }
    // The granule clock counts from the start of the stream,
    // pre-skip included: trim the head the encoder delayed by,
    // then cut any padding the final frame carried past the
    // announced end.
    if pre_skip > 0 {
        pcm.drain(..pre_skip.min(pcm.len()));
    }
    if final_granule > pre_skip as u64 {
        let announced = (final_granule - pre_skip as u64) as usize;
        if announced < pcm.len() {
            pcm.truncate(announced);
        }
    }
    if pcm.is_empty() {
        return Err("That voice note has no audio.".to_owned());
    }
    Ok(pcm)
}

/// The one decode failure that covers every "not an Opus voice
/// note" shape: the note may be a music file in another codec, or
/// its bytes may be damaged.
fn not_playable() -> String {
    "That voice note is not in a format Zaprust can play.".to_owned()
}

/// Which output a player runs against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VoiceSink {
    /// The machine's default output device, through cpal.
    Device,
    /// A sink that consumes the stream in real time and emits
    /// nothing. It exists so the engine's state machine runs on
    /// machines without speakers (CI, this development VM) and in
    /// tests; production playback uses the device.
    // Constructed by the engine tests and headless runs; a normal
    // build only ever selects the device sink.
    #[allow(dead_code)]
    Null,
}

/// Playback state shared between the UI and the output thread.
/// `pos` is measured in source frames and is advanced only by
/// [`pull`], at the sink's consumption rate.
struct PlayState {
    pcm: Arc<[i16]>,
    /// Source frames consumed so far. Fractional, because the
    /// playback rate and the sink's own rate both fold into the
    /// per-frame step.
    pos: f64,
    playing: bool,
    rate: f64,
    ended: bool,
}

fn lock(state: &Mutex<PlayState>) -> std::sync::MutexGuard<'_, PlayState> {
    state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The interpolated source sample at a fractional frame position.
fn sample_at(pcm: &[i16], pos: f64) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let last = pcm.len() - 1;
    let base = (pos.floor() as usize).min(last);
    let next = (base + 1).min(last);
    let frac = (pos - base as f64) as f32;
    let a = f32::from(pcm[base]) / 32768.0;
    let b = f32::from(pcm[next]) / 32768.0;
    a + (b - a) * frac.clamp(0.0, 1.0)
}

/// Fills `out` with the next mono frames of the stream, advancing
/// the position by what was actually consumed. `source_per_frame`
/// is how many source frames one output frame represents at rate
/// 1 (the source rate over the sink's rate). Paused or ended
/// streams emit silence and consume nothing. Reaching the end
/// flips `ended` and stops consumption mid-buffer; the rest of
/// the buffer is silence.
fn pull(state: &mut PlayState, out: &mut [f32], source_per_frame: f64) {
    let len = state.pcm.len() as f64;
    for slot in out.iter_mut() {
        if state.playing && !state.ended {
            *slot = sample_at(&state.pcm, state.pos);
            state.pos += state.rate * source_per_frame;
            if state.pos >= len {
                state.pos = len;
                state.playing = false;
                state.ended = true;
            }
        } else {
            *slot = 0.0;
        }
    }
}

/// One voice note loaded in the playback engine. Dropping it
/// stops the output thread and the sound with it.
pub(crate) struct VoicePlayer {
    /// App message ID of the note being played.
    pub(crate) id: u64,
    state: Arc<Mutex<PlayState>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VoicePlayer {
    /// Loads a decoded note and starts playing it. Starting is
    /// honest: the output must actually open before this returns
    /// Ok, so a machine with no output device fails here with the
    /// reason instead of playing to nowhere.
    pub(crate) fn start(
        id: u64,
        pcm: Arc<[i16]>,
        rate: f64,
        sink: VoiceSink,
    ) -> Result<Self, String> {
        let state = Arc::new(Mutex::new(PlayState {
            pcm,
            pos: 0.0,
            playing: true,
            rate: if rate > 0.0 { rate } else { 1.0 },
            ended: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        // Startup handshake: the worker reports whether the output
        // opened before `start` answers, so a missing device is an
        // immediate, plainly stated failure.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("zaprust-voice".into())
            .spawn(move || match sink {
                VoiceSink::Null => {
                    let _ = ready_tx.send(Ok(()));
                    run_null(thread_state, thread_stop);
                }
                VoiceSink::Device => run_device(thread_state, thread_stop, ready_tx),
            })
            .map_err(|e| format!("Could not start playback: {e}"))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { id, state, stop, thread: Some(thread) }),
            Ok(Err(message)) => {
                stop.store(true, Ordering::SeqCst);
                let _ = thread.join();
                Err(message)
            }
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                let _ = thread.join();
                Err("Playback stopped before it started.".to_owned())
            }
        }
    }

    /// Seconds of the note the output has consumed so far.
    pub(crate) fn position_secs(&self) -> f64 {
        lock(&self.state).pos / f64::from(PCM_RATE)
    }

    /// Progress through the note, 0.0 to 1.0.
    pub(crate) fn progress(&self) -> f32 {
        let state = lock(&self.state);
        if state.pcm.is_empty() {
            return 1.0;
        }
        (state.pos / state.pcm.len() as f64) as f32
    }

    pub(crate) fn is_playing(&self) -> bool {
        lock(&self.state).playing
    }

    /// True once the output has consumed the whole note.
    pub(crate) fn is_ended(&self) -> bool {
        lock(&self.state).ended
    }

    /// The rate the engine is really running at.
    pub(crate) fn rate(&self) -> f64 {
        lock(&self.state).rate
    }

    /// Pauses if playing, resumes if paused. A finished note does
    /// not resume here; the app restarts it by seeking to zero.
    pub(crate) fn toggle_pause(&self) {
        let mut state = lock(&self.state);
        if state.playing {
            state.playing = false;
        } else if !state.ended {
            state.playing = true;
        }
    }

    /// Resumes a paused note (no-op once it has ended).
    pub(crate) fn resume(&self) {
        let mut state = lock(&self.state);
        if !state.ended {
            state.playing = true;
        }
    }

    /// Changes the playback rate. The position is the sink's own
    /// count, so a rate change moves nothing by itself; only the
    /// speed of what follows changes.
    pub(crate) fn set_rate(&self, rate: f64) {
        if rate > 0.0 {
            lock(&self.state).rate = rate;
        }
    }

    /// Moves the playhead to a fraction of the note, keeping the
    /// playing/paused state. Seeking back from the end revives the
    /// session; seeking to the very end finishes it.
    pub(crate) fn seek_fraction(&self, fraction: f64) {
        let mut state = lock(&self.state);
        let len = state.pcm.len() as f64;
        state.pos = (fraction.clamp(0.0, 1.0) * len).clamp(0.0, len);
        if state.pos >= len {
            state.playing = false;
            state.ended = true;
        } else {
            state.ended = false;
        }
    }
}

impl Drop for VoicePlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The null sink's pump: consumes the stream at the PCM's own
/// rate in small real-time ticks, through the same [`pull`] the
/// device callback uses, and discards the samples.
fn run_null(state: Arc<Mutex<PlayState>>, stop: Arc<AtomicBool>) {
    let tick = std::time::Duration::from_millis(5);
    let frames_per_tick = (PCM_RATE as f64 * 0.005) as usize;
    let mut scratch = vec![0.0f32; frames_per_tick];
    while !stop.load(Ordering::SeqCst) {
        pull(&mut lock(&state), &mut scratch, 1.0);
        std::thread::sleep(tick);
    }
}

/// The device sink: owns the cpal output stream on this worker
/// thread (cpal streams are platform-bound and never leave it),
/// reports through `ready` whether the stream opened and started,
/// then parks until the player is dropped.
fn run_device(
    state: Arc<Mutex<PlayState>>,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        let _ = ready.send(Err("No audio output device found.".to_owned()));
        return;
    };
    let supported = match device.default_output_config() {
        Ok(config) => config,
        Err(e) => {
            let _ = ready.send(Err(format!("Audio output unavailable: {e}")));
            return;
        }
    };
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let stream = match format {
        cpal::SampleFormat::F32 => build_output::<f32>(&device, &config, Arc::clone(&state), |s| s),
        cpal::SampleFormat::I16 => build_output::<i16>(&device, &config, Arc::clone(&state), |s| {
            (s.clamp(-1.0, 1.0) * 32767.0) as i16
        }),
        cpal::SampleFormat::U16 => build_output::<u16>(&device, &config, Arc::clone(&state), |s| {
            ((s.clamp(-1.0, 1.0) * 0.5 + 0.5) * 65535.0) as u16
        }),
        other => Err(format!("The audio output format {other:?} is not supported.")),
    };
    let stream = match stream {
        Ok(stream) => stream,
        Err(message) => {
            let _ = ready.send(Err(message));
            return;
        }
    };
    if let Err(e) = stream.play() {
        let _ = ready.send(Err(format!("Could not start the audio output: {e}")));
        return;
    }
    let _ = ready.send(Ok(()));
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // The stream drops here, with the thread.
}

/// Builds one output stream whose callback drains the shared
/// state through [`pull`] and writes the mono result to every
/// channel of the device buffer.
fn build_output<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    state: Arc<Mutex<PlayState>>,
    convert: impl Fn(f32) -> T + Send + 'static,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample,
{
    use cpal::traits::DeviceTrait;
    let channels = usize::from(config.channels).max(1);
    let source_per_frame = f64::from(PCM_RATE) / f64::from(config.sample_rate);
    let mut scratch: Vec<f32> = Vec::new();
    device
        .build_output_stream(
            *config,
            move |data: &mut [T], _info| {
                let frames = data.len() / channels;
                scratch.clear();
                scratch.resize(frames, 0.0);
                pull(&mut lock(&state), &mut scratch, source_per_frame);
                for (i, sample) in scratch.iter().enumerate() {
                    for channel in 0..channels {
                        data[i * channels + channel] = convert(*sample);
                    }
                }
            },
            |err| tracing::warn!("audio output stream error: {err}"),
            None,
        )
        .map_err(|e| format!("Could not open the audio output: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A one-second 440 Hz tone at a healthy level, as PCM.
    fn tone_pcm(secs: f64) -> Vec<i16> {
        let n = (f64::from(PCM_RATE) * secs) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / f64::from(PCM_RATE);
                ((t * 440.0 * std::f64::consts::TAU).sin() * 16_000.0) as i16
            })
            .collect()
    }

    fn rms(pcm: &[i16]) -> f64 {
        if pcm.is_empty() {
            return 0.0;
        }
        let sum: f64 = pcm
            .iter()
            .map(|s| {
                let v = f64::from(*s) / 32768.0;
                v * v
            })
            .sum();
        (sum / pcm.len() as f64).sqrt()
    }

    /// The bytes the recorder produces for a tone decode back to
    /// the tone: this is the round trip every sent note depends
    /// on (record here, play here), exercised with the app's own
    /// encoder on one side and the player on the other.
    #[test]
    fn decode_round_trips_a_recorded_note() {
        let pcm = tone_pcm(1.0);
        let bytes = crate::recorder::encode_ogg_opus(&pcm).expect("the tone encodes");
        let decoded = decode_voice(&bytes).expect("a recorded note decodes");
        assert!(
            (decoded.len() as i64 - pcm.len() as i64).abs() <= 960,
            "decoded {} samples from {} recorded",
            decoded.len(),
            pcm.len()
        );
        assert!(rms(&decoded) > 0.2, "the tone survives the round trip, rms {}", rms(&decoded));
    }

    #[test]
    fn decode_of_silence_is_silence() {
        let bytes = crate::recorder::encode_ogg_opus(&vec![0i16; PCM_RATE as usize])
            .expect("silence encodes");
        let decoded = decode_voice(&bytes).expect("silence decodes");
        assert!(rms(&decoded) < 0.01, "silence stays silent, rms {}", rms(&decoded));
    }

    #[test]
    fn decode_rejects_bytes_that_are_not_an_opus_note() {
        assert!(decode_voice(&[]).is_err());
        assert!(decode_voice(b"this is not an ogg stream at all").is_err());
        // A PNG (the other media kind's shape) is not playable audio.
        assert!(decode_voice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]).is_err());
        // A real note, truncated mid-stream, fails instead of
        // playing a fragment as if it were whole.
        let bytes = crate::recorder::encode_ogg_opus(&tone_pcm(1.0)).unwrap();
        assert!(decode_voice(&bytes[..bytes.len() / 3]).is_err());
    }

    fn play_state(pcm: Vec<i16>) -> PlayState {
        PlayState { pcm: Arc::from(pcm), pos: 0.0, playing: true, rate: 1.0, ended: false }
    }

    /// `pull` is the only place position advances: it advances by
    /// consumption, stops at the end, and a paused stream consumes
    /// nothing while emitting silence.
    #[test]
    fn pull_advances_by_consumption_and_stops_at_the_end() {
        let mut state = play_state(vec![8_000; 1_000]);
        let mut out = [0.0f32; 400];
        pull(&mut state, &mut out, 1.0);
        assert_eq!(state.pos, 400.0);
        assert!(out.iter().all(|s| *s > 0.2), "the tone's samples come out");
        assert!(!state.ended);
        // Paused: silence out, nothing consumed.
        state.playing = false;
        pull(&mut state, &mut out, 1.0);
        assert_eq!(state.pos, 400.0, "a paused stream consumes nothing");
        assert!(out.iter().all(|s| *s == 0.0), "a paused stream is silent");
        // Resumed at 2x into a 500-frame buffer: 600 remain, so the
        // buffer ends the stream partway and pads with silence.
        state.playing = true;
        state.rate = 2.0;
        let mut tail = [0.0f32; 500];
        pull(&mut state, &mut tail, 1.0);
        assert!(state.ended, "consumption reached the end");
        assert_eq!(state.pos, 1_000.0);
        assert!(!state.playing);
        assert!(tail[299] > 0.2 && tail[300] == 0.0, "sound stops exactly at the end");
    }

    /// A sink running at twice the source rate consumes half a
    /// source frame per output frame at rate 1: the rate folding
    /// is what keeps pitch and duration honest on any device.
    #[test]
    fn pull_folds_the_sink_rate_into_the_step() {
        let mut state = play_state(vec![8_000; 1_000]);
        let mut out = [0.0f32; 200];
        pull(&mut state, &mut out, 0.5);
        assert_eq!(state.pos, 100.0);
    }

    /// Waits until `cond` holds or the deadline passes; engine
    /// tests run against a real-time sink, so they poll rather
    /// than sleep exact durations.
    fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    #[test]
    fn null_sink_playback_advances_pauses_and_resumes() {
        let player = VoicePlayer::start(7, Arc::from(tone_pcm(2.0)), 1.0, VoiceSink::Null).unwrap();
        assert!(player.is_playing());
        assert!(
            wait_until(Duration::from_secs(2), || player.position_secs() > 0.05),
            "the sink consumes the stream, position {}",
            player.position_secs()
        );
        player.toggle_pause();
        assert!(!player.is_playing());
        // Let any in-flight tick settle, then the position must be
        // frozen for as long as the pause lasts.
        std::thread::sleep(Duration::from_millis(30));
        let frozen = player.position_secs();
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(player.position_secs(), frozen, "pause freezes the real position");
        player.toggle_pause();
        assert!(player.is_playing());
        assert!(
            wait_until(Duration::from_secs(2), || player.position_secs() > frozen + 0.05),
            "resume consumes again"
        );
    }

    #[test]
    fn seek_and_rate_move_the_real_position() {
        let player = VoicePlayer::start(7, Arc::from(tone_pcm(2.0)), 1.0, VoiceSink::Null).unwrap();
        player.toggle_pause();
        player.seek_fraction(0.5);
        assert!(
            (player.position_secs() - 1.0).abs() < 0.01,
            "seek lands at half of 2 s, got {}",
            player.position_secs()
        );
        assert!(!player.is_playing(), "seeking while paused stays paused");
        player.set_rate(2.0);
        assert_eq!(player.rate(), 2.0);
        player.toggle_pause();
        // The pump consumes in real time, so how far it gets in
        // one fixed sleep depends on the CPU this machine gives
        // it: under parallel build load the pump starves and an
        // exact-window assertion flakes. Poll for the target
        // instead, with two load-immune checks. Load can only
        // slow the pump, never speed it past real time, so the
        // position may never outrun the consumption the rate
        // allows for the elapsed wall time, and reaching the
        // target has a physical minimum duration at 2x (0.30 s
        // of note is 30 pump ticks, each followed by a 5 ms
        // sleep), so a rate applied twice still fails. That the
        // rate is not silently ignored is pinned by `rate()`
        // above, which reads the very field `pull` consumes by,
        // and by the deterministic `pull` tests; the claim here
        // is that the running pump really consumes at that rate.
        let start = player.position_secs();
        let began = Instant::now();
        let reached = wait_until(Duration::from_secs(10), || {
            let elapsed = began.elapsed().as_secs_f64();
            let advanced = player.position_secs() - start;
            assert!(
                advanced <= 2.0 * elapsed + 0.05,
                "the position outran 2x consumption: {advanced} s of note in {elapsed} s"
            );
            advanced >= 0.30
        });
        let elapsed = began.elapsed();
        assert!(reached, "the pump consumes at 2x, position {}", player.position_secs());
        assert!(
            elapsed >= Duration::from_millis(120),
            "0.30 s of note at 2x takes at least ~150 ms of pump time, took {elapsed:?}"
        );
        // Seeking to the very end finishes the session.
        player.seek_fraction(1.0);
        assert!(player.is_ended());
        assert!(!player.is_playing());
        // And seeking back revives it, paused state kept.
        player.seek_fraction(0.25);
        assert!(!player.is_ended());
        assert!((player.position_secs() - 0.5).abs() < 0.01);
    }

    #[test]
    fn stream_end_ends_the_session() {
        let player = VoicePlayer::start(7, Arc::from(tone_pcm(0.3)), 1.0, VoiceSink::Null).unwrap();
        assert!(
            wait_until(Duration::from_secs(3), || player.is_ended()),
            "a 0.3 s note ends on its own"
        );
        assert!(!player.is_playing());
        assert!((player.progress() - 1.0).abs() < 1e-6);
        assert!((player.position_secs() - 0.3).abs() < 1e-9, "the whole 0.3 s note was consumed");
    }
}
