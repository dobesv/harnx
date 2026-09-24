# Package model policy

Reviewed 2026-09-24 for issues #1765, #1514 and #2025. This policy covers Pantheon and
the standalone coding package. It is a selection based on provider documentation
and the agents' responsibilities, not a measured Harnx performance benchmark.

## Selection rationale

- Oracle and Plato prioritize reasoning quality: GPT-6 Astra at maximum effort,
  with Fable 5.1 at maximum effort as the Claude alternative.
- Sisyphus, Daedalus, and Atlas prioritize orchestration quality with Opus 5.5.
  Atlas keeps Gemini 3.8 Flash as its first fallback.
- General Gemini workers move to 3.8 Flash, including former Pro-preview users.
  Heavy implementation, security/privacy review, plan review, and investigation
  use GPT-6 Sol; Hephaestus retains high reasoning. Everyday coding and
  bounded judging use Terra. Existing GLM 5 specialists keep model diversity.
- The reasoning-heavy agents take Kimi K3 as their Bedrock fallback: Oracle,
  Plato, Hephaestus, Daedalus, Sisyphus, Melpomene, Momus, Polyhymnia and
  Zosimus. It is the only frontier-class open-weight model on Bedrock, and its
  1M context and vision cover the two things the other Bedrock choices cannot.
  It costs roughly three times GLM 5 on input and five times on output, which
  is affordable only because it sits last in every chain that selects it. The
  mid-tier agents keep GLM 5, where it is often the primary and the 128K output
  ceiling is the best available at that price.
- Hermes uses Luna for small fixes; Hestia uses Bedrock MiniMax M2.5 for routine
  maintenance. Their Claude fallback is Haiku 4.5.
- Compaction retains Gemini 3.5 Flash-Lite, with Luna as the OpenAI-family
  alternative and GLM 4.7 Flash on Bedrock. The Claude fallback is Sonnet 5
  rather than Haiku because long transcripts benefit from its 1M context window.

## Ordered chains

Each row lists the primary model followed by its fallbacks. Every agent has
Gemini, Claude, Codex, OpenAI API, and non-Anthropic Bedrock coverage.
Codex immediately precedes the same OpenAI model and reasoning setting.

| Agents | Primary → fallbacks |
|--------|---------------------|
| `aeacus` | `codex:gpt-6-terra` → `openai:gpt-6-terra` → `gemini:gemini-3.8-flash` → `claude:claude-sonnet-5` → `bedrock:zai.glm-5` |
| `apollo`, `argus`, `calliope`, `clio`, `erato`, `euterpe`, `iris`, `librarian`, `minos`, `peitho`, `pytheas`, `terpsichore`, `thalia` | `gemini:gemini-3.8-flash` → `codex:gpt-6-terra` → `openai:gpt-6-terra` → `claude:claude-sonnet-5` → `bedrock:zai.glm-5` |
| `aristarchus`, `coder` | `claude:claude-sonnet-5` → `codex:gpt-6-terra` → `openai:gpt-6-terra` → `gemini:gemini-3.8-flash` → `bedrock:zai.glm-5` |
| `athena`, `metis`, `mnemosyne`, `nemesis`, `opis`, `rhadamanthus`, `tyche`, `urania` | `bedrock:zai.glm-5` → `gemini:gemini-3.8-flash` → `codex:gpt-6-terra` → `openai:gpt-6-terra` → `claude:claude-sonnet-5` |
| `atlas` | `claude:claude-opus-5-5` → `gemini:gemini-3.8-flash` → `codex:gpt-6-terra` → `openai:gpt-6-terra` → `bedrock:zai.glm-5` |
| All nine `compact-*` agents (including coding) | `gemini:gemini-3.5-flash-lite` → `codex:gpt-6-luna` → `openai:gpt-6-luna` → `claude:claude-sonnet-5` → `bedrock:zai.glm-4.7-flash` |
| `daedalus`, `sisyphus` | `claude:claude-opus-5-5` → `codex:gpt-6-sol` → `openai:gpt-6-sol` → `gemini:gemini-3.8-flash` → `bedrock:us.moonshotai.kimi-k3` |
| `hephaestus` | `codex:gpt-6-sol:high` → `openai:gpt-6-sol:high` → `claude:claude-opus-5-5` → `gemini:gemini-3.8-flash` → `bedrock:us.moonshotai.kimi-k3` |
| `hermes` | `codex:gpt-6-luna` → `openai:gpt-6-luna` → `gemini:gemini-3.8-flash` → `claude:claude-haiku-4-5` → `bedrock:minimax.minimax-m2.5` |
| `hestia` | `bedrock:minimax.minimax-m2.5` → `gemini:gemini-3.8-flash` → `codex:gpt-6-luna` → `openai:gpt-6-luna` → `claude:claude-haiku-4-5` |
| `melpomene`, `momus`, `polyhymnia`, `zosimus` | `codex:gpt-6-sol` → `openai:gpt-6-sol` → `claude:claude-sonnet-5` → `gemini:gemini-3.8-flash` → `bedrock:us.moonshotai.kimi-k3` |
| `oracle`, `plato` | `codex:gpt-6-astra:max` → `openai:gpt-6-astra:max` → `claude:claude-fable-5-1:max` → `gemini:gemini-3.8-flash` → `bedrock:us.moonshotai.kimi-k3` |

