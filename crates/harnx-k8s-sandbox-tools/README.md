# harnx-k8s-sandbox-tools

`harnx-k8s-sandbox-tools` is a native Harnx tool gateway for Kubernetes
[Agent Sandbox](https://github.com/kubernetes-sigs/agent-sandbox) workloads.
It exposes the normal `bash_*` and `fs_*` tools plus `sandbox_connect`,
`sandbox_status`, and `sandbox_release` over Harnx's NATS tool protocol.

The gateway runs centrally. It creates or wakes a `SandboxClaim`, resolves the
sandbox pod IP, and forwards the call to an MCP endpoint inside that pod. The
sandbox does not connect to NATS and does not receive NATS credentials.

Use `--bash-name`, `--fs-name`, and `--sandbox-name` to override the three
registered toolset names. These options do not change the in-sandbox MCP
endpoints used for forwarding.

## Attachment handling

When an agent calls `fs_write_attachment(path, url)` on a sandbox:

1. The gateway resolves the `cid:media:` or `cid:plan:` URL centrally using its own NATS JetStream connection, up to the 100 MiB limit.
2. The gateway encodes the resolved bytes as standard base64.
3. The gateway invokes the pod's private `write_bytes(path, data_base64)` MCP tool over HTTP.

Sandbox pods have no NATS credentials and never connect to NATS. The pod filesystem server must run with `--sandbox-transport` (or `HARNX_FS_SANDBOX_TRANSPORT=1`), which exposes `write_bytes` and raises the HTTP body limit to 140,858,712 bytes ≈ 134.3 MiB (base64 of the 100 MiB file cap plus 1 MiB envelope). `--sandbox-transport` (or `HARNX_FS_SANDBOX_TRANSPORT=1`) works only with `--mcp-http`; startup rejects it in NATS-hosted and MCP stdio modes. If a pod lacks this flag, the gateway returns a recoverable error advising the operator to enable `--sandbox-transport` in the sandbox image.

See the [Kubernetes sandbox gateway guide](../../docs/kubernetes-sandbox-tools.md)
for architecture, deployment, RBAC, networking, and lifecycle details.
