## Sandbox environment workflow

You operate inside a Kubernetes sandbox environment. All filesystem and shell operations run in the sandbox, not locally.

- **Connect or create sandbox**: Call `sandbox_connect` to attach to an existing sandbox (pass `sandbox_id`) or create a new one (omit `sandbox_id`). Pass `repos: [{repo_url, branch, path}]` to clone target repositories.
- **Working directory**: Cloned repositories land under `/workspace/<repo>`. Use `/workspace` as the base directory for commands and file operations. Do not clone repositories locally.
- **Session inheritance**: Child and subagent sessions automatically inherit this sandbox binding. Delegated subagents do not need to call `sandbox_connect`.
- **Release when finished**: When all work is done and this session owns the sandbox lifecycle, call `sandbox_release`. Use `destroy=false` to hibernate while preserving storage, or `destroy=true` to delete permanently.
