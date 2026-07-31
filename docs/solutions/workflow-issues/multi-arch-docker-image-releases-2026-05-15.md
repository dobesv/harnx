---
title: "Multi-arch Docker image releases from pre-compiled GitHub release assets"
date: 2026-05-15
category: workflow-issues
problem_type: workflow_issue
component: release-pipeline
root_cause: "new executable release path without established patterns"
resolution_type: workflow_improvement
severity: medium
tags:
  - docker
  - multi-arch
  - github-actions
  - ghcr
  - release-workflow
  - buildx
plan_ref: docker-image-releases
---

## Problem

Adding Docker image publishing to an existing Rust release workflow required building multi-arch images from pre-compiled release assets. The release job already produces per-target archives, but Docker images needed separate handling for architecture mapping, asset extraction, and verification before pushing to GHCR.

## Symptoms

- No existing Docker image publishing in release workflow
- Need to support `linux/amd64` and `linux/arm64` from `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` Rust targets
- Release assets produced by matrix build with `fail-fast: false` — some targets may fail without blocking others
- `harnx-mcp-bash` archive contains TWO binaries (`harnx-mcp-bash` AND `harnx-mcp-bash-sandbox-run`)

## Investigation Steps

1. **Analyzed existing release workflow**: Matrix build produces `.tar.gz` archives per target. Archives follow naming convention `harnx-$VERSION-$TARGET.tar.gz`.

2. **Determined base images**:
   - `gcr.io/distroless/static-debian12:nonroot` for pure static binaries (MCP servers) — provides CA certs + tzdata (~2MB total)
   - `debian:bookworm-slim` for all-in-one image — required for `harnx-mcp-bash` which needs real shell, `git`, and GNU `env --chdir`

3. **Mapped Rust targets to Docker arch**:
   - `x86_64-unknown-linux-musl` → `linux-amd64` directory → `TARGETARCH=amd64`
   - `aarch64-unknown-linux-musl` → `linux-arm64` directory → `TARGETARCH=arm64`

4. **Discovered knope version stripping**: Tags like `harnx/v0.32.0` need prefix stripped: `VERSION="${GITHUB_REF_NAME#harnx/}"`

5. **Found `if: always() && !cancelled()` requirement**: User explicitly needed to handle flaky Windows/macOS release legs — Docker job should proceed when non-Linux targets fail.

## Root Cause

Release workflow had no Docker image publishing. Adding it required:
1. Downloading release assets AFTER release job completes
2. Extracting archives into architecture-specific directories
3. Building multi-arch images with `docker buildx`
4. Pushing to GHCR with appropriate tags

The key non-obvious issue: `ARG TARGETARCH` MUST be declared BEFORE any `COPY` instruction that uses `${TARGETARCH}`. Docker buildx sets this automatically from `--platform` flag.

## Solution

### Dockerfile Pattern for Multi-Arch

**Distroless single-binary image:**
```dockerfile
FROM gcr.io/distroless/static-debian12:nonroot
ARG TARGETARCH
COPY linux-${TARGETARCH}/harnx-mcp-time /harnx-mcp-time
ENTRYPOINT ["/harnx-mcp-time"]
```

**Debian all-in-one image:**
```dockerfile
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates tzdata git && \
    rm -rf /var/lib/apt/lists/*

ARG TARGETARCH

COPY linux-${TARGETARCH}/harnx /usr/local/bin/harnx
COPY linux-${TARGETARCH}/harnx-serve /usr/local/bin/harnx-serve
COPY linux-${TARGETARCH}/harnx-mcp-bash /usr/local/bin/harnx-mcp-bash
COPY linux-${TARGETARCH}/harnx-mcp-bash-sandbox-run /usr/local/bin/harnx-mcp-bash-sandbox-run
# ... additional binaries
```

### Release Workflow Pattern

