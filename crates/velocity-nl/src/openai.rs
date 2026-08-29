//! A planner and checker for any OpenAI-compatible Chat Completions API
//! (`POST {base}/chat/completions`): OpenAI, Google Gemini's OpenAI
//! endpoint, OpenRouter, Ollama, vLLM, LM Studio, Groq and the like, chosen
//! by base URL.
//!
//! The answer is constrained the same way as with Anthropic. JSON-schema
//! structured output (`response_format: json_schema`, strict) is tried
//! first; an endpoint that rejects it gets a forced function call with the
//! same schema instead, and keeps getting it for the rest of the run. The
//! reply is parsed by [`PlannedStep::from_reply`], so an endpoint that
//! ignores both and answers in prose still gets the tolerant parse and the
//! resolver's one re-ask.
//!
//! Screenshots (checks only; planning is text) go as `image_url` data URIs.
//! A model that rejects images degrades to text-only once, with a warning,
//! or never gets them with `images: false` / `VELOCITY_NL_IMAGES=0`.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use velocity_common::{NaturalKind, Result, VelocityError};

use crate::anthropic::{
    attempt_budget, fnv1a, is_retryable, retry_after, retry_delay, step_schema, SYSTEM_PROMPT,
};
use crate::checker::{
    check_prompt, check_schema, image_media_type, CheckAnswer, CheckRequest, ScreenChecker,
    CHECK_SYSTEM_PROMPT,
};
use crate::planner::{
    NaturalPlanner, PlanRequest, PlannedStep, Usage, UsageMeter, OUT_OF_TOKENS_REPLY, REFUSED_REPLY,
};

/// Attempts for retryable failures (408, 409, 429, 5xx, network).
const MAX_ATTEMPTS: u32 = 3;
/// One request's limit and the planning call's deadline. Wider than the
/// Anthropic low-effort ones: a local model on a laptop CPU is slow.
const LIMITS: (Duration, Duration) = (Duration::from_secs(60), Duration::from_secs(150));

/// How the answer is constrained, in order of preference. A model that
/// rejects one falls back to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// `response_format: { type: json_schema, strict: true }` (preferred).
    JsonSchema,
    /// A forced call of one function whose parameters are the schema.
    ToolCall,
    /// Neither: the schema goes in the prompt and the reply is parsed
    /// strictly (one JSON object, checked field by field; a bad one is
    /// re-asked once by the resolver).
    Text,
}

impl OutputMode {
    fn code(self) -> u8 {
        self as u8
    }

    fn from_code(c: u8) -> Self {
        match c {
            0 => Self::JsonSchema,
            1 => Self::ToolCall,
            _ => Self::Text,
        }
    }
}

/// How a request authenticates.
#[derive(Clone)]
pub enum Auth {
    /// Nothing (a local server).
    None,
    /// `Authorization: Bearer <key>`.
    Bearer(String),
    /// A key in a named header (Azure's `api-key`).
    Header(String, String),
    /// A short-lived bearer token fetched per request (Vertex).
    Token(Arc<dyn crate::gcp::TokenSource>),
}

/// Everything an OpenAI-compatible backend needs; the provider presets
/// ([`crate::provider`]) fill it in.
pub struct OpenAiOptions {
    /// The full chat-completions URL, query included (Azure's
    /// `?api-version=`).
    pub endpoint: String,
    pub auth: Auth,
    /// Extra headers (`OpenAI-Organization`).
    pub headers: Vec<(String, String)>,
    /// The `model` field.
    pub model: String,
    /// Where to start; see [`OutputMode`].
    pub mode: OutputMode,
    /// Whether the endpoint takes tool calls, the fallback after
    /// `json_schema`. Without them it falls back to [`OutputMode::Text`].
    pub tools: bool,
    /// Send screenshots to checks.
    pub images: bool,
    /// The service's name in errors (`OpenRouter`, `Azure OpenAI`).
    pub service: String,
}

/// The OpenAI-compatible backend. One value per model; the output mode and
/// image support it learns from the endpoint stick for its lifetime.
pub struct OpenAiPlanner {
    client: reqwest::Client,
    endpoint: String,
    auth: Auth,
    headers: Vec<(String, String)>,
    model: String,
    service: String,
    /// The current [`OutputMode`] (moves down on a rejection).
    mode: AtomicU8,
    tools_ok: bool,
    /// Send screenshots to checks (off once the model rejected one).
    images: AtomicBool,
    usage: UsageMeter,
}

