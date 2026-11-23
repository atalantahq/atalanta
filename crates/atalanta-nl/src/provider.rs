// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Which model service plans and checks: named provider presets on top of
//! three wire formats (Anthropic Messages, OpenAI Chat Completions, Bedrock
//! Converse), so a user picks `provider: <name>` and sets only the key's
//! environment variable.
//!
//! One `provider:model` string works everywhere: `--model groq:llama-…`,
//! `ATALANTA_NL_MODEL=gemini:gemini-3.8-flash`, `config.ai.model`. A model
//! that only looks prefixed (`llama3.1:8b`, `anthropic.claude-…-v1:0`) is
//! left alone: the prefix must be a preset name.
//!
//! Resolution, field by field: the environment, then the suite's
//! `config.ai`, then the preset's default.
//!
//! | Setting | Environment | `config.ai` |
//! | --- | --- | --- |
//! | provider | `ATALANTA_NL_PROVIDER`, or the prefix of `ATALANTA_NL_MODEL` | `provider`, or the prefix of `model` |
//! | step model | `ATALANTA_NL_MODEL` | `model` |
//! | goal model | `ATALANTA_NL_EXPLORE_MODEL` | `exploreModel` |
//! | endpoint | `ATALANTA_NL_API_URL`, then the preset's URL variable | `baseUrl` |
//! | key | the variable `apiKeyEnv` names, else the preset's | `apiKeyEnv` |
//! | screenshots | `ATALANTA_NL_IMAGES=0` | `images` |
//!
//! With no provider named, the first preset in [`PRESETS`] order whose key
//! is set is used (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, Azure's key and
//! endpoint, `GEMINI_API_KEY`/`GOOGLE_API_KEY`, then the OpenAI-compatible
//! services, then `AWS_BEARER_TOKEN_BEDROCK`), and the others found are
//! logged. Plain AWS or Google credentials never pick Bedrock or Vertex on
//! their own: those are named. `ATALANTA_NL_API_KEY` with
//! `ATALANTA_NL_API_URL` is a generic OpenAI-compatible endpoint. Keys are
//! only read from the environment, registered for redaction, and never
//! logged.
//!
//! Every default model is provisional: chosen for cost (a small model for
//! single-step picks), not yet by measured accuracy. `atalanta nl-eval
//! --model … --recommend` measures them.

use std::sync::Arc;

use atalanta_common::{AiConfig, AtalantaError, Result};

use crate::anthropic::AnthropicPlanner;
use crate::bedrock::{BedrockAuth, BedrockPlanner};
use crate::checker::ScreenChecker;
use crate::openai::{chat_endpoint, Auth, OpenAiOptions, OpenAiPlanner, OutputMode};
use crate::planner::NaturalPlanner;

/// Default Anthropic model for single steps and checks (provisional).
pub const ANTHROPIC_STEP_MODEL: &str = "claude-haiku-5-5";
/// Default Anthropic model for goal-level planning (provisional).
pub const ANTHROPIC_EXPLORE_MODEL: &str = "claude-sonnet-5-5";
/// Default OpenAI model for single steps and checks (provisional).
pub const OPENAI_STEP_MODEL: &str = "gpt-6-luna";
/// Default OpenAI model for goal-level planning (provisional).
pub const OPENAI_EXPLORE_MODEL: &str = "gpt-6-astra";
/// OpenAI's own endpoint.
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// The wire format and auth a preset speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// Anthropic Messages, `x-api-key`.
    Anthropic,
    /// OpenAI Chat Completions, bearer key (or none, for local servers).
    OpenAi,
    /// Azure OpenAI: Chat Completions under the resource endpoint,
    /// `api-key` header.
    Azure,
    /// Amazon Bedrock Converse, SigV4 or a Bedrock API key.
    Bedrock,
    /// Google Vertex AI: Claude through `rawPredict`, Gemini through the
    /// OpenAI-compatible endpoint, Google access tokens.
    Vertex,
}

/// What a provider's models can do. Each is a starting point: a model
/// that rejects a feature falls back (json_schema → tool call → JSON in
/// text; screenshots → the text screen list only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub images: bool,
    pub json_schema: bool,
    pub tools: bool,
}

/// A named provider.
#[derive(Debug, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// Shown in errors and docs.
    pub display: &'static str,
    pub wire: Wire,
    /// The API base (Anthropic: the Messages URL). Empty: must be given.
    pub base_url: &'static str,
    /// Variables that give the base URL, before the default.
    pub url_envs: &'static [&'static str],
    /// Variables that hold the key, first set wins.
    pub key_envs: &'static [&'static str],
    /// A key is required (local servers, Bedrock and Vertex need none).
    pub needs_key: bool,
    /// Picked automatically when its key is set (in [`PRESETS`] order).
    pub detect: bool,
    /// Default step model (provisional); empty: must be set.
    pub model: &'static str,
    /// Default goal-level model (provisional); empty: the step model.
    pub explore_model: &'static str,
    pub caps: Caps,
}

const ALL: Caps = Caps {
    images: true,
    json_schema: true,
    tools: true,
};
const TEXT_SCHEMA_TOOLS: Caps = Caps {
    images: false,
    json_schema: true,
    tools: true,
};

