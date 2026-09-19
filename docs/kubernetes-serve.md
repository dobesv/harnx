# Deploy harnx-serve on Kubernetes

`harnx-serve` provides the HTTP server for Harnx, serving the Web UI alongside embeddings and reranking API endpoints.

By default, `harnx-serve` listens on loopback (`127.0.0.1:8000`). In Kubernetes, pass `--addr 0.0.0.0:8000` so pods and ingress controllers can reach the server.

You can deploy `harnx-serve` using either of two container strategies:
1. **Baked image (simplest):** Run `ghcr.io/dobesv/harnx:<version>`, which packages both the binaries and the Web UI.
2. **Split containers (flexible):** Run a dedicated server container and use `ghcr.io/dobesv/harnx-web-assets:<version>` as an init container to copy assets into a shared volume.

---

## Path 1: Baked image (simplest)

The official all-in-one image `ghcr.io/dobesv/harnx:<version>` bakes the static Web UI files directly into `/usr/local/share/harnx/web-assets`.

The image sets `ENV HARNX_WEB_ASSETS=/usr/local/share/harnx/web-assets`. `harnx-serve` reads this environment variable automatically on startup, so the UI is served without any extra configuration or volume mounts.

### Minimal Deployment manifest

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
          ports:
            - containerPort: 8000
              name: http
```

Use this path if you deploy Harnx as a self-contained service and do not need to separate static assets from the application binary.

---

## Path 2: Split containers (flexible)

In architectures where components like `harnx-serve`, `harnx-worker`, and tool servers run in separate pods or minimal custom images, you can pull web assets independently using the standalone assets image `ghcr.io/dobesv/harnx-web-assets:<version>`.

The assets image contains only the compiled static bundle located at `/web-assets`. It is built on `busybox:1.37` (which includes `/bin/cp` and `/bin/sh`) with no entrypoint. Use `/bin/cp -R` (not `-a`) when copying: `-a` preserves ownership and breaks for non-root serving containers.

To use it, configure an init container that copies the assets into a shared `emptyDir` volume mounted by the `harnx-serve` container. Use the explicit exec array form `command: ["/bin/cp", "-R", "/web-assets/.", "/shared/web-assets"]` to avoid shell-glob issues, and use `-R` rather than `-a` to avoid permission errors when the serving container runs as non-root. Point `harnx-serve` to this directory using the `HARNX_WEB_ASSETS` environment variable or the `--web-assets <path>` flag. Alternatively, on Kubernetes 1.33+, you can use it directly as an OCI image volume without an init container.

### Minimal Deployment manifest

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: harnx-serve-split
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
      securityContext:
        fsGroup: 10001
      initContainers:
        - name: copy-web-assets
          image: ghcr.io/dobesv/harnx-web-assets:0.1.0
          command:
            - /bin/cp
            - -R
            - /web-assets/.
            - /shared/web-assets
          volumeMounts:
            - name: web-assets-volume
              mountPath: /shared/web-assets
      containers:
        - name: harnx-serve
          image: ghcr.io/dobesv/harnx:0.1.0
          command:
            - harnx-serve
            - --addr
            - 0.0.0.0:8000
          env:
            - name: HARNX_WEB_ASSETS
              value: /shared/web-assets
          ports:
            - containerPort: 8000
              name: http
          volumeMounts:
            - name: web-assets-volume
              mountPath: /shared/web-assets
              readOnly: true
      volumes:
        - name: web-assets-volume
          emptyDir: {}
```

### Non-root security context

BusyBox runs as root by default, while production serving containers typically run non-root. Setting `spec.template.spec.securityContext.fsGroup` (as shown above) assigns the shared `emptyDir` volume to that group GID. This ensures the non-root serving container can read the copied assets without manual `chown` steps.

---

## Version pinning

Always pin `ghcr.io/dobesv/harnx-web-assets` to the **exact same release `<version>` tag** as the `harnx` or `harnx-serve` image.

Both images are built from the same release pipeline and derived from the same git tag (`harnx/v<version>`). Stable releases publish both `<version>` and `:latest` tags.

Avoid using `:latest` in production environments:
- The Web UI communicates directly with `harnx-serve` API endpoints.
- Mismatched versions can lead to UI/API drift, where frontend components request updated endpoints or payloads that older server binaries do not support (or vice-versa).
- Keeping both images on the identical version tag ensures feature compatibility across updates. When upgrading, update both image tags together.

---

## Startup diagnostics

When `harnx-serve` starts, it validates the resolved web assets directory before binding the HTTP listener.

If the directory does not exist or does not contain an `index.html` file, `harnx-serve` logs a warning:

```text
Web assets unavailable at '<path>': directory is missing or does not contain index.html; set HARNX_WEB_ASSETS, pass --web-assets, or run `cargo xtask install`
```

The server continues to run and serves API endpoints, but requests to the Web UI return `404 Not Found`.

If you see this warning in Kubernetes pod logs:
1. **Check the volume mount:** Confirm that the `emptyDir` volume is mounted at the expected path inside both the init container and the `harnx-serve` container.
2. **Check the target path:** Ensure the init container copied the files so that `index.html` resides directly at the root of the mounted directory (for example, `/shared/web-assets/index.html`).
3. **Check the environment variable:** Verify that `HARNX_WEB_ASSETS` matches the mount path in `volumeMounts` (or that `--web-assets` was passed with that exact path).
