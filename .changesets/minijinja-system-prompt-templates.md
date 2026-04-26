---
harnx: minor
---
System prompt templates now use MiniJinja (Jinja2-compatible) syntax. Templates can reference `{{__os__}}`, `{{__shell__}}`, `{{__now__}}`, `{{__cwd__}}`, `{{__arch__}}`, `{{__locale__}}`, `{{__os_distro__}}`, `{{__os_family__}}`, and the full `{{agent.*}}` context object. User-defined agent variables are available as top-level template variables. Undefined template variables are now hard errors.

**Breaking change — migration required:** Existing prompts containing literal `{{ ... }}` that are not valid template expressions will now fail to render. Escape literal braces using `{% raw %}...{% endraw %}` or `{{ '{{' }}` / `{{ '}}' }}`.

**Note for maintainers:** The `harnx: minor` bump above assumes a 0.x version line where minor versions may contain breaking changes. If the project has reached 1.x, this change likely warrants a `major` bump — please adjust the header accordingly before releasing.
