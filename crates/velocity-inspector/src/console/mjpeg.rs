//! A reader for an MJPEG stream over HTTP (`multipart/x-mixed-replace`),
//! as WebDriverAgent's MJPEG server sends the iOS screen. Loopback only:
//! the URL comes from the driver, never from the page.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::live::{FrameFeed, ImageFrame};

/// The largest frame accepted.
const MAX_FRAME: usize = 16 << 20;

/// `http://127.0.0.1:<port>/…` → (port, path). Anything else is refused.
fn loopback(url: &str) -> Option<(u16, String)> {
    let rest = url.strip_prefix("http://127.0.0.1:")?;
    let (port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    Some((port.parse().ok()?, path.to_string()))
}

/// Connect to `url` and wait for its first frame (within `first`).
pub async fn open(url: &str, first: Duration, source: &'static str) -> Result<FrameFeed, String> {
    let (port, path) = loopback(url).ok_or_else(|| format!("not a loopback URL: {url}"))?;
    let mut s = tokio::time::timeout(
        Duration::from_secs(2),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .map_err(|_| format!("{url}: connect timed out"))?
    .map_err(|e| format!("{url}: {e}"))?;
    let _ = s.set_nodelay(true);
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: keep-alive\r\n\r\n")
            .as_bytes(),
    )
    .await
    .map_err(|e| e.to_string())?;
    let mut r = BufReader::with_capacity(256 << 10, s);
    let (tx, frames) = mpsc::channel(2);
    let mut first_frame = tokio::time::timeout(first, read_frame(&mut r))
        .await
        .map_err(|_| format!("{url}: no frame in {first:?}"))??;
    let task = tokio::spawn(async move {
        loop {
            if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(first_frame) {
                return;
            }
            first_frame = match read_frame(&mut r).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::debug!("mjpeg stream ended: {e}");
                    return;
                }
            };
        }
    });
    Ok(FrameFeed {
        frames,
        source,
        guard: Box::new(AbortOnDrop(task)),
    })
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The next part's JPEG: headers up to a blank line (the HTTP response's
/// own headers are skipped the same way), then `Content-Length` bytes.
async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> Result<ImageFrame, String> {
    loop {
        let mut len: Option<usize> = None;
        let mut saw_header = false;
        loop {
            let mut line = String::new();
            let n = r.read_line(&mut line).await.map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("stream closed".into());
            }
            if line.len() > 8192 {
                return Err("header line too long".into());
            }
            let t = line.trim();
            if t.is_empty() {
                if saw_header {
                    break;
                }
                continue;
            }
            saw_header = true;
            if let Some((k, v)) = t.split_once(':') {
                if k.trim().eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().ok();
                }
            }
        }
        // The HTTP response head has no length: go on to the first part.
        let Some(len) = len else { continue };
        if len > MAX_FRAME {
            return Err(format!("frame of {len} bytes"));
        }
        let mut data = vec![0u8; len];
        r.read_exact(&mut data).await.map_err(|e| e.to_string())?;
        if data.starts_with(&[0xFF, 0xD8]) {
            return Ok(ImageFrame::jpeg(data, 0, 0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_urls_are_read() {
        assert_eq!(loopback("http://127.0.0.1:9101/"), Some((9101, "/".into())));
        assert_eq!(loopback("http://127.0.0.1:9100"), Some((9100, "/".into())));
        assert!(loopback("http://evil.example:9100/").is_none());
        assert!(loopback("http://127.0.0.1.evil:1/").is_none());
        assert!(loopback("file:///etc/passwd").is_none());
    }

    #[tokio::test]
    async fn parts_are_read_by_content_length() {
        let jpeg = [0xFFu8, 0xD8, 0xFF, 0xE0, 1, 2, 3];
        let mut body =
            b"HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=--B\r\n\r\n"
                .to_vec();
        for _ in 0..2 {
            body.extend_from_slice(b"--B\r\nContent-type: image/jpeg\r\nContent-Length: 7\r\n\r\n");
            body.extend_from_slice(&jpeg);
            body.extend_from_slice(b"\r\n");
        }
        let mut r = BufReader::new(&body[..]);
        for _ in 0..2 {
            let f = read_frame(&mut r).await.unwrap();
            assert_eq!(&f.data[..], &jpeg);
            assert!(!f.png);
        }
        assert!(read_frame(&mut r).await.is_err());
    }
}
