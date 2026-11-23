// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! ENT-062: a CA the machine trusts through `SSL_CERT_FILE` (a corporate
//! proxy's, mitmproxy's) is trusted by the `http` step; before, only
//! Mozilla's bundled roots were.
//!
//! Its own test binary: it sets `SSL_CERT_FILE` for the whole process. It
//! needs the `openssl` command for the CA; without one it says so and
//! passes, except on CI (`CI` or `ATALANTA_TEST_STRICT` set), where that
//! fails. The TLS server is rustls, in the test: `openssl s_server -www`
//! behaved differently per build (LibreSSL never listened; the build on
//! macos-15-intel closed without close_notify, which rustls refuses).

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use atalanta_api::HttpSession;
use atalanta_common::{HttpConfig, HttpRequest};

fn openssl(dir: &Path, args: &[&str]) -> bool {
    Command::new("openssl")
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A CA, and a certificate for 127.0.0.1 it signed, in `dir`.
fn make_ca(dir: &Path) -> bool {
    std::fs::write(
        dir.join("leaf.ext"),
        "basicConstraints=CA:FALSE\nsubjectAltName=IP:127.0.0.1\nextendedKeyUsage=serverAuth\n",
    )
    .unwrap();
    openssl(
        dir,
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "2",
            "-subj",
            "/CN=Atalanta test CA",
            "-keyout",
            "ca.key",
            "-out",
            "ca.pem",
        ],
    ) && openssl(
        dir,
        &[
            "req",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=127.0.0.1",
            "-keyout",
            "leaf.key",
            "-out",
            "leaf.csr",
        ],
    ) && openssl(
        dir,
        &[
            "x509",
            "-req",
            "-in",
            "leaf.csr",
            "-CA",
            "ca.pem",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-days",
            "2",
            "-extfile",
            "leaf.ext",
            "-out",
            "leaf.pem",
        ],
    )
}

/// Serve `200 ok` over TLS with `dir`'s leaf certificate, on a free port of
/// 127.0.0.1, until the test ends. Returns the port.
async fn serve_tls(dir: &Path) -> u16 {
    let certs = CertificateDer::pem_file_iter(dir.join("leaf.pem"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_file(dir.join("leaf.key")).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                // A client that refuses the certificate ends the handshake.
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let mut got = Vec::new();
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = tls
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .await;
                // Sends close_notify.
                let _ = tls.shutdown().await;
            });
        }
    });
    port
}

#[tokio::test]
async fn a_ca_from_ssl_cert_file_is_trusted() {
    let dir = std::env::temp_dir().join(format!("atalanta-ca-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    if !make_ca(&dir) {
        // A green CI run must not mean "skipped" (review 07 M6): on CI the
        // runner image has openssl, so its absence is a failure there.
        let strict = ["CI", "ATALANTA_TEST_STRICT"]
            .iter()
            .any(|k| std::env::var(k).is_ok_and(|v| !v.is_empty() && v != "0" && v != "false"));
        assert!(
            !strict,
            "no working `openssl` command to make a test CA (required on CI)"
        );
        eprintln!("skipped: no working `openssl` command to make a test CA");
        return;
    }
    let port = serve_tls(&dir).await;

    let config = HttpConfig {
        base_url: Some(format!("https://127.0.0.1:{port}")),
        ..Default::default()
    };
    let step: HttpRequest = serde_yaml::from_str("url: /\nexpect:\n  status: 200\n").unwrap();
    let t = Duration::from_secs(10);

    // Not trusted yet: the CA is in no store.
    std::env::remove_var("SSL_CERT_DIR");
    std::env::set_var("SSL_CERT_FILE", dir.join("nothing.pem"));
    let err = HttpSession::new(&config).unwrap().run(&step, t).await;
    assert!(err.is_err(), "an unknown CA was trusted");

    // Trusted once SSL_CERT_FILE names it (a new session reads the store).
    std::env::set_var("SSL_CERT_FILE", dir.join("ca.pem"));
    let ok = HttpSession::new(&config).unwrap().run(&step, t).await;
    assert!(ok.is_ok(), "{ok:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
