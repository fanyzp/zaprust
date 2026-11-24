//! Call video: the bridge between the protocol library's video
//! ports (pre-encoded H.264 Annex-B access units; the library
//! transports them and never touches pixels) and the devices a
//! video call actually uses.
//!
//! Two directions, each placed next to what it serves:
//! - Sending is a device concern like the microphone, so it
//!   lives here next to [`crate::audio::CallAudio`]: a capture
//!   thread speaks Video4Linux2 to the machine's camera,
//!   converts YUYV to YUV420, encodes with the same vendored
//!   OpenH264 the app decodes chat video with, and paces access
//!   units into the source channel at 15 fps. The thread also
//!   publishes a downscaled RGBA preview of the local picture
//!   (the newest frame only) for the call card's local tile.
//! - Receiving is a display concern: the sink side only
//!   forwards each access unit to the app as a
//!   [`BackendEvent::CallVideoAu`] (dropped under pressure via
//!   [`EventSink::try_emit`], never blocking the media path),
//!   and the app decodes it with its own player pipeline.
//!
//! A machine with no camera, a camera that will not open, or a
//! camera offering no usable format is a normal case, reported
//! through [`CallVideo::camera_note`]: the call still works,
//! receiving video and sending voice. Nothing here panics on a
//! device problem.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use whatsapp_rust::async_channel::{Receiver, Sender};
use whatsapp_rust::voip::{VideoFrame, VideoSink, VideoSource};

use crate::{BackendEvent, EventSink};

/// Capture width asked of the camera. 640x480 at 15 fps sits
/// inside the H.264 Constrained Baseline envelope the call
/// profile documents (up to 1280x720 at 20 fps) and keeps the
/// encode cheap on the machines this app runs on.
const CAPTURE_W: u32 = 640;
/// Capture height asked of the camera.
const CAPTURE_H: u32 = 480;
/// Encoded frames per second.
const FPS: u32 = 15;
/// Target encoder bitrate.
const BITRATE_BPS: u32 = 900_000;
/// The preview tile's widest edge, in pixels.
const PREVIEW_MAX_W: usize = 320;

/// A camera endpoint: hands the engine a receiver of access
/// units.
#[derive(Clone)]
pub(crate) struct ChannelVideoSource {
    rx: Receiver<Vec<u8>>,
}

impl VideoSource for ChannelVideoSource {
    fn frames(&self) -> Receiver<Vec<u8>> {
        self.rx.clone()
    }
}

/// A remote-video endpoint: hands the engine a sender for the
/// frames it reassembles.
#[derive(Clone)]
pub(crate) struct ChannelVideoSink {
    tx: Sender<VideoFrame>,
}

impl VideoSink for ChannelVideoSink {
    fn playout(&self) -> Sender<VideoFrame> {
        self.tx.clone()
    }
}

/// One call's video plumbing: the two ports plus the lifecycle
/// handles for the capture and forwarding threads behind them.
pub(crate) struct CallVideo {
    pub source: ChannelVideoSource,
    pub sink: ChannelVideoSink,
    /// A camera opened and is feeding the source.
    pub camera_on: bool,
    /// Why the camera is not feeding the source, in user words,
    /// when it is not. None while the camera runs.
    pub camera_note: Option<String>,
    /// Asks the capture thread to make the next encoded frame a
    /// keyframe (the engine requests one when a peer joins or
    /// its decoder loses sync).
    pub force_keyframe: Arc<AtomicBool>,
    /// Remote frames the forwarder had to shed because the UI
    /// was not draining. The call pump watches this counter:
    /// when it advances, the peer is asked for a fresh keyframe
    /// so the app's decoder resyncs instead of painting a
    /// damaged picture until the peer's next periodic one.
    pub dropped_frames: Arc<std::sync::atomic::AtomicU64>,
    stop: std::sync::mpsc::Sender<()>,
    /// Parks the access-unit sender when no capture thread owns
    /// it (no camera), so the source channel stays open and the
    /// engine sees a quiet source rather than a closed one.
    _parked_au_tx: Option<Sender<Vec<u8>>>,
}

