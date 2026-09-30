//! Shared helpers for the integration test suite.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A tiny local HTTP/1.1 server for engine tests.
pub struct LocalServer {
    listener: TcpListener,
    routes: Arc<HashMap<String, (u16, String, Vec<u8>)>>,
    hits: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LocalServer {
    /// Starts the server; routes map path → (status, content_type, body).
    pub fn start(routes: HashMap<String, (u16, String, Vec<u8>)>) -> LocalServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let routes = Arc::new(routes);
        let hits = Arc::new(AtomicU64::new(0));
        let server = LocalServer {
            listener,
            routes: Arc::clone(&routes),
            hits: Arc::clone(&hits),
            thread: None,
        };
        server
    }

    /// The server base URL.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.listener.local_addr().unwrap().port())
    }

    /// Total requests served.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Serves until the process exits (call from a test thread).
    pub fn serve(&mut self) {
        let listener = self.listener.try_clone().expect("clone listener");
        let routes = Arc::clone(&self.routes);
        let hits = Arc::clone(&self.hits);
        self.thread = Some(
            std::thread::Builder::new()
                .name("test-http-server".into())
                .spawn(move || {
                    for stream in listener.incoming() {
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
        // Drop the accept loop by connecting once (the thread ends when the
        // listener is exhausted; tests are short-lived anyway).
        let _ = std::net::TcpStream::connect(self.listener.local_addr().unwrap());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_one(
    stream: &mut TcpStream,
    routes: &HashMap<String, (u16, String, Vec<u8>)>,
) -> Option<(u16, String, Vec<u8>)> {
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