/// Every provider, in auto-detection order.
pub static PRESETS: &[Preset] = &[
    Preset {
        name: "anthropic",
        aliases: &["claude"],
        display: "Anthropic API",
        wire: Wire::Anthropic,
        base_url: "https://api.anthropic.com/v1/messages",
        url_envs: &[],
        key_envs: &["ANTHROPIC_API_KEY"],
        needs_key: true,
        detect: true,
        model: ANTHROPIC_STEP_MODEL,
        explore_model: ANTHROPIC_EXPLORE_MODEL,
        caps: ALL,
    },
    Preset {
        name: "openai",
        aliases: &[],
        display: "OpenAI API",
        wire: Wire::OpenAi,
        base_url: OPENAI_BASE_URL,
        url_envs: &["OPENAI_BASE_URL"],
        key_envs: &["OPENAI_API_KEY"],
        needs_key: true,
        detect: true,
        model: OPENAI_STEP_MODEL,
        explore_model: OPENAI_EXPLORE_MODEL,
        caps: ALL,
    },
    Preset {
        name: "azure-openai",
        aliases: &["azure"],
        display: "Azure OpenAI",
        wire: Wire::Azure,
        base_url: "",
        url_envs: &["AZURE_OPENAI_ENDPOINT"],
        key_envs: &["AZURE_OPENAI_API_KEY"],
        needs_key: true,
        detect: true,
        // The deployment name (AZURE_OPENAI_DEPLOYMENT).
        model: "",
        explore_model: "",
        caps: ALL,
    },
    Preset {
        name: "gemini",
        aliases: &["google"],
        display: "Gemini API",
        wire: Wire::OpenAi,
        // Google's OpenAI-compatible endpoint takes json_schema output,
        // image_url data URIs and tools, so the native API isn't needed.
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        url_envs: &[],
        key_envs: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        needs_key: true,
        detect: true,
        model: "gemini-3.8-flash",
        explore_model: "gemini-3.1-pro-preview",
        caps: ALL,
    },
    Preset {
        name: "openrouter",
        aliases: &[],
        display: "OpenRouter",
        wire: Wire::OpenAi,
        base_url: "https://openrouter.ai/api/v1",
        url_envs: &[],
        key_envs: &["OPENROUTER_API_KEY"],
        needs_key: true,
        detect: true,
        model: "anthropic/claude-haiku-5-5",
        explore_model: "anthropic/claude-sonnet-5-5",
        caps: ALL,
    },
    Preset {
        name: "groq",
        aliases: &[],
        display: "Groq",
        wire: Wire::OpenAi,
        base_url: "https://api.groq.com/openai/v1",
        url_envs: &[],
        key_envs: &["GROQ_API_KEY"],
        needs_key: true,
        detect: true,
        model: "llama-3.3-70b-versatile",
        explore_model: "openai/gpt-oss-120b",
        caps: Caps {
            images: false,
            json_schema: false,
            tools: true,
        },
    },
    Preset {
        name: "mistral",
        aliases: &[],
        display: "Mistral AI",
        wire: Wire::OpenAi,
        base_url: "https://api.mistral.ai/v1",
        url_envs: &[],
        key_envs: &["MISTRAL_API_KEY"],
        needs_key: true,
        detect: true,
        model: "mistral-small-latest",
        explore_model: "mistral-large-latest",
        caps: ALL,
    },
    Preset {
        name: "deepseek",
        aliases: &[],
        display: "DeepSeek",
        wire: Wire::OpenAi,
        base_url: "https://api.deepseek.com/v1",
        url_envs: &[],
        key_envs: &["DEEPSEEK_API_KEY"],
        needs_key: true,
        detect: true,
        model: "deepseek-chat",
        explore_model: "deepseek-reasoner",
        caps: Caps {
            images: false,
            json_schema: false,
            tools: true,
        },
    },
    Preset {
        name: "xai",
        aliases: &["grok"],
        display: "xAI",
        wire: Wire::OpenAi,
        base_url: "https://api.x.ai/v1",
        url_envs: &[],
        key_envs: &["XAI_API_KEY"],
        needs_key: true,
        detect: true,
        model: "grok-4-fast",
        explore_model: "grok-4",
        caps: ALL,
    },
    Preset {
        name: "together",
        aliases: &[],
        display: "Together AI",
        wire: Wire::OpenAi,
        base_url: "https://api.together.xyz/v1",
        url_envs: &[],
        key_envs: &["TOGETHER_API_KEY"],
        needs_key: true,
        detect: true,
        model: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        explore_model: "",
        caps: TEXT_SCHEMA_TOOLS,
    },
    Preset {
        name: "fireworks",
        aliases: &[],
        display: "Fireworks AI",
        wire: Wire::OpenAi,
        base_url: "https://api.fireworks.ai/inference/v1",
        url_envs: &[],
        key_envs: &["FIREWORKS_API_KEY"],
        needs_key: true,
        detect: true,
        model: "accounts/fireworks/models/llama-v3p3-70b-instruct",
        explore_model: "",
        caps: TEXT_SCHEMA_TOOLS,
    },
    Preset {
        name: "dashscope",
        aliases: &["qwen"],
        display: "Alibaba DashScope (Qwen)",
        wire: Wire::OpenAi,
        base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        url_envs: &[],
        key_envs: &["DASHSCOPE_API_KEY"],
        needs_key: true,
        detect: true,
        model: "qwen-plus",
        explore_model: "qwen-max",
        caps: Caps {
            images: false,
            json_schema: false,
            tools: true,
        },
    },
    Preset {
        name: "perplexity",
        aliases: &[],
        display: "Perplexity",
        wire: Wire::OpenAi,
        base_url: "https://api.perplexity.ai",
        url_envs: &[],
        key_envs: &["PERPLEXITY_API_KEY"],
        needs_key: true,
        detect: true,
        model: "sonar",
        explore_model: "sonar-pro",
        caps: Caps {
            images: false,
            json_schema: true,
            tools: false,
        },
    },
    Preset {
        name: "bedrock",
        aliases: &["aws", "aws-bedrock"],
        display: "Amazon Bedrock",
        wire: Wire::Bedrock,
        base_url: "",
        url_envs: &[],
        // Only the Bedrock API key picks Bedrock on its own; SigV4
        // credentials are used once the provider is named.
        key_envs: &["AWS_BEARER_TOKEN_BEDROCK"],
        needs_key: false,
        detect: true,
        model: "anthropic.claude-haiku-5-5",
        explore_model: "anthropic.claude-sonnet-5-5",
        caps: Caps {
            images: true,
            json_schema: false,
            tools: true,
        },
    },
    Preset {
        name: "vertex",
        aliases: &["vertex-ai", "google-vertex"],
        display: "Google Vertex AI",
        wire: Wire::Vertex,
        base_url: "",
        url_envs: &[],
        key_envs: &[],
        needs_key: false,
        detect: false,
        model: ANTHROPIC_STEP_MODEL,
        explore_model: ANTHROPIC_EXPLORE_MODEL,
        caps: Caps {
            images: true,
            json_schema: false,
            tools: false,
        },
    },
    Preset {
        name: "ollama",
        aliases: &[],
        display: "Ollama",
        wire: Wire::OpenAi,
        base_url: "http://localhost:11434/v1",
        url_envs: &[],
        key_envs: &["OLLAMA_API_KEY"],
        needs_key: false,
        detect: false,
        model: "llama3.2",
        explore_model: "",
        caps: TEXT_SCHEMA_TOOLS,
    },
    Preset {
        name: "lmstudio",
        aliases: &["lm-studio"],
        display: "LM Studio",
        wire: Wire::OpenAi,
        base_url: "http://localhost:1234/v1",
        url_envs: &[],
        key_envs: &["LMSTUDIO_API_KEY"],
        needs_key: false,
        detect: false,
        // Whatever model is loaded: name it.
        model: "",
        explore_model: "",
        caps: TEXT_SCHEMA_TOOLS,
    },
    Preset {
        name: "vllm",
        aliases: &[],
        display: "vLLM",
        wire: Wire::OpenAi,
        base_url: "",
        url_envs: &["VLLM_BASE_URL"],
        key_envs: &["VLLM_API_KEY"],
        needs_key: false,
        detect: false,
        model: "",
        explore_model: "",
        caps: Caps {
            images: false,
            json_schema: true,
            tools: false,
        },
    },
    Preset {
        name: "openai-compatible",
        aliases: &["openai_compatible", "custom"],
        display: "OpenAI-compatible API",
        wire: Wire::OpenAi,
        base_url: "",
        url_envs: &[],
        key_envs: &["ATALANTA_NL_API_KEY"],
        needs_key: false,
        detect: false,
        model: "",
        explore_model: "",
        caps: ALL,
    },
];

/// The preset called `name` (or one of its aliases), case-insensitive.
pub fn find_preset(name: &str) -> Option<&'static Preset> {
    let name = name.trim().to_ascii_lowercase();
    PRESETS
        .iter()
        .find(|p| p.name == name || p.aliases.contains(&name.as_str()))
}

/// Every preset name, for the schema and errors.
pub fn preset_names() -> Vec<&'static str> {
    PRESETS.iter().map(|p| p.name).collect()
}

fn parse_preset(name: &str) -> Result<&'static Preset> {
    find_preset(name).ok_or_else(|| {
        AtalantaError::Config(format!(
            "AI provider {name:?} is not one of {}",
            preset_names().join(", ")
        ))
    })
}

/// `provider:model` split when the prefix is a preset; otherwise no
/// provider and the whole string (`llama3.1:8b` stays a model).
pub fn split_model(s: &str) -> (Option<&'static Preset>, &str) {
    match s.split_once(':') {
        Some((p, m)) if !m.is_empty() => match find_preset(p) {
            Some(preset) => (Some(preset), m),
            None => (None, s),
        },
        _ => (None, s),
    }
}

