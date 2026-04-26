---
harnx: minor
---
System prompt templates now use MiniJinja (Jinja2-compatible) syntax. Templates can reference `{{__os__}}`, `{{__shell__}}`, `{{__now__}}`, `{{__cwd__}}`, `{{__arch__}}`, `{{__locale__}}`, `{{__os_distro__}}`, `{{__os_family__}}`, and the full `{{agent.*}}` context object. User-defined agent variables are available as top-level template variables. Undefined template variables are now hard errors.
