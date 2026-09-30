---
harnx: minor
---
Stop agents that keep repeating the same tool call. The 2nd to 4th identical call with an identical result within 10 minutes gets a note, the 5th is refused, and a model that keeps asking has its turn ended with a `repetition` stop, which parent agents and CLI one-shots receive as a termination. A repetition-stopped one-shot exits with code 2 and prints the termination JSON on stderr, like a token-budget stop. Configure with `loop_detection.tool_calls` or `HARNX_LOOP_DETECTION`. `harnx dump session --check-loop-detection` replays a stored session through the same rules. `time_wait` and `time_wait_until` now report their start and end times, and `bash_wait` reports how long a still-running process has been running.
