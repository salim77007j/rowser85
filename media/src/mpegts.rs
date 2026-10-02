//! An MPEG-TS demuxer for HLS TS segments and legacy broadcast streams.
//!
//! Feeds the same pipeline lanes as the ISOBMFF demuxer: PAT/PMT discovery,
//! PES assembly with 90 kHz timestamps, and per-access-unit samples —
//! Annex-B H.264 video plus ADTS AAC audio (the two codecs the decoders
//! support). Video parameter sets travel in-band (Annex-B start codes), so
//! no avcC record is needed; the audio AudioSpecificConfig is synthesized
//! from the first ADTS frame header.

use std::collections::HashMap;

use crate::isobmff::{Codec, DemuxError, Sample, StreamInfo, TrackConfig};

/// TS packet size in bytes.
const PACKET: usize = 188;
/// Sync byte at the head of every packet.
const SYNC: u8 = 0x47;
/// Program association table pid.
const PAT_PID: u16 = 0;
/// Stream type: AVC video (ITU-T H.264).
const STREAM_TYPE_AVC: u8 = 0x1B;
/// Stream type: AAC audio (ADTS framing).
const STREAM_TYPE_AAC_ADTS: u8 = 0x0F;
/// Parses the PES prefix: returns `(pts, dts, payload_offset)`.
const AAC_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// Heuristic: do these bytes look like MPEG-TS (not ISOBMFF)?
/// `0x47` is also ASCII `G`, so a long buffer must show the 188-byte
/// alignment to count.
pub fn looks_like_ts(bytes: &[u8]) -> bool {
    if bytes.first() != Some(&SYNC) {
        return false;
    }
    if bytes.len() >= PACKET * 2 {
        return bytes[PACKET] == SYNC && bytes[PACKET * 2] == SYNC;
    }
    bytes.len() >= PACKET && bytes.len() % PACKET == 0
}

/// Streaming MPEG-TS demuxer.
#[derive(Debug, Default)]
pub struct TsDemuxer {
    /// Bytes not yet aligned into whole packets.
    carry: Vec<u8>,
    /// PMT pid for the first program (set after PAT).
    pmt_pid: Option<u16>,
    pmt_parsed: bool,
    video_pid: Option<u16>,
    audio_pid: Option<u16>,
    /// PES assembly buffers per elementary pid.
    pes: HashMap<u16, PesBuf>,
    /// Per-pid first presentation time: every pid is normalized so
    /// playback starts at t=0 regardless of the muxer's start offset
       /// (broadcast streams and ffmpeg TS start ~1.4 s in).
    starts: HashMap<u16, f64>,
    video_samples: Vec<Sample>,
    audio_samples: Vec<Sample>,
    video_config: Option<TrackConfig>,
    audio_config: Option<TrackConfig>,
    end_pts: f64,
}

#[derive(Debug, Default)]
struct PesBuf {
    data: Vec<u8>,
    started: bool,
    /// Random-access indicator seen in any TS packet of this PES.
    keyframe_hint: bool,
    /// Total PES length when the header advertised one (non-zero field).
    expected: Option<usize>,
}

impl TsDemuxer {
    pub fn new() -> TsDemuxer {
        TsDemuxer::default()
    }

