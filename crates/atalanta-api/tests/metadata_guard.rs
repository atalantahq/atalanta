//! `VELOCITY_HTTP_BLOCK_METADATA=1` end to end: the guard's pieces are unit
//! tested in `guard.rs`, but nothing showed a step is actually refused, or a
//! redirect into the metadata address stopped. Its own test binary: it sets
//! the variable for the whole process.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::Duration;

use velocity_api::HttpSession;
use velocity_common::{HttpConfig, HttpRequest, VelocityError};

fn step(yaml: &str) -> HttpRequest {
    serde_yaml::from_str(yaml).unwrap()
}

#[tokio::test]
async fn metadata_addresses_are_refused_even_through_a_redirect() {
    std::env::set_var("VELOCITY_HTTP_BLOCK_METADATA", "1");
    let t = Duration::from_secs(5);
    let s = HttpSession::new(&HttpConfig::default()).unwrap();
    let e = s
        .run(&step("url: http://169.254.169.254/latest/meta-data"), t)
        .await
        .unwrap_err();
    assert!(matches!(e, VelocityError::Config(_)), "{e:?}");
    assert_eq!(e.exit_code(), 2);

    // A server that sends the request on to the metadata address.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            let mut r = BufReader::new(conn.try_clone().unwrap());
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim_end().is_empty() {
                    break;
                }
            }
            let _ = conn.write_all(
                b"HTTP/1.1 302 X\r\nlocation: http://169.254.169.254/latest\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            );
        }
    });
    let e = s
        .run(&step(&format!("url: http://{addr}/go")), t)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("link-local or cloud-metadata"), "{e}");
    // A local server is not the metadata service.
    assert!(!velocity_api::guard::blocked_ip(
        "127.0.0.1".parse().unwrap()
    ));
}
