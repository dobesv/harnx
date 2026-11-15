"""Provider-specific base model rules and generated aliases."""

from __future__ import annotations

import re
from dataclasses import dataclass
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
# Delete sampling fields without discarding the builder's maxTokens.
BEDROCK_ADAPTIVE_THINKING_PATCH = (
    ".body.inferenceConfig = ((.body.inferenceConfig // {}) | "
    "del(.temperature) | del(.topP)) | "
    ".body.additionalModelRequestFields = "
    '{"thinking":{"type":"adaptive","display":"summarized"}}'
)
GEMINI_THOUGHTS_PATCH = (
    ".body.generationConfig.thinkingConfig = "
    '((.body.generationConfig.thinkingConfig // {}) + {"includeThoughts":true})'
)
GEMINI_SAMPLING_THOUGHTS_PATCH = (
    ".body.generationConfig = ((.body.generationConfig // {}) | "
    "del(.temperature) | del(.topP) | del(.topK) | del(.candidateCount)) | "
    + GEMINI_THOUGHTS_PATCH
)
ARGON_REQUEST_PATCH = GEMINI_SAMPLING_THOUGHTS_PATCH + (
    " | .body.generationConfig.maxOutputTokens = "
    "([.body.generationConfig.maxOutputTokens // 65536, "
    '(if (.url | endswith(":generateContent")) then 32768 else 65536 end)] | min)'
)
# Gemini 2.5 and later think, except these models.
GEMINI_NON_THINKING_MARKERS = ("-image", "-tts", "-native-audio")

# Effort levels above the default that get their own alias on adaptive Claude
# models, where the registry says the model accepts them.
ADAPTIVE_EFFORT_VARIANTS = ("xhigh", "max")
# Model field holding the registry's `ThinkingSupport`. update_models.py renders
# only the fields in its FIELD_ORDER, so this never reaches models.yaml.
THINKING_SUPPORT_FIELD = "thinking_support"

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


@dataclass(frozen=True)
class ThinkingSupport:
    """The extended-thinking modes a model's registry entry says it accepts."""

    budget: bool = False
    """Accepts `{"type":"enabled","budget_tokens":N}`."""
    adaptive: bool = False
    """Accepts `{"type":"adaptive"}`."""
    efforts: tuple[str, ...] = ()
    """Entries of `ADAPTIVE_EFFORT_VARIANTS` the model accepts."""

    @classmethod
    def from_registry(cls, payload: dict[str, Any]) -> ThinkingSupport:
        adaptive = payload.get("supports_adaptive_thinking") is True
        # A fixed budget was the only mode before adaptive thinking, so the
        # registry flags it separately only on models that accept both.
        budget = payload.get("supports_legacy_thinking") is True or (
            payload.get("supports_reasoning") is True and not adaptive
        )
        efforts = tuple(
            effort
            for effort in ADAPTIVE_EFFORT_VARIANTS
            if adaptive and payload.get(f"supports_{effort}_reasoning_effort") is True
        )
        return cls(budget=budget, adaptive=adaptive, efforts=efforts)

    @property
    def adaptive_only(self) -> bool:
        return self.adaptive and not self.budget


def thinking_support(model: dict[str, Any]) -> ThinkingSupport:
    """The registry's thinking support for `model`; none when it has no entry."""
    return model.get(THINKING_SUPPORT_FIELD) or ThinkingSupport()


def uses_thinking_binding_controls(name: str) -> bool:
    """Whether to ask the API to drop thinking blocks that history edits invalidated.

    The registry doesn't record which models check replayed thinking, so this
    stays a per-model rule: Fable and Mythos from 5.1 on, and Haiku 5.5.
    """
    match = re.search(r"claude-(?:fable|mythos)-(\d+)(?:-(\d{1,2}))?(?=$|@|:|-\d{8})", name)
    version = (int(match.group(1)), int(match.group(2) or 0)) if match else None
    return (version is not None and version >= (5, 1)) or name.startswith("claude-haiku-5-5")


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


def claude_adaptive_patch(effort: str | None = None) -> str:
    """Request patch enabling adaptive Claude thinking.

    Without an effort the model uses its own default, which differs by model.
    """
    patch = (
        "del(.body.temperature) | del(.body.top_p) | "
        '.body.thinking = {"type":"adaptive","display":"summarized"}'
    )
    if effort is not None:
        patch += f' | .body.output_config = {{"effort":"{effort}"}}'
    return patch


def adaptive_model_patch(name: str, effort: str | None, provider: str) -> str:
    patch = claude_adaptive_patch(effort)
    if provider in ("claude", "vertexai") and uses_thinking_binding_controls(name):
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
    if name.startswith("claude-") and thinking_support(model).adaptive_only:
        # Unpatched, these models either don't think or hide what they think.
        # Replace only thinking settings; curated patches such as headers stay.
        curated = [p for p in model.get("patches", []) if ".body.thinking =" not in p]
        model["patches"] = [*curated, adaptive_model_patch(name, None, provider)]
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
    if provider == "gemini" and model["name"] == "gemini-4-argon":
        apply_argon_limits(model)
    elif provider == "gemini" and model["name"] == "gemini-3.8-flash":
        model["patches"] = [GEMINI_SAMPLING_THOUGHTS_PATCH]
    elif "patches" not in model and gemini_thinks(model):
        model["patches"] = [GEMINI_THOUGHTS_PATCH]


def apply_argon_limits(model: dict[str, Any]) -> None:
    """Replace owned Argon rules once, retaining unrelated curated patches."""
    # Reserve streaming output inside Argon's unified 1,048,576-token window.
    # https://ai.google.dev/gemini-api/docs/generate-content/long-decode
    model.update(max_input_tokens=983040, max_output_tokens=65536, require_max_tokens=True)
    owned = {GEMINI_THOUGHTS_PATCH, GEMINI_SAMPLING_THOUGHTS_PATCH, ARGON_REQUEST_PATCH}
    curated = [patch for patch in model.get("patches", []) if patch not in owned]
    model["patches"] = [*curated, ARGON_REQUEST_PATCH]


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
    support = thinking_support(base_model)
    # A fixed budget wherever the model still accepts one; models that only
    # accept adaptive thinking keep the alias name with an adaptive patch.
    if support.budget:
        thinking_patch = CLAUDE_THINKING_PATCH
    elif support.adaptive:
        thinking_patch = adaptive_model_patch(name, None, provider)
    else:
        return []
    thinking = _variant_from_base(
        base_model, "thinking", thinking_patch, max_output_tokens=24000
    )
    return [thinking] + [
        _variant_from_base(
            base_model,
            effort,
            adaptive_model_patch(name, effort, provider),
            max_output_tokens=base_model.get("max_output_tokens"),
        )
        for effort in support.efforts
    ]


def _bedrock_thinking_variants(base_model: dict[str, Any]) -> list[dict[str, Any]]:
    support = thinking_support(base_model)
    if support.budget:
        patch = BEDROCK_THINKING_PATCH
    elif support.adaptive:
        patch = BEDROCK_ADAPTIVE_THINKING_PATCH
    else:
        return []
    return [_variant_from_base(base_model, "thinking", patch, max_output_tokens=24000)]


def thinking_variants(base_model: dict[str, Any], provider: str) -> list[dict[str, Any]]:
    """Generate the thinking aliases the registry says a Claude model supports."""
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
