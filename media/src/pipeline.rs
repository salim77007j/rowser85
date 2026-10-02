//! The per-element media worker: one thread drives demux → decode →
//! presentation for a playing media element.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::audio::{self, AudioDecoder, SinkFeed};
use crate::decode::VideoDecoder;
use ringbuf::traits::Producer;

use crate::isobmff::{Sample, StreamDemuxer};
use crate::mpegts::TsDemuxer;
use crate::{Control, MediaEvent, MediaIngressSender, MediaNotification};

/// Decode-ahead lead in seconds (video + audio scheduling window).
const LEAD: f64 = 0.30;
/// Maximum presentable video frames kept ahead.
const VIDEO_QUEUE_CAP: usize = 4;
/// Audio ring fill target in seconds.
const AUDIO_FILL: f64 = 0.40;
/// Worker tick.
const TICK: Duration = Duration::from_millis(8);

/// Snapshot of element state readable from the page thread.
#[derive(Debug, Clone, Default)]
pub struct MediaInfo {
    /// Duration in seconds (0 = unknown/live).
    pub duration: f64,
    /// Video width (0 = audio-only).
    pub width: u32,
    /// Video height.
    pub height: u32,
    pub has_video: bool,
    pub has_audio: bool,
}

pub(crate) struct Shared {
    /// Latest presentable video frame (straight RGBA8).
    pub frame: Mutex<Option<Arc<rowser_image_type::DecodedImage>>>,
    pub info: Mutex<MediaInfo>,
    /// Current presentation time (seconds; worker-owned, page-read).
    pub time: Mutex<f64>,
    pub paused: AtomicBool,
    /// Decoded PCM byte counter (diagnostics + validation).
    pub audio_bytes: AtomicU64,
    pub ended: AtomicBool,
    pub error: Mutex<Option<String>>,
}

// A tiny local RGBA image type so this crate does not depend on the
// rendering crate (the engine converts at the boundary).
pub(crate) mod rowser_image_type {
    use std::sync::Arc;
    #[derive(Debug, Clone)]
    pub struct DecodedImage {
        pub width: u32,
        pub height: u32,
        pub rgba: Arc<Vec<u8>>,
    }
}

/// Page-thread handle to a running pipeline.
pub struct MediaPipeline {
    pub(crate) shared: Arc<Shared>,
    cmd_tx: std::sync::mpsc::Sender<Control>,
    /// Demuxed playback window end per lane (seconds).
    lane_ends: Arc<Mutex<HashMap<u64, f64>>>,
}

impl MediaPipeline {
    /// Latest presentable video frame, if any.
    pub fn latest_frame(&self) -> Option<Arc<rowser_image_type::DecodedImage>> {
        self.shared.frame.lock().ok()?.clone()
    }

    /// Element info snapshot.
    pub fn info(&self) -> MediaInfo {
        self.shared
            .info
            .lock()
            .map(|i| i.clone())
            .unwrap_or_default()
    }

    /// Current presentation time in seconds.
    pub fn current_time(&self) -> f64 {
        self.shared.time.lock().map(|t| *t).unwrap_or(0.0)
    }

    /// True while paused (also true before the first play()).
    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    /// Decoded audio PCM bytes so far.
    pub fn audio_bytes(&self) -> u64 {
        self.shared.audio_bytes.load(Ordering::Relaxed)
    }

    /// Demuxed playback window end for a lane (`buffered.end()` analogue).
    pub fn buffered_end(&self, lane: u64) -> f64 {
        self.lane_ends
            .lock()
            .map(|m| m.get(&lane).copied().unwrap_or(0.0))
            .unwrap_or(0.0)
    }

    /// First error reported by the pipeline, if any.
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().ok()?.clone()
    }

    pub fn play(&self) {
        let _ = self.cmd_tx.send(Control::Play);
    }

    pub fn pause(&self) {
        let _ = self.cmd_tx.send(Control::Pause);
    }

    /// Forward seek (clamped to the current position in v1).
    pub fn seek(&self, time: f64) {
        let _ = self.cmd_tx.send(Control::Seek(time));
    }

    pub fn set_volume(&self, volume: f32) {
        let _ = self.cmd_tx.send(Control::SetVolume(volume.clamp(0.0, 1.0)));
    }

    pub fn set_muted(&self, muted: bool) {
        let _ = self.cmd_tx.send(Control::SetMuted(muted));
    }

    /// Stops the worker and frees decode resources.
    pub fn close(&self) {
        let _ = self.cmd_tx.send(Control::Close);
    }
}

