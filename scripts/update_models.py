#!/usr/bin/env python3
from __future__ import annotations

import copy
import datetime as dt
import json
import re
import sys
from collections import OrderedDict, defaultdict
from pathlib import Path
from typing import Any

import requests
import yaml

REPO_ROOT = Path(__file__).resolve().parents[1]
MODELS_YAML_PATH = REPO_ROOT / "crates" / "harnx" / "models.yaml"
LITELLM_URL = (
    "https://raw.githubusercontent.com/BerriAI/litellm/main/"
    "model_prices_and_context_window.json"
)

FIELD_ORDER = [
    "name",
    "real_name",
    "max_input_tokens",
    "max_output_tokens",
    "require_max_tokens",
    "input_price",
    "output_price",
    "supports_vision",
    "supports_tool_use",
    "patches",
    "endpoint",
    "type",
    "max_tokens_per_chunk",
    "default_chunk_size",
    "max_batch_size",
    "system_prompt_prefix",
]

HARNX_ONLY_FIELDS = [
    "patches",
    "endpoint",
    "require_max_tokens",
    "real_name",
    "system_prompt_prefix",
    "max_tokens_per_chunk",
    "default_chunk_size",
    "max_batch_size",
]

INCLUDED_MODES = {"chat", "embedding", "reranker"}
EXCLUDED_MODES = {
    "image_generation",
    "audio_speech",
    "audio_transcription",
    "completion",
    "moderation",
    "speech",
    "image_edit",
    "image_variation",
    "realtime",
}

CHAT_PROVIDER_DEFAULTS = {
    "openai",
    "anthropic",
    "google_ai",
    "vertex_ai",
    "bedrock",
    "mistral",
    "cohere",
    "groq",
    "perplexity",
    "deepseek",
    "cloudflare",
    "openrouter",
    "ai21",
    "x_ai",
    "zhipu",
    "alibaba_cloud",
    "baidu",
    "tencent",
    "minimax",
    "moonshot",
    "deepinfra",
    "github",
}

LITELLM_TO_HARNX_PROVIDER = {
    "openai": "openai",
    "anthropic": "claude",
    "google_ai": "gemini",
    "vertex_ai": "vertexai",
    "bedrock": "bedrock",
    "mistral": "mistral",
    "cohere": "cohere",
    "groq": "groq",
    "perplexity": "perplexity",
    "deepseek": "deepseek",
    "voyage": "voyageai",
    "cloudflare": "cloudflare",
    "openrouter": "openrouter",
    "ai21": "ai21",
    "x_ai": "xai",
    "zhipu": "zhipuai",
    "alibaba_cloud": "qianwen",
    "baidu": "ernie",
    "tencent": "hunyuan",
    "minimax": "minimax",
    "moonshot": "moonshot",
    "deepinfra": "deepinfra",
    "github": "github",
    "jina_ai": "jina",
}

PROVIDER_ORDER = [
    "openai",
    "gemini",
    "claude",
    "mistral",
    "ai21",
    "cohere",
    "xai",
    "perplexity",
    "groq",
    "vertexai",
    "bedrock",
    "cloudflare",
    "ernie",
    "qianwen",
    "hunyuan",
    "moonshot",
    "deepseek",
    "zhipuai",
    "minimax",
    "openrouter",
    "github",
    "deepinfra",
    "jina",
    "voyageai",
]

