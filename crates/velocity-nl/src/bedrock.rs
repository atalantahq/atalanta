//! A planner and checker on Amazon Bedrock's Converse API
//! (`POST /model/{modelId}/converse`), which speaks one format for Claude,
//! Llama, Mistral and Nova on AWS.
//!
//! Converse has no JSON-schema output mode for every model, so the answer
//! comes back as a call of one tool whose input schema is the planner's
//! (`toolChoice: auto` with an instruction to call it: the 5.5 Claude
//! models refuse a forced `toolChoice`). A model without tool use falls
//! back to the schema in the prompt and a strict parse. Screenshots go as
//! `image` blocks and are dropped for a model that rejects them.
//!
//! Requests are signed with SigV4 ([`crate::sigv4`]) from the standard
//! AWS credentials, or carry a Bedrock API key
//! (`AWS_BEARER_TOKEN_BEDROCK`) as a bearer token.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};
use velocity_common::{NaturalKind, Result, VelocityError};

use crate::anthropic::{
    attempt_budget, fnv1a, retry_after, retry_delay, step_schema, SYSTEM_PROMPT,
};
use crate::checker::{
    check_prompt, check_schema, image_media_type, CheckAnswer, CheckRequest, ScreenChecker,
    CHECK_SYSTEM_PROMPT,
};
use crate::planner::{
    NaturalPlanner, PlanRequest, PlannedStep, Usage, UsageMeter, OUT_OF_TOKENS_REPLY, REFUSED_REPLY,
};
use crate::sigv4::{amz_date_now, sign, uri_encode, AwsCredentials, SignRequest};

const MAX_ATTEMPTS: u32 = 3;
const LIMITS: (Duration, Duration) = (Duration::from_secs(60), Duration::from_secs(150));

/// How Bedrock requests authenticate.
#[derive(Clone)]
pub enum BedrockAuth {
    /// SigV4 with AWS credentials.
    SigV4(AwsCredentials),
    /// A Bedrock API key (`AWS_BEARER_TOKEN_BEDROCK`).
    Bearer(String),
}

/// The Bedrock backend for one model.
pub struct BedrockPlanner {
    client: reqwest::Client,
    /// `https://bedrock-runtime.{region}.amazonaws.com` (or a test server).
    origin: String,
    region: String,
    model: String,
    auth: BedrockAuth,
    /// Answer through a tool call (off once the model rejected tools).
    tools: AtomicBool,
    images: AtomicBool,
    usage: UsageMeter,
}

