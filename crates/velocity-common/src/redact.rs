//! Secret redaction for everything Velocity writes or prints: reports,
//! traces, the live console, MCP replies, natural-language traces.
//!
//! Two layers:
//! - **Known values.** The values of `${VAR}`s whose names look secret
//!   (`PASSWORD`, `TOKEN`, `SECRET`, `KEY`, `PIN`, `OTP`, …) or are listed in
//!   `VELOCITY_SECRETS` are registered when the suite is interpolated, and
//!   every occurrence is replaced.
//! - **Shapes.** Bearer tokens, auth/API-key headers, `?token=` style URL
//!   parameters, JSON `"password": "…"` fields, JWTs and well-known key
//!   prefixes (`sk-`, `ghp_`, `AIza`, `AKIA`, `xox…`, …).
//!
//! Redaction never fails and never changes text that holds no secret.

use std::borrow::Cow;
use std::sync::{LazyLock, RwLock};

use regex::Regex;

/// What a secret is replaced with.
pub const MASK: &str = "[redacted]";

/// Values shorter than this aren't registered: masking "1" or "on" would
/// mangle ordinary text.
const MIN_SECRET_LEN: usize = 3;

/// Secrets shorter than this (PINs, CVVs, short passwords: 3–7
/// characters) are masked only as a whole token, never inside a word:
/// "abc" must not shred "abcdef", nor "1234" every order number holding
/// it. From this length on they are masked wherever they occur, and also
/// handed to GitHub's `::add-mask::` (D-29; GitLab won't mask under 8
/// either).
pub const WORD_ONLY_BELOW: usize = 8;

/// Every masked form of every secret, with the length (in characters) of
/// the secret it came from: a short secret's escaped form is longer, but
/// is still masked only as a whole token.
static SECRETS: LazyLock<RwLock<Vec<(String, usize)>>> = LazyLock::new(|| RwLock::new(Vec::new()));

/// (pattern, replacement): `$1` keeps the label part (`Bearer `, `token=`).
static SHAPES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]{8,}", "${1}[redacted]"),
        // The credential after an auth scheme word (`Basic <base64>`): the
        // scheme alone is 5 characters, so it was never what got masked and
        // the credential after it stayed in clear (V1).
        (
            r#"(?i)((?:authorization|x-api-key|api-key|x-auth-token)\s*[:=]\s*(?:(?:basic|bearer|token|negotiate|ntlm|apikey)\s+)?)[^\s,;"']{6,}"#,
            "${1}[redacted]",
        ),
        // A cookie header is a list (`a=1; b=2`): all of it, to the end of
        // the line (V2).
        (
            r#"(?i)\b((?:set-)?cookie\s*[:=]\s*)[^\r\n"']{6,}"#,
            "${1}[redacted]",
        ),
        (
            r"(?i)([?&](?:key|api_?key|token|access_token|refresh_token|id_token|secret|client_secret|password|passwd|pwd|sig|signature|auth)=)[^&\s#]+",
            "${1}[redacted]",
        ),
        // A JSON string value, escapes included: `[^"]*` stopped at the
        // `\"` inside a password and left the rest of it in clear (V3).
        (
            r#"(?i)("(?:password|passwd|pwd|secret|client_secret|token|access_token|refresh_token|id_token|api_?key|apikey|authorization|proxy-authorization|x-api-key|x-auth-token|cookie|set-cookie|private_key)"\s*:\s*")(?:[^"\\]|\\.)*(")"#,
            "${1}[redacted]${2}",
        ),
        // `scheme://user:password@host`: a password in a URL's userinfo (V4).
        (
            r"(?i)(\b[a-z][a-z0-9+.-]*://[^\s/?#@:]*:)[^\s/?#]+(@)",
            "${1}[redacted]${2}",
        ),
        (r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b", "[redacted]"),
        (r"\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{20,}", "[redacted]"),
        (r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}\b", "[redacted]"),
        (r"\bgithub_pat_[A-Za-z0-9_]{30,}\b", "[redacted]"),
        (r"\bglpat-[A-Za-z0-9_-]{20,}\b", "[redacted]"),
        (r"\bAIza[0-9A-Za-z_-]{35}\b", "[redacted]"),
        (r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", "[redacted]"),
        (r"\bxox[abprs]-[A-Za-z0-9-]{10,}\b", "[redacted]"),
        (r"\bhf_[A-Za-z0-9]{30,}\b", "[redacted]"),
        (r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----", "[redacted]"),
    ]
    .into_iter()
    .filter_map(|(p, r)| Regex::new(p).ok().map(|re| (re, r)))
    .collect()
});

