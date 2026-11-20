//! Call audio: the bridge between the protocol library's audio
//! ports (16 kHz mono i16 PCM, 960-sample / 60 ms frames) and the
//! devices a call actually uses.
//!
//! Two implementations behind the same shape:
//! - [`CallAudio::real`] owns cpal streams on a dedicated thread
//!   (cpal `Stream` values are not `Send` on some platforms, so
//!   they never leave that thread). Capture is downmixed to mono,
//!   resampled to 16 kHz and chunked into frames; playout runs
//!   the reverse into a lock-free-ish ring the output callback
//!   drains. Nothing in the callbacks allocates per frame beyond
//!   the frame hand-off the trait API requires.
//! - [`CallAudio::fake`] produces silence frames and counts what
//!   it is handed, for tests and headless runs. It makes no sound
//!   and listens to nothing; it exists so frame flow is testable
//!   without a microphone.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use whatsapp_rust::async_channel::{Receiver, Sender};
use whatsapp_rust::voip::{AudioSink, AudioSource};

/// Samples per frame: 60 ms at 16 kHz.
pub(crate) const FRAME_SAMPLES: usize = 960;
/// Engine sample rate.
pub(crate) const RATE: u32 = 16_000;

/// A microphone endpoint: hands the engine a receiver of frames.
#[derive(Clone)]
pub(crate) struct ChannelSource {
    rx: Receiver<Vec<i16>>,
}

impl AudioSource for ChannelSource {
    fn frames(&self) -> Receiver<Vec<i16>> {
        self.rx.clone()
    }
}

/// A speaker endpoint: hands the engine a sender for frames.
#[derive(Clone)]
pub(crate) struct ChannelSink {
    tx: Sender<Vec<i16>>,
}

impl AudioSink for ChannelSink {
    fn playout(&self) -> Sender<Vec<i16>> {
        self.tx.clone()
    }
}

/// One call's audio plumbing: the two ports plus the lifecycle
/// handle for whatever device machinery sits behind them.
pub(crate) struct CallAudio {
    pub source: ChannelSource,
    pub sink: ChannelSink,
    /// Frames handed to playout so far.
    #[allow(dead_code)]
    pub frames_played: Arc<AtomicU64>,
    /// Requests the alternate (speaker) output device.
    pub speaker: Arc<AtomicBool>,
    stop: std::sync::mpsc::Sender<()>,
}

impl CallAudio {
    /// Stops the device machinery (idempotent).
    pub(crate) fn shutdown(&self) {
        let _ = self.stop.send(());
    }

    /// Silence devices: a 60 ms ticker feeds zero frames; playout
    /// frames are counted and dropped.
    #[allow(dead_code)]
    pub(crate) fn fake() -> Self {
        let (frame_tx, frame_rx) = whatsapp_rust::async_channel::bounded::<Vec<i16>>(32);
        let (play_tx, play_rx) = whatsapp_rust::async_channel::bounded::<Vec<i16>>(32);
        let frames_played = Arc::new(AtomicU64::new(0));
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("zaprust-audio-fake-in".to_owned())
            .spawn(move || loop {
                if stop_rx.try_recv().is_ok() {
                    return;
                }
                let _ = frame_tx.try_send(vec![0i16; FRAME_SAMPLES]);
                std::thread::sleep(std::time::Duration::from_millis(60));
            })
            .ok();
        {
            let played = frames_played.clone();
            std::thread::Builder::new()
                .name("zaprust-audio-fake-out".to_owned())
                .spawn(move || {
                    while play_rx.recv_blocking().is_ok() {
                        played.fetch_add(1, Ordering::Relaxed);
                    }
                })
                .ok();
        }
        Self {
            source: ChannelSource { rx: frame_rx },
            sink: ChannelSink { tx: play_tx },
            frames_played,
            speaker: Arc::new(AtomicBool::new(false)),
            stop: stop_tx,
        }
    }

    /// Real devices through cpal. `speaker` selects the alternate
    /// output device at open time when one exists. Errors are
    /// user-readable: no device, a busy device, or a denied
    /// microphone permission all land here.
    pub(crate) fn real(speaker_on: bool) -> Result<Self, String> {
        let frames_played = Arc::new(AtomicU64::new(0));
        let speaker = Arc::new(AtomicBool::new(speaker_on));
        let (frame_tx, frame_rx) = whatsapp_rust::async_channel::bounded::<Vec<i16>>(32);
        let (play_tx, play_rx) = whatsapp_rust::async_channel::bounded::<Vec<i16>>(32);
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        {
            let played = frames_played.clone();
            let speaker_flag = speaker.clone();
            std::thread::Builder::new()
                .name("zaprust-audio".to_owned())
                .spawn(move || {
                    audio_thread(frame_tx, play_rx, stop_rx, ready_tx, played, speaker_flag);
                })
                .map_err(|e| format!("Could not start the audio thread: {e}"))?;
        }
        ready_rx
            .recv()
            .map_err(|_| "The audio thread stopped before opening the devices.".to_owned())??;
        Ok(Self {
            source: ChannelSource { rx: frame_rx },
            sink: ChannelSink { tx: play_tx },
            frames_played,
            speaker,
            stop: stop_tx,
        })
    }
}

