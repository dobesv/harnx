"""Provider-specific base model rules and generated aliases."""

from __future__ import annotations

import re
from typing import Any

CLAUDE_THINKING_PATCH = (
    "del(.body.temperature) | del(.body.top_p) | "
    '.body.thinking = {"type":"enabled","budget_tokens":16000,"display":"summarized"}'
)
BEDROCK_THINKING_PATCH = (
    '.body.inferenceConfig = {"temperature":null,"topP":null} | '
    ".body.additionalModelRequestFields = "
    '{"thinking":{"type":"enabled","budget_tokens":16000}}'
)
BEDROCK_SUMMARIZED_THINKING_PATCH = (
    '.body.inferenceConfig = {"temperature":null,"topP":null} | '
    ".body.additionalModelRequestFields = "
    '{"thinking":{"type":"enabled","budget_tokens":16000,"display":"summarized"}}'
)
GEMINI_THOUGHTS_PATCH = (
    ".body.generationConfig.thinkingConfig = "
    '((.body.generationConfig.thinkingConfig // {}) + {"includeThoughts":true})'
)
# Gemini 2.5 and later think, except these models.
GEMINI_NON_THINKING_MARKERS = ("-image", "-tts", "-native-audio")

# Opus 4.7+, Opus 5.5, Fable 5.1 and Haiku 5.5 use adaptive thinking
# instead of fixed token budgets. Opus 5.5 and Haiku 5.5 default to medium;
# other adaptive models use high.
ADAPTIVE_ONLY_OPUS_MIN_MINOR = 7
BASE_EFFORT = "high"
ADAPTIVE_EFFORT_VARIANTS = ("xhigh", "max")

# OpenAI aliases are regenerated from the refreshed base so their prices,
# limits, capabilities, and endpoint cannot drift from the provider model.
OPENAI_EFFORT_VARIANTS = {
    "gpt-5.6-sol": ("high", "max"),
    "gpt-5.6-terra": ("high",),
    "gpt-6-astra": ("high", "max"),
    "gpt-6-sol": ("high", "max"),
    "gpt-6.1-sol": ("high", "max"),
}
OPENAI_NO_SAMPLING_PATCH = "del(.body.temperature) | del(.body.top_p)"
OPENAI_SUMMARY_PATCH = '.body.reasoning = {"summary":"auto"}'

VARIANT_SUFFIXES = ("thinking", "high", *ADAPTIVE_EFFORT_VARIANTS)
CLAUDE_VARIANT_SUFFIXES = ("thinking", *ADAPTIVE_EFFORT_VARIANTS)
OPENAI_GENERATED_VARIANTS = frozenset(
    f"{base_name}:{effort}"
    for base_name, efforts in OPENAI_EFFORT_VARIANTS.items()
    for effort in efforts
)
PROVIDER_VARIANT_RULES = {
    "claude": ("claude-", CLAUDE_VARIANT_SUFFIXES),
    "vertexai": ("claude-", CLAUDE_VARIANT_SUFFIXES),
    "bedrock": ("us.anthropic.claude-", ("thinking",)),
}


def opus_minor_version(name: str) -> int | None:
    """Return the minor version from a modern `claude-opus-4-N` name."""
    match = re.search(r"opus-4-(\d{1,2})(?=$|@|:|-\d{8})", name)
    return int(match.group(1)) if match else None


def is_adaptive_only_opus(name: str) -> bool:
    minor = opus_minor_version(name)
    return (minor is not None and minor >= ADAPTIVE_ONLY_OPUS_MIN_MINOR) or bool(
        re.search(r"(?:^|\.)claude-opus-5-5(?:$|@|-\d)", name)
    )


def is_adaptive_only_claude(name: str) -> bool:
    return is_adaptive_only_opus(name) or name.startswith(
        ("claude-fable-5-1", "claude-haiku-5-5")
    )


def claude_requires_max_tokens(name: str) -> bool:
    """Whether a modern Claude model requires `max_tokens`."""
    match = re.search(
        r"claude-(sonnet|haiku|opus)-(\d+)(?:-(\d{1,2}))?(?=$|@|:|-\d{8})",
        name,
    )
    if not match:
        return False
    major = int(match.group(2))
    minor = int(match.group(3)) if match.group(3) is not None else 0
    if match.group(1) == "opus":
        return major > 5 or (major == 5 and minor >= 5)
    return major >= 5 or (major == 4 and minor >= 5)