/// Whether a variable named `name` holds a secret: its name looks like one,
/// or `VELOCITY_SECRETS` (comma-separated) lists it.
pub fn is_secret_name(name: &str) -> bool {
    // Whole words only (`_`, `-`, camelCase, acronym and digit runs split):
    // "PIN" must not catch SHIPPING_ADDRESS, nor "AUTH" AUTHOR. Acronyms and
    // digits split too, so APIToken, TOKEN2 and dbPASSWORD are caught
    // (VEL-075).
    let chars: Vec<char> = name.chars().collect();
    let mut words = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        let boundary = match prev {
            None => false,
            Some(p) => {
                (c.is_uppercase() && p.is_lowercase())
                    // The last capital of an acronym starts the next word:
                    // "APIToken" is API + Token.
                    || (c.is_uppercase() && p.is_uppercase() && next.is_some_and(char::is_lowercase))
                    || (c.is_ascii_digit() != p.is_ascii_digit()
                        && c.is_alphanumeric()
                        && p.is_alphanumeric())
            }
        };
        if (!c.is_alphanumeric() || boundary) && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        if c.is_alphanumeric() && !c.is_ascii_digit() {
            cur.push(c.to_ascii_uppercase());
        } else if c.is_ascii_digit() && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    const WORDS: &[&str] = &[
        "PASSWORD",
        "PASSWD",
        "PASS",
        "PWD",
        "PASSCODE",
        "PASSPHRASE",
        "PINCODE",
        "SECRET",
        "SECRETS",
        "TOKEN",
        "TOKENS",
        "APIKEY",
        "KEY",
        "PRIVATE",
        "PRIVATEKEY",
        "CREDENTIAL",
        "CREDENTIALS",
        "PIN",
        "OTP",
        "AUTH",
        "COOKIE",
        "SESSION",
        "JWT",
        "BEARER",
        "CVV",
        "CVC",
    ];
    if words.iter().any(|w| WORDS.contains(&w.as_str())) {
        return true;
    }
    listed_secret(name)
}

/// Whether `VELOCITY_SECRETS` (the `secrets` setting, comma-separated)
/// lists `name`.
pub fn listed_secret(name: &str) -> bool {
    crate::env::var("VELOCITY_SECRETS")
        .map(|list| list.split(',').any(|v| v.trim().eq_ignore_ascii_case(name)))
        .unwrap_or(false)
}

/// Names of the variables a suite used as secrets (`${NAME}` with a
/// secret-looking name, interpolated in this process).
static SECRET_NAMES: LazyLock<RwLock<std::collections::HashSet<String>>> =
    LazyLock::new(|| RwLock::new(std::collections::HashSet::new()));

/// Remember that a suite read variable `name` as a secret.
pub fn register_secret_name(name: &str) {
    SECRET_NAMES
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string());
}

/// Whether a variable may be read by name on request (MCP `type_text
/// {secret}`): listed in `VELOCITY_SECRETS`, or used as a secret by a
/// suite in this process. Anything else in the environment
/// (`AWS_SECRET_ACCESS_KEY`, `GITHUB_TOKEN`, …) stays out of reach.
pub fn is_allowed_secret_name(name: &str) -> bool {
    listed_secret(name)
        || SECRET_NAMES
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(name)
}

/// Mask `value` wherever it appears from now on, raw or in the escaped
/// forms messages and reports put it in.
///
/// A multi-line secret (a PEM key, a JSON credential) is also registered
/// line by line: output is printed, logged and wrapped a line at a time,
/// so the whole value rarely appears in one piece, and GitHub's
/// `::add-mask::` only masks single lines (C3, D-29 leftover). Each line
/// keeps the length rule of its own length (a short line is masked as a
/// whole token only).
pub fn register_secret(value: &str) {
    if value.contains(['\n', '\r']) {
        for line in secret_lines(value) {
            register_one(line);
        }
    }
    register_one(value);
}