impl OpenAiPlanner {
    /// A backend from full options (see [`OpenAiOptions`]).
    pub fn with_options(o: OpenAiOptions) -> Result<Self> {
        if !(o.endpoint.starts_with("http://") || o.endpoint.starts_with("https://")) {
            return Err(VelocityError::Config(format!(
                "the {} URL {:?} must start with http:// or https://",
                o.service, o.endpoint
            )));
        }
        let client = reqwest::Client::builder()
            .timeout(LIMITS.0)
            .build()
            .map_err(|e| VelocityError::Internal(anyhow::anyhow!("HTTP client: {e}")))?;
        let mode = if o.mode == OutputMode::ToolCall && !o.tools {
            OutputMode::Text
        } else {
            o.mode
        };
        Ok(Self {
            client,
            endpoint: o.endpoint,
            auth: o.auth,
            headers: o.headers,
            model: o.model,
            service: o.service,
            mode: AtomicU8::new(mode.code()),
            tools_ok: o.tools,
            images: AtomicBool::new(o.images),
            usage: UsageMeter::default(),
        })
    }

    /// A backend at `base_url` (e.g. `https://api.openai.com/v1`; a URL
    /// ending in `/chat/completions` is taken as the endpoint itself).
    /// `api_key` may be absent for local servers. `images: false` never
    /// sends screenshots.
    pub fn new(
        base_url: &str,
        api_key: Option<String>,
        model: &str,
        mode: OutputMode,
        images: bool,
    ) -> Result<Self> {
        Self::with_options(OpenAiOptions {
            endpoint: chat_endpoint(base_url),
            auth: match api_key.filter(|k| !k.is_empty()) {
                Some(k) => Auth::Bearer(k),
                None => Auth::None,
            },
            headers: vec![],
            model: model.to_string(),
            mode,
            tools: true,
            images,
            service: "OpenAI-compatible API".into(),
        })
    }

