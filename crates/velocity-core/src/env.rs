use std::collections::{BTreeSet, HashMap};

use velocity_common::{Action, Result, Selector, TestSuite, VelocityError};

/// Replace every `${VAR}`, `${VAR:-default}` and `${VAR:?message}` in
/// `input`, in one pass. A value is inserted as is: it is never scanned for
/// `${` again, so a literal `${` in a value, or a value naming another
/// variable or itself, stays literal (VEL-060; the old loop re-expanded until
/// nothing changed, and a self-reference never stopped). Braces nest, so
/// `${A:-${B}}` works whether or not A is set (VEL-192).
///
/// Escaping (D-13, compose-style but only before `{`): in a run of `$`
/// right before `{`, each `$$` writes one `$`; an odd one left over starts
/// a placeholder. So `$${X}` writes the literal `${X}`, `$$${X}` writes `$`
/// and X's value (a price: `$$${TOTAL}` → `$12.40`), and `$$$${X}` writes
/// `$${X}`. `$$` anywhere else (`$$5`, a trailing `$$`) stays as written.
pub fn interpolate(input: &str, overrides: &HashMap<String, String>) -> Result<String> {
    interpolate_except(input, overrides, &BTreeSet::new())
}

/// [`interpolate`], leaving the placeholders of `deferred` names exactly as
/// written (escapes and defaults included) for [`bind_captured`].
fn interpolate_except(
    input: &str,
    overrides: &HashMap<String, String>,
    deferred: &BTreeSet<String>,
) -> Result<String> {
    // Most strings have nothing to replace (VEL-210: a regex was compiled
    // for every one of them).
    if !input.contains("${") {
        return Ok(input.to_string());
    }
    let mut missing_vars = Vec::new();
    let lookup = |name: &str| lookup_var(name, overrides);
    let pass = Pass {
        fills: &|name| !deferred.contains(name),
        lookup: &lookup,
    };
    let result = expand(input, &pass, &mut missing_vars);
    if !missing_vars.is_empty() {
        return Err(VelocityError::MissingEnvVars { vars: missing_vars });
    }
    Ok(result)
}

/// Fill in the `${NAME}`s of the names `http` steps capture (`names`) from
/// the values the running test captured so far. Every other placeholder was
/// filled when the suite was loaded, so it is left alone: what is left of
/// it is a value or an escaped literal, never a placeholder.
pub fn bind_captured(
    input: &str,
    captured: &HashMap<String, String>,
    names: &BTreeSet<String>,
) -> Result<String> {
    if !input.contains("${") {
        return Ok(input.to_string());
    }
    let mut unset = Vec::new();
    let lookup = |name: &str| captured.get(name).cloned();
    let pass = Pass {
        fills: &|name| names.contains(name),
        lookup: &lookup,
    };
    let result = expand(input, &pass, &mut unset);
    if let Some(first) = unset.first() {
        // `${NAME:?message}` reports as `NAME: message`.
        let name = first.split(':').next().unwrap_or(first);
        return Err(VelocityError::Config(format!(
            "`${{{name}}}` has no value yet: no earlier step of this test captured it \
             (an `http` step's `capture:` sets it; `${{{name}:-default}}` gives a fallback)"
        )));
    }
    Ok(result)
}

/// One interpolation pass: which names it fills in, and where their values
/// come from. A placeholder of any other name is written back as is.
struct Pass<'a> {
    fills: &'a dyn Fn(&str) -> bool,
    lookup: &'a dyn Fn(&str) -> Option<String>,
}

