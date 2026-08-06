---
harnx: patch
---
Fix request patches for the Opus 4.7/4.8 effort aliases and the `gpt-5.6-*:high` aliases, which silently did nothing. jaq (unlike jq) won't create a missing parent object for a nested path assignment, so `.body.output_config.effort = "high"` failed at runtime and took the rest of the patch with it — those models were sent `temperature`/`top_p` and no thinking or effort config.

A failing request patch is now reported as an error naming the patch source, the expression, and the jaq message, instead of only a `warn!` that needed debug logging to see.
