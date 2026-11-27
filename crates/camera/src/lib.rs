//! Camera capture over Video4Linux2, behind a safe API.
//!
//! This crate exists because the rest of the workspace forbids
//! unsafe code, and talking to a camera on Linux means ioctls
//! and mmap. The whole unsafe surface lives here, in one small
//! audited module: open a device, negotiate YUYV, queue mmap
//! buffers, and hand out borrowed frames that requeue
//! themselves on drop. Everything above this crate sees plain
//! safe Rust.
//!
//! The ioctl numbers and struct layouts are the fixed kernel
//! ABI, cross-checked against <linux/videodev2.h> (sizes:
//! capability 104, format 208, requestbuffers 20, buffer 88)
//! and pinned by the layout test at the bottom of this file.
//! The `v4l` crate is deliberately not used: its sys crate
//! runs bindgen at build time and would require libclang on
//! every build host, release builds included.
//!
//! Windows has no Video4Linux2, so it compiles a no-op camera
//! with the same public API instead: `open_first` always
//! errors, and calls stay receive-only there, exactly like a
//! machine with no camera at all. A native Windows capture
//! backend is a later round, not this one. macOS keeps the
//! V4L2 code as it compiles today (it finds no device node
//! and reports no camera).

use std::time::Duration;

#[cfg(not(target_os = "windows"))]
const VIDIOC_QUERYCAP: libc::c_ulong = 0x8068_5600;
#[cfg(not(target_os = "windows"))]
const VIDIOC_S_FMT: libc::c_ulong = 0xc0d0_5605;
#[cfg(not(target_os = "windows"))]
const VIDIOC_REQBUFS: libc::c_ulong = 0xc014_5608;
#[cfg(not(target_os = "windows"))]
const VIDIOC_QUERYBUF: libc::c_ulong = 0xc058_5609;
#[cfg(not(target_os = "windows"))]
const VIDIOC_QBUF: libc::c_ulong = 0xc058_560f;
#[cfg(not(target_os = "windows"))]
const VIDIOC_DQBUF: libc::c_ulong = 0xc058_5611;
#[cfg(not(target_os = "windows"))]
const VIDIOC_STREAMON: libc::c_ulong = 0x4004_5612;
#[cfg(not(target_os = "windows"))]
const VIDIOC_STREAMOFF: libc::c_ulong = 0x4004_5613;

#[cfg(not(target_os = "windows"))]
const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
#[cfg(not(target_os = "windows"))]
const V4L2_MEMORY_MMAP: u32 = 1;
#[cfg(not(target_os = "windows"))]
const V4L2_FIELD_NONE: u32 = 1;
#[cfg(not(target_os = "windows"))]
const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
#[cfg(not(target_os = "windows"))]
const V4L2_CAP_STREAMING: u32 = 0x0400_0000;
#[cfg(not(target_os = "windows"))]
const FOURCC_YUYV: u32 = 0x5659_5559;

/// `struct v4l2_format` (208 bytes): only the pix fields are
/// modeled; the rest of the union stays zeroed bytes.
#[cfg(not(target_os = "windows"))]
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
#[cfg(not(target_os = "windows"))]
#[repr(C)]
struct V4l2RequestBuffers {
    count: u32,
    typ: u32,
    memory: u32,
    capabilities: u32,
    reserved: u32,
}

/// `struct v4l2_buffer` (88 bytes), mmap flavor.
#[cfg(not(target_os = "windows"))]
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

/// The outcome of one [`Camera::next_frame`] wait.
pub enum FrameOutcome<'a> {
    /// A frame is ready; it requeues when dropped.
    Frame(Frame<'a>),
    /// No frame arrived within the timeout.
    Timeout,
    /// The device errored.
    Failed,
}

/// One captured frame: a borrowed view of the driver's mmap
/// buffer, valid until the frame is dropped (which hands the
/// buffer back to the driver).
pub struct Frame<'a> {
    camera: &'a mut Camera,
    index: u32,
    data: &'a [u8],
}

impl Frame<'_> {
    /// The frame's YUYV bytes (two bytes per pixel).
    #[must_use]
    pub fn data(&self) -> &[u8] {
        self.data
    }
}

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        // Best effort: a failed requeue loses one buffer from
        // the rotation, it must not panic during teardown.
        let _ = self.camera.queue(self.index);
    }
}

/// One open camera: the device fd, the negotiated frame
/// shape, and the mmap'd capture buffers.
pub struct Camera {
    #[cfg(not(target_os = "windows"))]
    fd: libc::c_int,
    width: u32,
    height: u32,
    #[cfg(not(target_os = "windows"))]
    buffers: Vec<(*mut libc::c_void, usize)>,
}

// SAFETY: the fd and buffers are only ever touched through
// &mut self (next_frame borrows exclusively, open/drop are
// exclusive), so moving the camera between threads cannot
// race. The capture thread owns it for its whole life.
unsafe impl Send for Camera {}

impl Camera {
    /// The negotiated frame width.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// The negotiated frame height.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }
}

