//! Shared helpers for the integration test suite.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Route table: path → (status, content_type, body).
type Routes = Arc<HashMap<String, (u16, String, Vec<u8>)>>;

/// A tiny local HTTP/1.1 server for engine tests.
///
/// The accept loop terminates when [`LocalServer`] is dropped, so tests
/// never leak the server thread (a bare `incoming()` loop would block
/// `Drop`'s join forever).
pub struct LocalServer {
    listener: TcpListener,
    routes: Routes,
    hits: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LocalServer {
    /// Starts the server; routes map path → (status, content_type, body).
    pub fn start(routes: HashMap<String, (u16, String, Vec<u8>)>) -> LocalServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        LocalServer {
            listener,
            routes: Arc::new(routes),
            hits: Arc::new(AtomicU64::new(0)),
            running: Arc::new(AtomicBool::new(true)),
            thread: None,
        }
    }

    /// The server base URL.
    pub fn url(&self) -> String {
        format!(
            "http://127.0.0.1:{}",
            self.listener.local_addr().unwrap().port()
        )
    }

    /// Total requests served.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Serves until dropped.
    pub fn serve(&mut self) {
        let listener = self.listener.try_clone().expect("clone listener");
        let routes = Arc::clone(&self.routes);
        let hits = Arc::clone(&self.hits);
        let running = Arc::clone(&self.running);
        self.thread = Some(
            std::thread::Builder::new()
                .name("test-http-server".into())
                .spawn(move || {
                    for stream in listener.incoming() {
                        if !running.load(Ordering::Relaxed) {
                            break;
                        }
                        let Ok(mut stream) = stream else { continue };
                        hits.fetch_add(1, Ordering::Relaxed);
                        let Some((status, ctype, body)) = serve_one(&mut stream, &routes) else {
                            continue;
                        };
                        write_response(&mut stream, status, &ctype, &body);
                    }
                })
                .expect("server thread"),
        );
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        // Stop the accept loop: flip the flag, then wake the accept() call
        // with a self-connection so the thread observes the flag and exits.
        self.running.store(false, Ordering::Relaxed);
        let _ = std::net::TcpStream::connect(self.listener.local_addr().unwrap());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_one(stream: &mut TcpStream, routes: &Routes) -> Option<(u16, String, Vec<u8>)> {
    let mut buffer = [0u8; 8192];
    let mut read = 0usize;
    // Read until end of headers.
    loop {
        let n = stream.read(&mut buffer[read..]).ok()?;
        if n == 0 {
            break;
        }
        read += n;
        if buffer[..read].windows(4).any(|w| w == b"\r\n\r\n") || read >= buffer.len() {
            break;
        }
    }
    let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
    let path = request
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_owned();
    routes
        .get(&path)
        .cloned()
        .or_else(|| Some((404, "text/plain".into(), b"not found".to_vec())))
}

fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Status",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}
