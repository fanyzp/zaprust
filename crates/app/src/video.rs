//! Received-video playback: a minimal MP4 (ISO-BMFF) demuxer for
//! the H.264-in-MP4 shape WhatsApp sends, an OpenH264 decode
//! session that turns access units into RGBA frames in
//! presentation order, and the worker-thread player the viewer
//! drives.
//!
//! The demuxer is deliberately small: it walks the box tree,
//! reads the first `avc1` video track's sample tables, and hands
//! out access units in decode order with presentation
//! timestamps. Anything outside that shape (a different codec, a
//! malformed table) is an error in user words, never a guess.
//!
//! The same walk reads the first sound track. An AAC track is
//! decoded with the Symphonia AAC decoder (a route verified the
//! same way as the video one: a generated fixture decoded through
//! a directly constructed decoder matched the reference
//! decoder's samples) and played by an output engine that
//! mirrors the voice-note engine's design: the playhead is the
//! sink's own consumption, never a clock. The video timeline
//! stays the master clock; the audio follows it through play,
//! pause and seek, and the player re-seeks the audio when the
//! two drift apart by more than a quarter of a second. A sound
//! track in any other codec is not silently dropped: the viewer
//! states the codec and that the video plays without sound.
//!
//! The decode route was verified before it was adopted: the
//! `openh264` crate builds from source offline, and its frames
//! matched a reference decoder's output pixel for pixel on a
//! generated fixture. The library's behaviour shaped the code
//! below, all of it established by experiment: the decoder
//! emits one frame per access unit, labelled here with that
//! unit's own presentation timestamp, in decode order, so the
//! player worker reorders frames into presentation order
//! before presenting them. One limitation is documented rather
//! than hidden: the decoder rejects a stream's final access
//! unit (its error is returned and no trailer packet coaxes
//! the frame out), so the last frame of a video, a thirtieth
//! of a second at the very end, is not presented; the stated
//! duration stays the container's own. The workspace forbids
//! unsafe code, so the library's raw delayed API, which would
//! avoid both quirks, is not an option here.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource;
use openh264::OpenH264API;

// ---------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------

/// Presentation facts about a demuxed video.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub duration_secs: f64,
}

/// One H.264 access unit (one video sample) in decode order,
/// still in AVCC form (length-prefixed NAL units).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AccessUnit {
    /// Presentation timestamp in seconds.
    pub pts: f64,
    pub keyframe: bool,
    pub avcc: Vec<u8>,
}

/// A demuxed MP4 video track, ready for the decoder.
#[derive(Debug, Clone)]
pub(crate) struct DemuxedVideo {
    pub info: VideoInfo,
    /// Access units in decode order.
    pub units: Vec<AccessUnit>,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
    /// Bytes per NAL length prefix in the access units (1 to 4).
    pub nal_length_size: usize,
    /// The file's first sound track, when it has one.
    pub audio: Option<AudioTrack>,
}

/// What the demuxer found for a file's sound track.
#[derive(Debug, Clone)]
pub(crate) enum AudioTrack {
    /// An AAC track, ready for the decoder.
    Aac(DemuxedAudio),
    /// A sound track in a codec this build cannot play. The
    /// name is user-facing: the viewer states it instead of
    /// playing the video in unexplained silence.
    Unsupported { codec: String },
    /// A sound track whose own tables could not be read.
    Unreadable,
}

/// A demuxed AAC sound track.
#[derive(Debug, Clone)]
pub(crate) struct DemuxedAudio {
    pub sample_rate: u32,
    /// Source channel count (1 or 2 in anything WhatsApp sends).
    pub channels: u16,
    /// The AudioSpecificConfig bytes from the track's `esds`.
    pub config: Vec<u8>,
    /// Encoded samples in decode order.
    pub samples: Vec<AudioSample>,
    /// How long after the movie's start this track begins, in
    /// seconds (leading empty edits in the track's edit list).
    pub start_delay_secs: f64,
    /// Encoder priming at the head of the decoded stream, in
    /// frames at `sample_rate`: the edit list's media time,
    /// converted. The decoder's first `priming_frames` frames
    /// precede the track's presentation and are trimmed.
    pub priming_frames: u64,
    /// The track's own duration in seconds; decoded audio is
    /// trimmed to it, which drops the encoder's end padding.
    pub duration_secs: f64,
}

/// One encoded audio sample, with its presentation time.
#[derive(Debug, Clone)]
pub(crate) struct AudioSample {
    /// Presentation timestamp in seconds.
    pub pts: f64,
    pub data: Vec<u8>,
}

/// The largest sample count the demuxer will lay out. A WhatsApp
/// video is minutes long at most; this bound exists so a corrupt
/// table cannot make the app allocate without limit.
const MAX_SAMPLES: usize = 1_000_000;

/// Reads a big-endian `u32` at `at`, or `None` when out of range.
fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Reads a big-endian `u64` at `at`, or `None` when out of range.
fn be_u64(data: &[u8], at: usize) -> Option<u64> {
    data.get(at..at + 8)
        .map(|b| u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

/// One box header: its type and the payload range within `data`.
struct BoxSpan {
    kind: u32,
    payload: std::ops::Range<usize>,
    /// Offset just past this box.
    end: usize,
}

/// Parses the box header at `at` within `data`. Boxes may use the
/// 32-bit size, the 64-bit large size, or size 0 (to the end of
/// the enclosing range).
fn box_at(data: &[u8], at: usize) -> Option<BoxSpan> {
    let size32 = be_u32(data, at)?;
    let kind = be_u32(data, at + 4)?;
    let (total, header) = match size32 {
        1 => (usize::try_from(be_u64(data, at + 8)?).ok()?, 16),
        0 => (data.len().checked_sub(at)?, 8),
        s => (usize::try_from(s).ok()?, 8),
    };
    if total < header || at.checked_add(total)? > data.len() {
        return None;
    }
    Some(BoxSpan { kind, payload: at + header..at + total, end: at + total })
}

/// Finds the first box of type `want` among the children of
/// `data` (a container's payload), returning its payload.
fn find_child(data: &[u8], want: u32) -> Option<&[u8]> {
    let mut at = 0;
    while at + 8 <= data.len() {
        let span = box_at(data, at)?;
        if span.kind == want {
            return Some(&data[span.payload]);
        }
        at = span.end;
    }
    None
}

/// Iterates the payloads of every child box of type `want`.
fn children(data: &[u8], want: u32) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 8 <= data.len() {
        let Some(span) = box_at(data, at) else { break };
        if span.kind == want {
            out.push(&data[span.payload.clone()]);
        }
        at = span.end;
    }
    out
}

const fn fourcc(s: &str) -> u32 {
    let b = s.as_bytes();
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// The track timescale and duration from an `mdhd` payload.
fn mdhd_timing(mdhd: &[u8]) -> Option<(f64, f64)> {
    let version = *mdhd.first()?;
    let (timescale, duration) = if version == 1 {
        (be_u32(mdhd, 20)?, be_u64(mdhd, 24)?)
    } else {
        (be_u32(mdhd, 12)?, u64::from(be_u32(mdhd, 16)?))
    };
    if timescale == 0 {
        return None;
    }
    Some((f64::from(timescale), duration as f64 / f64::from(timescale)))
}

/// Per-sample durations expanded from an `stts` payload.
fn stts_durations(stts: &[u8]) -> Option<Vec<u32>> {
    let count = usize::try_from(be_u32(stts, 4)?).ok()?;
    let mut out = Vec::new();
    let mut at = 8;
    for _ in 0..count {
        let n = be_u32(stts, at)?;
        let delta = be_u32(stts, at + 4)?;
        at += 8;
        if out.len() + (n as usize) > MAX_SAMPLES {
            return None;
        }
        out.extend(std::iter::repeat_n(delta, n as usize));
    }
    Some(out)
}

/// Per-sample composition offsets expanded from a `ctts`
/// payload (absent table means every offset is zero).
fn ctts_offsets(ctts: Option<&[u8]>, samples: usize) -> Option<Vec<i64>> {
    let Some(ctts) = ctts else {
        return Some(vec![0; samples]);
    };
    let signed = ctts.first().copied() == Some(1);
    let count = usize::try_from(be_u32(ctts, 4)?).ok()?;
    let mut out = Vec::new();
    let mut at = 8;
    for _ in 0..count {
        let n = be_u32(ctts, at)?;
        let raw = be_u32(ctts, at + 4)?;
        at += 8;
        let offset = if signed { i64::from(raw as i32) } else { i64::from(raw) };
        if out.len() + (n as usize) > MAX_SAMPLES {
            return None;
        }
        out.extend(std::iter::repeat_n(offset, n as usize));
    }
    if out.len() < samples {
        out.resize(samples, 0);
    }
    Some(out)
}

/// The keyframe flags from an `stss` payload, indexed by sample
/// (the table itself stores 1-based sample numbers).
fn stss_keyframes(stss: &[u8]) -> Option<Vec<bool>> {
    let count = usize::try_from(be_u32(stss, 4)?).ok()?;
    let mut marked = Vec::with_capacity(count);
    let mut at = 8;
    for _ in 0..count {
        marked.push(be_u32(stss, at)? as usize);
        at += 4;
    }
    let mut flags = vec![false; marked.iter().max().copied().unwrap_or(0)];
    for n in marked {
        if n >= 1 {
            flags[n - 1] = true;
        }
    }
    Some(flags)
}

/// Chunk offsets from `stco` or `co64`.
fn chunk_offsets(stbl: &[u8]) -> Option<Vec<u64>> {
    if let Some(stco) = find_child(stbl, fourcc("stco")) {
        let count = usize::try_from(be_u32(stco, 4)?).ok()?;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            out.push(u64::from(be_u32(stco, 8 + i * 4)?));
        }
        return Some(out);
    }
    let co64 = find_child(stbl, fourcc("co64"))?;
    let count = usize::try_from(be_u32(co64, 4)?).ok()?;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        out.push(be_u64(co64, 8 + i * 8)?);
    }
    Some(out)
}

