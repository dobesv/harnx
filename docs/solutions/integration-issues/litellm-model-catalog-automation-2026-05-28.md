---
title: "Automated model catalog sync from LiteLLM registry to YAML"
date: 2026-05-28
category: "integration-issues"
problem_type: integration_issue
component: "model-catalog-automation"
root_cause: "manual maintenance burden for 2600+ model metadata entries"
resolution_type: code_fix
severity: medium
tags:
  - automation
  - litellm
  - yaml-generation
  - merge-strategy
  - github-actions
plan_ref: "harnx-auto-models"
---

# Automated Model Catalog Sync from LiteLLM Registry to YAML

## Problem

Manual maintenance of `crates/harnx/models.yaml` required updating pricing, context windows, and capabilities for 2600+ models across 24 providers. LiteLLM maintains a public JSON registry updated daily by the community, but converting this to harnx's YAML format with provider-specific customizations was a manual, error-prone process.

## Symptoms

- Manual updates took hours and were often incomplete
- Pricing data became stale between updates
- Provider-specific comment headers and formatting required careful preservation
- Curated fields (patches, real_name aliases, require_max_tokens) were accidentally lost during manual edits
- Bedrock regional variants and commitment-plan pseudo-models added noise

## Investigation Steps

Evaluated LiteLLM's `model_prices_and_context_window.json` as single source of truth. Key challenges identified:

1. **Provider name mapping**: LiteLLM uses different names (anthropic→claude, google_ai→gemini, vertex_ai→vertexai)
2. **Pricing units**: LiteLLM uses per-token; harnx uses per-million-tokens
3. **Curated field preservation**: Need to keep harnx-only fields while letting LiteLLM data win on shared fields
4. **YAML comment headers**: PyYAML strips comments; custom rendering needed
5. **Bedrock noise**: Regional paths (ap-northeast-1/...) and commitment plans (/1-month-commitment/...)
6. **Thinking variants**: Generate `:thinking` siblings for Claude models with specific jaq patches

Built a 678-line Python script (`scripts/update_models.py`) and 52-test suite addressing these challenges.

## Root Cause

No automation existed to transform LiteLLM's JSON registry into harnx's YAML format while preserving manual customizations. The transformation required:

- Provider name normalization (25+ mappings)
- Per-token to per-million pricing scaling
- Merge strategy favoring LiteLLM on shared fields, preserving harnx-only fields
- Custom YAML rendering to maintain comment headers and indentation
- Filtering logic for Bedrock canonical names
- Auto-generation of Claude `:thinking` variants with provider-specific patches

## Solution

### Data Flow

```
LiteLLM JSON → Filter → Provider Map → Build Models → Merge Curated → Generate Variants → Render YAML
```

### Key Implementation Patterns

**1. Provider Name Mapping**

```python
LITELLM_TO_HARNX_PROVIDER = {
    "openai": "openai",
    "anthropic": "claude",
    "google_ai": "gemini",
    "vertex_ai": "vertexai",
    # ... 25+ mappings
}

def provider_prefix_and_model_name(key: str) -> tuple[str, str] | None:
    if "/" not in key:
        return None
    prefix, model_name = key.split("/", 1)
    return prefix, model_name
```

**2. Pricing Scale (per-token → per-million)**

```python
def scale_price(value: Any) -> float | int | None:
    number = normalize_number(value)
    if number is None:
        return None
    scaled = round(float(number) * 1_000_000, 6)
    return int(scaled) if scaled.is_integer() else scaled
```

**3. Merge Strategy (preserve harnx-only fields)**

```python
HARNX_ONLY_FIELDS = [
    "patches",
    "require_max_tokens",
    "real_name",
    "system_prompt_prefix",
    "max_tokens_per_chunk",
    "default_chunk_size",
    "max_batch_size",
]

def merge_old_fields(new_model: dict, old_model: dict | None) -> dict:
    if not old_model:
        return new_model
    merged = copy.deepcopy(new_model)
    for field in HARNX_ONLY_FIELDS:
        if field in old_model and field not in merged:
            merged[field] = copy.deepcopy(old_model[field])
    return merged
```

LiteLLM wins on shared fields (pricing, context, capabilities). Harnx-only fields preserved from existing file.

