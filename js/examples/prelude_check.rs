//! prelude_check.rs — evaluate the prelude in isolation to find the hang.
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Instant;

use rowser_dom::Dom;
use rowser_js::{JsConfig, PageBridge};

fn main() {
    let dom = Rc::new(std::cell::RefCell::new(Dom::new()));
    let (tx, _rx) = mpsc::channel();
    let (rects, viewport) = rowser_js::PageBridge::empty_mirrors();
    let bridge = PageBridge {
        dom: Rc::clone(&dom),
        document: 0,
        body: 0,
        html: 0,
        url: "https://example.com/".to_owned(),
        origin: "https://example.com".to_owned(),
        storage: None,
        spoof: rowser_privacy::fingerprint::SpoofProfile::from_seed([1u8; 32]),
        outgoing: Some(tx),
        media_mirror: std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new())),
        rects,
        viewport,
        mo: Default::default(),
        history: Default::default(),
    };
    let t0 = Instant::now();
    println!("creating runtime (evals prelude)…");
    match rowser_js::JsRuntime::new(JsConfig::default(), bridge) {
        Ok(_js) => println!("prelude OK in {}ms", t0.elapsed().as_millis()),
        Err(e) => println!("prelude FAILED after {}ms: {e}", t0.elapsed().as_millis()),
    }
    let _ = _rx;
}