/// Per-sample sizes from an `stsz` payload.
fn sample_sizes(stsz: &[u8]) -> Option<Vec<usize>> {
    let fixed = be_u32(stsz, 4)?;
    let count = usize::try_from(be_u32(stsz, 8)?).ok()?;
    if count > MAX_SAMPLES {
        return None;
    }
    if fixed != 0 {
        return Some(vec![fixed as usize; count]);
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        out.push(be_u32(stsz, 12 + i * 4)? as usize);
    }
    Some(out)
}

/// Samples per chunk, resolved per chunk index from `stsc`.
fn samples_per_chunk(stsc: &[u8], chunks: usize) -> Option<Vec<usize>> {
    let count = usize::try_from(be_u32(stsc, 4)?).ok()?;
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        entries.push((be_u32(stsc, 8 + i * 12)? as usize, be_u32(stsc, 12 + i * 12)? as usize));
    }
    let mut out = Vec::with_capacity(chunks);
    for chunk in 1..=chunks {
        let mut spc = None;
        for &(first, per) in &entries {
            if first <= chunk {
                spc = Some(per);
            } else {
                break;
            }
        }
        out.push(spc?);
    }
    Some(out)
}

/// The SPS, PPS and NAL length size from an `avcC` payload.
fn parse_avcc(avcc: &[u8]) -> Option<(Vec<u8>, Vec<u8>, usize)> {
    let nal_length_size = usize::from(avcc.get(4)? & 0x03) + 1;
    let sps_count = usize::from(avcc.get(5)? & 0x1F);
    let mut at = 6;
    let mut sps = Vec::new();
    for i in 0..sps_count {
        let len = usize::from(u16::from_be_bytes([*avcc.get(at)?, *avcc.get(at + 1)?]));
        at += 2;
        let bytes = avcc.get(at..at + len)?;
        if i == 0 {
            sps = bytes.to_vec();
        }
        at += len;
    }
    let pps_count = usize::from(*avcc.get(at)?);
    at += 1;
    let mut pps = Vec::new();
    for i in 0..pps_count {
        let len = usize::from(u16::from_be_bytes([*avcc.get(at)?, *avcc.get(at + 1)?]));
        at += 2;
        let bytes = avcc.get(at..at + len)?;
        if i == 0 {
            pps = bytes.to_vec();
        }
        at += len;
    }
    if sps.is_empty() || pps.is_empty() || !(1..=4).contains(&nal_length_size) {
        return None;
    }
    Some((sps, pps, nal_length_size))
}

/// Demuxes the first H.264 video track of an MP4 file's bytes,
/// together with its first sound track when it has one. A sound
/// track that cannot be read never fails the video: it degrades
/// to an [`AudioTrack`] marker the player reports honestly.
///
/// # Errors
/// A user-facing sentence when the bytes are not an MP4 with an
/// H.264 video track this demuxer understands.
pub(crate) fn demux(data: &[u8]) -> Result<DemuxedVideo, String> {
    let not_video = || "That file is not a playable video.".to_owned();
    let moov = find_child(data, fourcc("moov")).ok_or_else(not_video)?;
    let movie_timescale = mvhd_timescale(moov);
    let mut video: Option<DemuxedVideo> = None;
    let mut video_error: Option<Option<String>> = None;
    let mut audio: Option<AudioTrack> = None;
    for trak in children(moov, fourcc("trak")) {
        let Some(mdia) = find_child(trak, fourcc("mdia")) else { continue };
        let Some(hdlr) = find_child(mdia, fourcc("hdlr")) else { continue };
        match be_u32(hdlr, 8) {
            Some(kind) if kind == fourcc("vide") && video.is_none() && video_error.is_none() => {
                match demux_track(data, mdia) {
                    Ok(found) => video = Some(found),
                    Err(e) => video_error = Some(e),
                }
            }
            Some(kind) if kind == fourcc("soun") && audio.is_none() => {
                audio = Some(demux_audio_track(data, trak, mdia, movie_timescale));
            }
            _ => {}
        }
    }
    if let Some(mut found) = video {
        found.audio = audio;
        return Ok(found);
    }
    match video_error {
        Some(e) => Err(e.unwrap_or_else(not_video)),
        None => Err("That video has no video track to play.".to_owned()),
    }
}

/// The movie timescale from the `mvhd` payload (the units edit
/// list segment durations are expressed in).
fn mvhd_timescale(moov: &[u8]) -> f64 {
    let Some(mvhd) = find_child(moov, fourcc("mvhd")) else { return 1000.0 };
    let timescale = if mvhd.first() == Some(&1) { be_u32(mvhd, 20) } else { be_u32(mvhd, 12) };
    match timescale {
        Some(0) | None => 1000.0,
        Some(t) => f64::from(t),
    }
}

/// One laid-out media sample: its decode timestamp in track
/// timescale ticks and its bytes.
struct LaidSample {
    dts_ticks: i64,
    bytes: Vec<u8>,
}

/// Lays out a track's samples from its sample tables (`stts`,
/// `stsz`, `stsc`, chunk offsets), in decode order. Shared by
/// the video and audio track readers; composition offsets and
/// keyframe flags stay with the video reader, which is the only
/// one that uses them.
fn layout_samples(data: &[u8], stbl: &[u8]) -> Option<(Vec<u32>, Vec<LaidSample>)> {
    let durations = stts_durations(find_child(stbl, fourcc("stts"))?)?;
    let samples = durations.len();
    let sizes = sample_sizes(find_child(stbl, fourcc("stsz"))?)?;
    if sizes.len() != samples {
        return None;
    }
    let chunks = chunk_offsets(stbl)?;
    let per_chunk = samples_per_chunk(find_child(stbl, fourcc("stsc"))?, chunks.len())?;
    let mut out = Vec::with_capacity(samples);
    let mut dts: i64 = 0;
    let mut sample_idx = 0usize;
    for (ci, &chunk_off) in chunks.iter().enumerate() {
        let mut off = usize::try_from(chunk_off).ok()?;
        for _ in 0..per_chunk[ci] {
            if sample_idx >= samples {
                return None;
            }
            let size = sizes[sample_idx];
            let bytes = data.get(off..off + size)?;
            out.push(LaidSample { dts_ticks: dts, bytes: bytes.to_vec() });
            dts += i64::from(durations[sample_idx]);
            off += size;
            sample_idx += 1;
        }
    }
    Some((durations, out))
}

/// Demuxes one video track. The error is `None` for "malformed"
/// and `Some` for a message worth showing verbatim.
fn demux_track(data: &[u8], mdia: &[u8]) -> Result<DemuxedVideo, Option<String>> {
    let mdhd = find_child(mdia, fourcc("mdhd")).ok_or(None)?;
    let (timescale, track_duration) = mdhd_timing(mdhd).ok_or(None)?;
    let minf = find_child(mdia, fourcc("minf")).ok_or(None)?;
    let stbl = find_child(minf, fourcc("stbl")).ok_or(None)?;
    let stsd = find_child(stbl, fourcc("stsd")).ok_or(None)?;

    // The first sample entry decides the codec.
    let entry_count = be_u32(stsd, 4).ok_or(None)?;
    if entry_count == 0 {
        return Err(None);
    }
    let entry = box_at(stsd, 8).ok_or(None)?;
    if entry.kind != fourcc("avc1") {
        let name = String::from_utf8_lossy(&entry.kind.to_be_bytes()).into_owned();
        return Err(Some(format!(
            "This video uses the {name} codec, which Zaprust cannot play yet."
        )));
    }
    let entry_payload = &stsd[entry.payload.clone()];
    // VisualSampleEntry: width and height sit at fixed offsets
    // in the entry payload, and child boxes (avcC) follow the
    // fixed 78-byte entry body.
    let width = u32::from(u16::from_be_bytes([
        *entry_payload.get(24).ok_or(None)?,
        *entry_payload.get(25).ok_or(None)?,
    ]));
    let height = u32::from(u16::from_be_bytes([
        *entry_payload.get(26).ok_or(None)?,
        *entry_payload.get(27).ok_or(None)?,
    ]));
    let avcc =
        find_child(&entry_payload[78.min(entry_payload.len())..], fourcc("avcC")).ok_or(None)?;
    let (sps, pps, nal_length_size) = parse_avcc(avcc).ok_or(None)?;

    let (_durations, laid) = layout_samples(data, stbl).ok_or(None)?;
    let samples = laid.len();
    let offsets_ctts = ctts_offsets(find_child(stbl, fourcc("ctts")), samples).ok_or(None)?;
    let keyframes = match find_child(stbl, fourcc("stss")) {
        Some(stss) => {
            let mut flags = stss_keyframes(stss).ok_or(None)?;
            flags.resize(samples, false);
            flags
        }
        None => vec![true; samples],
    };

    // Attach presentation timestamps and keyframe flags to the
    // laid-out samples.
    let mut units = Vec::with_capacity(samples);
    for (idx, sample) in laid.into_iter().enumerate() {
        let pts_ticks = sample.dts_ticks + offsets_ctts[idx];
        units.push(AccessUnit {
            pts: pts_ticks as f64 / timescale,
            keyframe: keyframes[idx],
            avcc: sample.bytes,
        });
    }
    if units.is_empty() {
        return Err(None);
    }
    // Presentation timestamps can carry an initial offset (the
    // composition delay an encoder with B-frames writes into
    // the table, normally cancelled by an edit list this
    // demuxer does not apply). Playback is relative, so the
    // timeline is normalised to start at zero.
    let base = units.iter().map(|u| u.pts).fold(f64::INFINITY, f64::min);
    if base.is_finite() && base != 0.0 {
        for unit in &mut units {
            unit.pts -= base;
        }
    }
    let duration_secs = if track_duration > 0.0 {
        track_duration
    } else {
        units.last().map(|u| u.pts).unwrap_or(0.0)
    };
    Ok(DemuxedVideo {
        info: VideoInfo { width, height, duration_secs },
        units,
        sps,
        pps,
        nal_length_size,
        audio: None,
    })
}

