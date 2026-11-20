//! App Store Connect API client for headless iOS provisioning.
//!
//! Only Apple's public, documented API (`api.appstoreconnect.apple.com`,
//! authenticated with an API key): no Apple ID password, no private Xcode
//! endpoints, no anisette, no third-party servers. It registers devices,
//! makes sure a bundle id exists and creates development profiles, the
//! steps a real-device run needs before the test runner can be signed.
//!
//! It never revokes certificates or deletes profiles: on a shared team that
//! would break other developers' signing.
//!
//! The key comes from `VELOCITY_ASC_KEY_PATH` (the `.p8` file),
//! `VELOCITY_ASC_KEY_ID` and `VELOCITY_ASC_ISSUER_ID` (App Store Connect →
//! Users and Access → Integrations → App Store Connect API). Neither the key
//! nor the tokens made from it are logged.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{json, Value};
use velocity_common::{Result, VelocityError};

const API: &str = "https://api.appstoreconnect.apple.com";
/// Apple accepts tokens that live at most 20 minutes.
const TOKEN_TTL_SECS: u64 = 15 * 60;
const _: () = assert!(
    TOKEN_TTL_SECS <= 20 * 60,
    "Apple rejects tokens over 20 minutes"
);

/// An App Store Connect API key.
pub struct AscKey {
    key_id: String,
    issuer_id: String,
    pkcs8: Vec<u8>,
}

impl std::fmt::Debug for AscKey {
    // Never print the key material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AscKey")
            .field("key_id", &self.key_id)
            .field("issuer_id", &self.issuer_id)
            .finish_non_exhaustive()
    }
}

/// Where the key's three parts come from (`VELOCITY_ASC_*`), when all are set.
#[derive(Debug, Clone)]
pub struct AscKeyRef {
    pub path: PathBuf,
    pub key_id: String,
    pub issuer_id: String,
}

impl AscKeyRef {
    /// `None` when no part is set; an error when only some are.
    pub fn from_env() -> Result<Option<Self>> {
        let get = |k: &str| {
            velocity_common::env::var(k)
                .ok()
                .filter(|v| !v.trim().is_empty())
        };
        let (path, key_id, issuer) = (
            get("VELOCITY_ASC_KEY_PATH"),
            get("VELOCITY_ASC_KEY_ID"),
            get("VELOCITY_ASC_ISSUER_ID"),
        );
        match (path, key_id, issuer) {
            (None, None, None) => Ok(None),
            (Some(path), Some(key_id), Some(issuer_id)) => Ok(Some(Self {
                path: PathBuf::from(path),
                key_id,
                issuer_id,
            })),
            _ => Err(config(
                "App Store Connect key: set all of VELOCITY_ASC_KEY_PATH, VELOCITY_ASC_KEY_ID \
                 and VELOCITY_ASC_ISSUER_ID, or none",
            )),
        }
    }

    pub fn load(&self) -> Result<AscKey> {
        AscKey::from_file(&self.path, &self.key_id, &self.issuer_id)
    }
}

impl AscKey {
    pub fn from_file(path: &Path, key_id: &str, issuer_id: &str) -> Result<Self> {
        let pem = std::fs::read_to_string(path)
            .map_err(|e| config(&format!("App Store Connect key {}: {e}", path.display())))?;
        Self::from_pem(&pem, key_id, issuer_id)
    }

