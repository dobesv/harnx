# acp_servers/

All agents defined in `agents/<name>.md` are **automatically registered** as ACP
servers using the current `harnx` binary with `args: ["--acp", "<name>"]`.

You only need to create files here when you want to **override** the default
auto-registration — for example, to use a different binary, add environment
variables, or set a custom description.

## Example: override an agent's timeout

```yaml
# acp_servers/my-agent.yaml
command: harnx
args: ["--acp", "my-agent"]
idle_timeout_secs: 600
operation_timeout_secs: 7200
description: "My custom agent with extended timeouts"
```

## Example: a non-harnx ACP server

```yaml
# acp_servers/external.yaml
command: /path/to/other-binary
args: ["--acp-mode"]
env:
  API_KEY: "your-key"
enabled: true
description: "External ACP server"
```
