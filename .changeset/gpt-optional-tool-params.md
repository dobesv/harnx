---
harnx: patch
---
GPT models on the Responses API (`openai` and `codex` clients) no longer fill optional tool parameters with placeholder values such as `""` or `0`. Each tool is now sent in strict mode: every property is listed as required, and optional ones also accept `null`. harnx removes those `null`s before the tool runs. Strict mode needs a schema it can express, so a tool that takes a free-form map, such as `bash_exec`'s `env` or the fetch tools' `headers`, is sent with `strict: false` and its schema unchanged.

Tool schemas now reach providers as the tool declared them. Before, harnx removed `null` from `type` lists and dropped `$ref`, `additionalProperties` and number bounds. That left `update_plan`'s `tasks` and `replace_in_content` with no visible fields. Local `$ref`s are inlined when a tool registers. Gemini now gets the full schema too, through `parametersJsonSchema` rather than the reduced subset `parameters` accepts.

When any model sends `null` for an optional parameter whose schema does not allow it, the parameter is treated as omitted.

The plans tools also accept placeholders when a model sends them anyway. A blank optional string, or `parent_issue: 0`, counts as omitted. `add_plan` no longer fails when a model sends both `body` and `content` with one of them empty. One consequence is that a blank string no longer clears a plan, task or note field. The `add_plan` schema now shows only `content` and the `update_plan` schema only `replace_content`. The synonyms `body` and `content` are still accepted.
