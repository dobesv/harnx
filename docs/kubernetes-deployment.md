# Deploy Harnx on Kubernetes

This guide covers deploying the Harnx stack on Kubernetes: `harnx-serve` (HTTP UI and API), `harnx-worker` (session execution and LLM orchestration), NATS JetStream (messaging fabric), and native tool servers.

## Architecture

```text
Browser / API client
        |
        v
  [harnx-serve] (HTTP UI :8000, embeddings/rerank API)
        |
        v
  [NATS JetStream] (session state, work queues, tool registry)
        ^
        | leases sessions & executes turns
  [harnx-worker] (scalable pool, --cluster prod)
        |
        v calls via NATS
  [tool servers] (harnx-time-tools, harnx-k8s-sandbox-tools, etc.)
```

`harnx-serve` does not run LLM loops. Instead, it accepts client requests and coordinates with NATS JetStream. One or more `harnx-worker` pods lease active sessions over NATS and execute agent turns. Workers call tool servers that register themselves as NATS consumers in a shared scope.

---

## Prerequisites

- **NATS 2.11+ with JetStream enabled** (mandatory). Harnx relies on JetStream key-value buckets, work-queue streams, and object stores.
- **Configuration directory (`HARNX_CONFIG_DIR`)**: Contains `config.yaml`, plus subdirectories `clients/`, `agents/`, `tool_servers/`, and `nats_servers/` (e.g., `nats_servers/prod.yaml`). Typically mounted into pods via a ConfigMap.
- **Secrets (`HARNX_ENV_FILE` or environment variables)**: Model provider API keys (such as `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`), mounted from Kubernetes Secrets.
- **Container images**:
  - `ghcr.io/dobesv/harnx:<version>`: All-in-one image containing workspace binaries and baked Web UI static assets.
  - `ghcr.io/dobesv/harnx-web-assets:<version>`: Minimal OCI image (`FROM scratch`) containing only the Web UI bundle at `/web-assets`.

---

## NATS Connection Configuration

All Harnx components communicate over NATS. Workers and standalone tool servers connect directly using `HARNX_NATS_URL` and `HARNX_NATS_TOKEN` from their environment. Front-ends (`harnx-serve`) connect as cluster clients by setting `HARNX_NATS_SERVER=<name>` pointing to a cluster definition under `nats_servers/<name>.yaml`, which can expand `${HARNX_NATS_URL}` and `${HARNX_NATS_TOKEN}`. For high-availability JetStream clusters, set `HARNX_NATS_REPLICAS=3`.

For TLS or mTLS clusters, set the corresponding TLS environment variables:
- `HARNX_NATS_TLS=true`
- `HARNX_NATS_TLS_CA=/etc/harnx/certs/ca.crt`
- `HARNX_NATS_TLS_CERT=/etc/harnx/certs/tls.crt`
- `HARNX_NATS_TLS_KEY=/etc/harnx/certs/tls.key`

`HARNX_NATS_URL` also accepts `ws://` and `wss://`, which is what reaches a
broker behind an HTTP load balancer such as an mTLS-gated AWS ALB. Peers the
broker advertises are ignored on those schemes, since they address the cluster
directly; `HARNX_NATS_IGNORE_DISCOVERED_SERVERS` overrides that either way. See
[NATS HA deployment](nats-ha.md) for the broker-side listener and health-check
configuration.

Harnx automatically creates the required JetStream streams and KV buckets on connect. See [NATS HA deployment](nats-ha.md) for cluster topology and stream replication settings.

---

## harnx-serve Deployment

`harnx-serve` provides the Web UI and HTTP endpoints (`/v1/embeddings`, `/v1/rerank`).

In Kubernetes, pass `--addr 0.0.0.0:8000` so pods and ingress controllers can reach the server (it defaults to `127.0.0.1:8000`).

Set `HARNX_SERVE_PUBLIC_URL` to the address users open the Web UI at, such as `https://harnx.example.com`. harnx-serve records each session's Web UI address so agents can link to it; without this setting it infers the address from each request's `X-Forwarded-Host`, `X-Forwarded-Proto` and `Host` headers, which only works when the ingress forwards the original host. Setting it later corrects the addresses inferred before.

