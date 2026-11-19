//! Google Cloud access tokens for Vertex AI, without the Google SDK.
//!
//! In order: the `GOOGLE_APPLICATION_CREDENTIALS` file (a service account,
//! signed into a JWT with RS256 and exchanged at its `token_uri`, or an
//! `authorized_user` refresh token), then the application-default file
//! `gcloud auth application-default login` writes, then
//! `gcloud auth print-access-token` from `PATH`. Tokens are cached until a
//! minute before they expire, and registered for redaction.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};
use velocity_common::{Result, VelocityError};

/// The OAuth scope Vertex needs.
pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// A bearer token, fetched (and refreshed) on demand.
#[async_trait]
pub trait TokenSource: Send + Sync {
    async fn token(&self) -> Result<String>;
}

/// Where a Google token comes from.
#[derive(Clone)]
enum Source {
    ServiceAccount {
        email: String,
        /// PKCS#8 DER of the private key.
        key: Vec<u8>,
        token_uri: String,
    },
    AuthorizedUser {
        client_id: String,
        client_secret: String,
        refresh_token: String,
        token_uri: String,
    },
    Gcloud {
        program: PathBuf,
    },
}

/// Google access tokens for Vertex.
pub struct GoogleToken {
    client: reqwest::Client,
    source: Source,
    cache: Mutex<Option<(String, Instant)>>,
}

impl GoogleToken {
    /// Find credentials (see the module docs). Returns the token source
    /// and the project the credentials name, if any. `env` reads the
    /// environment (`PATH` included).
    pub fn discover(env: &dyn Fn(&str) -> Option<String>) -> Result<(Self, Option<String>)> {
        let file = env("GOOGLE_APPLICATION_CREDENTIALS")
            .map(PathBuf::from)
            .or_else(|| {
                let home = env("HOME")?;
                let adc =
                    Path::new(&home).join(".config/gcloud/application_default_credentials.json");
                adc.exists().then_some(adc)
            });
        if let Some(file) = file {
            let text = std::fs::read_to_string(&file).map_err(|e| {
                VelocityError::Config(format!(
                    "can't read Google credentials {}: {e}",
                    file.display()
                ))
            })?;
            let (source, project) = parse_credentials(&text)?;
            return Ok((Self::with_source(source), project));
        }
        match find_in_path("gcloud", &env("PATH").unwrap_or_default()) {
            Some(program) => Ok((Self::with_source(Source::Gcloud { program }), None)),
            None => Err(VelocityError::Config(
                "vertex needs Google credentials: set GOOGLE_APPLICATION_CREDENTIALS to a service \
                 account key file, run `gcloud auth application-default login`, or have gcloud on PATH"
                    .into(),
            )),
        }
    }

    fn with_source(source: Source) -> Self {
        Self {
            client: reqwest::Client::builder()
                // A provider never redirects; following one would carry the key
                // (`x-api-key`, `api-key`) to another host (audit 2026-11).
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            source,
            cache: Mutex::new(None),
        }
    }