PROVIDER_COMMENTS = {
    "openai": [
        "# Links:",
        "#  - https://platform.openai.com/docs/models",
        "#  - https://platform.openai.com/docs/api-reference/chat",
    ],
    "gemini": [
        "# Links:",
        "#  - https://ai.google.dev/models/gemini",
        "#  - https://ai.google.dev/pricing",
        "#  - https://ai.google.dev/api/rest/v1beta/models/streamGenerateContent",
    ],
    "claude": [
        "# Links:",
        "#  - https://docs.anthropic.com/en/docs/about-claude/models/all-models",
        "#  - https://docs.anthropic.com/en/api/messages",
    ],
    "mistral": [
        "# Links:",
        "#  - https://docs.mistral.ai/getting-started/models/models_overview/",
        "#  - https://mistral.ai/pricing#api-pricing",
        "#  - https://docs.mistral.ai/api/",
    ],
    "ai21": [
        "# Links:",
        "#  - https://docs.ai21.com/docs/jamba-foundation-models",
        "#  - https://www.ai21.com/pricing",
        "#  - https://docs.ai21.com/reference/jamba-1-6-api-ref",
    ],
    "cohere": [
        "# Links:",
        "#  - https://docs.cohere.com/docs/models",
        "#  - https://cohere.com/pricing",
        "#  - https://docs.cohere.com/reference/chat",
    ],
    "xai": [
        "# Links:",
        "#  - https://docs.x.ai/docs/models",
        "#  - https://docs.x.ai/docs/api-reference#chat-completions",
    ],
    "perplexity": [
        "# Links:",
        "#  - https://docs.perplexity.ai/getting-started/models",
        "#  - https://docs.perplexity.ai/api-reference/chat-completions",
    ],
    "groq": [
        "# Links:",
        "#  - https://console.groq.com/docs/models",
        "#  - https://console.groq.com/docs/api-reference#chat",
    ],
    "vertexai": [
        "# Links:",
        "#  - https://cloud.google.com/vertex-ai/generative-ai/docs/learn/models",
        "#  - https://cloud.google.com/vertex-ai/generative-ai/pricing",
        "#  - https://cloud.google.com/vertex-ai/generative-ai/docs/model-reference/gemini",
    ],
    "bedrock": [
        "# Links:",
        "#  - https://docs.aws.amazon.com/bedrock/latest/userguide/model-ids.html#model-ids-arns",
        "#  - https://docs.aws.amazon.com/bedrock/latest/userguide/conversation-inference-supported-models-features.html",
        "#  - https://aws.amazon.com/bedrock/pricing/",
        "#  - https://docs.aws.amazon.com/bedrock/latest/userguide/conversation-inference-call.html",
    ],
    "cloudflare": [
        "# Links:",
        "#  - https://developers.cloudflare.com/workers-ai/models/",
        "#  - https://developers.cloudflare.com/workers-ai/configuration/open-ai-compatibility/",
    ],
    "ernie": [
        "# Links:",
        "#  - https://cloud.baidu.com/doc/qianfan/s/rmh4stp0j",
        "#  - https://cloud.baidu.com/doc/qianfan/s/wmh4sv6ya",
    ],
    "qianwen": [
        "# Links:",
        "#  - https://help.aliyun.com/zh/model-studio/getting-started/models",
        "#  - https://help.aliyun.com/zh/model-studio/developer-reference/use-qwen-by-calling-api",
    ],
    "hunyuan": [
        "# links:",
        "#  - https://cloud.tencent.com/document/product/1729/104753",
        "#  - https://cloud.tencent.com/document/product/1729/97731",
        "#  - https://cloud.tencent.com/document/product/1729/111007",
    ],
    "moonshot": [
        "# Links:",
        "#  - https://platform.moonshot.cn/docs/pricing/chat#%E8%AE%A1%E8%B4%B9%E5%9F%BA%E6%9C%AC%E6%A6%82%E5%BF%B5",
        "#  - https://platform.moonshot.cn/docs/api/chat#%E5%85%AC%E5%BC%80%E7%9A%84%E6%9C%8D%E5%8A%A1%E5%9C%B0%E5%9D%80",
    ],
    "deepseek": [
        "# Links:",
        "#  - https://api-docs.deepseek.com/quick_start/pricing",
        "#  - https://platform.deepseek.com/api-docs/api/create-chat-completion",
    ],
    "zhipuai": [
        "# Links:",
        "#  - https://open.bigmodel.cn/pricing",
        "#  - https://open.bigmodel.cn/dev/api#glm-4",
    ],
    "minimax": [
        "# Links:",
        "# - https://platform.minimaxi.com/docs/guides/pricing-paygo",
        "# - https://platform.minimaxi.com/document/ChatCompletion%20v2",
    ],
    "openrouter": [
        "# Links:",
        "#  - https://openrouter.ai/models",
        "#  - https://openrouter.ai/docs/api-reference/chat-completion",
    ],
    "github": [
        "# Links:",
        "#  - https://github.com/marketplace?type=models",
    ],
    "deepinfra": [
        "# Links:",
        "#  - https://deepinfra.com/models",
        "#  - https://deepinfra.com/docs/openai_api",
    ],
    "jina": [
        "# Links:",
        "#  - https://jina.ai/models",
        "#  - https://api.jina.ai/redoc",
    ],
    "voyageai": [
        "# Links:",
        "#  - https://docs.voyageai.com/docs/embeddings",
        "#  - https://docs.voyageai.com/docs/pricing",
        "#  - https://docs.voyageai.com/reference/",
    ],
}