def is_variant_name(name: str) -> bool:
    """Whether a name has a suffix that should sort beside its base."""
    return ":" in name and name.rsplit(":", 1)[1] in VARIANT_SUFFIXES


def is_generated_variant_name(provider: str, name: str) -> bool:
    """Whether `name` is owned by this generator for `provider`."""
    if provider == "openai":
        return name in OPENAI_GENERATED_VARIANTS
    rule = PROVIDER_VARIANT_RULES.get(provider)
    if rule is None:
        return False
    prefix, suffixes = rule
    base_name, separator, suffix = name.rpartition(":")
    return bool(separator) and base_name.startswith(prefix) and suffix in suffixes


def claude_adaptive_patch(effort: str) -> str:
    """Request patch enabling adaptive Claude thinking at one effort level."""
    return (
        "del(.body.temperature) | del(.body.top_p) | "
        '.body.thinking = {"type":"adaptive","display":"summarized"} | '
        f'.body.output_config = {{"effort":"{effort}"}}'
    )


def adaptive_model_patch(name: str, effort: str, provider: str) -> str:
    patch = claude_adaptive_patch(effort)
    if provider == "claude" and name.startswith(("claude-fable-5-1", "claude-haiku-5-5")):
        # Harnx can compact history and rebuild dynamic system prompts. Ask the
        # API to drop invalidated thinking rather than reject the next tool turn.
        patch = patch.replace(
            '{"type":"adaptive","display":"summarized"}',
            '{"type":"adaptive","display":"summarized",'
            '"block_binding":{"prefix_mismatch_behavior":"drop_block"}}',
        )
        patch += (
            ' | .headers += {"anthropic-beta":(['
            '.headers["anthropic-beta"] // empty, '
            '"thinking-binding-controls-2026-08-01"] | join(","))}'
        )
    return patch


def _variant_from_base(
    base_model: dict[str, Any], suffix: str, patch: str, *, max_output_tokens: int | None
) -> dict[str, Any]:
    name = base_model["name"]
    variant: dict[str, Any] = {
        "name": f"{name}:{suffix}",
        "real_name": base_model.get("real_name", name),
        "max_output_tokens": max_output_tokens,
        "require_max_tokens": True,
        "patches": [patch],
    }
    for field in (
        "max_input_tokens",
        "input_price",
        "output_price",
        "cache_read_price",
        "cache_write_price",
        "supports_vision",
        "supports_tool_use",
        "endpoint",
    ):
        if field in base_model and field not in variant:
            variant[field] = base_model[field]
    return variant


def apply_base_thinking(model: dict[str, Any], provider: str) -> None:
    """Apply Claude thinking and token-limit rules to a base model."""
    name = model["name"]
    if provider not in ("claude", "vertexai"):
        return
    if is_generated_variant_name(provider, name):
        return
    if name.startswith("claude-") and is_adaptive_only_claude(name):
        effort = (
            "medium" if name.startswith(("claude-opus-5-5", "claude-haiku-5-5")) else BASE_EFFORT
        )
        model["patches"] = [adaptive_model_patch(name, effort, provider)]
        model["require_max_tokens"] = True
    if claude_requires_max_tokens(name):
        model["require_max_tokens"] = True


def apply_openai_base_patches(model: dict[str, Any], provider: str) -> None:
    """Apply request rules for OpenAI base models with fixed reasoning."""
    if provider == "openai" and model["name"] in (
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-6-astra",
        "gpt-6-sol",
        "gpt-6.1-sol",
        "gpt-6.1-terra",
        "gpt-6-luna",
    ):
        model["patches"] = [f"{OPENAI_NO_SAMPLING_PATCH} | {OPENAI_SUMMARY_PATCH}"]


