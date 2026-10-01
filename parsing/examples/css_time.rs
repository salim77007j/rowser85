//! Times parse_stylesheet on real-world CSS files to locate the parser hang.
use std::time::Instant;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        // default: all css-*.css in /tmp/css-repro
        args = std::fs::read_dir("/tmp/css-repro")
            .expect("dir")
            .filter_map(|e| {
                let p = e.expect("e").path();
                if p.extension().map(|x| x == "css").unwrap_or(false) {
                    Some(p.to_string_lossy().into_owned())
                } else {
                    None
                }
            })
            .collect();
        args.sort();
    }
    for path in args {
        let css = std::fs::read_to_string(&path).expect("read");
        let media = rowser_parsing::css::MediaContext {
            width: 1360.0,
            height: 860.0,
            dark_mode: false,
        };
        println!(
            "parsing {} ({} bytes, {} lines)…",
            path,
            css.len(),
            css.lines().count()
        );
        let t0 = Instant::now();
        let sheet = rowser_parsing::parse_stylesheet(&css, &media);
        println!(
            "  -> {} rules in {}ms",
            sheet.rules.len(),
            t0.elapsed().as_millis()
        );
    }
}
