---
title: "Dual-repo Pantheon prompt architecture — template engine mismatch and verification workflow"
date: 2026-07-18
category: "workflow-issues"
problem_type: workflow_issue
component: "pantheon-agents, aristarchus-review-pipeline"
root_cause: "two template engines with incompatible include semantics; no verification harness for rendered prompts"
resolution_type: workflow_improvement
severity: high
tags:
  - minijinja
  - kagent
  - template-include
  - prompt-verification
  - dual-repo
  - pantheon
  - aristarchus
plan_ref: "aristarchus-prompt-improvements"
---

## Problem

Aristarchus multi-agent review pipeline deploys to two repos (harnx local + k8s-manifests cloud) using different template engines with incompatible include semantics. Shared core prompt files contained `{{ include }}` directives that rendered correctly in k8s but leaked as literal text in harnx — silently broken with no error. Additionally, k8s configMap key resolution differs from filesystem paths, causing silent include failures.

## Symptoms

```text
- harnx prompts rendered with literal `{{ include "shared/policy-..." }}` text passed to LLM
- MiniJinja has NO {{ include }} support — variables only expand in top-level wrapper body
- k8s includes resolved by configMap KEY (e.g., `policy-test-coverage`) not filesystem path (`policies/test-coverage`)
- No verification harness existed to detect unrendered directives before production
- Coverage double-jeopardy: three Judges each scanned for coverage issues independently
- Calibration drift: ~15 per-Muse refinements needed across both repos in tandem
```

## Investigation Steps

1. Captured baseline renders via `HARNX_CONFIG_DIR=<tempdir> harnx info agent pantheon/<name>` — found literal `{{ include }}` in 5 shared cores
2. Traced MiniJinja behavior: `render_str` with no loader, Strict undefined — no `{{ include }}` builtin
3. Discovered variable expansion is single-pass: `{{varname}}` only expands in wrapper body, NOT inside loaded file content
4. Consulted Oracle on k8s placement: recommended keeping include inline in shared core (approach A) over wrapper injection (approach B) to preserve prose position
5. Verified k8s include resolution: checked `shared/kustomization.yaml` for configMap key mappings (e.g., `policy-test-coverage` → `prompts/policies/test-coverage.md`)

## Root Cause

**Template engine mismatch:**

| Repo | Engine | Include Support | Variable Expansion |
|---|---|---|---|
| harnx | MiniJinja `render_str` | None — literal pass-through | Top-level wrapper body only; NOT re-expanded inside `path:` loaded content |
| k8s-manifests | kagent preprocessor | Recursive `{{ include "shared/KEY" }}` at any depth | Resolved transitively |

**k8s configMap key resolution:**
Invoking `{{ include "shared/policies/test-coverage" }}` fails silently — kagent resolves by configMap KEY registered in `shared/kustomization.yaml`, not filesystem path. Correct form: `{{ include "shared/policy-test-coverage" }}` matching the key `policy-test-coverage`.

**Architecture requirement (engine-specific — the rules differ per repo):**

- **harnx (MiniJinja):** shared core files must contain NO `{{ include }}` and NO cross-file `{{varname}}` — neither resolves inside loaded core content. All composition must be declared and referenced at the wrapper level: add a `variables:` entry and reference `{{varname}}` in the wrapper body.
- **k8s (kagent):** shared-core includes ARE allowed and resolve recursively at any depth. `{{ include "shared/KEY" }}` may live directly in a shared core or in a wrapper; kagent expands it transitively. (The include target must match the configMap KEY, not the filesystem path — see below.)

Each engine keeps its own distinct syntax and behavior; do not apply the harnx no-include rule to k8s cores or vice-versa.

**Process pitfalls:**
- Parallel `fs_edit` calls on same file corrupted content (collisions → truncation/mangling) — must edit sequentially
- Disk-full event mid-write truncated files; `git checkout --` recovered from committed state

## Solution

### harnx Template Pattern

Wrapper YAML frontmatter:
```yaml
variables:
  - name: policy_test_coverage
    description: Test coverage policy
    path: shared/policy-test-coverage.md
```

Wrapper body:
```markdown
{{thalia_core}}

{{policy_test_coverage}}
```

Shared core (`shared/thalia.md`) contains NO placeholders — just prose. Policy rendered after core content.

### k8s Template Pattern

