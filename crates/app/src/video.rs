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

/// Demuxes the first H.264 video track of an MP4 file's bytes.
///
/// # Errors
/// A user-facing sentence when the bytes are not an MP4 with an
/// H.264 video track this demuxer understands.
pub(crate) fn demux(data: &[u8]) -> Result<DemuxedVideo, String> {
    let not_video = || "That file is not a playable video.".to_owned();
    let moov = find_child(data, fourcc("moov")).ok_or_else(not_video)?;
    for trak in children(moov, fourcc("trak")) {
        let Some(mdia) = find_child(trak, fourcc("mdia")) else { continue };
        let Some(hdlr) = find_child(mdia, fourcc("hdlr")) else { continue };
        if be_u32(hdlr, 8) != Some(fourcc("vide")) {
            continue;
        }
        return demux_track(data, mdia).map_err(|e| e.unwrap_or_else(not_video));
    }
    Err("That video has no video track to play.".to_owned())
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

    let durations = stts_durations(find_child(stbl, fourcc("stts")).ok_or(None)?).ok_or(None)?;
    let samples = durations.len();
    let offsets_ctts = ctts_offsets(find_child(stbl, fourcc("ctts")), samples).ok_or(None)?;
    let sizes = sample_sizes(find_child(stbl, fourcc("stsz")).ok_or(None)?).ok_or(None)?;
    if sizes.len() != samples {
        return Err(None);
    }
    let keyframes = match find_child(stbl, fourcc("stss")) {
        Some(stss) => {
            let mut flags = stss_keyframes(stss).ok_or(None)?;
            flags.resize(samples, false);
            flags
        }
        None => vec![true; samples],
    };
    let chunks = chunk_offsets(stbl).ok_or(None)?;
    let per_chunk = samples_per_chunk(find_child(stbl, fourcc("stsc")).ok_or(None)?, chunks.len())
        .ok_or(None)?;

    // Lay out each sample's bytes and timestamps.
    let mut units = Vec::with_capacity(samples);
    let mut dts: i64 = 0;
    let mut sample_idx = 0usize;
    for (ci, &chunk_off) in chunks.iter().enumerate() {
        let mut off = usize::try_from(chunk_off).map_err(|_| None)?;
        for _ in 0..per_chunk[ci] {
            if sample_idx >= samples {
                return Err(None);
            }
            let size = sizes[sample_idx];
            let bytes = data.get(off..off + size).ok_or(None)?;
            let pts_ticks = dts + offsets_ctts[sample_idx];
            units.push(AccessUnit {
                pts: pts_ticks as f64 / timescale,
                keyframe: keyframes[sample_idx],
                avcc: bytes.to_vec(),
            });
            dts += i64::from(durations[sample_idx]);
            off += size;
            sample_idx += 1;
        }
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
    worker: Option<std::thread::JoinHandle<()>>,
}

impl VideoPlayer {
    /// Starts a player for one message's video, paused on its
    /// first frame. Demuxing happens on the worker; failures
    /// arrive through [`VideoPlayer::pump`] as `failed`.
    pub(crate) fn start(key: u64, source: VideoSource, info: VideoInfo) -> Self {
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
                Ok(WorkerMsg::Frame(frame)) => self.queue.push_back(frame),
                Ok(WorkerMsg::End) => self.stream_done = true,
                Ok(WorkerMsg::Failed(message)) => {
                    self.failed = Some(message);
                    self.playing = false;
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
            }
        }
        changed
    }

    /// Starts or resumes playback from the current position.
    pub(crate) fn play(&mut self) {
        if self.ended {
            self.seek(0.0);
        }
        self.anchor_pts = self.current.as_ref().map_or(self.anchor_pts, |f| f.pts);
        self.anchor_at = Instant::now();
        self.playing = true;
    }

    /// Pauses playback, freezing on the presented frame.
    pub(crate) fn pause(&mut self) {
        self.anchor_pts = self.position();
        self.anchor_at = Instant::now();
        self.playing = false;
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
