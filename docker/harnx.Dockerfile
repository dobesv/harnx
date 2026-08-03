FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates tzdata git && \
    rm -rf /var/lib/apt/lists/*

ARG TARGETARCH

COPY linux-${TARGETARCH}/harnx /usr/local/bin/harnx
COPY linux-${TARGETARCH}/harnx-serve /usr/local/bin/harnx-serve
COPY linux-${TARGETARCH}/harnx-bash-tools /usr/local/bin/harnx-bash-tools
COPY linux-${TARGETARCH}/harnx-fs-tools /usr/local/bin/harnx-fs-tools
COPY linux-${TARGETARCH}/harnx-grep-tools /usr/local/bin/harnx-grep-tools
COPY linux-${TARGETARCH}/harnx-plans-tools /usr/local/bin/harnx-plans-tools
COPY linux-${TARGETARCH}/harnx-mcp-time /usr/local/bin/harnx-mcp-time
COPY linux-${TARGETARCH}/harnx-time-server /usr/local/bin/harnx-time-server
COPY linux-${TARGETARCH}/harnx-mcp-bridge /usr/local/bin/harnx-mcp-bridge
COPY linux-${TARGETARCH}/harnx-aws-creds /usr/local/bin/harnx-aws-creds
COPY linux-${TARGETARCH}/harnx-k8s-creds /usr/local/bin/harnx-k8s-creds
COPY linux-${TARGETARCH}/harnx-pkg /usr/local/bin/harnx-pkg
COPY linux-${TARGETARCH}/harnx-proxy-auth /usr/local/bin/harnx-proxy-auth
COPY linux-${TARGETARCH}/harnx-sandbox-run /usr/local/bin/harnx-sandbox-run
COPY linux-${TARGETARCH}/harnx-sandbox-exec /usr/local/bin/harnx-sandbox-exec
