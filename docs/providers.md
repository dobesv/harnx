# LLM Providers Reference

This document provides a complete reference for every supported LLM provider in `harnx`.

## Global Client Configuration

All clients support these common fields:

| Field | Description | Env Var Override |
|-------|-------------|------------------|
| `patch` | Regex-based patches for API requests (url, headers, body). | N/A |
| `extra` | Provider-specific extra configuration. | N/A |
| `system_prompt_prefix` | List of strings prepended to all system prompts for this client (each item is a separate paragraph). | N/A |
| `models` | List of manual model definitions. | N/A |

> **Note on Environment Variables:** The `{NAME}` prefix in environment variables is the client's **filename stem, uppercased as-is** — env var names are built as `${NAME}_${FIELD}` with no character substitution. For example, a client defined in `clients/my_openai.yaml` uses `MY_OPENAI_API_KEY`. If the filename is just the provider type (e.g., `openai.yaml`), it defaults to `OPENAI_API_KEY`. Hyphens are **not** converted to underscores, so a stem with a hyphen produces a hyphen in the prefix (e.g., `clients/my-openai.yaml` → `MY-OPENAI_API_KEY`); prefer underscores in filenames if you intend to use env var overrides. Any `name` field in the file is ignored.

---

## Model Definitions & Pricing

Clients can override model defaults or supply custom model entries using the `models` list field in client configuration files. Default model metadata and prices are loaded from Harnx's built-in model catalog (auto-generated from LiteLLM).

### Model Configuration Fields

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | **Required.** Model name or deployment identifier. |
| `real_name` | string | Optional actual API model identifier passed to the backend provider. |
| `type` | string | Model category (`chat` or `embedding`). Defaults to `chat`. |
| `max_input_tokens` | integer | Maximum context/input token capacity. |
| `max_output_tokens` | integer | Maximum completion/output token capacity. |
| `input_price` | float | USD price per 1,000,000 uncached input tokens. |
| `output_price` | float | USD price per 1,000,000 output tokens. |
| `cache_read_price` | float | USD price per 1,000,000 cache-read input tokens (optional; auto-generated from LiteLLM base rates; tiered rates are unrepresented). |
| `cache_write_price` | float | USD price per 1,000,000 cache-write/creation input tokens (optional; auto-generated from LiteLLM base rates; tiered rates are unrepresented). |
| `cache_accounting` | string | Cache token accounting mode (`subset` or `disjoint`; default `subset`). |
| `supports_vision` | boolean | Whether the model supports vision/image inputs. |
| `supports_tool_use` | boolean | Whether the model supports tool calls. |

### Cache Accounting (`cache_accounting`)

The `cache_accounting` field controls how input token counts from API responses are normalized into OpenTelemetry-subset token counts:

- `subset` (**default**): The API's reported `input_tokens` already includes all cache tokens (`input_tokens >= cache_read + cache_write`). Uncached input tokens are computed as `input_tokens - cache_read - cache_write`.
- `disjoint`: The API's reported `input_tokens` excludes cache tokens. Total input tokens are computed as `input_tokens + cache_read + cache_write`.

#### Usage and Proxy Guidance

- **Native Providers**: Native `claude`, `bedrock`, and `vertexai` clients normalize usage automatically. You do **not** need to set `cache_accounting` for native client definitions.
- **Proxies (`openai` / `openai-compatible`)**: The `cache_accounting` flag exists for `openai` or `openai-compatible` client configurations pointing at an intermediary proxy (such as LiteLLM or agentgateway) fronting a different provider backend.
- **Default is Correct for Standard Proxies**: LiteLLM and agentgateway already normalize token usage to subset accounting by default, so the default `subset` setting is correct for almost all proxy setups.
- **When to set `disjoint`**: Set `cache_accounting: disjoint` **only** for a passthrough or legacy proxy that forwards a backend's cache-excluding input token count (where input tokens do not already include cache tokens).
- **Misconfiguration Warning**: Setting `disjoint` when the proxy is actually `subset` silently **overcharges** by adding cache tokens to total input twice (this is not auto-detectable). Conversely, if `subset` is configured but the API returns `input_tokens < cache_read + cache_write`, Harnx logs a one-time warning per model and clamps uncached input tokens to zero.
- **Field Name Aliases**: OpenAI-family parsers automatically handle common response field aliases, including OpenAI `cache_write_tokens`, LiteLLM `cache_creation_tokens`, and top-level `cache_read_input_tokens`/`cache_creation_input_tokens`. Non-standard field layouts are a documented limitation.

