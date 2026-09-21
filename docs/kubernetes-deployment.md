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
