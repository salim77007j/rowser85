//! Shell-level session save probe.
fn main() {
    let dir = std::env::temp_dir().join("rowser-shell-session-probe");
    let _ = std::fs::remove_dir_all(&dir);
    let mut shell = rowser_shell::Shell::start(&dir).expect("shell");
    let tab = shell.browser().new_tab(Some("https://example.com".into()));
    let _ = tab;
    // Pump events like the UI does.
    for _ in 0..40 {
        let _ = shell.poll_events();
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    shell.save_session();
    let session = std::fs::read_to_string(dir.join("session.json")).unwrap_or_default();
    println!("session.json: {session}");
    println!("engine tabs: {:?}", shell.browser().tabs());
    let t = shell.browser().tabs()[0];
    if let Some(s) = shell.browser().snapshot(t) {
        println!("snapshot url={:?} title={:?}", s.url, s.title);
    }
    shell.shutdown();
}