CLAUDE_THINKING_PATCH = (
    'del(.body.temperature) | del(.body.top_p) | '
    '.body.thinking = {"type":"enabled","budget_tokens":16000}'
)
BEDROCK_THINKING_PATCH = (
    '.body.inferenceConfig = {"temperature":null,"topP":null} | '
    '.body.additionalModelRequestFields = '
    '{"thinking":{"type":"enabled","budget_tokens":16000}}'
)

# Opus models from this minor version onward dropped manual extended thinking
# (`thinking: {type: "enabled", budget_tokens: N}` returns a 400). They only
# accept adaptive thinking plus the `effort` parameter. For these we enable
# adaptive thinking at the default `high` effort on the base model and expose
# higher effort levels as `:suffix` variants instead of a `:thinking` variant.
ADAPTIVE_ONLY_OPUS_MIN_MINOR = 7

# Effort level baked into the base (suffix-less) model, and the higher levels
# exposed as `model:<effort>` variants. See
# https://platform.claude.com/docs/en/build-with-claude/effort
BASE_EFFORT = "high"
ADAPTIVE_EFFORT_VARIANTS = ("xhigh", "max")

# Suffixes of models derived from a base model (regenerated, never preserved).
VARIANT_SUFFIXES = ("thinking", *ADAPTIVE_EFFORT_VARIANTS)


def opus_minor_version(name: str) -> int | None:
    """Minor version of an Opus model name, e.g. `claude-opus-4-8` → 8. Returns
    None for non-Opus names or the older `claude-4-opus-*` ordering."""
    match = re.search(r"opus-4-(\d+)", name)
    return int(match.group(1)) if match else None


def is_adaptive_only_opus(name: str) -> bool:
    minor = opus_minor_version(name)
    return minor is not None and minor >= ADAPTIVE_ONLY_OPUS_MIN_MINOR


def claude_requires_max_tokens(name: str) -> bool:
    """True for Claude models that reject requests without an explicit
    `max_tokens` (the Messages API returns `max_tokens: Field required`).

    Covers the modern Sonnet/Haiku families from the 4-5 generation onward and
    any bare major >= 5 name (e.g. `claude-sonnet-5`), matched by name so new
    releases are handled automatically without a per-model list. Opus models
    are handled separately by `is_adaptive_only_opus` (see apply_base_thinking),
    so they are intentionally excluded here to avoid changing their existing
    behaviour.
    """
    match = re.search(r"claude-(sonnet|haiku)-(\d+)(?:-(\d+))?", name)
    if not match:
        return False
    major = int(match.group(2))
    minor = int(match.group(3)) if match.group(3) is not None else 0
    # Major version >= 5 (e.g. claude-sonnet-5), or the 4-5+ point releases.
    return major >= 5 or (major == 4 and minor >= 5)


def is_variant_name(name: str) -> bool:
    return ":" in name and name.rsplit(":", 1)[1] in VARIANT_SUFFIXES


def apply_openai_endpoint_default(
    model: dict[str, Any], provider: str, payload: dict[str, Any]
) -> None:
    """Route OpenAI chat models to the `responses` endpoint when the LiteLLM
    registry says they support it.

    OpenAI's Responses API (`/v1/responses`) is the path forward for chat models
    (starting with gpt-5.4, tool calling is unsupported on Chat Completions with
    `reasoning: none`). The registry's per-model `supported_endpoints` array is a
    machine-readable capability signal — when it lists `/v1/responses` we set
    `endpoint: responses`, so new model families pick this up automatically
    without a hand-maintained version list. Legacy models whose registry entry
    omits the field (gpt-4o, gpt-4, gpt-3.5-turbo, o1, ...) are left on Chat
    Completions.

    A human choice recorded in models.yaml wins: an already-present `endpoint`
    (preserved via merge_old_fields) is never overwritten.
    """
    if provider != "openai":
        return
    if model.get("type") is not None:  # non-chat (embedding/reranker) models
        return
    if "endpoint" in model:  # preserve any human-curated value
        return
    supported = payload.get("supported_endpoints")
    if isinstance(supported, list) and "/v1/responses" in supported:
        model["endpoint"] = "responses"


