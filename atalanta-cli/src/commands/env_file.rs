//! `atalanta run --env-file`: variable overrides from a dotenv file
//! (Maestro #2817). Read only when named: a `.env` lying in the project is
//! never picked up on its own, so a run gets only the values asked for.
//!
//! The format docker compose and dotenv agree on: `KEY=VALUE` lines, `#`
//! comments, blank lines, an optional `export `, values in `"…"` or `'…'`.
//! Nothing is expanded: `$HOME` or `$(cmd)` in a value stays as written.

/// The pairs in the file at `path`, in file order (a later duplicate wins
/// once they are collected). An unreadable file or a malformed line is a
/// usage error (exit 2) with the line number; the line itself is never
/// quoted, as it may hold a secret.
pub fn read(path: &str) -> anyhow::Result<Vec<(String, String)>> {
    let text = std::fs::read_to_string(path).map_err(|e| usage_err!("--env-file {path}: {e}"))?;
    parse(&text).map_err(|(line, why)| usage_err!("--env-file {path}:{line}: {why}"))
}

/// The pairs in dotenv `text`, or the first bad line's number and what is
/// wrong with it.
fn parse(text: &str) -> Result<Vec<(String, String)>, (usize, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line
            .strip_prefix("export")
            .filter(|rest| rest.starts_with([' ', '\t']))
            .map_or(line, str::trim_start);
        let Some((key, raw)) = line.split_once('=') else {
            return Err((i + 1, "expected KEY=VALUE".into()));
        };
        let key = key.trim();
        if !is_name(key) {
            return Err((
                i + 1,
                "the name before `=` isn't a variable name (letters, digits and _, not starting with a digit)".into(),
            ));
        }
        let value = value(raw.trim()).map_err(|why| (i + 1, format!("{key}: {why}")))?;
        out.push((key.to_string(), value));
    }
    Ok(out)
}

fn is_name(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A value: bare (an inline comment needs whitespace before its `#`, so
/// `a#b` stays whole), `'…'` taken as written, or `"…"` with `\n`, `\r`,
/// `\t`, `\"` and `\\` escapes (any other backslash stays). One line each.
fn value(raw: &str) -> Result<String, String> {
    let Some(quote) = raw.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
        let end = raw
            .char_indices()
            .find(|&(i, c)| c == '#' && raw[..i].ends_with([' ', '\t']))
            .map_or(raw.len(), |(i, _)| i);
        return Ok(raw[..end].trim_end().to_string());
    };
    let body = &raw[1..];
    let mut out = String::new();
    let mut chars = body.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == quote {
            let rest = body[i + 1..].trim_start();
            if rest.is_empty() || rest.starts_with('#') {
                return Ok(out);
            }
            return Err(format!("text after the closing {quote}"));
        }
        if c == '\\' && quote == '"' {
            match chars.next() {
                Some((_, 'n')) => out.push('\n'),
                Some((_, 'r')) => out.push('\r'),
                Some((_, 't')) => out.push('\t'),
                Some((_, e @ ('"' | '\\'))) => out.push(e),
                Some((_, other)) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
            continue;
        }
        out.push(c);
    }
    Err(format!("no closing {quote} (a value is one line)"))
}

#[cfg(test)]
mod tests {
    use super::parse;

    fn pairs(text: &str) -> Vec<(String, String)> {
        parse(text).unwrap_or_else(|e| panic!("{text:?}: {e:?}"))
    }

    fn pair(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    #[test]
    fn comments_blank_lines_and_export_are_skipped() {
        let text = "\u{feff}# the API\n\nAPI_URL=https://api.test\n  export TOKEN=abc  \nexport\tA=1\r\nexport=2\n";
        assert_eq!(
            pairs(text),
            vec![
                pair("API_URL", "https://api.test"),
                pair("TOKEN", "abc"),
                pair("A", "1"),
                pair("export", "2"),
            ]
        );
    }

    #[test]
    fn values_keep_their_text() {
        assert_eq!(
            pairs("A = spaced out \nB=\nC=a=b\nD=x#y\nE=x # note\nF=p@ss,w0rd\n"),
            vec![
                pair("A", "spaced out"),
                pair("B", ""),
                pair("C", "a=b"),
                pair("D", "x#y"),
                pair("E", "x"),
                pair("F", "p@ss,w0rd"),
            ]
        );
    }

    #[test]
    fn quotes_hold_spaces_hashes_and_escapes() {
        let text = r#"A="two words # not a comment"
B='single $HOME \n kept'
C="line\nbreak \"quoted\" back\\slash \d"
D="x"   # trailing comment
E=''
"#;
        assert_eq!(
            pairs(text),
            vec![
                pair("A", "two words # not a comment"),
                pair("B", r"single $HOME \n kept"),
                pair("C", "line\nbreak \"quoted\" back\\slash \\d"),
                pair("D", "x"),
                pair("E", ""),
            ]
        );
    }

    #[test]
    fn nothing_is_expanded() {
        assert_eq!(
            pairs("A=$HOME/${USER}\nB=\"$(whoami)\"\n"),
            vec![pair("A", "$HOME/${USER}"), pair("B", "$(whoami)")]
        );
    }

    #[test]
    fn a_malformed_line_is_named_by_number() {
        let err = |text: &str| parse(text).unwrap_err();
        assert_eq!(
            err("A=1\n\njust-a-secret\n"),
            (3, "expected KEY=VALUE".into())
        );
        assert_eq!(err("=1\n").0, 1);
        assert_eq!(err("A=1\n1A=2\n").0, 2);
        assert_eq!(err("MY KEY=1\n").0, 1);
        assert_eq!(
            err("A=\"open\n").1,
            "A: no closing \" (a value is one line)"
        );
        assert_eq!(err("A='x' y\n").1, "A: text after the closing '");
        // The value never appears in the message.
        assert!(!err("TOKEN=\"s3cr3t\n").1.contains("s3cr3t"));
    }

    #[test]
    fn an_unreadable_file_is_a_usage_error() {
        let e = super::read("/nonexistent/atalanta/.env").unwrap_err();
        assert_eq!(crate::commands::exit_code_for(&e), 2);
        assert!(
            e.to_string()
                .contains("--env-file /nonexistent/atalanta/.env: "),
            "{e}"
        );
    }
}
