---
harnx: minor
---
Separate durable interrupt acceptance from physical cleanup in tool protocol v4. Worker cleanup reconciliation survives turn completion and recovers retained stops after restart. Tool, MCP and sub-agent shutdown no longer hold execution control; late replies remain fenced. Deploy workers and tool servers together. Frontend early-return policy remains unchanged.