/// Reads one descriptor's header at `at` within an `esds`
/// payload: its tag and payload range, and the offset just past
/// it. Descriptor lengths use the expandable form (up to four
/// bytes, seven bits each, high bit set on all but the last).
fn descriptor_at(data: &[u8], at: usize) -> Option<(u8, std::ops::Range<usize>, usize)> {
    let tag = *data.get(at)?;
    let mut len: usize = 0;
    let mut pos = at + 1;
    loop {
        let byte = *data.get(pos)?;
        pos += 1;
        len = (len << 7) | usize::from(byte & 0x7F);
        if byte & 0x80 == 0 {
            break;
        }
        if pos - at > 5 {
            return None;
        }
    }
    let end = pos.checked_add(len)?;
    if end > data.len() {
        return None;
    }
    Some((tag, pos..end, end))
}

/// Extracts the decoder object type and the AudioSpecificConfig
/// from an `esds` payload: ES_Descriptor (0x03) wraps a
/// DecoderConfigDescriptor (0x04, whose first byte is the object
/// type) which in turn wraps the DecoderSpecificInfo (0x05).
fn parse_esds(esds: &[u8]) -> Option<(u8, Option<Vec<u8>>)> {
    // Skip the version and flags.
    let (tag, es_range, _) = descriptor_at(esds, 4)?;
    if tag != 0x03 {
        return None;
    }
    let es = &esds[es_range];
    // ES_ID (2 bytes) and a flags byte; the flags announce
    // optional fields that must be stepped over.
    let flags = *es.get(2)?;
    let mut at = 3;
    if flags & 0x80 != 0 {
        at += 2; // dependsOn_ES_ID
    }
    if flags & 0x40 != 0 {
        let url_len = usize::from(*es.get(at)?);
        at += 1 + url_len;
    }
    if flags & 0x20 != 0 {
        at += 2; // OCR_ES_ID
    }
    let (tag, cfg_range, _) = descriptor_at(es, at)?;
    if tag != 0x04 {
        return None;
    }
    let cfg = &es[cfg_range];
    let object_type = *cfg.first()?;
    // The config's fixed part is 13 bytes; the specific info
    // follows inside the same descriptor.
    let mut config = None;
    if cfg.len() > 13 {
        if let Some((tag, info_range, _)) = descriptor_at(cfg, 13) {
            if tag == 0x05 {
                config = Some(cfg[info_range].to_vec());
            }
        }
    }
    Some((object_type, config))
}

/// The sample rate and channel count an AudioSpecificConfig
/// announces, when it carries both in the plain form (an
/// explicit rate and a channel configuration of 1 to 7; the
/// program-config form and 7.1 fall back to the sample entry's
/// own fields).
fn parse_asc(asc: &[u8]) -> Option<(u32, u16)> {
    let bit = |at: usize| -> Option<u32> {
        let byte = asc.get(at / 8)?;
        Some(u32::from((byte >> (7 - (at % 8))) & 1))
    };
    let bits = |at: usize, n: usize| -> Option<u32> {
        let mut out = 0;
        for i in 0..n {
            out = (out << 1) | bit(at + i)?;
        }
        Some(out)
    };
    let mut at = 0;
    let object_type = bits(at, 5)?;
    at += 5;
    if object_type == 31 {
        at += 6;
    }
    let rate_index = bits(at, 4)?;
    at += 4;
    let rate = match rate_index {
        15 => {
            let rate = bits(at, 24)?;
            at += 24;
            rate
        }
        i => {
            const RATES: [u32; 13] = [
                96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000,
                11_025, 8_000, 7_350,
            ];
            *RATES.get(i as usize)?
        }
    };
    let channel_config = bits(at, 4)?;
    if rate == 0 || !(1..=7).contains(&channel_config) {
        return None;
    }
    Some((rate, channel_config as u16))
}

/// The presentation placement of an audio track from its edit
/// list: how long the track is delayed before it starts
/// (leading empty edits), the media time of its first media
/// edit (the encoder priming, in track timescale ticks), and
/// how many seconds of media the list presents in total. The
/// presented length matters because the sample timeline runs
/// past it: priming at the head and padding at the tail are on
/// the timeline but are never presented. Absent or unreadable
/// edit lists mean no delay, no priming, and no stated length.
fn edit_list_placement(trak: &[u8], movie_timescale: f64) -> (f64, i64, Option<f64>) {
    let Some(edts) = find_child(trak, fourcc("edts")) else { return (0.0, 0, None) };
    let Some(elst) = find_child(edts, fourcc("elst")) else { return (0.0, 0, None) };
    let version = elst.first().copied().unwrap_or(0);
    let Some(count) = be_u32(elst, 4) else { return (0.0, 0, None) };
    let mut delay_secs = 0.0;
    let mut media_time = 0;
    let mut presented_secs = 0.0;
    let mut at = 8;
    for _ in 0..count {
        let (segment, entry_media_time) = if version == 1 {
            let entry = (be_u64(elst, at), be_u64(elst, at + 8).map(|v| v as i64));
            at += 20;
            (entry.0, entry.1)
        } else {
            let entry = (
                be_u32(elst, at).map(u64::from),
                be_u32(elst, at + 4).map(|v| i64::from(v as i32)),
            );
            at += 12;
            (entry.0, entry.1)
        };
        let (Some(segment), Some(entry_media_time)) = (segment, entry_media_time) else { break };
        if entry_media_time < 0 {
            if presented_secs == 0.0 && movie_timescale > 0.0 {
                // A leading empty edit: the track starts this
                // much later.
                delay_secs += segment as f64 / movie_timescale;
            }
            continue;
        }
        if presented_secs == 0.0 {
            media_time = entry_media_time;
        }
        if movie_timescale > 0.0 {
            presented_secs += segment as f64 / movie_timescale;
        }
    }
    (delay_secs, media_time, (presented_secs > 0.0).then_some(presented_secs))
}

/// A user-facing name for a sound codec this build cannot play.
fn audio_codec_name(entry_kind: u32, object_type: Option<u8>) -> String {
    match object_type {
        // Object types inside an `mp4a` entry.
        Some(0x69) | Some(0x6B) => "MP3".to_owned(),
        Some(other) => format!("MPEG-4 audio (type 0x{other:02X})"),
        // Any other sample entry is named by its own fourcc.
        None => String::from_utf8_lossy(&entry_kind.to_be_bytes()).into_owned(),
    }
}

/// Demuxes one sound track. Every failure degrades to an
/// [`AudioTrack`] marker instead of an error: the video must
/// still play, with the sound situation stated, never silently
/// dropped.
fn demux_audio_track(data: &[u8], trak: &[u8], mdia: &[u8], movie_timescale: f64) -> AudioTrack {
    let unreadable = AudioTrack::Unreadable;
    let Some(mdhd) = find_child(mdia, fourcc("mdhd")) else { return unreadable };
    let Some((timescale, track_duration)) = mdhd_timing(mdhd) else { return unreadable };
    let Some(minf) = find_child(mdia, fourcc("minf")) else { return unreadable };
    let Some(stbl) = find_child(minf, fourcc("stbl")) else { return unreadable };
    let Some(stsd) = find_child(stbl, fourcc("stsd")) else { return unreadable };
    if be_u32(stsd, 4) == Some(0) {
        return unreadable;
    }
    let Some(entry) = box_at(stsd, 8) else { return unreadable };
    if entry.kind != fourcc("mp4a") {
        return AudioTrack::Unsupported { codec: audio_codec_name(entry.kind, None) };
    }
    let entry_payload = &stsd[entry.payload.clone()];
    // AudioSampleEntry: channel count and sample rate sit at
    // fixed offsets in the version 0 body; child boxes (the
    // `esds`) follow it, 16 bytes later again in version 1.
    let version = u16::from_be_bytes([
        *entry_payload.get(8).unwrap_or(&0),
        *entry_payload.get(9).unwrap_or(&0),
    ]);
    let entry_channels =
        entry_payload.get(16..18).map(|b| u16::from_be_bytes([b[0], b[1]])).unwrap_or(0);
    let entry_rate = entry_payload
        .get(24..28)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) >> 16)
        .unwrap_or(0);
    let children_at = match version {
        0 => 28,
        1 => 44,
        _ => 28,
    };
    let esds = entry_payload
        .get(children_at.min(entry_payload.len())..)
        .and_then(|rest| find_child(rest, fourcc("esds")))
        .or_else(|| {
            // The child walk missed it (an entry layout this
            // reader does not model): scan for the box header.
            let needle = b"esds";
            entry_payload
                .windows(4)
                .position(|w| w == needle)
                .and_then(|pos| pos.checked_sub(4))
                .and_then(|start| box_at(entry_payload, start))
                .map(|span| &entry_payload[span.payload])
        });
    let Some(esds) = esds else { return unreadable };
    let Some((object_type, config)) = parse_esds(esds) else { return unreadable };
    // 0x40 is MPEG-4 AAC and 0x67 its MPEG-2 form; both are the
    // AAC this decoder speaks.
    if object_type != 0x40 && object_type != 0x67 {
        return AudioTrack::Unsupported { codec: audio_codec_name(entry.kind, Some(object_type)) };
    }
    let Some(config) = config.filter(|c| !c.is_empty()) else { return unreadable };
    let (mut sample_rate, mut channels) = (entry_rate, entry_channels);
    if let Some((asc_rate, asc_channels)) = parse_asc(&config) {
        sample_rate = asc_rate;
        channels = asc_channels;
    }
    if sample_rate == 0 || channels == 0 {
        return unreadable;
    }
    let Some((_durations, laid)) = layout_samples(data, stbl) else { return unreadable };
    if laid.is_empty() {
        return unreadable;
    }
    let samples: Vec<AudioSample> = laid
        .into_iter()
        .map(|s| AudioSample { pts: s.dts_ticks as f64 / timescale, data: s.bytes })
        .collect();
    let (mut start_delay_secs, media_time, presented_secs) =
        edit_list_placement(trak, movie_timescale);
    if media_time == 0 {
        // No edit list places the track: the sample timeline
        // itself says where it starts (a first sample past zero
        // is a track that begins late).
        start_delay_secs += samples.first().map_or(0.0, |s| s.pts).max(0.0);
    }
    // The presented length comes from the edit list when it
    // states one; the raw sample timeline runs longer by the
    // priming and any tail padding.
    let duration_secs = presented_secs.unwrap_or(track_duration);
    let priming_frames = if media_time > 0 && timescale > 0.0 {
        (media_time as f64 * f64::from(sample_rate) / timescale).round() as u64
    } else {
        0
    };
    AudioTrack::Aac(DemuxedAudio {
        sample_rate,
        channels,
        config,
        samples,
        start_delay_secs,
        priming_frames,
        duration_secs,
    })
}