    /// Pushes raw segment bytes (whole 188-byte packets, plus any
    /// alignment residue).
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), DemuxError> {
        self.carry.extend_from_slice(bytes);
        // Process every complete packet; resync on byte loss.
        loop {
            if self.carry.len() < PACKET {
                break;
            }
            if self.carry[0] != SYNC {
                // Resync: drop until the next sync byte.
                if let Some(pos) = self.carry.iter().position(|&b| b == SYNC) {
                    self.carry.drain(0..pos);
                } else {
                    self.carry.clear();
                }
                if self.carry.len() < PACKET {
                    break;
                }
            }
            let mut packet: [u8; PACKET] = [0; PACKET];
            packet.copy_from_slice(&self.carry[0..PACKET]);
            self.carry.drain(0..PACKET);
            self.handle_packet(&packet);
        }
        Ok(())
    }

    /// Signals end of stream: flushes every in-progress PES.
    pub fn finish(&mut self) {
        let pids: Vec<u16> = self.pes.keys().copied().collect();
        for pid in pids {
            if let Some(buf) = self.pes.remove(&pid) {
                self.finalize_pes(pid, buf);
            }
        }
    }

    /// Stream info (configs discovered from PMT + first ADTS frame).
    pub fn info(&self) -> Option<StreamInfo> {
        if self.video_config.is_none() && self.audio_config.is_none() {
            return None;
        }
        Some(StreamInfo {
            duration: self.end_pts,
            video: self.video_config.clone(),
            audio: self.audio_config.clone(),
        })
    }

    /// Highest presentation time seen, in seconds.
    pub fn end_pts(&self) -> f64 {
        self.end_pts
    }

    /// Drains decoded video samples (Annex-B access units).
    pub fn take_video_samples(&mut self, out: &mut Vec<Sample>) {
        out.append(&mut self.video_samples);
    }

    /// Drains audio samples (ADTS frames).
    pub fn take_audio_samples(&mut self, out: &mut Vec<Sample>) {
        out.append(&mut self.audio_samples);
    }

    /// Unconsumed byte estimate (diagnostics).
    pub fn buffered_bytes(&self) -> usize {
        self.carry.len() + self.pes.values().map(|b| b.data.len()).sum::<usize>()
    }

    // ---------------------------------------------------------------- internals

    fn handle_packet(&mut self, packet: &[u8; PACKET]) {
        let flags = u16::from_be_bytes([packet[1], packet[2]]);
        let pusi = flags & 0x4000 != 0;
        let pid = flags & 0x1FFF;
        let adaptation = (packet[3] >> 4) & 0x03;
        let mut rai = false;
        let mut payload: &[u8] = &packet[4..];
        if adaptation & 0b10 != 0 {
            // adaptation_field_length + flags (random_access_indicator).
            let af_len = payload[0] as usize;
            if payload.len() > 1 {
                rai = payload[1] & 0x40 != 0;
            }
            let skip = 1 + af_len.min(payload.len().saturating_sub(1));
            payload = &payload[skip.min(payload.len())..];
        } else if adaptation & 0b01 == 0 {
            return; // no payload
        }
        if payload.is_empty() {
            return;
        }
        if pid == PAT_PID {
            if pusi {
                self.parse_pat(payload);
            }
            return;
        }
        if Some(pid) == self.pmt_pid {
            if pusi && !self.pmt_parsed {
                self.parse_pmt(payload);
            }
            return;
        }
        let is_video = Some(pid) == self.video_pid;
        let is_audio = Some(pid) == self.audio_pid;
        if !is_video && !is_audio {
            return;
        }
        if pusi {
            if let Some(buf) = self.pes.remove(&pid) {
                self.finalize_pes(pid, buf);
            }
            let mut buf = PesBuf {
                started: true,
                keyframe_hint: rai,
                ..PesBuf::default()
            };
            buf.data.extend_from_slice(payload);
            buf.expected = pes_expected_len(&buf.data);
            self.pes.insert(pid, buf);
        } else if let Some(buf) = self.pes.get_mut(&pid) {
            buf.data.extend_from_slice(payload);
            if !buf.keyframe_hint && rai {
                buf.keyframe_hint = true;
            }
        }
    }

    fn parse_pat(&mut self, payload: &[u8]) {
        let table = skip_pointer(payload);
        let Some(table) = table else { return };
        if std::env::var("ROWSER_TS_TRACE").is_ok() {
            eprintln!("[ts] PAT table len={} head={:02X?}", table.len(), &table[..8.min(table.len())]);
        }
        if table.len() < 12 || table[0] != 0x00 {
            return;
        }
        let section_length = (u16::from_be_bytes([table[1], table[2]]) & 0x0FFF) as usize;
        // Entries: program_number(2) + pid(2), starting at byte 8, ending
        // at section_length - 1 (prefix 3 + length − CRC 4).
        let body_end = section_length.saturating_sub(1).min(table.len());
        // Take the first non-NIT program.
        let mut pos = 8;
        while pos + 4 <= body_end {
            let program = u16::from_be_bytes([table[pos], table[pos + 1]]);
            let entry_pid = u16::from_be_bytes([table[pos + 2], table[pos + 3]]) & 0x1FFF;
            if program != 0 {
                self.pmt_pid = Some(entry_pid);
                return;
            }
            pos += 4;
        }
    }

    fn parse_pmt(&mut self, payload: &[u8]) {
        let table = skip_pointer(payload);
        let Some(table) = table else { return };
        if std::env::var("ROWSER_TS_TRACE").is_ok() {
            eprintln!("[ts] PMT table len={} head={:02X?}", table.len(), &table[..16.min(table.len())]);
        }
        if table.len() < 12 || table[0] != 0x02 {
            return;
        }
        let section_length = (u16::from_be_bytes([table[1], table[2]]) & 0x0FFF) as usize;
        // Elementary streams end at section_length - 1 (same arithmetic
        // as the PAT: 3-byte prefix + length − 4-byte CRC).
        let body_end = section_length.saturating_sub(1).min(table.len());
        let program_info_len = u16::from_be_bytes([table[10], table[11]]) as usize & 0x0FFF;
        let mut pos = 12 + program_info_len;
        while pos + 5 <= body_end {
            let stream_type = table[pos];
            let elementary_pid =
                u16::from_be_bytes([table[pos + 1], table[pos + 2]]) & 0x1FFF;
            let es_info_len = u16::from_be_bytes([table[pos + 3], table[pos + 4]]) as usize & 0x0FFF;
            pos += 5 + es_info_len;
            if std::env::var("ROWSER_TS_TRACE").is_ok() {
                eprintln!("[ts] PMT entry type={stream_type:#04X} pid={elementary_pid:#06X}");
            }
            match stream_type {
                STREAM_TYPE_AVC if self.video_pid.is_none() => {
                    self.video_pid = Some(elementary_pid);
                    self.video_config = Some(TrackConfig {
                        track_id: u32::from(elementary_pid),
                        codec: Codec::Avc,
                        // Annex-B: parameter sets arrive in-band.
                        extra: Vec::new(),
                        width: 0,
                        height: 0,
                        sample_rate: 0,
                        channels: 0,
                    });
                }
                STREAM_TYPE_AAC_ADTS if self.audio_pid.is_none() => {
                    // Config (AudioSpecificConfig) is completed from the
                    // first ADTS header in `finalize_pes`.
                    self.audio_pid = Some(elementary_pid);
                }
                _ => {}
            }
        }
        self.pmt_parsed = true;
    }

    fn finalize_pes(&mut self, pid: u16, buf: PesBuf) {
        if !buf.started || buf.data.len() < 6 {
            return;
        }
        let Some((raw_pts, _dts, offset)) = parse_pes_header(&buf.data) else {
            return;
        };
        // Normalize to this pid's first presentation time.
        let base = *self.starts.entry(pid).or_insert(raw_pts);
        let pts = (raw_pts - base).max(0.0);
        let payload = &buf.data[offset.min(buf.data.len())..];
        if payload.is_empty() {
            return;
        }
        if Some(pid) == self.video_pid {
            let keyframe = buf.keyframe_hint || annexb_starts_with_parameter_set(payload);
            self.end_pts = self.end_pts.max(pts);
            self.video_samples.push(Sample {
                pts,
                dts: _dts - base,
                keyframe,
                data: payload.to_vec(),
            });
            return;
        }
        if Some(pid) == self.audio_pid {
            self.split_adts(pid, pts, payload);
        }
    }

    /// Splits a PES payload into ADTS frames; synthesizes the track config
    /// (AudioSpecificConfig) from the first header.
    fn split_adts(&mut self, pid: u16, pts: f64, payload: &[u8]) {
        let mut pos = 0usize;
        let mut frame_index = 0u64;
        while pos + 7 <= payload.len() {
            if payload[pos] != 0xFF || (payload[pos + 1] & 0xF0) != 0xF0 {
                pos += 1; // resync scan
                continue;
            }
            let frame_len = (((payload[pos + 3] & 0x03) as usize) << 11)
                | ((payload[pos + 4] as usize) << 3)
                | ((payload[pos + 5] >> 5) & 0x07) as usize;
            if frame_len == 0 || pos + frame_len > payload.len() {
                break;
            }
            let frame = &payload[pos..pos + frame_len];
            if self.audio_config.is_none() {
                if let Some(config) = adts_track_config(pid, frame) {
                    self.audio_config = Some(config);
                }
            }
            let rate = self
                .audio_config
                .as_ref()
                .map(|c| c.sample_rate.max(1))
                .unwrap_or(48_000) as f64;
            let frame_pts = pts + frame_index as f64 * 1024.0 / rate;
            self.end_pts = self.end_pts.max(frame_pts);
            self.audio_samples.push(Sample {
                pts: frame_pts,
                dts: frame_pts,
                keyframe: true,
                data: frame.to_vec(),
            });
            pos += frame_len;
            frame_index += 1;
        }
    }
}