/// Linear-interpolation resampler between two rates, fed with
/// arbitrary chunks. Keeps the fractional position so chunk
/// boundaries do not drift.
pub(crate) struct Resampler {
    step: f64,
    /// Input samples not yet consumed.
    backlog: std::collections::VecDeque<i16>,
    /// Fractional read position inside the backlog (in samples).
    pos: f64,
    /// Last consumed input sample, for interpolation across
    /// chunk boundaries.
    last: i16,
}

impl Resampler {
    pub(crate) fn new(from: u32, to: u32) -> Self {
        Self {
            step: f64::from(from) / f64::from(to.max(1)),
            backlog: std::collections::VecDeque::new(),
            pos: 0.0,
            last: 0,
        }
    }

    /// Feeds input samples, appends produced output samples to
    /// `out`.
    pub(crate) fn push(&mut self, input: &[i16], out: &mut Vec<i16>) {
        self.backlog.extend(input.iter().copied());
        loop {
            // `pos` is a fractional sample position: finite and
            // never negative, and an index at or past the backlog
            // end simply ends the loop below, so clamping the
            // cast to the backlog length loses nothing.
            let idx = (self.pos.floor() as usize).min(self.backlog.len());
            if idx + 1 >= self.backlog.len() {
                // Not enough input to interpolate the next output
                // sample. Drop the consumed prefix, remembering
                // the fraction for the next chunk.
                let consumed = idx.min(self.backlog.len());
                for _ in 0..consumed {
                    if let Some(v) = self.backlog.pop_front() {
                        self.last = v;
                    }
                }
                self.pos -= consumed as f64;
                return;
            }
            // `idx` is the floor of `pos` on this path, so the
            // fraction lies in [0, 1) by construction; the clamp
            // states that range at the cast instead of relying
            // on it silently.
            let frac = (self.pos - idx as f64).clamp(0.0, 1.0) as f32;
            let a = self.backlog[idx];
            let b = self.backlog[idx + 1];
            let v = f32::from(a) + (f32::from(b) - f32::from(a)) * frac;
            out.push(v.round().clamp(-32_768.0, 32_767.0) as i16);
            self.pos += self.step;
        }
    }
}

/// The cpal-owning thread: builds both streams, reports readiness
/// once, then parks until shutdown, rebuilding the output stream
/// when the speaker flag flips.
fn audio_thread(
    frame_tx: Sender<Vec<i16>>,
    play_rx: Receiver<Vec<i16>>,
    stop_rx: std::sync::mpsc::Receiver<()>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
    played: Arc<AtomicU64>,
    speaker_flag: Arc<AtomicBool>,
) {
    use cpal::traits::HostTrait;
    use cpal::traits::StreamTrait;

    let host = cpal::default_host();
    let Some(input_device) = host.default_input_device() else {
        let _ = ready_tx.send(Err(
            "No microphone was found. Connect one, or check the system microphone permission."
                .to_owned(),
        ));
        return;
    };
    let input_stream = match build_input_stream(&input_device, frame_tx) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    // Playout ring: the drainer thread fills it, the output
    // callback empties it.
    let ring: Arc<std::sync::Mutex<std::collections::VecDeque<i16>>> =
        Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    {
        let ring = ring.clone();
        let played = played.clone();
        std::thread::Builder::new()
            .name("zaprust-audio-out".to_owned())
            .spawn(move || {
                while let Ok(frame) = play_rx.recv_blocking() {
                    played.fetch_add(1, Ordering::Relaxed);
                    if let Ok(mut q) = ring.lock() {
                        // Cap the ring near half a second; VoIP is
                        // loss tolerant and stale audio is worse
                        // than a gap.
                        if q.len() < RATE as usize / 2 {
                            q.extend(frame.iter().copied());
                        }
                    }
                }
            })
            .ok();
    }
    let mut current_speaker = speaker_flag.load(Ordering::Relaxed);
    let output_device = pick_output_device(&host, current_speaker);
    let mut output_stream = if let Some(device) = output_device {
        match build_output_stream(&device, ring.clone()) {
            Ok(s) => s,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        }
    } else {
        let _ = ready_tx.send(Err("No output device was found.".to_owned()));
        return;
    };
    if let Err(e) = input_stream.play().and_then(|()| output_stream.play()) {
        let _ = ready_tx.send(Err(format!(
            "The audio device refused to start ({e}). It may be busy, or microphone permission was denied."
        )));
        return;
    }
    let _ = ready_tx.send(Ok(()));
    // Park: keep the streams alive until shutdown or a device
    // re-route request.
    loop {
        match stop_rx.recv_timeout(std::time::Duration::from_millis(250)) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        let want = speaker_flag.load(Ordering::Relaxed);
        if want != current_speaker {
            current_speaker = want;
            if let Some(device) = pick_output_device(&host, current_speaker) {
                if let Ok(stream) = build_output_stream(&device, ring.clone()) {
                    if stream.play().is_ok() {
                        // Replacing the stream drops (stops) the
                        // previous device.
                        output_stream = stream;
                    }
                }
            }
        }
        // The current stream must stay alive for the whole call.
        let _ = &output_stream;
    }
}