// ---------------------------------------------------------------------
// Decode session
// ---------------------------------------------------------------------

/// One decoded video frame: RGBA pixels and the presentation
/// timestamp they belong to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DecodedFrame {
    pub pts: f64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// An OpenH264 decode session over one demuxed track.
///
/// The decoder emits one frame per access unit, in decode
/// order (see the module docs), so the session labels each
/// emitted frame with the timestamp of the unit that produced
/// it; reordering into presentation order is the player
/// worker's job. A unit the decoder rejects (a corrupt unit,
/// and empirically a stream's final unit) produces no frame:
/// the session counts it and carries on, and only a stream
/// that produces nothing at all is a failure.
pub(crate) struct DecodeSession {
    decoder: Decoder,
    nal_length_size: usize,
    config_fed: bool,
    sps: Vec<u8>,
    pps: Vec<u8>,
    /// Timestamps of units that produced no frame, kept so a
    /// frame the decoder releases only at the end-of-stream
    /// flush can still be labelled.
    unclaimed_pts: VecDeque<f64>,
    /// Any frame was decoded in this session.
    decoded_any: bool,
    /// The decoder rejected at least one unit.
    saw_error: bool,
}

impl DecodeSession {
    /// Opens a session for a demuxed track.
    ///
    /// # Errors
    /// A user-facing sentence when the decoder cannot start.
    pub(crate) fn new(demuxed: &DemuxedVideo) -> Result<Self, String> {
        let decoder = Decoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
            .map_err(|_| "The video decoder could not start on this device.".to_owned())?;
        Ok(Self {
            decoder,
            nal_length_size: demuxed.nal_length_size,
            config_fed: false,
            sps: demuxed.sps.clone(),
            pps: demuxed.pps.clone(),
            unclaimed_pts: VecDeque::new(),
            decoded_any: false,
            saw_error: false,
        })
    }

    /// Converts one access unit from AVCC to Annex B form.
    fn annex_b(&self, avcc: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(avcc.len() + 16);
        if !self.config_fed {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&self.sps);
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&self.pps);
        }
        let mut at = 0;
        while at + self.nal_length_size <= avcc.len() {
            let mut len = 0usize;
            for i in 0..self.nal_length_size {
                len = (len << 8) | usize::from(avcc[at + i]);
            }
            at += self.nal_length_size;
            let Some(nal) = avcc.get(at..at + len) else { break };
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
            at += len;
        }
        out
    }

    /// Feeds one access unit; returns the frame it produced, if
    /// any. The frame is labelled with the unit's own
    /// presentation timestamp (the decoder emits the fed unit's
    /// picture; see the module docs). A rejected unit produces
    /// no frame and is counted, not fatal: playback of the
    /// remaining units is worth more than the whole video
    /// failing over one bad unit.
    pub(crate) fn decode_unit(&mut self, unit: &AccessUnit) -> Vec<DecodedFrame> {
        let annex = self.annex_b(&unit.avcc);
        self.config_fed = true;
        match self.decoder.decode(&annex) {
            Ok(Some(yuv)) => {
                self.decoded_any = true;
                vec![label_frame(unit.pts, &yuv)]
            }
            Ok(None) => {
                self.unclaimed_pts.push_back(unit.pts);
                Vec::new()
            }
            Err(_) => {
                self.saw_error = true;
                self.unclaimed_pts.push_back(unit.pts);
                Vec::new()
            }
        }
    }

    /// Drains any frames still buffered in the decoder at the
    /// end of the stream.
    ///
    /// # Errors
    /// A user-facing sentence when the stream produced no
    /// decodable frame at all: that is a video this decoder
    /// cannot play, and saying so beats a silent black tile.
    pub(crate) fn flush(&mut self) -> Result<Vec<DecodedFrame>, String> {
        let rest = self.decoder.flush_remaining();
        match rest {
            Ok(yuvs) => {
                let mut out = Vec::with_capacity(yuvs.len());
                for yuv in &yuvs {
                    self.decoded_any = true;
                    let pts = self.unclaimed_pts.pop_front().unwrap_or(0.0);
                    out.push(label_frame(pts, yuv));
                }
                if !self.decoded_any {
                    return Err("That video could not be decoded.".to_owned());
                }
                Ok(out)
            }
            Err(_) => {
                if self.decoded_any {
                    Ok(Vec::new())
                } else {
                    Err("That video could not be decoded.".to_owned())
                }
            }
        }
    }
}

/// Converts one raw decoder output into a frame labelled with
/// `pts`: the pixels become RGBA through the library's own
/// conversion.
fn label_frame(pts: f64, yuv: &openh264::decoder::DecodedYUV) -> DecodedFrame {
    let (width, height) = yuv.dimensions();
    let mut rgba = vec![0u8; width * height * 4];
    yuv.write_rgba8(&mut rgba);
    DecodedFrame { pts, width: width as u32, height: height as u32, rgba }
}

// ---------------------------------------------------------------------
// Audio decode
// ---------------------------------------------------------------------

/// A video's sound track, decoded to interleaved PCM and ready
/// for the output engine.
#[derive(Debug, Clone)]
pub(crate) struct DecodedAudio {
    pub pcm: Arc<[i16]>,
    /// Channels in `pcm`: 1 or 2 (wider layouts are folded down
    /// while decoding).
    pub channels: u16,
    pub sample_rate: u32,
    /// How long after the video's start the sound begins.
    pub start_delay_secs: f64,
}

/// The longest sound track decoded in full, in seconds: six
/// minutes, the same spirit as the voice-note cap in the player
/// module. A longer track is refused in user words rather than
/// decoded into unbounded memory.
const MAX_AUDIO_SECS: u64 = 360;

/// What the decode worker reports about a video's sound track.
#[derive(Debug)]
enum AudioReport {
    Ready(DecodedAudio),
    /// A sound track in a codec this build cannot play.
    Unsupported {
        codec: String,
    },
    /// A user-facing sentence: the track exists but its sound
    /// could not be produced.
    Failed(String),
}

/// The channel mask the AAC decoder is opened with. Only mono
/// and stereo get their true mask; anything wider is opened as
/// stereo and the decoder's own buffer spec is trusted for the
/// real layout while folding down (see [`decode_audio`]).
fn channels_mask(channels: u16) -> symphonia_core::audio::Channels {
    use symphonia_core::audio::Channels;
    match channels {
        1 => Channels::FRONT_CENTRE,
        _ => Channels::FRONT_LEFT | Channels::FRONT_RIGHT,
    }
}

/// Appends one decoded buffer to the PCM, folding the buffer's
/// channel layout down to `out_channels`: mono stays mono, and
/// for wider sources the even channels average into the left
/// and the odd into the right.
fn append_folded(pcm: &mut Vec<i16>, samples: &[i16], src_channels: usize, out_channels: usize) {
    if src_channels == out_channels {
        pcm.extend_from_slice(samples);
        return;
    }
    if out_channels == 1 {
        for frame in samples.chunks_exact(src_channels) {
            let sum: i32 = frame.iter().map(|s| i32::from(*s)).sum();
            pcm.push((sum / src_channels as i32) as i16);
        }
        return;
    }
    for frame in samples.chunks_exact(src_channels) {
        let mut sides = [0i32; 2];
        let mut counts = [0i32; 2];
        for (i, s) in frame.iter().enumerate() {
            sides[i % 2] += i32::from(*s);
            counts[i % 2] += 1;
        }
        for side in 0..2 {
            pcm.push(if counts[side] > 0 { (sides[side] / counts[side]) as i16 } else { 0 });
        }
    }
}

/// Decodes a demuxed AAC track to PCM with the Symphonia AAC
/// decoder, opened directly on the track's own configuration
/// (the route the pre-adoption probe verified against a
/// reference decoder). Per-packet decode failures are skipped,
/// as with video units: the remaining sound is worth more than
/// failing the whole track over one bad packet. The edit list's
/// priming is trimmed from the head and the track's own
/// duration bounds the tail, so frame zero is the track's
/// presentation start.
///
/// # Errors
/// A user-facing sentence when nothing decodable comes out or
/// the track is beyond the length cap.
pub(crate) fn decode_audio(audio: &DemuxedAudio) -> Result<DecodedAudio, String> {
    use symphonia_core::audio::SampleBuffer;
    use symphonia_core::codecs::{CodecParameters, Decoder as _, DecoderOptions, CODEC_TYPE_AAC};
    use symphonia_core::formats::Packet;

    let failed = || "This video's audio track could not be decoded.".to_owned();
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_AAC)
        .with_sample_rate(audio.sample_rate)
        .with_channels(channels_mask(audio.channels))
        .with_extra_data(audio.config.clone().into_boxed_slice());
    let mut decoder = symphonia_codec_aac::AacDecoder::try_new(&params, &DecoderOptions::default())
        .map_err(|_| failed())?;

    let max_samples = (MAX_AUDIO_SECS * u64::from(audio.sample_rate)) as usize * 2;
    let mut pcm: Vec<i16> = Vec::new();
    let mut out_channels: Option<usize> = None;
    let mut scratch: Option<SampleBuffer<i16>> = None;
    let mut scratch_spec: Option<symphonia_core::audio::SignalSpec> = None;
    for (idx, sample) in audio.samples.iter().enumerate() {
        let packet = Packet::new_from_slice(0, idx as u64, 1024, &sample.data);
        let Ok(decoded) = decoder.decode(&packet) else { continue };
        let spec = *decoded.spec();
        let src_channels = spec.channels.count();
        if src_channels == 0 {
            continue;
        }
        let target = *out_channels.get_or_insert_with(|| src_channels.min(2));
        let reusable = scratch_spec == Some(spec)
            && scratch.as_ref().is_some_and(|b| b.capacity() >= decoded.capacity());
        if !reusable {
            scratch = Some(SampleBuffer::<i16>::new(decoded.capacity() as u64, spec));
            scratch_spec = Some(spec);
        }
        let samples_buf = scratch.as_mut().expect("the scratch buffer was just set");
        samples_buf.copy_interleaved_ref(decoded);
        append_folded(&mut pcm, samples_buf.samples(), src_channels, target);
        if pcm.len() > max_samples {
            return Err("This video's audio track is too long to play.".to_owned());
        }
    }
    let channels = out_channels.ok_or_else(failed)?;
    if pcm.is_empty() {
        return Err(failed());
    }
    // Trim the encoder priming at the head and the padding the
    // final frame carries past the track's announced duration.
    let total_frames = pcm.len() / channels;
    let skip = (audio.priming_frames as usize).min(total_frames);
    let keep = ((audio.duration_secs * f64::from(audio.sample_rate)).round() as usize)
        .min(total_frames - skip);
    let pcm: Vec<i16> = pcm.into_iter().skip(skip * channels).take(keep * channels).collect();
    if pcm.is_empty() {
        return Err(failed());
    }
    Ok(DecodedAudio {
        pcm: Arc::from(pcm),
        channels: channels as u16,
        sample_rate: audio.sample_rate,
        start_delay_secs: audio.start_delay_secs,
    })
}