impl CallVideo {
    /// Stops the capture and forwarding threads (idempotent).
    pub(crate) fn shutdown(&self) {
        let _ = self.stop.send(());
    }

    /// Builds the plumbing: starts the frame forwarder, then
    /// tries the machine's cameras in order. Never fails: a
    /// camera problem lands in `camera_note` instead.
    pub(crate) fn real(events: EventSink) -> Self {
        let (au_tx, au_rx) = whatsapp_rust::async_channel::bounded::<Vec<u8>>(8);
        let (frame_tx, frame_rx) = whatsapp_rust::async_channel::bounded::<VideoFrame>(8);
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let force_keyframe = Arc::new(AtomicBool::new(true));
        let dropped_frames = Arc::new(std::sync::atomic::AtomicU64::new(0));

        // Forwarder: engine frames to app events. try_emit only:
        // a stalled UI sheds frames here rather than backing up
        // into the call's media driver; the shed count feeds the
        // pump's keyframe repair.
        {
            let events = events.clone();
            let dropped = dropped_frames.clone();
            std::thread::Builder::new()
                .name("zaprust-video-out".to_owned())
                .spawn(move || {
                    while let Ok(frame) = frame_rx.recv_blocking() {
                        let event = BackendEvent::CallVideoAu {
                            sender: frame.sender.map(|j| j.to_string()),
                            data: frame.data,
                            keyframe: frame.keyframe,
                            orientation: frame.orientation,
                        };
                        if !events.try_emit(event) {
                            dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                })
                .ok();
        }

        // Capture: the thread reports its setup outcome on a
        // handshake channel so the caller knows synchronously
        // whether a camera is feeding the source. The stop
        // channel's only listener is the capture thread (the
        // forwarder ends when its sender drops with the call);
        // if that thread already exited, shutdown's send simply
        // lands nowhere, which the send's caller ignores.
        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let capture_stop = stop_rx;
        let thread = {
            let au_tx = au_tx.clone();
            let kf = force_keyframe.clone();
            std::thread::Builder::new()
                .name("zaprust-video-in".to_owned())
                .spawn(move || {
                    capture_thread(au_tx, events, kf, capture_stop, setup_tx);
                })
        };
        let (camera_on, camera_note, parked) = match thread {
            Ok(_) => match setup_rx.recv_timeout(std::time::Duration::from_secs(3)) {
                Ok(Ok(())) => (true, None, None),
                Ok(Err(note)) => (false, Some(note), Some(au_tx.clone())),
                Err(_) => (
                    false,
                    Some("The camera did not answer in time.".to_owned()),
                    Some(au_tx.clone()),
                ),
            },
            Err(_) => (
                false,
                Some("The camera could not be started on this machine.".to_owned()),
                Some(au_tx.clone()),
            ),
        };
        Self {
            source: ChannelVideoSource { rx: au_rx },
            sink: ChannelVideoSink { tx: frame_tx },
            camera_on,
            camera_note,
            force_keyframe,
            dropped_frames,
            stop: stop_tx,
            _parked_au_tx: parked,
        }
    }
}

/// The capture thread: finds a camera, negotiates YUYV at the
/// capture size, then loops poll, dequeue, convert, encode,
/// requeue until told to stop. Every failure path reports
/// through `setup` (before the handshake) or just ends the
/// thread (after it: the call keeps its receive side, and the
/// engine's own keyframe requests simply go unanswered).
fn capture_thread(
    au_tx: Sender<Vec<u8>>,
    events: EventSink,
    force_keyframe: Arc<AtomicBool>,
    stop_rx: std::sync::mpsc::Receiver<()>,
    setup: std::sync::mpsc::Sender<Result<(), String>>,
) {
    let mut camera = match Camera::open_first() {
        Ok(c) => c,
        Err(note) => {
            let _ = setup.send(Err(note));
            return;
        }
    };
    let encoder = match make_encoder() {
        Ok(e) => e,
        Err(note) => {
            let _ = setup.send(Err(note));
            return;
        }
    };
    let _ = setup.send(Ok(()));
    camera.run(au_tx, events, force_keyframe, stop_rx, encoder);
}

/// Opens an OpenH264 encoder for the capture size: Constrained
/// Baseline (the call profile's codec envelope), a keyframe at
/// least every three seconds, no frame skipping (the capture
/// loop paces instead, so the encoder never has to guess which
/// frames matter).
fn make_encoder() -> Result<openh264::encoder::Encoder, String> {
    use openh264::encoder::{
        BitRate, EncoderConfig, FrameRate, IntraFramePeriod, Profile, UsageType,
    };
    use openh264::OpenH264API;
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(BITRATE_BPS))
        .max_frame_rate(FrameRate::from_hz(FPS as f32))
        .usage_type(UsageType::CameraVideoRealTime)
        .profile(Profile::Baseline)
        .intra_frame_period(IntraFramePeriod::from_num_frames(FPS * 3))
        .enable_skip_frame(false);
    openh264::encoder::Encoder::with_api_config(OpenH264API::from_source(), config)
        .map_err(|_| "The video encoder could not start on this device.".to_owned())
}

// ---------------------------------------------------------------------
// Video4Linux2, spoken directly over libc.
//
// The ioctl numbers and struct layouts below are the fixed
// kernel ABI, cross-checked against <linux/videodev2.h> on the
// build box (sizes: capability 104, format 208, requestbuffers
// 20, buffer 88) and pinned by the layout test at the bottom of
// this file. The `v4l` crate is deliberately not used: its sys
// crate runs bindgen at build time and would require libclang
// on every build host.
// ---------------------------------------------------------------------

const VIDIOC_QUERYCAP: libc::c_ulong = 0x8068_5600;
const VIDIOC_S_FMT: libc::c_ulong = 0xc0d0_5605;
const VIDIOC_REQBUFS: libc::c_ulong = 0xc014_5608;
const VIDIOC_QUERYBUF: libc::c_ulong = 0xc058_5609;
const VIDIOC_QBUF: libc::c_ulong = 0xc058_560f;
const VIDIOC_DQBUF: libc::c_ulong = 0xc058_5611;
const VIDIOC_STREAMON: libc::c_ulong = 0x4004_5612;
const VIDIOC_STREAMOFF: libc::c_ulong = 0x4004_5613;

const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const V4L2_MEMORY_MMAP: u32 = 1;
const V4L2_FIELD_NONE: u32 = 1;
const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const V4L2_CAP_STREAMING: u32 = 0x0400_0000;
const FOURCC_YUYV: u32 = 0x5659_5559;

/// `struct v4l2_format` (208 bytes): only the pix fields are
/// modeled; the rest of the union stays zeroed bytes.
#[repr(C)]
struct V4l2Format {
    typ: u32,
    _align: u32,
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
    _rest: [u8; 208 - 56],
}

/// `struct v4l2_requestbuffers` (20 bytes).
#[repr(C)]
struct V4l2RequestBuffers {
    count: u32,
    typ: u32,
    memory: u32,
    capabilities: u32,
    reserved: u32,
}

/// `struct v4l2_buffer` (88 bytes), mmap flavor.
#[repr(C)]
struct V4l2Buffer {
    index: u32,
    typ: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    _ts_align: u32,
    timestamp_s: i64,
    timestamp_us: i64,
    _timecode: [u8; 16],
    sequence: u32,
    memory: u32,
    m_offset: u32,
    _m_pad: u32,
    length: u32,
    _reserved: [u8; 12],
}

/// One open camera: the device fd, the negotiated frame shape,
/// and the mmap'd capture buffers.
struct Camera {
    fd: libc::c_int,
    width: u32,
    height: u32,
    buffers: Vec<(*mut libc::c_void, usize)>,
}

impl Camera {
    /// Tries /dev/video0 through /dev/video7 in order and
    /// returns the first device that answers as a streaming
    /// capture device and accepts YUYV at (or near) the capture
    /// size. The notes distinguish the three ways this fails,
    /// because the call card words them differently.
    fn open_first() -> Result<Self, String> {
        let mut saw_device = false;
        for index in 0..8 {
            let path = format!("/dev/video{index}");
            let c_path = match std::ffi::CString::new(path) {
                Ok(p) => p,
                Err(_) => continue,
            };
            // SAFETY: a plain open of a device node; the fd is
            // owned by the returned struct from here on.
            let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            if fd < 0 {
                continue;
            }
            saw_device = true;
            match Self::negotiate(fd) {
                Ok(camera) => return Ok(camera),
                Err(()) => {
                    // SAFETY: fd came from the open above and is
                    // not used after this.
                    unsafe { libc::close(fd) };
                }
            }
        }
        Err(if saw_device {
            "The camera would not open, or its format is not supported.".to_owned()
        } else {
            "No camera found on this machine.".to_owned()
        })
    }

