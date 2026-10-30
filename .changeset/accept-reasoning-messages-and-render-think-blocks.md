---
harnx: minor
---

Accept AG-UI messages with non-standard roles such as `reasoning`. The server now ignores unknown roles instead of rejecting them with a 400 error. TUI, CLI `prompt` on a TTY, and Web UI now render thinking content in a distinct style instead of showing literal `<think>` tags. Web UI shows live reasoning in a collapsible Thinking block.