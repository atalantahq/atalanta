// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Newline-delimited JSON client shared by the headless bridges (the Flutter
//! bridge's in-app Dart server and the React Native sidecar).
//!
//! - Every request carries an `id`. A reply with a different `id` is the late
//!   answer to an abandoned request and is skipped; a reply without one (an
//!   old server) pairs by order.
//! - Writing the request and reading its reply share one deadline.
//! - A reply line is capped at [`MAX_REPLY`] bytes.
//! - Replies parse without serde_json's 128-level recursion limit, up to
//!   [`MAX_DEPTH`] levels (checked before parsing, so a deep reply is an error,
//!   not a stack overflow).
//! - Any transport failure drops the connection, so a late reply can never
//!   answer the next request.

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

use crate::AtalantaError;

/// Largest reply line accepted (a screenshot is base64 PNG, a few MiB).
pub const MAX_REPLY: usize = 64 * 1024 * 1024;

/// Deepest JSON nesting accepted in a reply.
pub const MAX_DEPTH: usize = 512;

/// A bridge's reply: `{"status":"ok","data":…}` or
/// `{"status":"error","message":…,"code":…}`.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Ok {
        data: Option<Value>,
    },
    Error {
        message: String,
        code: Option<String>,
    },
}

impl Reply {
    pub fn from_value(v: Value) -> anyhow::Result<Self> {
        let Value::Object(mut m) = v else {
            anyhow::bail!("reply is not a JSON object");
        };
        match m.get("status").and_then(Value::as_str) {
            Some("ok") => Ok(Reply::Ok {
                data: m.remove("data").filter(|d| !d.is_null()),
            }),
            Some("error") => Ok(Reply::Error {
                message: m
                    .remove("message")
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| "error without a message".to_string()),
                code: m
                    .remove("code")
                    .and_then(|v| v.as_str().map(str::to_string)),
            }),
            other => anyhow::bail!("reply has no valid status: {other:?}"),
        }
    }

    /// The `data` of an ok reply. An error reply fails `op` on `target` with
    /// `CommandFailed`, like a device command that ran and failed.
    pub fn into_result(self, op: &str, target: &str) -> crate::Result<Option<Value>> {
        match self {
            Reply::Ok { data } => Ok(data),
            Reply::Error { message, code } => Err(AtalantaError::CommandFailed {
                device_id: target.to_string(),
                command: op.to_string(),
                detail: match code {
                    Some(code) => format!("{code}: {message}"),
                    None => message,
                },
            }),
        }
    }
}

/// One connection: the reader lives as long as the connection, so bytes it
/// buffered past one reply are still there for the next.
struct Conn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    /// The reply line read so far. Kept here, not in the exchange, so a
    /// request dropped half way through a reply (a step timeout, Ctrl-C)
    /// leaves the bytes it read for the next request to finish the line
    /// with; a local buffer lost them and the next read began mid-line (V7).
    pending: Vec<u8>,
}

/// A JSON-lines client for one bridge server on 127.0.0.1.
pub struct JsonLineClient {
    name: &'static str,
    port: u16,
    conn: Option<Conn>,
    next_id: u64,
    /// Deadline for one request: write, then read its reply.
    pub timeout: Duration,
    /// Largest reply line accepted.
    pub max_reply: usize,
}