/// A key held in memory: registered for redaction, never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    fn new(value: String) -> Self {
        atalanta_common::redact::register_secret(&value);
        Self(value)
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// The resolved model settings: which preset, which models, where, and
/// the key (by variable name; the value is held redacted). Public so
/// `atalanta explore` builds its goal planner
/// ([`explore_planner`](Self::explore_planner)) and checker from the same
/// place as `atalanta run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiSettings {
    pub preset: &'static Preset,
    /// Single-step resolution and `verify:` checks.
    pub model: String,
    /// Goal-level planning (`atalanta explore`).
    pub explore_model: String,
    /// The endpoint (Anthropic: the Messages URL; OpenAI-style: the API
    /// base; Bedrock and Vertex: an origin override). `None`: the preset's.
    pub base_url: Option<String>,
    /// The variable the key was read from (never the key).
    pub api_key_env: Option<String>,
    api_key: Option<Secret>,
    /// Send screenshots to checks.
    pub images: bool,
    /// Bedrock region / Vertex location, when given.
    pub region: Option<String>,
    /// Vertex project, when given.
    pub project: Option<String>,
    /// Azure `api-version`, when given.
    pub api_version: Option<String>,
}

/// The environment, as a lookup.
type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

impl AiSettings {
    /// Settings from the environment and `config` (the suite's `config.ai`).
    /// `Ok(None)` when no provider can be used (no key set, or a named
    /// provider whose key is missing): not an error, since a committed
    /// cache still replays. A bad provider name, model or `apiKeyEnv` is a
    /// configuration error.
    pub fn resolve(config: Option<&AiConfig>) -> Result<Option<Self>> {
        Self::resolve_with(config, &process_env)
    }

    /// [`resolve`](Self::resolve) from the environment alone.
    pub fn from_env() -> Result<Option<Self>> {
        Self::resolve(None)
    }

    /// [`resolve`](Self::resolve) with the environment passed in (tests).
    pub fn resolve_with(config: Option<&AiConfig>, env: Env<'_>) -> Result<Option<Self>> {
        let empty = AiConfig::default();
        let cfg = config.unwrap_or(&empty);
        let get = |k: &str| env(k).filter(|v| !v.trim().is_empty());
        if let Some(name) = &cfg.api_key_env {
            check_env_name(name)?;
        }
        let env_model = get("ATALANTA_NL_MODEL");
        let (env_model_preset, _) = env_model.as_deref().map(split_model).unwrap_or((None, ""));
        let (cfg_model_preset, _) = cfg.model.as_deref().map(split_model).unwrap_or((None, ""));
        let env_named = match get("ATALANTA_NL_PROVIDER") {
            Some(p) => Some(parse_preset(&p)?),
            None => env_model_preset,
        };
        let cfg_named = match &cfg.provider {
            Some(p) => Some(parse_preset(p)?),
            None => cfg_model_preset,
        };
        let preset = match env_named.or(cfg_named) {
            Some(p) => p,
            None => match cfg.api_key_env.as_deref() {
                Some(var) => PRESETS
                    .iter()
                    .find(|p| p.key_envs.contains(&var))
                    .unwrap_or_else(|| find_preset("openai-compatible").expect("preset")),
                None => match detect(&get) {
                    Some(p) => p,
                    None => return Ok(None),
                },
            },
        };
        // The suite's settings belong to its provider: when the
        // environment switched to another one, they don't apply.
        let cfg_applies = cfg_named.is_none_or(|p| std::ptr::eq(p, preset));
        let cfg_field = |f: &Option<String>| if cfg_applies { f.clone() } else { None };

        let api_key_env = cfg_field(&cfg.api_key_env)
            .or_else(|| {
                preset
                    .key_envs
                    .iter()
                    .find(|k| get(k).is_some())
                    .map(|k| k.to_string())
            })
            .or_else(|| preset.key_envs.first().map(|k| k.to_string()));
        let api_key = api_key_env.as_deref().and_then(&get).map(Secret::new);
        let base_url = get("ATALANTA_NL_API_URL")
            .or_else(|| cfg_field(&cfg.base_url))
            .or_else(|| preset.url_envs.iter().find_map(|k| get(k)));
        if preset.needs_key && api_key.is_none() {
            tracing::warn!(
                "atalanta: the {} provider needs ${}; natural-language steps will only replay",
                preset.name,
                api_key_env.as_deref().unwrap_or("its key")
            );
            return Ok(None);
        }
        if preset.wire == Wire::Azure && base_url.is_none() {
            return Err(AtalantaError::Config(
                "azure-openai needs AZURE_OPENAI_ENDPOINT (or config.ai.baseUrl)".into(),
            ));
        }
        if matches!(preset.wire, Wire::OpenAi) && preset.base_url.is_empty() && base_url.is_none() {
            return Err(AtalantaError::Config(format!(
                "the {} provider needs a base URL: set ATALANTA_NL_API_URL or config.ai.baseUrl",
                preset.name
            )));
        }
        // Only the chosen provider's own prefix goes: `mistral:7b` is an
        // Ollama model, not `7b` on Mistral's name.
        let strip = |m: String| match split_model(&m) {
            (Some(p), rest) if std::ptr::eq(p, preset) => rest.to_string(),
            _ => m,
        };
        let model = env_model
            .map(strip)
            .or_else(|| cfg_field(&cfg.model).map(strip))
            .or_else(|| (preset.wire == Wire::Azure).then(|| get("AZURE_OPENAI_DEPLOYMENT")).flatten())
            .or_else(|| (!preset.model.is_empty()).then(|| preset.model.to_string()))
            .ok_or_else(|| {
                AtalantaError::Config(format!(
                    "the {} provider has no default model: set ATALANTA_NL_MODEL or config.ai.model{}",
                    preset.name,
                    if preset.wire == Wire::Azure {
                        " (the deployment name), or AZURE_OPENAI_DEPLOYMENT"
                    } else {
                        ""
                    }
                ))
            })?;
        let explore_model = get("ATALANTA_NL_EXPLORE_MODEL")
            .map(strip)
            .or_else(|| cfg_field(&cfg.explore_model).map(strip))
            .or_else(|| {
                (!preset.explore_model.is_empty()).then(|| preset.explore_model.to_string())
            })
            .unwrap_or_else(|| model.clone());
        let images = match get("ATALANTA_NL_IMAGES").as_deref() {
            Some(v) => !matches!(v.trim(), "0" | "false" | "off" | "no"),
            None => cfg
                .images
                .filter(|_| cfg_applies)
                .unwrap_or(preset.caps.images),
        };
        let region = cfg_field(&cfg.region).or_else(|| match preset.wire {
            Wire::Vertex => [
                "GOOGLE_CLOUD_LOCATION",
                "VERTEX_LOCATION",
                "CLOUD_ML_REGION",
            ]
            .iter()
            .find_map(|k| get(k)),
            Wire::Bedrock => get("AWS_REGION").or_else(|| get("AWS_DEFAULT_REGION")),
            _ => None,
        });
        let project = cfg_field(&cfg.project).or_else(|| match preset.wire {
            Wire::Vertex => [
                "GOOGLE_CLOUD_PROJECT",
                "VERTEX_PROJECT",
                "ANTHROPIC_VERTEX_PROJECT_ID",
                "CLOUDSDK_CORE_PROJECT",
            ]
            .iter()
            .find_map(|k| get(k)),
            _ => None,
        });
        let api_version = cfg_field(&cfg.api_version).or_else(|| get("AZURE_OPENAI_API_VERSION"));
        Ok(Some(Self {
            preset,
            model,
            explore_model,
            base_url,
            api_key_env,
            api_key,
            images,
            region,
            project,
            api_version,
        }))
    }

    /// Settings for an explicit `provider:model[@base-url]` (`nl-eval
    /// --model`): the provider's key from the environment, nothing else
    /// from `ATALANTA_NL_*`.
    pub fn for_spec(spec: &str) -> Result<Self> {
        Self::for_spec_with(spec, &process_env)
    }