def claude_adaptive_patch(effort: str) -> str:
    """Request-body patch enabling adaptive thinking at a given effort level for
    the Claude/Vertex AI body shape. Sampling params are stripped because they
    also 400 on adaptive-only Opus models.

    `output_config` is assigned as a whole object rather than via
    `.body.output_config.effort`: jaq, unlike jq, does not create the missing
    intermediate object, so the nested form errors at request time and takes
    the rest of the pipeline down with it."""
    return (
        "del(.body.temperature) | del(.body.top_p) | "
        '.body.thinking = {"type":"adaptive"} | '
        f'.body.output_config = {{"effort":"{effort}"}}'
    )


def ordered_model(model: dict[str, Any]) -> OrderedDict[str, Any]:
    ordered = OrderedDict()
    for field in FIELD_ORDER:
        if field in model and model[field] is not None:
            ordered[field] = model[field]
    return ordered


class OrderedDumper(yaml.SafeDumper):
    pass


def _represent_ordered_dict(dumper: yaml.SafeDumper, data: OrderedDict[str, Any]) -> Any:
    return dumper.represent_dict(data.items())


OrderedDumper.add_representer(OrderedDict, _represent_ordered_dict)


def normalize_number(value: Any) -> Any:
    if value is None:
        return None
    if isinstance(value, bool):
        return value
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        if value.is_integer():
            return int(value)
        return value
    if isinstance(value, str):
        stripped = value.strip()
        if stripped == "":
            return None
        try:
            number = float(stripped)
        except ValueError:
            return value
        if number.is_integer():
            return int(number)
        return number
    return value


def normalize_bool(value: Any) -> bool | None:
    if value is None:
        return None
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        lowered = value.strip().lower()
        if lowered in {"true", "1", "yes"}:
            return True
        if lowered in {"false", "0", "no"}:
            return False
    return None


def scale_price(value: Any) -> float | int | None:
    number = normalize_number(value)
    if number is None:
        return None
    scaled = round(float(number) * 1_000_000, 6)
    if scaled.is_integer():
        return int(scaled)
    return scaled


def parse_date(value: Any) -> dt.date | None:
    if not value:
        return None
    if isinstance(value, (int, float)):
        return None
    text = str(value).strip()
    if not text:
        return None
    date_part = text[:10]
    try:
        return dt.date.fromisoformat(date_part)
    except ValueError:
        return None


def provider_prefix_and_model_name(
    key: str, payload: dict[str, Any]
) -> tuple[str, str] | None:
    if key == "sample_spec":
        return None
    if "/" in key:
        # An explicit `provider/model` prefix is authoritative.
        prefix, model_name = key.split("/", 1)
        return prefix, model_name
    # Bare keys (no `provider/` prefix) are how LiteLLM lists many providers'
    # first-party models, e.g. `claude-opus-4-8` or `gpt-4o`. Recover the
    # provider from the `litellm_provider` field so these still get ingested;
    # without it there's nothing to map against.
    litellm_provider = payload.get("litellm_provider")
    if isinstance(litellm_provider, str) and litellm_provider:
        return litellm_provider, key
    return None


def is_valid_bedrock_model_name(model_name: str) -> bool:
    if not model_name.startswith("us."):
        return False
    if "/" in model_name:
        return False
    return True


def infer_mode(prefix: str, payload: dict[str, Any]) -> str | None:
    mode = payload.get("mode")
    if isinstance(mode, str) and mode.strip():
        return mode.strip()
    if prefix in CHAT_PROVIDER_DEFAULTS:
        return "chat"
    return None


def model_type_for_mode(mode: str) -> str | None:
    if mode == "chat":
        return None
    if mode in {"embedding", "reranker"}:
        return mode
    return None


