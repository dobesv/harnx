---
harnx: minor
---
Stop a model that streams the same text over and over. When the answer or the reasoning ends in at least 2,000 characters of one repeated piece of text, harnx stops the response, retries once with a note telling the model what happened, then tries the next fallback model, and finally ends the turn with a `repetition` stop (`source` `answer` or `thinking`). Turn it off with `loop_detection.output`. `harnx dump session --check-loop-detection` now also reports repeated replies.
