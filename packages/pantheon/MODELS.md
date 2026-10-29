# Package model policy

Reviewed 2026-10-29, following the selections for issues #1765, #1514 and #2025.
This policy covers Pantheon and the standalone coding package. It is a selection
based on provider documentation and the agents' responsibilities, not a measured
Harnx performance benchmark.

## Selection rationale

- Oracle and Plato prioritize reasoning quality: GPT-6 Astra at maximum effort,
  with Fable 5.1 at maximum effort as the Claude alternative.
- Sisyphus, Daedalus, and Atlas prioritize orchestration quality with Opus 5.5.
  Atlas now tries Sol 6.1 before Gemini 4 Argon to preserve orchestration
  quality when Opus is unavailable.
- General Gemini workers move to 3.8 Flash, including former Pro-preview users.
  Heavy implementation, security/privacy review, plan review, and investigation
  use GPT-6.1 Sol; Hephaestus retains high reasoning. Replace old Terra slots
  with Sol 6.1 for coding and substantive judgment. Writing, visual work, and
  research agents (Erato, Iris, Librarian, Mnemosyne, Peitho and Pytheas) use
  Terra 6.1 as their OpenAI-family alternative. Existing GLM 5 specialists
  keep model diversity with GLM 5.3.
- The reasoning-heavy agents take Kimi K3 as their Bedrock fallback: Oracle,
  Plato, Hephaestus, Daedalus, Sisyphus, Melpomene, Momus, Polyhymnia and
  Zosimus. Its vision support complements the text-only GLM 5.3 choice. Both
  have 1M context and 128K output. Mid-tier agents use GLM 5.3, including eight
  Bedrock-primary specialists, at high effort rather than the API default max.
  Kimi remains the reasoning-heavy alternative at a higher token price.
- Reasoning-heavy agents and Atlas use Gemini 4 Argon as their Gemini
  fallback. Routine Gemini workers retain 3.8 Flash; compactors retain
  3.5 Flash-Lite as a fallback. Argon uses medium thinking by default.
- Hermes and Hestia use Luna for small fixes and routine maintenance. Hestia
  tries direct MiniMax M3.1 Flash at low effort after both Luna routes. Their
  Claude fallback is Haiku 5.5; Bedrock MiniMax M2.5 remains available.
- All nine compactors prefer Luna through Codex, then the OpenAI API, with
  Gemini 3.5 Flash-Lite, Haiku 5.5 and Bedrock Luna as fallbacks. Bedrock Luna
  removes GLM 4.7 Flash's 4K output ceiling. Haiku's 1M context window removes
  the earlier need for Sonnet. A synthetic retention spot check supports the
  switch; representative long-session evaluation remains useful.

## Ordered chains

Each row lists the primary model followed by its fallbacks. Every agent has
Gemini, Claude, Codex, OpenAI API, and non-Anthropic Bedrock coverage.
Codex immediately precedes the same OpenAI model and reasoning setting.

