//! css_probe.rs — parse a real-world stylesheet and count rules.
use rowser_parsing::css::{parse_stylesheet, MediaContext};

fn main() {
    let url = std::env::args().nth(1).expect("usage: css_probe <url-or-file>");
    let css = if url.starts_with("http") {
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Rrowser/1.0 Chrome/140.0.0.0 Safari/537.36";
        let resp = ureq::get(&url).set("User-Agent", ua).set("Accept", "text/css,*/*;q=0.1").call();
        match resp {
            Ok(r) => r.into_string().unwrap_or_default(),
            Err(e) => { eprintln!("fetch failed: {e}"); return; }
        }
    } else {
        std::fs::read_to_string(&url).expect("read file")
    };
    println!("css bytes: {}", css.len());
    let media = MediaContext { width: 1360.0, height: 724.0, dark_mode: false };
    let sheet = parse_stylesheet(&css, &media);
    println!("parsed rules: {}", sheet.rules.len());
    println!("  rule count OK");
}
