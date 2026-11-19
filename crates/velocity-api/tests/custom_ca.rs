//! ENT-062: a CA the machine trusts through `SSL_CERT_FILE` (a corporate
//! proxy's, mitmproxy's) is trusted by the `http` step; before, only
//! Mozilla's bundled roots were.
//!
//! Its own test binary: it sets `SSL_CERT_FILE` for the whole process. It
//! needs the `openssl` command for the CA and the TLS server; without one
//! it says so and passes, except on CI (`CI` or `VELOCITY_TEST_STRICT`
//! set), where that fails.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use velocity_api::HttpSession;
use velocity_common::{HttpConfig, HttpRequest};

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
            "/CN=Velocity test CA",
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

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn a_ca_from_ssl_cert_file_is_trusted() {
    let dir = std::env::temp_dir().join(format!("velocity-ca-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    if !make_ca(&dir) {
        // A green CI run must not mean "skipped" (review 07 M6): on CI the
        // runner image has openssl, so its absence is a failure there.
        let strict = ["CI", "VELOCITY_TEST_STRICT"]
            .iter()
            .any(|k| std::env::var(k).is_ok_and(|v| !v.is_empty() && v != "0" && v != "false"));
        assert!(
            !strict,
            "no working `openssl` command to make a test CA (required on CI)"
        );
        eprintln!("skipped: no working `openssl` command to make a test CA");
        return;
    }
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _server = Server(
        Command::new("openssl")
            .args([
                "s_server", "-quiet", "-www", "-cert", "leaf.pem", "-key", "leaf.key",
            ])
            .args(["-accept", &format!("127.0.0.1:{port}")])
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "s_server never listened"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

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
