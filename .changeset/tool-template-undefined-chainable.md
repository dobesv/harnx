---
harnx: patch
---
Stop logging spurious `template error in tool '...' result_template: undefined value` warnings (#1537).

Tool call/result display templates rendered under MiniJinja's Lenient undefined mode, which still raises on attribute/index access into an undefined intermediate. The shared plans result template `{{ result.content[0].text | default('') }}` walks into `result.content`, which is absent on recoverable-error results (`{"is_error": true, "error": ...}`), so it raised before `default('')` could apply and every plans/time tool logged a warning on its error path. Templates now render with Chainable undefined behavior so `default()` is honored; syntax errors and other hard failures still surface.
