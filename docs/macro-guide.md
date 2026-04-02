# Macro Guide

Macros are predefined sequences of REPL commands that automate repetitive tasks or workflows, enabling efficient execution of a series of commands with customizable variables and isolated execution contexts.

## Macro Definition

Macros are defined using YAML files stored in the `<harnx-config-dir>/macros/` directory. Each YAML file represents a single macro, and the filename (excluding the `.yaml` extension) serves as the macro's name.

The YAML file for a macro consists of two main sections:

- **`steps`** (Required):
   An array of strings, where each string represents a single REPL command to be executed in sequence. These commands must be valid REPL commands.

- **`variables`** (Optional):
   An array of variable definitions that can be used within the macro. Each variable has the following properties:
   - **`name`** (Required): The name of the variable, which can be referenced in the macro steps using the `{{name}}` syntax.
   - **`default`** (Optional): A default value for the variable. If no value is provided during execution, the default will be used. If no default is specified and no value is provided, an error will occur.
   - **`rest`** (Optional, Boolean): When set to `true`, this variable will collect all remaining arguments into a single variable. This is only applicable to the last variable in the list. The default value is `false`.

## Macro Examples

```yaml
# <harnx-config-dir>/macros/generate-commit-message.yaml
steps:
  - .file `git diff` -- generate git commit message
```

```yaml
# <harnx-config-dir>/macros/within-agent.yaml
variables:
  - name: agent
  - name: args
    rest: true
    default: What can your do?
steps:
  - .agent {{agent}}
  - '{{args}}'
```

```yaml
# <harnx-config-dir>/macros/multi-agents.yaml
variables:
  - name: args
    rest: true
steps:
  - .agent design-tui
  - '{{args}}'
  - .agent build-tui
  - .file %%
  - .agent fixing-bugs
  - .file %%
```

## Macro Execution

When a Macro is executed, it runs in an **isolated context**:

- It does **not** inherit any pre-existing agent, session, or RAG state.
- Executing a Macro will **not** affect your current context. This ensures that your workflow remains clean and unaffected by Macro operations.