| Agents | Primary → fallbacks |
|--------|---------------------|
| `coder`, `aristarchus` | `claude:claude-sonnet-5-5` → `codex:gpt-6.1-sol` → `openai:gpt-6.1-sol` → `gemini:gemini-3.8-flash` → `bedrock:us.zai.glm-5.3` |
| All nine `compact-*` agents (including coding) | `codex:gpt-6-luna` → `openai:gpt-6-luna` → `gemini:gemini-3.5-flash-lite` → `claude:claude-haiku-5-5` → `bedrock:us.openai.gpt-6-luna` |
| `aeacus` | `codex:gpt-6.1-sol` → `openai:gpt-6.1-sol` → `gemini:gemini-3.8-flash` → `claude:claude-sonnet-5-5` → `bedrock:us.zai.glm-5.3` |
| `apollo`, `argus`, `calliope`, `clio`, `euterpe`, `minos`, `terpsichore`, `thalia` | `gemini:gemini-3.8-flash` → `codex:gpt-6.1-sol` → `openai:gpt-6.1-sol` → `claude:claude-sonnet-5-5` → `bedrock:us.zai.glm-5.3` |
| `athena`, `metis`, `nemesis`, `opis`, `rhadamanthus`, `tyche`, `urania` | `bedrock:us.zai.glm-5.3` → `gemini:gemini-3.8-flash` → `codex:gpt-6.1-sol` → `openai:gpt-6.1-sol` → `claude:claude-sonnet-5-5` |
| `atlas` | `claude:claude-opus-5-5` → `codex:gpt-6.1-sol` → `openai:gpt-6.1-sol` → `gemini:gemini-4-argon` → `bedrock:us.zai.glm-5.3` |
| `daedalus`, `sisyphus` | `claude:claude-opus-5-5` → `codex:gpt-6.1-sol` → `openai:gpt-6.1-sol` → `gemini:gemini-4-argon` → `bedrock:us.moonshotai.kimi-k3` |
| `erato`, `iris`, `librarian`, `peitho`, `pytheas` | `gemini:gemini-3.8-flash` → `codex:gpt-6.1-terra` → `openai:gpt-6.1-terra` → `claude:claude-sonnet-5-5` → `bedrock:us.zai.glm-5.3` |
| `hephaestus` | `codex:gpt-6.1-sol:high` → `openai:gpt-6.1-sol:high` → `claude:claude-opus-5-5` → `gemini:gemini-4-argon` → `bedrock:us.moonshotai.kimi-k3` |
| `hermes` | `codex:gpt-6-luna` → `openai:gpt-6-luna` → `gemini:gemini-3.8-flash` → `claude:claude-haiku-5-5` → `bedrock:minimax.minimax-m2.5` |
| `hestia` | `codex:gpt-6-luna` → `openai:gpt-6-luna` → `minimax:MiniMax-M3.1-Flash` → `gemini:gemini-3.8-flash` → `claude:claude-haiku-5-5` → `bedrock:minimax.minimax-m2.5` |
| `melpomene`, `momus`, `polyhymnia`, `zosimus` | `codex:gpt-6.1-sol` → `openai:gpt-6.1-sol` → `claude:claude-sonnet-5-5` → `gemini:gemini-4-argon` → `bedrock:us.moonshotai.kimi-k3` |
| `mnemosyne` | `bedrock:us.zai.glm-5.3` → `gemini:gemini-3.8-flash` → `codex:gpt-6.1-terra` → `openai:gpt-6.1-terra` → `claude:claude-sonnet-5-5` |
| `oracle`, `plato` | `codex:gpt-6-astra:max` → `openai:gpt-6-astra:max` → `claude:claude-fable-5-1:max` → `gemini:gemini-4-argon` → `bedrock:us.moonshotai.kimi-k3` |

## Cost and provider evidence

Indicative standard input/output USD per million text tokens, checked on the
review date. These API prices are not subscription usage prices; actual task
cost also depends on reasoning tokens, caching, retries, and tool turns.

