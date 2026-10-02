//! Demuxer + decoder validation against committed ffmpeg-generated fixtures.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rowser_media::isobmff::{Codec, StreamDemuxer};
use rowser_media::{open_pipeline, MediaEvent, MediaNotification};

fn load(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/testdata/{}", env!("CARGO_MANIFEST_DIR"), name))
        .expect("fixture present")
}

#[test]
fn demuxes_progressive_mp4() {
    let bytes = load("clock-mp4.mp4");
    let mut demuxer = StreamDemuxer::new();
    // Push in 8 KB chunks like the network would.
    for chunk in bytes.chunks(8192) {
        demuxer.push(chunk).expect("chunk parses");
    }
    demuxer.finish();
    let info = demuxer.info().expect("moov parsed");
    assert!(info.video.is_some(), "video track found");
    assert!(info.audio.is_some(), "audio track found");
    let video = info.video.as_ref().unwrap();
    assert_eq!(video.codec, Codec::Avc);
    assert!(!video.extra.is_empty(), "avcC present");
    assert_eq!((video.width, video.height), (160, 120));
    let audio = info.audio.as_ref().unwrap();
    assert_eq!(audio.codec, Codec::AacLc, "AAC-LC track");
    assert!(!audio.extra.is_empty(), "AudioSpecificConfig present");
    assert!(
        (info.duration - 4.0).abs() < 0.1,
        "duration ~4.0, got {}",
        info.duration
    );
    drop(info);

    let mut samples = Vec::new();
    demuxer.take_video_samples(&mut samples);
    assert!(!samples.is_empty(), "video samples emitted");
    assert!(samples[0].keyframe, "first video sample is a sync sample");
    let first_pts = samples[0].pts;
    assert!(
        (first_pts - 0.0).abs() < 0.05,
        "starts at 0, got {first_pts}"
    );
    for s in &samples {
        assert!(!s.data.is_empty());
    }
    let mut audio_samples = Vec::new();
    demuxer.take_audio_samples(&mut audio_samples);
    assert!(!audio_samples.is_empty(), "audio samples emitted");
}

#[test]
fn demuxes_fragmented_mp4() {
    let bytes = load("clock-fmp4.mp4");
    let mut demuxer = StreamDemuxer::new();
    for chunk in bytes.chunks(4096) {
        demuxer.push(chunk).expect("chunk parses");
    }
    demuxer.finish();
    let info = demuxer.info().expect("init segment parsed");
    assert!(info.video.is_some() && info.audio.is_some());
    let video = info.video.as_ref().unwrap();
    assert_eq!(video.codec, Codec::Avc);

    let mut samples = Vec::new();
    demuxer.take_video_samples(&mut samples);
    assert!(!samples.is_empty(), "fragment video samples emitted");
    let mut audio_samples = Vec::new();
    demuxer.take_audio_samples(&mut audio_samples);
    assert!(!audio_samples.is_empty(), "fragment audio samples emitted");
    // Ordered timestamps within a track.
    for w in samples.windows(2) {
        assert!(w[1].pts >= w[0].pts, "pts monotonic");
    }
}

#[test]
fn demuxes_audio_only_m4a() {
    let bytes = load("sine-fmp4.m4a");
    let mut demuxer = StreamDemuxer::new();
    demuxer.push(&bytes).expect("parses");
    demuxer.finish();
    let info = demuxer.info().expect("parsed");
    assert!(info.video.is_none());
    let audio = info.audio.clone().expect("audio track");
    assert_eq!(audio.codec, Codec::AacLc);
    assert!(
        (info.duration - 3.0).abs() < 0.1,
        "duration ~3.0, got {}",
        info.duration
    );
}

#[test]
fn rejects_non_mp4() {
    let mut demuxer = StreamDemuxer::new();
    assert!(demuxer
        .push(b"<!DOCTYPE html><html><body>hello</body></html>")
        .is_err());
}

