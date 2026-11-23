// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `--confirm`: send the scaffold's GET and HEAD requests to a real server
//! and drop each mined check its response disproves. A dropped check is a
//! finding, not noise: the server and its spec disagree there. Nothing
//! else is ever sent, whatever the spec says: a POST, PUT, PATCH or DELETE
//! changes the server, and this runs against whatever `--base-url` names.

use std::collections::HashMap;

use atalanta_common::{AtalantaError, HttpConfig, HttpExpect};

use super::Scaffold;
use crate::client::excerpt;
use crate::HttpSession;

/// The only methods confirm mode sends.
pub const SAFE_METHODS: [&str; 2] = ["GET", "HEAD"];

#[derive(Debug, Default)]
pub struct ConfirmReport {
    /// Checks a response disproved, now gone from the suite.
    pub dropped: Vec<Finding>,
    /// Checks a response bore out.
    pub held: usize,
    /// Responses that weren't the documented success: their checks stay,
    /// unconfirmed.
    pub unconfirmed: Vec<Finding>,
    /// Requests not sent, and why.
    pub skipped: Vec<Finding>,
    /// Requests that got a response.
    pub sent: usize,
    /// The first request that got none (connection refused, timeout).
    pub unreachable: Option<String>,
    /// Suite-wide: a `config.http.headers` left out for want of its value.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub test: String,
    pub request: String,
    /// The check's JSONPath; empty for a finding about the whole request.
    pub path: String,
    pub why: String,
}

/// `env` gives a `${NAME}`'s value (the process environment, in the CLI):
/// a token, or an id that a POST would have captured.
pub async fn confirm(
    s: &mut Scaffold,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ConfirmReport, AtalantaError> {
    let mut report = ConfirmReport::default();
    let none = HashMap::new();
    let mut config = HttpConfig {
        base_url: match s.config.base_url.as_deref().map(|b| fill(b, &none, env)) {
            Some(Ok(b)) => Some(b),
            Some(Err(name)) => {
                return Err(AtalantaError::Config(format!(
                    "--confirm needs the server: set {name} or pass --base-url"
                )))
            }
            None => None,
        },
        ..s.config.clone()
    };
    config
        .headers
        .retain_mut(|(h, v)| match fill(v, &none, env) {
            Ok(filled) => {
                *v = filled;
                true
            }
            Err(name) => {
                report
                    .notes
                    .push(format!("{h} not sent: {name} is not set"));
                false
            }
        });

    for test in &mut s.tests {
        let session = HttpSession::new(&config)?;
        let timeout = session.request_timeout(None);
        let mut captured: HashMap<String, String> = HashMap::new();
        for step in &mut test.steps {
            let finding = |path: &str, why: String| Finding {
                test: test.name.clone(),
                request: step.label.clone(),
                path: path.to_string(),
                why,
            };
            let method = step.request.method();
            if !SAFE_METHODS.contains(&method.as_str()) {
                report.skipped.push(finding(
                    "",
                    format!("{method} is never sent in confirm mode"),
                ));
                continue;
            }
            let mut req = step.request.clone();
            req.expect = HttpExpect::default();
            req.capture.clear();
            let filled =
                req.map_strings(&mut |t| fill(t, &captured, env).map_err(AtalantaError::Config));
            if let Err(AtalantaError::Config(name)) = filled {
                report.skipped.push(finding(
                    "",
                    format!("needs {name}: set it, or it comes from a request confirm mode doesn't send"),
                ));
                continue;
            }
            let (mut exchange, _) = match session.send(&req, timeout).await {
                Ok(x) => x,
                Err(e) => {
                    let why = e.to_string();
                    if matches!(e, AtalantaError::HttpUnreachable { .. }) {
                        report.unreachable.get_or_insert_with(|| why.clone());
                    }
                    report.skipped.push(finding("", why));
                    continue;
                }
            };
            report.sent += 1;
            let documented = match &step.request.expect.status {
                Some(want) => want.accepts(exchange.status),
                None => exchange.status < 400,
            };
            if !documented {
                let want = step
                    .request
                    .expect
                    .status
                    .as_ref()
                    .map_or_else(|| "below 400".to_string(), ToString::to_string);
                report.unconfirmed.push(finding(
                    "",
                    format!(
                        "answered {}, the spec documents {want}: {}",
                        exchange.status,
                        excerpt(&exchange.body)
                    ),
                ));
                continue;
            }
            let mut kept = Vec::new();
            for (path, m) in std::mem::take(&mut step.request.expect.json) {
                let one = HttpExpect {
                    json: vec![(path.clone(), m.clone())],
                    ..Default::default()
                };
                let failures = exchange.check(&one);
                if failures.is_empty() {
                    report.held += 1;
                    kept.push((path, m));
                } else {
                    let why = failures.join("; ");
                    step.notes.push(format!(
                        "--confirm dropped `{path}`: the server disagrees with the spec ({why})"
                    ));
                    report.dropped.push(finding(&path, why));
                }
            }
            step.request.expect.json = kept;
            for (name, from) in &step.request.capture {
                if let Ok(v) = exchange.capture(from) {
                    captured.insert(name.clone(), v);
                }
            }
        }
    }
    Ok(report)
}

/// `${NAME}` and `${NAME:-default}` from what was captured, then `env`;
/// the name of the first with no value as the error.
fn fill(
    text: &str,
    captured: &HashMap<String, String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        // A run of `$` right before the `{`: each `$$` is one literal `$`,
        // so `$${X}` is the text `${X}` (the suite loader's rule, D-13,
        // which the scaffold uses to keep a spec's own `${...}` inert).
        let before = &rest[..start];
        let k = before.len() - before.trim_end_matches('$').len();
        out.push_str(&before[..before.len() - k]);
        out.push_str(&"$".repeat(k.div_ceil(2)));
        if (k + 1).is_multiple_of(2) {
            out.push('{');
            rest = &rest[start + 2..];
            continue;
        }
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let inner = &after[..end];
        let (name, default) = match inner.split_once(":-") {
            Some((n, d)) => (n, Some(d)),
            None => (inner, None),
        };
        let value = captured
            .get(name)
            .cloned()
            .or_else(|| env(name))
            .or_else(|| default.map(str::to_string))
            .ok_or_else(|| format!("${{{name}}}"))?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_come_from_captures_then_the_environment() {
        let captured = HashMap::from([("PET_ID".to_string(), "7".to_string())]);
        let env = |n: &str| (n == "API_TOKEN").then(|| "t".to_string());
        assert_eq!(
            fill(
                "/pets/${PET_ID}?k=${API_TOKEN}&p=${PAGE:-1}",
                &captured,
                &env
            ),
            Ok("/pets/7?k=t&p=1".into())
        );
        assert_eq!(
            fill("/o/${ORDER_ID}", &captured, &env),
            Err("${ORDER_ID}".into())
        );
        assert_eq!(fill("a ${ b", &captured, &env), Ok("a ${ b".into()));
        // `$${X}` is the text `${X}`, never X's value; `$$${X}` is `$` and the value.
        assert_eq!(
            fill("$${API_TOKEN}/$$${API_TOKEN}/$5", &captured, &env),
            Ok("${API_TOKEN}/$t/$5".into())
        );
    }
}
