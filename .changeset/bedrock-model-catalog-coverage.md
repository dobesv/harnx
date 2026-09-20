---
harnx: minor
---
Restore Bedrock coverage in the weekly `models.yaml` refresh, and move the
reasoning-heavy package agents onto Kimi K3 as their Bedrock fallback.

The updater recognised only LiteLLM's `bedrock` provider tag, but most of the
live Bedrock catalog is tagged `bedrock_converse` — the very API the Bedrock
client calls. A second filter then admitted only `us.`, `zai.` and `minimax.`
model ids. Between them the refresh yielded 18 legacy models, so every Bedrock
entry the packages actually select had been added by hand and its price frozen
ever since. Selecting on id shape rather than on a list of known vendors brings
the count to 117 and picks up Qwen3, Kimi K2.5, GLM 4.7, DeepSeek V3.2,
Nemotron, gpt-oss and the Claude 5 family on Bedrock. Capability flags that a
curated entry asserted and LiteLLM omits, such as vision on Llama 4, now
survive a refresh rather than silently switching off.

Two figures the registry states and the AWS model cards contradict are now
pinned with a citation: GLM 4.7 Flash caps output at 4K rather than the
reported 128K, and MiniMax M2.5 takes 196K of context rather than 1M. Both
were wrong in the direction that makes harnx ask for more than the model
accepts.

Kimi K3 (`us.moonshotai.kimi-k3`) is added by hand because LiteLLM carries no
Bedrock listing for it yet, and becomes the Bedrock fallback for Oracle, Plato,
Hephaestus, Daedalus, Sisyphus, Melpomene, Momus, Polyhymnia and Zosimus. It is
the only frontier-class open-weight model on Bedrock, and its 1M context and
vision close gaps the other Bedrock choices cannot. It costs about three times
GLM 5 on input and five times on output, which is acceptable only because it
sits last in every chain selecting it; the mid-tier agents keep GLM 5.

AWS documents Kimi K3 rejecting a Converse request that replays earlier
reasoning. That does not reach harnx: the model returns reasoning without a
signature, so the existing signature gate already omits the block, and both
packages reach Bedrock through the OpenAI-compatible endpoint, which never
replays reasoning at all. Both paths were verified with two-turn tool-calling
sessions against a live account.
