---
harnx: patch
pantheon: minor
coding: minor
---
Move the shipped agents to GPT-6.2 Astra and Sol, Claude Fable 5.2 and the Gemini 4 Carbon, Neon and Hydrogen models.

Oracle and Plato use GPT-6.2 Astra at maximum effort, then Fable 5.2 and Gemini 4 Carbon. Every Sol 6.1 slot becomes Sol 6.2 at the same price. Gemini 3.8 Flash slots become Gemini 4 Hydrogen, which costs twice 3.8 Flash's introductory price until December 31, 2026 and the same afterwards. The other Gemini 4 Argon slots, including Minos, become Gemini 4 Neon.

The model catalog adds `:high` and `:max` aliases for `gpt-6.2-astra` and `gpt-6.2-sol` and drops their sampling parameters. Gemini 4 Carbon, Neon and Hydrogen now reserve one request's 65,536 output tokens inside their shared 1,048,576-token window instead of listing the whole window as the output limit.