/// The lines of a multi-line secret worth masking on their own: without
/// their line ending and surrounding blanks, empty ones dropped.
pub fn secret_lines(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|l| !l.is_empty())
}

fn register_one(value: &str) {
    if value.chars().count() < MIN_SECRET_LEN {
        return;
    }
    let raw_len = value.chars().count();
    let mut list = SECRETS.write().unwrap_or_else(|e| e.into_inner());
    for v in encoded_forms(value) {
        match list.iter_mut().find(|(f, _)| *f == v) {
            // The same text from a longer secret too: masked anywhere.
            Some(e) => e.1 = e.1.max(raw_len),
            None => list.push((v, raw_len)),
        }
    }
    // Longest first, so a secret containing another is masked whole.
    list.sort_by_key(|(v, _)| std::cmp::Reverse(v.len()));
}

/// Every masked form of every registered secret.
pub fn registered_secrets() -> Vec<String> {
    registered_secrets_at_least(0)
}

/// The masked forms of the registered secrets of at least `min_len`
/// characters (for GitHub's `::add-mask::`, which masks them as
/// substrings in the whole job's log: only secrets of 8 or more, D-29).
pub fn registered_secrets_at_least(min_len: usize) -> Vec<String> {
    SECRETS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(_, n)| *n >= min_len)
        .map(|(f, _)| f.clone())
        .collect()
}

/// `value` as it appears once quoted by Rust `{:?}` (errors format values
/// that way, and an error inside an error twice), as a JSON string, in
/// XML/HTML text or attributes, and percent-encoded in a URL. Matching only
/// the raw value let a password holding `"` or `\` through everywhere.
fn encoded_forms(value: &str) -> Vec<String> {
    fn unquote(s: String) -> String {
        s.strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .map(str::to_string)
            .unwrap_or(s)
    }
    let debug = unquote(format!("{value:?}"));
    let debug2 = unquote(format!("{debug:?}"));
    let json = serde_json::to_string(value)
        .map(unquote)
        .unwrap_or_default();
    let mut forms = vec![value.to_string(), debug, debug2, json];
    for quote in ["&quot;", "&#34;", "&#x22;"] {
        for apos in ["&apos;", "&#39;", "&#x27;"] {
            let mut x = String::new();
            for c in value.chars() {
                match c {
                    '&' => x.push_str("&amp;"),
                    '<' => x.push_str("&lt;"),
                    '>' => x.push_str("&gt;"),
                    '"' => x.push_str(quote),
                    '\'' => x.push_str(apos),
                    c => x.push(c),
                }
            }
            forms.push(x);
        }
    }
    let mut url = String::new();
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                url.push(b as char)
            }
            b => url.push_str(&format!("%{b:02X}")),
        }
    }
    forms.push(url.replace("%20", "+"));
    forms.push(lowercase_hex(&url));
    forms.push(url);
    forms.sort();
    forms.dedup();
    forms.retain(|f| f.chars().count() >= MIN_SECRET_LEN);
    forms
}

/// `%2F` → `%2f`: some encoders write lowercase hex.
fn lowercase_hex(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let mut rest = url;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let esc = rest.get(i..i + 3).unwrap_or(&rest[i..]);
        out.push_str(&esc.to_ascii_lowercase());
        rest = &rest[i + esc.len()..];
    }
    out.push_str(rest);
    out
}

/// `s` with every registered secret and secret-shaped string masked.
pub fn redact(s: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(s);
    {
        let list = SECRETS.read().unwrap_or_else(|e| e.into_inner());
        if let Some(masked) = mask_known(s, &list) {
            out = Cow::Owned(masked);
        }
    }
    let mut shaped = false;
    for (re, rep) in SHAPES.iter() {
        if re.is_match(&out) {
            out = Cow::Owned(re.replace_all(&out, *rep).into_owned());
            shaped = true;
        }
    }
    // A short secret glued to a masked shape ("sk-…pin") is a whole token
    // now: mask it too, or a second pass would (redaction is idempotent).
    if shaped {
        let list = SECRETS.read().unwrap_or_else(|e| e.into_inner());
        if let Some(masked) = mask_known(&out, &list) {
            out = Cow::Owned(masked);
        }
    }
    out
}