#### Example: Proxy Model with Cache Pricing and Disjoint Accounting

```yaml
type: openai-compatible
api_base: "https://proxy.example.com/v1"
api_key: "..."
models:
  - name: "custom-passthrough-claude"
    max_input_tokens: 200000
    max_output_tokens: 8192
    input_price: 3.0
    output_price: 15.0
    cache_read_price: 0.30
    cache_write_price: 3.75
    cache_accounting: disjoint
```

---

## OpenAI

Standard OpenAI API integration.

| Field | Description | Env Var Override |
|-------|-------------|------------------|
| `api_key` | Your OpenAI API key. | `OPENAI_API_KEY` |
| `api_base` | Custom API base URL. | `OPENAI_API_BASE` |
| `organization_id` | OpenAI Organization ID. | Config file only (no env var override). |

**Example:**
```yaml
type: openai
api_key: "sk-..."
```

---

## Claude (Anthropic)

Integration for Anthropic's Claude models.

| Field | Description | Env Var Override |
|-------|-------------|------------------|
| `api_key` | Your Anthropic API key. | `CLAUDE_API_KEY` |
| `api_base` | Custom API base URL. | `CLAUDE_API_BASE` |

**Example:**
```yaml
type: claude
api_key: "sk-ant-..."
```

---

## Gemini (Google)

Integration for Google Gemini models via the Google AI Studio API.

| Field | Description | Env Var Override |
|-------|-------------|------------------|
| `api_key` | Your Gemini API key. | `GEMINI_API_KEY` |
| `api_base` | Custom API base URL. | `GEMINI_API_BASE` |

**Example:**
```yaml
type: gemini
api_key: "..."
```

---

## AWS Bedrock

Access models hosted on AWS Bedrock.

| Field | Description | Env Var Override |
|-------|-------------|------------------|
| `region` | **Required.** AWS Region (e.g., `us-east-1`). | `BEDROCK_REGION` |
| `access_key_id` | AWS Access Key ID. | `BEDROCK_ACCESS_KEY_ID` |
| `secret_access_key` | AWS Secret Access Key. | `BEDROCK_SECRET_ACCESS_KEY` |
| `session_token` | AWS Session Token. | `BEDROCK_SESSION_TOKEN` |
| `profile` | AWS Profile name. Pin a specific `~/.aws/config` profile. | Use `AWS_PROFILE` env var (standard AWS convention) or set `profile` in config. |

