//! Voice-note recording: a cpal input stream captured to PCM in a
//! dedicated thread, encoded to an Opus-in-Ogg voice note on stop.
//! The UI reads elapsed time and recent level taps for the
//! recording bar; the bytes only exist when the user sends.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Target capture format for voice notes: WhatsApp voice notes are
/// 48 kHz mono Opus.
const TARGET_RATE: u32 = 48_000;
/// Opus frame length used for encoding: 20 ms at 48 kHz.
const FRAME: usize = 960;
/// Recording cap: levels and PCM stop growing past five minutes,
/// and the composer auto-sends at the cap. Long enough for a real
/// note, small enough that the PCM buffer stays modest.
pub(crate) const MAX_SECS: u64 = 300;
/// One level tap per 50 ms, enough resolution for a live waveform.
const LEVEL_CHUNK: usize = TARGET_RATE as usize / 20;

/// Shared capture buffer between the cpal thread and the UI.
#[derive(Default)]
struct Capture {
    samples: Vec<i16>,
    /// RMS level per 50 ms chunk, 0.0-1.0, newest last.
    levels: Vec<f32>,
    chunk_acc: Vec<i16>,
    capped: bool,
}

/// A running voice recording. Dropping it stops the capture thread
/// and discards the audio; call [`Recorder::finish`] to encode.
pub(crate) struct Recorder {
    capture: Arc<Mutex<Capture>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    started: Instant,
}

impl Recorder {
    /// Starts capturing the default input device. Fails honestly
    /// when there is no microphone or the host refuses the stream.
    pub(crate) fn start() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or("No microphone found.")?;
        let supported =
            device.default_input_config().map_err(|e| format!("Microphone unavailable: {e}"))?;
        let capture = Arc::new(Mutex::new(Capture::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let err_fn = |err: cpal::Error| {
            // Stream errors surface as silence, not a crash; the
            // recording bar keeps running and the user can cancel.
            tracing::warn!("microphone stream error: {err}");
        };

        // The stream lives inside the worker thread (cpal streams
        // are platform-bound); the stop flag ends the thread and
        // drops the stream with it.
        let thread_capture = Arc::clone(&capture);
        let thread_stop = Arc::clone(&stop);
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let thread = std::thread::Builder::new()
            .name("zaprust-recorder".into())
            .spawn(move || {
                let push = |samples: &[i16], cap: &mut Capture| {
                    if cap.capped {
                        return;
                    }
                    cap.samples.extend_from_slice(samples);
                    cap.chunk_acc.extend_from_slice(samples);
                    while cap.chunk_acc.len() >= LEVEL_CHUNK {
                        let chunk: &[i16] = &cap.chunk_acc[..LEVEL_CHUNK];
                        let sum: f32 = chunk
                            .iter()
                            .map(|s| {
                                let v = f32::from(*s) / 32768.0;
                                v * v
                            })
                            .sum();
                        cap.levels.push((sum / LEVEL_CHUNK as f32).sqrt().clamp(0.0, 1.0));
                        cap.chunk_acc.drain(..LEVEL_CHUNK);
                    }
                    if cap.samples.len() >= MAX_SECS as usize * TARGET_RATE as usize {
                        cap.capped = true;
                    }
                };
                match sample_format {
                    cpal::SampleFormat::F32 => run_stream::<f32, _>(
                        &device,
                        config,
                        &thread_stop,
                        err_fn,
                        thread_capture,
                        move |d, c| {
                            let scaled: Vec<i16> =
                                d.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16).collect();
                            push(&scaled, c);
                        },
                    ),
                    cpal::SampleFormat::I16 => run_stream::<i16, _>(
                        &device,
                        config,
                        &thread_stop,
                        err_fn,
                        thread_capture,
                        push,
                    ),
                    cpal::SampleFormat::U16 => run_stream::<u16, _>(
                        &device,
                        config,
                        &thread_stop,
                        err_fn,
                        thread_capture,
                        move |d, c| {
                            let scaled: Vec<i16> =
                                d.iter().map(|s| (*s as i32 - 32768) as i16).collect();
                            push(&scaled, c);
                        },
                    ),
                    other => tracing::warn!("unsupported input sample format {other:?}"),
                }
            })
            .map_err(|e| format!("Could not start the recorder: {e}"))?;
        Ok(Self { capture, stop, thread: Some(thread), started: Instant::now() })
    }