/// Every occurrence of every secret in `s`, in one pass over the original
/// text: overlapping matches are merged into one masked span. Replacing one
/// secret after another left the tail of an overlapping one in clear, and
/// could match inside an earlier "[redacted]" (VEL-232).
fn mask_known(s: &str, secrets: &[(String, usize)]) -> Option<String> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut short: Vec<(usize, usize)> = Vec::new();
    for (secret, raw_len) in secrets {
        let word_only = *raw_len < WORD_ONLY_BELOW;
        for (at, _) in s.match_indices(secret.as_str()) {
            let end = at + secret.len();
            if word_only {
                short.push((at, end));
            } else {
                spans.push((at, end));
            }
        }
    }
    // A short secret is a whole token when each neighbour is a non-word
    // character or is itself masked: "LONGSECRETpin" must not leave "pin"
    // in clear beside the mask (redaction is idempotent).
    let masked = |spans: &[(usize, usize)], c: usize| spans.iter().any(|&(a, b)| a <= c && c < b);
    loop {
        let before_len = spans.len();
        short.retain(|&(at, end)| {
            let before = s[..at].char_indices().next_back();
            let after = s[end..].chars().next();
            let ok = before.is_none_or(|(i, c)| !c.is_alphanumeric() || masked(&spans, i))
                && after.is_none_or(|c| !c.is_alphanumeric() || masked(&spans, end));
            if ok {
                spans.push((at, end));
            }
            !ok
        });
        if spans.len() == before_len {
            break;
        }
    }
    if spans.is_empty() {
        return None;
    }
    spans.sort_unstable();
    let mut out = String::with_capacity(s.len());
    let mut done = 0;
    let mut i = 0;
    while i < spans.len() {
        let (start, mut end) = spans[i];
        while i + 1 < spans.len() && spans[i + 1].0 < end {
            i += 1;
            end = end.max(spans[i].1);
        }
        out.push_str(&s[done..start]);
        out.push_str(MASK);
        done = end;
        i += 1;
    }
    out.push_str(&s[done..]);
    Some(out)
}

/// A writer that holds what is written and passes it on redacted when
/// dropped (or flushed): one per log event, so a secret split across
/// several `write` calls of one line is still masked. For the log output,
/// which used to print an interpolated selector in clear (VEL-066).
pub struct RedactingWriter<W: std::io::Write> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: std::io::Write> RedactingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            buf: Vec::new(),
        }
    }
}

impl<W: std::io::Write> std::io::Write for RedactingWriter<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        self.inner.write_all(redact(&text).as_bytes())?;
        self.inner.flush()
    }
}

impl<W: std::io::Write> Drop for RedactingWriter<W> {
    fn drop(&mut self) {
        let _ = std::io::Write::flush(self);
    }
}

/// Redact every string inside a JSON value, in place.
pub fn redact_value(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => {
            if let Cow::Owned(r) = redact(s) {
                *s = r;
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_value),
        serde_json::Value::Object(map) => map.values_mut().for_each(redact_value),
        _ => {}
    }
}

/// Redact a whole serializable result (a suite result with its traces):
/// every string field, wherever it is. Unchanged if it can't round-trip.
pub fn redact_serde<T: serde::Serialize + serde::de::DeserializeOwned>(value: &mut T) {
    let Ok(mut json) = serde_json::to_value(&*value) else {
        return;
    };
    redact_value(&mut json);
    if let Ok(back) = serde_json::from_value(json) {
        *value = back;
    }
}

#[cfg(test)]
mod tests {