    /// The model this backend asks.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The output mode in use now.
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_code(self.mode.load(Ordering::Relaxed))
    }

    /// The chat-completions URL requests go to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The mode after `mode` was rejected.
    fn fall_back_from(&self, mode: OutputMode) -> OutputMode {
        match mode {
            OutputMode::JsonSchema if self.tools_ok => OutputMode::ToolCall,
            _ => OutputMode::Text,
        }
    }

    /// The screen list as its own delimited block (see the Anthropic
    /// planner): the tag carries a hash of the list.
    fn screen_block(screen: &str) -> String {
        let screen = if screen.is_empty() {
            "(none captured)"
        } else {
            screen
        };
        let tag = format!("screen-{:016x}", fnv1a(screen.as_bytes()));
        format!("<{tag}>\n{screen}\n</{tag}>")
    }

    fn plan_prompt(req: &PlanRequest<'_>) -> String {
        let kind = match req.kind {
            NaturalKind::Act => "action",
            NaturalKind::Assert => "assertion",
        };
        let mut prompt = format!(
            "Platform: {}\nStep kind: {kind}\nStep: {}\n",
            req.platform, req.intent
        );
        if let Some(data) = req.data {
            prompt.push_str(&format!("Value to use: {data}\n"));
        }
        if let Some(w) = req.wait_until {
            prompt.push_str(&format!("(After this step the app should reach: {w})\n"));
        }
        prompt.push_str("\nThe elements on screen are in the screen block above.\n");
        if let Some(fb) = req.feedback {
            prompt.push_str(&format!(
                "\nYour previous answer for this step was rejected: {fb}\nAnswer again.\n"
            ));
        }
        prompt
    }

    /// A request in `mode`: `system`, one user turn of `content` parts,
    /// the answer constrained to `schema` (named `name`).
    fn body(
        &self,
        mode: OutputMode,
        system: &str,
        content: Vec<Value>,
        name: &str,
        schema: Value,
    ) -> Value {
        let system = match mode {
            OutputMode::Text => format!(
                "{system}\n\nReply with exactly one JSON object and nothing else: no prose, no code fence. \
                 It must match this JSON schema: {schema}"
            ),
            _ => system.to_string(),
        };
        let mut body = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": content }
            ],
        });
        match mode {
            OutputMode::Text => {}
            OutputMode::JsonSchema => {
                body["response_format"] = json!({
                    "type": "json_schema",
                    "json_schema": { "name": name, "strict": true, "schema": schema }
                });
            }
            OutputMode::ToolCall => {
                body["tools"] = json!([{
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": "Give your answer by calling this function exactly once.",
                        "parameters": schema,
                        "strict": true
                    }
                }]);
                body["tool_choice"] = json!({ "type": "function", "function": { "name": name } });
            }
        }
        body
    }

    fn plan_content(req: &PlanRequest<'_>) -> Vec<Value> {
        vec![
            json!({ "type": "text", "text": Self::screen_block(req.screen) }),
            json!({ "type": "text", "text": Self::plan_prompt(req) }),
        ]
    }

    fn goal_content(req: &crate::goal::GoalRequest<'_>, images: bool) -> Vec<Value> {
        let mut content = vec![json!({ "type": "text", "text": Self::screen_block(req.screen) })];
        if let (true, Some(img)) = (images, req.screenshot) {
            use base64::Engine as _;
            let data = base64::engine::general_purpose::STANDARD.encode(img);
            content.push(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{data}", image_media_type(img)) }
            }));
        }
        content.push(json!({ "type": "text", "text": crate::goal::goal_prompt(req) }));
        content
    }

    fn check_content(req: &CheckRequest<'_>, images: bool) -> Vec<Value> {
        let mut content = vec![json!({ "type": "text", "text": Self::screen_block(req.screen) })];
        if let (true, Some(img)) = (images, req.screenshot) {
            use base64::Engine as _;
            let data = base64::engine::general_purpose::STANDARD.encode(img);
            content.push(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{data}", image_media_type(img)) }
            }));
        }
        content.push(json!({ "type": "text", "text": check_prompt(req) }));
        content
    }

    /// Ask once, falling back from `json_schema` to a tool call, and from
    /// images to text, when the endpoint says it can't do them. Returns the
    /// answer's JSON text.
    async fn ask(
        &self,
        system: &str,
        content: impl Fn(bool) -> Vec<Value>,
        name: &str,
        schema: Value,
        what: &str,
    ) -> Result<String> {
        // At most one fallback of each kind.
        for _ in 0..4 {
            let mode = self.output_mode();
            let images = self.images.load(Ordering::Relaxed);
            let body = self.body(mode, system, content(images), name, schema.clone());
            match self.send(&body).await {
                Ok(parsed) => return self.answer_text(&parsed, what),
                Err(Rejected { status, text }) => {
                    let lower = text.to_ascii_lowercase();
                    let has_image = body.to_string().contains("\"image_url\"");
                    if has_image && lower.contains("image") {
                        tracing::warn!(
                            model = %self.model,
                            "velocity-nl: the model rejected the screenshot; checking with text only from now on"
                        );
                        self.images.store(false, Ordering::Relaxed);
                        continue;
                    }
                    let rejected_mode = match mode {
                        OutputMode::JsonSchema => {
                            ["response_format", "json_schema", "structured", "schema"]
                                .iter()
                                .any(|k| lower.contains(k))
                        }
                        OutputMode::ToolCall => {
                            ["tool", "function"].iter().any(|k| lower.contains(k))
                        }
                        OutputMode::Text => false,
                    };
                    if rejected_mode && status != 401 && status != 403 && status != 404 {
                        let next = self.fall_back_from(mode);
                        tracing::warn!(
                            model = %self.model,
                            "velocity-nl: the endpoint rejected {mode:?} output; using {next:?} from now on"
                        );
                        self.mode.store(next.code(), Ordering::Relaxed);
                        continue;
                    }
                    return Err(client_error(&self.service, status, &text));
                }
                Err(Failed(e)) => return Err(e),
            }
        }
        Err(VelocityError::Config(format!(
            "{} at {} rejected every output mode for {}",
            self.service, self.endpoint, self.model
        )))
    }

    /// The answer's JSON text from a completion, after counting tokens and
    /// turning a refusal or a cut-off into the planner's errors.
    fn answer_text(&self, parsed: &Value, what: &str) -> Result<String> {
        let usage = &parsed["usage"];
        self.usage.record(
            usage["prompt_tokens"].as_u64().unwrap_or(0),
            usage["completion_tokens"].as_u64().unwrap_or(0),
        );
        let choice = &parsed["choices"][0];
        let message = &choice["message"];
        if let Some(refusal) = message["refusal"].as_str().filter(|r| !r.is_empty()) {
            return Err(VelocityError::Config(format!(
                "{REFUSED_REPLY}: {what:?} ({refusal})"
            )));
        }
        if choice["finish_reason"] == "length" {
            return Err(VelocityError::Internal(anyhow::anyhow!(
                "{OUT_OF_TOKENS_REPLY} for step {what:?}"
            )));
        }
        // A tool call's arguments, else the content (a string, or parts).
        let from_tool = message["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .map(String::from);
        let from_content = match &message["content"] {
            Value::String(s) => Some(s.clone()),
            Value::Array(parts) => Some(
                parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        };
        match from_tool.or(from_content).filter(|t| !t.trim().is_empty()) {
            Some(t) => Ok(t),
            None => Err(VelocityError::Config(format!(
                "planner returned empty content for step {what:?}"
            ))),
        }
    }

    /// POST with retries: 408, 409, 429, 5xx and network failures are
    /// retried with the server's `retry-after` or a jittered backoff, within
    /// one deadline. A service that stays unavailable is a transient
    /// `ServiceUnavailable` (VEL-S37), never an internal error; another 4xx
    /// comes back as [`SendError::Rejected`] for the caller to classify or
    /// fall back on.
    async fn send(&self, body: &Value) -> std::result::Result<Value, SendError> {
        let mut last_err = String::new();
        let started = std::time::Instant::now();
        let service = || self.service.clone();
        for attempt in 1..=MAX_ATTEMPTS {
            let Some(budget) = attempt_budget(started.elapsed(), LIMITS) else {
                break;
            };
            let mut request = self
                .client
                .post(&self.endpoint)
                .timeout(budget)
                .header("content-type", "application/json")
                .json(body);
            for (k, v) in &self.headers {
                request = request.header(k, v);
            }
            request = match &self.auth {
                Auth::None => request,
                Auth::Bearer(key) => request.bearer_auth(key),
                Auth::Header(name, key) => request.header(name, key),
                Auth::Token(source) => request.bearer_auth(source.token().await.map_err(Failed)?),
            };
            let wait = match request.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let headers = resp.headers();
                    let header = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
                    let retry_after = retry_after(header("retry-after-ms"), header("retry-after"));
                    let text = resp.text().await.unwrap_or_default();
                    if (200..300).contains(&status) {
                        // A 200 that isn't JSON is not this API at all (a
                        // login page, a wrong base URL): a setup problem.
                        return serde_json::from_str(&text).map_err(|e| {
                            Failed(VelocityError::Config(format!(
                                "{} answered 200 with something that isn't a chat completion ({e}); \
                                 check the base URL",
                                self.endpoint
                            )))
                        });
                    }
                    if !is_retryable(status) {
                        return Err(Rejected { status, text });
                    }
                    last_err = format!("API error {status}: {}", clip(&text));
                    retry_after
                }
                Err(e) => {
                    last_err = format!("request failed: {e}");
                    None
                }
            };
            if attempt < MAX_ATTEMPTS {
                let delay = retry_delay(wait, attempt - 1);
                if attempt_budget(started.elapsed() + delay, LIMITS).is_none() {
                    break;
                }
                tracing::warn!(attempt, ?delay, "velocity-nl: {last_err}; retrying");
                tokio::time::sleep(delay).await;
            }
        }
        Err(Failed(VelocityError::ServiceUnavailable {
            service: service(),
            detail: format!("{last_err} (gave up after {:.0?})", started.elapsed()),
        }))
    }
}