    /// Seconds since the recording started.
    pub(crate) fn elapsed_secs(&self) -> u64 {
        self.started.elapsed().as_secs().min(MAX_SECS)
    }

    /// The last `bars` level taps, resampled to exactly `bars`
    /// values in 0.0-1.0 for the live waveform.
    pub(crate) fn levels(&self, bars: usize) -> Vec<f32> {
        let guard = self.capture.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        resample_bars(&guard.levels, bars)
    }

    /// Whether the capture hit the length cap (the composer then
    /// auto-sends the note).
    pub(crate) fn capped(&self) -> bool {
        self.capture.lock().unwrap_or_else(std::sync::PoisonError::into_inner).capped
    }

    /// Stops the capture and encodes the note to Ogg/Opus. Returns
    /// the file bytes and its duration in whole seconds.
    pub(crate) fn finish(mut self) -> Result<(Vec<u8>, u32), String> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // The stream thread is gone, so the UI's levels() call is
        // the only other possible clone holder; the swap hands the
        // Arc out of the Drop-guarded self and both paths below
        // unwrap the capture by value.
        let capture_arc =
            std::mem::replace(&mut self.capture, Arc::new(Mutex::new(Capture::default())));
        let capture = match Arc::try_unwrap(capture_arc) {
            Ok(mutex) => mutex.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner),
            Err(arc) => {
                let mut guard = arc.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                std::mem::take(&mut *guard)
            }
        };
        let secs = u32::try_from(capture.samples.len() / TARGET_RATE as usize).unwrap_or(0);
        if capture.samples.is_empty() {
            return Err("Nothing was recorded.".to_owned());
        }
        let bytes = encode_ogg_opus(&capture.samples)?;
        Ok((bytes, secs))
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // A recorder dropped without finish (cancel path) stops the
        // thread and discards the audio.
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Builds and runs one input stream, feeding every callback through
/// `push` against the shared capture. Parks the calling thread until
/// `stop` is raised, then returns (dropping the stream).
fn run_stream<T, F>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    stop: &AtomicBool,
    err_fn: impl FnMut(cpal::Error) + Send + 'static,
    capture: Arc<Mutex<Capture>>,
    mut push: F,
) where
    T: cpal::SizedSample,
    F: FnMut(&[T], &mut Capture) + Send + 'static,
{
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _info| {
            let mut cap = capture.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            push(data, &mut cap);
        },
        err_fn,
        None,
    );
    match stream {
        Ok(stream) => {
            if stream.play().is_ok() {
                // Park until stop: the stream is owned by this frame
                // and only stays alive while the thread runs.
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        Err(e) => tracing::warn!("could not open the microphone: {e}"),
    }
}

fn resample_bars(levels: &[f32], bars: usize) -> Vec<f32> {
    if levels.is_empty() {
        return vec![0.0; bars];
    }
    if levels.len() <= bars {
        let mut out = levels.to_vec();
        out.resize(bars, 0.0);
        return out;
    }
    (0..bars)
        .map(|i| {
            let start = i * levels.len() / bars;
            let end = ((i + 1) * levels.len() / bars).max(start + 1);
            levels[start..end].iter().cloned().fold(0.0_f32, f32::max)
        })
        .collect()
}

/// Encodes mono i16 PCM at 48 kHz into an Opus-in-Ogg voice note
/// (OpusHead, OpusTags, 20 ms frames, 48 kHz granule clock).
fn encode_ogg_opus(pcm: &[i16]) -> Result<Vec<u8>, String> {
    let mut encoder =
        opus::Encoder::new(TARGET_RATE, opus::Channels::Mono, opus::Application::Audio)
            .map_err(|e| format!("Opus encoder: {e}"))?;
    encoder.set_bitrate(opus::Bitrate::Bits(48_000)).map_err(|e| format!("Opus bitrate: {e}"))?;

    // Encode every 20 ms frame first: the final audio packet must
    // carry EndPage so the Ogg muxer flushes the last partial page
    // (this writer has no flush of its own).
    let mut packets: Vec<(Vec<u8>, u64)> = Vec::new();
    let mut packet = [0u8; 4_000];
    let mut granule: u64 = 0;
    for frame in pcm.chunks(FRAME) {
        let n = encoder.encode(frame, &mut packet).map_err(|e| format!("Opus encode: {e}"))?;
        granule += u64::try_from(frame.len()).unwrap_or(0);
        packets.push((packet[..n].to_vec(), granule));
    }

    let mut out = Vec::new();
    let serial = 0x5A_50_56_4E; // "ZPVN": stable per-note stream id
    let mut writer = ogg::PacketWriter::new(&mut out);

    writer
        .write_packet(opus_head(1, TARGET_RATE), serial, ogg::PacketWriteEndInfo::EndPage, 0)
        .map_err(|e| format!("Ogg header: {e}"))?;
    writer
        .write_packet(opus_tags(), serial, ogg::PacketWriteEndInfo::EndPage, 0)
        .map_err(|e| format!("Ogg tags: {e}"))?;
    let last = packets.len().saturating_sub(1);
    for (i, (bytes, granule)) in packets.into_iter().enumerate() {
        let info = if i == last {
            ogg::PacketWriteEndInfo::EndPage
        } else {
            ogg::PacketWriteEndInfo::NormalPacket
        };
        writer
            .write_packet(bytes, serial, info, granule)
            .map_err(|e| format!("Ogg packet: {e}"))?;
    }
    Ok(out)
}

/// The Opus identification header: magic, version, channel count,
/// pre-skip and the input sample rate (little endian).
fn opus_head(channels: u8, rate: u32) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(channels);
    head.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    head.extend_from_slice(&rate.to_le_bytes()); // input sample rate
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family
    head
}