    /// [`for_spec`](Self::for_spec) with the environment passed in.
    pub fn for_spec_with(spec: &str, env: Env<'_>) -> Result<Self> {
        let (head, url) = match spec.split_once('@') {
            Some((h, u)) => (h, Some(u.to_string())),
            None => (spec, None),
        };
        let (preset, model) = split_model(head);
        let Some(preset) = preset else {
            return Err(AtalantaError::Config(format!(
                "--model {spec:?} must be provider:model (providers: {})",
                preset_names().join(", ")
            )));
        };
        let cfg = AiConfig {
            provider: Some(preset.name.into()),
            model: Some(model.to_string()),
            base_url: url,
            ..Default::default()
        };
        // The environment's endpoint is for its own provider, so it is
        // hidden from another one. It still applies when the environment
        // names this provider, or when this provider has no endpoint of its
        // own (`openai-compatible`): hidden there, `--model
        // openai-compatible:m` could never work, and the error told the user
        // to set the very variable it ignored (AndroidWorld rerun,
        // 2026-11-07).
        let env_provider = env("ATALANTA_NL_PROVIDER")
            .and_then(|p| find_preset(&p))
            .is_some_and(|p| std::ptr::eq(p, preset));
        // An inline `@url` always wins.
        let keep_url = cfg.base_url.is_none() && (preset.base_url.is_empty() || env_provider);
        let hide = [
            "ATALANTA_NL_PROVIDER",
            "ATALANTA_NL_MODEL",
            "ATALANTA_NL_EXPLORE_MODEL",
            "ATALANTA_NL_API_URL",
        ];
        let filtered = |k: &str| {
            if hide.contains(&k) && !(keep_url && k == "ATALANTA_NL_API_URL") {
                None
            } else {
                env(k)
            }
        };
        Self::resolve_with(Some(&cfg), &filtered)?.ok_or_else(|| {
            AtalantaError::Config(format!(
                "--model {spec}: the {} provider needs ${}",
                preset.name,
                preset.key_envs.first().copied().unwrap_or("its key")
            ))
        })
    }

    /// The same settings with another step model.
    pub fn with_model(&self, model: &str) -> Self {
        Self {
            model: model.to_string(),
            ..self.clone()
        }
    }

    /// `provider:model` (and `@<url>` for a custom endpoint).
    pub fn label(&self) -> String {
        match &self.base_url {
            Some(url) => format!("{}:{}@{url}", self.preset.name, self.model),
            None => format!("{}:{}", self.preset.name, self.model),
        }
    }

    /// The single-step planner (`model`).
    pub fn planner(&self) -> Result<Box<dyn NaturalPlanner>> {
        self.planner_for(&self.model, &process_env)
    }

    /// The goal-level planner (`explore_model`) for `atalanta explore`.
    pub fn explore_planner(&self) -> Result<Box<dyn NaturalPlanner>> {
        self.planner_for(&self.explore_model, &process_env)
    }

    /// The `verify:` checker (`model`).
    pub fn checker(&self) -> Result<Box<dyn ScreenChecker>> {
        self.checker_with(&process_env)
    }

    /// [`checker`](Self::checker) with the environment passed in.
    pub fn checker_with(&self, env: Env<'_>) -> Result<Box<dyn ScreenChecker>> {
        Ok(match self.backend(&self.model, env)? {
            Backend::Anthropic(p) => Box::new(p),
            Backend::OpenAi(p) => Box::new(p),
            Backend::Bedrock(p) => Box::new(p),
        })
    }

    /// The planner for `model` with the environment passed in (tests).
    pub fn planner_for(&self, model: &str, env: Env<'_>) -> Result<Box<dyn NaturalPlanner>> {
        Ok(match self.backend(model, env)? {
            Backend::Anthropic(p) => Box::new(p),
            Backend::OpenAi(p) => Box::new(p),
            Backend::Bedrock(p) => Box::new(p),
        })
    }

    fn key(&self) -> Result<String> {
        self.api_key
            .as_ref()
            .map(|k| k.expose().to_string())
            .ok_or_else(|| {
                AtalantaError::Config(format!(
                    "the {} provider needs its key in ${}",
                    self.preset.name,
                    self.api_key_env.as_deref().unwrap_or("?")
                ))
            })
    }

    fn output_mode(&self, env: Env<'_>) -> OutputMode {
        match env("ATALANTA_NL_OUTPUT").as_deref() {
            Some("tools") | Some("tool") | Some("tool_call") => OutputMode::ToolCall,
            Some("text") => OutputMode::Text,
            Some("json_schema") => OutputMode::JsonSchema,
            _ if self.preset.caps.json_schema => OutputMode::JsonSchema,
            _ if self.preset.caps.tools => OutputMode::ToolCall,
            _ => OutputMode::Text,
        }
    }

    fn backend(&self, model: &str, env: Env<'_>) -> Result<Backend> {
        let p = self.preset;
        match p.wire {
            Wire::Anthropic => Ok(Backend::Anthropic(AnthropicPlanner::new(
                self.key()?,
                model.to_string(),
                self.base_url
                    .clone()
                    .or_else(|| Some(p.base_url.to_string())),
            )?)),
            Wire::OpenAi => {
                let base = self
                    .base_url
                    .clone()
                    .unwrap_or_else(|| p.base_url.to_string());
                let mut headers = Vec::new();
                if p.name == "openai" {
                    if let Some(org) = env("OPENAI_ORG_ID").filter(|v| !v.is_empty()) {
                        headers.push(("OpenAI-Organization".to_string(), org));
                    }
                }
                Ok(Backend::OpenAi(OpenAiPlanner::with_options(
                    OpenAiOptions {
                        endpoint: chat_endpoint(&base),
                        auth: match &self.api_key {
                            Some(k) => Auth::Bearer(k.expose().to_string()),
                            None => Auth::None,
                        },
                        headers,
                        model: model.to_string(),
                        mode: self.output_mode(env),
                        tools: p.caps.tools,
                        images: self.images,
                        service: p.display.to_string(),
                    },
                )?))
            }
            Wire::Azure => {
                let endpoint = azure_endpoint(
                    self.base_url.as_deref().unwrap_or_default(),
                    model,
                    self.api_version.as_deref(),
                );
                Ok(Backend::OpenAi(OpenAiPlanner::with_options(
                    OpenAiOptions {
                        endpoint,
                        auth: Auth::Header("api-key".into(), self.key()?),
                        headers: vec![],
                        model: model.to_string(),
                        mode: self.output_mode(env),
                        tools: p.caps.tools,
                        images: self.images,
                        service: p.display.to_string(),
                    },
                )?))
            }
            Wire::Bedrock => {
                let (auth, chain_region) =
                    match env("AWS_BEARER_TOKEN_BEDROCK").filter(|v| !v.is_empty()) {
                        Some(token) => (BedrockAuth::Bearer(Secret::new(token).0), None),
                        None => {
                            let (creds, region) = crate::sigv4::credentials_from(env)?;
                            (BedrockAuth::SigV4(creds), region)
                        }
                    };
                let region = self
                    .region
                    .clone()
                    .or(chain_region)
                    .or_else(|| env("AWS_REGION"))
                    .unwrap_or_default();
                Ok(Backend::Bedrock(BedrockPlanner::new(
                    &region,
                    model,
                    auth,
                    self.base_url.clone(),
                    self.images,
                )?))
            }
            Wire::Vertex => {
                let (token, sa_project) = crate::gcp::GoogleToken::discover(env)?;
                let token: Arc<dyn crate::gcp::TokenSource> = Arc::new(token);
                let project = self.project.clone().or(sa_project).ok_or_else(|| {
                    AtalantaError::Config(
                        "vertex needs a project: set GOOGLE_CLOUD_PROJECT or config.ai.project"
                            .into(),
                    )
                })?;
                let region = self.region.clone().unwrap_or_else(|| "global".into());
                let origin = self
                    .base_url
                    .clone()
                    .unwrap_or_else(|| vertex_origin(&region));
                let origin = origin.trim_end_matches('/');
                let base = format!("{origin}/v1/projects/{project}/locations/{region}");
                if model.starts_with("claude") {
                    Ok(Backend::Anthropic(AnthropicPlanner::vertex(
                        format!("{base}/publishers/anthropic/models/{model}:rawPredict"),
                        token,
                        model.to_string(),
                    )?))
                } else {
                    let model = if model.contains('/') {
                        model.to_string()
                    } else {
                        format!("google/{model}")
                    };
                    Ok(Backend::OpenAi(OpenAiPlanner::with_options(
                        OpenAiOptions {
                            endpoint: format!("{base}/endpoints/openapi/chat/completions"),
                            auth: Auth::Token(token),
                            headers: vec![],
                            model,
                            // Gemini on Vertex's OpenAI endpoint.
                            mode: OutputMode::JsonSchema,
                            tools: true,
                            images: self.images,
                            service: p.display.to_string(),
                        },
                    )?))
                }
            }
        }
    }
}