Shared core (`shared/prompts/thalia.md`) can use inline include:
```markdown
...coverage rules appear here...
{{ include "shared/policy-test-coverage" }}
...
```

Key proviso: include path MUST match configMap key in `shared/kustomization.yaml`, NOT filesystem path.

### Verification Harness

```bash
# Create temp config dir with symlink to packages
VERIFY_DIR=$(mktemp -d)
ln -s /mnt/projects/ai-tools/harnx/packages $VERIFY_DIR/packages

# Render each agent, then extract ONLY the rendered prompt body before the leak
# check. `harnx info agent` emits YAML frontmatter + a variable-default metadata
# dump (which legitimately contains placeholder-looking text) followed by the
# rendered prompt body after the closing `---`. Strip everything up to and
# including the last frontmatter delimiter so the leak grep sees only the body.
for agent in minos rhadamanthus aeacus thalia aristarchus; do
  HARNX_CONFIG_DIR=$VERIFY_DIR harnx info agent pantheon/$agent \
    | awk 'f{print} /^---$/{n++} n>=2{f=1}' > /tmp/body-$agent.txt

  # Leak check on the BODY only — any match here is a real unrendered directive.
  if grep -nE '\{\{ ?include|\{\{[a-z_]+\}\}' /tmp/body-$agent.txt; then
    echo "LEAK in $agent"
  fi
done
```

The awk step drops the frontmatter and the variable-default metadata dump so
expected placeholders in the metadata do not cause false positives. Any
`{{ include ... }}` or `{{varname}}` that survives into the body is a genuine
unrendered directive. (Adjust the delimiter count if the `info` output format
changes.)

### k8s Generated Manifests

After editing source manifest/prompt:
```bash
cd /mnt/projects/formative/k8s-manifests
./tools/scripts/validate-all.sh  # runs kubeconform + regenerates
```

Adding new agent requires:
1. Shared core prompt
2. Wrapper prompt
3. `<agent>.agent.yaml`
4. `<agent>.modelconfig.yaml`
5. Review `kustomization.yaml` — resources + configMapGenerator
6. Shared `kustomization.yaml` — key registration
7. Coordinator's `aristarchus.agent.yaml` A2A delegation list

Missing delegation entry = coordinator references Muse it cannot invoke.

## Why This Works

**Single-pass composition**: harnx wrapper assembles all content at render time — no transitive expansion needed.

**ConfigMap key alignment**: k8s includes resolve by registered key, ensuring `shared/kustomization.yaml` is source of truth for include paths.

**Sequential edit safety**: One file at a time prevents race conditions in parallel tool batches.

**Commit-before-edit**: `git checkout --` restores from HEAD if disk full or other failure truncates mid-write.

## Prevention Strategies

**Verification checklist:**
- [ ] Run `harnx info agent pantheon/<name>` after ANY prompt edit
- [ ] Grep rendered output for literal `{{ include }}` or `{{varname}}`
- [ ] Check k8s include paths against `shared/kustomization.yaml` keys
- [ ] Run `validate-all.sh` after k8s manifest/prompt edits
- [ ] Edit files sequentially — never batch multiple edits to same file

**Architecture rules:**
- [ ] Shared core files: NO `{{ include }}`, NO cross-file `{{varname}}`
- [ ] All composition at wrapper level (harnx) or via registered configMap keys (k8s)
- [ ] New Muse: add to A2A delegation list in coordinator
- [ ] Policy snippets: single source of truth, injected at wrapper level

**Process guards:**
- [ ] Commit verified work promptly (enables `git checkout --` recovery)
- [ ] Capture baselines before editing; diff after each change
- [ ] Independent verification pass (Argus) for complex multi-file changes

## Related Issues

- **Plan notes:** `aristarchus-prompt-improvements` notes `352ac1cd` (decisions), `dcb00b00` (harnx placeholder learning), `ecc32faf` (disk-full + parallel edit pitfalls), `617d2307` (k8s configMap key bug)
- **Related Solution:** [logic-errors/minijinja-system-prompt-templating-2026-04-25.md](../logic-errors/minijinja-system-prompt-templating-2026-04-25.md) — MiniJinja context construction
- **Related Solution:** [integration-issues/rendered-agent-config-dump-pipeline-2026-06-19.md](../integration-issues/rendered-agent-config-dump-pipeline-2026-06-19.md) — Agent config rendering pipeline