def gemini_version(name: str) -> tuple[int, int] | None:
    """`(major, minor)` of a `gemini-<major>[.<minor>]-...` model name."""
    if not name.startswith("gemini-"):
        return None
    numbers = name.removeprefix("gemini-").split("-")[0].split(".")
    major, minor = numbers[0], (numbers[1] if len(numbers) > 1 else "0")
    if not all(part.isascii() and part.isdigit() for part in (major, minor)):
        return None
    return int(major), int(minor)


def gemini_thinks(model: dict[str, Any]) -> bool:
    """Whether a Gemini chat model can return thought summaries.

    Keep in step with `gemini_thinking_capable` in
    crates/harnx-client/tests/it/models_yaml_patches.rs, which fails the
    weekly refresh for any model it counts that lacks the patch.
    """
    name = model.get("real_name", model["name"])
    version = gemini_version(name)
    return (
        model.get("type", "chat") == "chat"
        and version is not None
        and version >= (2, 5)
        and not any(marker in name for marker in GEMINI_NON_THINKING_MARKERS)
    )


def apply_gemini_thoughts(model: dict[str, Any], provider: str) -> None:
    """Ask Gemini thinking models for thought summaries, keeping curated patches.

    Models the registry adds have no curated patch to carry over.
    """
    if provider not in ("gemini", "vertexai"):
        return
    if provider == "gemini" and model["name"] in ("gemini-3.8-flash", "gemini-4-argon"):
        model["patches"] = [
            ".body.generationConfig = ((.body.generationConfig // {}) | "
            "del(.temperature) | del(.topP) | del(.topK) | del(.candidateCount)) | "
            + GEMINI_THOUGHTS_PATCH
        ]
    elif "patches" not in model and gemini_thinks(model):
        model["patches"] = [GEMINI_THOUGHTS_PATCH]
    if provider == "gemini" and model["name"] == "gemini-4-argon":
        # GenerateContent REST has no continuation loop in harnx. Reserve the
        # streaming output ceiling inside Argon's unified 1,048,576-token window.
        # https://ai.google.dev/gemini-api/docs/generate-content/long-decode
        model.update(max_input_tokens=983040, max_output_tokens=65536, require_max_tokens=True)
        model["patches"][0] += (
            " | .body.generationConfig.maxOutputTokens = "
            "([.body.generationConfig.maxOutputTokens // 65536, "
            '(if (.url | endswith(":generateContent")) then 32768 else 65536 end)] | min)'
        )


def apply_bedrock_base_patches(model: dict[str, Any], provider: str) -> None:
    """Set an explicit GLM effort in the shape used by the selected client."""
    if provider == "bedrock" and model["name"] == "us.zai.glm-5.3":
        # GLM 5.3 defaults to max. Use high for everyday engineering specialists.
        # The package client uses Chat Completions; native Bedrock uses Converse.
        model["patches"] = [
            'if (.body | has("inferenceConfig")) then '
            ".body.additionalModelRequestFields = "
            '((.body.additionalModelRequestFields // {}) + {"reasoning_effort":"high"}) '
            'else .body.reasoning_effort = "high" end'
        ]


def apply_base_model_rules(model: dict[str, Any], provider: str) -> None:
    """Apply provider rules to one suffix-less base model in place."""
    apply_base_thinking(model, provider)
    apply_openai_base_patches(model, provider)
    apply_gemini_thoughts(model, provider)
    apply_bedrock_base_patches(model, provider)
    if provider == "zai" and model["name"] == "glm-5.3":
        model["patches"] = [
            '.body.thinking = {"type":"enabled"} | .body.reasoning_effort = "high"'
        ]
    if provider == "minimax" and model["name"] == "MiniMax-M3.1-Flash":
        # Always reasons; use low for the routine-maintenance fallback.
        # https://platform.minimax.io/docs/api-reference/text-openai-api
        model.update(max_input_tokens=868928, max_output_tokens=131072, require_max_tokens=True)
        model["patches"] = [
            f'{OPENAI_NO_SAMPLING_PATCH} | '
            '.body.thinking = {"type":"adaptive"} | .body.reasoning_effort = "low" | '
            '.body.max_completion_tokens = (.body.max_completion_tokens // .body.max_tokens) | '
            'del(.body.max_tokens)'
        ]