/// Default output device, or the first other one when `alternate`
/// is set (desktop "speaker" routing).
fn pick_output_device(host: &cpal::Host, alternate: bool) -> Option<cpal::Device> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let default = host.default_output_device()?;
    if !alternate {
        return Some(default);
    }
    let default_id = default.id().ok();
    host.output_devices().ok()?.find(|d| d.id().ok() != default_id).or(Some(default))
}

/// Builds the capture stream: native format to mono 16 kHz,
/// chunked into engine frames. Data-conversion goes through f32
/// for the three sample formats cpal hands out.
fn build_input_stream(
    device: &cpal::Device,
    frame_tx: Sender<Vec<i16>>,
) -> Result<cpal::Stream, String> {
    use cpal::traits::DeviceTrait;
    // Capture state shared by the format-specific callbacks.
    struct Capture {
        resampler: Resampler,
        pending: Vec<i16>,
        frame_tx: Sender<Vec<i16>>,
        channels: usize,
        scratch: Vec<i16>,
    }
    impl Capture {
        fn feed(&mut self, mono: &[i16]) {
            self.scratch.clear();
            self.resampler.push(mono, &mut self.scratch);
            self.pending.extend_from_slice(&self.scratch);
            while self.pending.len() >= FRAME_SAMPLES {
                let frame: Vec<i16> = self.pending.drain(..FRAME_SAMPLES).collect();
                let _ = self.frame_tx.try_send(frame);
            }
        }
    }
    let config = device
        .default_input_config()
        .map_err(|e| format!("Microphone unavailable or permission denied: {e}"))?;
    let channels = usize::from(config.channels());
    let native_rate = config.sample_rate();
    let format = config.sample_format();
    let err_fn = |e: cpal::Error| {
        // Device errors mid-call are not actionable per-sample;
        // the engine notices silence and the call UI reports the
        // ended call. Nothing to do here.
        let _ = e;
    };
    let stream_config: cpal::StreamConfig = config.into();
    let mut capture = Capture {
        resampler: Resampler::new(native_rate, RATE),
        pending: Vec::new(),
        frame_tx,
        channels,
        scratch: Vec::new(),
    };
    macro_rules! build {
        ($t:ty, $conv:expr) => {{
            let conv: fn(&[$t], usize) -> Vec<i16> = $conv;
            device.build_input_stream(
                stream_config.clone(),
                move |data: &[$t], _| {
                    let mono = conv(data, capture.channels);
                    capture.feed(&mono);
                },
                err_fn,
                None,
            )
        }};
    }
    let stream = match format {
        cpal::SampleFormat::F32 => build!(f32, |data: &[f32], ch: usize| {
            downmix(data.iter().map(|s| float_to_i16(*s)), ch)
        }),
        cpal::SampleFormat::I16 => {
            build!(i16, |data: &[i16], ch: usize| { downmix(data.iter().copied(), ch) })
        }
        cpal::SampleFormat::U16 => build!(u16, |data: &[u16], ch: usize| {
            downmix(data.iter().map(|s| (i32::from(*s) - 32_768) as i16), ch)
        }),
        _ => return Err("The microphone uses a sample format Zaprust cannot read.".to_owned()),
    };
    stream.map_err(|e| format!("Microphone unavailable or permission denied: {e}"))
}