enum Backend {
    Anthropic(AnthropicPlanner),
    OpenAi(OpenAiPlanner),
    Bedrock(BedrockPlanner),
}

fn process_env(k: &str) -> Option<String> {
    atalanta_common::env::var(k).ok()
}

/// The first preset whose key is set (Azure also needs its endpoint);
/// the others found are logged.
fn detect(get: &dyn Fn(&str) -> Option<String>) -> Option<&'static Preset> {
    let found: Vec<&'static Preset> = PRESETS
        .iter()
        .filter(|p| p.detect)
        .filter(|p| p.key_envs.iter().any(|k| get(k).is_some()))
        .filter(|p| p.wire != Wire::Azure || p.url_envs.iter().any(|k| get(k).is_some()))
        .collect();
    if let Some(first) = found.first() {
        if found.len() > 1 {
            let others: Vec<&str> = found[1..].iter().map(|p| p.name).collect();
            tracing::info!(
                "atalanta: using the {} provider (first key found); also set: {} \
                 (choose with ATALANTA_NL_PROVIDER)",
                first.name,
                others.join(", ")
            );
        }
        return Some(first);
    }
    // A generic key with an endpoint: an OpenAI-compatible service.
    (get("ATALANTA_NL_API_KEY").is_some() && get("ATALANTA_NL_API_URL").is_some())
        .then(|| find_preset("openai-compatible"))
        .flatten()
}

/// Azure OpenAI's chat URL: the deployments path with `api-version` when
/// one is given, else the version-less `/openai/v1` path (model in the
/// body).
pub fn azure_endpoint(resource: &str, deployment: &str, api_version: Option<&str>) -> String {
    let base = resource.trim().trim_end_matches('/');
    let base = base
        .strip_suffix("/openai/v1")
        .or_else(|| base.strip_suffix("/openai"))
        .unwrap_or(base);
    match api_version {
        Some(v) => format!(
            "{base}/openai/deployments/{}/chat/completions?api-version={}",
            crate::sigv4::uri_encode(deployment),
            crate::sigv4::uri_encode(v)
        ),
        None => format!("{base}/openai/v1/chat/completions"),
    }
}

/// Vertex's host for a location (`global` has no regional prefix).
fn vertex_origin(region: &str) -> String {
    if region == "global" {
        "https://aiplatform.googleapis.com".into()
    } else {
        format!("https://{region}-aiplatform.googleapis.com")
    }
}

/// `apiKeyEnv` must name a variable. A value that looks like a key itself
/// is refused without echoing it: keys never go in the suite.
fn check_env_name(name: &str) -> Result<()> {
    let valid = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(AtalantaError::Config(
            "config.ai.apiKeyEnv must be the NAME of an environment variable (like OPENAI_API_KEY), \
             not a key; never write a key into the suite"
                .into(),
        ))
    }
}