/// Byte ingress handle (shared with engine fetch tasks).
/// Alias of the crate-level [`crate::MediaIngressSender`].
pub type MediaIngress = crate::MediaIngressSender;

/// Spawns the worker thread.
pub(crate) fn spawn(
    notify: Arc<dyn Fn(MediaNotification) + Send + Sync>,
) -> (MediaPipeline, MediaIngressSender) {
    let shared = Arc::new(Shared {
        frame: Mutex::new(None),
        info: Mutex::new(MediaInfo::default()),
        time: Mutex::new(0.0),
        paused: AtomicBool::new(true),
        audio_bytes: AtomicU64::new(0),
        ended: AtomicBool::new(false),
        error: Mutex::new(None),
    });
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Control>();
    let (ing_tx, ing_rx) = std::sync::mpsc::channel::<crate::IngressMessage>();
    let lane_ends: Arc<Mutex<HashMap<u64, f64>>> = Arc::new(Mutex::new(HashMap::new()));
    let worker = Worker {
        shared: Arc::clone(&shared),
        notify: Arc::clone(&notify),
        lane_ends: Arc::clone(&lane_ends),
        lanes: HashMap::new(),
        video: None,
        audio: None,
        video_queue: Vec::new(),
        pending_video: std::collections::VecDeque::new(),
        pending_audio: std::collections::VecDeque::new(),
        last_presented_pts: f64::NEG_INFINITY,
        play_accum: 0.0,
        resume_at: None,
        skip_video_before: 0.0,
        skip_audio_before: 0.0,
        sink: None,
        sink_feed: None,
        sink_rate: 0,
        metadata_sent: false,
        canplay_sent: false,
        last_timeupdate: Instant::now(),
        have_duration: false,
    };
    let pipeline = MediaPipeline {
        shared,
        cmd_tx,
        lane_ends: Arc::clone(&lane_ends),
    };
    let ingress = MediaIngressSender { tx: ing_tx };
    std::thread::Builder::new()
        .name("rowser-media".into())
        .spawn(move || worker.run(cmd_rx, ing_rx))
        .expect("spawn media worker");
    (pipeline, ingress)
}

// Re-exported for engine use.
pub use crate::MediaIngressSender as IngressSenderAlias;

struct LaneState {
    demuxer: LaneDemuxer,
    eof: bool,
}

/// Per-lane demuxer: MP4 (direct / fMP4 / MSE) or MPEG-TS (HLS TS
/// segments), chosen by sniffing the first bytes on the lane.
enum LaneDemuxer {
    Mp4(StreamDemuxer),
    Ts(TsDemuxer),
}

impl LaneDemuxer {
    fn push(&mut self, bytes: &[u8]) -> Result<(), crate::isobmff::DemuxError> {
        match self {
            LaneDemuxer::Mp4(demuxer) => demuxer.push(bytes),
            LaneDemuxer::Ts(demuxer) => demuxer.push(bytes),
        }
    }

    fn finish(&mut self) {
        match self {
            LaneDemuxer::Mp4(demuxer) => demuxer.finish(),
            LaneDemuxer::Ts(demuxer) => demuxer.finish(),
        }
    }

    fn info(&self) -> Option<crate::isobmff::StreamInfo> {
        match self {
            LaneDemuxer::Mp4(demuxer) => demuxer.info().cloned(),
            LaneDemuxer::Ts(demuxer) => demuxer.info(),
        }
    }

    fn end_pts(&self) -> f64 {
        match self {
            LaneDemuxer::Mp4(demuxer) => demuxer.end_pts(),
            LaneDemuxer::Ts(demuxer) => demuxer.end_pts(),
        }
    }

    fn take_video_samples(&mut self, out: &mut Vec<Sample>) {
        match self {
            LaneDemuxer::Mp4(demuxer) => demuxer.take_video_samples(out),
            LaneDemuxer::Ts(demuxer) => demuxer.take_video_samples(out),
        }
    }

    fn take_audio_samples(&mut self, out: &mut Vec<Sample>) {
        match self {
            LaneDemuxer::Mp4(demuxer) => demuxer.take_audio_samples(out),
            LaneDemuxer::Ts(demuxer) => demuxer.take_audio_samples(out),
        }
    }

    /// True when this lane produces Annex-B H.264 (MPEG-TS).
    fn is_ts(&self) -> bool {
        matches!(self, LaneDemuxer::Ts(_))
    }
}