// ---------------------------------------------------------------------
// Audio output engine
// ---------------------------------------------------------------------

/// Which output a video's sound runs against. The same two
/// sinks as the voice engine: production uses the device; the
/// null sink consumes in real time and emits nothing, so the
/// engine runs on machines without speakers and in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioSink {
    Device,
    // Constructed by the engine tests and headless runs; a
    // normal build only ever selects the device sink.
    #[allow(dead_code)]
    Null,
}

/// Sound playback state shared between the player and the
/// output thread. `pos` is in source frames and is advanced
/// only by [`audio_pull`], at the sink's consumption rate: a
/// paused, stalled or absent output cannot produce a moving
/// playhead over silence.
struct AudioPlayState {
    pcm: Arc<[i16]>,
    channels: usize,
    pos: f64,
    playing: bool,
    ended: bool,
}

fn lock_audio(state: &Mutex<AudioPlayState>) -> std::sync::MutexGuard<'_, AudioPlayState> {
    state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The interpolated source sample of one channel at a
/// fractional frame position.
fn audio_sample_at(pcm: &[i16], channels: usize, pos: f64, channel: usize) -> f32 {
    let frames = pcm.len() / channels.max(1);
    if frames == 0 {
        return 0.0;
    }
    let last = frames - 1;
    let base = (pos.floor() as usize).min(last);
    let next = (base + 1).min(last);
    let frac = (pos - base as f64) as f32;
    let a = f32::from(pcm[base * channels + channel]) / 32768.0;
    let b = f32::from(pcm[next * channels + channel]) / 32768.0;
    a + (b - a) * frac.clamp(0.0, 1.0)
}

/// Fills `out` (interleaved over `out_channels`) with the next
/// frames of the sound track, advancing the position by what
/// was actually consumed. An output channel beyond the source's
/// count reuses the last source channel, so mono plays on every
/// channel of a stereo device. Paused or ended streams emit
/// silence and consume nothing.
fn audio_pull(state: &mut AudioPlayState, out: &mut [f32], out_channels: usize, step: f64) {
    let frames = (state.pcm.len() / state.channels.max(1)) as f64;
    for frame in out.chunks_exact_mut(out_channels) {
        if state.playing && !state.ended {
            for (out_channel, slot) in frame.iter_mut().enumerate() {
                let src_channel = out_channel.min(state.channels.saturating_sub(1));
                *slot = audio_sample_at(&state.pcm, state.channels, state.pos, src_channel);
            }
            state.pos += step;
            if state.pos >= frames {
                state.pos = frames;
                state.playing = false;
                state.ended = true;
            }
        } else {
            frame.fill(0.0);
        }
    }
}

/// A video's sound track loaded in the output engine. Dropping
/// it stops the output thread and the sound with it.
pub(crate) struct VideoAudioPlayer {
    state: Arc<Mutex<AudioPlayState>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    sample_rate: u32,
}

