// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta api scaffold`: an API test suite from an OpenAPI 3 document.
//!
//! One test per resource (`/pets`, `/pets/{petId}` → `pets`), its requests
//! ordered so a value is captured before a later request uses it. What one
//! operation returns is linked to what another takes by name (`id` of
//! `POST /pets` → `{petId}`), scored, the best above a threshold kept:
//! AutoRestTest (ICSE 2025) found a graph of such links the main reason it
//! beat four other REST testers. Each request checks what its response
//! schema promises ([`mine`]), and [`confirm`](fn@confirm) drops the checks a real
//! server's GET responses disprove (RBCTest, ICSE 2026, does the same with
//! its mined constraints). Bodies, links and checks never invent a secret:
//! security schemes become `${API_TOKEN}`-style variables.

pub mod confirm;
pub mod mine;
mod render;
pub mod spec;
pub mod values;

use atalanta_common::{Capture, HttpConfig, HttpExpect, HttpRequest};
use serde_json::Value;

pub use confirm::{confirm, ConfirmReport};
pub use render::render;
pub use spec::Spec;

/// A link below this score is no link: the parameter becomes a variable.
const LINK_THRESHOLD: f64 = 0.75;

pub struct Options {
    /// `--base-url`; else the spec's first server.
    pub base_url: Option<String>,
    /// Most `expect.json` checks per request.
    pub max_checks: usize,
    /// Where the spec came from, for the header comment.
    pub source: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            base_url: None,
            max_checks: 12,
            source: "the spec".into(),
        }
    }
}

/// The suite before it is written: [`confirm`](fn@confirm) edits it, [`render`]
/// writes it.
#[derive(Debug, Clone)]
pub struct Scaffold {
    pub title: String,
    pub source: String,
    pub config: HttpConfig,
    pub tests: Vec<TestPlan>,
    /// `${NAME}`s the user sets, and what each is for.
    pub variables: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct TestPlan {
    pub name: String,
    pub steps: Vec<StepPlan>,
}

#[derive(Debug, Clone)]
pub struct StepPlan {
    /// `GET /pets/{petId} (getPet)`.
    pub label: String,
    pub request: HttpRequest,
    /// Comment lines written above the step.
    pub notes: Vec<String>,
}

impl Scaffold {
    pub fn requests(&self) -> usize {
        self.tests.iter().map(|t| t.steps.len()).sum()
    }

    pub fn checks(&self) -> usize {
        self.tests
            .iter()
            .flat_map(|t| &t.steps)
            .map(|s| s.request.expect.json.len())
            .sum()
    }
}

pub fn scaffold(spec: &Spec, opts: &Options) -> Result<Scaffold, String> {
    let operations = spec.operations();
    if operations.is_empty() {
        return Err("the spec has no operations under `paths`".into());
    }
    let ops: Vec<Op> = operations.iter().map(|o| Op::new(spec, o)).collect();
    let mut vars = Variables::default();

    let base_url = match &opts.base_url {
        Some(b) => b.clone(),
        None => match spec.server() {
            Some(s) if s.starts_with("http://") || s.starts_with("https://") => s,
            Some(path) => {
                vars.add(
                    "API_URL",
                    "the server, before the spec's relative server url",
                );
                format!("${{API_URL}}{path}")
            }
            None => {
                vars.add("API_URL", "the server (the spec lists none)");
                "${API_URL}".into()
            }
        },
    };
    let auth = Auth::new(spec, &mut vars);
    let config = HttpConfig {
        base_url: Some(base_url),
        headers: auth.config_headers(),
        ..Default::default()
    };

    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        match groups.iter_mut().find(|(g, _)| *g == op.group) {
            Some((_, members)) => members.push(i),
            None => groups.push((op.group.clone(), vec![i])),
        }
    }
    let tests = groups
        .iter()
        .map(|(name, members)| TestPlan {
            name: name.clone(),
            steps: plan_test(spec, &ops, members, &auth, &mut vars, opts),
        })
        .collect();
    let mut scaffold = Scaffold {
        title: spec.title(),
        source: opts.source.clone(),
        config,
        tests,
        variables: vars.list,
    };
    defuse(&mut scaffold);
    Ok(scaffold)
}