    /// From the `.p8` file's text (PKCS#8, PEM).
    pub fn from_pem(pem: &str, key_id: &str, issuer_id: &str) -> Result<Self> {
        // A BOM (files saved by some Windows editors) or an indented header
        // line made the header part of the base64 (VEL-301).
        let body: String = pem
            .trim_start_matches('\u{feff}')
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("-----"))
            .collect();
        let pkcs8 = base64::engine::general_purpose::STANDARD
            .decode(body)
            // Not the decoder's error: it can quote a character of the key.
            .map_err(|_| config("App Store Connect key: not a PEM (.p8) file"))?;
        // Check it now, not on the first request.
        EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            &pkcs8,
            &SystemRandom::new(),
        )
        .map_err(|e| {
            config(&format!(
                "App Store Connect key: not an EC P-256 private key (.p8): {e}"
            ))
        })?;
        Ok(Self {
            key_id: key_id.to_string(),
            issuer_id: issuer_id.to_string(),
            pkcs8,
        })
    }

    /// A signed ES256 token (JWT) valid for `TOKEN_TTL_SECS` from `now`.
    pub fn token_at(&self, now: u64) -> Result<String> {
        let b64 = |v: &Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string().as_bytes())
        };
        let header = json!({ "alg": "ES256", "kid": self.key_id, "typ": "JWT" });
        let claims = json!({
            "iss": self.issuer_id,
            "iat": now,
            "exp": now + TOKEN_TTL_SECS,
            "aud": "appstoreconnect-v1",
        });
        let signing_input = format!("{}.{}", b64(&header), b64(&claims));
        let rng = SystemRandom::new();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &self.pkcs8, &rng)
            .map_err(|e| config(&format!("App Store Connect key: unusable: {e}")))?;
        // FIXED signing gives r||s (64 bytes), the JWS ES256 format.
        let sig = pair
            .sign(&rng, signing_input.as_bytes())
            .map_err(|_| config("App Store Connect key: signing failed"))?;
        Ok(format!(
            "{signing_input}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.as_ref())
        ))
    }

    pub fn token(&self) -> Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.token_at(now)
    }
}

/// A registered device.
#[derive(Debug, Clone, PartialEq)]
pub struct AscDevice {
    pub id: String,
    pub udid: String,
    pub name: String,
    pub enabled: bool,
}

/// A development certificate the team can sign with.
#[derive(Debug, Clone, PartialEq)]
pub struct AscCertificate {
    pub id: String,
    pub name: String,
    pub expires: String,
}

pub struct AscClient {
    key: AscKey,
    http: reqwest::Client,
    base: String,
}

impl AscClient {
    pub fn new(key: AscKey) -> Self {
        let base = velocity_common::env::var("VELOCITY_ASC_API_URL").unwrap_or_else(|_| API.into());
        Self {
            key,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base: base.trim_end_matches('/').to_string(),
        }
    }

    /// A client for the API at `base` (tests use a fake server).
    pub fn with_base(key: AscKey, base: &str) -> Self {
        let mut c = Self::new(key);
        c.base = base.trim_end_matches('/').to_string();
        c
    }

