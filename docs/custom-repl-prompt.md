# Custom REPL Prompt

The REPL prompt displays context information about the active role, session, RAG, and agent.

## Prompt States

The prompt adapts to show relevant context depending on what's active:

- none
- role
- session
- agent
- rag
- session + role
- session + rag
- session + role + rag
- agent + session
- agent + rag
- agent + session + rag

## Configuration

Edit `left_prompt` and `right_prompt` in your config file to customize the REPL prompt.

### Default left_prompt

```
'{color.green}{?session {?agent {agent}>}{session}{?role /}}{!session {?agent {agent}>}}{role}{?rag @{rag}}{color.cyan}{?session )}{!session >}{color.reset} '
```

### Default right_prompt

```
'{color.purple}{?session {?consume_tokens {consume_tokens}({consume_percent}%)}{!consume_tokens {consume_tokens}}}{color.reset}'
```

## Syntax

| Syntax | Description |
|---|---|
| `{var}` | Replace with the value of `var` |
| `{?var template}` | Evaluate `template` when `var` is truthy |
| `{!var template}` | Evaluate `template` when `var` is falsy |

## Variables

| Variable | Description |
|---|---|
| `model` | Current model |
| `client_name` | Client name |
| `model_name` | Model name |
| `max_input_tokens` | Max input tokens |
| `temperature` | Temperature |
| `top_p` | Top P |
| `dry_run` | Dry run mode |
| `stream` | Stream mode |
| `save` | Save mode |
| `wrap` | Wrap mode |
| `role` | Active role |
| `session` | Active session |
| `dirty` | Session dirty flag |
| `consume_tokens` | Consumed tokens |
| `consume_percent` | Consumed token percentage |
| `user_messages_len` | Number of user messages |
| `rag` | Active RAG |
| `agent` | Active agent |

## Color Variables

All `color.*` variables map to ANSI color codes:

| Variable | Variable |
|---|---|
| `color.reset` | `color.black` |
| `color.dark_gray` | `color.red` |
| `color.light_red` | `color.green` |
| `color.light_green` | `color.yellow` |
| `color.light_yellow` | `color.blue` |
| `color.light_blue` | `color.purple` |
| `color.light_purple` | `color.magenta` |
| `color.light_magenta` | `color.cyan` |
| `color.light_cyan` | `color.white` |
| `color.light_gray` | |
