# LLM Providers Reference

This document provides a complete reference for every supported LLM provider in `harnx`.

## Global Client Configuration

All clients support these common fields:

| Field | Description | Env Var Override |
|-------|-------------|------------------|
| `name` | Unique ID for the client (used in model identifiers like `my-name:gpt-4`). Defaults to the provider type. | N/A |
| `patch` | Regex-based patches for API requests (url, headers, body). | N/A |
| `extra` | Provider-specific extra configuration. | N/A |
| `system_prompt_prefix` | List of strings prepended to all system prompts for this client (each item is a separate paragraph). | N/A |
| `models` | List of manual model definitions. | N/A |

> **Note on Environment Variables:** The `{NAME}` prefix in environment variables is the `name` field of the client (uppercased). If `name` is not set, it defaults to the provider `type` (e.g., `OPENAI_API_KEY`).

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
| `profile` | AWS Profile name. | Config file only (no env var override). |

**Special Behavior:** Credentials are optional. If omitted, `harnx` uses the standard AWS credential provider chain (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` env vars, `~/.aws/config`, IAM roles, etc.).

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
| `api_base` | **Required.** Format: `https://{RESOURCE}.openai.azure.com` | `AZURE-OPENAI_API_BASE` ¹ |
| `api_key` | **Required.** Your Azure OpenAI API key. | `AZURE-OPENAI_API_KEY` ¹ |

**Special Behavior:** Models must be listed manually in the `models` field, with `name` matching the Azure deployment name.

> ¹ The env var names contain a hyphen (`AZURE-OPENAI_*`), which most shells do not allow in variable names. To use env var overrides for Azure OpenAI, set `name: azure_openai` (underscore) in your config file — this makes the env vars `AZURE_OPENAI_API_KEY` and `AZURE_OPENAI_API_BASE`, which are shell-safe.

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
name: my-provider
api_base: "https://api.example.com/v1"
api_key: "..."
```

---

## Ollama

Ollama provides a local OpenAI-compatible API.

**Example:**
```yaml
type: openai-compatible
name: ollama
api_base: "http://localhost:11434/v1"
```

### Named Shortcuts

The following providers have pre-configured `api_base` URLs. Use `type: openai-compatible` with `name:` set to one of the names below, and harnx will fill in the correct `api_base` automatically (you can still override it explicitly). The env var prefix for the API key is the name uppercased.

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

**Example (using `type: openai-compatible` with `name:`):**
```yaml
type: openai-compatible
name: groq
api_base: https://api.groq.com/openai/v1
# api_key: "..."   # or set GROQ_API_KEY
```

> **Note:** The shorthand names (e.g., `groq`, `deepseek`, `xai`) are used as the `name` field, **not** the `type` field. The `type` must always be `openai-compatible`. The `api_base` is pre-configured by harnx when the name matches a known provider, so you can omit it if using a standard provider name.

> ² Cloudflare's URL contains a literal `{ACCOUNT_ID}` placeholder. You must supply the correct URL explicitly via `api_base` in your config file — the placeholder is not substituted automatically.
