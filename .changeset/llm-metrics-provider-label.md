---
harnx: minor
---
Add a `provider` label to the `harnx_llm_tokens_total` and `harnx_llm_cost_dollars` metrics carrying the canonical backend kind (`openai`, `claude`, `bedrock`, `openai-compatible`, …), resolved from the selected model's configured client. The existing `client` label is unchanged; `provider` is additive and empty (`""`) when the client cannot be resolved. Note for dashboard operators: adding a label starts new Prometheus time series, so series that existed before the upgrade stop updating; queries that don't group by `provider` are unaffected.
