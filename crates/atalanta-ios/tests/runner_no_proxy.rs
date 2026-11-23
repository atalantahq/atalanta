//! ENT-062: the iOS runner's traffic never goes through a proxy, even with
//! `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` set for everything else.
//!
//! Its own test binary: it sets proxy variables for the whole process.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A server that answers every request with `body` and counts connections.
async fn serve(body: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), hits)
}

#[tokio::test]
async fn runner_requests_bypass_the_proxy_variables() {
    let (runner, runner_hits) = serve("runner").await;
    let (proxy, proxy_hits) = serve("proxy").await;
    // Set before any client is built; this binary has no other test.
    for var in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "all_proxy",
    ] {
        std::env::set_var(var, &proxy);
    }
    for var in ["NO_PROXY", "no_proxy"] {
        std::env::remove_var(var);
    }

    // Control: a default client does go through the proxy for loopback.
    let body = reqwest::Client::new()
        .get(format!("{runner}/status"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "proxy");
    assert_eq!(proxy_hits.load(Ordering::SeqCst), 1);

    let body = atalanta_ios::wda::runner_client()
        .build()
        .unwrap()
        .get(format!("{runner}/status"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "runner");
    assert_eq!(runner_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        proxy_hits.load(Ordering::SeqCst),
        1,
        "the runner call was proxied"
    );
}