impl VideoAudioPlayer {
    /// Loads a decoded track and parks it at `at_secs`, playing
    /// or paused as told. Starting is honest, like the voice
    /// engine: with the device sink the output must actually
    /// open before this returns Ok, so a machine with no output
    /// device fails here with the reason.
    pub(crate) fn start(
        audio: &DecodedAudio,
        at_secs: f64,
        playing: bool,
        sink: AudioSink,
    ) -> Result<Self, String> {
        let frames = audio.pcm.len() / usize::from(audio.channels.max(1));
        let pos = (at_secs.max(0.0) * f64::from(audio.sample_rate)).min(frames as f64);
        let state = Arc::new(Mutex::new(AudioPlayState {
            pcm: Arc::clone(&audio.pcm),
            channels: usize::from(audio.channels.max(1)),
            pos,
            playing,
            ended: pos >= frames as f64 && frames > 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let rate = audio.sample_rate;
        let thread = std::thread::Builder::new()
            .name("zaprust-video-audio".into())
            .spawn(move || match sink {
                AudioSink::Null => {
                    let _ = ready_tx.send(Ok(()));
                    run_audio_null(thread_state, thread_stop, rate);
                }
                AudioSink::Device => run_audio_device(thread_state, thread_stop, rate, ready_tx),
            })
            .map_err(|e| format!("Could not start playback: {e}"))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { state, stop, thread: Some(thread), sample_rate: rate }),
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

    /// Seconds of the track the output has consumed so far.
    pub(crate) fn position_secs(&self) -> f64 {
        lock_audio(&self.state).pos / f64::from(self.sample_rate)
    }

    pub(crate) fn is_playing(&self) -> bool {
        lock_audio(&self.state).playing
    }

    /// True once the output has consumed the whole track.
    #[cfg(test)]
    pub(crate) fn is_ended(&self) -> bool {
        lock_audio(&self.state).ended
    }

    pub(crate) fn play(&self) {
        let mut state = lock_audio(&self.state);
        if !state.ended {
            state.playing = true;
        }
    }

    pub(crate) fn pause(&self) {
        lock_audio(&self.state).playing = false;
    }

    /// Moves the playhead to `secs`, keeping the playing or
    /// paused state. Seeking back from the end revives the
    /// track; seeking to the very end finishes it.
    pub(crate) fn seek(&self, secs: f64) {
        let mut state = lock_audio(&self.state);
        let frames = (state.pcm.len() / state.channels.max(1)) as f64;
        state.pos = (secs.max(0.0) * f64::from(self.sample_rate)).clamp(0.0, frames);
        if state.pos >= frames {
            state.playing = false;
            state.ended = true;
        } else {
            state.ended = false;
        }
    }
}

impl Drop for VideoAudioPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The null sink's pump: consumes the track at its own rate in
/// small real-time ticks, through the same [`audio_pull`] the
/// device callback uses, and discards the samples.
fn run_audio_null(state: Arc<Mutex<AudioPlayState>>, stop: Arc<AtomicBool>, rate: u32) {
    let tick = Duration::from_millis(5);
    let frames_per_tick = (f64::from(rate) * 0.005) as usize;
    let channels = lock_audio(&state).channels;
    let mut scratch = vec![0.0f32; frames_per_tick * channels];
    while !stop.load(Ordering::SeqCst) {
        audio_pull(&mut lock_audio(&state), &mut scratch, channels, 1.0);
        std::thread::sleep(tick);
    }
}

/// The device sink: owns the cpal output stream on this worker
/// thread, reports through `ready` whether the stream opened
/// and started, then parks until the player is dropped. It is
/// the voice engine's device path with the channel count and
/// rate of the video's own track.
fn run_audio_device(
    state: Arc<Mutex<AudioPlayState>>,
    stop: Arc<AtomicBool>,
    rate: u32,
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
        cpal::SampleFormat::F32 => {
            build_audio_output::<f32>(&device, &config, Arc::clone(&state), rate, |s| s)
        }
        cpal::SampleFormat::I16 => {
            build_audio_output::<i16>(&device, &config, Arc::clone(&state), rate, |s| {
                (s.clamp(-1.0, 1.0) * 32767.0) as i16
            })
        }
        cpal::SampleFormat::U16 => {
            build_audio_output::<u16>(&device, &config, Arc::clone(&state), rate, |s| {
                ((s.clamp(-1.0, 1.0) * 0.5 + 0.5) * 65535.0) as u16
            })
        }
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
        std::thread::sleep(Duration::from_millis(20));
    }
    // The stream drops here, with the thread.
}

/// Builds one output stream whose callback drains the shared
/// state through [`audio_pull`]. The source rate folds into the
/// per-frame step, as in the voice engine, so pitch and
/// duration stay honest on any device rate.
fn build_audio_output<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    state: Arc<Mutex<AudioPlayState>>,
    rate: u32,
    convert: impl Fn(f32) -> T + Send + 'static,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample,
{
    use cpal::traits::DeviceTrait;
    let channels = usize::from(config.channels).max(1);
    let step = f64::from(rate) / f64::from(config.sample_rate);
    let mut scratch: Vec<f32> = Vec::new();
    device
        .build_output_stream(
            *config,
            move |data: &mut [T], _info| {
                scratch.clear();
                scratch.resize(data.len(), 0.0);
                audio_pull(&mut lock_audio(&state), &mut scratch, channels, step);
                for (slot, sample) in data.iter_mut().zip(scratch.iter()) {
                    *slot = convert(*sample);
                }
            },
            |err| tracing::warn!("video audio output stream error: {err}"),
            None,
        )
        .map_err(|e| format!("Could not open the audio output: {e}"))
}

// ---------------------------------------------------------------------
// Player
// ---------------------------------------------------------------------

/// Where a video's bytes live for the player: a fetched cache
/// file (regular videos) or memory (view-once videos, whose
/// bytes are never written to disk).
#[derive(Debug, Clone)]
pub(crate) enum VideoSource {
    File(PathBuf),
    Bytes(Vec<u8>),
}

/// Control shared with the decode worker: a stop flag and a
/// pending seek target.
#[derive(Default)]
struct WorkerControl {
    stop: AtomicBool,
    seek: Mutex<Option<f64>>,
}

/// Messages from the decode worker to the UI thread.
enum WorkerMsg {
    /// The demuxed stream's real facts, sent before its frames:
    /// they replace the caller's estimates (a message's stated
    /// duration can be absent or rounded).
    Meta(VideoInfo),
    /// The sound track's outcome, sent once, after the meta.
    /// A video with no sound track sends nothing: its silence
    /// is legitimate and earns no note.
    Audio(AudioReport),
    Frame(DecodedFrame),
    /// The stream ended (all frames delivered).
    End,
    /// Playback cannot proceed; the string is user-facing.
    Failed(String),
}

/// A playing (or paused) video: a worker thread decodes ahead
/// into a bounded channel while the UI thread presents frames
/// against a playback clock. The position the UI reports is
/// always the presentation timestamp of the frame actually on
/// screen or the clock's target while playing; nothing is
/// simulated.
pub(crate) struct VideoPlayer {
    /// The app message ID this player is showing, so the viewer
    /// can tell its player apart from a stale one.
    pub key: u64,
    pub info: VideoInfo,
    control: Arc<WorkerControl>,
    rx: mpsc::Receiver<WorkerMsg>,
    /// The frame currently presented.
    pub current: Option<DecodedFrame>,
    /// Frames the worker has delivered but the clock has not
    /// reached yet. Kept short: the worker's bounded channel
    /// provides backpressure, and the UI only drains while this
    /// queue has room.
    queue: VecDeque<DecodedFrame>,
    playing: bool,
    /// Playback-clock anchor: the position `anchor_at` maps to.
    anchor_pts: f64,
    anchor_at: Instant,
    /// The worker has delivered the whole stream.
    stream_done: bool,
    /// Set when the stream is done and the clock has run out.
    pub ended: bool,
    /// A user-facing failure, when playback cannot proceed.
    pub failed: Option<String>,
    /// The sound track's output engine, once its decode report
    /// has arrived and the output opened.
    audio: Option<VideoAudioPlayer>,
    /// How long after the video's start the sound begins.
    audio_delay: f64,
    /// A user-facing sentence about the sound track, when the
    /// video has one that is not playing (an unplayable codec, a
    /// failed decode, no output device). The viewer shows it
    /// under the transport; sound that plays needs no note, and
    /// a video with no sound track never gets one.
    pub audio_note: Option<String>,
    audio_sink: AudioSink,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl VideoPlayer {
    /// Starts a player for one message's video, paused on its
    /// first frame. Demuxing happens on the worker; failures
    /// arrive through [`VideoPlayer::pump`] as `failed`.
    pub(crate) fn start(key: u64, source: VideoSource, info: VideoInfo) -> Self {
        Self::start_with_audio_sink(key, source, info, AudioSink::Device)
    }

    /// [`VideoPlayer::start`] with the sound output chosen:
    /// production passes the device sink; tests pass the null
    /// sink so the sync behaviour runs without speakers.
    pub(crate) fn start_with_audio_sink(
        key: u64,
        source: VideoSource,
        info: VideoInfo,
        audio_sink: AudioSink,
    ) -> Self {
        let control = Arc::new(WorkerControl::default());
        let (tx, rx) = mpsc::sync_channel::<WorkerMsg>(8);
        let worker_control = Arc::clone(&control);
        let worker = std::thread::spawn(move || run_worker(source, worker_control, tx));
        Self {
            key,
            info,
            control,
            rx,
            current: None,
            queue: VecDeque::new(),
            playing: false,
            anchor_pts: 0.0,
            anchor_at: Instant::now(),
            stream_done: false,
            ended: false,
            failed: None,
            audio: None,
            audio_delay: 0.0,
            audio_note: None,
            audio_sink,
            worker: Some(worker),
        }
    }

    /// The position the playback clock has reached, in seconds.
    /// Once the stream has ended that is the full duration: the
    /// playhead reached the end even when the final frame
    /// itself is the one the decoder does not produce (see the
    /// module docs).
    pub(crate) fn position(&self) -> f64 {
        if self.ended {
            self.info.duration_secs
        } else if self.playing {
            (self.anchor_pts + self.anchor_at.elapsed().as_secs_f64()).min(self.info.duration_secs)
        } else if let Some(frame) = &self.current {
            frame.pts
        } else {
            self.anchor_pts
        }
    }

    pub(crate) fn is_playing(&self) -> bool {
        self.playing
    }

    /// Advances the player: drains worker messages and presents
    /// the frame whose timestamp the clock has reached. Returns
    /// true when the presented frame changed (the caller should
    /// repaint).
    pub(crate) fn pump(&mut self) -> bool {
        let mut changed = false;
        // Drain what the worker has delivered, keeping the
        // queue short so the worker's channel backpressures
        // instead of decoding a whole video into memory.
        while self.queue.len() < 4 {
            match self.rx.try_recv() {
                Ok(WorkerMsg::Meta(info)) => self.info = info,
                Ok(WorkerMsg::Audio(report)) => self.handle_audio_report(report),
                Ok(WorkerMsg::Frame(frame)) => self.queue.push_back(frame),
                Ok(WorkerMsg::End) => self.stream_done = true,
                Ok(WorkerMsg::Failed(message)) => {
                    self.failed = Some(message);
                    self.playing = false;
                    if let Some(audio) = &self.audio {
                        audio.pause();
                    }
                }
                Err(_) => break,
            }
        }
        if self.current.is_none() {
            if let Some(frame) = self.queue.pop_front() {
                self.anchor_pts = frame.pts;
                self.anchor_at = Instant::now();
                self.current = Some(frame);
                changed = true;
            }
        }
        if self.playing {
            let target = self.position();
            // Present every queued frame whose time has come.
            while let Some(next) = self.queue.front() {
                if next.pts <= target {
                    let frame = self.queue.pop_front().expect("front checked");
                    self.current = Some(frame);
                    changed = true;
                } else {
                    break;
                }
            }
            if self.stream_done && self.queue.is_empty() && target >= self.info.duration_secs - 0.05
            {
                self.ended = true;
                self.playing = false;
                self.anchor_pts = self.info.duration_secs;
                self.anchor_at = Instant::now();
                if let Some(audio) = &self.audio {
                    audio.pause();
                }
            }
            // The video clock is the master; the sound follows
            // it. Ordinary device-clock drift stays far below
            // this bound; beyond it the sound is re-seeked onto
            // the presented position rather than left audibly
            // out of step.
            if let Some(audio) = &self.audio {
                if audio.is_playing() {
                    let drift = audio.position_secs() + self.audio_delay - target;
                    if drift.abs() > 0.25 {
                        audio.seek((target - self.audio_delay).max(0.0));
                    }
                }
            }
        }
        changed
    }

    /// Applies the worker's sound-track report: a decoded track
    /// opens the output engine at the presented position, in
    /// step with the transport; anything else becomes the note
    /// the viewer shows.
    fn handle_audio_report(&mut self, report: AudioReport) {
        match report {
            AudioReport::Ready(decoded) => {
                self.audio_delay = decoded.start_delay_secs;
                let at = (self.position() - self.audio_delay).max(0.0);
                match VideoAudioPlayer::start(&decoded, at, self.playing, self.audio_sink) {
                    Ok(player) => self.audio = Some(player),
                    Err(reason) => self.audio_note = Some(reason),
                }
            }
            AudioReport::Unsupported { codec } => {
                self.audio_note =
                    Some(format!("This video uses {codec} audio, which Zaprust cannot play yet."));
            }
            AudioReport::Failed(message) => self.audio_note = Some(message),
        }
    }

    /// The sound track's own consumed position, in video
    /// timeline seconds, when sound is playing. Exposed for the
    /// sync tests; the viewer reads the video position only.
    #[cfg(test)]
    pub(crate) fn audio_position(&self) -> Option<f64> {
        self.audio.as_ref().map(|a| a.position_secs() + self.audio_delay)
    }

    /// Whether the sound track's output engine exists and is
    /// consuming. Exposed for the sync tests.
    #[cfg(test)]
    pub(crate) fn audio_playing(&self) -> bool {
        self.audio.as_ref().is_some_and(|a| a.is_playing())
    }

    /// Whether the sound track's output has consumed the whole
    /// track. Exposed for the sync tests.
    #[cfg(test)]
    pub(crate) fn audio_ended(&self) -> bool {
        self.audio.as_ref().is_some_and(|a| a.is_ended())
    }

    /// Starts or resumes playback from the current position.
    pub(crate) fn play(&mut self) {
        if self.ended {
            self.seek(0.0);
        }
        self.anchor_pts = self.current.as_ref().map_or(self.anchor_pts, |f| f.pts);
        self.anchor_at = Instant::now();
        self.playing = true;
        if let Some(audio) = &self.audio {
            // Resume exactly on the presented position, not
            // wherever the sound happened to stop.
            audio.seek((self.anchor_pts - self.audio_delay).max(0.0));
            audio.play();
        }
    }

    /// Pauses playback, freezing on the presented frame.
    pub(crate) fn pause(&mut self) {
        self.anchor_pts = self.position();
        self.anchor_at = Instant::now();
        self.playing = false;
        if let Some(audio) = &self.audio {
            audio.pause();
        }
    }

    pub(crate) fn toggle(&mut self) {
        if self.playing {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Seeks to a position in seconds. The worker restarts
    /// decoding at the nearest preceding keyframe and drops the
    /// frames before the target, so the next presented frame is
    /// the first one at or after the target.
    pub(crate) fn seek(&mut self, target: f64) {
        let target = target.clamp(0.0, self.info.duration_secs);
        if let Ok(mut slot) = self.control.seek.lock() {
            *slot = Some(target);
        }
        self.current = None;
        self.queue.clear();
        self.stream_done = false;
        self.ended = false;
        self.anchor_pts = target;
        self.anchor_at = Instant::now();
        if let Some(audio) = &self.audio {
            // The sound follows the seek, keeping its playing
            // or paused state.
            audio.seek((target - self.audio_delay).max(0.0));
        }
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Sends one message to the UI, waiting out a full channel in
/// short slices so a stop or seek is never stuck behind it.
/// Returns false when the worker should exit.
fn send_msg(tx: &mpsc::SyncSender<WorkerMsg>, control: &WorkerControl, msg: WorkerMsg) -> bool {
    let mut msg = msg;
    loop {
        match tx.try_send(msg) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Full(returned)) => {
                msg = returned;
                if control.stop.load(Ordering::Relaxed) {
                    return false;
                }
                if control.seek.lock().map(|s| s.is_some()).unwrap_or(false) {
                    return true; // caller re-checks the seek slot
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
        }
    }
}

/// Takes a pending seek target out of the control slot.
fn take_seek(control: &WorkerControl) -> Option<f64> {
    control.seek.lock().ok().and_then(|mut slot| slot.take())
}

/// How a decode pass ended: the worker should exit, or the
/// stream is finished and the worker should idle for seeks.
enum DecodeOutcome {
    Stop,
    Finished,
}

/// The decode worker: demuxes, then decodes sequentially,
/// honouring seeks by restarting at the nearest preceding
/// keyframe with a fresh decoder. After the stream ends it
/// reports `End` and idles until a seek restarts it.
fn run_worker(source: VideoSource, control: Arc<WorkerControl>, tx: mpsc::SyncSender<WorkerMsg>) {
    let bytes = match &source {
        VideoSource::File(path) => match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(_) => {
                let _ = tx.send(WorkerMsg::Failed(
                    "That video's downloaded file could not be read.".to_owned(),
                ));
                return;
            }
        },
        VideoSource::Bytes(bytes) => bytes.clone(),
    };
    let demuxed = match demux(&bytes) {
        Ok(demuxed) => demuxed,
        Err(message) => {
            let _ = tx.send(WorkerMsg::Failed(message));
            return;
        }
    };
    if tx.send(WorkerMsg::Meta(demuxed.info)).is_err() {
        return;
    }
    // The sound track is decoded whole, up front: it is small
    // next to the video, and having it ready before the first
    // frame means the sound starts in step when the user
    // presses play. Its report goes out before the frames.
    let audio_report = match &demuxed.audio {
        Some(AudioTrack::Aac(audio)) => Some(match decode_audio(audio) {
            Ok(decoded) => AudioReport::Ready(decoded),
            Err(message) => AudioReport::Failed(message),
        }),
        Some(AudioTrack::Unsupported { codec }) => {
            Some(AudioReport::Unsupported { codec: codec.clone() })
        }
        Some(AudioTrack::Unreadable) => {
            Some(AudioReport::Failed("This video's audio track could not be read.".to_owned()))
        }
        None => None,
    };
    if let Some(report) = audio_report {
        if !send_msg(&tx, &control, WorkerMsg::Audio(report)) {
            return;
        }
    }
    let mut start = 0;
    let mut drop_before = 0.0;
    loop {
        match decode_from(&demuxed, start, drop_before, &control, &tx) {
            DecodeOutcome::Stop => return,
            DecodeOutcome::Finished => {
                if tx.send(WorkerMsg::End).is_err() {
                    return;
                }
                // Idle until a seek restarts decoding or the
                // player goes away.
                loop {
                    if control.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Some(target) = take_seek(&control) {
                        match restart_for_seek(&demuxed, target) {
                            Ok((_, key)) => {
                                start = key;
                                drop_before = target;
                                break;
                            }
                            Err(message) => {
                                let _ = tx.send(WorkerMsg::Failed(message));
                                return;
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
}

/// A fresh decode session positioned at the last keyframe at
/// or before `target`, for a seek restart.
fn restart_for_seek(demuxed: &DemuxedVideo, target: f64) -> Result<(DecodeSession, usize), String> {
    let key = demuxed.units.iter().rposition(|u| u.keyframe && u.pts <= target).unwrap_or(0);
    Ok((DecodeSession::new(demuxed)?, key))
}

/// How many decoded frames the worker holds back to reorder
/// decode order into presentation order. B-frame reordering in
/// practice stays well under this; the holdback only delays the
/// first frames of a stream (or of a seek) by the time it takes
/// to decode this many frames, and the end of the stream
/// releases everything regardless.
const REORDER_DEPTH: usize = 12;

/// Inserts a frame into the reorder buffer, ordered by
/// presentation timestamp.
fn reorder_insert(buffer: &mut Vec<DecodedFrame>, frame: DecodedFrame) {
    let pos = buffer.partition_point(|f| f.pts <= frame.pts);
    buffer.insert(pos, frame);
}

/// Decodes from unit `start`, dropping frames before
/// `drop_before`. Seeks that land mid-stream restart decoding
/// internally at the nearest preceding keyframe. Frames leave
/// through the reorder buffer in presentation order.
fn decode_from(
    demuxed: &DemuxedVideo,
    start: usize,
    drop_before: f64,
    control: &WorkerControl,
    tx: &mpsc::SyncSender<WorkerMsg>,
) -> DecodeOutcome {
    let mut session = match DecodeSession::new(demuxed) {
        Ok(session) => session,
        Err(message) => {
            let _ = tx.send(WorkerMsg::Failed(message));
            return DecodeOutcome::Stop;
        }
    };
    let mut idx = start;
    let mut drop_before = drop_before;
    let mut reorder: Vec<DecodedFrame> = Vec::new();
    'outer: loop {
        if control.stop.load(Ordering::Relaxed) {
            return DecodeOutcome::Stop;
        }
        if let Some(target) = take_seek(control) {
            match restart_for_seek(demuxed, target) {
                Ok((fresh, key)) => {
                    session = fresh;
                    idx = key;
                    drop_before = target;
                    reorder.clear();
                    continue 'outer;
                }
                Err(message) => {
                    let _ = tx.send(WorkerMsg::Failed(message));
                    return DecodeOutcome::Stop;
                }
            }
        }
        if idx >= demuxed.units.len() {
            break 'outer;
        }
        let unit = &demuxed.units[idx];
        idx += 1;
        for frame in session.decode_unit(unit) {
            if frame.pts >= drop_before {
                reorder_insert(&mut reorder, frame);
            }
        }
        while reorder.len() > REORDER_DEPTH {
            let frame = reorder.remove(0);
            if !send_msg(tx, control, WorkerMsg::Frame(frame)) {
                return DecodeOutcome::Stop;
            }
        }
    }
    // End of the units: flush what the decoder still holds,
    // then release the whole reorder buffer in order.
    match session.flush() {
        Ok(frames) => {
            for frame in frames {
                if frame.pts >= drop_before {
                    reorder_insert(&mut reorder, frame);
                }
            }
            for frame in reorder.drain(..) {
                if !send_msg(tx, control, WorkerMsg::Frame(frame)) {
                    return DecodeOutcome::Stop;
                }
            }
        }
        Err(message) => {
            let _ = tx.send(WorkerMsg::Failed(message));
            return DecodeOutcome::Stop;
        }
    }
    DecodeOutcome::Finished
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real 1-second H.264 MP4 (128x96, 10 fps, B-frames on),
    /// generated with ffmpeg's testsrc. It is the fixture the
    /// decode route was verified with before being adopted.
    const FIXTURE: &[u8] = include_bytes!("testdata/tiny.mp4");

    #[test]
    fn demux_reads_the_fixture_track() {
        let demuxed = demux(FIXTURE).expect("the fixture demuxes");
        assert_eq!(demuxed.info.width, 128);
        assert_eq!(demuxed.info.height, 96);
        assert!((demuxed.info.duration_secs - 1.0).abs() < 0.05);
        assert_eq!(demuxed.units.len(), 10);
        assert!(demuxed.units[0].keyframe);
        assert!(!demuxed.sps.is_empty());
        assert!(!demuxed.pps.is_empty());
        // The fixture has B-frames, so decode order differs
        // from presentation order; the timestamps are still
        // exactly the ten presentation slots.
        let mut pts: Vec<f64> = demuxed.units.iter().map(|u| u.pts).collect();
        pts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        for (i, p) in pts.iter().enumerate() {
            assert!((*p - i as f64 * 0.1).abs() < 0.01, "slot {i} has pts {p}");
        }
        let decode_order: Vec<f64> = demuxed.units.iter().map(|u| u.pts).collect();
        assert_ne!(decode_order, pts, "the fixture must exercise reordering");
    }

    #[test]
    fn decode_labels_each_frame_with_its_own_timestamp() {
        let demuxed = demux(FIXTURE).expect("the fixture demuxes");
        let mut session = DecodeSession::new(&demuxed).expect("the decoder starts");
        let mut frames = Vec::new();
        for unit in &demuxed.units {
            frames.extend(session.decode_unit(unit));
        }
        frames.extend(session.flush().expect("the flush succeeds"));
        // The decoder emits in decode order, one frame per unit,
        // and rejects the fixture's final unit (the documented
        // limitation in the module docs): nine frames, labelled
        // with their own timestamps, which are exactly the
        // presentation slots 0.0 to 0.8 between them.
        assert_eq!(frames.len(), 9, "all but the final frame decode");
        let mut pts: Vec<f64> = frames.iter().map(|f| f.pts).collect();
        pts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        for (i, p) in pts.iter().enumerate() {
            assert!((*p - i as f64 * 0.1).abs() < 0.01, "slot {i} has pts {p}");
        }
        for frame in &frames {
            assert_eq!((frame.width, frame.height), (128, 96));
            assert_eq!(frame.rgba.len(), 128 * 96 * 4);
        }
        // Emission really is decode order for this fixture, and
        // the content moves: consecutive frames are not copies.
        let emitted: Vec<f64> = frames.iter().map(|f| f.pts).collect();
        assert_ne!(emitted, pts, "the session emits in decode order");
        assert!(frames.windows(2).any(|w| w[0].rgba != w[1].rgba));
    }

    #[test]
    fn demux_rejects_bytes_that_are_not_a_video() {
        assert!(demux(b"not an mp4 at all").is_err());
        assert!(demux(&[]).is_err());
        assert!(demux(&FIXTURE[..FIXTURE.len() / 2]).is_err());
    }

    #[test]
    fn player_presents_real_frames_plays_to_the_end_and_seeks() {
        let info = VideoInfo { width: 128, height: 96, duration_secs: 1.0 };
        let mut player = VideoPlayer::start(1, VideoSource::Bytes(FIXTURE.to_vec()), info);
        let deadline = Instant::now() + Duration::from_secs(15);
        while player.current.is_none() {
            player.pump();
            assert!(Instant::now() < deadline, "the first frame never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        let first = player.current.as_ref().expect("a frame is presented");
        assert_eq!(first.pts, 0.0);
        assert_eq!(first.rgba.len(), 128 * 96 * 4);
        assert!(!player.is_playing(), "the player starts paused on the first frame");

        player.play();
        while !player.ended {
            player.pump();
            assert!(Instant::now() < deadline, "playback never reached the end");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(player.position() >= 0.95, "the end position is the stream's end");

        player.seek(0.5);
        while player.current.is_none() {
            player.pump();
            assert!(Instant::now() < deadline, "the seek frame never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        let landed = player.current.as_ref().expect("a frame is presented").pts;
        assert!(
            (0.5..0.65).contains(&landed),
            "the seek presents the first frame at or after the target, got {landed}"
        );
    }

    /// A real 1-second H.264 MP4 with a 440 Hz AAC stereo
    /// track at 44.1 kHz, generated with ffmpeg alongside the
    /// video-only fixture. Its edit list carries the encoder's
    /// 1024-frame priming, like a real capture's.
    const AV_FIXTURE: &[u8] = include_bytes!("testdata/tiny_av.mp4");

    /// The same video with an MP3 sound track: the codec the
    /// honest unsupported path exists for.
    const MP3_FIXTURE: &[u8] = include_bytes!("testdata/tiny_mp3.mp4");

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

    #[test]
    fn demux_reads_the_av_fixture_audio_track() {
        let demuxed = demux(AV_FIXTURE).expect("the fixture demuxes");
        assert_eq!(demuxed.units.len(), 10, "the video track is unaffected");
        let Some(AudioTrack::Aac(audio)) = &demuxed.audio else {
            panic!("the fixture's sound track demuxes as AAC, got {:?}", demuxed.audio);
        };
        assert_eq!(audio.sample_rate, 44_100);
        assert_eq!(audio.channels, 2);
        // The AudioSpecificConfig from the esds: AAC LC,
        // 44.1 kHz, stereo.
        assert_eq!(&audio.config[..2], &[0x12, 0x10]);
        assert_eq!(audio.samples.len(), 45, "45 AAC frames of 1024 samples");
        assert!((audio.duration_secs - 1.0).abs() < 0.05);
        // The edit list's media time is the encoder priming:
        // 1024 frames at the track's own timescale.
        assert_eq!(audio.priming_frames, 1024);
        assert_eq!(audio.start_delay_secs, 0.0);
    }

    #[test]
    fn demux_of_the_video_only_fixture_has_no_audio() {
        let demuxed = demux(FIXTURE).expect("the fixture demuxes");
        assert!(demuxed.audio.is_none(), "silence needs no note");
    }

    #[test]
    fn decode_audio_decodes_the_fixture_track() {
        let demuxed = demux(AV_FIXTURE).expect("the fixture demuxes");
        let Some(AudioTrack::Aac(audio)) = &demuxed.audio else {
            panic!("the fixture's sound track demuxes as AAC");
        };
        let decoded = decode_audio(audio).expect("the track decodes");
        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.sample_rate, 44_100);
        // Priming trimmed and the tail cut to the track's own
        // second: exactly the reference decoder's frame count
        // for this fixture (ffmpeg reports 44100 frames).
        assert_eq!(decoded.pcm.len(), 44_100 * 2, "one second of stereo PCM");
        // The 440 Hz tone survives at its recorded level
        // (ffmpeg measures RMS 0.0618 for this fixture).
        let level = rms(&decoded.pcm);
        assert!((0.04..0.08).contains(&level), "the tone is really there, rms {level}");
    }

    #[test]
    fn demux_marks_mp3_audio_unsupported_with_its_name() {
        let demuxed = demux(MP3_FIXTURE).expect("the fixture demuxes");
        match &demuxed.audio {
            Some(AudioTrack::Unsupported { codec }) => assert_eq!(codec, "MP3"),
            other => panic!("MP3 audio is reported unsupported, got {other:?}"),
        }
    }

    fn audio_state(pcm: Vec<i16>, channels: usize) -> AudioPlayState {
        AudioPlayState { pcm: Arc::from(pcm), channels, pos: 0.0, playing: true, ended: false }
    }

    /// `audio_pull` is the only place the sound position
    /// advances: by consumption, in both channels, stopping
    /// exactly at the end, and a paused stream consumes nothing.
    #[test]
    fn audio_pull_advances_by_consumption_and_stops_at_the_end() {
        // 1000 stereo frames of a constant tone.
        let mut state = audio_state(vec![8_000; 2_000], 2);
        let mut out = [0.0f32; 800];
        audio_pull(&mut state, &mut out, 2, 1.0);
        assert_eq!(state.pos, 400.0);
        assert!(out.iter().all(|s| *s > 0.2), "both channels carry the tone");
        state.playing = false;
        audio_pull(&mut state, &mut out, 2, 1.0);
        assert_eq!(state.pos, 400.0, "a paused stream consumes nothing");
        assert!(out.iter().all(|s| *s == 0.0), "a paused stream is silent");
        state.playing = true;
        let mut tail = [0.0f32; 1_400];
        audio_pull(&mut state, &mut tail, 2, 1.0);
        assert!(state.ended, "consumption reached the end");
        assert_eq!(state.pos, 1_000.0);
        assert!(tail[1_198] > 0.2 && tail[1_200] == 0.0, "sound stops exactly at the end");
    }

    /// A mono track pulled for a stereo output lands on both
    /// channels: the channel fold is what a mono voice-heavy
    /// video sounds like on a stereo device.
    #[test]
    fn audio_pull_duplicates_mono_onto_a_stereo_output() {
        let mut state = audio_state(vec![8_000; 500], 1);
        let mut out = [0.0f32; 200];
        audio_pull(&mut state, &mut out, 2, 1.0);
        assert_eq!(state.pos, 100.0);
        assert!(out.iter().all(|s| *s > 0.2), "every output channel carries the mono tone");
    }

    /// Waits until `cond` holds or the deadline passes; the
    /// sync tests run against a real-time sink, so they poll
    /// rather than sleep exact durations.
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
    fn player_plays_the_sound_track_in_step() {
        let info = VideoInfo { width: 128, height: 96, duration_secs: 1.0 };
        let mut player = VideoPlayer::start_with_audio_sink(
            3,
            VideoSource::Bytes(AV_FIXTURE.to_vec()),
            info,
            AudioSink::Null,
        );
        let deadline = Duration::from_secs(15);
        // The first frame and the sound engine both arrive.
        assert!(
            wait_until(deadline, || {
                player.pump();
                player.current.is_some() && player.audio_position().is_some()
            }),
            "the frame and the sound both arrive"
        );
        assert!(player.audio_note.is_none(), "playable sound earns no note");
        assert!(!player.audio_playing(), "the sound starts paused with the video");

        player.play();
        assert!(player.audio_playing(), "play starts the sound");
        assert!(
            wait_until(deadline, || {
                player.pump();
                player.position() > 0.3
            }),
            "playback advances"
        );
        let drift = (player.audio_position().expect("sound exists") - player.position()).abs();
        assert!(drift < 0.35, "the sound stays in step with the picture, drift {drift}");

        player.pause();
        std::thread::sleep(Duration::from_millis(30));
        let frozen = player.audio_position().expect("sound exists");
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(
            player.audio_position().expect("sound exists"),
            frozen,
            "pause freezes the sound with the picture"
        );

        player.seek(0.5);
        let landed = player.audio_position().expect("sound exists");
        assert!((landed - 0.5).abs() < 0.05, "the seek moves the sound too, got {landed}");

        player.play();
        assert!(
            wait_until(deadline, || {
                player.pump();
                player.ended
            }),
            "playback reaches the end"
        );
        let stopped = player.audio_position().expect("sound exists");
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(
            player.audio_position().expect("sound exists"),
            stopped,
            "the sound stops at the end with the picture"
        );
        assert!(!player.audio_playing(), "the sound is no longer consuming at the end");
        player.seek(0.0);
        assert!(!player.audio_ended(), "seeking back from the end revives the sound track");
    }

    #[test]
    fn player_states_the_codec_when_the_audio_cannot_play() {
        let info = VideoInfo { width: 128, height: 96, duration_secs: 1.0 };
        let mut player = VideoPlayer::start_with_audio_sink(
            4,
            VideoSource::Bytes(MP3_FIXTURE.to_vec()),
            info,
            AudioSink::Null,
        );
        let deadline = Duration::from_secs(15);
        assert!(
            wait_until(deadline, || {
                player.pump();
                player.audio_note.is_some() && player.current.is_some()
            }),
            "the note and the picture both arrive"
        );
        let note = player.audio_note.clone().expect("a note is stated");
        assert!(note.contains("MP3"), "the note names the codec: {note}");
        assert!(player.audio_position().is_none(), "no sound engine pretends to play");
    }

    #[test]
    fn player_reports_an_unplayable_source_instead_of_pretending() {
        let info = VideoInfo { width: 0, height: 0, duration_secs: 0.0 };
        let mut player = VideoPlayer::start(2, VideoSource::Bytes(b"junk".to_vec()), info);
        let deadline = Instant::now() + Duration::from_secs(15);
        while player.failed.is_none() {
            player.pump();
            assert!(Instant::now() < deadline, "the failure never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(player.current.is_none(), "no frame is presented for junk bytes");
    }
}
