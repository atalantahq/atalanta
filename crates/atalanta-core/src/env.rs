use std::collections::{BTreeSet, HashMap};

use atalanta_common::{Action, AtalantaError, Result, Selector, TestSuite};

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

/// `text` written so that [`interpolate`] gives it back as is: every run of
/// `$` before a `{` is doubled (D-13), so `${HOME}` stays `${HOME}`.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = 0usize;
    for c in text.chars() {
        if c == '$' {
            run += 1;
            continue;
        }
        let dollars = if c == '{' { run * 2 } else { run };
        out.extend(std::iter::repeat_n('$', dollars));
        run = 0;
        out.push(c);
    }
    out.extend(std::iter::repeat_n('$', run));
    out
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
    too_deep(input)?;
    let mut missing_vars = Vec::new();
    let lookup = |name: &str| lookup_var(name, overrides);
    let pass = Pass {
        fills: &|name| !deferred.contains(name),
        lookup: &lookup,
    };
    let result = expand(input, &pass, &mut missing_vars);
    if !missing_vars.is_empty() {
        return Err(AtalantaError::MissingEnvVars { vars: missing_vars });
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
    too_deep(input)?;
    let mut unset = Vec::new();
    let lookup = |name: &str| {
        captured
            .get(name)
            .cloned()
            .or_else(|| name.starts_with(TOTP).then(|| totp_value(name)).flatten())
    };
    let pass = Pass {
        fills: &|name| names.contains(name),
        lookup: &lookup,
    };
    let result = expand(input, &pass, &mut unset);
    if let Some(first) = unset.first() {
        // `${NAME:?message}` reports as `NAME: message`.
        let name = first.split(':').next().unwrap_or(first);
        return Err(AtalantaError::Config(format!(
            "`${{{name}}}` has no value yet: no earlier step of this test captured it \
             (an `http` step's `capture:` sets it; `${{{name}:-default}}` gives a fallback)"
        )));
    }
    Ok(result)
}

/// Deepest nesting of placeholders in defaults (`${A:-${B:-…}}`) a string
/// may have. Expansion recurses once per level, so a megabyte of nested
/// defaults overflowed the stack (a SIGSEGV, not an error; audit 2026-11
/// CORE-1), in `atalanta run` and in the MCP server's check of a suite an
/// agent wrote.
pub const MAX_PLACEHOLDER_NESTING: usize = 32;

/// How deeply `${` placeholders nest in `input` (braces counted the way
/// [`closing_brace`] pairs them, approximately: a stray `}` is ignored).
fn placeholder_nesting(input: &str) -> usize {
    let bytes = input.as_bytes();
    let mut stack: Vec<bool> = Vec::new();
    let mut open = 0usize;
    let mut max = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'{' => {
                let placeholder = i > 0 && bytes[i - 1] == b'$';
                stack.push(placeholder);
                if placeholder {
                    open += 1;
                    max = max.max(open);
                }
            }
            b'}' if stack.pop() == Some(true) => open -= 1,
            _ => {}
        }
    }
    max
}

