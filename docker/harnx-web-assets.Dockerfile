# Assets-only image: the built Web UI bundle at /web-assets, nothing else.
# BusyBox base (not distroless) so the image ships /bin/cp and /bin/sh: a
# Kubernetes initContainer can copy /web-assets into a shared emptyDir on any
# cluster version. It also works as a read-only OCI image volume on k8s 1.33+.
# Use `/bin/cp -R` (not `-a`) when copying: `-a` preserves ownership and will
# break for non-root serving containers that lack permission to chown.
FROM busybox:1.37
COPY web-assets/ /web-assets/