/// Full pipeline: bytes → demux → decode → RGBA frames + PCM, driven like
/// the engine would (push chunks, play, wait, collect notifications).
#[test]
fn pipeline_decodes_video_and_audio() {
    let events: Arc<std::sync::Mutex<Vec<MediaEvent>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let frames = Arc::new(AtomicU64::new(0));
    let (pipeline, ingress) = {
        let events = Arc::clone(&events);
        let frames = Arc::clone(&frames);
        open_pipeline(Arc::new(move |n: MediaNotification| match n {
            MediaNotification::FrameReady => {
                frames.fetch_add(1, Ordering::Relaxed);
            }
            MediaNotification::Event(ev) => events.lock().unwrap().push(ev),
        }))
    };
    let bytes = load("clock-fmp4.mp4");
    for chunk in bytes.chunks(16384) {
        ingress.push(0, chunk.to_vec());
    }
    ingress.close_lane(0);
    pipeline.play();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let done = pipeline.info().width > 0
            && pipeline.audio_bytes() > 0
            && events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, MediaEvent::Ended));
        if done || std::time::Instant::now() > deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let info = pipeline.info();
    assert_eq!((info.width, info.height), (160, 120), "dimensions surface");
    assert!(info.has_video && info.has_audio);
    assert!(
        pipeline.audio_bytes() > 0,
        "PCM decoded: {}",
        pipeline.audio_bytes()
    );
    assert!(frames.load(Ordering::Relaxed) > 0, "frames presented");
    let evs = events.lock().unwrap();
    assert!(evs
        .iter()
        .any(|e| matches!(e, MediaEvent::LoadedMetadata { .. })));
    assert!(evs.iter().any(|e| matches!(e, MediaEvent::CanPlay)));
    assert!(
        evs.iter().any(|e| matches!(e, MediaEvent::Ended)),
        "playback reached the end: {evs:?}"
    );
    // A frame with real content must be present (RGBA, opaque).
    let frame = pipeline.latest_frame().expect("latest frame");
    assert_eq!((frame.width, frame.height), (160, 120));
    assert_eq!(frame.rgba.len(), (160 * 120 * 4) as usize);
}

/// Decoded video pixels must actually CHANGE over time (the fixture is a
/// moving test pattern — this catches a stuck clock or repeated frame).
#[test]
fn video_frames_advance() {
    let (pipeline, ingress) = open_pipeline(Arc::new(|_| {}));
    let bytes = load("clock-mp4.mp4");
    for chunk in bytes.chunks(16384) {
        ingress.push(0, chunk.to_vec());
    }
    ingress.close_lane(0);
    pipeline.play();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut seen: Vec<Vec<u8>> = Vec::new();
    while std::time::Instant::now() < deadline && seen.len() < 6 {
        if let Some(frame) = pipeline.latest_frame() {
            // Hash the full frame: the test pattern's top-left corner is
            // static — only the mid/lower bands move.
            let mut sig = vec![0u8; 8];
            let mut h: u64 = 1469598103934665603;
            for (i, b) in frame.rgba.iter().enumerate() {
                h ^= u64::from(*b);
                h = h.wrapping_mul(1099511628211);
            }
            sig[0..8].copy_from_slice(&h.to_le_bytes());
            if seen.last() != Some(&sig) {
                seen.push(sig);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
    assert!(
        seen.len() >= 3,
        "at least 3 distinct frames, got {}",
        seen.len()
    );
}

#[test]
fn debug_dump_boxes() {
    for name in ["clock-fmp4.mp4", "sine-fmp4.m4a"] {
        let bytes = load(name);
        let mut d = StreamDemuxer::new();
        for chunk in bytes.chunks(4096) {
            if let Err(e) = d.push(chunk) {
                eprintln!("[{name}] push error: {e}");
                break;
            }
        }
        d.finish();
        eprintln!("[{name}] info: {:?}", d.info());
        let mut v = Vec::new();
        d.take_video_samples(&mut v);
        eprintln!("[{name}] video samples: {}", v.len());
        let mut a = Vec::new();
        d.take_audio_samples(&mut a);
        eprintln!("[{name}] audio samples: {}", a.len());
    }
    let bytes = load("clock-mp4.mp4");
    let mut demuxer = StreamDemuxer::new();
    for chunk in bytes.chunks(8192) {
        demuxer.push(chunk).ok();
    }
    demuxer.finish();
    eprintln!("info: {:?}", demuxer.info());
    eprintln!("buffered: {}", demuxer.buffered_bytes());
}
