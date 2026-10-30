//! A local HTTP server for provider tests: answers each request with the
//! next scripted reply and records what it was sent.

use std::sync::{Arc, Mutex};

use serde_json::Value;

/// A scripted reply: status, extra header lines (`name: value\r\n`), body.
pub(crate) type Reply = (u16, &'static str, String);

/// One request as the server saw it.
#[derive(Debug, Clone)]
pub(crate) struct Captured {
    pub method: String,
    /// Path and query, as sent.
    pub target: String,
    /// Header names lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Value,
    pub raw_body: String,
}

impl Captured {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Serve `script` on 127.0.0.1; returns `http://127.0.0.1:<port>` and the
/// requests seen so far.
pub(crate) async fn serve(script: Vec<Reply>) -> (String, Arc<Mutex<Vec<Captured>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        for (status, extra, body) in script {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut req = Vec::new();
            let mut buf = [0u8; 16384];
            let mut head_end = None;
            loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&req).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if req.len() >= end + 4 + len {
                        head_end = Some(end);
                        break;
                    }
                }
            }
            if let Some(end) = head_end {
                let text = String::from_utf8_lossy(&req[..end]).to_string();
                let mut lines = text.lines();
                let first = lines.next().unwrap_or("");
                let mut parts = first.split(' ');
                let method = parts.next().unwrap_or("").to_string();
                let target = parts.next().unwrap_or("").to_string();
                let headers = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let raw_body = String::from_utf8_lossy(&req[end + 4..]).to_string();
                let body = serde_json::from_str(&raw_body).unwrap_or(Value::Null);
                log.lock().unwrap().push(Captured {
                    method,
                    target,
                    headers,
                    body,
                    raw_body,
                });
            }
            let resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (origin, seen)
}
