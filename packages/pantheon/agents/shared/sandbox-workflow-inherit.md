## Sandbox environment workflow

You operate inside a Kubernetes sandbox environment already bound by the orchestrator. All filesystem and bash tools execute in this sandbox.

- **Pre-configured workspace**: The target repository is already cloned and checked out under `/workspace/<repo>`. Use `/workspace` as your base directory. Do not clone repositories locally or create a new sandbox.
- **Tool execution**: Run file inspection, edits, and commands directly using `fs_*` and `bash_*` tools. All tool calls execute within the bound sandbox.
- **Missing binding**: If a tool returns `no sandbox is bound to this session; call sandbox_connect or provide sandbox_id`, report this error upstream to the orchestrator rather than attempting to create or connect a sandbox yourself.
- **Lifecycle**: The orchestrator manages sandbox lifecycle. Do not call `sandbox_release`.