/// Make every `${...}` the spec wrote inert. A spec is someone else's text:
/// a server url, a path, a default or an enum value of
/// `${AWS_SECRET_ACCESS_KEY}` would be filled in from the environment when
/// the suite runs (or at once, under `--confirm`) and sent to the spec's
/// server. Only the placeholders this module wrote (`${API_TOKEN}`,
/// `${PET_ID}`, ...) stay live; any other run of `$` before a `{` is
/// doubled, which the loader reads back as the text the spec had (D-13).
fn defuse(s: &mut Scaffold) {
    let mut ours: Vec<String> = s.variables.iter().map(|(n, _)| n.clone()).collect();
    for step in s.tests.iter().flat_map(|t| &t.steps) {
        ours.extend(step.request.capture.iter().map(|(n, _)| n.clone()));
    }
    let mut inert = |t: &str| -> atalanta_common::Result<String> { Ok(defuse_text(t, &ours)) };
    if let Some(base) = s.config.base_url.as_mut() {
        *base = defuse_text(base, &ours);
    }
    for (_, v) in s.config.headers.iter_mut() {
        *v = defuse_text(v, &ours);
    }
    for step in s.tests.iter_mut().flat_map(|t| &mut t.steps) {
        // The closure never fails.
        let _ = step.request.map_strings(&mut inert);
    }
}

fn defuse_text(text: &str, ours: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = 0usize;
    for (i, c) in text.char_indices() {
        if c == '$' {
            run += 1;
            continue;
        }
        let mut dollars = run;
        if c == '{' && run > 0 {
            let rest = &text[i + 1..];
            let name = rest.split(['}', ':']).next().unwrap_or("");
            let live =
                run == 1 && rest[name.len()..].starts_with('}') && ours.iter().any(|n| n == name);
            if !live {
                dollars = run * 2;
            }
        }
        out.extend(std::iter::repeat_n('$', dollars));
        run = 0;
        out.push(c);
    }
    out.extend(std::iter::repeat_n('$', run));
    out
}

/// An operation with what linking needs to know about it.
struct Op<'a> {
    op: &'a spec::Operation,
    /// The test it goes in: its path's first resource segment.
    group: String,
    /// Its path's last resource segment, singular, as written (`line-item`).
    noun: String,
    fields: Vec<Field>,
    needs: Vec<Need>,
    /// The request body's least valid value (or its example).
    body: Option<Value>,
}

/// A value a response carries.
struct Field {
    path: String,
    name: String,
    kind: Option<String>,
    /// How sure a capture of it is to work: an element of a list may be
    /// absent.
    weight: f64,
}

