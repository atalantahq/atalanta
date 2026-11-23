# AI providers

Natural-language steps (`do:`, `assert:`, bare strings), `verify:` checks, `atalanta explore` (its goal model) and `atalanta nl-eval` use one model service. Pick it by name and set only its key's environment variable. The 19 presets live in `crates/atalanta-nl/src/provider.rs` (`PRESETS`); this page lists them.

Whatever the provider, the model only picks an operation and an element number. Atalanta builds and checks the selector, fills in the values, and judges checks against the elements on screen ("the model picks; code decides").

## Choosing a provider

One `provider:model` string works everywhere:

```bash
atalanta nl-eval --model anthropic:claude-haiku-5-5 --model groq:llama-3.3-70b-versatile
ATALANTA_NL_MODEL=gemini:gemini-3.8-flash atalanta run app.yaml
```

```yaml
config:
  ai:
    provider: openrouter            # or model: openrouter:anthropic/claude-haiku-5-5
    model: anthropic/claude-haiku-5-5
    exploreModel: anthropic/claude-sonnet-5-5
    apiKeyEnv: OPENROUTER_API_KEY   # the variable's NAME, never the key
```

The prefix counts only when it is a provider name, so `ollama:llama3.1:8b` is Ollama's `llama3.1:8b`, and a Bedrock id like `anthropic.claude-haiku-5-5-v1:0` stays whole.

Each setting is read from the environment first, then from `config.ai`, then from the provider's default. If the environment names a different provider than the suite, the suite's `config.ai` fields don't apply.

| Setting | Environment | `config.ai` |
|---|---|---|
| Provider | `ATALANTA_NL_PROVIDER`, or the prefix of `ATALANTA_NL_MODEL` | `provider`, or the prefix of `model` |
| Step and check model | `ATALANTA_NL_MODEL` | `model` |
| Goal model (`atalanta explore`) | `ATALANTA_NL_EXPLORE_MODEL` | `exploreModel` |
| Endpoint | `ATALANTA_NL_API_URL`, then the provider's URL variable | `baseUrl` |
| Key | the variable `apiKeyEnv` names, else the provider's | `apiKeyEnv` |
| Screenshots to checks | `ATALANTA_NL_IMAGES=0` | `images` |
| Bedrock region / Vertex location | `AWS_REGION` / `GOOGLE_CLOUD_LOCATION` | `region` |
| Vertex project | `GOOGLE_CLOUD_PROJECT` | `project` |
| Azure `api-version` | `AZURE_OPENAI_API_VERSION` | `apiVersion` |
| Output mode override | `ATALANTA_NL_OUTPUT=json_schema\|tools\|text` | - |

**Auto-detection.** If no provider is named, Atalanta uses the first provider in the table below whose key is set. It logs which one it chose and which other keys it found. Some cases never auto-detect:

- Azure also needs `AZURE_OPENAI_ENDPOINT`.
- Bedrock is auto-detected only from a Bedrock API key (`AWS_BEARER_TOKEN_BEDROCK`). Plain AWS or Google credentials are common on developer machines, so Bedrock (with AWS keys) and Vertex are only used when named.
- Local servers are only used when named.

`ATALANTA_NL_API_KEY` together with `ATALANTA_NL_API_URL` is a generic OpenAI-compatible endpoint. With no usable provider, a committed `.atalanta/nl-cache.json` still replays, but nothing new is planned.

**Keys** are only read from environment variables. `apiKeyEnv` must be a variable name: a value that looks like a key is refused, and the error doesn't echo it. Every key, AWS secret, session token and Google token is registered for redaction and never logged.

## Providers