    /// QUERYCAP, S_FMT, REQBUFS, mmap: the full setup for one
    /// device. Any failure is a plain Err; the caller closes
    /// the fd and tries the next device.
    fn negotiate(fd: libc::c_int) -> Result<Self, ()> {
        // Capability check: capture + streaming, from the
        // device_caps when the driver reports them, else the
        // top-level capabilities (offsets 84 and 88 in the
        // 104-byte capability struct).
        let mut capability = [0u8; 104];
        // SAFETY: ioctl with a correctly sized buffer for
        // VIDIOC_QUERYCAP.
        if unsafe { libc::ioctl(fd, VIDIOC_QUERYCAP, capability.as_mut_ptr()) } < 0 {
            return Err(());
        }
        let read_u32 = |at: usize| {
            u32::from_ne_bytes([
                capability[at],
                capability[at + 1],
                capability[at + 2],
                capability[at + 3],
            ])
        };
        let caps = {
            let device_caps = read_u32(88);
            if device_caps != 0 { device_caps } else { read_u32(84) }
        };
        if caps & (V4L2_CAP_VIDEO_CAPTURE | V4L2_CAP_STREAMING)
            != (V4L2_CAP_VIDEO_CAPTURE | V4L2_CAP_STREAMING)
        {
            return Err(());
        }

        let mut format: V4l2Format = unsafe { std::mem::zeroed() };
        format.typ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        format.width = CAPTURE_W;
        format.height = CAPTURE_H;
        format.pixelformat = FOURCC_YUYV;
        format.field = V4L2_FIELD_NONE;
        // SAFETY: ioctl with a struct of the ABI size for
        // VIDIOC_S_FMT; the driver writes the accepted shape
        // back into it.
        if unsafe { libc::ioctl(fd, VIDIOC_S_FMT, &mut format) } < 0 {
            return Err(());
        }
        // The driver may adjust the size; it may not substitute
        // a compressed format this pipeline cannot convert.
        if format.pixelformat != FOURCC_YUYV
            || format.width == 0
            || format.height == 0
            || format.width % 2 != 0
            || format.height % 2 != 0
        {
            return Err(());
        }
        let (width, height) = (format.width, format.height);

        let mut request: V4l2RequestBuffers = unsafe { std::mem::zeroed() };
        request.count = 4;
        request.typ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        request.memory = V4L2_MEMORY_MMAP;
        // SAFETY: ioctl with a struct of the ABI size for
        // VIDIOC_REQBUFS.
        if unsafe { libc::ioctl(fd, VIDIOC_REQBUFS, &mut request) } < 0 || request.count == 0 {
            return Err(());
        }

        let mut buffers = Vec::new();
        for index in 0..request.count {
            let mut buffer: V4l2Buffer = unsafe { std::mem::zeroed() };
            buffer.typ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            buffer.memory = V4L2_MEMORY_MMAP;
            buffer.index = index;
            // SAFETY: ioctl with a struct of the ABI size for
            // VIDIOC_QUERYBUF; the driver fills length and the
            // mmap offset.
            if unsafe { libc::ioctl(fd, VIDIOC_QUERYBUF, &mut buffer) } < 0 {
                return Err(());
            }
            // SAFETY: mmap of the driver-owned capture buffer
            // at the offset QUERYBUF returned, for exactly its
            // length.
            let map = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    buffer.length as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    libc::off_t::from(buffer.m_offset),
                )
            };
            if map == libc::MAP_FAILED {
                return Err(());
            }
            buffers.push((map, buffer.length as usize));
        }

