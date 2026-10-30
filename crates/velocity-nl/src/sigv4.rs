//! AWS Signature Version 4 for Bedrock requests, and the standard
//! credential chain (environment, then the shared `~/.aws` files).
//!
//! Hand-written on `ring` (already in the dependency tree through rustls)
//! instead of `aws-sigv4`, which brings the smithy runtime with it: one
//! signing routine is ~100 lines and is checked against AWS's published
//! test vectors below. Not covered: SSO, `credential_process`, assumed
//! roles and instance metadata. For those, export temporary keys
//! (`aws configure export-credentials --format env`) or use a Bedrock API
//! key (`AWS_BEARER_TOKEN_BEDROCK`).

use std::fmt::Write as _;

use ring::{digest, hmac};
use velocity_common::{Result, VelocityError};

/// AWS credentials. The secret parts are registered for redaction when
/// read.
#[derive(Clone)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

/// What to sign.
pub struct SignRequest<'a> {
    pub method: &'a str,
    pub host: &'a str,
    /// The path as sent, already percent-encoded (`/model/a%3A1/converse`).
    pub path: &'a str,
    /// The query string as sent, without `?` (may be empty).
    pub query: &'a str,
    /// Headers to sign besides `host` and `x-amz-date` (names any case).
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
    pub region: &'a str,
    pub service: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    pub amz_date: &'a str,
}

fn sha256_hex(data: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, data).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&key, data).as_ref().to_vec()
}

/// RFC 3986 encoding of everything but unreserved characters.
pub fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// The signing key for `date` (`YYYYMMDD`), region and service.
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// The canonical query string: parameters sorted, names and values
/// encoded.
fn canonical_query(query: &str) -> String {
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (uri_encode(&decode(k)), uri_encode(&decode(v)))
        })
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-decoding (for re-encoding the query canonically).
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // The two hex digits are read as bytes: `%a` before a multi-byte
        // character would cut it in half as a `str` slice, and panic.
        let hex = || {
            let h = bytes.get(i + 1..i + 3)?;
            let h = std::str::from_utf8(h).ok()?;
            h.bytes()
                .all(|b| b.is_ascii_hexdigit())
                .then(|| u8::from_str_radix(h, 16).ok())?
        };
        if bytes[i] == b'%' {
            if let Some(b) = hex() {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The canonical URI for every service but S3: each segment of the path as
/// sent is encoded again.
fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        return "/".into();
    }
    path.split('/')
        .map(uri_encode)
        .collect::<Vec<_>>()
        .join("/")
}

/// Sign a request: the headers to add (`x-amz-date`, the session token
/// when there is one, and `authorization`), plus the string to sign for
/// tests.
pub fn sign(req: &SignRequest<'_>, creds: &AwsCredentials) -> (Vec<(String, String)>, String) {
    let mut headers: Vec<(String, String)> = req
        .headers
        .iter()
        .map(|(n, v)| (n.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    headers.push(("host".into(), req.host.to_string()));
    headers.push(("x-amz-date".into(), req.amz_date.to_string()));
    if let Some(token) = &creds.session_token {
        headers.push(("x-amz-security-token".into(), token.clone()));
    }
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(n, v)| format!("{n}:{v}\n")).collect();
    let signed: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
    let signed = signed.join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed}\n{}",
        req.method,
        canonical_uri(req.path),
        canonical_query(req.query),
        sha256_hex(req.body)
    );
    let date = req.amz_date.get(..8).unwrap_or(req.amz_date);
    let scope = format!("{date}/{}/{}/aws4_request", req.region, req.service);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        req.amz_date,
        sha256_hex(canonical.as_bytes())
    );
    let key = signing_key(&creds.secret_access_key, date, req.region, req.service);
    let signature = hex(&hmac_sha256(&key, to_sign.as_bytes()));
    let mut out = vec![("x-amz-date".to_string(), req.amz_date.to_string())];
    if let Some(token) = &creds.session_token {
        out.push(("x-amz-security-token".into(), token.clone()));
    }
    out.push((
        "authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            creds.access_key_id
        ),
    ));
    (out, to_sign)
}

/// Now as `YYYYMMDDTHHMMSSZ` (UTC).
pub fn amz_date_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    amz_date(secs)
}

