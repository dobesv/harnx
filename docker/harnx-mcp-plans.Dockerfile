FROM gcr.io/distroless/static-debian12:nonroot
ARG TARGETARCH
COPY linux-${TARGETARCH}/harnx-mcp-plans /harnx-mcp-plans
ENTRYPOINT ["/harnx-mcp-plans"]
