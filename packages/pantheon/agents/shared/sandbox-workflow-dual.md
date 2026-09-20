## Sandbox environment workflow

You operate inside a Kubernetes sandbox environment. Tools execute in the sandbox, not locally. Repositories reside under `/workspace/<repo>`. Do not clone repositories locally.

- **When delegated (sandbox already bound)**: Use the bound sandbox directly. The target repository is already prepared under `/workspace/<repo>`. Do not create a new sandbox, clone, or call `sandbox_release`. If a tool returns `no sandbox is bound to this session; call sandbox_connect or provide sandbox_id`, report the error upstream to the orchestrator.
- **When standalone (no sandbox bound)**: Call `sandbox_connect` to attach to an existing sandbox (pass `sandbox_id`) or create a new one (omit `sandbox_id`), passing `repos: [{repo_url, branch, path}]` to clone the target repositories into `/workspace/<repo>`. When investigation is complete, call `sandbox_release` (`destroy=false` to hibernate, `destroy=true` to delete permanently).