/// A value a request needs.
#[derive(Clone)]
struct Need {
    name: String,
    location: Location,
    kind: Option<String>,
    /// For a path parameter: the singular segment before it (`pet` for
    /// `/pets/{id}`).
    context: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Location {
    Path,
    Query,
    Header,
    Body,
}

impl<'a> Op<'a> {
    fn new(spec: &Spec, op: &'a spec::Operation) -> Self {
        let segments: Vec<&str> = op.path.split('/').filter(|s| !s.is_empty()).collect();
        let literal = |s: &str| !s.starts_with('{');
        let group = segments
            .iter()
            .find(|s| literal(s) && !is_version(s))
            .map_or_else(|| "root".to_string(), |s| s.to_string());
        let noun = segments
            .iter()
            .rev()
            .find(|s| literal(s))
            .map_or_else(|| "item".to_string(), |s| singular(s));

        let mut needs = Vec::new();
        for p in &op.params {
            let location = match p.location.as_str() {
                "path" => Location::Path,
                "query" => Location::Query,
                "header" => Location::Header,
                _ => continue,
            };
            if !p.required {
                continue;
            }
            let context = (location == Location::Path)
                .then(|| {
                    let at = segments
                        .iter()
                        .position(|s| *s == format!("{{{}}}", p.name))?;
                    segments[..at]
                        .iter()
                        .rev()
                        .find(|s| literal(s))
                        .map(|s| singular(s))
                })
                .flatten();
            needs.push(Need {
                name: p.name.clone(),
                location,
                kind: values::type_of(&spec.flatten(&p.schema)),
                context,
            });
        }
        let body = op.body.as_ref().and_then(|b| {
            let schema = b.json.as_ref()?;
            let flat = spec.flatten(schema);
            for name in values::required(&flat) {
                let prop = flat.pointer(&format!("/properties/{}", pointer_escape(&name)));
                needs.push(Need {
                    kind: prop.and_then(|p| values::type_of(&spec.flatten(p))),
                    name,
                    location: Location::Body,
                    context: None,
                });
            }
            Some(
                b.example
                    .clone()
                    .unwrap_or_else(|| values::example(spec, schema)),
            )
        });

        let mut fields = Vec::new();
        if let Some(schema) = op.success.as_ref().and_then(|s| s.schema.as_ref()) {
            let root = spec.flatten(schema);
            match values::type_of(&root).as_deref() {
                Some("array") => {
                    if let Some(items) = root.get("items") {
                        collect_fields(spec, &spec.flatten(items), "$[0]", 0.8, &mut fields);
                    }
                }
                _ => {
                    collect_fields(spec, &root, "$", 1.0, &mut fields);
                    if let Some(data) = root.pointer("/properties/data") {
                        collect_fields(spec, &spec.flatten(data), "$.data", 0.9, &mut fields);
                    }
                }
            }
        }
        Op {
            op,
            group,
            noun,
            fields,
            needs,
            body,
        }
    }

    fn can_produce(&self) -> bool {
        matches!(self.op.method.as_str(), "POST" | "GET" | "PUT" | "PATCH")
            && !self.fields.is_empty()
    }

    /// Creates first, reads next, changes after, deletes last.
    fn rank(&self) -> (u8, usize) {
        let ends_in_param = self.op.path.trim_end_matches('/').ends_with('}');
        let r = match self.op.method.as_str() {
            "POST" => 0,
            "GET" if !ends_in_param => 1,
            "GET" | "HEAD" => 2,
            "PUT" | "PATCH" => 3,
            "DELETE" => 9,
            _ => 4,
        };
        (r, self.op.path.matches('/').count())
    }

