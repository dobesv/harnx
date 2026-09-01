---
harnx: minor
---

Add cached-token cost accounting. Every provider now normalizes token usage to
the OpenTelemetry-subset convention (`input_tokens` includes cache tokens;
cache-read and cache-write are subsets), and cost is computed with a single
formula that prices uncached input, cache-read, cache-write, and output
separately. Cache prices (`cache_read_price`/`cache_write_price`) are
auto-generated from LiteLLM. Prometheus gains `harnx_llm_tokens_total{type=cache_read|cache_write}`
and cache-inclusive `harnx_llm_cost_dollars`; OTel spans gain
`gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_write.input_tokens`,
and `harnx.gen_ai.cost.usd`. A per-model `cache_accounting: subset|disjoint`
flag lets an OpenAI-compatible proxy fronting a disjoint backend be priced
correctly.
