//! H.264 video decoding (Cisco openh264) and YUV420 → RGBA conversion.

use openh264::formats::YUVSource;

use crate::isobmff::TrackConfig;

/// H.264 decoder bound to a track's `avcC` configuration.
pub struct VideoDecoder {
    decoder: openh264::decoder::Decoder,
    /// Pre-built Annex-B SPS/PPS blob fed before the first slice.
    parameter_sets: Vec<u8>,
    parameter_sets_sent: bool,
    /// Annex-B conversion scratch buffer.
    annexb: Vec<u8>,
    /// Annex-B mode: input samples are already start-code NALUs with
    /// in-band parameter sets (MPEG-TS sources).
    annexb_mode: bool,
}

impl VideoDecoder {
    /// Creates a decoder for the given track config (must be `Codec::Avc`).
    pub fn new(config: &TrackConfig) -> Result<VideoDecoder, String> {
        let decoder =
            openh264::decoder::Decoder::new().map_err(|e| format!("openh264 init failed: {e}"))?;
        let parameter_sets = avcc_to_annexb(&config.extra).unwrap_or_default();
        Ok(VideoDecoder {
            decoder,
            parameter_sets,
            parameter_sets_sent: false,
            annexb: Vec::with_capacity(256 * 1024),
            annexb_mode: false,
        })
    }

    /// Creates a decoder for Annex-B byte streams (MPEG-TS), where
    /// parameter sets travel in-band with the samples.
    pub fn new_annexb() -> Result<VideoDecoder, String> {
        let decoder =
            openh264::decoder::Decoder::new().map_err(|e| format!("openh264 init failed: {e}"))?;
        Ok(VideoDecoder {
            decoder,
            parameter_sets: Vec::new(),
            parameter_sets_sent: true,
            annexb: Vec::with_capacity(256 * 1024),
            annexb_mode: true,
        })
    }

    /// Decodes one H.264 sample (avcC length-prefixed NALUs) into straight
    /// RGBA8. Returns `(width, height, rgba)`.
    pub fn decode(&mut self, sample: &[u8]) -> Option<(u32, u32, std::sync::Arc<Vec<u8>>)> {
        self.annexb.clear();
        if self.annexb_mode {
            self.annexb.extend_from_slice(sample);
        } else {
            if !self.parameter_sets.is_empty() && !self.parameter_sets_sent {
                self.annexb.extend_from_slice(&self.parameter_sets);
                self.parameter_sets_sent = true;
            }
            sample_to_annexb(&self.parameter_sets, sample, &mut self.annexb);
        }
        if self.annexb.is_empty() {
            return None;
        }
        let yuv = match self.decoder.decode(&self.annexb) {
            Ok(Some(yuv)) => yuv,
            Ok(None) => return None,
            Err(err) => {
                // A corrupted NAL must not kill playback; skip the sample.
                tracing_debug_decode_error(&err);
                return None;
            }
        };
        let (w, h) = yuv.dimensions();
        if w == 0 || h == 0 {
            return None;
        }
        let (y_stride, u_stride, v_stride) = yuv.strides();
        let mut rgba = Vec::with_capacity(w * h * 4);
        yuv420_to_rgba(
            yuv.y(),
            yuv.u(),
            yuv.v(),
            y_stride,
            u_stride,
            v_stride,
            w,
            h,
            &mut rgba,
        );
        Some((w as u32, h as u32, std::sync::Arc::new(rgba)))
    }
}

fn tracing_debug_decode_error(err: &openh264::Error) {
    // Bounded, allocation-free diagnostics: decoder errors on damaged
    // streams are expected and recoverable.
    let _ = err;
}

/// Converts an avcC record into an Annex-B parameter-set blob
/// (`00 00 00 01 SPS 00 00 00 01 PPS...`).
fn avcc_to_annexb(avcc: &[u8]) -> Option<Vec<u8>> {
    if avcc.len() < 7 || avcc[0] != 1 {
        return None;
    }
    let num_sps = (avcc[5] & 0x1F) as usize;
    let mut out = Vec::with_capacity(64);
    let mut pos = 6;
    for _ in 0..num_sps {
        if pos + 2 > avcc.len() {
            break;
        }
        let len = u16::from_be_bytes([avcc[pos], avcc[pos + 1]]) as usize;
        pos += 2;
        let end = pos.checked_add(len)?;
        if end > avcc.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[pos..end]);
        pos = end;
    }
    let num_pps = *avcc.get(pos)? as usize;
    pos += 1;
    for _ in 0..num_pps {
        if pos + 2 > avcc.len() {
            break;
        }
        let len = u16::from_be_bytes([avcc[pos], avcc[pos + 1]]) as usize;
        pos += 2;
        let end = pos.checked_add(len)?;
        if end > avcc.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[pos..end]);
        pos = end;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Appends one avcC-format sample as Annex-B (start-code delimited NALUs).
fn sample_to_annexb(avcc: &[u8], sample: &[u8], out: &mut Vec<u8>) {
    let length_size = if avcc.len() >= 5 {
        ((avcc[4] & 0x03) + 1) as usize
    } else {
        4
    };
    let start_code: &[u8] = &[0, 0, 0, 1];
    let mut pos = 0usize;
    while pos + length_size <= sample.len() {
        let nalu_len = match length_size {
            1 => u64::from(sample[pos]) as usize,
            2 => u16::from_be_bytes([sample[pos], sample[pos + 1]]) as usize,
            3 => {
                ((u32::from(sample[pos]) << 16)
                    | (u32::from(sample[pos + 1]) << 8)
                    | u32::from(sample[pos + 2])) as usize
            }
            _ => u32::from_be_bytes([
                sample[pos],
                sample[pos + 1],
                sample[pos + 2],
                sample[pos + 3],
            ]) as usize,
        };
        pos += length_size;
        if nalu_len == 0 || pos + nalu_len > sample.len() {
            break;
        }
        out.extend_from_slice(start_code);
        out.extend_from_slice(&sample[pos..pos + nalu_len]);
        pos += nalu_len;
    }
}

/// ITU-R BT.601 (limited range) YUV420 planar → straight RGBA8. Strides are
/// honored: openh264 pads rows (e.g. 16-byte alignment), so luma/chroma row
/// starts come from the decoder-provided strides, not width/2.
#[allow(clippy::too_many_arguments)]
fn yuv420_to_rgba(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    y_stride: usize,
    u_stride: usize,
    v_stride: usize,
    width: usize,
    height: usize,
    out: &mut Vec<u8>,
) {
    out.reserve(width * height * 4);
    for row in 0..height {
        let y_off = row * y_stride;
        let uv_row = row / 2;
        let u_off = uv_row * u_stride;
        let v_off = uv_row * v_stride;
        for col in 0..width {
            let y = i32::from(y.get(y_off + col).copied().unwrap_or(16));
            let cb = i32::from(u.get(u_off + col / 2).copied().unwrap_or(128));
            let cr = i32::from(v.get(v_off + col / 2).copied().unwrap_or(128));
            let r = (1_164 * (y - 16) + 1_596 * (cr - 128) + 512) >> 10;
            let g = (1_164 * (y - 16) - 391 * (cb - 128) - 813 * (cr - 128) + 512) >> 10;
            let b = (1_164 * (y - 16) + 2_018 * (cb - 128) + 512) >> 10;
            out.push(r.clamp(0, 255) as u8);
            out.push(g.clamp(0, 255) as u8);
            out.push(b.clamp(0, 255) as u8);
            out.push(255);
        }
    }
}