## Cost and provider evidence

Indicative standard input/output USD per million text tokens, checked on the
review date. These API prices are not subscription usage prices; actual task
cost also depends on reasoning tokens, caching, retries, and tool turns.

| Model | Input / output | Selection source |
|-------|----------------|------------------|
| GPT-6 Astra | $10 / $50 | [OpenAI model specifications](https://developers.openai.com/api/docs/models/gpt-6-astra) |
| GPT-6 Sol / Terra / Luna | $2 / $10; —; $0.10 / $0.50 | [OpenAI model catalog](https://developers.openai.com/api/docs/models) |
| Claude Opus 5.5 | $4 / $20 | [Claude model comparison](https://platform.claude.com/docs/en/about-claude/models/overview) |
| Fable 5.1 / Sonnet 5 / Haiku 4.5 | $10 / $50; $2 / $10; $1 / $5 | [Claude model comparison](https://platform.claude.com/docs/en/models/fable-5-1/overview) |
| Gemini 3.8 Flash | $0.75 / $3.75 introductory | [Gemini 3.8 guide](https://ai.google.dev/gemini-api/docs/latest-model) |
| Bedrock Kimi K3 | $3.30 / $16.50 (US CRIS; $0.33 cache read) | [AWS pricing](https://aws.amazon.com/bedrock/pricing/) |
| Bedrock GLM 5 / MiniMax M2.5 / GLM 4.7 Flash | $1 / $3.20; $0.30 / $1.20; $0.07 / $0.40 | [AWS pricing](https://aws.amazon.com/bedrock/pricing/) |

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
[GLM 5](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-zai-glm-5.html),
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
retries move to the next model and apply a cooldown. Missing local credentials
can consume retries before moving on. This does not discover account entitlements:
HTTP 400/404 request errors stop the turn, and valid credentials do not guarantee
access to every selected model. Override the chain for restricted accounts.
Context windows and modalities also differ. Kimi K3 takes images and 1M tokens
of context; GLM 5, MiniMax M2.5 and GLM 4.7 Flash are text-only at roughly
200K, and GLM 4.7 Flash has a 4K output cap. Kimi K3 is reachable only through
cross-Region inference, so its id carries the `us.` prefix and there is no
in-Region form.
A fallback is not a guarantee that an oversized or image-bearing request fits.

## Maintaining the defaults

Keep model limits, prices, capabilities, and request patches in
`crates/harnx/models.yaml`. Package clients inherit them; do not duplicate
`models:` blocks just to select an agent's model. Each package client names its
catalog with `model_catalog:`, so inheritance no longer depends on what the
file is called.

Update `scripts/model_variants.py` when adding generated reasoning aliases.
Astra needs Responses for tool use. Gemini call IDs must survive parsing and
appear on both the replayed call and its response. Fable 5.1 needs adaptive
thinking and an explicit output limit; its patch requests summarized thinking
and the binding-controls beta so the API drops thinking invalidated by dynamic
prompts or compaction instead of rejecting the turn. The
[Fable migration guide](https://platform.claude.com/docs/en/models/fable-5-1/migration-guide)
also describes its conversation-history restrictions.

The shipped-agent integration test loads both packages, checks metadata and
provider coverage, enforces Codex/OpenAI ordering and the Opus/Bedrock policy,
and exercises the production retry loop with each provider available alone.
Those tests simulate authentication failures and do not make paid model calls.