    fn label(&self) -> String {
        match &self.op.id {
            Some(id) => format!("{} {} ({id})", self.op.method, self.op.path),
            None => format!("{} {}", self.op.method, self.op.path),
        }
    }
}

fn collect_fields(spec: &Spec, s: &Value, at: &str, weight: f64, out: &mut Vec<Field>) {
    let Some(props) = s.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (name, p) in props {
        let p = spec.flatten(p);
        if p.get("writeOnly") == Some(&Value::Bool(true)) {
            continue;
        }
        let plain = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !plain {
            continue;
        }
        out.push(Field {
            path: format!("{at}.{name}"),
            name: name.clone(),
            kind: values::type_of(&p),
            weight,
        });
    }
}

/// How well `field` of `producer`'s response fills `need`, 0 to 1.
fn score(producer: &Op, field: &Field, need: &Need) -> f64 {
    let scalar = |t: &str| matches!(t, "string" | "integer" | "number");
    if let (Some(a), Some(b)) = (&field.kind, &need.kind) {
        if a != b && !(scalar(a) && scalar(b)) {
            return 0.0;
        }
    }
    let (nf, nn, noun) = (norm(&field.name), norm(&need.name), norm(&producer.noun));
    // Outside the path, only ids are worth a link: a `name` in a body is
    // better made up than copied.
    if need.location != Location::Path && !nn.ends_with("id") {
        return 0.0;
    }
    let by_name = if nf == nn {
        match (nn.as_str(), &need.context) {
            ("id", Some(c)) if norm(c) == noun => 1.0,
            ("id", _) => 0.3,
            _ => 1.0,
        }
    } else if nf == "id" && nn == format!("{noun}id") {
        0.95
    } else if nf == "id"
        && need.location == Location::Path
        && need.context.as_deref().map(norm) == Some(noun.clone())
    {
        0.85
    } else if nn == format!("{noun}{nf}") {
        0.8
    } else {
        0.7 * jaccard(&words(&field.name), &words(&need.name))
    };
    let method = match producer.op.method.as_str() {
        "POST" => 1.0,
        "GET" => 0.95,
        _ => 0.9,
    };
    by_name * field.weight * method
}

/// A request's need: (operation, need) indexes.
type NeedAt = (usize, usize);
/// What fills it: (producer operation, field, score).
type Link = (usize, usize, f64);

/// One test's steps: its operations ordered producers first, each need
/// filled by a capture, a value, or a variable.
fn plan_test(
    spec: &Spec,
    ops: &[Op],
    members: &[usize],
    auth: &Auth,
    vars: &mut Variables,
    opts: &Options,
) -> Vec<StepPlan> {
    // (consumer, need) → (producer, field, score)
    let mut links: Vec<(NeedAt, Link)> = Vec::new();
    for &b in members {
        for (ni, need) in ops[b].needs.iter().enumerate() {
            let best = members
                .iter()
                .filter(|&&a| a != b && ops[a].can_produce())
                .flat_map(|&a| {
                    ops[a]
                        .fields
                        .iter()
                        .enumerate()
                        .map(move |(fi, f)| (a, fi, score(&ops[a], f, need)))
                })
                .filter(|(_, _, s)| *s >= LINK_THRESHOLD)
                .max_by(|x, y| x.2.total_cmp(&y.2));
            if let Some(best) = best {
                links.push(((b, ni), best));
            }
        }
    }

    // Kahn's order, ties (and a cycle) broken by rank, then the spec's order.
    let mut order: Vec<usize> = Vec::new();
    while order.len() < members.len() {
        let ready = |i: &usize| {
            !order.contains(i)
                && links
                    .iter()
                    .filter(|((b, _), _)| b == i)
                    .all(|(_, (a, _, _))| order.contains(a))
        };
        let pick = members
            .iter()
            .copied()
            .filter(ready)
            .min_by_key(|&i| (ops[i].rank(), i))
            .or_else(|| {
                members
                    .iter()
                    .copied()
                    .filter(|i| !order.contains(i))
                    .min_by_key(|&i| (ops[i].rank(), i))
            });
        match pick {
            Some(i) => order.push(i),
            None => break,
        }
    }
    let pos = |i: usize| order.iter().position(|&o| o == i).unwrap_or(usize::MAX);
    links.retain(|((b, _), (a, _, _))| pos(*a) < pos(*b));

    // A need nothing in this test fills may be filled by another resource's
    // create: it runs first, as a setup step (captures don't cross tests).
    let mut setups: Vec<usize> = Vec::new();
    for &b in &order {
        for (ni, need) in ops[b].needs.iter().enumerate() {
            if links.iter().any(|(k, _)| *k == (b, ni)) {
                continue;
            }
            let best = (0..ops.len())
                .filter(|a| {
                    !members.contains(a) && ops[*a].op.method == "POST" && ops[*a].can_produce()
                })
                .flat_map(|a| {
                    ops[a]
                        .fields
                        .iter()
                        .enumerate()
                        .map(move |(fi, f)| (a, fi, score(&ops[a], f, need)))
                })
                .filter(|(_, _, s)| *s >= LINK_THRESHOLD)
                .max_by(|x, y| x.2.total_cmp(&y.2));
            if let Some(best) = best {
                if !setups.contains(&best.0) {
                    setups.push(best.0);
                }
                links.push(((b, ni), best));
            }
        }
    }

    let capture_name = |a: usize, fi: usize| -> String {
        let f = &ops[a].fields[fi];
        if f.name == "id" {
            screaming(&format!("{}_id", ops[a].noun))
        } else {
            screaming(&f.name)
        }
    };
    let mut steps = Vec::new();
    for (i, &o) in setups.iter().chain(order.iter()).enumerate() {
        let is_setup = i < setups.len();
        let op = &ops[o];
        let filled = |ni: usize| -> Option<String> {
            if is_setup {
                return None;
            }
            links
                .iter()
                .find(|(k, _)| *k == (o, ni))
                .map(|(_, (a, fi, _))| capture_name(*a, *fi))
        };
        let mut req = HttpRequest {
            method: Some(op.op.method.clone()),
            url: op.op.path.clone(),
            headers: Vec::new(),
            query: Vec::new(),
            json: None,
            body: None,
            form: Vec::new(),
            expect: HttpExpect::default(),
            capture: Vec::new(),
            retry: None,
            follow_redirects: None,
        };
        let mut notes = Vec::new();
        if is_setup {
            notes.push(format!(
                "setup: {} creates a value this test needs",
                op.op.path
            ));
        }
        let mut json_links = Vec::new();
        for (ni, need) in op.needs.iter().enumerate() {
            let var = filled(ni);
            let param = op.op.params.iter().find(|p| p.name == need.name);
            match need.location {
                Location::Path => {
                    let name = var.unwrap_or_else(|| {
                        let v = env_name(&need.name);
                        vars.add(
                            &v,
                            &format!("{}: path parameter `{}`", op.label(), need.name),
                        );
                        v
                    });
                    req.url = req
                        .url
                        .replace(&format!("{{{}}}", need.name), &format!("${{{name}}}"));
                }
                Location::Query | Location::Header => {
                    let given = param.and_then(|p| {
                        p.example
                            .clone()
                            .or_else(|| literal_of(&spec.flatten(&p.schema)))
                    });
                    let value = match (var, given) {
                        (Some(v), _) => format!("${{{v}}}"),
                        (None, Some(g)) if need.location == Location::Query => spec::text_of(&g),
                        (None, _) => {
                            let v = env_name(&need.name);
                            let what = if need.location == Location::Query {
                                "query"
                            } else {
                                "header"
                            };
                            vars.add(
                                &v,
                                &format!("{}: {what} parameter `{}`", op.label(), need.name),
                            );
                            format!("${{{v}}}")
                        }
                    };
                    if need.location == Location::Query {
                        req.query.push((need.name.clone(), value));
                    } else {
                        req.headers.push((need.name.clone(), value));
                    }
                }
                Location::Body => {
                    if let Some(v) = var {
                        json_links.push((need.name.clone(), v));
                    }
                }
            }
        }
        match (&op.body, &op.op.body) {
            (Some(body), _) => {
                let mut body = body.clone();
                if let Value::Object(fields) = &mut body {
                    for (name, var) in json_links {
                        fields.insert(name, Value::String(format!("${{{var}}}")));
                    }
                }
                req.json = Some(body);
            }
            (None, Some(b)) => notes.push(format!(
                "the body is {}: not generated, add it by hand",
                b.media_type
            )),
            (None, None) => {}
        }
        auth.apply(op.op, &mut req);

        if let Some(success) = &op.op.success {
            req.expect.status = Some(success.status.clone());
            if let Some(schema) = &success.schema {
                let mined = mine::mine(spec, schema, opts.max_checks);
                req.expect.json = mined.checks;
                if mined.left_out > 0 {
                    notes.push(format!(
                        "{} more checks the spec implies were left out (limit {})",
                        mined.left_out, opts.max_checks
                    ));
                }
            }
        }
        // Capture what a later step of this test uses.
        for ((b, _), (a, fi, _)) in &links {
            if *a == o && *b != o {
                let name = capture_name(*a, *fi);
                if !req.capture.iter().any(|(n, _)| *n == name) {
                    req.capture
                        .push((name, Capture::Json(ops[*a].fields[*fi].path.clone())));
                }
            }
        }
        steps.push(StepPlan {
            label: op.label(),
            request: req,
            notes,
        });
    }
    steps
}

/// A value a parameter's schema names: its default, or its enum's first.
fn literal_of(s: &Value) -> Option<Value> {
    s.get("default")
        .or_else(|| s.get("enum").and_then(|e| e.get(0)))
        .cloned()
}

/// The `${NAME}`s a suite needs from its user, once each.
#[derive(Default)]
struct Variables {
    list: Vec<(String, String)>,
}

impl Variables {
    fn add(&mut self, name: &str, what: &str) {
        match self.list.iter_mut().find(|(n, _)| n == name) {
            Some((_, w)) if !w.contains(what) => {
                w.push_str("; ");
                w.push_str(what);
            }
            Some(_) => {}
            None => self.list.push((name.to_string(), what.to_string())),
        }
    }
}

/// Security schemes as headers or query parameters with a variable in
/// place of the secret.
struct Auth {
    schemes: Vec<(String, Inject)>,
    global: Vec<String>,
}

#[derive(Clone)]
enum Inject {
    Header(String, String),
    Query(String, String),
}

impl Auth {
    fn new(spec: &Spec, vars: &mut Variables) -> Self {
        let all = spec.security_schemes();
        let single = all.len() == 1;
        let mut schemes = Vec::new();
        for (name, s) in &all {
            let field = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("");
            let var = |suffix: &str| {
                if single {
                    format!("API_{suffix}")
                } else {
                    let base = env_name(name);
                    if base.ends_with(suffix) {
                        base
                    } else {
                        format!("{base}_{suffix}")
                    }
                }
            };
            let (inject, var, what) =
                match (field("type"), field("scheme").to_ascii_lowercase().as_str()) {
                    ("http", "basic") => {
                        let v = var("BASIC_AUTH");
                        (
                            Inject::Header("Authorization".into(), format!("Basic ${{{v}}}")),
                            v,
                            "base64 of user:password",
                        )
                    }
                    ("http", _) | ("oauth2", _) | ("openIdConnect", _) => {
                        let v = var("TOKEN");
                        (
                            Inject::Header("Authorization".into(), format!("Bearer ${{{v}}}")),
                            v,
                            "an access token",
                        )
                    }
                    ("apiKey", _) => {
                        let v = var("KEY");
                        let key = field("name").to_string();
                        let inject = match field("in") {
                            "query" => Inject::Query(key, format!("${{{v}}}")),
                            "cookie" => Inject::Header("Cookie".into(), format!("{key}=${{{v}}}")),
                            _ => Inject::Header(key, format!("${{{v}}}")),
                        };
                        (inject, v, "an API key")
                    }
                    _ => continue,
                };
            vars.add(&var, &format!("{what} (securitySchemes.{name})"));
            schemes.push((name.clone(), inject));
        }
        Self {
            schemes,
            global: spec.global_security(),
        }
    }