// SAFETY: the worker's decoders (openh264 / symphonia) hold raw pointers
// and are deliberately !Send. They are confined to this worker thread: the
// only cross-thread move is the spawn handoff itself, after which no other
// thread ever touches the Worker. This is the standard thread-confinement
// pattern; the page thread communicates exclusively through the mpsc
// channels and the atomics in `Shared`.
unsafe impl Send for Worker {}

struct Worker {
    shared: Arc<Shared>,
    notify: Arc<dyn Fn(MediaNotification) + Send + Sync>,
    lane_ends: Arc<Mutex<HashMap<u64, f64>>>,
    lanes: HashMap<u64, LaneState>,
    video: Option<VideoDecoder>,
    audio: Option<AudioDecoder>,
    /// Presentable frames: (pts, image), ascending pts.
    video_queue: Vec<(f64, Arc<rowser_image_type::DecodedImage>)>,
    /// Samples drained from the demuxer awaiting their scheduling window.
    pending_video: std::collections::VecDeque<Sample>,
    pending_audio: std::collections::VecDeque<Sample>,
    last_presented_pts: f64,
    /// Accumulated playback seconds while paused.
    play_accum: f64,
    /// Instant of the current playing stretch (None while paused).
    resume_at: Option<Instant>,
    skip_video_before: f64,
    skip_audio_before: f64,
    sink: Option<audio::AudioSink>,
    sink_feed: Option<SinkFeed>,
    sink_rate: u32,
    metadata_sent: bool,
    canplay_sent: bool,
    last_timeupdate: Instant,
    have_duration: bool,
}

impl Worker {
    fn clock(&self) -> f64 {
        let mut t = self.play_accum;
        if let Some(resume) = self.resume_at {
            t += resume.elapsed().as_secs_f64();
        }
        t
    }

    fn notify(&self, n: MediaNotification) {
        (self.notify)(n);
    }

    fn set_error(&mut self, message: String) {
        let mut slot = self.shared.error.lock().unwrap();
        if slot.is_none() {
            *slot = Some(message.clone());
            drop(slot);
            self.play_accum = self.clock();
            self.resume_at = None;
            self.shared.paused.store(true, Ordering::Relaxed);
            self.notify(MediaNotification::Event(MediaEvent::Error(message)));
        }
    }

    fn run(
        mut self,
        cmd_rx: std::sync::mpsc::Receiver<Control>,
        ing_rx: std::sync::mpsc::Receiver<crate::IngressMessage>,
    ) {
        loop {
            // Controls.
            let mut close = false;
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    Control::Close => {
                        close = true;
                    }
                    Control::Play => {
                        // Restarting after `ended` (loop attribute, replay
                        // requests) must clear the ended latch.
                        self.shared.ended.store(false, Ordering::Relaxed);
                        if self.resume_at.is_none() {
                            self.resume_at = Some(Instant::now());
                            self.shared.paused.store(false, Ordering::Relaxed);
                            self.notify(MediaNotification::Event(MediaEvent::Playing));
                        }
                    }
                    Control::Pause => {
                        if let Some(resume) = self.resume_at.take() {
                            self.play_accum += resume.elapsed().as_secs_f64();
                        }
                        self.shared.paused.store(true, Ordering::Relaxed);
                        self.notify(MediaNotification::Event(MediaEvent::Paused));
                    }
                    Control::Seek(time) => {
                        let current = self.clock();
                        let target = time.max(current);
                        self.play_accum = target;
                        if let Some(resume) = self.resume_at.take() {
                            self.play_accum += resume.elapsed().as_secs_f64();
                        }
                        self.resume_at = if self.shared.paused.load(Ordering::Relaxed) {
                            None
                        } else {
                            Some(Instant::now())
                        };
                        self.skip_video_before = target + 0.02;
                        self.skip_audio_before = target + 0.02;
                        self.video_queue.clear();
                        self.last_presented_pts = f64::NEG_INFINITY;
                    }
                    Control::SetVolume(v) => {
                        if let Some(feed) = &self.sink_feed {
                            feed.state.volume.store(v.to_bits(), Ordering::Relaxed);
                        }
                    }
                    Control::SetMuted(m) => {
                        if let Some(feed) = &self.sink_feed {
                            feed.state.muted.store(m, Ordering::Relaxed);
                        }
                    }
                }
            }
            if close {
                return;
            }

