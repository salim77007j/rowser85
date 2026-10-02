// css_deep_probe.rs — parse a CSS file, report rule count, selector samples,
// and declaration survival. Diagnoses wholesale rule drops (var(), @layer,
// nesting, light-dark, etc).
use rowser_parsing::css::{parse_stylesheet, MediaContext};

fn main() {
    let media = MediaContext {
        width: 1360.0,
        height: 860.0,
        dark_mode: false,
    };
    for path in std::env::args().skip(1) {
        let css = std::fs::read_to_string(&path).unwrap_or_default();
        let sheet = parse_stylesheet(&css, &media);
        let total = sheet.rules.len();
        let bytes = css.len();
        // Sample selectors
        let mut sels: Vec<String> = Vec::new();
        for r in sheet.rules.iter() {
            if sels.len() < 12 {
                sels.push(r.selector_text.clone());
            }
        }
        println!(
            "== {path} ({bytes}B) → {total} rules across {} buckets",
            sheet.rules.len()
        );
        for s in sels {
            println!("   {s}");
        }
        // Feature census on the raw text
        for feat in [
            "--",
            "var(",
            "@layer",
            "@media",
            "@supports",
            "light-dark(",
            ":is(",
            ":where(",
            "clamp(",
            "color-mix(",
            "oklch(",
        ] {
            let n = css.matches(feat).count();
            if n > 0 {
                println!("   feature {feat}: {n} occurrences");
            }
        }
    }
}