    fn get(&self, name: &str) -> Option<&Inject> {
        self.schemes.iter().find(|(n, _)| n == name).map(|(_, i)| i)
    }

    /// The document's security, on every request.
    fn config_headers(&self) -> Vec<(String, String)> {
        self.global
            .iter()
            .filter_map(|s| match self.get(s) {
                Some(Inject::Header(h, v)) => Some((h.clone(), v.clone())),
                _ => None,
            })
            .collect()
    }

    /// An operation's own security beyond the document's, and any key that
    /// goes in the query.
    fn apply(&self, op: &spec::Operation, req: &mut HttpRequest) {
        let own = op.security.as_ref().unwrap_or(&self.global);
        for s in own {
            match self.get(s) {
                Some(Inject::Header(h, v)) if !self.global.contains(s) => {
                    req.headers.push((h.clone(), v.clone()))
                }
                Some(Inject::Query(q, v)) => req.query.push((q.clone(), v.clone())),
                _ => {}
            }
        }
    }
}

fn is_version(segment: &str) -> bool {
    let s = segment.to_ascii_lowercase();
    s == "api"
        || s.strip_prefix('v')
            .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

/// `pets` → `pet`, `categories` → `category`, `address` stays.
fn singular(word: &str) -> String {
    if let Some(stem) = word.strip_suffix("ies") {
        format!("{stem}y")
    } else if word.ends_with("ss") || word.ends_with("us") {
        word.to_string()
    } else {
        word.strip_suffix('s').unwrap_or(word).to_string()
    }
}

/// Lower case, letters and digits only: `pet_id`, `petId` and `pet-id`
/// are one name.
fn norm(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// `ownerUserId` → owner, user, id.
fn words(s: &str) -> Vec<String> {
    screaming(s)
        .split('_')
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn jaccard(a: &[String], b: &[String]) -> f64 {
    let common = a.iter().filter(|w| b.contains(w)).count();
    let all = a.len() + b.len() - common;
    if all == 0 {
        0.0
    } else {
        common as f64 / all as f64
    }
}

/// `petId`, `pet-id`, `line-item_id` → `PET_ID`, `LINE_ITEM_ID`: a name
/// `${…}` takes.
/// The variable a placeholder named after the spec (a parameter, a security
/// scheme) reads: always under `API_`. A spec could otherwise name a parameter
/// `aws_secret_access_key`, and `--confirm` would read that variable from the
/// environment and send it to the spec's server.
fn env_name(s: &str) -> String {
    let name = screaming(s);
    let name = name.trim_start_matches('_');
    if name.starts_with("API_") {
        name.to_string()
    } else {
        format!("API_{name}")
    }
}

fn screaming(s: &str) -> String {
    let mut out = String::new();
    let mut prev: Option<char> = None;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            if c.is_ascii_uppercase()
                && prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
            {
                out.push('_');
            }
            out.push(c.to_ascii_uppercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
        prev = Some(c);
    }
    let out = out.trim_end_matches('_').to_string();
    if out.starts_with(|c: char| c.is_ascii_digit()) || out.is_empty() {
        format!("_{out}")
    } else {
        out
    }
}

fn pointer_escape(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_own_placeholders_stay_live() {
        let ours = ["API_TOKEN".to_string()];
        assert_eq!(
            defuse_text("Bearer ${API_TOKEN}", &ours),
            "Bearer ${API_TOKEN}"
        );
        assert_eq!(defuse_text("/a/${HOME}/b", &ours), "/a/$${HOME}/b");
        assert_eq!(defuse_text("${API_TOKEN:-x}", &ours), "$${API_TOKEN:-x}");
        assert_eq!(defuse_text("$${API_TOKEN}", &ours), "$$$${API_TOKEN}");
        assert_eq!(
            defuse_text("cost $5, $$ and ${", &ours),
            "cost $5, $$ and $${"
        );
        assert_eq!(defuse_text("e$${e}$", &ours), "e$$$${e}$");
    }

    #[test]
    fn names_become_variables() {
        assert_eq!(screaming("petId"), "PET_ID");
        assert_eq!(screaming("line-item_id"), "LINE_ITEM_ID");
        assert_eq!(screaming("X-Request-ID"), "X_REQUEST_ID");
        assert_eq!(screaming("2fa"), "_2FA");
        assert_eq!(singular("categories"), "category");
        assert_eq!(singular("address"), "address");
        assert!(is_version("v2") && is_version("api") && !is_version("vets"));
    }
}
