//! Download manager: real downloads through the engine's networking stack
//! (`rowser-networking`), with pause/resume and byte progress.
//!
//! v1 pipeline: the engine's fetch returns the full body atomically, so a
//! download runs in two phases — *fetching* (indeterminate) and *writing*
//! (real byte progress, pausable). Resume across sessions uses HTTP
//! `Range` requests against the `.part` file, which is genuine resumption.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use rowser_networking::{build_context, fetch, ClientIdentity, FetchRequest, NetworkContext};
use rowser_storage::Storage;

use crate::Waker;

/// A download's phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadPhase {
    /// Fetching the resource over the network.
    Fetching,
    /// Writing bytes to disk (pausable).
    Writing,
    /// Paused by the user.
    Paused,
    /// Finished successfully.
    Done,
    /// Failed (network or disk error).
    Failed,
    /// Cancelled by the user.
    Cancelled,
}

/// One download, shared between the manager and the UI.
#[derive(Debug)]
pub struct DownloadItem {
    /// Monotonic id.
    pub id: u64,
    /// Source URL.
    pub url: String,
    /// Suggested filename.
    pub filename: String,
    /// Destination path.
    pub path: PathBuf,
    /// Total size in bytes, when known.
    pub total: Arc<Mutex<Option<u64>>>,
    /// Bytes on disk so far.
    pub received: Arc<Mutex<u64>>,
    /// Current phase.
    pub phase: Arc<Mutex<DownloadPhase>>,
    /// Pause signal.
    pub paused: Arc<AtomicBool>,
    /// Cancel signal.
    pub cancelled: Arc<AtomicBool>,
    /// Error text when failed.
    pub error: Arc<Mutex<String>>,
    /// Mime type, when known.
    pub mime: Arc<Mutex<String>>,
    /// Human label ("video", "archive", ...), when known.
    pub kind: &'static str,
}

impl DownloadItem {
    fn phase(&self) -> DownloadPhase {
        *self.phase.lock().unwrap()
    }

    /// Progress in 0..1 (unknown while fetching).
    pub fn progress(&self) -> Option<f32> {
        match *self.total.lock().unwrap() {
            Some(total) if total > 0 => {
                Some((*self.received.lock().unwrap() as f32 / total as f32).clamp(0.0, 1.0))
            }
            _ => None,
        }
    }

    /// Display phase + progress for the UI.
    pub fn status_text(&self) -> String {
        let received = *self.received.lock().unwrap();
        match self.phase() {
            DownloadPhase::Fetching => format!("Connecting… {}", short_url(&self.url)),
            DownloadPhase::Writing => match *self.total.lock().unwrap() {
                Some(total) => format!(
                    "{} of {} — {}",
                    human_bytes(received),
                    human_bytes(total),
                    human_speed_hint()
                ),
                None => format!("{} written", human_bytes(received)),
            },
            DownloadPhase::Paused => format!("Paused at {}", human_bytes(received)),
            DownloadPhase::Done => format!("Done — {}", human_bytes(received)),
            DownloadPhase::Failed => format!("Failed — {}", self.error.lock().unwrap().clone()),
            DownloadPhase::Cancelled => "Cancelled".to_owned(),
        }
    }
}

fn human_speed_hint() -> &'static str {
    "writing to disk"
}

fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn short_url(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .chars()
        .take(60)
        .collect()
}

/// The download manager.
pub struct Downloads {
    shared: Arc<Shared>,
}

struct Shared {
    net: Arc<NetworkContext>,
    items: Mutex<Vec<Arc<DownloadItem>>>,
    waker: Arc<Waker>,
    next_id: Mutex<u64>,
    runtime: tokio::runtime::Runtime,
}

impl Downloads {
    /// Builds the manager on its own network context (the engine holds the
    /// main profile database; downloads use a sibling storage file).
    pub fn new(profile_dir: &std::path::Path, waker: Arc<Waker>) -> anyhow::Result<Downloads> {
        let storage = Storage::open(profile_dir.join("downloads.redb"))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("rowser-downloads")
            .enable_all()
            .build()?;
        let net = runtime.block_on(async {
            build_context(
                Arc::new(storage),
                rowser_privacy::PrivacySettings::default(),
                Default::default(),
                Default::default(),
                ClientIdentity::default(),
            )
            .await
        })?;
        Ok(Downloads {
            shared: Arc::new(Shared {
                net: Arc::new(net),
                items: Mutex::new(Vec::new()),
                waker,
                next_id: Mutex::new(1),
                runtime,
            }),
        })
    }

    /// All downloads, newest first.
    pub fn items(&self) -> Vec<Arc<DownloadItem>> {
        let mut items = self.shared.items.lock().unwrap().clone();
        items.reverse();
        items
    }