def build_model_from_litellm(name: str, payload: dict[str, Any], mode: str) -> dict[str, Any]:
    model: dict[str, Any] = {"name": name}

    max_input = normalize_number(payload.get("max_input_tokens") or payload.get("max_tokens"))
    max_output = normalize_number(payload.get("max_output_tokens") or payload.get("max_output_token"))
    input_price = scale_price(payload.get("input_cost_per_token"))
    output_price = scale_price(payload.get("output_cost_per_token"))
    supports_vision = normalize_bool(
        payload.get("supports_vision")
        if "supports_vision" in payload
        else payload.get("supports_image_input")
    )
    supports_tool_use = normalize_bool(payload.get("supports_function_calling"))
    model_type = model_type_for_mode(mode)

    if max_input is not None:
        model["max_input_tokens"] = max_input
    if max_output is not None:
        model["max_output_tokens"] = max_output
    if input_price is not None:
        model["input_price"] = input_price
    if output_price is not None and mode == "chat":
        model["output_price"] = output_price
    if supports_vision is not None and mode == "chat":
        model["supports_vision"] = supports_vision
    if supports_tool_use is not None and mode == "chat":
        model["supports_tool_use"] = supports_tool_use
    if model_type is not None:
        model["type"] = model_type

    return model


def should_skip_model(today: dt.date, payload: dict[str, Any]) -> bool:
    deprecation_date = parse_date(payload.get("deprecation_date"))
    if deprecation_date is not None and deprecation_date < today:
        return True
    # Skip models with no useful data (e.g. LiteLLM's "container" entry)
    useful_fields = {
        "max_input_tokens", "max_output_tokens", "max_tokens",
        "input_cost_per_token", "output_cost_per_token",
        "supports_vision", "supports_function_calling",
    }
    if not any(payload.get(f) for f in useful_fields):
        return True
    return False


def load_existing_data() -> tuple[list[dict[str, Any]], dict[str, dict[str, dict[str, Any]]]]:
    raw = yaml.safe_load(MODELS_YAML_PATH.read_text())
    old_by_provider: dict[str, dict[str, dict[str, Any]]] = {}
    for provider_entry in raw:
        provider = provider_entry["provider"]
        models = {model["name"]: model for model in provider_entry.get("models", [])}
        old_by_provider[provider] = models
    return raw, old_by_provider


def fetch_litellm_data() -> dict[str, Any]:
    response = requests.get(LITELLM_URL, timeout=60)
    response.raise_for_status()
    return response.json()


def merge_old_fields(new_model: dict[str, Any], old_model: dict[str, Any] | None) -> dict[str, Any]:
    if not old_model:
        return new_model
    merged = copy.deepcopy(new_model)
    for field in HARNX_ONLY_FIELDS:
        if field in old_model and field not in merged:
            merged[field] = copy.deepcopy(old_model[field])
    return merged


def _variant_from_base(
    base_model: dict[str, Any], suffix: str, patch: str, *, max_output_tokens: int | None
) -> dict[str, Any]:
    name = base_model["name"]
    variant: dict[str, Any] = {
        "name": f"{name}:{suffix}",
        # Use the base model's real_name if it has one (aliased model), otherwise
        # the name. This routes the variant to the correct provider-side model ID.
        "real_name": base_model.get("real_name", name),
        # Both thinking and effort variants reject requests without max_tokens.
        "max_output_tokens": max_output_tokens,
        "require_max_tokens": True,
        "patches": [patch],
    }
    for field in (
        "max_input_tokens",
        "input_price",
        "output_price",
        "supports_vision",
        "supports_tool_use",
    ):
        if field in base_model:
            variant[field] = base_model[field]
    return variant


def apply_base_thinking(model: dict[str, Any], provider: str) -> None:
    """Enable adaptive thinking at the default effort on the base (suffix-less)
    model for adaptive-only Opus models. These reject manual thinking and
    sampling params, so the base model must send the adaptive shape itself."""
    name = model["name"]
    if is_variant_name(name) or provider not in ("claude", "vertexai"):
        return
    if name.startswith("claude-") and is_adaptive_only_opus(name):
        model["patches"] = [claude_adaptive_patch(BASE_EFFORT)]
        # Adaptive-only Opus rejects requests without an explicit max_tokens.
        model["require_max_tokens"] = True
    # Modern Claude models reject requests without an explicit max_tokens,
    # regardless of the adaptive-thinking patch above.
    if claude_requires_max_tokens(name):
        model["require_max_tokens"] = True