To run `harnx-serve` against your external cluster, set `HARNX_NATS_SERVER=<name>` (e.g. `remote`) and mount `nats_servers/<name>.yaml` into the configuration directory (`HARNX_CONFIG_DIR`). The server reads connection parameters from that file, which can use `${HARNX_NATS_URL}` and `${HARNX_NATS_TOKEN}` expansion.

Setting only `HARNX_NATS_URL` and `HARNX_NATS_TOKEN` on a front-end no longer joins the cluster. When `HARNX_NATS_SERVER` is unset, `harnx-serve` treats sessions as local (`__local__`), ignores operator `HARNX_NATS_URL` and `HARNX_NATS_TOKEN` for its own routing, and self-hosts a local broker instead. In container environments without `nats-server` installed, that startup fails. Even if a local broker binary were present, sessions would stay confined to the pod instead of routing to your shared `harnx-worker` pool.

### Cluster configuration ConfigMap

The `config-volume` mounts `/etc/harnx/config` from the `harnx-config` ConfigMap. Provide `nats_servers/remote.yaml` inside that directory (matching `HARNX_NATS_SERVER=remote`). Using `${VAR}` expansion keeps credentials in secrets:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: harnx-config
data:
  # Mounts into /etc/harnx/config/nats_servers/remote.yaml
  remote.yaml: |
    url: "${HARNX_NATS_URL}"
    token: "${HARNX_NATS_TOKEN}"
```

Keep `HARNX_NATS_URL` and `HARNX_NATS_TOKEN` in `env:` on the deployment: `remote.yaml` expands them at connection time, and child worker/tool-server processes still rely on them for transport.

### Deployment and Service manifest (Baked image)

The standard image `ghcr.io/dobesv/harnx:<version>` packages the Web UI at `/usr/local/share/harnx/web-assets` and sets `ENV HARNX_WEB_ASSETS=/usr/local/share/harnx/web-assets`. It serves the UI immediately with no extra volume mounts.

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: harnx-serve
  labels:
    app: harnx-serve
spec:
  replicas: 1
  selector:
    matchLabels:
      app: harnx-serve
  template:
    metadata:
      labels:
        app: harnx-serve
    spec:
      containers:
        - name: harnx-serve
          image: ghcr.io/dobesv/harnx:0.1.0
          command:
            - harnx-serve
            - --addr
            - 0.0.0.0:8000
            - --healthz-addr
            - :8081
            - --metrics-addr
            - :8456
          env:
            - name: HARNX_NATS_SERVER
              value: remote
            - name: HARNX_NATS_URL
              value: nats://nats.default.svc.cluster.local:4222
            - name: HARNX_NATS_TOKEN
              valueFrom:
                secretKeyRef:
                  name: harnx-secrets
                  key: nats-token
            - name: HARNX_CONFIG_DIR
              value: /etc/harnx/config
          ports:
            - containerPort: 8000
              name: http
            - containerPort: 8081
              name: healthz
            - containerPort: 8456
              name: metrics
          readinessProbe:
            httpGet:
              path: /healthz
              port: 8081
            initialDelaySeconds: 2
            periodSeconds: 5
          volumeMounts:
            - name: config-volume
              mountPath: /etc/harnx/config
              readOnly: true
      volumes:
        - name: config-volume
          configMap:
            name: harnx-config
            items:
              - key: remote.yaml
                path: nats_servers/remote.yaml
---
apiVersion: v1
kind: Service
metadata:
  name: harnx-serve
  labels:
    app: harnx-serve
spec:
  type: ClusterIP
  selector:
    app: harnx-serve
  ports:
    - name: http
      port: 8000
      targetPort: 8000
    - name: metrics
      port: 8456
      targetPort: 8456
```

---

## Web UI Assets: Two Deployment Paths

### Path 1: Baked image (simplest, works on any Kubernetes version)

Run `ghcr.io/dobesv/harnx:<version>`. The Web UI files are baked into `/usr/local/share/harnx/web-assets` and `HARNX_WEB_ASSETS` is preset in the image environment. No extra configuration, volumes, or init containers are required.

### Path 2: OCI image volume (Kubernetes 1.33+, split assets)

On Kubernetes clusters with the `ImageVolume` feature gate enabled (beta in Kubernetes 1.33+), you can mount `ghcr.io/dobesv/harnx-web-assets:<version>` directly as a read-only OCI image volume.

