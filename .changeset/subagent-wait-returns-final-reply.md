---
harnx: minor
pantheon: patch
---
`session_wait` and `session_send` with `wait` now return the child's final reply instead of a page of its transcript.

A settled prompt's result carries the child's last reply as `response`, thinking stripped, the same text `session_prompt` returns, and a failed turn's error as `failure`. A reply too long for the 128 KiB result is cut short and marked `response_truncated: true`. Before, both tools returned up to 128 KiB of the turn read forward from the prompt, so a long child's reply was cut off and every repeated wait on a running child returned the same oldest events.

A wait that ends while the child is still running returns `activity` instead: the child's tool call count, token usage and title, and its latest tool step (the start of its narration and up to four calls with trimmed arguments). `include_partial: false` leaves it out.

The `read` option is removed from both wait tools; use `session_load` with `prompt_id` to read a prompt's transcript. Pantheon's delegation guidance describes the new result fields.