/// A `--model` entry for `atalanta nl-eval` (see [`AiSettings::for_spec`]).
pub fn parse_model_spec(spec: &str) -> Result<AiSettings> {
    AiSettings::for_spec(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{Op, PlanRequest};
    use crate::testutil::serve;

    /// The schema and validator read the preset names from
    /// `atalanta-common` (core does not depend on this crate).
    #[test]
    fn the_shared_preset_list_matches_the_presets() {
        assert_eq!(
            atalanta_common::natural::AI_PROVIDER_PRESETS,
            preset_names().as_slice()
        );
    }
    use atalanta_common::{NaturalKind, Platform};
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn resolve(cfg: Option<&AiConfig>, pairs: &[(&str, &str)]) -> Option<AiSettings> {
        AiSettings::resolve_with(cfg, &env_of(pairs)).unwrap()
    }

    fn name(s: &Option<AiSettings>) -> &'static str {
        s.as_ref().map_or("none", |s| s.preset.name)
    }

    // A model tag that starts with another preset's name (Ollama's
    // `mistral:7b`, `qwen:7b`) is the model, not a `provider:model` pair,
    // when the provider is named some other way: the prefix was stripped and
    // `7b` was asked for.
    #[test]
    fn a_model_tag_that_looks_like_another_provider_prefix_is_kept() {
        let s = resolve(
            None,
            &[
                ("ATALANTA_NL_PROVIDER", "ollama"),
                ("ATALANTA_NL_MODEL", "mistral:7b"),
            ],
        )
        .unwrap();
        assert_eq!((s.preset.name, s.model.as_str()), ("ollama", "mistral:7b"));
        let cfg = AiConfig {
            provider: Some("ollama".into()),
            model: Some("qwen:7b".into()),
            explore_model: Some("mistral:latest".into()),
            ..Default::default()
        };
        let s = resolve(Some(&cfg), &[]).unwrap();
        assert_eq!(s.model, "qwen:7b");
        assert_eq!(s.explore_model, "mistral:latest");
        // `nl-eval --model ollama:mistral:7b`.
        let s = AiSettings::for_spec_with("ollama:mistral:7b", &env_of(&[])).unwrap();
        assert_eq!((s.preset.name, s.model.as_str()), ("ollama", "mistral:7b"));
        // The prefix of the provider actually chosen still goes.
        let s = resolve(
            None,
            &[
                ("ATALANTA_NL_PROVIDER", "ollama"),
                ("ATALANTA_NL_MODEL", "ollama:mistral:7b"),
            ],
        )
        .unwrap();
        assert_eq!(s.model, "mistral:7b");
    }

    #[test]
    fn the_first_key_in_preset_order_picks_the_provider() {
        assert_eq!(name(&resolve(None, &[])), "none");
        for (key, want) in [
            ("ANTHROPIC_API_KEY", "anthropic"),
            ("OPENAI_API_KEY", "openai"),
            ("GEMINI_API_KEY", "gemini"),
            ("GOOGLE_API_KEY", "gemini"),
            ("OPENROUTER_API_KEY", "openrouter"),
            ("GROQ_API_KEY", "groq"),
            ("MISTRAL_API_KEY", "mistral"),
            ("DEEPSEEK_API_KEY", "deepseek"),
            ("XAI_API_KEY", "xai"),
            ("TOGETHER_API_KEY", "together"),
            ("FIREWORKS_API_KEY", "fireworks"),
            ("DASHSCOPE_API_KEY", "dashscope"),
            ("PERPLEXITY_API_KEY", "perplexity"),
            ("AWS_BEARER_TOKEN_BEDROCK", "bedrock"),
        ] {
            assert_eq!(name(&resolve(None, &[(key, "k")])), want, "{key}");
        }
        // Order decides when several are set (and the rest are logged).
        let both = resolve(None, &[("GROQ_API_KEY", "g"), ("OPENAI_API_KEY", "o")]);
        assert_eq!(name(&both), "openai");
        // Azure needs its endpoint too; plain AWS/Google credentials never
        // pick Bedrock or Vertex.
        assert_eq!(
            name(&resolve(None, &[("AZURE_OPENAI_API_KEY", "k")])),
            "none"
        );
        let azure = resolve(
            None,
            &[
                ("AZURE_OPENAI_API_KEY", "k"),
                ("AZURE_OPENAI_ENDPOINT", "https://r.openai.azure.com"),
                ("AZURE_OPENAI_DEPLOYMENT", "my-gpt"),
            ],
        );
        assert_eq!(name(&azure), "azure-openai");
        assert_eq!(azure.unwrap().model, "my-gpt");
        assert_eq!(
            name(&resolve(
                None,
                &[("AWS_ACCESS_KEY_ID", "a"), ("AWS_SECRET_ACCESS_KEY", "b")]
            )),
            "none"
        );
        assert_eq!(
            name(&resolve(None, &[("GOOGLE_APPLICATION_CREDENTIALS", "/x")])),
            "none"
        );
        // A generic key with an endpoint.
        let g = resolve(
            None,
            &[
                ("ATALANTA_NL_API_KEY", "k"),
                ("ATALANTA_NL_API_URL", "https://x.example/v1"),
                ("ATALANTA_NL_MODEL", "m"),
            ],
        );
        assert_eq!(name(&g), "openai-compatible");
        assert_eq!(
            name(&resolve(None, &[("ATALANTA_NL_API_KEY", "k")])),
            "none"
        );
    }

    #[test]
    fn provider_model_strings_work_everywhere() {
        let s = resolve(
            None,
            &[
                ("ATALANTA_NL_MODEL", "groq:llama-3.1-8b-instant"),
                ("GROQ_API_KEY", "g"),
                ("ANTHROPIC_API_KEY", "a"),
            ],
        )
        .unwrap();
        assert_eq!(
            (s.preset.name, s.model.as_str()),
            ("groq", "llama-3.1-8b-instant")
        );
        let cfg = AiConfig {
            model: Some("ollama:llama3.1:8b".into()),
            ..Default::default()
        };
        let s = resolve(Some(&cfg), &[]).unwrap();
        assert_eq!((s.preset.name, s.model.as_str()), ("ollama", "llama3.1:8b"));
        assert_eq!(split_model("llama3.1:8b"), (None, "llama3.1:8b"));
        assert_eq!(split_model("anthropic.claude-haiku-5-5-v1:0").0, None);
        assert_eq!(
            split_model("claude:claude-haiku-5-5").0.unwrap().name,
            "anthropic"
        );
        // The environment switched provider: the suite's model doesn't apply.
        let cfg = AiConfig {
            provider: Some("openai".into()),
            model: Some("gpt-x".into()),
            ..Default::default()
        };
        let s = resolve(
            Some(&cfg),
            &[
                ("ATALANTA_NL_PROVIDER", "mistral"),
                ("MISTRAL_API_KEY", "m"),
            ],
        )
        .unwrap();
        assert_eq!(
            (s.preset.name, s.model.as_str()),
            ("mistral", "mistral-small-latest")
        );
        assert_eq!(s.explore_model, "mistral-large-latest");
    }

    #[test]
    fn named_providers_need_their_settings() {
        // Named but no key: replay only, not an error.
        assert!(resolve(
            Some(&AiConfig {
                provider: Some("anthropic".into()),
                ..Default::default()
            }),
            &[]
        )
        .is_none());
        let err = |cfg: AiConfig, pairs: &[(&str, &str)]| {
            AiSettings::resolve_with(Some(&cfg), &env_of(pairs)).unwrap_err()
        };
        let e = err(
            AiConfig {
                provider: Some("cohere".into()),
                ..Default::default()
            },
            &[],
        );
        assert!(e.to_string().contains("openrouter"), "{e}");
        let e = err(
            AiConfig {
                provider: Some("vllm".into()),
                ..Default::default()
            },
            &[],
        );
        assert!(e.to_string().contains("base URL"), "{e}");
        let e = err(
            AiConfig {
                provider: Some("lmstudio".into()),
                ..Default::default()
            },
            &[],
        );
        assert!(e.to_string().contains("no default model"), "{e}");
        let e = err(
            AiConfig {
                provider: Some("azure-openai".into()),
                ..Default::default()
            },
            &[("AZURE_OPENAI_API_KEY", "k")],
        );
        assert!(e.to_string().contains("AZURE_OPENAI_ENDPOINT"), "{e}");
        // apiKeyEnv names a variable; a key there is refused unechoed.
        let e = err(
            AiConfig {
                api_key_env: Some("sk-proj-abc/def".into()),
                ..Default::default()
            },
            &[],
        );
        assert_eq!(e.exit_code(), 2);
        assert!(!e.to_string().contains("sk-proj"));
        let s = resolve(
            Some(&AiConfig {
                api_key_env: Some("MY_GROQ".into()),
                provider: Some("groq".into()),
                ..Default::default()
            }),
            &[("MY_GROQ", "gsk-T1-secret")],
        )
        .unwrap();
        assert_eq!(s.api_key_env.as_deref(), Some("MY_GROQ"));
        // The key is held redacted and never printed.
        assert!(!format!("{s:?}").contains("gsk-T1-secret"));
        assert!(!atalanta_common::redact::redact("x gsk-T1-secret").contains("gsk-T1-secret"));
    }

    fn req() -> PlanRequest<'static> {
        PlanRequest {
            intent: "tap login",
            kind: NaturalKind::Act,
            data: None,
            wait_until: None,
            platform: Platform::Web,
            screen: "[0] <button> text=\"Login\" @1,1",
            feedback: None,
        }
    }

    const TAP0: &str =
        r#"{"op":"tap","element":0,"text":"","direction":"none","key":"none","ms":0}"#;

    fn completion(content: &str) -> String {
        serde_json::json!({ "choices": [{ "finish_reason": "stop", "message": { "content": content } }] }).to_string()
    }

    fn tool_completion() -> String {
        serde_json::json!({ "choices": [{ "finish_reason": "tool_calls", "message": { "content": null,
            "tool_calls": [{ "type": "function", "function": { "name": "ui_step", "arguments": TAP0 } }] } }] })
        .to_string()
    }

    // Every OpenAI-style preset: its default endpoint, and against a mock
    // (base URL overridden) the path, auth header, model and output mode
    // its capabilities call for.
    #[tokio::test]
    async fn every_openai_style_preset_sends_the_right_request() {
        for p in PRESETS.iter().filter(|p| p.wire == Wire::OpenAi) {
            let key_env = p.key_envs.first().copied();
            let mut pairs: Vec<(&str, &str)> = vec![];
            if let Some(k) = key_env {
                pairs.push((k, "test-key"));
            }
            if p.model.is_empty() {
                pairs.push(("ATALANTA_NL_MODEL", "some-model"));
            }
            if p.base_url.is_empty() && p.url_envs.is_empty() {
                pairs.push(("ATALANTA_NL_API_URL", "https://custom.example/v1"));
            } else if p.base_url.is_empty() {
                pairs.push((p.url_envs[0], "https://custom.example/v1"));
            }
            let cfg = AiConfig {
                provider: Some(p.name.into()),
                ..Default::default()
            };
            let env = env_of(&pairs);
            let s = AiSettings::resolve_with(Some(&cfg), &env)
                .unwrap()
                .unwrap_or_else(|| panic!("{}", p.name));
            let want_base = if p.base_url.is_empty() {
                "https://custom.example/v1"
            } else {
                p.base_url
            };
            let planner = match s.backend(&s.model, &env).unwrap() {
                Backend::OpenAi(o) => o,
                _ => panic!("{}", p.name),
            };
            assert_eq!(
                planner.endpoint(),
                format!("{want_base}/chat/completions"),
                "{}",
                p.name
            );
            let expected_mode = if p.caps.json_schema {
                OutputMode::JsonSchema
            } else if p.caps.tools {
                OutputMode::ToolCall
            } else {
                OutputMode::Text
            };
            assert_eq!(planner.output_mode(), expected_mode, "{}", p.name);

            // The same against a mock.
            let reply = match expected_mode {
                OutputMode::ToolCall => tool_completion(),
                _ => completion(TAP0),
            };
            let (origin, seen) = serve(vec![(200, "", reply)]).await;
            let mut mocked = pairs.clone();
            let url = format!("{origin}/v1");
            mocked.push(("ATALANTA_NL_API_URL", &url));
            let env = env_of(&mocked);
            let s = AiSettings::resolve_with(Some(&cfg), &env).unwrap().unwrap();
            let step = s
                .planner_for(&s.model, &env)
                .unwrap()
                .plan(&req())
                .await
                .unwrap();
            assert_eq!(step.op, Op::Tap, "{}", p.name);
            let r = seen.lock().unwrap()[0].clone();
            assert_eq!(r.target, "/v1/chat/completions", "{}", p.name);
            assert_eq!(r.body["model"], serde_json::json!(s.model), "{}", p.name);
            match key_env {
                Some(_) => assert_eq!(
                    r.header("authorization"),
                    Some("Bearer test-key"),
                    "{}",
                    p.name
                ),
                None => assert_eq!(r.header("authorization"), None, "{}", p.name),
            }
            match expected_mode {
                OutputMode::JsonSchema => assert_eq!(
                    r.body["response_format"]["type"], "json_schema",
                    "{}",
                    p.name
                ),
                OutputMode::ToolCall => assert_eq!(
                    r.body["tool_choice"]["function"]["name"], "ui_step",
                    "{}",
                    p.name
                ),
                OutputMode::Text => {
                    assert!(
                        r.body.get("response_format").is_none() && r.body.get("tools").is_none()
                    );
                    assert!(r.body["messages"][0]["content"]
                        .as_str()
                        .unwrap()
                        .contains("JSON schema"));
                }
            }
        }
    }

    #[tokio::test]
    async fn openai_takes_its_base_url_and_organization_from_the_environment() {
        let (origin, seen) = serve(vec![(200, "", completion(TAP0))]).await;
        let url = format!("{origin}/proxy/v1");
        let env = env_of(&[
            ("OPENAI_API_KEY", "k"),
            ("OPENAI_BASE_URL", &url),
            ("OPENAI_ORG_ID", "org-1"),
        ]);
        let s = AiSettings::resolve_with(None, &env).unwrap().unwrap();
        s.planner_for(&s.model, &env)
            .unwrap()
            .plan(&req())
            .await
            .unwrap();
        let r = seen.lock().unwrap()[0].clone();
        assert_eq!(r.target, "/proxy/v1/chat/completions");
        assert_eq!(r.header("openai-organization"), Some("org-1"));
        assert_eq!(r.body["model"], OPENAI_STEP_MODEL);
    }

    #[tokio::test]
    async fn azure_uses_the_api_key_header_and_both_url_shapes() {
        assert_eq!(
            azure_endpoint("https://r.openai.azure.com/", "gpt dep", Some("2024-10-21")),
            "https://r.openai.azure.com/openai/deployments/gpt%20dep/chat/completions?api-version=2024-10-21"
        );
        assert_eq!(
            azure_endpoint("https://r.openai.azure.com/openai/v1", "d", None),
            "https://r.openai.azure.com/openai/v1/chat/completions"
        );
        for version in [Some("2024-10-21"), None] {
            let (origin, seen) = serve(vec![(200, "", completion(TAP0))]).await;
            let mut pairs = vec![
                ("AZURE_OPENAI_API_KEY", "az-key"),
                ("AZURE_OPENAI_ENDPOINT", origin.as_str()),
                ("AZURE_OPENAI_DEPLOYMENT", "prod-gpt"),
            ];
            if let Some(v) = version {
                pairs.push(("AZURE_OPENAI_API_VERSION", v));
            }
            let env = env_of(&pairs);
            let s = AiSettings::resolve_with(None, &env).unwrap().unwrap();
            assert_eq!(s.preset.name, "azure-openai");
            s.planner_for(&s.model, &env)
                .unwrap()
                .plan(&req())
                .await
                .unwrap();
            let r = seen.lock().unwrap()[0].clone();
            assert_eq!(r.header("api-key"), Some("az-key"));
            assert_eq!(r.header("authorization"), None);
            match version {
                Some(_) => assert_eq!(
                    r.target,
                    "/openai/deployments/prod-gpt/chat/completions?api-version=2024-10-21"
                ),
                None => {
                    assert_eq!(r.target, "/openai/v1/chat/completions");
                    assert_eq!(r.body["model"], "prod-gpt");
                }
            }
        }
    }

    #[tokio::test]
    async fn bedrock_signs_with_the_credential_chain() {
        let reply = serde_json::json!({ "output": { "message": { "content": [ { "toolUse": { "name": "ui_step",
            "input": serde_json::from_str::<serde_json::Value>(TAP0).unwrap() } } ] } }, "stopReason": "tool_use" })
        .to_string();
        let (origin, seen) = serve(vec![(200, "", reply)]).await;
        let env = env_of(&[
            ("ATALANTA_NL_PROVIDER", "bedrock"),
            ("AWS_ACCESS_KEY_ID", "AKIDT1"),
            ("AWS_SECRET_ACCESS_KEY", "sk"),
            ("AWS_REGION", "us-west-2"),
            ("ATALANTA_NL_API_URL", &origin),
        ]);
        let s = AiSettings::resolve_with(None, &env).unwrap().unwrap();
        assert_eq!(
            (s.preset.name, s.model.as_str()),
            ("bedrock", "anthropic.claude-haiku-5-5")
        );
        s.planner_for(&s.model, &env)
            .unwrap()
            .plan(&req())
            .await
            .unwrap();
        let r = seen.lock().unwrap()[0].clone();
        assert_eq!(r.target, "/model/anthropic.claude-haiku-5-5/converse");
        assert!(r
            .header("authorization")
            .unwrap()
            .contains("Credential=AKIDT1/"));
        assert!(r
            .header("authorization")
            .unwrap()
            .contains("/us-west-2/bedrock/aws4_request"));
        // No credentials at all: a configuration error at build.
        let none = env_of(&[
            ("ATALANTA_NL_PROVIDER", "bedrock"),
            ("AWS_REGION", "us-east-1"),
            ("HOME", "/nonexistent"),
        ]);
        let s = AiSettings::resolve_with(None, &none).unwrap().unwrap();
        assert_eq!(s.planner_for(&s.model, &none).err().unwrap().exit_code(), 2);
    }

    #[tokio::test]
    async fn vertex_routes_claude_to_raw_predict_and_gemini_to_the_openai_endpoint() {
        let dir = std::env::temp_dir().join(format!("velnl-vertex-{}", std::process::id()));
        let token_reply =
            serde_json::json!({ "access_token": "ya29.vertex", "expires_in": 3600 }).to_string();
        let anthropic_reply = serde_json::json!({ "stop_reason": "end_turn",
            "content": [{ "type": "text", "text": TAP0 }] })
        .to_string();
        let (origin, seen) = serve(vec![
            (200, "", token_reply.clone()),
            (200, "", anthropic_reply),
            (200, "", token_reply),
            (200, "", completion(TAP0)),
        ])
        .await;
        let sa = crate::gcp::tests::sa_file(&dir, &format!("{origin}/token"));
        let sa = sa.display().to_string();
        let env = env_of(&[
            ("ATALANTA_NL_PROVIDER", "vertex"),
            ("GOOGLE_APPLICATION_CREDENTIALS", &sa),
            ("GOOGLE_CLOUD_LOCATION", "us-east5"),
            ("ATALANTA_NL_API_URL", &origin),
        ]);
        let s = AiSettings::resolve_with(None, &env).unwrap().unwrap();
        assert_eq!(
            (s.preset.name, s.model.as_str()),
            ("vertex", "claude-haiku-5-5")
        );
        s.planner_for(&s.model, &env)
            .unwrap()
            .plan(&req())
            .await
            .unwrap();
        s.planner_for("gemini-3.8-flash", &env)
            .unwrap()
            .plan(&req())
            .await
            .unwrap();
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen[1].target, "/v1/projects/atalanta-test/locations/us-east5/publishers/anthropic/models/claude-haiku-5-5:rawPredict");
        assert_eq!(seen[1].header("authorization"), Some("Bearer ya29.vertex"));
        assert_eq!(seen[1].body["anthropic_version"], "vertex-2023-10-16");
        assert!(seen[1].body.get("model").is_none() && seen[1].body.get("output_config").is_none());
        assert!(seen[1].header("x-api-key").is_none());
        assert_eq!(
            seen[3].target,
            "/v1/projects/atalanta-test/locations/us-east5/endpoints/openapi/chat/completions"
        );
        assert_eq!(seen[3].body["model"], "google/gemini-3.8-flash");
        assert_eq!(vertex_origin("global"), "https://aiplatform.googleapis.com");
        assert_eq!(
            vertex_origin("europe-west4"),
            "https://europe-west4-aiplatform.googleapis.com"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    // Capability fallbacks: json_schema rejected → tool call rejected →
    // JSON in text, each once, and it sticks.
    #[tokio::test]
    async fn output_modes_fall_back_down_to_json_in_text() {
        let (origin, seen) = serve(vec![
            (
                400,
                "",
                r#"{"error":{"message":"response_format json_schema not supported"}}"#.into(),
            ),
            (
                400,
                "",
                r#"{"error":{"message":"tools are not supported for this model"}}"#.into(),
            ),
            (200, "", completion(&format!("Here you go: {TAP0}"))),
            (200, "", completion(TAP0)),
        ])
        .await;
        let url = format!("{origin}/v1");
        let env = env_of(&[
            ("ATALANTA_NL_PROVIDER", "openai-compatible"),
            ("ATALANTA_NL_API_URL", &url),
            ("ATALANTA_NL_MODEL", "m"),
        ]);
        let s = AiSettings::resolve_with(None, &env).unwrap().unwrap();
        let p = s.planner_for(&s.model, &env).unwrap();
        assert_eq!(p.plan(&req()).await.unwrap().op, Op::Tap);
        assert_eq!(p.plan(&req()).await.unwrap().op, Op::Tap);
        let seen = seen.lock().unwrap();
        assert!(seen[0].body.get("response_format").is_some());
        assert!(seen[1].body.get("tools").is_some());
        assert!(
            seen[2].body.get("tools").is_none() && seen[2].body.get("response_format").is_none()
        );
        assert!(seen[3].body.get("tools").is_none(), "text mode sticks");
    }

    #[test]
    fn specs_name_a_provider_and_take_only_its_key() {
        let env = env_of(&[
            ("GROQ_API_KEY", "g"),
            ("ATALANTA_NL_MODEL", "anthropic:x"),
            ("ATALANTA_NL_API_URL", "https://ignored"),
        ]);
        let s = AiSettings::for_spec_with("groq:llama-3.1-8b-instant", &env).unwrap();
        assert_eq!(
            (s.preset.name, s.model.as_str(), s.base_url.as_deref()),
            ("groq", "llama-3.1-8b-instant", None)
        );
        let s = AiSettings::for_spec_with("ollama:qwen3:8b@http://box:11434/v1", &env).unwrap();
        assert_eq!(
            (s.model.as_str(), s.base_url.as_deref()),
            ("qwen3:8b", Some("http://box:11434/v1"))
        );
        assert_eq!(s.label(), "ollama:qwen3:8b@http://box:11434/v1");
        assert!(AiSettings::for_spec_with("gpt-5", &env).is_err());
        let e = AiSettings::for_spec_with("mistral:large", &env).unwrap_err();
        assert!(e.to_string().contains("MISTRAL_API_KEY"), "{e}");
    }

    #[test]
    fn a_provider_without_an_endpoint_takes_the_environments() {
        let env = env_of(&[
            ("ATALANTA_NL_API_KEY", "k"),
            ("ATALANTA_NL_API_URL", "http://127.0.0.1:8788/v1"),
        ]);
        let s = AiSettings::for_spec_with("openai-compatible:claude-sonnet-5-5", &env).unwrap();
        assert_eq!(s.base_url.as_deref(), Some("http://127.0.0.1:8788/v1"));
        // An inline URL wins over it.
        let s = AiSettings::for_spec_with("openai-compatible:m@http://other/v1", &env).unwrap();
        assert_eq!(s.base_url.as_deref(), Some("http://other/v1"));
        // A provider with its own endpoint still ignores it, unless the
        // environment names that provider.
        let env = env_of(&[
            ("GROQ_API_KEY", "g"),
            ("ATALANTA_NL_API_URL", "http://proxy/v1"),
        ]);
        let s = AiSettings::for_spec_with("groq:llama", &env).unwrap();
        assert_eq!(s.base_url, None);
        let env = env_of(&[
            ("GROQ_API_KEY", "g"),
            ("ATALANTA_NL_PROVIDER", "groq"),
            ("ATALANTA_NL_API_URL", "http://proxy/v1"),
        ]);
        let s = AiSettings::for_spec_with("groq:llama", &env).unwrap();
        assert_eq!(s.base_url.as_deref(), Some("http://proxy/v1"));
    }

    #[test]
    fn the_preset_table_is_consistent() {
        let mut names = std::collections::HashSet::new();
        for p in PRESETS {
            assert!(names.insert(p.name), "{}", p.name);
            for a in p.aliases {
                assert!(names.insert(a), "{a}");
            }
            assert_eq!(
                find_preset(&p.name.to_uppercase()).map(|f| f.name),
                Some(p.name)
            );
            if p.needs_key {
                assert!(!p.key_envs.is_empty(), "{}", p.name);
            }
        }
        for required in [
            "anthropic",
            "openai",
            "azure-openai",
            "gemini",
            "bedrock",
            "vertex",
            "openrouter",
            "groq",
            "mistral",
            "deepseek",
            "xai",
            "together",
            "fireworks",
            "dashscope",
            "perplexity",
            "ollama",
            "lmstudio",
            "vllm",
            "openai-compatible",
        ] {
            assert!(find_preset(required).is_some(), "{required}");
        }
        assert_eq!(find_preset("qwen").unwrap().name, "dashscope");
    }
}