/// True when the first Annex-B NALU is an SPS/PPS/IDR (keyframe evidence).
fn annexb_starts_with_parameter_set(payload: &[u8]) -> bool {
    let header = if payload.len() >= 4 && payload[0..3] == [0, 0, 1] {
        3
    } else if payload.len() >= 5 && payload[0..4] == [0, 0, 0, 1] {
        4
    } else {
        return false;
    };
    let nal_type = payload.get(header).map(|b| b & 0x1F).unwrap_or(0);
    matches!(nal_type, 5 | 7)
}

/// Parses the PES prefix: returns `(pts, dts, payload_offset)`.
/// Layout: start code(3) + stream_id(1) + length(2) + flags1(1) + flags2(1)
/// + header_data_len(1) + optional fields + payload.
fn parse_pes_header(data: &[u8]) -> Option<(f64, f64, usize)> {
    if data.len() < 10 {
        return None;
    }
    if data[0] != 0x00 || data[1] != 0x00 || data[2] != 0x01 {
        return None;
    }
    let flags = data[7];
    let header_data_len = data[8] as usize;
    let mut pts = None;
    let mut dts = None;
    let has_pts = flags & 0x80 != 0;
    let has_dts = flags & 0x40 != 0;
    if has_pts && data.len() >= 14 {
        pts = read_pts90(&data[9..14]);
        if has_dts && data.len() >= 19 {
            dts = read_pts90(&data[14..19]);
        }
    }
    let offset = 9 + header_data_len;
    if offset > data.len() {
        return None;
    }
    let pts = pts?;
    Some((pts, dts.unwrap_or(pts), offset))
}