def thinking_variants(base_model: dict[str, Any], provider: str) -> list[dict[str, Any]]:
    """Variant models derived from a base model: higher effort levels for
    adaptive-only Opus, or a manual `:thinking` variant for everything else."""
    name = base_model["name"]
    if is_variant_name(name):
        return []

    if provider in ("claude", "vertexai"):
        if not name.startswith("claude-"):
            return []
        if is_adaptive_only_opus(name):
            return [
                _variant_from_base(
                    base_model,
                    effort,
                    claude_adaptive_patch(effort),
                    max_output_tokens=base_model.get("max_output_tokens"),
                )
                for effort in ADAPTIVE_EFFORT_VARIANTS
            ]
        patch = CLAUDE_THINKING_PATCH
    elif provider == "bedrock":
        if not name.startswith("us.anthropic.claude-"):
            return []
        # Manual thinking 400s on adaptive-only Opus, and the adaptive+effort
        # shape on Bedrock is unconfirmed — emit no variant rather than a broken
        # one.
        if is_adaptive_only_opus(name):
            return []
        patch = BEDROCK_THINKING_PATCH
    else:
        return []

    return [
        _variant_from_base(
            base_model,
            "thinking",
            patch,
            max_output_tokens=24000,
        )
    ]


def provider_sort_key(name: str) -> tuple[str, str]:
    if is_variant_name(name):
        base = name.rsplit(":", 1)[0]
        return (base, f"1:{name}")
    return (name, f"0:{name}")


def render_provider_block(provider: str, models: list[dict[str, Any]]) -> str:
    lines = []
    lines.extend(PROVIDER_COMMENTS[provider])
    lines.append(f"- provider: {provider}")
    lines.append("  models:")
    if not models:
        lines.append("    []")
        return "\n".join(lines)

    ordered_models = [ordered_model(model) for model in models]
    wrapper = OrderedDict([("models", ordered_models)])
    dumped = yaml.dump(
        wrapper,
        Dumper=OrderedDumper,
        sort_keys=False,
        default_flow_style=False,
        allow_unicode=True,
        width=1000,
    ).rstrip()
    dumped_lines = dumped.splitlines()
    if not dumped_lines or dumped_lines[0] != "models:":
        raise ValueError(f"unexpected yaml wrapper for provider {provider}")
    for line in dumped_lines[1:]:
        # yaml.dump produces items at column 0 (e.g. "- name: foo").
        # We need 4-space indent to match original models.yaml format ("    - name: foo").
        # yaml.dump internal fields are already 2-space indented relative to the list item,
        # so adding "    " gives us 4+2=6 for field lines which is correct.
        lines.append(f"    {line}")
    return "\n".join(lines)


def render_models_yaml(provider_models: OrderedDict[str, list[dict[str, Any]]]) -> str:
    blocks = [render_provider_block(provider, models) for provider, models in provider_models.items()]
    return "\n\n".join(blocks) + "\n"


def compare_price_fields(old_model: dict[str, Any], new_model: dict[str, Any]) -> list[str]:
    changes = []
    for field in ("input_price", "output_price"):
        old_val = old_model.get(field)
        new_val = new_model.get(field)
        if old_val != new_val:
            changes.append(f"{field} {old_val} -> {new_val}")
    return changes


def price_change_lines(
    old_models: dict[str, dict[str, Any]],
    new_models: dict[str, dict[str, Any]],
    shared: list[str],
) -> list[str]:
    lines = []
    for name in shared:
        changes = compare_price_fields(old_models[name], new_models[name])
        if changes:
            lines.append(f"- {name}: {', '.join(changes)}")
    return lines


def patch_change_lines(
    old_models: dict[str, dict[str, Any]],
    new_models: dict[str, dict[str, Any]],
    shared: list[str],
) -> list[str]:
    lines = []
    for name in shared:
        old_patches = old_models[name].get("patches")
        new_patches = new_models[name].get("patches")
        if old_patches != new_patches:
            lines.append(f"- {name}:")
            lines.append(f"    old: {old_patches}")
            lines.append(f"    new: {new_patches}")
    return lines


def provider_diff_sections(
    old_models: dict[str, dict[str, Any]],
    new_models: dict[str, dict[str, Any]],
) -> list[tuple[str, list[str]]]:
    """`(heading, lines)` per kind of change in one provider, empty sections
    dropped — so an empty result means the provider is unchanged."""
    shared = sorted(set(new_models) & set(old_models), key=provider_sort_key)
    added = sorted(set(new_models) - set(old_models), key=provider_sort_key)
    removed = sorted(set(old_models) - set(new_models), key=provider_sort_key)
    candidates = [
        ("added", [f"- {name}" for name in added]),
        ("removed", [f"- {name}" for name in removed]),
        ("pricing changes", price_change_lines(old_models, new_models, shared)),
        # A regenerated patch differing from the shipped one is how a hand-fixed
        # expression silently gets reverted, so it has to show up here too.
        ("request patch changes", patch_change_lines(old_models, new_models, shared)),
    ]
    return [(heading, lines) for heading, lines in candidates if lines]


