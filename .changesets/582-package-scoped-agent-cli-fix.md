---
harnx: patch
---
Fix `harnx -a <pkg>/<agent>` and `.agent <pkg>/<agent>` so they load the
package-scoped agent. Previously the loader dropped the package qualifier and
the agent reported its name as the bare stem, which meant the top-level agent
of the same stem appeared to be selected and per-package patches and manager
scoping were skipped.