    /// Review MAJOR 4: only listed names, or names a suite used as
    /// secrets, may be read by name.
    #[test]
    fn only_listed_or_suite_secret_names_are_allowed() {
        let name = "FD12_TEST_SUITE_ONLY_TOKEN";
        assert!(!is_allowed_secret_name(name));
        register_secret_name(name);
        assert!(is_allowed_secret_name(name));
        assert!(!is_allowed_secret_name("FD12_TEST_OTHER_TOKEN"));
    }
    use super::*;

    #[test]
    fn known_shapes_are_masked_and_plain_text_is_left_alone() {
        // (input, the secret part that must be gone, text that must stay)
        let cases = [
            (
                "Authorization: Bearer abcdefghijklmnop.qrs",
                "abcdefghijklmnop",
                "Authorization",
            ),
            (
                "GET https://api.x.com/v1?user=7&token=s3cr3tvalue&x=1",
                "s3cr3tvalue",
                "&x=1",
            ),
            (
                r#"{"user":"qa","password":"hunter22"}"#,
                "hunter22",
                r#""user":"qa""#,
            ),
            (
                "key sk-ant-REDACTEDTESTKEYPLACEHOLDER0001 used",
                "REDACTEDTESTKEY",
                " used",
            ),
            (
                "ghp_0123456789abcdefghijABCDEFGHIJ0123 leaked",
                "0123456789abcdef",
                "leaked",
            ),
            (
                "AWS AKIAABCDEFGHIJKLMNOP here",
                "AKIAABCDEFGHIJKLMNOP",
                "AWS",
            ),
            ("x-api-key=abcdef123456", "abcdef123456", "x-api-key"),
        ];
        for (input, secret, kept) in cases {
            let got = redact(input);
            assert!(
                !got.contains(secret) && got.contains(kept) && got.contains(MASK),
                "{input} -> {got}"
            );
        }
        let plain = "assertVisible { id: \"login_button\" }: Element not found after 10000ms";
        assert!(matches!(redact(plain), Cow::Borrowed(_)));
    }

    #[test]
    fn registered_values_are_masked_everywhere() {
        register_secret("Tr0ub4dor&3-unique");
        register_secret("ab"); // too short to register
        let got = redact("inputText typed Tr0ub4dor&3-unique into password; ab stays");
        assert_eq!(got, "inputText typed [redacted] into password; ab stays");
        let mut v = serde_json::json!({ "steps": [ { "text": "Tr0ub4dor&3-unique" } ] });
        redact_value(&mut v);
        assert_eq!(v["steps"][0]["text"], "[redacted]");
    }

    // VEL-015: a secret holding `"` or `\\` was never masked, because
    // messages carry it Debug-, JSON-, XML- or URL-escaped.
    #[test]
    fn escaped_forms_of_a_secret_are_masked() {
        // Unique to this test: SECRETS is process-wide, and a plain
        // "CANARY" would be masked in other tests' output too.
        let secret = "VEL015-9f7e\"q\\z<&>' x/+";
        register_secret(secret);
        let debug = format!("expected {secret:?}, got \"wrong\"");
        let nested = format!("{:?}", debug);
        let json = serde_json::to_string(&serde_json::json!({ "m": secret })).unwrap();
        let xml = "VEL015-9f7e&quot;q\\z&lt;&amp;&gt;&#39; x/+";
        let url = "?p=VEL015-9f7e%22q%5Cz%3C%26%3E%27%20x%2F%2B&n=1";
        let url_plus = "?p=VEL015-9f7e%22q%5Cz%3C%26%3E%27+x%2F%2B&n=1";
        for text in [debug.as_str(), &nested, &json, xml, url, url_plus] {
            let got = redact(text);
            assert!(!got.contains("VEL015"), "{text} -> {got}");
            assert!(got.contains(MASK), "{text} -> {got}");
        }
    }

    // VEL-232: 3-character secrets leaked; overlapping secrets left a tail
    // in clear.
    #[test]
    fn short_and_overlapping_secrets_are_masked() {
        register_secret("q7z");
        let got = redact("pin q7z entered; q7zebra stays");
        assert_eq!(got, "pin [redacted] entered; q7zebra stays");
        register_secret("vel232-abcd");
        register_secret("cd-vel232-tail");
        let got = redact("x vel232-abcd-vel232-tail y");
        assert_eq!(got, "x [redacted] y");
    }