fn too_deep(input: &str) -> Result<()> {
    let depth = placeholder_nesting(input);
    if depth > MAX_PLACEHOLDER_NESTING {
        return Err(AtalantaError::Config(format!(
            "a value nests `${{…}}` placeholders {depth} deep; at most \
             {MAX_PLACEHOLDER_NESTING} levels are allowed"
        )));
    }
    Ok(())
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
        if atalanta_common::redact::is_secret_name(var_name)
            && default_val.chars().count() >= MIN_SECRET_DEFAULT
        {
            atalanta_common::redact::register_secret(&default_val);
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

const KEYCHAIN: &str = "keychain:";
const TOTP: &str = "totp:";

/// The base32 secret behind each `${totp:NAME}`, read when the suite loads
/// (from `--env`, the environment or the keychain), so the code can be
/// worked out when its step runs.
static TOTP_SECRETS: std::sync::RwLock<Option<HashMap<String, String>>> =
    std::sync::RwLock::new(None);

/// The `totp:…` names in `s`.
fn totp_names(s: &str) -> Vec<String> {
    s.match_indices("${totp:")
        .filter_map(|(at, _)| {
            let body = &s[at + 2..];
            body.find('}').map(|end| body[..end].to_string())
        })
        .collect()
}

/// The current code for a `totp:NAME` placeholder, from its stored secret.
fn totp_value(name: &str) -> Option<String> {
    let secret = TOTP_SECRETS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()?
        .get(name)?
        .clone();
    match atalanta_common::secrets::totp_now(&secret) {
        Ok(code) => Some(code),
        Err(e) => {
            tracing::warn!("{name}: {e}");
            None
        }
    }
}

fn lookup_var(name: &str, overrides: &HashMap<String, String>) -> Option<String> {
    // `${keychain:NAME}` (row 1.5): the OS keychain, always a secret.
    if let Some(account) = name.strip_prefix(KEYCHAIN) {
        return match atalanta_common::secrets::keychain(account) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("{e}");
                None
            }
        };
    }
    let value = overrides
        .get(name)
        .cloned()
        .or_else(|| atalanta_common::env::var(name).ok());
    // A secret-looking variable's value is masked in every report, trace,
    // console line and MCP reply from here on.
    if let Some(v) = &value {
        if atalanta_common::redact::is_secret_name(name) {
            atalanta_common::redact::register_secret(v);
            atalanta_common::redact::register_secret_name(name);
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
        .flat_map(|s| match &s.action {
            Action::Http(req) => req.capture.iter().map(|(name, _)| name.clone()).collect(),
            Action::Exec(e) => e.capture.iter().cloned().collect(),
            _ => Vec::new(),
        })
        .collect()
}

/// Fill in every `${VAR}` of the suite from `overrides` and the environment,
/// except the names `http` steps capture and the names of flow parameters:
/// those are left for the executor to fill in as the test runs ([`bind_step`]
/// and the flow scope stack), and recorded here so load-time interpolation
/// neither fails on them nor empties them.
/// Every `runFlow: { flow, env }` parameter name in the suite.
pub fn flow_param_names(suite: &TestSuite) -> BTreeSet<String> {
    suite
        .flows
        .iter()
        .flat_map(|f| f.steps.iter())
        .chain(suite.tests.iter().flat_map(|t| t.steps.iter()))
        .filter_map(|s| match &s.action {
            Action::RunFlow { env, .. } => Some(env.keys().cloned()),
            _ => None,
        })
        .flatten()
        .collect()
}

pub fn interpolate_suite(suite: &mut TestSuite, overrides: &HashMap<String, String>) -> Result<()> {
    let mut deferred = captured_names(suite);
    // `runFlow: { flow, env }` parameter names stay `${NAME}` inside
    // `flows:` (the resolver fills them per call); tests are interpolated
    // as before, so a parameter never hides an environment variable there.
    let params = flow_param_names(suite);
    let in_flow = std::cell::Cell::new(false);
    // `${totp:NAME}` is filled when its step runs (a code lasts 30 s); its
    // secret is read now, while `--env` and the keychain are at hand.
    let totp = std::cell::RefCell::new(BTreeSet::new());
    let mut missing = Vec::new();
    let mut fill = |s: &str| {
        for name in totp_names(s) {
            if totp.borrow_mut().insert(name.clone()) {
                match lookup_var(&name[TOTP.len()..], overrides) {
                    Some(secret) => {
                        atalanta_common::redact::register_secret(&secret);
                        TOTP_SECRETS
                            .write()
                            .unwrap_or_else(|e| e.into_inner())
                            .get_or_insert_with(HashMap::new)
                            .insert(name.clone(), secret);
                    }
                    None => missing.push(name[TOTP.len()..].to_string()),
                }
            }
        }
        let mut skip: BTreeSet<String> = deferred.union(&totp.borrow()).cloned().collect();
        if in_flow.get() {
            skip.extend(params.iter().cloned());
        }
        interpolate_except(s, overrides, &skip)
    };
    suite.app_id = fill(&suite.app_id)?;
    if let Some(http) = suite.config.http.as_mut() {
        if let Some(base) = http.base_url.as_mut() {
            *base = fill(base)?;
        }
        for (_, v) in http.headers.iter_mut() {
            *v = fill(v)?;
        }
    }

    in_flow.set(true);
    for flow in &mut suite.flows {
        for step in &mut flow.steps {
            interpolate_action(&mut step.action, &mut fill)?;
        }
    }
    in_flow.set(false);

    for test in &mut suite.tests {
        for step in &mut test.steps {
            interpolate_action(&mut step.action, &mut fill)?;
        }
    }

    if let Some(name) = missing.first() {
        return Err(AtalantaError::Config(format!(
            "`${{totp:{name}}}`: {name} has no value; set it to the base32 TOTP secret \
             (or use totp:keychain:{name})"
        )));
    }
    deferred.extend(totp.into_inner());
    // What is still `${…}` in the flows now is a parameter name (or a name
    // in its default): read the environment for those once, here, so a call
    // that doesn't pass one gets the value any other `${NAME}` would.
    let mut flow_env = HashMap::new();
    if !params.is_empty() {
        let flows_only = TestSuite {
            tests: Vec::new(),
            ..suite.clone()
        };
        for p in placeholders(&flows_only) {
            let name = p.name;
            if deferred.contains(&name) || name.starts_with(TOTP) || flow_env.contains_key(&name) {
                continue;
            }
            if let Some(v) = lookup_var(&name, overrides) {
                flow_env.insert(name, v);
            }
        }
    }
    suite.config.flow_env = flow_env;
    suite.config.captured_vars = deferred;
    Ok(())
}

/// One `${…}` placeholder a suite reads, and whether its value is only
/// typed into the device (`inputText`, `inputRaw`, a natural step's
/// `data`) rather than sent anywhere else (a URL, a header, a selector, a
/// prompt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placeholder {
    /// The name as written: `LOGIN_PW`, `keychain:LOGIN_PW`,
    /// `totp:keychain:OTP`. A default's own placeholders count too
    /// (`${A:-${B}}` reads A and B); an escaped `$${A}` reads nothing.
    pub name: String,
    pub typed: bool,
}

/// Every placeholder [`interpolate_suite`] and [`bind_step`] would fill in,
/// in the same strings they visit, without reading any value. A caller
/// that runs a suite someone else wrote (the MCP server, with a suite an
/// agent wrote) checks these against what it allows before the suite can
/// read the environment or the keychain.
pub fn placeholders(suite: &TestSuite) -> Vec<Placeholder> {
    fn push(s: &str, typed: bool, out: &mut Vec<Placeholder>) {
        if placeholder_nesting(s) > MAX_PLACEHOLDER_NESTING {
            // Not walked (it would overflow the stack); a name no allow-list
            // holds, so a check built on this list refuses the string.
            out.push(Placeholder {
                name: "<placeholders nested too deep>".into(),
                typed,
            });
            return;
        }
        let mut names = Vec::new();
        placeholder_names(s, &mut names);
        out.extend(names.into_iter().map(|name| Placeholder { name, typed }));
    }
    let mut out = Vec::new();
    push(&suite.app_id, false, &mut out);
    if let Some(http) = suite.config.http.as_ref() {
        if let Some(base) = &http.base_url {
            push(base, false, &mut out);
        }
        for (_, v) in &http.headers {
            push(v, false, &mut out);
        }
    }
    let steps = suite
        .flows
        .iter()
        .flat_map(|f| &f.steps)
        .chain(suite.tests.iter().flat_map(|t| &t.steps));
    // `atalanta run` fills tags too (VEL-175; audit 2026-11 MCP-6).
    for tag in suite.tests.iter().flat_map(|t| &t.tags) {
        push(tag, false, &mut out);
    }
    for step in steps {
        let mut action = step.action.clone();
        // The typed value of a typing step is taken out; the rest of the
        // step is visited as interpolation visits it.
        let typed: Option<String> = match &mut action {
            Action::InputText { text, .. } | Action::InputRaw { text } => {
                Some(std::mem::take(text))
            }
            Action::Natural { data, .. } => data.take(),
            _ => None,
        };
        if let Some(t) = typed {
            push(&t, true, &mut out);
        }
        let _ = interpolate_action(&mut action, &mut |s: &str| {
            push(s, false, &mut out);
            Ok(s.to_string())
        });
    }
    out
}

/// The names of the placeholders in `input`, read the way [`expand`]
/// reads them (escapes, nesting, defaults).
fn placeholder_names(input: &str, out: &mut Vec<String>) {
    let mut rest = input;
    while let Some(at) = rest.find("${") {
        let pre = &rest[..at];
        let k = pre.len() - pre.trim_end_matches('$').len();
        let body = &rest[at + 2..];
        if (k + 1).is_multiple_of(2) {
            // `$${` is a literal `${`.
            rest = body;
            continue;
        }
        match closing_brace(body) {
            Some(end) if end > 0 => {
                let expr = &body[..end];
                out.push(var_name(expr).to_string());
                if let Some((i, b'-')) = operator(expr) {
                    placeholder_names(&expr[i + 2..], out);
                }
                rest = &body[end + 1..];
            }
            Some(end) => rest = &body[end + 1..],
            None => return,
        }
    }
}

/// `step` with the captured `${NAME}`s (`names`) filled in from `captured`,
/// or `None` when it has none to fill.
pub fn bind_step(
    step: &atalanta_common::Step,
    captured: &HashMap<String, String>,
    names: &BTreeSet<String>,
) -> Result<Option<atalanta_common::Step>> {
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
    Ok(changed.then(|| atalanta_common::Step {
        action,
        ..step.clone()
    }))
}

fn interpolate_action(
    action: &mut Action,
    f: &mut dyn FnMut(&str) -> Result<String>,
) -> Result<()> {
    match action {
        Action::LaunchApp {
            app_id,
            clear_state: _,
            arguments,
        } => {
            *app_id = f(app_id)?;
            for value in arguments.inner.values_mut() {
                if let atalanta_common::LaunchArg::Str(s) = value {
                    *s = f(s)?;
                }
            }
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
        Action::Exec(e) => {
            e.run.split_before_filling();
            for part in e.run.strings_mut() {
                *part = f(part)?;
            }
            if let Some(c) = e.cwd.as_mut() {
                *c = f(c)?;
            }
            if let Some(c) = e.expect.contains.as_mut() {
                *c = f(c)?;
            }
        }
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
        Action::Drag { from, to, .. } => {
            interpolate_selector(from, f)?;
            interpolate_selector(to, f)?;
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
        Action::RunFlow { flow_id, env } => {
            for v in env.values_mut() {
                *v = f(v)?;
            }
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
        | Action::Refresh
        | Action::HideKeyboard => {}
        Action::Goto { url } => *url = f(url)?,
        Action::SaveState { name } | Action::RestoreState { name } => *name = f(name)?,
        Action::AssertScreenshot {
            name, mask, crop, ..
        } => {
            *name = f(name)?;
            for sel in mask {
                interpolate_selector(sel, f)?;
            }
            if let Some(atalanta_common::ScreenshotCrop::Element(sel)) = crop {
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
            AtalantaError::MissingEnvVars { vars } => {
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
        use atalanta_common::{SuiteConfig, TestCase};

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
        use atalanta_common::{Step, SuiteConfig, TestCase};

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
        assert!(!atalanta_common::redact::redact("x default-secret-062 y")
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
        assert!(atalanta_common::redact::redact("order 4417").contains("4417"));
        // D-29: masked from 8 characters; a 7-character default is not.
        interpolate("${VEL_D29_TOKEN:-d29sev7}", &HashMap::new()).unwrap();
        assert!(atalanta_common::redact::redact("x d29sev7 y").contains("d29sev7"));
        interpolate("${VEL_D29_TOKEN2:-d29eight}", &HashMap::new()).unwrap();
        assert!(!atalanta_common::redact::redact("x d29eight y").contains("d29eight"));
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

    // Row 1.5: `${totp:NAME}` stays a placeholder at load and becomes the
    // current code when its step runs; the secret and the code are masked.
    #[test]
    fn a_totp_code_is_worked_out_when_its_step_runs() {
        let mut suite = crate::parser::parse_suite_from_str(
            r#"
appId: com.app
tests:
  - name: t
    steps:
      - inputText: { selector: { id: otp }, text: "${totp:VEL15_TOTP_SECRET}" }
"#,
        )
        .unwrap();
        let ovr = overrides(&[("VEL15_TOTP_SECRET", "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ")]);
        interpolate_suite(&mut suite, &ovr).unwrap();
        assert!(suite
            .config
            .captured_vars
            .contains("totp:VEL15_TOTP_SECRET"));
        let Action::InputText { text, .. } = &suite.tests[0].steps[0].action else {
            panic!()
        };
        assert_eq!(text, "${totp:VEL15_TOTP_SECRET}", "not filled at load");
        let bound = bind_step(
            &suite.tests[0].steps[0],
            &HashMap::new(),
            &suite.config.captured_vars,
        )
        .unwrap()
        .unwrap();
        let Action::InputText { text, .. } = &bound.action else {
            panic!()
        };
        assert!(
            text.len() == 6 && text.chars().all(|c| c.is_ascii_digit()),
            "{text}"
        );
        let masked = atalanta_common::redact::redact(&format!("code {text}")).into_owned();
        assert!(!masked.contains(text.as_str()), "{masked}");
        assert!(atalanta_common::redact::registered_secrets()
            .contains(&"GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ".to_string()));

        // A missing secret is a config error naming it.
        let mut bad = crate::parser::parse_suite_from_str(
            "appId: a\ntests:\n  - name: t\n    steps:\n      - inputText: { selector: { id: o }, text: \"${totp:VEL15_NOPE}\" }\n",
        )
        .unwrap();
        let e = interpolate_suite(&mut bad, &HashMap::new()).unwrap_err();
        assert!(e.to_string().contains("VEL15_NOPE has no value"), "{e}");
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

    // Review 05 #1: what a suite reads is listed without reading it, and
    // a typed value is told apart from one sent anywhere else.
    #[test]
    fn placeholders_lists_every_name_and_where_it_goes() {
        let suite = crate::parse_suite_from_str(
            r#"
appId: "${APP:-com.app}"
config:
  http:
    baseUrl: "https://${HOST}"
    headers: { Authorization: "Bearer ${TOKEN}" }
tests:
  - name: t
    steps:
      - inputText: { selector: { id: "${FIELD}" }, text: "${keychain:LOGIN_PW}" }
      - http: { url: "https://x.dev/c", method: POST, body: "${AWS_SECRET_ACCESS_KEY} $${LITERAL} ${A:-${B}}" }
      - assertVisible: { text: "${totp:OTP}" }
"#,
        )
        .unwrap();
        let got: Vec<(String, bool)> = placeholders(&suite)
            .into_iter()
            .map(|p| (p.name, p.typed))
            .collect();
        let want: Vec<(String, bool)> = [
            ("APP", false),
            ("HOST", false),
            ("TOKEN", false),
            ("keychain:LOGIN_PW", true),
            ("FIELD", false),
            ("AWS_SECRET_ACCESS_KEY", false),
            ("A", false),
            ("B", false),
            ("totp:OTP", false),
        ]
        .iter()
        .map(|(n, t)| (n.to_string(), *t))
        .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn escaped_text_interpolates_back_to_itself() {
        for s in [
            "${HOME}",
            "$${X}",
            "$$${X} and $5",
            "plain",
            "a$",
            "{x}",
            "${A:-${B}}",
        ] {
            let e = escape(s);
            assert_eq!(interpolate(&e, &HashMap::new()).unwrap(), s, "{e}");
            assert!(placeholder_names_of(&e).is_empty(), "{e}");
        }
    }

    fn placeholder_names_of(s: &str) -> Vec<String> {
        let mut v = Vec::new();
        placeholder_names(s, &mut v);
        v
    }

    #[test]
    fn placeholder_names_follow_the_escape_rules() {
        let names = |s: &str| {
            let mut v = Vec::new();
            placeholder_names(s, &mut v);
            v
        };
        assert!(names("$${X}").is_empty());
        assert_eq!(names("$$${X}"), vec!["X"]);
        assert_eq!(names("${X:?need X} ${}"), vec!["X"]);
        assert_eq!(names("${unclosed"), Vec::<String>::new());
    }

    #[test]
    fn deeply_nested_defaults_are_an_error_not_a_stack_overflow() {
        let n = 200_000;
        let deep = format!("{}x{}", "${A_UNSET_AUDIT:-".repeat(n), "}".repeat(n));
        let e = interpolate(&deep, &HashMap::new()).unwrap_err();
        assert!(e.to_string().contains("nests"), "{e}");
        // A few levels still work.
        let ok = "${A_UNSET_AUDIT:-${B_UNSET_AUDIT:-fallback}}";
        assert_eq!(interpolate(ok, &HashMap::new()).unwrap(), "fallback");
        // JSON braces are not placeholders.
        let json = format!("{}{}", "{\"a\":".repeat(100), "1}".repeat(100));
        assert_eq!(placeholder_nesting(&json), 0);
    }
}