/// A minimal Opus comment header: vendor string, no comments.
fn opus_tags() -> Vec<u8> {
    let mut tags = Vec::new();
    tags.extend_from_slice(b"OpusTags");
    let vendor = b"zaprust";
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes()); // comment count
    tags
}

#[cfg(test)]
mod tests {
    use super::{opus_head, opus_tags, resample_bars, MAX_SECS};

    #[test]
    fn opus_head_is_well_formed() {
        let head = opus_head(1, 48_000);
        assert_eq!(&head[..8], b"OpusHead");
        assert_eq!(head.len(), 19);
        assert_eq!(head[8], 1); // version
        assert_eq!(head[9], 1); // channels
        let rate = u32::from_le_bytes([head[12], head[13], head[14], head[15]]);
        assert_eq!(rate, 48_000);
    }

    #[test]
    fn opus_tags_are_well_formed() {
        let tags = opus_tags();
        assert_eq!(&tags[..8], b"OpusTags");
        let vendor_len = u32::from_le_bytes([tags[8], tags[9], tags[10], tags[11]]);
        assert_eq!(vendor_len, 7);
        assert_eq!(&tags[12..19], b"zaprust");
        let comments = u32::from_le_bytes([tags[19], tags[20], tags[21], tags[22]]);
        assert_eq!(comments, 0);
    }

    #[test]
    fn level_bars_resample_to_the_requested_count() {
        let empty = resample_bars(&[], 28);
        assert_eq!(empty.len(), 28);
        assert!(empty.iter().all(|v| *v == 0.0));

        let levels: Vec<f32> = (0..280).map(|i| i as f32 / 280.0).collect();
        let bars = resample_bars(&levels, 28);
        assert_eq!(bars.len(), 28);
        // Each bar is the max of its 10-tap window, so the first bar
        // peaks at 9/280 and the last at 279/280.
        assert!((bars[0] - 9.0 / 280.0).abs() < 1e-6);
        assert!((bars[27] - 279.0 / 280.0).abs() < 1e-6);
    }

    #[test]
    fn the_cap_is_five_minutes() {
        assert_eq!(MAX_SECS, 300);
    }
}
