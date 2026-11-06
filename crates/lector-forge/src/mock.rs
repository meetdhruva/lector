//! A minimal HTTP/1.1 server over `std::net` that answers recorded JSON: tests of the
//! forge adapters and offline screenshots (`cargo run -p lector-forge --example
//! mock_forge -- <fixtures dir> <port>`). Never used against real hosts.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde::Deserialize;

/// A canned response.
#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    /// HTTP method.
    pub method: String,
    /// Path; with a `?` it must match the query too, without one any query matches.
    pub path: String,
    /// Only when the request body contains this (GraphQL operations).
    #[serde(default)]
    pub body_contains: Option<String>,
    /// Status code (200 by default).
    #[serde(default = "ok")]
    pub status: u16,
    /// Response body.
    #[serde(default)]
    pub body: String,
    /// File (relative to the fixtures dir) to read the body from.
    #[serde(default)]
    pub file: Option<String>,
    /// Extra headers.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

fn ok() -> u16 {
    200
}

impl Route {
    /// `method path` → `status` with a JSON body.
    pub fn json(method: &str, path: &str, body: impl Into<String>) -> Route {
        Route {
            method: method.into(),
            path: path.into(),
            body_contains: None,
            status: 200,
            body: body.into(),
            file: None,
            headers: Vec::new(),
        }
    }

    /// Sets the status.
    pub fn status(mut self, status: u16) -> Route {
        self.status = status;
        self
    }

    /// Adds a header.
    pub fn header(mut self, name: &str, value: &str) -> Route {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Requires the body to contain `needle`.
    pub fn when_body(mut self, needle: &str) -> Route {
        self.body_contains = Some(needle.into());
        self
    }

    fn matches(&self, method: &str, target: &str, body: &str) -> bool {
        if !self.method.eq_ignore_ascii_case(method) {
            return false;
        }
        let path_ok = if self.path.contains('?') {
            self.path == target
        } else {
            target.split('?').next() == Some(self.path.as_str())
        };
        path_ok
            && self
                .body_contains
                .as_ref()
                .is_none_or(|n| body.contains(n.as_str()))
    }
}

/// A request the server received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// Method.
    pub method: String,
    /// Path with query.
    pub target: String,
    /// Lowercase header names with values.
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: String,
}

impl Recorded {
    /// A header value.
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The running server; stops when dropped.
pub struct MockServer {
    addr: SocketAddr,
    routes: Arc<Mutex<Vec<Route>>>,
    requests: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MockServer {
    /// Starts on `127.0.0.1:port` (0 = any free port).
    pub fn start(port: u16, routes: Vec<Route>) -> std::io::Result<MockServer> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let addr = listener.local_addr()?;
        let routes = Arc::new(Mutex::new(routes));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (r, q, s) = (routes.clone(), requests.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if s.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (r, q) = (r.clone(), q.clone());
                std::thread::spawn(move || {
                    if let Err(err) = serve(stream, &r, &q) {
                        tracing::debug!("mock forge: {err}");
                    }
                });
            }
        });
        Ok(MockServer {
            addr,
            routes,
            requests,
            stop,
            thread: Some(thread),
        })
    }

    /// Starts with the routes of `<dir>/routes.json` (bodies from `file`s in `dir`).
    pub fn from_dir(dir: &Path, port: u16) -> std::io::Result<MockServer> {
        Self::start(port, load_routes(dir)?)
    }

    /// `http://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Adds a route (checked before earlier ones).
    pub fn add(&self, route: Route) {
        if let Ok(mut r) = self.routes.lock() {
            r.insert(0, route);
        }
    }

    /// Requests received so far.
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Reads `<dir>/routes.json`.
pub fn load_routes(dir: &Path) -> std::io::Result<Vec<Route>> {
    let text = std::fs::read_to_string(dir.join("routes.json"))?;
    let mut routes: Vec<Route> = serde_json::from_str(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    for r in &mut routes {
        if let Some(f) = &r.file {
            r.body = std::fs::read_to_string(dir.join(f))?;
        }
    }
    Ok(routes)
}

fn serve(
    stream: TcpStream,
    routes: &Mutex<Vec<Route>>,
    requests: &Mutex<Vec<Recorded>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    if method.is_empty() {
        return Ok(());
    }
    let mut headers = Vec::new();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim().to_string();
            if k == "content-length" {
                len = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }
    let mut body = vec![0; len.min(16 * 1024 * 1024)];
    reader.read_exact(&mut body)?;
    let body = String::from_utf8_lossy(&body).into_owned();
    let route = routes.lock().ok().and_then(|r| {
        r.iter()
            .find(|r| r.matches(&method, &target, &body))
            .cloned()
    });
    if let Ok(mut q) = requests.lock() {
        q.push(Recorded {
            method,
            target: target.clone(),
            headers,
            body,
        });
    }
    let route = route.unwrap_or_else(|| {
        Route::json(
            "GET",
            &target,
            format!("{{\"message\":\"no mock route for {target}\"}}"),
        )
        .status(404)
    });
    let mut out = stream;
    let mut head = format!(
        "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        route.status,
        route.body.len()
    );
    for (k, v) in &route.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes())?;
    out.write_all(route.body.as_bytes())?;
    out.flush()
}
