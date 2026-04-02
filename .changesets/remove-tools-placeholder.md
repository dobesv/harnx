---
harnx: minor
---
Removed the `{{__tools__}}` placeholder mechanism from agent prompts. Tool descriptions are now automatically injected after the system prompt when tools are available, without requiring any special placeholder in the agent markdown file. This also fixes a bug where the placeholder replacement would permanently bake tool text into saved agent files.