/// Builds the playout stream: ring samples (16 kHz mono) are
/// resampled to the device rate and spread across its channels.
fn build_output_stream(
    device: &cpal::Device,
    ring: Arc<std::sync::Mutex<std::collections::VecDeque<i16>>>,
) -> Result<cpal::Stream, String> {
    use cpal::traits::DeviceTrait;
    struct Playout {
        resampler: Resampler,
        ring: Arc<std::sync::Mutex<std::collections::VecDeque<i16>>>,
        staged: std::collections::VecDeque<i16>,
        channels: usize,
        /// Device sample rate (the resampler's target).
        rate: u32,
    }
    impl Playout {
        /// Fills `frames` mono output samples at the device rate.
        fn fill(&mut self, frames: usize, out: &mut Vec<i16>) {
            let want_in =
                ((frames as f64) * (f64::from(RATE) / f64::from(self.rate.max(1)))) as usize + 2;
            if self.staged.len() < want_in {
                if let Ok(mut q) = self.ring.lock() {
                    let take = (want_in - self.staged.len()).min(q.len());
                    let have: Vec<i16> = q.drain(..take).collect();
                    self.staged.extend(have);
                }
            }
            let input: Vec<i16> = self.staged.drain(..).collect();
            let mut produced = Vec::new();
            self.resampler.push(&input, &mut produced);
            produced.resize(frames, 0);
            *out = produced;
        }
    }
    let config =
        device.default_output_config().map_err(|e| format!("No working output device: {e}"))?;
    let channels = usize::from(config.channels()).max(1);
    let native_rate = config.sample_rate();
    let format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.into();
    let err_fn = |e: cpal::Error| {
        let _ = e;
    };
    let mut playout = Playout {
        resampler: Resampler::new(RATE, native_rate),
        ring,
        staged: std::collections::VecDeque::new(),
        channels,
        rate: native_rate,
    };
    macro_rules! build {
        ($t:ty, $conv:expr) => {{
            let conv: fn(i16) -> $t = $conv;
            device.build_output_stream(
                stream_config.clone(),
                move |data: &mut [$t], _| {
                    let frames = data.len() / playout.channels;
                    let mut mono = Vec::new();
                    playout.fill(frames, &mut mono);
                    for (i, sample) in data.iter_mut().enumerate() {
                        let v = mono.get(i / playout.channels).copied().unwrap_or(0);
                        *sample = conv(v);
                    }
                },
                err_fn,
                None,
            )
        }};
    }
    let stream = match format {
        cpal::SampleFormat::F32 => build!(f32, |v: i16| i16_to_float(v)),
        cpal::SampleFormat::I16 => build!(i16, |v: i16| v),
        cpal::SampleFormat::U16 => build!(u16, |v: i16| (i32::from(v) + 32_768) as u16),
        _ => return Err("The output device uses a sample format Zaprust cannot write.".to_owned()),
    };
    stream.map_err(|e| format!("No working output device: {e}"))
}

/// Averages one frame of interleaved samples per channel.
fn downmix(samples: impl Iterator<Item = i16>, channels: usize) -> Vec<i16> {
    let channels = channels.max(1);
    samples
        .collect::<Vec<_>>()
        .chunks(channels)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|s| i32::from(*s)).sum();
            (sum / channels.max(1) as i32) as i16
        })
        .collect()
}

fn float_to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32_767.0) as i16
}

fn i16_to_float(v: i16) -> f32 {
    f32::from(v) / 32_768.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_identity_rate_passes_samples_through() {
        let mut r = Resampler::new(16_000, 16_000);
        let input: Vec<i16> = (0..2000).map(|i| (i % 300) as i16).collect();
        let mut out = Vec::new();
        r.push(&input, &mut out);
        // One sample of lookahead is always held back for
        // interpolation; everything before it must match exactly.
        assert!(out.len() >= input.len() - 1, "{} vs {}", out.len(), input.len());
        assert_eq!(&out[..], &input[..out.len()]);
    }

    #[test]
    fn resampler_48k_to_16k_thirds_the_count() {
        let mut r = Resampler::new(48_000, 16_000);
        let input = vec![1000i16; 4800];
        let mut out = Vec::new();
        r.push(&input, &mut out);
        assert!((1590..=1600).contains(&out.len()), "got {}", out.len());
        assert!(out.iter().all(|v| *v == 1000), "constant signal stays constant");
    }

    #[test]
    fn fake_audio_flows_frames_both_ways() {
        let audio = CallAudio::fake();
        let out_tx = audio.sink.playout();
        let in_rx = audio.source.frames();
        // Playout frames are counted by the drainer.
        for _ in 0..3 {
            out_tx.try_send(vec![0i16; FRAME_SAMPLES]).expect("sink accepts");
        }
        // Capture frames arrive from the ticker.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut got = 0;
        while std::time::Instant::now() < deadline && got < 2 {
            if in_rx.try_recv().is_ok() {
                got += 1;
            } else {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert!(got >= 2, "fake source produced frames");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while audio.frames_played.load(Ordering::Relaxed) < 3
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(audio.frames_played.load(Ordering::Relaxed), 3);
        audio.shutdown();
    }
}