    // D-29: secrets of 4–7 characters are masked as whole tokens only, in
    // every escaped form; from 8 they are masked anywhere.
    #[test]
    fn short_secrets_are_masked_only_as_whole_tokens() {
        register_secret("d29Pass");
        assert_eq!(redact("pw d29Pass ok"), "pw [redacted] ok");
        assert_eq!(redact("user=d29Pass"), "user=[redacted]");
        assert_eq!(redact("id d29Passport stays"), "id d29Passport stays");
        // Its escaped (longer) forms are still a 7-character secret's.
        register_secret("d29\"ab&");
        assert_eq!(redact("x d29&quot;ab&amp;y"), "x d29&quot;ab&amp;y");
        assert_eq!(redact("x d29&quot;ab&amp; y"), "x [redacted] y");
        register_secret("d29-eight-chars");
        assert_eq!(redact("xd29-eight-charsx"), "x[redacted]x");
        let long = registered_secrets_at_least(WORD_ONLY_BELOW);
        assert!(long.iter().any(|f| f == "d29-eight-chars"));
        assert!(!long.iter().any(|f| f.contains("d29Pass")));
        assert!(!long.iter().any(|f| f.contains("d29&quot;ab")), "{long:?}");
    }

    // C3 (D-29 leftover): a multi-line secret (a PEM key) printed a line
    // at a time leaked every line, because only the whole value was
    // registered.
    #[test]
    fn every_line_of_a_multi_line_secret_is_masked() {
        let pem = "-----BEGIN C3 KEY-----\r\nMIIC3firstLineOfKeyData\nC3secondLineData\nab\n\n-----END C3 KEY-----\n";
        register_secret(pem);
        assert_eq!(
            redact("got MIIC3firstLineOfKeyData here"),
            "got [redacted] here"
        );
        assert_eq!(redact("C3secondLineData"), "[redacted]");
        assert_eq!(redact("-----END C3 KEY-----"), "[redacted]");
        // The whole value, as it was before.
        assert_eq!(redact(&format!("k={pem}")), "k=[redacted]");
        // A 2-character line is too short to mask on its own.
        assert_eq!(redact("ab"), "ab");
        // Lines of 8+ characters are masked inside words too; the
        // multi-line value is never handed on with a line break in it.
        let long = registered_secrets_at_least(WORD_ONLY_BELOW);
        assert!(long.iter().any(|f| f == "C3secondLineData"), "{long:?}");
    }