/// How a request failed: a 4xx the caller may fall back on, or a final
/// error.
enum SendError {
    Rejected { status: u16, text: String },
    Failed(VelocityError),
}
use SendError::{Failed, Rejected};

/// A 4xx that no fallback fixes: a bad key or model is configuration
/// (exit 2), not a test failure.
fn client_error(service: &str, status: u16, text: &str) -> VelocityError {
    let hint = match status {
        401 | 403 => " (check the API key and its environment variable)",
        404 => " (check the base URL and the model name)",
        _ => "",
    };
    VelocityError::Config(format!("{service} error {status}{hint}: {}", clip(text)))
}

/// The chat-completions URL under an API base (`…/v1`); a URL that
/// already ends in `/chat/completions` is kept.
pub fn chat_endpoint(base_url: &str) -> String {
    let base = base_url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches("/chat/completions");
    format!("{base}/chat/completions")
}

/// An error body, cut to a readable length.
fn clip(text: &str) -> String {
    const MAX: usize = 500;
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(MAX).collect::<String>())
    }
}

#[async_trait]
impl NaturalPlanner for OpenAiPlanner {
    async fn plan(&self, req: &PlanRequest<'_>) -> Result<PlannedStep> {
        let reply = self
            .ask(
                SYSTEM_PROMPT,
                |_| Self::plan_content(req),
                "ui_step",
                step_schema(),
                req.intent,
            )
            .await?;
        PlannedStep::from_reply(&reply)
    }

