//! AAC audio decoding (symphonia) and the real-time output sink (cpal).
//!
//! The sink is deliberately optional at runtime: when no audio device is
//! present (headless CI, servers) playback continues with a silent fallback
//! and a wall clock. PCM flows through a bounded lock-free ring — the
//! decoder backpressures naturally when the device runs behind.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use ringbuf::{HeapProd, HeapRb};
use symphonia_codec_aac::AacDecoder;
use symphonia_core::audio::SampleBuffer;
use symphonia_core::codecs::Decoder as _;
use symphonia_core::codecs::CODEC_TYPE_AAC;
use symphonia_core::codecs::{CodecParameters, DecoderOptions};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Split};

use crate::isobmff::TrackConfig;

/// AAC-LC decoder bound to a track's AudioSpecificConfig.
pub struct AudioDecoder {
    decoder: Box<dyn symphonia_core::codecs::Decoder>,
    sample_rate: u32,
}

impl AudioDecoder {
    pub fn new(config: &TrackConfig) -> Result<AudioDecoder, String> {
        let channels_bits = match config.channels {
            1 => 0b1,
            _ => 0b11,
        };
        let mut params = CodecParameters::new();
        params
            .for_codec(CODEC_TYPE_AAC)
            .with_sample_rate(config.sample_rate.max(1))
            .with_channels(
                symphonia_core::audio::Channels::from_bits(channels_bits).unwrap_or(
                    symphonia_core::audio::Channels::FRONT_LEFT
                        | symphonia_core::audio::Channels::FRONT_RIGHT,
                ),
            )
            .with_extra_data(config.extra.clone().into_boxed_slice());
        let decoder = AacDecoder::try_new(&params, &DecoderOptions::default())
            .map_err(|e| format!("aac decoder init failed: {e}"))?;
        Ok(AudioDecoder {
            decoder: Box::new(decoder),
            sample_rate: config.sample_rate.max(8000),
        })
    }

    /// Sample rate of the decoded output.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Decodes one AAC sample into interleaved f32 (native channel count).
    /// Returns `(pts, rate, channels, samples)`.
    pub fn decode(&mut self, sample: &[u8], pts: f64) -> Option<(f64, u32, usize, Vec<f32>)> {
        let packet = symphonia_core::formats::Packet::new_from_slice(
            0,
            (pts * self.sample_rate as f64) as u64,
            1024,
            sample,
        );
        match self.decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = decoded.spec().to_owned();
                let cap = decoded.capacity() as u64;
                let mut sbuf = SampleBuffer::<f32>::new(cap, spec);
                sbuf.copy_interleaved_ref(decoded);
                let rate = spec.rate;
                let channels = spec.channels.count();
                Some((pts, rate, channels, sbuf.samples().to_vec()))
            }
            Err(_) => None,
        }
    }
}

/// Shared sink state (device thread ↔ worker thread).
pub struct SinkState {
    /// PCM frames actually consumed by the device.
    pub played_frames: AtomicU64,
    /// Volume 0.0-1.0 (f32 bits).
    pub volume: AtomicU32,
    pub muted: AtomicBool,
}

impl SinkState {
    fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed)).clamp(0.0, 1.0)
    }
}

/// A live cpal output stream.
pub struct AudioSink {
    pub state: Arc<SinkState>,
    pub sample_rate: u32,
    _stream: cpal::Stream,
}

/// Producer side handed to the pipeline worker.
pub struct SinkFeed {
    pub state: Arc<SinkState>,
    pub producer: HeapProd<f32>,
    pub sample_rate: u32,
    /// Total PCM frames produced (for fill-level computation).
    pub produced_frames: AtomicU64,
    /// Ring capacity in frames.
    pub capacity_frames: u64,
}

/// Opens the default output device at `rate` Hz. Returns `None` when no
/// device/format is available — callers must fall back to silence + wall
/// clock.
pub fn open_sink(rate: u32, _channels: usize) -> Option<(AudioSink, SinkFeed)> {
    let device = cpal::default_host().default_output_device()?;
    let cfg = device.default_output_config().ok()?;
    let dev_channels = cfg.channels().max(1) as usize;
    let stream_cfg = cpal::StreamConfig {
        channels: dev_channels as u16,
        sample_rate: cpal::SampleRate(rate),
        buffer_size: cpal::BufferSize::Default,
    };
    let state = Arc::new(SinkState {
        played_frames: AtomicU64::new(0),
        volume: AtomicU32::new(1.0f32.to_bits()),
        muted: AtomicBool::new(false),
    });
    // Ring: ~500 ms of stereo at the requested rate.
    let cap_frames = (rate as usize / 2).max(2048);
    let ring = HeapRb::<f32>::new(cap_frames * 2);
    let (producer, mut consumer) = ring.split();
    let cb_state = Arc::clone(&state);
    let err_fn = |err| {
        let _ = err; // device errors: stream dies, worker falls back
    };
    let stream = device
        .build_output_stream(
            &stream_cfg,
            move |out: &mut [f32], _: &_| {
                let volume = cb_state.volume();
                let muted = cb_state.muted.load(Ordering::Relaxed);
                let mut frames_written = 0usize;
                let frames = out.len() / dev_channels;
                for frame in 0..frames {
                    // Device channel count may differ from the source; the
                    // pipeline always produces stereo-interleaved, and mono
                    // devices take the left channel only (simple, correct
                    // enough for v1).
                    let l = consumer.try_pop().unwrap_or(0.0);
                    let r = if dev_channels > 1 {
                        consumer.try_pop().unwrap_or(l)
                    } else {
                        0.0
                    };
                    let (l, r) = if muted {
                        (0.0, 0.0)
                    } else {
                        (l * volume, r * volume)
                    };
                    out[frame * dev_channels] = l;
                    if dev_channels > 1 {
                        out[frame * dev_channels + 1] = r;
                    }
                    frames_written += 1;
                }
                cb_state
                    .played_frames
                    .fetch_add(frames_written as u64, Ordering::Relaxed);
            },
            err_fn,
            None,
        )
        .ok()?;
    stream.play().ok()?;
    Some((
        AudioSink {
            state: Arc::clone(&state),
            sample_rate: rate,
            _stream: stream,
        },
        SinkFeed {
            state,
            producer,
            sample_rate: rate,
            produced_frames: AtomicU64::new(0),
            capacity_frames: cap_frames as u64,
        },
    ))
}

/// Downmixes interleaved f32 with `in_channels` to stereo (mono duplicated).
pub fn to_stereo(pcm: &[f32], in_channels: usize, out: &mut Vec<f32>) {
    match in_channels {
        1 => {
            for &s in pcm {
                out.push(s);
                out.push(s);
            }
        }
        2 => out.extend_from_slice(pcm),
        _ => {
            for chunk in pcm.chunks(in_channels) {
                out.push(chunk.first().copied().unwrap_or(0.0));
                out.push(chunk.get(1).copied().unwrap_or(0.0));
            }
        }
    }
}
