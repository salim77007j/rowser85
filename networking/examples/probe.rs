//! probe.rs — fetch real sites through the engine's own networking stack and
//! print status/transport/error so we see exactly what the browser sees.
use std::sync::Arc;
use std::time::Duration;

use rowser_networking::{build_context, fetch, FetchRequest, H3Settings};
use rowser_privacy::PrivacySettings;
use rowser_storage::Storage;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let storage = Arc::new(Storage::open("/tmp/probe-store.db").expect("storage"));
    let settings = PrivacySettings::default();
    let ctx = build_context(
        Arc::clone(&storage),
        settings,
        Default::default(),
        H3Settings::default(),
        rowser_networking::ClientIdentity::default(),
    )
    .await
    .expect("context");

    let sites = [
        "https://example.com",
        "https://en.wikipedia.org/wiki/Main_Page",
        "https://www.google.com/search?q=rust+programming",
        "https://youtube.com",
        "https://www.youtube.com",
        "https://duckduckgo.com/html/?q=rust",
        "https://news.ycombinator.com",
        "https://github.com/",
    ];
    for site in sites {
        let req = FetchRequest {
            url: site.to_owned(),
            headers: vec![
                ("user-agent".into(), "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Rrowser/1.0 Chrome/140.0.0.0 Safari/537.36".into()),
                ("accept".into(), "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8".into()),
                ("accept-language".into(), "en-US,en;q=0.9".into()),
            ],
            ..FetchRequest::default()
        };
        let t0 = std::time::Instant::now();
        match tokio::time::timeout(Duration::from_secs(20), fetch(&ctx, req)).await {
            Ok(Ok(resp)) => {
                let ct = resp
                    .header("content-type")
                    .unwrap_or("")
                    .chars()
                    .take(40)
                    .collect::<String>();
                let loc = resp.header("location").unwrap_or("").to_owned();
                println!(
                    "{:50} -> {} {} {}B {}ms ct={}",
                    site,
                    resp.status,
                    resp.transport,
                    resp.body.len(),
                    t0.elapsed().as_millis(),
                    ct
                );
                if !loc.is_empty() {
                    println!("{:50}    location: {}", "", loc);
                }
                // show first header set for debugging
                let hs: Vec<String> = resp.headers.iter().take(0).map(|_| String::new()).collect();
                let _ = hs;
            }
            Ok(Err(e)) => println!(
                "{:50} -> ERROR {} ({}ms)",
                site,
                e,
                t0.elapsed().as_millis()
            ),
            Err(_) => println!("{:50} -> TIMEOUT 20s", site),
        }
    }
}
