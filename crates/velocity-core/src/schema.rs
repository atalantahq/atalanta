//! JSON Schema (draft 2020-12) for Velocity suite YAML, for editor
//! completion and validation (`velocity schema > velocity.schema.json`, then
//! `# yaml-language-server: $schema=./velocity.schema.json` at the top of a
//! suite). Written by hand from the parser's DSL structs; the tests check
//! it accepts every example suite in the repo and the README, rejects the
//! mistakes the parser rejects, and covers every action the parser knows.

use serde_json::{json, Value};

/// Every action key the parser accepts: the parser's own list, so the
/// two can't drift (VEL-208).
pub const ACTIONS: &[&str] = crate::parser::ACTION_KEYS;

/// The actions whose step may carry `data:` / `waitUntil:`.
const NATURAL: &[&str] = &["do", "act", "assert", "expect"];
/// Steps that take `force: true` (D-3).
const FORCE: &[&str] = &[
    "tap",
    "doubleTap",
    "longPress",
    "inputText",
    "clearText",
    "do",
    "act",
];

/// One of these fields must be given (`anyOf` of `required`).
fn one_of_required(fields: &[&str]) -> Value {
    Value::Array(fields.iter().map(|f| json!({ "required": [f] })).collect())
}

/// A selector: css alone, xpath alone, or native fields with at least one
/// that matches by itself (`index` or a relation alone means nothing).
/// The parser refuses css/xpath beside other fields (VEL-194/208).
fn selector_schema() -> Value {
    let mut native = selector_fields();
    let m = native.as_object_mut().expect("object");
    m.remove("css");
    m.remove("xpath");
    json!({ "oneOf": [
        obj(json!({ "css": { "type": "string", "description": "Web only" } }), &["css"]),
        obj(json!({ "xpath": { "type": "string", "description": "Web only" } }), &["xpath"]),
        {
            "type": "object",
            "properties": native,
            "additionalProperties": false,
            "anyOf": one_of_required(&["id", "text", "textContains", "textMatches", "idMatches", "accessibilityId", "className", "role", "name"])
        }
    ]})
}

