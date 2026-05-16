FROM gcr.io/distroless/static-debian12:nonroot
ARG TARGETARCH
COPY linux-${TARGETARCH}/harnx-mcp-time /harnx-mcp-time
ENTRYPOINT ["/harnx-mcp-time"]
