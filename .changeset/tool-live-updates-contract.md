---
"harnx": minor
---

Add foundation contract for tool live updates (issue #2096)

**Phase 1 — Update contract**

Extends `ToolEvent::Update` with optional fields for progressive status:
- `title`: concise activity label (distinct from `markdown` body content)
- `kind`: dynamic `ToolKind` refinement during execution
- `locations`: affected file/location snapshots (replace semantics)
- `usage`: per-call display usage snapshot (replaces, not sums)

All new fields have `#[serde(default, skip_serializing_if = "Option::is_none")]` for backward compatibility.

**New types in `harnx-core/src/tool.rs`:**
- `ToolUpdatePatch`: patch payload mirroring Update fields, all optional
- `ToolProgress` trait: object-safe progress sink with `fn update(&self, patch)`
- `NoopToolProgress`: no-op implementation
- `ToolDisplayState`: reducer implementing patch-merge semantics
- `ToolProvider::call_tool_with_progress`: new method with default delegation to `call_tool_with_id`

**Patch semantics:**
- `None`/omitted = unchanged
- `Some(vec![])` for collections = clear
- Collections replace (never append)
- Usage snapshots replace (never summed)
- Patches cannot set terminal status (Completed/Failed)

No behavior change — this establishes types and traits only. Engine dispatch, rendering, and ACP mapping come in subsequent phases.
