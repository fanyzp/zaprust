//! One-off generator for the fake backend's call-video fixture
//! (crates/backend/testdata/call-video-fixture.h264). Deleted
//! after the fixture was generated; the fixture itself is the
//! checked-in artifact. The stream is a real OpenH264 encode of
//! a synthetic test pattern (gradient, moving bar, border), so
//! the app's real decoder is what QA captures exercise.
//!
//! Record format: repeated [u32 LE length][u8 keyframe][AU
//! bytes], one record per access unit.

use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    UsageType,
};
use openh264::formats::YUVBuffer;
use openh264::OpenH264API;

const W: usize = 320;
const H: usize = 240;
const FRAMES: usize = 45;

fn frame_yuv(frame: usize) -> Vec<u8> {
    let mut yuv = vec![0u8; W * H * 3 / 2];
    let (luma, chroma) = yuv.split_at_mut(W * H);
    let (u_plane, v_plane) = chroma.split_at_mut(W * H / 4);
    let bar_x = (frame * 7) % W;
    for y in 0..H {
        for x in 0..W {
            let border = x < 4 || y < 4 || x >= W - 4 || y >= H - 4;
            let in_bar = x.abs_diff(bar_x) < 10;
            luma[y * W + x] = if border {
                235
            } else if in_bar {
                200
            } else {
                ((x * 255) / W) as u8
            };
        }
    }
    for i in 0..u_plane.len() {
        u_plane[i] = 90;
        v_plane[i] = 160;
    }
    yuv
}

fn main() {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(400_000))
        .max_frame_rate(FrameRate::from_hz(15.0))
        .usage_type(UsageType::CameraVideoRealTime)
        .profile(Profile::Baseline)
        .intra_frame_period(IntraFramePeriod::from_num_frames(45))
        .enable_skip_frame(false);
    let mut encoder =
        Encoder::with_api_config(OpenH264API::from_source(), config).expect("encoder starts");
    let mut out: Vec<u8> = Vec::new();
    let mut aus = 0usize;
    let mut keyframes = 0usize;
    for frame in 0..FRAMES {
        let source = YUVBuffer::from_vec(frame_yuv(frame), W, H);
        let stream = encoder.encode(&source).expect("frame encodes");
        let keyframe = matches!(stream.frame_type(), FrameType::IDR | FrameType::I);
        let mut au = Vec::new();
        stream.write_vec(&mut au);
        if au.is_empty() {
            continue;
        }
        out.extend_from_slice(&(au.len() as u32).to_le_bytes());
        out.push(u8::from(keyframe));
        out.extend_from_slice(&au);
        aus += 1;
        keyframes += usize::from(keyframe);
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata/call-video-fixture.h264");
    std::fs::create_dir_all(path.parent().expect("testdata dir")).expect("mkdir");
    std::fs::write(&path, &out).expect("fixture writes");
    println!("wrote {} bytes, {aus} AUs, {keyframes} keyframes to {}", out.len(), path.display());
}
