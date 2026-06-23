use anyhow::{Context, Result};
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, SpsPpsStrategy, UsageType,
};
use openh264::formats::{YUVBuffer, YUVSource};
use openh264::OpenH264API;

use crate::convert::bgra_to_i420;

pub struct VideoEncoder {
    enc: Encoder,
}

impl VideoEncoder {
    pub fn new(fps: u32, bitrate_mbps: u32) -> Result<Self> {
        let api = OpenH264API::from_source();
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate_mbps * 1_000_000))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            // Tune for screen content (sharp edges, text) rather than camera video
            .usage_type(UsageType::ScreenContentRealTime)
            // Repeat SPS/PPS so a viewer can recover after packet loss
            .sps_pps_strategy(SpsPpsStrategy::IncreasingId)
            // Emit a keyframe at least once per second. Over UDP, a single lost
            // packet corrupts the dependent P-frames until the next keyframe —
            // without this the viewer freezes permanently on the first drop.
            .intra_frame_period(IntraFramePeriod::from_num_frames(fps.max(1)));
        Ok(Self {
            enc: Encoder::with_api_config(api, config).context("create H.264 encoder")?,
        })
    }

    /// Force the next encoded frame to be a keyframe (used to satisfy a viewer's
    /// recovery request after it detects corruption).
    pub fn force_keyframe(&mut self) {
        self.enc.force_intra_frame();
    }

    /// Encode a BGRA frame; returns raw H.264 bytes (all NAL units concatenated)
    pub fn encode_bgra(&mut self, bgra: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
        let i420 = bgra_to_i420(bgra, width, height);
        let yuv = YUVBuffer::from_vec(i420, width, height);
        let stream = self.enc.encode(&yuv).context("H.264 encode")?;

        // Collect NAL units from all layers into a flat byte vec
        let mut out = Vec::new();
        for i in 0..stream.num_layers() {
            if let Some(layer) = stream.layer(i) {
                for j in 0..layer.nal_count() {
                    if let Some(nal) = layer.nal_unit(j) {
                        out.extend_from_slice(nal);
                    }
                }
            }
        }
        Ok(out)
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
    /// Returns (RGBA bytes, width, height) when a frame is ready; None while buffering.
    pub fn decode(&mut self, nal: &[u8]) -> Result<Option<(Vec<u8>, u32, u32)>> {
        let maybe = self.dec.decode(nal).context("H.264 decode")?;
        let Some(yuv) = maybe else { return Ok(None) };

        let (w, h) = yuv.dimensions();
        let mut rgba = vec![0u8; w * h * 4];
        yuv.write_rgba8(&mut rgba);

        Ok(Some((rgba, w as u32, h as u32)))
    }
}