The standalone assets image is built `FROM scratch` containing only static files at `/web-assets`. Because Kubernetes mounts the image filesystem directly into the container, no shell, utilities, init containers, or copy steps are needed:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: harnx-serve-image-volume
  labels:
    app: harnx-serve
spec:
  replicas: 1
  selector:
    matchLabels:
      app: harnx-serve
  template:
    metadata:
      labels:
        app: harnx-serve
    spec:
      containers:
        - name: harnx-serve
          image: ghcr.io/dobesv/harnx:0.1.0
          command:
            - harnx-serve
            - --addr
            - 0.0.0.0:8000
            - --healthz-addr
            - :8081
            - --metrics-addr
            - :8456
          env:
            - name: HARNX_WEB_ASSETS
              value: /web-assets
            - name: HARNX_NATS_SERVER
              value: remote
            - name: HARNX_NATS_URL
              value: nats://nats.default.svc.cluster.local:4222
            - name: HARNX_NATS_TOKEN
              valueFrom:
                secretKeyRef:
                  name: harnx-secrets
                  key: nats-token
            - name: HARNX_CONFIG_DIR
              value: /etc/harnx/config
          ports:
            - containerPort: 8000
              name: http
            - containerPort: 8081
              name: healthz
            - containerPort: 8456
              name: metrics
          readinessProbe:
            httpGet:
              path: /healthz
              port: 8081
            initialDelaySeconds: 2
            periodSeconds: 5
          volumeMounts:
            - name: web-assets-volume
              mountPath: /web-assets
              readOnly: true
            - name: config-volume
              mountPath: /etc/harnx/config
              readOnly: true
      volumes:
        - name: web-assets-volume
          image:
            reference: ghcr.io/dobesv/harnx-web-assets:0.1.0
            pullPolicy: IfNotPresent
        - name: config-volume
          configMap:
            name: harnx-config
            items:
              - key: remote.yaml
                path: nats_servers/remote.yaml
```

---

## harnx-worker Deployment (Scalable Execution Pool)

`harnx-worker` processes execute agent turns, run tool loops, and manage model communication. Workers connect to NATS JetStream, join a shared work queue (`WORK_NOTIFY_<cluster>`), and lease sessions via the `harnx_leases` key-value bucket. Multiple worker pods can run concurrently.

### Critical configuration requirements

A Kubernetes worker pod requires three distinct configuration settings:

1. **`--cluster prod`**: Specifies the session connection cluster profile (`nats_servers/prod.yaml`) used for leases, session logs, and worker coordination.
2. **`HARNX_NATS_URL` and `HARNX_NATS_TOKEN`**: **Mandatory in the pod environment.** Tool and hook discovery is a separate connection that does not read `nats_servers/<cluster>.yaml`. It resolves connection settings strictly from the worker's own process environment. If these variables are omitted, session execution connects, but tool and hook discovery fails.
3. **`HARNX_SERVER_SCOPE=shared`**: Workers and independently deployed tool servers must share the same scope identifier to discover one another.

In Kubernetes, leave `--manage-servers` off (the default). Workers do not spawn local subprocesses for tools; tool servers run in their own containers.

### Worker Deployment manifest

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: harnx-worker
  labels:
    app: harnx-worker
spec:
  replicas: 3
  selector:
    matchLabels:
      app: harnx-worker
  template:
    metadata:
      labels:
        app: harnx-worker
    spec:
      containers:
        - name: harnx-worker
          image: ghcr.io/dobesv/harnx:0.1.0
          command:
            - harnx-worker
            - --cluster
            - prod
            - --worker-id
            - $(POD_NAME)
            - --healthz-addr
            - :8081
            - --metrics-addr
            - :8456
          env:
            - name: POD_NAME
              valueFrom:
                fieldRef:
                  fieldPath: metadata.name
            - name: HARNX_SERVER_SCOPE
              value: shared
            - name: HARNX_NATS_URL
              value: nats://nats.default.svc.cluster.local:4222
            - name: HARNX_NATS_TOKEN
              valueFrom:
                secretKeyRef:
                  name: harnx-secrets
                  key: nats-token
            - name: HARNX_CONFIG_DIR
              value: /etc/harnx/config
            - name: ANTHROPIC_API_KEY
              valueFrom:
                secretKeyRef:
                  name: harnx-secrets
                  key: anthropic-api-key
          ports:
            - containerPort: 8081
              name: healthz
            - containerPort: 8456
              name: metrics
          readinessProbe:
            httpGet:
              path: /healthz
              port: 8081
            initialDelaySeconds: 5
            periodSeconds: 10
          volumeMounts:
            - name: config-volume
              mountPath: /etc/harnx/config
              readOnly: true
      volumes:
        - name: config-volume
          configMap:
            name: harnx-config
```

