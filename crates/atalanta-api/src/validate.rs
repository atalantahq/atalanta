//! What can be checked in an `http` step before anything runs. A part
//! with a `${…}` in it is checked when it runs, with the value in.

use velocity_common::{Capture, HttpRequest};

use crate::check;
use crate::path::Path;

/// Most `retry.count`: a poll that needs more wants a longer interval.
pub const MAX_RETRIES: u32 = 100;

pub fn validate(req: &HttpRequest) -> Result<(), String> {
    let fixed = |s: &str| !s.contains("${");
    if req.url.trim().is_empty() {
        return Err("needs a url".into());
    }
    if let Some(m) = req.method.as_deref().filter(|m| fixed(m)) {
        // A method is a token (RFC 9110), as the client takes it: `M-SEARCH`
        // and `VERSION-CONTROL` are methods.
        let ok = !m.is_empty() && m.bytes().all(is_token_byte);
        if !ok {
            return Err(format!(
                "`{m}` is not an HTTP method (GET, POST, PUT, PATCH, DELETE, …)"
            ));
        }
    }
    let bodies = [req.json.is_some(), req.body.is_some(), !req.form.is_empty()];
    if bodies.iter().filter(|b| **b).count() > 1 {
        return Err("takes one body: json, body or form".into());
    }
    for (name, _) in &req.headers {
        if name.is_empty() || !name.bytes().all(is_token_byte) {
            return Err(format!("`{name}` is not a header name"));
        }
    }
    let e = &req.expect;
    for (name, m) in &e.headers {
        check::validate(m).map_err(|err| format!("expect.headers.{name}: {err}"))?;
    }
    for (path, m) in &e.json {
        if fixed(path) {
            Path::parse(path).map_err(|err| format!("expect.json: {err}"))?;
        }
        check::validate(m).map_err(|err| format!("expect.json.{path}: {err}"))?;
    }
    if let Some(m) = &e.body {
        check::validate(m).map_err(|err| format!("expect.body: {err}"))?;
    }
    for (name, from) in &req.capture {
        if !is_var_name(name) {
            return Err(format!(
                "capture `{name}`: a name is letters, digits and `_`, not starting with a digit (it is used as ${{{name}}})"
            ));
        }
        match from {
            Capture::Json(p) if fixed(p) => {
                Path::parse(p).map_err(|err| format!("capture {name}: {err}"))?;
            }
            Capture::Regex(r) if fixed(r) => {
                regex::Regex::new(r).map_err(|err| {
                    format!("capture {name}: bad regular expression /{r}/: {err}")
                })?;
            }
            _ => {}
        }
    }
    if let Some(r) = &req.retry {
        if r.count > MAX_RETRIES {
            return Err(format!(
                "retry.count is at most {MAX_RETRIES} (give a longer intervalMs instead)"
            ));
        }
    }
    Ok(())
}

/// What `${NAME}` can name.
pub fn is_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// RFC 9110 `tchar`.
pub(crate) fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(json: &str) -> Result<(), String> {
        let req: HttpRequest = serde_json::from_str(json).unwrap();
        validate(&req)
    }

    #[test]
    fn mistakes_are_found_before_the_run() {
        assert!(v(r#"{"url": "/x"}"#).is_ok());
        assert!(v(r#"{"url": " "}"#).unwrap_err().contains("needs a url"));
        assert!(v(r#"{"url": "/x", "method": "GE T"}"#)
            .unwrap_err()
            .contains("not an HTTP method"));
        // Any token is a method, as the client sends it.
        assert!(v(r#"{"url": "/x", "method": "M-SEARCH"}"#).is_ok());
        assert!(v(r#"{"url": "/x", "method": "m-search"}"#).is_ok());
        assert!(v(r#"{"url": "/x", "method": "GET /"}"#).is_err());
        assert!(v(r#"{"url": "/x", "json": {}, "body": "a"}"#)
            .unwrap_err()
            .contains("one body"));
        assert!(v(r#"{"url": "/x", "headers": {"a b": "1"}}"#)
            .unwrap_err()
            .contains("not a header name"));
        let e = v(r#"{"url": "/x", "expect": {"json": {"$.a[": 1}}}"#).unwrap_err();
        assert!(e.starts_with("expect.json: JSONPath `$.a[`"), "{e}");
        let e = v(r#"{"url": "/x", "expect": {"body": {"matches": "("}}}"#).unwrap_err();
        assert!(e.contains("expect.body: bad regular expression"), "{e}");
        let e = v(r#"{"url": "/x", "capture": {"order-id": "$.id"}}"#).unwrap_err();
        assert!(e.contains("letters, digits and `_`"), "{e}");
        let e = v(r#"{"url": "/x", "capture": {"A": {"regex": "("}}}"#).unwrap_err();
        assert!(e.contains("capture A: bad regular expression"), "{e}");
        assert!(v(r#"{"url": "/x", "retry": {"count": 101}}"#)
            .unwrap_err()
            .contains("at most 100"));
    }

    #[test]
    fn a_part_with_a_variable_waits_for_its_value() {
        assert!(v(r#"{"url": "/x", "method": "${VERB}"}"#).is_ok());
        assert!(v(r#"{"url": "/x", "expect": {"json": {"$.a[?@.id=='${ID}'": 1}}}"#).is_ok());
    }
}
