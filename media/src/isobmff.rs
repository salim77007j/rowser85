//! A sequential ISOBMFF (MP4 / fragmented MP4) demuxer.
//!
//! Designed for byte-stream sources: the engine pushes bytes as they arrive
//! (ranged HTTP chunks, MSE `appendBuffer` segments, HLS media segments) and
//! the demuxer extracts codec configuration plus timestamped samples without
//! ever seeking. Consumed bytes are dropped, which keeps resident memory
//! bounded for live streams and long files.
//!
//! Supported:
//! * progressive MP4 (moov anywhere in the stream; non-faststart files with
//!   a huge leading mdat are rejected with an explicit error),
//! * fragmented MP4 (fMP4): the MSE and fMP4-HLS profile,
//! * H.264 (avcC) video, AAC-LC (esds → AudioSpecificConfig) audio.

use std::collections::VecDeque;
use std::ops::Range;

/// Maximum buffered-but-unconsumed bytes before the demuxer gives up
/// (non-faststart progressive file guard).
const BUFFER_CAP: usize = 48 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Codec {
    /// H.264/AVC with an avcC decoder configuration record.
    Avc,
    /// AAC-LC with an AudioSpecificConfig.
    AacLc,
    /// Recognized but not decodable in this build.
    Unsupported(&'static str),
}

#[derive(Debug, Clone)]
pub struct TrackConfig {
    /// MP4 track id.
    pub track_id: u32,
    /// Codec + decoder configuration bytes (avcC or AudioSpecificConfig).
    pub codec: Codec,
    /// Codec configuration record (decoder init data).
    pub extra: Vec<u8>,
    /// Video width (fixed-point 16.16 in tkhd/stsd resolved to pixels).
    pub width: u32,
    /// Video height.
    pub height: u32,
    /// Audio sample rate in Hz (0 for video tracks).
    pub sample_rate: u32,
    /// Audio channel count (0 for video tracks).
    pub channels: u16,
}

impl TrackConfig {
    /// True when this track can be decoded by the pipeline.
    pub fn decodable(&self) -> bool {
        matches!(self.codec, Codec::Avc | Codec::AacLc)
    }
}

#[derive(Debug, Clone, Default)]
pub struct StreamInfo {
    /// Duration in seconds (`f64::INFINITY`/0 for unknown or live).
    pub duration: f64,
    /// Video track config, when present.
    pub video: Option<TrackConfig>,
    /// Audio track config, when present.
    pub audio: Option<TrackConfig>,
}

impl StreamInfo {
    /// True when at least one track is decodable.
    pub fn decodable(&self) -> bool {
        self.video.as_ref().map(|t| t.decodable()).unwrap_or(false)
            || self.audio.as_ref().map(|t| t.decodable()).unwrap_or(false)
    }
}

/// One extracted, fully-buffered sample (bytes copied out of the stream).
#[derive(Debug, Clone)]
pub struct Sample {
    /// Presentation timestamp in seconds.
    pub pts: f64,
    /// Decode timestamp in seconds.
    pub dts: f64,
    /// True for a sync sample (IDR keyframe for H.264).
    pub keyframe: bool,
    /// Encoded sample bytes.
    pub data: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum DemuxError {
    /// The stream is not recognizable ISOBMFF data.
    #[error("not an MP4/ISOBMFF stream")]
    NotIsobmff,
    /// No moov at stream head and the buffer cap was hit (non-faststart).
    #[error("moov missing from stream head (non-faststart MP4 unsupported)")]
    MissingMoov,
    /// Structure violation.
    #[error("malformed ISOBMFF: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum TrackKind {
    Video,
    Audio,
}

#[derive(Debug, Clone, Copy)]
struct TableSample {
    offset: u64,
    size: u32,
    dts: u64,
    duration: u32,
    cts: i64,
    sync: bool,
}

/// Per-track parse state.
#[derive(Debug)]
struct Track {
    kind: TrackKind,
    track_id: u32,
    timescale: u32,
    config: Option<TrackConfig>,
    /// Progressive mode: full sample table (decode order).
    table: Vec<TableSample>,
    /// Index of the next table sample to emit.
    cursor: usize,
    /// Fragmented mode: samples from parsed fragments awaiting data.
    frag: VecDeque<TableSample>,
    /// Furthest sample end seen, in seconds.
    end_pts: f64,
}

struct StreamBuffer {
    data: Vec<u8>,
    /// Absolute stream offset of `data[0]`.
    base: u64,
}

impl StreamBuffer {
    fn new() -> Self {
        StreamBuffer {
            data: Vec::new(),
            base: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
    }

    /// Absolute end offset of buffered data.
    fn end(&self) -> u64 {
        self.base + self.data.len() as u64
    }

    /// Drops bytes before absolute offset `to`.
    fn advance(&mut self, to: u64) {
        let drop = (to.min(self.end()) - self.base) as usize;
        if drop > 0 {
            self.data.drain(..drop);
            self.base = to;
        }
    }

    fn slice(&self, offset: u64, len: usize) -> Option<&[u8]> {
        let start = offset.checked_sub(self.base)? as usize;
        let end = start.checked_add(len)?;
        self.data.get(start..end)
    }
}

/// A parsed top-level or child box: type + absolute content range.
#[derive(Debug, Clone)]
struct BoxRef {
    ty: [u8; 4],
    /// Content range (excluding the 8/16-byte header).
    content: Range<u64>,
}

fn box_type(b: &[u8; 4]) -> String {
    b.iter().map(|&c| c as char).collect()
}

fn u16_at(b: &[u8], pos: usize) -> Option<u16> {
    b.get(pos..pos + 2)
        .map(|s| u16::from_be_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], pos: usize) -> Option<u32> {
    b.get(pos..pos + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn i32_at(b: &[u8], pos: usize) -> Option<i32> {
    u32_at(b, pos).map(|v| v as i32)
}

fn u64_at(b: &[u8], pos: usize) -> Option<u64> {
    b.get(pos..pos + 8)
        .map(|s| u64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}

/// Walks child boxes within `[from, to)` of the buffer (absolute offsets).
fn walk(buf: &StreamBuffer, from: u64, to: u64) -> Vec<BoxRef> {
    let mut out = Vec::new();
    let mut p = from;
    while p + 8 <= to {
        let Some(head) = buf.slice(p, 8) else { break };
        let size32 = u32_at(head, 0).unwrap_or(0) as u64;
        let mut ty = [0u8; 4];
        ty.copy_from_slice(&head[4..8]);
        let header: u64 = if size32 == 1 { 16 } else { 8 };
        let size = if size32 == 1 {
            buf.slice(p + 8, 8).and_then(|s| u64_at(s, 0)).unwrap_or(0)
        } else {
            size32
        };
        if size < header || p + size > to {
            break;
        }
        out.push(BoxRef {
            ty,
            content: p + header..p + size,
        });
        p += size;
    }
    out
}

fn find<'a>(boxes: &'a [BoxRef], ty: &str) -> Option<&'a BoxRef> {
    let want: Vec<char> = ty.chars().collect();
    boxes
        .iter()
        .find(|b| b.ty.iter().map(|&c| c as char).eq(want.iter().copied()))
}

/// Expands an ISO descriptor length (0x80 continuation bits).
fn descriptor_len(buf: &[u8], pos: &mut usize) -> Option<u32> {
    let mut len = 0u32;
    for _ in 0..4 {
        let b = *buf.get(*pos)?;
        *pos += 1;
        len = (len << 7) | (b & 0x7F) as u32;
        if b & 0x80 == 0 {
            break;
        }
    }
    Some(len)
}

/// Parses an `esds` box content into the AudioSpecificConfig bytes.
fn parse_esds(content: &[u8]) -> Option<Vec<u8>> {
    // esds: version(4) then ES_Descriptor.
    if content.len() < 4 {
        return None;
    }
    let mut pos = 4;
    let tag = *content.get(pos)?;
    pos += 1;
    if tag != 0x03 {
        return None;
    }
    let _es_len = descriptor_len(content, &mut pos)?;
    if content.len() < pos + 3 {
        return None;
    }
    pos += 2; // ES_ID
    let flags = content[pos];
    pos += 1;
    if flags & 0x80 != 0 {
        pos += 2; // streamDependenceRank
    }
    if flags & 0x40 != 0 {
        pos += 2; // URL
    }
    if flags & 0x20 != 0 {
        pos += 2; // OCR stream
    }
    // Skip extension descriptors until DecoderConfigDescriptor (0x04).
    let mut guard = 0;
    loop {
        guard += 1;
        if guard > 8 || pos >= content.len() {
            return None;
        }
        let tag = content[pos];
        pos += 1;
        match tag {
            0x04 => {
                let _cfg_len = descriptor_len(content, &mut pos)?;
                // objectTypeIndication(1) streamType(1) bufferSizeDB(3)
                // maxBitrate(4) avgBitrate(4) = 13 bytes
                pos += 13;
                if pos >= content.len() {
                    return None;
                }
                let tag2 = content[pos];
                pos += 1;
                if tag2 != 0x05 {
                    return None;
                }
                let asc_len = descriptor_len(content, &mut pos)? as usize;
                return content.get(pos..pos + asc_len).map(|s| s.to_vec());
            }
            0x05 => {
                // Direct DecSpecificInfo (non-standard but seen in the wild).
                let asc_len = descriptor_len(content, &mut pos)? as usize;
                return content.get(pos..pos + asc_len).map(|s| s.to_vec());
            }
            _ => {
                let len = descriptor_len(content, &mut pos)? as usize;
                pos += len;
            }
        }
    }
}

/// Parses a `avcC` content (kept whole as decoder extra data).
fn parse_avcc(content: &[u8]) -> Option<Vec<u8>> {
    if content.len() < 8 {
        return None;
    }
    Some(content.to_vec())
}

impl Track {
    fn new(kind: TrackKind, track_id: u32, timescale: u32) -> Track {
        Track {
            kind,
            track_id,
            timescale: timescale.max(1),
            config: None,
            table: Vec::new(),
            cursor: 0,
            frag: VecDeque::new(),
            end_pts: 0.0,
        }
    }

    fn next_table_sample(&self) -> Option<&TableSample> {
        self.table.get(self.cursor)
    }

    fn next_frag_sample(&self) -> Option<&TableSample> {
        self.frag.front()
    }
}

/// The streaming demuxer for one byte lane.
pub struct StreamDemuxer {
    buf: StreamBuffer,
    tracks: Vec<Track>,
    info: Option<StreamInfo>,
    /// True once a moov has been parsed.
    have_moov: bool,
    /// True when the stream is fragmented (mvex seen, or a moof parsed).
    fragmented: bool,
    /// True after EOF (no more bytes will arrive).
    eof: bool,
    /// Absolute stream offset up to which top-level boxes are parsed.
    parsed_to: u64,
    /// First-push format validation done.
    validated: bool,
    /// Video samples ready for the decoder.
    video_ready: VecDeque<Sample>,
    /// Audio samples ready for the decoder.
    audio_ready: VecDeque<Sample>,
}

impl Default for StreamDemuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDemuxer {
    pub fn new() -> StreamDemuxer {
        StreamDemuxer {
            buf: StreamBuffer::new(),
            tracks: Vec::new(),
            info: None,
            have_moov: false,
            fragmented: false,
            eof: false,
            parsed_to: 0,
            validated: false,
            video_ready: VecDeque::new(),
            audio_ready: VecDeque::new(),
        }
    }

    /// Appends bytes and processes as many complete boxes as possible.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), DemuxError> {
        if !self.validated && bytes.len() >= 8 {
            // Require ftyp as the first box (all MP4 streams start with one).
            // Only the FIRST bytes ever seen are validated — the buffer may
            // legitimately drain to empty mid-stream.
            self.validated = true;
            if &bytes[4..8] != b"ftyp" && &bytes[4..8] != b"moov" && &bytes[4..8] != b"styp" {
                return Err(DemuxError::NotIsobmff);
            }
        }
        self.buf.push(bytes);
        self.process()?;
        Ok(())
    }

    /// Marks end-of-stream; buffered samples remain retrievable.
    pub fn finish(&mut self) {
        self.eof = true;
        let _ = self.process();
        // Fragmented streams carry mvhd duration 0; the real duration is
        // the furthest sample end observed.
        if let Some(info) = &mut self.info {
            if info.duration == 0.0 {
                let max_end = self.tracks.iter().map(|t| t.end_pts).fold(0.0f64, f64::max);
                if max_end > 0.0 {
                    info.duration = max_end;
                }
            }
        }
    }

    /// Byte count currently buffered (diagnostics / tests).
    pub fn buffered_bytes(&self) -> usize {
        self.buf.data.len()
    }

    fn process(&mut self) -> Result<(), DemuxError> {
        loop {
            let parsed_before = self.parsed_to;
            let base_before = self.buf.base;
            self.step()?;
            self.emit_ready();
            // Drop consumed bytes only once every track's sample stream is
            // fully defined — before the moov arrives we must not discard
            // anything (progressive files may place moov after a large
            // leading mdat; the BUFFER_CAP guard turns that into an
            // explicit error instead).
            let all_defined = !self.tracks.is_empty()
                && self
                    .tracks
                    .iter()
                    .all(|t| self.fragmented || !t.table.is_empty());
            if all_defined {
                let advance_to = self.next_consume_offset().unwrap_or(self.parsed_to);
                if advance_to > self.buf.base {
                    self.buf.advance(advance_to);
                }
            }
            if self.parsed_to == parsed_before && self.buf.base == base_before {
                break;
            }
        }
        if !self.have_moov && self.buf.data.len() > BUFFER_CAP {
            return Err(DemuxError::MissingMoov);
        }
        Ok(())
    }

    fn next_consume_offset(&self) -> Option<u64> {
        let mut min: Option<u64> = None;
        for track in &self.tracks {
            let next = if self.fragmented {
                track.next_frag_sample().map(|s| s.offset)
            } else {
                track.next_table_sample().map(|s| s.offset)
            };
            if let Some(offset) = next {
                min = Some(min.map_or(offset, |m: u64| m.min(offset)));
            }
        }
        min
    }

    /// Parses one complete top-level box (if fully buffered).
    fn step(&mut self) -> Result<(), DemuxError> {
        let p = self.parsed_to;
        let end = self.buf.end();
        if p >= end {
            return Ok(());
        }
        let Some(head) = self.buf.slice(p, 8) else {
            return Ok(());
        };
        let size32 = u32_at(head, 0).unwrap_or(0) as u64;
        let mut ty = [0u8; 4];
        ty.copy_from_slice(&head[4..8]);
        let header: u64 = if size32 == 1 { 16 } else { 8 };
        let size = if size32 == 1 {
            self.buf
                .slice(p + 8, 8)
                .and_then(|s| u64_at(s, 0))
                .unwrap_or(0)
        } else {
            size32
        };
        let (header, size) = if size == 0 {
            // Box extends to end of stream: only complete at EOF.
            if !self.eof {
                return Ok(());
            }
            (header, end - p)
        } else {
            (header, size)
        };
        if size < header {
            return Err(DemuxError::Malformed(format!(
                "box {} size {size} < header",
                box_type(&ty)
            )));
        }
        if end - p < size {
            return Ok(()); // wait for more bytes
        }
        self.consume_box(ty, p + header, p + size, p)?;
        self.parsed_to = p + size;
        Ok(())
    }

    fn consume_box(
        &mut self,
        ty: [u8; 4],
        content_from: u64,
        content_to: u64,
        box_start: u64,
    ) -> Result<(), DemuxError> {
        match &ty {
            b"ftyp" | b"styp" => {}
            b"moov" => self.parse_moov(content_from, content_to)?,
            b"moof" => {
                self.fragmented = true;
                self.parse_moof(content_from, content_to, box_start)?;
            }
            b"mdat" => {
                // Sample bytes live inside; nothing to do (offsets were
                // pre-registered by the sample tables / truns).
            }
            _ => {}
        }
        Ok(())
    }

    // -- moov ------------------------------------------------------------

    fn parse_moov(&mut self, from: u64, to: u64) -> Result<(), DemuxError> {
        if self.have_moov {
            return Ok(()); // second moov (rare): keep the first
        }
        self.have_moov = true;
        let top = walk(&self.buf, from, to);
        let mut duration = 0f64;
        if let Some(mvhd) = find(&top, "mvhd") {
            if let Some(c) = self.buf.slice(
                mvhd.content.start,
                (mvhd.content.end - mvhd.content.start) as usize,
            ) {
                let (timescale, dur) = if c.first().copied().unwrap_or(0) == 1 {
                    (u32_at(c, 20).unwrap_or(0), u64_at(c, 24).unwrap_or(0))
                } else {
                    (
                        u32_at(c, 12).unwrap_or(0),
                        u64::from(u32_at(c, 16).unwrap_or(0)),
                    )
                };
                if timescale > 0 {
                    duration = dur as f64 / timescale as f64;
                }
            }
        }
        let fragmented_hint = find(&top, "mvex").is_some();
        self.fragmented = self.fragmented || fragmented_hint;
        for trak in top.iter().filter(|b| &b.ty == b"trak") {
            self.parse_trak(trak.content.clone(), duration)?;
        }
        // Publish info.
        let mut info = StreamInfo {
            duration: if duration.is_finite() && duration > 0.0 {
                duration
            } else {
                0.0
            },
            video: None,
            audio: None,
        };
        for track in &self.tracks {
            if let Some(config) = &track.config {
                let config = config.clone();
                match track.kind {
                    TrackKind::Video => info.video = Some(config),
                    TrackKind::Audio => info.audio = Some(config),
                }
            }
        }
        self.info = Some(info);
        Ok(())
    }

    fn parse_trak(&mut self, range: Range<u64>, _moov_duration: f64) -> Result<(), DemuxError> {
        let top = walk(&self.buf, range.start, range.end);
        let Some(mdia) = find(&top, "mdia") else {
            return Ok(());
        };
        // Real track id from tkhd (tfhd fragments reference it).
        let mut track_id = self.tracks.len() as u32 + 1;
        if let Some(tkhd) = find(&top, "tkhd") {
            if let Some(c) = self.buf.slice(
                tkhd.content.start,
                (tkhd.content.end - tkhd.content.start) as usize,
            ) {
                let id_pos = if c.first().copied().unwrap_or(0) == 1 {
                    20
                } else {
                    12
                };
                if let Some(id) = u32_at(c, id_pos) {
                    track_id = id;
                }
            }
        }
        let children = walk(&self.buf, mdia.content.start, mdia.content.end);
        let (mut timescale, mut kind) = (0u32, None);
        if let Some(mdhd) = find(&children, "mdhd") {
            if let Some(c) = self.buf.slice(
                mdhd.content.start,
                (mdhd.content.end - mdhd.content.start) as usize,
            ) {
                if c.first().copied().unwrap_or(0) == 1 {
                    timescale = u32_at(c, 20).unwrap_or(0);
                } else {
                    timescale = u32_at(c, 12).unwrap_or(0);
                }
            }
        }
        if let Some(hdlr) = find(&children, "hdlr") {
            if let Some(c) = self.buf.slice(
                hdlr.content.start,
                (hdlr.content.end - hdlr.content.start) as usize,
            ) {
                if c.len() >= 12 {
                    kind = match &c[8..12] {
                        b"vide" => Some(TrackKind::Video),
                        b"soun" => Some(TrackKind::Audio),
                        _ => None,
                    };
                }
            }
        }
        let Some(kind) = kind else { return Ok(()) };
        let mut track = Track::new(kind, track_id, timescale.max(1));
        // stbl is nested mdia/minf/stbl.
        let stbl: Option<BoxRef> = find(&children, "stbl").cloned().or_else(|| {
            find(&children, "minf").and_then(|minf| {
                let minf_children = walk(&self.buf, minf.content.start, minf.content.end);
                find(&minf_children, "stbl").cloned()
            })
        });
        if let Some(stbl) = stbl {
            self.parse_stbl(&mut track, stbl.content.clone())?;
        }
        self.tracks.push(track);
        Ok(())
    }

    fn parse_stbl(&mut self, track: &mut Track, range: Range<u64>) -> Result<(), DemuxError> {
        let boxes = walk(&self.buf, range.start, range.end);
        let Some(stsd) = find(&boxes, "stsd") else {
            return Ok(());
        };
        let stsd_len = (stsd.content.end - stsd.content.start) as usize;
        let Some(stsd_c) = self.buf.slice(stsd.content.start, stsd_len) else {
            return Ok(());
        };
        // stsd: version/flags(4) entry_count(4) then entries.
        let entries = walk_bytes(stsd_c, 8, stsd_c.len());
        for entry in &entries {
            let format = entry.ty;
            let content = &stsd_c[entry.content.start as usize..entry.content.end as usize];
            match &format {
                b"avc1" | b"avc3" => {
                    // VisualSampleEntry: 6 reserved + 2 data_ref + 16
                    // predefined + width(2) height(2) ... 78 bytes then boxes.
                    let (mut width, mut height) = (0u32, 0u32);
                    if content.len() >= 32 {
                        width = u16_at(content, 24).unwrap_or(0) as u32;
                        height = u16_at(content, 26).unwrap_or(0) as u32;
                    }
                    let mut config = TrackConfig {
                        track_id: track.track_id,
                        codec: Codec::Unsupported("avc1 without avcC"),
                        extra: Vec::new(),
                        width,
                        height,
                        sample_rate: 0,
                        channels: 0,
                    };
                    let children = walk_bytes(content, 78.min(content.len()), content.len());
                    if let Some(avcc) = children.iter().find(|b| &b.ty == b"avcC") {
                        if let Some(extra) = parse_avcc(
                            &content[avcc.content.start as usize..avcc.content.end as usize],
                        ) {
                            config.codec = Codec::Avc;
                            config.extra = extra;
                        }
                    }
                    track.config = Some(config);
                }
                b"mp4a" => {
                    let mut config = TrackConfig {
                        track_id: track.track_id,
                        codec: Codec::Unsupported("mp4a without esds"),
                        extra: Vec::new(),
                        width: 0,
                        height: 0,
                        sample_rate: 0,
                        channels: 0,
                    };
                    if content.len() >= 28 {
                        // AudioSampleEntry (content offsets): reserved(6),
                        // data_ref(2), version(2)@8, revision(2)@10,
                        // vendor(4)@12, channels(2)@16, sample_size(2)@18,
                        // pre_defined(2)@20, reserved(2)@22, rate(4)@24.
                        config.channels = u16_at(content, 16).unwrap_or(2);
                        config.sample_rate = u32_at(content, 24).unwrap_or(0) >> 16;
                    }
                    // esds may be a direct child or nested in `wave`. Child
                    // boxes start at 28 (v0), 44 (v1), 64 (v2).
                    let version = u16_at(content, 8).unwrap_or(0);
                    let children_from = 28
                        + match version {
                            1 => 16,
                            2 => 36,
                            _ => 0,
                        };
                    let children =
                        walk_bytes(content, children_from.min(content.len()), content.len());
                    let mut esds = children.iter().find(|b| &b.ty == b"esds").cloned();
                    if esds.is_none() {
                        if let Some(wave) = children.iter().find(|b| &b.ty == b"wave") {
                            let wave_children = walk_bytes(
                                content,
                                wave.content.start as usize,
                                wave.content.end as usize,
                            );
                            esds = wave_children.iter().find(|b| &b.ty == b"esds").cloned();
                        }
                    }
                    if let Some(esds) = esds {
                        let asc = parse_esds(
                            &content[esds.content.start as usize..esds.content.end as usize],
                        );
                        if let Some(asc) = asc {
                            config.codec = Codec::AacLc;
                            config.extra = asc;
                            if config.sample_rate == 0 || config.channels == 0 {
                                // Fall back to the ASC's own declaration.
                                if let Some((rate, ch)) =
                                    audio_specific_config_params(&config.extra)
                                {
                                    if config.sample_rate == 0 {
                                        config.sample_rate = rate;
                                    }
                                    if config.channels == 0 {
                                        config.channels = ch;
                                    }
                                }
                            }
                        }
                    }
                    track.config = Some(config);
                }
                b"opus" | b"Opus" => {
                    let mut config = TrackConfig {
                        track_id: track.track_id,
                        codec: Codec::Unsupported("opus in mp4 pending"),
                        extra: Vec::new(),
                        width: 0,
                        height: 0,
                        sample_rate: 48000,
                        channels: 2,
                    };
                    let children = walk_bytes(content, 36.min(content.len()), content.len());
                    if let Some(dops) = children.iter().find(|b| &b.ty == b"dOps") {
                        let raw = &content[dops.content.start as usize..dops.content.end as usize];
                        if raw.len() >= 11 {
                            config.channels = u16::from_be_bytes([raw[9], raw[10]]).clamp(1, 2);
                            let pre_skip = u16::from_be_bytes([
                                raw[11.min(raw.len() - 1)],
                                raw.get(12).copied().unwrap_or(0),
                            ]);
                            let _ = pre_skip;
                        }
                        config.extra = raw.to_vec();
                    }
                    track.config = Some(config);
                }
                _ => {}
            }
            if track.config.is_some() {
                break;
            }
        }
        if self.fragmented {
            return Ok(());
        }
        // Progressive: build the full sample table.
        let Some(stts) = find(&boxes, "stts") else {
            return Ok(());
        };
        let stts_c = self.read_content(stts);
        let stsz = find(&boxes, "stsz").map(|b| self.read_content(b));
        let stco = find(&boxes, "stco").map(|b| self.read_content(b));
        let co64 = find(&boxes, "co64").map(|b| self.read_content(b));
        let stsc = find(&boxes, "stsc").map(|b| self.read_content(b));
        let stss = find(&boxes, "stss").map(|b| self.read_content(b));
        let ctts = find(&boxes, "ctts").map(|b| self.read_content(b));
        build_sample_table(
            track,
            &stts_c,
            stsz.as_deref(),
            stco.as_deref(),
            co64.as_deref(),
            stsc.as_deref(),
            stss.as_deref(),
            ctts.as_deref(),
        );
        Ok(())
    }

    fn read_content(&self, b: &BoxRef) -> Vec<u8> {
        self.buf
            .slice(b.content.start, (b.content.end - b.content.start) as usize)
            .map(|s| s.to_vec())
            .unwrap_or_default()
    }

    // -- moof ------------------------------------------------------------

    fn parse_moof(&mut self, from: u64, to: u64, moof_start: u64) -> Result<(), DemuxError> {
        let top = walk(&self.buf, from, to);
        for traf in top.iter().filter(|b| &b.ty == b"traf") {
            self.parse_traf(traf.content.clone(), moof_start)?;
        }
        Ok(())
    }

    fn parse_traf(&mut self, range: Range<u64>, moof_start: u64) -> Result<(), DemuxError> {
        let boxes = walk(&self.buf, range.start, range.end);
        let Some(tfhd) = find(&boxes, "tfhd") else {
            return Ok(());
        };
        let tfhd_c = self.read_content(tfhd);
        if tfhd_c.len() < 4 {
            return Ok(());
        }
        // tfhd is a full box: version(1) + flags(3), then track_ID(4).
        let tf_flags =
            (u32::from(tfhd_c[1]) << 16) | (u32::from(tfhd_c[2]) << 8) | u32::from(tfhd_c[3]);
        let track_id = u32_at(&tfhd_c, 4).unwrap_or(0);
        let mut pos = 8usize;
        let mut base_data_offset: Option<u64> = None;
        let mut _default_sample_description_index: Option<u32> = None;
        let mut default_duration: Option<u32> = None;
        let mut default_size: Option<u32> = None;
        let mut default_flags: Option<u32> = None;
        if tf_flags & 0x000001 != 0 {
            base_data_offset = u64_at(&tfhd_c, pos);
            pos += 8;
        }
        if tf_flags & 0x000002 != 0 {
            _default_sample_description_index = u32_at(&tfhd_c, pos);
            pos += 4;
        }
        if tf_flags & 0x000008 != 0 {
            default_duration = u32_at(&tfhd_c, pos);
            pos += 4;
        }
        if tf_flags & 0x000010 != 0 {
            default_size = u32_at(&tfhd_c, pos);
            pos += 4;
        }
        if tf_flags & 0x000020 != 0 {
            default_flags = u32_at(&tfhd_c, pos);
            let _ = pos;
        }
        // Explicit base-data-offset wins; otherwise the moof itself is the
        // base (the default_base_is_moof flag and the first-traf default).
        let base = base_data_offset.unwrap_or(moof_start);

        let Some(tfdt) = find(&boxes, "tfdt") else {
            return Ok(());
        };
        let tfdt_c = self.read_content(tfdt);
        let base_dts = if tfdt_c.first().copied().unwrap_or(0) == 1 {
            u64_at(&tfdt_c, 4).unwrap_or(0)
        } else {
            u64::from(u32_at(&tfdt_c, 4).unwrap_or(0))
        };

        let Some(trun) = find(&boxes, "trun") else {
            return Ok(());
        };
        let trun_c = self.read_content(trun);
        if trun_c.len() < 8 {
            return Ok(());
        }
        // trun is a full box: version(1) + flags(3).
        let trun_flags =
            (u32::from(trun_c[1]) << 16) | (u32::from(trun_c[2]) << 8) | u32::from(trun_c[3]);
        let sample_count = u32_at(&trun_c, 4).unwrap_or(0) as usize;
        let mut pos = 8usize;
        let mut data_offset: Option<i32> = None;
        if trun_flags & 0x000001 != 0 {
            data_offset = i32_at(&trun_c, pos);
            pos += 4;
        }
        let mut first_flags: Option<u32> = None;
        if trun_flags & 0x000004 != 0 {
            first_flags = u32_at(&trun_c, pos);
            pos += 4;
        }

        let Some(track) = self.tracks.iter_mut().find(|t| t.track_id == track_id) else {
            return Ok(());
        };
        let timescale = track.timescale.max(1) as f64;
        let mut offset: i64 = base as i64 + data_offset.unwrap_or(0) as i64;
        let mut dts = base_dts;
        for i in 0..sample_count {
            let duration = if trun_flags & 0x000100 != 0 {
                u32_at(&trun_c, pos).unwrap_or(0)
            } else {
                default_duration.unwrap_or(0)
            };
            if trun_flags & 0x000100 != 0 {
                pos += 4;
            }
            let size = if trun_flags & 0x000200 != 0 {
                u32_at(&trun_c, pos).unwrap_or(0)
            } else {
                default_size.unwrap_or(0)
            };
            if trun_flags & 0x000200 != 0 {
                pos += 4;
            }
            let flags = if trun_flags & 0x000400 != 0 {
                u32_at(&trun_c, pos).unwrap_or(0)
            } else if i == 0 && first_flags.is_some() {
                first_flags.unwrap_or(0)
            } else {
                default_flags.unwrap_or(0x0200_0000)
            };
            if trun_flags & 0x000400 != 0 {
                pos += 4;
            }
            let cts = if trun_flags & 0x000800 != 0 {
                i32_at(&trun_c, pos).unwrap_or(0) as i64
            } else {
                0
            };
            if trun_flags & 0x000800 != 0 {
                pos += 4;
            }
            let keyframe = flags & 0x0001_0000 == 0; // sample_is_non_sync_sample
            track.frag.push_back(TableSample {
                offset: offset.max(0) as u64,
                size,
                dts,
                duration,
                cts,
                sync: keyframe,
            });
            offset += size as i64;
            dts += u64::from(duration);
        }
        let _ = timescale;
        Ok(())
    }

    // -- emission ----------------------------------------------------------

    /// Moves fully-buffered samples into the ready queues.
    fn emit_ready(&mut self) {
        let end = self.buf.end();
        for track in &mut self.tracks {
            loop {
                let sample = if self.fragmented {
                    track.next_frag_sample().copied()
                } else {
                    track.next_table_sample().copied()
                };
                let Some(sample) = sample else { break };
                if sample.offset < self.buf.base {
                    // Already-consumed bytes: skip forward (seek case).
                    if self.fragmented {
                        track.frag.pop_front();
                    } else {
                        track.cursor += 1;
                    }
                    continue;
                }
                if sample.offset + sample.size as u64 > end {
                    break;
                }
                let timescale = track.timescale as f64;
                let pts = (sample.dts as i64 + sample.cts) as f64 / timescale;
                let dts = sample.dts as f64 / timescale;
                let data = self
                    .buf
                    .slice(sample.offset, sample.size as usize)
                    .map(|s| s.to_vec())
                    .unwrap_or_default();
                if data.len() == sample.size as usize {
                    if pts + sample.duration as f64 / timescale > track.end_pts {
                        track.end_pts = pts + sample.duration as f64 / timescale;
                    }
                    let out = Sample {
                        pts,
                        dts,
                        keyframe: sample.sync,
                        data,
                    };
                    match track.kind {
                        TrackKind::Video => {
                            if self.video_ready.len() < 4096 {
                                self.video_ready.push_back(out);
                            }
                        }
                        TrackKind::Audio => {
                            if self.audio_ready.len() < 8192 {
                                self.audio_ready.push_back(out);
                            }
                        }
                    }
                }
                if self.fragmented {
                    track.frag.pop_front();
                } else {
                    track.cursor += 1;
                }
            }
        }
    }

    /// Stream metadata, once the moov has been parsed.
    pub fn info(&self) -> Option<&StreamInfo> {
        self.info.as_ref()
    }

    /// Furthest sample end (seconds) seen on any track — the demuxed
    /// playback window's end, i.e. the MSE `buffered.end()` analogue.
    pub fn end_pts(&self) -> f64 {
        self.tracks.iter().map(|t| t.end_pts).fold(0.0f64, f64::max)
    }

    /// Drains ready video samples.
    pub fn take_video_samples(&mut self, out: &mut Vec<Sample>) {
        out.extend(self.video_ready.drain(..));
    }

    /// Drains ready audio samples.
    pub fn take_audio_samples(&mut self, out: &mut Vec<Sample>) {
        out.extend(self.audio_ready.drain(..));
    }
}

/// Byte-range child box walk for in-memory content slices.
fn walk_bytes(content: &[u8], from: usize, to: usize) -> Vec<BoxRef> {
    let mut out = Vec::new();
    let mut p = from;
    while p + 8 <= to && p + 8 <= content.len() {
        let size = u32_at(content, p).unwrap_or(0) as u64;
        let mut ty = [0u8; 4];
        ty.copy_from_slice(&content[p + 4..p + 8]);
        let (header, size) = if size == 1 {
            let large = content
                .get(p + 8..p + 16)
                .and_then(|s| u64_at(s, 0))
                .unwrap_or(0);
            (16u64, large)
        } else {
            (8u64, size)
        };
        if size < header || p as u64 + size > to as u64 {
            break;
        }
        out.push(BoxRef {
            ty,
            content: p as u64 + header..p as u64 + size,
        });
        p += size as usize;
    }
    out
}

/// Builds the progressive-mode sample list from the sample tables.
#[allow(clippy::too_many_arguments)]
fn build_sample_table(
    track: &mut Track,
    stts: &[u8],
    stsz: Option<&[u8]>,
    stco: Option<&[u8]>,
    co64: Option<&[u8]>,
    stsc: Option<&[u8]>,
    stss: Option<&[u8]>,
    ctts: Option<&[u8]>,
) {
    // Durations (delta per sample).
    let mut dts_list: Vec<(u64, u32)> = Vec::new(); // (dts, duration)
    {
        let mut dts = 0u64;
        if stts.len() >= 8 {
            let count = u32_at(stts, 4).unwrap_or(0) as usize;
            for i in 0..count {
                let pos = 8 + i * 8;
                let run = u32_at(stts, pos).unwrap_or(0) as usize;
                let delta = u32_at(stts, pos + 4).unwrap_or(0);
                for _ in 0..run {
                    dts_list.push((dts, delta));
                    dts += u64::from(delta);
                }
            }
        }
    }
    // Sizes.
    let mut sizes: Vec<u32> = Vec::new();
    if let Some(stsz) = stsz {
        if stsz.len() >= 12 {
            let uniform = u32_at(stsz, 4).unwrap_or(0);
            let count = u32_at(stsz, 8).unwrap_or(0) as usize;
            if uniform != 0 {
                sizes = vec![uniform; count];
            } else {
                for i in 0..count {
                    if let Some(v) = u32_at(stsz, 12 + i * 4) {
                        sizes.push(v);
                    }
                }
            }
        }
    }
    // Chunks.
    let mut chunks: Vec<u64> = Vec::new();
    if let Some(stco) = stco {
        if stco.len() >= 8 {
            let count = u32_at(stco, 4).unwrap_or(0) as usize;
            for i in 0..count {
                if let Some(v) = u32_at(stco, 8 + i * 4) {
                    chunks.push(u64::from(v));
                }
            }
        }
    } else if let Some(co64) = co64 {
        if co64.len() >= 8 {
            let count = u32_at(co64, 4).unwrap_or(0) as usize;
            for i in 0..count {
                if let Some(v) = u64_at(co64, 8 + i * 8) {
                    chunks.push(v);
                }
            }
        }
    }
    // Samples-per-chunk runs.
    let mut per_chunk: Vec<(u64, u32)> = Vec::new(); // (first_chunk_1based, samples)
    if let Some(stsc) = stsc {
        if stsc.len() >= 8 {
            let count = u32_at(stsc, 4).unwrap_or(0) as usize;
            for i in 0..count {
                let pos = 8 + i * 12;
                let first = u32_at(stsc, pos).unwrap_or(0);
                let spc = u32_at(stsc, pos + 4).unwrap_or(0);
                per_chunk.push((u64::from(first), spc));
            }
        }
    }
    // Composition offsets.
    let mut ctts_list: Vec<i64> = Vec::new();
    if let Some(ctts) = ctts {
        if ctts.len() >= 8 {
            let count = u32_at(ctts, 4).unwrap_or(0) as usize;
            for i in 0..count {
                let pos = 8 + i * 8;
                let run = u32_at(ctts, pos).unwrap_or(0) as usize;
                let raw = u32_at(ctts, pos + 4).unwrap_or(0);
                // v0 unsigned; v1 signed (version byte differs; treat raw
                // as unsigned i32 when the high bit is set).
                let off = if raw & 0x8000_0000 != 0 {
                    (raw as i32) as i64
                } else {
                    i64::from(raw)
                };
                for _ in 0..run {
                    ctts_list.push(off);
                }
            }
        }
    }
    // Sync samples (1-based sample numbers).
    let mut sync: std::collections::HashSet<u64> = std::collections::HashSet::new();
    if let Some(stss) = stss {
        if stss.len() >= 8 {
            let count = u32_at(stss, 4).unwrap_or(0) as usize;
            for i in 0..count {
                if let Some(v) = u32_at(stss, 8 + i * 4) {
                    sync.insert(u64::from(v));
                }
            }
        }
    }

    // Assemble per sample: walk chunks, assign sample counts.
    let total = sizes.len();
    let mut samples: Vec<TableSample> = Vec::with_capacity(total.min(1 << 20));
    let mut sample_idx = 0usize;
    for (chunk_no, chunk_offset) in chunks.iter().enumerate() {
        let first_chunk_1based = (chunk_no + 1) as u64;
        let spc = {
            // Find the stsc run covering this chunk.
            let mut run_spc = per_chunk.first().map(|c| c.1).unwrap_or(0);
            for (i, (first, spc)) in per_chunk.iter().enumerate() {
                if *first <= first_chunk_1based {
                    run_spc = *spc;
                    let _ = i;
                } else {
                    break;
                }
            }
            run_spc
        };
        let mut offset = *chunk_offset;
        for _ in 0..spc {
            if sample_idx >= total {
                break;
            }
            let size = sizes[sample_idx];
            let (dts, duration) = dts_list.get(sample_idx).copied().unwrap_or((0, 0));
            let cts = ctts_list.get(sample_idx).copied().unwrap_or(0);
            let is_sync = sync.is_empty() || sync.contains(&((sample_idx + 1) as u64));
            samples.push(TableSample {
                offset,
                size,
                dts,
                duration,
                cts,
                sync: is_sync,
            });
            offset += u64::from(size);
            sample_idx += 1;
        }
    }
    track.table = samples;
    track.cursor = 0;
}

/// Minimal AudioSpecificConfig peek: (sample_rate, channels) for fallbacks.
fn audio_specific_config_params(asc: &[u8]) -> Option<(u32, u16)> {
    if asc.is_empty() {
        return None;
    }
    let b0 = asc[0];
    let b1 = *asc.get(1).unwrap_or(&0);
    let object_type = (b0 >> 3) & 0x1F;
    let sampling_index = ((b0 & 0x07) << 1) | (b1 >> 7);
    let rate = match sampling_index {
        0 => 96000,
        1 => 88200,
        2 => 64000,
        3 => 48000,
        4 => 44100,
        5 => 32000,
        6 => 24000,
        7 => 22050,
        8 => 16000,
        9 => 12000,
        10 => 11025,
        11 => 8000,
        12 => 7350,
        _ => 44100,
    };
    let channels = if sampling_index == 13 || sampling_index == 14 {
        // Explicit frequency: skip 24 bits (3 bytes over the two we have).
        let b4 = *asc.get(3).unwrap_or(&0);
        u16::from(b4 >> 6) & 0x3
    } else {
        let ch = (b1 >> 3) & 0x0F;
        match ch {
            0 => 2, // defined in AOT-specific config; assume stereo
            7 => 8,
            n => u16::from(n),
        }
    };
    let _ = object_type;
    Some((rate, channels))
}
