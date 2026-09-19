# Assets-only image: the built Web UI bundle at /web-assets, nothing else.
# `scratch` base — the image is data, not a runnable container. Mount it
# directly as a Kubernetes OCI image volume (k8s 1.33+) so harnx-serve reads
# the UI from it with no init container and no copy step. Point the server at
# the mount path with HARNX_WEB_ASSETS or --web-assets.
FROM scratch
COPY web-assets/ /web-assets/