fn expand(input: &str, pass: &Pass, errors: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find("${") {
        // The run of `$` ending in this one (D-13): pairs are literal `$`s.
        let pre = &rest[..at];
        let k = pre.len() - pre.trim_end_matches('$').len();
        let body = &rest[at + 2..];
        let close = closing_brace(body);
        // Another pass's placeholder, escaped or not: kept with its `$`s,
        // so that pass reads it exactly as it was written.
        if let Some(end) = close.filter(|&end| end > 0) {
            if !(pass.fills)(var_name(&body[..end])) {
                out.push_str(&rest[..at + 2 + end + 1]);
                rest = &body[end + 1..];
                continue;
            }
        }
        out.push_str(&pre[..pre.len() - k]);
        out.push_str(&"$".repeat(k.div_ceil(2)));
        if (k + 1).is_multiple_of(2) {
            // `$${` is a literal `${`: `$${HOME}` writes `${HOME}`.
            out.push('{');
            rest = &rest[at + 2..];
            continue;
        }
        match close {
            Some(end) if end > 0 => {
                out.push_str(&resolve_var_expr(&body[..end], pass, errors));
                rest = &body[end + 1..];
            }
            // `${}` or no closing brace: literal text.
            Some(end) => {
                out.push_str(&rest[at..at + 2 + end + 1]);
                rest = &body[end + 1..];
            }
            None => {
                out.push_str(&rest[at..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Where the `}` that closes a `${` is, counting nested `${ … }` pairs.
fn closing_brace(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'{') {
            depth += 1;
            i += 2;
            continue;
        }
        if bytes[i] == b'}' {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
        }
        i += 1;
    }
    None
}

/// Where a placeholder's `:-` / `:?` operator is, and which one. The
/// operator is the first `:` of the name, never one inside a nested
/// default: `${A:-${B:?need B}}` read `A:-${B` as the name (review m1).
fn operator(expr: &str) -> Option<(usize, u8)> {
    expr.find(':')
        .filter(|&i| !expr[..i].contains("${"))
        .and_then(|i| match expr.as_bytes().get(i + 1) {
            Some(b'?') => Some((i, b'?')),
            Some(b'-') => Some((i, b'-')),
            _ => None,
        })
}

/// The variable a placeholder names: `A` in `A`, `A:-x` and `A:?why`.
fn var_name(expr: &str) -> &str {
    operator(expr).map_or(expr, |(i, _)| &expr[..i])
}

/// Shortest inline default of a secret-named variable that is masked (D-29:
/// the same 8 as GitHub's `add-mask`; a shorter default is a placeholder).
const MIN_SECRET_DEFAULT: usize = 8;

fn resolve_var_expr(expr: &str, pass: &Pass, errors: &mut Vec<String>) -> String {
    let op = operator(expr);

    // ${VAR:?error message}
    if let Some((idx, b'?')) = op {
        let var_name = &expr[..idx];
        let error_msg = &expr[idx + 2..];
        if let Some(val) = (pass.lookup)(var_name) {
            return val;
        }
        errors.push(format!("{var_name}: {error_msg}"));
        return format!("${{{expr}}}");
    }

    // ${VAR:-default}
    if let Some((idx, _)) = op {
        let var_name = &expr[..idx];
        if let Some(val) = (pass.lookup)(var_name) {
            return val;
        }
        // The default may name other variables (`${A:-${B}}`).
        let default_val = expand(&expr[idx + 2..], pass, errors);
        // A secret's default is as secret as its value (VEL-062), unless it
        // is a short placeholder: `${TEST_OTP:-1234}` masked every "1234"
        // (order numbers, prices) in every report (review m2).
        if velocity_common::redact::is_secret_name(var_name)
            && default_val.chars().count() >= MIN_SECRET_DEFAULT
        {
            velocity_common::redact::register_secret(&default_val);
        }
        return default_val;
    }

    // ${VAR}
    let var_name = expr;
    if let Some(val) = (pass.lookup)(var_name) {
        return val;
    }

    errors.push(var_name.to_string());
    format!("${{{expr}}}")
}

fn lookup_var(name: &str, overrides: &HashMap<String, String>) -> Option<String> {
    let value = overrides
        .get(name)
        .cloned()
        .or_else(|| velocity_common::env::var(name).ok());
    // A secret-looking variable's value is masked in every report, trace,
    // console line and MCP reply from here on.
    if let Some(v) = &value {
        if velocity_common::redact::is_secret_name(name) {
            velocity_common::redact::register_secret(v);
            velocity_common::redact::register_secret_name(name);
        }
    }
    value
}

/// The names `http` steps capture (`capture: { ORDER_ID: "$.id" }`), in
/// the tests and the flows. Their `${…}` has no value until the step runs.
pub fn captured_names(suite: &TestSuite) -> BTreeSet<String> {
    suite
        .flows
        .iter()
        .flat_map(|f| &f.steps)
        .chain(suite.tests.iter().flat_map(|t| &t.steps))
        .filter_map(|s| match &s.action {
            Action::Http(req) => Some(req.capture.iter().map(|(name, _)| name.clone())),
            _ => None,
        })
        .flatten()
        .collect()
}

/// Fill in every `${VAR}` of the suite from `overrides` and the environment,
/// except the names `http` steps capture: those are left for the executor
/// to fill in as the test runs ([`bind_step`]), and recorded in
/// `config.captured_vars`.
pub fn interpolate_suite(suite: &mut TestSuite, overrides: &HashMap<String, String>) -> Result<()> {
    let deferred = captured_names(suite);
    let mut fill = |s: &str| interpolate_except(s, overrides, &deferred);
    suite.app_id = fill(&suite.app_id)?;
    if let Some(http) = suite.config.http.as_mut() {
        if let Some(base) = http.base_url.as_mut() {
            *base = fill(base)?;
        }
        for (_, v) in http.headers.iter_mut() {
            *v = fill(v)?;
        }
    }

    for flow in &mut suite.flows {
        for step in &mut flow.steps {
            interpolate_action(&mut step.action, &mut fill)?;
        }
    }

    for test in &mut suite.tests {
        for step in &mut test.steps {
            interpolate_action(&mut step.action, &mut fill)?;
        }
    }

    suite.config.captured_vars = deferred;
    Ok(())
}

/// `step` with the captured `${NAME}`s (`names`) filled in from `captured`,
/// or `None` when it has none to fill.
pub fn bind_step(
    step: &velocity_common::Step,
    captured: &HashMap<String, String>,
    names: &BTreeSet<String>,
) -> Result<Option<velocity_common::Step>> {
    if names.is_empty() {
        return Ok(None);
    }
    let mut changed = false;
    let mut action = step.action.clone();
    interpolate_action(&mut action, &mut |s: &str| {
        let out = bind_captured(s, captured, names)?;
        changed |= out != s;
        Ok(out)
    })?;
    Ok(changed.then(|| velocity_common::Step {
        action,
        ..step.clone()
    }))
}

fn interpolate_action(
    action: &mut Action,
    f: &mut dyn FnMut(&str) -> Result<String>,
) -> Result<()> {
    match action {
        Action::LaunchApp { app_id, .. } => {
            *app_id = f(app_id)?;
        }
        Action::StopApp { app_id } => {
            *app_id = f(app_id)?;
        }
        Action::MockRequest { rule } => {
            rule.url = f(&rule.url)?;
            if let Some(b) = rule.body.as_mut() {
                *b = f(b)?;
            }
            if let Some(file) = rule.file.as_mut() {
                *file = f(file)?;
            }
        }
        Action::ClearMocks => {}
        Action::Http(req) => req.map_strings(f)?,
        Action::SetConfig { values } => {
            for v in values.values_mut() {
                *v = f(v)?;
            }
        }
        Action::AssertSystemState { media_title, .. } => {
            if let Some(t) = media_title {
                *t = f(t)?;
            }
        }
        Action::Tap { selector }
        | Action::DoubleTap { selector }
        | Action::ClearText { selector }
        | Action::AssertVisible { selector }
        | Action::AssertNotVisible { selector } => {
            interpolate_selector(selector, f)?;
        }
        Action::LongPress { selector, .. } => {
            interpolate_selector(selector, f)?;
        }
        Action::InputText { selector, text } => {
            interpolate_selector(selector, f)?;
            *text = f(text)?;
        }
        Action::AssertText { selector, expected } => {
            interpolate_selector(selector, f)?;
            *expected = f(expected)?;
        }
        // `within:` too: it was never interpolated, so `${LIST_ID}` was
        // looked up literally (VEL-196).
        Action::ScrollUntilVisible {
            selector, within, ..
        } => {
            interpolate_selector(selector, f)?;
            if let Some(w) = within {
                interpolate_selector(w, f)?;
            }
        }
        Action::Screenshot { filename } => {
            if let Some(name) = filename {
                *name = f(name)?;
            }
        }
        Action::RunFlow { flow_id } => {
            *flow_id = f(flow_id)?;
        }
        Action::InputRaw { text } => {
            *text = f(text)?;
        }
        Action::Natural {
            intent,
            data,
            data_name,
            wait_until,
            intent_template,
            ..
        } => {
            if intent.contains("${") && intent_template.is_none() {
                *intent_template = Some(intent.clone());
            }
            *intent = f(intent)?;
            if let Some(d) = data {
                if d.contains("${") && data_name.is_none() {
                    *data_name = Some(d.clone());
                }
                *d = f(d)?;
            }
            if let Some(w) = wait_until {
                *w = f(w)?;
            }
        }
        Action::Swipe { within, .. } => {
            if let Some(w) = within {
                interpolate_selector(w, f)?;
            }
        }
        Action::PressKey { .. }
        | Action::Wait { .. }
        | Action::TapXY { .. }
        | Action::GoBack
        | Action::Refresh => {}
        Action::Goto { url } => *url = f(url)?,
        Action::SaveState { name } | Action::RestoreState { name } => *name = f(name)?,
        Action::AssertScreenshot {
            name, mask, crop, ..
        } => {
            *name = f(name)?;
            for sel in mask {
                interpolate_selector(sel, f)?;
            }
            if let Some(velocity_common::ScreenshotCrop::Element(sel)) = crop {
                interpolate_selector(sel, f)?;
            }
        }
    }
    Ok(())
}

fn interpolate_selector(
    selector: &mut Selector,
    f: &mut dyn FnMut(&str) -> Result<String>,
) -> Result<()> {
    match selector {
        Selector::Id(v)
        | Selector::Text(v)
        | Selector::TextContains(v)
        | Selector::TextMatches(v)
        | Selector::IdMatches(v)
        | Selector::AccessibilityId(v)
        | Selector::ClassName(v)
        | Selector::Role(v)
        | Selector::Name(v) => {
            *v = f(v)?;
        }
        Selector::Checked(_)
        | Selector::Focused(_)
        | Selector::Selected(_)
        | Selector::Enabled(_) => {}
        Selector::Index {
            selector: inner, ..
        } => {
            interpolate_selector(inner, f)?;
        }
        Selector::Compound(selectors) => {
            for s in selectors {
                interpolate_selector(s, f)?;
            }
        }
        // CSS/XPath selectors are plain strings; interpolate the value.
        Selector::Css(v) | Selector::XPath(v) => {
            *v = f(v)?;
        }
        Selector::Relative { target, anchor, .. } => {
            interpolate_selector(target, f)?;
            interpolate_selector(anchor, f)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overrides(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn interpolate_simple_var() {
        let ovr = overrides(&[("USER", "alice")]);
        assert_eq!(interpolate("Hello ${USER}", &ovr).unwrap(), "Hello alice");
    }

    #[test]
    fn interpolate_default_value() {
        let ovr = HashMap::new();
        assert_eq!(
            interpolate("${HOST:-localhost}", &ovr).unwrap(),
            "localhost"
        );
    }

    #[test]
    fn interpolate_default_not_used_when_present() {
        let ovr = overrides(&[("HOST", "prod.example.com")]);
        assert_eq!(
            interpolate("${HOST:-localhost}", &ovr).unwrap(),
            "prod.example.com"
        );
    }

    #[test]
    fn interpolate_required_var_present() {
        let ovr = overrides(&[("API_KEY", "secret123")]);
        assert_eq!(
            interpolate("${API_KEY:?API key is required}", &ovr).unwrap(),
            "secret123"
        );
    }

    #[test]
    fn interpolate_required_var_missing() {
        let ovr = HashMap::new();
        let result = interpolate("${API_KEY:?API key is required}", &ovr);
        assert!(result.is_err());
        match result.unwrap_err() {
            VelocityError::MissingEnvVars { vars } => {
                assert!(vars[0].contains("API_KEY"));
            }
            other => panic!("expected MissingEnvVars, got {:?}", other),
        }
    }

    #[test]
    fn interpolate_missing_var_no_default() {
        let ovr = HashMap::new();
        let result = interpolate("${MISSING_VAR}", &ovr);
        assert!(result.is_err());
    }

    #[test]
    fn interpolate_multiple_vars() {
        let ovr = overrides(&[("A", "1"), ("B", "2")]);
        assert_eq!(interpolate("${A}-${B}", &ovr).unwrap(), "1-2");
    }

    #[test]
    fn interpolate_no_vars() {
        let ovr = HashMap::new();
        assert_eq!(
            interpolate("no variables here", &ovr).unwrap(),
            "no variables here"
        );
    }

    #[test]
    fn interpolate_from_process_env() {
        // PATH should exist in virtually all environments
        let ovr = HashMap::new();
        let result = interpolate("${PATH}", &ovr);
        assert!(result.is_ok());
        assert!(!result.unwrap().is_empty());
    }

    #[test]
    fn interpolate_override_takes_precedence() {
        let ovr = overrides(&[("PATH", "my_custom_path")]);
        assert_eq!(interpolate("${PATH}", &ovr).unwrap(), "my_custom_path");
    }

    #[test]
    fn interpolate_suite_app_id() {
        use velocity_common::{SuiteConfig, TestCase};

        let ovr = overrides(&[("APP", "com.prod.app")]);
        let mut suite = TestSuite {
            app_id: "${APP}".to_string(),
            config: SuiteConfig::default(),
            flows: vec![],
            tests: vec![TestCase {
                name: "t".to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![],
                description: None,
                verify: None,
            }],
        };

        interpolate_suite(&mut suite, &ovr).unwrap();
        assert_eq!(suite.app_id, "com.prod.app");
    }

    #[test]
    fn interpolate_suite_step_text() {
        use velocity_common::{Step, SuiteConfig, TestCase};

        let ovr = overrides(&[("USERNAME", "testuser")]);
        let mut suite = TestSuite {
            app_id: "com.test".to_string(),
            config: SuiteConfig::default(),
            flows: vec![],
            tests: vec![TestCase {
                name: "t".to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![Step {
                    action: Action::InputText {
                        selector: Selector::Id("email".to_string()),
                        text: "${USERNAME}@example.com".to_string(),
                    },
                    timeout_ms: None,
                    settle: None,
                    source: None,
                    modifiers: Default::default(),
                }],
                description: None,
                verify: None,
            }],
        };

        interpolate_suite(&mut suite, &ovr).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::InputText { text, .. } => {
                assert_eq!(text, "testuser@example.com");
            }
            other => panic!("expected InputText, got {:?}", other),
        }
    }

    #[test]
    fn a_doubled_dollar_writes_a_literal_placeholder() {
        // D-13: there was no way to write `${` itself.
        let o = overrides(&[("USER", "ada")]);
        assert_eq!(interpolate("$${USER}", &o).unwrap(), "${USER}");
        assert_eq!(
            interpolate("hi ${USER}, type $${NAME} here", &o).unwrap(),
            "hi ada, type ${NAME} here"
        );
        // Not an unset variable: no error for a literal.
        assert_eq!(interpolate("$${MISSING:?x}", &o).unwrap(), "${MISSING:?x}");
        // A lone `$` and `$$` without a brace are untouched.
        assert_eq!(
            interpolate("$5 and $$ ${USER}", &o).unwrap(),
            "$5 and $$ ada"
        );
    }

    #[test]
    fn a_dollar_then_a_value_is_written_with_three() {
        // Review m8: a price (`$12.40`) had no spelling once `$${` became
        // the escape: `$$${X}` gave `$${X}`.
        let o = overrides(&[("TOTAL", "12.40")]);
        assert_eq!(interpolate("$$${TOTAL}", &o).unwrap(), "$12.40");
        assert_eq!(
            interpolate("Total: $$${TOTAL} due", &o).unwrap(),
            "Total: $12.40 due"
        );
        assert_eq!(interpolate("$$$${TOTAL}", &o).unwrap(), "$${TOTAL}");
        assert_eq!(interpolate("$$$$${TOTAL}", &o).unwrap(), "$$12.40");
        // Before a brace only: `$$5` and a trailing `$$` stay as written.
        assert_eq!(interpolate("$$5 ${TOTAL} $$", &o).unwrap(), "$$5 12.40 $$");
    }

    #[test]
    fn values_are_inserted_once_never_re_expanded() {
        // VEL-060: a literal `${` in a value broke the run, a value naming
        // another variable pulled it in, and a self-reference never ended.
        let o = overrides(&[
            ("PW", "a${b"),
            ("REF", "${OTHER}"),
            ("OTHER", "leaked"),
            ("SELF", "x${SELF}"),
        ]);
        assert_eq!(interpolate("${PW}", &o).unwrap(), "a${b");
        assert_eq!(interpolate("${REF}", &o).unwrap(), "${OTHER}");
        assert_eq!(interpolate("${SELF}", &o).unwrap(), "x${SELF}");
        assert_eq!(
            interpolate("cost ${} and ${open", &o).unwrap(),
            "cost ${} and ${open"
        );
    }

    #[test]
    fn nested_defaults_leave_no_stray_brace() {
        // VEL-192: `${A:-${B}}` with A set gave "a}".
        let o = overrides(&[("A", "a"), ("B", "b")]);
        assert_eq!(interpolate("${A:-${B}}", &o).unwrap(), "a");
        assert_eq!(interpolate("${NOPE_192:-${B}}!", &o).unwrap(), "b!");
        assert_eq!(
            interpolate("${NOPE_192:-x${NOPE2_192:-y}z}", &o).unwrap(),
            "xyz"
        );
    }

    #[test]
    fn a_secret_default_is_masked() {
        // VEL-062.
        let v = interpolate("${API_TOKEN_VEL062:-default-secret-062}", &HashMap::new()).unwrap();
        assert_eq!(v, "default-secret-062");
        assert!(!velocity_common::redact::redact("x default-secret-062 y")
            .contains("default-secret-062"));
    }

    #[test]
    fn a_required_variable_inside_a_default_is_only_asked_for_when_used() {
        // Review m1: with A set, `${A:-${B:?need B}}` reported A as missing.
        let mut env = HashMap::new();
        env.insert("VEL_M1_A".to_string(), "a".to_string());
        assert_eq!(
            interpolate("${VEL_M1_A:-${VEL_M1_B:?need B}}", &env).unwrap(),
            "a"
        );
        let e = interpolate("${VEL_M1_UNSET:-${VEL_M1_B:?need B}}", &env).unwrap_err();
        assert!(e.to_string().contains("VEL_M1_B: need B"), "{e}");
        // A literal `:?` in a default is just text.
        assert_eq!(
            interpolate("${VEL_M1_UNSET:-what:?}", &env).unwrap(),
            "what:?"
        );
    }

    #[test]
    fn a_short_placeholder_default_is_not_masked() {
        // Review m2: `${TEST_OTP:-1234}` masked every 1234 in every report.
        let v = interpolate("${VEL_M2_OTP:-4417}", &HashMap::new()).unwrap();
        assert_eq!(v, "4417");
        assert!(velocity_common::redact::redact("order 4417").contains("4417"));
        // D-29: masked from 8 characters; a 7-character default is not.
        interpolate("${VEL_D29_TOKEN:-d29sev7}", &HashMap::new()).unwrap();
        assert!(velocity_common::redact::redact("x d29sev7 y").contains("d29sev7"));
        interpolate("${VEL_D29_TOKEN2:-d29eight}", &HashMap::new()).unwrap();
        assert!(!velocity_common::redact::redact("x d29eight y").contains("d29eight"));
    }

    fn names(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_captured_name_is_left_as_written_for_the_run() {
        let o = overrides(&[("HOST", "x.dev"), ("ORDER_ID", "from-env")]);
        let d = names(&["ORDER_ID"]);
        let load = |s: &str| interpolate_except(s, &o, &d).unwrap();
        // The captured name is kept, escapes and defaults included; the
        // others are filled in as before.
        assert_eq!(load("${HOST}/o/${ORDER_ID}"), "x.dev/o/${ORDER_ID}");
        assert_eq!(
            load("$${ORDER_ID} $$${ORDER_ID}"),
            "$${ORDER_ID} $$${ORDER_ID}"
        );
        assert_eq!(
            load("${ORDER_ID:-none} $${HOST}"),
            "${ORDER_ID:-none} ${HOST}"
        );
        assert_eq!(load("${NOPE_CAP:-${ORDER_ID}}"), "${ORDER_ID}");
        // A typo is still caught at load time.
        assert!(interpolate_except("${ORDR_ID}", &o, &d).is_err());
    }

    #[test]
    fn the_run_fills_only_the_captured_names() {
        let d = names(&["ORDER_ID", "LATER"]);
        let cap: HashMap<String, String> = [("ORDER_ID".to_string(), "o-7".to_string())].into();
        let run = |s: &str| bind_captured(s, &cap, &d);
        assert_eq!(run("/orders/${ORDER_ID}").unwrap(), "/orders/o-7");
        // What load time made literal stays literal: `$${HOME}` became
        // `${HOME}` then, and is not a placeholder now.
        assert_eq!(
            run("${HOME} $$${ORDER_ID} $${ORDER_ID}").unwrap(),
            "${HOME} $o-7 ${ORDER_ID}"
        );
        assert_eq!(run("${LATER:-none}").unwrap(), "none");
        let e = run("${LATER}").unwrap_err().to_string();
        assert!(e.contains("`${LATER}` has no value yet"), "{e}");
        let e = run("${LATER:?log in first}").unwrap_err().to_string();
        assert!(e.contains("`${LATER}` has no value yet"), "{e}");
    }

    #[test]
    fn a_suite_records_what_its_http_steps_capture() {
        let mut suite = crate::parse_suite_from_str(
            r#"
flows:
  - id: login
    steps:
      - http: { url: "https://x.dev/login", capture: { TOKEN: token } }
tests:
  - name: t
    steps:
      - runFlow: login
      - http:
          url: "https://x.dev/orders/${ORDER_ID_VEL}"
          headers: { Authorization: "Bearer ${TOKEN}" }
          capture: { ORDER_ID_VEL: "$.id" }
"#,
        )
        .unwrap();
        interpolate_suite(&mut suite, &HashMap::new()).unwrap();
        assert_eq!(
            suite.config.captured_vars,
            names(&["ORDER_ID_VEL", "TOKEN"])
        );
        let step = &suite.tests[0].steps[1];
        let cap: HashMap<String, String> = [
            ("TOKEN".to_string(), "t0k".to_string()),
            ("ORDER_ID_VEL".to_string(), "o-9".to_string()),
        ]
        .into();
        let bound = bind_step(step, &cap, &suite.config.captured_vars)
            .unwrap()
            .unwrap();
        let Action::Http(req) = &bound.action else {
            panic!()
        };
        assert_eq!(req.url, "https://x.dev/orders/o-9");
        assert_eq!(req.headers[0].1, "Bearer t0k");
        // A step with nothing captured in it is left alone.
        assert!(
            bind_step(&suite.tests[0].steps[0], &cap, &suite.config.captured_vars)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn within_selectors_are_interpolated() {
        // VEL-196: `within: { id: ${LIST} }` was looked up literally.
        let mut suite = crate::parse_suite_from_str(
            r#"
appId: x
tests:
  - name: t
    steps:
      - scrollUntilVisible: { selector: { text: "Row" }, within: { id: "${VEL196_LIST}" } }
      - swipe: { direction: up, within: { id: "${VEL196_LIST}" } }
"#,
        )
        .unwrap();
        interpolate_suite(&mut suite, &overrides(&[("VEL196_LIST", "feed")])).unwrap();
        let feed = Some(Selector::Id("feed".into()));
        match &suite.tests[0].steps[0].action {
            Action::ScrollUntilVisible { within, .. } => assert_eq!(within, &feed),
            other => panic!("{other:?}"),
        }
        match &suite.tests[0].steps[1].action {
            Action::Swipe { within, .. } => assert_eq!(within, &feed),
            other => panic!("{other:?}"),
        }
    }
}
