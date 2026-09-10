# harnx-k8s-sandbox-tools

`harnx-k8s-sandbox-tools` is a native Harnx tool gateway for Kubernetes
[Agent Sandbox](https://github.com/kubernetes-sigs/agent-sandbox) workloads.
It exposes the normal `bash_*` and `fs_*` tools plus `sandbox_connect`,
`sandbox_status`, and `sandbox_release` over Harnx's NATS tool protocol.

The gateway runs centrally. It creates or wakes a `SandboxClaim`, resolves the
sandbox pod IP, and forwards the call to an MCP endpoint inside that pod. The
sandbox does not connect to NATS and does not receive NATS credentials.

See the [Kubernetes sandbox gateway guide](../../docs/kubernetes-sandbox-tools.md)
for architecture, deployment, RBAC, networking, and lifecycle details.