        let mut camera = Self { fd, width, height, buffers };
        camera.queue_all()?;
        let mut typ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        // SAFETY: STREAMON takes a pointer to the buffer type.
        if unsafe { libc::ioctl(fd, VIDIOC_STREAMON, &mut typ) } < 0 {
            return Err(());
        }
        Ok(camera)
    }

    /// Queues every capture buffer with the driver.
    fn queue_all(&mut self) -> Result<(), ()> {
        for index in 0..self.buffers.len() {
            self.queue(index as u32)?;
        }
        Ok(())
    }

    /// Queues one capture buffer.
    fn queue(&self, index: u32) -> Result<(), ()> {
        let mut buffer: V4l2Buffer = unsafe { std::mem::zeroed() };
        buffer.typ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        buffer.memory = V4L2_MEMORY_MMAP;
        buffer.index = index;
        // SAFETY: ioctl with a struct of the ABI size for
        // VIDIOC_QBUF.
        if unsafe { libc::ioctl(self.fd, VIDIOC_QBUF, &mut buffer) } < 0 {
            return Err(());
        }
        Ok(())
    }

    /// The frame loop. Runs until the stop channel fires or the
    /// device fails repeatedly; every exit path streams off and
    /// unmaps through Drop.
    fn run(
        &mut self,
        au_tx: Sender<Vec<u8>>,
        events: EventSink,
        force_keyframe: Arc<AtomicBool>,
        stop_rx: std::sync::mpsc::Receiver<()>,
        mut encoder: openh264::encoder::Encoder,
    ) {
        let frame_interval = std::time::Duration::from_millis(1000 / u64::from(FPS));
        let mut last_encode: Option<std::time::Instant> = None;
        let mut encoded: u64 = 0;
        let mut failures: u32 = 0;
        loop {
            if stop_rx.try_recv().is_ok() {
                return;
            }
            // Wait for a frame (or the stop check's turn).
            let mut pollfd = libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll on the camera fd with a one-slot
            // pollfd array.
            let ready = unsafe { libc::poll(&mut pollfd, 1, 200) };
            if ready < 0 {
                failures += 1;
                if failures > 10 {
                    return;
                }
                continue;
            }
            if ready == 0 || pollfd.revents & libc::POLLIN == 0 {
                continue;
            }
            let mut buffer: V4l2Buffer = unsafe { std::mem::zeroed() };
            buffer.typ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            buffer.memory = V4L2_MEMORY_MMAP;
            // SAFETY: ioctl with a struct of the ABI size for
            // VIDIOC_DQBUF, gated by the poll above.
            if unsafe { libc::ioctl(self.fd, VIDIOC_DQBUF, &mut buffer) } < 0 {
                failures += 1;
                if failures > 10 {
                    return;
                }
                continue;
            }
            failures = 0;
            let index = buffer.index as usize;
            let Some(&(map, map_len)) = self.buffers.get(index) else {
                return;
            };
            // Some drivers leave bytesused at zero for capture;
            // the mapped length is the frame's size then.
            let used = if buffer.bytesused == 0 {
                map_len
            } else {
                (buffer.bytesused as usize).min(map_len)
            };
            // SAFETY: the driver filled `used` bytes of the
            // mmap'd buffer; the slice is consumed (converted)
            // before the buffer is requeued below.
            let raw = unsafe { std::slice::from_raw_parts(map.cast::<u8>(), used) };
            let pace_ok = last_encode.is_none_or(|at| at.elapsed() >= frame_interval);
            if pace_ok && used > 0 {
                last_encode = Some(std::time::Instant::now());
                if let Some(yuv) = yuyv_to_yuv420(raw, self.width, self.height) {
                    if force_keyframe.swap(false, Ordering::Relaxed) {
                        encoder.force_intra_frame();
                    }
                    let source = openh264::formats::YUVBuffer::from_vec(
                        yuv.clone(),
                        self.width as usize,
                        self.height as usize,
                    );
                    if let Ok(stream) = encoder.encode(&source) {
                        let mut au = Vec::new();
                        stream.write_vec(&mut au);
                        if !au.is_empty() {
                            // Drop rather than queue stale video:
                            // the channel is shallow on purpose.
                            let _ = au_tx.try_send(au);
                        }
                    }
                    encoded += 1;
                    if encoded % 5 == 1 {
                        if let Some((rgba, w, h)) = preview_rgba(&yuv, self.width, self.height) {
                            events.try_emit(BackendEvent::CallVideoFrame {
                                rgba,
                                width: w,
                                height: h,
                            });
                        }
                    }
                }
            }
            if self.queue(buffer.index).is_err() {
                return;
            }
        }
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        let mut typ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        // SAFETY: STREAMOFF on the owned fd, then unmap the
        // capture buffers and close. All best effort: teardown
        // must not panic.
        unsafe {
            libc::ioctl(self.fd, VIDIOC_STREAMOFF, &mut typ);
            for (map, len) in &self.buffers {
                libc::munmap(*map, *len);
            }
            libc::close(self.fd);
        }
    }
}