### Customizing agent packages (pantheon.patch.yaml)

When running agent packages such as Pantheon in Kubernetes, agents often need cluster-specific tools, proxy settings, or git credential instructions. The package provides a cluster-agnostic baseline, while environment customizations live in a patch file sibling to the package directory: `<config_dir>/packages/pantheon.patch.yaml`.

To apply this patch in Kubernetes, mount the patch file into the `harnx-worker` container via a ConfigMap volume:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: harnx-pantheon-patch
data:
  pantheon.patch.yaml: |
    agents:
      - 'if .name == "atlas" then .use_tools += ["cluster_k8s_tools"] end'
      - 'if .name == "clio" then .prompt += "\n\n## Cluster git\nUse the credentials mounted at /etc/git-creds/helper." end'
```

Add the volume and mount to your `harnx-worker` deployment:

```yaml
          volumeMounts:
            - name: config-volume
              mountPath: /etc/harnx/config
              readOnly: true
            - name: pantheon-patch
              mountPath: /etc/harnx/config/packages/pantheon.patch.yaml
              subPath: pantheon.patch.yaml
              readOnly: true
      volumes:
        - name: config-volume
          configMap:
            name: harnx-config
        - name: pantheon-patch
          configMap:
            name: harnx-pantheon-patch
```

See [Customizing pantheon agents for your environment](../packages/pantheon/README.md#customizing-pantheon-agents-for-your-environment) in `packages/pantheon/README.md` for full patch syntax, filter examples, and variable override rules.

---

## Timing receipt compatibility cutover and rollback

Use this procedure for the release that introduces durable `[Runtime note] ⏱ v1`
User-role timing receipts (`timing-v1-…` message IDs, issue #2411). **This is a
coordinated hard cutover, not a rolling upgrade.** Writers are always on; there
is no setting that makes a mixed old/new execution fleet safe.

Pre-receipt workers treat these rows as executable prompts and can fail with
`refusing executable prompt without durable run admission`. Draining active
turns does not remove receipts from completed sessions. Compaction, process
restart, lease expiry and matching frontend/worker readiness protocols do not
make an old reader compatible. Never grant admissions to receipts to bypass
this check. Existing admissions and deadlines must remain unchanged.

### Scope and release gates

The commands below use the `harnx-serve` and `harnx-worker` Deployments and labels
from this guide. They require Bash, kubectl and jq. Run them only during an
approved maintenance window; they stop service. They do not stop NATS or tool
servers and must not delete streams, consumers, KV admissions or session logs.

Before running them, record in the release ticket:

- NATS account/cluster and **every** process that can access its histories:
  worker pools in all namespaces, CLI/TUI clients, `harnx-serve`, MCP/A2A
  backends, jobs and frontend-owned local worker children. A different cluster
  profile name does not isolate processes using the same account and logs.
- Desired replica counts, current manifests/image digests, and the exact new
  image digest built from the receipt-compatible release (this implementation
  includes `harnx-core::timing_identity` and its worker reader filters). Keep
  that artifact available as the minimum compatible recovery image. A
  pre-#2411 image is **not** a rollback candidate after first receipt emission.
- Maintenance access restrictions that prevent external clients from reconnecting
  and prevent old binaries from restarting. Pause GitOps/release automation,
  HPA/KEDA and scheduled jobs for the inventory. Restrict deployment rights to
  the release operator until the gates pass. Keep old images out of approved
  deployment manifests after cutover. If these controls or the inventory cannot
  be established, **abort before starting any new worker**.
- Passing isolated verification below on the exact release source, and a staging
  receipt-bearing session check using the chosen image. Readiness alone is not
  a semantic compatibility test.

For fleets beyond these two Deployments, repeat the stop/absence/image gates
for every inventoried workload before starting any of them. Do not copy the
two-Deployment example unchanged into a larger fleet.

### Full-stop upgrade

1. Close public ingress and all other prompt submission paths using the site's
   maintenance controls. Let admitted turns reach durable completion or use
   existing cancellation controls and confirm the durable stop. Do not infer
   task drain from closed HTTP connections. Follow [A2A operations](a2a-operations.md#upgrade-without-mixed-writers)
   for A2A work. Record unresolved work; never extend its frozen deadline or
   replay tool side effects to make the release pass.
2. Stop **all** inventoried frontends and workers, including external clients and
   supervised children. For this guide's two-Deployment topology:

   ```bash
   set -euo pipefail
   : "${NS:?set the target Kubernetes namespace}"
   : "${COMPAT_IMAGE:?set ghcr.io/dobesv/harnx@sha256:<approved digest>}"
   [[ "$COMPAT_IMAGE" =~ ^ghcr\.io/dobesv/harnx@sha256:[0-9a-f]{64}$ ]]
   kubectl -n "$NS" get deploy harnx-serve harnx-worker -o yaml > timing-cutover-before.yaml
   kubectl -n "$NS" scale deploy/harnx-serve deploy/harnx-worker --replicas=0
   kubectl -n "$NS" wait --for=delete pod -l 'app in (harnx-serve,harnx-worker)' --timeout=300s
   test "$(kubectl -n "$NS" get pods -l 'app in (harnx-serve,harnx-worker)' -o json | jq '.items | length')" -eq 0
   ```

   **Stop gate:** every old process is gone, not merely unready or draining;
   automated/external restarts remain blocked. Record the empty pod list and
   equivalent evidence for the rest of the inventory. Do not proceed on a wait
   timeout. Scaling only shared workers is insufficient if a frontend can
   respawn an old child.
3. With replicas still zero, update each runtime image. Do not apply saved old
   manifests or use `kubectl rollout undo` against the pre-receipt release.

   ```bash
   kubectl -n "$NS" set image deploy/harnx-worker harnx-worker="$COMPAT_IMAGE"
   kubectl -n "$NS" set image deploy/harnx-serve harnx-serve="$COMPAT_IMAGE"
   for name in harnx-worker harnx-serve; do
     test "$(kubectl -n "$NS" get deploy "$name" -o jsonpath='{.spec.replicas}')" -eq 0
     test "$(kubectl -n "$NS" get deploy "$name" -o jsonpath='{.spec.template.spec.containers[0].image}')" = "$COMPAT_IMAGE"
   done
   ```

4. Start one compatible worker with submission paths still closed. Queued
   activations can emit receipts immediately, so this is the compatibility
   point of no return for old readers, even before reopening ingress.

   ```bash
   kubectl -n "$NS" scale deploy/harnx-worker --replicas=1
   kubectl -n "$NS" rollout status deploy/harnx-worker --timeout=300s
   kubectl -n "$NS" get pods -l app=harnx-worker -o json |
     jq -e --arg image "$COMPAT_IMAGE" '.items | length > 0 and all(.[]; all(.spec.containers[]; .image == $image))'
   kubectl -n "$NS" get pods -l app=harnx-worker -o wide
   ```

   Record running image IDs (`kubectl get pods -o json`) and worker logs.
   Use only upgraded operator clients for the receipt-session probe below.
   If it fails, leave ingress closed and follow retained-reader rollback.
5. Restore the recorded worker/frontend replica counts using the **updated**
   manifests, wait for rollout readiness, and verify every runtime image against
   the approved digest. Reopen submission paths only after the inventory and
   receipt-session gates pass. Resume automation only with updated manifests
   and image restrictions; do not let it restore an old ReplicaSet or CLI image.

For local installations, close every frontend sharing the broker and confirm
its child has exited before replacing binaries. Upgrade `harnx`, `harnx-serve`
and `harnx-worker` together, plus other runtime backends in the inventory.
Check `HARNX_WORKER_BIN`, the frontend's sibling binary and PATH; a stale override
can start an old worker despite an upgraded frontend. Restart frontends only
after all those paths resolve to compatible artifacts. See [worker binary resolution](../crates/harnx-worker/README.md#frontend-managed-local-workers).

### Rollback: retain compatible readers

Once a new worker has started, assume the whole account contains receipts.
**Do not downgrade its runtime readers to a pre-receipt binary, even when all
sessions are idle.** There is no supported in-place receipt-stripping repair
or automatic quarantine in this release.

1. Close submission paths and stop automation again. Drain/cancel and stop the
   runtime fleet using the same full-stop and absence gates above.
2. Revert only configuration or surrounding infrastructure that works with the
   compatible runtime. Keep `COMPAT_IMAGE` pinned to the tested receipt-compatible
   artifact, or use a forward-fixed compatible image after the same tests. On
   the first receipt release, retaining this runtime may be the only rollback
   option. If it cannot run safely, keep service stopped while preparing a fix.
3. Reapply runtime image pins at zero replicas (step 3), start one compatible
   worker (step 4), and run the receipt-bearing probe against the **same stored
   session**. Restore capacity/ingress only after it passes. Do not restore a
   pre-upgrade NATS snapshot: that loses admitted work and may replay side effects.
4. Record retained image IDs, preserved session/receipt identities and probe
   results. Keep old runtime deployments blocked for as long as those histories,
   backups or imported copies can be read. No zero-coordination rollback claim.

An emergency pre-receipt binary must use a separate isolated account with no
access to receipt-bearing histories, not this rollback path. Any migration or
history repair requires a separately reviewed procedure and evidence; stopping
writers or hiding rows with compaction is not such a procedure.

### Receipt-bearing verification

Run this isolated check from the release checkout; it starts only disposable
NATS/test workers, uses mock model calls, and cannot reach deployed services
under the repository test sandbox:

```bash
command -v nats-server
PROTOC=/usr/bin/protoc cargo nextest run -p harnx-runtime \
  -E 'test(timing_receipt_hard_cutover_and_retained_reader_rollback) | test(timing_recovery)' --no-fail-fast