fn selector_fields() -> Value {
    json!({
        "id": { "type": "string", "description": "Whole id, or its short form after `:id/`" },
        "text": { "type": "string", "description": "Exact visible text or label" },
        "textContains": { "type": "string", "description": "Part of the text or label, ignoring case" },
        "textMatches": { "type": "string", "description": "Regex the whole text, label or hint matches (ignoring case, `.` matches newlines; Maestro's text:)" },
        "idMatches": { "type": "string", "description": "Regex the whole id (or its short form) matches, ignoring case (Maestro's id:)" },
        "accessibilityId": { "type": "string" },
        "className": { "type": "string", "description": "Full (android.widget.Button) or short (Button)" },
        "role": { "anyOf": [{ "enum": velocity_common::role::ROLES }, { "type": "string", "pattern": "\\$\\{" }], "description": "Role, by Playwright's role names (web: computed ARIA role; devices: the class or element type mapped, see the README's role table)" },
        "name": { "type": "string", "description": "Accessible name (label, else text, else the content of a button/link/…), compared like text:" },
        "checked": { "type": "boolean", "description": "State filter: a checkbox / switch / radio is checked (only checkable elements have the state)" },
        "focused": { "type": "boolean", "description": "State filter: the element has input focus. Never matches where the driver can't report focus (headless)" },
        "selected": { "type": "boolean", "description": "State filter: the element is selected (tab, row, option). Never matches where the driver can't report selection (headless)" },
        "enabled": { "type": "boolean", "description": "State filter: the element is enabled" },
        "index": { "type": "integer", "minimum": 0, "description": "Nth match in reading order" },
        "css": { "type": "string", "description": "Web only" },
        "xpath": { "type": "string", "description": "Web only" },
        "below": { "$ref": "#/$defs/selector" },
        "above": { "$ref": "#/$defs/selector" },
        "leftOf": { "$ref": "#/$defs/selector" },
        "rightOf": { "$ref": "#/$defs/selector" },
        "containsChild": { "$ref": "#/$defs/selector" },
        "childOf": { "$ref": "#/$defs/selector" },
    })
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

fn any_value() -> Value {
    json!({})
}

/// The schema for one action's argument.
fn action_arg(action: &str) -> Value {
    let sel = json!({ "$ref": "#/$defs/selector" });
    let key =
        json!({ "enum": velocity_common::Key::ALL.iter().map(|k| k.name()).collect::<Vec<_>>() });
    let direction = json!({ "enum": ["up", "down", "left", "right"] });
    let point = obj(
        json!({ "x": { "type": "integer" }, "y": { "type": "integer" } }),
        &["x", "y"],
    );
    match action {
        "launchApp" => json!({ "oneOf": [
            { "type": "null" },
            obj(json!({
                "appId": { "type": "string" },
                "clearState": { "type": "boolean" },
                "arguments": {
                    "type": "object",
                    "description": "Launch arguments passed to the app (name -> string | bool | number); names are `[A-Za-z0-9_.-]`, not starting with `-`",
                    "additionalProperties": { "anyOf": [{ "type": "string" }, { "type": "boolean" }, { "type": "number" }] }
                }
            }), &[])
        ]}),
        "stopApp" => json!({ "oneOf": [
            { "type": "null" },
            obj(json!({ "appId": { "type": "string" } }), &[])
        ]}),
        "assertSystemState" => obj(
            json!({
                "mediaPlaying": { "type": "boolean" },
                "mediaTitle": { "type": "string" },
                "inCall": { "type": "boolean" }
            }),
            &[],
        ),
        "mockRequest" => mock_rule(),
        "http" => http_request(),
        "exec" => obj(
            json!({
                "run": {
                    "description": "The program and its arguments: a string split on spaces (quotes keep a part whole; no shell) or a list. The program must be in config.exec.allow",
                    "anyOf": [{ "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1 }]
                },
                "expect": obj(json!({
                    "exitCode": { "type": "integer", "description": "The exit code it must end with (default 0)" },
                    "contains": { "type": "string", "description": "Text its output must contain" }
                }), &[]),
                "capture": { "type": "string", "pattern": "^[A-Za-z_][A-Za-z0-9_]*$", "description": "${NAME} for its trimmed standard output, for later steps" },
                "cwd": { "type": "string", "description": "Its working directory" }
            }),
            &["run"],
        ),
        "clearMocks" | "goBack" | "refresh" => no_args(),
        "setConfig" => json!({
            "type": "object",
            "description": "Change settings (`velocity settings`) or sync.timeoutMs/reactionMs/quietMs/settleCapMs for the rest of this test",
            "additionalProperties": { "type": ["string", "number", "boolean"] }
        }),
        "tap" | "doubleTap" | "clearText" | "assertVisible" | "assertNotVisible" => sel,
        // `selector:` or the selector's fields inline, never both (VEL-221).
        "drag" => obj(
            json!({
                "from": sel.clone(),
                "to": sel.clone(),
                "holdMs": { "type": "integer", "minimum": 0, "description": "Hold before moving, so the list picks the row up (default 700)" },
                "moveMs": { "type": "integer", "minimum": 0, "description": "How long the move takes (default 900)" }
            }),
            &["from", "to"],
        ),
        "longPress" => {
            let duration = json!({ "type": "integer", "minimum": 0 });
            let mut inline = selector_fields();
            inline["durationMs"] = duration.clone();
            json!({ "oneOf": [
                obj(json!({ "selector": sel, "durationMs": duration }), &["selector"]),
                {
                    "type": "object",
                    "properties": inline,
                    "additionalProperties": false,
                    "anyOf": one_of_required(&["id", "text", "textContains", "textMatches", "idMatches", "accessibilityId", "className", "role", "name", "css", "xpath"])
                }
            ]})
        }
        "inputText" => obj(
            json!({ "selector": sel, "text": { "type": "string" } }),
            &["selector", "text"],
        ),
        "assertText" => obj(
            json!({ "selector": sel, "expected": { "type": "string" } }),
            &["selector", "expected"],
        ),
        "assertScreenshot" => json!({ "oneOf": [
            { "type": "string" },
            obj(json!({
                "name": { "type": "string" },
                "threshold": { "type": "number", "minimum": 0, "maximum": 1, "description": "Share of compared pixels that must match (default 0.98); not a colour tolerance" },
                "clusterPx": { "type": "integer", "minimum": 0 },
                "mask": { "type": "array", "items": sel.clone() },
                "tolerance": { "type": "integer", "minimum": 0, "maximum": 255, "description": "comparator strict: largest per-channel difference that still matches (default 16)" },
                "comparator": { "enum": ["strict", "perceptual"], "description": "strict (default): per-channel tolerance; perceptual: YIQ distance, threshold 0.1 (pixelmatch/Playwright)" },
                "crop": { "description": "Compare only this part: an element (selector) or a rect in screen coordinates", "oneOf": [
                    obj(json!({
                        "x": { "type": "integer" },
                        "y": { "type": "integer" },
                        "width": { "type": "integer", "minimum": 1 },
                        "height": { "type": "integer", "minimum": 1 }
                    }), &["x", "y", "width", "height"]),
                    sel
                ]}
            }), &["name"])
        ]}),
        "scrollUntilVisible" => obj(
            json!({ "selector": sel, "direction": direction, "maxScrolls": { "type": "integer", "minimum": 1 }, "within": sel }),
            &["selector"],
        ),
        "swipe" => obj(
            json!({ "direction": direction, "from": point.clone(), "to": point, "within": sel }),
            &[],
        ),
        "screenshot" => json!({ "oneOf": [
            { "type": "null" },
            { "type": "string", "description": "File name; .png is added when it has no extension" },
            obj(json!({ "filename": { "type": "string" } }), &[])
        ]}),
        "pressKey" => obj(json!({ "key": key }), &["key"]),
        "wait" => obj(
            json!({ "ms": { "type": "integer", "minimum": 0 } }),
            &["ms"],
        ),
        "runFlow" => json!({ "oneOf": [
            { "type": "string" },
            {
                "type": "object",
                "properties": {
                    "flowId": { "type": "string" },
                    "flow": { "type": "string" },
                    "env": {
                        "type": "object",
                        "description": "Parameters for this call: `${NAME}` inside the flow (and flows it runs) takes the value. Values are strings, integers or booleans; quote a decimal such as \"1.50\" so it is typed exactly as written",
                        "additionalProperties": { "type": ["string", "integer", "boolean"] }
                    }
                },
                "additionalProperties": false,
                "anyOf": one_of_required(&["flowId", "flow"])
            }
        ]}),
        "tapXY" => point,
        "inputRaw" => obj(json!({ "text": { "type": "string" } }), &["text"]),
        "goto" | "openLink" => json!({ "oneOf": [
            { "type": "string" },
            obj(json!({ "url": { "type": "string" } }), &["url"]),
            obj(json!({ "link": { "type": "string" } }), &["link"])
        ]}),
        "saveState" | "restoreState" => json!({ "oneOf": [
            { "type": "string" },
            obj(json!({ "name": { "type": "string" } }), &["name"])
        ]}),
        "do" | "act" | "assert" | "expect" => json!({ "type": "string" }),
        _ => any_value(),
    }
}

/// Nothing, or `{}`.
fn no_args() -> Value {
    json!({ "oneOf": [{ "type": "null" }, obj(json!({}), &[])] })
}

/// String values that may be written as numbers or booleans (`page: 2`).
fn scalar_map() -> Value {
    json!({
        "type": "object",
        "additionalProperties": { "anyOf": [{ "type": "string" }, { "type": "number" }, { "type": "boolean" }] }
    })
}

/// The `http` step (velocity-common's `HttpRequest`).
fn http_request() -> Value {
    let matcher = json!({ "$ref": "#/$defs/matcher" });
    let matchers = json!({ "type": "object", "additionalProperties": matcher });
    let status = json!({ "anyOf": [
        { "type": "integer", "minimum": 100, "maximum": 599 },
        { "type": "string", "pattern": "^([1-5][xX][xX]|[1-5][0-9][0-9])$" },
        { "type": "array", "items": { "type": "integer", "minimum": 100, "maximum": 599 } }
    ], "description": "201, [200, 204] or 2xx; without it any status below 400 passes" });
    let capture = json!({ "anyOf": [
        { "type": "string", "description": "JSONPath into the body" },
        obj(json!({ "json": { "type": "string" } }), &["json"]),
        obj(json!({ "header": { "type": "string" } }), &["header"]),
        obj(json!({ "regex": { "type": "string", "description": "The first group (else the whole match) in the body" } }), &["regex"])
    ]});
    obj(
        json!({
            "method": { "type": "string", "description": "GET (default), POST, PUT, PATCH, DELETE, …" },
            "url": { "type": "string", "description": "Absolute, or appended to config.http.baseUrl" },
            "headers": scalar_map(),
            "query": scalar_map(),
            "json": { "description": "JSON body" },
            "body": { "type": "string", "description": "Text body" },
            "form": scalar_map(),
            "expect": obj(
                json!({
                    "status": status,
                    "headers": matchers,
                    "json": { "type": "object", "additionalProperties": matcher, "description": "JSONPath (RFC 9535; `$.` may be left out) → matcher" },
                    "body": matcher,
                    "maxMs": { "type": "integer", "minimum": 0 }
                }),
                &[],
            ),
            "capture": { "type": "object", "additionalProperties": capture, "description": "NAME → where its value comes from; used as ${NAME} in later steps" },
            "retry": obj(
                json!({
                    "count": { "type": "integer", "minimum": 0, "maximum": velocity_api::validate::MAX_RETRIES },
                    "intervalMs": { "type": "integer", "minimum": 0 }
                }),
                &["count"],
            ),
            "followRedirects": { "type": "boolean" }
        }),
        &["url"],
    )
}

/// An `http` expectation: a plain value to equal, or a map of checks.
fn matcher_schema() -> Value {
    let any = json!({});
    let num = json!({ "type": "number" });
    let mut checks = obj(
        json!({
            "eq": any, "ne": any,
            "gt": num, "gte": num, "lt": num, "lte": num,
            "contains": { "description": "Substring, list element, or object subset" },
            "matches": { "type": "string", "description": "Regex the whole value must match" },
            "exists": { "type": "boolean" },
            "type": { "enum": velocity_common::JsonType::ALL },
            "length": { "$ref": "#/$defs/matcher" },
            "oneOf": { "type": "array" }
        }),
        &[],
    );
    checks["minProperties"] = json!(1);
    json!({ "anyOf": [
        { "type": "string" }, { "type": "number" }, { "type": "boolean" }, { "type": "null" }, { "type": "array" },
        checks
    ]})
}

fn mock_rule() -> Value {
    obj(
        json!({
            "url": { "type": "string", "description": "Glob over the whole URL (with * or ?), else a substring of it" },
            "method": { "type": "string" },
            "status": { "type": "integer", "minimum": 100, "maximum": 599 },
            "json": { "description": "JSON body" },
            "body": { "type": "string" },
            "file": { "type": "string" },
            "headers": { "type": "object", "additionalProperties": { "type": "string" } },
            "delayMs": { "type": "integer", "minimum": 0 },
            "block": { "type": "boolean" },
            "times": { "type": "integer", "minimum": 1 }
        }),
        &["url"],
    )
}

fn step() -> Value {
    let variants: Vec<Value> = ACTIONS
        .iter()
        .map(|a| {
            let mut props = json!({
                *a: action_arg(a),
                "timeoutMs": { "type": "integer", "minimum": 0 },
                "timeout_ms": { "type": "integer", "minimum": 0 },
                "settle": {
                    "description": "This step's settle: none (act and move on), auto, or its own timings",
                    "oneOf": [
                        { "enum": ["none", "off", "auto"] },
                        obj(json!({
                            "reactionMs": { "type": "integer", "minimum": 0 },
                            "quietMs": { "type": "integer", "minimum": 0 },
                            "capMs": { "type": "integer", "minimum": 0 },
                            "reaction_ms": { "type": "integer", "minimum": 0 },
                            "quiet_ms": { "type": "integer", "minimum": 0 },
                            "cap_ms": { "type": "integer", "minimum": 0 }
                        }), &[])
                    ]
                },
            });
            props["optional"] = json!({
                "type": "boolean",
                "description": "A failure of this step is a warning; the test goes on. On runFlow: the flow stops at its first failing step and the test goes on after it"
            });
            if FORCE.contains(a) {
                props["force"] = json!({
                    "type": "boolean",
                    "description": "Act without the actionability checks (stable, enabled, hit target)"
                });
            }
            if *a == "assertNotVisible" {
                props["appInFront"] = json!({
                    "type": "boolean",
                    "description": "false: may pass while another app is in front"
                });
            }
            props["checkpoint"] = if NATURAL.contains(a) {
                json!({
                    "oneOf": [{ "type": "boolean" }, { "type": "string" }],
                    "description": "An AI check after this step under verify: checkpoints or strict; true checks the step's own waitUntil or intent, a string is the claim to check"
                })
            } else {
                json!({
                    "type": "string",
                    "description": "The claim an AI check confirms after this step under verify: checkpoints or strict (on natural-language steps also true: their own waitUntil or intent)"
                })
            };
            // The parser refuses them on any other step (VEL-194).
            if NATURAL.contains(a) {
                props["data"] = json!({ "oneOf": [
                    { "type": "string" },
                    obj(json!({ "value": { "type": "string" } }), &["value"])
                ]});
                props["waitUntil"] = json!({ "type": "string" });
                props["wait_until"] = json!({ "type": "string" });
            }
            json!({
                "type": "object",
                "description": crate::reference::action_doc(a),
                "properties": props,
                "required": [a],
                "additionalProperties": false,
            })
        })
        .collect();
    let mut all = vec![json!({ "type": "string", "description": "A plain-English step" })];
    all.extend(variants);
    json!({ "oneOf": all })
}

/// The whole schema.
pub fn suite_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://velocity.dev/schema/suite.json",
        "title": "Velocity test suite",
        "type": "object",
        "required": ["tests"],
        "additionalProperties": false,
        "properties": {
            "appId": { "type": "string", "description": "Bundle id / package name (a suite of only http steps may leave it out)" },
            "config": {
                "type": "object",
                "properties": {
                    "platform": { "enum": ["ios", "android", "web"] },
                    "app": {
                        "description": "App build Velocity installs before the first test (skipped when the device has this exact build): one path, or one per platform",
                        "oneOf": [
                            { "type": "string" },
                            {
                                "type": "object",
                                "properties": {
                                    "android": { "type": "string", "description": ".apk" },
                                    "ios": { "type": "string", "description": ".app bundle, or .ipa for a device" }
                                },
                                "additionalProperties": false
                            }
                        ]
                    },
                    "sync": {
                        "type": "object",
                        "properties": {
                            "mode": { "enum": ["auto", "polling", "native", "none"] },
                            "quietMs": { "type": "integer", "minimum": 0 },
                            "reactionMs": { "type": "integer", "minimum": 0 },
                            "settleCapMs": { "type": "integer", "minimum": 0 },
                            "timeoutMs": { "type": "integer", "minimum": 0 },
                            "maxTimerMs": { "type": "integer", "minimum": 0, "description": "Native probe: one-shot timers due further out than this don't keep the app busy (default 1500; iOS probe)" },
                            "intervalMs": { "type": "integer", "deprecated": true },
                            "stabilityCount": { "type": "integer", "deprecated": true }
                        }
                    },
                    "retry": { "type": "object", "properties": { "count": { "type": "integer", "minimum": 0 }, "flakyResult": { "enum": ["pass", "fail"], "description": "fail: a test that passed only on retry exits 1 (as --fail-on-flaky)" } } },
                    "artifacts": {
                        "type": "object",
                        "properties": {
                            "onFailure": { "type": "boolean" },
                            "on_failure": { "type": "boolean" },
                            "outputDir": { "type": "string" },
                            "output_dir": { "type": "string" },
                            "screenshots": { "enum": crate::config_keys::variants::<velocity_common::ScreenshotPolicy>(), "description": "screenshot: steps write at once, only for a failing test, or not at all" },
                            "stepScreenshots": { "type": "boolean", "description": "Screen after every step, written only for a failing test" },
                            "step_screenshots": { "type": "boolean" },
                            "video": { "enum": ["off", "on-failure", "onFailure", "always"], "description": "Record each test's screen (Android: H.264 ring buffer; iOS simulator: simctl recordVideo; web: screencast frames), kept for failed tests by default, as traces/<test>/video.mp4" },
                            "videoSeconds": { "type": "integer", "minimum": 1, "description": "Android: seconds of video kept in memory per test (default 30)" },
                            "video_seconds": { "type": "integer", "minimum": 1 },
                            "logs": { "enum": ["off", "on-failure", "onFailure", "always"], "description": "Capture device logs per test (logcat, simulator log stream, browser console), kept for failed tests by default; secrets are masked" }
                        }
                    },
                    "reset": {
                        "type": "object",
                        "properties": {
                            "strategy": { "enum": ["clear", "reinstall", "snapshot"] },
                            "appPath": { "type": "string" },
                            "snapshot": { "type": "string" }
                        }
                    },
                    "http": obj(
                        json!({
                            "baseUrl": { "type": "string", "description": "Prefix of an http step's url without a scheme" },
                            "headers": scalar_map(),
                            "timeoutMs": { "type": "integer", "minimum": 1, "description": "How long a request may take (default 30000); a step's timeoutMs overrides it" },
                            "insecure": { "type": "boolean", "description": "Accept any TLS certificate (a dev server's self-signed one)" }
                        }),
                        &[],
                    ),
                    "exec": obj(
                        json!({
                            "allow": { "type": "array", "items": { "type": "string" }, "description": "Programs exec steps may run, exactly as a step names them (adb, ./scripts/seed.sh); exec is off without them" }
                        }),
                        &[],
                    ),
                    "settings": {
                        "type": "object",
                        "description": "Driver and runtime settings (`velocity settings` lists them); flat `android.scroll:` or nested keys"
                    },
                    "dialog": { "type": "object" },
                    "healing": {
                        "type": "object",
                        "properties": {
                            "enabled": { "type": "boolean" },
                            "mode": {
                                "enum": ["auto", "report", "off"],
                                "description": "auto: heal and pass; report: fail with the proposed patch (default in CI); off"
                            },
                            "confidenceThreshold": { "type": "number", "minimum": 0, "maximum": 1 },
                            "persistHealed": { "type": "boolean" }
                        }
                    },
                    "actionability": {
                        "enum": ["strict", "lenient"],
                        "description": "strict (default): an action fails when its target never holds still, stays disabled or is covered; lenient: act anyway with a warning"
                    },
                    "nl": {
                        "type": "object",
                        "properties": {
                            "allowCrossOrigin": { "type": "boolean", "description": "A natural-language goto may leave the current page's origin" },
                            "dataPolicy": { "enum": ["verbatim", "describe"], "description": "describe: the model sees only a description of each data value" }
                        },
                        "additionalProperties": false
                    },
                    "verify": {
                        "enum": velocity_common::VerifyLevel::NAMES,
                        "description": "AI checks: off (default; deterministic assertions only), final (the test's description holds at the end), checkpoints (also after assert: and checkpoint: steps; failures reported at the end), strict (the first failed check fails the test)"
                    },
                    "ai": {
                        "type": "object",
                        "description": "The model service for natural-language steps and checks; VELOCITY_NL_* environment variables win over it",
                        "properties": {
                            "provider": { "enum": velocity_common::natural::AI_PROVIDER_PRESETS, "description": "A provider preset: the native APIs (anthropic, openai, azure-openai, gemini, bedrock, vertex), OpenAI-compatible services (openrouter, groq, mistral, deepseek, xai, together, fireworks, dashscope, perplexity), local servers (ollama, lmstudio, vllm) or openai-compatible with baseUrl; docs/AI-PROVIDERS.md" },
                            "model": { "type": "string", "description": "Model for single steps and checks; may be provider:model" },
                            "exploreModel": { "type": "string", "description": "Model for goal-level planning (velocity explore)" },
                            "baseUrl": { "type": "string", "description": "API base URL, e.g. http://localhost:11434/v1" },
                            "apiKeyEnv": { "type": "string", "pattern": "^[A-Za-z_][A-Za-z0-9_]*$", "description": "Name of the environment variable holding the key (never the key itself)" },
                            "images": { "type": "boolean", "description": "Send screenshots to the checker (default: the provider's)" },
                            "region": { "type": "string", "description": "Bedrock AWS region or Vertex location" },
                            "project": { "type": "string", "description": "Vertex Google Cloud project" },
                            "apiVersion": { "type": "string", "description": "Azure OpenAI api-version (deployments URL); unset uses /openai/v1" }
                        },
                        "additionalProperties": false
                    },
                    "performance": {
                        "type": "object",
                        "description": "Opt-in memory profiling and the memory check (Android only: the other drivers report no memory)",
                        "properties": {
                            "enabled": { "type": "boolean", "description": "Sample the app's memory and CPU during the run (default false)" },
                            "intervalMs": { "type": "integer", "minimum": 1, "description": "Sampling interval during long actions, in ms (default 500)" },
                            "pssGrowthThresholdPct": { "type": "integer", "minimum": 0, "description": "Fail a test whose peak total PSS is more than this percent above its saved baseline (default 15). Only clean passes update the baseline. Was heapGrowthThresholdPct, which still works with a warning" }
                        }
                    },
                    "web": {
                        "type": "object",
                        "properties": {
                            "mocks": { "type": "array", "items": mock_rule() },
                            "block": { "type": "array", "items": { "type": "string" } },
                            "device": {
                                "type": "string",
                                "examples": velocity_common::web_devices::LEGACY_NAMES.to_vec(),
                                "description": "A mobile-web profile from Playwright's device list (any name, or \"<name> landscape\"): viewport, pixel ratio, user agent, mobile layout and touch (Chromium, not Safari)"
                            },
                            "parallelMode": {
                                "enum": ["contexts", "processes"],
                                "description": "What each --parallel browser is: contexts (default: browser contexts in one Chrome, each with its own cookies, storage and mocks) or processes (a Chrome process each)"
                            },
                            "settle": {
                                "type": "object",
                                "description": "Web-only settle timings; they win over sync.* on the web (defaults: 30 ms reaction, sync.quietMs, 1 s cap)",
                                "properties": {
                                    "reactionMs": { "type": "integer", "minimum": 0 },
                                    "quietMs": { "type": "integer", "minimum": 0 },
                                    "capMs": { "type": "integer", "minimum": 0 }
                                },
                                "additionalProperties": false
                            }
                        }
                    },
                    "screenshots": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "freezeStatusBar": { "type": "boolean", "description": "Freeze the status bar (Android demo mode, iOS simulator simctl status_bar) before assertScreenshot and compare it instead of masking it" },
                            "freeze_status_bar": { "type": "boolean" }
                        }
                    },
                    "accessibility": {
                        "type": "object",
                        "additionalProperties": false,
                        "description": "Accessibility audit of the screens each test reads (docs/DSL.md#accessibility-audit)",
                        "properties": {
                            "audit": { "type": "boolean", "description": "Audit each distinct screen once per test and report findings as warnings (default off)" },
                            "fail": { "type": "boolean", "description": "Fail a test on an error-severity finding" },
                            "density": { "type": "number", "exclusiveMinimum": 0, "description": "Android pixels per dp for the 48 dp target size (default: the screen's short side is 411 dp)" }
                        }
                    },
                    "headless": {
                        "type": "object",
                        "properties": {
                            "width": { "type": "integer", "minimum": 1 },
                            "height": { "type": "integer", "minimum": 1 },
                            "density": { "type": "number", "exclusiveMinimum": 0 },
                            "devicePixelRatio": { "type": "number", "exclusiveMinimum": 0, "description": "Physical px per logical px: native headless density, Flutter test surface ratio" },
                            "device_pixel_ratio": { "type": "number", "exclusiveMinimum": 0 },
                            "fontScale": { "type": "number", "exclusiveMinimum": 0, "description": "Text size multiplier for native headless renders (default 1)" },
                            "font_scale": { "type": "number", "exclusiveMinimum": 0 },
                            "appPath": { "type": "string" },
                            "app_path": { "type": "string" },
                            "initialLayout": { "type": "string" },
                            "initial_layout": { "type": "string" }
                        }
                    },
                    "reactNative": { "type": "object", "deprecated": true, "description": "Ignored: React Native has no headless renderer (run on a device or simulator)" },
                    "flutter": {
                        "type": "object",
                        "properties": {
                            "hierarchy": { "enum": ["semantics", "widgets"], "description": "semantics (default) or widgets (deprecated opt-out)" }
                        }
                    },
                    "driver": { "enum": crate::config_keys::variants::<velocity_common::config::DriverMode>() },
                    "framework": { "enum": crate::config_keys::variants::<velocity_common::config::Framework>() },
                    "batch": { "type": "boolean", "description": "Share one launch across consecutive tests" }
                }
            },
            "flows": {
                "type": "array",
                "items": obj(json!({
                    "id": { "type": "string" },
                    "steps": { "type": "array", "items": { "$ref": "#/$defs/step" } }
                }), &["id", "steps"])
            },
            "tests": {
                "type": "array",
                "items": obj(json!({
                    "name": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "isolated": { "type": "boolean" },
                    "dependsOn": { "type": "array", "items": { "type": "string" } },
                    "depends_on": { "type": "array", "items": { "type": "string" } },
                    "sideEffects": { "type": "boolean", "description": "Never skip this test from the result cache" },
                    "description": { "type": "string", "description": "What the test shows; verify: final checks it on the last screen (the name stands in without it)" },
                    "verify": { "enum": velocity_common::VerifyLevel::NAMES, "description": "This test's AI verification level, over config.verify" },
                    "steps": { "type": "array", "items": { "$ref": "#/$defs/step" } }
                }), &["name", "steps"])
            }
        },
        "$defs": {
            "selector": selector_schema(),
            "matcher": matcher_schema(),
            "step": step()
        }
    })
}