/// Converts one YUYV frame (Y0 U Y1 V quads) to planar YUV420:
/// luma per pixel, chroma averaged over each 2x2 block. Returns
/// None when the byte count does not match the frame shape.
fn yuyv_to_yuv420(raw: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    if raw.len() < w * h * 2 || w % 2 != 0 || h % 2 != 0 {
        return None;
    }
    let mut yuv = vec![0u8; w * h * 3 / 2];
    let (luma, chroma) = yuv.split_at_mut(w * h);
    let (u_plane, v_plane) = chroma.split_at_mut(w * h / 4);
    for row in 0..h {
        for col in 0..w {
            let quad = (row * w + (col & !1)) * 2;
            let y = if col % 2 == 0 { raw[quad] } else { raw[quad + 2] };
            luma[row * w + col] = y;
        }
    }
    for row in (0..h).step_by(2) {
        for col in (0..w).step_by(2) {
            let quad = (row * w + col) * 2;
            let below = ((row + 1) * w + col) * 2;
            let u = (u16::from(raw[quad + 1]) + u16::from(raw[below + 1])) / 2;
            let v = (u16::from(raw[quad + 3]) + u16::from(raw[below + 3])) / 2;
            let at = (row / 2) * (w / 2) + col / 2;
            u_plane[at] = u as u8;
            v_plane[at] = v as u8;
        }
    }
    Some(yuv)
}