    // V1-V4 (second pass, November 2026): shapes that let a credential
    // through. Each secret below is unregistered, so only a shape can mask it.
    #[test]
    fn basic_and_other_scheme_authorization_headers_are_masked() {
        for (input, secret) in [
            (
                "Authorization: Basic dXNlcjpwYXNzd29yZA==",
                "dXNlcjpwYXNzd29yZA",
            ),
            (
                "Proxy-Authorization: Basic dXNlcjpwYXNzd29yZA==",
                "dXNlcjpwYXNzd29yZA",
            ),
            ("Authorization: Token abcdef0123456789", "abcdef0123456789"),
            ("Authorization: Bearer short1", "short1"),
        ] {
            let got = redact(input);
            assert!(!got.contains(secret), "{input} -> {got}");
            assert!(got.contains(MASK), "{input} -> {got}");
            assert_eq!(redact(&got), got, "idempotent: {input}");
        }
        // Not a header: prose about "basic" stays.
        assert!(matches!(
            redact("Authorization is basic information"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn every_cookie_of_a_cookie_header_is_masked() {
        for (input, secrets) in [
            (
                "Cookie: session=abc123def456; csrf=xyz789uvw012; theme=dark",
                vec!["abc123def456", "xyz789uvw012", "theme=dark"],
            ),
            (
                "Set-Cookie: id=a1b2c3d4e5f6; Path=/; HttpOnly",
                vec!["a1b2c3d4e5f6"],
            ),
        ] {
            let got = redact(input);
            for s in secrets {
                assert!(!got.contains(s), "{input} -> {got}");
            }
            assert_eq!(redact(&got), got, "idempotent: {input}");
        }
    }

    #[test]
    fn a_json_secret_holding_an_escaped_quote_is_masked_whole() {
        let input = r#"{"user":"qa","password":"ab\"cdEFGH1234","next":"x"}"#;
        let got = redact(input);
        assert!(
            !got.contains("cdEFGH1234") && !got.contains("ab\\"),
            "{got}"
        );
        assert!(got.contains(r#""next":"x""#), "{got}");
        assert_eq!(redact(&got), got);
    }

    #[test]
    fn a_password_in_url_userinfo_is_masked() {
        for input in [
            "GET https://qa:s3cr3tPassw0rd@api.example.com/v1 answered 500",
            "git clone http://token:ghs3cr3tvalue@host:8080/repo.git",
            "postgres://app:hunter22xyz@db.internal:5432/main",
        ] {
            let got = redact(input);
            for secret in ["s3cr3tPassw0rd", "ghs3cr3tvalue", "hunter22xyz"] {
                assert!(!got.contains(secret), "{input} -> {got}");
            }
            assert!(got.contains('@') && got.contains(MASK), "{got}");
            assert_eq!(redact(&got), got);
        }
        // No password, or no userinfo: unchanged.
        for plain in [
            "https://token@github.com/org/repo",
            "https://example.com:8080/a@b",
            "see https://example.com/path?x=a:b@c",
        ] {
            assert!(matches!(redact(plain), Cow::Borrowed(_)), "{plain}");
        }
    }

    #[test]
    fn secret_variable_names_are_recognised() {
        for n in [
            "LOGIN_PASSWORD",
            "api_key",
            "GITHUB_TOKEN",
            "OTP_CODE",
            "STRIPE_SECRET",
            "SIGNING_KEY",
        ] {
            assert!(is_secret_name(n), "{n}");
        }
        assert!(is_secret_name("apiKey") && is_secret_name("loginPassword"));
        // VEL-075: acronym and digit runs, and more secret words.
        for n in [
            "APIToken",
            "TOKEN2",
            "PASSCODE",
            "PRIVATEKEY",
            "userPASSWORD",
            "CARD_CVV",
            "authJWT",
            "PIN1",
        ] {
            assert!(is_secret_name(n), "{n}");
        }
        for n in [
            "EMAIL",
            "BASE_URL",
            "USERNAME",
            "KEYBOARD_LAYOUT",
            "SHIPPING_ADDRESS",
            "AUTHOR",
            "MONKEY",
            "SPINNER",
            "HTTP2_URL",
        ] {
            assert!(!is_secret_name(n), "{n}");
        }
    }

    #[test]
    fn log_lines_and_command_errors_are_redacted() {
        use std::io::Write;
        register_secret("logline-S3cret-77");
        let mut out = Vec::new();
        {
            let mut w = RedactingWriter::new(&mut out);
            // One event, written in pieces that split the secret.
            w.write_all(b"WARN healed text=\"logline-S3c").unwrap();
            w.write_all(b"ret-77\" -> other\n").unwrap();
        }
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("S3cret"), "{text}");
        assert!(
            text.contains("[redacted]") && text.ends_with("other\n"),
            "{text}"
        );

        let e = crate::VelocityError::CommandFailed {
            device_id: "emulator-5554".into(),
            command: "input text logline-S3cret-77".into(),
            detail: "echo logline-S3cret-77 failed".into(),
        };
        let shown = e.to_string();
        assert!(!shown.contains("S3cret"), "{shown}");
        assert!(shown.contains("emulator-5554"), "{shown}");
    }

    mod props {
        use super::*;
        use proptest::prelude::*;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// SECRETS is process-wide: every generated secret carries a
        /// counter and a marker no other test's text holds, so it can
        /// never mask anything elsewhere.
        fn unique(marker: &str, tail: &str) -> String {
            static N: AtomicUsize = AtomicUsize::new(0);
            format!("{marker}{}{tail}", N.fetch_add(1, Ordering::Relaxed))
        }

        /// Secret tails: ASCII, punctuation, escapes and multi-byte text.
        /// No `[`/`]` (a secret spelling part of "[redacted]" would be
        /// "found" again across a mask) and no line breaks (each line of a
        /// multi-line secret is registered on its own, without the marker).
        const TAIL: &str = "[a-zA-Z0-9 !#$%&'()*+,./:;<=>?@^_`{|}~\"\\\\éü日本😀-]{0,20}";

        /// Secret-shaped strings, to put next to multi-byte text.
        const SHAPES_SAMPLE: &[&str] = &[
            "Bearer abcdefghijklmnop",
            "Authorization: xyzxyzxyz",
            "?token=s3cr3t&x=1",
            r#"{"password":"hunter22"}"#,
            "sk-ant-ABCDEFGHIJKLMNOPQRSTUV",
            "ghp_0123456789abcdefghijABCDEFGHIJ0123",
            "AKIAABCDEFGHIJKLMNOP",
            "eyJabcdefghij.abcdefghij.abcdefghij",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY-----",
        ];

        /// Text that can hold no registered secret and no shape: no ASCII
        /// letters or digits (every secret registered by any test, and
        /// every shape, needs some), plus a few fixed plain words.
        fn plain_text() -> impl Strategy<Value = String> {
            prop::collection::vec(
                prop_oneof![
                    "[^a-zA-Z0-9]{0,12}",
                    prop::sample::select(vec![
                        "hello",
                        "tap ",
                        "login_button",
                        "élan",
                        "日本語",
                        "😀",
                        " x ",
                    ])
                    .prop_map(str::to_string),
                ],
                0..8,
            )
            .prop_map(|parts| parts.concat())
        }

        proptest! {
            #[test]
            fn redact_never_panics(
                before in "\\PC{0,16}",
                after in "\\PC{0,16}",
                noise in any::<String>(),
                short in "[a-zé日😀]{0,3}",
                long in TAIL,
                shape in prop::sample::select(SHAPES_SAMPLE),
            ) {
                // A short (word-only) and a long secret, both multi-byte.
                let short = unique("ⓟ", &short);
                let long = unique("ⓠ", &long);
                register_secret(&short);
                register_secret(&long);
                let text = format!("{before}{short}{after}{long}{shape}{noise}é{short}日");
                let _ = redact(&text);
                let _ = redact(&noise);
                let _ = redact(&format!("{before}{shape}{after}"));
            }

            #[test]
            fn a_long_registered_secret_never_survives(
                tail in TAIL,
                before in "\\PC{0,16}",
                after in "\\PC{0,16}",
            ) {
                let secret = unique("PTlongsecret", &tail);
                prop_assert!(secret.chars().count() >= WORD_ONLY_BELOW);
                register_secret(&secret);
                for text in [
                    format!("{before}{secret}{after}"),
                    format!("{secret}{secret}"),
                    secret.clone(),
                ] {
                    let got = redact(&text);
                    prop_assert!(!got.contains(&secret), "{:?} -> {:?}", text, got);
                }
            }

            #[test]
            fn text_without_secrets_is_unchanged(text in plain_text()) {
                let got = redact(&text);
                prop_assert!(matches!(got, Cow::Borrowed(_)), "{:?} -> {:?}", text, got);
            }

            #[test]
            fn redaction_is_idempotent(
                tail in TAIL,
                short in "[a-z0-9]{1,3}",
                before in "\\PC{0,16}",
                after in "\\PC{0,16}",
                shape in prop::sample::select(SHAPES_SAMPLE),
            ) {
                let long = unique("PTidem", &tail);
                let short = unique("ⓘ", &short);
                register_secret(&long);
                register_secret(&short);
                for text in [
                    format!("{before}{long}{after}"),
                    format!("{before}{short}{after}{shape}"),
                    format!("{long}{short} {shape}{after}"),
                    format!("{before}{shape}{short}"),
                    format!("{before}{after}"),
                ] {
                    let once = redact(&text).into_owned();
                    let twice = redact(&once).into_owned();
                    prop_assert_eq!(&once, &twice, "input {:?}", text);
                }
            }
        }
    }
}
