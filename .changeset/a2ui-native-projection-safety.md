---
harnx: patch
---
Keep user-only A2UI resources out of model output when a sibling surface is invalid. Reject cyclic, overly deep, or excessively expanded component graphs, and safely truncate old invalid graphs in text output. Redact obscured text-field defaults and resolve data-model-bound text and input labels in the text fallback.
