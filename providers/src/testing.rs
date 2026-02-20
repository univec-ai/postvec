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

    Mock { url, requests }
}

/// Convenience: a server that returns the same `(status, body)` every time.
pub async fn always(status: u16, body: &str) -> Mock {
    spawn(vec![(status, body.to_string())]).await
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

    Mock { url, requests }
}