            // Ingress: bounded work per tick.
            let mut ingested = 0;
            while ingested < 64 {
                match ing_rx.try_recv() {
                    Ok(crate::IngressMessage { lane, data }) => {
                        ingested += 1;
                        match data {
                            crate::IngressData::Bytes(bytes) => self.ingest(lane, &bytes),
                            crate::IngressData::Eof => {
                                if let Some(state) = self.lanes.get_mut(&lane) {
                                    state.eof = true;
                                    state.demuxer.finish();
                                }
                                self.publish_metadata();
                                // Fragmented streams learn their real
                                // duration only at EOF (mvhd carries 0).
                                self.refresh_duration();
                            }
                        }
                    }
                    Err(_) => break,
                }
            }

            if self.resume_at.is_some() {
                self.pump();
            }
            std::thread::sleep(TICK);
        }
    }

    fn ingest(&mut self, lane: u64, bytes: &[u8]) {
        let trace = std::env::var("ROWSER_MEDIA_TRACE").is_ok();
        if trace {
            eprintln!("[media] ingest lane={} bytes={}", lane, bytes.len());
        }
        let state = self.lanes.entry(lane).or_insert_with(|| LaneState {
            demuxer: if crate::mpegts::looks_like_ts(bytes) {
                LaneDemuxer::Ts(TsDemuxer::new())
            } else {
                LaneDemuxer::Mp4(StreamDemuxer::new())
            },
            eof: false,
        });
        if let Err(err) = state.demuxer.push(bytes) {
            if trace {
                eprintln!("[media] ingest error lane={lane}: {err}");
            }
            self.set_error(err.to_string());
            return;
        }
        if trace {
            eprintln!(
                "[media] lane {} info={:?}",
                lane,
                state
                    .demuxer
                    .info()
                    .map(|i| (i.duration, i.video.is_some(), i.audio.is_some()))
            );
        }
        self.publish_metadata();
    }

    /// Refreshes the shared duration once lanes report their sample-derived
    /// duration (finish() computes it for fragmented streams).
    fn refresh_duration(&mut self) {
        let mut max_duration = 0f64;
        for state in self.lanes.values() {
            if let Some(info) = state.demuxer.info() {
                if info.duration > max_duration {
                    max_duration = info.duration;
                }
            }
        }
        if max_duration > 0.0 {
            {
                let mut slot = self.shared.info.lock().unwrap();
                if max_duration > slot.duration {
                    slot.duration = max_duration;
                }
            }
            self.have_duration = true;
            let info = self.shared.info.lock().unwrap().clone();
            self.notify(MediaNotification::Event(MediaEvent::LoadedMetadata {
                duration: info.duration,
                width: info.width,
                height: info.height,
                has_video: info.has_video,
                has_audio: info.has_audio,
            }));
        }
    }

    /// Publishes LoadedMetadata once any lane reports stream info. TS
    /// lanes discover the audio config only from the first ADTS header,
    /// so a late audio config re-publishes (creating the audio decoder).
    fn publish_metadata(&mut self) {
        let mut duration = 0f64;
        let mut video_cfg = None;
        let mut audio_cfg = None;
        let mut video_annexb = false;
        for state in self.lanes.values() {
            if let Some(info) = state.demuxer.info() {
                if info.duration > duration {
                    duration = info.duration;
                }
                if video_cfg.is_none() {
                    if let Some(cfg) = info.video.clone().filter(|c| c.decodable()) {
                        video_annexb = state.demuxer.is_ts();
                        video_cfg = Some(cfg);
                    }
                }
                if audio_cfg.is_none() {
                    audio_cfg = info.audio.clone().filter(|c| c.decodable());
                }
            }
        }
        if video_cfg.is_none() && audio_cfg.is_none() {
            return;
        }
        let audio_new = audio_cfg.is_some() && self.audio.is_none();
        if self.metadata_sent && !audio_new {
            return;
        }
        // Instantiate decoders on first sight of the configs.
        if self.video.is_none() {
            if let Some(cfg) = &video_cfg {
                let built = if video_annexb {
                    VideoDecoder::new_annexb()
                } else {
                    VideoDecoder::new(cfg)
                };
                match built {
                    Ok(decoder) => self.video = Some(decoder),
                    Err(err) => {
                        self.set_error(err);
                        return;
                    }
                }
            }
        }
        if self.audio.is_none() {
            if let Some(cfg) = &audio_cfg {
                match AudioDecoder::new(cfg) {
                    Ok(decoder) => {
                        self.open_sink(decoder.sample_rate());
                        self.audio = Some(decoder);
                    }
                    Err(err) => {
                        self.set_error(err);
                        return;
                    }
                }
            }
        }
        let info = MediaInfo {
            duration,
            width: video_cfg.as_ref().map(|c| c.width).unwrap_or(0),
            height: video_cfg.as_ref().map(|c| c.height).unwrap_or(0),
            has_video: self.video.is_some(),
            has_audio: self.audio.is_some(),
        };
        *self.shared.info.lock().unwrap() = info.clone();
        self.have_duration = duration > 0.0;
        self.metadata_sent = true;
        self.notify(MediaNotification::Event(MediaEvent::LoadedMetadata {
            duration: info.duration,
            width: info.width,
            height: info.height,
            has_video: info.has_video,
            has_audio: info.has_audio,
        }));
    }

    fn open_sink(&mut self, rate: u32) {
        if self.sink.is_some() {
            return;
        }
        #[cfg(feature = "audio-cpal")]
        {
            if let Some((sink, feed)) = audio::open_sink(rate, 2) {
                self.sink_rate = sink.sample_rate;
                self.sink = Some(sink);
                self.sink_feed = Some(feed);
                return;
            }
        }
        let _ = rate;
        self.sink_rate = 0;
    }

    /// One decode/present tick while playing.
    fn pump(&mut self) {
        let clock = self.clock();
        if std::env::var("ROWSER_MEDIA_TRACE").is_ok() {
            static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let now = clock as u64;
            if now > LAST.load(std::sync::atomic::Ordering::Relaxed) {
                LAST.store(now, std::sync::atomic::Ordering::Relaxed);
                eprintln!(
                    "[media] pump clock={:.2} pend_v={} pend_a={}",
                    clock,
                    self.pending_video.len(),
                    self.pending_audio.len()
                );
            }
        }
        // Refresh per-lane demuxed window ends.
        if let Ok(mut ends) = self.lane_ends.lock() {
            for (lane, state) in self.lanes.iter() {
                ends.insert(*lane, state.demuxer.end_pts());
            }
        }
        // Drain new samples from every lane into the pending queues.
        let mut samples: Vec<Sample> = Vec::new();
        for state in self.lanes.values_mut() {
            state.demuxer.take_video_samples(&mut samples);
        }
        for sample in samples.drain(..) {
            if self.pending_video.len() < 4096 {
                self.pending_video.push_back(sample);
            }
        }
        for state in self.lanes.values_mut() {
            state.demuxer.take_audio_samples(&mut samples);
        }
        for sample in samples.drain(..) {
            if self.pending_audio.len() < 8192 {
                self.pending_audio.push_back(sample);
            }
        }
        // Feed decoders only inside the scheduling window; samples beyond it
        // stay queued at the front for the next tick.
        while let Some(sample) = self.pending_video.front().cloned() {
            if sample.pts > clock + LEAD {
                break;
            }
            let sample = self.pending_video.pop_front().unwrap();
            if !self.feed_video(sample, clock) {
                break; // decoder queue full: retry next tick
            }
        }
        while let Some(sample) = self.pending_audio.front().cloned() {
            if sample.pts > clock + AUDIO_FILL + 0.2 {
                break;
            }
            let sample = self.pending_audio.pop_front().unwrap();
            if !self.feed_audio(sample, clock) {
                break;
            }
        }
        self.present(clock);
        if self.last_timeupdate.elapsed() > Duration::from_millis(250) {
            self.last_timeupdate = Instant::now();
            *self.shared.time.lock().unwrap() = clock;
            self.notify(MediaNotification::Event(MediaEvent::TimeUpdate {
                time: clock,
            }));
        }
        self.check_ended(clock);
    }

    /// Decodes one video sample. Returns false when the decoder queue is
    /// full (caller re-queues the sample).
    fn feed_video(&mut self, sample: Sample, _clock: f64) -> bool {
        if sample.pts < self.skip_video_before {
            return true;
        }
        let Some(decoder) = self.video.as_mut() else {
            return true;
        };
        if self.video_queue.len() >= VIDEO_QUEUE_CAP {
            return false; // backpressure: presentation will drain
        }
        if let Some((w, h, rgba)) = decoder.decode(&sample.data) {
            // TS streams learn their dimensions only from the first decoded
            // frame; refresh the mirror so aspect-correct layout applies.
            if w > 0 {
                let dims_changed = {
                    let mut info = self.shared.info.lock().unwrap();
                    if info.width != w || info.height != h {
                        info.width = w;
                        info.height = h;
                        true
                    } else {
                        false
                    }
                };
                if dims_changed && self.metadata_sent {
                    let info = self.shared.info.lock().unwrap().clone();
                    self.notify(MediaNotification::Event(MediaEvent::LoadedMetadata {
                        duration: info.duration,
                        width: info.width,
                        height: info.height,
                        has_video: info.has_video,
                        has_audio: info.has_audio,
                    }));
                }
            }
            self.video_queue.push((
                sample.pts,
                Arc::new(rowser_image_type::DecodedImage {
                    width: w,
                    height: h,
                    rgba,
                }),
            ));
            if !self.canplay_sent {
                self.canplay_sent = true;
                self.notify(MediaNotification::Event(MediaEvent::CanPlay));
            }
        }
        true
    }

    /// Decodes one audio sample. Returns false when backpressured.
    fn feed_audio(&mut self, sample: Sample, _clock: f64) -> bool {
        if sample.pts < self.skip_audio_before {
            return true;
        }
        let Some(decoder) = self.audio.as_mut() else {
            return true;
        };
        // Ring fill check via produced−played counters (no ringbuf API
        // dependency for the fill level).
        if let Some(feed) = &self.sink_feed {
            let produced = feed.produced_frames.load(Ordering::Relaxed);
            let played = feed.state.played_frames.load(Ordering::Relaxed);
            let fill = produced.saturating_sub(played);
            if fill as f64 / self.sink_rate.max(1) as f64 > AUDIO_FILL {
                return false; // ring full: backpressure
            }
        }
        let dres = decoder.decode(&sample.data, sample.pts);
        if let Some((_, _rate, channels, pcm)) = dres {
            let mut stereo = Vec::with_capacity(pcm.len());
            audio::to_stereo(&pcm, channels, &mut stereo);
            self.shared
                .audio_bytes
                .fetch_add((stereo.len() * 4) as u64, Ordering::Relaxed);
            if let Some(feed) = &mut self.sink_feed {
                let mut pushed_frames = 0u64;
                for pair in stereo.chunks(2) {
                    let l = pair.first().copied().unwrap_or(0.0);
                    let r = pair.get(1).copied().unwrap_or(l);
                    if feed.producer.try_push(l).is_ok() && feed.producer.try_push(r).is_ok() {
                        pushed_frames += 1;
                    } else {
                        break;
                    }
                }
                feed.produced_frames
                    .fetch_add(pushed_frames, Ordering::Relaxed);
            }
        }
        true
    }

    /// Promotes the newest frame with pts <= clock into `shared.frame`.
    fn present(&mut self, clock: f64) {
        // Drop frames that are already stale beyond the newest presentable.
        let mut presentable_idx: Option<usize> = None;
        for (i, (pts, _)) in self.video_queue.iter().enumerate() {
            if *pts <= clock {
                presentable_idx = Some(i);
            }
        }
        if let Some(idx) = presentable_idx {
            if std::env::var("ROWSER_MEDIA_TRACE").is_ok() {
                eprintln!(
                    "[media] present pts={} clock={:.2} queue={}",
                    self.video_queue[idx].0,
                    clock,
                    self.video_queue.len()
                );
            }
            let (_, image) = self.video_queue.remove(idx);
            // Remove everything older than idx (stale).
            for _ in 0..idx {
                self.video_queue.remove(0);
            }
            if image.width > 0 {
                let mut slot = self.shared.frame.lock().unwrap();
                if slot
                    .as_ref()
                    .map(|prev| !Arc::ptr_eq(prev, &image))
                    .unwrap_or(true)
                {
                    *slot = Some(image);
                    drop(slot);
                    self.notify(MediaNotification::FrameReady);
                }
            }
        }
    }

    fn check_ended(&mut self, clock: f64) {
        if self.ended() {
            return;
        }
        let all_eof = self.lanes.values().all(|s| s.eof) && !self.lanes.is_empty();
        if !all_eof {
            return;
        }
        let duration = self.shared.info.lock().unwrap().duration;
        let queue_empty = self.video_queue.is_empty();
        if !self.have_duration {
            return;
        }
        if queue_empty && duration > 0.0 && clock >= duration - 0.05 {
            self.play_accum = duration;
            *self.shared.time.lock().unwrap() = duration;
            self.resume_at = None;
            self.shared.ended.store(true, Ordering::Relaxed);
            self.shared.paused.store(true, Ordering::Relaxed);
            self.notify(MediaNotification::Event(MediaEvent::Ended));
        }
    }

    fn ended(&self) -> bool {
        self.shared.ended.load(Ordering::Relaxed)
    }
}