/// `secs` since the epoch as `YYYYMMDDTHHMMSSZ`.
pub fn amz_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The credentials and region from the standard chain: the environment
/// (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`,
/// `AWS_REGION` / `AWS_DEFAULT_REGION`), then the profile
/// (`AWS_PROFILE`, default `default`) in `~/.aws/credentials` and
/// `~/.aws/config` (or `AWS_SHARED_CREDENTIALS_FILE` / `AWS_CONFIG_FILE`).
pub fn credentials_from(
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(AwsCredentials, Option<String>)> {
    let region_env = env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION"));
    if let (Some(id), Some(secret)) = (env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY")) {
        let creds = AwsCredentials {
            access_key_id: id,
            secret_access_key: secret,
            session_token: env("AWS_SESSION_TOKEN"),
        };
        register(&creds);
        return Ok((creds, region_env));
    }
    let profile = env("AWS_PROFILE").unwrap_or_else(|| "default".into());
    let home = env("HOME").unwrap_or_default();
    let creds_file =
        env("AWS_SHARED_CREDENTIALS_FILE").unwrap_or_else(|| format!("{home}/.aws/credentials"));
    let config_file = env("AWS_CONFIG_FILE").unwrap_or_else(|| format!("{home}/.aws/config"));
    let creds_ini = std::fs::read_to_string(&creds_file).unwrap_or_default();
    let config_ini = std::fs::read_to_string(&config_file).unwrap_or_default();
    let from_creds = ini_section(&creds_ini, &profile);
    let config_name = if profile == "default" {
        "default".to_string()
    } else {
        format!("profile {profile}")
    };
    let from_config = ini_section(&config_ini, &config_name);
    let get = |k: &str| {
        from_creds
            .iter()
            .chain(from_config.iter())
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
    };
    match (get("aws_access_key_id"), get("aws_secret_access_key")) {
        (Some(id), Some(secret)) => {
            let creds = AwsCredentials {
                access_key_id: id,
                secret_access_key: secret,
                session_token: get("aws_session_token"),
            };
            register(&creds);
            Ok((creds, region_env.or_else(|| get("region"))))
        }
        _ => Err(VelocityError::Config(format!(
            "bedrock needs AWS credentials: set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY \
             (and AWS_SESSION_TOKEN), keys for profile {profile:?} in {creds_file}, or a \
             Bedrock API key in AWS_BEARER_TOKEN_BEDROCK. SSO and role profiles: run \
             `aws configure export-credentials --format env` first"
        ))),
    }
}

fn register(creds: &AwsCredentials) {
    velocity_common::redact::register_secret(&creds.secret_access_key);
    if let Some(t) = &creds.session_token {
        velocity_common::redact::register_secret(t);
    }
}

/// The `key = value` pairs of `[section]` in an INI file.
fn ini_section(text: &str, section: &str) -> Vec<(String, String)> {
    let mut inside = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            inside = name.trim() == section;
            continue;
        }
        if inside {
            if let Some((k, v)) = line.split_once('=') {
                out.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_a_query_never_splits_a_character() {
        // `%a` then a two-byte `é`: the hex slice would end inside it.
        assert_eq!(decode("%aé"), "%aé");
        assert_eq!(decode("caf%C3%A9%2"), "café%2");
        assert_eq!(decode("%é%"), "%é%");
        assert_eq!(
            canonical_query("q=%a日本&b=1"),
            "b=1&q=%25a%E6%97%A5%E6%9C%AC"
        );
    }

    /// The credentials of AWS's SigV4 test suite and documentation.
    fn example() -> AwsCredentials {
        AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        }
    }

    fn auth(headers: &[(String, String)]) -> String {
        headers
            .iter()
            .find(|(n, _)| n == "authorization")
            .unwrap()
            .1
            .clone()
    }

    // AWS SigV4 test suite: get-vanilla.
    #[test]
    fn get_vanilla_matches_the_aws_test_suite() {
        let req = SignRequest {
            method: "GET",
            host: "example.amazonaws.com",
            path: "/",
            query: "",
            headers: &[],
            body: b"",
            region: "us-east-1",
            service: "service",
            amz_date: "20150830T123600Z",
        };
        let (headers, _) = sign(&req, &example());
        assert_eq!(
            auth(&headers),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    // AWS SigV4 test suite: post-vanilla.
    #[test]
    fn post_vanilla_matches_the_aws_test_suite() {
        let req = SignRequest {
            method: "POST",
            host: "example.amazonaws.com",
            path: "/",
            query: "",
            headers: &[],
            body: b"",
            region: "us-east-1",
            service: "service",
            amz_date: "20150830T123600Z",
        };
        let (headers, _) = sign(&req, &example());
        assert!(auth(&headers).ends_with(
            "Signature=5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b"
        ));
    }

    // The worked example in AWS's SigV4 documentation (IAM ListUsers).
    #[test]
    fn the_documented_iam_example_signs_the_same() {
        let key = signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex(&key),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
        let req = SignRequest {
            method: "GET",
            host: "iam.amazonaws.com",
            path: "/",
            query: "Action=ListUsers&Version=2010-05-08",
            headers: &[(
                "Content-Type",
                "application/x-www-form-urlencoded; charset=utf-8",
            )],
            body: b"",
            region: "us-east-1",
            service: "iam",
            amz_date: "20150830T123600Z",
        };
        let (headers, to_sign) = sign(&req, &example());
        assert!(
            to_sign.ends_with("f536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59"),
            "{to_sign}"
        );
        assert!(auth(&headers).ends_with(
            "SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        ), "{}", auth(&headers));
    }

    #[test]
    fn a_session_token_is_signed_and_sent() {
        let creds = AwsCredentials {
            session_token: Some("tok".into()),
            ..example()
        };
        let req = SignRequest {
            method: "POST",
            host: "bedrock-runtime.us-east-1.amazonaws.com",
            path: "/model/anthropic.claude-haiku-5-5/converse",
            query: "",
            headers: &[("content-type", "application/json")],
            body: b"{}",
            region: "us-east-1",
            service: "bedrock",
            amz_date: "20261025T000000Z",
        };
        let (headers, _) = sign(&req, &creds);
        assert!(headers
            .iter()
            .any(|(n, v)| n == "x-amz-security-token" && v == "tok"));
        assert!(auth(&headers)
            .contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token"));
    }

    #[test]
    fn paths_are_encoded_twice_and_dates_format() {
        assert_eq!(uri_encode("a:b/c d"), "a%3Ab%2Fc%20d");
        assert_eq!(
            canonical_uri("/model/a%3A0/converse"),
            "/model/a%253A0/converse"
        );
        assert_eq!(canonical_query("b=2&a=1&c"), "a=1&b=2&c=");
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(951_782_400), "20000229T000000Z");
    }

    #[test]
    fn the_chain_reads_env_then_profile_files() {
        let env = |pairs: Vec<(&'static str, String)>| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
        };
        let e = env(vec![
            ("AWS_ACCESS_KEY_ID", "AKID".into()),
            ("AWS_SECRET_ACCESS_KEY", "SECRET-T1-A".into()),
            ("AWS_REGION", "eu-west-1".into()),
        ]);
        let (c, region) = credentials_from(&e).unwrap();
        assert_eq!(
            (c.access_key_id.as_str(), region.as_deref()),
            ("AKID", Some("eu-west-1"))
        );
        assert!(!velocity_common::redact::redact("x SECRET-T1-A").contains("SECRET-T1-A"));

        let dir = std::env::temp_dir().join(format!("velnl-aws-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("credentials"),
            "[default]\naws_access_key_id = D\naws_secret_access_key = DS\n[work]\naws_access_key_id = W\naws_secret_access_key = WS\naws_session_token = WT\n",
        )
        .unwrap();
        std::fs::write(dir.join("config"), "[profile work]\nregion = us-west-2\n").unwrap();
        let files = |profile: Option<&str>| {
            let mut v = vec![
                (
                    "AWS_SHARED_CREDENTIALS_FILE",
                    dir.join("credentials").display().to_string(),
                ),
                ("AWS_CONFIG_FILE", dir.join("config").display().to_string()),
            ];
            if let Some(p) = profile {
                v.push(("AWS_PROFILE", p.to_string()));
            }
            env(v)
        };
        let (c, region) = credentials_from(&files(Some("work"))).unwrap();
        assert_eq!(c.access_key_id, "W");
        assert_eq!(c.session_token.as_deref(), Some("WT"));
        assert_eq!(region.as_deref(), Some("us-west-2"));
        let (c, region) = credentials_from(&files(None)).unwrap();
        assert_eq!((c.access_key_id.as_str(), region), ("D", None));
        let e = credentials_from(&files(Some("missing"))).unwrap_err();
        assert_eq!(e.exit_code(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }
}
