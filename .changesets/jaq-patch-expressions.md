---
harnx: major
---
**Breaking Change**: Replaced the regex-keyed YAML DSL patching system with `jq` (via `jaq`) expression strings for both client request patches and package patches.

This change provides significantly more power and flexibility for modifying requests and configurations, while simplifying the underlying logic. Matching and transformation are now both handled by `jq` filters.

### Key Changes
- **Client Patches**: The `patch:` field in provider configurations and `models.yaml` is renamed to `patches:`.
- **Expression Arrays**: All patch fields now accept an array of `jq` strings instead of a regex-keyed object.
- **Request Context**: Client patch expressions receive a JSON object `{url, headers, body}` and must return the modified version.
- **Package Context**: Package patch expressions (`agents`, `clients`, `mcp_servers`) receive the full configuration struct as JSON and must return the modified version.
- **Environment Variables**: `$HARNX_PATCH_{CLIENT}_{API}` environment variables now expect a JSON array of strings.
- **Interpolation Removed**: Variable interpolation (e.g., `$HARNX_MODEL`, `$HARNX_CLIENT`) is removed. Use `jq` to access fields directly (e.g., `.body.model`).

### Migration Guide

#### 1. Client Patches (Provider Config)
**Before:**
```yaml
patch:
  chat_completions:
    'gpt-4o.*':
      body:
        temperature: 0
```
**After:**
```yaml
patches:
  chat_completions:
    - 'if (.body.model | test("gpt-4o.*")) then .body.temperature = 0 else . end'
```

#### 2. Package Patches
**Before:**
```yaml
agents:
  "coder":
    temperature: 0.5
```
**After:**
```yaml
agents:
  - 'if .name == "coder" then .temperature = 0.5 else . end'
```

#### 3. Per-Model Patches (models.yaml)
**Before:**
```yaml
# models.yaml
- id: o1-preview
  patch:
    body:
      temperature: null
```
**After:**
```yaml
# models.yaml
- id: o1-preview
  patches:
    - 'del(.body.temperature)'
```

#### 4. Environment Variables
**Before:**
```bash
export HARNX_PATCH_OPENAI_CHAT_COMPLETIONS='{"gpt-4o": {"body": {"max_tokens": 100}}}'
```
**After:**
```bash
export HARNX_PATCH_OPENAI_CHAT_COMPLETIONS='["if .body.model == \"gpt-4o\" then .body.max_tokens = 100 else . end"]'
```