/// 5-byte 33-bit PTS at 90 kHz → seconds.
fn read_pts90(b: &[u8]) -> Option<f64> {
    if b.len() < 5 {
        return None;
    }
    let pts = ((u64::from(b[0] >> 1) & 0x07) << 30)
        | (u64::from(b[1]) << 22)
        | ((u64::from(b[2] >> 1) & 0x7F) << 15)
        | (u64::from(b[3]) << 7)
        | (u64::from(b[4] >> 1) & 0x7F);
    Some(pts as f64 / 90_000.0)
}

/// Total PES length when the header advertises one (0 = unbounded video).
fn pes_expected_len(data: &[u8]) -> Option<usize> {
    if data.len() < 6 || data[0..3] != [0, 0, 1] {
        return None;
    }
    let len = u16::from_be_bytes([data[4], data[5]]) as usize;
    if len == 0 {
        None
    } else {
        Some(6 + len)
    }
}

/// TS tables start with a pointer field on payload-unit-start packets.
fn skip_pointer(payload: &[u8]) -> Option<&[u8]> {
    let pointer = *payload.first()? as usize;
    let rest = payload.get(1 + pointer..)?;
    Some(rest)
}

/// Builds a TrackConfig (with AudioSpecificConfig) from an ADTS header.
fn adts_track_config(pid: u16, frame: &[u8]) -> Option<TrackConfig> {
    if frame.len() < 7 {
        return None;
    }
    let profile = ((frame[2] >> 6) & 0x03) as u32; // 01 = LC
    let freq_index = ((frame[2] >> 2) & 0x0F) as usize;
    let channels = (((frame[2] & 0x01) as u32) << 2) | ((frame[3] as u32 >> 6) & 0x03);
    let rate = *AAC_RATES.get(freq_index).unwrap_or(&44_100);
    // AudioSpecificConfig: AOT(5) | freqIdx(4) | chanCfg(4) | zeros.
    let aot = profile + 1; // ADTS profile field is AOT - 1.
    let asc0 = ((aot as u8) << 3) | ((freq_index as u8) >> 1);
    let asc1 = (((freq_index as u8) & 0x01) << 7) | ((channels as u8) << 3);
    Some(TrackConfig {
        track_id: u32::from(pid),
        codec: Codec::AacLc,
        extra: vec![asc0, asc1],
        width: 0,
        height: 0,
        sample_rate: rate,
        channels: channels.clamp(1, 6) as u16,
    })
}