**Special Behavior:** Credentials are optional. If omitted, `harnx` uses the standard AWS credential provider chain (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` env vars, `AWS_PROFILE`, `~/.aws/config`, IAM roles, EC2 instance profiles, SSO, etc.).

**Example:**
```yaml
type: bedrock
region: us-east-1
```

---

## Vertex AI (Google Cloud)

Access models on Google Cloud Vertex AI.

| Field | Description | Env Var Override |
|-------|-------------|------------------|
| `project_id` | **Required.** Google Cloud Project ID. | `VERTEXAI_PROJECT_ID` |
| `location` | **Required.** Google Cloud Location. | `VERTEXAI_LOCATION` |
| `adc_file` | Path to a service account JSON file. | Config file only (no env var override). |

**Special Behavior:** Uses Google Application Default Credentials (ADC). `adc_file` can be used to override the default ADC path.

**Example:**
```yaml
type: vertexai
project_id: "my-project"
location: "us-central1"
```

---

## Azure OpenAI

Integration for Azure-hosted OpenAI deployments.

| Field | Description | Env Var Override |
|-------|-------------|------------------|
| `api_base` | **Required.** Format: `https://{RESOURCE}.openai.azure.com` | `AZURE_OPENAI_API_BASE` ¹ |
| `api_key` | **Required.** Your Azure OpenAI API key. | `AZURE_OPENAI_API_KEY` ¹ |

**Special Behavior:** Models must be listed manually in the `models` field, with `name` matching the Azure deployment name.

> ¹ The env var prefix is derived from the filename stem. To ensure env var names are shell-safe (no hyphens), name your configuration file `azure_openai.yaml` (with an underscore). This results in env vars like `AZURE_OPENAI_API_KEY`.

**Example:**
```yaml
type: azure-openai
api_base: "https://my-resource.openai.azure.com"
api_key: "..."
models:
  - name: "gpt-4o-deployment"
    type: chat
```

---

## Cohere

Integration for Cohere models.

| Field | Description | Env Var Override |
|-------|-------------|------------------|
| `api_key` | Your Cohere API key. | `COHERE_API_KEY` |
| `api_base` | Custom API base URL. | `COHERE_API_BASE` |

**Example:**
```yaml
type: cohere
api_key: "..."
```

---

## OpenAI Compatible

Used for any provider that implements the OpenAI API specification.

| Field | Description | Env Var Override |
|-------|-------------|------------------|
| `api_base` | **Required.** The base URL of the API. | `{NAME}_API_BASE` |
| `api_key` | API key. | `{NAME}_API_KEY` |

**Example:**
```yaml
type: openai-compatible
api_base: "https://api.example.com/v1"
api_key: "..."
```

---

## Ollama

Ollama provides a local OpenAI-compatible API.

**Example:**
```yaml
type: openai-compatible
api_base: "http://localhost:11434/v1"
```

### Named Shortcuts

The following providers have pre-configured `api_base` URLs. Use `type: openai-compatible` and name the configuration file one of the names below (e.g., `clients/groq.yaml`), and harnx will fill in the correct `api_base` automatically (you can still override it explicitly). The env var prefix for the API key is the filename stem uppercased.

| Name | API Base URL | Env Var Prefix |
|------|--------------|----------------|
| `ai21` | `https://api.ai21.com/studio/v1` | `AI21` |
| `cloudflare` | `https://api.cloudflare.com/client/v4/accounts/{ACCOUNT_ID}/ai/v1` ² | `CLOUDFLARE` |
| `deepinfra` | `https://api.deepinfra.com/v1/openai` | `DEEPINFRA` |
| `deepseek` | `https://api.deepseek.com` | `DEEPSEEK` |
| `ernie` | `https://qianfan.baidubce.com/v2` | `ERNIE` |
| `github` | `https://models.inference.ai.azure.com` | `GITHUB` |
| `groq` | `https://api.groq.com/openai/v1` | `GROQ` |
| `hunyuan` | `https://api.hunyuan.cloud.tencent.com/v1` | `HUNYUAN` |
| `jina` | `https://api.jina.ai/v1` | `JINA` |
| `minimax` | `https://api.minimax.chat/v1` | `MINIMAX` |
| `mistral` | `https://api.mistral.ai/v1` | `MISTRAL` |
| `moonshot` | `https://api.moonshot.cn/v1` | `MOONSHOT` |
| `openrouter` | `https://openrouter.ai/api/v1` | `OPENROUTER` |
| `perplexity` | `https://api.perplexity.ai` | `PERPLEXITY` |
| `qianwen` | `https://dashscope.aliyuncs.com/compatible-mode/v1` | `QIANWEN` |
| `voyageai` | `https://api.voyageai.com/v1` | `VOYAGEAI` |
| `xai` | `https://api.x.ai/v1` | `XAI` |
| `zhipuai` | `https://open.bigmodel.cn/api/paas/v4` | `ZHIPUAI` |

**Example (using `type: openai-compatible` with the filename `groq.yaml`):**
```yaml
type: openai-compatible
api_base: https://api.groq.com/openai/v1
# api_key: "..."   # or set GROQ_API_KEY
```

> **Note:** The shorthand names (e.g., `groq`, `deepseek`, `xai`) must be used as the **filename** (e.g., `groq.yaml`), not the `type` field. The `type` must always be `openai-compatible`. The `api_base` is pre-configured by harnx when the filename stem matches a known provider, so you can omit it if using a standard provider name.
>
> ² Cloudflare's URL contains a literal `{ACCOUNT_ID}` placeholder. You must supply the correct URL explicitly via `api_base` in your config file — the placeholder is not substituted automatically.
