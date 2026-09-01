"""Provider-specific base model rules and generated aliases."""

from __future__ import annotations

import re
from typing import Any

CLAUDE_THINKING_PATCH = (
    "del(.body.temperature) | del(.body.top_p) | "
    '.body.thinking = {"type":"enabled","budget_tokens":16000}'
)
BEDROCK_THINKING_PATCH = (
    '.body.inferenceConfig = {"temperature":null,"topP":null} | '
    ".body.additionalModelRequestFields = "
    '{"thinking":{"type":"enabled","budget_tokens":16000}}'
)

# Opus models from this minor version onward only accept adaptive thinking plus
# the effort parameter. The base uses high and aliases expose higher efforts.
ADAPTIVE_ONLY_OPUS_MIN_MINOR = 7
BASE_EFFORT = "high"
ADAPTIVE_EFFORT_VARIANTS = ("xhigh", "max")

# OpenAI aliases are regenerated from the refreshed base so their prices,
# limits, capabilities, and endpoint cannot drift from the provider model.
OPENAI_EFFORT_VARIANTS = {
    "gpt-5.6-sol": ("high", "max"),
    "gpt-5.6-terra": ("high",),
}
OPENAI_NO_SAMPLING_PATCH = "del(.body.temperature) | del(.body.top_p)"

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
    match = re.search(r"opus-4-(\d+)", name)
    return int(match.group(1)) if match else None


def is_adaptive_only_opus(name: str) -> bool:
    minor = opus_minor_version(name)
    return minor is not None and minor >= ADAPTIVE_ONLY_OPUS_MIN_MINOR


def claude_requires_max_tokens(name: str) -> bool:
    """Whether a modern Claude Sonnet or Haiku requires `max_tokens`."""
    match = re.search(r"claude-(sonnet|haiku)-(\d+)(?:-(\d+))?", name)
    if not match:
        return False
    major = int(match.group(2))
    minor = int(match.group(3)) if match.group(3) is not None else 0
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
        '.body.thinking = {"type":"adaptive"} | '
        f'.body.output_config = {{"effort":"{effort}"}}'
    )


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
    if name.startswith("claude-") and is_adaptive_only_opus(name):
        model["patches"] = [claude_adaptive_patch(BASE_EFFORT)]
        model["require_max_tokens"] = True
    if claude_requires_max_tokens(name):
        model["require_max_tokens"] = True


def apply_openai_base_patches(model: dict[str, Any], provider: str) -> None:
    """Apply request rules for OpenAI base models with fixed reasoning."""
    if provider == "openai" and model["name"] == "gpt-5.6-sol":
        model["patches"] = [OPENAI_NO_SAMPLING_PATCH]


def apply_base_model_rules(model: dict[str, Any], provider: str) -> None:
    """Apply provider rules to one suffix-less base model in place."""
    apply_base_thinking(model, provider)
    apply_openai_base_patches(model, provider)


def thinking_variants(base_model: dict[str, Any], provider: str) -> list[dict[str, Any]]:
    """Generate Claude manual-thinking or adaptive-effort variants."""
    name = base_model["name"]
    if is_generated_variant_name(provider, name):
        return []
    if provider in ("claude", "vertexai") and name.startswith("claude-"):
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
    elif provider == "bedrock" and name.startswith("us.anthropic.claude-"):
        if is_adaptive_only_opus(name):
            return []
        patch = BEDROCK_THINKING_PATCH
    else:
        return []
    return [_variant_from_base(base_model, "thinking", patch, max_output_tokens=24000)]


def _openai_effort_variant(base_model: dict[str, Any], effort: str) -> dict[str, Any]:
    patch = f'{OPENAI_NO_SAMPLING_PATCH} | .body.reasoning = {{"effort":"{effort}"}}'
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
