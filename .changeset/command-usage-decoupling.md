---
"harnx": patch
---

fix(commands): decouple `Command.name` from usage hints to fix tab-completion

Commands whose registered name embedded usage syntax (e.g. `.rewind <n>`,
`.edit message <n>`, `.info env [name]`) tab-completed to the literal usage
string instead of a real command/subcommand.

- Add a dedicated `Command.usage: Option<&'static str>` field plus a
  `Command::with_usage()` constructor so `name` stays a clean dispatch/
  completion key while `.help` still renders the argument syntax.
- Split placeholder usage out of the affected command names.
- Deduplicate first-word completions by bare name in the TUI.
- Add real subcommand completions for `.edit` and `.delete`.
- Add regression tests asserting completions never surface `<n>`, `[server]`,
  `[name]`, or `<n>-<m>` literals.
