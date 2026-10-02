//! Fetches a YouTube watch page twice through the full browser-style
//! pipeline (privacy settings, cookies) to diagnose stub responses.
use rowser_networking::{build_context, fetch, FetchRequest, ResourceKind};
use rowser_storage::Storage;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let dir = std::env::temp_dir().join("rowser-yt-probe");
    let _ = std::fs::create_dir_all(&dir);
    let storage = Storage::open(dir.join("probe.redb")).expect("storage");
    let ctx = build_context(
        Arc::new(storage),
        rowser_privacy::PrivacySettings::default(),
        Default::default(),
        Default::default(),
        rowser_networking::ClientIdentity::default(),
    )
    .await
    .expect("context");
    for round in 1..=3 {
        let request = FetchRequest {
            url: "https://www.youtube.com/watch?v=jNQXAC9IVRw".into(),
            resource_type: ResourceKind::Document,
            ..FetchRequest::default()
        };
        match fetch(&ctx, request).await {
            Ok(response) => println!(
                "round {round}: status={} transport={} url={} bytes={}",
                response.status,
                response.transport,
                response.url,
                response.body.len()
            ),
            Err(err) => println!("round {round}: FAILED: {err:?}"),
        }
    }
}