PROTOC=/usr/bin/protoc cargo nextest run -p harnx-runtime \
  -E 'test(timing_receipt_hard_cutover_and_retained_reader_rollback)' --stress-count=20
```

`timing_receipt_hard_cutover_and_retained_reader_rollback` starts with a
receipt-free admitted prompt, cold-starts a compatible worker, completes/drains
it, stops that worker, then cold-starts a retained compatible reader over the
unchanged receipt-bearing history. A new genuinely admitted turn completes
once; the original log prefix and frozen limits stay unchanged, and receipts
never receive prompt admissions. Its negative control copies the baseline
`5a6c2cba3` User-row selector and confirms it selects an unadmitted receipt
after drain. This is **not** an old-binary execution test or a cross-version
rollback guarantee. The retained reader is the same compatible runtime build.
The existing recovery tests also cover interrupted admitted runs and compacted
raw receipts. Missing nats-server fails the cutover test, rather than counting
an unexercised check as passing.

Before production cutover, repeat a receipt-session probe in isolated staging
with the approved image and real frontend route: submit a harmless prompt,
save `harnx dump session <agent>@<cluster> <session-id> --format json`, and confirm
a durable timing receipt and normal completion. Stop all runtime processes,
restart only compatible images as in the rollback steps, submit a second
prompt to that same session and dump it again. Require the old receipt/log
prefix unchanged, a fresh initial receipt only for the new invocation, no
admission refusal and no timing-only extra turn. Record both dumps, image IDs
and logs in the release ticket. Repository tests verify reader/history behavior;
they do not verify a site's Kubernetes access controls or claim this staging
or production deployment has been performed.

## Tool Servers

Harnx native tool servers are long-running processes that connect to NATS as consumers and register themselves in the `harnx_tool_registry` KV bucket. They do not run as per-call stdio processes.

To deploy a tool server in Kubernetes:
1. Provide the same `HARNX_NATS_URL` and `HARNX_NATS_TOKEN` as the workers.
2. Provide the same `HARNX_SERVER_SCOPE=shared`.
3. Do not pass `--manage-servers` on either worker or tool server.

### Stateless tools: harnx-time-tools

Stateless tool servers run cleanly as standalone Deployments:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: harnx-time-tools
  labels:
    app: harnx-time-tools
spec:
  replicas: 1
  selector:
    matchLabels:
      app: harnx-time-tools
  template:
    metadata:
      labels:
        app: harnx-time-tools
    spec:
      containers:
        - name: time-server
          image: ghcr.io/dobesv/harnx:0.1.0
          command:
            - harnx-time-tools
            - --healthz-addr
            - :8081
            - --metrics-addr
            - :8456
          env:
            - name: HARNX_SERVER_SCOPE
              value: shared
            - name: HARNX_NATS_URL
              value: nats://nats.default.svc.cluster.local:4222
            - name: HARNX_NATS_TOKEN
              valueFrom:
                secretKeyRef:
                  name: harnx-secrets
                  key: nats-token
          ports:
            - containerPort: 8081
              name: healthz
            - containerPort: 8456
              name: metrics
          readinessProbe:
            httpGet:
              path: /healthz
              port: 8081
            initialDelaySeconds: 2
            periodSeconds: 5
```