**4. Bedrock Filtering (canonical us.* only)**

```python
def is_valid_bedrock_model_name(model_name: str) -> bool:
    if not model_name.startswith("us."):
        return False
    if "/" in model_name:
        return False  # Reject regional paths and commitment plans
    return True
```

**5. Thinking Variant Generation**

```python
def thinking_variant(base_model: dict, provider: str) -> dict | None:
    name = base_model["name"]
    if name.endswith(":thinking"):
        return None
    
    if provider == "claude":
        if not name.startswith("claude-"):
            return None
        patch = CLAUDE_THINKING_PATCH
    elif provider == "bedrock":
        if not name.startswith("us.anthropic.claude-"):
            return None
        patch = BEDROCK_THINKING_PATCH
    else:
        return None
    
    return {
        "name": f"{name}:thinking",
        "real_name": base_model.get("real_name", name),  # Inherit alias if present
        "max_output_tokens": 24000,
        "require_max_tokens": True,
        "patches": [patch],
        # Inherit pricing and capabilities
        **{k: v for k, v in base_model.items() 
           if k in ["max_input_tokens", "input_price", "output_price", 
                    "supports_vision", "supports_tool_use"] and k in base_model}
    }
```

**6. YAML Rendering with Comments**

PyYAML strips comments. Solution: write comment headers manually, use `yaml.dump()` only for models list:

```python
def render_provider_block(provider: str, models: list[dict]) -> str:
    lines = []
    lines.extend(PROVIDER_COMMENTS[provider])  # Pre-defined comment headers
    lines.append(f"- provider: {provider}")
    lines.append("  models:")
    
    if not models:
        lines.append("    []")
        return "\n".join(lines)
    
    # yaml.dump produces items at column 0, need 4-space indent
    dumped = yaml.dump({"models": models}, ...)
    for line in dumped.splitlines()[1:]:  # Skip "models:" wrapper
        lines.append(f"    {line}")  # Prepend 4 spaces
    return "\n".join(lines)
```

**7. Orphan Model Preservation**

Models in harnx but not in LiteLLM are preserved with warnings:

```python
extra_names = sorted(set(old_models) - set(fetched_models))
if extra_names:
    warnings.append(
        f"provider {provider}: preserving models not present in LiteLLM: {', '.join(extra_names)}"
    )
    for name in extra_names:
        merged[name] = copy.deepcopy(old_models[name])
```

## Why This Works

- **Single source of truth**: LiteLLM's community-maintained registry stays current
- **Merge policy prevents data loss**: Curated fields never overwritten by automation
- **Provider-specific logic isolated**: Filtering, patches, and comments per provider
- **Idempotent**: Running repeatedly produces same output (modulo upstream changes)
- **Validation gates**: YAML parse + `cargo build --workspace` before PR creation
- **Test coverage**: 52 pure-function tests cover edge cases without network dependency

## Prevention Strategies

**Test Cases:**
- `scale_price`: zero, None, integer vs float, per-token multipliers
- `is_valid_bedrock_model_name`: region filtering, slash rejection
- `thinking_variant`: patch structure, field inheritance, alias routing via real_name
- `merge_old_fields`: preservation on shared fields, independent on harnx-only
- `render_provider_block`: indentation, comment headers, empty models

**Best Practices:**
- Load existing file FIRST before fetching upstream (prevents feedback loops)
- Filter early (Bedrock) to avoid noise in downstream processing
- Use `copy.deepcopy()` when merging to prevent mutation bugs
- Preserve entire provider sections when LiteLLM has no data for them

**Code Review Checklist:**
- [ ] Provider mapping table matches current LiteLLM naming
- [ ] Thinking variant generation restricted to supported model families
- [ ] Comment headers preserved verbatim (check lowercase quirks like hunyuan's "# links:")
- [ ] Deprecation date filtering tested
- [ ] Pricing scaling verified (spot-check a few models)

## Related Issues

- **Issue:** #675 — Keep models.yaml updated automatically
- **File:** `scripts/update_models.py` (678 lines)
- **Tests:** `scripts/test_update_models.py` (52 tests, 358 lines)
- **Workflow:** `.github/workflows/update-models.yml` (weekly schedule + manual trigger)
