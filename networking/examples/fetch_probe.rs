//! Probes the engine's fetch pipeline against a real URL.
use rowser_networking::{build_context, fetch, FetchRequest};
use rowser_storage::Storage;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://example.com".into());
    let dir = std::env::temp_dir().join("rowser-fetch-probe");
    let _ = std::fs::create_dir_all(&dir);
    let storage = Storage::open(dir.join("probe.redb")).expect("storage");
    let ctx = build_context(
        Arc::new(storage),
        rowser_privacy::PrivacySettings::default(),
        Default::default(),
        Default::default(),
    )
    .await
    .expect("context");
    println!("fetching {url} …");
    let request = FetchRequest {
        url: url.clone(),
        ..FetchRequest::default()
    };
    let start = std::time::Instant::now();
    match fetch(&ctx, request).await {
        Ok(response) => println!(
            "OK: status={} transport={} url={} bytes={} in {:?}",
            response.status,
            response.transport,
            response.url,
            response.body.len(),
            start.elapsed()
        ),
        Err(err) => {
            println!("FAILED after {:?}: {err:?}", start.elapsed());
            let mut source = std::error::Error::source(&err);
            let mut depth = 1;
            while let Some(s) = source {
                println!("  {depth}: {s}");
                source = s.source();
                depth += 1;
            }
        }
    }
}
