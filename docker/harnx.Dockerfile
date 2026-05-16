FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates tzdata git && \
    rm -rf /var/lib/apt/lists/*

ARG TARGETARCH

COPY linux-${TARGETARCH}/harnx /usr/local/bin/harnx
COPY linux-${TARGETARCH}/harnx-serve /usr/local/bin/harnx-serve
COPY linux-${TARGETARCH}/harnx-acp-server /usr/local/bin/harnx-acp-server
COPY linux-${TARGETARCH}/harnx-mcp-bash /usr/local/bin/harnx-mcp-bash
COPY linux-${TARGETARCH}/harnx-mcp-bash-sandbox-run /usr/local/bin/harnx-mcp-bash-sandbox-run
COPY linux-${TARGETARCH}/harnx-mcp-fs /usr/local/bin/harnx-mcp-fs
COPY linux-${TARGETARCH}/harnx-mcp-plans /usr/local/bin/harnx-mcp-plans
COPY linux-${TARGETARCH}/harnx-mcp-time /usr/local/bin/harnx-mcp-time
COPY linux-${TARGETARCH}/harnx-aws-creds /usr/local/bin/harnx-aws-creds
COPY linux-${TARGETARCH}/harnx-pkg /usr/local/bin/harnx-pkg