### Filesystem and shell tools: use sandbox gateway

`harnx-fs-tools` and `harnx-bash-tools` are not suitable as standalone container deployments. A standalone container has its own isolated, empty filesystem rather than the repository or project files that agents need to inspect and modify.

For filesystem and shell execution in Kubernetes, deploy `harnx-k8s-sandbox-tools`. It manages isolated agent sandboxes dynamically via Kubernetes Agent Sandboxes. See [Kubernetes Sandbox Tool Gateway](kubernetes-sandbox-tools.md).

### External MCP servers: harnx-mcp-bridge

To use external Model Context Protocol (MCP) servers that communicate over stdio, run `harnx-mcp-bridge` in a container that installs the external MCP tool. `harnx-mcp-bridge` wraps the stdio process and exposes it as a native tool server on NATS within `HARNX_SERVER_SCOPE`.

Set the bridge container's `HARNX_MCP_MAX_RESULT_BYTES` to change the default
32,768-byte compact JSON cap per tool result. A `--max-result-bytes` argument
before `--` overrides that environment variable; the minimum is 1,024 bytes:

```yaml
env:
  - name: HARNX_MCP_MAX_RESULT_BYTES
    value: "32768"
```

Oversized originals use the existing session-owned `harnx_attachments` object
store when trusted caller identity is available. Keep that store available and
size its retention for original outputs, not only previews. Contextless calls
cannot retain oversized originals and return explicit errors. See
[bridge result limits](../crates/harnx-mcp-bridge/README.md#result-limits-and-original-attachments)
and [journal/broker limits](nats-ha.md#oversized-terminal-tool-replies).

---

## Startup Diagnostics

When `harnx-serve` starts, it validates the resolved web assets directory before binding the HTTP listener.

If the directory does not exist or does not contain `index.html`, `harnx-serve` logs an actionable warning:

```text
Web assets unavailable at '<path>': directory is missing or does not contain index.html; set HARNX_WEB_ASSETS, pass --web-assets, or run `cargo xtask install`
```

The server continues running to serve API endpoints (`/v1/embeddings`, `/v1/rerank`), but requests to the Web UI return `404 Not Found`.

If you see this warning in your pod logs:
1. **Baked image path:** Ensure the container image was not overridden with a custom binary-only build.
2. **Image volume path:** Verify that the `image` volume is mounted at the exact path configured in `HARNX_WEB_ASSETS` (for example, `/web-assets`), and that `volumeMounts[].mountPath` matches.
3. **Image reference:** Verify that the image volume references `ghcr.io/dobesv/harnx-web-assets:<version>` and that the image was pulled successfully.

---

## Version Pinning

Always pin `ghcr.io/dobesv/harnx-web-assets` to the **exact same release `<version>` tag** as `ghcr.io/dobesv/harnx`.

Both images are published by the same release workflow from git release tags (`harnx/v<version>`). Stable releases publish both `<version>` and `:latest` tags.

Avoid `:latest` in production:
- The Web UI interacts directly with `harnx-serve` HTTP routes and payload schemas.
- Version mismatches between frontend assets and backend APIs can lead to silent errors or UI drift.
- Because both images share release versioning, upgrading is a coordinated one-line change across image tags.

## A2A backends behind ingress

A2A replicas sharing NATS do not require sticky routing. Follow
[A2A replica operations](a2a-operations.md) before scaling: configure service
permissions, provision finite non-evicting task events and size retention for your
workload. Drain admitted old turns before stopping old writers; don't mix ownership
implementations. Runtime readers must understand `admission_closed` before its
writers are enabled. Keep exports configured until unresolved work settles.

Lease-TTL recovery is eventual, with no fixed failover SLO during NATS outages.
Interrupted work isn't replayed and completed tool side effects aren't rolled back.
Ingress disconnect is not task cancellation. Preserve the existing trusted-user
header handling and disable SSE buffering. R3/deployment fanout capacity must be
measured separately from the R1 regression probes documented in the operations guide.