def build_diff_summary(
    old_by_provider: dict[str, dict[str, dict[str, Any]]],
    new_by_provider: OrderedDict[str, list[dict[str, Any]]],
) -> str:
    lines = ["Model catalog diff summary", ""]
    any_changes = False
    for provider, new_models_list in new_by_provider.items():
        sections = provider_diff_sections(
            old_by_provider.get(provider, {}),
            {model["name"]: model for model in new_models_list},
        )
        if not sections:
            continue
        any_changes = True
        lines.append(f"{provider}:")
        for heading, section_lines in sections:
            lines.append(f"- {heading}:")
            lines.extend([f"  {line}" for line in section_lines])
        lines.append("")
    if not any_changes:
        lines.append("No model additions, removals, pricing, or request patch changes.")
    return "\n".join(lines).rstrip()


def main() -> int:
    _old_raw, old_by_provider = load_existing_data()
    litellm = fetch_litellm_data()
    today = dt.date.today()

    new_provider_models: dict[str, dict[str, dict[str, Any]]] = defaultdict(dict)
    warnings: list[str] = []

    for key, payload in litellm.items():
        parsed = provider_prefix_and_model_name(key, payload)
        if parsed is None:
            continue
        prefix, model_name = parsed
        if prefix not in LITELLM_TO_HARNX_PROVIDER:
            continue
        if should_skip_model(today, payload):
            continue

        mode = infer_mode(prefix, payload)
        if mode is None:
            continue
        if mode in EXCLUDED_MODES or mode not in INCLUDED_MODES:
            continue

        provider = LITELLM_TO_HARNX_PROVIDER[prefix]
        if provider == "bedrock" and not is_valid_bedrock_model_name(model_name):
            continue
        model = build_model_from_litellm(model_name, payload, mode)
        old_model = old_by_provider.get(provider, {}).get(model_name)
        model = merge_old_fields(model, old_model)
        apply_openai_endpoint_default(model, provider, payload)
        new_provider_models[provider][model_name] = model

    final_provider_models: OrderedDict[str, list[dict[str, Any]]] = OrderedDict()
    provider_order = PROVIDER_ORDER + sorted(
        provider for provider in new_provider_models.keys() if provider not in PROVIDER_ORDER
    )

    for provider in provider_order:
        old_models = old_by_provider.get(provider, {})
        fetched_models = new_provider_models.get(provider)
        if not fetched_models:
            if provider in old_by_provider:
                final_models = [copy.deepcopy(old_models[name]) for name in sorted(old_models, key=provider_sort_key)]
                final_provider_models[provider] = final_models
            continue

        merged = {name: copy.deepcopy(model) for name, model in fetched_models.items()}
        # Derived variants (`:thinking`, `:xhigh`, `:max`) are regenerated from
        # the base models below, so don't preserve stale ones from the YAML.
        extra_names = sorted(
            (
                name
                for name in set(old_models) - set(fetched_models)
                if not is_variant_name(name)
            ),
            key=provider_sort_key,
        )
        if extra_names:
            warnings.append(
                f"provider {provider}: preserving models not present in LiteLLM: {', '.join(extra_names)}"
            )
            for name in extra_names:
                merged[name] = copy.deepcopy(old_models[name])

        with_variants: dict[str, dict[str, Any]] = {}
        for name in sorted(merged, key=provider_sort_key):
            model = merged[name]
            apply_base_thinking(model, provider)
            with_variants[name] = model
            for variant in thinking_variants(model, provider):
                with_variants[variant["name"]] = variant

        final_provider_models[provider] = [
            with_variants[name] for name in sorted(with_variants, key=provider_sort_key)
        ]

    MODELS_YAML_PATH.write_text(render_models_yaml(final_provider_models))

    for warning in warnings:
        print(f"WARNING: {warning}", file=sys.stderr)

    print(build_diff_summary(old_by_provider, final_provider_models))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