    /// `(status, body)` for one request; the caller decides what's an error.
    ///
    /// Rate limits (429), server errors (5xx) and network failures are
    /// retried up to [`ATTEMPTS`] times, waiting what `Retry-After` asks
    /// (capped) or a short backoff; when they persist they are
    /// `ServiceUnavailable`, a transient error, not a config error
    /// (VEL-297). A POST is retried only when Apple surely didn't act on it
    /// (429, or no connection): a 5xx may have created the record.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let idempotent = method != reqwest::Method::POST;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut req = self
                .http
                .request(method.clone(), format!("{}{path}", self.base))
                .bearer_auth(self.key.token()?);
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    if attempt < ATTEMPTS && (idempotent || e.is_connect()) {
                        tokio::time::sleep(backoff(attempt, None)).await;
                        continue;
                    }
                    return Err(unavailable(format!("{e}")));
                }
            };
            let status = resp.status().as_u16();
            if status == 429 || (500..600).contains(&status) {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok());
                if attempt < ATTEMPTS && (idempotent || status == 429) {
                    tokio::time::sleep(backoff(attempt, wait)).await;
                    continue;
                }
                let value = resp.json::<Value>().await.unwrap_or(Value::Null);
                return Err(unavailable(format!(
                    "HTTP {status} after {attempt} attempt(s): {}",
                    error_detail(&value)
                )));
            }
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none")
                .to_string();
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| unavailable(format!("reading the response: {e}")))?;
            let value = match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) => v,
                // A 2xx that isn't JSON (a proxy's login page) read as an
                // empty list (VEL-300).
                Err(_) if (200..300).contains(&status) && !bytes.is_empty() => {
                    return Err(config(&format!(
                        "App Store Connect {status}: the response is not JSON (content-type {content_type})"
                    )));
                }
                Err(_) => Value::Null,
            };
            return Ok((status, value));
        }
    }

    /// A list endpoint's items across every page (`links.next`): lists are
    /// capped at 200 a page, so a team's 201st device was missing (VEL-298).
    /// Only pages on this API's own host are followed (they carry the token).
    async fn all(&self, path: &str) -> Result<Value> {
        let mut items = Vec::new();
        let mut next = Some(path.to_string());
        let mut pages = 0;
        while let Some(p) = next.take() {
            pages += 1;
            let v = self.ok(reqwest::Method::GET, &p, None).await?;
            items.extend(v["data"].as_array().cloned().unwrap_or_default());
            if pages >= MAX_PAGES {
                break;
            }
            next = v["links"]["next"]
                .as_str()
                .and_then(|u| u.strip_prefix(&self.base))
                .filter(|rest| rest.starts_with('/'))
                .map(String::from);
        }
        Ok(json!({ "data": items }))
    }

    async fn ok(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let (status, value) = self.send(method, path, body).await?;
        if (200..300).contains(&status) {
            Ok(value)
        } else {
            Err(api_error(status, &value))
        }
    }

    /// One cheap call that proves the key works (and has access).
    pub async fn check(&self) -> Result<()> {
        self.ok(reqwest::Method::GET, "/v1/devices?limit=1", None)
            .await
            .map(|_| ())
    }

    pub async fn devices(&self) -> Result<Vec<AscDevice>> {
        let v = self
            .all("/v1/devices?filter[platform]=IOS&limit=200")
            .await?;
        Ok(parse_devices(&v))
    }

    /// Register `udid` with the team unless it already is; returns the device
    /// and whether it was added (or re-enabled) now. Apple answers 409 for a
    /// known UDID. A disabled device is enabled again: as it was, it read
    /// "already registered" and was left out of every profile (VEL-299).
    pub async fn ensure_device(&self, udid: &str, name: &str) -> Result<(AscDevice, bool)> {
        let find = format!("/v1/devices?filter[udid]={udid}&limit=1");
        if let Some(d) = parse_devices(&self.ok(reqwest::Method::GET, &find, None).await?).pop() {
            if d.enabled {
                return Ok((d, false));
            }
            return self.enable_device(d).await.map(|d| (d, true));
        }
        let body = json!({ "data": { "type": "devices",
            "attributes": { "name": name, "udid": udid, "platform": "IOS" } } });
        let (status, value) = self
            .send(reqwest::Method::POST, "/v1/devices", Some(body))
            .await?;
        match status {
            200..=299 => parse_devices(&json!({ "data": [value["data"]] }))
                .pop()
                .map(|d| (d, true))
                .ok_or_else(|| config("App Store Connect: device response had no device")),
            409 => parse_devices(&self.ok(reqwest::Method::GET, &find, None).await?)
                .pop()
                .map(|d| (d, false))
                .ok_or_else(|| api_error(status, &value)),
            _ => Err(api_error(status, &value)),
        }
    }

    /// Set a disabled device's status back to ENABLED.
    async fn enable_device(&self, d: AscDevice) -> Result<AscDevice> {
        let body = json!({ "data": { "type": "devices", "id": d.id,
            "attributes": { "status": "ENABLED" } } });
        let v = self
            .ok(
                reqwest::Method::PATCH,
                &format!("/v1/devices/{}", d.id),
                Some(body),
            )
            .await
            .map_err(|e| {
                config(&format!(
                    "device {} is registered but disabled, and enabling it failed: {e}",
                    d.udid
                ))
            })?;
        Ok(parse_devices(&json!({ "data": [v["data"]] }))
            .pop()
            .unwrap_or(AscDevice { enabled: true, ..d }))
    }

    /// The bundle id record for `identifier`, created if missing. Returns its
    /// API id and whether it was created.
    pub async fn ensure_bundle_id(&self, identifier: &str, name: &str) -> Result<(String, bool)> {
        // `filter[identifier]` matches prefixes too: check equality here.
        // Every page: the exact one may be past the first 200 prefix
        // matches, and creating it then fails with 409 (VEL-298).
        let find = format!("/v1/bundleIds?filter[identifier]={identifier}&limit=200");
        let list = self.all(&find).await?;
        if let Some(id) = exact_bundle_id(&list, identifier) {
            return Ok((id, false));
        }
        let body = json!({ "data": { "type": "bundleIds",
            "attributes": { "identifier": identifier, "name": name, "platform": "IOS" } } });
        let (status, v) = self
            .send(reqwest::Method::POST, "/v1/bundleIds", Some(body))
            .await?;
        if status == 409 {
            // Created meanwhile (or listed late): use it.
            return exact_bundle_id(&self.all(&find).await?, identifier)
                .map(|id| (id, false))
                .ok_or_else(|| api_error(status, &v));
        }
        if !(200..300).contains(&status) {
            return Err(api_error(status, &v));
        }
        v["data"]["id"]
            .as_str()
            .map(|id| (id.to_string(), true))
            .ok_or_else(|| config("App Store Connect: bundle id response had no id"))
    }

    pub async fn development_certificates(&self) -> Result<Vec<AscCertificate>> {
        let v = self
            .all("/v1/certificates?filter[certificateType]=DEVELOPMENT,IOS_DEVELOPMENT&limit=200")
            .await?;
        Ok(parse_certificates(&v))
    }

    /// Create a development profile and return its contents (the
    /// `.mobileprovision` bytes). A new, uniquely named profile every time:
    /// existing ones are left alone.
    pub async fn create_development_profile(
        &self,
        name: &str,
        bundle_id: &str,
        certificate_ids: &[String],
        device_ids: &[String],
    ) -> Result<Vec<u8>> {
        let v = self
            .ok(
                reqwest::Method::POST,
                "/v1/profiles",
                Some(profile_body(name, bundle_id, certificate_ids, device_ids)),
            )
            .await?;
        profile_content(&v)
    }
}

