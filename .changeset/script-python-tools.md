---
harnx: major
coding: patch
pantheon: patch
---
Python in scripts now calls tools as `tools.NAME(...)`, in the `bash` tool, in a script tool's `python:` body, and in middleware and hook scripts.

`call_tool(name, args)` is removed from every Python context, and a script that still uses it fails with `NameError`. Convert a call by passing the tool's schema properties as keywords: `call_tool("x", {"a": 1})` becomes `tools.x(a=1)`. Required properties may also be positional, a property name Python cannot spell is passed as `**{"from": 1}`, and a tool whose name is not an identifier or is chosen at run time is `getattr(tools, name)(**args)`. A script that can call tools cannot start with a `from __future__` import.

Upgrade the `coding` and `pantheon` packages together with harnx (`harnx-pkg update`). An installed older package on a newer harnx fails its `proxy-auth` middleware with `NameError: name 'call_tool' is not defined`, and because that middleware denies on error, every `bash_exec` and `bash_spawn` is refused until the packages are updated. A newer package on an older harnx fails the same way on `tools`.

A wrong tool name raises `AttributeError` and a wrong argument raises `TypeError`, before the tool is called. A keyword the tool's schema does not list is now a `TypeError`, unless the schema allows additional properties or lists no properties at all; `call_tool` passed any object through. In the `bash` tool a call returns what the tool's command prints with `-o json` as a dict, or its text as a string, and a tool failure raises `RuntimeError`. In script tool bodies, middleware and hooks a call returns what `call_tool` returned there. In middleware a tool's own error comes back as a result marked `is_error` (or `isError`) that the script must check, and only a call that cannot complete ends the run. `help <tool>` shows the call on a `PYTHON` line. The shipped `coding` and `pantheon` middleware are converted. The old limit of about 95 Python invocations per script no longer applies.