/// Builds the local preview's RGBA pixels from a YUV420 frame,
/// downscaled by nearest neighbor to at most PREVIEW_MAX_W
/// wide. None for a degenerate frame.
fn preview_rgba(yuv: &[u8], width: u32, height: u32) -> Option<(Vec<u8>, u32, u32)> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || yuv.len() < w * h * 3 / 2 {
        return None;
    }
    let step = (w / PREVIEW_MAX_W).max(1);
    let (tw, th) = (w / step, h / step);
    if tw == 0 || th == 0 {
        return None;
    }
    let (luma, chroma) = yuv.split_at(w * h);
    let (u_plane, v_plane) = chroma.split_at(w * h / 4);
    let mut rgba = vec![0u8; tw * th * 4];
    for ty in 0..th {
        for tx in 0..tw {
            let (sx, sy) = (tx * step, ty * step);
            let y = f32::from(luma[sy * w + sx]);
            let ci = (sy / 2) * (w / 2) + sx / 2;
            let u = f32::from(u_plane[ci]) - 128.0;
            let v = f32::from(v_plane[ci]) - 128.0;
            let at = (ty * tw + tx) * 4;
            rgba[at] = clamp_u8(1.164 * (y - 16.0) + 1.596 * v);
            rgba[at + 1] = clamp_u8(1.164 * (y - 16.0) - 0.813 * v - 0.391 * u);
            rgba[at + 2] = clamp_u8(1.164 * (y - 16.0) + 2.018 * u);
            rgba[at + 3] = 255;
        }
    }
    Some((rgba, tw as u32, th as u32))
}

