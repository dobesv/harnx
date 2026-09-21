FROM gcr.io/distroless/static-debian12:nonroot
ARG TARGETARCH
COPY linux-${TARGETARCH}/harnx-plans-tools /harnx-plans-tools
ENTRYPOINT ["/harnx-plans-tools"]