    async fn plan_goal(&self, req: &crate::goal::GoalRequest<'_>) -> Result<crate::goal::GoalStep> {
        let reply = self
            .ask(
                crate::goal::GOAL_SYSTEM_PROMPT,
                |images| Self::goal_content(req, images),
                "ui_goal_step",
                crate::goal::goal_schema(),
                req.goal,
            )
            .await?;
        crate::goal::GoalStep::from_reply(&reply)
    }

    fn usage(&self) -> Usage {
        self.usage.get()
    }
}

#[async_trait]
impl ScreenChecker for OpenAiPlanner {
    async fn check(&self, req: &CheckRequest<'_>) -> Result<CheckAnswer> {
        let reply = self
            .ask(
                CHECK_SYSTEM_PROMPT,
                |images| Self::check_content(req, images),
                "screen_check",
                check_schema(),
                req.claim,
            )
            .await?;
        CheckAnswer::from_reply(&reply)
    }

    fn usage(&self) -> Usage {
        self.usage.get()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::planner::Op;
    use std::sync::{Arc, Mutex};
    use velocity_common::Platform;

    /// A scripted reply: status, extra header lines, body.
    pub(crate) type Reply = (u16, &'static str, String);

    /// A local OpenAI-compatible server answering each request with the
    /// next scripted reply; returns the base URL and every request body.
    pub(crate) async fn mock(script: Vec<Reply>) -> (String, Arc<Mutex<Vec<Value>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for (status, headers, body) in script {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 8192];
                let mut body_at = None;
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&req).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if req.len() >= end + 4 + len {
                            body_at = Some((
                                end + 4,
                                text.contains("authorization: Bearer")
                                    || text.contains("Authorization: Bearer"),
                            ));
                            break;
                        }
                    }
                }
                if let Some((at, auth)) = body_at {
                    let mut v: Value = serde_json::from_slice(&req[at..]).unwrap_or(Value::Null);
                    v["_auth"] = json!(auth);
                    log.lock().unwrap().push(v);
                }
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (base, seen)
    }

    /// A completion whose message content is `content`.
    pub(crate) fn content_reply(content: &str) -> String {
        json!({
            "choices": [{ "finish_reason": "stop", "message": { "role": "assistant", "content": content } }],
            "usage": { "prompt_tokens": 120, "completion_tokens": 9 }
        })
        .to_string()
    }

    fn tool_reply(args: &str) -> String {
        json!({
            "choices": [{ "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{ "id": "c1", "type": "function",
                    "function": { "name": "ui_step", "arguments": args } }]
            } }],
            "usage": { "prompt_tokens": 100, "completion_tokens": 20 }
        })
        .to_string()
    }

    const TAP0: &str =
        r#"{"op":"tap","element":0,"text":"","direction":"none","key":"none","ms":0}"#;

    fn req() -> PlanRequest<'static> {
        PlanRequest {
            intent: "tap the login button",
            kind: NaturalKind::Act,
            data: None,
            wait_until: None,
            platform: Platform::Web,
            screen: "[0] <button> text=\"Login\" @10,20",
            feedback: None,
        }
    }

    fn backend(base: &str, key: Option<&str>) -> OpenAiPlanner {
        OpenAiPlanner::new(
            base,
            key.map(String::from),
            "gpt-test",
            OutputMode::JsonSchema,
            true,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn json_schema_output_is_preferred_and_parsed() {
        let (base, seen) = mock(vec![(200, "", content_reply(TAP0))]).await;
        let p = backend(&base, Some("sk-test"));
        let step = p.plan(&req()).await.unwrap();
        assert_eq!((step.op, step.element), (Op::Tap, Some(0)));
        let body = seen.lock().unwrap()[0].clone();
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert!(body.get("tools").is_none());
        assert_eq!(body["messages"][0]["role"], "system");
        let user = body["messages"][1]["content"].as_array().unwrap();
        assert!(user[0]["text"].as_str().unwrap().starts_with("<screen-"));
        assert!(user[1]["text"]
            .as_str()
            .unwrap()
            .contains("tap the login button"));
        assert_eq!(body["_auth"], true);
        let u = NaturalPlanner::usage(&p);
        assert_eq!((u.calls, u.input_tokens, u.output_tokens), (1, 120, 9));
    }

    #[tokio::test]
    async fn an_endpoint_without_json_schema_falls_back_to_a_tool_call() {
        let (base, seen) = mock(vec![
            (
                400,
                "",
                r#"{"error":{"message":"response_format json_schema is not supported"}}"#.into(),
            ),
            (200, "", tool_reply(TAP0)),
            (200, "", tool_reply(TAP0)),
        ])
        .await;
        let p = backend(&base, None);
        assert_eq!(p.plan(&req()).await.unwrap().op, Op::Tap);
        assert_eq!(p.output_mode(), OutputMode::ToolCall);
        // The fallback sticks: the next call goes straight to tools.
        assert_eq!(p.plan(&req()).await.unwrap().op, Op::Tap);
        let bodies = seen.lock().unwrap().clone();
        assert_eq!(bodies.len(), 3);
        assert!(bodies[1].get("response_format").is_none());
        assert_eq!(bodies[1]["tools"][0]["function"]["name"], "ui_step");
        assert_eq!(bodies[1]["tool_choice"]["function"]["name"], "ui_step");
        assert_eq!(
            bodies[1]["tools"][0]["function"]["parameters"]["additionalProperties"],
            false
        );
        // No key, no Authorization header (local servers).
        assert_eq!(bodies[0]["_auth"], false);
    }

    #[tokio::test]
    async fn tool_call_mode_can_be_chosen_up_front() {
        let (base, seen) = mock(vec![(200, "", tool_reply(TAP0))]).await;
        let p = OpenAiPlanner::new(&base, None, "m", OutputMode::ToolCall, true).unwrap();
        p.plan(&req()).await.unwrap();
        assert!(seen.lock().unwrap()[0].get("tools").is_some());
    }

    // VEL-S37 / root cause 7: an exhausted 429 is a transient service error,
    // never an internal one (exit 5).
    #[tokio::test]
    async fn exhausted_rate_limits_are_transient_not_internal() {
        let r = || (429, "retry-after-ms: 1\r\n", "{}".to_string());
        let (base, seen) = mock(vec![r(), r(), r()]).await;
        let e = backend(&base, Some("k")).plan(&req()).await.unwrap_err();
        assert!(
            matches!(e, VelocityError::ServiceUnavailable { .. }),
            "{e:?}"
        );
        assert!(e.is_transient(), "{e:?}");
        assert_ne!(e.exit_code(), 5);
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn rate_limits_and_server_errors_are_retried() {
        let (base, seen) = mock(vec![
            (429, "retry-after: 0.001\r\n", "{}".into()),
            (503, "retry-after-ms: 1\r\n", "{}".into()),
            (200, "", content_reply(TAP0)),
        ])
        .await;
        assert!(backend(&base, Some("k")).plan(&req()).await.is_ok());
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn auth_and_not_found_are_configuration_errors() {
        let (base, seen) = mock(vec![(401, "", r#"{"error":"bad key"}"#.into())]).await;
        let e = backend(&base, Some("k")).plan(&req()).await.unwrap_err();
        assert_eq!(e.exit_code(), 2, "{e:?}");
        assert!(e.to_string().contains("API key"), "{e}");
        assert_eq!(seen.lock().unwrap().len(), 1, "a 401 is not retried");
        let (base, _) = mock(vec![(404, "", r#"{"error":"no such model"}"#.into())]).await;
        let e = backend(&base, None).plan(&req()).await.unwrap_err();
        assert!(e.to_string().contains("model name"), "{e}");
    }

    #[tokio::test]
    async fn refusals_cutoffs_and_prose_are_planner_failures() {
        let refusal = json!({ "choices": [{ "finish_reason": "stop",
            "message": { "content": null, "refusal": "I can't help with that" } }] })
        .to_string();
        let cut = json!({ "choices": [{ "finish_reason": "length",
            "message": { "content": "{\"op\":" } }] })
        .to_string();
        let (base, _) = mock(vec![
            (200, "", refusal),
            (200, "", cut),
            (200, "", content_reply("I would tap Login")),
        ])
        .await;
        let p = backend(&base, None);
        for e in [
            p.plan(&req()).await.unwrap_err(),
            p.plan(&req()).await.unwrap_err(),
            p.plan(&req()).await.unwrap_err(),
        ] {
            assert!(crate::planner::is_model_failure(&e), "{e:?}");
        }
    }

    #[tokio::test]
    async fn a_200_that_is_not_json_is_a_setup_error() {
        let (base, _) = mock(vec![(200, "", "<html>login</html>".into())]).await;
        let e = backend(&base, None).plan(&req()).await.unwrap_err();
        assert_eq!(e.exit_code(), 2, "{e:?}");
    }

    fn check_req(shot: Option<&'static [u8]>) -> CheckRequest<'static> {
        CheckRequest {
            claim: "the cart shows 2 items",
            platform: Platform::Android,
            screen: "[0] <text> text=\"2 items\" @1,1",
            screenshot: shot,
        }
    }

    const HOLDS: &str = r#"{"holds":true,"evidence_element_ids":[0],"reason":"2 items shown"}"#;

    #[tokio::test]
    async fn checks_send_screenshots_as_data_uris() {
        let (base, seen) = mock(vec![(200, "", content_reply(HOLDS))]).await;
        let p = backend(&base, None);
        let a = p.check(&check_req(Some(b"\x89PNG..."))).await.unwrap();
        assert!(a.holds);
        let body = seen.lock().unwrap()[0].clone();
        assert_eq!(
            body["response_format"]["json_schema"]["name"],
            "screen_check"
        );
        let parts = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(parts[1]["type"], "image_url");
        assert!(parts[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
    }

    #[tokio::test]
    async fn a_model_that_rejects_images_degrades_to_text() {
        let (base, seen) = mock(vec![
            (
                400,
                "",
                r#"{"error":{"message":"this model does not support image input"}}"#.into(),
            ),
            (200, "", content_reply(HOLDS)),
            (200, "", content_reply(HOLDS)),
        ])
        .await;
        let p = backend(&base, None);
        assert!(p.check(&check_req(Some(b"\x89PNG"))).await.unwrap().holds);
        assert!(p.check(&check_req(Some(b"\x89PNG"))).await.unwrap().holds);
        let bodies = seen.lock().unwrap().clone();
        let has_image = |b: &Value| b.to_string().contains("image_url");
        assert!(has_image(&bodies[0]));
        assert!(!has_image(&bodies[1]) && !has_image(&bodies[2]));
        // Configured off: never sent.
        let (base, seen) = mock(vec![(200, "", content_reply(HOLDS))]).await;
        let p = OpenAiPlanner::new(&base, None, "m", OutputMode::JsonSchema, false).unwrap();
        p.check(&check_req(Some(b"\x89PNG"))).await.unwrap();
        assert!(!seen.lock().unwrap()[0].to_string().contains("image_url"));
    }

    #[test]
    fn base_urls_are_normalised_and_checked() {
        let p = backend("http://localhost:11434/v1/", None);
        assert_eq!(p.endpoint(), "http://localhost:11434/v1/chat/completions");
        let p = backend("https://x.example/v1/chat/completions", None);
        assert_eq!(p.endpoint(), "https://x.example/v1/chat/completions");
        assert!(
            OpenAiPlanner::new("localhost:1234", None, "m", OutputMode::JsonSchema, true).is_err()
        );
    }
}