/// Rounds and clamps a color channel to a byte.
fn clamp_u8(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-written ABI structs must match the kernel's
    /// sizes exactly: an ioctl number encodes its struct size,
    /// so a drift here would fail silently at runtime instead.
    /// (Cross-checked against <linux/videodev2.h>: format 208,
    /// requestbuffers 20, buffer 88.)
    #[test]
    fn v4l2_struct_layouts_match_the_kernel_abi() {
        assert_eq!(std::mem::size_of::<V4l2Format>(), 208);
        assert_eq!(std::mem::offset_of!(V4l2Format, width), 8);
        assert_eq!(std::mem::offset_of!(V4l2Format, pixelformat), 16);
        assert_eq!(std::mem::offset_of!(V4l2Format, sizeimage), 28);
        assert_eq!(std::mem::size_of::<V4l2RequestBuffers>(), 20);
        assert_eq!(std::mem::size_of::<V4l2Buffer>(), 88);
        assert_eq!(std::mem::offset_of!(V4l2Buffer, bytesused), 8);
        assert_eq!(std::mem::offset_of!(V4l2Buffer, memory), 60);
        assert_eq!(std::mem::offset_of!(V4l2Buffer, m_offset), 64);
        assert_eq!(std::mem::offset_of!(V4l2Buffer, length), 72);
    }

    /// A known 2x2 YUYV quad converts to the luma of all four
    /// pixels and the single shared chroma sample.
    #[test]
    fn yuyv_conversion_keeps_luma_and_averages_chroma() {
        // Two rows of one quad each: Y0=10 U=100 Y1=20 V=200,
        // then Y0=30 U=110 Y1=40 V=210.
        let raw: [u8; 16] = [
            10, 100, 20, 200, //
            30, 110, 40, 210,
        ];
        let yuv = yuyv_to_yuv420(&raw, 2, 2).expect("a 2x2 frame converts");
        assert_eq!(&yuv[0..4], &[10, 20, 30, 40], "luma in raster order");
        assert_eq!(yuv[4], 105, "U averaged over the block");
        assert_eq!(yuv[5], 205, "V averaged over the block");
    }

    /// A short buffer is not a frame.
    #[test]
    fn yuyv_conversion_rejects_a_short_buffer() {
        assert!(yuyv_to_yuv420(&[0u8; 4], 640, 480).is_none());
    }

    /// The preview of a solid mid-gray frame is mid-gray at the
    /// capped size.
    #[test]
    fn preview_downscales_and_converts() {
        let (w, h) = (640usize, 480usize);
        let mut yuv = vec![128u8; w * h * 3 / 2];
        for byte in &mut yuv[w * h..] {
            *byte = 128;
        }
        let (rgba, tw, th) = preview_rgba(&yuv, 640, 480).expect("preview builds");
        assert_eq!((tw, th), (320, 240));
        assert_eq!(rgba.len(), 320 * 240 * 4);
        // Y=128 with neutral chroma: about 130 per channel.
        assert!((125..=136).contains(&rgba[0]), "gray stays gray: {}", rgba[0]);
        assert_eq!(rgba[3], 255, "opaque");
    }
}