| Model | Input / output | Selection source |
|-------|----------------|------------------|
| GPT-6 Astra | $10 / $50 | [OpenAI model specifications](https://developers.openai.com/api/docs/models/gpt-6-astra) |
| GPT-6.1 Sol / GPT-6 Luna | $2 / $10; $0.10 / $0.50 | [OpenAI model catalog](https://developers.openai.com/api/docs/models) |
| GPT-6.1 Terra | $1 / $5 | [OpenAI model catalog](https://developers.openai.com/api/docs/models) |
| Claude Opus 5.5 | $4 / $20 | [Claude model comparison](https://platform.claude.com/docs/en/about-claude/models/overview) |
| Fable 5.1 / Sonnet 5.5 | $10 / $50; $2 / $10 | [Claude model comparison](https://platform.claude.com/docs/en/models/fable-5-1/overview) |
| Haiku 5.5 | $0.10 / $0.50 up to 100K prompt tokens; $0.50 / $2.50 above 100K | [Haiku specifications](https://platform.claude.com/docs/en/models/haiku-5-5/overview) |
| Gemini 3.8 Flash | $0.75 / $3.75 introductory | [Gemini 3.8 guide](https://ai.google.dev/gemini-api/docs/models/gemini-3.8-flash) |
| Gemini 4 Argon | $2 / $10 introductory; $4 / $20 from December 1, 2026 | [Gemini pricing](https://ai.google.dev/gemini-api/docs/pricing) |
| Bedrock Kimi K3 | $3.30 / $16.50 (US CRIS; $0.33 cache read) | [AWS pricing](https://aws.amazon.com/bedrock/pricing/) |
| Bedrock GLM 5.3 / MiniMax M2.5 | $1.848 / $5.808; $0.30 / $1.20 | [AWS pricing](https://aws.amazon.com/bedrock/pricing/) |
| Bedrock Luna | $0.11 / $0.55 up to 272K input tokens; $0.22 / $0.825 above 272K | [AWS model card](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-openai-gpt-6-luna.html) |
| Direct GLM 5.3 | $1.40 / $4.40 | [Z.AI pricing](https://docs.z.ai/guides/overview/pricing) |
| Direct MiniMax M3.1 Flash | $0.15 / $0.60 | [MiniMax pricing](https://platform.minimax.io/docs/pricing/overview) |

Gemini 3.8 introductory pricing ends December 31, 2026; the announced standard
price from January 1, 2027 is $1.50 input / $7.50 output. Revisit the selection
and shared catalog prices at that transition. Bedrock prices above are for
`us-east-1`; other regions differ.

[Codex's model guidance](https://developers.openai.com/codex/models/) lists Astra,
Sol, Terra, and Luna and describes their workload tradeoffs. Availability varies
with the account, plan, and rollout. The
[Gemini model list](https://ai.google.dev/gemini-api/docs/models) retains
3.5 Flash-Lite as the small, inexpensive tier.
AWS documents [Kimi K3](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-moonshot-ai-kimi-k3.html),
[GLM 5.3](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-zai-glm-5-3.html),
[MiniMax M2.5](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-minimax-minimax-m2-5.html),
and [GLM 4.7 Flash](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-zai-glm-4-7-flash.html)
for its Chat Completions endpoint. A newer upstream model name is not sufficient
evidence of Bedrock availability.

## Credentials and limits

Run `codex login` for subscription access, or configure any one of
`OPENAI_API_KEY`, `CLAUDE_API_KEY`, `GEMINI_API_KEY`, or `BEDROCK_API_KEY`.
Package clients use those unqualified environment names. Codex reads
`~/.codex/auth.json`; a package patch can override its `auth_file`.
Bedrock uses an API key (bearer token) and defaults to `us-east-1`.

The runtime attempts the chain in order. Authentication errors and exhausted
retries move to the next model and apply a cooldown. Specific model-unavailable
diagnostics (including OpenAI `model_not_found`, Z.AI unknown-model/plan-access
codes, and model-specific Anthropic/Google not-found responses) skip retries.
Generic HTTP 400/404 request errors still stop the turn. This is not an account
entitlement discovery service: unrecognized provider diagnostics remain errors.
Missing required client settings and local Codex credentials are typed access
failures too, so they skip retries and preserve an earlier overflow for recovery.

Request preparation rejects text-only models when images are present, including
images from tool results, before message patching can strip them. A conservative
input-size check includes tool schemas and image allowances. Incompatible models
and provider-reported context overflow move to the next fallback for this request
without a cooldown. If none succeeds, overflow remains available to the existing
forced-compaction recovery. The estimate is not an exact provider tokenizer.

Kimi K3 and GLM 5.3 have 1M context; GLM is text-only. MiniMax M2.5 is text-only
at roughly 200K. Kimi K3, GLM 5.3 and Bedrock Luna use cross-Region inference,
hence their `us.` prefixes. GLM 5.3 access is limited to eligible AWS customers.

### Optional direct providers

Hestia's MiniMax fallback uses `https://api.minimax.io/v1` and `MINIMAX_API_KEY`.
M3.1 Flash always reasons; the catalog explicitly selects low effort and
`max_completion_tokens`. Harnx preserves the thinking and text accompanying tool
calls when continuing MiniMax or GLM Chat Completions conversations. See the
[MiniMax compatibility requirements](https://platform.minimax.io/docs/api-reference/text-openai-api).
The catalog uses the recommended 131,072 output ceiling and reserves that space
inside its 1M context, leaving 868,928 input tokens.

Direct GLM 5.3 is optional and costs about 24% less than the US Bedrock route at
these rates. Set `ZAI_API_KEY`, then copy the package's
[`zai.patch.example.yaml`](zai.patch.example.yaml) to
`~/.config/harnx/packages/pantheon.patch.yaml`, or use the
[coding example](../coding/zai.patch.example.yaml) for `coding.patch.yaml`.
Merge its `agents` entry into an existing patch instead of overwriting your
other overrides. The patch inserts direct Z.AI immediately before every existing
Bedrock GLM 5.3 slot, including primary slots, while retaining Bedrock afterward.
The `zai` client has its own international catalog, separate from `zhipuai`.
That catalog is curated and currently preserved verbatim by the LiteLLM refresh;
check international Z.AI prices manually when maintaining it.

The default Z.AI endpoint is the pay-as-you-go Chat Completions route,
`https://api.z.ai/api/paas/v4`. For Coding Plan keys, override that package
client's `api_base` to `https://api.z.ai/api/coding/paas/v4`. GLM 5.3 requires
thinking enabled; the shared catalog selects high effort. Account history can
restrict which API protocols are available, so these clients use Chat
Completions. See the [GLM 5.3 protocol guide](https://docs.z.ai/guides/llm/glm-5.3).

Argon shares a 1,048,576-token window across input, thinking and output. The
catalog reserves 65,536 tokens for output, leaving 983,040 input tokens. The
native Gemini client uses `streamGenerateContent` for streaming and
`generateContent` otherwise; Argon patches cap output at 65,536 or 32,768
respectively, preserving lower requested limits. Harnx does not implement
Argon long-decode continuation. See the [GenerateContent limits](https://ai.google.dev/gemini-api/docs/generate-content/long-decode).

## Maintaining the defaults

Codex requires `stream: true` on the wire even for non-streaming callers.
The Codex client buffers the terminal `response.completed` or `response.incomplete` object and uses the
normal Responses extractor to return text, reasoning, tool signatures, IDs and
usage without emitting early chunk events. Preserve this path when changing
compaction or title generation; forwarding `stream: false` returns HTTP 400.

MiniMax and GLM Chat Completions continuations replay the thought and text from
the generation that issued each tool call. Merged non-session tool rounds retain
their original boundaries in `MessageContentToolCalls.rounds`; aggregate reasoning
must not be copied onto every assistant message. Legacy merged messages lacking
those boundaries cannot reconstruct the original reasoning association.

Keep model limits, prices, capabilities, and request patches in
`crates/harnx/models.yaml`. Package clients inherit them; do not duplicate
`models:` blocks just to select an agent's model. Each package client names its
catalog with `model_catalog:`, so inheritance no longer depends on what the
file is called.

Update `scripts/model_variants.py` when adding generated reasoning aliases or
request-specific limits. Argon's input reserve, REST output caps and sampling
cleanup are reapplied during refresh. GLM 5.3's high-effort patch selects the
Chat Completions or Converse field shape without discarding other fields.
Astra, Sol 6.1 and Terra 6.1 need Responses for tool use. Terra 6.1 request
patches also remove sampling parameters when reasoning is active. Gemini call
IDs must survive parsing and appear on both the replayed call and its response.
Fable 5.1 and Haiku 5.5 need adaptive thinking and an explicit output limit.
Their direct Claude patches request summarized thinking and the binding-controls beta so the API drops thinking
invalidated by dynamic prompts or compaction instead of rejecting the turn.
Haiku defaults to medium effort; its existing `:thinking` alias also uses
adaptive thinking because the former fixed-budget setting returns HTTP 400.
See the [Haiku migration guide](https://platform.claude.com/docs/en/models/haiku-5-5/migration-guide). The
[Fable migration guide](https://platform.claude.com/docs/en/models/fable-5-1/migration-guide)
also describes its conversation-history restrictions. The Bedrock Haiku
`:thinking` alias uses adaptive medium thinking in
`additionalModelRequestFields`, preserving the builder's output limit. These
Converse request patches are checked locally; live two-turn acceptance remains
unverified.

The shipped-agent integration test loads both packages, checks metadata and
provider coverage, enforces Codex/OpenAI ordering and the Opus/Bedrock policy,
and exercises the production retry loop with each provider available alone.
Those tests simulate authentication failures and do not make paid model calls.

On 2026-10-29, live two-turn checks with freshly built binaries confirmed tool
calls and result continuation for GLM 5.3 through the package's OpenAI-compatible
Bedrock client and for Argon through the native Gemini streaming client. Argon
also returned streamed thoughts. The generic reasoning probe requires Converse
for Bedrock, so its GLM verdict is a protocol mismatch; the Chat Completions
trace was checked separately for high effort, the matching tool-call/result ID,
and terminal `tool_calls` then `stop` events. This does not validate Converse
for GLM 5.3 or account entitlements other than the credentials used for the check.

On the same date, Luna through Codex and Gemini 3.5 Flash-Lite were compared with
the shipped developer, researcher and planner compactor prompts. Each received
the same synthetic 32K-character transcript with 130 exploration exchanges and
ten exact identifiers/commands/errors to preserve. Luna retained 10/10, 10/10 and
9/10 respectively; Flash-Lite retained 10/10, 4/10 and 10/10. Both preserved the
main decisions, pending work and constraints. Luna took 7.6–11.1 seconds versus
3.6–4.0 seconds for Flash-Lite. This was one run per prompt/model, not a real-session
or near-context-limit benchmark; Luna still omitted a path in the planner case.
Direct Z.AI and MiniMax routes have local request/replay coverage but no live
acceptance claim from this evaluation.