/// Tries per request for rate limits, 5xx and network failures.
const ATTEMPTS: u32 = 3;
/// Pages followed per list (200 items each).
const MAX_PAGES: u32 = 50;

/// The wait before retry `attempt`: `Retry-After` when given (at most 30 s),
/// else 1 s, 2 s, …
fn backoff(attempt: u32, retry_after: Option<u64>) -> std::time::Duration {
    let units = retry_after.map_or(u64::from(attempt), |s| s.min(30));
    // Tests wait milliseconds, not seconds.
    if cfg!(test) {
        std::time::Duration::from_millis(units)
    } else {
        std::time::Duration::from_secs(units)
    }
}

fn unavailable(detail: String) -> VelocityError {
    VelocityError::ServiceUnavailable {
        service: "App Store Connect".into(),
        detail,
    }
}

fn config(msg: &str) -> VelocityError {
    VelocityError::Config(msg.to_string())
}

/// Apple's error (`errors[0]`) as a readable message.
fn api_error(status: u16, body: &Value) -> VelocityError {
    let detail = error_detail(body);
    let hint = match status {
        401 => {
            " (check VELOCITY_ASC_KEY_ID / VELOCITY_ASC_ISSUER_ID and that the key isn't revoked)"
        }
        403 => " (the key's role can't do this: it needs Admin or App Manager / Developer access)",
        _ => "",
    };
    config(&format!("App Store Connect {status}: {detail}{hint}"))
}

fn error_detail(body: &Value) -> &str {
    let e = &body["errors"][0];
    e["detail"]
        .as_str()
        .or_else(|| e["title"].as_str())
        .unwrap_or("no details")
}