Default models are **provisional**: they were chosen for cost, not measured. See [Measuring the defaults](#measuring-the-defaults). Columns:

- **Images**: screenshots are sent to `verify:` checks.
- **JSON schema**: the answer is constrained with structured output.
- **Tools**: a tool call is the fallback when structured output isn't available.

| Provider (aliases) | Wire | Key / auth | Endpoint | Default model (goal model) | Images | JSON schema | Tools |
|---|---|---|---|---|---|---|---|
| `anthropic` (`claude`) | Anthropic Messages | `ANTHROPIC_API_KEY` | api.anthropic.com | `claude-haiku-5-5` (`claude-sonnet-5-5`) | ✓ | ✓ | ✓ |
| `openai` | Chat Completions | `OPENAI_API_KEY` (+ `OPENAI_ORG_ID`) | `OPENAI_BASE_URL`, else api.openai.com/v1 | `gpt-6-luna` (`gpt-6-astra`) | ✓ | ✓ | ✓ |
| `azure-openai` (`azure`) | Chat Completions, `api-key` header | `AZURE_OPENAI_API_KEY` | `AZURE_OPENAI_ENDPOINT` | the deployment: `AZURE_OPENAI_DEPLOYMENT` | ✓ | ✓ | ✓ |
| `gemini` (`google`) | Chat Completions | `GEMINI_API_KEY` or `GOOGLE_API_KEY` | generativelanguage.googleapis.com/v1beta/openai | `gemini-3.8-flash` (`gemini-3.1-pro-preview`) | ✓ | ✓ | ✓ |
| `openrouter` | Chat Completions | `OPENROUTER_API_KEY` | openrouter.ai/api/v1 | `anthropic/claude-haiku-5-5` (`anthropic/claude-sonnet-5-5`) | ✓ | ✓ | ✓ |
| `groq` | Chat Completions | `GROQ_API_KEY` | api.groq.com/openai/v1 | `llama-3.3-70b-versatile` (`openai/gpt-oss-120b`) | - | - | ✓ |
| `mistral` | Chat Completions | `MISTRAL_API_KEY` | api.mistral.ai/v1 | `mistral-small-latest` (`mistral-large-latest`) | ✓ | ✓ | ✓ |
| `deepseek` | Chat Completions | `DEEPSEEK_API_KEY` | api.deepseek.com/v1 | `deepseek-chat` (`deepseek-reasoner`) | - | - | ✓ |
| `xai` (`grok`) | Chat Completions | `XAI_API_KEY` | api.x.ai/v1 | `grok-4-fast` (`grok-4`) | ✓ | ✓ | ✓ |
| `together` | Chat Completions | `TOGETHER_API_KEY` | api.together.xyz/v1 | `meta-llama/Llama-3.3-70B-Instruct-Turbo` | - | ✓ | ✓ |
| `fireworks` | Chat Completions | `FIREWORKS_API_KEY` | api.fireworks.ai/inference/v1 | `accounts/fireworks/models/llama-v3p3-70b-instruct` | - | ✓ | ✓ |
| `dashscope` (`qwen`) | Chat Completions | `DASHSCOPE_API_KEY` | dashscope-intl.aliyuncs.com/compatible-mode/v1 | `qwen-plus` (`qwen-max`) | - | - | ✓ |
| `perplexity` | Chat Completions | `PERPLEXITY_API_KEY` | api.perplexity.ai | `sonar` (`sonar-pro`) | - | ✓ | - |
| `bedrock` (`aws`) | Bedrock Converse | SigV4 from `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`/`AWS_SESSION_TOKEN` or `AWS_PROFILE` (`~/.aws`), or `AWS_BEARER_TOKEN_BEDROCK` | bedrock-runtime.`AWS_REGION`.amazonaws.com | `anthropic.claude-haiku-5-5` (`anthropic.claude-sonnet-5-5`) | ✓ | - | ✓ |
| `vertex` (`vertex-ai`) | Claude: `rawPredict`; Gemini: Vertex's OpenAI endpoint | `GOOGLE_APPLICATION_CREDENTIALS` (service account or authorized user), the gcloud ADC file, or `gcloud auth print-access-token` | `GOOGLE_CLOUD_LOCATION` (default `global`), `GOOGLE_CLOUD_PROJECT` | `claude-haiku-5-5` (`claude-sonnet-5-5`); any `gemini-*` too | ✓ | - | - |
| `ollama` | Chat Completions | none (`OLLAMA_API_KEY` if set) | localhost:11434/v1 | `llama3.2` | - | ✓ | ✓ |
| `lmstudio` | Chat Completions | none (`LMSTUDIO_API_KEY` if set) | localhost:1234/v1 | the loaded model (set it) | - | ✓ | ✓ |
| `vllm` | Chat Completions | `VLLM_API_KEY` if set | `VLLM_BASE_URL` (required) | set it | - | ✓ | - |
| `openai-compatible` (`custom`) | Chat Completions | `ATALANTA_NL_API_KEY` or `apiKeyEnv` | `ATALANTA_NL_API_URL` / `baseUrl` (required) | set it | ✓ | ✓ | ✓ |

Notes:

- **Gemini** uses Google's OpenAI-compatible endpoint. Google's docs show it taking `response_format` structured output, `image_url` data URIs and tools, so the native `generateContent` API isn't needed.
- **Azure**: with `AZURE_OPENAI_API_VERSION` (or `apiVersion`) set, requests go to `/openai/deployments/<deployment>/chat/completions?api-version=…`. Without it, they go to the version-less `/openai/v1/chat/completions` path, with the deployment as `model`.
- **Bedrock** answers through one tool call with `toolChoice: auto` and an instruction to use the tool, because the 5.5 Claude models reject a forced tool choice. A model without tool use answers in JSON text. Model ids may need a cross-region inference profile prefix (`us.`, `global.`), depending on the account and region. SigV4 is implemented on `ring` and tested against AWS's published test vectors. SSO, `credential_process` and role profiles are not read: export temporary keys first (`aws configure export-credentials --format env`) or use a Bedrock API key.
- **Vertex**: a `claude-*` model goes to `publishers/anthropic/models/<model>:rawPredict` with `anthropic_version: vertex-2023-10-16`. Any other model goes to `endpoints/openapi/chat/completions` as `google/<model>`. Structured output isn't confirmed for Claude on Vertex, so Claude is asked for JSON in text and parsed strictly. Service-account JWTs are signed RS256 with `ring`.
- **Local servers** need no key and are never auto-detected.

## Fallbacks

Each provider starts at what its capabilities say and steps down when the endpoint rejects a feature. The step-down sticks for the rest of the run, and each one is logged once.

- **Answer format**: `json_schema` structured output, then a tool call, then JSON in text (the schema in the prompt, the reply parsed strictly, one re-ask if it doesn't parse).
- **Screenshots** (checks only; planning is always text): the image, then the numbered text screen list alone.

## Errors

The same rules apply for every provider:

- **401 / 403 / 404**: a configuration error (exit 2), with a hint to check the key, the base URL or the model.
- **429, 529, overloaded, 408, 409, 5xx** (and Bedrock's 424, model not ready): retried with the server's `retry-after` or a jittered backoff, within one deadline.
- **Retries exhausted**: `ServiceUnavailable`. It is transient, so the test is retried, and it never counts as an internal error (exit 5).
- **A 200 reply that isn't the API's format** (a wrong base URL): a configuration error.

## Measuring the defaults

The defaults above were not measured: no keys were available when they were set. Before 1.0, a maintainer with keys runs the fixed task set across providers and records the table in [research/README.md](research/README.md#model-defaults-provisional):

```bash
atalanta nl-eval -k 5 --tasks builtin --recommend --json nl-models.json \
  --model anthropic:claude-haiku-5-5 --model anthropic:claude-sonnet-5-5 \
  --model openai:gpt-6-luna --model gemini:gemini-3.8-flash --model groq:llama-3.3-70b-versatile \
  --price anthropic:claude-haiku-5-5=0.10,0.50 --price anthropic:claude-sonnet-5-5=2,10
```
