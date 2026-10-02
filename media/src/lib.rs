//! Rrowser media: the HTML5 media decode pipeline.
//!
//! Architecture (see `docs/MEDIA.md`):
//!
//! * **Byte-stream core.** Every source — a direct `<video src>` URL streamed
//!   in ranged chunks by the engine, an fMP4-HLS live playlist, or MSE
//!   `SourceBuffer.appendBuffer()` pushes from page JavaScript — feeds the
//!   same sequential byte ingress. No seek required anywhere downstream.
//! * **Original ISOBMFF demuxer** (`isobmff`): a streaming MP4/fMP4 box
//!   parser that produces codec configs (avcC / AudioSpecificConfig) and
//!   timestamped samples from both progressive sample tables and movie
//!   fragments. Consumed bytes are discarded, so memory stays bounded for
//!   live streams.
//! * **Decoders are pluggable and crash-bounded**: H.264 via the openh264
//!   crate, AAC via symphonia, presented as straight RGBA8 frames (the
//!   display-list image type) plus interleaved f32 PCM.
//! * **One worker thread per playing element**, posting frame-ready
//!   notifications to the page thread. A video frame is painted by the
//!   existing image blit path; audio goes to a real-time sink (cpal) with a
//!   graceful silent fallback.
//!
//! The crate has no tokio and no engine dependencies: the engine owns
//! fetching, the page thread owns wiring.

pub mod audio;
pub mod decode;
pub mod isobmff;
pub mod mpegts;
pub mod pipeline;

pub use pipeline::{MediaInfo, MediaIngress, MediaPipeline};

/// Events a media element reports to the page (mapped onto DOM media
/// events by the engine).
#[derive(Debug, Clone, PartialEq)]
pub enum MediaEvent {
    /// Duration/dimensions known (maps to `loadedmetadata`).
    LoadedMetadata {
        /// Duration in seconds (f64::INFINITY for live).
        duration: f64,
        /// Video width (0 for audio-only).
        width: u32,
        /// Video height (0 for audio-only).
        height: u32,
        /// True when a decodable video track exists.
        has_video: bool,
        /// True when a decodable audio track exists.
        has_audio: bool,
    },
    /// Enough data decoded to start playback (`canplay`).
    CanPlay,
    /// `play` event fired (play() called).
    Playing,
    /// `pause` event fired.
    Paused,
    /// `ended` event fired.
    Ended,
    /// `timeupdate` (fired at ~4 Hz while playing).
    TimeUpdate {
        /// Current presentation time in seconds.
        time: f64,
    },
    /// Decode/source failure (`error`).
    Error(String),
}

/// Notifications posted from the pipeline worker thread to the page thread.
#[derive(Debug, Clone)]
pub enum MediaNotification {
    /// A new presentable video frame is available in `latest_frame()`.
    FrameReady,
    /// A media event fired.
    Event(MediaEvent),
}

/// Byte ingress for one media element. Lanes map 1:1 to MSE SourceBuffers
/// (a typical YouTube player uses two: one fMP4 video track, one fMP4 audio
/// track); direct and HLS sources use lane 0 only.
#[derive(Clone)]
pub struct MediaIngressSender {
    tx: std::sync::mpsc::Sender<IngressMessage>,
}

impl MediaIngressSender {
    /// Push bytes into a lane (a completed ranged chunk, an appended MSE
    /// segment, an HLS media segment).
    pub fn push(&self, lane: u64, bytes: Vec<u8>) {
        let _ = self.tx.send(IngressMessage {
            lane,
            data: IngressData::Bytes(bytes),
        });
    }

    /// Signal that no more bytes will arrive on a lane (end of stream).
    pub fn close_lane(&self, lane: u64) {
        let _ = self.tx.send(IngressMessage {
            lane,
            data: IngressData::Eof,
        });
    }
}

pub(crate) struct IngressMessage {
    pub lane: u64,
    pub data: IngressData,
}

pub(crate) enum IngressData {
    Bytes(Vec<u8>),
    Eof,
}

/// Worker control commands.
#[derive(Debug)]
pub(crate) enum Control {
    Play,
    Pause,
    /// Seek forward to `time` (samples before it are skipped as they
    /// stream through). Backward seeks clamp to the current position.
    Seek(f64),
    SetVolume(f32),
    SetMuted(bool),
    Close,
}

/// Create a media pipeline: returns the handle (page-thread side) plus the
/// byte ingress handle (safe to share with engine tasks).
///
/// `notify` is invoked from the worker thread; it must be cheap (send a
/// message, never block).
pub fn open_pipeline(
    notify: std::sync::Arc<dyn Fn(MediaNotification) + Send + Sync>,
) -> (MediaPipeline, MediaIngressSender) {
    pipeline::spawn(notify)
}
