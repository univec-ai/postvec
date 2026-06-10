//! A minimal single-purpose mock HTTP/1.1 server for tests.
//!
//! Extracted from the integration tests so the gateway's unit tests and the
//! inference hosts' test suites (feature `test-util`) can all point a real
//! client at one in-process server.
//!
//! It accepts one request per connection (responses set `Connection: close`,
//! so reqwest never reuses a socket), records the request body, and replies
//! with the next canned `(status, body)` from the queue — repeating the last
//! entry once the queue is exhausted. That repeat-last behaviour is what lets
//! a retry test serve `503` then `200` and have every subsequent attempt see
//! `200`.

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub struct Mock {
    /// Base URL (e.g. `http://127.0.0.1:54321`) to hand to a client.
    pub url: String,
    /// Bodies of the requests the server has received, in arrival order.
    pub requests: Arc<Mutex<Vec<String>>>,
    /// `METHOD /path` of every request, in arrival order ([`routes`] only).
    pub paths: Arc<Mutex<Vec<String>>>,
    /// Body bytes written by [`flood`], for asserting where a bounded reader
    /// gave up. `None` for every other server shape.
    flooded: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

impl Mock {
    /// The single most-recent request body. Panics if none were received.
    pub fn last_request(&self) -> String {
        self.requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("expected at least one request")
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// How many requests hit `path` ([`routes`] servers only).
    pub fn path_count(&self, path: &str) -> usize {
        self.paths
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.split_whitespace().nth(1) == Some(path))
            .count()
    }

    /// Body bytes a [`flood`] server managed to write before the client
    /// stopped reading.
    pub fn flooded_bytes(&self) -> usize {
        self.flooded
            .as_ref()
            .map(|n| n.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn content_length(headers: &[u8]) -> usize {
    let text = String::from_utf8_lossy(headers);
    for line in text.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                return value.trim().parse().unwrap_or(0);
            }
        }
    }
    0
}

/// Spawn a mock server that replies with `responses` in order (last entry
/// repeated). Returns immediately once the listener is bound. Must be called
/// from within a tokio runtime (the accept loop is a spawned task).
pub async fn spawn(responses: Vec<(u16, String)>) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}", addr);
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let reqs = Arc::clone(&requests);

    tokio::spawn(async move {
        let mut idx = 0usize;
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            // Read the full request (headers + body) so the client can finish
            // writing before we reply — otherwise it may see a broken pipe.
            let mut buf = [0u8; 8192];
            let mut data = Vec::new();
            loop {
                let n = match socket.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                data.extend_from_slice(&buf[..n]);
                if let Some(pos) = find(&data, b"\r\n\r\n") {
                    let need = content_length(&data[..pos]);
                    if data.len() - (pos + 4) >= need {
                        break;
                    }
                }
            }

            let body = match find(&data, b"\r\n\r\n") {
                Some(pos) => String::from_utf8_lossy(&data[pos + 4..]).to_string(),
                None => String::new(),
            };
            reqs.lock().unwrap().push(body);

            let (status, payload) = responses[idx.min(responses.len() - 1)].clone();
            idx += 1;
            let resp = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });

    Mock {
        url,
        requests,
        paths: Arc::new(Mutex::new(Vec::new())),
        flooded: None,
    }
}

/// Convenience: a server that returns the same `(status, body)` every time.
pub async fn always(status: u16, body: &str) -> Mock {
    spawn(vec![(status, body.to_string())]).await
}

/// A server that answers 200 and then streams **chunked** body forever,
/// declaring no `Content-Length`, until the client hangs up.
///
/// This is the shape a `Content-Length` check cannot defend against, and the
/// reason the connectors read through a bounded reader rather than
/// `bytes()`/`text()`: in embedded mode the allocation this would otherwise
/// make is the PostgreSQL launcher's RSS. `written` reports how many body
/// bytes the peer accepted before giving up, so a test can assert the client
/// stopped near its budget instead of at the peer's convenience.
pub async fn flood(status: u16) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}", addr);
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let reqs = Arc::clone(&requests);
    let written = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&written);

    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let mut buf = [0u8; 8192];
            let mut data = Vec::new();
            loop {
                let n = match socket.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                data.extend_from_slice(&buf[..n]);
                if let Some(pos) = find(&data, b"\r\n\r\n") {
                    let need = content_length(&data[..pos]);
                    if data.len() - (pos + 4) >= need {
                        break;
                    }
                }
            }
            reqs.lock().unwrap().push(match find(&data, b"\r\n\r\n") {
                Some(pos) => String::from_utf8_lossy(&data[pos + 4..]).to_string(),
                None => String::new(),
            });

            let head = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n\
                 Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            );
            if socket.write_all(head.as_bytes()).await.is_err() {
                continue;
            }
            // 64 KiB of `x` per chunk, forever.
            let payload = "x".repeat(64 * 1024);
            let chunk = format!("{:x}\r\n{payload}\r\n", payload.len());
            loop {
                if socket.write_all(chunk.as_bytes()).await.is_err() {
                    break;
                }
                counter.fetch_add(payload.len(), std::sync::atomic::Ordering::Relaxed);
                // Do not let a runaway peer starve the test runtime.
                if counter.load(std::sync::atomic::Ordering::Relaxed) > 512 * 1024 * 1024 {
                    break;
                }
            }
        }
    });

    Mock {
        url,
        requests,
        paths: Arc::new(Mutex::new(Vec::new())),
        flooded: Some(written),
    }
}

