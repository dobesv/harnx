---
harnx: patch
---
Claude thinking aliases now match what each model accepts, and the model catalog drops models LiteLLM no longer lists.

- `:thinking` on Claude models that only accept adaptive thinking (Opus 4.7 and later and every 5.x model, on the Claude API, Vertex AI and Bedrock) sent a fixed `budget_tokens`, which those models reject. It now requests adaptive thinking, and their base models ask for readable thinking summaries.
- These models gain `:xhigh` and `:max` aliases on the Claude API and Vertex AI where supported, Opus 4.7 and later gain a `:thinking` alias, and Claude Fable 5.2 and Mythos 5.2 are added.
- Base models and `:thinking` no longer pin an effort level, so each model runs at its own default.
- Claude 3.x models no longer get a `:thinking` alias they can't use.
- Models LiteLLM stops listing or marks deprecated are removed, such as `claude-opus-4-1`, `gpt-3.5-turbo`, `o1`, `o3-mini` and `deepseek-chat`.
- xAI and Z.ai models, rerankers and Cohere's chat models are now refreshed from LiteLLM too, which adds current Grok and GLM models and updates Grok 4.20's prices.
- Models harnx's client can't call are no longer listed: Vertex AI's partner models such as Llama, gpt-oss and Grok, Vertex multimodal embeddings, and Bedrock rerankers.