impl JsonLineClient {
    /// `name` names the server in errors ("Flutter bridge", "RN sidecar").
    pub fn new(name: &'static str, port: u16) -> Self {
        Self {
            name,
            port,
            conn: None,
            next_id: 0,
            timeout: Duration::from_secs(30),
            max_reply: MAX_REPLY,
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Connect to the server; fails if nothing listens within 2 s.
    pub async fn connect(&mut self) -> anyhow::Result<()> {
        let addr = format!("127.0.0.1:{}", self.port);
        match timeout(Duration::from_secs(2), TcpStream::connect(&addr)).await {
            Ok(Ok(stream)) => {
                tracing::info!("Connected to {} at {addr}", self.name);
                let (r, w) = stream.into_split();
                self.conn = Some(Conn {
                    reader: BufReader::new(r),
                    writer: w,
                    pending: Vec::new(),
                });
                Ok(())
            }
            Ok(Err(e)) => anyhow::bail!("{} not reachable at {addr}: {e}", self.name),
            Err(_) => anyhow::bail!("{} not reachable at {addr}: connect timed out", self.name),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.conn.is_some()
    }

    pub fn disconnect(&mut self) {
        self.conn = None;
    }

    /// Send `cmd` (a JSON object; `id` is added) and wait for its reply.
    pub async fn request<C: Serialize>(&mut self, cmd: &C) -> anyhow::Result<Reply> {
        let name = self.name;
        let conn = self
            .conn
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Not connected to {name}"))?;
        self.next_id += 1;
        let id = self.next_id;
        let mut value = serde_json::to_value(cmd)?;
        if let Value::Object(m) = &mut value {
            m.insert("id".into(), id.into());
        }
        let mut line = serde_json::to_vec(&value)?;
        line.push(b'\n');

        let wait = self.timeout;
        let max = self.max_reply;
        let result = match timeout(wait, exchange(conn, &line, id, max, name)).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("{name} reply timed out after {wait:?}")),
        };
        if result.is_err() {
            self.conn = None;
        }
        result
    }
}

async fn exchange(
    conn: &mut Conn,
    line: &[u8],
    id: u64,
    max: usize,
    name: &str,
) -> anyhow::Result<Reply> {
    conn.writer.write_all(line).await?;
    conn.writer.flush().await?;
    loop {
        // Cancel-safe: whatever `read_until` has appended stays in
        // `conn.pending` if this future is dropped, and the next call
        // carries on from it.
        while conn.pending.last() != Some(&b'\n') {
            let room = (max + 1).saturating_sub(conn.pending.len());
            if room == 0 {
                anyhow::bail!("{name} reply is larger than {max} bytes");
            }
            let n = (&mut conn.reader)
                .take(room as u64)
                .read_until(b'\n', &mut conn.pending)
                .await?;
            if n == 0 {
                if conn.pending.is_empty() {
                    anyhow::bail!("{name} disconnected");
                }
                anyhow::bail!("{name} disconnected mid-reply");
            }
        }
        let buf = std::mem::take(&mut conn.pending);
        let value = parse_line(&buf)?;
        match value.get("id") {
            Some(got) if got.as_u64() != Some(id) => {
                tracing::debug!("{name}: skipping a late reply (id {got}, want {id})");
                continue;
            }
            _ => return Reply::from_value(value),
        }
    }
}

/// Parse one reply line without serde_json's recursion limit, after checking
/// its nesting is at most [`MAX_DEPTH`].
pub fn parse_line(bytes: &[u8]) -> anyhow::Result<Value> {
    let depth = nesting_depth(bytes);
    if depth > MAX_DEPTH {
        anyhow::bail!("reply nests {depth} levels deep (limit {MAX_DEPTH})");
    }
    let mut de = serde_json::Deserializer::from_slice(bytes);
    de.disable_recursion_limit();
    let value = <Value as serde::Deserialize>::deserialize(&mut de)?;
    de.end()?;
    Ok(value)
}

/// Deepest `[`/`{` nesting in a JSON text, ignoring brackets inside strings.
fn nesting_depth(bytes: &[u8]) -> usize {
    let (mut depth, mut max) = (0usize, 0usize);
    let (mut in_str, mut escaped) = (false, false);
    for &b in bytes {
        if in_str {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'[' | b'{' => {
                depth += 1;
                max = max.max(depth);
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn pair() -> (JsonLineClient, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = JsonLineClient::new("test server", listener.local_addr().unwrap().port());
        client.timeout = Duration::from_millis(300);
        client.connect().await.unwrap();
        let (s, _) = listener.accept().await.unwrap();
        (client, s)
    }

    async fn read_request(s: &mut TcpStream) -> Value {
        let mut line = String::new();
        let mut r = BufReader::new(s);
        r.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn nesting_ignores_brackets_in_strings() {
        assert_eq!(nesting_depth(br#"{"a":"[[[{{{","b":[1,{"c":2}]}"#), 3);
        assert_eq!(nesting_depth(br#"{"a":"\"[["}"#), 1);
    }

    #[test]
    fn deep_replies_parse_up_to_the_cap() {
        let deep = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
        // serde_json alone stops at 128.
        assert!(serde_json::from_str::<Value>(&deep(200)).is_err());
        assert!(parse_line(deep(200).as_bytes()).is_ok());
        assert!(parse_line(deep(MAX_DEPTH).as_bytes()).is_ok());
        let err = parse_line(deep(MAX_DEPTH + 1).as_bytes()).unwrap_err();
        assert!(err.to_string().contains("levels deep"), "{err}");
    }

    /// Review MINOR 13: the deepest reply the cap lets through parses and
    /// drops on a 2 MiB stack (a tokio worker's), not just the test thread's.
    #[test]
    fn the_deepest_allowed_reply_fits_a_tokio_worker_stack() {
        let line = format!(
            "{}1{}\n",
            "[{\"a\":".repeat(MAX_DEPTH / 2),
            "}]".repeat(MAX_DEPTH / 2)
        );
        assert_eq!(nesting_depth(line.as_bytes()), MAX_DEPTH);
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let v = parse_line(line.as_bytes()).unwrap();
                let copy = v.clone();
                assert_eq!(serde_json::to_string(&copy).unwrap().len(), line.len() - 1);
                drop(copy);
                drop(v);
            })
            .unwrap()
            .join()
            .expect("a reply at the depth cap must not overflow a 2 MiB stack");
    }

    #[test]
    fn error_replies_become_command_failed() {
        let r = Reply::from_value(serde_json::json!({
            "status": "error", "message": "covered", "code": "not_hittable", "id": 3
        }))
        .unwrap();
        let e = r.into_result("tap", "flutter-0").unwrap_err();
        assert!(matches!(e, AtalantaError::CommandFailed { .. }), "{e:?}");
        assert!(e.to_string().contains("not_hittable: covered"), "{e}");
        assert_eq!(
            Reply::from_value(serde_json::json!({"status":"ok"}))
                .unwrap()
                .into_result("tap", "x")
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn requests_carry_ids_and_stale_replies_are_skipped() {
        let (mut client, mut s) = pair().await;
        let server = tokio::spawn(async move {
            let req = read_request(&mut s).await;
            assert_eq!(req["cmd"], "ping");
            let id = req["id"].as_u64().unwrap();
            // A late reply to an earlier request, then the real one.
            let stale = format!(
                "{{\"status\":\"error\",\"message\":\"old\",\"id\":{}}}\n",
                id + 100
            );
            let fresh = format!("{{\"status\":\"ok\",\"data\":1,\"id\":{id}}}\n");
            s.write_all(format!("{stale}{fresh}").as_bytes())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let reply = client
            .request(&serde_json::json!({"cmd": "ping"}))
            .await
            .unwrap();
        assert_eq!(
            reply,
            Reply::Ok {
                data: Some(1.into())
            }
        );
        server.await.unwrap();
    }

    // V7: a request dropped (a step watchdog, Ctrl-C) while its reply was
    // half read lost those bytes, so the next request began mid-line, failed
    // to parse it and dropped the connection.
    #[tokio::test]
    async fn a_request_dropped_mid_reply_does_not_corrupt_the_next() {
        let (mut client, mut s) = pair().await;
        client.timeout = Duration::from_secs(5);
        let server = tokio::spawn(async move {
            let first = read_request(&mut s).await;
            let id1 = first["id"].as_u64().unwrap();
            // Half of the first reply, then a pause the caller's own
            // deadline falls into.
            s.write_all(b"{\"status\":\"ok\",\"da").await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            let second = read_request(&mut s).await;
            let id2 = second["id"].as_u64().unwrap();
            // The rest of the first reply, then the second's.
            let rest = format!("ta\":\"first\",\"id\":{id1}}}\n");
            let reply = format!("{{\"status\":\"ok\",\"data\":\"second\",\"id\":{id2}}}\n");
            s.write_all(format!("{rest}{reply}").as_bytes())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let dropped = tokio::time::timeout(
            Duration::from_millis(100),
            client.request(&serde_json::json!({"cmd": "one"})),
        )
        .await;
        assert!(dropped.is_err(), "the first request was dropped mid-reply");
        assert!(client.is_connected(), "dropping it leaves the connection");
        let reply = client
            .request(&serde_json::json!({"cmd": "two"}))
            .await
            .unwrap();
        assert_eq!(
            reply,
            Reply::Ok {
                data: Some("second".into())
            }
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_reply_without_a_newline_is_capped() {
        let (mut client, mut s) = pair().await;
        client.max_reply = 1024;
        let server = tokio::spawn(async move {
            let _ = read_request(&mut s).await;
            let _ = s.write_all(&vec![b'x'; 4096]).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let err = client
            .request(&serde_json::json!({"cmd": "big"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("larger than 1024"), "{err}");
        assert!(!client.is_connected());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_stuck_write_times_out() {
        let (mut client, s) = pair().await;
        // The server never reads, so the socket buffers fill and write blocks.
        let big = "y".repeat(32 * 1024 * 1024);
        let body = serde_json::json!({"cmd": "upload", "blob": big});
        let started = std::time::Instant::now();
        let err = client.request(&body).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        // The timeout is 300 ms; the bound only has to tell "gave up" from
        // "hung". Serialising 32 MB in a debug build on a loaded CI runner
        // took over 5 s (macOS, 2026-11-22), so leave it room.
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "{:?}",
            started.elapsed()
        );
        assert!(!client.is_connected());
        drop(s);
    }
}