```yaml
docker:
  needs: release
  if: always() && !cancelled()  # Proceed even if non-Linux matrix legs fail
  runs-on: ubuntu-latest
  permissions:
    contents: read
    packages: write
  steps:
    - uses: actions/checkout@v6

    - name: Determine version
      id: version
      run: |
        VERSION="${GITHUB_REF_NAME#harnx/}"  # Strip knope monorepo prefix
        echo "version=$VERSION" >> "$GITHUB_OUTPUT"

    - name: Set up Docker Buildx
      uses: docker/setup-buildx-action@v3

    - name: Log in to GHCR
      uses: docker/login-action@v3
      with:
        registry: ghcr.io
        username: ${{ github.actor }}
        password: ${{ secrets.GITHUB_TOKEN }}

    - name: Create arch directories
      run: mkdir -p linux-amd64 linux-arm64 docker-assets

    - name: Download x86_64 assets
      run: |
        V="${{ steps.version.outputs.version }}"
        gh release download "$GITHUB_REF_NAME" \
          --pattern "harnx-$V-x86_64-unknown-linux-musl.tar.gz" || true
        # ... additional per-binary downloads
      working-directory: docker-assets

    - name: Download aarch64 assets
      run: |
        V="${{ steps.version.outputs.version }}"
        gh release download "$GITHUB_REF_NAME" \
          --pattern "harnx-$V-aarch64-unknown-linux-musl.tar.gz" || true
        # ... additional per-binary downloads
      working-directory: docker-assets

    - name: Extract archives
      run: |
        cd docker-assets
        for f in *x86_64*.tar.gz; do
          [ -f "$f" ] && tar -xzf "$f" -C ../linux-amd64/
        done
        for f in *aarch64*.tar.gz; do
          [ -f "$f" ] && tar -xzf "$f" -C ../linux-arm64/
        done

    - name: Verify extracted binaries
      run: |
        missing=0
        for dir in linux-amd64 linux-arm64; do
            if [ ! -f "$dir/$bin" ]; then
              echo "ERROR: missing $dir/$bin"
              missing=1
            fi
          done
        done
        [ "$missing" -eq 0 ] || { echo "Some binaries are missing."; exit 1; }

    - name: Build and push image
      uses: docker/build-push-action@v6
      with:
        context: .
        file: docker/harnx.Dockerfile
        platforms: linux/amd64,linux/arm64
        push: true
        tags: |
          ghcr.io/${{ github.repository_owner }}/harnx:${{ steps.version.outputs.version }}
          ${{ needs.release.outputs.rc == 'false' && 'ghcr.io/${{ github.repository_owner }}/harnx:latest' || '' }}
```

### CI Lint Job for Dockerfiles

```yaml
docker-lint:
  name: Lint Dockerfiles
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v6
    - name: Set up Docker Buildx
      uses: docker/setup-buildx-action@v3
    - name: Check Dockerfile
      run: docker buildx build --check -f docker/harnx.Dockerfile .
```

## Why This Works

1. **`ARG TARGETARCH` before `COPY`**: Docker buildx automatically sets `TARGETARCH` to `amd64` or `arm64` based on `--platform`. This MUST be declared before any instruction that references it.

2. **`|| true` on downloads + explicit verification**: Downloads use `|| true` because non-Linux matrix failures mean some assets legitimately don't exist. Verification step catches missing Linux binaries before build/push.

3. **`if: always() && !cancelled()`**: Allows Docker job to proceed when non-Linux release legs fail. Combined with verification step, prevents broken images from being published.

4. **`gcr.io/distroless/static-debian12:nonroot`**: Minimal base (~2MB) with CA certs and tzdata. Appropriate for pure static binaries. Does NOT work for binaries requiring shell or `env --chdir`.

5. **`debian:bookworm-slim`**: Required when binaries need real shell, `git`, or GNU coreutils features like `env --chdir`.

6. **Version stripping**: Knope monorepo tags include package prefix (`harnx/v0.32.0`). Strip with `${GITHUB_REF_NAME#harnx/}`.

7. **`latest` tag gating**: Use `needs.release.outputs.rc == 'false'` to apply `latest` only on stable releases.

## Prevention Strategies

**Required Patterns:**
- Always declare `ARG TARGETARCH` before any `COPY` that uses `${TARGETARCH}`
- Verify all expected binaries exist after extraction, before `docker build`
- Use `docker buildx build --check` in CI for Dockerfile validation
- Check `rc` output from release job before applying `latest` tag

**Test Coverage:**
- CI job to lint Dockerfiles on PRs
- Release workflow verification step checks all 8 binaries × 2 architectures
- Build fails at COPY instruction if binaries missing — verification step catches earlier with clear message

**Code Review Checklist:**
- [ ] `ARG TARGETARCH` declared before `COPY` instructions
- [ ] Architecture directories match `linux-${TARGETARCH}` pattern
- [ ] Verification step runs before any `docker/build-push-action`
- [ ] Multi-arch archives contain both `harnx-mcp-bash` AND `harnx-mcp-bash-sandbox-run`
- [ ] `latest` tag conditional on `rc == 'false'`

## Related Issues

- **Issue:** [GitHub #551](https://github.com/dobesv/harnx/issues/551) — Build docker image releases
- **Related Solution:** [cli-wrapper-sandboxing-for-tokio-servers-2026-04-28.md](../integration-issues/cli-wrapper-sandboxing-for-tokio-servers-2026-04-28.md) — Documents why `harnx-mcp-bash` ships two binaries
- **Related Solution:** [private-oci-registry-auth-2026-05-14.md](../integration-issues/private-oci-registry-auth-2026-05-14.md) — GHCR authentication patterns
