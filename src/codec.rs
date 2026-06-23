use anyhow::{Context, Result};
use openh264::decoder::Decoder;
use openh264::encoder::{Encoder, EncoderConfig};
use openh264::formats::YUVSource;

use crate::convert::{bgra_to_i420, i420_strided_to_xrgb};

// YUVSource backed by a packed I420 Vec (Y plane | U plane | V plane, no stride padding)
// Holds a single allocation; planes are slices into it.
struct I420Frame {
    data: Vec<u8>,
    width: usize,
    height: usize,
}

impl I420Frame {
    fn new(data: Vec<u8>, width: usize, height: usize) -> Self {
        Self { data, width, height }
    }

    fn y_end(&self) -> usize { self.width * self.height }
    fn u_end(&self) -> usize { self.y_end() + self.width * self.height / 4 }
}

impl YUVSource for I420Frame {
    fn width(&self) -> i32  { self.width as i32 }
    fn height(&self) -> i32 { self.height as i32 }
    fn y(&self) -> &[u8]    { &self.data[..self.y_end()] }
    fn u(&self) -> &[u8]    { &self.data[self.y_end()..self.u_end()] }
    fn v(&self) -> &[u8]    { &self.data[self.u_end()..] }
    fn y_stride(&self) -> i32 { self.width as i32 }
    fn u_stride(&self) -> i32 { (self.width / 2) as i32 }
    fn v_stride(&self) -> i32 { (self.width / 2) as i32 }
}

pub struct VideoEncoder {
    enc: Encoder,
    width: usize,
    height: usize,
}

impl VideoEncoder {
    pub fn new(width: usize, height: usize, fps: u32, bitrate_mbps: u32) -> Result<Self> {
        // NOTE: EncoderConfig API varies by openh264 version.
        // If compilation fails here, try: EncoderConfig::new(width as u32, height as u32)
        // or remove the .debug() call if that method doesn't exist.
        let config = EncoderConfig::new()
            .set_bitrate_bps(bitrate_mbps * 1_000_000)
            .set_max_frame_rate(fps as f32);
        Ok(Self {
            enc: Encoder::with_config(config).context("create H.264 encoder")?,
            width,
            height,
        })
    }

    /// Encode a BGRA frame; returns H.264 NAL bytes (may be empty for non-producing frames)
    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
        let i420 = bgra_to_i420(bgra, self.width, self.height);
        let frame = I420Frame::new(i420, self.width, self.height);
        let bs = self.enc.encode(&frame).context("H.264 encode")?;
        Ok(bs.to_vec())
    }
}

pub struct VideoDecoder {
    dec: Decoder,
}

impl VideoDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            dec: Decoder::new().context("create H.264 decoder")?,
        })
    }

    /// Decode H.264 NAL bytes.
    /// Returns (XRGB u32 pixels, width, height) when a complete frame is ready;
    /// None while the decoder buffers the first frames.
    ///
    /// NOTE: if DecodedYUV method names differ in your openh264 version, check:
    ///   dimension_rgb() → width/height pair
    ///   strides_yuv()   → (y_stride, u_stride, v_stride)
    pub fn decode(&mut self, nal: &[u8]) -> Result<Option<(Vec<u32>, u32, u32)>> {
        let maybe = self.dec.decode(nal).context("H.264 decode")?;
        let Some(yuv) = maybe else { return Ok(None) };

        let (width, height) = yuv.dimension_rgb();
        let (ys, us, vs) = yuv.strides_yuv();

        let pixels = i420_strided_to_xrgb(yuv.y(), ys, yuv.u(), us, yuv.v(), vs, width, height);
        Ok(Some((pixels, width as u32, height as u32)))
    }
}