impl BedrockPlanner {
    /// A backend for `model` in `region`. `origin` overrides the regional
    /// endpoint (a VPC endpoint, or a test server).
    pub fn new(
        region: &str,
        model: &str,
        auth: BedrockAuth,
        origin: Option<String>,
        images: bool,
    ) -> Result<Self> {
        if region.is_empty() {
            return Err(VelocityError::Config(
                "bedrock needs a region: set AWS_REGION (or the profile's region)".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(LIMITS.0)
            .build()
            .map_err(|e| VelocityError::Internal(anyhow::anyhow!("HTTP client: {e}")))?;
        Ok(Self {
            client,
            origin: origin
                .map(|o| o.trim_end_matches('/').to_string())
                .unwrap_or_else(|| format!("https://bedrock-runtime.{region}.amazonaws.com")),
            region: region.to_string(),
            model: model.to_string(),
            auth,
            tools: AtomicBool::new(true),
            images: AtomicBool::new(images),
            usage: UsageMeter::default(),
        })
    }

    /// The Converse path, the model id encoded (`:` in `…-v1:0`).
    fn path(&self) -> String {
        format!("/model/{}/converse", uri_encode(&self.model))
    }

    fn screen_block(screen: &str) -> String {
        let screen = if screen.is_empty() {
            "(none captured)"
        } else {
            screen
        };
        let tag = format!("screen-{:016x}", fnv1a(screen.as_bytes()));
        format!("<{tag}>\n{screen}\n</{tag}>")
    }

    fn body(
        &self,
        system: &str,
        content: Vec<Value>,
        name: &str,
        schema: Value,
        tools: bool,
    ) -> Value {
        let mut body = if tools {
            json!({
                "system": [{ "text": format!(
                    "{system}\n\nGive your answer by calling the {name} tool exactly once, with no other text."
                ) }],
                "toolConfig": {
                    "tools": [{ "toolSpec": {
                        "name": name,
                        "description": "Your answer.",
                        "inputSchema": { "json": schema }
                    } }],
                    "toolChoice": { "auto": {} }
                }
            })
        } else {
            json!({
                "system": [{ "text": format!(
                    "{system}\n\nReply with exactly one JSON object and nothing else: no prose, no code fence. \
                     It must match this JSON schema: {schema}"
                ) }]
            })
        };
        body["messages"] = json!([{ "role": "user", "content": content }]);
        body["inferenceConfig"] = json!({ "maxTokens": 4096 });
        body
    }

    fn plan_content(req: &PlanRequest<'_>) -> Vec<Value> {
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
        vec![
            json!({ "text": Self::screen_block(req.screen) }),
            json!({ "text": prompt }),
        ]
    }

    fn goal_content(req: &crate::goal::GoalRequest<'_>, images: bool) -> Vec<Value> {
        let mut content = vec![json!({ "text": Self::screen_block(req.screen) })];
        if let (true, Some(img)) = (images, req.screenshot) {
            let format = if image_media_type(img) == "image/jpeg" {
                "jpeg"
            } else {
                "png"
            };
            content.push(json!({ "image": {
                "format": format,
                "source": { "bytes": base64::engine::general_purpose::STANDARD.encode(img) }
            } }));
        }
        content.push(json!({ "text": crate::goal::goal_prompt(req) }));
        content
    }

    fn check_content(req: &CheckRequest<'_>, images: bool) -> Vec<Value> {
        let mut content = vec![json!({ "text": Self::screen_block(req.screen) })];
        if let (true, Some(img)) = (images, req.screenshot) {
            let format = if image_media_type(img) == "image/jpeg" {
                "jpeg"
            } else {
                "png"
            };
            content.push(json!({ "image": {
                "format": format,
                "source": { "bytes": base64::engine::general_purpose::STANDARD.encode(img) }
            } }));
        }
        content.push(json!({ "text": check_prompt(req) }));
        content
    }

    async fn ask(
        &self,
        system: &str,
        content: impl Fn(bool) -> Vec<Value>,
        name: &str,
        schema: Value,
        what: &str,
    ) -> Result<String> {
        for _ in 0..3 {
            let tools = self.tools.load(Ordering::Relaxed);
            let images = self.images.load(Ordering::Relaxed);
            let body = self.body(system, content(images), name, schema.clone(), tools);
            match self.send(&body).await {
                Ok(v) => return self.answer_text(&v, what),
                Err(Err4xx { status, text }) => {
                    let lower = text.to_ascii_lowercase();
                    if body.to_string().contains("\"image\"") && lower.contains("image") {
                        tracing::warn!(model = %self.model, "velocity-nl: the model rejected the screenshot; text only from now on");
                        self.images.store(false, Ordering::Relaxed);
                        continue;
                    }
                    if tools && (lower.contains("tool") || lower.contains("toolconfig")) {
                        tracing::warn!(model = %self.model, "velocity-nl: the model rejected tool use; answering in text from now on");
                        self.tools.store(false, Ordering::Relaxed);
                        continue;
                    }
                    let hint = match status {
                        401 | 403 => " (check the AWS credentials and the model's access in the Bedrock console)",
                        404 => " (check the model id and the region)",
                        _ => "",
                    };
                    return Err(VelocityError::Config(format!(
                        "Bedrock error {status}{hint}: {}",
                        text.chars().take(500).collect::<String>()
                    )));
                }
                Err(Final(e)) => return Err(e),
            }
        }
        Err(VelocityError::Config(format!(
            "Bedrock rejected every answer format for {}",
            self.model
        )))
    }

    fn answer_text(&self, v: &Value, what: &str) -> Result<String> {
        self.usage.record(
            v["usage"]["inputTokens"].as_u64().unwrap_or(0),
            v["usage"]["outputTokens"].as_u64().unwrap_or(0),
        );
        match v["stopReason"].as_str() {
            Some("max_tokens") => {
                return Err(VelocityError::Internal(anyhow::anyhow!(
                    "{OUT_OF_TOKENS_REPLY} for step {what:?}"
                )))
            }
            Some("guardrail_intervened") | Some("content_filtered") => {
                return Err(VelocityError::Config(format!("{REFUSED_REPLY}: {what:?}")))
            }
            _ => {}
        }
        let blocks = v["output"]["message"]["content"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(input) = blocks
            .iter()
            .find_map(|b| b.get("toolUse"))
            .map(|t| &t["input"])
        {
            return Ok(input.to_string());
        }
        let text: String = blocks.iter().filter_map(|b| b["text"].as_str()).collect();
        if text.trim().is_empty() {
            return Err(VelocityError::Config(format!(
                "planner returned empty content for step {what:?}"
            )));
        }
        Ok(text)
    }

    /// POST with retries (429 throttling, 408, 424 model not ready, 5xx),
    /// re-signed per attempt. Exhausted retries are `ServiceUnavailable`.
    async fn send(&self, body: &Value) -> std::result::Result<Value, SendError> {
        let bytes = serde_json::to_vec(body).unwrap_or_default();
        let path = self.path();
        let url = format!("{}{path}", self.origin);
        let host = self
            .origin
            .split("://")
            .nth(1)
            .unwrap_or(&self.origin)
            .to_string();
        let started = std::time::Instant::now();
        let mut last_err = String::new();
        for attempt in 1..=MAX_ATTEMPTS {
            let Some(budget) = attempt_budget(started.elapsed(), LIMITS) else {
                break;
            };
            let mut request = self
                .client
                .post(&url)
                .timeout(budget)
                .header("content-type", "application/json")
                .body(bytes.clone());
            match &self.auth {
                BedrockAuth::Bearer(key) => request = request.bearer_auth(key),
                BedrockAuth::SigV4(creds) => {
                    let date = amz_date_now();
                    let (headers, _) = sign(
                        &SignRequest {
                            method: "POST",
                            host: &host,
                            path: &path,
                            query: "",
                            headers: &[("content-type", "application/json")],
                            body: &bytes,
                            region: &self.region,
                            service: "bedrock",
                            amz_date: &date,
                        },
                        creds,
                    );
                    for (k, v) in headers {
                        request = request.header(k, v);
                    }
                }
            }
            let wait = match request.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let retry = retry_after(
                        None,
                        resp.headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok()),
                    );
                    let text = resp.text().await.unwrap_or_default();
                    if (200..300).contains(&status) {
                        return serde_json::from_str(&text).map_err(|e| {
                            Final(VelocityError::Config(format!(
                                "Bedrock answered 200 with something that isn't a Converse response ({e})"
                            )))
                        });
                    }
                    if !(matches!(status, 408 | 424 | 429) || status >= 500) {
                        return Err(Err4xx { status, text });
                    }
                    last_err = format!(
                        "Bedrock error {status}: {}",
                        text.chars().take(300).collect::<String>()
                    );
                    retry
                }
                Err(e) => {
                    last_err = format!("Bedrock request failed: {e}");
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
        Err(Final(VelocityError::ServiceUnavailable {
            service: "Amazon Bedrock".into(),
            detail: format!("{last_err} (gave up after {:.0?})", started.elapsed()),
        }))
    }
}

enum SendError {
    Err4xx { status: u16, text: String },
    Final(VelocityError),
}
use SendError::{Err4xx, Final};

#[async_trait]
impl NaturalPlanner for BedrockPlanner {
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
impl ScreenChecker for BedrockPlanner {
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
mod tests {
    use super::*;
    use crate::planner::Op;
    use crate::testutil::serve;
    use velocity_common::Platform;

    fn req() -> PlanRequest<'static> {
        PlanRequest {
            intent: "tap the login button",
            kind: NaturalKind::Act,
            data: None,
            wait_until: None,
            platform: Platform::Ios,
            screen: "[0] <button> text=\"Login\" @10,20",
            feedback: None,
        }
    }

    fn tool_reply() -> String {
        json!({
            "output": { "message": { "role": "assistant", "content": [
                { "toolUse": { "toolUseId": "t1", "name": "ui_step", "input":
                    { "op": "tap", "element": 0, "text": "", "direction": "none", "key": "none", "ms": 0 } } }
            ] } },
            "stopReason": "tool_use",
            "usage": { "inputTokens": 300, "outputTokens": 40 }
        })
        .to_string()
    }

    fn creds() -> AwsCredentials {
        AwsCredentials {
            access_key_id: "AKIDTEST".into(),
            secret_access_key: "secret".into(),
            session_token: Some("session".into()),
        }
    }

    #[tokio::test]
    async fn converse_requests_are_signed_and_answer_through_a_tool() {
        let (origin, seen) = serve(vec![(200, "", tool_reply())]).await;
        let p = BedrockPlanner::new(
            "us-east-1",
            "us.anthropic.claude-haiku-5-5-v1:0",
            BedrockAuth::SigV4(creds()),
            Some(origin),
            true,
        )
        .unwrap();
        let step = p.plan(&req()).await.unwrap();
        assert_eq!((step.op, step.element), (Op::Tap, Some(0)));
        let r = seen.lock().unwrap()[0].clone();
        assert_eq!(r.method, "POST");
        assert_eq!(
            r.target,
            "/model/us.anthropic.claude-haiku-5-5-v1%3A0/converse"
        );
        let auth = r.header("authorization").unwrap();
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDTEST/"),
            "{auth}"
        );
        assert!(auth.contains("/us-east-1/bedrock/aws4_request"), "{auth}");
        assert!(auth.contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token"));
        assert_eq!(r.header("x-amz-security-token"), Some("session"));
        assert!(r.header("x-amz-date").unwrap().ends_with('Z'));
        assert_eq!(r.body["toolConfig"]["toolChoice"], json!({ "auto": {} }));
        assert_eq!(
            r.body["toolConfig"]["tools"][0]["toolSpec"]["name"],
            "ui_step"
        );
        assert!(r.body["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("<screen-"));
        let u = NaturalPlanner::usage(&p);
        assert_eq!((u.input_tokens, u.output_tokens), (300, 40));
    }

    #[tokio::test]
    async fn a_model_without_tools_answers_in_text_and_throttling_is_retried() {
        let text_reply = json!({
            "output": { "message": { "content": [ { "text":
                "{\"op\":\"tap\",\"element\":0,\"text\":\"\",\"direction\":\"none\",\"key\":\"none\",\"ms\":0}" } ] } },
            "stopReason": "end_turn"
        })
        .to_string();
        let (origin, seen) = serve(vec![
            (
                400,
                "",
                r#"{"message":"This model doesn't support tool use."}"#.into(),
            ),
            (
                429,
                "retry-after: 0\r\n",
                r#"{"message":"ThrottlingException"}"#.into(),
            ),
            (200, "", text_reply),
        ])
        .await;
        let p = BedrockPlanner::new(
            "eu-west-1",
            "meta.llama3-70b",
            BedrockAuth::Bearer("bk".into()),
            Some(origin),
            true,
        )
        .unwrap();
        assert_eq!(p.plan(&req()).await.unwrap().op, Op::Tap);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen[1].body.get("toolConfig").is_none());
        assert!(seen[1].body["system"][0]["text"]
            .as_str()
            .unwrap()
            .contains("JSON schema"));
        assert_eq!(seen[0].header("authorization"), Some("Bearer bk"));
    }

    #[tokio::test]
    async fn bedrock_errors_are_classified() {
        let throttled = || (429, "retry-after: 0\r\n", "{}".to_string());
        let (origin, _) = serve(vec![throttled(), throttled(), throttled()]).await;
        let p = BedrockPlanner::new(
            "us-east-1",
            "m",
            BedrockAuth::Bearer("k".into()),
            Some(origin),
            true,
        )
        .unwrap();
        let e = p.plan(&req()).await.unwrap_err();
        assert!(e.is_transient(), "{e:?}");
        let (origin, _) = serve(vec![(
            403,
            "",
            r#"{"message":"AccessDeniedException"}"#.into(),
        )])
        .await;
        let p = BedrockPlanner::new(
            "us-east-1",
            "m",
            BedrockAuth::Bearer("k".into()),
            Some(origin),
            true,
        )
        .unwrap();
        assert_eq!(p.plan(&req()).await.unwrap_err().exit_code(), 2);
        assert!(BedrockPlanner::new("", "m", BedrockAuth::Bearer("k".into()), None, true).is_err());
    }

    #[tokio::test]
    async fn checks_send_images_as_bytes() {
        let reply = json!({
            "output": { "message": { "content": [ { "toolUse": { "name": "screen_check",
                "input": { "holds": true, "evidence_element_ids": [0], "reason": "x" } } } ] } },
            "stopReason": "tool_use"
        })
        .to_string();
        let (origin, seen) = serve(vec![(200, "", reply)]).await;
        let p = BedrockPlanner::new(
            "us-east-1",
            "m",
            BedrockAuth::Bearer("k".into()),
            Some(origin),
            true,
        )
        .unwrap();
        let a = p
            .check(&CheckRequest {
                claim: "x",
                platform: Platform::Android,
                screen: "[0] <text> text=\"x\" @1,1",
                screenshot: Some(b"\x89PNG"),
            })
            .await
            .unwrap();
        assert!(a.holds);
        let body = seen.lock().unwrap()[0].body.clone();
        assert_eq!(body["messages"][0]["content"][1]["image"]["format"], "png");
    }
}