/// The Video4Linux2 implementation (Linux and macOS, where
/// the code compiles as it does today).
#[cfg(not(target_os = "windows"))]
impl Camera {
    /// Tries /dev/video0 through /dev/video7 in order and
    /// returns the first device that answers as a streaming
    /// capture device and accepts YUYV at (or near) the asked
    /// size. The error distinguishes the ways this fails,
    /// because callers word them differently for the user.
    ///
    /// # Errors
    /// A user-readable sentence: no device node answered, or
    /// every answering device refused the format.
    pub fn open_first(width: u32, height: u32) -> Result<Self, String> {
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
            match Self::negotiate(fd, width, height) {
                Ok(camera) => return Ok(camera),
                Err(()) => {
                    // SAFETY: fd came from the open above and
                    // is not used after this.
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
    fn negotiate(fd: libc::c_int, want_w: u32, want_h: u32) -> Result<Self, ()> {
        // Capability check: capture + streaming, from the
        // device_caps when the driver reports them, else the
        // top-level capabilities (offsets 88 and 84 in the
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
            if device_caps != 0 {
                device_caps
            } else {
                read_u32(84)
            }
        };
        if caps & (V4L2_CAP_VIDEO_CAPTURE | V4L2_CAP_STREAMING)
            != (V4L2_CAP_VIDEO_CAPTURE | V4L2_CAP_STREAMING)
        {
            return Err(());
        }

        let mut format: V4l2Format = unsafe { std::mem::zeroed() };
        format.typ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        format.width = want_w;
        format.height = want_h;
        format.pixelformat = FOURCC_YUYV;
        format.field = V4L2_FIELD_NONE;
        // SAFETY: ioctl with a struct of the ABI size for
        // VIDIOC_S_FMT; the driver writes the accepted shape
        // back into it.
        if unsafe { libc::ioctl(fd, VIDIOC_S_FMT, &mut format) } < 0 {
            return Err(());
        }
        // The driver may adjust the size; it may not
        // substitute a compressed format this pipeline cannot
        // convert.
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

        let camera = Self { fd, width, height, buffers };
        for index in 0..camera.buffers.len() {
            camera.queue(index as u32)?;
        }
        let mut typ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        // SAFETY: STREAMON takes a pointer to the buffer type.
        if unsafe { libc::ioctl(fd, VIDIOC_STREAMON, &mut typ) } < 0 {
            return Err(());
        }
        Ok(camera)
    }

    /// Queues one capture buffer with the driver.
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

    /// Waits up to `timeout` for the next frame and dequeues
    /// it. The returned frame borrows the camera; dropping it
    /// requeues the buffer.
    pub fn next_frame(&mut self, timeout: Duration) -> FrameOutcome<'_> {
        let mut pollfd = libc::pollfd { fd: self.fd, events: libc::POLLIN, revents: 0 };
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: poll on the camera fd with a one-slot pollfd
        // array.
        let ready = unsafe { libc::poll(&mut pollfd, 1, millis) };
        if ready < 0 {
            return FrameOutcome::Failed;
        }
        if ready == 0 || pollfd.revents & libc::POLLIN == 0 {
            return FrameOutcome::Timeout;
        }
        let mut buffer: V4l2Buffer = unsafe { std::mem::zeroed() };
        buffer.typ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        buffer.memory = V4L2_MEMORY_MMAP;
        // SAFETY: ioctl with a struct of the ABI size for
        // VIDIOC_DQBUF, gated by the poll above.
        if unsafe { libc::ioctl(self.fd, VIDIOC_DQBUF, &mut buffer) } < 0 {
            return FrameOutcome::Failed;
        }
        let index = buffer.index;
        let Some(&(map, map_len)) = self.buffers.get(index as usize) else {
            return FrameOutcome::Failed;
        };
        // Some drivers leave bytesused at zero for capture;
        // the mapped length is the frame's size then.
        let used =
            if buffer.bytesused == 0 { map_len } else { (buffer.bytesused as usize).min(map_len) };
        // SAFETY: the driver filled `used` bytes of the
        // mmap'd buffer; the slice lives exactly as long as
        // the Frame that requeues the buffer on drop, and the
        // &mut borrow of the camera prevents a second dequeue
        // (or any other access) in between.
        let data = unsafe { std::slice::from_raw_parts(map.cast::<u8>(), used) };
        FrameOutcome::Frame(Frame { camera: self, index, data })
    }
}

/// The no-op Windows implementation: same public API, no
/// capture. `open_first` always errors, so the caller treats
/// the machine as having no camera and the call stays
/// receive-only.
#[cfg(target_os = "windows")]
impl Camera {
    /// Camera capture is not available on Windows yet; a
    /// native capture backend is a later round.
    ///
    /// # Errors
    /// Always: this platform has no capture backend.
    pub fn open_first(_width: u32, _height: u32) -> Result<Self, String> {
        Err("Camera capture is not available on Windows yet.".to_owned())
    }

    /// There is no capture buffer to requeue; the no-op
    /// camera never produces a frame.
    fn queue(&self, _index: u32) -> Result<(), ()> {
        Ok(())
    }

    /// Never reached in practice (`open_first` always
    /// errors), but must stay valid: no frame ever arrives.
    pub fn next_frame(&mut self, _timeout: Duration) -> FrameOutcome<'_> {
        FrameOutcome::Timeout
    }
}

#[cfg(not(target_os = "windows"))]
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

#[cfg(all(test, not(target_os = "windows")))]
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
}
