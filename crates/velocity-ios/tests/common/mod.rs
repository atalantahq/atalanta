//! A scriptable fake WebDriverAgent on 127.0.0.1 for the driver's tests:
//! each request goes to a handler that answers JSON, raw bytes, a closed
//! connection or nothing at all.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    pub path: String,
    /// What followed `?` in the request target, if anything.
    pub query: String,
    pub body: String,
}

pub enum Resp {
    /// Status and a JSON body.
    Json(u16, String),
    /// Status and a body sent as is.
    Raw(u16, String),
    /// Read the request, then close without answering.
    Close,
    /// Answer nothing for this long, then close.
    Hang(Duration),
}

pub type Handler = dyn Fn(&Req) -> Resp + Send + Sync;

pub struct FakeWda {
    pub url: String,
    pub log: Arc<Mutex<Vec<Req>>>,
}

impl FakeWda {
    pub async fn start(handler: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> Self {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let log2 = log.clone();
        tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                let (h, log) = (handler.clone(), log2.clone());
                tokio::spawn(serve(s, h, log));
            }
        });
        Self { url, log }
    }

    /// Requests whose path ends with `suffix`.
    pub fn count(&self, method: &str, suffix: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == method && r.path.ends_with(suffix))
            .count()
    }
}

async fn serve(mut s: TcpStream, h: Arc<Handler>, log: Arc<Mutex<Vec<Req>>>) {
    let mut buf = Vec::new();
    loop {
        let mut tmp = [0u8; 8192];
        let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            match s.read(&mut tmp).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..pos]).to_string();
        let len = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        while buf.len() < pos + 4 + len {
            match s.read(&mut tmp).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let body = String::from_utf8_lossy(&buf[pos + 4..pos + 4 + len]).to_string();
        buf.drain(..pos + 4 + len);
        let mut first = head.lines().next().unwrap_or("").split_whitespace();
        let method = first.next().unwrap_or("").to_string();
        let target = first.next().unwrap_or("");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let req = Req {
            method,
            path: path.to_string(),
            query: query.to_string(),
            body,
        };
        log.lock().unwrap().push(req.clone());
        let (status, body) = match h(&req) {
            Resp::Json(st, b) | Resp::Raw(st, b) => (st, b),
            Resp::Close => return,
            Resp::Hang(d) => {
                tokio::time::sleep(d).await;
                return;
            }
        };
        let out = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        if s.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
}

pub fn ok(value: &str) -> Resp {
    Resp::Json(200, format!(r#"{{"value":{value}}}"#))
}

pub fn w3c(status: u16, code: &str, message: &str) -> Resp {
    Resp::Json(
        status,
        format!(r#"{{"value":{{"error":"{code}","message":"{message}"}}}}"#),
    )
}

/// `/source` XML as WDA's JSON string.
pub fn source(xml: &str) -> Resp {
    ok(&serde_json::to_string(xml).unwrap())
}

pub fn app(children: &str) -> String {
    format!(
        r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="402" height="874" enabled="true">{children}</XCUIElementTypeApplication>"#
    )
}

pub fn node(ty: &str, label: &str, x: i32, y: i32, w: i32, h: i32) -> String {
    format!(
        r#"<XCUIElementType{ty} type="XCUIElementType{ty}" label="{label}" x="{x}" y="{y}" width="{w}" height="{h}" enabled="true"/>"#
    )
}

/// A simulator-shaped UDID (not a real device).
pub const SIM: &str = "00000000-0000-0000-0000-0000000000AB";

/// (x, y) of the first pointerMove of a W3C actions body.
pub fn tap_xy(body: &str) -> Option<(f64, f64)> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let mv = v["actions"][0]["actions"]
        .as_array()?
        .iter()
        .find(|a| a["type"] == "pointerMove")?
        .clone();
    Some((mv["x"].as_f64()?, mv["y"].as_f64()?))
}

impl FakeWda {
    /// Tap points sent through `/actions`, in order.
    pub fn taps(&self) -> Vec<(f64, f64)> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path.ends_with("/actions"))
            .filter_map(|r| tap_xy(&r.body))
            .collect()
    }
}