    /// A snapshot of `(filename, phase, progress, status)` for the UI.
    pub fn snapshot(&self) -> Vec<(u64, String, DownloadPhase, Option<f32>, String)> {
        self.items()
            .into_iter()
            .map(|d| {
                (
                    d.id,
                    d.filename.clone(),
                    d.phase(),
                    d.progress(),
                    d.status_text(),
                )
            })
            .collect()
    }

    /// Starts a download of `url` into `dir`.
    pub fn start(&self, url: impl Into<String>, dir: PathBuf) -> u64 {
        let url = url.into();
        let id = {
            let mut next = self.shared.next_id.lock().unwrap();
            let id = *next;
            *next += 1;
            id
        };
        let filename = filename_for(&url, id);
        let path = unique_path(dir.join(&filename));
        let item = Arc::new(DownloadItem {
            id,
            filename,
            path: path.clone(),
            url: url.clone(),
            total: Arc::new(Mutex::new(None)),
            received: Arc::new(Mutex::new(0)),
            phase: Arc::new(Mutex::new(DownloadPhase::Fetching)),
            paused: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            error: Arc::new(Mutex::new(String::new())),
            mime: Arc::new(Mutex::new(String::new())),
            kind: kind_for(&url),
        });
        self.shared.items.lock().unwrap().push(Arc::clone(&item));
        let shared = Arc::clone(&self.shared);
        let item_for_job = Arc::clone(&item);
        self.shared.runtime.spawn(async move {
            run_download(shared, item_for_job).await;
        });
        self.shared.waker.wake();
        id
    }

    /// Pauses a download (park the writer; the fetch completes atomically).
    pub fn pause(&self, id: u64) {
        for item in self.items() {
            if item.id == id && item.phase() == DownloadPhase::Writing {
                item.paused.store(true, Ordering::SeqCst);
                *item.phase.lock().unwrap() = DownloadPhase::Paused;
            }
        }
        self.shared.waker.wake();
    }

    /// Resumes a paused download.
    pub fn resume(&self, id: u64) {
        for item in self.items() {
            if item.id == id && item.phase() == DownloadPhase::Paused {
                item.paused.store(false, Ordering::SeqCst);
                *item.phase.lock().unwrap() = DownloadPhase::Writing;
            }
        }
        self.shared.waker.wake();
    }

    /// Cancels a download and removes its `.part` file.
    pub fn cancel(&self, id: u64) {
        for item in self.items() {
            if item.id == id {
                item.cancelled.store(true, Ordering::SeqCst);
                item.paused.store(false, Ordering::SeqCst);
                *item.phase.lock().unwrap() = DownloadPhase::Cancelled;
                let _ = std::fs::remove_file(part_path(&item.path));
            }
        }
        self.shared.waker.wake();
    }

    /// Removes a finished/failed/cancelled entry from the list.
    pub fn remove(&self, id: u64) {
        self.shared.items.lock().unwrap().retain(|i| i.id != id);
        self.shared.waker.wake();
    }

    /// Restarts a failed/cancelled download from its `.part` offset.
    pub fn restart(&self, id: u64) {
        for item in self.items() {
            if item.id == id {
                item.cancelled.store(false, Ordering::SeqCst);
                item.paused.store(false, Ordering::SeqCst);
                *item.error.lock().unwrap() = String::new();
                *item.phase.lock().unwrap() = DownloadPhase::Fetching;
                let shared = Arc::clone(&self.shared);
                let item_for_job = Arc::clone(&item);
                self.shared.runtime.spawn(async move {
                    run_download(shared, item_for_job).await;
                });
            }
        }
        self.shared.waker.wake();
    }
}