    async fn fetch(&self) -> Result<(String, Duration)> {
        match &self.source {
            Source::ServiceAccount {
                email,
                key,
                token_uri,
            } => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let jwt = service_account_jwt(email, key, token_uri, now)?;
                let form = format!(
                    "grant_type={}&assertion={jwt}",
                    crate::sigv4::uri_encode("urn:ietf:params:oauth:grant-type:jwt-bearer")
                );
                self.exchange(token_uri, form).await
            }
            Source::AuthorizedUser {
                client_id,
                client_secret,
                refresh_token,
                token_uri,
            } => {
                let enc = crate::sigv4::uri_encode;
                let form = format!(
                    "grant_type=refresh_token&client_id={}&client_secret={}&refresh_token={}",
                    enc(client_id),
                    enc(client_secret),
                    enc(refresh_token)
                );
                self.exchange(token_uri, form).await
            }
            Source::Gcloud { program } => {
                // A blocking run on the blocking pool: it works on any
                // runtime flavour, without a child-reaping driver.
                let program = program.clone();
                let run = tokio::task::spawn_blocking(move || {
                    std::process::Command::new(&program)
                        .args(["auth", "print-access-token"])
                        .stdin(std::process::Stdio::null())
                        .output()
                        .map_err(|e| format!("can't run {}: {e}", program.display()))
                });
                let out = tokio::time::timeout(Duration::from_secs(30), run)
                    .await
                    .map_err(|_| {
                        VelocityError::Config("gcloud auth print-access-token timed out".into())
                    })?
                    .map_err(|e| VelocityError::Internal(anyhow::anyhow!("gcloud task: {e}")))?
                    .map_err(VelocityError::Config)?;
                let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !out.status.success() || token.is_empty() {
                    return Err(VelocityError::Config(format!(
                        "gcloud auth print-access-token failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    )));
                }
                // gcloud tokens live an hour; refresh well before.
                Ok((token, Duration::from_secs(45 * 60)))
            }
        }
    }

    /// POST a token request and read `access_token` / `expires_in`.
    async fn exchange(&self, token_uri: &str, form: String) -> Result<(String, Duration)> {
        let resp = self
            .client
            .post(token_uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form)
            .send()
            .await
            .map_err(|e| VelocityError::ServiceUnavailable {
                service: "Google OAuth".into(),
                detail: e.to_string(),
            })?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            let msg = format!("Google token exchange failed ({status}): {text}");
            return Err(if status >= 500 || status == 429 {
                VelocityError::ServiceUnavailable {
                    service: "Google OAuth".into(),
                    detail: msg,
                }
            } else {
                VelocityError::Config(msg)
            });
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| VelocityError::Config(format!("bad Google token response: {e}")))?;
        let token = v["access_token"]
            .as_str()
            .ok_or_else(|| {
                VelocityError::Config("Google token response has no access_token".into())
            })?
            .to_string();
        let ttl = v["expires_in"].as_u64().unwrap_or(3600);
        Ok((token, Duration::from_secs(ttl)))
    }
}

#[async_trait]
impl TokenSource for GoogleToken {
    async fn token(&self) -> Result<String> {
        // A poisoned lock still holds a usable token (or none).
        if let Some((t, until)) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            if Instant::now() < *until {
                return Ok(t.clone());
            }
        }
        let (token, ttl) = self.fetch().await?;
        velocity_common::redact::register_secret(&token);
        let until = Instant::now() + ttl.saturating_sub(Duration::from_secs(60));
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((token.clone(), until));
        Ok(token)
    }
}

/// A credentials file: the token source and the project it names.
fn parse_credentials(text: &str) -> Result<(Source, Option<String>)> {
    let v: Value = serde_json::from_str(text)
        .map_err(|e| VelocityError::Config(format!("Google credentials file isn't JSON: {e}")))?;
    let s = |k: &str| v[k].as_str().map(String::from);
    let token_uri = s("token_uri").unwrap_or_else(|| DEFAULT_TOKEN_URI.into());
    match v["type"].as_str() {
        Some("service_account") => {
            let pem = s("private_key")
                .ok_or_else(|| VelocityError::Config("service account file has no private_key".into()))?;
            let key = pem_to_der(&pem)?;
            Ok((
                Source::ServiceAccount {
                    email: s("client_email").unwrap_or_default(),
                    key,
                    token_uri,
                },
                s("project_id"),
            ))
        }
        Some("authorized_user") => {
            let refresh_token = s("refresh_token").unwrap_or_default();
            velocity_common::redact::register_secret(&refresh_token);
            Ok((
                Source::AuthorizedUser {
                    client_id: s("client_id").unwrap_or_default(),
                    client_secret: s("client_secret").unwrap_or_default(),
                    refresh_token,
                    token_uri,
                },
                s("quota_project_id"),
            ))
        }
        other => Err(VelocityError::Config(format!(
            "Google credentials of type {other:?} aren't supported (service_account or authorized_user)"
        ))),
    }
}

/// The DER bytes of a PEM `PRIVATE KEY` block (PKCS#8).
fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .map(str::trim)
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|e| VelocityError::Config(format!("service account private_key isn't PEM: {e}")))
}

/// The signed JWT a service account exchanges for a token (RS256, an
/// hour, the cloud-platform scope).
pub fn service_account_jwt(
    email: &str,
    key_der: &[u8],
    token_uri: &str,
    now: u64,
) -> Result<String> {
    let b64 = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
    let header = json!({ "alg": "RS256", "typ": "JWT" });
    let claims = json!({
        "iss": email,
        "scope": SCOPE,
        "aud": token_uri,
        "iat": now,
        "exp": now + 3600,
    });
    let signing_input = format!(
        "{}.{}",
        b64(header.to_string().as_bytes()),
        b64(claims.to_string().as_bytes())
    );
    let pair = ring::signature::RsaKeyPair::from_pkcs8(key_der).map_err(|e| {
        VelocityError::Config(format!("service account key isn't a PKCS#8 RSA key: {e}"))
    })?;
    let mut sig = vec![0u8; pair.public().modulus_len()];
    pair.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        signing_input.as_bytes(),
        &mut sig,
    )
    .map_err(|_| VelocityError::Internal(anyhow::anyhow!("RSA signing failed")))?;
    Ok(format!("{signing_input}.{}", b64(&sig)))
}