/// A server that promises more body than it sends and then closes the
/// socket: a 200 whose body arrives truncated, which is what a reset
/// connection looks like to the client.
///
/// This is the failure the clients must NOT report as a decode failure.
/// reqwest gives an incomplete body the same error kind as unparseable JSON,
/// so a client that used `response.json()` would make the two
/// indistinguishable and the gateway would dead-letter a transient outage.
pub async fn truncated(body: &str) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}", addr);
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let reqs = Arc::clone(&requests);
    let payload = body.to_string();

    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let mut buf = [0u8; 8192];
            let mut data = Vec::new();
            loop {
                let n = match socket.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                data.extend_from_slice(&buf[..n]);
                if let Some(pos) = find(&data, b"\r\n\r\n") {
                    let need = content_length(&data[..pos]);
                    if data.len() - (pos + 4) >= need {
                        break;
                    }
                }
            }
            reqs.lock().unwrap().push(match find(&data, b"\r\n\r\n") {
                Some(pos) => String::from_utf8_lossy(&data[pos + 4..]).to_string(),
                None => String::new(),
            });

            // Announce 64 bytes more than we are going to write, then hang up.
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len() + 64
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.flush().await;
            drop(socket);
        }
    });

    Mock {
        url,
        requests,
        paths: Arc::new(Mutex::new(Vec::new())),
        flooded: None,
    }
}

/// One canned answer for a `(method, path)`, optionally gated on a bearer.
#[derive(Clone)]
pub struct Route {
    pub method: &'static str,
    pub path: &'static str,
    pub status: u16,
    pub body: String,
    /// `Some(key)`: answer 401 unless `Authorization: Bearer key` is sent.
    pub bearer: Option<String>,
}

impl Route {
    pub fn new(method: &'static str, path: &'static str, status: u16, body: &str) -> Self {
        Self {
            method,
            path,
            status,
            body: body.to_string(),
            bearer: None,
        }
    }
}

/// A server that dispatches on method + path (404 otherwise), records the
/// request line of every request in [`Mock::paths`], and the body in
/// [`Mock::requests`]. For tests that drive several routes of one API in
/// one process — the canned queue above cannot tell them apart.
pub async fn routes(routes: Vec<Route>) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}", addr);
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let paths = Arc::new(Mutex::new(Vec::<String>::new()));
    let (reqs, seen) = (Arc::clone(&requests), Arc::clone(&paths));

    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let mut buf = [0u8; 8192];
            let mut data = Vec::new();
            loop {
                let n = match socket.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                data.extend_from_slice(&buf[..n]);
                if let Some(pos) = find(&data, b"\r\n\r\n") {
                    if data.len() - (pos + 4) >= content_length(&data[..pos]) {
                        break;
                    }
                }
            }
            let (head, body) = match find(&data, b"\r\n\r\n") {
                Some(pos) => (
                    String::from_utf8_lossy(&data[..pos]).to_string(),
                    String::from_utf8_lossy(&data[pos + 4..]).to_string(),
                ),
                None => (String::new(), String::new()),
            };
            let mut words = head.split_whitespace();
            let (method, path) = (
                words.next().unwrap_or("").to_string(),
                words.next().unwrap_or("").to_string(),
            );
            let authorized = |route: &Route| {
                route.bearer.as_ref().is_none_or(|key| {
                    head.lines().any(|l| {
                        l.trim()
                            .eq_ignore_ascii_case(&format!("authorization: bearer {key}"))
                    })
                })
            };
            seen.lock().unwrap().push(format!("{method} {path}"));
            reqs.lock().unwrap().push(body);
            let (status, payload) =
                match routes.iter().find(|r| r.method == method && r.path == path) {
                    Some(route) if authorized(route) => (route.status, route.body.clone()),
                    Some(_) => (
                        401,
                        r#"{"error":{"message":"invalid api key"}}"#.to_string(),
                    ),
                    None => (404, r#"{"error":{"message":"no such path"}}"#.to_string()),
                };
            let resp = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });

    Mock {
        url,
        requests,
        paths,
        flooded: None,
    }
}