fn part_path(path: &std::path::Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

/// The async phase of a download: fetch via the engine networking stack,
/// then hand the body to a writer thread that honors pause/cancel.
async fn run_download(shared: Arc<Shared>, item: Arc<DownloadItem>) {
    // Range resume: continue from the existing .part file, when any.
    let part = part_path(&item.path);
    let offset = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let mut request = FetchRequest {
        url: item.url.clone(),
        ..FetchRequest::default()
    };
    if offset > 0 {
        request
            .headers
            .push(("Range".into(), format!("bytes={offset}-")));
    }

    let response = match fetch(&shared.net, request).await {
        Ok(response) => response,
        Err(err) => {
            *item.error.lock().unwrap() = err.to_string();
            *item.phase.lock().unwrap() = DownloadPhase::Failed;
            shared.waker.wake();
            return;
        }
    };
    if item.cancelled.load(Ordering::SeqCst) {
        return;
    }

    let partial = response.status == 206;
    if !response.is_success() {
        *item.error.lock().unwrap() = format!("HTTP {}", response.status);
        *item.phase.lock().unwrap() = DownloadPhase::Failed;
        shared.waker.wake();
        return;
    }

    let total = response
        .header("content-range")
        .and_then(|cr| cr.split('/').nth(1))
        .and_then(|n| n.parse::<u64>().ok())
        .or_else(|| {
            response
                .header("content-length")
                .and_then(|n| n.parse::<u64>().ok())
                .map(|len| len + if partial { offset } else { 0 })
        });
    *item.total.lock().unwrap() = total;
    *item.mime.lock().unwrap() = response.content_type().to_owned();

    // Network phase done → writer thread with real byte progress.
    *item.phase.lock().unwrap() = DownloadPhase::Writing;
    let start_offset = if partial { offset } else { 0 };
    {
        let mut received = item.received.lock().unwrap();
        *received = start_offset;
    }
    let body = response.body.to_vec();
    let item_writer = Arc::clone(&item);
    let waker = Arc::clone(&shared.waker);
    std::thread::Builder::new()
        .name(format!("rowser-dl-{}", item.id))
        .spawn(move || write_body(item_writer, body, start_offset, waker))
        .ok();
    shared.waker.wake();
}

/// Writes the body in 1 MiB chunks, honoring pause (parks on a condvar) and
/// cancel (abandons the `.part` file).
fn write_body(item: Arc<DownloadItem>, body: Vec<u8>, offset: u64, waker: Arc<Waker>) {
    let pair = Arc::new((Mutex::new(()), Condvar::new()));
    let part = part_path(&item.path);
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(offset == 0)
        .open(&part)
    {
        Ok(file) => file,
        Err(err) => {
            *item.error.lock().unwrap() = err.to_string();
            *item.phase.lock().unwrap() = DownloadPhase::Failed;
            waker.wake();
            return;
        }
    };
    use std::io::{Seek, Write};
    if offset > 0 {
        if let Err(err) = file.seek(std::io::SeekFrom::Start(offset)) {
            *item.error.lock().unwrap() = err.to_string();
            *item.phase.lock().unwrap() = DownloadPhase::Failed;
            waker.wake();
            return;
        }
    }
    const CHUNK: usize = 1024 * 1024;
    let mut written = offset;
    let mut cursor = 0usize;
    while cursor < body.len() {
        // Pause: park until resumed or cancelled.
        while item.paused.load(Ordering::SeqCst)
            && !item.cancelled.load(Ordering::SeqCst)
            && *item.phase.lock().unwrap() == DownloadPhase::Paused
        {
            let (lock, cond) = &*pair;
            let _unused =
                cond.wait_timeout(lock.lock().unwrap(), std::time::Duration::from_millis(200));
        }
        if item.cancelled.load(Ordering::SeqCst) {
            return;
        }
        let chunk_start = cursor;
        let end = (cursor + CHUNK).min(body.len());
        if let Err(err) = file.write_all(&body[cursor..end]) {
            *item.error.lock().unwrap() = err.to_string();
            *item.phase.lock().unwrap() = DownloadPhase::Failed;
            waker.wake();
            return;
        }
        cursor = end;
        written += (end - chunk_start) as u64;
        {
            let mut received = item.received.lock().unwrap();
            *received = written;
        }
        waker.wake();
    }
    let _ = file.sync_all();
    if item.cancelled.load(Ordering::SeqCst) {
        return;
    }
    match std::fs::rename(&part, &item.path) {
        Ok(()) => {
            *item.phase.lock().unwrap() = DownloadPhase::Done;
            *item.received.lock().unwrap() = written;
        }
        Err(err) => {
            *item.error.lock().unwrap() = err.to_string();
            *item.phase.lock().unwrap() = DownloadPhase::Failed;
        }
    }
    waker.wake();
}

fn filename_for(url: &str, id: u64) -> String {
    let name = url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut segs| segs.next_back().map(|s| s.to_owned()))
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("download-{id}"));
    let decoded = percent_decode(name);
    let safe: String = decoded
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    safe.chars().take(80).collect()
}

fn percent_decode(text: String) -> String {
    percent_encoding::percent_decode_str(&text)
        .decode_utf8()
        .map(|cow| cow.into_owned())
        .unwrap_or(text)
}

fn unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| e.to_os_string())
        .unwrap_or_default();
    for i in 1..1000 {
        let mut candidate = path.clone();
        candidate.set_file_name(format!(
            "{} ({}).{}",
            stem.to_string_lossy(),
            i,
            ext.to_string_lossy()
        ));
        if !candidate.exists() {
            return candidate;
        }
    }
    path
}

fn kind_for(url: &str) -> &'static str {
    let lowered = url.to_lowercase();
    if lowered.ends_with(".pdf") {
        "document"
    } else if [".zip", ".tar", ".gz", ".7z", ".rar"]
        .iter()
        .any(|e| lowered.ends_with(e))
    {
        "archive"
    } else if [".mp4", ".webm", ".mkv", ".avi", ".mov"]
        .iter()
        .any(|e| lowered.ends_with(e))
    {
        "video"
    } else if [".mp3", ".ogg", ".wav", ".flac"]
        .iter()
        .any(|e| lowered.ends_with(e))
    {
        "audio"
    } else if [".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg"]
        .iter()
        .any(|e| lowered.ends_with(e))
    {
        "image"
    } else {
        "file"
    }
}