fn parse_devices(v: &Value) -> Vec<AscDevice> {
    v["data"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|d| {
                    let a = &d["attributes"];
                    Some(AscDevice {
                        id: d["id"].as_str()?.to_string(),
                        udid: a["udid"].as_str()?.to_string(),
                        name: a["name"].as_str().unwrap_or("").to_string(),
                        enabled: a["status"].as_str() != Some("DISABLED"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_certificates(v: &Value) -> Vec<AscCertificate> {
    v["data"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|c| {
                    let a = &c["attributes"];
                    Some(AscCertificate {
                        id: c["id"].as_str()?.to_string(),
                        name: a["name"].as_str().unwrap_or("").to_string(),
                        expires: a["expirationDate"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn exact_bundle_id(list: &Value, identifier: &str) -> Option<String> {
    list["data"].as_array()?.iter().find_map(|b| {
        (b["attributes"]["identifier"].as_str() == Some(identifier))
            .then(|| b["id"].as_str().map(String::from))
            .flatten()
    })
}

fn profile_body(
    name: &str,
    bundle_id: &str,
    certificate_ids: &[String],
    device_ids: &[String],
) -> Value {
    let refs = |kind: &str, ids: &[String]| {
        Value::Array(
            ids.iter()
                .map(|id| json!({ "type": kind, "id": id }))
                .collect(),
        )
    };
    json!({ "data": {
        "type": "profiles",
        "attributes": { "name": name, "profileType": "IOS_APP_DEVELOPMENT" },
        "relationships": {
            "bundleId": { "data": { "type": "bundleIds", "id": bundle_id } },
            "certificates": { "data": refs("certificates", certificate_ids) },
            "devices": { "data": refs("devices", device_ids) },
        }
    } })
}

fn profile_content(v: &Value) -> Result<Vec<u8>> {
    let b64 = v["data"]["attributes"]["profileContent"]
        .as_str()
        .ok_or_else(|| config("App Store Connect: profile response had no content"))?;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| {
            config(&format!(
                "App Store Connect: profile content isn't base64 ({e})"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::fake_wda::*;
    use ring::signature::{KeyPair, UnparsedPublicKey, ECDSA_P256_SHA256_FIXED};

    /// A throwaway P-256 key as a `.p8` PEM, and its public key.
    fn test_key() -> (String, Vec<u8>) {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(pkcs8.as_ref());
        let body: Vec<String> = b64
            .as_bytes()
            .chunks(64)
            .map(|c| String::from_utf8(c.to_vec()).unwrap())
            .collect();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            body.join("\n")
        );
        (pem, pair.public_key().as_ref().to_vec())
    }

    #[test]
    fn the_token_is_an_es256_jwt_apple_can_verify() {
        let (pem, public) = test_key();
        let key =
            AscKey::from_pem(&pem, "ABC123DEFG", "69a6de7f-0000-1111-2222-333344445555").unwrap();
        let token = key.token_at(1_700_000_000).unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let dec = |s: &str| -> Value {
            serde_json::from_slice(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(s)
                    .unwrap(),
            )
            .unwrap()
        };
        let (header, claims) = (dec(parts[0]), dec(parts[1]));
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["kid"], "ABC123DEFG");
        assert_eq!(header["typ"], "JWT");
        assert_eq!(claims["aud"], "appstoreconnect-v1");
        assert_eq!(claims["iss"], "69a6de7f-0000-1111-2222-333344445555");
        assert_eq!(claims["iat"], 1_700_000_000u64);
        assert_eq!(claims["exp"], 1_700_000_000u64 + TOKEN_TTL_SECS);
        // The signature is r||s and verifies with the key's public half.
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[2])
            .unwrap();
        assert_eq!(sig.len(), 64);
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &public)
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .expect("signature verifies");
    }

    #[test]
    fn a_bad_key_file_is_reported_and_never_echoed() {
        let err = AscKey::from_pem(
            "-----BEGIN PRIVATE KEY-----\nnot-base64!!\n-----END PRIVATE KEY-----",
            "k",
            "i",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not a PEM"), "{err}");
        let (pem, _) = test_key();
        let key = AscKey::from_pem(&pem, "KID", "ISS").unwrap();
        let shown = format!("{key:?}");
        assert!(
            shown.contains("KID") && !shown.contains(&pem[30..60]),
            "{shown}"
        );
    }

    #[test]
    fn a_bundle_id_must_match_exactly_not_by_prefix() {
        let list = json!({ "data": [
            { "id": "A1", "attributes": { "identifier": "com.acme.velocity.WebDriverAgentRunner.extra" } },
            { "id": "B2", "attributes": { "identifier": "com.acme.velocity.WebDriverAgentRunner" } }
        ] });
        assert_eq!(
            exact_bundle_id(&list, "com.acme.velocity.WebDriverAgentRunner").as_deref(),
            Some("B2")
        );
        assert_eq!(exact_bundle_id(&list, "com.acme.velocity"), None);
    }

    #[test]
    fn devices_certificates_and_profiles_parse() {
        let devices = parse_devices(&json!({ "data": [
            { "id": "D1", "attributes": { "udid": "00008110-001", "name": "Test iPhone", "status": "ENABLED" } },
            { "id": "D2", "attributes": { "udid": "00008110-002", "name": "Old", "status": "DISABLED" } }
        ] }));
        assert_eq!(devices.len(), 2);
        assert!(devices[0].enabled && !devices[1].enabled);
        let certs = parse_certificates(&json!({ "data": [
            { "id": "C1", "attributes": { "name": "Apple Development: CI", "expirationDate": "2027-01-01T00:00:00Z" } }
        ] }));
        assert_eq!(certs[0].id, "C1");
        let body = profile_body(
            "Velocity runner",
            "B2",
            &["C1".into()],
            &["D1".into(), "D2".into()],
        );
        assert_eq!(
            body["data"]["attributes"]["profileType"],
            "IOS_APP_DEVELOPMENT"
        );
        assert_eq!(
            body["data"]["relationships"]["devices"]["data"][1]["id"],
            "D2"
        );
        let content = base64::engine::general_purpose::STANDARD.encode(b"PROFILE");
        let got =
            profile_content(&json!({ "data": { "attributes": { "profileContent": content } } }))
                .unwrap();
        assert_eq!(got, b"PROFILE");
    }

    #[test]
    fn api_errors_explain_the_likely_fix() {
        let body = json!({ "errors": [ { "title": "Authentication credentials are missing or invalid." } ] });
        let e = api_error(401, &body).to_string();
        assert!(
            e.contains("401") && e.contains("VELOCITY_ASC_KEY_ID"),
            "{e}"
        );
        let e = api_error(403, &json!({ "errors": [ { "detail": "forbidden" } ] })).to_string();
        assert!(e.contains("role"), "{e}");
    }

    fn client(url: &str) -> AscClient {
        let (pem, _) = test_key();
        AscClient::with_base(AscKey::from_pem(&pem, "K", "I").unwrap(), url)
    }

    fn device(id: &str, udid: &str, status: &str) -> String {
        format!(
            r#"{{"id":"{id}","attributes":{{"udid":"{udid}","name":"n","status":"{status}"}}}}"#
        )
    }

    #[test]
    fn a_key_with_a_bom_or_an_indented_header_loads() {
        // VEL-301: the header line became part of the base64.
        let (pem, _) = test_key();
        AscKey::from_pem(&format!("\u{feff}{pem}"), "K", "I").unwrap();
        let indented: String = pem.lines().map(|l| format!("  {l}\r\n")).collect();
        AscKey::from_pem(&indented, "K", "I").unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_2xx_that_is_not_json_is_an_error_not_an_empty_list() {
        // VEL-300: a proxy's HTML page read as "no devices".
        let f = FakeWda::start(|_| Resp::Raw(200, "<html>sign in</html>".into())).await;
        let e = client(&f.url).devices().await.unwrap_err().to_string();
        assert!(e.contains("not JSON"), "{e}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rate_limits_and_server_errors_are_retried_then_transient() {
        // VEL-297: a 429 or 503 was a config error, never retried.
        let n = std::sync::Arc::new(std::sync::Mutex::new(0));
        let n2 = n.clone();
        let f = FakeWda::start(move |_| {
            let mut n = n2.lock().unwrap();
            *n += 1;
            match *n {
                1 => Resp::Json(429, r#"{"errors":[{"detail":"slow down"}]}"#.into()),
                2 => Resp::Json(503, "{}".into()),
                _ => Resp::Json(
                    200,
                    format!(r#"{{"data":[{}]}}"#, device("D1", "U1", "ENABLED")),
                ),
            }
        })
        .await;
        assert_eq!(client(&f.url).devices().await.unwrap().len(), 1);
        assert_eq!(*n.lock().unwrap(), 3);

        let f = FakeWda::start(|_| Resp::Json(503, "{}".into())).await;
        let e = client(&f.url).devices().await.unwrap_err();
        assert!(
            matches!(e, VelocityError::ServiceUnavailable { .. }),
            "{e:?}"
        );
        assert_eq!(f.count("GET", "/v1/devices"), ATTEMPTS as usize);
        assert_eq!(e.kind(), velocity_common::ErrorKind::ConnectionLost);
        // A POST that may have been acted on is not repeated.
        let e = client(&f.url)
            .create_development_profile("p", "B", &[], &[])
            .await
            .unwrap_err();
        assert!(
            matches!(e, VelocityError::ServiceUnavailable { .. }),
            "{e:?}"
        );
        assert_eq!(f.count("POST", "/v1/profiles"), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lists_follow_every_page_on_the_same_host_only() {
        // VEL-298: only the first 200 devices / bundle ids were seen.
        let base = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let b2 = base.clone();
        let f = FakeWda::start(move |r| {
            let base = b2.lock().unwrap().clone();
            if r.path == "/v1/devices" && r.query.contains("cursor=2") {
                return Resp::Json(
                    200,
                    format!(
                        r#"{{"data":[{}],"links":{{"next":"https://evil.example/v1/devices?cursor=3"}}}}"#,
                        device("D2", "U2", "ENABLED")
                    ),
                );
            }
            if r.path == "/v1/devices" {
                return Resp::Json(
                    200,
                    format!(
                        r#"{{"data":[{}],"links":{{"next":"{base}/v1/devices?cursor=2"}}}}"#,
                        device("D1", "U1", "ENABLED")
                    ),
                );
            }
            if r.path == "/v1/bundleIds" && r.method == "GET" && r.query.contains("cursor=2") {
                return Resp::Json(200, r#"{"data":[{"id":"B9","attributes":{"identifier":"com.acme.app"}}]}"#.into());
            }
            if r.path == "/v1/bundleIds" && r.method == "GET" {
                return Resp::Json(
                    200,
                    format!(
                        r#"{{"data":[{{"id":"B1","attributes":{{"identifier":"com.acme.app.other"}}}}],"links":{{"next":"{base}/v1/bundleIds?cursor=2"}}}}"#
                    ),
                );
            }
            Resp::Json(409, r#"{"errors":[{"detail":"exists"}]}"#.into())
        })
        .await;
        *base.lock().unwrap() = f.url.clone();
        let c = client(&f.url);
        let ids: Vec<String> = c
            .devices()
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(ids, ["D1", "D2"], "page 2 read, the foreign host never");
        assert_eq!(f.count("GET", "/v1/devices"), 2);
        assert_eq!(
            c.ensure_bundle_id("com.acme.app", "x").await.unwrap(),
            ("B9".to_string(), false)
        );
        assert_eq!(f.count("POST", "/v1/bundleIds"), 0, "found on page 2");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_disabled_device_is_enabled_not_reported_as_registered() {
        // VEL-299: a disabled device read "already registered" and was then
        // left out of the profile.
        let f = FakeWda::start(|r| {
            if r.method == "PATCH" {
                assert!(r.body.contains("ENABLED"), "{}", r.body);
                return Resp::Json(
                    200,
                    format!(r#"{{"data":{}}}"#, device("D7", "U7", "ENABLED")),
                );
            }
            Resp::Json(
                200,
                format!(r#"{{"data":[{}]}}"#, device("D7", "U7", "DISABLED")),
            )
        })
        .await;
        let (d, changed) = client(&f.url).ensure_device("U7", "n").await.unwrap();
        assert!(d.enabled && changed, "{d:?}");
        assert_eq!(f.count("PATCH", "/v1/devices/D7"), 1);
    }
}