#[cfg(test)]
pub(crate) mod validate {
    //! The subset of JSON Schema the suite schema uses: type, enum,
    //! properties, required, additionalProperties: false, items, oneOf,
    //! $ref into $defs, minimum/maximum, minProperties, pattern.
    use serde_json::Value;

    pub fn errors(schema: &Value, root: &Value, v: &Value, path: &str, out: &mut Vec<String>) {
        if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
            let name = r.trim_start_matches("#/$defs/");
            return errors(&root["$defs"][name], root, v, path, out);
        }
        if let Some(any) = schema.get("anyOf").and_then(Value::as_array) {
            let ok = any.iter().any(|s| {
                let mut e = Vec::new();
                errors(s, root, v, path, &mut e);
                e.is_empty()
            });
            if !ok {
                out.push(format!("{path}: matches none of anyOf"));
            }
        }
        if let Some(variants) = schema.get("oneOf").and_then(Value::as_array) {
            let ok = variants
                .iter()
                .filter(|s| {
                    let mut e = Vec::new();
                    errors(s, root, v, path, &mut e);
                    e.is_empty()
                })
                .count();
            if ok != 1 {
                out.push(format!("{path}: matches {ok} of the allowed forms"));
            }
            return;
        }
        if let Some(t) = schema.get("type").and_then(Value::as_str) {
            let good = match t {
                "object" => v.is_object(),
                "array" => v.is_array(),
                "string" => v.is_string(),
                "integer" => v.is_i64() || v.is_u64(),
                "number" => v.is_number(),
                "boolean" => v.is_boolean(),
                "null" => v.is_null(),
                _ => true,
            };
            if !good {
                out.push(format!("{path}: expected {t}"));
                return;
            }
        }
        if let (Some(p), Some(text)) = (schema.get("pattern").and_then(Value::as_str), v.as_str()) {
            if !regex::Regex::new(p).is_ok_and(|re| re.is_match(text)) {
                out.push(format!("{path}: {text:?} doesn't match {p:?}"));
            }
        }
        if let Some(options) = schema.get("enum").and_then(Value::as_array) {
            if !options.contains(v) {
                out.push(format!("{path}: {v} not one of {options:?}"));
            }
        }
        if let (Some(min), Some(n)) = (schema.get("minimum").and_then(Value::as_f64), v.as_f64()) {
            if n < min {
                out.push(format!("{path}: below {min}"));
            }
        }
        if let (Some(max), Some(n)) = (schema.get("maximum").and_then(Value::as_f64), v.as_f64()) {
            if n > max {
                out.push(format!("{path}: above {max}"));
            }
        }
        if let Some(o) = v.as_object() {
            if let Some(min) = schema.get("minProperties").and_then(Value::as_u64) {
                if (o.len() as u64) < min {
                    out.push(format!("{path}: needs at least {min} field(s)"));
                }
            }
            let props = schema.get("properties").and_then(Value::as_object);
            for req in schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(k) = req.as_str() {
                    if !o.contains_key(k) {
                        out.push(format!("{path}: missing {k}"));
                    }
                }
            }
            for (k, val) in o {
                match props.and_then(|p| p.get(k)) {
                    Some(s) => errors(s, root, val, &format!("{path}.{k}"), out),
                    None if schema.get("additionalProperties") == Some(&Value::Bool(false)) => {
                        out.push(format!("{path}: unknown field {k}"))
                    }
                    None => {}
                }
            }
        }
        if let (Some(items), Some(a)) = (schema.get("items"), v.as_array()) {
            for (i, x) in a.iter().enumerate() {
                errors(items, root, x, &format!("{path}[{i}]"), out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(yaml: &str) -> Vec<String> {
        let v: Value = serde_yaml::from_str(yaml).unwrap();
        let schema = suite_schema();
        let mut out = Vec::new();
        validate::errors(&schema, &schema, &v, "$", &mut out);
        out
    }

    fn suites() -> Vec<(String, String)> {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
        let mut files = vec![
            format!("{root}/tests/smoke/android-settings.yaml"),
            format!("{root}/tests/smoke/ios-settings.yaml"),
            format!("{root}/examples/web-example/velocity.yaml"),
        ];
        if let Ok(dir) = std::fs::read_dir(format!("{root}/tests/fixtures")) {
            for e in dir.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "yaml") {
                    files.push(p.to_string_lossy().into_owned());
                }
            }
        }
        let mut out: Vec<(String, String)> = files
            .into_iter()
            .filter_map(|f| std::fs::read_to_string(&f).ok().map(|s| (f, s)))
            .filter(|(_, s)| s.contains("appId:") && s.contains("tests:"))
            .collect();
        let readme = std::fs::read_to_string(format!("{root}/README.md")).unwrap();
        for block in readme.split("```yaml\n").skip(1) {
            let y = block.split("```").next().unwrap_or("");
            if y.contains("appId:") && y.contains("tests:") {
                out.push(("README".into(), y.to_string()));
            }
        }
        out
    }

    #[test]
    fn every_suite_the_parser_accepts_validates() {
        let all = suites();
        assert!(all.len() >= 3);
        for (name, yaml) in all {
            if crate::parser::parse_suite_from_str(&yaml).is_err() {
                continue;
            }
            let errs = check(&yaml);
            assert!(errs.is_empty(), "{name}: {errs:?}");
        }
    }

    #[test]
    fn the_app_build_is_one_path_or_one_per_platform() {
        let tests = "tests:\n  - name: t\n    steps:\n      - launchApp: {}\n";
        for app in [
            "app: build/app.apk",
            "app: { android: build/app.apk, ios: build/App.app }",
            "app:\n    ios: build/App.app",
        ] {
            let y = format!("appId: x\nconfig:\n  {app}\n{tests}");
            crate::parser::parse_suite_from_str(&y).unwrap_or_else(|e| panic!("{app}: {e}"));
            assert!(check(&y).is_empty(), "{app}: {:?}", check(&y));
        }
        let typo = format!("appId: x\nconfig:\n  app: {{ andriod: a.apk }}\n{tests}");
        assert!(crate::parser::parse_suite_from_str(&typo).is_err());
        assert!(!check(&typo).is_empty());
    }

    #[test]
    fn rejects_what_the_parser_rejects() {
        let base = "appId: x\ntests:\n  - name: t\n    steps:\n";
        for bad in [
            "      - tap: { txet: \"Go\" }\n",
            "      - assertVisible: { text: \"Done\", timeoutMs: 5 }\n",
            "      - teleport: { to: \"Mars\" }\n",
            "      - pressKey: { key: power }\n",
        ] {
            let y = format!("{base}{bad}");
            assert!(
                crate::parser::parse_suite_from_str(&y).is_err(),
                "parser accepts {bad}"
            );
            assert!(!check(&y).is_empty(), "schema accepts {bad}");
        }
    }

    /// S1/S2: role, name and state keys validate where the parser takes
    /// them, and both refuse an unknown role and a state alone.
    #[test]
    fn role_name_and_state_keys_validate() {
        let base = "appId: x\ntests:\n  - name: t\n    steps:\n";
        for good in [
            "      - tap: { role: button, name: \"Pay\" }\n",
            "      - assertVisible: { text: \"Remember\", checked: true, enabled: true }\n",
            "      - tap: { role: \"${ROLE}\", selected: true }\n",
            "      - longPress: { role: listitem, name: Coffee, durationMs: 9 }\n",
        ] {
            let y = format!("{base}{good}");
            crate::parser::parse_suite_from_str(&y).unwrap_or_else(|e| panic!("{good}: {e}"));
            assert!(check(&y).is_empty(), "{good}: {:?}", check(&y));
        }
        for bad in [
            "      - tap: { checked: true }\n",
            "      - tap: { role: buton }\n",
        ] {
            let y = format!("{base}{bad}");
            let parsed = crate::parser::parse_suite_from_str(&y)
                .and_then(|s| crate::validator::validate_suite(&s).map(|_| s));
            assert!(parsed.is_err(), "parser accepts {bad}");
            assert!(!check(&y).is_empty(), "schema accepts {bad}");
        }
    }

    #[test]
    fn every_schema_action_parses() {
        let example = |a: &str| -> String {
            match a {
                "launchApp" | "stopApp" | "screenshot" | "goBack" | "refresh" => {
                    format!("{a}: {{}}")
                }
                "tap" | "doubleTap" | "clearText" | "assertVisible" | "assertNotVisible" => {
                    format!("{a}: {{ text: \"x\" }}")
                }
                "longPress" => format!("{a}: {{ text: \"x\" }}"),
                "drag" => format!("{a}: {{ from: {{ text: \"Milk\" }}, to: {{ text: \"Eggs\" }}, holdMs: 800 }}"),
                "inputText" => format!("{a}: {{ selector: {{ id: \"e\" }}, text: \"t\" }}"),
                "assertText" => format!("{a}: {{ selector: {{ id: \"e\" }}, expected: \"t\" }}"),
                "assertScreenshot" | "saveState" | "restoreState" => format!("{a}: \"home\""),
                "scrollUntilVisible" => format!("{a}: {{ selector: {{ text: \"x\" }} }}"),
                "swipe" => format!("{a}: {{ direction: up }}"),
                "pressKey" => format!("{a}: {{ key: back }}"),
                "wait" => format!("{a}: {{ ms: 1 }}"),
                "runFlow" => format!("{a}: \"f\""),
                "tapXY" => format!("{a}: {{ x: 1, y: 2 }}"),
                "assertSystemState" => {
                    format!("{a}: {{ mediaPlaying: true, mediaTitle: \"Song\" }}")
                }
                "inputRaw" => format!("{a}: {{ text: \"t\" }}"),
                "setConfig" => format!("{a}: {{ android.scroll: semantic, sync.quietMs: 50 }}"),
                "mockRequest" => {
                    format!("{a}: {{ url: \"*/api/*\", status: 500, json: {{ error: x }} }}")
                }
                "clearMocks" => format!("{a}: {{}}"),
                "goto" | "openLink" => format!("{a}: \"https://x.dev\""),
                "exec" => format!("{a}: {{ run: \"adb shell pm clear com.app\", expect: {{ exitCode: 0 }}, capture: OUT }}"),
                "http" => format!(
                    "{a}: {{ method: POST, url: \"https://x.dev/o\", json: {{ a: 1 }}, query: {{ p: 2 }}, \
                     expect: {{ status: 2xx, json: {{ \"$.id\": {{ type: string, length: {{ gte: 1 }} }}, n: 3 }}, maxMs: 500 }}, \
                     capture: {{ ID: \"$.id\", LOC: {{ header: location }} }}, retry: {{ count: 2 }} }}"
                ),
                _ => format!("{a}: \"tap the button\""),
            }
        };
        for a in ACTIONS {
            let y = format!(
                "appId: x\nflows:\n  - id: f\n    steps:\n      - wait: {{ ms: 1 }}\ntests:\n  - name: t\n    steps:\n      - {}\n",
                example(a)
            );
            crate::parser::parse_suite_from_str(&y).unwrap_or_else(|e| panic!("{a}: {e}"));
            assert!(check(&y).is_empty(), "{a}: {:?}", check(&y));
        }
    }

    /// VEL-208: the schema and the parser agree on each of these, one way
    /// or the other.
    #[test]
    fn the_schema_agrees_with_the_parser_on_the_known_drifts() {
        let suite = |extra: &str, step: &str| {
            format!("appId: x\n{extra}flows:\n  - id: f\n    steps: [goBack]\ntests:\n  - name: t\n    steps:\n      - {step}\n  - name: u\n    depends_on: [t]\n    steps: [goBack]\n")
        };
        let cases = [
            ("", "longPress: { text: Edit, below: { text: Shipping } }"),
            ("", "longPress: { css: .row, durationMs: 5 }"),
            ("", "longPress: { selector: { id: a }, text: b }"),
            ("", "tap: { index: 0 }"),
            ("", "tap: { below: { text: a } }"),
            ("", "tap: { text: Pay, css: .cancel }"),
            ("", "tap: { css: .cancel }"),
            ("", "tap: { text: a }\n        settle: { quiet_ms: 5 }"),
            ("", "tap: { text: a }\n        waitUntil: done"),
            ("", "do: add\n        waitUntil: done"),
            ("", "goBack: { nonsense: 1 }"),
            ("", "clearMocks: { nonsense: 1 }"),
            ("", "goBack: {}"),
            ("", "runFlow: {}"),
            ("", "runFlow: { flow: f }"),
            ("", "assertScreenshot: { name: a, clusterPx: 0 }"),
            ("", "assertScreenshot: { name: a, tolerance: 300 }"),
            ("", "assertScreenshot: { name: a, comparator: perceptual }"),
            ("", "assertScreenshot: { name: a, comparator: fuzzy }"),
            ("", "assertScreenshot: { name: a, crop: { id: card } }"),
            (
                "",
                "assertScreenshot: { name: a, crop: { x: 0, y: 0, width: 10, height: 10 } }",
            ),
            (
                "",
                "assertScreenshot: { name: a, crop: { x: 0, y: 0, width: 10 } }",
            ),
            (
                "config:\n  screenshots:\n    freezeStatusBar: true\n",
                "goBack",
            ),
            ("config:\n  screenshots:\n    freeze: true\n", "goBack"),
            (
                "config:\n  artifacts:\n    screenshots: on_failure\n",
                "goBack",
            ),
            (
                "config:\n  accessibility:\n    audit: true\n    fail: true\n    density: 2.625\n",
                "goBack",
            ),
            ("config:\n  accessibility:\n    audits: true\n", "goBack"),
            ("config:\n  driver: bogus\n", "goBack"),
            ("config:\n  driver: headless\n  framework: rn\n", "goBack"),
        ];
        let mut disagree = Vec::new();
        for (extra, step) in cases {
            let y = suite(extra, step);
            let parser = crate::parser::parse_suite_from_str(&y).is_ok();
            let schema = check(&y);
            if parser != schema.is_empty() {
                disagree.push(format!(
                    "{extra}{step}: parser ok={parser}, schema={schema:?}"
                ));
            }
        }
        assert!(disagree.is_empty(), "{disagree:#?}");
    }

    /// Every key the parser reads for the suite, a test and a flow (aliases
    /// included, straight from serde) is in the schema.
    #[test]
    fn the_schema_has_every_key_the_parser_reads() {
        let schema = suite_schema();
        let [suite, test, flow] = crate::parser::suite_keys();
        let props = |v: &Value| -> Vec<String> {
            v["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect()
        };
        let have = [
            props(&schema),
            props(&schema["properties"]["tests"]["items"]),
            props(&schema["properties"]["flows"]["items"]),
        ];
        for (keys, have) in [suite, test, flow].iter().zip(&have) {
            for k in *keys {
                assert!(have.iter().any(|h| h == k), "schema lacks `{k}`: {have:?}");
            }
        }
        let config = props(&schema["properties"]["config"]);
        for k in crate::config_keys::fields::<velocity_common::SuiteConfig>() {
            // Both spellings are accepted; the schema lists the camelCase one.
            if k.contains('_') {
                continue;
            }
            assert!(config.iter().any(|h| h == k), "schema config lacks `{k}`");
        }
    }
}
