---
harnx: minor
---

Enable embedded Monty Python in script mode. `python3` and `python` can process
virtual `/tmp` files and read `/attachments`, without host filesystem, network,
or tool-call access. Apply per-run Python memory and time limits, and stop busy
loops on cancellation without blocking the agent executor.

Known risks: Python memory limits (64 MiB) and timeouts are cooperative VM limits,
not hard process isolation. Under BashKit 0.20.2 / Monty 0.0.19, certain inputs can
crash or OOM the entire host worker process. Known failure examples include:
`json.dumps(0, indent=10**9)` aborts with SIGABRT (~159 MiB at indent 134217728),
returning deeply shared list graphs (`x=[x,x]` ×30) expands to ~500 MiB during export
ignoring timeouts, `pass; import re` bypasses the import gate (~302 MiB via `re.split`),
parser recursion on long expression chains, and untracked native allocations in
`str`/`bytes` `join`/`split`. A shared work budget also caps scripts at ~95 Python calls.
Python cannot be disabled separately in this version. Use script mode with Python only
where a crashed worker is acceptable, or leave `bash_tools` unset for untrusted prompts.