def _claude_thinking_variants(
    base_model: dict[str, Any], provider: str
) -> list[dict[str, Any]]:
    name = base_model["name"]
    if name.startswith("claude-haiku-5-5"):
        # Keep the existing alias, but replace its rejected fixed token budget.
        return [
            _variant_from_base(
                base_model,
                "thinking",
                adaptive_model_patch(name, "medium", provider),
                max_output_tokens=24000,
            )
        ]
    if is_adaptive_only_claude(name):
        return [
            _variant_from_base(
                base_model,
                effort,
                adaptive_model_patch(name, effort, provider),
                max_output_tokens=base_model.get("max_output_tokens"),
            )
            for effort in ADAPTIVE_EFFORT_VARIANTS
        ]
    return [
        _variant_from_base(
            base_model, "thinking", CLAUDE_THINKING_PATCH, max_output_tokens=24000
        )
    ]


def _bedrock_thinking_patch(name: str) -> str:
    if name in ("us.anthropic.claude-sonnet-5-5", "us.anthropic.claude-haiku-5-5"):
        # These models need adaptive thinking, not a fixed token budget.
        # Use their documented defaults, not a budget-to-effort mapping:
        # https://platform.claude.com/docs/en/build-with-claude/effort
        # Delete sampling fields without discarding the builder's maxTokens.
        effort = "medium" if name == "us.anthropic.claude-haiku-5-5" else "high"
        return (
            ".body.inferenceConfig = ((.body.inferenceConfig // {}) | "
            "del(.temperature) | del(.topP)) | "
            ".body.additionalModelRequestFields = "
            '{"thinking":{"type":"adaptive","display":"summarized"},'
            f'"output_config":{{"effort":"{effort}"}}}}'
        )
    if name in (
        "us.anthropic.claude-fable-5",
        "us.anthropic.claude-fable-5-1",
        "us.anthropic.claude-opus-5",
        "us.anthropic.claude-sonnet-5",
    ):
        # Preserve shipped summary requests without opting older aliases in.
        return BEDROCK_SUMMARIZED_THINKING_PATCH
    return BEDROCK_THINKING_PATCH


def _bedrock_thinking_variants(base_model: dict[str, Any]) -> list[dict[str, Any]]:
    name = base_model["name"]
    if is_adaptive_only_opus(name):
        return []
    return [
        _variant_from_base(
            base_model, "thinking", _bedrock_thinking_patch(name), max_output_tokens=24000
        )
    ]


def thinking_variants(base_model: dict[str, Any], provider: str) -> list[dict[str, Any]]:
    """Generate Claude manual-thinking or adaptive-effort variants."""
    name = base_model["name"]
    if is_generated_variant_name(provider, name):
        return []
    if provider in ("claude", "vertexai") and name.startswith("claude-"):
        return _claude_thinking_variants(base_model, provider)
    if provider == "bedrock" and name.startswith("us.anthropic.claude-"):
        return _bedrock_thinking_variants(base_model)
    return []


def _openai_effort_variant(base_model: dict[str, Any], effort: str) -> dict[str, Any]:
    patch = (
        f'{OPENAI_NO_SAMPLING_PATCH} | '
        f'.body.reasoning = {{"effort":"{effort}","summary":"auto"}}'
    )
    variant = _variant_from_base(
        base_model,
        effort,
        patch,
        max_output_tokens=base_model.get("max_output_tokens"),
    )
    # OpenAI Responses aliases do not require an explicitly configured limit.
    del variant["require_max_tokens"]
    return variant


def openai_effort_variants(base_model: dict[str, Any], provider: str) -> list[dict[str, Any]]:
    """Generate curated OpenAI reasoning aliases from refreshed base data."""
    if provider != "openai":
        return []
    efforts = OPENAI_EFFORT_VARIANTS.get(base_model["name"], ())
    return [_openai_effort_variant(base_model, effort) for effort in efforts]


def model_variants(base_model: dict[str, Any], provider: str) -> list[dict[str, Any]]:
    return [
        *thinking_variants(base_model, provider),
        *openai_effort_variants(base_model, provider),
    ]


def provider_sort_key(name: str) -> tuple[str, str]:
    if is_variant_name(name):
        base = name.rsplit(":", 1)[0]
        return (base, f"1:{name}")
    return (name, f"0:{name}")