/// `name` in the directories of `path_var` (a `PATH` value).
pub fn find_in_path(name: &str, path_var: &str) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A throwaway 2048-bit test key (PKCS#8), generated for these tests.
    pub(crate) const TEST_KEY: &str = include_str!("../fixtures/test-service-account-key.pem");

    pub(crate) fn sa_file(dir: &Path, token_uri: &str) -> PathBuf {
        let path = dir.join("sa.json");
        let doc = json!({
            "type": "service_account",
            "project_id": "velocity-test",
            "client_email": "tester@velocity-test.iam.gserviceaccount.com",
            "private_key": TEST_KEY,
            "token_uri": token_uri,
        });
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(&path, doc.to_string()).unwrap();
        path
    }

    #[test]
    fn the_jwt_is_rs256_signed_with_the_service_account_key() {
        let der = pem_to_der(TEST_KEY).unwrap();
        let jwt = service_account_jwt(
            "sa@x.iam",
            &der,
            "https://oauth2.googleapis.com/token",
            1000,
        )
        .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let dec = |p: &str| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(p)
                .unwrap()
        };
        let header: Value = serde_json::from_slice(&dec(parts[0])).unwrap();
        let claims: Value = serde_json::from_slice(&dec(parts[1])).unwrap();
        assert_eq!(header["alg"], "RS256");
        assert_eq!(claims["iss"], "sa@x.iam");
        assert_eq!(claims["scope"], SCOPE);
        assert_eq!(
            (claims["iat"].as_u64(), claims["exp"].as_u64()),
            (Some(1000), Some(4600))
        );
        // The signature verifies with the key's public half.
        let pair = ring::signature::RsaKeyPair::from_pkcs8(&der).unwrap();
        use ring::signature::KeyPair as _;
        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            pair.public_key().as_ref().to_vec(),
        );
        let input = format!("{}.{}", parts[0], parts[1]);
        assert!(public.verify(input.as_bytes(), &dec(parts[2])).is_ok());
        assert!(public.verify(b"tampered", &dec(parts[2])).is_err());
    }

    #[tokio::test]
    async fn a_service_account_exchanges_its_jwt_and_caches_the_token() {
        let reply = json!({ "access_token": "ya29.from-sa", "expires_in": 3600 }).to_string();
        let (origin, seen) = crate::testutil::serve(vec![(200, "", reply)]).await;
        let dir = std::env::temp_dir().join(format!("velnl-gcp-sa-{}", std::process::id()));
        let file = sa_file(&dir, &format!("{origin}/token"));
        let env =
            |k: &str| (k == "GOOGLE_APPLICATION_CREDENTIALS").then(|| file.display().to_string());
        let (source, project) = GoogleToken::discover(&env).unwrap();
        assert_eq!(project.as_deref(), Some("velocity-test"));
        assert_eq!(source.token().await.unwrap(), "ya29.from-sa");
        assert_eq!(source.token().await.unwrap(), "ya29.from-sa", "cached");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].target, "/token");
        assert!(seen[0].raw_body.starts_with(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion=ey"
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn gcloud_on_path_is_the_fallback() {
        // The fake gcloud is /bin/echo under that name: it prints its
        // arguments, so the "token" shows it was asked for one. (A fresh
        // script would do too, but endpoint scanners can hold a new
        // executable for a minute.)
        let dir = std::env::temp_dir().join(format!("velnl-gcloud-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("/bin/echo", dir.join("gcloud")).unwrap();
        let path = format!("/nonexistent:{}", dir.display());
        let env = move |k: &str| match k {
            "PATH" => Some(path.clone()),
            "HOME" => Some("/nonexistent-home".into()),
            _ => None,
        };
        let (source, project) = GoogleToken::discover(&env).unwrap();
        assert_eq!(project, None);
        assert_eq!(source.token().await.unwrap(), "auth print-access-token");
        // Nothing at all: a configuration error.
        let none = |k: &str| match k {
            "PATH" => Some("/nonexistent".to_string()),
            "HOME" => Some("/nonexistent-home".to_string()),
            _ => None,
        };
        assert_eq!(GoogleToken::discover(&none).err().unwrap().exit_code(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn other_credential_types_are_refused() {
        assert!(parse_credentials(r#"{"type":"external_account"}"#).is_err());
        let (s, _) = parse_credentials(
            r#"{"type":"authorized_user","client_id":"c","client_secret":"s","refresh_token":"r-T1"}"#,
        )
        .unwrap();
        assert!(matches!(s, Source::AuthorizedUser { .. }));
    }
}
