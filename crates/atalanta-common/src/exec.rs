//! The `exec` step (row 3.2, AUT-017; Maestro #1682): run a program on the
//! host (an `adb` command, a seed script), check how it ended, and keep its
//! output for later steps. Off unless the suite allows the program:
//!
//! ```yaml
//! config:
//!   exec:
//!     allow: [adb, ./scripts/seed.sh]
//! tests:
//!   - name: seeded cart
//!     steps:
//!       - exec: { run: "./scripts/seed.sh cart 3", capture: CART_ID }
//!       - exec:
//!           run: [adb, -s, "${ATALANTA_DEVICE_ID}", shell, pm, grant, com.app, android.permission.CAMERA]
//!       - launchApp: {}
//!       - assertVisible: { text: "Cart ${CART_ID}" }
//! ```
//!
//! No shell runs it: a string is split on spaces (quotes keep a part
//! whole), so `;`, `|` and `$(…)` are plain characters. The program (the
//! first part) must be listed in `config.exec.allow`, exactly as written.
//! An MCP agent never gets it outside `--profile full` (docs/SECURITY.md).

use serde::{Deserialize, Serialize};

/// One `exec` step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecStep {
    /// The command: a string (split on spaces) or its parts.
    pub run: ExecCommand,
    /// How it must end (default: exit code 0).
    #[serde(default, skip_serializing_if = "ExecExpect::is_default")]
    pub expect: ExecExpect,
    /// `${NAME}` for its standard output (trimmed), for later steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<String>,
    /// Its working directory (default: where `atalanta` runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExecCommand {
    Line(String),
    Parts(Vec<String>),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecExpect {
    /// The exit code it must end with (default 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Text its output must contain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
}

impl ExecExpect {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// `config.exec`: which programs `exec` steps may run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecConfig {
    /// Programs, exactly as a step's first part names them.
    #[serde(default)]
    pub allow: Vec<String>,
}

impl ExecCommand {
    /// The parts: a line split on spaces, `"…"` and `'…'` kept whole.
    pub fn argv(&self) -> Vec<String> {
        match self {
            ExecCommand::Parts(p) => p.clone(),
            ExecCommand::Line(l) => split(l),
        }
    }

    /// A line with `${NAME}`s split into its parts before they are filled
    /// in, so a value is always one argument: a captured `x.apk -s other`
    /// or a value with quotes used to add arguments to the allowed program
    /// (review 05 #6, audit 2026-11 EXEC-1). A `${…}` is never split, even
    /// with spaces in its default.
    pub fn split_before_filling(&mut self) {
        if let ExecCommand::Line(l) = self {
            if l.contains("${") {
                *self = ExecCommand::Parts(split_template(l));
            }
        }
    }

    /// Every string in it, for `${NAME}` filling.
    pub fn strings_mut(&mut self) -> Vec<&mut String> {
        match self {
            ExecCommand::Line(l) => vec![l],
            ExecCommand::Parts(p) => p.iter_mut().collect(),
        }
    }
}

/// [`split`], keeping each `${…}` in one part.
fn split_template(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    let mut depth = 0usize;
    let mut prev = '\0';
    for c in line.chars() {
        match (quote, c) {
            (_, '{') if prev == '$' => {
                depth += 1;
                cur.push(c);
            }
            (_, '}') if depth > 0 => {
                depth -= 1;
                cur.push(c);
            }
            (Some(q), c) if c == q && depth == 0 => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') if depth == 0 => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() && depth == 0 => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
        prev = c;
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn split(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl ExecStep {
    /// Why `allow` doesn't let this step run, if it doesn't.
    pub fn refused_by(&self, config: Option<&ExecConfig>) -> Option<String> {
        let argv = self.run.argv();
        let Some(program) = argv.first() else {
            return Some("exec: `run` is empty".into());
        };
        let allowed = config.is_some_and(|c| c.allow.iter().any(|a| a == program));
        (!allowed).then(|| {
            format!(
                "exec runs `{program}`, which config.exec.allow doesn't list; exec is off unless \
                 the suite allows the program (config: {{ exec: {{ allow: [{program}] }} }})"
            )
        })
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_filled_value_is_one_argument() {
        let mut c = ExecCommand::Line("adb -s ${DEVICE} install '${APK}' ${X:-a b}".into());
        c.split_before_filling();
        assert_eq!(
            c.argv(),
            ["adb", "-s", "${DEVICE}", "install", "${APK}", "${X:-a b}"]
        );
        for part in c.strings_mut() {
            *part = part.replace("${APK}", "x.apk -s other-device");
        }
        assert_eq!(c.argv()[4], "x.apk -s other-device");
        assert_eq!(c.argv().len(), 6);
        let mut plain = ExecCommand::Line("echo hi".into());
        plain.split_before_filling();
        assert_eq!(plain, ExecCommand::Line("echo hi".into()));
    }
    use super::*;

    #[test]
    fn a_line_splits_on_spaces_with_quotes_and_no_shell() {
        let c = ExecCommand::Line(r#"./seed.sh "two words" 'x y' a;b $(rm)  "" end"#.into());
        assert_eq!(
            c.argv(),
            ["./seed.sh", "two words", "x y", "a;b", "$(rm)", "", "end"]
        );
    }

    #[test]
    fn only_an_allowed_program_runs() {
        let step: ExecStep = serde_yaml::from_str("run: adb shell pm clear com.app").unwrap();
        assert!(step.refused_by(None).unwrap().contains("doesn't list"));
        let cfg = ExecConfig {
            allow: vec!["adb".into()],
        };
        assert!(step.refused_by(Some(&cfg)).is_none());
        let other: ExecStep = serde_yaml::from_str("run: [sh, -c, 'rm -rf /']").unwrap();
        assert!(other.refused_by(Some(&cfg)).is_some());
        // A path must match as written: `adb` doesn't allow `/tmp/adb`.
        let path: ExecStep = serde_yaml::from_str("run: /tmp/adb devices").unwrap();
        assert!(path.refused_by(Some(&cfg)).is_some());
        assert!(serde_yaml::from_str::<ExecStep>("run: x\nshell: true").is_err());
    }
}
