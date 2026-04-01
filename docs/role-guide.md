# Role Guide

## Role Definition

Roles are instrumental in customizing Large Language Model (LLM) behaviors, thereby enhancing user interactions and boosting overall productivity.

A role primarily consists of a name and a prompt, alongside several optional configurations:

- **`model`:** The preferred LLM (e.g. `openai:gpt-4o`).
- **`temperature`:** Controls the creativity and randomness of the LLM's response.
- **`top_p`:** Alternative way to control LLM's output diversity, affecting the probability distribution of tokens.
- **`use_tools`:** Tools attached to this role.

Below is an example of the `grammar-genie` role located at `<harnx-config-dir>/roles/grammar-genie.md`:

```
---
model: openai:gpt-4o
temperature: 0
top_p: 0

---
Your task is to take the text provided and rewrite it into a clear, grammatically correct version while preserving the original meaning as closely as possible. Correct any spelling mistakes, punctuation errors, verb tense issues, word choice problems, and other grammatical mistakes.
```

Just like `%functions%`, we can also create a role with only `use_tools` configuration that specify the functions to include when chatting.

```
---
use_tools: web_search,execute_command
---
```

## Types of Prompts

There are three types of prompts in Harnx:

### Embedded Prompt

- Contains `__INPUT__` placeholder, which gets replaced with your input.
- Ideal for concise, input-driven replies.

```yaml
- name: emoji
  prompt: convert __INPUT__ to emoji
```

Running `harnx -r emoji angry` would generate messages:
```json
[
  {"role": "user", "content": "convert angry to emoji"}
]
```

### System Prompt

- Does not include `__INPUT__`.
- Sets a general context for the LLM's behavior.

```yaml
- name: emoji
  prompt: convert my words to emoji
```

Running `harnx -r emoji angry` would generate messages:

```json
[
  {"role": "system", "content": "convert my words to emoji"},
  {"role": "user", "content": "angry"}
]
```

### Few-shot Prompt

- An extension of the system prompt, offering more precise instructions.
- Uses `### INPUT:` and `### OUTPUT:` to denote user and assistant messages.

```yaml
- name: code
  prompt: |-
    Provide only code without comments or explanations.
    ### INPUT:
    async sleep in js
    ### OUTPUT:
    ```javascript
    async function timeout(ms) {
      return new Promise(resolve => setTimeout(resolve, ms));
    }
```

Running `harnx -r code echo server in node.js` would generate messages:

```json
[
  {"role": "system", "content": "Provide only code without comments or explanations."},
  {"role": "user", "content": "async sleep in js"},
  {"role": "assistant", "content": "```javascript\nasync function timeout(ms) {\n  return new Promise(resolve => setTimeout(resolve, ms));\n}\n```"},
  {"role": "user", "content": "echo server in node.js"}
]
```

## Built-in Roles

Harnx includes these built-in roles:

- `%shell%`: Generates shell commands (used by `harnx -e`)
- `%explain-shell%`: Explains shell commands (used by `harnx -e` > `explain`)
- `%code%`: Generates code (used by `harnx -c`)
- `%functions%`: Attach function declarations of all tools (`use_tools: all`).

Built-in role names are always enclosed in `%...%`. You can override them by creating a role with the same name.
