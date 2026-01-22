//! Test-only helpers.
//!
//! A hand-rolled HTTP fixture instead of a mock-server dependency: the probes
//! under test only need "a real socket that answers a canned body for a path",
//! and keeping the dependency set to crates already in the workspace lockfile
//! matters more here than convenience.

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct HttpFixture {
    address: std::net::SocketAddr,
    /// Dropping the fixture stops the accept loop.
    _task: tokio::task::JoinHandle<()>,
}

impl HttpFixture {
    /// Start a server that answers every request by calling `handler` with the
    /// request path.
    pub async fn start<F>(handler: F) -> Self
    where
        F: Fn(&str) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture");
        let address = listener.local_addr().expect("fixture address");
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    let read = match stream.read(&mut buffer).await {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    let (status, body) = handler(&path);
                    let response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\
                         Content-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        Self {
            address,
            _task: task,
        }
    }

    pub fn address(&self) -> String {
        self.address.to_string()
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self._task.abort();
    }
}
